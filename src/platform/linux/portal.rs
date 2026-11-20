use std::os::fd::OwnedFd;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use ashpd::desktop::{PersistMode, Session};

/// An active ScreenCast portal session. Dropping it ends the stream.
pub struct PortalStream {
    pub fd: OwnedFd,
    pub node: u32,
    _session: Session<Screencast>,
    _proxy: Screencast,
}

fn token_path() -> PathBuf {
    super::state_dir().join("restore_token")
}

/// Whether a screen was picked before, so `open(false)` restores it without
/// showing the picker (unless the desktop has revoked it).
pub fn has_token() -> bool {
    std::fs::read_to_string(token_path()).is_ok_and(|t| !t.trim().is_empty())
}

/// The user closed the picker without choosing a screen.
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("screen selection was cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// Asks the desktop portal for a screen. The choice is remembered with a
/// restore token, so the picker only shows on first run (or with --reselect).
/// Only monitors are offered: a window would vanish when the game restarts
/// and the token would be worthless for the next game.
pub async fn open(reselect: bool) -> Result<PortalStream> {
    let proxy = Screencast::new()
        .await
        .context("ScreenCast portal not available; is xdg-desktop-portal (plus a backend for your desktop) running?")?;
    let token = if reselect {
        None
    } else {
        std::fs::read_to_string(token_path())
            .ok()
            .map(|t| t.trim().to_owned())
    };

    let session = proxy.create_session(Default::default()).await?;
    proxy
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(CursorMode::EMBEDDED)
                .set_sources(SourceType::MONITOR)
                .set_multiple(false)
                .set_restore_token(token.as_deref().filter(|t| !t.is_empty()))
                .set_persist_mode(PersistMode::ExplicitlyRevoked),
        )
        .await?;

    let streams = proxy
        .start(&session, None, Default::default())
        .await?
        .response()
        .map_err(|e| match e {
            ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled) => {
                anyhow::Error::new(Cancelled)
            }
            e => anyhow::Error::new(e).context("screen selection failed"),
        })?;
    let Some(stream) = streams.streams().first() else {
        bail!("portal returned no stream");
    };
    if let Some(token) = streams.restore_token() {
        let path = token_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, token);
    }
    let node = stream.pipe_wire_node_id();
    let fd = proxy
        .open_pipe_wire_remote(&session, Default::default())
        .await?;
    Ok(PortalStream {
        fd,
        node,
        _session: session,
        _proxy: proxy,
    })
}
