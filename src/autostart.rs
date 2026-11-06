//! Starting Topebag when you sign in.
//!
//! **Windows** ("Start with Windows"): enabling it writes a single value,
//! `Topebag`, under
//! `HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Run`, the
//! standard per-user startup list (visible in Task Manager → Startup apps).
//! It launches `topebag.exe gui --background`, which starts straight into the
//! system tray. Disabling it deletes that value again.
//!
//! **macOS** ("Open at login"): enabling it writes a per-user LaunchAgent,
//! `~/Library/LaunchAgents/io.github.jack-crawford.topebag.plist`, which runs
//! `topebag gui --background` when you log in. There's no tray icon on macOS
//! yet, so that simply opens the window. Disabling it deletes the file.
//!
//! Nothing else is touched and no administrator rights are needed. Other
//! systems aren't supported yet.

use std::path::{Path, PathBuf};

use anyhow::Result;

/// Registry key of the per-user startup list.
pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// Name of Topebag's value in that key.
pub const VALUE_NAME: &str = "Topebag";

/// Whether this system can start Topebag at sign-in.
pub const SUPPORTED: bool = cfg!(any(windows, target_os = "macos"));
/// What the setting is called here.
pub const LABEL: &str =
    if cfg!(target_os = "macos") { "Open Topebag when you log in" } else { "Start with Windows (in the tray)" };

/// The command line registered for startup.
pub fn command_line(exe: &Path) -> String {
    format!("\"{}\" gui --background", exe.display())
}

/// Is Topebag registered to start at sign-in?
#[cfg(windows)]
pub fn is_enabled() -> bool {
    platform::read(RUN_KEY, VALUE_NAME).is_some()
}

/// Register (or unregister) the current executable to start at sign-in.
#[cfg(windows)]
pub fn set_enabled(enabled: bool) -> Result<()> {
    if enabled {
        let exe = std::env::current_exe()?;
        platform::write(RUN_KEY, VALUE_NAME, &command_line(&exe))
    } else {
        platform::delete(RUN_KEY, VALUE_NAME)
    }
}

/// Is Topebag registered to start at sign-in?
#[cfg(not(windows))]
pub fn is_enabled() -> bool {
    cfg!(target_os = "macos") && launch_agent::path().is_some_and(|p| p.exists())
}

/// Register (or unregister) the current executable to start at sign-in.
#[cfg(not(windows))]
pub fn set_enabled(enabled: bool) -> Result<()> {
    if !cfg!(target_os = "macos") {
        anyhow::bail!("starting at login isn't supported on this system yet");
    }
    let path = launch_agent::path().ok_or_else(|| anyhow::anyhow!("no home folder"))?;
    if enabled { launch_agent::install(&path, &std::env::current_exe()?) } else { launch_agent::remove(&path) }
}

/// The macOS LaunchAgent. Plain file handling, compiled (and tested) on
/// every platform.
pub mod launch_agent {
    use super::*;

    /// The agent's label (and file name).
    pub const LABEL: &str = "io.github.jack-crawford.topebag";

    /// `~/Library/LaunchAgents/<label>.plist`.
    pub fn path() -> Option<PathBuf> {
        directories::BaseDirs::new()
            .map(|d| d.home_dir().join("Library").join("LaunchAgents").join(format!("{LABEL}.plist")))
    }

    /// The agent: run `exe gui --background` at login.
    pub fn plist(exe: &Path) -> String {
        let exe = exe.display().to_string().replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>gui</string>
        <string>--background</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>ProcessType</key>
    <string>Interactive</string>
</dict>
</plist>
"#
        )
    }

    pub fn install(path: &Path, exe: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, plist(exe))?;
        Ok(())
    }

    pub fn remove(path: &Path) -> Result<()> {
        match std::fs::remove_file(path) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(err.into()),
            _ => Ok(()),
        }
    }
}

#[cfg(windows)]
mod platform {
    use anyhow::{Result, bail};
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows::Win32::System::Registry::{
        HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
    };
    #[cfg(test)]
    use windows::Win32::System::Registry::RegDeleteKeyW;
    use windows::core::{HSTRING, PCWSTR};

