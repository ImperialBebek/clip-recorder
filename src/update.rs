//! Self-update from GitHub releases. Uses `curl`, `tar` and `sha256sum`, so
//! the binary carries no HTTP or TLS stack of its own.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::platform::ipc;

const REPO: &str = "ImperialBebek/clip-recorder";

/// The release asset this copy wants: the AppImage when running as one, the
/// install tarball otherwise.
fn asset_name(appimage: bool) -> String {
    let arch = env!("CLIP_RECORDER_ARCH");
    if appimage {
        format!("clip-recorder-{arch}.AppImage")
    } else {
        format!("clip-recorder-{arch}-linux.tar.gz")
    }
}

/// `$APPIMAGE`, when this process runs as an AppImage: its path on the real
/// filesystem (`current_exe` points inside the read-only mount then).
fn appimage() -> Option<PathBuf> {
    let p = PathBuf::from(std::env::var_os("APPIMAGE")?);
    p.is_file().then_some(p)
}

pub struct Release {
    pub version: String,
    asset_url: String,
    sum_url: Option<String>,
}

/// `v1.2.3` / `1.2.3` -> (1, 2, 3).
fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let mut it = v.trim().trim_start_matches('v').split('.');
    let mut n = || {
        it.next()?
            .split(|c: char| !c.is_ascii_digit())
            .next()?
            .parse()
            .ok()
    };
    Some((n()?, n()?, n()?))
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

/// A token for a private repository: `GITHUB_TOKEN`, `GH_TOKEN` or `gh auth token`.
fn token() -> Option<String> {
    for var in ["GITHUB_TOKEN", "GH_TOKEN"] {
        if let Ok(t) = std::env::var(var) {
            if !t.trim().is_empty() {
                return Some(t.trim().to_owned());
            }
        }
    }
    let out = Command::new("gh").args(["auth", "token"]).output().ok()?;
    let t = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (out.status.success() && !t.is_empty()).then_some(t)
}

fn curl(url: &str, accept: &str, out: Option<&Path>) -> Result<Vec<u8>> {
    let mut cmd = Command::new("curl");
    cmd.args(["-fsSL", "--max-time", "300", "-H"])
        .arg(format!("Accept: {accept}"))
        .args(["-H", "X-GitHub-Api-Version: 2022-11-28"]);
    if let Some(p) = out {
        cmd.arg("-o").arg(p);
    }
    cmd.arg(url)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // The token goes in through a config on stdin, not argv, so other users
    // can't read it from the process list. curl drops the header when the
    // download redirects to another host.
    let auth = token().map(|t| format!("header = \"Authorization: Bearer {t}\"\n"));
    if auth.is_some() {
        cmd.args(["-K", "-"]);
    }
    let mut child = cmd.spawn().context("curl is not installed")?;
    {
        let mut stdin = child.stdin.take().unwrap();
        if let Some(a) = &auth {
            stdin.write_all(a.as_bytes())?;
        }
    }
    let res = child.wait_with_output()?;
    if !res.status.success() {
        let err = String::from_utf8_lossy(&res.stderr);
        if err.contains("404") {
            bail!(
                "no published release at github.com/{REPO} (for a private repository, \
                 set GITHUB_TOKEN or log in with `gh auth login`)"
            );
        }
        bail!("download failed: {}", err.trim());
    }
    Ok(res.stdout)
}

/// The latest release, if it is newer than this binary.
pub fn check() -> Result<Option<Release>> {
    let body = curl(
        &format!("https://api.github.com/repos/{REPO}/releases/latest"),
        "application/vnd.github+json",
        None,
    )?;
    let json: serde_json::Value = serde_json::from_slice(&body).context("bad release data")?;
    let version = json["tag_name"]
        .as_str()
        .context("release has no tag")?
        .trim_start_matches('v')
        .to_owned();
    if !is_newer(&version, env!("CARGO_PKG_VERSION")) {
        return Ok(None);
    }
    let assets = json["assets"].as_array().cloned().unwrap_or_default();
    let asset = asset_name(appimage().is_some());
    let (asset_url, sum_url) =
        pick_urls(&assets, &asset).with_context(|| format!("release v{version} has no {asset}"))?;
    Ok(Some(Release {
        version,
        sum_url,
        asset_url,
    }))
}

/// The asset and checksum download URLs for `asset` in a release's asset list.
fn pick_urls(assets: &[serde_json::Value], asset: &str) -> Option<(String, Option<String>)> {
    let url_of = |name: &str| {
        assets
            .iter()
            .find(|a| a["name"] == name)
            .and_then(|a| a["url"].as_str())
            .map(str::to_owned)
    };
    Some((url_of(asset)?, url_of(&format!("{asset}.sha256"))))
}

/// Downloads the release and swaps it in for the installed binary. Call
/// `restart` afterwards to move a running recorder onto it. As an AppImage it
/// replaces `$APPIMAGE` itself and returns its path.
pub fn apply(rel: &Release) -> Result<PathBuf> {
    if let Some(path) = appimage() {
        return apply_appimage(rel, &path);
    }
    let exe = std::env::current_exe()?.canonicalize()?;
    let dir = exe.parent().context("binary has no directory")?;
    let work = dir.join(format!(".clip-recorder-update-{}", std::process::id()));
    std::fs::create_dir(&work).with_context(|| {
        format!(
            "{} already exists or {} is not writable; remove it and retry, \
             or update with your package manager / reinstall it",
            work.display(),
            dir.display()
        )
    })?;
    let result = (|| {
        let tarball = work.join(asset_name(false));
        curl(&rel.asset_url, "application/octet-stream", Some(&tarball))?;
        verify_sum(rel, &tarball)?;
        let ok = Command::new("tar")
            .arg("-xzf")
            .arg(&tarball)
            .arg("-C")
            .arg(&work)
            .status()?
            .success();
        let new = work.join("clip-recorder").join("clip-recorder");
        if !ok || !new.exists() {
            bail!("release archive has no clip-recorder binary");
        }
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o755))?;
        // A rename keeps the running process on the old inode.
        std::fs::rename(&new, &exe).with_context(|| format!("cannot replace {}", exe.display()))?;
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&work);
    result?;
    Ok(exe)
}

