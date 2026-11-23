//! Display names and icons for the games the clips came from: Steam
//! appmanifests first (they carry the real name and an app id with cached
//! art), then freedesktop .desktop files for shortcuts and non-Steam games.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub struct GameInfo {
    pub name: Option<String>,
    pub icon: Option<PathBuf>,
}

const ICON_SIZES: [&str; 6] = ["256x256", "128x128", "96x96", "64x64", "48x48", "32x32"];

/// Resolved names and icons persist under `state_dir()/games`: a game that
/// gets uninstalled takes its Steam manifest and icons with it, and this is
/// the only way clips keep their real name afterwards.
fn store_dir() -> PathBuf {
    crate::platform::state_dir().join("games")
}

/// Slugs come from `clips::slug` so they should already be filename-safe;
/// anything that isn't never touches the store (no path tricks).
fn slug_safe(slug: &str) -> bool {
    !slug.is_empty() && !slug.starts_with('.') && !slug.contains('/')
}

/// Resolves each slug (as produced by `clips::slug`) to a display name and
/// an icon path, backed by the persistent store. Slugs nothing matched map
/// to an empty `GameInfo`.
pub fn lookup(slugs: &[String]) -> HashMap<String, GameInfo> {
    lookup_in(slugs, &store_dir(), live_lookup)
}

/// The persisted icon path for a slug, if the store already holds one.
/// Reads only the cache dir: callers that can't afford a live scan (the
/// OSD toast) use this instead of `lookup`.
pub fn cached_icon(slug: &str) -> Option<PathBuf> {
    slug_safe(slug)
        .then(|| stored_icon(&store_dir(), slug))
        .flatten()
}

/// (name, icon path in the store) already persisted for a slug.
fn stored(store: &Path, slug: &str) -> (Option<String>, Option<PathBuf>) {
    let name = std::fs::read_to_string(store.join(format!("{slug}.name"))).ok();
    let icon = stored_icon(store, slug);
    (name, icon)
}

/// `<slug>.icon.<ext>` in the store; the extension varies by what was cached.
fn stored_icon(store: &Path, slug: &str) -> Option<PathBuf> {
    let prefix = format!("{slug}.icon.");
    std::fs::read_dir(store)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&prefix) && !n.ends_with(".tmp"))
        })
}

