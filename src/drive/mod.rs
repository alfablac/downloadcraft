//! Drives: native cloud/server storage backends behind one trait.
//! No rclone sidecar. Google/OneDrive/Dropbox = REST+OAuth2 (user-supplied
//! client_id, tokens plain in state.json), S3 = SigV4, WebDAV = PROPFIND,
//! SFTP/FTP reuse existing transports, Teldrive = user server REST /v1,
//! Local = filesystem (dual-pane local side + tests).
pub mod crypt;
pub mod dropbox;
pub mod ftpdrive;
pub mod gdrive;
pub mod http_util;
pub mod jobs;
pub mod local;
pub mod oauth;
pub mod onedrive;
pub mod rclone_import;
pub mod s3;
pub mod sftpdrive;
pub mod sync;
pub mod teldrive;
pub mod webdav;

use crate::model::{DriveAccount, DriveKind};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

/// One remote entry. `id` = opaque provider id (drive file id, s3 key,
/// path for path-based backends). `path` = display path from account root.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct RemoteEntry {
    pub id: String,
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    /// Unix seconds, 0 = unknown.
    pub modified: u64,
    pub mime: String,
}

/// Transfer progress sink shared by backends and jobs runner.
#[derive(Clone, Default)]
pub struct Progress {
    pub done: Arc<AtomicU64>,
    pub total: Arc<AtomicU64>,
    pub cancelled: Arc<AtomicBool>,
    pub paused: Arc<AtomicBool>,
}
impl Progress {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn add(&self, n: u64) {
        self.done.fetch_add(n, Ordering::Relaxed);
    }
    pub fn set_total(&self, n: u64) {
        self.total.store(n, Ordering::Relaxed);
    }
    /// Keep largest total (parallel per-file downloads share one Progress).
    pub fn grow_total(&self, n: u64) {
        self.total.fetch_max(n, Ordering::Relaxed);
    }
    pub fn check(&self) -> Result<(), String> {
        if self.cancelled.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        Ok(())
    }
    pub async fn wait(&self) {
        while self.paused.load(Ordering::Relaxed) && !self.cancelled.load(Ordering::Relaxed) {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        }
    }
}

/// Direct playable source for embedded mpv: URL + extra HTTP headers
/// (auth). Empty headers = URL self-authenticates (presigned/temp link).
#[derive(Clone, Debug, PartialEq, Default)]
pub struct StreamSource {
    pub url: String,
    pub headers: Vec<(String, String)>,
}

/// Backend seam: all operations take display `path` ("" or "/" = root).
/// Implementations map path <-> provider ids internally.
#[async_trait::async_trait]
pub trait DriveBackend: Send + Sync {
    fn kind(&self) -> DriveKind;
    /// Account/storage label for quota line ("" = unsupported).
    async fn about(&self) -> Result<String, String> {
        Ok(String::new())
    }
    async fn list(&self, path: &str) -> Result<Vec<RemoteEntry>, String>;
    async fn mkdir(&self, path: &str) -> Result<(), String>;
    async fn delete(&self, entry: &RemoteEntry) -> Result<(), String>;
    async fn rename(&self, entry: &RemoteEntry, new_name: &str) -> Result<(), String>;
    /// Stream download to local file with parallel ranged reads where
    /// possible. Backends without range support fall back single-stream
    /// inside their impl (honest: `parallel` ignored there).
    async fn download(
        &self,
        entry: &RemoteEntry,
        dest: &std::path::Path,
        parallel: u32,
        prog: &Progress,
    ) -> Result<(), String>;
    /// Upload local file. Multipart/parallel where backend supports.
    async fn upload(
        &self,
        local: &std::path::Path,
        dest_dir: &str,
        name: &str,
        prog: &Progress,
    ) -> Result<RemoteEntry, String>;
    /// S3 bucket for CopyObject source header ("" = not S3-compatible
    /// copy source). Lets same-kind server-side copy name source bucket
    /// without downcasting the source backend object.
    fn s3_bucket(&self) -> String {
        String::new()
    }
    /// True when backend wraps another (crypt): ids/paths exposed are
    /// logical, inner stored bytes differ, server-side copy invalid.
    fn crypt_wrapped(&self) -> bool {
        false
    }
    /// Server-side copy INTO this backend from `src_entry` on `src`:
    /// provider copies bytes server-to-server under this (destination)
    /// identity; no data passes through this machine. Default Err; jobs
    /// runner then falls back to download+upload through local disk.
    /// `src` is the same backend object that produced `src_entry`, so
    /// providers needing source credentials (S3 bucket/key) read them
    /// from it. Implementations must reject crypt-wrapped peers.
    async fn copy_from(
        &self,
        _src: &Arc<dyn DriveBackend>,
        _src_entry: &RemoteEntry,
        _dst_dir: &str,
        _prog: &Progress,
    ) -> Result<(), String> {
        Err("server-side copy unsupported".into())
    }
    /// Direct stream source for player: provider URL mpv can GET with
    /// returned headers, bytes never touch local disk. Default Err =
    /// not streamable (SFTP/FTP/crypt/local): caller says download first.
    /// Implementations must reject crypt-wrapped peers (default covers
    /// crypt wrapper itself; inner backends only reached unwrapped).
    async fn stream_url(&self, _entry: &RemoteEntry) -> Result<StreamSource, String> {
        Err("streaming not supported for this account: download file first".into())
    }
}

