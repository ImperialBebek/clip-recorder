//! KDE Plasma: record one window through KWin's own screencast protocol
//! (`zkde_screencast_unstable_v1`) instead of the portal. That is what lets the
//! recorder follow the game with no picker at all: the window list (with each
//! window's PID and app id) tells which window is the game, and KWin hands back
//! a PipeWire node for just that window.
//!
//! The Wayland connection lives on its own thread; the rest of the program talks
//! to it through a shared window list and a command channel.

use std::collections::BTreeMap;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tokio::sync::oneshot;
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols_plasma::plasma_window_management::client::org_kde_plasma_window::{
    self as win, OrgKdePlasmaWindow,
};
use wayland_protocols_plasma::plasma_window_management::client::org_kde_plasma_window_management::{
    self as wm, OrgKdePlasmaWindowManagement,
};
use wayland_protocols_plasma::screencast::v1::client::zkde_screencast_stream_unstable_v1::{
    self as st, ZkdeScreencastStreamUnstableV1,
};
use wayland_protocols_plasma::screencast::v1::client::zkde_screencast_unstable_v1::ZkdeScreencastUnstableV1;

/// `zkde_screencast_unstable_v1.pointer`: draw the cursor into the stream.
const POINTER_EMBEDDED: u32 = 2;
/// `org_kde_plasma_window_management.state.active`.
const STATE_ACTIVE: u32 = 1;

/// A window as KWin reports it.
#[derive(Clone, Debug, Default)]
pub struct Window {
    pub uuid: String,
    pub pid: u32,
    pub app_id: String,
    pub title: String,
    pub active: bool,
}

enum Cmd {
    Stream {
        uuid: String,
        reply: oneshot::Sender<Result<Stream>>,
    },
    Close(u64),
}

/// A window being streamed. Dropping it ends the stream.
pub struct Stream {
    /// PipeWire node to record, on the default PipeWire connection.
    pub node: u32,
    id: u64,
    closed: Arc<AtomicBool>,
    cmd: mpsc::Sender<Cmd>,
}

impl Stream {
    /// KWin ended the stream, normally because the window went away.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let _ = self.cmd.send(Cmd::Close(self.id));
    }
}

#[derive(Default)]
struct Shared {
    windows: Vec<Window>,
}

/// Handle to the KWin connection.
pub struct Kwin {
    shared: Arc<Mutex<Shared>>,
    cmd: mpsc::Sender<Cmd>,
}

