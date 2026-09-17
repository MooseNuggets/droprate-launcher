//! The two decisions that must not be wrong, kept free of every dependency so
//! they can be compiled and tested on their own:
//!
//!   1. may this game launch?   (the resale gate)
//!   2. where may this archive entry be written?   (zip path traversal)
//!
//! Nothing here does I/O or networking. `rustc --test gate.rs` builds it alone.

use std::path::{Component, Path, PathBuf};

// ---------------------------------------------------------------------------
// 1. The resale gate
// ---------------------------------------------------------------------------

/// How long a game may launch on a previously-good ownership check when the
/// server can't be reached. Covers a weekend offline; does not cover a month.
pub const OWNERSHIP_GRACE_SECS: i64 = 72 * 60 * 60;

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum LaunchDecision {
    /// Ownership is good — either confirmed just now, or confirmed recently
    /// enough that a temporary network failure shouldn't strand the owner.
    Allow,
    /// The chain says this wallet does not own a copy. Never overridable.
    Blocked(&'static str),
    /// We don't know, and the last time we did know was too long ago.
    NeedsVerification(&'static str),
}

/// `owned_now` is the *server's* answer, which is chain-checked:
///   `Some(true)`  — verified owned right now
///   `Some(false)` — verified NOT owned (sold, transferred, refunded)
///   `None`        — we could not ask (offline, timeout, server down)
///
/// The asymmetry is the whole point. A definite "no" blocks immediately and no
/// amount of grace saves it — that is the resale rule. Grace exists only for the
/// case where we genuinely don't know, so that a flaky connection doesn't take
/// someone's game away from them.
pub fn launch_decision(
    owned_now: Option<bool>,
    last_verified_at: i64,
    now: i64,
    grace_secs: i64,
) -> LaunchDecision {
    match owned_now {
        Some(true) => LaunchDecision::Allow,
        Some(false) => LaunchDecision::Blocked(
            "This copy is no longer owned by the paired wallet. It can't be launched.",
        ),
        None => {
            // A last_verified_at in the future means the clock moved backwards
            // (or was tampered with). Treat it as not verified rather than as
            // infinitely fresh, which is what a naive `now - last < grace` does.
            if last_verified_at <= 0 || last_verified_at > now {
                return LaunchDecision::NeedsVerification(
                    "Connect to the internet once to confirm you own this game.",
                );
            }
            if now.saturating_sub(last_verified_at) <= grace_secs {
                LaunchDecision::Allow
            } else {
                LaunchDecision::NeedsVerification(
                    "It's been a while since we could check this game. Reconnect to keep playing.",
                )
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 2. Archive path safety
// ---------------------------------------------------------------------------

/// Resolve an archive entry's path against the install directory, refusing
/// anything that would escape it.
///
/// A build is a zip uploaded by a developer. A hostile or merely broken one can
/// contain `../../../.ssh/authorized_keys`, an absolute path, or on Windows a
/// drive-relative path like `C:\Windows\...`. Extracting those verbatim writes
/// outside the game folder, which is arbitrary file write on the player's
/// machine. Every entry goes through here first.
///
/// Returns `None` when the entry must be skipped.
pub fn safe_join(install_dir: &Path, entry: &str) -> Option<PathBuf> {
    // Zip stores forward slashes; Windows-built archives sometimes use back
    // slashes anyway. Normalise before parsing so `..\..\x` is not read as one
    // long filename that happens to contain dots.
    let normalised = entry.replace('\\', "/");

    if normalised.is_empty() {
        return None;
    }
    // Reject drive letters and UNC paths outright rather than trying to
    // interpret them. No legitimate game archive needs them.
    if normalised.starts_with('/') || normalised.starts_with("//") {
        return None;
    }
    let bytes = normalised.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && (bytes[0] as char).is_ascii_alphabetic() {
        return None;
    }
    // NUL is not legal in a path and is a classic truncation trick.
    if normalised.contains('\0') {
        return None;
    }

    let mut out = install_dir.to_path_buf();
    let mut depth = 0usize;

    for part in normalised.split('/') {
        match part {
            // Skip empty segments from "a//b" and no-op "." segments.
            "" | "." => continue,
            ".." => {
                // Never allow climbing, not even back to where we started.
                // `a/../b` is harmless in principle, but permitting it means
                // reasoning about balance; refusing it costs nothing real.
                return None;
            }
            other => {
                // Reject any component Rust would treat as special once parsed.
                let candidate = Path::new(other);
                let mut comps = candidate.components();
                match (comps.next(), comps.next()) {
                    (Some(Component::Normal(_)), None) => {}
                    _ => return None,
                }
                out.push(other);
                depth += 1;
            }
        }
    }

    // "./" or "" resolved to the directory itself — nothing to write.
    if depth == 0 {
        return None;
    }
    Some(out)
}

/// Pick the executable to run from the files an archive produced.
///
/// Prefers an exact platform-appropriate match at the shallowest depth, because
/// a build usually has `Game.exe` at the root and incidentals like
/// `tools/crashpad.exe` further down.
pub fn pick_executable(files: &[String], platform: &str) -> Option<String> {
    let is_candidate = |f: &str| -> bool {
        let lower = f.to_lowercase();
        match platform {
            "win" => lower.ends_with(".exe"),
            "mac" => lower.ends_with(".app") || !lower.contains('.'),
            _ => !lower.contains('.'),
        }
    };

    let mut best: Option<(usize, &String)> = None;
    for f in files {
        if !is_candidate(f) {
            continue;
        }
        let lower = f.to_lowercase();
        // Installers and redistributables are not the game.
        if lower.contains("unins")
            || lower.contains("vcredist")
            || lower.contains("dxsetup")
            || lower.contains("crashpad")
            || lower.contains("setup.exe")
        {
            continue;
        }
        let depth = f.matches('/').count();
        match best {
            Some((d, _)) if d <= depth => {}
            _ => best = Some((depth, f)),
        }
    }
    best.map(|(_, f)| f.clone())
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: i64 = 3600;
    const NOW: i64 = 1_800_000_000;

    // ---- the resale gate --------------------------------------------------

    #[test]
    fn owned_right_now_always_launches() {
        assert_eq!(
            launch_decision(Some(true), 0, NOW, OWNERSHIP_GRACE_SECS),
            LaunchDecision::Allow,
            "a live yes should not depend on any local state"
        );
    }

    #[test]
    fn sold_copy_is_blocked_even_though_it_was_verified_seconds_ago() {
        // This is the case the whole feature exists for: the player owned it a
        // moment ago, sold it, and is still holding the files.
        let d = launch_decision(Some(false), NOW - 5, NOW, OWNERSHIP_GRACE_SECS);
        assert!(matches!(d, LaunchDecision::Blocked(_)));
    }

    #[test]
    fn grace_never_rescues_a_definite_no() {
        for last in [NOW, NOW - HOUR, NOW - 1000 * HOUR, 0] {
            assert!(
                matches!(
                    launch_decision(Some(false), last, NOW, OWNERSHIP_GRACE_SECS),
                    LaunchDecision::Blocked(_)
                ),
                "a chain-verified 'not owned' must block regardless of local history"
            );
        }
    }

    #[test]
    fn offline_within_grace_still_plays() {
        assert_eq!(
            launch_decision(None, NOW - 71 * HOUR, NOW, OWNERSHIP_GRACE_SECS),
            LaunchDecision::Allow,
            "a weekend without internet should not take someone's game away"
        );
    }

    #[test]
    fn offline_past_grace_asks_for_a_check() {
        assert!(matches!(
            launch_decision(None, NOW - 73 * HOUR, NOW, OWNERSHIP_GRACE_SECS),
            LaunchDecision::NeedsVerification(_)
        ));
    }

    #[test]
    fn grace_boundary_is_inclusive_and_does_not_overflow() {
        assert_eq!(
            launch_decision(None, NOW - OWNERSHIP_GRACE_SECS, NOW, OWNERSHIP_GRACE_SECS),
            LaunchDecision::Allow
        );
        assert!(matches!(
            launch_decision(None, NOW - OWNERSHIP_GRACE_SECS - 1, NOW, OWNERSHIP_GRACE_SECS),
            LaunchDecision::NeedsVerification(_)
        ));
        // i64::MIN would panic on a plain subtraction.
        assert!(matches!(
            launch_decision(None, i64::MIN, NOW, OWNERSHIP_GRACE_SECS),
            LaunchDecision::NeedsVerification(_)
        ));
    }

    #[test]
    fn never_verified_offline_cannot_play() {
        assert!(
            matches!(
                launch_decision(None, 0, NOW, OWNERSHIP_GRACE_SECS),
                LaunchDecision::NeedsVerification(_)
            ),
            "a fresh install must prove ownership once before it runs anything"
        );
    }

    #[test]
    fn a_clock_rolled_forward_does_not_buy_infinite_grace() {
        // Set the system clock back a year and last_verified_at is now "in the
        // future". A naive now-last < grace comparison would allow this forever.
        assert!(matches!(
            launch_decision(None, NOW + 365 * 24 * HOUR, NOW, OWNERSHIP_GRACE_SECS),
            LaunchDecision::NeedsVerification(_)
        ));
    }

    // ---- archive path safety ---------------------------------------------

    fn dir() -> PathBuf {
        PathBuf::from("/games/7")
    }

    #[test]
    fn ordinary_entries_land_inside_the_install_folder() {
        assert_eq!(
            safe_join(&dir(), "Game.exe"),
            Some(PathBuf::from("/games/7/Game.exe"))
        );
        assert_eq!(
            safe_join(&dir(), "data/assets/pack.bin"),
            Some(PathBuf::from("/games/7/data/assets/pack.bin"))
        );
    }

    #[test]
    fn parent_traversal_is_refused() {
        for evil in [
            "../evil.exe",
            "../../../../etc/passwd",
            "data/../../escape.txt",
            "a/b/../../../c",
        ] {
            assert_eq!(safe_join(&dir(), evil), None, "must refuse {evil}");
        }
    }

    #[test]
    fn backslash_traversal_is_refused() {
        // A Windows-built zip can carry backslashes; splitting only on '/' would
        // treat this as one harmless filename.
        for evil in ["..\\evil.exe", "data\\..\\..\\escape.txt", "..\\..\\a\\b"] {
            assert_eq!(safe_join(&dir(), evil), None, "must refuse {evil}");
        }
    }

    #[test]
    fn absolute_and_drive_paths_are_refused() {
        for evil in [
            "/etc/passwd",
            "//server/share/x",
            "C:/Windows/System32/evil.dll",
            "c:\\Windows\\evil.dll",
            "Z:/anything",
        ] {
            assert_eq!(safe_join(&dir(), evil), None, "must refuse {evil}");
        }
    }

    #[test]
    fn nul_byte_is_refused() {
        assert_eq!(safe_join(&dir(), "Game.exe\0.txt"), None);
    }

    #[test]
    fn empty_and_dot_entries_are_skipped() {
        assert_eq!(safe_join(&dir(), ""), None);
        assert_eq!(safe_join(&dir(), "."), None);
        assert_eq!(safe_join(&dir(), "./"), None);
        assert_eq!(safe_join(&dir(), "//"), None);
    }

    #[test]
    fn redundant_separators_are_tolerated_not_rejected() {
        // Real archives do contain "data//file"; it isn't an attack.
        assert_eq!(
            safe_join(&dir(), "data//file.bin"),
            Some(PathBuf::from("/games/7/data/file.bin"))
        );
        assert_eq!(
            safe_join(&dir(), "./data/file.bin"),
            Some(PathBuf::from("/games/7/data/file.bin"))
        );
    }

    #[test]
    fn every_output_stays_under_the_install_dir() {
        // Property-ish sweep: whatever we accept must be a descendant.
        let candidates = [
            "a", "a/b", "a/b/c.exe", "./a", "a//b", "Game.exe", "x/y/z/w/v.dat",
            "../a", "..", "a/../b", "/a", "C:/a", "a\\..\\b", "\0",
        ];
        for c in candidates {
            if let Some(p) = safe_join(&dir(), c) {
                assert!(
                    p.starts_with(dir()),
                    "{c} resolved to {p:?}, which escapes the install dir"
                );
            }
        }
    }

    // ---- executable picking ----------------------------------------------

    #[test]
    fn picks_the_shallowest_exe_on_windows() {
        let files = vec![
            "tools/editor.exe".to_string(),
            "NeonDrift.exe".to_string(),
            "data/pack.bin".to_string(),
        ];
        assert_eq!(pick_executable(&files, "win"), Some("NeonDrift.exe".into()));
    }

    #[test]
    fn skips_installers_and_redistributables() {
        let files = vec![
            "vcredist_x64.exe".to_string(),
            "unins000.exe".to_string(),
            "bin/Game.exe".to_string(),
        ];
        assert_eq!(pick_executable(&files, "win"), Some("bin/Game.exe".into()));
    }

    #[test]
    fn no_executable_is_not_a_crash() {
        let files = vec!["readme.txt".to_string(), "data/pack.bin".to_string()];
        assert_eq!(pick_executable(&files, "win"), None);
    }
}