/// Human setup steps per kind (wizard panel). Short, console-exact.
pub fn setup_steps(kind: DriveKind) -> (&'static str, &'static [&'static str]) {
    use DriveKind::*;
    match kind {
        GoogleDrive => (
            "console.cloud.google.com",
            &[
                "Create project, enable Google Drive API.",
                "Google Auth Platform > Clients > Create client > Desktop app.",
                "Paste Client ID + Client secret below.",
                "Redirect: none needed for Desktop app (loopback any port).",
                "Save, Connect, sign in in browser.",
            ],
        ),
        OneDrive => (
            "entra.microsoft.com",
            &[
                "App registrations > New: account types = personal + any org.",
                "Authentication > Mobile and desktop applications.",
                "Add redirect http://127.0.0.1:53682/ exactly.",
                "Copy Application (client) ID (+ secret Value, not ID).",
                "API permissions > Microsoft Graph delegated: Files.ReadWrite.All + offline_access.",
            ],
        ),
        Dropbox => (
            "dropbox.com/developers/apps",
            &[
                "Create app > Scoped access > Full Dropbox.",
                "Permissions tab: files.metadata/content + account_info.read, Submit.",
                "Settings > Redirect URIs: add http://127.0.0.1:53682/ exactly.",
                "Paste App key (client_id) + App secret below.",
            ],
        ),
        WebDav => (
            "your server /settings/user/security (Nextcloud)",
            &["Files > bottom-left shows DAV URL; create App password there.", "Paste URL + username + app password (not login password with 2FA)."],
        ),
        S3 => (
            "provider keys page (AWS IAM / R2 API Tokens / B2 App Keys / MinIO)",
            &["Create bucket-scoped key, copy Access + Secret (shown once).", "Endpoint blank=AWS; R2=https://<acct>.r2.cloudflarestorage.com region auto."],
        ),
        Sftp => ("server panel / SSH", &["Host, port 22, username.", "Password or private key file (not .pub; passphrase = password field if key set)."]),
        Ftp => ("hosting panel", &["Host (+ftps:// for explicit), port 21/990, username, password."]),
        Teldrive => ("your teldrive server URL", &["Log in on server, set default channel.", "Settings > API Keys > Create, paste as api_key:XXXX in token field."]),
        Local => ("this PC", &["Folder path below."]),
    }
}

/// Test account: real check. about() when meaningful, else list root.
pub async fn test_account(acc: &DriveAccount) -> Result<String, String> {
    let be = backend_for(acc)?;
    let about = be.about().await.unwrap_or_default();
    match be.list("").await {
        Ok(v) => Ok(format!(
            "{}: OK ({} item(s) at root{})",
            acc.kind.label(),
            v.len(),
            if about.is_empty() {
                String::new()
            } else {
                format!(" · {about}")
            }
        )),
        Err(e) => Err(format!("{}: {e}", acc.kind.label())),
    }
}

/// Google Shared Drives for account (empty when not GoogleDrive/connected).
pub async fn shared_drives(acc: &DriveAccount) -> Result<Vec<(String, String)>, String> {
    if acc.kind != DriveKind::GoogleDrive {
        return Ok(Vec::new());
    }
    let mut a = acc.clone();
    oauth::ensure_fresh(&mut a).await?;
    let g = gdrive::GDrive::new(a);
    g.list_shared_drives().await
}

/// Build backend for account. Crypt wrapper applied when crypt_password set.
pub fn backend_for(acc: &DriveAccount) -> Result<Arc<dyn DriveBackend>, String> {
    let inner: Arc<dyn DriveBackend> = match acc.kind {
        DriveKind::GoogleDrive => Arc::new(gdrive::GDrive::new(acc.clone())),
        DriveKind::OneDrive => Arc::new(onedrive::OneDrive::new(acc.clone())),
        DriveKind::Dropbox => Arc::new(dropbox::Dropbox::new(acc.clone())),
        DriveKind::WebDav => Arc::new(webdav::WebDav::new(acc.clone())),
        DriveKind::S3 => Arc::new(s3::S3::new(acc.clone())),
        DriveKind::Sftp => Arc::new(sftpdrive::SftpDrive::new(acc.clone())),
        DriveKind::Ftp => Arc::new(ftpdrive::FtpDrive::new(acc.clone())),
        DriveKind::Teldrive => Arc::new(teldrive::Teldrive::new(acc.clone())),
        DriveKind::Local => Arc::new(local::LocalDrive::new(acc.clone())),
    };
    if !acc.crypt_password.is_empty() && acc.kind != DriveKind::Local {
        Ok(Arc::new(crypt::CryptBackend::new(
            inner,
            acc.crypt_password.clone(),
            acc.crypt_salt.clone(),
        )?))
    } else {
        Ok(inner)
    }
}

/// Persist one account's updated fields (token refresh from backends).
/// Reads/writes state.json directly — backends have no Runtime handle.
/// ponytail: full-state rewrite like save_state; DriveAccount match by id.
pub fn persist_account(acc: &DriveAccount) {
    let p = crate::platform::state_path();
    let Ok(txt) = std::fs::read_to_string(&p) else {
        return;
    };
    let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&txt) else {
        return;
    };
    let Some(list) = v
        .get_mut("settings")
        .and_then(|s| s.get_mut("drive_accounts"))
        .and_then(|a| a.as_array_mut())
    else {
        return;
    };
    for item in list.iter_mut() {
        if item.get("id").and_then(|i| i.as_str()) == Some(acc.id.as_str()) {
            if let Ok(nv) = serde_json::to_value(acc) {
                *item = nv;
            }
            break;
        }
    }
    if let Ok(out) = serde_json::to_string_pretty(&v) {
        let tmp = p.with_extension("json.tmp");
        if std::fs::write(&tmp, out).is_ok() {
            let _ = std::fs::remove_file(&p);
            let _ = std::fs::rename(&tmp, &p);
        }
    }
}

