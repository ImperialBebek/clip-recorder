//! The clips folder: naming new clips and listing recent ones.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub struct Clip {
    pub path: PathBuf,
    pub title: String,
    /// The slug before the `_YYYY-MM-DD_` stamp: the grouping key that a
    /// Steam or .desktop name resolves back to ("clip" for unnamed clips).
    pub game: String,
    /// Written by the trim feature: a `_trim` segment after the timestamp.
    pub exported: bool,
    pub seconds: Option<f64>,
    pub bytes: u64,
    pub modified: SystemTime,
}

/// The file-name-safe form of a game name: `R.E.P.O.` -> `R_E_P_O_`.
pub fn slug(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// `Elden Ring` + now -> `Elden_Ring_2026-11-19_1452.mp4`, unique in `dir`.
pub fn new_path(dir: &Path, game: Option<&str>) -> PathBuf {
    let slug = slug(game.unwrap_or("clip"));
    let stamp = gstreamer::glib::DateTime::now_local()
        .and_then(|t| t.format("%Y-%m-%d_%H%M%S"))
        .map(|s| s.to_string())
        .unwrap_or_default();
    unique_path(dir, &format!("{slug}_{stamp}.mp4"))
}

/// `dir/name`, or `dir/stem-2.ext`, `-3`... if that already exists, so two
/// saves in the same second never overwrite each other.
pub fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let (stem, ext) = name.rsplit_once('.').unwrap_or((name, ""));
    let mut path = dir.join(name);
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("{stem}-{n}.{ext}"));
        n += 1;
    }
    path
}

/// Moves the finished file `tmp` to `path`, or to the next free `-N` name if
/// something took `path` meanwhile (e.g. a clip restored from the Trash). A
/// hard link never replaces an existing file, unlike `rename`.
pub fn publish(tmp: &Path, path: &Path) -> std::io::Result<PathBuf> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let mut dest = path.to_path_buf();
    loop {
        match std::fs::hard_link(tmp, &dest) {
            Ok(()) => {
                let _ = std::fs::remove_file(tmp);
                return Ok(dest);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                dest = unique_path(dir, &name);
            }
            // Filesystems without hard links (FAT, some FUSE mounts).
            Err(_) if !dest.exists() => {
                std::fs::rename(tmp, &dest)?;
                return Ok(dest);
            }
            Err(_) => dest = unique_path(dir, &name),
        }
    }
}

/// (grouping slug, is-a-trim-export) from a clip stem: the slug is
/// everything before the `_YYYY-MM-DD` segment, and a `_trim`/`_trim-N`
/// segment after the timestamp marks an exported clip.
fn game_of(stem: &str) -> (String, bool) {
    let parts: Vec<&str> = stem.split('_').collect();
    let is_trim = |p: &str| {
        p == "trim"
            || p.strip_prefix("trim-")
                .is_some_and(|n| n.parse::<u32>().is_ok())
    };
    let Some(i) = date_pos(&parts) else {
        // A clip renamed without its timestamp still gets `_trim` on export.
        return match parts.split_last() {
            Some((last, rest)) if !rest.is_empty() && is_trim(last) => (rest.join("_"), true),
            _ => (stem.to_owned(), false),
        };
    };
    (
        parts[..i].join("_"),
        parts[i + 1..]
            .iter()
            .any(|p| *p == "trim" || p.starts_with("trim-")),
    )
}

fn date_pos(parts: &[&str]) -> Option<usize> {
    parts
        .iter()
        .position(|p| p.len() == 10 && p.as_bytes()[4] == b'-' && p.as_bytes()[7] == b'-')
}

/// Game name from a clip file name: everything before the `_YYYY-MM-DD_` stamp.
fn title_of(stem: &str) -> String {
    let parts: Vec<&str> = stem.split('_').collect();
    let date = date_pos(&parts);
    let name = match date {
        Some(i) if i > 0 => parts[..i].join(" "),
        _ => stem.replace('_', " "),
    };
    if name == "clip" {
        "Clip".into()
    } else {
        name
    }
}