impl Kwin {
    /// Connects and binds KWin's window and screencast protocols; fails anywhere
    /// else (other compositors, X11) so the caller can use the portal.
    pub fn connect() -> Result<Self> {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let (cmd, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread_shared = shared.clone();
        let thread_cmd = cmd.clone();
        std::thread::Builder::new()
            .name("kwin".into())
            .spawn(move || {
                let mut worker = match Worker::new(thread_shared, thread_cmd) {
                    Ok(w) => {
                        let _ = ready_tx.send(Ok(()));
                        w
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                if let Err(e) = worker.run(rx) {
                    eprintln!("kwin: {e:#}");
                }
            })
            .context("cannot start the kwin thread")?;
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| anyhow!("KWin did not answer"))??;
        Ok(Self { shared, cmd })
    }

    /// The windows KWin currently shows, as of the last event.
    pub fn windows(&self) -> Vec<Window> {
        self.shared.lock().unwrap().windows.clone()
    }

    /// Asks KWin to stream a window. Resolves once the PipeWire node exists.
    pub fn stream(&self, uuid: &str) -> impl std::future::Future<Output = Result<Stream>> {
        let (reply, rx) = oneshot::channel();
        let sent = self.cmd.send(Cmd::Stream {
            uuid: uuid.to_owned(),
            reply,
        });
        async move {
            sent.map_err(|_| anyhow!("the KWin connection is gone"))?;
            tokio::time::timeout(Duration::from_secs(5), rx)
                .await
                .map_err(|_| anyhow!("KWin did not start the stream"))?
                .map_err(|_| anyhow!("the KWin connection is gone"))?
        }
    }
}

struct Entry {
    info: Window,
    _obj: OrgKdePlasmaWindow,
}

struct PendingStream {
    _proxy: ZkdeScreencastStreamUnstableV1,
    reply: Option<oneshot::Sender<Result<Stream>>>,
    closed: Arc<AtomicBool>,
}

struct State {
    wins: BTreeMap<String, Entry>,
    streams: BTreeMap<u64, PendingStream>,
    dirty: bool,
    cmd: mpsc::Sender<Cmd>,
}

struct Worker {
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
    wm: OrgKdePlasmaWindowManagement,
    screencast: ZkdeScreencastUnstableV1,
    shared: Arc<Mutex<Shared>>,
    next_id: AtomicU64,
}

impl Worker {
    fn new(shared: Arc<Mutex<Shared>>, cmd: mpsc::Sender<Cmd>) -> Result<Self> {
        let conn = Connection::connect_to_env().context("no Wayland session")?;
        let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
        let qh = queue.handle();
        let wm = globals
            .bind(&qh, 13..=18, ())
            .map_err(|e| anyhow!("KWin window management not available: {e}"))?;
        let screencast = globals
            .bind(&qh, 1..=4, ())
            .map_err(|e| anyhow!("KWin screencast not available: {e}"))?;
        let mut state = State {
            wins: BTreeMap::new(),
            streams: BTreeMap::new(),
            dirty: true,
            cmd,
        };
        // Windows arrive as events after the bind, each with details of its own.
        for _ in 0..3 {
            queue.roundtrip(&mut state)?;
        }
        let mut w = Self {
            conn,
            queue,
            state,
            wm,
            screencast,
            shared,
            next_id: AtomicU64::new(1),
        };
        w.publish();
        Ok(w)
    }

    fn publish(&mut self) {
        if self.state.dirty {
            self.state.dirty = false;
            self.shared.lock().unwrap().windows =
                self.state.wins.values().map(|e| e.info.clone()).collect();
        }
    }

    fn run(&mut self, rx: mpsc::Receiver<Cmd>) -> Result<()> {
        let _ = &self.wm;
        loop {
            loop {
                match rx.try_recv() {
                    Ok(Cmd::Stream { uuid, reply }) => {
                        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
                        let qh = self.queue.handle();
                        let proxy = self
                            .screencast
                            .stream_window(uuid, POINTER_EMBEDDED, &qh, id);
                        self.state.streams.insert(
                            id,
                            PendingStream {
                                _proxy: proxy,
                                reply: Some(reply),
                                closed: Arc::new(AtomicBool::new(false)),
                            },
                        );
                    }
                    Ok(Cmd::Close(id)) => {
                        if let Some(s) = self.state.streams.remove(&id) {
                            s._proxy.close();
                        }
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    // The program dropped its handle: nothing left to serve.
                    Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
                }
            }
            self.queue.dispatch_pending(&mut self.state)?;
            self.conn.flush()?;
            if let Some(guard) = self.queue.prepare_read() {
                let mut p = libc::pollfd {
                    fd: guard.connection_fd().as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: one valid pollfd.
                let n = unsafe { libc::poll(&mut p, 1, 100) };
                if n > 0 {
                    guard.read()?;
                }
            }
            self.queue.dispatch_pending(&mut self.state)?;
            self.publish();
        }
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: wayland_client::protocol::wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<OrgKdePlasmaWindowManagement, ()> for State {
    fn event(
        s: &mut Self,
        m: &OrgKdePlasmaWindowManagement,
        ev: wm::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wm::Event::WindowWithUuid { uuid, .. } = ev {
            let obj = m.get_window_by_uuid(uuid.clone(), qh, uuid.clone());
            s.wins.insert(
                uuid.clone(),
                Entry {
                    info: Window {
                        uuid,
                        ..Default::default()
                    },
                    _obj: obj,
                },
            );
            s.dirty = true;
        }
    }
    wayland_client::event_created_child!(State, OrgKdePlasmaWindowManagement, []);
}

impl Dispatch<OrgKdePlasmaWindow, String> for State {
    fn event(
        s: &mut Self,
        _: &OrgKdePlasmaWindow,
        ev: win::Event,
        uuid: &String,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let win::Event::Unmapped = ev {
            s.wins.remove(uuid);
            s.dirty = true;
            return;
        }
        let Some(e) = s.wins.get_mut(uuid) else {
            return;
        };
        match ev {
            win::Event::PidChanged { pid } => e.info.pid = pid,
            win::Event::AppIdChanged { app_id } => e.info.app_id = app_id,
            win::Event::TitleChanged { title } => e.info.title = title,
            win::Event::StateChanged { flags } => e.info.active = flags & STATE_ACTIVE != 0,
            _ => return,
        }
        s.dirty = true;
    }
}

impl Dispatch<ZkdeScreencastUnstableV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZkdeScreencastUnstableV1,
        _: <ZkdeScreencastUnstableV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZkdeScreencastStreamUnstableV1, u64> for State {
    fn event(
        s: &mut Self,
        _: &ZkdeScreencastStreamUnstableV1,
        ev: st::Event,
        id: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(p) = s.streams.get_mut(id) else {
            return;
        };
        match ev {
            st::Event::Created { node } => {
                if let Some(reply) = p.reply.take() {
                    let _ = reply.send(Ok(Stream {
                        node,
                        id: *id,
                        closed: p.closed.clone(),
                        cmd: s.cmd.clone(),
                    }));
                }
            }
            st::Event::Failed { error } => {
                if let Some(reply) = p.reply.take() {
                    let _ = reply.send(Err(anyhow!("KWin refused the stream: {error}")));
                }
                p.closed.store(true, Ordering::SeqCst);
            }
            st::Event::Closed => p.closed.store(true, Ordering::SeqCst),
            _ => {}
        }
    }
}
