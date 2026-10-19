// GUI subsystem on Windows: no console window, launching from PowerShell/cmd
// must not keep caller shell busy until app exits (was CUI subsystem exe).
#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;
#[allow(dead_code)]
mod icons;
mod theme;
use downloadcraft_lib::{
    engine, grabber, model, nntp, platform, queue, scheduler, stream, torrent, ytdlp,
};
use model::*;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Poison-tolerant lock: a panic while a thread held the mutex must not
/// cascade into every later lock (GUI thread death, invisible on Windows
/// GUI-subsystem builds). Data may be mid-update; callers tolerate that.
pub fn lock_unpoison<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub struct Runtime {
    pub state: AppState,
    pub controls: HashMap<String, Arc<engine::Controls>>,
    pub messages: Vec<String>,
    pub clip_last: String,
    pub tray_visible: bool,
    /// Browser captures awaiting IDM-style confirm dialog. In-memory only:
    /// inbox files are already deleted when a capture lands here, so an
    /// unconfirmed capture does not survive restart (accepted trade-off).
    pub pending_captures: Vec<DownloadTask>,
    /// Completion power action awaiting GUI confirm (Schedule.power_confirmed).
    /// Set by run_completion on worker thread; drained by the egui dialog.
    pub pending_power: Option<downloadcraft_lib::platform::PowerAction>,
}

impl Runtime {
    pub fn load() -> Self {
        platform::migrate_legacy();
        let _ = platform::ensure_dirs();
        let mut state = load_state();
        if state.queues.is_empty() {
            state.queues.push(Queue::default());
        }
        // reset Downloading -> Paused on restart (resume available)
        for t in &mut state.tasks {
            if t.status == TaskStatus::Downloading {
                t.status = TaskStatus::Paused;
            }
        }
        Self {
            state,
            controls: HashMap::new(),
            messages: vec!["Ready".into()],
            clip_last: String::new(),
            tray_visible: false,
            pending_captures: Vec::new(),
            pending_power: None,
        }
    }
    pub fn save(&self) {
        save_state(&self.state);
    }
    pub fn log(&mut self, m: impl Into<String>) {
        self.messages.push(m.into());
        if self.messages.len() > 200 {
            self.messages.drain(..50);
        }
    }
    pub fn add_url(&mut self, url: &str, queue_id: &str, referrer: &str, cookies: &str) -> String {
        // Dedup exact URL: duplicate magnet = same librqbit handle shared by two
        // app tasks; removing one detaches other into never-finishing zombie.
        if let Some(t) = self.state.tasks.iter().find(|t| t.url == url) {
            return t.id.clone();
        }
        // NZB tasks produce NZB file names at download time; don't seed
        // `something.nzb` as the final filename (overwritten anyway).
        let filename = if url
            .split(['?', '#'])
            .next()
            .unwrap_or(url)
            .trim_end()
            .to_ascii_lowercase()
            .ends_with(".nzb")
        {
            "nzb-files".to_string()
        } else {
            model::filename_from_url(url)
        };
        // Single funnel: rename template + category subfolder for every
        // task created via add_url (UI, clipboard, grabber, tray, CLI).
        let filename = model::apply_folder_template(
            &self.state.settings.folder_template,
            &filename,
            url,
        );
        let order = self.state.tasks.len() as i64 + 1;
        let task = DownloadTask {
            id: new_id(),
            url: url.to_string(),
            dest_dir: model::dest_dir_for(
                &self.state.settings.download_dir,
                &filename,
                self.state.settings.category_subfolders,
            ),
            filename: filename.clone(),
            queue_id: if queue_id.is_empty() {
                "main".into()
            } else {
                queue_id.to_string()
            },
            order,
            status: TaskStatus::Queued,
            category: category_for(&filename).into(),
            referrer: referrer.to_string(),
            cookies: cookies.to_string(),
            ..Default::default()
        };
        let id = task.id.clone();
        self.state.tasks.push(task);
        self.save();
        id
    }
}

/// Pure parse: None on any corruption (never panics).
pub fn parse_state(txt: &str) -> Option<AppState> {
    serde_json::from_str(txt).ok()
}

