// GUI subsystem on Windows: no console window, launching from PowerShell/cmd
// must not keep caller shell busy until app exits (was CUI subsystem exe).
#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;
#[allow(dead_code)]
mod icons;
mod theme;
use downloadcraft_lib::{engine, grabber, model, platform, queue, scheduler, stream};
use model::*;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub struct Runtime {
    pub state: AppState,
    pub controls: HashMap<String, Arc<engine::Controls>>,
    pub messages: Vec<String>,
    pub clip_last: String,
    pub tray_visible: bool,
}

impl Runtime {
    pub fn load() -> Self {
        platform::migrate_legacy();
        let _ = platform::ensure_dirs();
        let mut state = load_state();
        if state.queues.is_empty() { state.queues.push(Queue::default()); }
        // reset Downloading -> Paused on restart (resume available)
        for t in &mut state.tasks { if t.status == TaskStatus::Downloading { t.status = TaskStatus::Paused; } }
        Self { state, controls: HashMap::new(), messages: vec!["Ready".into()], clip_last: String::new(), tray_visible: false }
    }
    pub fn save(&self) { save_state(&self.state); }
    pub fn log(&mut self, m: impl Into<String>) { self.messages.push(m.into()); if self.messages.len() > 200 { self.messages.drain(..50); } }
    pub fn add_url(&mut self, url: &str, queue_id: &str, referrer: &str, cookies: &str) -> String {
        let filename = model::filename_from_url(url);
        let order = self.state.tasks.len() as i64 + 1;
        let task = DownloadTask { id: new_id(), url: url.to_string(), filename: filename.clone(), dest_dir: self.state.settings.download_dir.clone(), queue_id: if queue_id.is_empty() { "main".into() } else { queue_id.to_string() }, order, status: TaskStatus::Queued, category: category_for(&filename).into(), referrer: referrer.to_string(), cookies: cookies.to_string(), ..Default::default() };
        let id = task.id.clone(); self.state.tasks.push(task); self.save(); id
    }
}

pub fn load_state() -> AppState {
    let p = platform::state_path();
    if let Ok(txt) = std::fs::read_to_string(&p) { if let Ok(s) = serde_json::from_str(&txt) { return s; } }
    AppState::default()
}
pub fn save_state(state: &AppState) {
    let _ = platform::ensure_dirs();
    if let Ok(txt) = serde_json::to_string_pretty(state) { let _ = std::fs::write(platform::state_path(), txt); }
}

