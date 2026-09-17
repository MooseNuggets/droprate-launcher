//! Download → verify → extract → launch.
//!
//! Downloads stream to a temp file rather than into memory: a 100 GB build must
//! not need 100 GB of RAM. Progress is emitted to the UI as it goes, because a
//! multi-hour download with no feedback is indistinguishable from a hang.

use crate::gate::{pick_executable, safe_join};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Emitter};
use tokio::io::AsyncWriteExt;

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("download failed: {0}")]
    Download(String),
    #[error("this build didn't match its checksum and was not installed")]
    Checksum,
    #[error("the build archive is damaged: {0}")]
    Archive(String),
    #[error("{0}")]
    Io(String),
}

impl From<std::io::Error> for InstallError {
    fn from(e: std::io::Error) -> Self {
        InstallError::Io(e.to_string())
    }
}

#[derive(Clone, Serialize)]
pub struct Progress {
    pub product_id: i64,
    /// "downloading" | "verifying" | "extracting" | "done"
    pub phase: &'static str,
    pub received: u64,
    pub total: u64,
    /// -1 when the server didn't send a length, so the UI shows a bar it can't fill.
    pub percent: f64,
}

fn emit(app: &AppHandle, p: Progress) {
    // A UI that has gone away is not a reason to abort a download.
    let _ = app.emit("install-progress", p);
}

pub struct Installed {
    pub install_dir: PathBuf,
    pub exe_path: Option<String>,
    pub bytes: u64,
}

/// Streams `url` to disk, checks sha256 when the server supplied one, then
/// extracts into `install_dir`.
pub async fn download_and_install(
    app: &AppHandle,
    product_id: i64,
    url: &str,
    expected_sha256: Option<&str>,
    install_dir: &Path,
    platform: &str,
    tmp_dir: &Path,
) -> Result<Installed, InstallError> {
    std::fs::create_dir_all(tmp_dir)?;
    let archive_path = tmp_dir.join(format!("{product_id}.download"));

    // ---- download -------------------------------------------------------
    let client = reqwest::Client::builder()
        // No overall timeout: this request legitimately runs for hours. Per-read
        // timeouts below catch a stalled connection instead.
        .read_timeout(std::time::Duration::from_secs(60))
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| InstallError::Download(e.to_string()))?;

    let res = client
        .get(url)
        .send()
        .await
        .map_err(|e| InstallError::Download(e.to_string()))?;

    if !res.status().is_success() {
        // A 403 here is usually an expired presigned link; the caller can just
        // ask for a fresh one, since ownership is re-checked on every request.
        return Err(InstallError::Download(format!(
            "storage returned {}",
            res.status().as_u16()
        )));
    }

    let total = res.content_length().unwrap_or(0);
    let mut received: u64 = 0;
    let mut hasher = Sha256::new();
    let mut file = tokio::fs::File::create(&archive_path).await?;
    let mut stream = res.bytes_stream();
    let mut last_emit = std::time::Instant::now();

    use futures_util::StreamExt;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| InstallError::Download(e.to_string()))?;
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
        received += chunk.len() as u64;

        // Throttle to ~4/sec. Emitting per chunk floods the webview and makes
        // the UI slower than the download.
        if last_emit.elapsed() >= std::time::Duration::from_millis(250) {
            emit(
                app,
                Progress {
                    product_id,
                    phase: "downloading",
                    received,
                    total,
                    percent: if total > 0 {
                        received as f64 * 100.0 / total as f64
                    } else {
                        -1.0
                    },
                },
            );
            last_emit = std::time::Instant::now();
        }
    }
    file.flush().await?;
    file.sync_all().await?;
    drop(file);

    // ---- verify ---------------------------------------------------------
    if let Some(expected) = expected_sha256 {
        emit(app, Progress { product_id, phase: "verifying", received, total, percent: 100.0 });
        let actual = hex::encode(hasher.finalize());
        if !actual.eq_ignore_ascii_case(expected.trim()) {
            let _ = std::fs::remove_file(&archive_path);
            return Err(InstallError::Checksum);
        }
    }

    // ---- extract --------------------------------------------------------
    emit(app, Progress { product_id, phase: "extracting", received, total, percent: 100.0 });

    // Replace rather than merge: leftovers from an older build are how you get a
    // game that loads half-new assets and crashes in a way nobody can reproduce.
    if install_dir.exists() {
        std::fs::remove_dir_all(install_dir)?;
    }
    std::fs::create_dir_all(install_dir)?;

    let written = extract_zip(&archive_path, install_dir)?;
    let _ = std::fs::remove_file(&archive_path);

    let exe_path = pick_executable(&written, platform);

    #[cfg(unix)]
    if let Some(ref rel) = exe_path {
        use std::os::unix::fs::PermissionsExt;
        let full = install_dir.join(rel);
        if let Ok(meta) = std::fs::metadata(&full) {
            let mut perms = meta.permissions();
            perms.set_mode(perms.mode() | 0o755);
            let _ = std::fs::set_permissions(&full, perms);
        }
    }

    emit(app, Progress { product_id, phase: "done", received, total, percent: 100.0 });

    Ok(Installed {
        install_dir: install_dir.to_path_buf(),
        exe_path,
        bytes: received,
    })
}

/// Extracts every entry that `safe_join` accepts. Returns the relative paths of
/// the regular files written, for executable detection.
fn extract_zip(archive: &Path, dest: &Path) -> Result<Vec<String>, InstallError> {
    let file = std::fs::File::open(archive)?;
    let mut zip =
        zip::ZipArchive::new(file).map_err(|e| InstallError::Archive(e.to_string()))?;
    let mut written = Vec::new();

    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| InstallError::Archive(e.to_string()))?;

        // `mangled_name` is zip's own sanitiser. We do not rely on it — safe_join
        // is the guard — but using the raw name keeps our checks over the bytes
        // the archive actually contains.
        let raw = entry.name().to_string();

        let Some(target) = safe_join(dest, &raw) else {
            // Silently skipping a traversal attempt would hide an attack; a
            // damaged archive is worth a line in the log either way.
            eprintln!("skipped unsafe archive entry: {raw}");
            continue;
        };

        if entry.is_dir() {
            std::fs::create_dir_all(&target)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut out = std::fs::File::create(&target)?;
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = entry.read(&mut buf)?;
            if n == 0 {
                break;
            }
            std::io::Write::write_all(&mut out, &buf[..n])?;
        }

        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode));
        }

        written.push(raw.replace('\\', "/"));
    }

    Ok(written)
}

/// Starts the game and returns immediately. The launcher does not babysit the
/// process — closing the launcher should not kill someone's game.
pub fn launch(install_dir: &Path, exe_rel: &str) -> Result<(), InstallError> {
    let Some(exe) = safe_join(install_dir, exe_rel) else {
        return Err(InstallError::Io("recorded executable path is invalid".into()));
    };
    if !exe.exists() {
        return Err(InstallError::Io(
            "the game's files are missing — reinstall it".into(),
        ));
    }

    let mut cmd = std::process::Command::new(&exe);
    // Games routinely load assets by relative path; launching from anywhere else
    // makes them fail in confusing ways.
    cmd.current_dir(exe.parent().unwrap_or(install_dir));

    cmd.spawn().map_err(|e| InstallError::Io(e.to_string()))?;
    Ok(())
}