/// Writes `path` via a same-dir temp file + rename; creates the store dir.
fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_file_name(format!(
        "{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)
}

/// Copies `src` to `<slug>.icon.<ext>` in the store via temp + rename;
/// older icon variants for the slug are removed first.
fn cache_icon(store: &Path, slug: &str, src: &Path) -> std::io::Result<PathBuf> {
    let ext = src.extension().and_then(|e| e.to_str()).unwrap_or("png");
    std::fs::create_dir_all(store)?;
    let dst = store.join(format!("{slug}.icon.{ext}"));
    if let Some(old) = stored_icon(store, slug) {
        if old != dst {
            let _ = std::fs::remove_file(old);
        }
    }
    let tmp = store.join(format!("{slug}.icon.{ext}.tmp"));
    std::fs::copy(src, &tmp)?;
    std::fs::rename(&tmp, &dst)?;
    Ok(dst)
}

/// The two-phase lookup: whatever is already in the store wins and skips
/// the live scan entirely (a fully-cached slug never touches the Steam
/// library), the rest resolve live and persist what they found.
fn lookup_in(
    slugs: &[String],
    store: &Path,
    live: impl Fn(&[String]) -> HashMap<String, GameInfo>,
) -> HashMap<String, GameInfo> {
    let mut out: HashMap<String, GameInfo> = HashMap::new();
    let mut want = Vec::new();
    for slug in slugs {
        if slug_safe(slug) {
            let (name, icon) = stored(store, slug);
            // A fully cached slug skips the live scan entirely.
            let full = name.is_some() && icon.is_some();
            out.insert(slug.clone(), GameInfo { name, icon });
            if !full {
                want.push(slug.clone());
            }
        } else {
            want.push(slug.clone());
        }
    }
    let found = if want.is_empty() {
        HashMap::new()
    } else {
        live(&want)
    };
    for slug in want {
        let prev = out.remove(&slug).unwrap_or(GameInfo {
            name: None,
            icon: None,
        });
        let found_i = found.get(&slug);
        let stored_had_icon = prev.icon.is_some();
        let mut info = GameInfo {
            name: found_i.and_then(|i| i.name.clone()).or(prev.name),
            icon: found_i.and_then(|i| i.icon.clone()).or(prev.icon),
        };
        // Persist what live found so an uninstall can't take it away. A
        // store error only costs the cache, never the lookup result.
        if slug_safe(&slug) {
            if let Some(name) = found_i.and_then(|i| i.name.clone()) {
                let path = store.join(format!("{slug}.name"));
                if std::fs::read_to_string(&path).ok().as_deref() != Some(name.as_str()) {
                    if let Err(e) = write_atomic(&path, name.as_bytes()) {
                        eprintln!("game name cache {slug}: {e:#}");
                    }
                }
            }
            if !stored_had_icon {
                if let Some(src) = found_i.and_then(|i| i.icon.clone()) {
                    match cache_icon(store, &slug, &src) {
                        Ok(dst) => info.icon = Some(dst),
                        Err(e) => eprintln!("game icon cache {slug}: {e:#}"),
                    }
                }
            }
        }
        out.insert(slug, info);
    }
    out
}

/// Steam manifests first, .desktop files after — the current machine truth.
fn live_lookup(slugs: &[String]) -> HashMap<String, GameInfo> {
    let mut out: HashMap<String, GameInfo> = HashMap::new();
    let roots = deduped_roots();
    let steam = steam_games(&roots);
    for slug in slugs {
        if let Some((name, appid)) = steam.get(slug) {
            out.insert(
                slug.clone(),
                GameInfo {
                    name: Some(name.clone()),
                    icon: steam_icon(appid, &roots),
                },
            );
        } else if let Some(info) = desktop_match(slug) {
            out.insert(slug.clone(), info);
        } else {
            out.insert(
                slug.clone(),
                GameInfo {
                    name: None,
                    icon: None,
                },
            );
        }
    }
    out
}

/// Steam roots with symlinked duplicates removed (`~/.steam/steam` and
/// `~/.local/share/Steam` are usually the same directory).
fn deduped_roots() -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    crate::sysinfo::steam_roots()
        .into_iter()
        .filter(|r| seen.insert(r.canonicalize().unwrap_or_else(|_| r.clone())))
        .collect()
}

/// slug -> (name, appid) for every Steam appmanifest that matches.
fn steam_games(roots: &[PathBuf]) -> HashMap<String, (String, String)> {
    let mut found = HashMap::new();
    for root in roots {
        let Ok(rd) = std::fs::read_dir(root.join("steamapps")) else {
            continue;
        };
        for e in rd.flatten() {
            let Some(id) = e
                .file_name()
                .to_str()
                .and_then(|n| n.strip_prefix("appmanifest_"))
                .and_then(|n| n.strip_suffix(".acf"))
                .map(str::to_owned)
            else {
                continue;
            };
            let Ok(text) = std::fs::read_to_string(e.path()) else {
                continue;
            };
            let Some(name) = acf_value(&text, "name") else {
                continue;
            };
            found.entry(crate::clips::slug(&name)).or_insert((name, id));
        }
    }
    found
}

fn acf_value(text: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\"");
    for line in text.lines() {
        if let Some(rest) = line.trim().strip_prefix(pat.as_str()) {
            let v = rest.trim().trim_matches('"');
            return (!v.is_empty()).then(|| v.to_owned());
        }
    }
    None
}

/// hicolor `steam_icon_<id>.png` first, then the icon Steam itself caches.
fn steam_icon(appid: &str, roots: &[PathBuf]) -> Option<PathBuf> {
    theme_icon(&format!("steam_icon_{appid}")).or_else(|| {
        roots.iter().find_map(|root| {
            let dir = root.join(format!("appcache/librarycache/{appid}"));
            // The one top-level .jpg in an appid dir is the small game icon;
            // subdirectories hold the bigger artwork.
            std::fs::read_dir(dir).ok().and_then(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .find(|p| p.is_file() && p.extension().is_some_and(|x| x == "jpg"))
            })
        })
    })
}

fn desktop_dirs() -> Vec<PathBuf> {
    let home = crate::home();
    vec![
        home.join(".local/share/applications"),
        PathBuf::from("/usr/share/applications"),
        home.join(".local/share/flatpak/exports/share/applications"),
        PathBuf::from("/var/lib/flatpak/exports/share/applications"),
    ]
}

