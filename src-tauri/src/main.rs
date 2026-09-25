// Windows: no console window behind the app in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! DropRate Launcher.
//!
//! The security shape, since it's the reason this is a native app at all:
//!
//!   * The launcher never sees a private key, a seed phrase, or a wallet.
//!     Pairing happens in the browser, where the wallet already lives and where
//!     the person can read what they're approving. The launcher receives a
//!     device token and nothing else.
//!   * That token can list a library and request download links. It cannot spend,
//!     transfer, list for sale, or sign. Revoking it from the website is instant.
//!   * The token lives in Rust. The webview is never given it, so a compromised
//!     page — an injected script, a bad dependency — has nothing to exfiltrate.
//!   * Ownership is re-checked server-side against the chain on every download
//!     and before every launch. A sold copy stops working; see gate.rs.

mod api;
mod gate;
mod install;
mod state;

use api::{ApiError, Client};
use gate::{launch_decision, LaunchDecision, OWNERSHIP_GRACE_SECS};
use serde::Serialize;
use state::{now_secs, InstalledGame, LauncherState};
use std::path::PathBuf;
use std::sync::Mutex;
use tauri::{AppHandle, Manager, State};

/// Everything the launcher holds in memory. `pending_poll_token` is the only
/// piece that isn't persisted — an interrupted pairing should be restarted, not
/// resumed, because the code on screen is gone.
struct App {
    state: Mutex<LauncherState>,
    pending_poll_token: Mutex<Option<String>>,
    /// Held here rather than passed back through the webview so that "open the
    /// pairing page" takes no argument at all. A command that accepts a URL is a
    /// command that can be asked to open any URL; this one can only ever open
    /// the pairing page the server just issued us.
    pending_pair_url: Mutex<Option<String>>,
    app_data: PathBuf,
    client: Client,
}

impl App {
    fn snapshot(&self) -> LauncherState {
        self.state.lock().expect("state lock").clone()
    }

    fn mutate<F: FnOnce(&mut LauncherState)>(&self, f: F) -> Result<(), String> {
        let mut guard = self.state.lock().expect("state lock");
        f(&mut guard);
        state::save(&self.app_data, &guard).map_err(|e| e.to_string())
    }

    fn token(&self) -> Result<String, String> {
        self.snapshot()
            .device_token
            .ok_or_else(|| "This computer isn't paired yet.".to_string())
    }

    fn games_dir(&self) -> PathBuf {
        self.app_data.join("games")
    }

    fn install_dir(&self, product_id: i64) -> PathBuf {
        self.games_dir().join(product_id.to_string())
    }
}

/// A device token that the server no longer recognises means the pairing was
/// revoked from the website (or expired). Drop it locally rather than leaving
/// the UI insisting it's signed in.
fn handle_auth_failure(app: &App, err: &ApiError) {
    if let ApiError::Server { status: 401, .. } = err {
        let _ = app.mutate(|s| s.sign_out());
    }
}

// ---------------------------------------------------------------------------
// What the UI renders
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct Status {
    paired: bool,
    wallet: Option<String>,
    device_name: Option<String>,
    platform: &'static str,
    games: Vec<GameView>,
}

#[derive(Serialize, Clone)]
struct GameView {
    product_id: i64,
    title: String,
    tagline: Option<String>,
    image: Option<String>,
    copy_number: Option<i64>,
    /// Chain-verified at the last library refresh.
    owned: bool,
    available_platforms: Vec<String>,
    /// Is there a build for THIS machine?
    installable: bool,
    installed: bool,
    installed_version: i64,
    latest_version: i64,
    update_available: bool,
    playable: bool,
    /// Why the Play button is disabled, in words meant for a person.
    blocked_reason: Option<String>,
    install_dir: Option<String>,
}