/// Every clip in `dir`, newest first.
pub fn list(dir: &Path) -> Vec<Clip> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut clips: Vec<(SystemTime, u64, PathBuf)> = rd
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "mp4") && e.path().is_file())
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            Some((m.modified().ok()?, m.len(), e.path()))
        })
        .collect();
    clips.sort_by(|a, b| b.0.cmp(&a.0));
    clips
        .into_iter()
        .map(|(modified, bytes, path)| {
            let stem = path.file_stem().unwrap_or_default().to_string_lossy();
            let (game, exported) = game_of(&stem);
            Clip {
                title: title_of(&stem),
                game,
                exported,
                seconds: mp4_seconds(&path),
                bytes,
                path,
                modified,
            }
        })
        .collect()
}

/// The paths from `paths` that are safe to touch: `.mp4` files that sit
/// directly in `dir`. Used by `trash` so it can't be
/// pointed at files outside the clips folder.
fn clips_in_dir<'a>(dir: &Path, paths: &'a [PathBuf]) -> Vec<&'a PathBuf> {
    let Ok(dir) = dir.canonicalize() else {
        return Vec::new();
    };
    paths
        .iter()
        .filter(|p| p.extension().is_some_and(|x| x == "mp4") && p.is_file())
        .filter(|p| p.parent().and_then(|d| d.canonicalize().ok()).as_deref() == Some(&dir))
        .collect()
}

/// Moves the given clips to the freedesktop Trash (same safety filter as
/// the safety filter). Returns the paths that were trashed; a clip that wouldn't go
/// is left alone rather than deleted.
pub fn trash(dir: &Path, paths: &[PathBuf]) -> Vec<PathBuf> {
    clips_in_dir(dir, paths)
        .into_iter()
        .filter(|p| match trash::delete(p) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("trash {}: {e}", p.display());
                false
            }
        })
        .cloned()
        .collect()
}

/// Restores `paths` (names as they were in `dir`) back from the Trash,
/// picking the most recently trashed entry for each name. Returns the paths
/// that reappeared.
pub fn untrash(dir: &Path, paths: &[PathBuf]) -> Vec<PathBuf> {
    let Ok(dir) = dir.canonicalize() else {
        return Vec::new();
    };
    let Ok(items) = trash::os_limited::list() else {
        return Vec::new();
    };
    let mut chosen = Vec::new();
    let mut wanted = Vec::new();
    for p in paths {
        if p.extension().is_none_or(|x| x != "mp4") {
            continue;
        }
        let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(item) = items
            .iter()
            .filter(|i| i.name == name)
            .filter(|i| {
                i.original_parent
                    .canonicalize()
                    .as_deref()
                    .unwrap_or(&i.original_parent)
                    == dir
            })
            .max_by_key(|i| i.time_deleted)
        else {
            continue;
        };
        chosen.push(item.clone());
        wanted.push(p.clone());
    }
    if let Err(e) = trash::os_limited::restore_all(chosen) {
        eprintln!("restore from trash: {e}");
        // The trash crate reserves each name with an empty file before its
        // rename; when the rename fails (e.g. a clips folder on another drive
        // than the Trash) that placeholder stays behind. Clips are never
        // empty, so drop those and leave the clip in the Trash.
        for p in &wanted {
            if std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.len() == 0) {
                let _ = std::fs::remove_file(p);
            }
        }
    }
    wanted
        .into_iter()
        .filter(|p| std::fs::metadata(p).is_ok_and(|m| m.len() > 0))
        .collect()
}

/// Total size and count of the clips in `dir`.
pub fn usage(dir: &Path) -> (u64, usize) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return (0, 0);
    };
    rd.flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "mp4") && e.path().is_file())
        .filter_map(|e| e.metadata().ok())
        .fold((0, 0), |(b, n), m| (b + m.len(), n + 1))
}

/// Duration from the `moov/mvhd` box, without decoding anything.
pub fn mp4_seconds(path: &Path) -> Option<f64> {
    let mut f = File::open(path).ok()?;
    let end = f.metadata().ok()?.len();
    let (start, len) = find_box(&mut f, 0, end, b"moov")?;
    let (mvhd, _) = find_box(&mut f, start, start + len, b"mvhd")?;
    f.seek(SeekFrom::Start(mvhd)).ok()?;
    let mut v = [0u8; 4];
    f.read_exact(&mut v).ok()?;
    let (scale, dur) = if v[0] == 1 {
        let mut b = [0u8; 28];
        f.read_exact(&mut b).ok()?;
        (
            u32::from_be_bytes(b[16..20].try_into().unwrap()) as f64,
            u64::from_be_bytes(b[20..28].try_into().unwrap()) as f64,
        )
    } else {
        let mut b = [0u8; 16];
        f.read_exact(&mut b).ok()?;
        (
            u32::from_be_bytes(b[8..12].try_into().unwrap()) as f64,
            u32::from_be_bytes(b[12..16].try_into().unwrap()) as f64,
        )
    };
    (scale > 0.0).then(|| dur / scale)
}

