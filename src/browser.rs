//! Headless browser fetching, for JavaScript-heavy and signed-in pages.
//!
//! Some sites send an almost empty HTML shell and build the article with
//! JavaScript; others only show the full text to signed-in subscribers. For
//! both, Topebag can load the page in **Microsoft Edge** (or Google Chrome)
//! running headless and take the rendered DOM (`--dump-dom`).
//!
//! The browser always runs with Topebag's **own profile folder**
//! (`<data dir>/browser-profile`), never your everyday browser profile.
//! [`open_sign_in_window`] opens a normal, visible window on that profile so
//! you can sign in to your subscriptions once; later headless fetches reuse
//! those cookies. Your main browser's passwords and cookies are never read.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use url::Url;

/// How long a headless page load may take before it is abandoned.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(45);

/// Topebag's dedicated browser profile inside the data directory.
pub fn profile_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("browser-profile")
}

/// Locate a Chromium-based browser: Edge first (always present on Windows
/// 10/11), then Chrome.
pub fn find_browser() -> Option<PathBuf> {
    if let Some(custom) = std::env::var_os("TOPEBAG_BROWSER").map(PathBuf::from).filter(|p| p.exists()) {
        return Some(custom);
    }
    candidates().into_iter().find(|p| p.exists())
}

fn candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    #[cfg(windows)]
    {
        let env = |k: &str| std::env::var_os(k).map(PathBuf::from);
        for base in [env("ProgramFiles(x86)"), env("ProgramFiles"), env("LOCALAPPDATA")].into_iter().flatten() {
            out.push(base.join(r"Microsoft\Edge\Application\msedge.exe"));
        }
        for base in [env("ProgramFiles"), env("ProgramFiles(x86)"), env("LOCALAPPDATA")].into_iter().flatten() {
            out.push(base.join(r"Google\Chrome\Application\chrome.exe"));
        }
    }
    #[cfg(target_os = "macos")]
    {
        out.push("/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge".into());
        out.push("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".into());
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        for name in ["microsoft-edge", "google-chrome", "google-chrome-stable", "chromium", "chromium-browser"] {
            for dir in ["/usr/bin", "/usr/local/bin", "/snap/bin"] {
                out.push(Path::new(dir).join(name));
            }
        }
    }
    out
}

/// Command-line arguments for a headless DOM dump.
pub fn dump_dom_args(url: &Url, profile: &Path) -> Vec<String> {
    vec![
        "--headless=new".into(),
        "--disable-gpu".into(),
        "--no-first-run".into(),
        "--no-default-browser-check".into(),
        "--disable-extensions".into(),
        "--disable-sync".into(),
        "--mute-audio".into(),
        "--hide-scrollbars".into(),
        "--window-size=1280,2000".into(),
        // Let scripts and lazy-loading run for a while before dumping.
        "--virtual-time-budget=10000".into(),
        format!("--user-data-dir={}", profile.display()),
        "--dump-dom".into(),
        url.to_string(),
    ]
}

/// Load `url` in a headless browser and return the rendered HTML.
pub fn dump_dom(url: &Url, profile: &Path, timeout: Duration) -> Result<String> {
    let browser = find_browser().context("no Microsoft Edge or Google Chrome installation was found")?;
    std::fs::create_dir_all(profile).with_context(|| format!("creating {}", profile.display()))?;

    let mut child = Command::new(&browser)
        .args(dump_dom_args(url, profile))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("starting {}", browser.display()))?;

    // Read both pipes on threads so a chatty browser can't deadlock us.
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let out_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let err_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        buf
    });

    let start = Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!("the browser took longer than {} s to load the page", timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let html = String::from_utf8_lossy(&out_thread.join().unwrap_or_default()).into_owned();
    let errors = String::from_utf8_lossy(&err_thread.join().unwrap_or_default()).into_owned();
    if html.trim().len() < 64 {
        let hint = if errors.contains("ProcessSingleton") || errors.contains("profile") {
            " — is the Topebag sign-in browser window still open? Close it and try again"
        } else {
            ""
        };
        bail!("the browser returned an empty page{hint}");
    }
    Ok(html)
}

/// Open a normal, visible browser window on Topebag's profile so the user
/// can sign in to sites. Returns immediately.
pub fn open_sign_in_window(profile: &Path, start_url: Option<&str>) -> Result<()> {
    let browser = find_browser().context("no Microsoft Edge or Google Chrome installation was found")?;
    std::fs::create_dir_all(profile)?;
    let mut cmd = Command::new(browser);
    cmd.arg(format!("--user-data-dir={}", profile.display()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--new-window");
    cmd.arg(start_url.unwrap_or("about:blank"));
    cmd.spawn().context("starting the browser")?;
    Ok(())
}

/// Does `url` belong to one of `sites` (bare domains such as `nytimes.com`;
/// subdomains match too)?
pub fn matches_site(url: &Url, sites: &[String]) -> bool {
    let Some(host) = url.host_str() else { return false };
    let host = host.trim_start_matches("www.").to_ascii_lowercase();
    sites.iter().any(|site| {
        let site = normalize_site(site);
        !site.is_empty() && (host == site || host.ends_with(&format!(".{site}")))
    })
}

/// Turn user input like `https://www.NYTimes.com/section` into `nytimes.com`.
pub fn normalize_site(input: &str) -> String {
    let s = input.trim().to_ascii_lowercase();
    let s = s.split("://").last().unwrap_or("");
    let s = s.split(['/', '?', '#']).next().unwrap_or("");
    s.trim_start_matches("www.").trim_matches('.').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn site_matching() {
        let sites = vec!["nytimes.com".to_string(), " https://www.FT.com/content ".to_string()];
        let u = |s: &str| Url::parse(s).unwrap();
        assert!(matches_site(&u("https://www.nytimes.com/2026/a.html"), &sites));
        assert!(matches_site(&u("https://cooking.nytimes.com/x"), &sites));
        assert!(matches_site(&u("https://ft.com/content/1"), &sites));
        assert!(!matches_site(&u("https://notnytimes.com/"), &sites));
        assert!(!matches_site(&u("https://example.com/"), &[]));
    }

    #[test]
    fn args_use_the_topebag_profile() {
        let url = Url::parse("https://example.com/").unwrap();
        let args = dump_dom_args(&url, Path::new("C:/data/browser-profile"));
        assert!(args.iter().any(|a| a == "--dump-dom"));
        assert!(args.iter().any(|a| a.starts_with("--user-data-dir=") && a.contains("browser-profile")));
        assert_eq!(args.last().unwrap(), "https://example.com/");
    }
}