#[tauri::command]
fn get_status(app: State<App>) -> Status {
    let s = app.snapshot();
    let platform = api::current_platform();

    // Offline view: everything we know from disk, with no live ownership answer.
    let games = s
        .games
        .values()
        .map(|g| {
            let decision = launch_decision(None, g.last_verified_at, now_secs(), OWNERSHIP_GRACE_SECS);
            GameView {
                product_id: g.product_id,
                title: g.title.clone(),
                tagline: None,
                image: None,
                copy_number: None,
                owned: !matches!(decision, LaunchDecision::Blocked(_)),
                available_platforms: vec![platform.to_string()],
                installable: true,
                installed: true,
                installed_version: g.build_version,
                latest_version: g.build_version,
                update_available: false,
                playable: matches!(decision, LaunchDecision::Allow) && g.exe_path.is_some(),
                blocked_reason: match decision {
                    LaunchDecision::Allow => {
                        if g.exe_path.is_none() {
                            Some("No runnable file was found in this build.".into())
                        } else {
                            None
                        }
                    }
                    LaunchDecision::Blocked(m) | LaunchDecision::NeedsVerification(m) => {
                        Some(m.to_string())
                    }
                },
                install_dir: Some(g.install_dir.clone()),
            }
        })
        .collect();

    Status {
        paired: s.is_paired(),
        wallet: s.wallet,
        device_name: s.device_name,
        platform,
        games,
    }
}

// ---------------------------------------------------------------------------
// Pairing
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct PairStarted {
    code: String,
    expires_in: i64,
}

#[tauri::command]
async fn pair_start(app: State<'_, App>) -> Result<PairStarted, String> {
    let name = hostname();
    let started = app
        .client
        .device_start(&name, api::current_platform())
        .await
        .map_err(|e| e.to_string())?;

    *app.pending_poll_token.lock().expect("poll lock") = Some(started.poll_token.clone());
    *app.pending_pair_url.lock().expect("url lock") = Some(started.pair_url.clone());

    // pair_url is deliberately not returned: the UI shows the code, and opening
    // the page goes through open_pair_page, which takes no argument.
    Ok(PairStarted {
        code: started.code,
        expires_in: started.expires_in,
    })
}