/// Join display path + name, normalizing slashes. "" root stays clean.
pub fn join_path(dir: &str, name: &str) -> String {
    let d = dir.trim_matches('/');
    let n = name.trim_matches('/');
    if d.is_empty() {
        n.to_string()
    } else if n.is_empty() {
        d.to_string()
    } else {
        format!("{d}/{n}")
    }
}

pub fn parent_path(path: &str) -> String {
    let p = path.trim_matches('/');
    match p.rfind('/') {
        Some(i) => p[..i].to_string(),
        None => String::new(),
    }
}

pub fn base_name(path: &str) -> String {
    path.trim_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_string()
}

/// Local file/dir -> pseudo RemoteEntry list (dual-pane local side).
pub fn list_local_dir(dir: &std::path::Path) -> Result<Vec<RemoteEntry>, String> {
    let mut out = Vec::new();
    let rd = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for e in rd.flatten() {
        let p = e.path();
        let md = e.metadata().ok();
        let is_dir = md.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        out.push(RemoteEntry {
            id: p.to_string_lossy().into_owned(),
            name: e.file_name().to_string_lossy().into_owned(),
            path: p.to_string_lossy().into_owned(),
            is_dir,
            size: if is_dir {
                0
            } else {
                md.as_ref().map(|m| m.len()).unwrap_or(0)
            },
            modified: md
                .as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0),
            mime: String::new(),
        });
    }
    sort_entries(&mut out);
    Ok(out)
}

