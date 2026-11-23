//! Small probes of the host: the running game, the GPU name, PipeWire version.

use std::path::PathBuf;

/// Name of a running game, if one is recognisable. Wayland has no portable
/// "focused window" API, so this looks for launchers' markers instead: the
/// `SteamAppId` Steam sets for every game (Proton included), Lutris/Heroic
/// variables, and Wine or gamescope processes.
pub fn running_game() -> Option<String> {
    let mut fallback = None;
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let dir = entry.path();
        let Ok(env) = std::fs::read(dir.join("environ")) else {
            continue;
        };
        for var in env.split(|&b| b == 0) {
            let var = String::from_utf8_lossy(var);
            if let Some(id) = var.strip_prefix("SteamAppId=") {
                if id != "0" && !id.is_empty() {
                    return Some(steam_name(id).unwrap_or_else(|| format!("Steam app {id}")));
                }
            } else if let Some(n) = var
                .strip_prefix("LUTRIS_GAME_NAME=")
                .or_else(|| var.strip_prefix("HEROIC_APP_NAME="))
            {
                fallback.get_or_insert_with(|| n.to_owned());
            }
        }
        if fallback.is_none() {
            let comm = std::fs::read_to_string(dir.join("comm")).unwrap_or_default();
            let comm = comm.trim();
            if comm == "gamescope" || comm.ends_with(".exe") {
                fallback = Some(comm.trim_end_matches(".exe").to_owned());
            }
        }
    }
    fallback
}

/// The game a window belongs to, from its app id and owning process: Steam
/// (`steam_app_<id>`, Proton included), Lutris, Heroic, or a Wine executable.
/// Launcher and client windows (Steam itself, Lutris' UI) are not games.
pub fn game_of_window(app_id: &str, pid: u32) -> Option<String> {
    // The Steam client is not a game; a window without an app id still can be
    // one, judged by its process below.
    if app_id == "steam" {
        return None;
    }
    if let Some(id) = app_id.strip_prefix("steam_app_") {
        return Some(steam_name(id).unwrap_or_else(|| format!("Steam app {id}")));
    }
    if let Ok(env) = std::fs::read(format!("/proc/{pid}/environ")) {
        let mut named = None;
        for var in env.split(|&b| b == 0) {
            let var = String::from_utf8_lossy(var);
            if let Some(id) = var
                .strip_prefix("SteamAppId")
                .and_then(|v| v.strip_prefix('='))
            {
                if id != "0" && !id.is_empty() {
                    return Some(steam_name(id).unwrap_or_else(|| format!("Steam app {id}")));
                }
            } else if let Some(n) = var
                .strip_prefix("LUTRIS_GAME_NAME=")
                .or_else(|| var.strip_prefix("HEROIC_APP_NAME="))
            {
                named.get_or_insert_with(|| n.to_owned());
            }
        }
        if named.is_some() {
            return named;
        }
    }
    let exe = app_id.strip_suffix(".exe").map(str::to_owned).or_else(|| {
        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
        comm.trim().strip_suffix(".exe").map(str::to_owned)
    });
    exe
}

pub(crate) fn steam_roots() -> Vec<PathBuf> {
    let home = crate::home();
    let mut roots = vec![
        home.join(".local/share/Steam"),
        home.join(".steam/steam"),
        home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam"),
    ];
    // Extra library folders listed in libraryfolders.vdf.
    for r in roots.clone() {
        if let Ok(vdf) = std::fs::read_to_string(r.join("steamapps/libraryfolders.vdf")) {
            for line in vdf.lines() {
                let t = line.trim();
                if let Some(rest) = t.strip_prefix("\"path\"") {
                    roots.push(PathBuf::from(rest.trim().trim_matches('"')));
                }
            }
        }
    }
    roots
}

fn steam_name(id: &str) -> Option<String> {
    for root in steam_roots() {
        let acf = root.join(format!("steamapps/appmanifest_{id}.acf"));
        if let Ok(text) = std::fs::read_to_string(acf) {
            for line in text.lines() {
                if let Some(rest) = line.trim().strip_prefix("\"name\"") {
                    return Some(rest.trim().trim_matches('"').to_owned());
                }
            }
        }
    }
    None
}

/// "AMD Radeon RX 7700 XT / 7800 XT" for the first GPU, from pci.ids.
pub fn gpu_name() -> Option<String> {
    let mut cards: Vec<PathBuf> = std::fs::read_dir("/sys/class/drm")
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let n = p
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            n.starts_with("card") && !n.contains('-')
        })
        .collect();
    cards.sort();
    let card = cards.first()?;
    let read = |f: &str| {
        std::fs::read_to_string(card.join("device").join(f))
            .ok()
            .map(|s| s.trim().trim_start_matches("0x").to_lowercase())
    };
    let (vendor, device) = (read("vendor")?, read("device")?);
    let short = match vendor.as_str() {
        "1002" => "AMD",
        "10de" => "NVIDIA",
        "8086" => "Intel",
        _ => "",
    };
    let ids = ["/usr/share/hwdata/pci.ids", "/usr/share/misc/pci.ids"]
        .iter()
        .find_map(|p| std::fs::read_to_string(p).ok());
    let model = ids.and_then(|ids| {
        let mut in_vendor = false;
        for line in ids.lines() {
            if !line.starts_with('\t') {
                in_vendor = line.starts_with(&vendor);
            } else if in_vendor && !line.starts_with("\t\t") {
                let l = line.trim_start();
                if l.starts_with(&device) {
                    let name = l[device.len()..].trim();
                    return Some(match (name.find('['), name.rfind(']')) {
                        (Some(a), Some(b)) if b > a => name[a + 1..b].to_owned(),
                        _ => name.to_owned(),
                    });
                }
            }
        }
        None
    });
    match (short, model) {
        ("", Some(m)) => Some(m),
        (v, Some(m)) => Some(format!("{v} {m}")),
        ("", None) => None,
        (v, None) => Some(format!("{v} GPU")),
    }
}

pub fn session_type() -> &'static str {
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        "Wayland"
    } else {
        "X11"
    }
}
