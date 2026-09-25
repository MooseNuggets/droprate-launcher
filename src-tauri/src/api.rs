//! DropRate API client.
//!
//! Every call rides the one endpoint the site already exposes:
//!     POST https://droprate.xyz/api/crate   with { ns: "devmarket", action, ... }
//!
//! This module is the ONLY place the device token is used, and it lives in Rust
//! on purpose. The webview never receives it, never stores it, and cannot read
//! it — a compromised page in the UI has nothing to steal. The token is also the
//! weakest credential we could ask for: it can list a library and fetch download
//! links, and it cannot spend, transfer, or sign anything.

use serde::{Deserialize, Serialize};
use std::time::Duration;

pub const API_URL: &str = "https://droprate.xyz/api/crate";

/// Longer than a page load because these hit a serverless function that may be
/// cold-starting a database. Short enough that a dead network still surfaces.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("network error: {0}")]
    Network(String),
    /// The server answered, and it answered with a refusal. `status` lets callers
    /// distinguish "you don't own this" (403) from "signed out" (401).
    #[error("{message}")]
    Server { status: u16, message: String },
    #[error("unexpected response: {0}")]
    Shape(String),
}

pub type ApiResult<T> = Result<T, ApiError>;

/// Errors come back as `{ "error": "..." }`; success bodies vary by action.
#[derive(Deserialize)]
struct ErrorBody {
    error: Option<String>,
}

pub struct Client {
    http: reqwest::Client,
}