    pub fn read(key: &str, name: &str) -> Option<String> {
        let (key, name) = (HSTRING::from(key), HSTRING::from(name));
        let mut len = 0u32;
        // SAFETY: first call only queries the size.
        let status = unsafe {
            RegGetValueW(HKEY_CURRENT_USER, PCWSTR(key.as_ptr()), PCWSTR(name.as_ptr()), RRF_RT_REG_SZ, None, None, Some(&mut len))
        };
        if status != ERROR_SUCCESS || len == 0 {
            return None;
        }
        let mut buf = vec![0u16; (len as usize).div_ceil(2)];
        // SAFETY: `buf` holds `len` bytes.
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                PCWSTR(key.as_ptr()),
                PCWSTR(name.as_ptr()),
                RRF_RT_REG_SZ,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&mut len),
            )
        };
        if status != ERROR_SUCCESS {
            return None;
        }
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..end]))
    }

    pub fn write(key: &str, name: &str, value: &str) -> Result<()> {
        let (key, name) = (HSTRING::from(key), HSTRING::from(name));
        let data: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `data` is a NUL-terminated UTF-16 string of the given byte size.
        let status = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                PCWSTR(key.as_ptr()),
                PCWSTR(name.as_ptr()),
                REG_SZ.0,
                Some(data.as_ptr().cast()),
                (data.len() * 2) as u32,
            )
        };
        if status != ERROR_SUCCESS {
            bail!("could not update the startup list (error {})", status.0);
        }
        Ok(())
    }

    pub fn delete(key: &str, name: &str) -> Result<()> {
        let (key, name) = (HSTRING::from(key), HSTRING::from(name));
        // SAFETY: plain registry call with valid NUL-terminated strings.
        let status = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, PCWSTR(key.as_ptr()), PCWSTR(name.as_ptr())) };
        if status != ERROR_SUCCESS && status != ERROR_FILE_NOT_FOUND {
            bail!("could not update the startup list (error {})", status.0);
        }
        Ok(())
    }

    /// Remove an (empty) key; only used to clean up after tests.
    #[cfg(test)]
    pub fn delete_key(key: &str) {
        let key = HSTRING::from(key);
        // SAFETY: plain registry call with a valid NUL-terminated string.
        unsafe {
            let _ = RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR(key.as_ptr()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_agent_runs_topebag_in_the_background_at_login() {
        let plist = launch_agent::plist(Path::new("/Applications/Tope & Bag/topebag"));
        assert!(plist.contains("<string>/Applications/Tope &amp; Bag/topebag</string>"), "{plist}");
        assert!(plist.contains("<string>gui</string>") && plist.contains("<string>--background</string>"));
        assert!(plist.contains("<key>RunAtLoad</key>\n    <true/>"));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("LaunchAgents").join("agent.plist");
        launch_agent::install(&path, Path::new("/usr/local/bin/topebag")).unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("/usr/local/bin/topebag"));
        launch_agent::remove(&path).unwrap();
        assert!(!path.exists());
        launch_agent::remove(&path).unwrap(); // removing twice is fine
        if let Some(real) = launch_agent::path() {
            assert!(real.ends_with("Library/LaunchAgents/io.github.jack-crawford.topebag.plist"));
        }
    }

    #[test]
    fn command_line_quotes_the_path() {
        let cmd = command_line(Path::new(r"C:\Program Files\Topebag\topebag.exe"));
        assert_eq!(cmd, r#""C:\Program Files\Topebag\topebag.exe" gui --background"#);
    }

    /// Exercises the registry helpers on a throwaway key (never the real
    /// startup list).
    #[cfg(windows)]
    #[test]
    fn registry_round_trip_on_a_scratch_key() {
        let key = r"Software\TopebagTest";
        let name = format!("t{}", std::process::id());
        platform::write(key, &name, "hello").unwrap();
        assert_eq!(platform::read(key, &name).as_deref(), Some("hello"));
        platform::delete(key, &name).unwrap();
        assert_eq!(platform::read(key, &name), None);
        platform::delete(key, &name).unwrap(); // deleting twice is fine
        platform::delete_key(key);
    }
}
