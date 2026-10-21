//! TorBox debrid client (<https://api.torbox.app/v1/api>).
//!
//! Token: torbox.app -> Settings/API. Sent as `Authorization: Bearer <token>`.
//! Token never appears in error strings or logs. Known exception forced by
//! API design: `requestdl` endpoints take `token` as query param — full
//! requestdl URLs must never be logged (see `request_*_dl`).
//!
//! Rate limit per case study: 300/min, 60/h (exact scope unverified — see
//! TODOs). No client-side throttle here; lead should pace create calls.
//!
//! # LEAD INTEGRATION TODO
//!
//! 1. Wire module: `pub mod debrid_tb;` in `src/lib.rs` (not done here).
//! 2. Unified trait: suggested shape —
//!    ```ignore
//!    trait Debrid {
//!        async fn me(&self) -> Result<AccountInfo, String>; // plan/expiry/premium
//!        async fn hosters(&self) -> Result<Vec<Hoster>, String>;
//!        async fn submit_url(&self, url: &str) -> Result<JobId, String>;
//!        async fn submit_magnet(&self, magnet: &str) -> Result<JobId, String>;
//!        async fn check_cached(&self, hash: &str) -> Result<bool, String>;
//!        async fn list_files(&self, job: &JobId) -> Result<Vec<RemoteFile>, String>;
//!        async fn direct_link(&self, job: &JobId, file_id: i64) -> Result<String, String>;
//!    }
//!    ```
//!    `TbClient` methods map 1:1. Adapt names/types to shared model at wire-up.
//! 3. Redirect handling for `request_*_dl`: reqwest redirect policy is
//!    per-`Client`, not per-request. `new()` builds `http` with
//!    `Policy::none()`; harmless now (live requestdl returns JSON, below),
//!    kept so any `Location` fallback is returned raw, not followed.
//!    TODO(lead): if shared/proxied client injection needed later, add
//!    `TbClient::with_client(token, redirect_none_client)`. IP-binding:
//!    TorBox may bind generated link to requesting IP; keep
//!    create/request/download egress IP same.
//! 4. POST bodies: create endpoints reject JSON (verified below). This
//!    crate's reqwest has no `json`/`form`/`multipart` features, so JSON
//!    bodies are manual (`serde_json::to_string` + explicit header, same
//!    as `debrid_ad.rs`) and create calls build `multipart/form-data`
//!    manually (`post_multipart`). Never put token in body/query except
//!    requestdl token param (module docs top).
//!
//! # VERIFIED (2026-10-19, unauthenticated live probe)
//!
//! - Envelope: `{success: bool, error: null|string, detail: string, data: ...}`.
//! - `GET /webdl/hosters` public, 162 entries. Hoster fields: `id, name,
//!   domains[], url, icon, status: bool, type ("hoster"|"stream"), note,
//!   nsfw, daily_link_limit/used, daily_bandwidth_limit/used,
//!   per_link_size_limit, regex`. Rapidgator: domains
//!   `["rapidgator.net", "rg.to"]`, `status: false` at probe time.
//! - `GET /user/me`, `GET /torrents/checkcached` return HTTP 401 unauth.
//!
//! # VERIFIED (2026-10-19, authenticated live probe)
//!
//! - Create endpoints reject JSON: `POST /webdl/createwebdownload` wants
//!   multipart field `link`; `POST /torrents/createtorrent` wants multipart
//!   field `magnet` (200 `{hash, torrent_id, auth_id}`). Web-download
//!   success shape still unconfirmed: live hoster returned 500
//!   `TEMPORARILY_DISABLED` envelope after form parsing.
//! - `checkcached` hit shape: map `{hash: {name, size, hash}}`.
//! - `mylist` items: id field is `id`; `download_state="cached"`,
//!   `download_present=true`, `progress=1`; files carry `short_name`
//!   (basename; `name` may be a path).
//! - requestdl success: HTTP 200 JSON envelope `{success:true, data:"<bare
//!   CDN URL string>"}` — no redirect/`Location`. `Policy::none()` kept
//!   (harmless); `Location` still accepted if ever sent.
//! - Torrent delete: no `/torrents/deletetorrent` (404). Working:
//!   `POST /torrents/controltorrent` JSON `{"torrent_id":N,
//!   "operation":"delete"}`.
//! - Plans: 0=Free, 1=Essential, 3=Standard confirmed non-sequential.
//! - Expiry: field `premium_expires_at`, format `MM/DD/YYYY HH:MM:SS`.
//!
//! # UNVERIFIED / TODO (needs authed probe with real token)
//!
//! - TODO: createwebdownload success `data` shape (hoster disabled at
//!   probe); ids assumed `web_id`, also accept `id`.
//! - TODO: checkcached miss shape — parser accepts bool, object keyed by
//!   hash, single torrent object, or list. Hit shape confirmed above.
//! - TODO: error field type: string assumed; parser also stringifies
//!   numeric/object `error` + falls back to `detail`.
//! - TODO: plan 2 labelled "Premium?" (unverified — do not rely on
//!   ordering, tiers not ordinal). Preserve raw `plan` always.
//! - Deliberately unused: `GET /torrents/torrentinfo?hash=` (huge payload,
//!   ~2.13 GB example). Not called anywhere; no struct for it.