/// Open a droprate.xyz URL in the system browser.
///
/// Private on purpose, and no command takes a URL. Every caller builds its own
/// from values it controls — a stored pairing link, or a numeric product id —
/// so there is no path by which the webview could ask the shell to open
/// something arbitrary.
fn open_external(url: &str) -> Result<(), String> {
    if !url.starts_with("https://droprate.xyz/") {
        return Err("Refusing to open a link outside droprate.xyz.".into());
    }

    // Windows needs care. `explorer <url>` looks like it should work and usually
    // does, but when it dislikes an argument it fails SILENTLY and opens the
    // user's Documents folder — no error, no browser, and the person is left
    // staring at a file manager. The query string is what trips it. `cmd /c
    // start` is worse: the shell parses the URL and `&` splits it into a second
    // command. rundll32 hands the string straight to the Win32 protocol handler
    // with no shell in between, so `?` and `&` survive intact.
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("rundll32")
            .arg("url.dll,FileProtocolHandler")
            .arg(url)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    #[cfg(not(target_os = "windows"))]
    {
        let program = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
        std::process::Command::new(program)
            .arg(url)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// Opens the pairing page issued by `pair_start` in the system browser.
#[tauri::command]
fn open_pair_page(app: State<App>) -> Result<(), String> {
    let url = app
        .pending_pair_url
        .lock()
        .expect("url lock")
        .clone()
        .ok_or_else(|| "No pairing in progress.".to_string())?;

    open_external(&url)
}

/// Opens one game's page on the website so the person can buy it there.
///
/// Checkout deliberately does NOT happen in this app. Buying needs a wallet
/// signature, and the entire security story here is that wallet keys never come
/// near the launcher — the same reason pairing happens in a browser. So the
/// store browses here and hands off there, and the library picks the copy up
/// once it lands on chain.
///
/// Takes a product id, not a URL: an integer cannot be turned into a link to
/// somewhere else.
#[tauri::command]
fn open_store_page(product_id: i64) -> Result<(), String> {
    if product_id <= 0 {
        return Err("That game id isn't valid.".into());
    }
    // The store is the site's homepage now; `?game=` scrolls to and opens the title.
    open_external(&format!("https://droprate.xyz/?game={product_id}"))
}

/// Opens the Developer Portal on the website.
///
/// Studios manage launches in exactly one place — the web portal — the same
/// way Steam's tools live on the partner site rather than inside the client.
/// The app just gets them there; nothing dev-side is duplicated here, so
/// there's one login, one dashboard, and no way for the two to disagree.
#[tauri::command]
fn open_dev_portal() -> Result<(), String> {
    open_external("https://droprate.xyz/devportal.html")
}

#[derive(Serialize)]
struct PairPolled {
    state: String,
    wallet: Option<String>,
}

/// Called on a timer by the UI. The device token it may receive is stored here
/// and never returned to the webview.
#[tauri::command]
async fn pair_poll(app: State<'_, App>) -> Result<PairPolled, String> {
    let Some(poll_token) = app.pending_poll_token.lock().expect("poll lock").clone() else {
        return Ok(PairPolled { state: "idle".into(), wallet: None });
    };

    let polled = app
        .client
        .device_poll(&poll_token)
        .await
        .map_err(|e| e.to_string())?;

    if polled.state == "paired" {
        if let Some(token) = polled.device_token.clone() {
            let wallet = polled.wallet.clone();
            let name = polled.name.clone();
            app.mutate(|s| {
                s.device_token = Some(token);
                s.wallet = wallet;
                s.device_name = name;
            })?;
            *app.pending_poll_token.lock().expect("poll lock") = None;
        }
    }

    Ok(PairPolled { state: polled.state, wallet: polled.wallet })
}

#[tauri::command]
fn pair_cancel(app: State<App>) {
    *app.pending_poll_token.lock().expect("poll lock") = None;
}

#[tauri::command]
async fn sign_out(app: State<'_, App>) -> Result<(), String> {
    if let Ok(token) = app.token() {
        // Best effort — the local token goes regardless.
        let _ = app.client.forget(&token).await;
    }
    app.mutate(|s| s.sign_out())
}

// ---------------------------------------------------------------------------
// Library
// ---------------------------------------------------------------------------

#[tauri::command]
async fn refresh_library(app: State<'_, App>) -> Result<Vec<GameView>, String> {
    let token = app.token()?;
    let platform = api::current_platform();

    let lib = match app.client.library(&token).await {
        Ok(l) => l,
        Err(e) => {
            handle_auth_failure(&app, &e);
            return Err(e.to_string());
        }
    };

    let owned_ids: Vec<i64> = lib.games.iter().filter(|g| g.owned).map(|g| g.product_id).collect();
    let stamped_at = now_secs();
    app.mutate(|s| {
        s.wallet = Some(lib.wallet.clone());
        s.stamp_verified(&owned_ids, stamped_at);
        // A game the chain says is gone must lose its grace immediately, or a
        // seller keeps playing for up to 72 hours by staying offline.
        for g in lib.games.iter().filter(|g| !g.owned) {
            if let Some(local) = s.games.get_mut(&g.product_id.to_string()) {
                local.last_verified_at = 0;
            }
        }
    })?;

    let snapshot = app.snapshot();
    Ok(lib
        .games
        .into_iter()
        .map(|g| {
            let local = snapshot.get(g.product_id);
            let installable = g.platforms.iter().any(|p| p == platform);
            let installed = local.is_some();
            let installed_version = local.map(|l| l.build_version).unwrap_or(0);
            let has_exe = local.and_then(|l| l.exe_path.as_ref()).is_some();

            let decision = launch_decision(Some(g.owned), local.map(|l| l.last_verified_at).unwrap_or(0), stamped_at, OWNERSHIP_GRACE_SECS);
            let playable = installed && has_exe && matches!(decision, LaunchDecision::Allow);

            let blocked_reason = match &decision {
                LaunchDecision::Blocked(m) | LaunchDecision::NeedsVerification(m) => Some(m.to_string()),
                LaunchDecision::Allow if installed && !has_exe => {
                    Some("No runnable file was found in this build.".into())
                }
                LaunchDecision::Allow if !installable => {
                    Some(format!("No {platform} build has been uploaded for this game yet."))
                }
                LaunchDecision::Allow => None,
            };

            GameView {
                product_id: g.product_id,
                title: g.title,
                tagline: g.tagline,
                image: g.image,
                copy_number: g.copy_number,
                owned: g.owned,
                available_platforms: g.platforms,
                installable,
                installed,
                installed_version,
                latest_version: g.build_version,
                update_available: installed && g.build_version > installed_version,
                playable,
                blocked_reason,
                install_dir: local.map(|l| l.install_dir.clone()),
            }
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

#[derive(Serialize, Clone)]
struct StoreView {
    product_id: i64,
    title: String,
    tagline: Option<String>,
    image: Option<String>,
    price_cents: Option<i64>,
    runtime: Option<String>,
    genres: Vec<String>,
    studio: Option<String>,
    trusted: bool,
    /// Already phrased for display — "Open edition", "3 left" — so the UI never
    /// has to reason about supply_model, available and minted_count itself.
    supply: String,
    sold_out: bool,
    owned: bool,
    installed: bool,
}

/// The public catalog, marked up with what this wallet already owns.
///
/// The catalog call needs no device token, so an unpaired launcher can still
/// browse — it just can't mark anything as owned. Someone should be able to see
/// what's for sale before deciding to connect a wallet at all.
#[tauri::command]
async fn browse_store(app: State<'_, App>) -> Result<Vec<StoreView>, String> {
    let market = app.client.market().await.map_err(|e| e.to_string())?;

    let owned: std::collections::HashSet<i64> = match app.token() {
        Ok(token) => match app.client.library(&token).await {
            Ok(lib) => lib.games.iter().filter(|g| g.owned).map(|g| g.product_id).collect(),
            Err(e) => {
                // A failed library lookup must not blank the store. Browsing
                // still works; nothing is marked owned.
                handle_auth_failure(&app, &e);
                Default::default()
            }
        },
        Err(_) => Default::default(),
    };

    let snapshot = app.snapshot();

    Ok(market
        .games
        .into_iter()
        .map(|g| {
            let supply = match (g.supply_model.as_deref(), g.available, g.minted_count) {
                (Some("finite"), Some(left), _) if left > 0 => format!("{left} left"),
                (Some("finite"), _, _) => "Limited run".to_string(),
                (_, _, Some(n)) if n > 0 => format!("Open edition · {n} sold"),
                _ => "Open edition".to_string(),
            };
            StoreView {
                product_id: g.id,
                owned: owned.contains(&g.id),
                installed: snapshot.get(g.id).is_some(),
                studio: g.dev.as_ref().and_then(|d| d.studio.clone()),
                trusted: g.dev.as_ref().map(|d| d.trusted).unwrap_or(false),
                title: g.title,
                tagline: g.tagline,
                image: g.image,
                price_cents: g.price_cents,
                runtime: g.runtime,
                genres: g.genres,
                supply,
                sold_out: g.sold_out,
            }
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Install / play / remove
// ---------------------------------------------------------------------------

#[tauri::command]
async fn install_game(
    handle: AppHandle,
    app: State<'_, App>,
    product_id: i64,
    title: String,
) -> Result<(), String> {
    let token = app.token()?;
    let platform = api::current_platform();

    // Ownership is re-checked inside this call, server-side, against the chain.
    let ticket = match app.client.download(&token, product_id, platform).await {
        Ok(t) => t,
        Err(e) => {
            handle_auth_failure(&app, &e);
            return Err(e.to_string());
        }
    };

    let dir = app.install_dir(product_id);
    let tmp = app.app_data.join("downloads");

    let installed = install::download_and_install(
        &handle,
        product_id,
        &ticket.url,
        ticket.sha256.as_deref(),
        &dir,
        platform,
        &tmp,
    )
    .await
    .map_err(|e| e.to_string())?;

    let at = now_secs();
    app.mutate(|s| {
        s.upsert(InstalledGame {
            product_id,
            title,
            build_version: ticket.build_version,
            install_dir: installed.install_dir.to_string_lossy().to_string(),
            exe_path: installed.exe_path.clone(),
            installed_at: at,
            // The download itself was an ownership check that passed.
            last_verified_at: at,
            bytes: installed.bytes,
        })
    })?;

    Ok(())
}

/// The resale gate. Asks the server first; only falls back to the grace window
/// when the server genuinely can't be reached.
#[tauri::command]
async fn launch_game(app: State<'_, App>, product_id: i64) -> Result<(), String> {
    let local = app
        .snapshot()
        .get(product_id)
        .cloned()
        .ok_or_else(|| "That game isn't installed.".to_string())?;

    let owned_now: Option<bool> = match app.token() {
        Ok(token) => match app.client.library(&token).await {
            Ok(lib) => Some(
                lib.games
                    .iter()
                    .find(|g| g.product_id == product_id)
                    .map(|g| g.owned)
                    // Absent from the library at all = no copy.
                    .unwrap_or(false),
            ),
            Err(ApiError::Network(_)) => None, // genuinely offline
            Err(e) => {
                handle_auth_failure(&app, &e);
                None
            }
        },
        Err(_) => None, // not paired; grace window decides
    };

    match launch_decision(owned_now, local.last_verified_at, now_secs(), OWNERSHIP_GRACE_SECS) {
        LaunchDecision::Blocked(m) => {
            // Revoke grace so a later offline attempt can't sneak through.
            let _ = app.mutate(|s| {
                if let Some(g) = s.games.get_mut(&product_id.to_string()) {
                    g.last_verified_at = 0;
                }
            });
            return Err(m.to_string());
        }
        LaunchDecision::NeedsVerification(m) => return Err(m.to_string()),
        LaunchDecision::Allow => {}
    }

    if owned_now == Some(true) {
        let at = now_secs();
        let _ = app.mutate(|s| s.stamp_verified(&[product_id], at));
    }

    let exe = local
        .exe_path
        .ok_or_else(|| "No runnable file was found in this build.".to_string())?;

    // SDK ticket. Best-effort: offline (or a server hiccup) still launches the
    // game — it just runs without achievements/saves for this session, which
    // is the right trade. A 403 is different: the copy was sold, so stop.
    let mut env: Vec<(&str, String)> = vec![
        ("DROPRATE_API", "https://droprate.xyz/sdk/v1".to_string()),
        ("DROPRATE_PRODUCT", product_id.to_string()),
    ];
    if let Ok(token) = app.token() {
        match app.client.ticket(&token, product_id).await {
            Ok(t) => {
                env.push(("DROPRATE_TICKET", t.ticket));
                env.push(("DROPRATE_WALLET", t.wallet));
            }
            Err(ApiError::Server { status: 403, message }) => return Err(message),
            Err(e) => {
                handle_auth_failure(&app, &e);
                // no ticket; the game runs as a guest
            }
        }
    }
    install::launch(std::path::Path::new(&local.install_dir), &exe, &env).map_err(|e| e.to_string())
}

#[tauri::command]
fn uninstall_game(app: State<App>, product_id: i64) -> Result<(), String> {
    let dir = app.install_dir(product_id);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(|e| e.to_string())?;
    }
    app.mutate(|s| {
        s.remove(product_id);
    })
}

#[tauri::command]
fn open_install_folder(app: State<App>, product_id: i64) -> Result<(), String> {
    let dir = app.install_dir(product_id);
    if !dir.exists() {
        return Err("That folder no longer exists.".into());
    }
    let cmd = if cfg!(target_os = "windows") {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(cmd)
        .arg(&dir)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .filter(|h| !h.trim().is_empty())
        .unwrap_or_else(|| "My PC".to_string())
}

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            let app_data = app
                .path()
                .app_data_dir()
                .expect("no app data directory on this platform");
            std::fs::create_dir_all(&app_data).ok();

            let loaded = state::load(&app_data);
            app.manage(App {
                state: Mutex::new(loaded),
                pending_poll_token: Mutex::new(None),
                pending_pair_url: Mutex::new(None),
                app_data,
                client: Client::new().expect("failed to build http client"),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_status,
            pair_start,
            open_pair_page,
            pair_poll,
            pair_cancel,
            sign_out,
            refresh_library,
            browse_store,
            open_store_page,
            open_dev_portal,
            install_game,
            launch_game,
            uninstall_game,
            open_install_folder,
        ])
        .run(tauri::generate_context!())
        .expect("error while running DropRate Launcher");
}
