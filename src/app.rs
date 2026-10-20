use crate::engine;
use crate::grabber::{Crawler, FoundFile, GrabberProject};
use crate::icons::{icon, Icon};
use crate::model::*;
use crate::platform;
use crate::queue::plan_starts;
use crate::scheduler::{missed_run, next_run_in, should_start, should_stop};
use crate::theme;
use crate::Runtime;
use downloadcraft_lib::update::{self, CheckOutcome};
use downloadcraft_lib::ytdlp;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Downloads,
    Queues,
    Scheduler,
    Grabber,
    Streams,
    Options,
}

/// Add-dialog torrent picker probe result: (trimmed URL probed, result).
/// URL tag drops stale results after user edits URL mid-probe.
type TorrentProbeSlot = std::sync::Arc<
    std::sync::Mutex<Option<(String, Result<Vec<crate::torrent::TorrentFile>, String>)>>,
>;

/// Add-dialog NZB probe result: (trimmed URL probed, result). Same stale-drop.
type NzbProbeSlot =
    std::sync::Arc<std::sync::Mutex<Option<(String, Result<crate::nntp::Nzb, String>)>>>;

/// yt-dlp -J probe result slot: (url probed, result). Same stale-drop pattern.
type YtProbeSlot =
    std::sync::Arc<std::sync::Mutex<Option<(String, Result<ytdlp::YtDump, String>)>>>;

/// HTTP prewarm state (Add + capture dialogs): last spawned URL + result
/// text ("Probing…" / "2.9 GB · 8 connections" / error). Runtime-only;
/// probe payload lives in engine::prewarm registry, never persisted/logged.
#[derive(Default)]
struct PrewarmState {
    url: String,
    loading: bool,
    text: String,
}

/// Prewarm gate: plain http(s) files only. Skips FTP/SFTP/torrent/NZB,
/// yt-dlp video pages (yt probe owns them), manifests (stream probe),
/// explicit yt-dlp tasks (non-http schemes excluded by prefix check).
fn prewarm_eligible(task: &DownloadTask) -> bool {
    let u = task.url.trim();
    let l = u.to_ascii_lowercase();
    if !l.starts_with("http://") && !l.starts_with("https://") {
        return false;
    }
    if task.headers.contains_key("x-ytdlp")
        || ytdlp::is_video_page(u)
        || crate::torrent::is_torrent_url(u)
        || crate::stream::is_nzb_url_str(u)
    {
        return false;
    }
    !crate::stream::is_manifest_url(u)
}

/// Spawn engine prewarm on tokio rt (non-blocking; confirm never waits).
/// No-op when same URL already probed/in flight. `conns` = planned segments cap.
fn spawn_prewarm(st: &mut PrewarmState, rt: &tokio::runtime::Runtime, task: &DownloadTask, conns: u32) {
    let url = task.url.trim().to_string();
    if url.is_empty() || st.url == url {
        return;
    }
    st.url = url.clone();
    st.loading = true;
    st.text = "Probing…".into();
    let mut probe_task = task.clone();
    probe_task.url = url.clone();
    let client = engine::build_client("");
    let slot = prewarm_ui_slot();
    rt.spawn(async move {
        match engine::prewarm(&client, &probe_task).await {
            Ok(cache) => {
                let text = match cache.total {
                    Some(t) if cache.accept_ranges => {
                        let n = engine::plan_segments(t, conns.max(1)).len();
                        format!("{} · {} connections", fmt_bytes(t), n)
                    }
                    Some(t) => format!("{} · single connection", fmt_bytes(t)),
                    None => "Size unknown".into(),
                };
                engine::prewarm_store(cache);
                if let Ok(mut g) = slot.lock() {
                    *g = Some((url, Ok(text)));
                }
            }
            Err(e) => {
                if let Ok(mut g) = slot.lock() {
                    *g = Some((url, Err(e)));
                }
            }
        }
    });
}

/// Prewarm UI result slot: (url, display text). Same stale-drop as yt probe.
type PrewarmSlot =
    std::sync::Arc<std::sync::Mutex<Option<(String, Result<String, String>)>>>;
static PREWARM_UI: std::sync::OnceLock<PrewarmSlot> = std::sync::OnceLock::new();

fn prewarm_ui_slot() -> PrewarmSlot {
    PREWARM_UI
        .get_or_init(|| std::sync::Arc::new(std::sync::Mutex::new(None)))
        .clone()
}

/// Poll prewarm result for `url`; changed URL resets (caller respawns).
fn prewarm_poll(st: &mut PrewarmState, url: &str) {
    let url = url.trim();
    if st.url != url {
        st.url = url.to_string();
        st.loading = false;
        st.text.clear();
        return;
    }
    if !st.loading {
        return;
    }
    let done = prewarm_ui_slot().lock().ok().and_then(|mut g| g.take());
    if let Some((tag, res)) = done {
        if tag == url {
            st.loading = false;
            st.text = match res {
                Ok(t) => t,
                Err(_) => String::new(), // probe failure silent: confirm does normal probe
            };
        }
    }
}

/// Add-dialog debrid magnet awaiting fan-out resolution.
#[derive(Clone)]
struct PendingDebridAdd {
    url: String,
    referrer: String,
    cookies: String,
    queue_id: String,
    start_now: bool,
}

type DebridAddSlot =
    std::sync::Arc<std::sync::Mutex<Option<Result<Vec<crate::debrid::ResolvedFile>, String>>>>;
type DebridHostsSlot =
    std::sync::Arc<std::sync::Mutex<Option<(String, Vec<crate::debrid::HostStatus>)>>>;

/// Debrid Options probes: ("provider_id:account"|"provider_id:hosts", result text).
/// Tokens never in slot, never logged.
type DebridProbeSlot = std::sync::Arc<std::sync::Mutex<Option<(String, Result<String, String>)>>>;

/// In-memory debrid hoster cache (Options + stale refresh). Not persisted.
#[derive(Default)]
struct DebridUiCache {
    /// provider id -> hosts
    hosts: std::collections::HashMap<String, Vec<crate::debrid::HostStatus>>,
    /// provider id -> last fetch (unix secs)
    checked_at: std::collections::HashMap<String, i64>,
    /// provider id -> account summary line
    account: std::collections::HashMap<String, String>,
    /// provider id -> last error
    error: std::collections::HashMap<String, String>,
    /// provider ids with probe in flight
    busy: std::collections::HashSet<String>,
    /// auto-refresh attempted this Options visit per provider
    stale_checked: std::collections::HashSet<String>,
}

#[derive(Default)]
struct YtProbeState {
    slot: Option<YtProbeSlot>,
    url: String,
    loading: bool,
    error: String,
    dump: Option<ytdlp::YtDump>,
    /// "" = Auto (best), format_id, or "audio:<id>".
    format: String,
    /// "" = Auto (best), else max height tier e.g. "1080". Mutually exclusive with `format`.
    quality: String,
    /// "" = auto, "best", "none", or audio format_id.
    audio: String,
    /// "" = keep, "mp3", "m4a".
    extract: String,
    subs: std::collections::HashSet<String>,
    auto_subs: std::collections::HashSet<String>,
    sub_embed: bool,
}

/// Live per-part progress snapshot for one task.
/// Segmented HTTP: one entry per part, (done_bytes, expected_bytes).
/// Manifest downloads: single entry (done_segments, total_segments).
#[derive(Clone, Default)]
pub struct SegView {
    pub parts: Vec<(u64, u64)>,
    /// Torrent files aligned 1:1 with parts: (librqbit index, relative path).
    pub files: Vec<(usize, String)>,
    pub manifest: bool,
    /// Download type: NNTP multi-file parts are files, torrent part is whole torrent.
    pub kind: crate::stream::ProtoKind,
    /// Torrent swarm snapshot (torrent kind only).
    pub torrent: Option<crate::engine::TorrentSnap>,
}
impl SegView {
    fn label(&self) -> String {
        if self.manifest {
            let (d, t) = self.parts.first().copied().unwrap_or((0, 0));
            format!("{d} / {t} segments")
        } else if self.kind == crate::stream::ProtoKind::Nntp {
            let done = self.parts.iter().filter(|(d, e)| e > &0 && d >= e).count();
            format!("{done} / {} files done", self.parts.len())
        } else {
            let done = self.parts.iter().filter(|(d, e)| e > &0 && d >= e).count();
            format!("{done} / {} parts done", self.parts.len())
        }
    }
}

impl SegView {
    /// Overall completion when byte total is unavailable but segment counts are.
    fn fraction(&self) -> Option<f32> {
        let (done, expected) = self.parts.first().copied()?;
        if self.manifest && expected > 0 {
            Some((done as f32 / expected as f32).clamp(0.0, 1.0))
        } else {
            None
        }
    }
}

/// Extract URL from pasted text: HTML `href=`/`src=` attribute first
/// (quoted or bare value), else first bare http(s) token, else trimmed input.
/// Hand-scan, no regex crate.
fn extract_pasted_url(text: &str) -> String {
    let t = text.trim();
    if t.contains('<') {
        let lower = t.to_ascii_lowercase();
        for attr in ["href=", "src="] {
            let mut from = 0usize;
            while let Some(off) = lower[from..].find(attr) {
                let i = from + off + attr.len();
                let rest = &t[i..];
                match rest.chars().next() {
                    Some(q @ ('"' | '\'')) => {
                        if let Some(end) = rest[1..].find(q) {
                            let v = &rest[1..1 + end];
                            if v.starts_with("http://") || v.starts_with("https://") {
                                return v.to_string();
                            }
                        }
                    }
                    Some(_) => {
                        let end = rest
                            .find(|c: char| {
                                c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>')
                            })
                            .unwrap_or(rest.len());
                        let v = &rest[..end];
                        if v.starts_with("http://") || v.starts_with("https://") {
                            return v.to_string();
                        }
                    }
                    None => {}
                }
                from = i;
            }
        }
    }
    // First bare http(s) token.
    for tok in t.split(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>')) {
        if tok.starts_with("http://") || tok.starts_with("https://") {
            return tok.to_string();
        }
    }
    t.to_string()
}

/// Add-dialog yt-dlp probe gate: any http(s) URL except torrent/NZB (own
/// pickers own those) and direct media files (probe adds nothing; native
/// path handles them). Includes .m3u8/.mpd/pages/playlists. Extension check
/// on query/fragment-stripped path.
fn yt_probe_eligible(url: &str) -> bool {
    let u = url.trim();
    let lower = u.to_ascii_lowercase();
    if !lower.starts_with("http://") && !lower.starts_with("https://") {
        return false;
    }
    if crate::torrent::is_torrent_url(u) || crate::stream::is_nzb_url_str(u) {
        return false;
    }
    let path = lower.split(['?', '#']).next().unwrap_or(&lower);
    const MEDIA: &[&str] = &[
        ".mp4", ".mkv", ".webm", ".mov", ".avi", ".mpg", ".mpeg", ".mp3", ".wav", ".flac", ".ogg",
        ".aac", ".m4a", ".wma",
    ];
    !MEDIA.iter().any(|e| path.ends_with(e))
}

/// WebDAV is plain HTTP(S) download: map webdav:// -> http://, webdavs:// -> https://.
fn normalize_webdav(url: &str) -> String {
    let t = url.trim();
    if let Some(rest) = t.strip_prefix("webdav://") {
        format!("http://{rest}")
    } else if let Some(rest) = t.strip_prefix("webdavs://") {
        format!("https://{rest}")
    } else {
        t.to_string()
    }
}

/// Effective per-Controls KB/s cap: own limit wins, else global split
/// across active tasks (count includes own-limit tasks, so no-own sum
/// never exceeds global; own tasks sit on top by design).
/// ponytail: static division, not token bucket — idle shares not reclaimed.
/// Upgrade: shared token bucket in engine::throttle when that matters.
fn effective_limit_kbps(global_kbps: u64, active_count: usize, own_kbps: u64) -> u64 {
    if own_kbps > 0 {
        own_kbps
    } else if global_kbps == 0 {
        0
    } else {
        global_kbps / (active_count.max(1) as u64)
    }
}

fn apply_effective_limits(
    state: &AppState,
    controls: &std::collections::HashMap<String, Arc<engine::Controls>>,
) {
    let count = controls.len();
    for (id, c) in controls {
        let own = state
            .tasks
            .iter()
            .find(|t| &t.id == id)
            .map(|t| t.speed_limit_kbps)
            .unwrap_or(0);
        let eff = effective_limit_kbps(state.settings.speed_limit_kbps, count, own);
        if c.limit_kbps.load(Ordering::Relaxed) != eff {
            c.limit_kbps.store(eff, Ordering::Relaxed);
        }
    }
}

/// Progress-dialog file toggle: after toggling `idx`, rebuild stored selection.
/// Full selection canonicalizes to empty vec (= all). Empty selection -> None
/// (rejected: task could never finish; caller ignores toggle).
fn toggled_selection(
    cur: &[usize],
    total_files: usize,
    idx: usize,
    on: bool,
) -> Option<Vec<usize>> {
    let mut set: std::collections::HashSet<usize> = if cur.is_empty() {
        (0..total_files).collect()
    } else {
        cur.iter().copied().filter(|i| *i < total_files).collect()
    };
    if on {
        set.insert(idx);
    } else {
        set.remove(&idx);
    }
    if set.is_empty() {
        return None;
    }
    if set.len() == total_files {
        return Some(Vec::new());
    }
    let mut v: Vec<usize> = set.into_iter().collect();
    v.sort_unstable();
    Some(v)
}

/// IDM-style displayed completion: byte fraction first, segment fraction for manifests.
fn display_fraction(task: &DownloadTask, view: Option<&SegView>) -> Option<f32> {
    if let Some(total) = task.total.filter(|t| *t > 0) {
        return Some((task.downloaded as f32 / total as f32).clamp(0.0, 1.0));
    }
    if task.status == TaskStatus::Complete {
        return Some(1.0);
    }
    view.and_then(|v| v.fraction())
}

/// Task URL schemes whose engine keeps partial state and continues from it:
/// segmented/streamed HTTP(S) (probe decides ranges), FTP REST, SFTP seek,
/// BitTorrent fast-resume, HLS/DASH segment caches. NNTP/yt-dlp excluded.
fn resume_capable(task: &DownloadTask) -> bool {
    let l = task.url.to_ascii_lowercase();
    let path = l.split(['?', '#']).next().unwrap_or(&l);
    !(l.starts_with("nzb://")
        || path.ends_with(".nzb")
        || task.headers.contains_key("x-nzb")
        || task.headers.contains_key("x-ytdlp")
        || ytdlp::is_video_page(&task.url))
}

/// Resume evidence for progress dialog (no probe I/O): live part bytes on
/// disk mean continuation happened/is possible; reaching part-dir checks
/// needs segmented layout, so plain `.part` partials (FTP/SFTP) count too.
fn resume_supported(task: &DownloadTask) -> bool {
    if !resume_capable(task) {
        return false;
    }
    if task.status == TaskStatus::Complete {
        return true;
    }
    let part_dir = task.dest_dir.join(format!(".downloadcraft-{}", task.id));
    if std::fs::read_dir(&part_dir)
        .map(|mut rd| {
            rd.any(|e| {
                e.map(|e| {
                    e.file_name().to_string_lossy().starts_with("part")
                        && e.metadata().map(|m| m.len() > 0).unwrap_or(false)
                })
                .unwrap_or(false)
            })
        })
        .unwrap_or(false)
    {
        return true;
    }
    task.downloaded > 0
        && (std::fs::metadata(task.dest_dir.join(format!("{}.part", task.filename)))
            .map(|m| m.len() > 0)
            .unwrap_or(false)
            || task
                .dest_dir
                .join(format!(".downloadcraft-media-{}", task.id))
                .exists()
            || task
                .dest_dir
                .join(format!(".downloadcraft-hls-{}", task.id))
                .exists())
}

/// Per-part state word from (done, expected): expected 0 = unknown length.
fn part_status_word(done: u64, expected: u64) -> Option<&'static str> {
    if expected == 0 {
        None
    } else if done >= expected {
        Some("Finished")
    } else if done > 0 {
        Some("Receiving")
    } else {
        Some("Pending")
    }
}

/// Rates (bytes/s) between consecutive per-task samples: (time used for
/// x-scaling only — sparkline x is index-based, matching global sparkline).
fn task_speed_series(samples: &[(f64, u64)]) -> Vec<f64> {
    samples
        .windows(2)
        .filter_map(|w| {
            let dt = w[1].0 - w[0].0;
            (dt > 0.0).then(|| w[1].1.saturating_sub(w[0].1) as f64 / dt)
        })
        .collect()
}

/// Draw per-part strip: each cell width proportional to expected size, fill = done fraction.
fn parts_strip(ui: &mut egui::Ui, view: &SegView, width: f32) {
    if view.parts.is_empty() {
        return;
    }
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 10.0), egui::Sense::hover());
    let total_exp: u64 = view.parts.iter().map(|(_, e)| *e).sum::<u64>().max(1);
    let painter = ui.painter();
    let mut x = rect.left();
    for (done, exp) in &view.parts {
        let w = rect.width() * (*exp as f32 / total_exp as f32);
        let cell = egui::Rect::from_min_size(
            egui::pos2(x, rect.top()),
            egui::vec2((w - 1.0).max(1.0), rect.height()),
        );
        painter.rect_filled(cell, 0.0, theme::G_700);
        let frac = if *exp > 0 {
            (*done as f32 / *exp as f32).clamp(0.0, 1.0)
        } else {
            0.0
        };
        if frac > 0.0 {
            let fill =
                egui::Rect::from_min_size(cell.min, egui::vec2(cell.width() * frac, cell.height()));
            painter.rect_filled(fill, 2.0, theme::PRIMARY);
        }
        x += w;
    }
}

#[derive(Default)]
struct GrabOutcome {
    done: bool,
    apply_files: bool,
    files: Vec<FoundFile>,
    message: String,
    progress: Option<Arc<Mutex<crate::grabber::CrawlProgress>>>,
}

pub struct DownloadApp {
    rt: Arc<Mutex<Runtime>>,
    tokio_rt: Arc<tokio::runtime::Runtime>,
    tab: Tab,
    add_url: String,
    add_referrer: String,
    add_cookies: String,
    add_user: String,
    add_pass: String,
    add_key: String,
    /// Custom HTTP(S)-only header draft rows (name, value); empty rows ignored.
    add_headers: Vec<(String, String)>,
    add_mirrors: String,
    add_desc: String,
    add_queue: String,
    add_start_now: bool,
    add_just_opened: bool,
    /// Frames left to force-raise Add viewport (egui Focus denied when
    /// browser holds foreground; platform::raise_window covers it).
    add_raise_frames: u32,
    /// URL field re-focused once Add viewport first reports focused.
    add_focus_done: bool,
    /// Torrent file-picker probe result slot (list_only, spawned on thread).
    add_torrent_slot: TorrentProbeSlot,
    /// URL the picker probe was run for (edit invalidates probe).
    add_torrent_url: String,
    add_torrent_files: Vec<(crate::torrent::TorrentFile, bool)>,
    add_torrent_loading: bool,
    add_torrent_error: String,
    /// Debrid magnet fan-out: confirm spawns resolve, drain creates one
    /// task per resolved file (debrid_file_name set). Failed resolve ->
    /// normal single magnet task (stream falls back to local torrent).
    debrid_add_slot: DebridAddSlot,
    pending_debrid_add: Option<PendingDebridAdd>,
    /// NZB probe result slot (parse-only, spawned on thread).
    add_nzb_slot: NzbProbeSlot,
    add_nzb_url: String,
    add_nzb: Option<crate::nntp::Nzb>,
    add_nzb_loading: bool,
    add_nzb_error: String,
    new_queue_name: String,
    grab_url: String,
    grab_depth: usize,
    grab_ext_depth: usize,
    grab_scope: u8,
    grab_exts: String,
    grab_parent_dirs: bool,
    grab_only_paths: String,
    grab_exclude_paths: String,
    grab_files: Vec<FoundFile>,
    grab_cookies: String,
    grab_project_json: String,
    grab_status: String,
    grab_busy: bool,
    grab_auto_explore: bool,
    stream_url: String,
    stream_status: String,
    /// true = stream_status is error (ERR color), else OK.
    stream_status_err: bool,
    last_tick: std::time::Instant,
    #[allow(dead_code)]
    tray: Option<TrayState>,
    hidden: bool,
    ytdlp_checked: bool,
    update_check: Option<CheckOutcome>,
    update_slot: std::sync::Arc<std::sync::Mutex<Option<CheckOutcome>>>,
    debrid_slot: DebridProbeSlot,
    debrid_hosts_slot: DebridHostsSlot,
    debrid_ctx: Option<egui::Context>,
    /// Clone of egui ctx for worker-completion wake (`start_task`
    /// spawned tasks). Set in `update()`; None until first frame.
    egui_ctx: Option<egui::Context>,
    debrid_ui: DebridUiCache,
    show_about: bool,
    search: String,
    /// Ctrl+F pressed: focus the topbar search field next frame.
    search_focus: bool,
    /// Visible matches vs total from last table render (status feedback).
    search_counts: (usize, usize),
    /// Grabber results sort: (col 0=Name,1=URL,2=Size,3=Type, asc).
    grab_sort: (u8, bool),
    /// Rail collapsed to icon-only (narrow window).
    rail_compact: bool,
    /// Last synced persisted table state (avoid saving state every frame).
    /// (sort_col, sort_asc, full widths, compact widths, grab widths).
    /// Widths kept per layout mode: full/compact vec lengths differ (8 vs 5),
    /// one shared vec would discard the other mode's widths on every
    /// breakpoint cross.
    table_state_synced: (u8, bool, Vec<f32>, Vec<f32>, Vec<f32>),
    minutes_today: (i64, u32),
    category: String,
    type_filter: Option<TypeSel>,
    sort_col: u8,
    sort_asc: bool,
    options_tab: u8,
    options_notice: String,
    show_add: bool,
    show_progress_for: Option<String>,
    progress_raise: bool,
    /// Frames left to force-raise Progress viewport (see add_raise_frames).
    prog_raise_frames: u32,
    /// Filename prefix of Progress viewport being raised (title match).
    prog_raise_title: String,
    selected: Option<String>,
    /// Multi-select set for downloads table (Ctrl/Shift/Ctrl+A). `selected`
    /// stays primary (last clicked) for toolbar/menus/progress.
    selected_ids: std::collections::HashSet<String>,
    /// Shift-click range anchor (downloads table).
    selection_anchor: Option<String>,
    /// Filtered+sorted task ids from last downloads render (Ctrl+A, Shift range).
    visible_ids: Vec<String>,
    /// Delete confirmation: ids pending removal (single, multi, completed).
    pending_delete: Option<Vec<String>>,
    /// Confirm-dialog checkbox: also remove downloaded files from disk.
    delete_from_disk: bool,
    selected_schedule: Option<String>,
    completed_notice: Option<String>, // task id
    /// Completion power action awaiting user confirm (mirrors Runtime.pending_power).
    pending_power_confirm: Option<crate::platform::PowerAction>,
    /// Task ids already Complete when app started (or already notified).
    /// Seeded in new() so restored history never pops "Download complete";
    /// only tasks finishing this session notify.
    seen_completed: std::collections::HashSet<String>,
    seen_failed: std::collections::HashSet<String>,
    /// Was any task active last poll; gates one "Queue finished" toast on drain.
    was_active: bool,
    speed_hist: Vec<f32>,
    last_dl_total: u64,
    /// Per-task rate samples (drives Transfer rate column + ETA per row).
    rate_map: std::collections::HashMap<String, Vec<(std::time::Instant, u64)>>,
    /// Live yt-dlp phase per task id (engine::phase_name); snapshot in poll.
    phase_map: std::collections::HashMap<String, u8>,
    /// Live segment/part snapshot per task (Controls.parts, fallback part-dir scan).
    seg_info: std::collections::HashMap<String, SegView>,
    /// Smoothed displayed rate per task (bytes/s), EMA over 5s window rate.
    rate_smooth: std::collections::HashMap<String, f64>,
    /// Per-task (epoch_s, downloaded) history for progress-dialog sparkline.
    /// Derived from `rate_map` each poll; capped 120 samples.
    task_hist: std::collections::HashMap<String, std::collections::VecDeque<(f64, u64)>>,
    /// Spawned download tasks; Remove aborts handle so background work really stops.
    handles: std::collections::HashMap<String, tokio::task::JoinHandle<()>>,
    /// Tray Quit / Tasks Quit set this so close-to-tray intercept lets window close.
    allow_close: bool,
    /// User-dragged column widths, downloads table per layout mode
    /// (persisted in Settings). One vec per mode: lengths differ
    /// (full 8, compact 5), mixing them discards widths on breakpoint cross.
    dl_widths: Option<Vec<f32>>,
    dl_widths_compact: Option<Vec<f32>>,
    grab_widths: Option<Vec<f32>>,
    /// IDM-style pre-download capture dialog state: task being confirmed +
    /// editable filename buffer (None = no capture dialog open).
    capture_task: Option<DownloadTask>,
    /// Edge guard: reveal window once per capture burst, not every poll while minimized.
    capture_revealed: bool,
    capture_name: String,
    /// True only for first frame after capture dialog opens: focus filename field.
    capture_just_opened: bool,
    /// Raise capture viewport once on pop (like progress_raise).
    capture_raise: bool,
    /// Frames left to force-raise Capture viewport (see add_raise_frames).
    cap_raise_frames: u32,
    /// Name field re-focused once Capture viewport first reports focused.
    cap_focus_done: bool,
    /// Startup trace: first update() reached (window/event loop live).
    first_update_traced: bool,
    yt_probe_add: YtProbeState,
    yt_probe_cap: YtProbeState,
    prewarm_add: PrewarmState,
    prewarm_cap: PrewarmState,
    add_vp_sent: Option<egui::Vec2>,
    cap_vp_sent: Option<egui::Vec2>,
    prog_vp_sent: Option<egui::Vec2>,
    player: Option<downloadcraft_lib::mpv::embedded::MpvHandle>,
    /// URL awaiting player viewport HWND (spawn on first ui_player frame).
    player_pending: Option<String>,
    player_seek: f64,
    player_seek_dragging: bool,
    player_speed: f64,
    /// Player viewport HWND resolved via EnumWindows by title (once, cached).
    player_hwnd: Option<isize>,
    /// Last resolved (paused,duration) for title/status; stale-tolerant.
    player_title_state: (bool, f64),
    prog_task: Option<String>,
    /// Last frame's measured dialog body height (+ closed-section heights);
    /// fed to fit_vp next frame so native viewports fit content as if all
    /// collapsible sections were expanded. None = not measured yet.
    cap_body_h: Option<f32>,
    add_body_h: Option<f32>,
    prog_body_h: Option<f32>,
}

/// Tray toggle label for current window visibility.
fn tray_toggle_label(visible: bool) -> &'static str {
    if visible {
        "Hide webcraft"
    } else {
        "Show webcraft"
    }
}

/// Live tray state: keep icon+menu alive; poll MenuEvent receiver each tick for Show/Add/Quit.
struct TrayState {
    _nav: Option<TrayNav>,
}
struct TrayNav {
    _tray: tray_icon::TrayIcon,
    _menu: tray_icon::menu::Menu,
    show_item: tray_icon::menu::MenuItem,
    show_id: tray_icon::menu::MenuId,
    add_id: tray_icon::menu::MenuId,
    quit_id: tray_icon::menu::MenuId,
}

impl DownloadApp {
    pub fn new(cc: &eframe::CreationContext<'_>, rt: Arc<Mutex<Runtime>>, hidden: bool) -> Self {
        crate::platform::trace("app::new start");
        theme::apply(&cc.egui_ctx);
        let saved_settings: Option<Settings> = rt.lock().ok().map(|g| g.state.settings.clone());
        // Seed before first poll(): restored Complete tasks are history, not news.
        let seen_completed: std::collections::HashSet<String> = rt
            .lock()
            .map(|g| {
                g.state
                    .tasks
                    .iter()
                    .filter(|t| t.status == TaskStatus::Complete)
                    .map(|t| t.id.clone())
                    .collect()
            })
            .unwrap_or_default();
        let seen_failed: std::collections::HashSet<String> = rt
            .lock()
            .map(|g| {
                g.state
                    .tasks
                    .iter()
                    .filter(|t| t.status == TaskStatus::Error)
                    .map(|t| t.id.clone())
                    .collect()
            })
            .unwrap_or_default();
        // Cap workers: default = CPU cores (32 threads parked forever). Downloads are network-bound; 4 is ample.
        let tokio_rt = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(4)
                .max_blocking_threads(16)
                .enable_all()
                .build()
                .expect("tokio rt"),
        );
        crate::platform::trace("app::new rt+tokio built");
        let tray = Self::make_tray(!hidden);
        crate::platform::trace("app::new tray built");
        // Tray actions must not depend on egui poll: hidden viewport stops update(),
        // so poll-drained Quit starved and process needed Task Manager. Handler runs
        // on muda event delivery; Quit saves + exits directly. Toggle/Add write
        // show.request -> platform watcher shows/hides HWND, poll then syncs UI.
        {
            let rt_ev = rt.clone();
            // No raw MenuItem pointer: TrayNav moves into DownloadApp (then heap),
            // captured stack address goes stale -> native AV in muda set_text.
            // Handlers only write show.request + flip WINDOW_VISIBLE; GUI thread
            // refreshes label via valid self.tray in sync_tray_label().
            let ids = tray
                .as_ref()
                .map(|t| (t.show_id.clone(), t.add_id.clone(), t.quit_id.clone()));
            tray_icon::menu::MenuEvent::set_event_handler(Some(
                move |ev: tray_icon::menu::MenuEvent| {
                    let Some((show_id, add_id, quit_id)) = ids.clone() else {
                        return;
                    };
                    if ev.id == quit_id {
                        if let Ok(g) = rt_ev.try_lock() {
                            g.save();
                        }
                        let lock = crate::platform::config_dir().join("instance.lock");
                        if let Ok(txt) = std::fs::read_to_string(&lock) {
                            if txt.trim() == std::process::id().to_string() {
                                let _ = std::fs::remove_file(&lock);
                            }
                        }
                        std::process::exit(0);
                    }
                    if ev.id == show_id {
                        let _ = std::fs::write(
                            crate::platform::config_dir().join("show.request"),
                            "toggle",
                        );
                        // Flip shared flag; GUI updates label via self.tray
                        // (no raw MenuItem pointer: TrayNav moves after capture).
                        crate::platform::WINDOW_VISIBLE
                            .fetch_xor(true, std::sync::atomic::Ordering::Relaxed);
                    } else if ev.id == add_id {
                        let _ = std::fs::write(
                            crate::platform::config_dir().join("show.request"),
                            "add",
                        );
                    }
                },
            ));
            // Left double-click toggles; right click refreshes label just
            // before menu shows (catches close-to-tray/taskbar changes).
            tray_icon::TrayIconEvent::set_event_handler(Some(
                move |ev: tray_icon::TrayIconEvent| {
                    match ev {
                        tray_icon::TrayIconEvent::DoubleClick {
                            button: tray_icon::MouseButton::Left,
                            ..
                        } => {
                            // Flip shared flag; GUI syncs label via self.tray.
                            crate::platform::WINDOW_VISIBLE
                                .fetch_xor(true, std::sync::atomic::Ordering::Relaxed);
                            let _ = std::fs::write(
                                crate::platform::config_dir().join("show.request"),
                                "toggle",
                            );
                        }
                        // Refresh just before menu shows: catches close-to-tray,
                        // taskbar minimize/restore, watcher-driven changes.
                        tray_icon::TrayIconEvent::Click {
                            button: tray_icon::MouseButton::Right,
                            button_state: tray_icon::MouseButtonState::Down,
                            ..
                        } => {
                            // Label refreshed by GUI poll (sync_tray_label).
                        }
                        _ => {}
                    }
                },
            ));
        }
        Self {
            rt,
            tokio_rt,
            tab: Tab::Downloads,
            add_url: String::new(),
            add_referrer: String::new(),
            add_cookies: String::new(),
            add_user: String::new(),
            add_pass: String::new(),
            add_key: String::new(),
            add_headers: Vec::new(),
            add_mirrors: String::new(),
            add_desc: String::new(),
            add_queue: "main".into(),
            add_start_now: true,
            add_just_opened: false,
            add_raise_frames: 0,
            add_focus_done: false,
            add_torrent_slot: std::sync::Arc::new(std::sync::Mutex::new(None)),
            add_torrent_url: String::new(),
            add_torrent_files: Vec::new(),
            add_torrent_loading: false,
            add_torrent_error: String::new(),
            add_nzb_slot: std::sync::Arc::new(std::sync::Mutex::new(None)),
            debrid_add_slot: std::sync::Arc::new(std::sync::Mutex::new(None)),
            pending_debrid_add: None,
            add_nzb_url: String::new(),
            add_nzb: None,
            add_nzb_loading: false,
            add_nzb_error: String::new(),
            new_queue_name: String::new(),
            grab_url: String::new(),
            grab_depth: 2,
            grab_ext_depth: 0,
            grab_scope: 0,
            grab_exts: String::new(),
            grab_parent_dirs: false,
            grab_only_paths: String::new(),
            grab_exclude_paths: String::new(),
            grab_files: Vec::new(),
            grab_cookies: String::new(),
            grab_project_json: String::new(),
            grab_status: "Enter a start URL, then Explore.".into(),
            grab_busy: false,
            grab_auto_explore: false,
            stream_url: String::new(),
            stream_status: String::new(),
            stream_status_err: false,
            last_tick: std::time::Instant::now(),
            tray: Some(TrayState { _nav: tray }),
            hidden,
            ytdlp_checked: false,
            update_check: None,
            update_slot: std::sync::Arc::new(std::sync::Mutex::new(None)),
            debrid_slot: std::sync::Arc::new(std::sync::Mutex::new(None)),
            debrid_hosts_slot: std::sync::Arc::new(std::sync::Mutex::new(None)),
            debrid_ctx: None,
            egui_ctx: None,
            debrid_ui: DebridUiCache::default(),
            show_about: false,
            search: String::new(),
            search_focus: false,
            search_counts: (0, 0),
            grab_sort: (1, false),
            rail_compact: false,
            table_state_synced: (u8::MAX, true, Vec::new(), Vec::new(), Vec::new()),
            minutes_today: today_parts(),
            category: "All Downloads".into(),
            type_filter: None,
            sort_col: saved_settings.as_ref().map(|s| s.dl_sort_col).unwrap_or(0),
            sort_asc: saved_settings
                .as_ref()
                .map(|s| s.dl_sort_asc)
                .unwrap_or(true),
            options_tab: 0,
            options_notice: "Changes save automatically.".into(),
            show_add: false,
            show_progress_for: None,
            player: None,
            player_pending: None,
            player_seek: 0.0,
            player_seek_dragging: false,
            player_speed: 1.0,
            player_hwnd: None,
            player_title_state: (false, 0.0),
            progress_raise: false,
            prog_raise_frames: 0,
            prog_raise_title: String::new(),
            selected: None,
            selected_ids: std::collections::HashSet::new(),
            selection_anchor: None,
            visible_ids: Vec::new(),
            pending_delete: None,
            delete_from_disk: false,
            selected_schedule: None,
            completed_notice: None,
            pending_power_confirm: None,
            seen_completed,
            seen_failed,
            was_active: false,
            speed_hist: Vec::new(),
            last_dl_total: 0,
            rate_map: std::collections::HashMap::new(),
            phase_map: std::collections::HashMap::new(),
            seg_info: std::collections::HashMap::new(),
            rate_smooth: std::collections::HashMap::new(),
            task_hist: std::collections::HashMap::new(),
            handles: std::collections::HashMap::new(),
            allow_close: false,
            dl_widths: saved_settings
                .as_ref()
                .and_then(|s| (!s.dl_col_widths.is_empty()).then(|| s.dl_col_widths.clone())),
            dl_widths_compact: saved_settings.as_ref().and_then(|s| {
                (!s.dl_col_widths_compact.is_empty()).then(|| s.dl_col_widths_compact.clone())
            }),
            grab_widths: saved_settings
                .as_ref()
                .and_then(|s| (!s.grab_col_widths.is_empty()).then(|| s.grab_col_widths.clone())),
            capture_task: None,
            capture_revealed: false,
            capture_name: String::new(),
            capture_just_opened: false,
            capture_raise: false,
            cap_raise_frames: 0,
            cap_focus_done: false,
            first_update_traced: false,
            yt_probe_add: YtProbeState::default(),
            yt_probe_cap: YtProbeState::default(),
            prewarm_add: PrewarmState::default(),
            prewarm_cap: PrewarmState::default(),
            add_vp_sent: None,
            cap_vp_sent: None,
            prog_vp_sent: None,
            prog_task: None,
            cap_body_h: None,
            add_body_h: None,
            prog_body_h: None,
        }
    }

    fn make_tray(visible: bool) -> Option<TrayNav> {
        // Tray icon: same PNG as window/.exe icon (was code-generated solid blue square).
        let data =
            eframe::icon_data::from_png_bytes(include_bytes!("../assets/webcraft.png")).ok()?;
        let icon = tray_icon::Icon::from_rgba(data.rgba, data.width, data.height).ok()?;
        let menu = tray_icon::menu::Menu::new();
        let show = tray_icon::menu::MenuItem::new(tray_toggle_label(visible), true, None);
        let add = tray_icon::menu::MenuItem::new("Add URL...", true, None);
        let quit = tray_icon::menu::MenuItem::new("Quit", true, None);
        let (show_id, add_id, quit_id) = (show.id().clone(), add.id().clone(), quit.id().clone());
        let _ = menu.append_items(&[&show, &add, &quit]);
        // Menu on right click only: left click reserved for double-click toggle.
        let tray = tray_icon::TrayIconBuilder::new()
            .with_icon(icon)
            .with_menu(Box::new(menu.clone()))
            .with_tooltip("webcraft")
            .with_menu_on_left_click(false)
            .build()
            .ok()?;
        Some(TrayNav {
            _tray: tray,
            _menu: menu,
            show_item: show,
            show_id,
            add_id,
            quit_id,
        })
    }

    /// Record window visibility: tray toggle label + shared state watcher syncs.
    fn sync_tray_label(&self) {
        crate::platform::WINDOW_VISIBLE.store(!self.hidden, std::sync::atomic::Ordering::Relaxed);
        if let Some(nav) = self.tray.as_ref().and_then(|t| t._nav.as_ref()) {
            nav.show_item.set_text(tray_toggle_label(!self.hidden));
        }
    }

    fn show_window(&mut self, ctx: &egui::Context) {
        self.hidden = false;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        self.sync_tray_label();
    }

    fn hide_window(&mut self, ctx: &egui::Context) {
        self.hidden = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        self.sync_tray_label();
    }

    fn toggle_window(&mut self, ctx: &egui::Context) {
        if self.hidden {
            self.show_window(ctx);
        } else {
            self.hide_window(ctx);
        }
    }

    /// Debrid Options probe: thread + rt.block_on + slot + repaint (update-check
    /// pattern). kind: account summary or hoster list. Token cloned into
    /// thread, never logged / never in slot tag.
    fn spawn_debrid_probe(
        &mut self,
        p: crate::debrid::Provider,
        account: bool,
        settings: &Settings,
    ) {
        let pid = p.id().to_string();
        if self.debrid_ui.busy.contains(&pid) {
            return;
        }
        let cfg = p.cfg(settings);
        if !cfg.usable() {
            return;
        }
        let key = cfg.api_key.clone();
        self.debrid_ui.busy.insert(pid.clone());
        let slot = self.debrid_slot.clone();
        let hosts_slot = self.debrid_hosts_slot.clone();
        let rt = self.tokio_rt.clone();
        let proxy = {
            let g = crate::lock_unpoison(&self.rt);
            g.state.settings.proxy_url.clone()
        };
        let ctx = self.debrid_ctx.clone();
        std::thread::spawn(move || {
            let client = crate::engine::build_client(&proxy);
            let tag = format!("{}:{}", pid, if account { "account" } else { "hosts" });
            let res = rt.block_on(async move {
                if account {
                    crate::debrid::account(client, p, &key)
                        .await
                        .map(|a| a.summary())
                } else {
                    match crate::debrid::hosts(client, p, &key).await {
                        Ok(list) => {
                            let n = list.len();
                            let up = list.iter().filter(|h| h.supported && h.up).count();
                            if let Ok(mut g) = hosts_slot.lock() {
                                *g = Some((pid.clone(), list));
                            }
                            Ok(format!("{n} hosters, {up} up"))
                        }
                        Err(e) => Err(e),
                    }
                }
            });
            if let Ok(mut g) = slot.lock() {
                *g = Some((tag, res));
            }
            if let Some(ctx) = ctx {
                ctx.request_repaint();
            }
        });
    }

    /// Drain Add-dialog debrid magnet resolve: success = one task per
    /// resolved file (shared magnet URL, debrid_file_name per file; dedup
    /// in add_url bypassed by pushing tasks directly). Failure/empty =
    /// single normal magnet task (stream/local fallback).
    fn poll_debrid_add(&mut self, ctx: &egui::Context) {
        if self.pending_debrid_add.is_none() {
            return;
        }
        let taken = self
            .debrid_add_slot
            .try_lock()
            .ok()
            .and_then(|mut g| g.take());
        let Some(res) = taken else { return };
        let pending = self.pending_debrid_add.take().unwrap();
        let files = match res {
            Ok(f) if !f.is_empty() => f,
            _ => Vec::new(),
        };
        let mut ids: Vec<String> = Vec::new();
        {
            let mut g = crate::lock_unpoison(&self.rt);
            if files.is_empty() {
                let id = g.add_url(
                    &pending.url,
                    &pending.queue_id,
                    &pending.referrer,
                    &pending.cookies,
                );
                ids.push(id);
            } else {
                for f in &files {
                    let fname = crate::model::sanitize_filename(&f.name);
                    let order = g.state.tasks.len() as i64 + 1;
                    let task = DownloadTask {
                        id: crate::model::new_id(),
                        url: pending.url.clone(),
                        dest_dir: crate::model::dest_dir_for(
                            &g.state.settings.download_dir,
                            &fname,
                            g.state.settings.category_subfolders,
                        ),
                        filename: fname,
                        queue_id: if pending.queue_id.is_empty() {
                            "main".into()
                        } else {
                            pending.queue_id.clone()
                        },
                        order,
                        status: TaskStatus::Queued,
                        category: crate::model::category_for(&f.name).into(),
                        debrid_file_name: f.name.clone(),
                        total: f.size,
                        referrer: pending.referrer.clone(),
                        cookies: pending.cookies.clone(),
                        ..Default::default()
                    };
                    ids.push(task.id.clone());
                    g.state.tasks.push(task);
                }
                g.save();
            }
        }
        if pending.start_now {
            for id in &ids {
                self.start_task(id);
            }
        }
        ctx.request_repaint();
    }

    fn start_task(&mut self, id: &str) {
        // Re-download of completed task may notify again when it finishes.
        self.seen_completed.remove(id);
        self.seen_failed.remove(id);
        // Never spawn a second worker for same task: it replaces Controls entry,
        // and Stop would then cancel only newest worker while old one keeps downloading.
        if self.handles.get(id).is_some_and(|h| !h.is_finished()) {
            return;
        }
        // Heal leaked Controls (worker panicked before cleanup ran): no
        // unfinished handle above => any entry stale, drop it and proceed.
        crate::lock_unpoison(&self.rt).controls.remove(id);
        let (task_opt, settings) = {
            let g = crate::lock_unpoison(&self.rt);
            (
                g.state.tasks.iter().find(|t| t.id == id).cloned(),
                g.state.settings.clone(),
            )
        };
        let Some(task) = task_opt else {
            return;
        };
        {
            let mut g = crate::lock_unpoison(&self.rt);
            if let Some(t) = g.state.tasks.iter_mut().find(|t| t.id == id) {
                t.status = TaskStatus::Downloading;
                t.error.clear();
            }
            g.save();
        }
        let ctrl = Arc::new(engine::Controls::new(if task.speed_limit_kbps > 0 {
            task.speed_limit_kbps
        } else {
            settings.speed_limit_kbps
        }));
        crate::lock_unpoison(&self.rt)
            .controls
            .insert(id.to_string(), ctrl.clone());
        let rt = self.rt.clone();
        let tid = id.to_string();
        let client = engine::build_client(&settings.proxy_url);
        let ctrl2 = ctrl.clone();
        let handle = self.tokio_rt.spawn(async move {
            // Inner spawn: downloader panic (e.g. librqbit internals) surfaces as
            // JoinError -> task Error below, not stuck "Downloading" forever.
            let settings2 = settings.clone();
            let result = match tokio::spawn(async move {
                crate::stream::download_url_with_settings(client, task, ctrl, &settings2).await
            })
            .await
            {
                Ok(r) => r,
                Err(e) => Err(format!("download worker panicked: {e}")),
            };
            let mut g = crate::lock_unpoison(&rt);
            // Only this worker may unregister its own Controls. A stopped worker
            // racing a fresh Resume must not remove the new worker's entry.
            if g.controls.get(&tid).is_some_and(|c| Arc::ptr_eq(c, &ctrl2)) {
                g.controls.remove(&tid);
            }
            match result {
                Ok(done) => {
                    let status = done.status;
                    let err = done.error.clone();
                    let fname = done.filename.clone();
                    if let Some(t) = g.state.tasks.iter_mut().find(|t| t.id == tid) {
                        *t = done;
                    }
                    g.log(format!("Finished {fname}: {:?} {err}", status));
                    if status == TaskStatus::Complete {
                        let queue_id = g
                            .state
                            .tasks
                            .iter()
                            .find(|t| t.id == tid)
                            .map(|t| t.queue_id.clone())
                            .unwrap_or_default();
                        // quota accounting (IDM download limits): accumulate bytes into schedule window
                        let bytes = g
                            .state
                            .tasks
                            .iter()
                            .find(|t| t.id == tid)
                            .map(|t| t.downloaded)
                            .unwrap_or(0);
                        let now = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0);
                        let mut quota_hit = false;
                        for s in g
                            .state
                            .schedules
                            .iter_mut()
                            .filter(|s| s.queue_id == queue_id && s.quota_mb > 0)
                        {
                            if s.quota_window_start == 0
                                || now.saturating_sub(s.quota_window_start)
                                    >= s.quota_hours.max(1) * 3600
                            {
                                s.quota_window_start = now;
                                s.quota_used_bytes = 0;
                            }
                            s.quota_used_bytes = s.quota_used_bytes.saturating_add(bytes);
                            if s.quota_used_bytes >= s.quota_mb * 1024 * 1024 {
                                quota_hit = true;
                            }
                        }
                        g.log(format!("Quota queue {queue_id}: hit={quota_hit}"));
                        // completion action only when queue drained (was: first task could exit/shutdown mid-queue)
                        let drained = !g.state.tasks.iter().any(|t| {
                            t.queue_id == queue_id
                                && matches!(t.status, TaskStatus::Queued | TaskStatus::Downloading)
                        });
                        if drained {
                            run_completion(&mut g, &queue_id, &tid);
                        }
                    }
                }
                Err(e) => {
                    if ctrl2.cancelled.load(Ordering::Relaxed) || e == "cancelled" {
                        // User Stop/Remove: stay Paused (parts kept), no retry, no resurrect if row deleted.
                        if let Some(t) = g.state.tasks.iter_mut().find(|t| t.id == tid) {
                            t.status = TaskStatus::Paused;
                            t.downloaded = ctrl2.downloaded.load(Ordering::Relaxed);
                        }
                        g.log(format!("Stopped {tid}"));
                        g.save();
                        return;
                    }
                    // retry per queue policy: attempts persisted on task, next_retry_at gates poll requeue
                    let (retry, interval) = {
                        let t = g.state.tasks.iter().find(|t| t.id == tid);
                        let q = t.and_then(|t| g.state.queues.iter().find(|q| q.id == t.queue_id));
                        (
                            q.map(|q| q.retry_count).unwrap_or(0),
                            q.map(|q| q.retry_interval_secs).unwrap_or(10),
                        )
                    };
                    let now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    let mut left = 0u32;
                    if let Some(t) = g.state.tasks.iter_mut().find(|t| t.id == tid) {
                        t.attempts += 1;
                        t.error = e.clone();
                        if t.attempts <= retry {
                            t.status = TaskStatus::Queued;
                            t.next_retry_at = now + interval;
                            left = retry - t.attempts;
                        } else {
                            t.status = TaskStatus::Error;
                            t.next_retry_at = 0;
                        }
                    }
                    g.log(format!(
                        "Error {tid}: {e} (queue retry left={left} interval={interval}s)"
                    ));
                }
            }
            g.save();
        });
        // Worker finished while poll may idle 5s: wake now so capped
        // deadline (retry gate / queue free slot) applies next frame.
        if let Some(ctx) = self.egui_ctx.clone() {
            ctx.request_repaint();
        }
        self.handles.insert(id.to_string(), handle);
    }

    /// Sample per-task rates from Controls counters (called every tick). Drives
    /// Transfer rate + ETA columns; before this, every row shared the global rate.
    fn sample_rates(&mut self) {
        let now = std::time::Instant::now();
        // Snapshot controls: (id, downloaded, total, parts, manifest, torrent, files)
        type Snap = (
            String,
            u64,
            u64,
            Vec<(u64, u64)>,
            bool,
            Option<crate::engine::TorrentSnap>,
            Vec<(usize, String)>,
        );
        let snap: Vec<Snap> = {
            let g = crate::lock_unpoison(&self.rt);
            g.controls
                .iter()
                .map(|(k, c)| {
                    (
                        k.clone(),
                        c.downloaded.load(Ordering::Relaxed),
                        c.total.load(Ordering::Relaxed),
                        // Poison-tolerant: see torrent publish() — no cascade to GUI.
                        c.parts.lock().unwrap_or_else(|e| e.into_inner()).clone(),
                        c.parts_are_segments.load(Ordering::Relaxed),
                        c.torrent.lock().unwrap_or_else(|e| e.into_inner()).clone(),
                        c.part_files
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .clone(),
                    )
                })
                .collect()
        };
        for (id, n, _total, parts, manifest, torrent_snap, files) in &snap {
            let v = self.rate_map.entry(id.clone()).or_default();
            if v.last()
                .map(|(t, b)| now.duration_since(*t).as_millis() >= 100 && *b != *n)
                .unwrap_or(true)
            {
                v.push((now, *n));
            }
            v.retain(|(t, _)| now.duration_since(*t).as_secs_f64() <= 6.0);
            if !parts.is_empty() {
                self.seg_info.insert(
                    id.clone(),
                    SegView {
                        parts: parts.clone(),
                        files: files.clone(),
                        manifest: *manifest,
                        torrent: torrent_snap.clone(),
                        kind: crate::lock_unpoison(&self.rt)
                            .state
                            .tasks
                            .iter()
                            .find(|t| &t.id == id)
                            .map(crate::stream::proto_kind)
                            .unwrap_or_default(),
                    },
                );
            }
        }
        {
            let g = crate::lock_unpoison(&self.rt);
            self.phase_map = g
                .controls
                .iter()
                .map(|(k, c)| (k.clone(), c.phase.load(Ordering::Relaxed)))
                .collect();
        }
        self.rate_map.retain(|id, _| {
            let g = crate::lock_unpoison(&self.rt);
            g.controls.contains_key(id)
                || g.state.tasks.iter().any(|t| {
                    &t.id == id && matches!(t.status, TaskStatus::Downloading | TaskStatus::Paused)
                })
        });
        self.seg_info.retain(|id, _| {
            let g = crate::lock_unpoison(&self.rt);
            g.controls.contains_key(id)
                || g.state.tasks.iter().any(|t| {
                    &t.id == id && matches!(t.status, TaskStatus::Downloading | TaskStatus::Paused)
                })
        });
        self.rate_smooth.retain(|id, _| {
            let g = crate::lock_unpoison(&self.rt);
            g.state.tasks.iter().any(|t| {
                &t.id == id && matches!(t.status, TaskStatus::Downloading | TaskStatus::Paused)
            })
        });
        // Fallback for tasks without controls yet (probe/queue): part files on disk vs known total.
        let tasks: Vec<(String, std::path::PathBuf, u64)> = {
            let g = crate::lock_unpoison(&self.rt);
            g.state
                .tasks
                .iter()
                .filter(|t| t.status == TaskStatus::Downloading)
                .map(|t| {
                    (
                        t.id.clone(),
                        t.dest_dir.join(format!(".downloadcraft-{}", t.id)),
                        t.total.unwrap_or(0),
                    )
                })
                .collect()
        };
        for (id, dir, total) in tasks {
            if self.seg_info.contains_key(&id) {
                continue;
            }
            if total == 0 {
                continue;
            }
            if let Ok(rd) = std::fs::read_dir(&dir) {
                let mut sizes: Vec<u64> = rd
                    .flatten()
                    .filter_map(|e| {
                        let p = e.path();
                        if p.file_name()
                            .and_then(|f| f.to_str())
                            .map(|f| f.starts_with("part"))
                            .unwrap_or(false)
                        {
                            e.metadata().map(|m| m.len()).ok()
                        } else {
                            None
                        }
                    })
                    .collect();
                if !sizes.is_empty() {
                    sizes.sort_unstable();
                    let n = sizes.len() as u64;
                    // Even split approximation until engine publishes exact ranges.
                    let parts: Vec<(u64, u64)> = sizes
                        .iter()
                        .enumerate()
                        .map(|(i, sz)| {
                            let exp = if i as u64 == n - 1 {
                                total - total / n * (n - 1)
                            } else {
                                total / n
                            };
                            (*sz, exp)
                        })
                        .collect();
                    self.seg_info.insert(
                        id,
                        SegView {
                            parts,
                            files: Vec::new(),
                            manifest: false,
                            kind: crate::stream::ProtoKind::Http,
                            torrent: None,
                        },
                    );
                }
            }
        }
    }

    fn task_rate(&self, id: &str) -> f64 {
        self.rate_smooth.get(id).copied().unwrap_or(0.0)
    }

    /// Once per poll: 6s window rate -> EMA. Done here, not in draw code, so
    /// repaint frequency cannot change smoothing.
    fn update_smooth_rates(&mut self) {
        let ids: Vec<String> = self.rate_map.keys().cloned().collect();
        for id in ids {
            // Anchor to newest sample, not wall clock: egui can stop repainting
            // in tray while worker keeps moving bytes.
            let raw = {
                let Some(v) = self.rate_map.get(&id) else {
                    continue;
                };
                if v.len() < 2 {
                    continue;
                }
                let (t_end, b_end) = v[v.len() - 1];
                let (t0, b0) = v
                    .iter()
                    .copied()
                    .find(|(t, _)| t_end.duration_since(*t).as_secs_f64() <= 5.0)
                    .unwrap_or(v[v.len() - 1]);
                let dt = t_end.duration_since(t0).as_secs_f64();
                if dt < 1.0 {
                    continue;
                } else {
                    b_end.saturating_sub(b0) as f64 / dt
                }
            };
            let e = self.rate_smooth.entry(id).or_insert(raw);
            *e = if raw > 0.0 {
                *e * 0.65 + raw * 0.35
            } else {
                *e * 0.8
            };
            if *e < 1.0 {
                *e = 0.0;
            }
        }
    }

    /// Modal confirm for completion power actions (Schedule.power_confirmed).
    /// request_power (worker thread) parks the action on Runtime.pending_power;
    /// Execute runs it, Cancel drops it. Shown every frame until resolved.
    /// Persist table sort/widths into Settings when changed (Settings is
    /// cloned per frame here, so sync only writes on actual change).
    fn sync_table_state(&mut self) {
        let cur = (
            self.sort_col,
            self.sort_asc,
            self.dl_widths.clone().unwrap_or_default(),
            self.dl_widths_compact.clone().unwrap_or_default(),
            self.grab_widths.clone().unwrap_or_default(),
        );
        if cur == self.table_state_synced {
            return;
        }
        let mut g = crate::lock_unpoison(&self.rt);
        let mut changed = false;
        if g.state.settings.dl_sort_col != cur.0 {
            g.state.settings.dl_sort_col = cur.0;
            changed = true;
        }
        if g.state.settings.dl_sort_asc != cur.1 {
            g.state.settings.dl_sort_asc = cur.1;
            changed = true;
        }
        if g.state.settings.dl_col_widths != cur.2 {
            g.state.settings.dl_col_widths = cur.2.clone();
            changed = true;
        }
        if g.state.settings.dl_col_widths_compact != cur.3 {
            g.state.settings.dl_col_widths_compact = cur.3.clone();
            changed = true;
        }
        if g.state.settings.grab_col_widths != cur.4 {
            g.state.settings.grab_col_widths = cur.4.clone();
            changed = true;
        }
        if changed {
            g.save();
        }
        drop(g);
        self.table_state_synced = cur;
    }

    /// Delete confirmation shared by every delete entry point (toolbar,
    /// menus, context menu, Delete key, delete-completed). Pattern follows
    /// `show_pending_power`: mutate self after Window::show returns.
    fn show_pending_delete(&mut self, ctx: &egui::Context) {
        let Some(ids) = self.pending_delete.clone() else {
            return;
        };
        let n = ids.len();
        let mut confirm = false;
        let mut cancel = false;
        egui::Window::new("Delete downloads")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(format!("Delete {n} download(s) from the list?"));
                ui.checkbox(&mut self.delete_from_disk, "Also delete files from disk");
                // Reserve warning height always: buttons must not jump when
                // checkbox toggles (checked/unchecked pixel-same size).
                // Height mirrors warning label (small text, may wrap at modal
                // width); allocate fixed region, draw only when checked.
                let (warn_rect, _) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), 28.0),
                    egui::Sense::hover(),
                );
                if self.delete_from_disk {
                    ui.allocate_new_ui(egui::UiBuilder::new().max_rect(warn_rect), |ui| {
                        ui.label(
                            egui::RichText::new(
                                "Removes downloaded files, partial data and temp files. Cannot be undone.",
                            )
                            .color(theme::TEXT_MUTED)
                            .small(),
                        );
                    });
                }
                ui.horizontal(|ui| {
                    if theme::primary_btn(ui, "Delete").clicked() {
                        confirm = true;
                    }
                    if theme::ghost_btn(ui, "Cancel").clicked() {
                        cancel = true;
                    }
                });
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    cancel = true;
                }
            });
        if confirm {
            self.pending_delete = None;
            let delete_files = self.delete_from_disk;
            // Snapshot tasks first: remove_task mutates state.
            let tasks: Vec<crate::model::DownloadTask> = {
                let rt = crate::lock_unpoison(&self.rt);
                rt.state
                    .tasks
                    .iter()
                    .filter(|t| ids.contains(&t.id))
                    .cloned()
                    .collect()
            };
            for t in &tasks {
                remove_task(self, &t.id);
            }
            if delete_files {
                for t in &tasks {
                    delete_task_files(t);
                }
            }
            self.delete_from_disk = false;
        }
        if cancel {
            self.pending_delete = None;
            self.delete_from_disk = false;
        }
    }

    fn show_pending_power(&mut self, ctx: &egui::Context) {
        if self.pending_power_confirm.is_none() {
            self.pending_power_confirm = crate::lock_unpoison(&self.rt).pending_power.take();
        }
        let Some(action) = self.pending_power_confirm else {
            return;
        };
        let label = match action {
            crate::platform::PowerAction::Shutdown => "Shut down",
            crate::platform::PowerAction::Sleep => "Sleep",
        };
        let mut execute = false;
        let mut cancel = false;
        egui::Window::new("Confirm power action")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(format!("Downloads complete. {label} this computer now?"));
                ui.horizontal(|ui| {
                    if ui.button(format!("{label} now")).clicked() {
                        execute = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        if execute {
            self.pending_power_confirm = None;
            let _ = crate::platform::execute_power(action);
        } else if cancel {
            self.pending_power_confirm = None;
            crate::lock_unpoison(&self.rt)
                .log(format!("Power action {action:?} cancelled by user"));
        }
    }

    fn poll(&mut self, ctx: &egui::Context) {
        if !self.ytdlp_checked {
            self.ytdlp_checked = true;
            let auto = crate::lock_unpoison(&self.rt)
                .state
                .settings
                .ytdlp_auto_update;
            let rt_b = self.tokio_rt.clone();
            std::thread::spawn(move || {
                rt_b.block_on(async move {
                    // ponytail: silent best-effort; manual Update button surfaces errors.
                    if let Ok(p) = ytdlp::ensure_ytdlp().await {
                        if auto {
                            let stale = std::fs::metadata(&p)
                                .and_then(|m| m.modified())
                                .map(|t| {
                                    t.elapsed()
                                        .map(|e| e.as_secs() > 30 * 86400)
                                        .unwrap_or(false)
                                })
                                .unwrap_or(false);
                            if stale {
                                let _ =
                                    tokio::task::spawn_blocking(move || ytdlp::ytdlp_update(&p))
                                        .await;
                            }
                        }
                    }
                });
            });
        }
        self.handles.retain(|_, h| !h.is_finished());
        // Purge Controls with no live handle (worker panicked before cleanup
        // ran). Live workers keep theirs: start_task inserts Controls then
        // handle synchronously on this thread, so live worker has handle here.
        {
            let handles = &self.handles;
            crate::lock_unpoison(&self.rt)
                .controls
                .retain(|id, _| handles.get(id).is_some_and(|h| !h.is_finished()));
        }
        // Tray menu events (cheap, event-driven): Toggle / Add URL / Quit.
        // Dead while MenuEvent handler is set (handler consumes channel),
        // kept as fallback toggle path.
        let tray_act = self
            .tray
            .as_ref()
            .and_then(|t| t._nav.as_ref())
            .map(|nav| {
                let mut act = "";
                while let Ok(ev) = tray_icon::menu::MenuEvent::receiver().try_recv() {
                    if ev.id == nav.show_id {
                        act = "toggle";
                    } else if ev.id == nav.add_id {
                        act = "add";
                    } else if ev.id == nav.quit_id {
                        act = "quit";
                    }
                }
                act
            })
            .unwrap_or("");
        match tray_act {
            "toggle" => self.toggle_window(ctx),
            "add" => {
                self.show_window(ctx);
                self.tab = Tab::Downloads;
                {
                    self.show_add = true;
                    self.add_just_opened = true; self.add_raise_frames = 90; self.add_focus_done = false;
                }
            }
            "quit" => {
                self.allow_close = true;
                crate::lock_unpoison(&self.rt).save();
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            _ => {}
        }
        // Claim atomically: watcher may claim "toggle" first (instant native
        // hide while egui idles); then no file here and window sync applies.
        if let Some(content) = crate::platform::claim_show_request() {
            match content.trim() {
                "toggle" => self.toggle_window(ctx),
                "add" => {
                    self.show_window(ctx);
                    self.tab = Tab::Downloads;
                    {
                        self.show_add = true;
                        self.add_just_opened = true; self.add_raise_frames = 90; self.add_focus_done = false;
                    }
                }
                _ => self.show_window(ctx),
            }
        }
        // Windows: watcher/native taskbar may show/hide HWND behind egui's
        // back (tray toggle while hidden, taskbar minimize/restore). Follow
        // real visibility so hidden flag + tray label stay correct.
        #[cfg(windows)]
        {
            let visible =
                crate::platform::WINDOW_VISIBLE.load(std::sync::atomic::Ordering::Relaxed);
            if visible == self.hidden {
                self.hidden = !visible;
                self.sync_tray_label();
            }
        }
        // Extension "Grab site": prefill Grabber and auto-explore.
        // JSON carries browser cookies (auth crawls); txt = legacy URL-only.
        {
            let pj = crate::platform::config_dir().join("pending-grab.json");
            let pt = crate::platform::config_dir().join("pending-grab.txt");
            let mut pending: Option<(String, String)> = None;
            // Delete only when accepted below; busy/corrupt = retry next poll.
            if !self.grab_busy {
                if let Ok(s) = std::fs::read_to_string(&pj) {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) {
                        pending = v.get("url").and_then(|x| x.as_str()).map(|u| {
                            (
                                u.to_string(),
                                v.get("cookies")
                                    .and_then(|c| c.as_str())
                                    .unwrap_or("")
                                    .to_string(),
                            )
                        });
                        if pending.is_some() {
                            let _ = std::fs::remove_file(&pj);
                        }
                    }
                }
                if pending.is_none() {
                    if let Ok(s) = std::fs::read_to_string(&pt) {
                        let u = s.trim().to_string();
                        if !u.is_empty() {
                            let _ = std::fs::remove_file(&pt);
                            pending = Some((u, String::new()));
                        }
                    }
                }
            }
            if let Some((u, cookies)) = pending {
                self.grab_url = u;
                self.grab_cookies = cookies;
                self.tab = Tab::Grabber;
                self.grab_auto_explore = true;
                self.show_window(ctx);
            }
        }
        crate::drain_inbox(&self.rt);
        // Browser capture arrived: reveal window so user sees confirm dialog.
        // Downloaded already-running pages fire no task.start guard — captures
        // sit in rt.pending_captures until confirmed, so no auto-start here.
        let has_capture = self.capture_task.is_some()
            || !crate::lock_unpoison(&self.rt).pending_captures.is_empty();
        if has_capture && !self.capture_revealed {
            self.show_window(ctx);
        }
        self.capture_revealed = has_capture;
        if self.grab_busy {
            GRAB_SLOT.with(|s| {
                if let Some(slot) = &*s.borrow() {
                    // Live results while exploring (IDM Grabber behavior).
                    { let o = slot.lock().unwrap(); if !o.done {
                        if let Some(p) = &o.progress {
                            let snap = p.lock().unwrap().clone();
                            if snap.files.len() >= self.grab_files.len() { self.grab_files = snap.files; }
                            if snap.pages_done > 0 || !snap.current_url.is_empty() { self.grab_status = format!("Exploring... depth {} · {} pages done · {} queued · {} files", snap.depth, snap.pages_done, snap.pages_queued, self.grab_files.len()); }
                        }
                    } }
                    let done = { let o = slot.lock().unwrap(); if o.done { Some((o.apply_files, o.files.clone(), o.message.clone())) } else { None } };
                    if let Some((apply, files, message)) = done {
                        // Error outcome carries empty files: keep live partial results.
                        if apply && (!files.is_empty() || self.grab_files.is_empty()) { self.grab_files = files; }
                        self.grab_status = message;
                        self.grab_busy = false;
                    }
                }
            });
        }
        self.sample_rates();
        self.update_smooth_rates();
        // progress sync
        {
            let mut g = crate::lock_unpoison(&self.rt);
            // Re-apply limits each poll: global split tracks active count,
            // per-task edits live-apply without restart. Covers settings
            // slider / schedule stores that write controls directly.
            apply_effective_limits(&g.state, &g.controls);
            let ctrls: Vec<(String, u64)> = g
                .controls
                .iter()
                .map(|(k, c)| {
                    (
                        k.clone(),
                        c.downloaded.load(std::sync::atomic::Ordering::Relaxed),
                    )
                })
                .collect();
            for (id, n) in ctrls {
                if let Some(t) = g.state.tasks.iter_mut().find(|t| t.id == id) {
                    t.downloaded = n;
                }
            }
            let totals: Vec<(String, u64)> = g
                .controls
                .iter()
                .map(|(k, c)| (k.clone(), c.total.load(Ordering::Relaxed)))
                .collect();
            for (id, tot) in totals {
                if tot > 0 {
                    if let Some(t) = g.state.tasks.iter_mut().find(|t| t.id == id) {
                        t.total = Some(tot);
                    }
                }
            }
            // OS notifications: max 1 toast per poll batch, "+N more" summary.
            {
                let notify_on = g.state.settings.notifications;
                let new_done: Vec<(String, String)> = g
                    .state
                    .tasks
                    .iter()
                    .filter(|t| {
                        t.status == TaskStatus::Complete && !self.seen_completed.contains(&t.id)
                    })
                    .map(|t| (t.id.clone(), t.filename.clone()))
                    .collect();
                let new_failed: Vec<String> = g
                    .state
                    .tasks
                    .iter()
                    .filter(|t| t.status == TaskStatus::Error && !self.seen_failed.contains(&t.id))
                    .map(|t| t.filename.clone())
                    .collect();
                // In-app banner (block below) still shows first of batch.
                if self.completed_notice.is_none() {
                    if let Some((id, _)) = new_done.first() {
                        self.completed_notice = Some(id.clone());
                    }
                }
                // Queue drained: was active, now all Complete/Paused, none active.
                // Suppressed when a completion already toasts this batch.
                let drained = notify_on
                    && self.was_active
                    && new_done.is_empty()
                    && !g.state.tasks.is_empty()
                    && g.state
                        .tasks
                        .iter()
                        .all(|t| matches!(t.status, TaskStatus::Complete | TaskStatus::Paused));
                let total = new_done.len() + new_failed.len() + usize::from(drained);
                if notify_on && total > 0 {
                    let (title, first) = if !new_failed.is_empty() {
                        ("Download failed", new_failed[0].clone())
                    } else if !new_done.is_empty() {
                        ("Download complete", new_done[0].1.clone())
                    } else {
                        ("Queue finished", "All downloads finished".to_string())
                    };
                    let body = if total > 1 {
                        format!("{first} (+{} more)", total - 1)
                    } else {
                        first
                    };
                    crate::platform::notify(title, &body);
                    // Toast consumed batch: mark all seen (in-app keeps first).
                    for (id, _) in &new_done {
                        self.seen_completed.insert(id.clone());
                    }
                }
                // Failed always marked, even muted: no replay on toggle-on.
                for t in &g.state.tasks {
                    if t.status == TaskStatus::Error {
                        self.seen_failed.insert(t.id.clone());
                    }
                }
                self.was_active = g
                    .state
                    .tasks
                    .iter()
                    .any(|t| t.status == TaskStatus::Downloading);
            }
            if self.completed_notice.is_none() {
                // Notify once per task id, only for tasks not already
                // Complete at startup (seeded in new()).
                let done = g
                    .state
                    .tasks
                    .iter()
                    .find(|t| {
                        t.status == TaskStatus::Complete && !self.seen_completed.contains(&t.id)
                    })
                    .map(|t| t.id.clone());
                if let Some(id) = done {
                    self.seen_completed.insert(id.clone());
                    self.completed_notice = Some(id);
                }
            }
            // clipboard monitor
            if g.state.settings.clipboard_monitor {
                if let Ok(mut cb) = arboard::Clipboard::new() {
                    if let Ok(txt) = cb.get_text() {
                        let txt = txt.trim().to_string();
                        if (txt.starts_with("http://")
                            || txt.starts_with("https://")
                            || txt.to_ascii_lowercase().starts_with("magnet:?"))
                            && txt != g.clip_last
                            && !g.state.tasks.iter().any(|t| t.url == txt)
                        {
                            g.clip_last = txt.clone();
                            drop(cb);
                            let id = g.add_url(&txt, "main", "", "");
                            g.log(format!("Clipboard URL added: {id}"));
                        } else {
                            g.clip_last = txt;
                        }
                    }
                }
            }
        }
        // queue auto-start (respect retry gate next_retry_at)
        {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let ready: Vec<String> = {
                let g = crate::lock_unpoison(&self.rt);
                if !g.state.queues.iter().any(|q| q.auto_start) {
                    Vec::new()
                } else {
                    let active: Vec<(String, String)> = g
                        .controls
                        .keys()
                        .map(|id| {
                            (
                                id.clone(),
                                g.state
                                    .tasks
                                    .iter()
                                    .find(|t| &t.id == id)
                                    .map(|t| t.queue_id.clone())
                                    .unwrap_or_default(),
                            )
                        })
                        .collect();
                    plan_starts(
                        &g.state.tasks,
                        &g.state.queues,
                        &active,
                        g.state.settings.global_max_concurrent,
                    )
                    .into_iter()
                    .filter(|id| {
                        g.state
                            .tasks
                            .iter()
                            .find(|t| &t.id == id)
                            .map(|t| {
                                t.next_retry_at <= now
                                    && g.state
                                        .queues
                                        .iter()
                                        .find(|q| q.id == t.queue_id)
                                        .map(|q| q.auto_start)
                                        .unwrap_or(false)
                                    && !quota_exhausted(&g, &t.queue_id, now)
                            })
                            .unwrap_or(false)
                    })
                    .collect()
                }
            };
            for id in ready {
                self.start_task(&id);
            }
        }
        // scheduler tick (every poll, cheap)
        {
            let (day, secs) = today_parts();
            self.minutes_today = (day, secs);
            let master = crate::lock_unpoison(&self.rt)
                .state
                .settings
                .scheduler_enabled;
            let mut to_start: Vec<String> = Vec::new();
            let mut to_stop: Vec<String> = Vec::new();
            let mut latched_stop = false;
            {
                let mut g = crate::lock_unpoison(&self.rt);
                for s in if master {
                    g.state.schedules.clone()
                } else {
                    Vec::new()
                } {
                    // should_start covers on-time+grace; missed_run fires late
                    // catch-up only when Schedule.run_missed set (once/day via
                    // last_started_day latch below).
                    if should_start(&s, day, secs)
                        || missed_run((day, secs), &s, s.last_started_day)
                    {
                        // mark started
                        if let Some(sch) = g.state.schedules.iter_mut().find(|x| x.id == s.id) {
                            sch.last_started_day = day;
                        }
                        to_start.push(s.queue_id.clone());
                        g.log(format!("Schedule start queue {}", s.queue_id));
                    }
                    // Latch stop once per day even with no Downloading task: app
                    // launched past stop time latches silently, manual Resume wins.
                    if should_stop(&s, day, secs) {
                        if let Some(sch) = g.state.schedules.iter_mut().find(|x| x.id == s.id) {
                            sch.last_stopped_day = day;
                        }
                        latched_stop = true;
                        if g.state.tasks.iter().any(|t| {
                            t.queue_id == s.queue_id && t.status == TaskStatus::Downloading
                        }) {
                            to_stop.push(s.queue_id.clone());
                        }
                    }
                }
                if !to_start.is_empty() || !to_stop.is_empty() || latched_stop {
                    g.save();
                }
            }
            for qid in to_start {
                // Scheduler trigger: copy sch.on_complete onto queue (run_completion
                // reads queue, sch.on_complete was never read) and apply speed-window
                // limit via global mechanism (settings.speed_limit_kbps + controls
                // atomic, same as Settings apply at save).
                {
                    let mut g = crate::lock_unpoison(&self.rt);
                    let sch = g
                        .state
                        .schedules
                        .iter()
                        .find(|s| s.queue_id == qid)
                        .cloned();
                    if let Some(sch) = sch {
                        if let Some(q) = g.state.queues.iter_mut().find(|q| q.id == qid) {
                            q.on_complete = sch.on_complete;
                        }
                        if sch.limit_kbps > 0 {
                            g.state.settings.speed_limit_kbps = sch.limit_kbps;
                            for c in g.controls.values() {
                                c.limit_kbps
                                    .store(sch.limit_kbps, std::sync::atomic::Ordering::Relaxed);
                            }
                        }
                        g.save();
                    }
                }
                // route through planner so global/per-queue caps hold (was: direct start, caps bypassed)
                let ids: Vec<String> = {
                    let g = crate::lock_unpoison(&self.rt);
                    let active: Vec<(String, String)> = g
                        .controls
                        .keys()
                        .map(|id| {
                            (
                                id.clone(),
                                g.state
                                    .tasks
                                    .iter()
                                    .find(|t| &t.id == id)
                                    .map(|t| t.queue_id.clone())
                                    .unwrap_or_default(),
                            )
                        })
                        .collect();
                    plan_starts(
                        &g.state.tasks,
                        &g.state.queues,
                        &active,
                        g.state.settings.global_max_concurrent,
                    )
                    .into_iter()
                    .filter(|id| {
                        g.state
                            .tasks
                            .iter()
                            .find(|t| &t.id == id)
                            .map(|t| t.queue_id == qid)
                            .unwrap_or(false)
                    })
                    .collect()
                };
                let limit = crate::lock_unpoison(&self.rt)
                    .state
                    .schedules
                    .iter()
                    .find(|s| s.queue_id == qid)
                    .map(|s| s.files_at_once)
                    .unwrap_or(ids.len());
                for id in ids
                    .into_iter()
                    .take(if limit == 0 { usize::MAX } else { limit })
                {
                    self.start_task(&id);
                }
            }
            for qid in to_stop {
                let ids: Vec<String> = {
                    let g = crate::lock_unpoison(&self.rt);
                    g.state
                        .tasks
                        .iter()
                        .filter(|t| t.queue_id == qid && t.status == TaskStatus::Downloading)
                        .map(|t| t.id.clone())
                        .collect()
                };
                for id in ids {
                    stop_task(self, &id);
                }
            }
        }
        // Drag-and-drop: URL text / .url files dropped on window become downloads.
        for file in ctx.input(|i| i.raw.dropped_files.clone()) {
            let local = file.path.as_ref().filter(|p| {
                p.extension().and_then(|e| e.to_str()).is_some_and(|e| {
                    e.eq_ignore_ascii_case("torrent") || e.eq_ignore_ascii_case("nzb")
                })
            });
            if let Some(path) = local {
                let path_str = path.to_string_lossy().into_owned();
                let mut g = crate::lock_unpoison(&self.rt);
                g.add_url(&path_str, "main", "", "");
                g.log(format!("Dropped file added: {path_str}"));
                continue;
            }
            let text = file
                .path
                .as_ref()
                .and_then(|p| std::fs::read_to_string(p).ok())
                .map(|c| {
                    c.lines()
                        .find_map(|l| l.strip_prefix("URL=").map(|u| u.trim().to_string()))
                        .unwrap_or_default()
                })
                .filter(|u| u.starts_with("http") || u.to_ascii_lowercase().starts_with("magnet:?"))
                .or_else(|| {
                    file.name
                        .split_whitespace()
                        .find(|w| {
                            w.starts_with("http") || w.to_ascii_lowercase().starts_with("magnet:?")
                        })
                        .map(|w| w.to_string())
                });
            if let Some(url) = text {
                let mut g = crate::lock_unpoison(&self.rt);
                g.add_url(&url, "main", "", "");
                g.log(format!("Dropped URL added: {url}"));
            }
        }
        // Idle cost: fast wake only while work runs; egui still repaints on input.
        let busy = self.grab_busy || {
            let g = crate::lock_unpoison(&self.rt);
            g.state
                .tasks
                .iter()
                .any(|t| t.status == TaskStatus::Downloading)
        };
        let minimized = ctx.input(|i| i.viewport().minimized == Some(true));
        // Dialog viewports run inside root pass: keep frames live while open
        // so child clicks/buttons don't wait 500ms-5s cadence.
        let dialog_open =
            self.show_add || self.show_progress_for.is_some() || self.capture_task.is_some();
        let base_ms = if self.hidden || minimized {
            5_000
        } else if dialog_open {
            50
        } else if busy {
            500
        } else if crate::lock_unpoison(&self.rt)
            .state
            .settings
            .clipboard_monitor
        {
            1_000
        } else {
            5_000
        };
        // Cap at nearest retry/scheduler deadline; startable-now = 0 (next
        // frame). Idle battery kept: deadline only shortens, never 60fps.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let ms = {
            let g = crate::lock_unpoison(&self.rt);
            let (day, secs_of_day) = today_parts();
            capped_wake_ms(
                base_ms,
                now,
                day,
                secs_of_day,
                &g.state.tasks,
                &g.state.queues,
                &g.state.schedules,
                g.state.settings.scheduler_enabled,
            )
        };
        if ms == 0 {
            ctx.request_repaint();
        } else {
            ctx.request_repaint_after(std::time::Duration::from_millis(ms));
        }
    }
}

/// Milliseconds until nearest armed deadline, capped at idle `base_ms`.
/// `now` = unix secs, `day`/`secs_of_day` = scheduler clock (`today_parts`).
/// `next_retry_at == 0` = ungated, `> now` = gated. Pure: unit-tested.
fn capped_wake_ms(
    base_ms: u64,
    now: u64,
    day: i64,
    secs_of_day: u32,
    tasks: &[DownloadTask],
    queues: &[Queue],
    schedules: &[Schedule],
    scheduler_enabled: bool,
) -> u64 {
    let mut wake = base_ms;
    let mut cap = |secs: i64| {
        if secs > 0 {
            wake = wake.min(((secs as u64) * 1000).max(1));
        } else {
            wake = 0; // due / overdue: poll next frame
        }
    };
    for t in tasks.iter().filter(|t| t.status == TaskStatus::Queued) {
        if !queues
            .iter()
            .find(|q| q.id == t.queue_id)
            .map(|q| q.auto_start)
            .unwrap_or(false)
        {
            continue;
        }
        cap(t.next_retry_at as i64 - now as i64);
    }
    if scheduler_enabled {
        for s in schedules {
            if let Some(d) = next_run_in(s, day, secs_of_day) {
                cap(d);
            }
        }
    }
    wake
}

fn today_parts() -> (i64, u32) {
    // Local time approx via system time UTC (scheduler tests use injected clock; UI display uses UTC offset not critical)
    // ponytail: use UTC day/secs; local tz conversion needs chrono/tz dep, skipped.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    ((now / 86400), (now % 86400) as u32)
}

/// Split on whitespace, keeping `"..."` as one token (quotes stripped).
fn split_cmdline(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut has = false;
    for c in s.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                has = true;
            }
            c if c.is_whitespace() && !quoted => {
                if has || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            c => cur.push(c),
        }
    }
    if has || !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// True when any enabled schedule targets `queue_id` with power action `a`
/// and asked for confirmation. Completion action lives on sched (copied onto
/// queue at trigger) and on queue itself; queue-level completions have no
/// confirm flag (no owning schedule) and execute immediately.
pub fn any_power_confirm_required(
    state: &crate::model::AppState,
    queue_id: &str,
    a: CompletionAction,
) -> bool {
    state
        .schedules
        .iter()
        .any(|s| s.enabled && s.queue_id == queue_id && s.on_complete == a && s.power_confirmed)
}

/// Execute power action, or defer to GUI confirm dialog when
/// `power_confirmed` applies. `run_completion` runs on the worker thread
/// (engines), so egui dialog cannot be shown here: park the request on
/// `Runtime.pending_power`, which the frame loop renders as a modal.
fn request_power(g: &mut Runtime, queue_id: &str, action: platform::PowerAction) {
    let ca = match action {
        platform::PowerAction::Shutdown => CompletionAction::Shutdown,
        platform::PowerAction::Sleep => CompletionAction::Sleep,
    };
    if any_power_confirm_required(&g.state, queue_id, ca) {
        g.pending_power = Some(action);
        g.log(format!("Power action {action:?} awaiting confirmation"));
    } else {
        let _ = platform::execute_power(action);
    }
}

fn run_completion(g: &mut Runtime, queue_id: &str, task_id: &str) {
    let Some(q) = g.state.queues.iter().find(|q| q.id == queue_id) else {
        return;
    };
    let task = g.state.tasks.iter().find(|t| t.id == task_id);
    match q.on_complete {
        CompletionAction::OpenFile => {
            if let Some(t) = task {
                let _ = platform::open_path(&t.dest_dir.join(&t.filename));
            }
        }
        CompletionAction::RunCommand => {
            if !q.on_complete_arg.is_empty() {
                // {file} = completed file full path, quoted if contains spaces.
                // split_cmdline keeps quoted segment as one arg (quotes stripped,
                // Command passes it as single argv entry).
                let arg = if let Some(t) = task {
                    let ps = t.dest_dir.join(&t.filename).to_string_lossy().into_owned();
                    let quoted = if ps.contains(' ') {
                        format!("\"{ps}\"")
                    } else {
                        ps
                    };
                    q.on_complete_arg.replace("{file}", &quoted)
                } else {
                    q.on_complete_arg.clone()
                };
                let parts = split_cmdline(&arg);
                if !parts.is_empty() {
                    let _ = std::process::Command::new(&parts[0])
                        .args(&parts[1..])
                        .spawn();
                }
            }
        }
        CompletionAction::ExitApp => {
            std::process::exit(0);
        }
        CompletionAction::Shutdown => {
            request_power(g, queue_id, platform::PowerAction::Shutdown);
        }
        CompletionAction::Sleep => {
            request_power(g, queue_id, platform::PowerAction::Sleep);
        }
        CompletionAction::None => {}
    }
}

impl DownloadApp {
    fn grab_project(&self, mirror: bool) -> GrabberProject {
        let split = |s: &str| {
            s.split(';')
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
                .collect()
        };
        GrabberProject {
            start_url: self.grab_url.clone(),
            depth: self.grab_depth,
            external_depth: self.grab_ext_depth,
            scope: self.grab_scope,
            extensions: self
                .grab_exts
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            max_pages: 200,
            mirror,
            dont_explore_parent_dirs: self.grab_parent_dirs,
            only_paths: split(&self.grab_only_paths),
            exclude_paths: split(&self.grab_exclude_paths),
            cookies: self.grab_cookies.clone(),
            ..Default::default()
        }
    }
}

impl DownloadApp {
    /// Embedded mpv player viewport. mpv child renders into viewport HWND
    /// (--wid); egui draws only controls bar under it. See spawn order below.
    fn ui_player(&mut self, ctx: &egui::Context) {
        // Title from last snapshot; lags one frame. ponytail: acceptable.
        let (t_paused, _) = self.player_title_state;
        let title = if self.player.is_some() && t_paused {
            "webcraft player (paused)".to_string()
        } else {
            "webcraft player".to_string()
        };
        let builder = egui::ViewportBuilder::default()
            .with_title(title)
            .with_inner_size([880.0, 560.0])
            .with_min_inner_size([480.0, 320.0]);
        let mut close = false;
        let mut stop_req = false;
        let mut toggle_req = false;
        // (target_secs, is_absolute) — slider: absolute; -/+10s: relative.
        let mut seek_req: Option<(f64, bool)> = None;
        let mut speed_req: Option<f64> = None;
        let mut vol_req: Option<f64> = None;
        ctx.show_viewport_immediate(vp_id("webcraft-player"), builder, |ctx, _class| {
            if ctx.input(|i| i.viewport().close_requested()) {
                close = true;
            }
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
            // FIRST: --wid INTO HWND. No HWND yet -> do not spawn; next frame
            // retries (show_viewport_immediate creates window first frame).
            if self.player.is_none() {
                if let Some(url) = self.player_pending.clone() {
                    let hwnd = self.player_hwnd.or_else(|| {
                        let h = find_player_hwnd();
                        self.player_hwnd = h;
                        h
                    });
                    match hwnd {
                        Some(h) => {
                            let settings = {
                                let g = crate::lock_unpoison(&self.rt);
                                g.state.settings.clone()
                            };
                            match downloadcraft_lib::mpv::embedded::spawn_embedded(
                                &settings, &url, h,
                            ) {
                                Ok(handle) => {
                                    self.stream_status = format!("Playing in player: {url}");
                                    self.stream_status_err = false;
                                    self.player = Some(handle);
                                    self.player_pending = None;
                                }
                                Err(e) => {
                                    self.stream_status = format!("ERR {e}");
                                    self.stream_status_err = true;
                                    self.player_pending = None;
                                    close = true;
                                }
                            }
                        }
                        None => { /* HWND not enumerated yet; keep pending */ }
                    }
                }
            }
            let snap = self.player.as_ref().map(|h| {
                let s = crate::lock_unpoison(&h.state);
                (
                    s.pause,
                    s.time_pos,
                    s.duration,
                    s.volume,
                    s.speed,
                    s.title.clone(),
                    s.error.clone(),
                    s.eof,
                )
            });
            if let Some((p, _, d, _, _, _, _, _)) = &snap {
                self.player_title_state = (*p, *d);
            }
            let paused = snap.as_ref().map(|s| s.0).unwrap_or(false);
            egui::TopBottomPanel::bottom("player-controls")
                .exact_height(92.0)
                .frame(
                    egui::Frame::none()
                        .fill(theme::SURFACE)
                        .inner_margin(egui::Margin::same(theme::SP_SM)),
                )
                .show(ctx, |ui| {
                    if let Some((_, tp, dur, vol, _spd, media, err, eof)) = &snap {
                        if !media.is_empty() {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(media.as_str())
                                        .color(theme::TEXT)
                                        .small(),
                                )
                                .truncate(),
                            );
                        }
                        if !err.is_empty() {
                            ui.label(
                                egui::RichText::new(format!("ERR {err}"))
                                    .color(theme::ERR)
                                    .small(),
                            );
                        } else if *eof {
                            ui.label(
                                egui::RichText::new("Ended")
                                    .color(theme::TEXT_FAINT)
                                    .small(),
                            );
                        }
                        if !self.player_seek_dragging {
                            self.player_seek = *tp;
                        }
                        let seek_max = dur.max(0.1);
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(format!(
                                    "{} / {}",
                                    fmt_hms(*tp),
                                    fmt_hms(*dur)
                                ))
                                .color(theme::TEXT_MUTED)
                                .small()
                                .monospace(),
                            );
                            let resp = ui.add(
                                egui::Slider::new(&mut self.player_seek, 0.0..=seek_max)
                                    .show_value(false),
                            );
                            if resp.drag_started() {
                                self.player_seek_dragging = true;
                            }
                            if resp.drag_stopped() {
                                self.player_seek_dragging = false;
                                seek_req = Some((self.player_seek, true));
                            }
                            if icon_btn(ui, Icon::Video, "Full", false, 52.0).clicked() {
                                ctx.send_viewport_cmd_to(
                                    vp_id("webcraft-player"),
                                    egui::ViewportCommand::Fullscreen(true),
                                );
                            }
                        });
                        ui.horizontal(|ui| {
                            let (ic, lbl) = if paused {
                                (Icon::Play, "Play")
                            } else {
                                (Icon::Pause, "Pause")
                            };
                            if icon_btn(ui, ic, lbl, true, 76.0).clicked() {
                                toggle_req = true;
                            }
                            // No Stop icon in set; Delete glyph = stop track.
                            if icon_btn(ui, Icon::Delete, "Stop", false, 64.0).clicked() {
                                stop_req = true;
                            }
                            if theme::ghost_btn(ui, "-10s").clicked() {
                                seek_req = Some((-10.0, false));
                            }
                            if theme::ghost_btn(ui, "+10s").clicked() {
                                seek_req = Some((10.0, false));
                            }
                            ui.label(egui::RichText::new("Vol").color(theme::TEXT_FAINT).small());
                            let mut v = *vol;
                            if ui
                                .add(egui::Slider::new(&mut v, 0.0..=100.0).show_value(false))
                                .changed()
                            {
                                vol_req = Some(v);
                            }
                            ui.label(
                                egui::RichText::new(format!("{vol:.0}"))
                                    .color(theme::TEXT_MUTED)
                                    .small()
                                    .monospace(),
                            );
                            ui.label(
                                egui::RichText::new("Speed")
                                    .color(theme::TEXT_FAINT)
                                    .small(),
                            );
                            let mut sel = self.player_speed;
                            egui::ComboBox::from_id_salt("player-speed")
                                .selected_text(format!("{sel}x"))
                                .show_ui(ui, |ui| {
                                    for s in [0.5, 0.75, 1.0, 1.25, 1.5, 2.0] {
                                        ui.selectable_value(&mut sel, s, format!("{s}x"));
                                    }
                                });
                            if sel != self.player_speed {
                                speed_req = Some(sel);
                            }
                        });
                    } else {
                        ui.label(
                            egui::RichText::new("Starting player\u{2026}")
                                .color(theme::TEXT_MUTED)
                                .small(),
                        );
                    }
                });
            let frame = if self.player.is_some() {
                egui::Frame::none()
            } else {
                egui::Frame::none().fill(egui::Color32::BLACK)
            };
            egui::CentralPanel::default()
                .frame(frame)
                .show(ctx, |_ui| {
                    // ponytail: central left unpainted past Frame bg; mpv owns
                    // pixels under egui GL surface (--wid child paints there).
                    // egui present may flicker on some drivers; controls panel
                    // above is opaque. Do NOT rect_filled here (covers video).
                });
        });
        if close {
            self.quit_player();
            return;
        }
        if let Some(h) = &self.player {
            if toggle_req {
                h.toggle_pause();
            }
            if stop_req {
                h.stop();
            }
            if let Some((t, absolute)) = seek_req {
                if absolute {
                    h.seek_abs(t);
                } else {
                    h.seek_rel(t);
                }
            }
            if let Some(v) = vol_req {
                h.set_volume(v);
            }
            if let Some(s) = speed_req {
                self.player_speed = s;
                h.set_speed(s);
            }
        }
    }

    fn quit_player(&mut self) {
        self.player_pending = None;
        self.player_hwnd = None;
        self.player_seek_dragging = false;
        if let Some(h) = self.player.take() {
            h.quit();
        }
    }

    /// Play `url` in embedded player: reuse live handle (loadfile) else
    /// queue viewport spawn (HWND known next frame).
    fn play_embedded(&mut self, url: &str) {
        if let Some(h) = &self.player {
            match h.loadfile(url) {
                Ok(()) => {
                    self.stream_status = format!("Playing in player: {url}");
                }
                Err(e) => {
                    self.stream_status = format!("ERR {e}");
                    self.player_pending = Some(url.to_string());
                }
            }
        } else {
            self.player_pending = Some(url.to_string());
            self.stream_status = format!("Opening player: {url}");
        }
    }
}

impl eframe::App for DownloadApp {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.quit_player();
        // Stop workers first so Runtime drop does not wait on blocking pool.
        {
            let g = crate::lock_unpoison(&self.rt);
            for c in g.controls.values() {
                c.cancelled
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                c.paused.store(false, std::sync::atomic::Ordering::Relaxed);
            }
        }
        for (_, h) in self.handles.drain() {
            h.abort();
        }
        crate::lock_unpoison(&self.rt).save();
        // Release single-instance lock only if it still holds our pid.
        let lock = crate::platform::config_dir().join("instance.lock");
        if let Ok(txt) = std::fs::read_to_string(&lock) {
            if txt.trim() == std::process::id().to_string() {
                let _ = std::fs::remove_file(&lock);
            }
        }
        // Backstop: run_native returning leaves tokio Runtime drop to chance.
        std::process::exit(0);
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.egui_ctx = Some(ctx.clone());
        if !self.first_update_traced {
            self.first_update_traced = true;
            crate::platform::trace("app::update first frame");
        }
        self.poll_debrid_add(ctx);
        // Paint full viewport first: panel gaps otherwise show OS black (#09) as a vertical bar.
        ctx.layer_painter(egui::LayerId::background()).rect_filled(
            ctx.screen_rect(),
            0.0,
            theme::BG,
        );
        self.poll(ctx);
        // Close-to-tray: window X hides to tray (setting existed but was never wired).
        if !self.allow_close && ctx.input(|i| i.viewport().close_requested()) {
            let to_tray = {
                let g = crate::lock_unpoison(&self.rt);
                g.state.settings.close_to_tray
            } && self.tray.as_ref().and_then(|t| t._nav.as_ref()).is_some();
            if to_tray {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                self.hide_window(ctx);
                // One-shot trim on hide: pages go to standby (Private unchanged, fault back on use).
                #[cfg(windows)]
                unsafe {
                    #[link(name = "psapi")]
                    extern "system" {
                        fn EmptyWorkingSet(h: isize) -> i32;
                        fn GetCurrentProcess() -> isize;
                    }
                    EmptyWorkingSet(GetCurrentProcess());
                }
                return;
            }
        }
        if self.allow_close && ctx.input(|i| i.viewport().close_requested()) {
            self.quit_player();
        }
        if self.hidden {
            return;
        }
        // Ctrl+F focuses main-window search (switches to Downloads so the
        // filter target is visible). Text fields keep native Ctrl+F.
        if ctx.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::F))
            && !ctx.memory(|m| {
                m.focused()
                    .is_some_and(|id| id != egui::Id::new("main-search"))
            })
        {
            self.tab = Tab::Downloads;
            self.search_focus = true;
        }
        // Global table shortcuts. Guard: text fields keep native keys (Ctrl+A
        // in search/URL edits text, Delete edits text); modal viewports
        // (add/capture/progress) own their keys. Delete routes through
        // confirmation dialog, never direct remove.
        let text_focused = ctx.memory(|m| m.focused().is_some());
        let modal_open = self.show_add
            || self.capture_task.is_some()
            || self.show_about
            || self.pending_power_confirm.is_some()
            || self.show_progress_for.is_some()
            || self.pending_delete.is_some();
        if !text_focused && !modal_open {
            if ctx.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::A)) {
                self.tab = Tab::Downloads;
                self.selected_ids = self.visible_ids.iter().cloned().collect();
                if self.selected.is_none() {
                    self.selected = self.visible_ids.first().cloned();
                    self.selection_anchor = self.selected.clone();
                }
            }
            if ctx.input(|i| i.key_pressed(egui::Key::Delete)) && !self.selected_ids.is_empty() {
                self.pending_delete = Some(self.selected_ids.iter().cloned().collect());
                self.delete_from_disk = false;
            }
            if ctx.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::N)) {
                self.show_add = true;
                self.add_just_opened = true; self.add_raise_frames = 90; self.add_focus_done = false;
            }
            if ctx.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::C)) {
                let urls: Vec<String> = {
                    let rt = crate::lock_unpoison(&self.rt);
                    rt.state
                        .tasks
                        .iter()
                        .filter(|t| self.selected_ids.contains(&t.id))
                        .map(|t| t.url.clone())
                        .collect()
                };
                if !urls.is_empty() {
                    ctx.output_mut(|o| o.copied_text = urls.join("\n"));
                }
            }
            if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                self.selected_ids.clear();
                self.selected = None;
                self.selection_anchor = None;
            }
            if ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
                if let Some(id) = self.selected.clone() {
                    self.show_progress_for = Some(id);
                    self.progress_raise = true;
                }
            }
        }
        self.sync_table_state();

        // ── Top bar: brand + menus (IDM: Tasks/File/Downloads/View/Help) ──
        egui::TopBottomPanel::top("topbar")
            .frame(
                egui::Frame::none()
                    .fill(theme::SURFACE)
                    .stroke(egui::Stroke::NONE)
                    .inner_margin(egui::Margin {
                        left: 12.0,
                        right: 12.0,
                        top: 4.0,
                        bottom: 4.0,
                    }),
            )
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    egui::menu::bar(ui, |ui| {
                        ui.menu_button("Tasks", |ui| {
                            if ui.button("Add new download…").clicked() {
                                {
                                    self.show_add = true;
                                    self.add_just_opened = true; self.add_raise_frames = 90; self.add_focus_done = false;
                                }
                                ui.close_menu();
                            }
                            if ui.button("Add batch from clipboard").clicked() {
                                add_batch_clipboard(self);
                                ui.close_menu();
                            }
                            if ui.button("Play video in mpv…").clicked() {
                                self.tab = Tab::Streams;
                                ui.close_menu();
                            }
                            if ui.button("Run Site Grabber").clicked() {
                                self.tab = Tab::Grabber;
                                ui.close_menu();
                            }
                            ui.separator();
                            if ui.button("Import URLs file...").clicked() {
                                import_urls_file(self);
                                ui.close_menu();
                            }
                            if ui.button("Export queue (JSON)").clicked() {
                                export_json(self);
                                ui.close_menu();
                            }
                            if ui.button("Quit").clicked() {
                                std::process::exit(0);
                            }
                        });
                        ui.menu_button("File", |ui| {
                            let sel = self.selected.clone();
                            let sel_status = sel.as_ref().and_then(|id| {
                                let g = crate::lock_unpoison(&self.rt);
                                g.state.tasks.iter().find(|t| &t.id == id).map(|t| t.status)
                            });
                            if ui
                                .add_enabled(
                                    sel_status.is_some_and(|s| {
                                        !matches!(s, TaskStatus::Downloading | TaskStatus::Complete)
                                    }),
                                    egui::Button::new("Download now"),
                                )
                                .clicked()
                            {
                                if let Some(id) = &sel {
                                    self.start_task(id);
                                }
                                ui.close_menu();
                            }
                            if ui
                                .add_enabled(
                                    sel_status == Some(TaskStatus::Downloading),
                                    egui::Button::new("Stop download"),
                                )
                                .clicked()
                            {
                                if let Some(id) = &sel {
                                    stop_task(self, id);
                                }
                                ui.close_menu();
                            }
                            if ui
                                .add_enabled(sel.is_some(), egui::Button::new("Remove"))
                                .clicked()
                            {
                                if let Some(id) = &sel {
                                    self.pending_delete = Some(vec![id.clone()]);
                                    self.delete_from_disk = false;
                                }
                                ui.close_menu();
                            }
                            if ui
                                .add_enabled(
                                    sel_status == Some(TaskStatus::Complete),
                                    egui::Button::new("Redownload"),
                                )
                                .clicked()
                            {
                                if let Some(id) = &sel {
                                    self.start_task(id);
                                }
                                ui.close_menu();
                            }
                        });
                        ui.menu_button("Downloads", |ui| {
                            if ui.button("Resume all").clicked() {
                                resume_all(self);
                                ui.close_menu();
                            }
                            if ui.button("Stop all").clicked() {
                                stop_all(self);
                                ui.close_menu();
                            }
                            if ui.button("Delete all completed").clicked() {
                                delete_completed(self);
                                ui.close_menu();
                            }
                            ui.separator();
                            if ui.button("Scheduler…").clicked() {
                                self.tab = Tab::Scheduler;
                                ui.close_menu();
                            }
                            if ui.button("Queues…").clicked() {
                                self.tab = Tab::Queues;
                                ui.close_menu();
                            }
                            if ui.button("Speed limiter: toggle").clicked() {
                                toggle_limiter(self);
                                ui.close_menu();
                            }
                            if ui.button("Options…").clicked() {
                                self.tab = Tab::Options;
                                ui.close_menu();
                            }
                        });
                        ui.menu_button("View", |ui| {
                            if ui.button("Downloads").clicked() {
                                self.tab = Tab::Downloads;
                                ui.close_menu();
                            }
                            if ui.button("Queues").clicked() {
                                self.tab = Tab::Queues;
                                ui.close_menu();
                            }
                            if ui.button("Scheduler").clicked() {
                                self.tab = Tab::Scheduler;
                                ui.close_menu();
                            }
                            if ui.button("Streams").clicked() {
                                self.tab = Tab::Streams;
                                ui.close_menu();
                            }
                            if ui.button("Site Grabber").clicked() {
                                self.tab = Tab::Grabber;
                                ui.close_menu();
                            }
                            if ui.button("Options").clicked() {
                                self.tab = Tab::Options;
                                ui.close_menu();
                            }
                        });
                        ui.menu_button("Help", |ui| {
                            if ui
                                .button(format!("About webcraft v{}", env!("CARGO_PKG_VERSION")))
                                .clicked()
                            {
                                self.show_about = true;
                                ui.close_menu();
                            }
                        });
                        ui.add_space(12.0);
                        let active = {
                            let g = crate::lock_unpoison(&self.rt);
                            g.state
                                .tasks
                                .iter()
                                .filter(|t| t.status == TaskStatus::Downloading)
                                .count()
                        };
                        let speed = self.speed_hist.last().copied().unwrap_or(0.0);
                        theme::badge(
                            ui,
                            &format!("{active} active · {}", fmt_rate(speed)),
                            theme::TEXT_MUTED,
                        );
                        // Search MUST live inside menu::bar closure: bar()
                        // claims full row width (set_min_size avail width),
                        // so after it returns parent cursor sits past right
                        // edge — siblings allocated there get zero width /
                        // clipped (search vanished). Inside, available_width
                        // = true remainder; RTL tail anchors to panel right.
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let avail_w = ui.available_width();
                            let busy = !self.search.is_empty();
                            // RTL: [SP_SM][Clear?][TextEdit][count? SP_XS]
                            let reserve = if busy {
                                theme::SP_SM
                                    + 52.0
                                    + theme::SP_SM
                                    + 56.0
                                    + theme::SP_XS
                                    + theme::SP_SM
                            } else {
                                theme::SP_SM
                            };
                            let w = (avail_w - reserve).clamp(96.0, 320.0);
                            if avail_w >= 96.0 {
                                ui.add_space(theme::SP_SM);
                                if busy && theme::ghost_btn(ui, "Clear").clicked() {
                                    self.search.clear();
                                }
                                let resp = ui.add_sized(
                                    [w, theme::BTN_H],
                                    egui::TextEdit::singleline(&mut self.search)
                                        .id(egui::Id::new("main-search"))
                                        .hint_text("Search downloads…")
                                        .desired_width(w),
                                );
                                if self.search_focus {
                                    resp.request_focus();
                                    self.search_focus = false;
                                }
                                if resp.has_focus()
                                    && ui.input(|i| i.key_pressed(egui::Key::Escape))
                                {
                                    self.search.clear();
                                    resp.surrender_focus();
                                }
                                if resp.changed() {
                                    self.tab = Tab::Downloads;
                                }
                                resp.on_hover_text("Search downloads  (Ctrl+F)");
                                if busy {
                                    let (shown, total) = self.search_counts;
                                    ui.label(
                                        egui::RichText::new(format!("{shown} of {total}"))
                                            .color(theme::TEXT_MUTED)
                                            .size(11.0),
                                    );
                                    ui.add_space(theme::SP_XS);
                                }
                            }
                        });
                    });
                });
            });

        // ── Left rail: navigation + categories (IDM category tree, modern rail) ──
        // Responsive: below 980px window width rail collapses to icon-only
        // 56px so the table keeps usable width instead of being squeezed.
        let rail_compact = ctx.screen_rect().width() < 980.0;
        self.rail_compact = rail_compact;
        egui::SidePanel::left("rail")
            .resizable(false)
            .show_separator_line(true)
            .exact_width(if rail_compact { 56.0 } else { 176.0 })
            .frame(
                egui::Frame::none()
                    .fill(theme::SURFACE)
                    .stroke(egui::Stroke::NONE)
                    .inner_margin(egui::Margin {
                        left: if rail_compact { 6.0 } else { 12.0 },
                        right: if rail_compact { 6.0 } else { 12.0 },
                        top: 8.0,
                        bottom: 8.0,
                    }),
            )
            .show(ctx, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing.y = 2.0;
                if !rail_compact {
                    ui.label(
                        egui::RichText::new("WORKSPACE")
                            .color(theme::TEXT_FAINT)
                            .small()
                            .strong(),
                    );
                    ui.add_space(4.0);
                }
                for (tab, label, ic) in [
                    (Tab::Downloads, "Downloads", Icon::Download),
                    (Tab::Queues, "Queues", Icon::Queue),
                    (Tab::Scheduler, "Scheduler", Icon::Clock),
                    (Tab::Grabber, "Site Grabber", Icon::Globe),
                    (Tab::Streams, "Streams", Icon::Play),
                    (Tab::Options, "Options", Icon::Settings),
                ] {
                    if nav_row_c(ui, ic, label, self.tab == tab, None, rail_compact, None).clicked()
                    {
                        self.tab = tab;
                    }
                }
                ui.add_space(8.0);
                ui.separator();
                if !rail_compact {
                    ui.horizontal(|ui| {
                        let (irect, _) =
                            ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover());
                        icon(ui.painter(), &Icon::Folder, irect, theme::TEXT_FAINT);
                        ui.label(
                            egui::RichText::new("CATEGORIES")
                                .color(theme::TEXT_FAINT)
                                .small()
                                .strong(),
                        );
                    });
                    ui.add_space(4.0);
                }
                let cats: &[(&str, Icon)] = &[
                    ("All Downloads", Icon::Download),
                    ("Compressed", Icon::Archive),
                    ("Documents", Icon::Document),
                    ("Programs", Icon::Program),
                    ("Video", Icon::Video),
                    ("Audio", Icon::Music),
                    ("General", Icon::File),
                    ("Unfinished", Icon::Clock),
                    ("Finished", Icon::Check),
                ];
                // Per-category counts, same membership rule as downloads table filter (below).
                let counts: Vec<usize> = {
                    let g = crate::lock_unpoison(&self.rt);
                    cats.iter()
                        .map(|(c, _)| match *c {
                            "All Downloads" => g.state.tasks.len(),
                            "Unfinished" => g
                                .state
                                .tasks
                                .iter()
                                .filter(|t| t.status != TaskStatus::Complete)
                                .count(),
                            "Finished" => g
                                .state
                                .tasks
                                .iter()
                                .filter(|t| t.status == TaskStatus::Complete)
                                .count(),
                            cat => g
                                .state
                                .tasks
                                .iter()
                                .filter(|t| {
                                    t.category == cat
                                        || crate::model::category_for(&t.filename) == cat
                                })
                                .count(),
                        })
                        .collect()
                };
                // Badge colors from settings clone; None = plain count text.
                let nav_settings = {
                    let g = crate::lock_unpoison(&self.rt);
                    g.state.settings.clone()
                };
                let badged = nav_settings.nav_count_badges;
                for ((c, ic), n) in cats.iter().zip(counts.iter()) {
                    let badge =
                        badged.then(|| theme::category_color_or(c, &nav_settings.category_colors));
                    if nav_row_c(
                        ui,
                        *ic,
                        c,
                        self.category == *c,
                        Some(*n),
                        rail_compact,
                        badge,
                    )
                    .clicked()
                    {
                        self.category = c.to_string();
                        self.tab = Tab::Downloads;
                    }
                }
                ui.add_space(8.0);
                ui.separator();
                if !rail_compact {
                    ui.horizontal(|ui| {
                        let (irect, _) =
                            ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover());
                        icon(ui.painter(), &Icon::Link, irect, theme::TEXT_FAINT);
                        ui.label(
                            egui::RichText::new("TYPE")
                                .color(theme::TEXT_FAINT)
                                .small()
                                .strong(),
                        );
                    });
                    ui.add_space(4.0);
                }
                // Single pass over tasks (proto_kind may stat local files).
                let type_counts: [usize; 7] = {
                    let g = crate::lock_unpoison(&self.rt);
                    let mut tc = [0usize; 7];
                    for t in &g.state.tasks {
                        let k = crate::stream::proto_kind(t);
                        for sel in TypeSel::ALL {
                            if sel.matches(k) {
                                tc[sel.index()] += 1;
                                break;
                            }
                        }
                    }
                    tc
                };
                if nav_row_c(
                    ui,
                    Icon::Download,
                    "All types",
                    self.type_filter.is_none(),
                    None,
                    rail_compact,
                    None,
                )
                .clicked()
                {
                    self.type_filter = None;
                    self.tab = Tab::Downloads;
                }
                for sel in TypeSel::ALL {
                    let badge =
                        badged.then(|| theme::type_color_or(sel.key(), &nav_settings.type_colors));
                    if nav_row_c(
                        ui,
                        sel.icon(),
                        sel.label(),
                        self.type_filter == Some(sel),
                        Some(type_counts[sel.index()]),
                        rail_compact,
                        badge,
                    )
                    .clicked()
                    {
                        self.type_filter = Some(sel);
                        self.tab = Tab::Downloads;
                    }
                }
                ui.add_space(8.0);
                ui.separator();
                if !rail_compact {
                    ui.label(
                        egui::RichText::new("ffmpeg: PATH/beside exe only")
                            .color(theme::TEXT_FAINT)
                            .small(),
                    );
                    ui.label(
                        egui::RichText::new("DRM content refused")
                            .color(theme::TEXT_FAINT)
                            .small(),
                    );
                }
            });

        // ── Bottom status bar ──
        egui::TopBottomPanel::bottom("statusbar")
            .frame(
                egui::Frame::none()
                    .fill(theme::SURFACE)
                    .stroke(egui::Stroke::NONE)
                    .inner_margin(egui::Margin::symmetric(12.0, 3.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    let (total, done, active) = {
                        let g = crate::lock_unpoison(&self.rt);
                        (
                            g.state.tasks.len(),
                            g.state
                                .tasks
                                .iter()
                                .filter(|t| t.status == TaskStatus::Complete)
                                .count(),
                            g.state
                                .tasks
                                .iter()
                                .filter(|t| t.status == TaskStatus::Downloading)
                                .count(),
                        )
                    };
                    ui.label(
                        egui::RichText::new(format!(
                            "{total} downloads · {done} finished · {active} active"
                        ))
                        .color(theme::TEXT_MUTED)
                        .small(),
                    );
                    ui.spacing_mut().item_spacing.x = 12.0;
                    ui.separator();
                    // speed sparkline (last 40 ticks)
                    let (id, rect) = ui.allocate_space(egui::vec2(120.0, 18.0));
                    let painter = ui.painter();
                    let max = self.speed_hist.iter().cloned().fold(1.0f32, f32::max);
                    let n = self.speed_hist.len();
                    for i in 1..n {
                        let x0 = rect.left() + (i - 1) as f32 / 39.0 * rect.width();
                        let x1 = rect.left() + i as f32 / 39.0 * rect.width();
                        let y0 = rect.bottom() - (self.speed_hist[i - 1] / max) * rect.height();
                        let y1 = rect.bottom() - (self.speed_hist[i] / max) * rect.height();
                        painter.line_segment(
                            [egui::pos2(x0, y0), egui::pos2(x1, y1)],
                            egui::Stroke::new(1.5f32, theme::TEAL_500),
                        );
                    }
                    let _ = id;
                    ui.label(
                        egui::RichText::new(fmt_rate(
                            self.speed_hist.last().copied().unwrap_or(0.0),
                        ))
                        .color(theme::TEAL_500)
                        .small(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 12.0;
                        ui.label(
                            egui::RichText::new(format!("webcraft v{}", env!("CARGO_PKG_VERSION")))
                                .color(theme::TEXT_FAINT)
                                .small(),
                        );
                        ui.separator();
                        ui.label(
                            egui::RichText::new("Ready")
                                .color(theme::TEXT_FAINT)
                                .small(),
                        );
                    });
                });
            });

        // Left margin 10: breathing gap between rail separator line and central content.
        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(theme::BG)
                    .inner_margin(egui::Margin {
                        left: 12.0,
                        right: 12.0,
                        top: 8.0,
                        bottom: 8.0,
                    }),
            )
            .show(ctx, |ui| {
                match self.tab {
                    Tab::Downloads => self.ui_downloads(ui),
                    // Grabber manages own scroll: results list fills remaining height.
                    Tab::Grabber => self.ui_grabber(ui),
                    Tab::Streams => self.ui_streams(ui),
                    Tab::Queues | Tab::Scheduler | Tab::Options => {
                        egui::ScrollArea::vertical()
                            .auto_shrink([false, false])
                            .show(ui, |ui| match self.tab {
                                Tab::Queues => self.ui_queues(ui, false),
                                Tab::Scheduler => self.ui_queues(ui, true),
                                Tab::Options => self.ui_options(ui),
                                _ => {}
                            });
                    }
                }
            });

        if self.show_about {
            egui::Window::new("About webcraft").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
                ui.label(format!("webcraft {} — clean-room cross-platform download manager.", env!("CARGO_PKG_VERSION")));
                ui.label("Rust + egui. Segmented downloads, DASH/HLS, queues, scheduler, Site Grabber.");
                ui.label("No proprietary code, binaries, icons, or activation logic.");
                if ui.button("Close").clicked() { self.show_about = false; }
            });
        }
        self.show_pending_delete(ctx);
        self.show_pending_power(ctx);
        if self.show_add {
            self.ui_add_dialog(ctx);
        }
        if let Some(id) = self.show_progress_for.clone() {
            self.ui_progress_dialog(ctx, &id);
        }
        self.ui_capture_dialog(ctx);
        if self.player.is_some() || self.player_pending.is_some() {
            self.ui_player(ctx);
        }
        if let Some(id) = self.completed_notice.clone() {
            // Resolve task by id; filename/path derive from it.
            let task = crate::lock_unpoison(&self.rt)
                .state
                .tasks
                .iter()
                .find(|t| t.id == id)
                .cloned();
            let name = task
                .as_ref()
                .map(|t| t.filename.clone())
                .unwrap_or_default();
            let path = task.as_ref().map(|t| t.dest_dir.join(&t.filename));
            // Offer protocol download only for plain-HTTP/file .nzb/.torrent
            // files (not already NNTP/Torrent downloads -> no recursion).
            let proto: Option<(&'static str, String)> = match (&task, &path) {
                (Some(t), Some(p)) if t.status == TaskStatus::Complete && p.is_file() => {
                    let ext = std::path::Path::new(&t.filename)
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("");
                    let plain = matches!(
                        crate::stream::proto_kind(t),
                        crate::stream::ProtoKind::Http | crate::stream::ProtoKind::File
                    );
                    if plain && ext.eq_ignore_ascii_case("nzb") {
                        Some(("Start NZB download file…", p.to_string_lossy().into_owned()))
                    } else if plain && ext.eq_ignore_ascii_case("torrent") {
                        Some((
                            "Start torrent download file…",
                            p.to_string_lossy().into_owned(),
                        ))
                    } else {
                        None
                    }
                }
                _ => None,
            };
            egui::Window::new("Download complete")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .default_width(380.0)
                .max_width((ctx.screen_rect().width() - 48.0).clamp(260.0, 480.0))
                .show(ctx, |ui| {
                    ui.add(egui::Label::new(format!("{name} finished.")).truncate())
                        .on_hover_text(name.clone());
                    ui.add_space(theme::SP_XS);
                    ui.horizontal_wrapped(|ui| {
                        if let Some((label, p)) = &proto {
                            if theme::primary_btn(ui, label).clicked() {
                                self.add_url = p.clone();
                                self.show_add = true;
                                self.add_just_opened = true; self.add_raise_frames = 90; self.add_focus_done = false;
                                self.completed_notice = None;
                            }
                        }
                        if theme::primary_btn(ui, "Open file").clicked() {
                            if let Some(p) = &path {
                                let _ = crate::platform::open_path(p);
                            }
                            self.completed_notice = None;
                        }
                        if theme::ghost_btn(ui, "Open folder").clicked() {
                            if let Some(p) = &path {
                                let _ = crate::platform::reveal_path(p);
                            }
                            self.completed_notice = None;
                        }
                        if theme::ghost_btn(ui, "Close").clicked() {
                            self.completed_notice = None;
                        }
                    });
                });
        }
        // Dialog per-task history: reuse rate_map samples (already
        // (Instant, downloaded)); convert to (epoch_s, bytes).
        if self.show_progress_for.is_some() {
            let epoch0 = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            let downloading: Vec<String> = {
                let g = crate::lock_unpoison(&self.rt);
                g.state
                    .tasks
                    .iter()
                    .filter(|t| t.status == TaskStatus::Downloading)
                    .map(|t| t.id.clone())
                    .collect()
            };
            for id in &downloading {
                if let Some(hist) = self.rate_map.get(id) {
                    let samples: Vec<(f64, u64)> = hist
                        .iter()
                        .map(|(inst, b)| (epoch0 - inst.elapsed().as_secs_f64(), *b))
                        .collect();
                    let entry = self.task_hist.entry(id.clone()).or_default();
                    *entry = samples.into_iter().collect();
                    while entry.len() > 120 {
                        entry.pop_front();
                    }
                }
            }
            self.task_hist
                .retain(|id, _| downloading.iter().any(|d| d == id));
        }
        // Global sparkline uses same smoothed per-task rates shown in rows.
        // End-of-frame aggregate byte deltas jump with chunk bursts and repaint cadence.
        let ids: Vec<String> = {
            let g = crate::lock_unpoison(&self.rt);
            g.state
                .tasks
                .iter()
                .filter(|t| t.status == TaskStatus::Downloading)
                .map(|t| t.id.clone())
                .collect()
        };
        let rate = ids.iter().map(|id| self.task_rate(id) as f32).sum::<f32>();
        self.last_tick = std::time::Instant::now();
        self.last_dl_total = ids
            .iter()
            .map(|id| {
                self.rate_map
                    .get(id)
                    .and_then(|v| v.last())
                    .map(|(_, b)| *b)
                    .unwrap_or(0)
            })
            .sum();
        if self.speed_hist.len() > 40 {
            self.speed_hist.remove(0);
        }
        self.speed_hist.push(rate);
    }
}

/// IDM download-limits quota: true when schedule for queue exhausted its MB/hours window.
fn quota_exhausted(g: &Runtime, queue_id: &str, now: u64) -> bool {
    g.state
        .schedules
        .iter()
        .filter(|s| s.queue_id == queue_id && s.quota_mb > 0)
        .any(|s| {
            s.quota_window_start != 0
                && now.saturating_sub(s.quota_window_start) < s.quota_hours.max(1) * 3600
                && s.quota_used_bytes >= s.quota_mb * 1024 * 1024
        })
}

/// Rail "TYPE" filter buckets over derived ProtoKind. Orthogonal to extension
/// category: AND semantics in downloads table filter.
#[derive(Clone, Copy, PartialEq)]
enum TypeSel {
    Torrent,
    Nntp,
    Ftp,
    YtDlp,
    Metalink,
    Http,
    File,
}

impl TypeSel {
    const ALL: [TypeSel; 7] = [
        TypeSel::Torrent,
        TypeSel::Nntp,
        TypeSel::Ftp,
        TypeSel::YtDlp,
        TypeSel::Metalink,
        TypeSel::Http,
        TypeSel::File,
    ];
    /// Stable key for Settings type_colors overrides.
    fn key(self) -> &'static str {
        match self {
            TypeSel::Torrent => "Torrent",
            TypeSel::Nntp => "Nntp",
            TypeSel::Ftp => "Ftp",
            TypeSel::YtDlp => "YtDlp",
            TypeSel::Metalink => "Metalink",
            TypeSel::Http => "Http",
            TypeSel::File => "File",
        }
    }
    fn label(self) -> &'static str {
        match self {
            TypeSel::Torrent => "Torrents",
            TypeSel::Nntp => "NNTP / NZB",
            TypeSel::Ftp => "FTP / SFTP",
            TypeSel::YtDlp => "YT-DLP",
            TypeSel::Metalink => "Metalink",
            TypeSel::Http => "HTTP",
            TypeSel::File => "Local files",
        }
    }
    fn icon(self) -> Icon {
        match self {
            TypeSel::Torrent => Icon::Down,
            TypeSel::Nntp => Icon::Queue,
            TypeSel::Ftp => Icon::Globe,
            TypeSel::YtDlp => Icon::Video,
            TypeSel::Metalink => Icon::Link,
            TypeSel::Http => Icon::Download,
            TypeSel::File => Icon::File,
        }
    }
    fn matches(self, k: crate::stream::ProtoKind) -> bool {
        use crate::stream::ProtoKind::*;
        matches!(
            (self, k),
            (TypeSel::Torrent, Torrent)
                | (TypeSel::Nntp, Nntp)
                | (TypeSel::Ftp, Ftp | Ftps | Sftp)
                | (TypeSel::YtDlp, YtDlp)
                | (TypeSel::Metalink, Metalink)
                | (TypeSel::Http, Http)
                | (TypeSel::File, File)
        )
    }
    fn index(self) -> usize {
        TypeSel::ALL.iter().position(|x| *x == self).unwrap_or(0)
    }
}

fn cat_icon(filename: &str) -> Icon {
    let e = filename
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match e.as_str() {
        "mp4" | "mkv" | "avi" | "mov" | "webm" | "ts" | "m3u8" | "mpd" => Icon::Video,
        "mp3" | "flac" | "ogg" | "wav" | "m4a" | "aac" => Icon::Music,
        "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz" => Icon::Archive,
        "exe" | "msi" | "dmg" | "pkg" | "deb" | "rpm" | "appimage" => Icon::Program,
        "pdf" | "doc" | "docx" | "txt" | "epub" | "xls" | "xlsx" | "ppt" | "pptx" => Icon::Document,
        _ => Icon::File,
    }
}

// free helpers used by menus
fn stop_task(app: &mut DownloadApp, id: &str) {
    // Stop must terminate worker, not only set a flag: phases waiting on a socket,
    // probe, or whole-body fetch can ignore flag indefinitely. Abort drops future;
    // parts stay on disk for Resume. Controls entry removed here because aborted
    // worker never runs its cleanup closure.
    if let Some(h) = app.handles.remove(id) {
        h.abort();
    }
    let mut g = crate::lock_unpoison(&app.rt);
    let downloaded = if let Some(c) = g.controls.remove(id) {
        c.cancelled.store(true, Ordering::Relaxed);
        c.paused.store(false, Ordering::Relaxed);
        c.downloaded.load(Ordering::Relaxed)
    } else {
        0
    };
    if let Some(t) = g.state.tasks.iter_mut().find(|t| t.id == id) {
        t.status = TaskStatus::Paused;
        if downloaded > 0 {
            t.downloaded = downloaded;
        }
    }
    app.rate_smooth.remove(id);
    g.save();
}
/// All on-disk paths owned by a task, per protocol. Best-effort delete set:
/// final file, `{filename}.part`, engine/media/HLS work dirs, single-stream
/// and checksum-quarantine temps, HLS mux temps, `{filename}.nzbstate`.
/// Torrent tasks: `dest_dir/filename` is torrent root (folder for multi-file,
/// file for single-file) so removing it covers both; owned-file progress
/// paths (`part_files`) would be more precise but are live-worker state,
/// unavailable after stop — ponytail: root delete, covers selected+shared
/// pieces alike. yt-dlp intermediates (`{stem}.f<id>.*`, `*.part`/`*.ytdl`)
/// match by stem scan of dest_dir.
fn task_disk_paths(task: &crate::model::DownloadTask) -> Vec<std::path::PathBuf> {
    let d = &task.dest_dir;
    let f = &task.filename;
    let mut v: Vec<std::path::PathBuf> = vec![
        d.join(f),
        d.join(format!("{f}.part")),
        d.join(format!("{f}.bad")),
        d.join(format!("{f}.nzbstate")),
        d.join(format!(".partial-{f}")),
        d.join(format!(".assemble-{f}")),
        d.join(format!(".{f}-audio")),
        d.join(format!(".mux-{f}")),
        d.join(format!(".downloadcraft-{}", task.id)),
        d.join(format!(".downloadcraft-media-{}", task.id)),
        d.join(format!(".downloadcraft-hls-{}", task.id)),
    ];
    // yt-dlp intermediates share task stem: {stem}.f<id>.*, {stem}.*.part/.ytdl
    if crate::stream::proto_kind(task) == crate::stream::ProtoKind::YtDlp {
        let stem = std::path::Path::new(f.as_str())
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        if !stem.is_empty() {
            if let Ok(rd) = std::fs::read_dir(d) {
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    let hit = name.strip_prefix(&stem).is_some_and(|rest| {
                        rest.starts_with(".f") || rest.ends_with(".part") || rest.ends_with(".ytdl")
                    });
                    if hit {
                        v.push(e.path());
                    }
                }
            }
        }
    }
    v
}

/// Remove every path from `task_disk_paths`. Running workers aborted first
/// by caller; deletes best-effort (missing/locked paths ignored).
fn delete_task_files(task: &crate::model::DownloadTask) {
    for p in task_disk_paths(task) {
        if p.is_dir() {
            let _ = std::fs::remove_dir_all(&p);
        } else {
            let _ = std::fs::remove_file(&p);
        }
    }
}

fn remove_task(app: &mut DownloadApp, id: &str) {
    if let Some(h) = app.handles.remove(id) {
        h.abort();
    }
    let mut g = crate::lock_unpoison(&app.rt);
    if let Some(c) = g.controls.get(id) {
        c.cancelled.store(true, Ordering::Relaxed);
        c.paused.store(false, Ordering::Relaxed);
    }
    g.state.tasks.retain(|t| t.id != id);
    g.controls.remove(id);
    if app.selected.as_deref() == Some(id) {
        app.selected = None;
    }
    app.selected_ids.remove(id);
    if app.selection_anchor.as_deref() == Some(id) {
        app.selection_anchor = None;
    }
    if app.show_progress_for.as_deref() == Some(id) {
        app.show_progress_for = None;
    }
    g.save();
}
fn resume_all(app: &mut DownloadApp) {
    // Error tasks exhausted retry budget (attempts>retry). start_task alone
    // re-enters worker, but first error re-increments past budget -> no real
    // retry. Reset attempts + retry gate so Retry-all actually retries.
    {
        let mut g = crate::lock_unpoison(&app.rt);
        let mut changed = false;
        for t in g.state.tasks.iter_mut() {
            if t.status == TaskStatus::Error {
                t.attempts = 0;
                t.next_retry_at = 0;
                t.error.clear();
                changed = true;
            }
        }
        if changed {
            g.save();
        }
    }
    let ids: Vec<String> = {
        let g = crate::lock_unpoison(&app.rt);
        g.state
            .tasks
            .iter()
            .filter(|t| {
                matches!(
                    t.status,
                    TaskStatus::Paused | TaskStatus::Error | TaskStatus::Queued
                )
            })
            .map(|t| t.id.clone())
            .collect()
    };
    for id in ids {
        app.start_task(&id);
    }
}
fn stop_all(app: &mut DownloadApp) {
    let ids: Vec<String> = {
        let g = crate::lock_unpoison(&app.rt);
        g.state
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Downloading)
            .map(|t| t.id.clone())
            .collect()
    };
    for id in ids {
        stop_task(app, &id);
    }
}
/// Route "delete completed" through confirmation dialog (all protocols).
fn delete_completed(app: &mut DownloadApp) {
    let ids: Vec<String> = {
        let g = crate::lock_unpoison(&app.rt);
        g.state
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Complete)
            .map(|t| t.id.clone())
            .collect()
    };
    if !ids.is_empty() {
        app.pending_delete = Some(ids);
        app.delete_from_disk = false;
    }
}
fn toggle_limiter(app: &mut DownloadApp) {
    let mut g = crate::lock_unpoison(&app.rt);
    g.state.settings.speed_limit_kbps = if g.state.settings.speed_limit_kbps == 0 {
        512
    } else {
        0
    };
    let lim = g.state.settings.speed_limit_kbps;
    for c in g.controls.values() {
        c.limit_kbps.store(lim, Ordering::Relaxed);
    }
    g.save();
}
fn export_json(app: &mut DownloadApp) {
    let g = crate::lock_unpoison(&app.rt);
    let txt = serde_json::to_string_pretty(
        &g.state
            .tasks
            .iter()
            .map(|t| serde_json::json!({"url":t.url,"filename":t.filename,"queue":t.queue_id}))
            .collect::<Vec<_>>(),
    )
    .unwrap_or_default();
    let path = g.state.settings.download_dir.join("webcraft-export.json");
    let _ = std::fs::write(path, txt);
}
fn import_urls_file(app: &mut DownloadApp) {
    let Some(path) = rfd::FileDialog::new()
        .add_filter("Text", &["txt"])
        .pick_file()
    else {
        return;
    };
    let Ok(txt) = std::fs::read_to_string(&path) else {
        return;
    };
    // Dedupe vs existing tasks + within file. Mirror add_batch_clipboard.
    let existing: std::collections::HashSet<String> = {
        let g = crate::lock_unpoison(&app.rt);
        g.state.tasks.iter().map(|t| t.url.clone()).collect()
    };
    let mut seen = existing;
    let mut added = 0usize;
    for line in txt.lines() {
        let u = line.trim();
        if (u.starts_with("http://") || u.starts_with("https://") || u.contains("://"))
            && seen.insert(u.to_string())
        {
            add_url_task(app, u);
            added += 1;
        }
    }
    crate::lock_unpoison(&app.rt).log(format!("Imported {added} URLs from {}", path.display()));
}

fn add_batch_clipboard(app: &mut DownloadApp) {
    if let Ok(mut cb) = arboard::Clipboard::new() {
        if let Ok(txt) = cb.get_text() {
            for line in txt.lines() {
                let u = line.trim();
                if u.starts_with("http://") || u.starts_with("https://") {
                    add_url_task(app, u);
                }
            }
        }
    }
}
fn add_url_task(app: &mut DownloadApp, url: &str) {
    let id = {
        let mut g = crate::lock_unpoison(&app.rt);
        g.add_url(url, "main", "", "")
    };
    app.start_task(&id);
}
fn fmt_rate(bps: f32) -> String {
    if bps <= 0.0 {
        "0 B/s".into()
    } else if bps < 1024.0 {
        format!("{bps:.0} B/s")
    } else if bps < 1024.0 * 1024.0 {
        format!("{:.1} KB/s", bps / 1024.0)
    } else if bps < 1024.0 * 1024.0 * 1024.0 {
        format!("{:.2} MB/s", bps / 1048576.0)
    } else {
        format!("{:.2} GB/s", bps / (1024.0 * 1024.0 * 1024.0))
    }
}
fn eta(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        "—".into()
    } else if secs < 60.0 {
        format!("{secs:.0}s")
    } else if secs < 3600.0 {
        format!("{:.0}m {:.0}s", secs / 60.0, secs % 60.0)
    } else {
        format!("{:.0}h {:.0}m", secs / 3600.0, (secs % 3600.0) / 60.0)
    }
}

/// IDM-style toolbar button: icon glyph + label in one bordered, clickable cell.
/// (egui Button takes text only; separate icon widgets left icon outside hit area and label looked like plain text.)
fn nav_row_c(
    ui: &mut egui::Ui,
    ic: Icon,
    label: &str,
    selected: bool,
    count: Option<usize>,
    compact: bool,
    badge: Option<egui::Color32>,
) -> egui::Response {
    let w = ui.available_width();
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(w, theme::ROW_H_NAV), egui::Sense::click());
    let painter = ui.painter();
    if selected {
        painter.rect_filled(rect, 4.0, theme::STEEL_DIM);
    } else if resp.hovered() {
        painter.rect_filled(rect, 4.0, theme::SURFACE_HOVER);
    }
    let fg = if selected {
        egui::Color32::WHITE
    } else if resp.hovered() {
        theme::TEXT
    } else {
        theme::TEXT_MUTED
    };
    let icon_rect = egui::Rect::from_min_size(
        if compact {
            egui::pos2(rect.center().x - 8.0, rect.min.y + 2.0)
        } else {
            rect.left_top() + egui::vec2(8.0, 2.0)
        },
        egui::vec2(16.0, 16.0),
    );
    icon(painter, &ic, icon_rect, fg);
    if compact {
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        return resp.on_hover_text(label.to_string());
    }
    let font = egui::FontId::proportional(13.5);
    // Count badge: measured, not guessed; label truncation reserves its width + gap.
    // Badged: smaller font + pill pads (7+7); plain keeps 13.5 selected/faint.
    let (count_font, count_color) = match badge {
        Some(base) => (egui::FontId::proportional(10.5), theme::badge_text(base)),
        None if selected => (font.clone(), egui::Color32::WHITE),
        None => (font.clone(), theme::TEXT_FAINT),
    };
    let count_galley = count
        .filter(|n| *n > 0)
        .map(|n| painter.layout_no_wrap(format!("{n}"), count_font, count_color));
    let reserve = count_galley
        .as_ref()
        .map(|g| g.size().x + (if badge.is_some() { 14.0 } else { 0.0 }) + 10.0)
        .unwrap_or(0.0);
    let avail = (rect.right() - (icon_rect.right() + 6.0) - 6.0 - reserve).max(0.0);
    // single row, ellipsis-truncated to avail width; never overflow rail
    let mut job = egui::text::LayoutJob::single_section(
        label.to_owned(),
        egui::TextFormat {
            font_id: font,
            color: fg,
            ..Default::default()
        },
    );
    job.wrap.max_width = avail;
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job.wrap.overflow_character = Some('…');
    let galley = painter.layout_job(job);
    painter.galley(
        egui::pos2(
            icon_rect.right() + 6.0,
            rect.center().y - galley.size().y / 2.0,
        ),
        galley,
        fg,
    );
    if let Some(g) = count_galley {
        let cw = g.size().x;
        let cy = rect.center().y - g.size().y / 2.0;
        if let Some(base) = badge {
            // Pill: 7/2 pad, right margin 6, radius height/2.
            let pill = egui::Rect::from_min_max(
                egui::pos2(rect.right() - 6.0 - cw - 14.0, cy - 2.0),
                egui::pos2(rect.right() - 6.0, cy + g.size().y + 2.0),
            );
            let (fill, border) = theme::badge_tint(base);
            painter.rect(
                pill,
                egui::Rounding::same(pill.height() / 2.0),
                fill,
                egui::Stroke::new(1.0_f32, border),
            );
        }
        let tx = rect.right() - 6.0 - (if badge.is_some() { 7.0 } else { 0.0 }) - cw;
        painter.galley(egui::pos2(tx, cy), g, count_color);
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}

fn icon_btn(ui: &mut egui::Ui, ic: Icon, label: &str, primary: bool, min_w: f32) -> egui::Response {
    icon_btn_enabled(ui, ic, label, primary, min_w, true)
}

fn icon_btn_enabled(
    ui: &mut egui::Ui,
    ic: Icon,
    label: &str,
    primary: bool,
    min_w: f32,
    enabled: bool,
) -> egui::Response {
    paint_btn(ui, Some(ic), label, primary, min_w, enabled)
}

/// Shared painted button (toolbar + dialogs): same fill/stroke per state everywhere.
fn paint_btn(
    ui: &mut egui::Ui,
    ic: Option<Icon>,
    label: &str,
    primary: bool,
    min_w: f32,
    enabled: bool,
) -> egui::Response {
    let enabled = enabled && ui.is_enabled();
    // Allocate first, paint after state known: previous Frame-then-interact painted
    // one flat box regardless of hover/press (buttons looked dead, clicks unreliable).
    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    // Fixed min_w under-measured icon+padding: label painted past border. Grow to fit text.
    let text_w = ui
        .painter()
        .layout_no_wrap(
            label.to_owned(),
            egui::TextStyle::Body.resolve(ui.style()),
            egui::Color32::WHITE,
        )
        .size()
        .x;
    let w = if ic.is_some() {
        min_w.max(8.0 + 16.0 + 6.0 + text_w + 8.0)
    } else {
        min_w.max(12.0 + text_w + 12.0)
    };
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, theme::BTN_H), sense);
    let (fill, stroke, fg) = if !enabled {
        (theme::SURFACE, theme::BORDER, theme::TEXT_FAINT)
    } else if primary {
        if resp.is_pointer_button_down_on() {
            (theme::STEEL_DIM, theme::STEEL, egui::Color32::WHITE)
        } else if resp.hovered() {
            (theme::STEEL_DIM, theme::STEEL_TEXT, egui::Color32::WHITE)
        } else {
            (theme::STEEL, theme::STEEL, egui::Color32::WHITE)
        }
    } else if resp.is_pointer_button_down_on() {
        (theme::STEEL_DIM, theme::STEEL, theme::TEXT)
    } else if resp.hovered() {
        (theme::SURFACE_HOVER, theme::BORDER_STRONG, theme::TEXT)
    } else {
        (theme::SURFACE_RAISED, theme::BORDER, theme::TEXT)
    };
    ui.painter().rect(
        rect,
        egui::Rounding::same(theme::R_CTRL),
        fill,
        egui::Stroke::new(1.0f32, stroke),
    );
    if enabled && resp.has_focus() {
        ui.painter().rect_stroke(
            rect.expand(2.0),
            egui::Rounding::same(theme::R_CTRL + 2.0),
            egui::Stroke::new(2.0f32, theme::STEEL_TEXT),
        );
    }
    if enabled && resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let Some(ic) = ic else {
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            label,
            egui::TextStyle::Body.resolve(ui.style()),
            fg,
        );
        return resp;
    };
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect.shrink2(egui::vec2(8.0, 0.0)))
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    child.spacing_mut().item_spacing.x = 6.0;
    let (irect, _) = child.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover());
    icon(child.painter(), &ic, irect, fg);
    child.add(
        egui::Label::new(egui::RichText::new(label).color(fg))
            .truncate()
            .selectable(false),
    );
    resp
}

/// Download-table column widths shared by header and rows (separate Grids auto-sized
/// independently, so header and row columns never lined up). Name column takes remainder.
/// Fixed widths follow live IDM ListSettings (File Name|Q 30|Size 75|Status 65|
/// Time left 73|Transfer rate 82); Progress/Actions are webcraft extras.
const COL_GAP: f32 = 10.0; // header/rows use item_spacing.x = 10; leading cell at row_origin_x
const COL_MIN: f32 = 36.0;
const NAME_MIN: f32 = 120.0;

/// Per-column non-Name minima (full mode idx: 1=Q, 2=Size, 3=Status, 4=ETA, 5=Rate,
/// 6=Progress, 7=Actions; compact: 1=Size, 2=Status, 3=Progress, 4=Actions).
/// Blanket COL_MIN let drags shrink columns below own content = clipped text.
fn col_min(idx: usize, compact: bool) -> f32 {
    if idx == 0 {
        return NAME_MIN;
    }
    match (compact, idx) {
        (false, 1) => 44.0,  // Q: "main"
        (false, 3) => 76.0,  // Status: "Downloading"
        (false, 6) => 132.0, // Progress: bar + %
        (false, 7) => 58.0,  // Actions: 2 buttons
        (true, 2) => 76.0,
        (true, 3) => 132.0,
        (true, 4) => 58.0,
        _ => COL_MIN,
    }
}

fn dl_cols(avail: f32, compact: bool, stored: Option<&[f32]>) -> Vec<f32> {
    let fixed: &[f32] = if compact {
        &[75.0, 76.0, 132.0, 58.0]
    } else {
        &[44.0, 75.0, 76.0, 73.0, 82.0, 132.0, 58.0]
    };
    if let Some(s) = stored.filter(|s| s.len() == fixed.len() + 1) {
        // User widths: clamp per-column; Name = remainder only when total fits avail, else keep user width (h-scroll handles overflow).
        let mut v: Vec<f32> = s
            .iter()
            .enumerate()
            .map(|(i, w)| (*w).max(col_min(i, compact)))
            .collect();
        let total: f32 = v.iter().sum::<f32>() + COL_GAP * fixed.len() as f32;
        if total <= avail {
            v[0] =
                (avail - v[1..].iter().sum::<f32>() - COL_GAP * fixed.len() as f32).max(NAME_MIN);
        }
        return v;
    }
    let gaps = COL_GAP * fixed.len() as f32;
    let mut v = vec![(avail - fixed.iter().sum::<f32>() - gaps).max(NAME_MIN)];
    v.extend_from_slice(fixed);
    v
}

/// Cumulative x of each column boundary (relative to first cell left edge).
fn col_bounds(cols: &[f32]) -> Vec<f32> {
    let mut xs = Vec::with_capacity(cols.len().saturating_sub(1));
    let mut x = 0.0;
    for w in &cols[..cols.len().saturating_sub(1)] {
        x += w + COL_GAP;
        xs.push(x - COL_GAP / 2.0);
    }
    xs
}

/// Draggable 8px separators over header row. Drag pairs i/i+1 (delta added
/// to i, subtracted from i+1, clamped per mode). Double-click resets pair
/// to default widths. `compact` selects the column-min table — callers MUST
/// pass the table's actual mode (hardcoded full-mode mins corrupt compact).
fn col_resize_handles(
    ui: &mut egui::Ui,
    header: egui::Rect,
    cols: &mut [f32],
    compact: bool,
) -> bool {
    let mut changed = false;
    for (i, bx) in col_bounds(cols).into_iter().enumerate() {
        let x = header.min.x + bx;
        let rect = egui::Rect::from_min_size(
            egui::pos2(x - 4.0, header.min.y),
            egui::vec2(8.0, header.height()),
        );
        let resp = ui.interact(
            rect,
            ui.id().with(("col_resize", i)),
            egui::Sense::click_and_drag(),
        );
        let active = resp.dragged() || resp.hovered();
        if resp.hovered() || resp.dragged() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        }
        ui.painter().line_segment(
            [
                egui::pos2(x, header.min.y + 3.0),
                egui::pos2(x, header.max.y - 3.0),
            ],
            egui::Stroke::new(
                if active { 2.0f32 } else { 1.0 },
                if active {
                    theme::STEEL
                } else {
                    theme::BORDER_STRONG
                },
            ),
        );
        // Double-click handled before drag: reset must fire even when the
        // press never becomes a drag (dragged() may be false that frame).
        if resp.double_clicked() {
            let def = dl_cols(760.0, compact, None);
            if let (Some(&a), Some(&b)) = (def.get(i), def.get(i + 1)) {
                cols[i] = a;
                cols[i + 1] = b;
                changed = true;
            }
            continue;
        }
        if resp.dragged() {
            let d = resp.drag_delta().x;
            if d != 0.0 {
                let (a, b) = (cols[i], cols[i + 1]);
                // Minima follow the table's actual mode; compact widths differ.
                let na = (a + d).max(col_min(i, compact));
                let nb = (b - (na - a)).max(col_min(i + 1, compact));
                cols[i] = na;
                cols[i + 1] = nb;
                changed = true;
            }
        }
    }
    changed
}

/// Sheet row fill: selected STEEL_DIM only; else zebra. Bottom 1px separator, no cell boxes.
fn sheet_row(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    sel: bool,
    idx: usize,
    last: bool,
    hovered: bool,
) {
    let _ = idx;
    let fill = if sel {
        theme::STEEL_DIM
    } else if hovered {
        theme::SURFACE_HOVER
    } else {
        theme::BG
    };
    ui.painter().rect_filled(rect, 0.0, fill);
    if !last {
        ui.painter().line_segment(
            [rect.left_bottom(), rect.right_bottom()],
            egui::Stroke::new(1.0f32, theme::BORDER),
        );
    }
}

/// Flat table header: SURFACE_RAISED fill, no rounding/outline, bottom border line.
fn sheet_header(ui: &mut egui::Ui, rect: egui::Rect) {
    ui.painter().rect_filled(rect, 0.0, theme::SURFACE_RAISED);
    ui.painter().line_segment(
        [rect.left_bottom(), rect.right_bottom()],
        egui::Stroke::new(1.0f32, theme::BORDER),
    );
}

/// IDM list pitch is 17px (Tahoma 8pt, 16px icon, 1px separator). egui 11px
/// small text needs ~15px line, so 20px cell + 1px borders = 22px pitch.
pub const ROW_H: f32 = 20.0;
/// Compact square row action (Play/Pause/Info glyph only; labels live in tooltips).
fn row_icon_btn(ui: &mut egui::Ui, ic: Icon, tip: &str) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(20.0, 20.0), egui::Sense::click());
    let (fill, stroke, fg) = if resp.is_pointer_button_down_on() {
        (theme::STEEL_DIM, theme::STEEL, theme::TEXT)
    } else if resp.hovered() {
        (theme::SURFACE_HOVER, theme::BORDER_STRONG, theme::TEXT)
    } else {
        (theme::SURFACE_RAISED, theme::BORDER, theme::TEXT_MUTED)
    };
    ui.painter().rect(
        rect,
        egui::Rounding::same(theme::R_CHIP),
        fill,
        egui::Stroke::new(1.0f32, stroke),
    );
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let irect = egui::Rect::from_center_size(rect.center(), egui::vec2(12.0, 12.0));
    icon(ui.painter(), &ic, irect, fg);
    resp.on_hover_text(tip)
}
fn cell<R>(ui: &mut egui::Ui, w: f32, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    cell_align(ui, w, false, add)
}

/// Right-aligned cell (numeric columns).
fn cell_r<R>(ui: &mut egui::Ui, w: f32, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    cell_align(ui, w, true, add)
}

fn cell_align<R>(
    ui: &mut egui::Ui,
    w: f32,
    right: bool,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    // Fixed height: zero-height allocation left every column on its own baseline.
    // 8px left padding (right-aligned: 8px right edge) so text never touches column edge.
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, ROW_H), egui::Sense::hover());
    let inner = if right {
        egui::Rect::from_min_max(rect.min, rect.max - egui::vec2(8.0, 0.0))
    } else {
        egui::Rect::from_min_max(rect.min + egui::vec2(8.0, 0.0), rect.max)
    };
    let layout = if right {
        egui::Layout::right_to_left(egui::Align::Center)
    } else {
        egui::Layout::left_to_right(egui::Align::Center)
    };
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(inner).layout(layout));
    child.set_width(inner.width());
    add(&mut child)
}

/// Form constants: label col 120 right-aligned, [12,6] gap, DragValue 90.
const LABEL_W: f32 = 120.0;
const DRAG_W: f32 = 90.0;

/// Download filename derived from URL (segment after last `/`, decoded).
fn grab_name(url: &str) -> String {
    crate::model::filename_from_url(url)
}

/// Viewport height fitting content as if every collapsible section were
/// expanded: measured body + still-collapsed section heights. Floor = base
/// (dialog minimum usable size), cap = 85% monitor. Width never inferred.
fn fit_content_h(base_h: f32, content_h: f32, monitor_h: f32) -> f32 {
    let cap = (monitor_h * 0.85).max(base_h);
    (base_h.max(content_h)).min(cap)
}

/// 2-col form grid: label col fixed 120 (right-aligned via col layout), field fills.
/// No max_col_width: egui caps the LAST col at max_cell_size, so any finite cap
/// froze fields (1024) once the window grew past it — resize did nothing.
fn form_grid(id: &str) -> egui::Grid {
    egui::Grid::new(id)
        .num_columns(2)
        .spacing([12.0, 6.0])
        .min_col_width(LABEL_W)
}

/// Right-aligned muted form label (use inside `form_grid` col 0).
fn form_label(ui: &mut egui::Ui, text: &str) {
    // Fixed LABEL_W cell: RTL in a non-last Grid col can paint the label
    // off-screen and widen the parent (right-aligned footers vanish).
    ui.allocate_ui_with_layout(
        egui::vec2(LABEL_W, theme::BTN_H),
        egui::Layout::right_to_left(egui::Align::Center),
        |ui| {
            ui.label(egui::RichText::new(text).color(theme::TEXT_MUTED));
        },
    );
}

/// One badge-color row (inside form_grid): swatch edits override for key;
/// untouched rows show theme default, changed rows insert-or-replace.
fn badge_color_row(
    ui: &mut egui::Ui,
    label: &str,
    key: &str,
    default: egui::Color32,
    overrides: &mut Vec<(String, [u8; 3])>,
) {
    form_label(ui, label);
    let mut rgb = overrides
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| *v)
        .unwrap_or([default.r(), default.g(), default.b()]);
    if ui.color_edit_button_srgb(&mut rgb).changed() {
        if let Some(slot) = overrides.iter_mut().find(|(k, _)| k == key) {
            slot.1 = rgb;
        } else {
            overrides.push((key.to_string(), rgb));
        }
    }
    ui.end_row();
}

/// Section header: label + 6px + separator (Options / editor groups).
fn section(ui: &mut egui::Ui, title: &str) {
    ui.label(egui::RichText::new(title).color(theme::TEXT).strong());
    ui.add_space(6.0);
    ui.separator();
}

/// Start silent yt-dlp -J probe (thread + slot, torrent-picker pattern).
/// Probe task carries referrer/cookies so gated pages (X/Reddit) resolve.
fn start_yt_probe(
    st: &mut YtProbeState,
    rt: &tokio::runtime::Runtime,
    url: &str,
    referrer: &str,
    cookies: &str,
    cookies_json: &str,
) {
    let slot: YtProbeSlot = std::sync::Arc::new(std::sync::Mutex::new(None));
    st.slot = Some(slot.clone());
    st.url = url.to_string();
    st.loading = true;
    st.error.clear();
    st.dump = None;
    st.format.clear();
    st.quality.clear();
    st.audio.clear();
    st.extract.clear();
    st.subs.clear();
    st.auto_subs.clear();
    st.sub_embed = false;
    let probe_task = DownloadTask {
        url: url.to_string(),
        referrer: referrer.to_string(),
        cookies: cookies.to_string(),
        cookies_json: cookies_json.to_string(),
        ..Default::default()
    };
    let url_owned = url.to_string();
    rt.spawn(async move {
        let res = async {
            ytdlp::ensure_ytdlp().await?;
            ytdlp::fetch_formats(&probe_task.url, &probe_task).await
        }
        .await;
        if let Ok(mut g) = slot.lock() {
            *g = Some((url_owned, res));
        }
    });
}

/// Consume finished probe; stale URL results dropped.
/// Returns true when URL changed and caller may auto-start a fresh probe.
fn yt_probe_poll(st: &mut YtProbeState, url: &str) -> bool {
    if url.trim() != st.url {
        let had = st.loading || st.dump.is_some() || !st.error.is_empty() || st.slot.is_some();
        if had {
            st.loading = false;
            st.dump = None;
            st.error.clear();
            st.slot = None;
            st.format.clear();
            st.quality.clear();
            st.audio.clear();
            st.extract.clear();
            st.subs.clear();
            st.auto_subs.clear();
            st.sub_embed = false;
        }
        st.url = url.trim().to_string();
        return had || st.format.is_empty();
    }
    let done = st
        .slot
        .as_ref()
        .and_then(|s| s.lock().ok().and_then(|mut g| g.take()));
    if let Some((tag, res)) = done {
        if tag == url.trim() {
            st.loading = false;
            match res {
                Ok(d) => {
                    st.dump = Some(d);
                    st.error.clear();
                }
                Err(e) => st.error = e,
            }
        }
    }
    false
}

/// Grow/shrink native dialog viewport to fit content height (measured last
/// frame, all sections as-if expanded). `sent` dedupes commands; None =
/// viewport just opened, next fit always sends. Width stays at base.
fn fit_vp(
    ctx: &egui::Context,
    vp: egui::ViewportId,
    base: egui::Vec2,
    content_h: Option<f32>,
    sent: &mut Option<egui::Vec2>,
) {
    let monitor_h = ctx
        .input(|i| i.viewport().monitor_size.map(|m| m.y).filter(|y| *y > 0.0))
        .unwrap_or(900.0);
    let want_h = match content_h {
        Some(h) => fit_content_h(base.y, h, monitor_h),
        None => base.y,
    };
    let want = egui::vec2(base.x, want_h);
    if *sent != Some(want) {
        ctx.send_viewport_cmd_to(vp, egui::ViewportCommand::InnerSize(want));
        *sent = Some(want);
    }
}

/// Choice -> persisted pseudo-headers on task (see ytdlp::YtChoice).
fn yt_choice_from_state(st: &YtProbeState) -> ytdlp::YtChoice {
    let mut subs: Vec<String> = st.subs.iter().cloned().collect();
    subs.sort();
    let mut auto: Vec<String> = st.auto_subs.iter().cloned().collect();
    auto.sort();
    ytdlp::YtChoice {
        format: if st.format.is_empty() {
            None
        } else {
            Some(st.format.clone())
        },
        audio: if st.audio.is_empty() {
            None
        } else {
            Some(st.audio.clone())
        },
        extract: if st.extract.is_empty() {
            None
        } else {
            Some(st.extract.clone())
        },
        subs,
        auto_subs: auto,
        sub_embed: st.sub_embed,
        quality_height: st.quality.parse().ok().filter(|q: &u32| *q > 0),
    }
}

/// Friendly one-line summary + first raw line (<=160 chars) for a yt-dlp error.
/// Full raw text must never render inline; UI shows it only under Details.
fn yt_error_summary(e: &str) -> (String, String) {
    let first = e
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default();
    let raw_first_line: String = first.chars().take(160).collect();
    let t = e.trim();
    let friendly = if t.contains("page needs") || t.contains("Sign in") {
        "YouTube rejected browser cookies (yt-dlp limitation). Retries without cookies; public videos need no login.".to_string()
    } else if t.contains("NoneType") || t.contains("has no attribute") || t.starts_with("ERROR:") {
        "yt-dlp could not read this page/stream (extractor error). Download below still works.".to_string()
    } else if raw_first_line.is_empty() {
        "yt-dlp probe failed.".to_string()
    } else {
        format!("yt-dlp probe failed: {raw_first_line}")
    };
    (friendly, raw_first_line)
}

/// Quality/audio/subtitle picker. `id` prefixes egui ids per dialog.
#[allow(clippy::too_many_arguments)]
fn ui_yt_probe_section(
    ui: &mut egui::Ui,
    st: &mut YtProbeState,
    id: &str,
    url: &str,
    referrer: &str,
    cookies: &str,
    cookies_json: &str,
    rt: &tokio::runtime::Runtime,
) {
    yt_probe_poll(st, url);
    section(ui, "Video (yt-dlp)");
    if st.loading {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(egui::RichText::new("Fetching formats (silent)…").color(theme::TEXT_MUTED));
        });
        return;
    }
    if !st.error.is_empty() {
        let (friendly, _) = yt_error_summary(&st.error);
        ui.add(
            egui::Label::new(egui::RichText::new(friendly).color(theme::ERR).small()).wrap(),
        );
        egui::CollapsingHeader::new(egui::RichText::new("Details").small())
            .id_salt(format!("{id}_yt_err_details"))
            .default_open(false)
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt(format!("{id}_yt_err_scroll"))
                    .max_height(110.0)
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(&st.error).color(theme::TEXT_FAINT).small(),
                            )
                            .wrap(),
                        );
                    });
            });
        if ghost_btn_w(ui, "Retry probe").clicked() {
            start_yt_probe(st, rt, url, referrer, cookies, cookies_json);
        }
        return;
    }
    let Some(dump) = st.dump.clone() else {
        if ghost_btn_w(ui, "Probe formats (quality / audio / subtitles)").clicked() {
            start_yt_probe(st, rt, url, referrer, cookies, cookies_json);
        }
        return;
    };
    if !dump.title.is_empty() {
        ui.add(egui::Label::new(egui::RichText::new(&dump.title).strong()).truncate())
            .on_hover_text(&dump.title);
    }
    let mut meta: Vec<String> = Vec::new();
    if !dump.uploader().is_empty() {
        meta.push(dump.uploader().to_string());
    }
    if let Some(d) = dump.duration {
        meta.push(format!("{}:{:02}", (d / 60.0) as u32, (d % 60.0) as u32));
    }
    if !meta.is_empty() {
        ui.label(
            egui::RichText::new(meta.join(" · "))
                .color(theme::TEXT_MUTED)
                .small(),
        );
    }
    let videos = dump.video_formats();
    let audios = dump.audio_formats();
    let is_audio_sel = st.format.starts_with("audio:") || !st.extract.is_empty();
    form_grid(&format!("{id}-yt")).show(ui, |ui| {
        form_label(ui, "Quality");
        egui::ComboBox::from_id_salt(format!("{id}-yt-q"))
            .selected_text(if st.format.is_empty() {
                "Auto (best)".to_string()
            } else if let Some(id) = st.format.strip_prefix("audio:") {
                format!("Audio only ({id})")
            } else {
                videos
                    .iter()
                    .find(|f| f.format_id == st.format)
                    .map(|f| f.label())
                    .unwrap_or_else(|| st.format.clone())
            })
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(st.format.is_empty(), "Auto (best)")
                    .clicked()
                {
                    st.format.clear();
                }
                for f in &videos {
                    if ui
                        .selectable_label(st.format == f.format_id, f.label())
                        .clicked()
                    {
                        st.format = f.format_id.clone();
                        st.quality.clear();
                        st.extract.clear();
                        if f.is_video_only() {
                            if st.audio.is_empty() {
                                st.audio = "best".into();
                            }
                        } else {
                            st.audio.clear(); // muxed: audio choice meaningless
                        }
                    }
                }
                if !audios.is_empty() {
                    ui.separator();
                    for f in &audios {
                        let v = format!("audio:{}", f.format_id);
                        if ui
                            .selectable_label(st.format == v, format!("Audio only · {}", f.label()))
                            .clicked()
                        {
                            st.format = v;
                            st.quality.clear();
                        }
                    }
                }
            });
        {
            let vids = dump.video_formats();
            let max_h = vids.iter().filter_map(|f| f.height).max().unwrap_or(0);
            if !vids.is_empty() && vids.len() <= 2 && max_h <= 360 {
                ui.label(
                    egui::RichText::new(
                        "yt-dlp returned limited formats (cookie session) — qualities may missing. Webcraft retries without cookies automatically.",
                    )
                    .color(theme::WARN)
                    .small(),
                );
            }
        }
        // Max-height tier; selecting a tier clears any exact format (mutually exclusive).
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("Max")
                    .color(theme::TEXT_MUTED)
                    .small(),
            );
            egui::ComboBox::from_id_salt(format!("{id}-yt-max"))
                .selected_text(if st.quality.is_empty() {
                    "Auto (best)".to_string()
                } else {
                    format!("{}p", st.quality)
                })
                .show_ui(ui, |ui| {
                    for (v, lbl) in [
                        ("", "Auto (best)"),
                        ("2160", "2160p"),
                        ("1440", "1440p"),
                        ("1080", "1080p"),
                        ("720", "720p"),
                        ("480", "480p"),
                        ("360", "360p"),
                    ] {
                        if ui.selectable_label(st.quality == v, lbl).clicked() {
                            st.quality = v.to_string();
                            if !v.is_empty() {
                                st.format.clear();
                            }
                        }
                    }
                })
                .response
                .on_hover_text("Max quality tier. Selecting a tier clears any exact format picked above.");
        });
        ui.end_row();
        if !is_audio_sel {
            form_label(ui, "Audio track");
            let cur_audio = videos.iter().find(|f| f.format_id == st.format);
            let video_only = cur_audio.map(|f| f.is_video_only()).unwrap_or(false);
            ui.add_enabled_ui(st.format.is_empty() || video_only, |ui| {
                egui::ComboBox::from_id_salt(format!("{id}-yt-a"))
                    .selected_text(match st.audio.as_str() {
                        "" | "best" => "Best audio".to_string(),
                        "none" => "None (silent)".to_string(),
                        other => audios
                            .iter()
                            .find(|f| f.format_id == other)
                            .map(|f| f.label())
                            .unwrap_or_else(|| other.to_string()),
                    })
                    .show_ui(ui, |ui| {
                        if ui
                            .selectable_label(
                                st.audio.is_empty() || st.audio == "best",
                                "Best audio",
                            )
                            .clicked()
                        {
                            st.audio = "best".into();
                        }
                        for f in &audios {
                            if ui
                                .selectable_label(st.audio == f.format_id, f.label())
                                .clicked()
                            {
                                st.audio = f.format_id.clone();
                            }
                        }
                        if ui
                            .selectable_label(st.audio == "none", "None (silent)")
                            .clicked()
                        {
                            st.audio = "none".into();
                        }
                    });
            });
            ui.end_row();
        } else {
            form_label(ui, "Audio format");
            egui::ComboBox::from_id_salt(format!("{id}-yt-x"))
                .selected_text(match st.extract.as_str() {
                    "mp3" => "MP3 (extract)",
                    "m4a" => "M4A (extract)",
                    _ => "Original",
                })
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(st.extract.is_empty(), "Original")
                        .clicked()
                    {
                        st.extract.clear();
                    }
                    if ui
                        .selectable_label(st.extract == "mp3", "MP3 (extract)")
                        .clicked()
                    {
                        st.extract = "mp3".into();
                        if st.format.is_empty() {
                            st.format = "audio:ba".into();
                        }
                    }
                    if ui
                        .selectable_label(st.extract == "m4a", "M4A (extract)")
                        .clicked()
                    {
                        st.extract = "m4a".into();
                        if st.format.is_empty() {
                            st.format = "audio:ba".into();
                        }
                    }
                });
            ui.end_row();
        }
    });
    let man = dump.sub_langs();
    let auto = dump.auto_langs();
    if !man.is_empty() || !auto.is_empty() {
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new("Subtitles (.srt sidecar)")
                .color(theme::TEXT_MUTED)
                .small(),
        );
        egui::ScrollArea::vertical()
            .id_salt(format!("{id}-yt-subs"))
            .max_height(110.0)
            .show(ui, |ui| {
                for lang in &man {
                    let mut on = st.subs.contains(lang);
                    if ui.checkbox(&mut on, lang.as_str()).changed() {
                        if on {
                            st.subs.insert(lang.clone());
                        } else {
                            st.subs.remove(lang);
                        }
                    }
                }
                for lang in &auto {
                    let mut on = st.auto_subs.contains(lang);
                    if ui.checkbox(&mut on, format!("{lang} (auto)")).changed() {
                        if on {
                            st.auto_subs.insert(lang.clone());
                        } else {
                            st.auto_subs.remove(lang);
                        }
                    }
                }
            });
        if !is_audio_sel {
            ui.checkbox(
                &mut st.sub_embed,
                "Embed subtitles in video (instead of .srt sidecar)",
            );
        }
    }
}

fn vp_id(name: &str) -> egui::ViewportId {
    egui::ViewportId::from_hash_of(name)
}

/// Compact stat tile: faint label above strong value (progress dialog).
fn stat_tile(ui: &mut egui::Ui, label: &str, value: String) {
    ui.vertical(|ui| {
        ui.label(egui::RichText::new(label).color(theme::TEXT_FAINT).small());
        ui.add(egui::Label::new(egui::RichText::new(value).strong()).truncate());
    });
}

/// TextEdit filling grid field col.
/// Header draft row check: Ok = empty (ignored) or valid wire header.
/// Name must parse as HTTP header name; value must not contain CR/LF.
/// Error text never includes the value (may hold a token).
fn header_row_error(name: &str, value: &str) -> Option<String> {
    let (name, value) = (name.trim(), value.trim());
    if name.is_empty() && value.is_empty() {
        return None;
    }
    if name.is_empty() {
        return Some("Header name is empty.".into());
    }
    if reqwest::header::HeaderName::from_bytes(name.as_bytes()).is_err() {
        return Some(format!("Invalid header name: {name}"));
    }
    if reqwest::header::HeaderValue::from_str(value).is_err() {
        return Some(format!(
            "Invalid value for header {name} (CR/LF not allowed)."
        ));
    }
    None
}

fn fill_edit<'a>(buf: &'a mut String) -> egui::TextEdit<'a> {
    egui::TextEdit::singleline(buf).desired_width(f32::INFINITY)
}

/// DragValue builder (width fixed via `drag_w` / add_sized at call site).
fn drag<'a, T: egui::emath::Numeric>(val: &'a mut T) -> egui::DragValue<'a> {
    egui::DragValue::new(val)
}

/// Right-aligned button row. RTL: first added = rightmost, so primary first.
/// Buttons: BTN_H height (theme), min width 72. Own top gap from preceding
/// content so footers never glue to the separator above (was y=4 global).
fn btn_row(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(theme::SP_XS);
    // One BTN_H row (was: whole remaining height, buttons floated mid-window).
    ui.allocate_ui_with_layout(
        egui::vec2(ui.available_width(), theme::BTN_H),
        egui::Layout::right_to_left(egui::Align::Center),
        add,
    );
}

/// Native dialog viewport frame: window fill + margin (Frame::none left black bg, edge-to-edge content).
fn dialog_frame() -> egui::Frame {
    egui::Frame::none()
        .fill(theme::SURFACE)
        .inner_margin(egui::Margin::same(theme::SP_LG))
}

/// Pinned dialog footer height: separator + button row.
const FOOTER_H: f32 = theme::BTN_H + theme::SP_SM + 12.0;

/// Primary/ghost button with min width 72 (height = BTN_H via theme).
/// Painted like toolbar icon_btn (fixed .fill() had no hover/pressed/disabled state).
fn primary_btn_w(ui: &mut egui::Ui, label: &str) -> egui::Response {
    paint_btn(ui, None, label, true, 72.0, true)
}
fn ghost_btn_w(ui: &mut egui::Ui, label: &str) -> egui::Response {
    paint_btn(ui, None, label, false, 72.0, true)
}

/// Bordered action group: related controls stay together and wrap as one unit.
fn action_group(ui: &mut egui::Ui, label: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.vertical(|ui| {
        ui.label(
            egui::RichText::new(label)
                .color(theme::TEXT_FAINT)
                .small()
                .strong(),
        );
        ui.horizontal_wrapped(add);
    });
    ui.add_space(8.0);
}

impl DownloadApp {
    fn ui_downloads(&mut self, ui: &mut egui::Ui) {
        // Header + actions (IDM toolbar: Add URL, Resume, Stop, Stop All, Delete, Options…)
        ui.horizontal_wrapped(|ui| {
            ui.heading("Downloads");
            theme::badge(
                ui,
                &self.category.clone(),
                theme::category_color(&self.category),
            );
        });
        ui.add_space(4.0);
        let selected_task = self.selected.as_ref().and_then(|id| {
            let g = crate::lock_unpoison(&self.rt);
            g.state.tasks.iter().find(|t| &t.id == id).cloned()
        });
        let has_active = {
            let g = crate::lock_unpoison(&self.rt);
            g.state
                .tasks
                .iter()
                .any(|t| t.status == TaskStatus::Downloading)
        };
        let has_completed = {
            let g = crate::lock_unpoison(&self.rt);
            g.state
                .tasks
                .iter()
                .any(|t| t.status == TaskStatus::Complete)
        };
        ui.horizontal_wrapped(|ui| {
            action_group(ui, "Add", |ui| {
                if icon_btn(ui, Icon::Plus, "Add URL", true, 86.0).clicked() {
                    {
                        self.show_add = true;
                        self.add_just_opened = true; self.add_raise_frames = 90; self.add_focus_done = false;
                    }
                }
            });
            action_group(ui, "Selected download", |ui| {
                let can_resume = selected_task.as_ref().is_some_and(|t| {
                    !matches!(t.status, TaskStatus::Downloading | TaskStatus::Complete)
                });
                let can_stop = selected_task
                    .as_ref()
                    .is_some_and(|t| t.status == TaskStatus::Downloading);
                if icon_btn_enabled(ui, Icon::Play, "Resume", false, 78.0, can_resume).clicked() {
                    if let Some(id) = self.selected.clone() {
                        self.start_task(&id);
                    }
                }
                if icon_btn_enabled(ui, Icon::Pause, "Stop", false, 62.0, can_stop).clicked() {
                    if let Some(id) = self.selected.clone() {
                        stop_task(self, &id);
                    }
                }
                if let Some(t) = &selected_task {
                    if icon_btn(ui, Icon::Info, "Progress", false, 82.0).clicked() {
                        self.show_progress_for = Some(t.id.clone());
                        self.progress_raise = true;
                    }
                } else {
                    let _ = icon_btn_enabled(ui, Icon::Info, "Progress", false, 82.0, false);
                }
                if icon_btn_enabled(
                    ui,
                    Icon::Delete,
                    "Delete",
                    false,
                    70.0,
                    selected_task.is_some(),
                )
                .clicked()
                    && !self.selected_ids.is_empty()
                {
                    self.pending_delete = Some(self.selected_ids.iter().cloned().collect());
                    self.delete_from_disk = false;
                }
            });
            action_group(ui, "All downloads", |ui| {
                if icon_btn_enabled(ui, Icon::Pause, "Stop all", false, 74.0, has_active).clicked()
                {
                    stop_all(self);
                }
                if icon_btn_enabled(
                    ui,
                    Icon::Check,
                    "Delete completed",
                    false,
                    128.0,
                    has_completed,
                )
                .clicked()
                {
                    delete_completed(self);
                }
            });
        });
        ui.add_space(theme::SP_XS);
        ui.horizontal_wrapped(|ui| {
            let (shown, total) = self.search_counts;
            let filtering = !self.search.trim().is_empty()
                || self.category != "All Downloads"
                || self.type_filter.is_some();
            ui.label(
                egui::RichText::new(if filtering {
                    format!("Showing {shown} of {total}")
                } else {
                    format!("{total} downloads")
                })
                .color(theme::TEXT_MUTED)
                .small(),
            );
            if filtering && theme::ghost_btn(ui, "Clear filters").clicked() {
                self.search.clear();
                self.category = "All Downloads".into();
                self.type_filter = None;
            }
            ui.label(
                egui::RichText::new(
                    "Click headers to sort · Drag header edges to resize · Double-click row: progress · Right-click: actions",
                )
                .color(theme::TEXT_FAINT)
                .small(),
            );
        });
        ui.add_space(theme::SP_XS);

        // Prune stale selection ids (deleted elsewhere) before render.
        {
            let alive: std::collections::HashSet<String> = {
                let g = crate::lock_unpoison(&self.rt);
                g.state.tasks.iter().map(|t| t.id.clone()).collect()
            };
            self.selected_ids.retain(|id| alive.contains(id));
            if self
                .selected
                .as_deref()
                .is_some_and(|id| !alive.contains(id))
            {
                self.selected = None;
            }
        }
        // snapshot + filter + sort
        let mut tasks: Vec<DownloadTask> = {
            let g = crate::lock_unpoison(&self.rt);
            g.state.tasks.clone()
        };
        let total_tasks = tasks.len();
        let cat = self.category.clone();
        // Search matches filename, URL, category, status, queue. Whitespace
        // terms AND together. Unicode-safe lowercase (filename uses same).
        let terms: Vec<String> = self
            .search
            .to_lowercase()
            .split_whitespace()
            .map(str::to_string)
            .collect();
        tasks.retain(|t| {
            let cat_ok = match cat.as_str() {
                "All Downloads" => true,
                "Unfinished" => t.status != TaskStatus::Complete,
                "Finished" => t.status == TaskStatus::Complete,
                c => t.category == c || crate::model::category_for(&t.filename) == c,
            };
            let type_ok = self
                .type_filter
                .is_none_or(|f| f.matches(crate::stream::proto_kind(t)));
            let search_ok = terms.iter().all(|q| {
                t.filename.to_lowercase().contains(q)
                    || t.url.to_lowercase().contains(q)
                    || t.category.to_lowercase().contains(q)
                    || t.queue_id.to_lowercase().contains(q)
                    || format!("{:?}", t.status).to_lowercase().contains(q)
            });
            cat_ok && type_ok && search_ok
        });
        self.search_counts = (tasks.len(), total_tasks);
        self.visible_ids = tasks.iter().map(|t| t.id.clone()).collect();
        // Stable sort; descending flips comparator result only, so equal
        // keys keep insertion order in both directions (was reverse()).
        let asc = self.sort_asc;
        let rates: std::collections::HashMap<String, f64> = tasks
            .iter()
            .map(|t| (t.id.clone(), self.task_rate(&t.id)))
            .collect();
        let cmp = |a: &DownloadTask, b: &DownloadTask| -> std::cmp::Ordering {
            use std::cmp::Ordering;
            let ord = match self.sort_col {
                0 => a.filename.to_lowercase().cmp(&b.filename.to_lowercase()),
                1 => a.queue_id.cmp(&b.queue_id),
                2 => a.total.unwrap_or(0).cmp(&b.total.unwrap_or(0)),
                3 => format!("{:?}", a.status).cmp(&format!("{:?}", b.status)),
                4 => {
                    let eta_of = |t: &DownloadTask| -> f64 {
                        match (t.total, rates.get(&t.id)) {
                            (Some(total), Some(r)) if *r > 1.0 && total > t.downloaded => {
                                (total - t.downloaded) as f64 / r
                            }
                            _ => f64::INFINITY,
                        }
                    };
                    eta_of(a).partial_cmp(&eta_of(b)).unwrap_or(Ordering::Equal)
                }
                5 => rates
                    .get(&a.id)
                    .copied()
                    .unwrap_or(0.0)
                    .partial_cmp(&rates.get(&b.id).copied().unwrap_or(0.0))
                    .unwrap_or(Ordering::Equal),
                6 => {
                    let frac = |t: &DownloadTask| -> f64 {
                        t.total
                            .filter(|n| *n > 0)
                            .map(|n| t.downloaded as f64 / n as f64)
                            .unwrap_or(0.0)
                    };
                    frac(a).partial_cmp(&frac(b)).unwrap_or(Ordering::Equal)
                }
                _ => a.order.cmp(&b.order),
            };
            if asc {
                ord
            } else {
                ord.reverse()
            }
        };
        tasks.sort_by(cmp);

        let mut to_start: Option<String> = None;
        let mut to_stop: Option<String> = None;
        let mut to_remove: Option<String> = None;
        let mut to_progress: Option<String> = None;
        let mut to_open: Option<std::path::PathBuf> = None;
        let compact_table = ui.available_width() < 760.0;
        // Outer vertical scroll wraps inner horizontal scroll (wide user widths scroll sideways, header+rows together).
        egui::ScrollArea::vertical().id_salt("dl_outer").auto_shrink([false, false]).show(ui, |ui| {
        egui::ScrollArea::horizontal().id_salt("dl_h").auto_shrink([false, false]).show(ui, |ui| {
        let avail = ui.available_width().max(200.0);
        let stored = if compact_table { self.dl_widths_compact.as_deref() } else { self.dl_widths.as_deref() };
        let mut cols = dl_cols(avail, compact_table, stored);
        // Header: flat sheet row + resize handles (handles overlay, after cells so drag wins over sort click at boundary).
        let (hdr_rect, _) = ui.allocate_exact_size(egui::vec2(cols.iter().sum::<f32>() + COL_GAP * (cols.len() - 1) as f32, ROW_H + 2.0), egui::Sense::hover());
        sheet_header(ui, hdr_rect);
        {
            let mut c = ui.new_child(egui::UiBuilder::new().max_rect(hdr_rect).layout(egui::Layout::left_to_right(egui::Align::Center)));
            let ui = &mut c;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = COL_GAP;
                for (i, (h, sc)) in if compact_table { vec![("File Name", Some(0u8)), ("Size", Some(2u8)), ("Status", Some(3u8)), ("Progress", Some(6u8)), ("", None)] } else { vec![("File Name", Some(0u8)), ("Q", Some(1u8)), ("Size", Some(2u8)), ("Status", Some(3u8)), ("Time left", Some(4u8)), ("Rate", Some(5u8)), ("Progress", Some(6u8)), ("", None)] }.iter().enumerate() {
                    let arrow = if Some(self.sort_col) == *sc { if self.sort_asc { " ▲" } else { " ▼" } } else { "" };
                    let hdr = |ui: &mut egui::Ui, f: &dyn Fn(&mut egui::Ui) -> bool| -> bool {
                        if matches!(i, 2 | 4 | 5) { cell_r(ui, cols[i], |ui| f(ui)) } else { cell(ui, cols[i], |ui| f(ui)) }
                    };
                    let clicked = match sc {
                        Some(_) => hdr(ui, &|ui| ui.add(egui::Label::new(theme::table_header_text(&format!("{h}{arrow}"))).truncate().selectable(false).sense(egui::Sense::click())).on_hover_cursor(egui::CursorIcon::PointingHand).clicked()),
                        None => { hdr(ui, &|ui| { ui.add(egui::Label::new(theme::table_header_text(h)).truncate().selectable(false)); false }) }
                    };
                    if clicked {
                        let sc = sc.unwrap();
                        if self.sort_col == sc { self.sort_asc = !self.sort_asc; } else { self.sort_col = sc; self.sort_asc = true; }
                    }
                }
            });
        }
        if col_resize_handles(ui, hdr_rect, &mut cols, compact_table) {
            if compact_table { self.dl_widths_compact = Some(cols.clone()); } else { self.dl_widths = Some(cols.clone()); }
        }

        if tasks.is_empty() {
            let filtering = !self.search.trim().is_empty()
                || self.category != "All Downloads"
                || self.type_filter.is_some();
            ui.vertical_centered(|ui| {
                ui.add_space(24.0);
                if filtering {
                    ui.label(egui::RichText::new("No downloads match filters").color(theme::TEXT_MUTED).size(14.0));
                    ui.label(egui::RichText::new("Try different search terms, or clear filters.").color(theme::TEXT_MUTED));
                    if theme::ghost_btn(ui, "Clear filters").clicked() {
                        self.search.clear();
                        self.category = "All Downloads".into();
                        self.type_filter = None;
                    }
                } else {
                    ui.label(egui::RichText::new("No downloads here").color(theme::TEXT_MUTED).size(14.0));
                    ui.label(egui::RichText::new("Add a URL, paste from clipboard (Tasks → Add batch), or send from browser extension.").color(theme::TEXT_MUTED));
                    if theme::primary_btn(ui, "+ Add URL").clicked() { { self.show_add = true; self.add_just_opened = true; self.add_raise_frames = 90; self.add_focus_done = false; } }
                }
                ui.add_space(24.0);
            });
            return;
        }

        {
            let n_tasks = tasks.len();
            for (row_idx, t) in tasks.iter().enumerate() {
              let t = t.clone();
              ui.push_id(t.id.as_str(), |ui| {
                let sel = self.selected_ids.contains(&t.id);
                let row_w = cols.iter().sum::<f32>() + COL_GAP * (cols.len() - 1) as f32;
                let (row_rect, resp) = ui.allocate_exact_size(egui::vec2(row_w, ROW_H + 2.0), egui::Sense::click());
                sheet_row(ui, row_rect, sel, row_idx, row_idx + 1 == n_tasks, resp.hovered());
                let mut c = ui.new_child(egui::UiBuilder::new().max_rect(row_rect).layout(egui::Layout::left_to_right(egui::Align::Center)));
                {
                    let ui = &mut c;

                    ui.horizontal(|ui| { ui.spacing_mut().item_spacing.x = COL_GAP; let mut ci = 0usize; let mut nw = || { ci += 1; cols[ci - 1] };
                        let task_rate = self.task_rate(&t.id);
                        cell(ui, nw(), |ui| { let (irect, _) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover()); icon(ui.painter(), &cat_icon(&t.filename), irect, theme::TEXT_MUTED); let kind = crate::stream::proto_kind(&t); let bw = theme::badge_width(ui, kind.label()); if ui.available_width() - bw >= 60.0 { theme::badge(ui, kind.label(), theme::proto_color(kind)); } if !t.debrid_info.is_empty() { let db = theme::badge_width(ui, "Debrid"); if ui.available_width() - db >= 60.0 { theme::badge(ui, "Debrid", theme::TEAL_500); } } ui.add(egui::Label::new(egui::RichText::new(&t.filename).color(theme::TEXT).small()).truncate().selectable(false)).on_hover_text(if t.error.is_empty() { format!("{} - {}\n{}{}", kind.full_name(), t.filename, t.url, if t.debrid_info.is_empty() { String::new() } else { format!("\nDebrid: {}", t.debrid_info) }) } else { format!("{}\nError: {}", t.filename, t.error) }); });
                        if !compact_table { let w = nw(); cell(ui, w, |ui| ui.add(egui::Label::new(egui::RichText::new(if t.queue_id.is_empty() { "main" } else { &t.queue_id }).color(theme::TEXT_MUTED).small()).truncate().selectable(false)).on_hover_text(if t.queue_id.is_empty() { "main".to_string() } else { t.queue_id.clone() })); }
                        let w = nw(); cell_r(ui, w, |ui| ui.add(egui::Label::new(egui::RichText::new(match t.total { Some(n) => fmt_bytes(n), None => "—".into() }).color(theme::TEXT_MUTED).small()).truncate().selectable(false)).on_hover_text(match t.total { Some(n) => fmt_bytes(n), None => "unknown size".to_string() }));
                        let sc = theme::status_color(t.status);
                        let phase_lbl = if t.status == TaskStatus::Downloading && (t.headers.contains_key("x-ytdlp") || ytdlp::is_video_page(&t.url)) { crate::engine::phase_name(self.phase_map.get(&t.id).copied().unwrap_or(0)) } else { "" };
                        let w = nw(); cell(ui, w, |ui| ui.add(egui::Label::new(egui::RichText::new(if phase_lbl.is_empty() { format!("{:?}", t.status) } else { phase_lbl.to_string() }).color(sc).small().strong()).truncate().selectable(false)).on_hover_text(if phase_lbl.is_empty() { format!("{:?}", t.status) } else { phase_lbl.to_string() }));
                        if !compact_table {
                            // ETA from THIS task's own rate (was global rate for all rows)
                            let eta_txt = if t.status == TaskStatus::Downloading { if let Some(total) = t.total { if task_rate > 1.0 && total > t.downloaded { eta((total - t.downloaded) as f64 / task_rate) } else { "—".into() } } else { "—".into() } } else { "—".into() };
                            let w = nw(); cell_r(ui, w, |ui| { let eta_hover = eta_txt.clone(); ui.add(egui::Label::new(egui::RichText::new(eta_txt).color(theme::TEXT_MUTED).small()).truncate().selectable(false)).on_hover_text(eta_hover); });
                            let w = nw(); cell_r(ui, w, |ui| { let rate_txt = if t.status == TaskStatus::Downloading { fmt_rate(task_rate as f32) } else { "—".into() }; ui.add(egui::Label::new(egui::RichText::new(&rate_txt).color(theme::TEXT_MUTED).small()).truncate().selectable(false)).on_hover_text(rate_txt); });
                        }
                        let seg_view = self.seg_info.get(&t.id);
                        let frac_opt = display_fraction(&t, seg_view);
                        let frac = frac_opt.unwrap_or(0.0);
                        let hover = format!("{}{}", if frac_opt.is_some() { format!("{:.1}% · ", frac * 100.0) } else { String::new() }, if t.total.is_some() { format!("{} of {}", fmt_bytes(t.downloaded), t.total.map(fmt_bytes).unwrap_or_else(|| "?".into())) } else if let Some(v) = seg_view.filter(|v| v.manifest) { format!("{} | {}", fmt_bytes(t.downloaded), v.label()) } else { format!("{} of ?", fmt_bytes(t.downloaded)) });
                        let w = nw(); cell(ui, w, |ui| {
                            let bar = egui::ProgressBar::new(frac).desired_width(72.0).desired_height(6.0).rounding(egui::Rounding::same(3.0)).fill(theme::PRIMARY);
                            ui.add(bar).on_hover_text(&hover);
                            ui.add(egui::Label::new(egui::RichText::new(if frac_opt.is_some() { format!("{:.1}%", frac * 100.0) } else { "?".into() }).color(theme::TEXT_MUTED).small()).truncate().selectable(false)).on_hover_text(&hover);
                        });
                        let w = nw(); cell(ui, w, |ui| {
                            if t.status == TaskStatus::Downloading {
                                if row_icon_btn(ui, Icon::Pause, "Stop").clicked() { to_stop = Some(t.id.clone()); }
                            } else if t.status != TaskStatus::Complete && row_icon_btn(ui, Icon::Play, "Start / Resume").clicked() { to_start = Some(t.id.clone()); }
                            if row_icon_btn(ui, Icon::Info, "Progress details").clicked() { to_progress = Some(t.id.clone()); }
                        });
                    });

                }

                if resp.clicked() {
                    // Ctrl toggle, Shift range over visible order, plain single.
                    let (ctrl, shift) = ui.input(|i| (i.modifiers.ctrl || i.modifiers.command, i.modifiers.shift));
                    if ctrl {
                        if !self.selected_ids.remove(&t.id) {
                            self.selected_ids.insert(t.id.clone());
                        }
                        self.selected = Some(t.id.clone());
                        self.selection_anchor = Some(t.id.clone());
                    } else if shift {
                        let range = self.selection_anchor.as_ref().and_then(|anchor| {
                            let a = self.visible_ids.iter().position(|x| x == anchor)?;
                            let b = self.visible_ids.iter().position(|x| x == &t.id)?;
                            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
                            Some(self.visible_ids[lo..=hi].to_vec())
                        });
                        if let Some(ids) = range {
                            self.selected_ids = ids.into_iter().collect();
                            self.selected = Some(t.id.clone());
                        } else {
                            self.selected_ids.clear();
                            self.selected_ids.insert(t.id.clone());
                            self.selected = Some(t.id.clone());
                            self.selection_anchor = Some(t.id.clone());
                        }
                    } else {
                        self.selected_ids.clear();
                        self.selected_ids.insert(t.id.clone());
                        self.selected = Some(t.id.clone());
                        self.selection_anchor = Some(t.id.clone());
                    }
                }
                if resp.double_clicked() {
                    // Complete + file on disk: open it. Else (incl. complete-but-missing): progress dialog.
                    let path = t.dest_dir.join(&t.filename);
                    if t.status == TaskStatus::Complete && path.exists() { to_open = Some(path); } else { to_progress = Some(t.id.clone()); }
                }
                resp.context_menu(|ui| {
                    if ui.add_enabled(matches!(t.status, TaskStatus::Paused | TaskStatus::Error | TaskStatus::Queued), egui::Button::new("Start / Resume")).clicked() { to_start = Some(t.id.clone()); ui.close_menu(); }
                    if ui.add_enabled(t.status == TaskStatus::Downloading, egui::Button::new("Stop")).clicked() { to_stop = Some(t.id.clone()); ui.close_menu(); }
                    if ui.button("Progress…").clicked() { to_progress = Some(t.id.clone()); ui.close_menu(); }
                    if ui.button("Open folder").clicked() { let _ = crate::platform::reveal_path(&t.dest_dir.join(&t.filename)); ui.close_menu(); }
                    if ui.button("Copy URL").clicked() { ui.output_mut(|o| o.copied_text = t.url.clone()); ui.close_menu(); }
                    ui.separator();
                    if ui.button("Remove").clicked() { to_remove = Some(t.id.clone()); ui.close_menu(); }
                });
              });
            }
        }
        });
        });
        if let Some(id) = to_start {
            self.start_task(&id);
        }
        if let Some(id) = to_stop {
            stop_task(self, &id);
        }
        if let Some(id) = to_remove {
            // Right-clicked row inside multi-selection deletes whole set.
            if self.selected_ids.contains(&id) && self.selected_ids.len() > 1 {
                self.pending_delete = Some(self.selected_ids.iter().cloned().collect());
            } else {
                self.pending_delete = Some(vec![id]);
            }
            self.delete_from_disk = false;
        }
        if let Some(p) = to_open {
            let _ = crate::platform::open_path(&p);
        }
        if let Some(id) = to_progress {
            self.show_progress_for = Some(id);
            self.progress_raise = true;
        }
    }

    /// IDM-style "Download File Info" confirm window for browser captures.
    /// One at a time; confirming/dropping pops next from rt.pending_captures.
    fn ui_capture_dialog(&mut self, ctx: &egui::Context) {
        // Pull next pending capture into editor state.
        if self.capture_task.is_none() {
            let next = {
                let mut g = crate::lock_unpoison(&self.rt);
                if g.pending_captures.is_empty() {
                    None
                } else {
                    Some(g.pending_captures.remove(0))
                }
            };
            match next {
                Some(t) => {
                    self.capture_name = t.filename.clone();
                    self.capture_task = Some(t);
                    self.capture_just_opened = true;
                    self.cap_vp_sent = None;
                    self.cap_body_h = None;
                    self.capture_raise = true;
                    self.cap_raise_frames = 90;
                    self.cap_focus_done = false;
                }
                None => return,
            }
        }
        let task = self.capture_task.clone().unwrap();
        let queues: Vec<(String, String)> = {
            let g = crate::lock_unpoison(&self.rt);
            g.state
                .queues
                .iter()
                .map(|q| (q.id.clone(), q.name.clone()))
                .collect()
        };
        let mut action: Option<&'static str> = None; // "start" | "later" | "cancel"
                                                     // Independent native window: no main-rect clip / shrink-wrap.
        let builder = egui::ViewportBuilder::default()
            .with_title("Download file")
            .with_inner_size([520.0, 480.0])
            .with_min_inner_size([440.0, 360.0])
            .with_max_inner_size([4096.0, 4096.0])
            .with_resizable(true);
        let cap_body = |ui: &mut egui::Ui, app: &mut DownloadApp| {
            ui.label(
                egui::RichText::new("URL")
                    .color(theme::TEXT_MUTED)
                    .small()
                    .strong(),
            );
            // Task clone edited locally; writes back into capture_task below.
            let mut t = task.clone();
            ui.add(
                egui::Label::new(egui::RichText::new(&t.url).color(theme::STEEL_TEXT).small())
                    .wrap(),
            );
            ui.horizontal(|ui| {
                let kind = crate::stream::proto_kind(&t);
                theme::badge(ui, kind.label(), theme::proto_color(kind));
                let cat = if t.category.is_empty() {
                    crate::model::category_for(&t.filename)
                } else {
                    t.category.as_str()
                };
                theme::badge(ui, cat, theme::category_color(cat));
            });
            ui.add_space(6.0);
            ui.add_space(6.0);
            let mut focus_name = false;
            form_grid("cap-grid").show(ui, |ui| {
                form_label(ui, "Save as");
                let name_resp = ui.add(
                    egui::TextEdit::singleline(&mut app.capture_name)
                        .id(egui::Id::new("capture-name"))
                        .desired_width(f32::INFINITY),
                );
                let vp_focused = ui.ctx().input(|i| i.focused);
                if app.capture_just_opened || (vp_focused && !app.cap_focus_done) {
                    name_resp.request_focus();
                    focus_name = true;
                    if vp_focused {
                        app.cap_focus_done = true;
                    }
                }
                ui.end_row();
                if let Some(total) = t.total {
                    if total > 0 {
                        form_label(ui, "Size");
                        ui.label(
                            egui::RichText::new(fmt_bytes(total))
                                .color(theme::TEXT)
                                .small(),
                        );
                        ui.end_row();
                    }
                }
                form_label(ui, "Folder");
                ui.horizontal(|ui| {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(t.dest_dir.display().to_string())
                                .color(theme::TEXT)
                                .small(),
                        )
                        .truncate(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ghost_btn_w(ui, "Browse…").clicked() {
                            if let Some(p) = rfd::FileDialog::new().pick_folder() {
                                t.dest_dir = p;
                            }
                        }
                    });
                });
                ui.end_row();
                form_label(ui, "Category");
                egui::ComboBox::from_id_salt("cap-cat")
                    .selected_text(t.category.clone())
                    .width(ui.available_width())
                    .show_ui(ui, |ui| {
                        for c in [
                            "General",
                            "Compressed",
                            "Documents",
                            "Programs",
                            "Video",
                            "Audio",
                        ] {
                            ui.selectable_value(&mut t.category, c.to_string(), c);
                        }
                    });
                ui.end_row();
                form_label(ui, "Queue");
                let cur = queues
                    .iter()
                    .find(|(id, _)| *id == t.queue_id)
                    .map(|(_, n)| n.clone())
                    .unwrap_or_else(|| t.queue_id.clone());
                egui::ComboBox::from_id_salt("cap-queue")
                    .selected_text(cur)
                    .width(ui.available_width())
                    .show_ui(ui, |ui| {
                        for (id, name) in &queues {
                            if ui.selectable_label(t.queue_id == *id, name).clicked() {
                                t.queue_id = id.clone();
                            }
                        }
                    });
                ui.end_row();
            });
            // Engine choice for streams/video pages: native (default) vs yt-dlp.
            // Stored as x-ytdlp header, honored before manifest dispatch.
            {
                use crate::stream::ProtoKind as K;
                let kind = crate::stream::proto_kind(&t);
                if matches!(kind, K::Dash | K::Hls | K::YtDlp) {
                    let mut use_ytdlp = t.headers.contains_key("x-ytdlp");
                    if ui
                        .checkbox(&mut use_ytdlp, "Use yt-dlp for this stream (else built-in)")
                        .changed()
                    {
                        if use_ytdlp {
                            t.headers.insert("x-ytdlp".into(), "1".into());
                        } else {
                            t.headers.remove("x-ytdlp");
                        }
                    }
                }
            }
            // HTTP prewarm for captured files (mirrors Add dialog).
            if prewarm_eligible(&t) {
                prewarm_poll(&mut app.prewarm_cap, &t.url);
                let rt = app.tokio_rt.clone();
                let conns = crate::lock_unpoison(&app.rt).state.settings.max_connections;
                spawn_prewarm(&mut app.prewarm_cap, &rt, &t, conns);
                if !app.prewarm_cap.text.is_empty() {
                    ui.label(egui::RichText::new(&app.prewarm_cap.text).small().color(theme::TEXT_MUTED));
                } else if app.prewarm_cap.loading {
                    ui.label(egui::RichText::new("Probing…").small().color(theme::TEXT_MUTED));
                }
            }
            // yt-dlp picker: video page or explicit yt-dlp engine.
            // Auto-start probe once per capture (silent, cookies attached).
            {
                let is_yt = t.headers.contains_key("x-ytdlp")
                    || ytdlp::is_video_page(&t.url)
                    || t.headers
                        .get("x-page-intent")
                        .map(|s| s == "page")
                        .unwrap_or(false);
                if is_yt {
                    // New capture: probe state still tagged with previous URL.
                    // Poll resets tag; autostart when state is virgin for this URL.
                    let virgin = app.yt_probe_cap.url != t.url
                        && !app.yt_probe_cap.loading
                        && app.yt_probe_cap.dump.is_none();
                    if virgin {
                        let rt = app.tokio_rt.clone();
                        start_yt_probe(
                            &mut app.yt_probe_cap,
                            &rt,
                            &t.url,
                            &t.referrer,
                            &t.cookies,
                            &t.cookies_json,
                        );
                    }
                    let (u, r, c) = (t.url.clone(), t.referrer.clone(), t.cookies.clone());
                    let rt = app.tokio_rt.clone();
                    ui.add_space(6.0);
                    ui_yt_probe_section(
                        ui,
                        &mut app.yt_probe_cap,
                        "cap",
                        &u,
                        &r,
                        &c,
                        &t.cookies_json,
                        &rt,
                    );
                    // Probe title fills filename while user kept funnel name.
                    if let Some(d) = &app.yt_probe_cap.dump {
                        if !d.title.is_empty()
                            && (app.capture_name.is_empty() || !app.capture_name.contains('.'))
                        {
                            let ext = "mp4";
                            app.capture_name =
                                crate::model::sanitize_filename(&format!("{}.{}", d.title, ext));
                        }
                    }
                    if let Some(stream) = t.headers.get("x-page-stream").cloned() {
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new("Sniffed stream fallback:")
                                    .color(theme::TEXT_FAINT)
                                    .small(),
                            );
                            if ghost_btn_w(ui, "Use sniffed stream instead").clicked() {
                                // Title-filled capture name belongs to page video, not stream.
                                app.capture_name = crate::model::filename_from_url(&stream);
                                t.url = stream;
                                t.headers.remove("x-ytdlp");
                                t.headers.remove("x-page-stream");
                                app.yt_probe_cap = YtProbeState::default();
                            }
                        });
                    }
                }
            }
            // Write back every edit (Browse/Category/Queue were dropped unless stream kind).
            app.capture_task = Some(t.clone());
            if focus_name {
                ui.ctx()
                    .memory_mut(|m| m.request_focus(egui::Id::new("capture-name")));
                app.capture_just_opened = false;
            }
            if !t.referrer.is_empty() {
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(format!("Referrer: {}", t.referrer))
                        .color(theme::TEXT_FAINT)
                        .small(),
                );
            }
        };
        if self.capture_raise {
            self.capture_raise = false;
            ctx.send_viewport_cmd_to(vp_id("webcraft-capture"), egui::ViewportCommand::Focus);
        } else if self.cap_raise_frames > 0 {
            self.cap_raise_frames -= 1;
            ctx.send_viewport_cmd_to(vp_id("webcraft-capture"), egui::ViewportCommand::Focus);
        }
        if self.cap_raise_frames > 0 {
            crate::platform::raise_window(std::process::id(), "Download file");
        }
        let mut cap_body = Some(cap_body);
        let mut cap_focused = false;
        ctx.show_viewport_immediate(vp_id("webcraft-capture"), builder, |ctx, class| {
            cap_focused = ctx.input(|i| i.focused);
            if !matches!(class, egui::ViewportClass::Embedded) {
                fit_vp(
                    ctx,
                    vp_id("webcraft-capture"),
                    egui::vec2(520.0, 480.0),
                    self.cap_body_h,
                    &mut self.cap_vp_sent,
                );
            }
            if ctx.input(|i| i.viewport().close_requested() || i.key_pressed(egui::Key::Escape)) {
                action = Some("cancel");
            }
            let bb = cap_body.as_mut().unwrap();
            let yt_loading = self.yt_probe_cap.loading;
            let footer = |ui: &mut egui::Ui, action: &mut Option<&'static str>| {
                ui.separator();
                btn_row(ui, |ui| {
                    // Start during probe would silently drop picker intent (Auto).
                    if ui
                        .add_enabled_ui(!yt_loading, |ui| {
                            primary_btn_w(
                                ui,
                                if yt_loading {
                                    "Probing formats…"
                                } else {
                                    "Start download"
                                },
                            )
                        })
                        .inner
                        .clicked()
                    {
                        *action = Some("start");
                    }
                    if ghost_btn_w(ui, "Download later").clicked() {
                        *action = Some("later");
                    }
                    if ghost_btn_w(ui, "Cancel").clicked() {
                        *action = Some("cancel");
                    }
                });
            };
            if matches!(class, egui::ViewportClass::Embedded) {
                let scr = ctx.screen_rect();
                egui::Window::new("Download file")
                    .collapsible(false)
                    .resizable(true)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .default_size([520.0, 480.0])
                    .min_size([360.0, 300.0])
                    .max_size([
                        (scr.width() - 24.0).max(360.0),
                        (scr.height() - 24.0).max(300.0),
                    ])
                    .show(ctx, |ui| {
                        let max_h = (ui.available_height() - FOOTER_H).max(48.0);
                        egui::ScrollArea::vertical()
                            .id_salt("cap-main-embedded")
                            .max_height(max_h)
                            .show(ui, |ui| bb(ui, self));
                        footer(ui, &mut action);
                    });
            } else {
                egui::CentralPanel::default()
                    .frame(dialog_frame())
                    .show(ctx, |ui| {
                        let avail = ui.available_height();
                        let out = egui::ScrollArea::vertical()
                            .id_salt("cap-main")
                            .max_height((avail - FOOTER_H).max(48.0))
                            .show(ui, |ui| bb(ui, self));
                        self.cap_body_h = Some(out.content_size.y + FOOTER_H + 36.0);
                        footer(ui, &mut action);
                    });
            }
        });
        if cap_focused {
            self.cap_raise_frames = 0;
        }
        let Some(act) = action else { return };
        self.cap_raise_frames = 0;
        let mut t = self.capture_task.take().unwrap();
        let name_input = std::mem::take(&mut self.capture_name);
        match act {
            "start" | "later" => {
                // Basename sanitized on confirm ("" -> "download.bin" via sanitizer).
                t.filename = crate::model::sanitize_filename(&name_input);
                // Persist picker choice only when yt-dlp engine still enabled
                // (respects user unchecking it after probing).
                if (t.headers.contains_key("x-ytdlp") || ytdlp::is_video_page(&t.url))
                    && self.yt_probe_cap.url == t.url
                    && self.yt_probe_cap.dump.is_some()
                {
                    let ch = yt_choice_from_state(&self.yt_probe_cap);
                    ch.write_headers(&mut t.headers);
                    // write_headers sets x-ytdlp only for explicit choices; route
                    // probed video pages through yt-dlp even for Auto (probe shown).
                    if !t.headers.contains_key("x-ytdlp") {
                        t.headers.insert("x-ytdlp".into(), "1".into());
                    }
                    // Only extract forces ext here; native audio ext resolved in ytdlp.rs from format.
                    if let Some(want) = &ch.extract {
                        let stem = crate::model::split_stem_ext(&t.filename).0.to_string();
                        t.filename = format!("{stem}.{want}");
                    }
                }
                self.yt_probe_cap = YtProbeState::default();
                // "later" keeps no download running: drop prewarm (single-use
                // take on next start would be stale anyway past 120s).
                if act == "later" && !self.prewarm_cap.url.is_empty() {
                    engine::prewarm_drop(&self.prewarm_cap.url.clone());
                }
                self.prewarm_cap = PrewarmState::default();
                // Category combo in dialog wins (write-back above); don't
                // recompute from filename and silently revert user pick.
                // dest_dir already shown/editable in dialog (Browse wins);
                // funnel set it before dialog, don't override user edit.
                t.status = if act == "start" {
                    TaskStatus::Queued
                } else {
                    TaskStatus::Paused
                };
                let start_id = (act == "start").then(|| t.id.clone());
                {
                    let mut g = crate::lock_unpoison(&self.rt);
                    if t.queue_id.is_empty() || !g.state.queues.iter().any(|q| q.id == t.queue_id) {
                        t.queue_id = "main".into();
                    }
                    g.state.tasks.push(t);
                    g.save();
                }
                // start_task locks rt itself — call outside lock scope.
                if let Some(id) = start_id {
                    self.start_task(&id);
                } else {
                    ctx.request_repaint();
                }
            }
            _ => {
                self.yt_probe_cap = YtProbeState::default();
                if !self.prewarm_cap.url.is_empty() {
                    engine::prewarm_drop(&self.prewarm_cap.url.clone());
                }
                self.prewarm_cap = PrewarmState::default();
                /* Cancel: drop task entirely. */
            }
        }
    }

    /// Torrent files section inside Add dialog. Probe = list_only add on thread;
    /// result lands in add_torrent_slot, consumed here. Selection gates confirm.
    fn ui_add_torrent_picker(&mut self, ui: &mut egui::Ui) {
        // URL edited since probe -> stale, reset (compare trimmed like probe stores).
        if self.add_torrent_url != self.add_url.trim() {
            self.add_torrent_url.clear();
            self.add_torrent_files.clear();
            self.add_torrent_error.clear();
            self.add_torrent_loading = false;
            if let Ok(mut g) = self.add_torrent_slot.lock() {
                *g = None;
            }
        }
        // Consume probe result; drop result tagged for another URL.
        if self.add_torrent_loading {
            let done = self
                .add_torrent_slot
                .try_lock()
                .ok()
                .and_then(|mut g| g.take());
            if let Some((probed_url, res)) = done {
                if probed_url != self.add_url.trim() {
                    // Stale: keep loading state for any live probe; ignore result.
                    if self.add_torrent_url != probed_url {
                        self.add_torrent_loading = false;
                    }
                } else {
                    self.add_torrent_loading = false;
                    match res {
                        Ok(files) => {
                            self.add_torrent_files = files.into_iter().map(|f| (f, true)).collect();
                        }
                        Err(e) => self.add_torrent_error = e,
                    }
                }
            }
        }
        ui.add_space(theme::SP_SM);
        section(ui, "Torrent files");
        if self.add_torrent_loading {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(
                    egui::RichText::new("Fetching torrent metadata…")
                        .color(theme::TEXT_MUTED)
                        .small(),
                );
            });
            return;
        }
        if !self.add_torrent_error.is_empty() {
            ui.label(
                egui::RichText::new(format!("Could not list files: {}", self.add_torrent_error))
                    .color(theme::ERR)
                    .small(),
            );
            ui.horizontal(|ui| {
                if ghost_btn_w(ui, "Retry").clicked() {
                    self.start_torrent_probe();
                }
                ui.label(
                    egui::RichText::new("or start now to download all files")
                        .color(theme::TEXT_FAINT)
                        .small(),
                );
            });
            return;
        }
        if self.add_torrent_files.is_empty() {
            if primary_btn_w(ui, "Load files…").clicked() {
                self.start_torrent_probe();
            }
            return;
        }
        if self.add_torrent_files.len() == 1 {
            let (f, _) = &self.add_torrent_files[0];
            ui.label(
                egui::RichText::new(format!("Single file: {} ({})", f.path, fmt_bytes(f.len)))
                    .color(theme::TEXT_MUTED)
                    .small(),
            );
            return;
        }
        let total_files = self.add_torrent_files.len();
        let sel_count = self.add_torrent_files.iter().filter(|(_, s)| *s).count();
        let sel_bytes: u64 = self
            .add_torrent_files
            .iter()
            .filter(|(_, s)| *s)
            .map(|(f, _)| f.len)
            .sum();
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(format!(
                    "{total_files} files · {sel_count} selected · {}",
                    fmt_bytes(sel_bytes)
                ))
                .color(theme::TEXT_MUTED)
                .small(),
            );
            if ghost_btn_w(ui, "Select all").clicked() {
                for (_, s) in &mut self.add_torrent_files {
                    *s = true;
                }
            }
            if ghost_btn_w(ui, "Select none").clicked() {
                for (_, s) in &mut self.add_torrent_files {
                    *s = false;
                }
            }
        });
        egui::ScrollArea::vertical()
            .id_salt("add-torrent-files")
            .max_height(280.0)
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                for (f, sel) in &mut self.add_torrent_files {
                    ui.horizontal(|ui| {
                        ui.checkbox(sel, "");
                        ui.label(
                            egui::RichText::new(crate::torrent::trunc_path(&f.path, 52))
                                .color(theme::TEXT)
                                .small(),
                        )
                        .on_hover_text(&f.path);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(
                                egui::RichText::new(fmt_bytes(f.len))
                                    .color(theme::TEXT_MUTED)
                                    .small(),
                            );
                        });
                    });
                }
            });
    }

    /// Spawn list_only probe for current add_url on a std thread (update.rs pattern).
    fn start_torrent_probe(&mut self) {
        let url = self.add_url.trim().to_string();
        if url.is_empty() || self.add_torrent_loading {
            return;
        }
        let referrer = self.add_referrer.trim().to_string();
        let cookies = self.add_cookies.trim().to_string();
        self.add_torrent_url = url.clone();
        self.add_torrent_error.clear();
        self.add_torrent_files.clear();
        self.add_torrent_loading = true;
        if let Ok(mut g) = self.add_torrent_slot.lock() {
            *g = None;
        }
        let slot = self.add_torrent_slot.clone();
        let rt = self.tokio_rt.clone();
        let (proxy, settings) = {
            let g = self.rt.lock().unwrap_or_else(|e| e.into_inner());
            (g.state.settings.proxy_url.clone(), g.state.settings.clone())
        };
        std::thread::spawn(move || {
            let task = DownloadTask {
                url: url.clone(),
                referrer,
                cookies,
                ..Default::default()
            };
            let client = crate::engine::build_client(&proxy);
            let res = rt.block_on(crate::torrent::list_torrent_files(client, &task, &settings));
            if let Ok(mut g) = slot.lock() {
                *g = Some((url, res));
            }
        });
    }

    fn ui_add_nzb_picker(&mut self, ui: &mut egui::Ui) {
        // URL edited since probe -> stale, reset (compare trimmed like probe stores).
        if self.add_nzb_url != self.add_url.trim() {
            self.add_nzb_url.clear();
            self.add_nzb = None;
            self.add_nzb_error.clear();
            self.add_nzb_loading = false;
            if let Ok(mut g) = self.add_nzb_slot.lock() {
                *g = None;
            }
        }
        if self.add_nzb_loading {
            let done = self.add_nzb_slot.try_lock().ok().and_then(|mut g| g.take());
            if let Some((probed_url, res)) = done {
                if probed_url != self.add_url.trim() {
                    if self.add_nzb_url != probed_url {
                        self.add_nzb_loading = false;
                    }
                } else {
                    self.add_nzb_loading = false;
                    match res {
                        Ok(nzb) => self.add_nzb = Some(nzb),
                        Err(e) => self.add_nzb_error = e,
                    }
                }
            }
        }
        ui.add_space(theme::SP_SM);
        section(ui, "NZB files");
        if self.add_nzb_loading {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(
                    egui::RichText::new("Reading NZB…")
                        .color(theme::TEXT_MUTED)
                        .small(),
                );
            });
            return;
        }
        if !self.add_nzb_error.is_empty() {
            ui.label(
                egui::RichText::new(format!("Could not read NZB: {}", self.add_nzb_error))
                    .color(theme::ERR)
                    .small(),
            );
            ui.horizontal(|ui| {
                if ghost_btn_w(ui, "Retry").clicked() {
                    self.start_nzb_probe();
                }
                ui.label(
                    egui::RichText::new("or start now and let download report error")
                        .color(theme::TEXT_FAINT)
                        .small(),
                );
            });
            return;
        }
        let Some(nzb) = &self.add_nzb else {
            if primary_btn_w(ui, "Load files…").clicked() {
                self.start_nzb_probe();
            }
            return;
        };
        // ponytail: list-only probe; NZB per-file selection needs model field
        // like torrent_files. Add when download_nzb honors selection.
        ui.label(
            egui::RichText::new(format!(
                "{} files · {} total",
                nzb.files.len(),
                fmt_bytes(nzb.total_bytes)
            ))
            .color(theme::TEXT_MUTED)
            .small(),
        );
        egui::ScrollArea::vertical()
            .id_salt("add-nzb-files")
            .max_height(200.0)
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                for f in &nzb.files {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(crate::torrent::trunc_path(&f.name, 52))
                                .color(theme::TEXT)
                                .small(),
                        )
                        .on_hover_text(&f.name);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(
                                egui::RichText::new(fmt_bytes(f.total_bytes))
                                    .color(theme::TEXT_MUTED)
                                    .small(),
                            );
                        });
                    });
                }
            });
    }

    /// Spawn parse-only NZB probe for current add_url on a std thread.
    fn start_nzb_probe(&mut self) {
        let url = self.add_url.trim().to_string();
        if url.is_empty() || self.add_nzb_loading {
            return;
        }
        let referrer = self.add_referrer.trim().to_string();
        let cookies = self.add_cookies.trim().to_string();
        self.add_nzb_url = url.clone();
        self.add_nzb_error.clear();
        self.add_nzb = None;
        self.add_nzb_loading = true;
        if let Ok(mut g) = self.add_nzb_slot.lock() {
            *g = None;
        }
        let slot = self.add_nzb_slot.clone();
        let rt = self.tokio_rt.clone();
        let proxy = {
            let g = self.rt.lock().unwrap_or_else(|e| e.into_inner());
            g.state.settings.proxy_url.clone()
        };
        std::thread::spawn(move || {
            let task = DownloadTask {
                url: url.clone(),
                referrer,
                cookies,
                ..Default::default()
            };
            let client = crate::engine::build_client(&proxy);
            let res = rt.block_on(crate::nntp::probe_nzb(client, &task));
            if let Ok(mut g) = slot.lock() {
                *g = Some((url, res));
            }
        });
    }

    fn ui_add_dialog(&mut self, ctx: &egui::Context) {
        // Independent native window: no main-rect clip / shrink-wrap weirdness.
        let builder = egui::ViewportBuilder::default()
            .with_title("Add download")
            .with_inner_size([560.0, 680.0])
            .with_min_inner_size([460.0, 420.0])
            .with_max_inner_size([4096.0, 4096.0])
            .with_resizable(true);
        let mut start: Option<bool> = None; // Some(true)=now, Some(false)=later
        let mut cancel = false;
        if self.add_just_opened {
            self.add_vp_sent = None;
            self.add_body_h = None;
            ctx.send_viewport_cmd_to(vp_id("webcraft-add"), egui::ViewportCommand::Focus);
        } else if self.add_raise_frames > 0 {
            self.add_raise_frames -= 1;
            ctx.send_viewport_cmd_to(vp_id("webcraft-add"), egui::ViewportCommand::Focus);
        }
        if self.add_raise_frames > 0 {
            crate::platform::raise_window(std::process::id(), "Add download");
        }
        let mut add_focused = false;
        ctx.show_viewport_immediate(vp_id("webcraft-add"), builder, |ctx, class| {
            add_focused = ctx.input(|i| i.focused);
            if !matches!(class, egui::ViewportClass::Embedded) {
                fit_vp(ctx, vp_id("webcraft-add"), egui::vec2(560.0, 680.0), self.add_body_h, &mut self.add_vp_sent);
            }
            if ctx.input(|i| i.viewport().close_requested()) {
                cancel = true;
            }
            if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                cancel = true;
            }
            let body = |ui: &mut egui::Ui, app: &mut Self, start: &mut Option<bool>, cancel: &mut bool| {
                // Invalid header row blocks Add like other dialog gates.
                let hdr_error = app
                    .add_headers
                    .iter()
                    .find_map(|(n, v)| header_row_error(n, v));
                let out = egui::ScrollArea::vertical()
                    .id_salt("add-main")
                    .auto_shrink([false, false])
                    .max_height((ui.available_height() - FOOTER_H).max(48.0))
                    .show(ui, |ui| {
                    section(ui, "Link");
                    ui.horizontal(|ui| {
                        let w = (ui.available_width() - 172.0).max(120.0);
                        let resp = ui.add_sized(
                            [w, theme::BTN_H],
                            egui::TextEdit::singleline(&mut app.add_url)
                                .id(egui::Id::new("add-url"))
                                .hint_text("https://… or magnet:?xt=urn:btih:…"),
                        );
                        let vp_focused = ui.ctx().input(|i| i.focused);
                        if app.add_just_opened || (vp_focused && !app.add_focus_done) {
                            resp.request_focus();
                            if vp_focused {
                                app.add_focus_done = true;
                            }
                            app.add_just_opened = false;
                        }
                        // Same gates as Start button: no Enter bypass while
                        // probing or with zero torrent files selected.
                        let enter_blocked = hdr_error.is_some() || app.add_torrent_loading || app.yt_probe_add.loading
                            || (crate::torrent::is_torrent_url(&app.add_url)
                                && app.add_torrent_files.len() > 1
                                && app.add_torrent_files.iter().all(|(_, s)| !s));
                        if resp.lost_focus()
                            && ui.input(|i| i.key_pressed(egui::Key::Enter))
                            && !enter_blocked
                        {
                            *start = Some(app.add_start_now);
                        }
                        if app.add_url.contains('<') {
                            app.add_url = extract_pasted_url(&app.add_url.clone());
                        }
                        if ghost_btn_w(ui, "Paste").clicked() {
                            if let Ok(mut cb) = arboard::Clipboard::new() {
                                if let Ok(text) = cb.get_text() {
                                    app.add_url = extract_pasted_url(&text);
                                }
                            }
                        }
                        // Local .torrent/.nzb file: backend dispatches on path
                        // in task.url (torrent build_add / nntp fetch_nzb_text).
                        if ghost_btn_w(ui, "Browse…").clicked() {
                            if let Some(p) = rfd::FileDialog::new()
                                .add_filter("Torrent / NZB", &["torrent", "nzb"])
                                .add_filter("Torrent", &["torrent"])
                                .add_filter("NZB", &["nzb"])
                                .pick_file()
                            {
                                app.add_url = p.to_string_lossy().into_owned();
                            }
                        }
                    });
                    ui.label(
                        egui::RichText::new("Supports HTTP(S), FTP/SFTP, NNTP/NZB, torrents, DASH/HLS manifests, metalink.")
                            .color(theme::TEXT_FAINT)
                            .small(),
                    );
                    // Torrent file picker (magnet / .torrent): probe file list first.
                    // Debrid-for-magnets route skips picker: provider file
                    // list != librqbit indices; fan-out uses all files.
                    let debrid_magnet = crate::debrid::is_magnet(&app.add_url) && {
                        let g = crate::lock_unpoison(&app.rt);
                        crate::debrid::any_for_magnets(&g.state.settings)
                    };
                    if debrid_magnet {
                        ui.label(egui::RichText::new("Debrid enabled for magnets: Add resolves via debrid and creates one task per file (local torrent picker skipped). Disable debrid magnets in Options to use local torrent.").color(theme::TEXT_FAINT).small());
                    } else if crate::torrent::is_torrent_url(&app.add_url) {
                        app.ui_add_torrent_picker(ui);
                    }
                    if crate::stream::is_nzb_url_str(&app.add_url) {
                        app.ui_add_nzb_picker(ui);
                    }
                    ui.add_space(theme::SP_SM);
                    section(ui, "Destination");
                    form_grid("add-grid").show(ui, |ui| {
                        form_label(ui, "Queue");
                        {
                            let queues: Vec<(String, String)> = {
                                let g = crate::lock_unpoison(&app.rt);
                                g.state.queues.iter().map(|q| (q.id.clone(), q.name.clone())).collect()
                            };
                            let cur = queues
                                .iter()
                                .find(|(id, _)| *id == app.add_queue)
                                .map(|(_, n)| n.clone())
                                .unwrap_or_else(|| app.add_queue.clone());
                            egui::ComboBox::from_id_salt("add-queue")
                                .selected_text(cur)
                                .width(ui.available_width())
                                .show_ui(ui, |ui| {
                                    for (id, name) in &queues {
                                        ui.selectable_value(&mut app.add_queue, id.clone(), name);
                                    }
                                });
                        }
                        ui.end_row();
                        form_label(ui, "Save to");
                        {
                            let dir = crate::lock_unpoison(&app.rt).state.settings.download_dir.display().to_string();
                            ui.add(
                                egui::Label::new(egui::RichText::new(&dir).color(theme::TEXT_FAINT).monospace().small()).truncate(),
                            )
                            .on_hover_text(dir);
                        }
                        ui.end_row();
                        form_label(ui, "Referrer");
                        ui.add(fill_edit(&mut app.add_referrer));
                        ui.end_row();
                        form_label(ui, "Cookies");
                        ui.add(fill_edit(&mut app.add_cookies));
                        ui.end_row();
                    });
                    if yt_probe_eligible(app.add_url.trim()) {
                        ui.add_space(theme::SP_SM);
                        let (u, r, c) = (app.add_url.trim().to_string(), app.add_referrer.clone(), app.add_cookies.clone());
                        let rt = app.tokio_rt.clone();
                        ui_yt_probe_section(ui, &mut app.yt_probe_add, "add", &u, &r, &c, "", &rt);
                    } else if !app.add_url.trim().is_empty() {
                        // HTTP prewarm: stable probe-able URL spawns background
                        // probe+plan; confirm never waits for it.
                        let t = DownloadTask {
                            url: normalize_webdav(app.add_url.trim()),
                            referrer: app.add_referrer.clone(),
                            cookies: app.add_cookies.clone(),
                            username: app.add_user.clone(),
                            password: app.add_pass.clone(),
                            ..Default::default()
                        };
                        // Changed URL since last frame: stale prewarm dropped.
                        if !app.prewarm_add.url.is_empty() && app.prewarm_add.url != t.url {
                            engine::prewarm_drop(&app.prewarm_add.url.clone());
                            app.prewarm_add = PrewarmState::default();
                        }
                        if prewarm_eligible(&t) {
                            prewarm_poll(&mut app.prewarm_add, &t.url);
                            let rt = app.tokio_rt.clone();
                            let conns = crate::lock_unpoison(&app.rt).state.settings.max_connections;
                            spawn_prewarm(&mut app.prewarm_add, &rt, &t, conns);
                            if !app.prewarm_add.text.is_empty() {
                                ui.label(egui::RichText::new(&app.prewarm_add.text).small().color(theme::TEXT_MUTED));
                            } else if app.prewarm_add.loading {
                                ui.label(egui::RichText::new("Probing…").small().color(theme::TEXT_MUTED));
                            }
                        }
                    }
                    ui.add_space(theme::SP_SM);
                    section(ui, "Transfer");
                    form_grid("add-transfer").show(ui, |ui| {
                        form_label(ui, "Mirrors");
                        ui.add(
                            egui::TextEdit::singleline(&mut app.add_mirrors)
                                .hint_text("mirror URLs, comma separated (failover)")
                                .desired_width(ui.available_width()),
                        );
                        ui.end_row();
                        form_label(ui, "Description");
                        ui.add(fill_edit(&mut app.add_desc));
                        ui.end_row();
                        form_label(ui, "When");
                        ui.horizontal(|ui| {
                            ui.radio_value(&mut app.add_start_now, true, "Start now");
                            ui.radio_value(&mut app.add_start_now, false, "Later");
                        });
                        ui.end_row();
                    });
                    ui.add_space(theme::SP_SM);
                    egui::CollapsingHeader::new("Headers (advanced) — HTTP/HTTPS only")
                        .id_salt("add-headers-hdr")
                        .default_open(false)
                        .show(ui, |ui| {
                            ui.label(egui::RichText::new("Sent only on HTTP/HTTPS downloads. Ignored for FTP, SFTP and torrents.").color(theme::TEXT_FAINT).small());
                            let mut rm: Option<usize> = None;
                            for (i, (hn, hv)) in app.add_headers.iter_mut().enumerate() {
                                ui.horizontal(|ui| {
                                    ui.add(egui::TextEdit::singleline(hn).hint_text("Name").desired_width(150.0));
                                    let is_auth = hn.trim().eq_ignore_ascii_case("authorization");
                                    ui.add(egui::TextEdit::singleline(hv).hint_text(if is_auth { "Bearer <token>" } else { "Value" }).password(is_auth).desired_width(220.0));
                                    if icon_btn(ui, Icon::Delete, "", false, 28.0).on_hover_text("Remove header").clicked() { rm = Some(i); }
                                });
                            }
                            if let Some(i) = rm { app.add_headers.remove(i); }
                            if let Some(e) = &hdr_error {
                                ui.label(egui::RichText::new(e.clone()).color(theme::ERR).small());
                            }
                            ui.horizontal_wrapped(|ui| {
                                if ui.small_button("+ Row").clicked() { app.add_headers.push((String::new(), String::new())); }
                                for (pn, pv) in [("User-Agent", ""), ("Referer", ""), ("Authorization", "Bearer "), ("Accept", "")] {
                                    if ui.small_button(pn).clicked() { app.add_headers.push((pn.into(), pv.into())); }
                                }
                            });
                            ui.label(egui::RichText::new("Authorization preset: replace trailing text with your token — stored as \"Bearer <token>\". Rows editable/removable.").color(theme::TEXT_FAINT).small());
                        });
                    let auth_resp = egui::CollapsingHeader::new("Auth (advanced)")
                        .id_salt("add-auth-hdr")
                        .default_open(false)
                        .show(ui, |ui| {
                            form_grid("add-auth").show(ui, |ui| {
                                form_label(ui, "Login");
                                ui.add(fill_edit(&mut app.add_user));
                                ui.end_row();
                                form_label(ui, "Password");
                                ui.add(
                                    egui::TextEdit::singleline(&mut app.add_pass)
                                        .password(true)
                                        .desired_width(ui.available_width()),
                                );
                                ui.end_row();
                                form_label(ui, "SSH key");
                                ui.add(
                                    egui::TextEdit::singleline(&mut app.add_key)
                                        .hint_text("path to private key, empty = ~/.ssh/id_*")
                                        .desired_width(ui.available_width()),
                                );
                                ui.end_row();
                            });
                        });
                    auth_resp.body_response.is_none()
                });
                // content_size clamps to viewport when scrolled: floor = base size,
                // so an over-cap measurement only ever grows to the 85% cap.
                let mut content_h = out.content_size.y;
                if out.inner {
                    content_h += 96.0;
                }
                app.add_body_h = Some(content_h + FOOTER_H + 36.0);
                ui.separator();
                // Zero-selected torrent files can never finish: gate confirm.
                let torrent_blocked = crate::torrent::is_torrent_url(&app.add_url)
                    && app.add_torrent_files.len() > 1
                    && app.add_torrent_files.iter().all(|(_, s)| !s);
                btn_row(ui, |ui| {
                    if torrent_blocked {
                        ui.label(
                            egui::RichText::new("Select at least one file")
                                .color(theme::ERR)
                                .small(),
                        );
                    }
                    if ui
                        .add_enabled_ui(
                            !app.add_url.trim().is_empty() && !app.add_torrent_loading && !app.yt_probe_add.loading && !torrent_blocked && hdr_error.is_none(),
                            |ui| primary_btn_w(ui, "Start download"),
                        )
                        .inner
                        .clicked()
                    {
                        *start = Some(app.add_start_now);
                    }
                    if ghost_btn_w(ui, "Cancel").clicked() {
                        *cancel = true;
                    }
                });
            };
            let mut body = Some(body);
            let bb = body.as_mut().unwrap();
            if matches!(class, egui::ViewportClass::Embedded) {
                let scr = ctx.screen_rect();
                egui::Window::new("Add download")
                    .collapsible(false)
                    .resizable(true)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .default_size([560.0, 680.0])
                    .min_size([380.0, 320.0])
                    .max_size([(scr.width() - 24.0).max(380.0), (scr.height() - 24.0).max(320.0)])
                    .show(ctx, |ui| bb(ui, self, &mut start, &mut cancel));
            } else {
                egui::CentralPanel::default().frame(dialog_frame()).show(ctx, |ui| {
                    bb(ui, self, &mut start, &mut cancel)
                });
            }
        });
        if add_focused {
            self.add_raise_frames = 0;
        }
        if cancel {
            self.show_add = false;
            self.add_raise_frames = 0;
            return;
        }
        if let Some(start_now) = start {
            let url = normalize_webdav(&self.add_url);
            if !url.is_empty() {
                let (referrer, cookies, queue) = (
                    self.add_referrer.clone(),
                    self.add_cookies.clone(),
                    if self.add_queue.is_empty() {
                        "main".into()
                    } else {
                        self.add_queue.clone()
                    },
                );
                let (user, pass, key, mirrors, desc) = (
                    self.add_user.clone(),
                    self.add_pass.clone(),
                    self.add_key.clone(),
                    self.add_mirrors
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>(),
                    self.add_desc.clone(),
                );
                // Valid non-empty rows only; invalid blocked at dialog gate.
                let custom_headers: std::collections::BTreeMap<String, String> = self
                    .add_headers
                    .iter()
                    .map(|(n, v)| (n.trim().to_string(), v.trim().to_string()))
                    .filter(|(n, v)| {
                        !(n.is_empty() && v.is_empty()) && header_row_error(n, v).is_none()
                    })
                    .collect();
                // Debrid magnet fan-out: resolve async, poll_debrid_add
                // creates one task per file. Picker selection ignored here
                // (provider file names != librqbit indices).
                if crate::debrid::is_magnet(&url) && self.pending_debrid_add.is_none() {
                    let (settings, proxy) = {
                        let g = crate::lock_unpoison(&self.rt);
                        (g.state.settings.clone(), g.state.settings.proxy_url.clone())
                    };
                    if crate::debrid::any_for_magnets(&settings) {
                        if let Ok(mut s) = self.debrid_add_slot.try_lock() {
                            *s = None;
                        }
                        self.pending_debrid_add = Some(PendingDebridAdd {
                            url: url.clone(),
                            referrer: referrer.clone(),
                            cookies: cookies.clone(),
                            queue_id: queue.clone(),
                            start_now,
                        });
                        let slot = self.debrid_add_slot.clone();
                        let rt2 = self.tokio_rt.clone();
                        let ctx2 = ctx.clone();
                        let magnet = url.clone();
                        std::thread::spawn(move || {
                            let client = engine::build_client(&proxy);
                            let res = rt2
                                .block_on(crate::debrid::resolve_magnet(client, &settings, &magnet))
                                .map(|(_p, files)| files);
                            if let Ok(mut g) = slot.lock() {
                                *g = Some(res);
                            }
                            ctx2.request_repaint();
                        });
                        self.show_add = false;
                        self.add_raise_frames = 0;
                        return;
                    }
                }
                let id = {
                    let mut g = crate::lock_unpoison(&self.rt);
                    let id = g.add_url(&url, &queue, &referrer, &cookies);
                    // NZB uses separate NNTP connections knob, not HTTP max_connections.
                    let segs = if crate::stream::is_nzb_url_str(&url) {
                        g.state.settings.nntp_connections
                    } else {
                        g.state.settings.max_connections
                    };
                    if let Some(t) = g.state.tasks.iter_mut().find(|t| t.id == id) {
                        t.segments = segs;
                        t.username = user;
                        t.password = pass;
                        t.ssh_key_path = key;
                        t.mirrors = mirrors;
                        t.description = desc;
                        t.headers.extend(custom_headers.clone());
                        // Torrent picker: empty vec = all files; validate vs probe.
                        // Only for torrent tasks; stale picker state must not leak.
                        if crate::torrent::is_torrent_task(t) && !self.add_torrent_files.is_empty()
                        {
                            let sel: Vec<usize> = self
                                .add_torrent_files
                                .iter()
                                .filter(|(_, s)| *s)
                                .map(|(f, _)| f.index)
                                .collect();
                            let all = sel.len() == self.add_torrent_files.len();
                            t.torrent_files = if all {
                                Vec::new()
                            } else {
                                // Buttons gate zero/invalid; fallback keeps prior
                                // (never silently invert to all-files).
                                crate::torrent::validate_selection(
                                    &sel,
                                    self.add_torrent_files.len(),
                                )
                                .unwrap_or_else(|_| t.torrent_files.clone())
                            };
                        }
                        // NZB probe already parsed: adopt real output name
                        // now (download re-derives same). Unprobed NZB tasks
                        // adopt at download start instead.
                        if crate::stream::is_nzb_url_str(&t.url) {
                            if let Some(nzb) = &self.add_nzb {
                                if self.add_nzb_url == url.trim() {
                                    t.filename = crate::nntp::output_name(nzb, &t.url);
                                    t.category =
                                        crate::model::category_for(&t.filename).to_string();
                                }
                            }
                        }
                        // yt-dlp picker choice for video-page URLs.
                        if self.yt_probe_add.url == url.trim() && self.yt_probe_add.dump.is_some() {
                            let ch = yt_choice_from_state(&self.yt_probe_add);
                            ch.write_headers(&mut t.headers);
                            // write_headers adds x-ytdlp only for explicit choices; route
                            // generic pages/playlists through yt-dlp too (probe matched).
                            t.headers.insert("x-ytdlp".into(), "1".into());
                            // Probe title as filename (mirror capture): only over virgin
                            // seed (no dot), user edits / URL-derived names untouched.
                            if !t.filename.contains('.') {
                                if let Some(d) = &self.yt_probe_add.dump {
                                    if !d.title.trim().is_empty() {
                                        t.filename =
                                            crate::model::sanitize_filename(d.title.trim());
                                    }
                                }
                            }
                            if let Some(want) = &ch.extract {
                                let stem = crate::model::split_stem_ext(&t.filename).0.to_string();
                                t.filename = format!("{stem}.{want}");
                            }
                        }
                        if !start_now {
                            t.status = TaskStatus::Paused;
                        }
                    }
                    // start_task saves after flipping to Downloading; double
                    // save skipped only on start_now path.
                    if !start_now {
                        g.save();
                    }
                    id
                };
                if start_now {
                    self.start_task(&id);
                } else {
                    ctx.request_repaint();
                }
                // Clear draft incl. password: no stale secrets next open.
                self.add_url.clear();
                self.add_torrent_url.clear();
                self.add_torrent_files.clear();
                self.add_torrent_error.clear();
                self.add_nzb_url.clear();
                self.add_nzb = None;
                self.add_nzb_error.clear();
                self.add_nzb_loading = false;
                self.add_referrer.clear();
                self.add_cookies.clear();
                self.add_user.clear();
                self.add_pass.clear();
                self.add_key.clear();
                self.add_headers.clear();
                self.add_mirrors.clear();
                self.add_desc.clear();
                self.yt_probe_add = YtProbeState::default();
                // Cancel: drop prewarm entry (abandoned cost = 1 range request).
                if !self.prewarm_add.url.is_empty() {
                    engine::prewarm_drop(&self.prewarm_add.url.clone());
                }
                self.prewarm_add = PrewarmState::default();
                self.show_add = false;
                self.add_raise_frames = 0;
            }
        }
    }

    fn ui_progress_dialog(&mut self, ctx: &egui::Context, id: &str) {
        // New task in same viewport slot: drop stale measured height.
        if self.prog_task.as_deref() != Some(id) {
            self.prog_task = Some(id.to_owned());
            self.prog_body_h = None;
            self.prog_vp_sent = None;
        }
        let task = {
            let g = crate::lock_unpoison(&self.rt);
            g.state.tasks.iter().find(|t| t.id == id).cloned()
        };
        let Some(t) = task else {
            self.show_progress_for = None;
            return;
        };
        let kind = crate::stream::proto_kind(&t);
        let mut close = false;
        let mut toggle_pause = false;
        let mut copy_link = false;
        // Independent native window: content outgrows main window rect.
        let builder = egui::ViewportBuilder::default()
            .with_title(format!("Progress - {}", t.filename))
            .with_inner_size([640.0, 720.0])
            .with_min_inner_size([440.0, 380.0])
            .with_max_inner_size([4096.0, 4096.0])
            .with_resizable(true);
        if self.progress_raise {
            self.progress_raise = false;
            self.prog_raise_frames = 90;
            self.prog_raise_title = format!("Progress - {}", t.filename);
            ctx.send_viewport_cmd_to(vp_id("webcraft-progress"), egui::ViewportCommand::Focus);
        } else if self.prog_raise_frames > 0 {
            self.prog_raise_frames -= 1;
            ctx.send_viewport_cmd_to(vp_id("webcraft-progress"), egui::ViewportCommand::Focus);
        }
        if self.prog_raise_frames > 0 {
            crate::platform::raise_window(std::process::id(), &self.prog_raise_title.clone());
        }
        let task_rate = self.task_rate(&t.id);
        let eta_txt = match t.total {
            Some(total)
                if t.status == TaskStatus::Downloading
                    && task_rate > 1.0
                    && total > t.downloaded =>
            {
                eta((total - t.downloaded) as f64 / task_rate)
            }
            _ => "—".into(),
        };
        let samples: Vec<(f64, u64)> = self
            .task_hist
            .get(&t.id)
            .map(|d| d.iter().copied().collect())
            .unwrap_or_default();
        let series = task_speed_series(&samples);
        let peak = series.iter().copied().fold(task_rate, f64::max);
        let frac_opt = display_fraction(&t, self.seg_info.get(&t.id));
        let frac = frac_opt.unwrap_or(0.0);
        let seg_view = self.seg_info.get(&t.id).cloned();
        // Viewport closure needs whole self (bb(ui, self)); measured-fit values
        // ride in as locals to avoid disjoint field captures conflicting.
        let prog_h_in = self.prog_body_h;
        let mut prog_sent = self.prog_vp_sent;
        let body = |ui: &mut egui::Ui, app: &mut Self| -> (bool, bool, bool) {
            let (mut b_close, mut b_toggle, mut b_copy) = (false, false, false);
            // HEADER: icon + filename + chips; URL truncated, hover full.
            ui.horizontal(|ui| {
                let (irect, _) =
                    ui.allocate_exact_size(egui::vec2(28.0, 28.0), egui::Sense::hover());
                ui.painter()
                    .rect_filled(irect, theme::R_CTRL, theme::SURFACE_RAISED);
                icon(
                    ui.painter(),
                    &cat_icon(&t.filename),
                    irect.shrink(6.0),
                    theme::TEXT_MUTED,
                );
                ui.vertical(|ui| {
                    ui.add(
                        egui::Label::new(egui::RichText::new(&t.filename).strong().size(13.0))
                            .truncate(),
                    )
                    .on_hover_text(&t.filename);
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(&t.url).color(theme::TEXT_FAINT).small(),
                        )
                        .truncate(),
                    )
                    .on_hover_text(&t.url);
                });
            });
            ui.horizontal(|ui| {
                // Raw STEEL as text fails contrast: use STEEL_TEXT for Downloading.
                let sc = if t.status == TaskStatus::Downloading {
                    theme::STEEL_TEXT
                } else {
                    theme::status_color(t.status)
                };
                let status_lbl = if t.status == TaskStatus::Downloading
                    && (t.headers.contains_key("x-ytdlp") || ytdlp::is_video_page(&t.url))
                {
                    crate::engine::phase_name(app.phase_map.get(&t.id).copied().unwrap_or(0))
                        .to_string()
                } else {
                    format!("{:?}", t.status)
                };
                theme::badge(ui, &status_lbl, sc);
                theme::badge(ui, kind.label(), theme::proto_color(kind));
                if !t.debrid_info.is_empty() {
                    ui.label(
                        egui::RichText::new(&t.debrid_info)
                            .color(theme::TEAL_500)
                            .small(),
                    )
                    .on_hover_text(&t.debrid_info);
                }
                theme::badge(
                    ui,
                    if t.category.is_empty() {
                        crate::model::category_for(&t.filename)
                    } else {
                        &t.category
                    },
                    theme::category_color(if t.category.is_empty() {
                        crate::model::category_for(&t.filename)
                    } else {
                        &t.category
                    }),
                );
            });
            ui.add_space(theme::SP_SM);
            // PROGRESS: % + finite bar + 4 stat tiles + collapsible graph.
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(if frac_opt.is_some() {
                        format!("{:.1}%", frac * 100.0)
                    } else if t.status == TaskStatus::Downloading {
                        "receiving".into()
                    } else {
                        "—".into()
                    })
                    .strong()
                    .size(18.0),
                );
                ui.label(
                    egui::RichText::new(format!("{:?}", t.status))
                        .color(theme::TEXT_FAINT)
                        .small(),
                );
            });
            let bar_w = ui.available_width();
            ui.add(
                egui::ProgressBar::new(frac)
                    .desired_width(bar_w)
                    .desired_height(8.0)
                    .fill(theme::PRIMARY),
            );
            ui.add_space(theme::SP_XS);
            ui.columns(4, |c| {
                let ui = &mut c[0];
                stat_tile(
                    ui,
                    "RATE",
                    if t.status == TaskStatus::Downloading {
                        fmt_rate(task_rate as f32)
                    } else {
                        "—".into()
                    },
                );
                stat_tile(&mut c[1], "ETA", eta_txt.clone());
                stat_tile(
                    &mut c[2],
                    "DOWNLOADED",
                    format!(
                        "{} / {}",
                        fmt_bytes(t.downloaded),
                        t.total.map(fmt_bytes).unwrap_or_else(|| "—".into())
                    ),
                );
                stat_tile(
                    &mut c[3],
                    "PEAK",
                    if samples.is_empty() {
                        "—".into()
                    } else {
                        fmt_rate(peak as f32)
                    },
                );
            });
            let graph_resp = egui::CollapsingHeader::new("Speed graph")
                .default_open(t.status == TaskStatus::Downloading && series.len() > 1)
                .show(ui, |ui| {
                    let (rect, _) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), 40.0),
                        egui::Sense::hover(),
                    );
                    ui.painter().rect_filled(rect, theme::R_CHIP, theme::G_950);
                    ui.painter().rect_stroke(
                        rect,
                        theme::R_CHIP,
                        egui::Stroke::new(1.0_f32, theme::BORDER),
                    );
                    if series.len() > 1 {
                        let peak = peak.max(1.0);
                        let n = series.len();
                        let pts: Vec<egui::Pos2> = series
                            .iter()
                            .enumerate()
                            .map(|(i, v)| {
                                egui::pos2(
                                    rect.left() + rect.width() * (i as f32 / (n - 1).max(1) as f32),
                                    rect.bottom()
                                        - 2.0
                                        - (*v as f32 / peak as f32) * (rect.height() - 5.0),
                                )
                            })
                            .collect();
                        for w in pts.windows(2) {
                            ui.painter().line_segment(
                                [w[0], w[1]],
                                egui::Stroke::new(1.5_f32, theme::TEAL_500),
                            );
                        }
                        if let Some(last) = pts.last() {
                            ui.painter().circle_filled(*last, 2.0, theme::TEAL_500);
                        }
                    } else {
                        ui.painter().text(
                            rect.center(),
                            egui::Align2::CENTER_CENTER,
                            "—",
                            egui::FontId::proportional(10.0),
                            theme::TEXT_FAINT,
                        );
                    }
                });
            if !t.error.is_empty() {
                ui.add_space(theme::SP_XS);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Error").color(theme::ERR).strong());
                    ui.add(
                        egui::Label::new(egui::RichText::new(&t.error).color(theme::ERR)).wrap(),
                    );
                });
            }
            // Scrollable middle: sections never widen window; footer pinned below.
            let prog_scroll = egui::ScrollArea::vertical()
                .id_salt("prog-main")
                .auto_shrink([false, false])
                .max_height((ui.available_height() - FOOTER_H).max(48.0))
                .show(ui, |ui| {
                    section(ui, "Connection");
                    {
                        use crate::stream::ProtoKind as K;
                        let mut pairs: Vec<(&str, String)> = vec![
                            ("Type", kind.full_name().to_string()),
                            (
                                "Queue",
                                if t.queue_id.is_empty() {
                                    "main".into()
                                } else {
                                    t.queue_id.clone()
                                },
                            ),
                        ];
                        match kind {
                            K::Nntp => {
                                let st = {
                                    let g = crate::lock_unpoison(&app.rt);
                                    g.state.settings.clone()
                                };
                                pairs.push((
                                    "Server",
                                    format!(
                                        "{}:{} {}",
                                        st.nntp_host,
                                        st.nntp_port,
                                        if st.nntp_tls { "(TLS)" } else { "(plain)" }
                                    ),
                                ));
                            }
                            K::Ftp | K::Ftps | K::Sftp => {
                                if let Ok(u) = url::Url::parse(&t.url) {
                                    let host = u.host_str().unwrap_or("");
                                    pairs.push((
                                        "Server",
                                        match u.port() {
                                            Some(p) => format!("{host}:{p}"),
                                            None => host.to_string(),
                                        },
                                    ));
                                    pairs.push((
                                        "Security",
                                        match kind {
                                            K::Sftp => {
                                                if t.ssh_key_path.is_empty() {
                                                    "Password auth".into()
                                                } else {
                                                    format!("Key: {}", t.ssh_key_path)
                                                }
                                            }
                                            K::Ftps
                                                if t.url
                                                    .to_ascii_lowercase()
                                                    .starts_with("ftpsi://") =>
                                            {
                                                "FTPS implicit".into()
                                            }
                                            K::Ftps => "FTPS (AUTH TLS)".into(),
                                            _ => "Plain".into(),
                                        },
                                    ));
                                }
                            }
                            _ => {}
                        }
                        if !matches!(kind, K::Torrent | K::Ftp | K::Ftps | K::Sftp | K::File | K::YtDlp) {
                            pairs.push((
                                "Segments",
                                seg_view
                                    .as_ref()
                                    .map(|v| v.label())
                                    .unwrap_or_else(|| format!("{} connections", t.segments)),
                            ));
                        }
                        if !matches!(kind, K::Nntp | K::YtDlp) {
                            pairs.push((
                                "Resume",
                                if resume_supported(&t) { "Yes" } else { "No" }.into(),
                            ));
                        }
                        // Equal halves, fixed key col (4-col Grid squeezed values to "HTT…").
                        for chunk in pairs.chunks(2) {
                            ui.columns(2, |c| {
                                for (i, (k, v)) in chunk.iter().enumerate() {
                                    c[i].horizontal(|ui| {
                                        ui.allocate_ui_with_layout(
                                            egui::vec2(64.0, 16.0),
                                            egui::Layout::left_to_right(egui::Align::Center),
                                            |ui| {
                                                ui.label(
                                                    egui::RichText::new(*k)
                                                        .color(theme::TEXT_FAINT)
                                                        .small(),
                                                );
                                            },
                                        );
                                        ui.add(
                                            egui::Label::new(egui::RichText::new(v).small())
                                                .truncate(),
                                        )
                                        .on_hover_text(v);
                                    });
                                }
                            });
                        }
                    }
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("Save to").color(theme::TEXT_FAINT).small());
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(t.dest_dir.display().to_string()).small(),
                            )
                            .truncate(),
                        )
                        .on_hover_text(t.dest_dir.display().to_string());
                    });
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("Limit").color(theme::TEXT_FAINT).small());
                        let mut lim = t.speed_limit_kbps;
                        if ui
                            .add_sized(
                                [DRAG_W, theme::BTN_H],
                                drag(&mut lim).range(0..=1_000_000),
                            )
                            .changed()
                        {
                            let mut g = crate::lock_unpoison(&app.rt);
                            if let Some(x) = g.state.tasks.iter_mut().find(|x| x.id == t.id) {
                                x.speed_limit_kbps = lim;
                            }
                            if let Some(c) = g.controls.get(&t.id) {
                                let n = g.controls.len();
                                let eff = effective_limit_kbps(
                                    g.state.settings.speed_limit_kbps,
                                    n,
                                    lim,
                                );
                                c.limit_kbps.store(eff, Ordering::Relaxed);
                            }
                            g.save();
                        }
                        ui.label(
                            egui::RichText::new("KB/s · 0 = off")
                                .color(theme::TEXT_FAINT)
                                .small(),
                        );
                    });
                    // SWARM (torrent snapshot; hidden when absent)
                    if kind == crate::stream::ProtoKind::Torrent {
                        if let Some(snap) = seg_view.as_ref().and_then(|v| v.torrent.clone()) {
                            ui.add_space(theme::SP_SM);
                            section(ui, "Swarm");
                            ui.columns(3, |c| {
                                                    stat_tile(
                                        &mut c[0],
                                        "PEERS",
                                        if snap.live {
                                            format!("{}", snap.peers_live)
                                        } else {
                                            "—".into()
                                        },
                                    );
                                    stat_tile(
                                        &mut c[1],
                                        "PIECES",
                                        if snap.pieces_total > 0 {
                                            format!("{} / {}", snap.pieces_done, snap.pieces_total)
                                        } else {
                                            "—".into()
                                        },
                                    );
                                    stat_tile(
                                        &mut c[2],
                                        "UPLOADED",
                                        format!("{} (run)", fmt_bytes(snap.uploaded)),
                                    );
                                });
                            if snap.live {
                                ui.label(
                                    egui::RichText::new(format!(
                                        "{} connecting · {} seen",
                                        snap.peers_connecting, snap.peers_seen
                                    ))
                                    .color(theme::TEXT_FAINT)
                                    .small(),
                                );
                            }
                            ui.horizontal(|ui| {
                                ui.label(
                                    egui::RichText::new("Info hash")
                                        .color(theme::TEXT_FAINT)
                                        .small(),
                                );
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(&snap.info_hash).small().monospace(),
                                    )
                                    .truncate(),
                                )
                                .on_hover_text(&snap.info_hash);
                            });
                            ui.label(
                                egui::RichText::new(format!("Trackers ({})", snap.trackers.len()))
                                    .color(theme::TEXT_FAINT)
                                    .small(),
                            );
                            egui::ScrollArea::vertical()
                                .id_salt("prog-trackers")
                                .max_height(88.0)
                                .show(ui, |ui| {
                                    if snap.trackers.is_empty() {
                                        ui.label(
                                            egui::RichText::new("— (DHT/PEX only or none yet)")
                                                .color(theme::TEXT_FAINT)
                                                .small(),
                                        );
                                    }
                                    for tr in &snap.trackers {
                                        ui.add(
                                            egui::Label::new(
                                                egui::RichText::new(tr)
                                                    .color(theme::TEXT_FAINT)
                                                    .small(),
                                            )
                                            .truncate(),
                                        )
                                        .on_hover_text(tr);
                                    }
                                });
                        }
                    }
                    // PARTS / FILES
                    if let Some(view) = &seg_view {
                        let suppress_single = matches!(
                            view.kind,
                            crate::stream::ProtoKind::Torrent
                                | crate::stream::ProtoKind::Ftp
                                | crate::stream::ProtoKind::Ftps
                                | crate::stream::ProtoKind::Sftp
                                | crate::stream::ProtoKind::File
                        ) && view.parts.len() <= 1;
                        if !view.parts.is_empty() && !suppress_single {
                            ui.add_space(theme::SP_SM);
                            section(
                                ui,
                                if matches!(
                                    view.kind,
                                    crate::stream::ProtoKind::Nntp
                                        | crate::stream::ProtoKind::Torrent
                                ) {
                                    "Files"
                                } else {
                                    "Parts"
                                },
                            );
                            if view.manifest {
                                let (d, tot) = view.parts.first().copied().unwrap_or((0, 0));
                                ui.label(
                                    egui::RichText::new(format!(
                                        "{d} of {tot} segments downloaded"
                                    ))
                                    .color(theme::TEXT_MUTED)
                                    .small(),
                                );
                            } else {
                                parts_strip(ui, view, ui.available_width());
                                ui.horizontal(|ui| {
                                    ui.label(egui::RichText::new("Finished").color(theme::OK).small());
                                    ui.label(
                                        egui::RichText::new("Receiving")
                                            .color(theme::STEEL_TEXT)
                                            .small(),
                                    );
                                    ui.label(
                                        egui::RichText::new("Pending")
                                            .color(theme::TEXT_FAINT)
                                            .small(),
                                    );
                                });
                                egui::ScrollArea::vertical()
                                    .id_salt("prog-parts")
                                    .max_height(240.0)
                                    .show(ui, |ui| {
                                        egui::Grid::new("prog-parts")
                                            .num_columns(4)
                                            .spacing([theme::SP_SM, 2.0])
                                            .show(ui, |ui| {
                                                // Torrent rows toggleable while live/paused.
                                                let torrent_files: Option<Vec<usize>> =
                                                    match view.kind {
                                                        crate::stream::ProtoKind::Torrent
                                                            if !view.files.is_empty() =>
                                                        {
                                                            Some(t.torrent_files.clone())
                                                        }
                                                        _ => None,
                                                    };
                                                let files_editable = matches!(
                                                    t.status,
                                                    TaskStatus::Downloading
                                                        | TaskStatus::Paused
                                                        | TaskStatus::Queued
                                                );
                                                for (i, (done, exp)) in view.parts.iter().enumerate() {
                                                    let pct = if *exp > 0 {
                                                        *done as f32 / *exp as f32 * 100.0
                                                    } else {
                                                        0.0
                                                    };
                                                    // Name cell: torrent = checkbox + path.
                                                    let file_entry =
                                                        torrent_files.as_ref().and_then(|_| {
                                                            view.files.get(i).cloned()
                                                        });
                                                    if let (Some(cur_sel), Some((fidx, fpath))) =
                                                        (torrent_files.as_ref(), file_entry)
                                                    {
                                                        let mut on = cur_sel.is_empty()
                                                            || cur_sel.contains(&fidx);
                                                        // One grid cell: checkbox + path grouped.
                                                        ui.push_id(fidx, |ui| {
                                                            ui.horizontal(|ui| {
                                                                let resp = ui.add_enabled(
                                                                    files_editable,
                                                                    egui::Checkbox::new(
                                                                        &mut on, "",
                                                                    ),
                                                                );
                                                                if resp.changed() {
                                                                    if let Some(new) =
                                                                        toggled_selection(
                                                                            cur_sel,
                                                                            view.files.len(),
                                                                            fidx,
                                                                            on,
                                                                        )
                                                                    {
                                                                        // Persist + signal live worker
                                                                        // (worker folds into state on return).
                                                                        let mut g = crate::lock_unpoison(&app.rt);
                                                                        if let Some(x) = g
                                                                            .state
                                                                            .tasks
                                                                            .iter_mut()
                                                                            .find(|x| x.id == t.id)
                                                                        {
                                                                            x.torrent_files = new.clone();
                                                                        }
                                                                        if let Some(c) = g.controls.get(&t.id) {
                                                                            *c.desired_files
                                                                                .lock()
                                                                                .unwrap_or_else(|e| e.into_inner()) = Some(new);
                                                                        }
                                                                        g.save();
                                                                    }
                                                                }
                                                                ui.label(
                                                                    egui::RichText::new(
                                                                        crate::torrent::trunc_path(
                                                                            &fpath, 44,
                                                                        ),
                                                                    )
                                                                    .color(if on {
                                                                        theme::TEXT
                                                                    } else {
                                                                        theme::TEXT_FAINT
                                                                    })
                                                                    .small(),
                                                                )
                                                                .on_hover_text(&fpath);
                                                            });
                                                        });
                                                    } else {
                                                        let hover = view.files.get(i).map(|(_, p)| p.clone());
                                                        let lbl = ui.label(
                                                            egui::RichText::new(match view.kind {
                                                                crate::stream::ProtoKind::Nntp
                                                                | crate::stream::ProtoKind::Torrent => {
                                                                    match view.files.get(i) {
                                                                        Some((_, p)) => crate::torrent::trunc_path(p, 44),
                                                                        None => format!("File {}", i + 1),
                                                                    }
                                                                }
                                                                _ => format!("Part {}", i + 1),
                                                            })
                                                            .color(theme::TEXT_FAINT)
                                                            .small(),
                                                        );
                                                        if let Some(h) = hover {
                                                            lbl.on_hover_text(h);
                                                        }
                                                    }
                                                    ui.label(
                                                        egui::RichText::new(format!("{:.0}%", pct))
                                                            .color(theme::TEXT)
                                                            .small(),
                                                    );
                                                    ui.label(
                                                        egui::RichText::new(if *exp == 0 {
                                                            format!("{} / —", fmt_bytes(*done))
                                                        } else {
                                                            format!(
                                                                "{} / {}",
                                                                fmt_bytes(*done),
                                                                fmt_bytes(*exp)
                                                            )
                                                        })
                                                        .color(theme::TEXT_FAINT)
                                                        .small(),
                                                    );
                                                    let word = part_status_word(*done, *exp).unwrap_or("");
                                                    ui.label(
                                                        egui::RichText::new(word)
                                                            .color(match word {
                                                                "Finished" => theme::OK,
                                                                "Receiving" => theme::STEEL_TEXT,
                                                                _ => theme::TEXT_FAINT,
                                                            })
                                                            .small(),
                                                    );
                                                    ui.end_row();
                                                }
                                            });
                                    });
                            }
                        }
                    }
                });
            let mut prog_h = prog_scroll.content_size.y + 208.0;
            if graph_resp.body_response.is_none() {
                prog_h += 74.0;
            }
            app.prog_body_h = Some(prog_h);
            ui.separator();
            btn_row(ui, |ui| {
                if t.status == TaskStatus::Downloading {
                    if icon_btn(ui, Icon::Pause, "Pause", true, 72.0).clicked() {
                        b_toggle = true;
                    }
                } else if t.status != TaskStatus::Complete
                    && icon_btn(ui, Icon::Play, "Resume", true, 72.0).clicked()
                {
                    b_toggle = true;
                }
                if icon_btn(ui, Icon::FolderOpen, "Open folder", false, 72.0).clicked() {
                    let _ = crate::platform::reveal_path(&t.dest_dir.join(&t.filename));
                }
                if icon_btn(
                    ui,
                    Icon::Link,
                    if crate::torrent::is_magnet(&t.url) {
                        "Copy magnet"
                    } else {
                        "Copy link"
                    },
                    false,
                    72.0,
                )
                .clicked()
                {
                    b_copy = true;
                }
                if ghost_btn_w(ui, "Close").clicked() {
                    b_close = true;
                }
            });
            (b_close, b_toggle, b_copy)
        };
        let mut body = Some(body);
        let mut prog_focused = false;
        ctx.show_viewport_immediate(vp_id("webcraft-progress"), builder, |ctx, class| {
            prog_focused = ctx.input(|i| i.focused);
            if ctx.input(|i| i.viewport().close_requested() || i.key_pressed(egui::Key::Escape)) {
                close = true;
            }
            let bb = body.as_mut().unwrap();
            let out = if matches!(class, egui::ViewportClass::Embedded) {
                let scr = ctx.screen_rect();
                egui::Window::new("Download progress")
                    .collapsible(false)
                    .resizable(true)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .default_size([640.0, 720.0])
                    .min_size([380.0, 320.0])
                    .max_size([
                        (scr.width() - 24.0).max(380.0),
                        (scr.height() - 24.0).max(320.0),
                    ])
                    .show(ctx, |ui| bb(ui, self))
                    .and_then(|r| r.inner)
                    .unwrap_or((false, false, false))
            } else {
                fit_vp(
                    ctx,
                    vp_id("webcraft-progress"),
                    egui::vec2(640.0, 720.0),
                    prog_h_in,
                    &mut prog_sent,
                );
                egui::CentralPanel::default()
                    .frame(dialog_frame())
                    .show(ctx, |ui| bb(ui, self))
                    .inner
            };
            // Accumulate: discarded extra pass must not erase click flags.
            close |= out.0;
            toggle_pause |= out.1;
            copy_link |= out.2;
        });
        self.prog_vp_sent = prog_sent;
        if prog_focused {
            self.prog_raise_frames = 0;
        }
        if close {
            self.prog_raise_frames = 0;
        }
        if copy_link {
            if let Ok(mut cb) = arboard::Clipboard::new() {
                let _ = cb.set_text(t.url.clone());
            }
        }
        if toggle_pause {
            if t.status == TaskStatus::Downloading {
                stop_task(self, id);
            } else {
                self.start_task(id);
            }
        }
        if close {
            self.show_progress_for = None;
        }
    }

    fn ui_queues(&mut self, ui: &mut egui::Ui, scheduler_only: bool) {
        if !scheduler_only {
            ui.heading("Queues");
            ui.add_space(4.0);
            theme::panel().show(ui, |ui| {
                form_grid("new-queue-grid").show(ui, |ui| {
                    form_label(ui, "New queue");
                    ui.add(fill_edit(&mut self.new_queue_name));
                    ui.end_row();
                });
                ui.horizontal(|ui| {
                    ui.add_space(LABEL_W + 12.0);
                    if ui
                        .add_enabled_ui(!self.new_queue_name.trim().is_empty(), |ui| {
                            ghost_btn_w(ui, "Add queue")
                        })
                        .inner
                        .clicked()
                        && !self.new_queue_name.trim().is_empty()
                    {
                        let mut g = crate::lock_unpoison(&self.rt);
                        let id = crate::model::new_id();
                        g.state.queues.push(Queue {
                            id,
                            name: self.new_queue_name.clone(),
                            max_concurrent: 1,
                            auto_start: false,
                            retry_count: 3,
                            retry_interval_secs: 10,
                            ..Default::default()
                        });
                        g.save();
                        self.new_queue_name.clear();
                    }
                });
            });
            ui.add_space(6.0);
            let queues = crate::lock_unpoison(&self.rt).state.queues.clone();
            for q in queues {
                theme::panel().show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(&q.name).strong());
                        theme::badge(ui, &format!("max {}", q.max_concurrent), theme::TEXT_MUTED);
                        if q.auto_start {
                            theme::badge(ui, "auto-start", theme::TEAL_500);
                        }
                        theme::badge(
                            ui,
                            &format!("retry {}x/{}s", q.retry_count, q.retry_interval_secs),
                            theme::TEXT_MUTED,
                        );
                        theme::badge(
                            ui,
                            &format!("on complete: {:?}", q.on_complete),
                            theme::TEXT_MUTED,
                        );
                    });
                    ui.separator();
                    let tasks: Vec<DownloadTask> = {
                        let g = crate::lock_unpoison(&self.rt);
                        let mut v: Vec<_> = g
                            .state
                            .tasks
                            .iter()
                            .filter(|t| t.queue_id == q.id)
                            .cloned()
                            .collect();
                        v.sort_by_key(|t| t.order);
                        v
                    };
                    if tasks.is_empty() {
                        ui.label(
                            egui::RichText::new("No downloads in queue.")
                                .color(theme::TEXT_FAINT)
                                .small(),
                        );
                    }
                    for t in &tasks {
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(format!("#{} [{:?}]", t.order, t.priority))
                                    .color(theme::TEXT_MUTED)
                                    .small()
                                    .monospace(),
                            );
                            let name_w = (ui.available_width() - 250.0).max(80.0);
                            ui.add_sized(
                                [name_w, theme::ROW_H_NAV],
                                egui::Label::new(&t.filename).truncate().selectable(false),
                            )
                            .on_hover_text(t.filename.clone());
                            let sc = if t.status == TaskStatus::Downloading {
                                theme::STEEL_TEXT
                            } else {
                                theme::status_color(t.status)
                            };
                            ui.label(
                                egui::RichText::new(format!("{:?}", t.status))
                                    .color(sc)
                                    .small(),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if icon_btn(ui, Icon::Play, "Start", false, 58.0).clicked() {
                                        self.start_task(&t.id.clone());
                                    }
                                    ui.add_space(theme::SP_XS);
                                    if icon_btn(ui, Icon::Down, "Down", false, 62.0).clicked() {
                                        let mut g = crate::lock_unpoison(&self.rt);
                                        move_task(&mut g.state.tasks, &t.id, 1);
                                        g.save();
                                    }
                                    if icon_btn(ui, Icon::Up, "Up", false, 48.0).clicked() {
                                        let mut g = crate::lock_unpoison(&self.rt);
                                        move_task(&mut g.state.tasks, &t.id, -1);
                                        g.save();
                                    }
                                },
                            );
                        });
                        // Priority on its own row: never glued to move buttons.
                        ui.horizontal(|ui| {
                            ui.add_space(52.0);
                            ui.label(
                                egui::RichText::new("Priority")
                                    .color(theme::TEXT_FAINT)
                                    .small(),
                            );
                            for (label, pr) in [
                                ("Low", Priority::Low),
                                ("Normal", Priority::Normal),
                                ("High", Priority::High),
                            ] {
                                let cur = t.priority == pr;
                                if ui
                                    .add_enabled(!cur, egui::Button::new(label).selected(cur))
                                    .clicked()
                                {
                                    let mut g = crate::lock_unpoison(&self.rt);
                                    if let Some(x) = g.state.tasks.iter_mut().find(|x| x.id == t.id)
                                    {
                                        x.priority = pr;
                                    }
                                    g.save();
                                }
                            }
                        });
                        ui.add_space(theme::SP_XS);
                    }
                    ui.separator();
                    form_grid(&format!("queue-grid-{}", q.id)).show(ui, |ui| {
                        let mut maxc = q.max_concurrent;
                        let mut auto = q.auto_start;
                        let mut retry = q.retry_count;
                        let mut interval = q.retry_interval_secs;
                        form_label(ui, "Max concurrent");
                        ui.horizontal(|ui| {
                            ui.add_sized([DRAG_W, theme::BTN_H], drag(&mut maxc).range(1..=16));
                            ui.checkbox(&mut auto, "Auto-start");
                        });
                        ui.end_row();
                        form_label(ui, "Retries");
                        ui.horizontal(|ui| {
                            ui.add_sized([DRAG_W, theme::BTN_H], drag(&mut retry).range(0..=20));
                            ui.label("every (s):");
                            ui.add_sized(
                                [DRAG_W, theme::BTN_H],
                                drag(&mut interval).range(1..=3600),
                            );
                        });
                        ui.end_row();
                        if maxc != q.max_concurrent
                            || auto != q.auto_start
                            || retry != q.retry_count
                            || interval != q.retry_interval_secs
                        {
                            let mut g = crate::lock_unpoison(&self.rt);
                            if let Some(x) = g.state.queues.iter_mut().find(|x| x.id == q.id) {
                                x.max_concurrent = maxc.max(1);
                                x.auto_start = auto;
                                x.retry_count = retry;
                                x.retry_interval_secs = interval;
                            }
                            g.save();
                            ui.ctx().request_repaint();
                        }
                    });
                });
                ui.add_space(6.0);
            }
        } else {
            // IDM Scheduler layout: left list (New/Delete), right grouped editor.
            ui.horizontal(|ui| {
                ui.heading("Scheduler");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let mut master = crate::lock_unpoison(&self.rt)
                        .state
                        .settings
                        .scheduler_enabled;
                    if ui.checkbox(&mut master, "Turn on scheduler").changed() {
                        let mut g = crate::lock_unpoison(&self.rt);
                        g.state.settings.scheduler_enabled = master;
                        g.save();
                        ui.ctx().request_repaint();
                    }
                });
            });
            ui.add_space(8.0);
            let schedules = crate::lock_unpoison(&self.rt).state.schedules.clone();
            let queues = crate::lock_unpoison(&self.rt).state.queues.clone();
            let (day, secs) = self.minutes_today;
            if self.selected_schedule.is_none()
                || !schedules
                    .iter()
                    .any(|x| Some(&x.id) == self.selected_schedule.as_ref())
            {
                self.selected_schedule = schedules.first().map(|x| x.id.clone());
            }
            ui.horizontal_top(|ui| {
                // Left: schedule list
                ui.vertical(|ui| {
                    ui.set_width((ui.available_width() * 0.28).clamp(160.0, 260.0));
                    ui.label(
                        egui::RichText::new("Schedules")
                            .color(theme::TEXT_MUTED)
                            .small()
                            .strong(),
                    );
                    egui::ScrollArea::vertical()
                        .max_height(300.0)
                        .show(ui, |ui| {
                            for x in &schedules {
                                let label = if x.name.trim().is_empty() {
                                    format!(
                                        "Start at {:02}:{:02} · {}",
                                        x.start_secs / 3600,
                                        (x.start_secs % 3600) / 60,
                                        x.queue_id
                                    )
                                } else {
                                    x.name.clone()
                                };
                                if ui
                                    .selectable_label(
                                        self.selected_schedule.as_deref() == Some(x.id.as_str()),
                                        label,
                                    )
                                    .clicked()
                                {
                                    self.selected_schedule = Some(x.id.clone());
                                }
                            }
                        });
                    ui.horizontal(|ui| {
                        if theme::primary_btn(ui, "New").clicked() {
                            let id = crate::model::new_id();
                            let mut g = crate::lock_unpoison(&self.rt);
                            g.state.schedules.push(Schedule {
                                id: id.clone(),
                                queue_id: "main".into(),
                                enabled: true,
                                recurrence: Recurrence::Daily,
                                start_secs: 3600,
                                stop_secs: Some(7200),
                                files_at_once: 3,
                                ..Default::default()
                            });
                            g.save();
                            self.selected_schedule = Some(id);
                        }
                        ui.add_enabled_ui(self.selected_schedule.is_some(), |ui| {
                            if theme::ghost_btn(ui, "Delete").clicked() {
                                if let Some(id) = self.selected_schedule.take() {
                                    let mut g = crate::lock_unpoison(&self.rt);
                                    g.state.schedules.retain(|x| x.id != id);
                                    g.save();
                                }
                            }
                        });
                    });
                });
                ui.separator();
                // Right: editor for selected schedule
                ui.vertical(|ui| {
                    let Some(sel) = self.selected_schedule.clone() else {
                        ui.label(
                            egui::RichText::new("No schedule selected. Use New.")
                                .color(theme::TEXT_FAINT),
                        );
                        return;
                    };
                    let Some(sch) = schedules.iter().find(|x| x.id == sel).cloned() else {
                        return;
                    };
                    let edit = |app: &DownloadApp, f: &dyn Fn(&mut Schedule)| {
                        let mut g = crate::lock_unpoison(&app.rt);
                        if let Some(x) = g.state.schedules.iter_mut().find(|x| x.id == sel) {
                            f(x);
                        }
                        g.save();
                    };
                    form_grid("sch-name-grid").show(ui, |ui| {
                        form_label(ui, "Name");
                        ui.horizontal(|ui| {
                            let mut name = sch.name.clone();
                            if ui
                                .add(
                                    egui::TextEdit::singleline(&mut name)
                                        .desired_width(f32::INFINITY),
                                )
                                .changed()
                            {
                                edit(self, &move |x: &mut Schedule| x.name = name.clone());
                            }
                            let mut en = sch.enabled;
                            if ui.checkbox(&mut en, "Enabled").changed() {
                                edit(self, &move |x: &mut Schedule| x.enabled = en);
                            }
                        });
                        ui.end_row();
                    });
                    ui.horizontal(|ui| match next_run_in(&sch, day, secs) {
                        Some(n) => theme::badge(
                            ui,
                            &format!("next in {}", eta(n as f64)),
                            theme::STEEL_TEXT,
                        ),
                        None => theme::badge(ui, "no next run", theme::TEXT_FAINT),
                    });
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new("Start time")
                            .color(theme::TEXT_MUTED)
                            .small()
                            .strong(),
                    );
                    form_grid("sch-start-grid").show(ui, |ui| {
                        form_label(ui, "Queue");
                        ui.horizontal_wrapped(|ui| {
                            egui::ComboBox::from_id_salt("sch-queue")
                                .selected_text(
                                    queues
                                        .iter()
                                        .find(|q| q.id == sch.queue_id)
                                        .map(|q| q.name.clone())
                                        .unwrap_or_else(|| sch.queue_id.clone()),
                                )
                                .show_ui(ui, |ui| {
                                    for q in &queues {
                                        if ui
                                            .selectable_label(sch.queue_id == q.id, &q.name)
                                            .clicked()
                                        {
                                            let qid = q.id.clone();
                                            edit(self, &move |x: &mut Schedule| {
                                                x.queue_id = qid.clone()
                                            });
                                        }
                                    }
                                });
                            let (hh, mut mm) =
                                (sch.start_secs / 3600, (sch.start_secs % 3600) / 60);
                            let mut pm = hh >= 12;
                            let mut h12 = if hh % 12 == 0 { 12 } else { hh % 12 };
                            ui.label("at");
                            ui.add_sized(
                                [DRAG_W, theme::BTN_H],
                                drag(&mut h12).range(1..=12).speed(1),
                            );
                            ui.label(":");
                            ui.add_sized(
                                [DRAG_W, theme::BTN_H],
                                drag(&mut mm).range(0..=59).speed(1),
                            );
                            egui::ComboBox::from_id_salt("sch-ampm")
                                .selected_text(if pm { "PM" } else { "AM" })
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(&mut pm, false, "AM");
                                    ui.selectable_value(&mut pm, true, "PM");
                                });
                            let new_start = ((h12 % 12) + if pm { 12 } else { 0 }) * 3600 + mm * 60;
                            if new_start != sch.start_secs {
                                edit(self, &move |x: &mut Schedule| x.start_secs = new_start);
                            }
                            // silence unused warnings for hh clone
                            let _ = hh;
                        });
                        ui.end_row();
                    });
                    ui.horizontal_wrapped(|ui| {
                        let mut rec = sch.recurrence;
                        ui.radio_value(&mut rec, Recurrence::Once, "Once");
                        ui.radio_value(&mut rec, Recurrence::Daily, "Every day");
                        ui.radio_value(&mut rec, Recurrence::Weekdays, "Weekdays");
                        ui.radio_value(&mut rec, Recurrence::Weekly, "Weekly");
                        if rec != sch.recurrence {
                            edit(self, &move |x: &mut Schedule| x.recurrence = rec);
                        }
                        if sch.recurrence == Recurrence::Once {
                            ui.label("on (YYYYMMDD):");
                            let mut d = sch.once_date;
                            ui.add_sized(
                                [DRAG_W, theme::BTN_H],
                                drag(&mut d).range(0..=99991231).speed(1),
                            );
                            if d != sch.once_date {
                                edit(self, &move |x: &mut Schedule| x.once_date = d);
                            }
                        }
                        if sch.recurrence == Recurrence::Daily {
                            ui.label("Repeat each");
                            let mut rd = sch.repeat_days;
                            ui.add_sized([DRAG_W, theme::BTN_H], drag(&mut rd).range(0..=365));
                            ui.label("days");
                            if rd != sch.repeat_days {
                                edit(self, &move |x: &mut Schedule| x.repeat_days = rd);
                            }
                        }
                    });
                    if sch.recurrence == Recurrence::Weekly {
                        ui.horizontal_wrapped(|ui| {
                            for (i, d) in ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"]
                                .iter()
                                .enumerate()
                            {
                                let bit = 1u8 << i;
                                let mut on = sch.weekdays_mask & bit != 0;
                                if ui.checkbox(&mut on, *d).changed() {
                                    edit(self, &move |x: &mut Schedule| {
                                        if on {
                                            x.weekdays_mask |= bit;
                                        } else {
                                            x.weekdays_mask &= !bit;
                                        }
                                    });
                                }
                            }
                        });
                    }
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new("Stop time")
                            .color(theme::TEXT_MUTED)
                            .small()
                            .strong(),
                    );
                    ui.horizontal_wrapped(|ui| {
                        let mut has_stop = sch.stop_secs.is_some();
                        if ui.checkbox(&mut has_stop, "Stop at").changed() {
                            edit(self, &move |x: &mut Schedule| {
                                x.stop_secs = if has_stop {
                                    Some(x.start_secs + 3600)
                                } else {
                                    None
                                }
                            });
                        }
                        if let Some(stop) = sch.stop_secs {
                            let (mut mm2, mut h12b) = ((stop % 3600) / 60, {
                                let h = (stop / 3600) % 24;
                                if h % 12 == 0 {
                                    12
                                } else {
                                    h % 12
                                }
                            });
                            let mut pmb = (stop / 3600) % 24 >= 12;
                            ui.add_sized(
                                [DRAG_W, theme::BTN_H],
                                drag(&mut h12b).range(1..=12).speed(1),
                            );
                            ui.label(":");
                            ui.add_sized(
                                [DRAG_W, theme::BTN_H],
                                drag(&mut mm2).range(0..=59).speed(1),
                            );
                            egui::ComboBox::from_id_salt("sch-stop-ampm")
                                .selected_text(if pmb { "PM" } else { "AM" })
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(&mut pmb, false, "AM");
                                    ui.selectable_value(&mut pmb, true, "PM");
                                });
                            let ns = ((h12b % 12) + if pmb { 12 } else { 0 }) * 3600 + mm2 * 60;
                            if ns != stop {
                                edit(self, &move |x: &mut Schedule| x.stop_secs = Some(ns));
                            }
                            ui.label(
                                egui::RichText::new(format!(
                                    "(after {})",
                                    eta((stop as i64 - sch.start_secs as i64).max(0) as f64)
                                ))
                                .color(theme::TEXT_FAINT)
                                .small(),
                            );
                        }
                    });
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new("Files in queue")
                            .color(theme::TEXT_MUTED)
                            .small()
                            .strong(),
                    );
                    form_grid("sch-files-grid").show(ui, |ui| {
                        form_label(ui, "Simultaneous");
                        ui.horizontal(|ui| {
                            let mut f = sch.files_at_once;
                            ui.add_sized([DRAG_W, theme::BTN_H], drag(&mut f).range(0..=32));
                            ui.label("(0 = all; queue cap also applies)");
                            if f != sch.files_at_once {
                                edit(self, &move |x: &mut Schedule| x.files_at_once = f);
                            }
                        });
                        ui.end_row();
                        form_label(ui, "On finish");
                        ui.horizontal_wrapped(|ui| {
                            let mut oc = sch.on_complete;
                            egui::ComboBox::from_id_salt("sch-oncomplete")
                                .selected_text(format!("{oc:?}"))
                                .show_ui(ui, |ui| {
                                    for a in [
                                        CompletionAction::None,
                                        CompletionAction::OpenFile,
                                        CompletionAction::RunCommand,
                                        CompletionAction::ExitApp,
                                        CompletionAction::Shutdown,
                                        CompletionAction::Sleep,
                                    ] {
                                        ui.selectable_value(&mut oc, a, format!("{a:?}"));
                                    }
                                });
                            if oc != sch.on_complete {
                                edit(self, &move |x: &mut Schedule| x.on_complete = oc);
                            }
                            if sch.on_complete == CompletionAction::RunCommand {
                                let mut arg = String::new();
                                if let Some(q) = queues.iter().find(|q| q.id == sch.queue_id) {
                                    arg = q.on_complete_arg.clone();
                                }
                                // Was dead: TextEdit wrote local `arg`, dropped. Write
                                // back to queue (run_completion reads q.on_complete_arg).
                                if ui
                                    .add(
                                        egui::TextEdit::singleline(&mut arg)
                                            .hint_text("command, {file} = file")
                                            .desired_width(f32::INFINITY),
                                    )
                                    .changed()
                                {
                                    let mut g = crate::lock_unpoison(&self.rt);
                                    if let Some(q) =
                                        g.state.queues.iter_mut().find(|q| q.id == sch.queue_id)
                                    {
                                        q.on_complete_arg = arg.clone();
                                    }
                                    g.save();
                                }
                            }
                            if matches!(
                                sch.on_complete,
                                CompletionAction::Shutdown | CompletionAction::Sleep
                            ) {
                                let mut cf = sch.power_confirmed;
                                if ui.checkbox(&mut cf, "Confirmed: PC power action").changed() {
                                    edit(self, &move |x: &mut Schedule| x.power_confirmed = cf);
                                }
                            }
                        });
                        ui.end_row();
                        form_label(ui, "Speed limit KB/s");
                        ui.horizontal(|ui| {
                            let mut lim = sch.limit_kbps;
                            if ui
                                .add_sized(
                                    [DRAG_W, theme::BTN_H],
                                    drag(&mut lim).range(0..=1_000_000),
                                )
                                .changed()
                            {
                                edit(self, &move |x: &mut Schedule| x.limit_kbps = lim);
                            }
                            ui.label("(0 = unlimited)");
                        });
                        ui.end_row();
                        form_label(ui, "Quota MB / hours");
                        ui.horizontal_wrapped(|ui| {
                            let (mut qmb, mut qh) = (sch.quota_mb, sch.quota_hours);
                            ui.add_sized(
                                [DRAG_W, theme::BTN_H],
                                drag(&mut qmb).range(0..=1_000_000),
                            );
                            ui.label("MB every");
                            ui.add_sized([DRAG_W, theme::BTN_H], drag(&mut qh).range(0..=720));
                            ui.label("hours (0 = off)");
                            ui.label(
                                egui::RichText::new(format!(
                                    "used {} this window",
                                    fmt_bytes(sch.quota_used_bytes)
                                ))
                                .color(theme::TEXT_FAINT)
                                .small(),
                            );
                            if qmb != sch.quota_mb || qh != sch.quota_hours {
                                edit(self, &move |x: &mut Schedule| {
                                    x.quota_mb = qmb;
                                    x.quota_hours = qh;
                                });
                            }
                        });
                        ui.end_row();
                    });
                    ui.horizontal(|ui| {
                        if icon_btn(ui, Icon::Play, "Start now", false, 96.0).clicked() {
                            let ids: Vec<String> = crate::lock_unpoison(&self.rt)
                                .state
                                .tasks
                                .iter()
                                .filter(|t| {
                                    t.queue_id == sch.queue_id && t.status == TaskStatus::Queued
                                })
                                .map(|t| t.id.clone())
                                .collect();
                            for id in ids {
                                self.start_task(&id);
                            }
                        }
                        let mut missed = sch.run_missed;
                        if ui
                            .checkbox(&mut missed, "Run missed jobs at start")
                            .changed()
                        {
                            edit(self, &move |x: &mut Schedule| x.run_missed = missed);
                        }
                    });
                });
            });
        }
    }

    fn ui_streams(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_space(8.0);
                ui.heading("Streams");
                ui.add_space(6.0);
                theme::panel().show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    section(ui, "Play in mpv (embedded in app)");
                    form_grid("streams-grid").show(ui, |ui| {
                        form_label(ui, "Video URL");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.stream_url)
                                .hint_text(
                                    "https://example.com/video.mp4, .m3u8, or local file path",
                                )
                                .desired_width(f32::INFINITY),
                        );
                        ui.end_row();
                    });
                    let url = self.stream_url.trim().to_string();
                    let ctx = ui.ctx().clone();
                    ui.horizontal_wrapped(|ui| {
                        if icon_btn_enabled(
                            ui,
                            Icon::Play,
                            "Play in mpv",
                            true,
                            120.0,
                            !url.is_empty(),
                        )
                        .clicked()
                        {
                            self.play_embedded(&url);
                            self.stream_status_err = self.stream_status.starts_with("ERR ");
                        }
                        if theme::ghost_btn(ui, "Paste").clicked() {
                            if let Ok(mut cb) = arboard::Clipboard::new() {
                                if let Ok(txt) = cb.get_text() {
                                    if !txt.trim().is_empty() {
                                        self.stream_url = txt.trim().to_string();
                                    }
                                }
                            }
                            ctx.request_repaint();
                        }
                        if theme::ghost_btn(ui, "Clear").clicked() {
                            self.stream_url.clear();
                            self.stream_status.clear();
                            self.stream_status_err = false;
                            ctx.request_repaint();
                        }
                        if theme::ghost_btn(ui, "Download").clicked() && !url.is_empty() {
                            let id = {
                                let mut g = crate::lock_unpoison(&self.rt);
                                g.add_url(&url, "main", "", "")
                            };
                            // Add dialog may have queued it already.
                            let queued = {
                                let g = crate::lock_unpoison(&self.rt);
                                g.state
                                    .tasks
                                    .iter()
                                    .find(|t| t.id == id)
                                    .is_some_and(|t| t.status == TaskStatus::Queued)
                            };
                            if queued {
                                self.start_task(&id);
                            }
                            self.tab = Tab::Downloads;
                            ctx.request_repaint();
                        }
                    });
                    if !self.stream_status.is_empty() {
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new(self.stream_status.clone())
                                .color(if self.stream_status_err {
                                    theme::ERR
                                } else {
                                    theme::OK
                                })
                                .small(),
                        );
                    }
                    ui.add_space(6.0);
                    let mpv_found = {
                        let g = crate::lock_unpoison(&self.rt);
                        downloadcraft_lib::mpv::find_mpv(&g.state.settings.mpv_path)
                    };
                    match mpv_found {
                        Some(p) => {
                            ui.label(
                                egui::RichText::new(format!(
                                    "mpv (embedded in app): {}",
                                    p.display()
                                ))
                                .color(theme::TEXT_FAINT)
                                .small(),
                            );
                        }
                        None => {
                            ui.label(
                                egui::RichText::new(
                                    "mpv not found. Install mpv or set Options > mpv > Path.",
                                )
                                .color(theme::ERR)
                                .small(),
                            );
                        }
                    }
                });
                ui.add_space(10.0);
                theme::panel().show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    section(ui, "Stream downloads");
                    let rows: Vec<(String, String, String, TaskStatus)> = {
                        let g = crate::lock_unpoison(&self.rt);
                        g.state
                            .tasks
                            .iter()
                            .filter(|t| {
                                matches!(
                                    crate::stream::proto_kind(t),
                                    crate::stream::ProtoKind::Dash | crate::stream::ProtoKind::Hls
                                )
                            })
                            .map(|t| (t.id.clone(), t.filename.clone(), t.url.clone(), t.status))
                            .collect()
                    };
                    if rows.is_empty() {
                        ui.label(
                            egui::RichText::new("No DASH/HLS downloads yet.")
                                .color(theme::TEXT_MUTED)
                                .small(),
                        );
                    }
                    let mut to_play: Option<String> = None;
                    let mut to_start: Option<String> = None;
                    for (id, filename, url, status) in &rows {
                        ui.horizontal(|ui| {
                            ui.vertical(|ui| {
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(filename).color(theme::TEXT),
                                    )
                                    .truncate(),
                                )
                                .on_hover_text(url);
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(url).color(theme::TEXT_MUTED).small(),
                                    )
                                    .truncate(),
                                )
                                .on_hover_text(url);
                            });
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    theme::badge(
                                        ui,
                                        &format!("{status:?}"),
                                        theme::status_color(*status),
                                    );
                                    if *status != TaskStatus::Downloading
                                        && theme::ghost_btn(ui, "Start download").clicked()
                                    {
                                        to_start = Some(id.clone());
                                    }
                                    if icon_btn(ui, Icon::Play, "Play", false, 78.0).clicked() {
                                        to_play = Some(url.clone());
                                    }
                                },
                            );
                        });
                        ui.separator();
                    }
                    if let Some(url) = to_play {
                        self.play_embedded(&url);
                        self.stream_status_err = self.stream_status.starts_with("ERR ");
                    }
                    if let Some(id) = to_start {
                        self.start_task(&id);
                    }
                });
            });
    }
    fn ui_grabber(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            ui.heading("Site Grabber");
        });
        ui.add_space(4.0);
        ui.add_space(4.0);
        theme::panel().show(ui, |ui| {
            ui.set_width(ui.available_width());
            form_grid("grab-grid").show(ui, |ui| {
                form_label(ui, "Start URL");
                ui.add(
                    egui::TextEdit::singleline(&mut self.grab_url)
                        .hint_text("https://example.com/")
                        .desired_width(f32::INFINITY),
                );
                ui.end_row();
                form_label(ui, "Extensions");
                ui.add(
                    egui::TextEdit::singleline(&mut self.grab_exts)
                        .hint_text("zip,mp4,pdf (empty = all)")
                        .desired_width(f32::INFINITY),
                );
                ui.end_row();
                form_label(ui, "Depth");
                ui.horizontal(|ui| {
                    ui.add_sized([DRAG_W, theme::BTN_H], drag(&mut self.grab_depth).range(0..=10));
                    ui.label("External levels:");
                    ui.add_sized([DRAG_W, theme::BTN_H], drag(&mut self.grab_ext_depth).range(0..=10));
                });
                ui.end_row();
                form_label(ui, "Scope");
                ui.horizontal_wrapped(|ui| {
                    egui::ComboBox::from_id_salt("grab-scope").width(ui.available_width()).selected_text(["Same host", "Same domain", "Any host"][self.grab_scope.min(2) as usize]).show_ui(ui, |ui| { for (i, l) in ["Same host", "Same domain", "Any host"].iter().enumerate() { ui.selectable_value(&mut self.grab_scope, i as u8, *l); } });
                    ui.checkbox(&mut self.grab_parent_dirs, "Don't explore parent dirs");
                });
                ui.end_row();
                form_label(ui, "Only paths");
                ui.add(
                    egui::TextEdit::singleline(&mut self.grab_only_paths)
                        .hint_text("/files/;cdn.example.com (; separated)")
                        .desired_width(f32::INFINITY),
                );
                ui.end_row();
                form_label(ui, "Exclude paths");
                ui.add(
                    egui::TextEdit::singleline(&mut self.grab_exclude_paths)
                        .hint_text("/logout;/ads/ (; separated)")
                        .desired_width(f32::INFINITY),
                );
                ui.end_row();
                form_label(ui, "Cookies");
                ui.add(
                    egui::TextEdit::singleline(&mut self.grab_cookies)
                        .hint_text("name=value; ... (browser Grab site fills this; SharePoint needs it)")
                        .password(true)
                        .desired_width(f32::INFINITY),
                );
                ui.end_row();
            });
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                if icon_btn_enabled(ui, Icon::Search, "Explore", true, 92.0, !self.grab_url.trim().is_empty()).clicked() || (self.grab_auto_explore && !self.grab_busy && !self.grab_url.trim().is_empty()) {
                    self.grab_auto_explore = false;
                    let proj = self.grab_project(false);
                    // Never show/persist session cookies in project JSON.
                    let mut shown = proj.clone();
                    shown.cookies.clear();
                    self.grab_project_json = serde_json::to_string_pretty(&shown).unwrap_or_default();
                    self.grab_busy = true;
                    self.grab_status = "Exploring site...".into();
                    let rt = self.rt.clone(); let client = engine::build_client("");
                    let progress = Arc::new(Mutex::new(crate::grabber::CrawlProgress::default()));
                    let crawler = Crawler { project: proj, progress: Some(progress.clone()) };
                    let files_slot = Arc::new(Mutex::new(GrabOutcome { progress: Some(progress), ..Default::default() }));
                    let slot2 = files_slot.clone();
                    let rt2 = self.tokio_rt.clone();
                    std::thread::spawn(move || {
                        let res = rt2.block_on(async move { crawler.crawl(&client).await });
                        let mut out = slot2.lock().unwrap(); out.done = true; out.apply_files = true;
                        match res { Ok((pages, files)) => { out.message = format!("Done. {} pages explored, {} files found.", pages.len(), files.len()); out.files = files; crate::lock_unpoison(&rt).log("Grabber explore done"); } Err(e) => { out.message = format!("Explore failed: {e}"); crate::lock_unpoison(&rt).log(format!("Grabber error: {e}")); } }
                    });
                    self.grab_files = Vec::new();
                    GRAB_SLOT.with(|s| *s.borrow_mut() = Some(files_slot));
                }
                ui.label(egui::RichText::new("Results update automatically").color(theme::TEXT_MUTED).small());
                if icon_btn_enabled(ui, Icon::Globe, "Mirror site", false, 104.0, !self.grab_url.trim().is_empty()).clicked() {
                    if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                        let proj = self.grab_project(true);
                        self.grab_busy = true;
                        self.grab_status = format!("Mirroring site to {}...", dir.display());
                        let slot = Arc::new(Mutex::new(GrabOutcome::default()));
                        let slot2 = slot.clone();
                        GRAB_SLOT.with(|s| *s.borrow_mut() = Some(slot));
                        let rt = self.rt.clone(); let rt2 = self.tokio_rt.clone();
                        std::thread::spawn(move || { let res = rt2.block_on(async move { crate::grabber::mirror_site(&engine::build_client(""), &proj, &dir).await }); let mut out = slot2.lock().unwrap(); out.done = true; out.apply_files = false; match res { Ok(v) => { out.message = format!("Mirror done: {} files written.", v.len()); crate::lock_unpoison(&rt).log(format!("Mirror done: {} files", v.len())); }, Err(e) => { out.message = format!("Mirror failed: {e}"); crate::lock_unpoison(&rt).log(format!("Mirror error: {e}")); } } });
                    }
                }
                ui.label(egui::RichText::new("Explore crawls pages; Enqueue adds selected files as downloads with page referrer.").color(theme::TEXT_FAINT).small());
                ui.label(egui::RichText::new(&self.grab_status).color(if self.grab_busy { theme::TEAL_500 } else { theme::TEXT_MUTED }).small());
            });
        });
        ui.add_space(6.0);
        theme::panel().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!(
                        "{} files found · {} selected",
                        self.grab_files.len(),
                        self.grab_files.iter().filter(|f| f.selected).count()
                    ))
                    .strong(),
                );
            });
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                action_group(ui, "Download", |ui| {
                    if icon_btn_enabled(
                        ui,
                        Icon::Download,
                        "Enqueue selected",
                        true,
                        140.0,
                        self.grab_files.iter().any(|f| f.selected),
                    )
                    .clicked()
                    {
                        for f in self.grab_files.iter().filter(|f| f.selected) {
                            // Session cookies only to files on start host.
                            let fh = url::Url::parse(&f.url)
                                .ok()
                                .and_then(|u| u.host_str().map(|h| h.to_string()));
                            let sh = url::Url::parse(&self.grab_url)
                                .ok()
                                .and_then(|u| u.host_str().map(|h| h.to_string()));
                            let ck = if fh.is_some() && fh == sh {
                                self.grab_cookies.as_str()
                            } else {
                                ""
                            };
                            let id = crate::lock_unpoison(&self.rt).add_url(
                                &f.url.clone(),
                                "main",
                                &f.page_url.clone(),
                                ck,
                            );
                            let _ = id;
                        }
                    }
                });
                action_group(ui, "Selection", |ui| {
                    if icon_btn_enabled(
                        ui,
                        Icon::Check,
                        "Select all",
                        false,
                        92.0,
                        !self.grab_files.is_empty(),
                    )
                    .clicked()
                    {
                        for f in &mut self.grab_files {
                            f.selected = true;
                        }
                    }
                    if icon_btn_enabled(
                        ui,
                        Icon::Delete,
                        "Select none",
                        false,
                        102.0,
                        self.grab_files.iter().any(|f| f.selected),
                    )
                    .clicked()
                    {
                        for f in &mut self.grab_files {
                            f.selected = false;
                        }
                    }
                });
                action_group(ui, "Files", |ui| {
                    if icon_btn_enabled(
                        ui,
                        Icon::Document,
                        "Export .txt",
                        false,
                        102.0,
                        !self.grab_files.is_empty(),
                    )
                    .clicked()
                    {
                        if let Some(p) = rfd::FileDialog::new()
                            .add_filter("txt", &["txt"])
                            .save_file()
                        {
                            let _ = std::fs::write(
                                p,
                                self.grab_files
                                    .iter()
                                    .map(|f| f.url.clone())
                                    .collect::<Vec<_>>()
                                    .join("\n"),
                            );
                        }
                    }
                    if icon_btn_enabled(
                        ui,
                        Icon::Document,
                        "Export CSV",
                        false,
                        102.0,
                        !self.grab_files.is_empty(),
                    )
                    .clicked()
                    {
                        if let Some(p) = rfd::FileDialog::new()
                            .add_filter("csv", &["csv"])
                            .save_file()
                        {
                            let mut s = "url,size,mime,page\n".to_string();
                            for f in &self.grab_files {
                                s.push_str(&format!(
                                    "{},{},{},{}\n",
                                    f.url,
                                    f.size.map(|v| v.to_string()).unwrap_or_default(),
                                    f.mime,
                                    f.page_url
                                ));
                            }
                            let _ = std::fs::write(p, s);
                        }
                    }
                    if icon_btn_enabled(
                        ui,
                        Icon::FolderOpen,
                        "Save project",
                        false,
                        112.0,
                        !self.grab_files.is_empty(),
                    )
                    .clicked()
                    {
                        if let Some(p) = rfd::FileDialog::new()
                            .add_filter("json", &["json"])
                            .save_file()
                        {
                            let _ = std::fs::write(p, &self.grab_project_json);
                        }
                    }
                    if icon_btn(ui, Icon::FolderOpen, "Load project", false, 112.0).clicked() {
                        if let Some(p) = rfd::FileDialog::new().pick_file() {
                            if let Ok(t) = std::fs::read_to_string(p) {
                                self.grab_project_json = t.clone();
                                if let Ok(proj) = serde_json::from_str::<GrabberProject>(&t) {
                                    self.grab_url = proj.start_url;
                                    self.grab_cookies = proj.cookies.clone();
                                    self.grab_depth = proj.depth;
                                    self.grab_ext_depth = proj.external_depth;
                                    self.grab_scope = proj.scope;
                                    self.grab_parent_dirs = proj.dont_explore_parent_dirs;
                                    self.grab_only_paths = proj.only_paths.join(";");
                                    self.grab_exclude_paths = proj.exclude_paths.join(";");
                                }
                            }
                        }
                    }
                });
            });
            ui.separator();
            // Captured links get remaining page height (was capped 320px).
            let list_h = ui.available_height().max(160.0);
            if self.grab_files.is_empty() {
                ui.label(
                    egui::RichText::new(if self.grab_busy {
                        "Working..."
                    } else {
                        "No files found yet."
                    })
                    .color(theme::TEXT_FAINT)
                    .small(),
                );
            }
            // Sheet-style grabber table: checkbox 28 | URL remainder | Size 76 | Type 90. Double-click row toggles.
            egui::ScrollArea::vertical()
                .id_salt("grab_v")
                .max_height(list_h)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    egui::ScrollArea::horizontal()
                        .id_salt("grab_h")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            let avail = ui.available_width().max(200.0);
                            // Grab cols [check, Name, URL, Size, Type]; stored vec uses
                            // Name at idx 0 (helpers clamp idx0 to NAME_MIN). Old 3-wide
                            // saves [URL, Size, Type] migrate: prepend Name width.
                            const CHECK_W: f32 = 28.0;
                            const NAME_W: f32 = 140.0;
                            const SIZE_T_W: [f32; 2] = [76.0, 90.0];
                            let stored: Option<Vec<f32>> = match self.grab_widths.as_deref() {
                                Some(s) if s.len() == 4 => Some(s.to_vec()),
                                Some(s) if s.len() == 3 => Some(vec![NAME_W, s[0], s[1], s[2]]),
                                _ => None,
                            };
                            let mut gcols: Vec<f32> = match stored {
                                Some(s) => {
                                    let mut v: Vec<f32> = s
                                        .iter()
                                        .enumerate()
                                        .map(|(i, w)| {
                                            if i <= 1 {
                                                w.max(NAME_MIN)
                                            } else {
                                                w.max(COL_MIN)
                                            }
                                        })
                                        .collect();
                                    let total: f32 =
                                        v.iter().sum::<f32>() + CHECK_W + COL_GAP * 4.0;
                                    if total <= avail {
                                        v[1] = (avail
                                            - v[0]
                                            - v[2..].iter().sum::<f32>()
                                            - CHECK_W
                                            - COL_GAP * 4.0)
                                            .max(NAME_MIN);
                                    }
                                    v
                                }
                                None => vec![
                                    NAME_W,
                                    (avail
                                        - NAME_W
                                        - SIZE_T_W.iter().sum::<f32>()
                                        - CHECK_W
                                        - COL_GAP * 4.0)
                                        .max(NAME_MIN),
                                    SIZE_T_W[0],
                                    SIZE_T_W[1],
                                ],
                            };
                            // Display widths in cell order: [check, Name, URL, Size, Type].
                            let disp = [CHECK_W, gcols[0], gcols[1], gcols[2], gcols[3]];
                            let row_w = disp.iter().sum::<f32>() + COL_GAP * 4.0;
                            let (hdr_rect, _) = ui.allocate_exact_size(
                                egui::vec2(row_w, ROW_H + 2.0),
                                egui::Sense::hover(),
                            );
                            sheet_header(ui, hdr_rect);
                            {
                                let mut c = ui.new_child(
                                    egui::UiBuilder::new()
                                        .max_rect(hdr_rect)
                                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                                );
                                let ui = &mut c;
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = COL_GAP;
                                    cell(ui, disp[0], |ui| ui.label(theme::table_header_text("")));
                                    // Sortable headers: click toggles direction.
                                    for (ci, (label, col)) in
                                        [("Name", 0u8), ("URL", 1u8), ("Size", 2u8), ("Type", 3u8)]
                                            .iter()
                                            .enumerate()
                                    {
                                        let arrow = if self.grab_sort.0 == *col {
                                            if self.grab_sort.1 {
                                                " \u{25b2}"
                                            } else {
                                                " \u{25bc}"
                                            }
                                        } else {
                                            ""
                                        };
                                        cell(ui, disp[ci + 1], |ui| {
                                            let r = ui.add(
                                                egui::Label::new(theme::table_header_text(
                                                    &format!("{label}{arrow}"),
                                                ))
                                                .truncate()
                                                .selectable(false)
                                                .sense(egui::Sense::click()),
                                            );
                                            if r.clicked() {
                                                if self.grab_sort.0 == *col {
                                                    self.grab_sort.1 = !self.grab_sort.1;
                                                } else {
                                                    self.grab_sort = (*col, true);
                                                }
                                            }
                                            if r.hovered() {
                                                ui.ctx().set_cursor_icon(
                                                    egui::CursorIcon::PointingHand,
                                                );
                                            }
                                        });
                                    }
                                });
                            }
                            // Resize handles operate on gcols [Name, URL, Size, Type]; check col fixed.
                            let hdr_shifted = egui::Rect::from_min_size(
                                egui::pos2(hdr_rect.min.x + CHECK_W + COL_GAP, hdr_rect.min.y),
                                egui::vec2(hdr_rect.width() - CHECK_W - COL_GAP, hdr_rect.height()),
                            );
                            if col_resize_handles(ui, hdr_shifted, &mut gcols, false) {
                                self.grab_widths = Some(gcols.clone());
                            }
                            let disp = [CHECK_W, gcols[0], gcols[1], gcols[2], gcols[3]];
                            let n = self.grab_files.len();
                            // Sorted view order (stable; selection lives on items).
                            let (gcol, gasc) = self.grab_sort;
                            let mut order: Vec<usize> = (0..n).collect();
                            order.sort_by(|&a, &b| {
                                let (fa, fb) = (&self.grab_files[a], &self.grab_files[b]);
                                let url_ord = fa.url.to_lowercase().cmp(&fb.url.to_lowercase());
                                let ord = match gcol {
                                    0 => grab_name(&fa.url)
                                        .to_lowercase()
                                        .cmp(&grab_name(&fb.url).to_lowercase())
                                        .then(url_ord),
                                    1 => url_ord,
                                    2 => fa.size.unwrap_or(0).cmp(&fb.size.unwrap_or(0)),
                                    _ => fa.mime.cmp(&fb.mime),
                                };
                                if gasc {
                                    ord
                                } else {
                                    ord.reverse()
                                }
                            });
                            for (idx, &fi) in order.iter().enumerate() {
                                let f = &mut self.grab_files[fi];
                                let (rect, resp) = ui.allocate_exact_size(
                                    egui::vec2(row_w, ROW_H + 2.0),
                                    egui::Sense::click(),
                                );
                                sheet_row(ui, rect, false, idx, idx + 1 == n, false);
                                let mut c = ui.new_child(
                                    egui::UiBuilder::new()
                                        .max_rect(rect)
                                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                                );
                                {
                                    let ui = &mut c;
                                    ui.horizontal(|ui| {
                                        ui.spacing_mut().item_spacing.x = COL_GAP;
                                        cell(ui, disp[0], |ui| {
                                            let mut sel = f.selected;
                                            ui.checkbox(&mut sel, "");
                                            if sel != f.selected {
                                                f.selected = sel;
                                            }
                                        });
                                        cell(ui, disp[1], |ui| {
                                            let name = grab_name(&f.url);
                                            ui.add(
                                                egui::Label::new(
                                                    egui::RichText::new(&name).color(theme::TEXT),
                                                )
                                                .truncate()
                                                .selectable(false),
                                            )
                                            .on_hover_text(name)
                                        });
                                        cell(ui, disp[2], |ui| {
                                            ui.add(
                                                egui::Label::new(
                                                    egui::RichText::new(&f.url).color(theme::TEXT),
                                                )
                                                .truncate()
                                                .selectable(false),
                                            )
                                            .on_hover_text(format!(
                                                "{}\nfrom: {}",
                                                f.url, f.page_url
                                            ))
                                        });
                                        cell(ui, disp[3], |ui| {
                                            ui.add(
                                                egui::Label::new(
                                                    egui::RichText::new(
                                                        f.size
                                                            .map(fmt_bytes)
                                                            .unwrap_or_else(|| "—".into()),
                                                    )
                                                    .color(theme::TEXT_MUTED)
                                                    .small(),
                                                )
                                                .truncate()
                                                .selectable(false),
                                            )
                                        });
                                        cell(ui, disp[4], |ui| {
                                            ui.add(
                                                egui::Label::new(
                                                    egui::RichText::new(&f.mime)
                                                        .color(theme::TEXT_FAINT)
                                                        .small(),
                                                )
                                                .truncate()
                                                .selectable(false),
                                            )
                                        });
                                    });
                                }
                                if resp.double_clicked() {
                                    f.selected = !f.selected;
                                }
                                resp.context_menu(|ui| {
                                    if ui.button("Copy filename").clicked() {
                                        ui.output_mut(|o| {
                                            o.copied_text = grab_name(&f.url);
                                        });
                                        ui.close_menu();
                                    }
                                    if ui.button("Copy URL").clicked() {
                                        ui.output_mut(|o| o.copied_text = f.url.clone());
                                        ui.close_menu();
                                    }
                                });
                            }
                        });
                });
        });
        ui.add_space(6.0);
        ui.collapsing("Project JSON", |ui| {
            ui.add(
                egui::TextEdit::multiline(&mut self.grab_project_json)
                    .desired_width(f32::INFINITY)
                    .font(egui::TextStyle::Monospace),
            );
        });
    }

    fn ui_options(&mut self, ui: &mut egui::Ui) {
        ui.heading("Options");
        ui.label(
            egui::RichText::new(&self.options_notice)
                .color(theme::TEAL_500)
                .small(),
        );
        ui.add_space(6.0);

        // Section navigation: icon + label in one control. Wraps on narrow windows.
        ui.horizontal_wrapped(|ui| {
            for (i, name, ic) in [
                (0u8, "General", Icon::Settings),
                (1, "File types", Icon::File),
                (2, "Save to", Icon::FolderOpen),
                (3, "Downloads", Icon::Download),
                (4, "Connection", Icon::Globe),
                (5, "Proxy", Icon::Link),
                (6, "Browser", Icon::Globe),
                (8, "Debrid", Icon::Link),
                (9, "mpv", Icon::Play),
                (7, "About", Icon::Info),
            ] {
                if icon_btn(ui, ic, name, self.options_tab == i, 104.0).clicked() {
                    self.options_tab = i;
                }
            }
        });
        ui.add_space(8.0);

        let mut settings = {
            let g = crate::lock_unpoison(&self.rt);
            g.state.settings.clone()
        };
        let before = serde_json::to_string(&settings).unwrap_or_default();
        let before_autostart = settings.autostart;
        let mut repair_hosts = false;
        let mut remove_hosts = false;
        let mut update_ytdlp = false;
        let mut test_provider: Option<crate::debrid::Provider> = None;
        let mut refresh_provider: Option<crate::debrid::Provider> = None;
        let mut stale_provider: Option<crate::debrid::Provider> = None;
        let mut export_settings_flag = false;
        let mut import_settings_flag = false;

        theme::panel().show(ui, |ui| {
            ui.set_width(ui.available_width());
            match self.options_tab {
                0 => {
                    section(ui, "General");
                    // Checkboxes aligned under label col (120 + 12 gap).
                    ui.horizontal(|ui| {
                        ui.add_space(LABEL_W + 12.0);
                        ui.vertical(|ui| {
                            ui.checkbox(&mut settings.start_minimized, "Start minimized");
                            ui.checkbox(&mut settings.notifications, "Desktop notifications");
                            ui.checkbox(&mut settings.close_to_tray, "Close to tray");
                            ui.checkbox(&mut settings.clipboard_monitor, "Start downloads for URLs copied to clipboard");
                            ui.checkbox(&mut settings.autostart, "Launch on OS startup");
                        });
                    });
                    ui.add_space(6.0);
                    section(ui, "Download directory");
                    form_grid("opts-dir-grid").show(ui, |ui| {
                        form_label(ui, "Directory");
                        ui.horizontal_wrapped(|ui| {
                            ui.add(egui::Label::new(egui::RichText::new(settings.download_dir.display().to_string()).monospace().small()).truncate()).on_hover_text(settings.download_dir.display().to_string());
                            if icon_btn(ui, Icon::FolderOpen, "Browse", false, 96.0).clicked() { if let Some(path) = rfd::FileDialog::new().pick_folder() { settings.download_dir = path; } }
                        });
                        ui.end_row();
                    });
                    ui.label(egui::RichText::new(format!("Config directory: {}", platform::config_dir().display())).color(theme::TEXT_FAINT).small());
                    ui.add_space(10.0);
                    section(ui, "Backup (settings only)");
                    ui.horizontal_wrapped(|ui| {
                        if icon_btn(ui, Icon::Download, "Export Settings", false, 140.0).clicked() { export_settings_flag = true; }
                        if icon_btn(ui, Icon::FolderOpen, "Import Settings", false, 140.0).clicked() { import_settings_flag = true; }
                    });
                    ui.label(egui::RichText::new("Exported file contains API tokens and passwords in plain text. Keep it private. Import replaces all settings; downloads and queues unchanged.").color(theme::ERR).small());
                }
                1 => {
                    section(ui, "File types");
                    form_grid("opts-types-grid").show(ui, |ui| {
                        for (cat, exts) in [("Compressed", "zip rar 7z tar gz bz2 xz"), ("Documents", "pdf doc docx xls xlsx ppt pptx txt epub"), ("Music", "mp3 flac ogg wav m4a aac"), ("Programs", "exe msi dmg pkg deb rpm appimage"), ("Video", "mp4 mkv avi mov webm ts m3u8 mpd")] {
                            form_label(ui, cat);
                            ui.label(egui::RichText::new(exts).color(theme::TEXT_MUTED).monospace().small());
                            ui.end_row();
                        }
                    });
                    ui.label(egui::RichText::new("Category comes from the file name when a download is added. The sidebar filters by category.").color(theme::TEXT_FAINT).small());
                    ui.add_space(10.0);
                    section(ui, "Sidebar count badges");
                    ui.horizontal(|ui| {
                        ui.add_space(LABEL_W + 12.0);
                        ui.checkbox(&mut settings.nav_count_badges, "Colored count badges");
                    });
                    if settings.nav_count_badges {
                    ui.add_space(6.0);
                    section(ui, "Category colors");
                    form_grid("opts-cat-colors-grid").show(ui, |ui| {
                        for name in ["All Downloads", "Compressed", "Documents", "Programs", "Video", "Audio", "General", "Unfinished", "Finished"] {
                            badge_color_row(ui, name, name, theme::category_color(name), &mut settings.category_colors);
                        }
                    });
                    ui.add_space(6.0);
                    section(ui, "Type colors");
                    form_grid("opts-type-colors-grid").show(ui, |ui| {
                        for sel in TypeSel::ALL {
                            badge_color_row(ui, sel.label(), sel.key(), theme::type_key_color(sel.key()), &mut settings.type_colors);
                        }
                    });
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.add_space(LABEL_W + 12.0);
                        if theme::ghost_btn(ui, "Reset badge colors").clicked() {
                            settings.category_colors.clear();
                            settings.type_colors.clear();
                        }
                    });
                    } // settings.nav_count_badges
                }
                2 => {
                    section(ui, "Save to");
                    form_grid("opts-saveto-grid").show(ui, |ui| {
                        form_label(ui, "Directory");
                        ui.horizontal_wrapped(|ui| {
                            ui.add(egui::Label::new(egui::RichText::new(settings.download_dir.display().to_string()).monospace().small()).truncate()).on_hover_text(settings.download_dir.display().to_string());
                            if icon_btn(ui, Icon::FolderOpen, "Browse", false, 96.0).clicked() { if let Some(path) = rfd::FileDialog::new().pick_folder() { settings.download_dir = path; } }
                        });
                        ui.end_row();
                    });
                    ui.label(egui::RichText::new("Each download stores its destination when it is added.").color(theme::TEXT_FAINT).small());
                }
                3 => {
                    section(ui, "Downloads");
                    ui.add(egui::Slider::new(&mut settings.max_connections, 1..=32).text("Connections per file"));
                    ui.add(egui::Slider::new(&mut settings.global_max_concurrent, 1..=16).text("Simultaneous downloads"));
                    ui.add(egui::Slider::new(&mut settings.speed_limit_kbps, 0..=100_000).text("Speed limit KB/s (0 = unlimited)"));
                    ui.label(egui::RichText::new("Double-click a download to open its progress window.").color(theme::TEXT_FAINT).small());
                    ui.add_space(8.0);
                    section(ui, "Rename template");
                    form_grid("opts-rename-grid").show(ui, |ui| {
                        form_label(ui, "Template");
                        ui.add(
                            egui::TextEdit::singleline(&mut settings.folder_template)
                                .hint_text("{name}-{date}{ext}")
                                .desired_width(f32::INFINITY),
                        );
                        ui.end_row();
                    });
                    ui.label(egui::RichText::new("Tokens: {name} stem, {ext} with dot, {date} YYYY-MM-DD, {host}, {category}. Empty keeps original name. New downloads only.").color(theme::TEXT_FAINT).small());
                    ui.horizontal(|ui| {
                        ui.add_space(LABEL_W + 12.0);
                        ui.checkbox(&mut settings.category_subfolders, "Category subfolders");
                    });
                    ui.label(egui::RichText::new("Save new downloads under download folder / category (Video, Music, …). General stays in download folder.").color(theme::TEXT_FAINT).small());
                }
                4 => {
                    section(ui, "Connection");
                    ui.add(egui::Slider::new(&mut settings.max_connections, 1..=32).text("Connections per file"));
                    ui.label(egui::RichText::new("Connect timeout 15 seconds. Read timeout 60 seconds. A failed segment retries with backoff.").color(theme::TEXT_FAINT).small());
                    ui.add_space(10.0);
                    section(ui, "BitTorrent");
                    form_grid("opts-bt-grid").show(ui, |ui| {
                        form_label(ui, "Listen port");
                        ui.horizontal(|ui| {
                            ui.add_sized([DRAG_W, theme::BTN_H], drag(&mut settings.torrent_listen_port).range(0..=65535));
                            ui.label("(0 = auto)");
                        });
                        ui.end_row();
                    });
                    ui.horizontal(|ui| {
                        ui.add_space(LABEL_W + 12.0);
                        ui.checkbox(&mut settings.torrent_seed_after_complete, "Seed after download completes");
                    });
                    ui.label(egui::RichText::new("Applies on restart. Speed limit above also caps torrent upload + download.").color(theme::TEXT_FAINT).small());
                }
                5 => {
                    section(ui, "Proxy / SOCKS");
                    form_grid("opts-proxy-grid").show(ui, |ui| {
                        form_label(ui, "Proxy");
                        ui.add(
                            egui::TextEdit::singleline(&mut settings.proxy_url)
                                .hint_text("http://user:pass@host:port or socks5://host:port")
                                .desired_width(f32::INFINITY),
                        );
                        ui.end_row();
                    });
                    ui.label(egui::RichText::new("Leave empty for a direct connection. New downloads use the saved proxy.").color(theme::TEXT_FAINT).small());
                    ui.add_space(10.0);
                    section(ui, "NNTP (Usenet / NZB)");
                    form_grid("opts-nntp-grid").show(ui, |ui| {
                        form_label(ui, "Server");
                        ui.add(
                            egui::TextEdit::singleline(&mut settings.nntp_host)
                                .hint_text("news.example.com")
                                .desired_width(f32::INFINITY),
                        );
                        ui.end_row();
                        form_label(ui, "Port");
                        ui.horizontal(|ui| {
                            ui.add_sized([DRAG_W, theme::BTN_H], drag(&mut settings.nntp_port).range(1..=65535));
                            ui.checkbox(&mut settings.nntp_tls, "TLS");
                        });
                        ui.end_row();
                        form_label(ui, "Username");
                        ui.add(fill_edit(&mut settings.nntp_user));
                        ui.end_row();
                        form_label(ui, "Password");
                        ui.add(
                            egui::TextEdit::singleline(&mut settings.nntp_pass)
                                .password(true)
                                .desired_width(f32::INFINITY),
                        );
                        ui.end_row();
                    });
                    ui.add(egui::Slider::new(&mut settings.nntp_connections, 1..=64).text("Connections"));
                    ui.label(egui::RichText::new("Provider connection limit applies per NZB download.").color(theme::TEXT_FAINT).small());
                    ui.label(egui::RichText::new("Add an .nzb URL (or NZB file path) as a download to fetch via NNTP. NNTP connections separate from HTTP Connections per file. Higher = faster, respect provider limit. PAR2 repair included when NZB ships .par2 files; unpack not included.").color(theme::TEXT_FAINT).small());
                }
                6 => {
                    section(ui, "Browser integration");
                    ui.horizontal_wrapped(|ui| {
                        action_group(ui, "Native host", |ui| {
                            if icon_btn(ui, Icon::Check, "Repair integration", false, 148.0).clicked() { repair_hosts = true; }
                            if icon_btn(ui, Icon::Delete, "Remove integration", false, 156.0).clicked() { remove_hosts = true; }
                        });
                    });
                    ui.label(egui::RichText::new(format!("Chromium extension ID: {}", downloadcraft_lib::platform::CHROME_EXT_ID)).color(theme::TEXT_FAINT).small().monospace());
                    ui.label(egui::RichText::new("Load extensions/chromium in Chromium browsers or extensions/firefox in Firefox. Native host: webcraft-host.").color(theme::TEXT_FAINT).small());
                    ui.add_space(10.0);
                    section(ui, "Browser capture filter");
                    form_grid("opts_capture_filter").show(ui, |ui| {
                        form_label(ui, "Allowed exts");
                        ui.add(
                            egui::TextEdit::singleline(&mut settings.capture_exts)
                                .hint_text("mp4,zip,pdf,mp3")
                                .desired_width(f32::INFINITY),
                        );
                        ui.end_row();
                        form_label(ui, "Min size MB");
                        ui.add_sized([DRAG_W, theme::BTN_H], drag(&mut settings.capture_min_mb).range(0..=100_000));
                        ui.end_row();
                    });
                    ui.label(egui::RichText::new("Empty extensions = all types. Size filter only skips files with known size below limit; unknown size still captured. Explicit browser sends (context menu, video panel) bypass filter.").color(theme::TEXT_FAINT).small());
                    ui.add_space(10.0);
                    section(ui, "yt-dlp (video pages)");
                    ui.horizontal(|ui| {
                        ui.add_space(LABEL_W + 12.0);
                        ui.vertical(|ui| {
                            ui.checkbox(&mut settings.ytdlp_auto_update, "Auto-update yt-dlp");
                            if ghost_btn_w(ui, "Update yt-dlp now").clicked() { update_ytdlp = true; }
                        });
                    });
                    ui.label(egui::RichText::new("Downloads video-page URLs (YouTube, Vimeo, ...). Binary: bundled next to exe, config bin/, or PATH; fetched on first use.").color(theme::TEXT_FAINT).small());
                }
                8 => {
                    section(ui, "Debrid providers");
                    ui.label(egui::RichText::new("Magnets and supported hoster links resolve to direct HTTP downloads. Failure falls back to local torrent / direct HTTP. Tokens stored plain in state.json (like NNTP creds), masked here, never logged. Resolved links expire/IP-bound: tasks keep original URL and re-resolve on every start.").color(theme::TEXT_FAINT).small());
                    form_grid("opts-debrid-pref").show(ui, |ui| {
                        form_label(ui, "Preferred");
                        let mut pref = crate::debrid::Provider::from_id(&settings.debrid_preferred);
                        egui::ComboBox::from_id_salt("debrid-pref")
                            .selected_text(match pref { Some(p) => p.label(), None => "Auto (RD, AD, TorBox order)" })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut pref, None, "Auto");
                                for p in crate::debrid::ALL {
                                    ui.selectable_value(&mut pref, Some(p), p.label());
                                }
                            });
                        settings.debrid_preferred = pref.map(|p| p.id().to_string()).unwrap_or_default();
                        ui.end_row();
                    });
                    self.debrid_ctx = Some(ui.ctx().clone());
                    if let Ok(mut g) = self.debrid_hosts_slot.try_lock() {
                        if let Some((pid, list)) = g.take() {
                            self.debrid_ui.hosts.insert(pid, list);
                        }
                    }
                    if let Ok(mut g) = self.debrid_slot.try_lock() {
                        if let Some((tag, res)) = g.take() {
                            if let Some((pid, kind)) = tag.split_once(':') {
                                self.debrid_ui.busy.remove(pid);
                                match (kind, res) {
                                    ("account", Ok(txt)) => { self.debrid_ui.account.insert(pid.to_string(), txt); self.debrid_ui.error.remove(pid); }
                                    ("hosts", Ok(_)) => { self.debrid_ui.checked_at.insert(pid.to_string(), crate::debrid::now_unix()); self.debrid_ui.error.remove(pid); }
                                    (_, Err(e)) => { self.debrid_ui.error.insert(pid.to_string(), e); }
                                    _ => {}
                                }
                            }
                        }
                    }
                    for p in crate::debrid::ALL {
                        let pid = p.id();
                        section(ui, p.label());
                        let cfg = match p {
                            crate::debrid::Provider::Rd => &mut settings.debrid_rd,
                            crate::debrid::Provider::Ad => &mut settings.debrid_ad,
                            crate::debrid::Provider::Tb => &mut settings.debrid_tb,
                        };
                        ui.horizontal(|ui| {
                            ui.checkbox(&mut cfg.enabled, "Enabled");
                            ui.checkbox(&mut cfg.use_for_hosters, "Use for hosters");
                            ui.checkbox(&mut cfg.use_for_magnets, "Use for magnets");
                            ui.checkbox(&mut cfg.cached_only, "Magnets: cached only");
                        });
                        form_grid(&format!("opts-debrid-{pid}")).show(ui, |ui| {
                            form_label(ui, "API token");
                            ui.add(egui::TextEdit::singleline(&mut cfg.api_key).password(true).desired_width(f32::INFINITY));
                            ui.end_row();
                        });
                        ui.label(egui::RichText::new(match p {
                            crate::debrid::Provider::Rd => "Get token: real-debrid.com/apitoken",
                            crate::debrid::Provider::Ad => "Get token: alldebrid.com/apikeys",
                            crate::debrid::Provider::Tb => "Get token: torbox.app -> Settings / API",
                        }).color(theme::TEXT_FAINT).small());
                        let busy = self.debrid_ui.busy.contains(pid);
                        ui.horizontal(|ui| {
                            if ui.add_enabled(!busy && cfg.usable(), egui::Button::new(if busy { "Testing..." } else { "Test connection" })).clicked() {
                                test_provider = Some(p);
                            }
                            if ui.add_enabled(!busy && cfg.usable(), egui::Button::new("Refresh hosters")).clicked() {
                                refresh_provider = Some(p);
                            }
                            if let Some(ts) = self.debrid_ui.checked_at.get(pid) {
                                ui.label(egui::RichText::new(format!("hosters checked {}", crate::debrid::fmt_date(*ts))).color(theme::TEXT_FAINT).small());
                            }
                        });
                        if let Some(a) = self.debrid_ui.account.get(pid).cloned() {
                            ui.label(&a);
                            if a.contains("NOT premium") {
                                ui.label(egui::RichText::new("Warning: account not premium - debrid downloads will fail until renewed.").color(theme::ERR).small());
                            }
                        }
                        if let Some(e) = self.debrid_ui.error.get(pid).cloned() {
                            ui.label(egui::RichText::new(e).color(theme::ERR).small());
                        }
                        if let Some(list) = self.debrid_ui.hosts.get(pid).cloned() {
                            let up = list.iter().filter(|h| h.supported && h.up).count();
                            ui.label(egui::RichText::new(format!("{} hosters, {} up", list.len(), up)).color(theme::TEXT_FAINT).small());
                            egui::ScrollArea::vertical().id_salt(format!("debrid-hosts-{pid}")).max_height(140.0).show(ui, |ui| {
                                for h in &list {
                                    let (txt, col) = if !h.supported { ("unsupported", theme::TEXT_FAINT) } else if h.up { ("up", theme::OK) } else { ("down", theme::ERR) };
                                    ui.horizontal(|ui| {
                                        ui.label(egui::RichText::new(txt).color(col).small());
                                        ui.label(format!("{} ({})", h.name, h.domain));
                                    });
                                }
                            });
                        }
                        if cfg.usable() && !busy && !self.debrid_ui.stale_checked.contains(pid) {
                            let stale = self.debrid_ui.checked_at.get(pid).map(|t| crate::debrid::now_unix() - t > 600).unwrap_or(true);
                            if stale {
                                stale_provider = Some(p);
                            }
                        }
                    }
                    ui.label(egui::RichText::new("Hoster status re-checked on Options open when stale (>10 min) and via Refresh. Multi-file magnets: Add dialog resolves via debrid and creates one task per file; starting a single magnet task directly only works for single-file magnets (falls back to local torrent otherwise).").color(theme::TEXT_FAINT).small());
                }
                9 => {
                    section(ui, "mpv player");
                    form_grid("opts-mpv-grid").show(ui, |ui| {
                        form_label(ui, "Path");
                        ui.horizontal_wrapped(|ui| {
                            ui.add(egui::TextEdit::singleline(&mut settings.mpv_path).hint_text("empty = auto-detect beside exe / config bin / PATH").desired_width(f32::INFINITY));
                            if icon_btn(ui, Icon::FolderOpen, "Browse", false, 96.0).clicked() {
                                if let Some(path) = rfd::FileDialog::new().pick_file() {
                                    settings.mpv_path = path.display().to_string();
                                }
                            }
                        });
                        ui.end_row();
                        form_label(ui, "Volume");
                        ui.add(egui::Slider::new(&mut settings.mpv_volume, 0..=100).text("Volume"));
                        ui.end_row();
                        form_label(ui, "Speed");
                        ui.add_sized([DRAG_W, theme::BTN_H], drag(&mut settings.mpv_speed).range(0.25..=4.0).speed(0.05));
                        ui.end_row();
                        form_label(ui, "Start position");
                        ui.add(egui::TextEdit::singleline(&mut settings.mpv_start).hint_text("90 or 00:01:30 or 50%").desired_width(f32::INFINITY));
                        ui.end_row();
                        form_label(ui, "Hardware decoding");
                        egui::ComboBox::from_id_salt("mpv-hwdec")
                            .selected_text(if settings.mpv_hwdec.is_empty() { "Auto" } else { settings.mpv_hwdec.as_str() })
                            .show_ui(ui, |ui| {
                                for opt in ["", "auto-safe", "yes", "no"] {
                                    ui.selectable_value(&mut settings.mpv_hwdec, opt.to_string(), if opt.is_empty() { "Auto" } else { opt });
                                }
                            });
                        ui.end_row();
                        form_label(ui, "Subtitles");
                        egui::ComboBox::from_id_salt("mpv-sub-auto")
                            .selected_text(if settings.mpv_sub_auto.is_empty() { "Auto" } else { settings.mpv_sub_auto.as_str() })
                            .show_ui(ui, |ui| {
                                for opt in ["", "all", "fuzzy", "exact", "no"] {
                                    ui.selectable_value(&mut settings.mpv_sub_auto, opt.to_string(), if opt.is_empty() { "Auto" } else { opt });
                                }
                            });
                        ui.end_row();
                        form_label(ui, "Cache seconds");
                        ui.add_sized([DRAG_W, theme::BTN_H], drag(&mut settings.mpv_cache_secs).range(0..=600));
                        ui.end_row();
                        form_label(ui, "Extra flags");
                        ui.add(egui::TextEdit::singleline(&mut settings.mpv_extra_args).hint_text("--profile=fast (advanced)").desired_width(f32::INFINITY));
                        ui.end_row();
                    });
                    ui.horizontal(|ui| {
                        ui.add_space(LABEL_W + 12.0);
                        ui.vertical(|ui| {
                            ui.checkbox(&mut settings.mpv_fullscreen, "Fullscreen");
                            ui.checkbox(&mut settings.mpv_keep_open, "Keep open (--keep-open)");
                            ui.checkbox(&mut settings.mpv_loop, "Loop file");
                            ui.checkbox(&mut settings.mpv_cache, "Cache");
                        });
                    });
                    ui.label(egui::RichText::new("Flags apply to Play in mpv on the Streams page. Changes save automatically like other options.").color(theme::TEXT_FAINT).small());
                }
                _ => {
                    section(ui, "About");
                    ui.label(egui::RichText::new("App updates").strong());
                    // ponytail: manual check only (no silent startup check / self-replace in v1);
                    // download lands as a normal task, user runs the installer.
                    let checking = matches!(self.update_check, Some(CheckOutcome::Checking));
                    if ui
                        .add_enabled(!checking, egui::Button::new("Check updates"))
                        .clicked()
                    {
                        self.update_check = Some(CheckOutcome::Checking);
                        let rt = self.tokio_rt.clone();
                        let ctx2 = ui.ctx().clone();
                        let shared = self.update_slot.clone();
                        std::thread::spawn(move || {
                            let res = rt.block_on(update::fetch_latest());
                            let outcome = match res {
                                Ok(rel) => update::evaluate(
                                    &rel,
                                    env!("CARGO_PKG_VERSION"),
                                    std::env::consts::OS,
                                    std::env::consts::ARCH,
                                ),
                                Err(e) => CheckOutcome::Failed(e),
                            };
                            *shared.lock().unwrap() = Some(outcome);
                            ctx2.request_repaint();
                        });
                    }
                    if let Some(slot) = self.update_slot.lock().unwrap().take() {
                        self.update_check = Some(slot);
                    }
                    // Clone: Download button needs &mut self (start_task).
                    match self.update_check.clone() {
                        None => {}
                        Some(CheckOutcome::Checking) => {
                            ui.label("Checking for updates...");
                        }
                        Some(CheckOutcome::UpToDate { latest }) => {
                            ui.colored_label(
                                egui::Color32::from_rgb(80, 200, 120),
                                format!("Up to date (latest {latest})"),
                            );
                        }
                        Some(CheckOutcome::Failed(e)) => {
                            ui.colored_label(
                                egui::Color32::from_rgb(230, 90, 90),
                                format!("Update check failed: {e}"),
                            );
                        }
                        Some(CheckOutcome::Available { latest, asset }) => {
                            ui.colored_label(
                                egui::Color32::from_rgb(240, 190, 80),
                                format!("New version {latest} available"),
                            );
                            match asset {
                                Some((name, url)) => {
                                    ui.label(format!("Asset: {name}"));
                                    if ui.button("Download update").clicked() {
                                        let id = crate::lock_unpoison(&self.rt)
                                            .add_url(&url, "main", "", "");
                                        // Never self-replace: plain download task.
                                        self.start_task(&id);
                                        crate::lock_unpoison(&self.rt)
                                            .log(format!("Update download queued: {name}"));
                                    }
                                }
                                None => {
                                    ui.label("No installer asset for this platform.");
                                }
                            }
                        }
                    }
                    ui.separator();
                    ui.label(format!("webcraft v{}", env!("CARGO_PKG_VERSION")));
                    ui.label("Cross-platform download manager written in Rust with egui.");
                    ui.label(egui::RichText::new("No proprietary code, binaries, icons, or licensing logic.").color(theme::TEXT_FAINT).small());
                }
            }
        });

        if let Some(p) = test_provider {
            self.spawn_debrid_probe(p, true, &settings);
        }
        if let Some(p) = refresh_provider.or(stale_provider) {
            self.spawn_debrid_probe(p, false, &settings);
            self.debrid_ui.stale_checked.insert(p.id().to_string());
        }
        if update_ytdlp {
            self.options_notice = "Updating yt-dlp...".into();
            let rt = self.rt.clone();
            let rt_b = self.tokio_rt.clone();
            std::thread::spawn(move || {
                let res = rt_b.block_on(async move {
                    match ytdlp::ensure_ytdlp().await {
                        Ok(p) => tokio::task::spawn_blocking(move || ytdlp::ytdlp_update(&p))
                            .await
                            .map_err(|e| e.to_string())
                            .and_then(|r| r),
                        Err(e) => Err(e),
                    }
                });
                let msg = match res {
                    Ok(t) => format!("yt-dlp update: {}", t.lines().last().unwrap_or("ok")),
                    Err(e) => format!("yt-dlp update failed: {e}"),
                };
                let mut g = crate::lock_unpoison(&rt);
                g.messages.push(msg);
                if g.messages.len() > 200 {
                    g.messages.drain(..50);
                }
            });
        }
        if repair_hosts {
            let mut g = crate::lock_unpoison(&self.rt);
            match platform::register_native_hosts(&crate::sibling_host_path()) {
                Ok(paths) => {
                    self.options_notice = format!(
                        "Browser integration repaired. Registered {} host manifests.",
                        paths.len()
                    );
                    g.log(self.options_notice.clone());
                }
                Err(e) => {
                    self.options_notice = format!("Browser integration failed: {e}");
                    g.log(self.options_notice.clone());
                }
            }
            let _ = platform::register_protocol();
        }
        if remove_hosts {
            let _ = platform::unregister_native_hosts();
            let _ = platform::set_autostart(false);
            settings.autostart = false;
            self.options_notice = "Browser integration and autostart removed.".into();
            crate::lock_unpoison(&self.rt).log(self.options_notice.clone());
        }

        // Export/import: import swaps `settings` before compare below, so existing
        // persist + autostart/capture-filter/speed-limit path applies. Errors leave it untouched.
        if export_settings_flag {
            if let Some(p) = rfd::FileDialog::new()
                .set_title("Export Settings")
                .add_filter("Settings JSON", &["json"])
                .set_file_name("webcraft-settings.json")
                .save_file()
            {
                let snapshot = { crate::lock_unpoison(&self.rt).state.settings.clone() };
                match crate::model::settings_export_json(&snapshot)
                    .map_err(|e| e.to_string())
                    .and_then(|json| std::fs::write(&p, json).map_err(|e| e.to_string()))
                {
                    Ok(()) => {
                        self.options_notice = format!("Settings exported to {}", p.display());
                        crate::lock_unpoison(&self.rt).log("Settings exported.");
                    }
                    Err(e) => {
                        self.options_notice = format!("Export failed: {e}");
                    }
                }
            }
        }
        if import_settings_flag {
            if let Some(p) = rfd::FileDialog::new()
                .set_title("Import Settings")
                .add_filter("Settings JSON", &["json"])
                .pick_file()
            {
                let parsed = std::fs::read_to_string(&p)
                    .map_err(|e| format!("cannot read file: {e}"))
                    .and_then(|txt| crate::model::parse_settings_import(&txt));
                match parsed {
                    Ok(imported) => {
                        settings = imported;
                        self.options_notice =
                            "Settings imported. Restart may be needed for startup-only settings."
                                .into();
                    }
                    Err(e) => {
                        self.options_notice = format!("Import failed: {e}. Settings unchanged.");
                    }
                }
            }
        }
        // Apply immediately. Options previously looked inert because edits only
        // persisted from a Save button that could sit below the visible page.
        if serde_json::to_string(&settings).unwrap_or_default() != before {
            if settings.autostart != before_autostart {
                let _ = platform::set_autostart(settings.autostart);
            }
            let speed_limit = settings.speed_limit_kbps;
            // Native host reads capture-filter.json on each captured download;
            // desktop owns settings, browser storage never synced.
            crate::platform::write_capture_filter(&settings);
            let limits: Vec<_> = {
                let mut g = crate::lock_unpoison(&self.rt);
                g.state.settings = settings;
                g.save();
                g.controls.values().cloned().collect::<Vec<_>>()
            };
            for c in limits {
                c.limit_kbps.store(speed_limit, Ordering::Relaxed);
            }
            self.options_notice = "Saved.".into();
        }
    }
}

thread_local! { static GRAB_SLOT: std::cell::RefCell<Option<Arc<Mutex<GrabOutcome>>>> = const { std::cell::RefCell::new(None) }; }

fn move_task(tasks: &mut [DownloadTask], id: &str, delta: i64) {
    let mut sorted: Vec<usize> = (0..tasks.len()).collect();
    sorted.sort_by_key(|&i| tasks[i].order);
    let pos = sorted.iter().position(|&i| tasks[i].id == id);
    if let Some(p) = pos {
        let swap_with = (p as i64 + delta).clamp(0, sorted.len() as i64 - 1) as usize;
        if swap_with != p {
            tasks[sorted[p]].order = swap_with as i64;
            tasks[sorted[swap_with]].order = p as i64;
        }
    }
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

#[cfg(test)]
mod progress_tests {
    use super::*;

    #[test]
    fn effective_limit_division() {
        // own wins, even above global
        assert_eq!(effective_limit_kbps(1000, 4, 200), 200);
        // global split
        assert_eq!(effective_limit_kbps(1000, 4, 0), 250);
        assert_eq!(effective_limit_kbps(1000, 1, 0), 1000);
        assert_eq!(effective_limit_kbps(999, 2, 0), 499);
        // unlimited / degenerate
        assert_eq!(effective_limit_kbps(0, 4, 0), 0);
        assert_eq!(effective_limit_kbps(1000, 0, 0), 1000);
        assert_eq!(effective_limit_kbps(0, 4, 50), 50);
    }

    #[test]
    fn capped_wake_ms_deadlines() {
        let now = 1_700_000_000u64;
        let (day, secs) = (20_000i64, 43_200u32); // noon
        let queues = vec![
            Queue {
                id: "main".into(),
                auto_start: true,
                ..Default::default()
            },
            Queue {
                id: "off".into(),
                auto_start: false,
                ..Default::default()
            },
        ];
        let mk = |queue: &str, retry_at: u64| DownloadTask {
            url: "https://ex.com/f".into(),
            queue_id: queue.into(),
            status: TaskStatus::Queued,
            next_retry_at: retry_at,
            ..Default::default()
        };
        let empty: &[DownloadTask] = &[];
        // Nothing armed: idle base untouched (no forced loop).
        assert_eq!(
            capped_wake_ms(5_000, now, day, secs, empty, &queues, &[], true),
            5_000
        );
        // Startable now (ungated / gate passed): next frame.
        assert_eq!(
            capped_wake_ms(5_000, now, day, secs, &[mk("main", 0)], &queues, &[], true),
            0
        );
        assert_eq!(
            capped_wake_ms(
                5_000,
                now,
                day,
                secs,
                &[mk("main", now - 1)],
                &queues,
                &[],
                true
            ),
            0
        );
        // Gate in 30s: capped at gate, not 5s idle cadence remainder.
        assert_eq!(
            capped_wake_ms(
                120_000,
                now,
                day,
                secs,
                &[mk("main", now + 30)],
                &queues,
                &[],
                true
            ),
            30_000
        );
        // Non-auto-start queue ignored.
        assert_eq!(
            capped_wake_ms(5_000, now, day, secs, &[mk("off", 0)], &queues, &[], true),
            5_000
        );
        // Scheduler next start in 60s caps; disabled master ignores it.
        let sch = Schedule {
            id: "s".into(),
            queue_id: "main".into(),
            enabled: true,
            recurrence: Recurrence::Daily,
            start_secs: secs + 60,
            ..Default::default()
        };
        assert_eq!(
            capped_wake_ms(
                120_000,
                now,
                day,
                secs,
                empty,
                &queues,
                &[sch.clone()],
                true
            ),
            60_000
        );
        assert_eq!(
            capped_wake_ms(120_000, now, day, secs, empty, &queues, &[sch], false),
            120_000
        );
    }

    #[test]
    fn toggled_selection_rules() {
        // From all (empty): uncheck one -> rest.
        assert_eq!(toggled_selection(&[], 3, 1, false), Some(vec![0, 2]));
        // Uncheck last selected -> rejected.
        assert_eq!(toggled_selection(&[1], 3, 1, false), None);
        // Check back to full -> canonical empty (all).
        assert_eq!(toggled_selection(&[0, 2], 3, 1, true), Some(vec![]));
        // Plain add/remove.
        assert_eq!(toggled_selection(&[0], 3, 2, true), Some(vec![0, 2]));
        assert_eq!(toggled_selection(&[0, 2], 3, 0, false), Some(vec![2]));
    }

    #[test]
    fn delete_task_files_removes_all_protocol_paths() {
        let dir = std::env::temp_dir().join(format!("wc-del-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".downloadcraft-t1")).unwrap();
        let t = DownloadTask {
            id: "t1".into(),
            filename: "f.bin".into(),
            dest_dir: dir.clone(),
            ..Default::default()
        };
        for p in task_disk_paths(&t) {
            if p.extension().is_some()
                || p.file_name()
                    .is_some_and(|n| n.to_string_lossy().contains('.'))
            {
                let _ = std::fs::write(&p, b"x");
            }
        }
        std::fs::write(dir.join(".downloadcraft-t1/state.json"), b"{}").unwrap();
        std::fs::write(dir.join("keep.txt"), b"k").unwrap();
        delete_task_files(&t);
        for p in task_disk_paths(&t) {
            assert!(!p.exists(), "leftover {}", p.display());
        }
        assert!(dir.join("keep.txt").exists(), "unrelated file deleted");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tray_toggle_label_follows_visibility() {
        assert_eq!(tray_toggle_label(true), "Hide webcraft");
        assert_eq!(tray_toggle_label(false), "Show webcraft");
    }

    fn task(total: Option<u64>, downloaded: u64) -> DownloadTask {
        DownloadTask {
            total,
            downloaded,
            ..Default::default()
        }
    }

    #[test]
    fn byte_total_wins_and_single_part_is_not_manifest() {
        let view = SegView {
            parts: vec![(25, 100)],
            manifest: false,
            ..Default::default()
        };
        assert_eq!(
            display_fraction(&task(Some(200), 50), Some(&view)),
            Some(0.25)
        );
        assert_eq!(view.fraction(), None);
    }

    #[test]
    fn manifest_segment_count_supplies_fraction_when_bytes_unknown() {
        let view = SegView {
            parts: vec![(3, 12)],
            manifest: true,
            ..Default::default()
        };
        assert_eq!(display_fraction(&task(None, 999), Some(&view)), Some(0.25));
    }

    #[test]
    fn webdav_schemes_map_to_http() {
        assert_eq!(
            normalize_webdav("webdav://example.com/a.bin"),
            "http://example.com/a.bin"
        );
        assert_eq!(
            normalize_webdav("webdavs://example.com/a.bin"),
            "https://example.com/a.bin"
        );
        assert_eq!(
            normalize_webdav("  webdavs://example.com/a.bin  "),
            "https://example.com/a.bin"
        );
        assert_eq!(
            normalize_webdav("https://example.com/a.bin"),
            "https://example.com/a.bin"
        );
    }

    #[test]
    fn unknown_total_without_manifest_count_stays_indeterminate() {
        let view = SegView {
            parts: vec![(10, 0)],
            manifest: false,
            ..Default::default()
        };
        assert_eq!(display_fraction(&task(None, 10), Some(&view)), None);
    }

    #[test]
    fn part_status_word_and_speed_series() {
        assert_eq!(part_status_word(10, 10), Some("Finished"));
        assert_eq!(part_status_word(5, 10), Some("Receiving"));
        assert_eq!(part_status_word(0, 10), Some("Pending"));
        assert_eq!(part_status_word(5, 0), None);
        // 100 bytes in 2s = 50 B/s; regressing counter saturates to 0.
        assert_eq!(
            task_speed_series(&[(0.0, 0), (2.0, 100), (4.0, 50)]),
            vec![50.0, 0.0]
        );
        assert!(task_speed_series(&[(0.0, 7)]).is_empty());
        assert!(task_speed_series(&[(1.0, 0), (1.0, 9)]).is_empty());
    }

    #[test]
    fn resume_supported_classifies_schemes() {
        let dir = std::env::temp_dir().join(format!("wc-resume-{}", std::process::id()));
        let mut t = task(None, 0);
        t.dest_dir = dir.clone();
        t.url = "https://example.com/f.bin".into();
        assert!(!resume_supported(&t)); // no partial data on disk
        t.url = "nzb://https://example.com/f.nzb".into();
        assert!(!resume_supported(&t));
        t.url = "https://example.com/f.nzb".into();
        assert!(!resume_supported(&t));
        t.url = "https://www.youtube.com/watch?v=aqz-KE-bpKQ".into();
        assert!(!resume_supported(&t));
        t.url = "https://example.com/f.bin".into();
        t.status = TaskStatus::Complete;
        assert!(resume_supported(&t));
        // Live partial: .part file with bytes counts as resumable.
        t.status = TaskStatus::Paused;
        t.filename = "x".into();
        t.downloaded = 5;
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("x.part"), b"bytes").unwrap();
        assert!(resume_supported(&t));
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn extract_pasted_url_variants() {
        // Double-quoted href.
        assert_eq!(
            extract_pasted_url(r#"<a href="https://example.com/v?id=1">x</a>"#),
            "https://example.com/v?id=1"
        );
        // Single-quoted src.
        assert_eq!(
            extract_pasted_url("<img src='https://cdn.example.com/a.mp4'>"),
            "https://cdn.example.com/a.mp4"
        );
        // Bare href value, >-delimited.
        assert_eq!(
            extract_pasted_url("<a href=https://example.com/bare>x</a>"),
            "https://example.com/bare"
        );
        // No href/src http: first bare http token wins.
        assert_eq!(
            extract_pasted_url("see https://example.com/page now"),
            "https://example.com/page"
        );
        // No URL at all: trimmed input unchanged.
        assert_eq!(extract_pasted_url("  just text  "), "just text");
        assert_eq!(
            extract_pasted_url("  https://example.com/plain  "),
            "https://example.com/plain"
        );
    }

    #[test]
    fn prewarm_eligible_gate() {
        let t = |u: &str| DownloadTask { url: u.into(), ..Default::default() };
        assert!(prewarm_eligible(&t("https://example.com/file.zip")));
        assert!(prewarm_eligible(&t("http://example.com/file.bin?tok=1")));
        assert!(!prewarm_eligible(&t("https://www.youtube.com/watch?v=aqz-KE-bpKQ")));
        assert!(!prewarm_eligible(&t("https://example.com/list.m3u8")));
        assert!(!prewarm_eligible(&t("https://example.com/manifest.mpd")));
        assert!(!prewarm_eligible(&t("https://example.com/file.torrent")));
        assert!(!prewarm_eligible(&t("ftp://example.com/file.zip")));
        assert!(!prewarm_eligible(&t("magnet:?xt=urn:btih:abc")));
        assert!(!prewarm_eligible(&t("not a url")));
        let mut yt = t("https://example.com/file.zip");
        yt.headers.insert("x-ytdlp".into(), "1".into());
        assert!(!prewarm_eligible(&yt));
    }

    fn yt_probe_eligible_gate() {
        assert!(yt_probe_eligible("https://example.com/watch?v=1"));
        assert!(yt_probe_eligible("https://example.com/list.m3u8?tok=1"));
        assert!(yt_probe_eligible("https://example.com/manifest.mpd"));
        assert!(!yt_probe_eligible("https://example.com/movie.mp4?tok=1"));
        assert!(!yt_probe_eligible("https://example.com/song.MP3#frag"));
        assert!(!yt_probe_eligible("https://example.com/file.torrent"));
        assert!(!yt_probe_eligible("https://example.com/file.nzb?x=1"));
        assert!(!yt_probe_eligible("magnet:?xt=urn:btih:abc"));
        assert!(!yt_probe_eligible("ftp://example.com/page"));
        assert!(!yt_probe_eligible("not a url"));
    }

    #[test]
    fn yt_choice_quality_tier_mapping() {
        let mut st = YtProbeState::default();
        assert_eq!(yt_choice_from_state(&st).quality_height, None);
        st.quality = "1080".into();
        assert_eq!(yt_choice_from_state(&st).quality_height, Some(1080));
        // Backend precedence exact > tier (defensive; UI clears are mutually exclusive).
        st.format = "137".into();
        let ch = yt_choice_from_state(&st);
        assert_eq!(ch.format.as_deref(), Some("137"));
        assert_eq!(ch.quality_height, Some(1080));
        st.quality = "bogus".into();
        assert_eq!(yt_choice_from_state(&st).quality_height, None);
    }

    #[test]
    fn fit_content_h_floors_and_caps() {
        // Floor = base even when content smaller; grows to content; 85% cap.
        assert_eq!(fit_content_h(680.0, 100.0, 1000.0), 680.0);
        assert_eq!(fit_content_h(680.0, 800.0, 1000.0), 800.0);
        assert_eq!(fit_content_h(680.0, 5000.0, 1000.0), 850.0);
        // Tiny monitor: cap never below base.
        assert_eq!(fit_content_h(680.0, 5000.0, 600.0), 680.0);
    }

    #[test]
    fn grab_name_takes_last_segment() {
        assert_eq!(grab_name("https://ex.com/a/b/file.zip?x=1"), "file.zip");
        assert_eq!(grab_name("https://ex.com/a/My%20File.pdf"), "My File.pdf");
        assert_eq!(grab_name("https://ex.com/a/"), "download.bin");
        assert_eq!(
            grab_name("magnet:?xt=urn:btih:abc&dn=Cool+Torrent"),
            "Cool Torrent"
        );
    }

    #[test]
    fn yt_err_summary_cookie() {
        let (friendly, raw) = yt_error_summary("page needs to be reloaded. Sign in to confirm");
        assert!(friendly.contains("rejected browser cookies"));
        assert_eq!(raw, "page needs to be reloaded. Sign in to confirm");
    }

    #[test]
    fn yt_err_summary_extractor() {
        let (friendly, _) = yt_error_summary("ERROR: 'NoneType' object has no attribute 'get'");
        assert!(friendly.contains("extractor error"));
    }

    #[test]
    fn yt_err_summary_generic_truncates_first_line() {
        let long = "x".repeat(250);
        let input = format!("{long}\nsecond line must not appear");
        let (friendly, raw) = yt_error_summary(&input);
        assert_eq!(raw.chars().count(), 160);
        assert!(friendly.starts_with("yt-dlp probe failed: "));
        assert!(!friendly.contains("second line"));
    }
}

fn fmt_hms(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    format!("{}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

/// Player viewport HWND via title match (same trick as platform watcher).
/// eframe 0.29 immediate-viewport frame exposes no typed HWND to app code;
/// `frame` param unused there. Cross-platform fallback: None (spawn retries,
/// then user closes). Windows primary target.
#[cfg(windows)]
fn find_player_hwnd() -> Option<isize> {
    type Hwnd = *mut core::ffi::c_void;
    type Bool = i32;
    extern "system" {
        fn EnumWindows(cb: extern "system" fn(Hwnd, isize) -> Bool, p: isize) -> Bool;
        fn GetWindowThreadProcessId(hwnd: Hwnd, pid: *mut u32) -> u32;
        fn GetWindowTextW(hwnd: Hwnd, text: *mut u16, max: i32) -> i32;
    }
    extern "system" fn find(hwnd: Hwnd, p: isize) -> Bool {
        unsafe {
            let mut pid = 0;
            GetWindowThreadProcessId(hwnd, &mut pid);
            if pid != std::process::id() {
                return 1;
            }
            let mut buf = [0u16; 64];
            let n = GetWindowTextW(hwnd, buf.as_mut_ptr(), 64);
            if n > 0 && String::from_utf16_lossy(&buf[..n as usize]).starts_with("webcraft player")
            {
                *(p as *mut Hwnd) = hwnd;
                return 0;
            }
        }
        1
    }
    let mut hwnd: Hwnd = std::ptr::null_mut();
    unsafe {
        EnumWindows(find, &mut hwnd as *mut Hwnd as isize);
    }
    if hwnd.is_null() {
        None
    } else {
        Some(hwnd as isize)
    }
}

#[cfg(not(windows))]
fn find_player_hwnd() -> Option<isize> {
    None
}

#[cfg(test)]
mod player_fmt_tests {
    use super::fmt_hms;

    #[test]
    fn hms_formats() {
        assert_eq!(fmt_hms(0.0), "0:00:00");
        assert_eq!(fmt_hms(65.0), "0:01:05");
        assert_eq!(fmt_hms(3661.0), "1:01:01");
        assert_eq!(fmt_hms(-5.0), "0:00:00");
    }
}