/// Returns (payload offset, payload length) of the first `kind` box in [from, to).
fn find_box(f: &mut File, mut at: u64, to: u64, kind: &[u8; 4]) -> Option<(u64, u64)> {
    while at + 8 <= to {
        f.seek(SeekFrom::Start(at)).ok()?;
        let mut h = [0u8; 8];
        f.read_exact(&mut h).ok()?;
        let mut size = u32::from_be_bytes(h[..4].try_into().unwrap()) as u64;
        let mut header = 8;
        if size == 1 {
            let mut l = [0u8; 8];
            f.read_exact(&mut l).ok()?;
            size = u64::from_be_bytes(l);
            header = 16;
        } else if size == 0 {
            size = to - at;
        }
        if size < header {
            return None;
        }
        if &h[4..8] == kind {
            return Some((at + header, size - header));
        }
        at += size;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_keeps_only_clips_in_dir() {
        let root = std::env::temp_dir().join(format!("clip-recorder-filt-{}", std::process::id()));
        let dir = root.join("clips");
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let keep = dir.join("a.mp4");
        let paths = vec![
            keep.clone(),
            dir.join("note.txt"),
            sub.join("nested.mp4"),
            root.join("outside.mp4"),
            dir.join("link.mp4"),
        ];
        for f in &paths[..4] {
            std::fs::write(f, b"x").unwrap();
        }
        // A symlink pointing at a real clip inside the dir still resolves
        // through its parent, so it counts too.
        std::os::unix::fs::symlink(&keep, &paths[4]).unwrap();
        assert_eq!(clips_in_dir(&dir, &paths), vec![&keep, &paths[4]]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn titles_from_names() {
        assert_eq!(title_of("Elden_Ring_2026-11-19_144930"), "Elden Ring");
        assert_eq!(title_of("clip_2026-11-19_144930-2"), "Clip");
        assert_eq!(title_of("whatever"), "whatever");
    }

    #[test]
    fn slugs_names() {
        assert_eq!(slug("Elden Ring"), "Elden_Ring");
        assert_eq!(slug("R.E.P.O."), "R_E_P_O_");
        assert_eq!(slug("clip"), "clip");
    }

    #[test]
    fn game_keys_and_exports() {
        assert_eq!(
            game_of("R_E_P_O__2026-11-22_224859_trim-2"),
            ("R_E_P_O_".to_owned(), true)
        );
        assert_eq!(
            game_of("R_E_P_O__2026-11-22_224859_trim"),
            ("R_E_P_O_".to_owned(), true)
        );
        assert_eq!(
            game_of("clip_2026-11-19_144930-2"),
            ("clip".to_owned(), false)
        );
        assert_eq!(
            game_of("Elden_Ring_2026-11-19_144930"),
            ("Elden_Ring".to_owned(), false)
        );
        assert_eq!(game_of("whatever"), ("whatever".to_owned(), false));
        assert_eq!(game_of("My_Clip_trim"), ("My_Clip".to_owned(), true));
        assert_eq!(game_of("My_Clip_trim-3"), ("My_Clip".to_owned(), true));
        assert_eq!(game_of("trim"), ("trim".to_owned(), false));
    }

    #[test]
    fn publish_never_replaces() {
        let dir = std::env::temp_dir().join(format!("clip-publish-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tmp = dir.join(".a.mp4.part");
        let taken = dir.join("a.mp4");
        std::fs::write(&taken, b"restored").unwrap();
        std::fs::write(&tmp, b"export").unwrap();
        let out = publish(&tmp, &taken).unwrap();
        assert_eq!(out, dir.join("a-2.mp4"));
        assert_eq!(std::fs::read(&taken).unwrap(), b"restored");
        assert_eq!(std::fs::read(&out).unwrap(), b"export");
        assert!(!tmp.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