use serde_json::{json, Value};

pub const TB_BASE: &str = "https://api.torbox.app/v1/api";

/// Plan label. Mapping non-sequential (0,1,3 known) — never sort by plan int.
/// ponytail: 2 => "Premium?" is unverified guess; confirm with authed /user/me
/// fixtures for each tier, then drop "?".
pub fn plan_label(plan: i64) -> &'static str {
    match plan {
        0 => "Free",
        1 => "Essential",
        2 => "Premium?",
        3 => "Standard",
        _ => "Unknown",
    }
}

/// Parse RFC3339 / `YYYY-MM-DD HH:MM:SS` / `MM/DD/YYYY HH:MM:SS`
/// (live `premium_expires_at` format) / date-only to unix seconds (UTC).
/// No chrono dep — civil-date math below. Returns None on garbage.
pub fn parse_expiry(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(n) = s.parse::<i64>() {
        // Bare epoch: seconds, or millis if implausibly large.
        return Some(if n > 100_000_000_000 { n / 1000 } else { n });
    }
    let (date, time) = match s.split_once(['T', ' ']) {
        Some((d, t)) => (d, t.trim_end_matches('Z')),
        None => (s, "00:00:00"),
    };
    // ponytail: branch on separator instead of rewriting into ISO —
    // unambiguous here since MM/DD/YYYY always has two slashes.
    let (y, m, day) = if date.contains('/') {
        let mut d = date.splitn(3, '/');
        let m: i64 = d.next()?.parse().ok()?;
        let day: i64 = d.next()?.parse().ok()?;
        let y: i64 = d.next()?.parse().ok()?;
        (y, m, day)
    } else {
        let mut d = date.splitn(3, '-');
        let y: i64 = d.next()?.parse().ok()?;
        let m: i64 = d.next()?.parse().ok()?;
        let day: i64 = d.next()?.parse().ok()?;
        (y, m, day)
    };
    let mut t = time.splitn(3, ':');
    let hh: i64 = t.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    let mm: i64 = t.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    let ss: i64 = t
        .next()
        .map(|v| v.trim_end_matches(|c: char| !c.is_ascii_digit()))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) {
        return None;
    }
    // Days since epoch, Howard Hinnant civil algorithm.
    let yy = if m <= 2 { y - 1 } else { y };
    let era = if yy >= 0 { yy } else { yy - 399 } / 400;
    let yoe = yy - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86400 + hh * 3600 + mm * 60 + ss)
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct TbUser {
    pub email: Option<String>,
    /// Raw plan int preserved — mapping non-sequential, see `plan_label`.
    pub plan: i64,
    /// Raw expiry string as returned (any of premium_expires_at/expires_at/…).
    pub expires_raw: Option<String>,
    /// Parsed unix seconds, None if unparseable/absent.
    pub expires_at: Option<i64>,
    /// Raw `data` for fields not modelled yet.
    pub raw: Value,
}

impl TbUser {
    pub fn plan_label(&self) -> &'static str {
        plan_label(self.plan)
    }
    /// Premium = paid plan and not expired. Unknown expiry counts as active
    /// (avoids false-downgrade when API omits field).
    pub fn is_premium_at(&self, now: i64) -> bool {
        self.plan != 0 && self.expires_at.map(|e| e > now).unwrap_or(true)
    }
    pub fn is_premium(&self) -> bool {
        self.is_premium_at(now_unix())
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct TbHoster {
    pub id: i64,
    pub name: String,
    pub domains: Vec<String>,
    pub url: String,
    /// Live status at fetch time (false = hoster down per TorBox).
    pub status: bool,
    /// "hoster" | "stream" (other values passed through).
    pub kind: String,
    pub note: Option<String>,
    pub regex: Option<String>,
}

/// Host match by URL host against hoster `domains` (exact or subdomain).
/// ponytail: domain match only, ignores API `regex` (needs regex dep).
/// Upgrade: compile `regex` field if TorBox adds hosters domains[] misses.
pub fn host_supported(hosters: &[TbHoster], url: &str) -> bool {
    host_for(hosters, url).is_some()
}

pub fn host_for<'a>(hosters: &'a [TbHoster], url: &str) -> Option<&'a TbHoster> {
    let host = url_host(url)?;
    hosters.iter().find(|h| {
        h.domains.iter().any(|d| {
            let d = d.trim().trim_start_matches('.').to_ascii_lowercase();
            !d.is_empty() && (host == d || host.ends_with(&format!(".{d}")))
        })
    })
}