/// The first .desktop entry whose `Name=` slugs to `slug`.
fn desktop_match(slug: &str) -> Option<GameInfo> {
    for dir in desktop_dirs() {
        let Ok(rd) = std::fs::read_dir(dir) else {
            continue;
        };
        for e in rd.flatten() {
            let path = e.path();
            if path.extension().is_none_or(|x| x != "desktop") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Some((name, icon)) = desktop_entry(&text) else {
                continue;
            };
            if crate::clips::slug(&name) == slug {
                return Some(GameInfo {
                    name: Some(name),
                    icon: resolve_desktop_icon(&icon),
                });
            }
        }
    }
    None
}

/// (Name, Icon) from the `[Desktop Entry]` group only; localized keys like
/// `Name[de]=` don't count.
fn desktop_entry(text: &str) -> Option<(String, String)> {
    let mut in_entry = false;
    let (mut name, mut icon) = (None, String::new());
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            if in_entry {
                break;
            }
            in_entry = t == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        if let Some(v) = t.strip_prefix("Name=") {
            name.get_or_insert_with(|| v.trim().to_owned());
        }
        if let Some(v) = t.strip_prefix("Icon=") {
            if icon.is_empty() {
                icon = v.trim().to_owned();
            }
        }
    }
    name.map(|n| (n, icon))
}

fn resolve_desktop_icon(icon: &str) -> Option<PathBuf> {
    if icon.is_empty() {
        return None;
    }
    let p = Path::new(icon);
    if p.is_absolute() {
        return p.exists().then(|| p.to_path_buf());
    }
    let stem = [".png", ".svg", ".xpm"]
        .iter()
        .find_map(|ext| icon.strip_suffix(ext))
        .unwrap_or(icon);
    theme_icon(stem)
}

fn hicolor_dirs() -> Vec<PathBuf> {
    vec![
        crate::home().join(".local/share/icons/hicolor"),
        PathBuf::from("/usr/share/icons/hicolor"),
    ]
}