impl Client {
    pub fn new() -> ApiResult<Self> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .user_agent(concat!("DropRateLauncher/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| ApiError::Network(e.to_string()))?;
        Ok(Self { http })
    }

    /// One request shape for every action. `body` must be a JSON object; `ns` and
    /// `action` are merged in here so no caller can forget them.
    async fn call<T: for<'de> Deserialize<'de>>(
        &self,
        action: &str,
        mut body: serde_json::Value,
    ) -> ApiResult<T> {
        {
            let obj = body
                .as_object_mut()
                .ok_or_else(|| ApiError::Shape("request body must be an object".into()))?;
            obj.insert("ns".into(), serde_json::json!("devmarket"));
            obj.insert("action".into(), serde_json::json!(action));
        }

        let res = self
            .http
            .post(API_URL)
            .json(&body)
            .send()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;

        let status = res.status().as_u16();
        let text = res
            .text()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;

        if !(200..300).contains(&status) {
            // Prefer the server's own wording — it is written for people
            // ("This wallet doesn't own a copy of that game.") and the UI shows it raw.
            let message = serde_json::from_str::<ErrorBody>(&text)
                .ok()
                .and_then(|e| e.error)
                .unwrap_or_else(|| format!("server error ({status})"));
            return Err(ApiError::Server { status, message });
        }

        serde_json::from_str::<T>(&text).map_err(|e| ApiError::Shape(e.to_string()))
    }

    // ---- pairing ----------------------------------------------------------

    pub async fn device_start(&self, name: &str, platform: &str) -> ApiResult<PairStart> {
        self.call(
            "native-device-start",
            serde_json::json!({ "name": name, "platform": platform }),
        )
        .await
    }

    pub async fn device_poll(&self, poll_token: &str) -> ApiResult<PairPoll> {
        self.call(
            "native-device-poll",
            serde_json::json!({ "poll_token": poll_token }),
        )
        .await
    }

    // ---- authenticated by device token ------------------------------------

    pub async fn device_me(&self, device_token: &str) -> ApiResult<DeviceMe> {
        self.call(
            "native-device-me",
            serde_json::json!({ "device_token": device_token }),
        )
        .await
    }

    pub async fn library(&self, device_token: &str) -> ApiResult<LibraryResponse> {
        self.call(
            "native-device-library",
            serde_json::json!({ "device_token": device_token }),
        )
        .await
    }

    /// Ownership is re-checked against the chain server-side on this call, so a
    /// 403 here is authoritative: the copy was sold.
    pub async fn download(
        &self,
        device_token: &str,
        product_id: i64,
        platform: &str,
    ) -> ApiResult<DownloadTicket> {
        self.call(
            "native-device-download",
            serde_json::json!({
                "device_token": device_token,
                "product_id": product_id,
                "platform": platform,
            }),
        )
        .await
    }

    /// The public storefront. Deliberately unauthenticated — browsing the catalog
    /// is something anyone can do on the website, so the launcher needs no device
    /// token to show it.
    /// An SDK ticket for one game: the credential the game itself uses for
    /// achievements, cloud saves and leaderboards. Ownership is re-checked
    /// server-side, so a 403 means the copy was sold since the last refresh.
    pub async fn ticket(&self, device_token: &str, product_id: i64) -> ApiResult<Ticket> {
        self.call(
            "native-device-ticket",
            serde_json::json!({ "device_token": device_token, "product_id": product_id }),
        )
        .await
    }

    pub async fn market(&self) -> ApiResult<MarketResponse> {
        self.call("native-market", serde_json::json!({})).await
    }

    /// Signs this device out. Best-effort: if it fails we still drop the local
    /// token, because the person asked to be signed out and the alternative is
    /// leaving a live credential on disk.
    pub async fn forget(&self, device_token: &str) -> ApiResult<serde_json::Value> {
        self.call(
            "native-device-forget",
            serde_json::json!({ "device_token": device_token }),
        )
        .await
    }
}

// ---- response shapes ------------------------------------------------------
// Mirrored from lib/devicepair.js. Unknown fields are ignored by serde, so the
// server can add to these responses without breaking an older launcher.

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct PairStart {
    pub code: String,
    pub poll_token: String,
    pub pair_url: String,
    pub expires_in: i64,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct PairPoll {
    /// "pending" | "paired" | "collected" | "expired" | "revoked"
    pub state: String,
    /// Present exactly once, on the first poll after approval.
    pub device_token: Option<String>,
    pub wallet: Option<String>,
    pub name: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct DeviceMe {
    pub wallet: String,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Ticket {
    pub ticket: String,
    pub wallet: String,
    pub product_id: i64,
    pub expires_at: i64,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct LibraryResponse {
    pub wallet: String,
    pub games: Vec<LibraryGame>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct LibraryGame {
    pub product_id: i64,
    pub title: String,
    #[serde(default)]
    pub slug: Option<String>,
    #[serde(default)]
    pub tagline: Option<String>,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub runtime: Option<String>,
    #[serde(default)]
    pub copy_number: Option<i64>,
    #[serde(default)]
    pub asset_address: Option<String>,
    /// Chain-verified server-side. False means the copy was sold or transferred.
    pub owned: bool,
    #[serde(default)]
    pub build_version: i64,
    #[serde(default)]
    pub platforms: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct DownloadTicket {
    pub url: String,
    pub expires_in: i64,
    #[serde(default)]
    pub bytes: Option<u64>,
    #[serde(default)]
    pub build_version: i64,
    /// Present once uploads record it. When it is present we verify it and refuse
    /// to install on a mismatch; when it is absent we install and say so.
    #[serde(default)]
    pub sha256: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct MarketResponse {
    pub games: Vec<StoreGame>,
}

/// Mirrors publicProduct() in lib/nativemarket.js — only the fields the store
/// renders. serde ignores the rest, so the server can keep adding to that shape
/// without breaking an older launcher.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct StoreGame {
    pub id: i64,
    pub title: String,
    #[serde(default)]
    pub tagline: Option<String>,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub price_cents: Option<i64>,
    #[serde(default)]
    pub runtime: Option<String>,
    #[serde(default)]
    pub genres: Vec<String>,
    #[serde(default)]
    pub supply_model: Option<String>,
    /// null when supply is infinite.
    #[serde(default)]
    pub available: Option<i64>,
    #[serde(default)]
    pub minted_count: Option<i64>,
    #[serde(default)]
    pub sold_out: bool,
    #[serde(default)]
    pub dev: Option<StoreDev>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct StoreDev {
    #[serde(default)]
    pub studio: Option<String>,
    #[serde(default)]
    pub trusted: bool,
}

/// What this build of the launcher can run. Sent at pairing time so the approval
/// screen in the browser can name the machine's platform, and used to pick which
/// build to ask for.
pub fn current_platform() -> &'static str {
    if cfg!(target_os = "windows") {
        "win"
    } else if cfg!(target_os = "macos") {
        "mac"
    } else {
        "linux"
    }
}