fn parse_kv_file(path: &std::path::Path) -> Option<DownloadTask> {
    let txt = std::fs::read_to_string(path).ok()?;
    let mut t = DownloadTask::default();
    for line in txt.lines() {
        if let Some((k, v)) = line.split_once('=') {
            match k.trim() {
                "url" => t.url = v.trim().to_string(),
                // Reject traversal: basename only; empty/invalid left empty for URL fallback at inbox site.
                "filename" => { let raw = v.trim(); let base = raw.rsplit(['/', '\\']).next().unwrap_or(""); if !raw.is_empty() && !base.is_empty() && base != "." && base != ".." { t.filename = model::sanitize_filename(base); } }
                "referrer" | "referer" => t.referrer = v.trim().to_string(),
                "cookies" | "cookie" => t.cookies = v.trim().to_string(),
                "queue" => t.queue_id = v.trim().to_string(),
                _ => {}
            }
        }
    }
    if t.url.is_empty() { None } else { Some(t) }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--smoke-test") {
        if let Err(e) = platform::ensure_dirs() { eprintln!("smoke: ensure_dirs failed: {e}"); std::process::exit(1); }
        // native-host handshake against self
        let host = sibling_host_path();
        let ok = match run_host_handshake(&host) {
            Ok(()) => true,
            Err(e) => { eprintln!("smoke: host handshake failed ({e}); host={}", host.display()); false }
        };
        println!("smoke ok: config={} host_handshake={}", platform::config_dir().display(), ok);
        if !ok { std::process::exit(1); }
        return;
    }
    if args.iter().any(|a| a == "--register-hosts") {
        let host = sibling_host_path();
        match platform::register_native_hosts(&host) { Ok(p) => println!("registered {} manifests", p.len()), Err(e) => { eprintln!("{e}"); std::process::exit(1); } }
        let _ = platform::register_protocol();
        return;
    }
    if args.iter().any(|a| a == "--unregister") {
        let _ = platform::unregister_native_hosts(); let _ = platform::set_autostart(false); println!("unregistered"); return;
    }
    // Single instance: lock file with pid; if live instance exists, forward URL args via inbox and exit.
    {
        let _ = platform::ensure_dirs();
        let lock = platform::config_dir().join("instance.lock");
        if let Ok(txt) = std::fs::read_to_string(&lock) {
            if let Ok(pid) = txt.trim().parse::<u32>() {
                if pid != std::process::id() && process_alive(pid) {
                    for a in &args[1..] {
                        if a.starts_with("http://") || a.starts_with("https://") || a.starts_with("downloadcraft://") {
                            let f = platform::inbox_dir().join(format!("fwd-{}.txt", model::new_id()));
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
    for a in &args[1..] { if a.starts_with("http://") || a.starts_with("https://") || a.starts_with("downloadcraft://") { initial_urls.push(a.clone()); } }
    let hidden = autostart || load_state().settings.start_minimized;
    run_gui(initial_urls, hidden);
}

pub fn sibling_host_path() -> std::path::PathBuf {
    let exe = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("downloadcraft"));
    let dir = exe.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    dir.join(if cfg!(windows) { "downloadcraft-host.exe" } else { "downloadcraft-host" })
}

pub fn run_host_handshake(host: &std::path::Path) -> Result<(), String> {
    use std::io::{Read, Write};
    // If host binary missing (cargo run without building host), build assumption fails -> try sibling in target profile dirs? Keep strict in release.
    if !host.exists() { return Err(format!("host not found at {}", host.display())); }
    let mut child = std::process::Command::new(host).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn().map_err(|e| e.to_string())?;
    let msg = serde_json::json!({"type":"ping"});
    let body = msg.to_string();
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    let len = body.len() as u32;
    stdin.write_all(&len.to_le_bytes()).map_err(|e| e.to_string())?;
    stdin.write_all(body.as_bytes()).map_err(|e| e.to_string())?;
    drop(stdin);
    let mut stdout = child.stdout.take().ok_or("no stdout")?;
    let mut lenbuf = [0u8; 4];
    stdout.read_exact(&mut lenbuf).map_err(|e| e.to_string())?;
    let n = u32::from_le_bytes(lenbuf) as usize;
    let mut buf = vec![0u8; n];
    stdout.read_exact(&mut buf).map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_slice(&buf).map_err(|e| e.to_string())?;
    let _ = child.kill();
    if v.get("type").and_then(|t| t.as_str()) == Some("pong") { Ok(()) } else { Err(format!("unexpected reply {v}")) }
}

fn run_gui(initial_urls: Vec<String>, hidden: bool) {
    // Register native messaging hosts on every launch: host exe path can move (portable/update), stale manifest = dead extension.
    let _ = platform::register_native_hosts(&sibling_host_path());
    let _ = platform::register_protocol();
    let rt = Arc::new(Mutex::new(Runtime::load()));
    for u in &initial_urls {
        let clean = u.strip_prefix("downloadcraft://add?url=").map(url_decode).unwrap_or_else(|| u.clone());
        rt.lock().unwrap().add_url(&clean, "main", "", "");
    }
    // drain inbox
    drain_inbox(&rt);
    let rt_clone = rt.clone();
    let options = eframe::NativeOptions { viewport: egui::ViewportBuilder::default().with_inner_size([1180.0, 720.0]).with_min_inner_size([760.0, 520.0]).with_title("downloadcraft").with_visible(!hidden), ..Default::default() };
    let _ = eframe::run_native("downloadcraft", options, Box::new(move |cc| Ok(Box::new(app::DownloadApp::new(cc, rt_clone, hidden)))));
}

fn process_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        std::process::Command::new("tasklist").args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"]).output().map(|o| String::from_utf8_lossy(&o.stdout).contains(&format!("\"{pid}\""))).unwrap_or(false)
    }
    #[cfg(unix)]
    { std::process::Command::new("kill").args(["-0", &pid.to_string()]).status().map(|s| s.success()).unwrap_or(false) }
    #[cfg(not(any(windows, unix)))]
    { let _ = pid; false }
}

fn url_decode(s: &str) -> String {
    let b = s.as_bytes(); let mut out = Vec::new(); let mut i = 0;
    while i < b.len() { if b[i] == b'%' && i + 2 < b.len() { if let Ok(v) = u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or(""), 16) { out.push(v); i += 3; continue; } } out.push(if b[i] == b'+' { b' ' } else { b[i] }); i += 1; }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn drain_inbox(rt: &Arc<Mutex<Runtime>>) {
    let dir = platform::inbox_dir();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let p = e.path();
            if let Some(mut t) = parse_kv_file(&p) {
                if let Some(enc) = t.url.strip_prefix("downloadcraft://add?url=") { t.url = url_decode(enc); }
                let mut g = rt.lock().unwrap();
                if t.id.is_empty() { t.id = new_id(); }
                if t.filename.is_empty() { t.filename = filename_from_url(&t.url); }
                if t.queue_id.is_empty() { t.queue_id = "main".into(); }
                if t.dest_dir.as_os_str().is_empty() { t.dest_dir = g.state.settings.download_dir.clone(); }
                t.order = g.state.tasks.len() as i64 + 1;
                t.category = category_for(&t.filename).into();
                g.log(format!("Inbox: {}", t.url));
                g.state.tasks.push(t); g.save();
            }
            let _ = std::fs::remove_file(p);
        }
    }
}
