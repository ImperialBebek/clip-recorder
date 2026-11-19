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