/// A theme icon by name: biggest hicolor png first, then a scalable svg,
/// then the legacy pixmaps dir.
fn theme_icon(name: &str) -> Option<PathBuf> {
    for base in hicolor_dirs() {
        for size in ICON_SIZES {
            let p = base.join(size).join("apps").join(format!("{name}.png"));
            if p.exists() {
                return Some(p);
            }
        }
        let p = base.join("scalable/apps").join(format!("{name}.svg"));
        if p.exists() {
            return Some(p);
        }
    }
    for ext in ["png", "svg", "xpm"] {
        let p = PathBuf::from(format!("/usr/share/pixmaps/{name}.{ext}"));
        if p.exists() {
            return Some(p);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acf_names() {
        let acf = "\
\"AppState\"
{
\t\"appid\"\t\t\"3241660\"
\t\"name\"\t\t\"R.E.P.O.\"
}";
        assert_eq!(acf_value(acf, "appid").as_deref(), Some("3241660"));
        assert_eq!(acf_value(acf, "name").as_deref(), Some("R.E.P.O."));
        assert_eq!(acf_value(acf, "installdir"), None);
    }

    #[test]
    fn desktop_names_and_icons() {
        let desktop = "\
[Desktop Entry]
Name=Crimson Desert Enhanced
Name[de]=Crimson Desert DE
Icon=steam_icon_3321460
Exec=steam
[Something Else]
Name=Not it
";
        assert_eq!(
            desktop_entry(desktop),
            Some((
                "Crimson Desert Enhanced".into(),
                "steam_icon_3321460".into()
            ))
        );
        // No [Desktop Entry] group at all.
        assert_eq!(desktop_entry("[Other]\nName=x"), None);
    }

    fn tmp_store(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("clip-games-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("games")
    }

    #[test]
    fn caches_what_live_found() {
        let store = tmp_store("cache");
        let src = store.parent().unwrap().join("icon.png");
        std::fs::create_dir_all(src.parent().unwrap()).unwrap();
        std::fs::write(&src, b"png-bytes").unwrap();
        let live = |_: &[String]| {
            HashMap::from([(
                "Elden_Ring".to_owned(),
                GameInfo {
                    name: Some("Elden Ring".into()),
                    icon: Some(src.clone()),
                },
            )])
        };
        let out = lookup_in(&["Elden_Ring".into()], &store, live);
        let info = &out["Elden_Ring"];
        assert_eq!(info.name.as_deref(), Some("Elden Ring"));
        let icon = info.icon.as_ref().unwrap();
        assert!(icon.starts_with(&store), "icon should be the store copy");
        assert_eq!(std::fs::read(icon).unwrap(), b"png-bytes");
        assert_eq!(
            std::fs::read_to_string(store.join("Elden_Ring.name")).unwrap(),
            "Elden Ring"
        );
        std::fs::remove_dir_all(store.parent().unwrap()).ok();
    }

    #[test]
    fn store_survives_uninstall() {
        let store = tmp_store("gone");
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(store.join("R_E_P_O_.name"), "R.E.P.O.").unwrap();
        std::fs::write(store.join("R_E_P_O_.icon.jpg"), b"jpg-bytes").unwrap();
        let asked = std::cell::RefCell::new(Vec::new());
        let live = |slugs: &[String]| {
            asked.borrow_mut().extend(slugs.iter().cloned());
            HashMap::<String, GameInfo>::new()
        };
        let out = lookup_in(&["R_E_P_O_".into()], &store, live);
        let info = &out["R_E_P_O_"];
        assert_eq!(info.name.as_deref(), Some("R.E.P.O."));
        assert_eq!(
            info.icon.as_deref(),
            Some(store.join("R_E_P_O_.icon.jpg").as_path())
        );
        assert!(
            !asked.borrow().contains(&"R_E_P_O_".to_owned()),
            "a fully cached slug must not reach the live lookup"
        );
        std::fs::remove_dir_all(store.parent().unwrap()).ok();
    }

    #[test]
    fn partial_cache_keeps_asking_live() {
        let store = tmp_store("name");
        let live = |_: &[String]| {
            HashMap::from([(
                "Half_Life".to_owned(),
                GameInfo {
                    name: Some("Half-Life".into()),
                    icon: None,
                },
            )])
        };
        let out = lookup_in(&["Half_Life".into()], &store, live);
        assert_eq!(out["Half_Life"].name.as_deref(), Some("Half-Life"));
        assert_eq!(
            std::fs::read_to_string(store.join("Half_Life.name")).unwrap(),
            "Half-Life"
        );
        assert!(stored_icon(&store, "Half_Life").is_none());
        // With only the name cached, the next lookup still asks live.
        let asked = std::cell::RefCell::new(Vec::new());
        let live2 = |slugs: &[String]| {
            asked.borrow_mut().extend(slugs.iter().cloned());
            HashMap::<String, GameInfo>::new()
        };
        let out = lookup_in(&["Half_Life".into()], &store, live2);
        assert!(asked.borrow().contains(&"Half_Life".to_owned()));
        assert_eq!(out["Half_Life"].name.as_deref(), Some("Half-Life"));
        std::fs::remove_dir_all(store.parent().unwrap()).ok();
    }

    #[test]
    fn unsafe_slugs_skip_the_store() {
        let store = tmp_store("evil");
        let live = |slugs: &[String]| {
            slugs
                .iter()
                .map(|s| {
                    (
                        s.clone(),
                        GameInfo {
                            name: Some("x".into()),
                            icon: None,
                        },
                    )
                })
                .collect::<HashMap<String, GameInfo>>()
        };
        let out = lookup_in(&["../evil".into(), "".into()], &store, live);
        assert_eq!(out["../evil"].name.as_deref(), Some("x"));
        assert_eq!(out[""].name.as_deref(), Some("x"));
        let root = store.parent().unwrap();
        assert!(
            !root.join("evil.name").exists() && !store.exists(),
            "no files may be written for unsafe slugs"
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    #[ignore = "depends on this machine's Steam library"]
    fn looks_up_real_games() {
        let out = lookup(&[
            "Crimson_Desert_Enhanced".into(),
            "R_E_P_O_".into(),
            "No_Such_Game_9f8d".into(),
        ]);
        for slug in ["Crimson_Desert_Enhanced", "R_E_P_O_", "No_Such_Game_9f8d"] {
            let info = &out[slug];
            println!("{slug}: name={:?} icon={:?}", info.name, info.icon);
        }
    }
}
