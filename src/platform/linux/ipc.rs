//! Control socket at `$XDG_RUNTIME_DIR/clip-recorder.sock`: one command per
//! connection, one reply. Used by `clip-recorder save`, the tray menu and the
//! settings window, and bindable to any compositor shortcut.

use std::io::{Read, Write};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{mpsc, oneshot};

pub type Reply = oneshot::Sender<Result<String, String>>;

pub struct Request {
    pub cmd: String,
    pub reply: Option<Reply>,
}

pub fn socket_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    dir.join("clip-recorder.sock")
}

pub fn serve(tx: mpsc::UnboundedSender<Request>) -> Result<()> {
    let path = socket_path();
    if std::os::unix::net::UnixStream::connect(&path).is_ok() {
        bail!("clip-recorder is already running ({})", path.display());
    }
    let _ = std::fs::remove_file(&path);
    let listener =
        UnixListener::bind(&path).with_context(|| format!("cannot bind {}", path.display()))?;
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let tx = tx.clone();
            tokio::spawn(async move {
                let (read, mut write) = stream.into_split();
                let mut line = String::new();
                if BufReader::new(read).read_line(&mut line).await.is_err() {
                    return;
                }
                let (rtx, rrx) = oneshot::channel();
                let _ = tx.send(Request {
                    cmd: line.trim().to_owned(),
                    reply: Some(rtx),
                });
                let reply = match rrx.await {
                    Ok(Ok(body)) => format!("ok {body}"),
                    Ok(Err(e)) => format!("error {e}"),
                    Err(_) => "error recorder stopped".into(),
                };
                let _ = write.write_all(reply.as_bytes()).await;
            });
        }
    });
    Ok(())
}

/// Sends `cmd` to the running recorder and returns the body of an `ok` reply.
pub fn request(cmd: &str) -> Result<String> {
    let path = socket_path();
    let mut stream = std::os::unix::net::UnixStream::connect(&path).with_context(|| {
        format!(
            "clip-recorder is not running (no socket at {})",
            path.display()
        )
    })?;
    stream.write_all(format!("{cmd}\n").as_bytes())?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply)?;
    if reply.is_empty() && cmd == "quit" {
        // The recorder can exit before the reply is flushed.
        Ok(String::new())
    } else if let Some(body) = reply.strip_prefix("ok") {
        Ok(body.strip_prefix(' ').unwrap_or(body).to_owned())
    } else if let Some(e) = reply.strip_prefix("error ") {
        bail!("{e}")
    } else {
        bail!("unexpected reply: {reply}")
    }
}

/// `status` reply: `key=value` lines.
pub fn status() -> Result<std::collections::HashMap<String, String>> {
    Ok(request("status")?
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect())
}

/// The abstract socket the library window listens on. Abstract names are
/// shared by every user on the machine, so scope it to ours.
pub fn library_name() -> String {
    use std::os::unix::fs::MetadataExt;
    let uid = std::fs::metadata("/proc/self").map_or(0, |m| m.uid());
    format!("clip-recorder-library-{uid}")
}

/// The library window's abstract socket address.
pub fn library_addr() -> Option<std::os::unix::net::SocketAddr> {
    use std::os::linux::net::SocketAddrExt;
    std::os::unix::net::SocketAddr::from_abstract_name(library_name().as_bytes()).ok()
}

/// Sends one line to the open library window. False when no library is open.
pub fn tell_library(msg: &str) -> bool {
    tell_library_at(&library_name(), msg)
}

/// `tell_library` against an arbitrary abstract socket name, for tests.
fn tell_library_at(name: &str, msg: &str) -> bool {
    use std::io::Write;
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixStream};
    let Some(addr) = SocketAddr::from_abstract_name(name.as_bytes()).ok() else {
        return false;
    };
    UnixStream::connect_addr(&addr)
        .and_then(|mut s| s.write_all(format!("{msg}\n").as_bytes()))
        .is_ok()
}

/// A line the library's control socket understands; an empty connection is
/// the old "just focus" poke.
#[derive(Debug, PartialEq)]
pub enum LibraryMsg {
    Focus(Option<String>),
    Quit,
}

pub fn parse_library_msg(line: &str) -> Option<LibraryMsg> {
    match line.trim().split_once(' ') {
        None => match line.trim() {
            "" | "focus" => Some(LibraryMsg::Focus(None)),
            "quit" => Some(LibraryMsg::Quit),
            _ => None,
        },
        Some(("focus", token)) if !token.is_empty() => {
            Some(LibraryMsg::Focus(Some(token.to_owned())))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixListener};

    #[test]
    fn parses_library_lines() {
        assert_eq!(parse_library_msg(""), Some(LibraryMsg::Focus(None)));
        assert_eq!(parse_library_msg("focus"), Some(LibraryMsg::Focus(None)));
        assert_eq!(
            parse_library_msg("focus kwin-12"),
            Some(LibraryMsg::Focus(Some("kwin-12".into())))
        );
        assert_eq!(parse_library_msg("quit"), Some(LibraryMsg::Quit));
        assert_eq!(parse_library_msg("focus "), Some(LibraryMsg::Focus(None)));
        assert_eq!(parse_library_msg("bogus"), None);
        assert_eq!(parse_library_msg("quit now"), None);
    }

    #[test]
    fn tell_library_at_round_trip() {
        let name = format!("clip-recorder-test-{}", std::process::id());
        let addr = SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
        let listener = UnixListener::bind_addr(&addr).unwrap();
        let got = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut line = String::new();
            s.read_to_string(&mut line).unwrap();
            line
        });
        assert!(tell_library_at(&name, "quit"));
        assert_eq!(got.join().unwrap(), "quit\n");
        assert!(!tell_library_at("clip-recorder-nobody-home", "focus"));
    }
}