pub fn sort_entries(v: &mut [RemoteEntry]) {
    v.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

pub fn fmt_bytes(n: u64) -> String {
    const U: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", U[i])
}

/// Percent-encode one path segment / query value (RFC3986 unreserved kept).
/// Minimal RFC 4648 base64 (Basic auth header; no dep for one header).
pub fn b64(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

pub fn pct(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Encode path preserving `/` separators.
pub fn pct_path(s: &str) -> String {
    s.split('/').map(pct).collect::<Vec<_>>().join("/")
}

/// HTTP date / RFC3339-ish parsers kept tiny: unix secs from common formats.
pub fn parse_time(s: &str) -> u64 {
    // RFC3339: 2024-01-02T03:04:05Z / with offset / fractional.
    let s = s.trim();
    if s.len() >= 19 {
        let (y, mo, d) = (
            s.get(0..4).and_then(|x| x.parse::<i64>().ok()),
            s.get(5..7).and_then(|x| x.parse::<i64>().ok()),
            s.get(8..10).and_then(|x| x.parse::<i64>().ok()),
        );
        let (h, mi, sec) = (
            s.get(11..13).and_then(|x| x.parse::<i64>().ok()),
            s.get(14..16).and_then(|x| x.parse::<i64>().ok()),
            s.get(17..19).and_then(|x| x.parse::<i64>().ok()),
        );
        if let (Some(y), Some(mo), Some(d), Some(h), Some(mi), Some(sec)) = (y, mo, d, h, mi, sec) {
            let days = days_from_civil(y, mo, d);
            let mut secs = days * 86400 + h * 3600 + mi * 60 + sec;
            // Offset +hh:mm / -hh:mm after seconds (ignore Z).
            if let Some(pos) = s[19..].find(['+', '-']) {
                let off = &s[19 + pos..];
                if off.len() >= 6 {
                    if let (Ok(oh), Ok(om)) = (
                        off.get(1..3).unwrap_or("").parse::<i64>(),
                        off.get(4..6).unwrap_or("").parse::<i64>(),
                    ) {
                        let delta = oh * 3600 + om * 60;
                        if off.starts_with('+') {
                            secs -= delta;
                        } else {
                            secs += delta;
                        }
                    }
                }
            }
            return secs.max(0) as u64;
        }
    }
    s.parse().unwrap_or(0)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paths() {
        assert_eq!(join_path("", "a"), "a");
        assert_eq!(join_path("a/", "/b/"), "a/b");
        assert_eq!(parent_path("a/b/c"), "a/b");
        assert_eq!(parent_path("a"), "");
        assert_eq!(base_name("a/b.txt"), "b.txt");
        assert_eq!(pct("a b&c"), "a%20b%26c");
        assert_eq!(pct_path("a b/c"), "a%20b/c");
    }
    #[test]
    fn b64_basic() {
        assert_eq!(b64(b""), "");
        assert_eq!(b64(b"u:p"), "dTpw");
        assert_eq!(b64(b"user:pass"), "dXNlcjpwYXNz");
    }
    #[test]
    fn time_parse() {
        assert_eq!(parse_time("1970-01-01T00:00:00Z"), 0);
        assert_eq!(parse_time("1970-01-01T01:00:00+01:00"), 0);
        assert_eq!(parse_time("2024-01-01T00:00:00.000Z"), 1704067200);
        assert_eq!(parse_time("1704067200"), 1704067200);
    }
}
