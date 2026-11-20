//! The clips folder: naming new clips and listing recent ones.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub struct Clip {
    pub path: PathBuf,
    pub title: String,
    pub seconds: Option<f64>,
    pub modified: SystemTime,
}

/// `Elden Ring` + now -> `Elden_Ring_2026-11-19_1452.mp4`, unique in `dir`.
pub fn new_path(dir: &Path, game: Option<&str>) -> PathBuf {
    let slug: String = game
        .unwrap_or("clip")
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
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

/// Game name from a clip file name: everything before the `_YYYY-MM-DD_` stamp.
fn title_of(stem: &str) -> String {
    let parts: Vec<&str> = stem.split('_').collect();
    let date = parts
        .iter()
        .position(|p| p.len() == 10 && p.as_bytes()[4] == b'-' && p.as_bytes()[7] == b'-');
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
    let mut clips: Vec<(SystemTime, PathBuf)> = rd
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "mp4"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    clips.sort_by(|a, b| b.0.cmp(&a.0));
    clips
        .into_iter()
        .map(|(modified, path)| Clip {
            title: title_of(&path.file_stem().unwrap_or_default().to_string_lossy()),
            seconds: mp4_seconds(&path),
            path,
            modified,
        })
        .collect()
}

/// Deletes the given clips, but only `.mp4` files that sit directly in `dir`.
/// Returns the paths that were removed.
pub fn delete(dir: &Path, paths: &[PathBuf]) -> Vec<PathBuf> {
    let Ok(dir) = dir.canonicalize() else {
        return Vec::new();
    };
    paths
        .iter()
        .filter(|p| p.extension().is_some_and(|x| x == "mp4"))
        .filter(|p| p.parent().and_then(|d| d.canonicalize().ok()).as_deref() == Some(&dir))
        .filter(|p| match std::fs::remove_file(p) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("delete {}: {e}", p.display());
                false
            }
        })
        .cloned()
        .collect()
}

/// Total size and count of the clips in `dir`.
pub fn usage(dir: &Path) -> (u64, usize) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return (0, 0);
    };
    rd.flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "mp4"))
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
    fn delete_only_touches_clips_in_dir() {
        let root = std::env::temp_dir().join(format!("clip-recorder-del-{}", std::process::id()));
        let dir = root.join("clips");
        std::fs::create_dir_all(&dir).unwrap();
        let (a, txt, outside) = (dir.join("a.mp4"), dir.join("note.txt"), root.join("b.mp4"));
        for f in [&a, &txt, &outside] {
            std::fs::write(f, b"x").unwrap();
        }
        let gone = delete(&dir, &[a.clone(), txt.clone(), outside.clone()]);
        assert_eq!(gone, vec![a.clone()]);
        assert!(!a.exists() && txt.exists() && outside.exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn titles_from_names() {
        assert_eq!(title_of("Elden_Ring_2026-11-19_144930"), "Elden Ring");
        assert_eq!(title_of("clip_2026-11-19_144930-2"), "Clip");
        assert_eq!(title_of("whatever"), "whatever");
    }
}
