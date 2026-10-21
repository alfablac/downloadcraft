use serde::{Deserialize, Serialize};

use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
pub enum Priority {
    Low,
    #[default]
    Normal,
    High,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum TaskStatus {
    #[default]
    Queued,
    Downloading,
    Paused,
    Complete,
    Error,
    Protected,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct DownloadTask {
    pub id: String,
    pub url: String,
    pub filename: String,
    pub dest_dir: PathBuf,
    #[serde(default)]
    pub queue_id: String,
    #[serde(default)]
    pub priority: Priority,
    #[serde(default)]
    pub order: i64,
    #[serde(default)]
    pub status: TaskStatus,
    #[serde(default)]
    pub total: Option<u64>,
    #[serde(default)]
    pub downloaded: u64,
    #[serde(default)]
    pub referrer: String,
    #[serde(default)]
    pub cookies: String,
    /// Structured browser cookies (JSON array from extension getAll) for yt-dlp cookie jar.
    #[serde(default)]
    pub cookies_json: String,
    /// Probed Content-Type (capture prewarm / download probe). Display + host KV round-trip only.
    #[serde(default)]
    pub mime: String,
    /// Probed HTTP Range support: None = never probed / not HTTP, Some = probe result.
    #[serde(default)]
    pub accept_ranges: Option<bool>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub segments: u32,
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub error: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub speed_limit_kbps: u64,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub next_retry_at: u64,
    #[serde(default)]
    pub created_at: u64,
    /// Mirror URLs (IDM registry Url1..UrlN beside primary Url0).
    #[serde(default)]
    pub mirrors: Vec<String>,
    /// Per-URL credentials (IDM U0_u/U0_EncP). Stored plain in state.json — ponytail: no OS keychain; add keyring when credential theft matters.
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub last_try_at: u64,
    #[serde(default)]
    pub description: String,
    /// SFTP private-key file (empty = password auth, else ~/.ssh/id_* probe).
    #[serde(default)]
    pub ssh_key_path: String,
    /// Per-download proxy override (http:// or socks5://, creds allowed).
    /// Empty = inherit Settings::proxy_url. Plain in state.json like global.
    #[serde(default)]
    pub proxy_url: String,
    /// Display-only debrid route info:
    /// `"<Provider> · <host> · <cached|cached: unknown|not cached>"`.
    /// Empty = not debrid.
    #[serde(default)]
    pub debrid_info: String,
    /// Last resolved debrid URL. BEARER TOKEN (path/query) persisted in
    /// state.json per user choice so progress modal can copy it after
    /// completion; expires server-side. Cleared on redownload/remove.
    #[serde(default)]
    pub debrided_url: String,
    /// Last final (post-redirect) fetch URL. May also carry signed token.
    #[serde(default)]
    pub final_url: String,
    /// Torrent file indices to download (empty = all files).
    #[serde(default)]
    pub torrent_files: Vec<usize>,
    /// Debrid fan-out marker: provider file name this magnet task downloads
    /// on debrid re-resolve (multi-file magnet = one task per file sharing
    /// magnet URL; stream matches resolved file by this name, not task filename).
    #[serde(default)]
    pub debrid_file_name: String,
    /// Per-download debrid override (set in Add/capture dialog):
    /// `""`/`"auto"` = Options defaults, `"none"` = never debrid (local),
    /// `"rd"`/`"ad"`/`"tb"` = force that provider (must be usable).
    #[serde(default)]
    pub debrid_choice: String,
}

/// Effective debrid choice for a task: raw override when recognized,
/// else `"auto"` (Options defaults).
pub fn debrid_choice_of(task: &DownloadTask) -> &'static str {
    match task.debrid_choice.trim().to_ascii_lowercase().as_str() {
        "none" => "none",
        "rd" => "rd",
        "ad" => "ad",
        "tb" => "tb",
        _ => "auto",
    }
}

/// Effective egress proxy: per-task override when non-empty, else global.
/// Empty result = direct (except reqwest env/system proxy fallback).
pub fn effective_proxy<'a>(task_proxy: &'a str, global_proxy: &'a str) -> &'a str {
    if task_proxy.trim().is_empty() {
        global_proxy
    } else {
        task_proxy
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CompletionAction {
    #[default]
    None,
    OpenFile,
    RunCommand,
    ExitApp,
    Shutdown,
    Sleep,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Queue {
    pub id: String,
    pub name: String,
    #[serde(default = "def_one")]
    pub max_concurrent: usize,
    #[serde(default)]
    pub auto_start: bool,
    #[serde(default)]
    pub retry_count: u32,
    #[serde(default)]
    pub retry_interval_secs: u64,
    #[serde(default)]
    pub on_complete: CompletionAction,
    #[serde(default)]
    pub on_complete_arg: String,
}
fn def_one() -> usize {
    1
}
fn def_true() -> bool {
    true
}
impl Default for Queue {
    fn default() -> Self {
        Self {
            id: "main".into(),
            name: "Main".into(),
            max_concurrent: 3,
            auto_start: true,
            retry_count: 3,
            retry_interval_secs: 10,
            on_complete: CompletionAction::None,
            on_complete_arg: String::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Recurrence {
    Once,
    #[default]
    Daily,
    Weekdays,
    Weekly,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Schedule {
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub queue_id: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub recurrence: Recurrence,
    /// days-bitmask Mon=1 Tue=2 Wed=4 Thu=8 Fri=16 Sat=32 Sun=64 (Weekly)
    #[serde(default)]
    pub weekdays_mask: u8,
    /// YYYYMMDD for Once; also anchor date for repeat_days cycle
    #[serde(default)]
    pub once_date: u32,
    /// Repeat each N days (Daily only; 0/1 = every day). IDM Scheduler field.
    #[serde(default)]
    pub repeat_days: u32,
    #[serde(default)]
    pub start_secs: u32,
    #[serde(default)]
    pub stop_secs: Option<u32>,
    #[serde(default)]
    pub files_at_once: usize,
    #[serde(default)]
    pub run_missed: bool,
    #[serde(default)]
    pub last_started_day: i64,
    /// Day stop was last applied. Latched so scheduler stop fires once/day and
    /// cannot re-Pause a task the user resumed manually after stop time.
    #[serde(default)]
    pub last_stopped_day: i64,
    #[serde(default)]
    pub on_complete: CompletionAction,
    #[serde(default)]
    pub power_confirmed: bool,
    /// Optional bandwidth window: kbps applied while schedule window active. 0 = unlimited.
    #[serde(default)]
    pub limit_kbps: u64,
    /// Quota scheduler (IDM Scheduler reg: isLimitEnabled/m_MBytes/m_hours): stop queue after quota_mb per quota_hours. 0 = off.
    #[serde(default)]
    pub quota_mb: u64,
    #[serde(default)]
    pub quota_hours: u64,
    #[serde(default)]
    pub quota_window_start: u64,
    #[serde(default)]
    pub quota_used_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Settings {
    // No default here broke whole-state parse (data loss) when absent in old state files.
    #[serde(default)]
    pub download_dir: PathBuf,
    #[serde(default = "def_conn")]
    pub max_connections: u32,
    #[serde(default = "def_global")]
    pub global_max_concurrent: usize,
    /// HTTP part size in MiB. 0 = auto (64 MiB, up to 256 MiB huge
    /// files); >0 = exact part size target for new downloads.
    #[serde(default)]
    pub max_part_mb: u64,
    #[serde(default)]
    pub speed_limit_kbps: u64,
    #[serde(default)]
    pub autostart: bool,
    #[serde(default)]
    pub start_minimized: bool,
    /// Minimize button hides the window to the tray (X always quits).
    /// Alias keeps the old `close_to_tray` value from existing state files.
    #[serde(default = "def_true", alias = "close_to_tray")]
    pub minimize_to_tray: bool,
    #[serde(default = "def_true")]
    pub notifications: bool,
    #[serde(default)]
    pub clipboard_monitor: bool,
    #[serde(default)]
    pub proxy_url: String,
    /// Master "Turn on scheduler" (IDM Scheduler dialog). Default on.
    #[serde(default = "def_true")]
    pub scheduler_enabled: bool,
    /// Browser-capture extension allowlist, raw comma-separated list e.g. "mp4,zip,pdf".
    /// Empty = capture all known types (filter in host via capture-filter.json).
    #[serde(default)]
    pub capture_exts: String,
    /// Browser-capture minimum size in MB. 0 = no minimum. Files with unknown
    /// size (missing Content-Length) denied when a minimum is set: cannot
    /// prove large enough. Allowed when no minimum set.
    #[serde(default)]
    pub capture_min_mb: u64,
    #[serde(default = "def_true")]
    pub ytdlp_auto_update: bool,
    /// NNTP (Usenet/NZB) server. Host empty = unconfigured. Creds stored
    /// plain like proxy creds — ponytail: no OS keychain; add keyring when needed.
    #[serde(default)]
    pub nntp_host: String,
    #[serde(default = "def_nntp_port")]
    pub nntp_port: u16,
    #[serde(default = "def_true")]
    pub nntp_tls: bool,
    #[serde(default)]
    pub nntp_user: String,
    #[serde(default)]
    pub nntp_pass: String,
    /// NNTP provider connections per NZB, independent of HTTP max_connections.
    #[serde(default = "def_nntp_connections")]
    pub nntp_connections: u32,
    /// Auto-unpack archives (rar/zip/7z) after NZB download completes.
    /// Archive deleted only after verified successful extraction.
    #[serde(default = "def_true")]
    pub nzb_auto_unpack: bool,
    /// Debrid providers (Real-Debrid / AllDebrid / TorBox). Tokens stored
    /// plain in state.json like NNTP/proxy creds — ponytail: no OS keychain;
    /// add keyring when credential theft matters. Tokens never logged.
    #[serde(default)]
    pub debrid_preferred: String,
    #[serde(default)]
    pub debrid_rd: DebridProviderCfg,
    #[serde(default)]
    pub debrid_ad: DebridProviderCfg,
    #[serde(default)]
    pub debrid_tb: DebridProviderCfg,
    /// BitTorrent listen port. 0 = librqbit default (random).
    #[serde(default)]
    pub torrent_listen_port: u16,
    /// Keep seeding after download completes instead of pausing torrent.
    #[serde(default)]
    pub torrent_seed_after_complete: bool,
    /// Rename template for new download filenames: {name} stem, {ext} with
    /// dot, {date} YYYY-MM-DD, {host}, {category}. Empty = keep original.
    #[serde(default)]
    pub folder_template: String,
    /// Route new downloads into download_dir/<Category>/ (except General).
    #[serde(default)]
    pub category_subfolders: bool,
    /// Downloads-table sort column (app sort_col encoding). Default 0 = File Name.
    #[serde(default)]
    pub dl_sort_col: u8,
    /// Downloads-table sort ascending. Absent in old state = true.
    #[serde(default = "def_true")]
    pub dl_sort_asc: bool,
    /// User-dragged downloads-table column widths, full layout (empty = defaults).
    #[serde(default)]
    pub dl_col_widths: Vec<f32>,
    /// User-dragged downloads-table column widths, compact layout (<760px).
    #[serde(default)]
    pub dl_col_widths_compact: Vec<f32>,
    /// User-dragged grabber-table column widths (empty = defaults).
    #[serde(default)]
    pub grab_col_widths: Vec<f32>,
    /// Sidebar rail counts rendered colored badges. Off = plain faint text.
    #[serde(default = "def_true")]
    pub nav_count_badges: bool,
    /// Per-category rail badge colors [r,g,b], keyed by category name
    /// ("Video", "Audio", ...). Empty/missing key = theme::category_color default.
    #[serde(default)]
    pub category_colors: Vec<(String, [u8; 3])>,
    /// Per-type rail badge colors [r,g,b], keyed by TypeSel key
    /// ("Torrent","Nntp","Ftp","Stream","YtDlp","Metalink","Http","File").
    /// Empty/missing key = theme::type_key_color default.
    #[serde(default)]
    pub type_colors: Vec<(String, [u8; 3])>,
    /// mpv player (Streams page + Options > mpv). Custom exe path; empty = auto-find.
    #[serde(default)]
    pub mpv_path: String,
    #[serde(default)]
    pub mpv_fullscreen: bool,
    #[serde(default)]
    pub mpv_keep_open: bool,
    #[serde(default)]
    pub mpv_loop: bool,
    #[serde(default = "def_mpv_volume")]
    pub mpv_volume: u32,
    #[serde(default = "def_mpv_speed")]
    pub mpv_speed: f32,
    #[serde(default)]
    pub mpv_start: String,
    /// mpv --hwdec value ("" = mpv default, e.g. "auto-safe").
    #[serde(default)]
    pub mpv_hwdec: String,
    #[serde(default = "def_true")]
    pub mpv_cache: bool,
    #[serde(default = "def_mpv_cache_secs")]
    pub mpv_cache_secs: u32,
    /// mpv --sub-auto value ("" = mpv default, e.g. "fuzzy"/"all").
    #[serde(default)]
    pub mpv_sub_auto: String,
    /// Extra mpv args, whitespace-split (Streams Options common flags).
    #[serde(default)]
    pub mpv_extra_args: String,
    /// Drives: configured cloud/server storage accounts (secrets plain, DriveAccount).
    #[serde(default)]
    pub drive_accounts: Vec<DriveAccount>,
    /// Legacy (removed global): per-account streams now controls parallel.
    /// Kept parsed for old state files; ignored.
    #[serde(default)]
    #[allow(dead_code)]
    pub drive_transfers: u32,
    /// Drives: saved sync jobs.
    #[serde(default)]
    pub drive_syncs: Vec<DriveSync>,
    /// Chatbot: any OpenAI-compatible API. Default = TokenHarbor free model.
    /// Key stored plain in state.json like debrid tokens — ponytail: no OS
    /// keychain; never logged, never included in bug reports.
    #[serde(default = "def_chatbot_base_url")]
    pub chatbot_base_url: String,
    #[serde(default)]
    pub chatbot_api_key: String,
    #[serde(default = "def_chatbot_model")]
    pub chatbot_model: String,
}

fn def_chatbot_base_url() -> String {
    "https://tokenharbor.ai/v1".to_string()
}
fn def_chatbot_model() -> String {
    "deepseek-v4.1-flash:free".to_string()
}

/// Saved Drives sync job. Mode: mirror|update|copy.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct DriveSync {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub src_account: String,
    #[serde(default)]
    pub src_path: String,
    #[serde(default)]
    pub dst_account: String,
    #[serde(default)]
    pub dst_path: String,
    #[serde(default)]
    pub mode: String,
}
fn def_mpv_volume() -> u32 {
    100
}
fn def_mpv_speed() -> f32 {
    1.0
}
fn def_mpv_cache_secs() -> u32 {
    30
}
fn def_nntp_port() -> u16 {
    563
}
fn def_nntp_connections() -> u32 {
    30
}

// ── Drives (cloud storage accounts) ─────────────────────────────────
// Secrets plain in state.json like debrid/NNTP creds (see comment at
// debrid fields) — ponytail: no OS keychain; add keyring when needed.
// Tokens/secrets never logged.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum DriveKind {
    #[default]
    GoogleDrive,
    OneDrive,
    Dropbox,
    WebDav,
    S3,
    Sftp,
    Ftp,
    Teldrive,
    Local,
}

impl DriveKind {
    pub fn label(self) -> &'static str {
        match self {
            DriveKind::GoogleDrive => "Google Drive",
            DriveKind::OneDrive => "OneDrive",
            DriveKind::Dropbox => "Dropbox",
            DriveKind::WebDav => "WebDAV",
            DriveKind::S3 => "S3-compatible",
            DriveKind::Sftp => "SFTP",
            DriveKind::Ftp => "FTP / FTPS",
            DriveKind::Teldrive => "Telegram (teldrive)",
            DriveKind::Local => "Local folder",
        }
    }
    pub fn key(self) -> &'static str {
        match self {
            DriveKind::GoogleDrive => "gdrive",
            DriveKind::OneDrive => "onedrive",
            DriveKind::Dropbox => "dropbox",
            DriveKind::WebDav => "webdav",
            DriveKind::S3 => "s3",
            DriveKind::Sftp => "sftp",
            DriveKind::Ftp => "ftp",
            DriveKind::Teldrive => "teldrive",
            DriveKind::Local => "local",
        }
    }
    pub const ALL: [DriveKind; 9] = [
        DriveKind::GoogleDrive,
        DriveKind::OneDrive,
        DriveKind::Dropbox,
        DriveKind::WebDav,
        DriveKind::S3,
        DriveKind::Sftp,
        DriveKind::Ftp,
        DriveKind::Teldrive,
        DriveKind::Local,
    ];
}

/// One configured storage account. `root` = provider root id/path
/// (drive id, bucket, base path, teldrive channel-root path...).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct DriveAccount {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub kind: DriveKind,
    /// Base URL / host (WebDAV URL, S3 endpoint, FTP/SFTP host, teldrive URL, local dir).
    #[serde(default)]
    pub endpoint: String,
    #[serde(default)]
    pub username: String,
    /// Password / app-password / S3 secret key / FTP-SFTP password. Plain.
    #[serde(default)]
    pub secret: String,
    /// OAuth access token (refreshed automatically). Plain.
    #[serde(default)]
    pub access_token: String,
    /// OAuth refresh token. Plain.
    #[serde(default)]
    pub refresh_token: String,
    /// OAuth token expiry, unix seconds (0 = unknown).
    #[serde(default)]
    pub token_expires: u64,
    /// User-supplied OAuth client id (Google/Microsoft/Dropbox own app).
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub client_secret: String,
    /// Root: bucket (S3), base path (WebDAV/SFTP/FTP/local), start-folder
    /// id/path (OAuth drives; Google: folder inside team_drive or My Drive).
    #[serde(default)]
    pub root: String,
    /// Google Shared Drive (Team Drive) id. Empty = My Drive (rclone team_drive).
    #[serde(default)]
    pub team_drive: String,
    /// S3 region (default "auto").
    #[serde(default)]
    pub region: String,
    /// SFTP/FTP port (0 = default), SSH key path (SFTP).
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub ssh_key_path: String,
    /// Parallel streams per file where backend supports ranged/segmented IO.
    #[serde(default = "def_drive_streams")]
    pub streams: u32,
    /// rclone-crypt: encrypt uploads with crypt password+salt (empty = off).
    #[serde(default)]
    pub crypt_password: String,
    #[serde(default)]
    pub crypt_salt: String,
}

fn def_drive_streams() -> u32 {
    4
}

impl DriveAccount {
    pub fn usable(&self) -> bool {
        !self.id.is_empty()
    }
    pub fn display(&self) -> String {
        if self.name.trim().is_empty() {
            format!("{} ({})", self.kind.label(), self.id)
        } else {
            self.name.clone()
        }
    }
}

/// One debrid provider's config. `Default` = disabled, empty token,
/// hosters+magnets routing on (matches Settings::default entries).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DebridProviderCfg {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "def_true")]
    pub use_for_hosters: bool,
    #[serde(default = "def_true")]
    pub use_for_magnets: bool,
    /// Magnets: only route via debrid when already cached there;
    /// otherwise fall back to local torrent engine.
    #[serde(default)]
    pub cached_only: bool,
}

impl Default for DebridProviderCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            api_key: String::new(),
            use_for_hosters: true,
            use_for_magnets: true,
            cached_only: false,
        }
    }
}

