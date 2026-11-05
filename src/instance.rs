//! Single-instance support for the desktop app.
//!
//! Topebag can live in the system tray, so launching it again (from the
//! Start menu, a shortcut, or `topebag gui <url>`) should *reuse* the running
//! copy rather than start a second one. The first instance listens on a
//! random localhost port and writes it, with a random token, to
//! `instance.json` in the data directory. Later launches connect, pass along
//! "show the window" and any links, and exit.
//!
//! Only connections that present the token are accepted, and the port is
//! bound to 127.0.0.1, so other machines can't reach it.

use std::collections::hash_map::RandomState;
use std::fs;
use std::hash::{BuildHasher, Hasher};
use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// A request from a later launch to the running instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// Bring the window to the front.
    Show,
    /// Queue a link.
    Add(String),
}

#[derive(Serialize, Deserialize)]
struct Info {
    port: u16,
    token: String,
}

/// The outcome of [`claim`].
pub enum Role {
    /// No other instance is running; this one should start the app and
    /// call [`Primary::listen`].
    Primary(Primary),
    /// Another instance is running and has received our messages.
    Secondary,
}

/// The running instance's listener.
pub struct Primary {
    listener: TcpListener,
    token: String,
    info_path: PathBuf,
}

fn info_path(data_dir: &Path) -> PathBuf {
    data_dir.join("instance.json")
}

fn random_token() -> String {
    let mut out = String::new();
    for _ in 0..2 {
        let mut h = RandomState::new().build_hasher();
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
        h.write_u32(std::process::id());
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out
}

/// Become the primary instance, or hand `messages` to the one already
/// running.
pub fn claim(data_dir: &Path, messages: &[Message]) -> Result<Role> {
    let path = info_path(data_dir);
    if let Some(info) = fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str::<Info>(t.trim_start_matches('\u{feff}')).ok()) {
        if send_to(&info, messages).is_ok() {
            return Ok(Role::Secondary);
        }
    }
    fs::create_dir_all(data_dir)?;
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).context("opening the single-instance port")?;
    let info = Info { port: listener.local_addr()?.port(), token: random_token() };
    fs::write(&path, serde_json::to_string(&info)?)?;
    Ok(Role::Primary(Primary { listener, token: info.token, info_path: path }))
}

fn send_to(info: &Info, messages: &[Message]) -> Result<()> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, info.port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(400))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    writeln!(stream, "{} ping", info.token)?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut reply = String::new();
    reader.read_line(&mut reply)?;
    if reply.trim() != "pong" {
        bail!("not a Topebag instance");
    }
    for message in messages {
        match message {
            Message::Show => writeln!(stream, "{} show", info.token)?,
            Message::Add(url) => writeln!(stream, "{} add {}", info.token, url.replace(['\r', '\n'], ""))?,
        }
    }
    stream.flush()?;
    Ok(())
}

impl Primary {
    /// Handle messages from later launches on a background thread.
    pub fn listen(self, on_message: impl Fn(Message) + Send + 'static) {
        let Primary { listener, token, .. } = self;
        std::thread::Builder::new()
            .name("topebag-instance".into())
            .spawn(move || {
                for stream in listener.incoming().flatten() {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    let Ok(reader_stream) = stream.try_clone() else { continue };
                    let mut writer = stream;
                    for line in BufReader::new(reader_stream).lines() {
                        let Ok(line) = line else { break };
                        let Some(rest) = line.strip_prefix(&token).and_then(|r| r.strip_prefix(' ')) else {
                            break; // wrong token: drop the connection
                        };
                        match rest.split_once(' ').map_or((rest, ""), |(a, b)| (a, b)) {
                            ("ping", _) => {
                                let _ = writeln!(writer, "pong");
                            }
                            ("show", _) => on_message(Message::Show),
                            ("add", url) if !url.trim().is_empty() => on_message(Message::Add(url.trim().to_string())),
                            _ => {}
                        }
                    }
                }
            })
            .expect("failed to start instance listener");
    }

    /// Path of the `instance.json` file (removed on clean exit).
    pub fn info_path(&self) -> &Path {
        &self.info_path
    }
}

/// Remove `instance.json` on clean exit (stale files are harmless: the
/// next launch simply fails to connect and becomes primary).
pub fn release(data_dir: &Path) {
    let _ = fs::remove_file(info_path(data_dir));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    #[test]
    fn second_launch_hands_over_to_the_first() {
        let dir = tempfile::tempdir().unwrap();
        let Role::Primary(primary) = claim(dir.path(), &[]).unwrap() else { panic!("first launch is primary") };
        let (tx, rx) = channel();
        primary.listen(move |m| {
            let _ = tx.send(m);
        });

        let role = claim(dir.path(), &[Message::Show, Message::Add("https://example.com/a".into())]).unwrap();
        assert!(matches!(role, Role::Secondary));
        let got: Vec<Message> = (0..2).map(|_| rx.recv_timeout(Duration::from_secs(5)).unwrap()).collect();
        assert_eq!(got, [Message::Show, Message::Add("https://example.com/a".into())]);
    }

    #[test]
    fn stale_info_file_means_primary() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(info_path(dir.path()), r#"{"port": 9, "token": "x"}"#).unwrap();
        assert!(matches!(claim(dir.path(), &[]).unwrap(), Role::Primary(_)));
    }

    #[test]
    fn wrong_token_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let Role::Primary(primary) = claim(dir.path(), &[]).unwrap() else { panic!() };
        let port = primary.listener.local_addr().unwrap().port();
        primary.listen(|_| panic!("must not be called"));
        let forged = Info { port, token: "wrong".into() };
        assert!(send_to(&forged, &[Message::Show]).is_err());
    }
}
