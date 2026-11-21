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
            let mut seen = HashMap::new();
            let own_pid = std::process::id();
            let reader = BufReader::new(stdout);
            for dump in
                serde_json::Deserializer::from_reader(reader).into_iter::<Vec<Value>>()
            {
                if !alive.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(dump) = dump else { break };
                for obj in &dump {
                    handle_object(obj, &mut seen, own_pid, &on_event);
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

/// One dump entry: `info: null` removes a tracked node; a node whose props we
/// see for the first time is classified and, for an app audio stream, added.
fn handle_object(
    obj: &Value,
    seen: &mut HashMap<u32, AppStream>,
    own_pid: u32,
    on_event: &impl Fn(StreamEvent),
) {
    let Some(id) = obj.get("id").and_then(|v| v.as_u64()).map(|v| v as u32) else {
        return;
    };
    let info = obj.get("info");
    if info.is_none() || info.is_some_and(|v| v.is_null()) {
        if seen.remove(&id).is_some() {
            on_event(StreamEvent::Removed(id));
        }
        return;
    }
    if seen.contains_key(&id) {
        return;
    }
    if obj.get("type").and_then(|v| v.as_str()) != Some("PipeWire:Interface:Node") {
        return;
    }
    let props = &info.unwrap()["props"];
    let Some(props) = props.as_object() else { return };
    if props
        .get("media.class")
        .and_then(|v| v.as_str())
        != Some("Stream/Output/Audio")
    {
        return;
    }
    let env = props
        .get("application.process.id")
        .and_then(|v| v.as_str())
        .and_then(|pid| std::fs::read(format!("/proc/{pid}/environ")).ok())
        .map(|raw| String::from_utf8_lossy(&raw).into_owned())
        .unwrap_or_default();
    let Some(category) = classify(props, &env, own_pid) else {
        return;
    };
    let serial = props
        .get("object.serial")
        .map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .unwrap_or_else(|| id.to_string());
    let name = props
        .get("application.name")
        .or_else(|| props.get("application.process.binary"))
        .and_then(|v| v.as_str())
        .unwrap_or("app")
        .to_string();
    let stream = AppStream {
        id,
        serial,
        name,
        category,
    };
    seen.insert(id, stream.clone());
    on_event(StreamEvent::Added(stream));
}

/// Which track an app's output belongs to; `None` for streams that must never
/// be recorded (our own recorder, library player and save chime).
fn classify(
    props: &serde_json::Map<String, Value>,
    env: &str,
    own_pid: u32,
) -> Option<Category> {
    let get = |key: &str| props.get(key).and_then(|v| v.as_str()).unwrap_or("");
    if get("application.process.id").parse::<u32>().ok() == Some(own_pid) {
        return None;
    }
    let binary = get("application.process.binary");
    let binary_l = binary.to_lowercase();
    let app = get("application.name");
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
        let mut seen = HashMap::new();
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
        handle_object(&add, &mut seen, 1, &emit);
        let update = json!({
            "id": 55, "type": "PipeWire:Interface:Node",
            "info": {"props": {"media.class": "Stream/Output/Audio"}},
        });
        handle_object(&update, &mut seen, 1, &emit);
        let remove = json!({"id": 55, "type": "PipeWire:Interface:Node", "info": null});
        handle_object(&remove, &mut seen, 1, &emit);
        handle_object(&remove, &mut seen, 1, &emit);
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 2);
        let StreamEvent::Added(s) = &events[0] else {
            panic!("expected Added");
        };
        assert_eq!(s.serial, "1234");
        assert_eq!(s.category, Category::Other);
        assert!(matches!(events[1], StreamEvent::Removed(55)));
    }
}