impl DebridProviderCfg {
    pub fn usable(&self) -> bool {
        self.enabled && !self.api_key.trim().is_empty()
    }
}
fn def_conn() -> u32 {
    8
}
fn def_global() -> usize {
    3
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            download_dir: crate::platform::default_download_dir(),
            max_connections: 8,
            global_max_concurrent: 3,
            max_part_mb: 0,
            speed_limit_kbps: 0,
            autostart: false,
            start_minimized: false,
            minimize_to_tray: true,
            notifications: true,
            clipboard_monitor: false,
            proxy_url: String::new(),
            scheduler_enabled: true,
            capture_exts: String::new(),
            capture_min_mb: 0,
            ytdlp_auto_update: true,
            nntp_host: String::new(),
            nntp_port: 563,
            nntp_tls: true,
            nntp_user: String::new(),
            nntp_pass: String::new(),
            nntp_connections: 30,
            nzb_auto_unpack: true,
            debrid_preferred: String::new(),
            debrid_rd: DebridProviderCfg::default(),
            debrid_ad: DebridProviderCfg::default(),
            debrid_tb: DebridProviderCfg::default(),
            torrent_listen_port: 0,
            torrent_seed_after_complete: false,
            folder_template: String::new(),
            category_subfolders: false,
            dl_sort_col: 0,
            dl_sort_asc: true,
            dl_col_widths: Vec::new(),
            dl_col_widths_compact: Vec::new(),
            grab_col_widths: Vec::new(),
            nav_count_badges: true,
            category_colors: Vec::new(),
            type_colors: Vec::new(),
            mpv_path: String::new(),
            mpv_fullscreen: false,
            mpv_keep_open: false,
            mpv_loop: false,
            mpv_volume: 100,
            mpv_speed: 1.0,
            mpv_start: String::new(),
            mpv_hwdec: String::new(),
            mpv_cache: true,
            mpv_cache_secs: 30,
            mpv_sub_auto: String::new(),
            mpv_extra_args: String::new(),
            drive_accounts: Vec::new(),
            drive_syncs: Vec::new(),
            drive_transfers: 4,
            chatbot_base_url: def_chatbot_base_url(),
            chatbot_api_key: String::new(),
            chatbot_model: def_chatbot_model(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct AppState {
    #[serde(default)]
    pub tasks: Vec<DownloadTask>,
    #[serde(default)]
    pub queues: Vec<Queue>,
    #[serde(default)]
    pub schedules: Vec<Schedule>,
    #[serde(default)]
    pub settings: Settings,
}

pub fn new_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{n:x}")
}

pub fn sanitize_filename(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let cleaned: String = base
        .chars()
        .map(|c| {
            if matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || (c as u32) < 32
            {
                '_'
            } else {
                c
            }
        })
        .collect();
    let t = cleaned.trim().trim_end_matches(['.', ' ']).trim();
    if t.is_empty() {
        return "download.bin".into();
    }
    // Windows reserved device names fail file creation; prefix stem.
    let stem = t.split('.').next().unwrap_or(t);
    let reserved = matches!(
        stem.to_ascii_uppercase().as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    );
    let mut out = if reserved {
        format!("_{t}")
    } else {
        t.to_string()
    };
    // Cap name length: hostile 10k names / >260-char full paths fail on Windows.
    if out.len() > 120 {
        let (ext, stem_part) = match out.rfind('.') {
            Some(i) if i > 0 && out.len() - i <= 10 => (out[i..].to_string(), out[..i].to_string()),
            _ => (String::new(), out.clone()),
        };
        let keep = 120usize.saturating_sub(ext.len());
        out = format!(
            "{}{}",
            stem_part.chars().take(keep).collect::<String>(),
            ext
        );
    }
    out
}

pub fn filename_from_url(url: &str) -> String {
    // Magnet has no path segment: use dn= display name, else placeholder
    // "torrent" (torrent poll loop replaces it once metadata arrives).
    if url
        .trim_start()
        .to_ascii_lowercase()
        .starts_with("magnet:?")
    {
        if let Some(q) = url.split_once('?').map(|(_, q)| q) {
            for pair in q.split('&') {
                if let Some(v) = pair.strip_prefix("dn=") {
                    if !v.is_empty() {
                        return sanitize_filename(&percent_decode(v));
                    }
                }
            }
        }
        return "torrent".into();
    }
    let clean = url.split(['?', '#']).next().unwrap_or(url);
    let last = clean.rsplit('/').next().unwrap_or("");
    let decoded = percent_decode(last);
    if decoded.is_empty() {
        "download.bin".into()
    } else {
        sanitize_filename(&decoded)
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() + 1 && i + 2 <= b.len() - 1 + 1 {
            if i + 2 < b.len() || i + 2 == b.len() { /* guard below */ }
            if i + 2 < b.len() {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("");
                if let Ok(v) = u8::from_str_radix(hex, 16) {
                    out.push(v);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// Adapted from downloadcraft (MIT): folder-rule/rename-template ideas
// (upstream src/model.rs FolderRule/apply_rename_template), rewritten to
// reuse this crate's Category map instead of a separate rule list.

/// Expand rename template. Pure: date injected so tests pin exact output.
/// `{name}` stem, `{ext}` extension incl dot ("" if none), `{date}`
/// YYYY-MM-DD, `{host}` URL host ("" if unparseable), `{category}`.
/// Empty/whitespace template returns original filename unchanged.
/// Sanitized via sanitize_filename; blank result falls back to original.
pub fn expand_template(
    tpl: &str,
    stem: &str,
    ext: &str,
    host: &str,
    category: &str,
    date_str: &str,
) -> String {
    if tpl.trim().is_empty() {
        return format!("{stem}{ext}");
    }
    let out = tpl
        .replace("{name}", stem)
        .replace("{ext}", ext)
        .replace("{date}", date_str)
        .replace("{host}", host)
        .replace("{category}", category);
    // Template may reference only {category} etc and omit {ext}; keep ext.
    let out = if !ext.is_empty()
        && !out
            .to_ascii_lowercase()
            .ends_with(&ext.to_ascii_lowercase())
    {
        format!("{out}{ext}")
    } else {
        out
    };
    let clean = sanitize_filename(out.trim());
    // sanitize maps ""/dot-only to "download.bin"; treat that as fallback.
    if clean == "download.bin" && !stem.is_empty() {
        return format!("{stem}{ext}");
    }
    clean
}

/// Split filename into (stem, ext-with-dot). Dot at index 0 = no ext.
pub fn split_stem_ext(filename: &str) -> (&str, &str) {
    match filename.rfind('.') {
        Some(i) if i > 0 => (&filename[..i], &filename[i..]),
        _ => (filename, ""),
    }
}

/// True when URL-seeded `filename` carries no title info, so extracted
/// (yt-dlp) title should replace it. Weak: stem empty / all-digits /
/// len<=2 / equals URL host first label / equals any URL path segment
/// (or that segment's stem). Exception: filename exactly equals last
/// path segment = direct-media URL, name is real -> not weak.
pub fn filename_stem_is_weak_for_title(filename: &str, url: &str) -> bool {
    let (stem, _) = split_stem_ext(filename);
    if stem.is_empty() {
        return true;
    }
    let parsed = url::Url::parse(url).ok();
    let last_segment = parsed
        .as_ref()
        .map(|u| u.path().rsplit('/').next().unwrap_or(""))
        .unwrap_or("");
    if !last_segment.is_empty() && filename.eq_ignore_ascii_case(last_segment) {
        return false;
    }
    if stem.chars().count() <= 2 || stem.chars().all(|c| c.is_ascii_digit()) {
        return true;
    }
    if let Some(u) = &parsed {
        if let Some(label) = u.host_str().and_then(|h| h.split('.').next()) {
            if stem.eq_ignore_ascii_case(label) {
                return true;
            }
        }
        for seg in u.path().split('/') {
            if seg.is_empty() {
                continue;
            }
            let seg_stem = seg.split('.').next().unwrap_or(seg);
            if stem.eq_ignore_ascii_case(seg) || stem.eq_ignore_ascii_case(seg_stem) {
                return true;
            }
        }
    }
    false
}

/// First free variant of `filename` in `dir`: as-is if not in `taken`
/// (case-insensitive) and not on disk, else `{stem}_{n}{ext}` n=1,2,...
pub fn unique_filename(dir: &std::path::Path, filename: &str, taken: &[String]) -> String {
    let is_taken =
        |name: &str| taken.iter().any(|t| t.eq_ignore_ascii_case(name)) || dir.join(name).exists();
    if !is_taken(filename) {
        return filename.to_string();
    }
    let (stem, ext) = split_stem_ext(filename);
    let mut n = 1;
    loop {
        let cand = format!("{stem}_{n}{ext}");
        if !is_taken(&cand) {
            return cand;
        }
        n += 1;
    }
}

/// True when `filename` collides in `dir` with another task (`taken`,
/// case-insensitive, excluding `exclude_filename`) or a disk file.
/// When filename == exclude_filename (own name), disk hit is self, so
/// only the (exclude-filtered) taken check counts.
pub fn name_taken(
    dir: &std::path::Path,
    filename: &str,
    taken: &[String],
    exclude_filename: &str,
) -> bool {
    let in_taken = taken
        .iter()
        .any(|t| t.eq_ignore_ascii_case(filename) && !t.eq_ignore_ascii_case(exclude_filename));
    if filename.eq_ignore_ascii_case(exclude_filename) {
        return in_taken;
    }
    in_taken || dir.join(filename).exists()
}

/// Current local date as YYYY-MM-DD. No chrono dep: UTC date from epoch
/// days via scheduler::civil_from_days inverse; local tz offset ignored
/// (ponytail: UTC date, off-by-one near midnight acceptable).
pub fn today_str() -> String {
    let days = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| (d.as_secs() / 86400) as i64)
        .unwrap_or(0);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Inverse of scheduler::days_from_civil (Howard Hinnant algorithm).
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Apply settings rename template to a filename for `url`. Empty template
/// returns filename unchanged. Category derived from original filename.
pub fn apply_folder_template(tpl: &str, filename: &str, url: &str) -> String {
    if tpl.trim().is_empty() {
        return filename.to_string();
    }
    let (stem, ext) = split_stem_ext(filename);
    let host = url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default();
    expand_template(tpl, stem, ext, &host, category_for(filename), &today_str())
}

/// Destination dir for new task: category subfolder when enabled and
/// category != "General" (this crate's Other-equivalent), else base.
pub fn dest_dir_for(
    base: &std::path::Path,
    filename: &str,
    category_subfolders: bool,
) -> std::path::PathBuf {
    let cat = category_for(filename);
    if category_subfolders && cat != "General" {
        base.join(cat)
    } else {
        base.to_path_buf()
    }
}

pub fn category_for(filename: &str) -> &'static str {
    let ext = filename
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "zip" | "rar" | "7z" | "gz" | "bz2" | "tar" | "xz" => "Compressed",
        "mp4" | "mkv" | "avi" | "mov" | "wmv" | "webm" | "mpg" | "mpeg" | "m3u8" | "mpd" => "Video",
        "mp3" | "wav" | "flac" | "ogg" | "aac" | "m4a" | "wma" => "Audio",
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "txt" => "Documents",
        "exe" | "msi" | "dmg" | "deb" | "rpm" | "appimage" | "apk" => "Programs",
        _ => "General",
    }
}

/// Pretty JSON for Settings export (Options > General > Backup).
/// ponytail: plain Settings JSON, no versioned wrapper; Settings #[serde(default)]
/// on every field keeps old/partial files importable.
pub fn settings_export_json(s: &Settings) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(s)
}

/// Parse imported Settings. Rejects non-object JSON and AppState shapes
/// (`tasks`/`settings` keys) so wrong file type fails without touching state.
pub fn parse_settings_import(txt: &str) -> Result<Settings, String> {
    let v: serde_json::Value =
        serde_json::from_str(txt).map_err(|e| format!("invalid settings file: {e}"))?;
    let obj = v
        .as_object()
        .ok_or_else(|| "invalid settings file: expected JSON object".to_string())?;
    if obj.contains_key("tasks") || obj.contains_key("settings") {
        return Err("invalid settings file: looks like app state, not settings".into());
    }
    serde_json::from_value(v).map_err(|e| format!("invalid settings file: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_template_exact() {
        assert_eq!(
            expand_template(
                "{name}-{date}{ext}",
                "Vid",
                ".mp4",
                "cdn.example",
                "Video",
                "2026-10-17"
            ),
            "Vid-2026-10-17.mp4"
        );
        assert_eq!(
            expand_template(
                "{category}_{host}_{name}{ext}",
                "a",
                ".mkv",
                "cdn.example",
                "Video",
                "2026-10-17"
            ),
            "Video_cdn.example_a.mkv"
        );
        assert_eq!(
            expand_template(
                "{name} from {host}",
                "a",
                ".mkv",
                "h",
                "Video",
                "2026-10-17"
            ),
            "a from h.mkv",
            "missing ext appended"
        );
        assert_eq!(
            expand_template("", "a", ".mp4", "h", "Video", "2026-10-17"),
            "a.mp4"
        );
        assert_eq!(
            expand_template("  ", "a", ".mp4", "h", "Video", "2026-10-17"),
            "a.mp4"
        );
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20743), (2026, 10, 17));
    }

    #[test]
    fn filename_from_magnet_uses_dn_or_placeholder() {
        assert_eq!(
            filename_from_url("magnet:?xt=urn:btih:abc&dn=My%20File%20%5B1080p%5D"),
            "My File [1080p]"
        );
        assert_eq!(filename_from_url("magnet:?xt=urn:btih:abc"), "torrent");
        assert_eq!(filename_from_url("magnet:?xt=urn:btih:abc&dn="), "torrent");
    }

    #[test]
    fn category_subfolder_path() {
        let base = std::path::Path::new("/dl");
        assert_eq!(dest_dir_for(base, "a.mp4", true), base.join("Video"));
        assert_eq!(dest_dir_for(base, "a.bin", true), base.to_path_buf());
        assert_eq!(dest_dir_for(base, "a.mp4", false), base.to_path_buf());
        // Old state without new fields deserializes to defaults.
        let mut v = serde_json::to_value(Settings::default()).unwrap();
        v.as_object_mut().unwrap().remove("folder_template");
        v.as_object_mut().unwrap().remove("category_subfolders");
        let s: Settings = serde_json::from_value(v).unwrap();
        assert_eq!(s.folder_template, "");
        assert!(!s.category_subfolders);
    }

    #[test]
    fn torrent_files_selection_persists_and_defaults() {
        let mut t = DownloadTask {
            id: "1".into(),
            torrent_files: vec![0, 2],
            ..Default::default()
        };
        let back: DownloadTask = serde_json::from_value(serde_json::to_value(&t).unwrap()).unwrap();
        assert_eq!(back.torrent_files, vec![0, 2]);
        assert_eq!(back.accept_ranges, None);
        assert!(back.debrided_url.is_empty());
        assert!(back.final_url.is_empty());
        // Old state.json without field -> empty (all files) / accept_ranges None.
        t.torrent_files.clear();
        let mut v = serde_json::to_value(&t).unwrap();
        v.as_object_mut().unwrap().remove("torrent_files");
        // accept_ranges absent in old state -> None (unknown, blank cell).
        v.as_object_mut().unwrap().remove("accept_ranges");
        let old: DownloadTask = serde_json::from_value(v).unwrap();
        assert!(old.torrent_files.is_empty());
        assert_eq!(old.accept_ranges, None);
        assert!(old.proxy_url.is_empty());
    }

    #[test]
    fn chatbot_settings_default_and_absent_fields() {
        let s = Settings::default();
        assert_eq!(s.chatbot_base_url, "https://tokenharbor.ai/v1");
        assert_eq!(s.chatbot_model, "deepseek-v4.1-flash:free");
        assert!(s.chatbot_api_key.is_empty());
        // Old state.json without chatbot fields loads with defaults.
        let mut v = serde_json::to_value(&s).unwrap();
        for k in ["chatbot_base_url", "chatbot_api_key", "chatbot_model"] {
            v.as_object_mut().unwrap().remove(k);
        }
        let old: Settings = serde_json::from_value(v).unwrap();
        assert_eq!(old.chatbot_base_url, "https://tokenharbor.ai/v1");
        assert_eq!(old.chatbot_model, "deepseek-v4.1-flash:free");
        assert!(old.chatbot_api_key.is_empty());
    }

    #[test]
    fn effective_proxy_prefers_task_override() {
        assert_eq!(effective_proxy("", "http://g:1"), "http://g:1");
        assert_eq!(effective_proxy("  ", "http://g:1"), "http://g:1");
        assert_eq!(
            effective_proxy("socks5://t:2", "http://g:1"),
            "socks5://t:2"
        );
        assert_eq!(effective_proxy("", ""), "");
    }

    #[test]
    fn table_layout_persists_and_defaults_when_absent() {
        let s = Settings {
            dl_sort_col: 5,
            dl_sort_asc: false,
            dl_col_widths: vec![200.0, 90.0],
            dl_col_widths_compact: vec![140.0, 70.0],
            grab_col_widths: vec![300.0, 80.0, 90.0],
            ..Default::default()
        };
        let back: Settings = serde_json::from_value(serde_json::to_value(&s).unwrap()).unwrap();
        assert_eq!(back.dl_sort_col, 5);
        assert!(!back.dl_sort_asc);
        assert_eq!(back.dl_col_widths, vec![200.0, 90.0]);
        assert_eq!(back.dl_col_widths_compact, vec![140.0, 70.0]);
        assert_eq!(back.grab_col_widths, vec![300.0, 80.0, 90.0]);
        // Old state without table fields deserializes to defaults.
        let mut v = serde_json::to_value(Settings::default()).unwrap();
        for k in [
            "dl_sort_col",
            "dl_sort_asc",
            "dl_col_widths",
            "dl_col_widths_compact",
            "grab_col_widths",
        ] {
            v.as_object_mut().unwrap().remove(k);
        }
        let old: Settings = serde_json::from_value(v).unwrap();
        assert_eq!(old.dl_sort_col, 0);
        assert!(old.dl_sort_asc);
        assert!(
            old.dl_col_widths.is_empty()
                && old.dl_col_widths_compact.is_empty()
                && old.grab_col_widths.is_empty()
        );
    }

    #[test]
    fn badge_colors_persist_and_default_when_absent() {
        let s = Settings {
            category_colors: vec![("Video".into(), [196, 138, 216])],
            type_colors: vec![("Ftp".into(), [127, 179, 170])],
            ..Default::default()
        };
        let back: Settings = serde_json::from_value(serde_json::to_value(&s).unwrap()).unwrap();
        assert!(back.nav_count_badges);
        assert_eq!(back.category_colors, s.category_colors);
        assert_eq!(back.type_colors, s.type_colors);
        // Old state without badge fields: badges on, theme default colors.
        let mut v = serde_json::to_value(Settings::default()).unwrap();
        for k in ["nav_count_badges", "category_colors", "type_colors"] {
            v.as_object_mut().unwrap().remove(k);
        }
        let old: Settings = serde_json::from_value(v).unwrap();
        assert!(old.nav_count_badges);
        assert!(old.category_colors.is_empty() && old.type_colors.is_empty());
    }

    #[test]
    fn nntp_connections_defaults_30_when_absent() {
        assert_eq!(Settings::default().nntp_connections, 30);
        let mut v = serde_json::to_value(Settings::default()).unwrap();
        v.as_object_mut().unwrap().remove("nntp_connections");
        let s: Settings = serde_json::from_value(v).unwrap();
        assert_eq!(
            s.nntp_connections, 30,
            "old state without field defaults 30"
        );
    }

    #[test]
    fn notifications_default_true_when_absent() {
        assert!(Settings::default().notifications);
        let mut v = serde_json::to_value(Settings::default()).unwrap();
        v.as_object_mut().unwrap().remove("notifications");
        let s: Settings = serde_json::from_value(v).unwrap();
        assert!(s.notifications, "old state without field defaults on");
    }

    #[test]
    fn settings_export_import_roundtrip_incl_debrid_tokens() {
        // Dummy fixture tokens only; never real credentials.
        let s = Settings {
            debrid_rd: DebridProviderCfg {
                api_key: "fixture-token".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        let s = Settings {
            debrid_preferred: "rd".into(),
            speed_limit_kbps: 123,
            ..s
        };
        let exported = settings_export_json(&s).unwrap();
        assert_eq!(parse_settings_import(&exported), Ok(s.clone()));
        // Token present in export, length only checked (present + nonzero).
        let v: serde_json::Value = serde_json::from_str(&exported).unwrap();
        assert!(v["debrid_rd"]["api_key"]
            .as_str()
            .is_some_and(|t| !t.is_empty()));
    }

    #[test]
    fn settings_import_partial_file_uses_defaults() {
        // Missing fields keep serde defaults, do not fail.
        let s = parse_settings_import(r#"{"speed_limit_kbps":42}"#).unwrap();
        assert_eq!(s.speed_limit_kbps, 42);
        assert_eq!(s.max_connections, Settings::default().max_connections);
        assert_eq!(
            s.debrid_rd,
            DebridProviderCfg::default(),
            "missing debrid cfg defaults"
        );
    }

    #[test]
    fn settings_mpv_defaults_roundtrip_and_old_state() {
        let s = Settings::default();
        assert_eq!(s.mpv_volume, 100);
        assert!((s.mpv_speed - 1.0).abs() < f32::EPSILON);
        assert!(s.mpv_cache);
        assert_eq!(s.mpv_cache_secs, 30);
        assert!(s.mpv_path.is_empty() && s.mpv_start.is_empty() && s.mpv_hwdec.is_empty());
        assert!(s.mpv_sub_auto.is_empty() && s.mpv_extra_args.is_empty());
        assert!(!s.mpv_fullscreen && !s.mpv_keep_open && !s.mpv_loop);
        let back: Settings = serde_json::from_value(serde_json::to_value(&s).unwrap()).unwrap();
        assert_eq!(
            serde_json::to_value(&back).unwrap(),
            serde_json::to_value(&s).unwrap()
        );
        // Old state without mpv_* keys: serde defaults fill them.
        let mut v = serde_json::to_value(Settings::default()).unwrap();
        let obj = v.as_object_mut().unwrap();
        for k in [
            "mpv_path",
            "mpv_fullscreen",
            "mpv_keep_open",
            "mpv_loop",
            "mpv_volume",
            "mpv_speed",
            "mpv_start",
            "mpv_hwdec",
            "mpv_cache",
            "mpv_cache_secs",
            "mpv_sub_auto",
            "mpv_extra_args",
        ] {
            obj.remove(k);
        }
        let old: Settings = serde_json::from_value(v).unwrap();
        assert_eq!(old.mpv_volume, 100);
        assert!((old.mpv_speed - 1.0).abs() < f32::EPSILON);
        assert!(old.mpv_cache);
        assert_eq!(old.mpv_cache_secs, 30);
    }

    #[test]
    fn settings_import_invalid_json_leaves_settings_unchanged() {
        let before = Settings {
            speed_limit_kbps: 99,
            ..Default::default()
        };
        let mut current = before.clone();
        if let Ok(imported) = parse_settings_import("{not json") {
            current = imported;
        }
        assert_eq!(current.speed_limit_kbps, before.speed_limit_kbps);
        assert!(parse_settings_import("{not json").is_err());
        assert!(parse_settings_import("null").is_err());
        assert!(parse_settings_import(r#"{"tasks":[]}"#).is_err());
    }

    #[test]
    fn filename_stem_is_weak_for_title_cases() {
        let x = "https://x.com/u/status/2112294755962949700";
        assert!(filename_stem_is_weak_for_title("x.mp4", x)); // host label
        assert!(filename_stem_is_weak_for_title("X.mp4", x)); // case-insens.
        assert!(filename_stem_is_weak_for_title(
            "2112294755962949700.mp4",
            x
        )); // digits
        assert!(filename_stem_is_weak_for_title("u.mp4", x)); // path seg + len
        assert!(filename_stem_is_weak_for_title(
            "ab.mp4",
            "https://youtube.com/watch?v=1"
        )); // len<=2
        assert!(filename_stem_is_weak_for_title(
            "",
            "https://youtube.com/watch?v=1"
        )); // empty
        assert!(filename_stem_is_weak_for_title(
            "123.mp4",
            "https://youtube.com/watch?v=1"
        ));
        assert!(!filename_stem_is_weak_for_title(
            "My Video.mp4",
            "https://youtube.com/watch?v=1"
        ));
        assert!(!filename_stem_is_weak_for_title(
            "video.mp4",
            "https://youtube.com/watch?v=123"
        ));
        // Direct-media: filename == last segment verbatim -> not weak.
        assert!(!filename_stem_is_weak_for_title(
            "video.mp4",
            "https://cdn.example.com/video.mp4"
        ));
        assert!(!filename_stem_is_weak_for_title(
            "Video.MP4",
            "https://cdn.example.com/video.mp4"
        ));
        // Stem == non-final path segment -> weak.
        assert!(filename_stem_is_weak_for_title("status.mp4", x));
    }

    #[test]
    fn debrid_choice_parsing() {
        let mut task = DownloadTask::default();
        assert_eq!(debrid_choice_of(&task), "auto");
        task.debrid_choice = " RD ".into();
        assert_eq!(debrid_choice_of(&task), "rd");
        task.debrid_choice = "none".into();
        assert_eq!(debrid_choice_of(&task), "none");
        task.debrid_choice = "junk".into();
        assert_eq!(debrid_choice_of(&task), "auto");
        // old state without field still parses
        let old: DownloadTask = serde_json::from_str(r#"{"id":"x","url":"https://e/f.bin","filename":"f.bin","dest_dir":"."}"#).unwrap();
        assert_eq!(debrid_choice_of(&old), "auto");
    }

    #[test]
    fn unique_and_taken_cases() {
        let dir = std::env::temp_dir().join(format!("webcraft-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let taken = vec!["a.mp4".to_string(), "b.mp4".to_string()];
        assert_eq!(unique_filename(&dir, "free.mp4", &taken), "free.mp4");
        assert_eq!(unique_filename(&dir, "a.mp4", &taken), "a_1.mp4");
        assert_eq!(unique_filename(&dir, "A.mp4", &taken), "A_1.mp4"); // case-insens.
        std::fs::write(dir.join("a_1.mp4"), b"").unwrap();
        assert_eq!(unique_filename(&dir, "a.mp4", &taken), "a_2.mp4"); // skips disk _1
        assert!(name_taken(&dir, "b.mp4", &taken, "self.mp4"));
        assert!(name_taken(&dir, "a_1.mp4", &taken, "self.mp4")); // disk only
        assert!(!name_taken(&dir, "free.mp4", &taken, "self.mp4"));
        assert!(!name_taken(&dir, "a_1.mp4", &taken, "a_1.mp4")); // own disk file = self
        let _ = std::fs::remove_file(dir.join("a_1.mp4"));
        let _ = std::fs::remove_dir(&dir);
    }
}