pub fn load_state() -> AppState {
    let p = platform::state_path();
    if let Ok(txt) = std::fs::read_to_string(&p) {
        if let Some(s) = parse_state(&txt) {
            return s;
        }
    }
    // Corrupt/truncated primary: fall back to last-known-good backup.
    let bak = p.with_extension("json.bak");
    if let Ok(txt) = std::fs::read_to_string(&bak) {
        if let Some(s) = parse_state(&txt) {
            return s;
        }
    }
    AppState::default()
}
pub fn save_state(state: &AppState) {
    let _ = platform::ensure_dirs();
    let Ok(txt) = serde_json::to_string_pretty(state) else {
        return;
    };
    let p = platform::state_path();
    // Preserve previous good state as .bak before replacing it.
    if std::fs::read_to_string(&p)
        .map(|old| parse_state(&old).is_some())
        .unwrap_or(false)
    {
        let _ = std::fs::copy(&p, p.with_extension("json.bak"));
    }
    // Atomic-ish replace: write tmp in same dir, rename over dest.
    // Windows rename fails when dest exists, so remove dest first;
    // on any failure fall back to direct write.
    let tmp = p.with_extension("json.tmp");
    let replaced = std::fs::write(&tmp, &txt).is_ok() && {
        let _ = std::fs::remove_file(&p);
        std::fs::rename(&tmp, &p).is_ok()
    };
    if !replaced {
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::write(&p, txt);
    }
}

/// Promote extension pageUrl to task URL? stream intent never; page intent
/// always (off-allowlist yt-dlp sites included); no intent = allowlist only.
pub fn should_promote(task_url: &str, page_url: &str, intent: &str) -> bool {
    if intent == "stream" {
        return false;
    }
    if ytdlp::is_video_page(task_url) {
        return false; // already page target
    }
    if intent == "page" {
        return page_url.starts_with("http://") || page_url.starts_with("https://");
    }
    ytdlp::is_video_page(page_url)
}

fn parse_kv_file(path: &std::path::Path) -> Option<DownloadTask> {
    let txt = std::fs::read_to_string(path).ok()?;
    let mut t = DownloadTask::default();
    for line in txt.lines() {
        if let Some((k, v)) = line.split_once('=') {
            match k.trim() {
                "url" => t.url = v.trim().to_string(),
                // Reject traversal: basename only; empty/invalid left empty for URL fallback at inbox site.
                "filename" => {
                    let raw = v.trim();
                    let base = raw.rsplit(['/', '\\']).next().unwrap_or("");
                    if !raw.is_empty() && !base.is_empty() && base != "." && base != ".." {
                        t.filename = model::sanitize_filename(base);
                    }
                }
                "referrer" | "referer" => t.referrer = v.trim().to_string(),
                "cookies" | "cookie" => t.cookies = v.trim().to_string(),
                // Extension video-panel sends page URL alongside sniffed stream.
                "pageUrl" => {
                    t.headers.insert("x-page-url".into(), v.trim().to_string());
                }
                // intent=page: user picked "this video" row — promote even off
                // host allowlist. intent=stream: never promote (manifest pick).
                "intent" => {
                    t.headers.insert("x-page-intent".into(), v.trim().to_string());
                }
                "queue" => t.queue_id = v.trim().to_string(),
                "size" => t.total = v.trim().parse::<u64>().ok().filter(|n| *n > 0),
                _ => {}
            }
        }
    }
    if t.url.is_empty() {
        None
    } else {
        Some(t)
    }
}

