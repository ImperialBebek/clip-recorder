//! PipeWire application stream watcher.
//!
//! `pw-dump --monitor` streams JSON arrays of changed objects. An object is
//! reported again whenever its properties change, and with `"info": null`
//! when it is removed. From that we derive which applications are currently
//! playing audio, so each category can get its own clip track.
use std::collections::HashMap;
use std::io::BufReader;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use anyhow::{Context, Result};
use serde_json::Value;

/// Which clip track an application's output belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    Game,
    Voice,
    Other,
}

impl Category {
    pub fn name(self) -> &'static str {
        match self {
            Self::Game => "game",
            Self::Voice => "voice",
            Self::Other => "other",
        }
    }
}

/// One application's output stream (a PipeWire node).
#[derive(Clone, Debug)]
pub struct AppStream {
    pub id: u32,
    /// `object.serial`, used as `pipewiresrc target-object`.
    pub serial: String,
    pub name: String,
    pub category: Category,
}

pub enum StreamEvent {
    Added(AppStream),
    Removed(u32),
}

/// The running `pw-dump` child and its reader thread; both are stopped on drop.
pub struct Watcher {
    child: Child,
    alive: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Relaxed);
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Follows PipeWire's application output streams; events arrive on a worker
/// thread until the Watcher is dropped.
pub fn watch(on_event: impl Fn(StreamEvent) + Send + 'static) -> Result<Watcher> {
    let mut child = Command::new("pw-dump")
        .args(["--monitor", "--no-colors"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("pw-dump could not be started; install PipeWire's tools for per-app audio")?;
    let stdout = child.stdout.take().unwrap();
    let alive = Arc::new(AtomicBool::new(true));
    let thread = std::thread::spawn({
        let alive = alive.clone();
        move || {
            let mut seen = Seen::default();
            let own_pid = std::process::id();
            let env_of = |pid: &str| {
                std::fs::read(format!("/proc/{pid}/environ"))
                    .map(|raw| String::from_utf8_lossy(&raw).into_owned())
                    .unwrap_or_default()
            };
            let reader = BufReader::new(stdout);
            for dump in serde_json::Deserializer::from_reader(reader).into_iter::<Vec<Value>>() {
                if !alive.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(dump) = dump else { break };
                for obj in &dump {
                    handle_object(obj, &mut seen, own_pid, &env_of, &on_event);
                }
            }
        }
    });
    Ok(Watcher {
        child,
        alive,
        thread: Some(thread),
    })
}

/// What the monitor thread knows about the objects it has seen so far.
#[derive(Default)]
struct Seen {
    /// Streams already reported with `StreamEvent::Added`, by node id.
    streams: HashMap<u32, AppStream>,
    /// Client objects' props, by client id; updated on every re-report.
    clients: HashMap<u32, serde_json::Map<String, Value>>,
    /// Audio nodes parked until their client object shows up, by client id.
    pending: HashMap<u32, Vec<u32>>,
    /// Props of the parked nodes, by node id.
    nodes: HashMap<u32, serde_json::Map<String, Value>>,
}

/// A prop value as a string; PipeWire reports numbers for ids and serials.
fn prop(props: &serde_json::Map<String, Value>, key: &str) -> String {
    match props.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

/// One dump entry: `info: null` removes a tracked object; a node whose props
/// we see for the first time is classified and, for an app audio stream,
/// added. Nodes carry no process info under Proton's native PipeWire driver;
/// it lives on their Client object, so a node's effective props are its
/// client's overlaid with its own (the node wins on conflicts).
fn handle_object(
    obj: &Value,
    seen: &mut Seen,
    own_pid: u32,
    env_of: &impl Fn(&str) -> String,
    on_event: &impl Fn(StreamEvent),
) {
    let Some(id) = obj.get("id").and_then(|v| v.as_u64()).map(|v| v as u32) else {
        return;
    };
    let info = obj.get("info");
    if info.is_none() || info.is_some_and(|v| v.is_null()) {
        if seen.streams.remove(&id).is_some() {
            on_event(StreamEvent::Removed(id));
        }
        seen.clients.remove(&id);
        seen.nodes.remove(&id);
        for nodes in seen.pending.values_mut() {
            nodes.retain(|n| *n != id);
        }
        return;
    }
    match obj.get("type").and_then(|v| v.as_str()) {
        Some("PipeWire:Interface:Client") => {
            let Some(props) = info.unwrap()["props"].as_object().cloned() else {
                return;
            };
            seen.clients.insert(id, props);
            // Nodes parked for this client can be classified now.
            let Some(pending) = seen.pending.remove(&id) else {
                return;
            };
            for node_id in pending {
                let Some(props) = seen.nodes.remove(&node_id) else {
                    continue;
                };
                add_node(node_id, &props, seen, own_pid, env_of, on_event);
            }
        }
        Some("PipeWire:Interface:Node") => {
            if seen.streams.contains_key(&id) {
                return;
            }
            let Some(props) = info.unwrap()["props"].as_object() else {
                return;
            };
            if prop(props, "media.class") != "Stream/Output/Audio" {
                return;
            }
            let client = prop(props, "client.id").parse::<u32>().unwrap_or(0);
            if client != 0 && !seen.clients.contains_key(&client) {
                if !seen.nodes.contains_key(&id) {
                    seen.pending.entry(client).or_default().push(id);
                }
                seen.nodes.insert(id, props.clone());
                return;
            }
            seen.nodes.remove(&id);
            for nodes in seen.pending.values_mut() {
                nodes.retain(|n| *n != id);
            }
            add_node(id, props, seen, own_pid, env_of, on_event);
        }
        _ => {}
    }
}

/// Classifies one app audio node and emits `StreamEvent::Added` for it.
fn add_node(
    id: u32,
    node_props: &serde_json::Map<String, Value>,
    seen: &mut Seen,
    own_pid: u32,
    env_of: &impl Fn(&str) -> String,
    on_event: &impl Fn(StreamEvent),
) {
    let client = prop(node_props, "client.id").parse::<u32>().unwrap_or(0);
    let mut props = seen.clients.get(&client).cloned().unwrap_or_default();
    for (k, v) in node_props {
        props.insert(k.clone(), v.clone());
    }
    let pid = {
        let pid = prop(&props, "application.process.id");
        if pid.is_empty() {
            prop(&props, "pipewire.sec.pid")
        } else {
            pid
        }
    };
    let env = if !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()) {
        env_of(&pid)
    } else {
        String::new()
    };
    let Some(category) = classify(&props, &env, own_pid) else {
        return;
    };
    let serial = {
        let serial = prop(&props, "object.serial");
        if serial.is_empty() {
            id.to_string()
        } else {
            serial
        }
    };
    let name = {
        let name = prop(&props, "application.name");
        if name.is_empty() {
            let binary = prop(&props, "application.process.binary");
            if binary.is_empty() {
                "app".into()
            } else {
                binary
            }
        } else {
            name
        }
    };
    let stream = AppStream {
        id,
        serial,
        name,
        category,
    };
    seen.streams.insert(id, stream.clone());
    on_event(StreamEvent::Added(stream));
}

/// Which track an app's output belongs to; `None` for streams that must never
/// be recorded (our own recorder, library player and save chime).
fn classify(props: &serde_json::Map<String, Value>, env: &str, own_pid: u32) -> Option<Category> {
    let pid = prop(props, "application.process.id");
    let pid = if pid.is_empty() {
        prop(props, "pipewire.sec.pid")
    } else {
        pid
    };
    if pid.parse::<u32>().ok() == Some(own_pid) {
        return None;
    }
    let binary = prop(props, "application.process.binary");
    let binary_l = binary.to_lowercase();
    let app = prop(props, "application.name");
    let app_l = app.to_lowercase();
    if binary_l == "clip-recorder" || app_l == "clip-recorder" {
        return None;
    }
    let has_env = |key: &str| {
        env.split('\0')
            .filter_map(|e| e.strip_prefix(key))
            .any(|v| !v.is_empty() && v != "0")
    };
    if has_env("SteamAppId=")
        || has_env("STEAM_COMPAT_APP_ID=")
        || has_env("LUTRIS_GAME_NAME=")
        || has_env("HEROIC_APP_NAME=")
        || binary_l.starts_with("wine")
        || binary_l.ends_with(".exe")
        || app.ends_with(".exe")
        || binary == "gamescope"
    {
        return Some(Category::Game);
    }
    const VOICE: &[&str] = &[
        "discord",
        "discordcanary",
        "discordptb",
        "discord-canary",
        "discord-ptb",
        "vesktop",
        "webcord",
        "legcord",
        "armcord",
        "teamspeak",
        "teamspeak3",
        "ts3client_linux_amd64",
        "mumble",
        "zoom",
    ];
    if VOICE.contains(&binary_l.as_str())
        || VOICE.contains(&app_l.as_str())
        || app == "WEBRTC VoiceEngine"
    {
        return Some(Category::Voice);
    }
    Some(Category::Other)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    fn props(binary: &str, name: &str, pid: u32) -> serde_json::Map<String, Value> {
        json!({
            "application.process.binary": binary,
            "application.name": name,
            "application.process.id": pid.to_string(),
        })
        .as_object()
        .unwrap()
        .clone()
    }

    #[test]
    fn classifies_real_streams() {
        let deadlock = props("wine64-preloader", "deadlock.exe", 51443);
        let env = "PATH=/bin\0SteamAppId=1422450\0STEAM_COMPAT_APP_ID=1422450\0";
        assert_eq!(classify(&deadlock, env, 1), Some(Category::Game));
        let zen = props("zen", "Zen", 2739);
        assert_eq!(classify(&zen, "", 1), Some(Category::Other));
        let discord = props("Discord", "WEBRTC VoiceEngine", 4049);
        assert_eq!(classify(&discord, "", 1), Some(Category::Voice));
    }

    #[test]
    fn excludes_our_own_audio() {
        let own = props("clip-recorder", "clip-recorder", 1234);
        assert_eq!(classify(&own, "", 1234), None);
        // The library process shares the binary name even with a foreign pid.
        assert_eq!(classify(&own, "", 9999), None);
        let player = props("clip-recorder", "Audio", 555);
        assert_eq!(classify(&player, "", 9999), None);
    }

    #[test]
    fn classifies_more_apps() {
        let scope = props("gamescope", "gamescope-wl", 1);
        assert_eq!(classify(&scope, "", 9999), Some(Category::Game));
        let vesk = props("vesktop", "Vesktop", 1);
        assert_eq!(classify(&vesk, "", 9999), Some(Category::Voice));
        let heroic = props("foo", "Foo", 1);
        let env = "A=1\0HEROIC_APP_NAME=SomeGame\0";
        assert_eq!(classify(&heroic, env, 9999), Some(Category::Game));
        // A zero SteamAppId is not a game.
        let not_steam = props("foo", "Foo", 1);
        assert_eq!(
            classify(&not_steam, "SteamAppId=0\0", 9999),
            Some(Category::Other)
        );
    }

    #[test]
    fn parses_monitor_stream() {
        let events = Mutex::new(Vec::new());
        let emit = |ev: StreamEvent| events.lock().unwrap().push(ev);
        let add = json!({
            "id": 55, "type": "PipeWire:Interface:Node",
            "info": {"props": {
                "media.class": "Stream/Output/Audio",
                "application.process.binary": "zen",
                "application.name": "Zen",
                "application.process.id": "4000000000",
                "object.serial": "1234",
            }},
        });
        let mut seen = Seen::default();
        let no_env = |_: &str| String::new();
        handle_object(&add, &mut seen, 1, &no_env, &emit);
        let update = json!({
            "id": 55, "type": "PipeWire:Interface:Node",
            "info": {"props": {"media.class": "Stream/Output/Audio"}},
        });
        handle_object(&update, &mut seen, 1, &no_env, &emit);
        let remove = json!({"id": 55, "type": "PipeWire:Interface:Node", "info": null});
        handle_object(&remove, &mut seen, 1, &no_env, &emit);
        handle_object(&remove, &mut seen, 1, &no_env, &emit);
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 2);
        let StreamEvent::Added(s) = &events[0] else {
            panic!("expected Added");
        };
        assert_eq!(s.serial, "1234");
        assert_eq!(s.category, Category::Other);
        assert!(matches!(events[1], StreamEvent::Removed(55)));
    }

    // Crimson Desert under Proton's native PipeWire driver: the node has no
    // process props; they live on its Client object.
    fn crimson_client() -> Value {
        json!({
            "id": 72, "type": "PipeWire:Interface:Client",
            "info": {"props": {
                "application.process.binary": "wine64-preloader",
                "application.process.id": "199350",
                "pipewire.sec.pid": 199350,
                "application.name": "Crimson Desert",
            }},
        })
    }

    fn crimson_node() -> Value {
        json!({
            "id": 73, "type": "PipeWire:Interface:Node",
            "info": {"props": {
                "media.class": "Stream/Output/Audio",
                "application.name": "Crimson Desert",
                "client.id": "72",
                "object.serial": 900,
            }},
        })
    }

    fn steam_env(pid: &str) -> String {
        if pid == "199350" {
            "SteamAppId=3321460\0".into()
        } else {
            String::new()
        }
    }

    #[test]
    fn classifies_node_through_its_client() {
        for swap in [false, true] {
            let events = Mutex::new(Vec::new());
            let emit = |ev: StreamEvent| events.lock().unwrap().push(ev);
            let mut seen = Seen::default();
            let (client, node) = (crimson_client(), crimson_node());
            if swap {
                handle_object(&node, &mut seen, 1, &steam_env, &emit);
                handle_object(&client, &mut seen, 1, &steam_env, &emit);
            } else {
                handle_object(&client, &mut seen, 1, &steam_env, &emit);
                handle_object(&node, &mut seen, 1, &steam_env, &emit);
            }
            let events = events.lock().unwrap();
            assert_eq!(events.len(), 1, "swap={swap}");
            let StreamEvent::Added(s) = &events[0] else {
                panic!("expected Added");
            };
            assert_eq!(s.category, Category::Game);
            assert_eq!(s.name, "Crimson Desert");
            assert_eq!(s.serial, "900");
        }
    }

    #[test]
    fn pending_node_is_dropped_with_its_client() {
        let events = Mutex::new(Vec::new());
        let emit = |ev: StreamEvent| events.lock().unwrap().push(ev);
        let mut seen = Seen::default();
        handle_object(&crimson_node(), &mut seen, 1, &steam_env, &emit);
        let gone = json!({"id": 72, "type": "PipeWire:Interface:Client", "info": null});
        handle_object(&gone, &mut seen, 1, &steam_env, &emit);
        assert!(events.lock().unwrap().is_empty());
    }
}
