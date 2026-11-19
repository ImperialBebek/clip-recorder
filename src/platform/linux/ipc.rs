use std::io::{Read, Write};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{mpsc, oneshot};

pub type Reply = oneshot::Sender<Result<String, String>>;

pub fn socket_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    dir.join("clip-recorder.sock")
}

/// Accepts `save` commands from `clip-recorder save`, so the clip can also be
/// bound to a compositor/DE shortcut instead of the built-in hotkey.
pub fn serve(tx: mpsc::UnboundedSender<Option<Reply>>) -> Result<()> {
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
                let reply = match line.trim() {
                    "save" => {
                        let (rtx, rrx) = oneshot::channel();
                        let _ = tx.send(Some(rtx));
                        match rrx.await {
                            Ok(Ok(path)) => format!("ok {path}\n"),
                            Ok(Err(e)) => format!("error {e}\n"),
                            Err(_) => "error recorder stopped\n".into(),
                        }
                    }
                    other => format!("error unknown command '{other}'\n"),
                };
                let _ = write.write_all(reply.as_bytes()).await;
            });
        }
    });
    Ok(())
}

pub fn send_save() -> Result<String> {
    let path = socket_path();
    let mut stream = std::os::unix::net::UnixStream::connect(&path).with_context(|| {
        format!(
            "clip-recorder is not running (no socket at {})",
            path.display()
        )
    })?;
    stream.write_all(b"save\n")?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply)?;
    match reply.trim().split_once(' ') {
        Some(("ok", path)) => Ok(path.to_owned()),
        Some(("error", e)) => bail!("{e}"),
        _ => bail!("unexpected reply: {reply}"),
    }
}