fn main() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    platform::install_crash_hook();
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
        return;
    }
    if args.iter().any(|a| a == "--smoke-test") {
        if let Err(e) = platform::ensure_dirs() {
            eprintln!("smoke: ensure_dirs failed: {e}");
            std::process::exit(1);
        }
        // native-host handshake against self
        let host = sibling_host_path();
        let ok = match run_host_handshake(&host) {
            Ok(()) => true,
            Err(e) => {
                eprintln!(
                    "smoke: host handshake failed ({e}); host={}",
                    host.display()
                );
                false
            }
        };
        println!(
            "smoke ok: config={} host_handshake={}",
            platform::config_dir().display(),
            ok
        );
        if !ok {
            std::process::exit(1);
        }
        return;
    }
    if args.iter().any(|a| a == "--register-hosts") {
        let host = sibling_host_path();
        match platform::register_native_hosts(&host) {
            Ok(p) => println!("registered {} manifests", p.len()),
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
        let _ = platform::register_protocol();
        return;
    }
    if args.iter().any(|a| a == "--unregister") {
        let _ = platform::unregister_native_hosts();
        let _ = platform::set_autostart(false);
        println!("unregistered");
        return;
    }
    // Single instance: lock file with pid; if live instance exists, forward URL args via inbox and exit.
    {
        let _ = platform::ensure_dirs();
        let lock = platform::config_dir().join("instance.lock");
        if let Ok(txt) = std::fs::read_to_string(&lock) {
            if let Ok(pid) = txt.trim().parse::<u32>() {
                if pid != std::process::id() && process_alive(pid) {
                    let _ = std::fs::write(platform::config_dir().join("show.request"), "show");
                    for a in &args[1..] {
                        if a.starts_with("http://")
                            || a.starts_with("https://")
                            || (a.starts_with("webcraft://") || a.starts_with("downloadcraft://"))
                        {
                            let f =
                                platform::inbox_dir().join(format!("fwd-{}.txt", model::new_id()));
                            let _ = std::fs::write(f, format!("url={a}\n"));
                        }
                    }
                    println!("already running (pid {pid}); forwarded args");
                    return;
                }
            }
        }
        let _ = std::fs::write(&lock, std::process::id().to_string());
    }
    let autostart = args.iter().any(|a| a == "--autostart");
    // Inbox: files dropped by native host
    let mut initial_urls: Vec<String> = Vec::new();
    for a in &args[1..] {
        if a.starts_with("http://")
            || a.starts_with("https://")
            || (a.starts_with("webcraft://") || a.starts_with("downloadcraft://"))
        {
            initial_urls.push(a.clone());
        }
    }
    let hidden = autostart || load_state().settings.start_minimized;
    run_gui(initial_urls, hidden);
}

pub fn sibling_host_path() -> std::path::PathBuf {
    let exe = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("webcraft"));
    let dir = exe.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    dir.join(if cfg!(windows) {
        "webcraft-host.exe"
    } else {
        "webcraft-host"
    })
}

pub fn run_host_handshake(host: &std::path::Path) -> Result<(), String> {
    use std::io::{Read, Write};
    // If host binary missing (cargo run without building host), build assumption fails -> try sibling in target profile dirs? Keep strict in release.
    if !host.exists() {
        return Err(format!("host not found at {}", host.display()));
    }
    let mut hcmd = std::process::Command::new(host);
    hcmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped());
    platform::no_window(&mut hcmd);
    let mut child = hcmd.spawn().map_err(|e| e.to_string())?;
    let msg = serde_json::json!({"type":"ping"});
    let body = msg.to_string();
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    let len = body.len() as u32;
    stdin
        .write_all(&len.to_le_bytes())
        .map_err(|e| e.to_string())?;
    stdin
        .write_all(body.as_bytes())
        .map_err(|e| e.to_string())?;
    drop(stdin);
    let mut stdout = child.stdout.take().ok_or("no stdout")?;
    let mut lenbuf = [0u8; 4];
    stdout.read_exact(&mut lenbuf).map_err(|e| e.to_string())?;
    let n = u32::from_le_bytes(lenbuf) as usize;
    let mut buf = vec![0u8; n];
    stdout.read_exact(&mut buf).map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_slice(&buf).map_err(|e| e.to_string())?;
    let _ = child.kill();
    if v.get("type").and_then(|t| t.as_str()) == Some("pong") {
        Ok(())
    } else {
        Err(format!("unexpected reply {v}"))
    }
}