/// The AppImage variant of `apply`: the new file is downloaded next to the
/// running image and renamed over it, so the running process keeps the old
/// inode until `restart` relaunches the path.
fn apply_appimage(rel: &Release, appimage: &Path) -> Result<PathBuf> {
    let dir = appimage.parent().context("binary has no directory")?;
    let tmp = dir.join(format!(
        ".{}.{}.part",
        appimage
            .file_name()
            .context("bad $APPIMAGE")?
            .to_string_lossy(),
        std::process::id()
    ));
    let mut created = false;
    let result = (|| -> Result<()> {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        // A leftover staging file is only removed when it is plainly ours;
        // anything else (e.g. a symlink someone planted) stops the update.
        if let Ok(md) = std::fs::symlink_metadata(&tmp) {
            let own = unsafe { libc::getuid() };
            if !(md.file_type().is_file() && md.uid() == own) {
                bail!(
                    "{} exists and is not a file we created; refusing to update",
                    tmp.display()
                );
            }
            std::fs::remove_file(&tmp)?;
        }
        // create_new fails on an existing path (incl. symlinks) instead of
        // following it into the AppImage's directory.
        let f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| {
                format!(
                    "{} is not writable; move or replace {} yourself",
                    dir.display(),
                    appimage.display()
                )
            })?;
        created = true;
        drop(f);
        curl(&rel.asset_url, "application/octet-stream", Some(&tmp))?;
        verify_sum(rel, &tmp)?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
        std::fs::rename(&tmp, appimage)
            .with_context(|| format!("cannot replace {}", appimage.display()))?;
        Ok(())
    })();
    if result.is_err() && created {
        let _ = std::fs::remove_file(&tmp);
    }
    result?;
    Ok(appimage.to_path_buf())
}

/// Checks `file` against the release's `.sha256` asset when there is one.
fn verify_sum(rel: &Release, file: &Path) -> Result<()> {
    if let Some(url) = &rel.sum_url {
        let want = String::from_utf8_lossy(&curl(url, "application/octet-stream", None)?)
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_owned();
        let got = Command::new("sha256sum").arg(file).output()?;
        let got = String::from_utf8_lossy(&got.stdout);
        if got.split_whitespace().next() != Some(want.as_str()) {
            bail!("checksum mismatch, not installing");
        }
    }
    Ok(())
}

/// Restarts a running recorder with the arguments it was started with. A
/// recorder run by a systemd user unit is restarted through systemd, since
/// quitting it would also stop this process when it runs in the same unit.
pub fn restart(exe: &Path) {
    let Ok(status) = ipc::status() else {
        return;
    };
    if let Some(unit) = status.get("unit").filter(|u| !u.is_empty()) {
        let _ = Command::new("systemctl")
            .args(["--user", "restart", "--no-block"])
            .arg(unit)
            .status();
        return;
    }
    let args: Vec<&str> = status
        .get("args")
        // `--reselect` was for that launch; replaying it would ask for a screen again.
        .map(|a| {
            a.split('\t')
                .filter(|s| !s.is_empty() && *s != "--reselect")
                .collect()
        })
        .unwrap_or_default();
    let _ = ipc::request("quit");
    for _ in 0..50 {
        if !ipc::socket_path().exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let args = if args.is_empty() { vec!["run"] } else { args };
    // Own process group and no terminal, so closing the terminal that ran
    // `clip-recorder update` doesn't take the new recorder down with it.
    use std::os::unix::process::CommandExt;
    let spawned = Command::new(exe)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn();
    if let Ok(mut child) = spawned {
        std::thread::spawn(move || child.wait());
        // Give it a moment to bind the socket before this process exits.
        std::thread::sleep(Duration::from_millis(500));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert!(is_newer("v0.2.0", "0.1.9"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(!is_newer("garbage", "0.1.0"));
    }

    #[test]
    fn names_assets() {
        let arch = env!("CLIP_RECORDER_ARCH");
        assert_eq!(
            asset_name(false),
            format!("clip-recorder-{arch}-linux.tar.gz")
        );
        assert_eq!(asset_name(true), format!("clip-recorder-{arch}.AppImage"));
    }

    #[test]
    fn picks_asset_urls() {
        let arch = env!("CLIP_RECORDER_ARCH");
        let assets = serde_json::json!([
            {"name": format!("clip-recorder-{arch}-linux.tar.gz"), "url": "u/tar"},
            {"name": format!("clip-recorder-{arch}-linux.tar.gz.sha256"), "url": "u/tar.sum"},
            {"name": format!("clip-recorder-{arch}.AppImage"), "url": "u/app"},
            {"name": format!("clip-recorder-{arch}.AppImage.sha256"), "url": "u/app.sum"},
        ]);
        let assets = assets.as_array().unwrap();
        assert_eq!(
            pick_urls(assets, &asset_name(false)),
            Some(("u/tar".to_owned(), Some("u/tar.sum".to_owned())))
        );
        assert_eq!(
            pick_urls(assets, &asset_name(true)),
            Some(("u/app".to_owned(), Some("u/app.sum".to_owned())))
        );
        assert_eq!(pick_urls(assets, "clip-recorder-missing"), None);
    }
}
