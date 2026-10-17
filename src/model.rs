use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[derive(Default)]
pub enum Priority { Low, #[default]
Normal, High }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[derive(Default)]
pub enum TaskStatus { #[default]
Queued, Downloading, Paused, Complete, Error, Protected }

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct DownloadTask {
    pub id: String,
    pub url: String,
    pub filename: String,
    pub dest_dir: PathBuf,
    #[serde(default)] pub queue_id: String,
    #[serde(default)] pub priority: Priority,
    #[serde(default)] pub order: i64,
    #[serde(default)] pub status: TaskStatus,
    #[serde(default)] pub total: Option<u64>,
    #[serde(default)] pub downloaded: u64,
    #[serde(default)] pub referrer: String,
    #[serde(default)] pub cookies: String,
    #[serde(default)] pub headers: BTreeMap<String, String>,
    #[serde(default)] pub segments: u32,
    #[serde(default)] pub sha256: Option<String>,
    #[serde(default)] pub error: String,
    #[serde(default)] pub category: String,
    #[serde(default)] pub speed_limit_kbps: u64,
    #[serde(default)] pub attempts: u32,
    #[serde(default)] pub next_retry_at: u64,
    #[serde(default)] pub created_at: u64,
    /// Mirror URLs (IDM registry Url1..UrlN beside primary Url0).
    #[serde(default)] pub mirrors: Vec<String>,
    /// Per-URL credentials (IDM U0_u/U0_EncP). Stored plain in state.json — ponytail: no OS keychain; add keyring when credential theft matters.
    #[serde(default)] pub username: String,
    #[serde(default)] pub password: String,
    #[serde(default)] pub last_try_at: u64,
    #[serde(default)] pub description: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[derive(Default)]
pub enum CompletionAction { #[default]
None, OpenFile, RunCommand, ExitApp, Shutdown, Sleep }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Queue {
    pub id: String,
    pub name: String,
    #[serde(default = "def_one")] pub max_concurrent: usize,
    #[serde(default)] pub auto_start: bool,
    #[serde(default)] pub retry_count: u32,
    #[serde(default)] pub retry_interval_secs: u64,
    #[serde(default)] pub on_complete: CompletionAction,
    #[serde(default)] pub on_complete_arg: String,
}
fn def_one() -> usize { 1 }
fn def_true() -> bool { true }
impl Default for Queue {
    fn default() -> Self { Self { id: "main".into(), name: "Main".into(), max_concurrent: 3, auto_start: true, retry_count: 3, retry_interval_secs: 10, on_complete: CompletionAction::None, on_complete_arg: String::new() } }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[derive(Default)]
pub enum Recurrence { Once, #[default]
Daily, Weekdays, Weekly }

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Schedule {
    pub id: String,
    #[serde(default)] pub name: String,
    pub queue_id: String,
    #[serde(default)] pub enabled: bool,
    #[serde(default)] pub recurrence: Recurrence,
    /// days-bitmask Mon=1 Tue=2 Wed=4 Thu=8 Fri=16 Sat=32 Sun=64 (Weekly)
    #[serde(default)] pub weekdays_mask: u8,
    /// YYYYMMDD for Once; also anchor date for repeat_days cycle
    #[serde(default)] pub once_date: u32,
    /// Repeat each N days (Daily only; 0/1 = every day). IDM Scheduler field.
    #[serde(default)] pub repeat_days: u32,
    #[serde(default)] pub start_secs: u32,
    #[serde(default)] pub stop_secs: Option<u32>,
    #[serde(default)] pub files_at_once: usize,
    #[serde(default)] pub run_missed: bool,
    #[serde(default)] pub last_started_day: i64,
    /// Day stop was last applied. Latched so scheduler stop fires once/day and
    /// cannot re-Pause a task the user resumed manually after stop time.
    #[serde(default)] pub last_stopped_day: i64,
    #[serde(default)] pub on_complete: CompletionAction,
    #[serde(default)] pub power_confirmed: bool,
    /// Optional bandwidth window: kbps applied while schedule window active. 0 = unlimited.
    #[serde(default)] pub limit_kbps: u64,
    /// Quota scheduler (IDM Scheduler reg: isLimitEnabled/m_MBytes/m_hours): stop queue after quota_mb per quota_hours. 0 = off.
    #[serde(default)] pub quota_mb: u64,
    #[serde(default)] pub quota_hours: u64,
    #[serde(default)] pub quota_window_start: u64,
    #[serde(default)] pub quota_used_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    pub download_dir: PathBuf,
    #[serde(default = "def_conn")] pub max_connections: u32,
    #[serde(default = "def_global")] pub global_max_concurrent: usize,
    #[serde(default)] pub speed_limit_kbps: u64,
    #[serde(default)] pub autostart: bool,
    #[serde(default)] pub start_minimized: bool,
    #[serde(default)] pub close_to_tray: bool,
    #[serde(default)] pub clipboard_monitor: bool,
    #[serde(default)] pub proxy_url: String,
    /// Master "Turn on scheduler" (IDM Scheduler dialog). Default on.
    #[serde(default = "def_true")] pub scheduler_enabled: bool,
}
fn def_conn() -> u32 { 8 }
fn def_global() -> usize { 3 }
impl Default for Settings {
    fn default() -> Self {
        Self { download_dir: crate::platform::default_download_dir(), max_connections: 8, global_max_concurrent: 3, speed_limit_kbps: 0, autostart: false, start_minimized: false, close_to_tray: true, clipboard_monitor: false, proxy_url: String::new(), scheduler_enabled: true }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct AppState {
    #[serde(default)] pub tasks: Vec<DownloadTask>,
    #[serde(default)] pub queues: Vec<Queue>,
    #[serde(default)] pub schedules: Vec<Schedule>,
    #[serde(default)] pub settings: Settings,
}

pub fn new_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("{n:x}")
}

pub fn sanitize_filename(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let cleaned: String = base.chars().map(|c| if matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || (c as u32) < 32 { '_' } else { c }).collect();
    let t = cleaned.trim().trim_end_matches('.').trim();
    if t.is_empty() { "download.bin".into() } else { t.to_string() }
}

pub fn filename_from_url(url: &str) -> String {
    let clean = url.split(['?', '#']).next().unwrap_or(url);
    let last = clean.rsplit('/').next().unwrap_or("");
    let decoded = percent_decode(last);
    if decoded.is_empty() { "download.bin".into() } else { sanitize_filename(&decoded) }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes(); let mut out = Vec::new(); let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() + 1 && i + 2 <= b.len() - 1 + 1 {
            if i + 2 < b.len() || i + 2 == b.len() { /* guard below */ }
            if i + 2 < b.len() {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("");
                if let Ok(v) = u8::from_str_radix(hex, 16) { out.push(v); i += 3; continue; }
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] }); i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn category_for(filename: &str) -> &'static str {
    let ext = filename.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "zip" | "rar" | "7z" | "gz" | "bz2" | "tar" | "xz" => "Compressed",
        "mp4" | "mkv" | "avi" | "mov" | "wmv" | "webm" | "mpg" | "mpeg" | "m3u8" | "mpd" => "Video",
        "mp3" | "wav" | "flac" | "ogg" | "aac" | "m4a" | "wma" => "Audio",
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "txt" => "Documents",
        "exe" | "msi" | "dmg" | "deb" | "rpm" | "appimage" | "apk" => "Programs",
        _ => "General",
    }
}
