//! On-disk launcher state: the device token, the paired wallet, and what's installed.
//!
//! Stored as JSON in the OS app-data directory, written atomically (temp file +
//! rename) so a crash mid-write can't leave a truncated file that strands the
//! player at the pairing screen with games already on disk.
//!
//! Known limitation, stated plainly: the device token sits in a user-readable
//! file. On Windows and macOS the app-data directory is already user-scoped, and
//! on Unix we tighten the file to 0600. That defends against other *users* on the
//! machine, not against malware running as this user. The OS keychain would be
//! the upgrade. It is a deliberately low-value credential — read a library,
//! fetch download links, nothing else — and revoking it from the website kills it
//! instantly, so the blast radius of a theft is "someone else can download games
//! you already own."

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Default, Serialize, Deserialize, Clone)]
pub struct LauncherState {
    /// None until paired. Present means signed in.
    #[serde(default)]
    pub device_token: Option<String>,
    #[serde(default)]
    pub wallet: Option<String>,
    #[serde(default)]
    pub device_name: Option<String>,
    /// Keyed by product_id rendered as a string, because JSON object keys are
    /// strings and round-tripping integers here has bitten every launcher ever.
    #[serde(default)]
    pub games: HashMap<String, InstalledGame>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct InstalledGame {
    pub product_id: i64,
    pub title: String,
    /// Which build is on disk. Compared against the library's build_version to
    /// decide whether to offer "Update".
    pub build_version: i64,
    pub install_dir: String,
    /// Relative to install_dir. None when the archive had nothing runnable, in
    /// which case the UI offers "Open folder" instead of "Play".
    pub exe_path: Option<String>,
    pub installed_at: i64,
    /// Last time the server confirmed this wallet owns a copy. Feeds the
    /// offline grace window in gate.rs.
    pub last_verified_at: i64,
    pub bytes: u64,
}

impl LauncherState {
    pub fn is_paired(&self) -> bool {
        self.device_token.is_some()
    }

    pub fn get(&self, product_id: i64) -> Option<&InstalledGame> {
        self.games.get(&product_id.to_string())
    }

    pub fn upsert(&mut self, game: InstalledGame) {
        self.games.insert(game.product_id.to_string(), game);
    }

    pub fn remove(&mut self, product_id: i64) -> Option<InstalledGame> {
        self.games.remove(&product_id.to_string())
    }

    /// Records a fresh ownership confirmation for every game the library says is
    /// owned. Called after each successful library refresh so the offline grace
    /// window is measured from the last time we actually knew.
    pub fn stamp_verified(&mut self, owned_product_ids: &[i64], at: i64) {
        for pid in owned_product_ids {
            if let Some(g) = self.games.get_mut(&pid.to_string()) {
                g.last_verified_at = at;
            }
        }
    }

    /// Signing out drops the token and the wallet but deliberately KEEPS the
    /// installed-game records. The files are still on disk; forgetting we put
    /// them there would orphan gigabytes with no way to find or remove them
    /// through the UI.
    pub fn sign_out(&mut self) {
        self.device_token = None;
        self.wallet = None;
        self.device_name = None;
        for g in self.games.values_mut() {
            g.last_verified_at = 0; // must re-prove ownership after re-pairing
        }
    }
}

pub fn state_path(app_data: &Path) -> PathBuf {
    app_data.join("state.json")
}

pub fn load(app_data: &Path) -> LauncherState {
    let path = state_path(app_data);
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
            // A corrupt state file must not brick the launcher. Keep the bad one
            // for diagnosis and start clean; the worst case is re-pairing.
            eprintln!("state.json unreadable ({e}); starting fresh");
            let _ = std::fs::rename(&path, path.with_extension("json.corrupt"));
            LauncherState::default()
        }),
        Err(_) => LauncherState::default(),
    }
}

pub fn save(app_data: &Path, state: &LauncherState) -> std::io::Result<()> {
    std::fs::create_dir_all(app_data)?;
    let path = state_path(app_data);
    let tmp = path.with_extension("json.tmp");

    let json = serde_json::to_string_pretty(state)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(json.as_bytes())?;
        f.sync_all()?; // the rename is only atomic if the bytes are already down
    }

    restrict_permissions(&tmp);
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) {
    // Windows: %APPDATA% is already scoped to the user account.
}