fn url_host(url: &str) -> Option<String> {
    let after = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let host = after
        .split(['/', '?', '#'])
        .next()?
        .rsplit('@')
        .next()?
        .split(':')
        .next()?
        .trim()
        .to_ascii_lowercase();
    if host.is_empty() {
        None
    } else {
        Some(host)
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct TbFile {
    pub id: i64,
    pub name: String,
    pub size: u64,
    /// Raw per-file payload (mimetype/shorturl variants unmodelled).
    pub raw: Value,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct TbWebDl {
    pub web_id: i64,
    pub name: String,
    pub files: Vec<TbFile>,
    /// Docs `download_state` string (no closed enum); empty if absent.
    pub download_state: String,
    /// Docs: file ready on TorBox CDN.
    pub download_present: bool,
    /// Docs: server-side fetch finished.
    pub download_finished: bool,
    /// 0.0..=1.0 server-side fetch progress, if reported.
    pub progress: Option<f64>,
    /// Server-side error text (`error` field), empty if none.
    pub error: String,
    pub raw: Value,
}

impl TbWebDl {
    pub fn ready(&self) -> bool {
        self.download_present || self.download_finished || !self.files.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct TbTorrentCreate {
    pub torrent_id: i64,
    pub hash: String,
    pub name: String,
    pub raw: Value,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct TbTorrent {
    pub torrent_id: i64,
    pub hash: String,
    pub name: String,
    pub size: u64,
    /// True when files present downloadable (`download_present`/ready state).
    pub download_ready: bool,
    /// 0.0..=1.0 if derivable, else None. See scale note in parser.
    pub progress: Option<f64>,
    pub files: Vec<TbFile>,
    pub raw: Value,
}

/// checkcached result. `name/size` filled when API returns torrent object.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TbCached {
    pub cached: bool,
    pub hash: String,
    pub name: String,
    pub size: u64,
    pub raw: Value,
}

// Manual Debug: token must not leak via {:?}.
impl std::fmt::Debug for TbClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TbClient")
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct TbClient {
    token: String,
    http: reqwest::Client,
    base: String,
}

impl TbClient {
    pub fn new(token: impl Into<String>) -> Self {
        Self::with_base(token, TB_BASE)
    }

    pub fn with_base(token: impl Into<String>, base: &str) -> Self {
        // redirect Policy::none: kept so any requestdl Location is returned
        // raw; live requestdl is JSON, so harmless (module docs #3).
        let http = reqwest::Client::builder()
            .user_agent(concat!("webcraft/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(std::time::Duration::from_secs(15))
            .timeout(std::time::Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_default();
        Self {
            token: token.into(),
            http,
            base: base.trim_end_matches('/').to_string(),
        }
    }

    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        req.bearer_auth(&self.token)
    }

    /// GET JSON envelope -> `data`. Endpoint name only in errors, never URL.
    async fn get_data(&self, path: &str, query: &[(&str, String)]) -> Result<Value, String> {
        let resp = self
            .auth(self.http.get(format!("{}{}", self.base, path)))
            .query(query)
            .send()
            .await
            .map_err(|e| format!("TorBox {path}: {}", sanitize(e.to_string())))?;
        parse_envelope(resp, path).await
    }

    /// POST single-field multipart body -> `data`. Manual boundary/body:
    /// crate reqwest has no multipart feature. Field value never logged.
    async fn post_multipart(&self, path: &str, field: &str, value: &str) -> Result<Value, String> {
        // Random-ish boundary from time+pid; collision with value irrelevant
        // in practice (value is URL/magnet text, never binary).
        let boundary = format!(
            "----webcraft{}x{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
            std::process::id()
        );
        let body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{field}\"\r\n\r\n{value}\r\n--{boundary}--\r\n"
        );
        let resp = self
            .auth(self.http.post(format!("{}{}", self.base, path)))
            .header(
                reqwest::header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(body)
            .send()
            .await
            .map_err(|e| format!("TorBox {path}: {}", sanitize(e.to_string())))?;
        parse_envelope(resp, path).await
    }

    /// POST JSON body -> `data`. Never form-encoded, token only in header.
    async fn post_data(&self, path: &str, body: Value) -> Result<Value, String> {
        // Crate reqwest lacks `json` feature: manual body + header.
        let text = serde_json::to_string(&body).map_err(|e| e.to_string())?;
        let resp = self
            .auth(self.http.post(format!("{}{}", self.base, path)))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(text)
            .send()
            .await
            .map_err(|e| format!("TorBox {path}: {}", sanitize(e.to_string())))?;
        parse_envelope(resp, path).await
    }

    pub async fn me(&self) -> Result<TbUser, String> {
        let data = self.get_data("/user/me", &[]).await?;
        Ok(parse_user(&data))
    }

    pub async fn hosters(&self) -> Result<Vec<TbHoster>, String> {
        let data = self.get_data("/webdl/hosters", &[]).await?;
        parse_hosters(&data)
    }

    pub async fn create_web_download(&self, url: &str) -> Result<TbWebDl, String> {
        if url.trim().is_empty() {
            return Err("TorBox createwebdownload: empty url".into());
        }
        // Live-verified: multipart field `link`; JSON body rejected.
        let data = self
            .post_multipart("/webdl/createwebdownload", "link", url)
            .await?;
        // Create payload may carry id only; fall back to list lookup.
        let mut web = parse_web_dl(&data);
        if web.web_id == 0 {
            return Err("TorBox createwebdownload: no web_id in response".into());
        }
        if web.files.is_empty() {
            if let Ok(list) = self.web_list(Some(web.web_id)).await {
                if let Some(found) = list.into_iter().next() {
                    web = found;
                }
            }
        }
        Ok(web)
    }

    pub async fn web_list(&self, id: Option<i64>) -> Result<Vec<TbWebDl>, String> {
        let query: Vec<(&str, String)> =
            id.map(|i| vec![("id", i.to_string())]).unwrap_or_default();
        let data = self.get_data("/webdl/mylist", &query).await?;
        Ok(parse_web_list(&data))
    }

    /// Returns CDN URL (live: JSON envelope `data` string, see
    /// `parse_location`). Token travels in query per API design — never
    /// log returned request URL.
    pub async fn request_web_dl(&self, web_id: i64, file_id: i64) -> Result<String, String> {
        self.request_dl("/webdl/requestdl", "web_id", web_id, file_id)
            .await
    }

    pub async fn create_torrent(&self, magnet: &str) -> Result<TbTorrentCreate, String> {
        if !magnet.trim_start().starts_with("magnet:") {
            return Err("TorBox createtorrent: not a magnet link".into());
        }
        // Live-verified: multipart field `magnet` (JSON / `file=` rejected).
        let data = self
            .post_multipart("/torrents/createtorrent", "magnet", magnet)
            .await?;
        let created = parse_torrent_create(&data);
        if created.torrent_id == 0 && created.hash.is_empty() {
            return Err("TorBox createtorrent: no torrent_id/hash in response".into());
        }
        Ok(created)
    }

    /// `hash`: 40-hex infohash (case-insensitive). GET per case study;
    /// endpoint also accepts POST — GET keeps no-body simplicity.
    pub async fn check_cached(&self, hash: &str) -> Result<TbCached, String> {
        let hash = hash.trim().to_ascii_lowercase();
        if hash.len() != 40 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("TorBox checkcached: hash must be 40 hex chars".into());
        }
        let data = self
            .get_data("/torrents/checkcached", &[("hash", hash.clone())])
            .await?;
        Ok(parse_cached(&data, &hash))
    }

    pub async fn torrent_list(&self, id: Option<i64>) -> Result<Vec<TbTorrent>, String> {
        let query: Vec<(&str, String)> =
            id.map(|i| vec![("id", i.to_string())]).unwrap_or_default();
        let data = self.get_data("/torrents/mylist", &query).await?;
        Ok(parse_torrent_list(&data))
    }

    /// Delete torrent. Live-verified: no `/torrents/deletetorrent` (404);
    /// control endpoint takes JSON `{"torrent_id":N,"operation":"delete"}`.
    /// Eventually consistent (~5s): torrent may still appear in `mylist`
    /// briefly after success.
    pub async fn delete_torrent(&self, torrent_id: i64) -> Result<(), String> {
        self.post_data(
            "/torrents/controltorrent",
            json!({ "torrent_id": torrent_id, "operation": "delete" }),
        )
        .await?;
        Ok(())
    }

    /// Returns CDN URL (live: JSON envelope `data` string, see
    /// `parse_location`). Token travels in query per API design — never
    /// log returned request URL.
    pub async fn request_torrent_dl(
        &self,
        torrent_id: i64,
        file_id: i64,
    ) -> Result<String, String> {
        self.request_dl("/torrents/requestdl", "torrent_id", torrent_id, file_id)
            .await
    }

    async fn request_dl(
        &self,
        path: &str,
        id_key: &str,
        id: i64,
        file_id: i64,
    ) -> Result<String, String> {
        // No bearer header here: docs specify token query param for requestdl.
        // Query built via .query() (percent-encoded), never string-logged.
        let resp = self
            .http
            .get(format!("{}{}", self.base, path))
            .query(&[
                ("token", self.token.as_str()),
                (id_key, &id.to_string()),
                ("file_id", &file_id.to_string()),
            ])
            .send()
            .await
            .map_err(|e| format!("TorBox {path}: {}", sanitize(e.to_string())))?;
        parse_location(resp, path).await
    }
}

/// Strip anything URL-shaped from transport errors (reqwest errors can embed
/// request URL, which for requestdl contains token).
fn sanitize(msg: String) -> String {
    msg.split_whitespace()
        .map(|w| {
            if w.contains("://") || w.contains("token=") {
                "<url>"
            } else {
                w
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// requestdl response: live-verified HTTP 200 JSON envelope with `data`
/// as bare CDN URL string (no redirect). `Location` header still honoured
/// first if ever sent; 200/204 without URL = not ready.
pub async fn parse_location(resp: reqwest::Response, path: &str) -> Result<String, String> {
    let status = resp.status();
    if let Some(loc) = resp
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
    {
        if !loc.is_empty() {
            return Ok(loc.to_string());
        }
    }
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(format!("TorBox {path}: HTTP 401 unauthorized"));
    }
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(format!("TorBox {path}: HTTP 429 rate limited"));
    }
    if status.is_redirection() {
        return Err(format!(
            "TorBox {path}: HTTP {} redirect without Location",
            status.as_u16()
        ));
    }
    // Non-redirect: may be JSON envelope (success or error) — reuse parser,
    // then extract URL from data.
    let data = parse_envelope(resp, path).await?;
    extract_url(&data).ok_or_else(|| {
        format!("TorBox {path}: no download URL in response (not ready or unknown shape)")
    })
}

fn extract_url(data: &Value) -> Option<String> {
    match data {
        Value::String(s) if s.starts_with("http") => Some(s.clone()),
        Value::Object(m) => ["url", "link", "download_url", "location"]
            .iter()
            .find_map(|k| m.get(*k).and_then(Value::as_str).map(str::to_string))
            .filter(|s| s.starts_with("http")),
        _ => None,
    }
}

/// Envelope: `{success, error, detail, data}`. HTTP status checked first for
/// 401/429 (body may not be envelope), then success flag, then data presence.
pub async fn parse_envelope(resp: reqwest::Response, path: &str) -> Result<Value, String> {
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("TorBox {path}: {}", sanitize(e.to_string())))?;
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(envelope_error(&text)
            .unwrap_or_else(|| format!("TorBox {path}: HTTP 401 unauthorized")));
    }
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(envelope_error(&text)
            .unwrap_or_else(|| format!("TorBox {path}: HTTP 429 rate limited")));
    }
    let v: Value = serde_json::from_str(&text)
        .map_err(|e| format!("TorBox {path}: HTTP {} invalid JSON: {e}", status.as_u16()))?;
    check_envelope(&v, path, status.as_u16())?;
    Ok(v.get("data").cloned().unwrap_or(Value::Null))
}

fn envelope_error(text: &str) -> Option<String> {
    let v: Value = serde_json::from_str(text).ok()?;
    envelope_error_value(&v)
}

fn envelope_error_value(v: &Value) -> Option<String> {
    let err = v.get("error")?;
    let msg = match err {
        Value::String(s) if !s.is_empty() => s.clone(),
        Value::Null => return None,
        other => other.to_string(),
    };
    Some(msg)
}

fn check_envelope(v: &Value, path: &str, http: u16) -> Result<(), String> {
    let success = v.get("success").and_then(Value::as_bool);
    if success == Some(false) {
        let msg = envelope_error_value(v)
            .or_else(|| v.get("detail").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_else(|| "unknown error".into());
        return Err(format!("TorBox {path}: {msg}"));
    }
    if success.is_none() && !(200..300).contains(&http) {
        return Err(format!("TorBox {path}: HTTP {http}"));
    }
    Ok(())
}

// ---------- pure parsers (fixture-tested) ----------

pub fn parse_user(data: &Value) -> TbUser {
    let expires_raw = [
        "premium_expires_at",
        "expires_at",
        "premium_expiry",
        "expiry_date",
        "expires",
    ]
    .iter()
    .find_map(|k| match data.get(*k) {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    });
    TbUser {
        email: data
            .get("email")
            .and_then(Value::as_str)
            .map(str::to_string),
        plan: data.get("plan").and_then(as_i64).unwrap_or(0),
        expires_at: expires_raw.as_deref().and_then(parse_expiry),
        expires_raw,
        raw: data.clone(),
    }
}

pub fn parse_hosters(data: &Value) -> Result<Vec<TbHoster>, String> {
    let arr = match data {
        Value::Array(a) => a,
        Value::Object(m) => m
            .get("hosters")
            .or_else(|| m.get("list"))
            .and_then(Value::as_array)
            .ok_or_else(|| "TorBox hosters: unexpected data shape".to_string())?,
        _ => return Err("TorBox hosters: unexpected data shape".into()),
    };
    Ok(arr.iter().map(parse_hoster).collect())
}

fn parse_hoster(v: &Value) -> TbHoster {
    TbHoster {
        id: pick_i64(v, &["id"]),
        name: pick_str(v, &["name"]),
        domains: match v.get("domains") {
            Some(Value::Array(a)) => a
                .iter()
                .filter_map(|d| d.as_str().map(str::to_string))
                .collect(),
            Some(Value::String(s)) => vec![s.clone()],
            _ => Vec::new(),
        },
        url: pick_str(v, &["url"]),
        status: pick_bool(v, &["status"]).unwrap_or(false),
        kind: pick_str(v, &["type", "kind"]),
        note: v.get("note").and_then(Value::as_str).map(str::to_string),
        regex: v.get("regex").and_then(Value::as_str).map(str::to_string),
    }
}

pub fn parse_web_list(data: &Value) -> Vec<TbWebDl> {
    as_list(data).iter().map(parse_web_dl).collect()
}

pub fn parse_web_dl(v: &Value) -> TbWebDl {
    TbWebDl {
        // Docs: create returns `data.webdownload_id` (string); mylist
        // items use `id` (number). pick_i64 accepts both shapes.
        web_id: pick_i64(v, &["webdownload_id", "web_id", "id"]),
        name: pick_str(v, &["name", "title"]),
        files: v
            .get("files")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(parse_file).collect())
            .unwrap_or_default(),
        download_state: pick_str(v, &["download_state"]),
        download_present: pick_bool(v, &["download_present"]).unwrap_or(false),
        download_finished: pick_bool(v, &["download_finished"]).unwrap_or(false),
        progress: pick_f64(v, &["progress"]),
        error: pick_str(v, &["error"]),
        raw: v.clone(),
    }
}

pub fn parse_torrent_create(v: &Value) -> TbTorrentCreate {
    TbTorrentCreate {
        torrent_id: pick_i64(v, &["torrent_id", "id"]),
        hash: pick_str(v, &["hash", "infohash", "info_hash"]).to_ascii_lowercase(),
        name: pick_str(v, &["name", "title"]),
        raw: v.clone(),
    }
}

pub fn parse_cached(data: &Value, hash: &str) -> TbCached {
    let miss = |raw: Value| TbCached {
        cached: false,
        hash: hash.to_string(),
        raw,
        ..Default::default()
    };
    match data {
        Value::Bool(b) => TbCached {
            cached: *b,
            hash: hash.to_string(),
            raw: data.clone(),
            ..Default::default()
        },
        Value::Null => miss(data.clone()),
        Value::Array(a) => {
            if a.is_empty() {
                miss(data.clone())
            } else {
                // List form: hit = entry mentioning hash, else first entry.
                let hit = a
                    .iter()
                    .find(|e| pick_str(e, &["hash", "infohash"]).eq_ignore_ascii_case(hash))
                    .unwrap_or(&a[0]);
                cached_from_obj(hit, hash, data)
            }
        }
        Value::Object(m) => {
            // Map form: {"<hash>": {...}} or {"<hash>": true/false}.
            if let Some(entry) = m
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(hash))
                .map(|(_, v)| v)
            {
                return match entry {
                    Value::Bool(b) => TbCached {
                        cached: *b,
                        hash: hash.to_string(),
                        raw: data.clone(),
                        ..Default::default()
                    },
                    Value::Null => miss(data.clone()),
                    obj => cached_from_obj(obj, hash, data),
                };
            }
            if m.is_empty() {
                return miss(data.clone());
            }
            // Single-object form; explicit false flags still mean miss.
            if pick_bool(data, &["cached", "found", "available"]) == Some(false) {
                return miss(data.clone());
            }
            cached_from_obj(data, hash, data)
        }
        _ => miss(data.clone()),
    }
}

fn cached_from_obj(obj: &Value, hash: &str, raw: &Value) -> TbCached {
    // Object with name/files and no explicit false flag counts as hit.
    TbCached {
        cached: true,
        hash: pick_str(obj, &["hash", "infohash"])
            .to_ascii_lowercase()
            .if_empty(hash),
        name: pick_str(obj, &["name", "title"]),
        size: pick_u64(obj, &["size", "total_size", "bytes"]),
        raw: raw.clone(),
    }
}

pub fn parse_torrent_list(data: &Value) -> Vec<TbTorrent> {
    as_list(data).iter().map(parse_torrent).collect()
}

pub fn parse_torrent(v: &Value) -> TbTorrent {
    let files = v
        .get("files")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(parse_file).collect::<Vec<_>>())
        .unwrap_or_default();
    let download_ready = pick_bool(v, &["download_present", "download_ready", "ready"])
        .unwrap_or_else(|| {
            let state = pick_str(v, &["download_state", "state", "status"]).to_ascii_lowercase();
            !files.is_empty()
                && (state.contains("finish")
                    || state.contains("ready")
                    || state.contains("complete")
                    || state.contains("downloaded")
                    || state == "download")
        });
    // Progress scale unverified: >1.0 treated as percent.
    let progress = pick_f64(v, &["progress", "download_progress", "percent"]).map(|p| {
        if p > 1.0 {
            (p / 100.0).clamp(0.0, 1.0)
        } else {
            p.clamp(0.0, 1.0)
        }
    });
    TbTorrent {
        torrent_id: pick_i64(v, &["torrent_id", "id"]),
        hash: pick_str(v, &["hash", "infohash", "info_hash"]).to_ascii_lowercase(),
        name: pick_str(v, &["name", "title"]),
        size: pick_u64(v, &["size", "total_size", "bytes"]),
        download_ready,
        progress,
        files,
        raw: v.clone(),
    }
}

fn parse_file(v: &Value) -> TbFile {
    TbFile {
        id: pick_i64(v, &["id", "file_id"]),
        // Live: `short_name` is basename; `name` may be full path — prefer it.
        name: pick_str(v, &["short_name", "name", "filename", "file_name"]),
        size: pick_u64(v, &["size", "bytes", "length"]),
        raw: v.clone(),
    }
}

/// `data` may be bare array, `{list|items|torrents|web_downloads: [...]}`,
/// or single object.
fn as_list(data: &Value) -> Vec<Value> {
    match data {
        Value::Array(a) => a.clone(),
        Value::Object(m) => {
            for k in ["list", "items", "torrents", "web_downloads", "downloads"] {
                if let Some(a) = m.get(k).and_then(Value::as_array) {
                    return a.clone();
                }
            }
            if m.is_empty() {
                Vec::new()
            } else {
                vec![data.clone()]
            }
        }
        _ => Vec::new(),
    }
}

fn as_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn pick_i64(v: &Value, keys: &[&str]) -> i64 {
    keys.iter()
        .find_map(|k| v.get(*k).and_then(as_i64))
        .unwrap_or(0)
}

fn pick_u64(v: &Value, keys: &[&str]) -> u64 {
    keys.iter()
        .find_map(|k| v.get(*k).and_then(as_i64))
        .filter(|n| *n >= 0)
        .map(|n| n as u64)
        .unwrap_or(0)
}

fn pick_f64(v: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|k| match v.get(*k) {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    })
}

fn pick_bool(v: &Value, keys: &[&str]) -> Option<bool> {
    keys.iter().find_map(|k| match v.get(*k) {
        Some(Value::Bool(b)) => Some(*b),
        Some(Value::Number(n)) => Some(n.as_i64().unwrap_or(0) != 0),
        Some(Value::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" => Some(true),
            "false" | "0" | "no" => Some(false),
            _ => None,
        },
        _ => None,
    })
}

fn pick_str(v: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|k| v.get(*k).and_then(Value::as_str))
        .unwrap_or("")
        .to_string()
}

trait IfEmpty {
    fn if_empty(self, fallback: &str) -> String;
}
impl IfEmpty for String {
    fn if_empty(self, fallback: &str) -> String {
        if self.is_empty() {
            fallback.to_string()
        } else {
            self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const RAPIDGATOR: &str =
        "https://rapidgator.net/file/b6df95dee983c06f41b3caa31755a9fe/Cas1tlevBelm7ontsCur4se-elamigos.part1.rar.html";
    const HASH: &str = "b38e5b966a72f86834ef0efb59388d636e82473b";

    #[test]
    fn plan_labels_preserve_non_sequential_mapping() {
        assert_eq!(plan_label(0), "Free");
        assert_eq!(plan_label(1), "Essential");
        assert_eq!(plan_label(2), "Premium?");
        assert_eq!(plan_label(3), "Standard");
        assert_eq!(plan_label(99), "Unknown");
        assert_eq!(plan_label(-1), "Unknown");
    }

    #[test]
    fn user_free_and_premium_expiry() {
        let free = parse_user(&json!({"email":"a@b.c","plan":0,"premium_expires_at":null}));
        assert_eq!(free.email.as_deref(), Some("a@b.c"));
        assert_eq!(free.plan, 0);
        assert_eq!(free.plan_label(), "Free");
        assert!(!free.is_premium_at(1_700_000_000));

        // 2027-01-01T00:00:00Z = 1798761600
        let pro = parse_user(&json!({"plan":3,"premium_expires_at":"2027-01-01T00:00:00Z"}));
        assert_eq!(pro.plan_label(), "Standard");
        assert_eq!(pro.expires_at, Some(1_798_761_600));
        assert!(pro.is_premium_at(1_700_000_000));
        assert!(!pro.is_premium_at(1_800_000_000));

        // expires_at fallback field + essential tier.
        let ess = parse_user(&json!({"plan":1,"expires_at":"2020-06-01 12:00:00"}));
        assert_eq!(ess.plan_label(), "Essential");
        assert_eq!(ess.expires_at, Some(1_591_012_800));
        assert!(!ess.is_premium_at(1_700_000_000));

        // Paid, no expiry field -> counts active (no false downgrade).
        let noexp = parse_user(&json!({"plan":2}));
        assert_eq!(noexp.plan_label(), "Premium?");
        assert!(noexp.is_premium_at(1_700_000_000));
    }

    #[test]
    fn expiry_parser_variants() {
        // Live premium_expires_at shape: MM/DD/YYYY HH:MM:SS (UTC).
        assert_eq!(parse_expiry("08/15/2027 14:31:01"), Some(1_818_340_261));
        assert_eq!(parse_expiry("2027-01-01T00:00:00Z"), Some(1_798_761_600));
        assert_eq!(parse_expiry("2027-01-01"), Some(1_798_761_600));
        assert_eq!(parse_expiry("1798761600"), Some(1_798_761_600));
        assert_eq!(parse_expiry("1798761600000"), Some(1_798_761_600));
        assert_eq!(parse_expiry(""), None);
        assert_eq!(parse_expiry("not-a-date"), None);
        assert_eq!(parse_expiry("2027-13-40T00:00:00Z"), None);
    }

    fn fixture_hosters() -> Vec<TbHoster> {
        // Trimmed from live 2026-10-19 /webdl/hosters (162 entries).
        parse_hosters(&json!([
            {"id":1,"name":"Rapidgator","domains":["rapidgator.net","rg.to"],
             "url":"https://rapidgator.net","status":false,"type":"hoster","note":null,
             "regex":"^https?://(?:[^./]+\\.)*(rapidgator\\.net|rg\\.to)(/|$)"},
            {"id":2,"name":"1Fichier","domains":["1fichier.com"],
             "url":"https://1fichier.com","status":true,"type":"hoster"}
        ]))
        .unwrap()
    }

    #[test]
    fn hosters_parse_and_rapidgator_matching() {
        let hosters = fixture_hosters();
        assert_eq!(hosters.len(), 2);
        assert_eq!(hosters[0].name, "Rapidgator");
        assert!(!hosters[0].status);
        assert!(host_supported(&hosters, RAPIDGATOR));
        assert!(host_supported(&hosters, "https://rg.to/file/abc"));
        assert!(host_supported(
            &hosters,
            "https://www.rapidgator.net/file/x"
        ));
        assert!(host_supported(&hosters, "https://1fichier.com/?abc"));
        assert!(!host_supported(
            &hosters,
            "https://rapidgator.net.evil.example/file/x"
        ));
        assert!(!host_supported(&hosters, "https://example.com/file/x"));
        assert!(!host_supported(&hosters, "not a url"));
        assert_eq!(
            host_for(&hosters, RAPIDGATOR).map(|h| h.name.as_str()),
            Some("Rapidgator")
        );
    }

    #[test]
    fn envelope_error_and_success_flag() {
        let err = json!({"success":false,"error":"Invalid link","detail":"x","data":null});
        assert_eq!(
            check_envelope(&err, "/webdl/createwebdownload", 200).unwrap_err(),
            "TorBox /webdl/createwebdownload: Invalid link"
        );
        // detail fallback when error null.
        let err2 = json!({"success":false,"error":null,"detail":"Boom detail","data":null});
        assert!(check_envelope(&err2, "/x", 200)
            .unwrap_err()
            .contains("Boom detail"));
        // Numeric error code stringified.
        let err3 = json!({"success":false,"error":403,"detail":"Forbidden","data":null});
        assert!(check_envelope(&err3, "/x", 200)
            .unwrap_err()
            .contains("403"));
        assert!(check_envelope(&json!({"success":true,"error":null,"data":{}}), "/x", 200).is_ok());
    }

    #[test]
    fn cached_hit_and_miss_shapes() {
        // Map-by-hash object hit.
        let hit = parse_cached(
            &json!({HASH: {"name":"FitGirl Repack","size":2_280_000_000u64}}),
            HASH,
        );
        assert!(hit.cached);
        assert_eq!(hit.name, "FitGirl Repack");
        assert_eq!(hit.size, 2_280_000_000);
        // Bare bool.
        assert!(parse_cached(&json!(true), HASH).cached);
        assert!(!parse_cached(&json!(false), HASH).cached);
        // Misses: null, empty obj, empty list, map false.
        assert!(!parse_cached(&Value::Null, HASH).cached);
        assert!(!parse_cached(&json!({}), HASH).cached);
        assert!(!parse_cached(&json!([]), HASH).cached);
        assert!(!parse_cached(&json!({HASH: false}), HASH).cached);
        // Single object = hit, hash preserved lowercase.
        let single = parse_cached(&json!({"hash": HASH.to_uppercase(), "name":"X"}), HASH);
        assert!(single.cached);
        assert_eq!(single.hash, HASH);
    }

    #[test]
    fn torrent_list_multi_file_ready_and_not() {
        let list = parse_torrent_list(&json!([
            {"id":42,"hash":HASH,"name":"Game [FitGirl]","size":4560,
             "download_present":true,"progress":1.0,
             "files":[{"id":1,"name":"setup.exe","size":1000},
                      {"id":2,"name":"data.bin","size":3560,"mimetype":"application/octet-stream"}]},
            {"id":43,"hash":"aaaabbbbccccddddeeeeffff0000111122223333","name":"Pending",
             "download_state":"downloading","progress":45.0,
             "files":[{"id":1,"name":"part.rar","size":10}]}
        ]));
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].torrent_id, 42);
        assert!(list[0].download_ready);
        assert_eq!(list[0].progress, Some(1.0));
        assert_eq!(list[0].files.len(), 2);
        assert_eq!(list[0].files[1].name, "data.bin");
        assert_eq!(list[0].files[1].size, 3560);
        assert!(!list[1].download_ready);
        assert_eq!(list[1].progress, Some(0.45)); // percent scale normalized
    }

    #[test]
    fn web_create_and_list_parse() {
        let created = parse_web_dl(&json!({"web_id":7,"name":"file.rar",
            "files":[{"id":3,"name":"file.rar","size":123}]}));
        assert_eq!(created.web_id, 7);
        assert_eq!(created.files[0].id, 3);
        // id-only create payload + wrapped list.
        let listed = parse_web_list(&json!({"list":[
            {"id":8,"files":[{"file_id":9,"filename":"a.zip","bytes":5}]}]}));
        assert_eq!(listed[0].web_id, 8);
        assert_eq!(listed[0].files[0].id, 9);
        assert_eq!(listed[0].files[0].name, "a.zip");
        assert_eq!(listed[0].files[0].size, 5);
    }

    #[test]
    fn torrent_create_parse() {
        let c = parse_torrent_create(&json!({"torrent_id":99,"hash":HASH.to_uppercase()}));
        assert_eq!(c.torrent_id, 99);
        assert_eq!(c.hash, HASH);
    }

    #[test]
    fn sanitize_strips_urls_and_tokens() {
        let s =
            sanitize("error sending request for url https://x/requestdl?token=SECRET done".into());
        assert!(!s.contains("SECRET"));
        assert!(!s.contains("https://"));
        assert!(s.contains("done"));
    }

    #[test]
    fn web_dl_docs_shapes_and_readiness() {
        // Create response (docs): data.webdownload_id is a STRING.
        let created = parse_web_dl(&json!({"webdownload_id": "123", "name": "f.mkv"}));
        assert_eq!(created.web_id, 123);
        assert!(!created.ready());
        // Mylist item: numeric id + state fields.
        let fetching = parse_web_dl(&json!({
            "id": 7, "name": "f.mkv", "download_state": "downloading",
            "download_present": false, "download_finished": false,
            "progress": 0.42, "error": "", "files": []
        }));
        assert_eq!(fetching.web_id, 7);
        assert_eq!(fetching.download_state, "downloading");
        assert_eq!(fetching.progress, Some(0.42));
        assert!(!fetching.ready());
        // Ready: files present.
        let ready = parse_web_dl(&json!({
            "id": 7, "download_present": true, "download_finished": true,
            "files": [{"id": 1, "name": "f.mkv", "size": 10}]
        }));
        assert!(ready.ready());
        // Server-side error surfaces verbatim.
        let failed = parse_web_dl(&json!({"id": 7, "error": "hoster unreachable"}));
        assert_eq!(failed.error, "hoster unreachable");
    }
}