fn run_gui(initial_urls: Vec<String>, hidden: bool) {
    platform::WINDOW_VISIBLE.store(!hidden, std::sync::atomic::Ordering::Relaxed);
    platform::spawn_show_watcher();
    // Register native messaging hosts on every launch: host exe path can move (portable/update), stale manifest = dead extension.
    let _ = platform::register_native_hosts(&sibling_host_path());
    let _ = platform::register_protocol();
    let rt = Arc::new(Mutex::new(Runtime::load()));
    // Emit capture filter at startup: deleted/restored filter file must not
    // silently disable filtering until user edits Options.
    platform::write_capture_filter(&crate::lock_unpoison(&rt).state.settings);
    for u in &initial_urls {
        let clean = u
            .strip_prefix("webcraft://add?url=")
            .or_else(|| u.strip_prefix("downloadcraft://add?url="))
            .map(url_decode)
            .unwrap_or_else(|| u.clone());
        crate::lock_unpoison(&rt).add_url(&clean, "main", "", "");
    }
    // drain inbox into pending captures (confirm dialog shows on first frame)
    crate::lock_unpoison(&rt).pending_captures = take_inbox_tasks(&rt);
    let rt_clone = rt.clone();
    // Window/taskbar icon: same artwork as .exe resource (assets/webcraft.png -> .ico).
    // Without this eframe/winit falls back to a default blue square.
    let win_icon = eframe::icon_data::from_png_bytes(include_bytes!("../assets/webcraft.png"))
        .unwrap_or_default();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_icon(std::sync::Arc::new(win_icon))
            .with_inner_size([1180.0, 720.0])
            .with_min_inner_size([760.0, 520.0])
            .with_title(format!("webcraft v{}", env!("CARGO_PKG_VERSION")))
            .with_visible(!hidden),
        ..Default::default()
    };
    if let Err(e) = eframe::run_native(
        "webcraft",
        options,
        Box::new(move |cc| Ok(Box::new(app::DownloadApp::new(cc, rt_clone, hidden)))),
    ) {
        platform::log_crash(&format!("eframe::run_native failed: {e}"));
    }
}

fn process_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        let mut tcmd = std::process::Command::new("tasklist");
        tcmd.args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"]);
        platform::no_window(&mut tcmd);
        tcmd.output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains(&format!("\"{pid}\"")))
            .unwrap_or(false)
    }
    #[cfg(unix)]
    {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = pid;
        false
    }
}

fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) =
                u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or(""), 16)
            {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Drain inbox files into pending captures (IDM "Download File Info").
/// Same parsing (parse_kv_file) and grab/add prefix handling as the old silent
/// path; returned tasks are finalized (id/filename/queue/dest_dir/order/
/// category) but NOT pushed to state.tasks — app confirms them first.
/// Inbox files are deleted here, so unconfirmed captures are lost on restart.
/// Re-apply rename template + category subfolder to tasks built outside
/// add_url (browser inbox override). Keeps funnel invariant: stored
/// dest_dir/filename already reflect current settings at capture time.
fn apply_task_naming(s: &Settings, t: &mut DownloadTask) {
    t.filename = model::apply_folder_template(&s.folder_template, &t.filename, &t.url);
    t.category = category_for(&t.filename).into();
    t.dest_dir = model::dest_dir_for(
        &s.download_dir,
        &t.filename,
        s.category_subfolders,
    );
}

pub fn take_inbox_tasks(rt: &Arc<Mutex<Runtime>>) -> Vec<DownloadTask> {
    let dir = platform::inbox_dir();
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let p = e.path();
            if let Some(mut t) = parse_kv_file(&p) {
                if let Some(enc) = t
                    .url
                    .strip_prefix("webcraft://grab?url=")
                    .or_else(|| t.url.strip_prefix("downloadcraft://grab?url="))
                {
                    let _ = std::fs::write(
                        platform::config_dir().join("pending-grab.txt"),
                        url_decode(enc),
                    );
                    let _ = std::fs::remove_file(&p);
                    continue;
                }
                if let Some(enc) = t
                    .url
                    .strip_prefix("webcraft://add?url=")
                    .or_else(|| t.url.strip_prefix("downloadcraft://add?url="))
                {
                    t.url = url_decode(enc);
                }
                // Video page: promote page URL to task URL so yt-dlp extractor
                // runs; sniffed stream kept as fallback. intent=page promotes
                // even off allowlist (any yt-dlp site); intent=stream never.
                // Absent intent = legacy heuristic (allowlist only).
                if let Some(pu) = t.headers.get("x-page-url").cloned() {
                    let intent = t.headers.get("x-page-intent").cloned().unwrap_or_default();
                    let promote = should_promote(&t.url, &pu, &intent);
                    if promote {
                        let stream = std::mem::replace(&mut t.url, pu.clone());
                        t.headers.insert("x-page-stream".into(), stream);
                        t.headers.insert("x-ytdlp".into(), "1".into());
                    }
                    if intent.as_str() == "page" && t.url == pu {
                        t.headers.insert("x-ytdlp".into(), "1".into());
                    }
                    // Manifest intent: no promotion; yt-dlp-first in stream dispatch.
                    // (should_promote already returns false for non-video pageUrl.)
                }
                let mut g = crate::lock_unpoison(rt);
                if t.id.is_empty() {
                    t.id = new_id();
                }
                if t.filename.is_empty() {
                    t.filename = filename_from_url(&t.url);
                }
                if t.queue_id.is_empty() {
                    t.queue_id = "main".into();
                }
                // Honor explicit savePath; empty gets settings dir, then
                // naming funnel applies template + category subfolder.
                let explicit_dir = !t.dest_dir.as_os_str().is_empty();
                if !explicit_dir {
                    apply_task_naming(&g.state.settings.clone(), &mut t);
                } else {
                    // Explicit dir from browser: still rename, keep dir.
                    let tpl = g.state.settings.folder_template.clone();
                    t.filename = model::apply_folder_template(&tpl, &t.filename, &t.url);
                    t.category = category_for(&t.filename).into();
                }
                t.order = g.state.tasks.len() as i64 + 1 + out.len() as i64;
                g.log(format!("Inbox: {}", t.url));
                drop(g);
                out.push(t);
            }
            let _ = std::fs::remove_file(p);
        }
    }
    out
}

pub fn drain_inbox(rt: &Arc<Mutex<Runtime>>) {
    let caps = take_inbox_tasks(rt);
    if !caps.is_empty() {
        crate::lock_unpoison(rt).pending_captures.extend(caps);
    }
}

#[cfg(test)]
mod state_tests {
    use super::*;

    #[test]
    fn promote_rules() {
        let page = "https://x.com/u/status/123";
        let stream = "https://video.twimg.com/amplify_video/123/vid/x.mp4";
        assert!(should_promote(stream, page, ""));
        assert!(!should_promote(stream, page, "stream"), "manifest pick never hijacked");
        assert!(should_promote(stream, "https://unknown-site.example/watch/1", "page"), "page intent off-allowlist");
        assert!(!should_promote(page, page, "page"), "already page target");
        assert!(!should_promote(stream, "https://example.com/file.mp4", ""));
    }

    #[test]
    fn parse_state_accepts_valid_rejects_corrupt() {
        let good = serde_json::to_string(&AppState::default()).unwrap();
        assert!(parse_state(&good).is_some());
        assert!(parse_state("").is_none());
        assert!(parse_state("{not json").is_none());
        assert!(parse_state("{\"tasks\":[").is_none(), "truncated write");
        assert!(parse_state("null").is_none());
    }

    #[test]
    fn bak_holds_previous_good_state() {
        let dir = std::env::temp_dir().join(format!("wc-state-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("state.json");
        std::fs::write(&p, "{\"tasks\":[]}").unwrap();
        if parse_state(&std::fs::read_to_string(&p).unwrap()).is_some() {
            std::fs::copy(&p, p.with_extension("json.bak")).unwrap();
        }
        std::fs::write(&p, "CORRUPT").unwrap();
        let bak_txt = std::fs::read_to_string(p.with_extension("json.bak")).unwrap();
        assert!(parse_state(&bak_txt).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
