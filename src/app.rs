use crate::engine;
use crate::grabber::{Crawler, FoundFile, GrabberProject};
use crate::model::*;
use crate::platform;
use crate::queue::plan_starts;
use crate::scheduler::{next_run_in, should_start, should_stop};
use crate::icons::{icon, Icon};
use crate::theme;
use crate::Runtime;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, PartialEq)]
enum Tab { Downloads, Queues, Scheduler, Grabber, Options }

/// Live per-part progress snapshot for one task.
/// Segmented HTTP: one entry per part, (done_bytes, expected_bytes).
/// Manifest downloads: single entry (done_segments, total_segments).
#[derive(Clone, Default)]
pub struct SegView { pub parts: Vec<(u64, u64)>, pub manifest: bool }
impl SegView {
    fn label(&self) -> String {
        if self.manifest {
            let (d, t) = self.parts.first().copied().unwrap_or((0, 0));
            format!("{d} / {t} segments")
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
        if self.manifest && expected > 0 { Some((done as f32 / expected as f32).clamp(0.0, 1.0)) } else { None }
    }
}

/// IDM-style displayed completion: byte fraction first, segment fraction for manifests.
fn display_fraction(task: &DownloadTask, view: Option<&SegView>) -> Option<f32> {
    if let Some(total) = task.total.filter(|t| *t > 0) {
        return Some((task.downloaded as f32 / total as f32).clamp(0.0, 1.0));
    }
    if task.status == TaskStatus::Complete { return Some(1.0); }
    view.and_then(|v| v.fraction())
}

/// Draw per-part strip: each cell width proportional to expected size, fill = done fraction.
fn parts_strip(ui: &mut egui::Ui, view: &SegView, width: f32) {
    if view.parts.is_empty() { return; }
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 10.0), egui::Sense::hover());
    let total_exp: u64 = view.parts.iter().map(|(_, e)| *e).sum::<u64>().max(1);
    let painter = ui.painter();
    let mut x = rect.left();
    for (done, exp) in &view.parts {
        let w = rect.width() * (*exp as f32 / total_exp as f32);
        let cell = egui::Rect::from_min_size(egui::pos2(x, rect.top()), egui::vec2((w - 1.0).max(1.0), rect.height()));
        painter.rect_filled(cell, 0.0, theme::G_700);
        let frac = if *exp > 0 { (*done as f32 / *exp as f32).clamp(0.0, 1.0) } else { 0.0 };
        if frac > 0.0 {
            let fill = egui::Rect::from_min_size(cell.min, egui::vec2(cell.width() * frac, cell.height()));
            painter.rect_filled(fill, 0.0, theme::PRIMARY);
        }
        x += w;
    }
}

#[derive(Default)]
struct GrabOutcome { done: bool, apply_files: bool, files: Vec<FoundFile>, message: String, progress: Option<Arc<Mutex<crate::grabber::CrawlProgress>>> }

pub struct DownloadApp {
    rt: Arc<Mutex<Runtime>>,
    tokio_rt: Arc<tokio::runtime::Runtime>,
    tab: Tab,
    add_url: String,
    add_referrer: String,
    add_cookies: String,
    add_user: String,
    add_pass: String,
    add_mirrors: String,
    add_desc: String,
    add_queue: String,
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
    grab_project_json: String,
    grab_status: String,
    grab_busy: bool,
    grab_auto_explore: bool,
    last_tick: std::time::Instant,
    #[allow(dead_code)]
    tray: Option<TrayState>,
    hidden: bool,
    show_about: bool,
    search: String,
    minutes_today: (i64, u32),
    category: String,
    sort_col: u8,
    sort_asc: bool,
    options_tab: u8,
    options_notice: String,
    show_add: bool,
    show_progress_for: Option<String>,
    selected: Option<String>,
    completed_notice: Option<String>,
    last_notified: Option<String>,
    speed_hist: Vec<f32>,
    last_dl_total: u64,
    /// Per-task rate samples (drives Transfer rate column + ETA per row).
    rate_map: std::collections::HashMap<String, Vec<(std::time::Instant, u64)>>,
    /// Live segment/part snapshot per task (Controls.parts, fallback part-dir scan).
    seg_info: std::collections::HashMap<String, SegView>,
    /// Smoothed displayed rate per task (bytes/s), EMA over 5s window rate.
    rate_smooth: std::collections::HashMap<String, f64>,
    /// Spawned download tasks; Remove aborts handle so background work really stops.
    handles: std::collections::HashMap<String, tokio::task::JoinHandle<()>>,
    /// Tray Quit / Tasks Quit set this so close-to-tray intercept lets window close.
    allow_close: bool,
}

/// Live tray state: keep icon+menu alive; poll MenuEvent receiver each tick for Show/Add/Quit.
struct TrayState { _nav: Option<TrayNav> }
struct TrayNav { _tray: tray_icon::TrayIcon, _menu: tray_icon::menu::Menu, show_id: tray_icon::menu::MenuId, add_id: tray_icon::menu::MenuId, quit_id: tray_icon::menu::MenuId }

impl DownloadApp {
    pub fn new(_cc: &eframe::CreationContext<'_>, rt: Arc<Mutex<Runtime>>, hidden: bool) -> Self {
        let tokio_rt = Arc::new(tokio::runtime::Runtime::new().expect("tokio rt"));
        let tray = Self::make_tray();
        Self { rt, tokio_rt, tab: Tab::Downloads, add_url: String::new(), add_referrer: String::new(), add_cookies: String::new(), add_user: String::new(), add_pass: String::new(), add_mirrors: String::new(), add_desc: String::new(), add_queue: "main".into(), new_queue_name: String::new(), grab_url: String::new(), grab_depth: 2, grab_ext_depth: 0, grab_scope: 0, grab_exts: String::new(), grab_parent_dirs: false, grab_only_paths: String::new(), grab_exclude_paths: String::new(), grab_files: Vec::new(), grab_project_json: String::new(), grab_status: "Enter a start URL, then Explore.".into(), grab_busy: false, grab_auto_explore: false, last_tick: std::time::Instant::now(), tray: Some(TrayState { _nav: tray }), hidden, show_about: false, search: String::new(), minutes_today: today_parts(), category: "All Downloads".into(), sort_col: 0, sort_asc: true, options_tab: 0, options_notice: "Changes save automatically.".into(), show_add: false, show_progress_for: None, selected: None, completed_notice: None, last_notified: None, speed_hist: Vec::new(), last_dl_total: 0, rate_map: std::collections::HashMap::new(), seg_info: std::collections::HashMap::new(), rate_smooth: std::collections::HashMap::new(), handles: std::collections::HashMap::new(), allow_close: false }
    }

    fn make_tray() -> Option<TrayNav> {
        // 16x16 RGBA icon generated in code (no proprietary asset).
        let mut rgba = vec![0u8; 16 * 16 * 4];
        for i in 0..256 { rgba[i * 4] = 30; rgba[i * 4 + 1] = 58; rgba[i * 4 + 2] = 95; rgba[i * 4 + 3] = 255; }
        let icon = tray_icon::Icon::from_rgba(rgba, 16, 16).ok()?;
        let menu = tray_icon::menu::Menu::new();
        let show = tray_icon::menu::MenuItem::new("Show downloadcraft", true, None);
        let add = tray_icon::menu::MenuItem::new("Add URL...", true, None);
        let quit = tray_icon::menu::MenuItem::new("Quit", true, None);
        let (show_id, add_id, quit_id) = (show.id().clone(), add.id().clone(), quit.id().clone());
        let _ = menu.append_items(&[&show, &add, &quit]);
        let tray = tray_icon::TrayIconBuilder::new().with_icon(icon).with_menu(Box::new(menu.clone())).with_tooltip("downloadcraft").build().ok()?;
        Some(TrayNav { _tray: tray, _menu: menu, show_id, add_id, quit_id })
    }

    fn start_task(&mut self, id: &str) {
        // Never spawn a second worker for same task: it replaces Controls entry,
        // and Stop would then cancel only newest worker while old one keeps downloading.
        if self.handles.get(id).is_some_and(|h| !h.is_finished()) { return; }
        if self.rt.lock().unwrap().controls.contains_key(id) { return; }
        let (task_opt, settings) = { let g = self.rt.lock().unwrap(); (g.state.tasks.iter().find(|t| t.id == id).cloned(), g.state.settings.clone()) };
        let Some(task) = task_opt else { return; };
        {
            let mut g = self.rt.lock().unwrap();
            if let Some(t) = g.state.tasks.iter_mut().find(|t| t.id == id) { t.status = TaskStatus::Downloading; t.error.clear(); }
            g.save();
        }
        let ctrl = Arc::new(engine::Controls::new(if task.speed_limit_kbps > 0 { task.speed_limit_kbps } else { settings.speed_limit_kbps }));
        self.rt.lock().unwrap().controls.insert(id.to_string(), ctrl.clone());
        let rt = self.rt.clone(); let tid = id.to_string();
        let client = engine::build_client(&settings.proxy_url);
        let ctrl2 = ctrl.clone();
        let handle = self.tokio_rt.spawn(async move {
            let result = crate::stream::download_url(client, task, ctrl).await;
            let mut g = rt.lock().unwrap();
            // Only this worker may unregister its own Controls. A stopped worker
            // racing a fresh Resume must not remove the new worker's entry.
            if g.controls.get(&tid).is_some_and(|c| Arc::ptr_eq(c, &ctrl2)) { g.controls.remove(&tid); }
            match result {
                Ok(done) => {
                    let status = done.status; let err = done.error.clone(); let fname = done.filename.clone();
                    if let Some(t) = g.state.tasks.iter_mut().find(|t| t.id == tid) { *t = done; }
                    g.log(format!("Finished {fname}: {:?} {err}", status));
                    if status == TaskStatus::Complete {
                        let queue_id = g.state.tasks.iter().find(|t| t.id == tid).map(|t| t.queue_id.clone()).unwrap_or_default();
                        // quota accounting (IDM download limits): accumulate bytes into schedule window
                        let bytes = g.state.tasks.iter().find(|t| t.id == tid).map(|t| t.downloaded).unwrap_or(0);
                        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
                        let mut quota_hit = false;
                        for s in g.state.schedules.iter_mut().filter(|s| s.queue_id == queue_id && s.quota_mb > 0) {
                            if s.quota_window_start == 0 || now.saturating_sub(s.quota_window_start) >= s.quota_hours.max(1) * 3600 { s.quota_window_start = now; s.quota_used_bytes = 0; }
                            s.quota_used_bytes += bytes;
                            if s.quota_used_bytes >= s.quota_mb * 1024 * 1024 { quota_hit = true; }
                        }
                        g.log(format!("Quota queue {queue_id}: hit={quota_hit}"));
                        // completion action only when queue drained (was: first task could exit/shutdown mid-queue)
                        let drained = !g.state.tasks.iter().any(|t| t.queue_id == queue_id && matches!(t.status, TaskStatus::Queued | TaskStatus::Downloading));
                        if drained { run_completion(&g, &queue_id, &tid); }
                    }
                }
                Err(e) => {
                    if ctrl2.cancelled.load(Ordering::Relaxed) || e == "cancelled" {
                        // User Stop/Remove: stay Paused (parts kept), no retry, no resurrect if row deleted.
                        if let Some(t) = g.state.tasks.iter_mut().find(|t| t.id == tid) { t.status = TaskStatus::Paused; t.downloaded = ctrl2.downloaded.load(Ordering::Relaxed); }
                        g.log(format!("Stopped {tid}"));
                        g.save();
                        return;
                    }
                    // retry per queue policy: attempts persisted on task, next_retry_at gates poll requeue
                    let (retry, interval) = {
                        let t = g.state.tasks.iter().find(|t| t.id == tid);
                        let q = t.and_then(|t| g.state.queues.iter().find(|q| q.id == t.queue_id));
                        (q.map(|q| q.retry_count).unwrap_or(0), q.map(|q| q.retry_interval_secs).unwrap_or(10))
                    };
                    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
                    let mut left = 0u32;
                    if let Some(t) = g.state.tasks.iter_mut().find(|t| t.id == tid) {
                        t.attempts += 1; t.error = e.clone();
                        if t.attempts <= retry { t.status = TaskStatus::Queued; t.next_retry_at = now + interval; left = retry - t.attempts; }
                        else { t.status = TaskStatus::Error; t.next_retry_at = 0; }
                    }
                    g.log(format!("Error {tid}: {e} (queue retry left={left} interval={interval}s)"));
                }
            }
            g.save();
        });
        self.handles.insert(id.to_string(), handle);
    }

    /// Sample per-task rates from Controls counters (called every tick). Drives
    /// Transfer rate + ETA columns; before this, every row shared the global rate.
    fn sample_rates(&mut self) {
        let now = std::time::Instant::now();
        // Snapshot controls: (id, downloaded, total, parts)
        type Snap = (String, u64, u64, Vec<(u64, u64)>, bool);
        let snap: Vec<Snap> = { let g = self.rt.lock().unwrap(); g.controls.iter().map(|(k, c)| (k.clone(), c.downloaded.load(Ordering::Relaxed), c.total.load(Ordering::Relaxed), c.parts.lock().unwrap().clone(), c.parts_are_segments.load(Ordering::Relaxed))).collect() };
        for (id, n, _total, parts, manifest) in &snap {
            let v = self.rate_map.entry(id.clone()).or_default();
            if v.last().map(|(t, b)| now.duration_since(*t).as_millis() >= 100 && *b != *n).unwrap_or(true) { v.push((now, *n)); }
            v.retain(|(t, _)| now.duration_since(*t).as_secs_f64() <= 6.0);
            if !parts.is_empty() {
                self.seg_info.insert(id.clone(), SegView { parts: parts.clone(), manifest: *manifest });
            }
        }
        self.rate_map.retain(|id, _| { let g = self.rt.lock().unwrap(); g.controls.contains_key(id) || g.state.tasks.iter().any(|t| &t.id == id && matches!(t.status, TaskStatus::Downloading | TaskStatus::Paused)) });
        self.seg_info.retain(|id, _| { let g = self.rt.lock().unwrap(); g.controls.contains_key(id) || g.state.tasks.iter().any(|t| &t.id == id && matches!(t.status, TaskStatus::Downloading | TaskStatus::Paused)) });
        // Fallback for tasks without controls yet (probe/queue): part files on disk vs known total.
        let tasks: Vec<(String, std::path::PathBuf, u64)> = { let g = self.rt.lock().unwrap(); g.state.tasks.iter().filter(|t| t.status == TaskStatus::Downloading).map(|t| (t.id.clone(), t.dest_dir.join(format!(".downloadcraft-{}", t.id)), t.total.unwrap_or(0))).collect() };
        for (id, dir, total) in tasks {
            if self.seg_info.contains_key(&id) { continue; }
            if total == 0 { continue; }
            if let Ok(rd) = std::fs::read_dir(&dir) {
                let mut sizes: Vec<u64> = rd.flatten().filter_map(|e| { let p = e.path(); if p.file_name().and_then(|f| f.to_str()).map(|f| f.starts_with("part")).unwrap_or(false) { e.metadata().map(|m| m.len()).ok() } else { None } }).collect();
                if !sizes.is_empty() {
                    sizes.sort_unstable();
                    let n = sizes.len() as u64;
                    // Even split approximation until engine publishes exact ranges.
                    let parts: Vec<(u64, u64)> = sizes.iter().enumerate().map(|(i, sz)| { let exp = if i as u64 == n - 1 { total - total / n * (n - 1) } else { total / n }; (*sz, exp) }).collect();
                    self.seg_info.insert(id, SegView { parts, manifest: false });
                }
            }
        }
    }

    fn task_rate(&self, id: &str) -> f64 { self.rate_smooth.get(id).copied().unwrap_or(0.0) }

    /// Once per poll: 6s window rate -> EMA. Done here, not in draw code, so
    /// repaint frequency cannot change smoothing.
    fn update_smooth_rates(&mut self) {
        let ids: Vec<String> = self.rate_map.keys().cloned().collect();
        for id in ids {
            // Anchor to newest sample, not wall clock: egui can stop repainting
            // in tray while worker keeps moving bytes.
            let raw = {
                let Some(v) = self.rate_map.get(&id) else { continue; };
                if v.len() < 2 { continue; }
                let (t_end, b_end) = v[v.len() - 1];
                let (t0, b0) = v.iter().copied().find(|(t, _)| t_end.duration_since(*t).as_secs_f64() <= 5.0).unwrap_or(v[v.len() - 1]);
                let dt = t_end.duration_since(t0).as_secs_f64();
                if dt < 1.0 { continue; } else { b_end.saturating_sub(b0) as f64 / dt }
            };
            let e = self.rate_smooth.entry(id).or_insert(raw);
            *e = if raw > 0.0 { *e * 0.65 + raw * 0.35 } else { *e * 0.8 };
            if *e < 1.0 { *e = 0.0; }
        }
    }

    fn poll(&mut self, ctx: &egui::Context) {
        self.handles.retain(|_, h| !h.is_finished());
        // Tray menu events (cheap, event-driven): Show / Add URL / Quit
        let tray_act = self.tray.as_ref().and_then(|t| t._nav.as_ref()).map(|nav| {
            let mut act = "";
            while let Ok(ev) = tray_icon::menu::MenuEvent::receiver().try_recv() {
                if ev.id == nav.show_id { act = "show"; } else if ev.id == nav.add_id { act = "add"; } else if ev.id == nav.quit_id { act = "quit"; }
            }
            act
        }).unwrap_or("");
        match tray_act {
            "show" => { ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true)); ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false)); ctx.send_viewport_cmd(egui::ViewportCommand::Focus); }
            "add" => { ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true)); ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false)); ctx.send_viewport_cmd(egui::ViewportCommand::Focus); self.tab = Tab::Downloads; self.show_add = true; }
            "quit" => { self.allow_close = true; self.rt.lock().unwrap().save(); ctx.send_viewport_cmd(egui::ViewportCommand::Close); }
            _ => {}
        }
        let show_request = crate::platform::config_dir().join("show.request");
        if show_request.exists() {
            let _ = std::fs::remove_file(&show_request);
            self.hidden = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        // Extension "Grab site": prefill Grabber and auto-explore.
        { let grabf = crate::platform::config_dir().join("pending-grab.txt"); if let Ok(u) = std::fs::read_to_string(&grabf) { let _ = std::fs::remove_file(&grabf); let u = u.trim().to_string(); if !u.is_empty() { self.grab_url = u; self.tab = Tab::Grabber; self.grab_auto_explore = true; } } }
        crate::drain_inbox(&self.rt);
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
                        if apply { self.grab_files = files; }
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
            let mut g = self.rt.lock().unwrap();
            let ctrls: Vec<(String, u64)> = g.controls.iter().map(|(k, c)| (k.clone(), c.downloaded.load(std::sync::atomic::Ordering::Relaxed))).collect();
            for (id, n) in ctrls { if let Some(t) = g.state.tasks.iter_mut().find(|t| t.id == id) { t.downloaded = n; } }
            let totals: Vec<(String, u64)> = g.controls.iter().map(|(k, c)| (k.clone(), c.total.load(Ordering::Relaxed))).collect();
            for (id, tot) in totals { if tot > 0 { if let Some(t) = g.state.tasks.iter_mut().find(|t| t.id == id) { t.total = Some(tot); } } }
            if self.completed_notice.is_none() {
                let done_name = g.state.tasks.iter().find(|t| t.status == TaskStatus::Complete).map(|t| t.filename.clone());
                if let Some(n) = done_name { if self.last_notified.as_deref() != Some(n.as_str()) { self.completed_notice = Some(n.clone()); self.last_notified = Some(n); } }
            }
            // clipboard monitor
            if g.state.settings.clipboard_monitor {
                if let Ok(mut cb) = arboard::Clipboard::new() {
                    if let Ok(txt) = cb.get_text() {
                        let txt = txt.trim().to_string();
                        if (txt.starts_with("http://") || txt.starts_with("https://")) && txt != g.clip_last && !g.state.tasks.iter().any(|t| t.url == txt) {
                            g.clip_last = txt.clone();
                            drop(cb);
                            let id = g.add_url(&txt, "main", "", "");
                            g.log(format!("Clipboard URL added: {id}"));
                        } else { g.clip_last = txt; }
                    }
                }
            }
        }
        // queue auto-start (respect retry gate next_retry_at)
        {
            let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            let ready: Vec<String> = {
                let g = self.rt.lock().unwrap();
                if !g.state.queues.iter().any(|q| q.auto_start) { Vec::new() } else {
                    let active: Vec<(String, String)> = g.controls.keys().map(|id| (id.clone(), g.state.tasks.iter().find(|t| &t.id == id).map(|t| t.queue_id.clone()).unwrap_or_default())).collect();
                    plan_starts(&g.state.tasks, &g.state.queues, &active, g.state.settings.global_max_concurrent).into_iter()
                        .filter(|id| { g.state.tasks.iter().find(|t| &t.id == id).map(|t| t.next_retry_at <= now && g.state.queues.iter().find(|q| q.id == t.queue_id).map(|q| q.auto_start).unwrap_or(false) && !quota_exhausted(&g, &t.queue_id, now)).unwrap_or(false) })
                        .collect()
                }
            };
            for id in ready { self.start_task(&id); }
        }
        // scheduler tick (every poll, cheap)
        {
            let (day, secs) = today_parts(); self.minutes_today = (day, secs);
            let mut to_start: Vec<String> = Vec::new(); let mut to_stop: Vec<String> = Vec::new();
            {
                let mut g = self.rt.lock().unwrap();
                for s in g.state.schedules.clone() {
                    if should_start(&s, day, secs) {
                        // mark started
                        if let Some(sch) = g.state.schedules.iter_mut().find(|x| x.id == s.id) { sch.last_started_day = day; }
                        to_start.push(s.queue_id.clone());
                        g.log(format!("Schedule start queue {}", s.queue_id));
                    }
                    if should_stop(&s, day, secs) { to_stop.push(s.queue_id.clone()); }
                }
                if !to_start.is_empty() || !to_stop.is_empty() { g.save(); }
            }
            for qid in to_start {
                // route through planner so global/per-queue caps hold (was: direct start, caps bypassed)
                let ids: Vec<String> = {
                    let g = self.rt.lock().unwrap();
                    let active: Vec<(String, String)> = g.controls.keys().map(|id| (id.clone(), g.state.tasks.iter().find(|t| &t.id == id).map(|t| t.queue_id.clone()).unwrap_or_default())).collect();
                    plan_starts(&g.state.tasks, &g.state.queues, &active, g.state.settings.global_max_concurrent).into_iter().filter(|id| g.state.tasks.iter().find(|t| &t.id == id).map(|t| t.queue_id == qid).unwrap_or(false)).collect()
                };
                let limit = self.rt.lock().unwrap().state.schedules.iter().find(|s| s.queue_id == qid).map(|s| s.files_at_once).unwrap_or(ids.len());
                for id in ids.into_iter().take(if limit == 0 { usize::MAX } else { limit }) { self.start_task(&id); }
            }
            for qid in to_stop {
                let g = self.rt.lock().unwrap();
                for (id, c) in &g.controls { if g.state.tasks.iter().find(|t| &t.id == id).map(|t| t.queue_id.clone()).as_deref() == Some(&qid) { c.paused.store(true, std::sync::atomic::Ordering::Relaxed); } }
                // mark paused so slot frees + status visible (was: stayed Downloading)
                let ids: Vec<String> = g.state.tasks.iter().filter(|t| t.queue_id == qid && t.status == TaskStatus::Downloading).map(|t| t.id.clone()).collect();
                drop(g);
                let mut g = self.rt.lock().unwrap();
                for id in ids { if let Some(t) = g.state.tasks.iter_mut().find(|t| t.id == id) { t.status = TaskStatus::Paused; } }
            }
        }
        // Drag-and-drop: URL text / .url files dropped on window become downloads.
        for file in ctx.input(|i| i.raw.dropped_files.clone()) {
            let text = file.path.as_ref().and_then(|p| std::fs::read_to_string(p).ok()).map(|c| c.lines().find_map(|l| l.strip_prefix("URL=").map(|u| u.trim().to_string())).unwrap_or_default()).filter(|u| u.starts_with("http")).or_else(|| file.name.split_whitespace().find(|w| w.starts_with("http")).map(|w| w.to_string()));
            if let Some(url) = text { let mut g = self.rt.lock().unwrap(); g.add_url(&url, "main", "", ""); g.log(format!("Dropped URL added: {url}")); }
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(500));
    }
}

fn today_parts() -> (i64, u32) {
    // Local time approx via system time UTC (scheduler tests use injected clock; UI display uses UTC offset not critical)
    // ponytail: use UTC day/secs; local tz conversion needs chrono/tz dep, skipped.
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as i64;
    ((now / 86400), (now % 86400) as u32)
}

fn run_completion(g: &Runtime, queue_id: &str, task_id: &str) {
    let Some(q) = g.state.queues.iter().find(|q| q.id == queue_id) else { return; };
    let task = g.state.tasks.iter().find(|t| t.id == task_id);
    match q.on_complete {
        CompletionAction::OpenFile => { if let Some(t) = task { let _ = platform::open_path(&t.dest_dir.join(&t.filename)); } }
        CompletionAction::RunCommand => { if !q.on_complete_arg.is_empty() { let parts: Vec<&str> = q.on_complete_arg.split_whitespace().collect(); if !parts.is_empty() { let _ = std::process::Command::new(parts[0]).args(&parts[1..]).spawn(); } } }
        CompletionAction::ExitApp => { std::process::exit(0); }
        CompletionAction::Shutdown => { let _ = platform::execute_power(platform::PowerAction::Shutdown); }
        CompletionAction::Sleep => { let _ = platform::execute_power(platform::PowerAction::Sleep); }
        CompletionAction::None => {}
    }
}

impl DownloadApp {
    fn grab_project(&self, mirror: bool) -> GrabberProject {
        let split = |s: &str| s.split(';').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect();
        GrabberProject { start_url: self.grab_url.clone(), depth: self.grab_depth, external_depth: self.grab_ext_depth, scope: self.grab_scope, extensions: self.grab_exts.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(), max_pages: 200, mirror, dont_explore_parent_dirs: self.grab_parent_dirs, only_paths: split(&self.grab_only_paths), exclude_paths: split(&self.grab_exclude_paths), ..Default::default() }
    }
}

impl eframe::App for DownloadApp {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.rt.lock().unwrap().save();
        // Release single-instance lock only if it still holds our pid.
        let lock = crate::platform::config_dir().join("instance.lock");
        if let Ok(txt) = std::fs::read_to_string(&lock) {
            if txt.trim() == std::process::id().to_string() { let _ = std::fs::remove_file(&lock); }
        }
    }
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        theme::apply(ctx);
        // Paint full viewport first: panel gaps otherwise show OS black (#09) as a vertical bar.
        ctx.layer_painter(egui::LayerId::background()).rect_filled(ctx.screen_rect(), 0.0, theme::BG);
        self.poll(ctx);
        // Close-to-tray: window X hides to tray (setting existed but was never wired).
        if !self.allow_close && ctx.input(|i| i.viewport().close_requested()) {
            let to_tray = { let g = self.rt.lock().unwrap(); g.state.settings.close_to_tray } && self.tray.as_ref().and_then(|t| t._nav.as_ref()).is_some();
            if to_tray {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                return;
            }
        }
        if self.hidden { return; }
        ctx.send_viewport_cmd(egui::ViewportCommand::Title("downloadcraft — Download Manager".into()));

        // ── Top bar: brand + menus (IDM: Tasks/File/Downloads/View/Help) ──
        egui::TopBottomPanel::top("topbar").frame(egui::Frame::none().fill(theme::SURFACE).stroke(egui::Stroke::new(1.0f32, theme::BORDER)).inner_margin(egui::Margin::symmetric(8.0, 4.0))).show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new("downloadcraft").color(theme::TEXT).strong().size(17.0));
                theme::chip(ui, "v0.2.0", theme::TEAL_500);
                ui.add_space(14.0);
                egui::menu::bar(ui, |ui| {
                    ui.menu_button("Tasks", |ui| {
                        if ui.button("Add new download…").clicked() { self.show_add = true; ui.close_menu(); }
                        if ui.button("Add batch from clipboard").clicked() { add_batch_clipboard(self); ui.close_menu(); }
                        if ui.button("Run Site Grabber").clicked() { self.tab = Tab::Grabber; ui.close_menu(); }
                        ui.separator();
                        if ui.button("Export queue (JSON)").clicked() { export_json(self); ui.close_menu(); }
                        if ui.button("Quit").clicked() { std::process::exit(0); }
                    });
                    ui.menu_button("File", |ui| {
                        let sel = self.selected.clone();
                        if ui.button("Download now").clicked() { if let Some(id) = &sel { self.start_task(id); } ui.close_menu(); }
                        if ui.button("Stop download").clicked() { if let Some(id) = &sel { stop_task(self, id); } ui.close_menu(); }
                        if ui.button("Remove").clicked() { if let Some(id) = &sel { remove_task(self, id); } ui.close_menu(); }
                        if ui.button("Redownload").clicked() { if let Some(id) = &sel { self.start_task(id); } ui.close_menu(); }
                    });
                    ui.menu_button("Downloads", |ui| {
                        if ui.button("Resume all").clicked() { resume_all(self); ui.close_menu(); }
                        if ui.button("Stop all").clicked() { stop_all(self); ui.close_menu(); }
                        if ui.button("Delete all completed").clicked() { delete_completed(self); ui.close_menu(); }
                        ui.separator();
                        if ui.button("Scheduler…").clicked() { self.tab = Tab::Scheduler; ui.close_menu(); }
                        if ui.button("Start queue").clicked() { self.tab = Tab::Queues; ui.close_menu(); }
                        if ui.button("Speed limiter: toggle").clicked() { toggle_limiter(self); ui.close_menu(); }
                        if ui.button("Options…").clicked() { self.tab = Tab::Options; ui.close_menu(); }
                    });
                    ui.menu_button("View", |ui| {
                        if ui.button("Downloads").clicked() { self.tab = Tab::Downloads; ui.close_menu(); }
                        if ui.button("Queues").clicked() { self.tab = Tab::Queues; ui.close_menu(); }
                        if ui.button("Scheduler").clicked() { self.tab = Tab::Scheduler; ui.close_menu(); }
                        if ui.button("Site Grabber").clicked() { self.tab = Tab::Grabber; ui.close_menu(); }
                        if ui.button("Options").clicked() { self.tab = Tab::Options; ui.close_menu(); }
                    });
                    ui.menu_button("Help", |ui| {
                        if ui.button("About downloadcraft").clicked() { self.show_about = true; ui.close_menu(); }
                    });
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let active = { let g = self.rt.lock().unwrap(); g.state.tasks.iter().filter(|t| t.status == TaskStatus::Downloading).count() };
                    let speed = self.speed_hist.last().copied().unwrap_or(0.0);
                    theme::chip(ui, &format!("{active} active · {}", fmt_rate(speed)), theme::TEXT_MUTED);
                });
            });
        });

        // ── Left rail: navigation + categories (IDM category tree, modern rail) ──
        egui::SidePanel::left("rail").resizable(false).show_separator_line(false).exact_width(208.0)
            .frame(egui::Frame::none().fill(theme::SURFACE).stroke(egui::Stroke::new(1.0f32, theme::BORDER)).inner_margin(egui::Margin::same(6.0))).show(ctx, |ui| {
            ui.set_width(ui.available_width());
                ui.label(egui::RichText::new("WORKSPACE").color(theme::TEXT_FAINT).small().strong());
                ui.add_space(4.0);
                for (tab, label, ic) in [(Tab::Downloads, "Downloads", Icon::Download), (Tab::Queues, "Queues", Icon::Queue), (Tab::Scheduler, "Scheduler", Icon::Clock), (Tab::Grabber, "Site Grabber", Icon::Globe), (Tab::Options, "Options", Icon::Settings)] {
                    let on = self.tab == tab;
                    let col = if on { egui::Color32::WHITE } else { theme::TEXT_MUTED };
                    if icon_btn(ui, ic, label, on, ui.available_width() - 4.0).clicked() { self.tab = tab; }
                    let _ = col;
                }
                ui.add_space(14.0);
                ui.separator();
                ui.horizontal(|ui| { let (irect, _) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover()); icon(ui.painter(), &Icon::Folder, irect, theme::TEXT_FAINT); ui.label(egui::RichText::new("CATEGORIES").color(theme::TEXT_FAINT).small().strong()); });
                ui.add_space(4.0);
                let cats: &[(&str, Icon)] = &[("All Downloads", Icon::Download), ("Compressed", Icon::Archive), ("Documents", Icon::Document), ("Music", Icon::Music), ("Programs", Icon::Program), ("Video", Icon::Video), ("Audio", Icon::Music), ("General", Icon::File), ("Unfinished", Icon::Clock), ("Finished", Icon::Check), ("Grabber projects", Icon::Globe), ("Queues", Icon::Queue)];
                for (c, ic) in cats {
                    let on = self.category == *c;
                    let col = if on { theme::TEAL_500 } else { theme::TEXT_MUTED };
                    if icon_btn(ui, *ic, c, on, ui.available_width() - 4.0).clicked() { self.category = c.to_string(); self.tab = Tab::Downloads; }
                    let _ = col;
                }
                ui.add_space(14.0);
                ui.separator();
                ui.label(egui::RichText::new("ffmpeg: PATH/beside exe only").color(theme::TEXT_FAINT).small());
                ui.label(egui::RichText::new("DRM content refused").color(theme::TEXT_FAINT).small());
            });

        // ── Bottom status bar ──
        egui::TopBottomPanel::bottom("statusbar").frame(egui::Frame::none().fill(theme::SURFACE).stroke(egui::Stroke::new(1.0f32, theme::BORDER)).inner_margin(egui::Margin::symmetric(8.0, 3.0))).show(ctx, |ui| {
            ui.horizontal(|ui| {
                let (total, done, active) = { let g = self.rt.lock().unwrap(); (g.state.tasks.len(), g.state.tasks.iter().filter(|t| t.status == TaskStatus::Complete).count(), g.state.tasks.iter().filter(|t| t.status == TaskStatus::Downloading).count()) };
                ui.label(egui::RichText::new(format!("{total} downloads · {done} finished · {active} active")).color(theme::TEXT_MUTED).small());
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
                    painter.line_segment([egui::pos2(x0, y0), egui::pos2(x1, y1)], egui::Stroke::new(1.5f32, theme::TEAL_500));
                }
                let _ = id;
                ui.label(egui::RichText::new(fmt_rate(self.speed_hist.last().copied().unwrap_or(0.0))).color(theme::TEAL_500).small());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(egui::RichText::new("Ready").color(theme::TEXT_FAINT).small());
                });
            });
        });

        // Left margin 0: any BG gap beside rail reads as a stray vertical bar.
        egui::CentralPanel::default().frame(egui::Frame::none().fill(theme::BG).inner_margin(egui::Margin { left: 0.0, right: 8.0, top: 8.0, bottom: 8.0 })).show(ctx, |ui| {
            match self.tab {
                Tab::Downloads => self.ui_downloads(ui),
                // Grabber manages own scroll: results list fills remaining height.
                Tab::Grabber => self.ui_grabber(ui),
                Tab::Queues | Tab::Scheduler | Tab::Options => {
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                        match self.tab {
                            Tab::Queues => self.ui_queues(ui, false),
                            Tab::Scheduler => self.ui_queues(ui, true),
                            Tab::Options => self.ui_options(ui),
                            _ => {}
                        }
                    });
                }
            }
        });

        if self.show_about {
            egui::Window::new("About downloadcraft").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
                ui.label("downloadcraft 0.2.0 — clean-room cross-platform download manager.");
                ui.label("Rust + egui. Segmented downloads, DASH/HLS, queues, scheduler, Site Grabber.");
                ui.label("No proprietary code, binaries, icons, or activation logic.");
                if ui.button("Close").clicked() { self.show_about = false; }
            });
        }
        if self.show_add { self.ui_add_dialog(ctx); }
        if let Some(id) = self.show_progress_for.clone() { self.ui_progress_dialog(ctx, &id); }
        if let Some(name) = self.completed_notice.clone() {
            egui::Window::new("Download complete").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
                ui.label(format!("{name} finished."));
                ui.horizontal(|ui| {
                    if theme::primary_btn(ui, "Open file").clicked() {
                        if let Some(t) = self.rt.lock().unwrap().state.tasks.iter().find(|t| t.filename == name) { let p = t.dest_dir.join(&t.filename); let _ = crate::platform::open_path(&p); }
                        self.completed_notice = None;
                    }
                    if theme::ghost_btn(ui, "Close").clicked() { self.completed_notice = None; }
                });
            });
        }
        // Global sparkline uses same smoothed per-task rates shown in rows.
        // End-of-frame aggregate byte deltas jump with chunk bursts and repaint cadence.
        let ids: Vec<String> = { let g = self.rt.lock().unwrap(); g.state.tasks.iter().filter(|t| t.status == TaskStatus::Downloading).map(|t| t.id.clone()).collect() };
        let rate = ids.iter().map(|id| self.task_rate(id) as f32).sum::<f32>();
        self.last_tick = std::time::Instant::now();
        self.last_dl_total = ids.iter().map(|id| self.rate_map.get(id).and_then(|v| v.last()).map(|(_, b)| *b).unwrap_or(0)).sum();
        if self.speed_hist.len() > 40 { self.speed_hist.remove(0); }
        self.speed_hist.push(rate);
    }
}

/// IDM download-limits quota: true when schedule for queue exhausted its MB/hours window.
fn quota_exhausted(g: &Runtime, queue_id: &str, now: u64) -> bool {
    g.state.schedules.iter().filter(|s| s.queue_id == queue_id && s.quota_mb > 0).any(|s| {
        s.quota_window_start != 0 && now.saturating_sub(s.quota_window_start) < s.quota_hours.max(1) * 3600 && s.quota_used_bytes >= s.quota_mb * 1024 * 1024
    })
}

/// Category icon by extension (sidebar + row glyphs).
fn cat_icon(filename: &str) -> Icon {
    let e = filename.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
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
    if let Some(h) = app.handles.remove(id) { h.abort(); }
    let mut g = app.rt.lock().unwrap();
    let downloaded = if let Some(c) = g.controls.remove(id) {
        c.cancelled.store(true, Ordering::Relaxed);
        c.paused.store(false, Ordering::Relaxed);
        c.downloaded.load(Ordering::Relaxed)
    } else { 0 };
    if let Some(t) = g.state.tasks.iter_mut().find(|t| t.id == id) {
        t.status = TaskStatus::Paused;
        if downloaded > 0 { t.downloaded = downloaded; }
    }
    app.rate_smooth.remove(id);
    g.save();
}
fn remove_task(app: &mut DownloadApp, id: &str) {
    if let Some(h) = app.handles.remove(id) { h.abort(); }
    let mut g = app.rt.lock().unwrap();
    if let Some(c) = g.controls.get(id) { c.cancelled.store(true, Ordering::Relaxed); c.paused.store(false, Ordering::Relaxed); }
    g.state.tasks.retain(|t| t.id != id);
    g.controls.remove(id);
    app.selected = None;
    if app.show_progress_for.as_deref() == Some(id) { app.show_progress_for = None; }
    g.save();
}
fn resume_all(app: &mut DownloadApp) { let ids: Vec<String> = { let g = app.rt.lock().unwrap(); g.state.tasks.iter().filter(|t| matches!(t.status, TaskStatus::Paused | TaskStatus::Error | TaskStatus::Queued)).map(|t| t.id.clone()).collect() }; for id in ids { app.start_task(&id); } }
fn stop_all(app: &mut DownloadApp) { let ids: Vec<String> = { let g = app.rt.lock().unwrap(); g.state.tasks.iter().filter(|t| t.status == TaskStatus::Downloading).map(|t| t.id.clone()).collect() }; for id in ids { stop_task(app, &id); } }
fn delete_completed(app: &mut DownloadApp) { let mut g = app.rt.lock().unwrap(); g.state.tasks.retain(|t| t.status != TaskStatus::Complete); g.save(); }
fn toggle_limiter(app: &mut DownloadApp) { let mut g = app.rt.lock().unwrap(); g.state.settings.speed_limit_kbps = if g.state.settings.speed_limit_kbps == 0 { 512 } else { 0 }; let lim = g.state.settings.speed_limit_kbps; for c in g.controls.values() { c.limit_kbps.store(lim, Ordering::Relaxed); } g.save(); }
fn export_json(app: &mut DownloadApp) { let g = app.rt.lock().unwrap(); let txt = serde_json::to_string_pretty(&g.state.tasks.iter().map(|t| serde_json::json!({"url":t.url,"filename":t.filename,"queue":t.queue_id})).collect::<Vec<_>>()).unwrap_or_default(); let path = g.state.settings.download_dir.join("downloadcraft-export.json"); let _ = std::fs::write(path, txt); }
fn add_batch_clipboard(app: &mut DownloadApp) {
    if let Ok(mut cb) = arboard::Clipboard::new() { if let Ok(txt) = cb.get_text() { for line in txt.lines() { let u = line.trim(); if u.starts_with("http://") || u.starts_with("https://") { add_url_task(app, u); } } } }
}
fn add_url_task(app: &mut DownloadApp, url: &str) {
    let id = { let mut g = app.rt.lock().unwrap(); g.add_url(url, "main", "", "") };
    app.start_task(&id);
}
fn fmt_rate(bps: f32) -> String { if bps <= 0.0 { "0 B/s".into() } else if bps < 1024.0 { format!("{bps:.0} B/s") } else if bps < 1024.0 * 1024.0 { format!("{:.1} KB/s", bps / 1024.0) } else { format!("{:.2} MB/s", bps / 1048576.0) } }
fn eta(secs: f64) -> String { if !secs.is_finite() || secs < 0.0 { "—".into() } else if secs < 60.0 { format!("{secs:.0}s") } else if secs < 3600.0 { format!("{:.0}m {:.0}s", secs / 60.0, secs % 60.0) } else { format!("{:.0}h {:.0}m", secs / 3600.0, (secs % 3600.0) / 60.0) } }


/// IDM-style toolbar button: icon glyph + label in one bordered, clickable cell.
/// (egui Button takes text only; separate icon widgets left icon outside hit area and label looked like plain text.)
fn icon_btn(ui: &mut egui::Ui, ic: Icon, label: &str, primary: bool, min_w: f32) -> egui::Response {
    icon_btn_enabled(ui, ic, label, primary, min_w, true)
}

fn icon_btn_enabled(ui: &mut egui::Ui, ic: Icon, label: &str, primary: bool, min_w: f32, enabled: bool) -> egui::Response {
    let fill = if !enabled { theme::SURFACE } else if primary { theme::STEEL_DIM } else { theme::SURFACE_RAISED };
    let fg = if enabled { theme::TEXT } else { theme::TEXT_FAINT };
    // min_w = total framed width. Frame adds 14px horizontal margin; counting it
    // twice overflowed the 208px rail by 10px and pushed CentralPanel right (grey bar).
    let resp = egui::Frame::none().fill(fill).stroke(egui::Stroke::new(1.0f32, theme::BORDER_STRONG)).inner_margin(egui::Margin::symmetric(7.0, 4.0)).show(ui, |ui| {
        ui.set_min_width((min_w - 14.0).max(0.0));
        ui.horizontal(|ui| {
            let (irect, _) = ui.allocate_exact_size(egui::vec2(15.0, 15.0), egui::Sense::hover());
            icon(ui.painter(), &ic, irect, fg);
            ui.label(egui::RichText::new(label).color(fg));
        });
    }).response;
    let resp = resp.interact(if enabled { egui::Sense::click() } else { egui::Sense::hover() });
    if enabled && resp.hovered() { ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand); }
    resp
}

/// Download-table column widths shared by header and rows (separate Grids auto-sized
/// independently, so header and row columns never lined up). Name column takes remainder.
fn dl_cols(avail: f32, compact: bool) -> Vec<f32> {
    let fixed: &[f32] = if compact { &[78.0, 86.0, 180.0, 150.0] } else { &[56.0, 78.0, 86.0, 70.0, 88.0, 180.0, 150.0] };
    let gaps = 10.0 * fixed.len() as f32;
    let mut v = vec![(avail - fixed.iter().sum::<f32>() - gaps).max(120.0)];
    v.extend_from_slice(fixed);
    v
}

fn cell<R>(ui: &mut egui::Ui, w: f32, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.allocate_ui_with_layout(egui::vec2(w, 0.0), egui::Layout::left_to_right(egui::Align::Center), |ui| { ui.set_width(w); add(ui) }).inner
}

/// Bordered action group: related controls stay together and wrap as one unit.
fn action_group(ui: &mut egui::Ui, label: &str, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::none().fill(theme::SURFACE).stroke(egui::Stroke::new(1.0f32, theme::BORDER)).inner_margin(egui::Margin::symmetric(6.0, 4.0)).show(ui, |ui| {
        ui.vertical(|ui| {
            ui.label(egui::RichText::new(label).color(theme::TEXT_FAINT).small().strong());
            ui.horizontal_wrapped(add);
        });
    });
}

impl DownloadApp {
    fn ui_downloads(&mut self, ui: &mut egui::Ui) {
        // Header + actions (IDM toolbar: Add URL, Resume, Stop, Stop All, Delete, Options…)
        ui.horizontal_wrapped(|ui| {
            ui.heading("Downloads");
            theme::chip(ui, &self.category.clone(), theme::TEXT_MUTED);
        });
        ui.add_space(4.0);
        let selected_task = self.selected.as_ref().and_then(|id| { let g = self.rt.lock().unwrap(); g.state.tasks.iter().find(|t| &t.id == id).cloned() });
        let has_active = { let g = self.rt.lock().unwrap(); g.state.tasks.iter().any(|t| t.status == TaskStatus::Downloading) };
        let has_completed = { let g = self.rt.lock().unwrap(); g.state.tasks.iter().any(|t| t.status == TaskStatus::Complete) };
        ui.horizontal_wrapped(|ui| {
            action_group(ui, "Add", |ui| {
                if icon_btn(ui, Icon::Plus, "Add URL", true, 86.0).clicked() { self.show_add = true; }
            });
            action_group(ui, "Selected download", |ui| {
                let can_resume = selected_task.as_ref().is_some_and(|t| !matches!(t.status, TaskStatus::Downloading | TaskStatus::Complete));
                let can_stop = selected_task.as_ref().is_some_and(|t| t.status == TaskStatus::Downloading);
                if icon_btn_enabled(ui, Icon::Play, "Resume", false, 78.0, can_resume).clicked() { if let Some(id) = self.selected.clone() { self.start_task(&id); } }
                if icon_btn_enabled(ui, Icon::Pause, "Stop", false, 62.0, can_stop).clicked() { if let Some(id) = self.selected.clone() { stop_task(self, &id); } }
                if let Some(t) = &selected_task {
                    if icon_btn(ui, Icon::Info, "Progress", false, 82.0).clicked() { self.show_progress_for = Some(t.id.clone()); }
                } else {
                    let _ = icon_btn_enabled(ui, Icon::Info, "Progress", false, 82.0, false);
                }
                if icon_btn_enabled(ui, Icon::Delete, "Delete", false, 70.0, selected_task.is_some()).clicked() { if let Some(id) = self.selected.clone() { remove_task(self, &id); } }
            });
            action_group(ui, "All downloads", |ui| {
                if icon_btn_enabled(ui, Icon::Pause, "Stop all", false, 74.0, has_active).clicked() { stop_all(self); }
                if icon_btn_enabled(ui, Icon::Check, "Delete completed", false, 128.0, has_completed).clicked() { delete_completed(self); }
            });
        });
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new("Filter").color(theme::TEXT_MUTED).small());
            ui.add(egui::TextEdit::singleline(&mut self.search).hint_text("Search filename or URL…").desired_width((ui.available_width() - 90.0).clamp(180.0, 360.0)));
            ui.label(egui::RichText::new("Click headers to sort · Double-click row: progress · Right-click: actions").color(theme::TEXT_FAINT).small());
        });
        ui.add_space(8.0);

        // snapshot + filter + sort
        let mut tasks: Vec<DownloadTask> = { let g = self.rt.lock().unwrap(); g.state.tasks.clone() };
        let cat = self.category.clone();
        tasks.retain(|t| {
            let cat_ok = match cat.as_str() {
                "All Downloads" => true,
                "Unfinished" => t.status != TaskStatus::Complete,
                "Finished" => t.status == TaskStatus::Complete,
                "Grabber projects" => t.queue_id == "grabber",
                "Queues" => true,
                c => t.category == c || crate::model::category_for(&t.filename) == c,
            };
            let q = self.search.to_ascii_lowercase();
            cat_ok && (q.is_empty() || t.filename.to_ascii_lowercase().contains(&q) || t.url.to_ascii_lowercase().contains(&q))
        });
        let rate_now = self.speed_hist.last().copied().unwrap_or(0.0) as f64; let _ = rate_now;
        match self.sort_col {
            0 => tasks.sort_by_key(|a| a.filename.to_lowercase()),
            1 => tasks.sort_by_key(|t| t.queue_id.clone()),
            2 => tasks.sort_by_key(|t| t.total.unwrap_or(0)),
            3 => tasks.sort_by_key(|t| format!("{:?}", t.status)),
            _ => tasks.sort_by_key(|t| t.order),
        }
        if !self.sort_asc { tasks.reverse(); }

        let mut to_start: Option<String> = None;
        let mut to_stop: Option<String> = None;
        let mut to_remove: Option<String> = None;
        let mut to_progress: Option<String> = None;
        let compact_table = ui.available_width() < 760.0;
        let cols = dl_cols(ui.available_width() - 18.0, compact_table);
        // IDM columns on wide windows; compact set on narrow windows (no squeezed overlap).
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        egui::Frame::none().fill(theme::SURFACE_RAISED).stroke(egui::Stroke::new(1.0f32, theme::BORDER)).inner_margin(egui::Margin::symmetric(8.0, 4.0)).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                for (i, h) in if compact_table { vec!["File Name", "Size", "Status", "Progress", ""] } else { vec!["File Name", "Q", "Size", "Status", "Time left", "Transfer rate", "Progress", ""] }.iter().enumerate() {
                    let arrow = if self.sort_col == i as u8 && !h.is_empty() { if self.sort_asc { " ▲" } else { " ▼" } } else { "" };
                    let clicked = cell(ui, cols[i], |ui| ui.add(egui::Button::new(egui::RichText::new(format!("{h}{arrow}")).color(theme::TEXT_MUTED).small().strong()).frame(false)).clicked());
                    if clicked {
                        if self.sort_col == i as u8 { self.sort_asc = !self.sort_asc; } else { self.sort_col = i as u8; self.sort_asc = true; }
                    }
                }
            });
        });
        ui.add_space(4.0);

        if tasks.is_empty() {
            egui::Frame::none().fill(theme::SURFACE).stroke(egui::Stroke::new(1.0f32, theme::BORDER)).inner_margin(egui::Margin::same(24.0)).show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.label(egui::RichText::new("No downloads here").color(theme::TEXT).size(16.0).strong());
                    ui.label(egui::RichText::new("Add a URL, paste from clipboard (Tasks → Add batch), or send from browser extension.").color(theme::TEXT_MUTED));
                    if theme::primary_btn(ui, "+ Add URL").clicked() { self.show_add = true; }
                });
            });
            return;
        }

        {
            for t in &tasks {
                let sel = self.selected.as_deref() == Some(t.id.as_str());
                let fill = if sel { theme::STEEL_DIM } else { theme::SURFACE };
                let resp = egui::Frame::none().fill(fill).stroke(egui::Stroke::new(1.0f32, theme::BORDER)).inner_margin(egui::Margin::symmetric(8.0, 4.0)).show(ui, |ui| {
                    ui.horizontal(|ui| { ui.spacing_mut().item_spacing.x = 10.0; let mut ci = 0usize; let mut nw = || { ci += 1; cols[ci - 1] };
                        let task_rate = self.task_rate(&t.id);
                        cell(ui, nw(), |ui| { let (irect, _) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover()); icon(ui.painter(), &cat_icon(&t.filename), irect, theme::TEXT_MUTED); ui.add(egui::Label::new(egui::RichText::new(&t.filename).color(theme::TEXT).strong()).truncate()).on_hover_text(&t.filename); });
                        if !compact_table { let w = nw(); cell(ui, w, |ui| ui.label(egui::RichText::new(if t.queue_id.is_empty() { "main" } else { &t.queue_id }).color(theme::TEXT_MUTED).small())); }
                        let w = nw(); cell(ui, w, |ui| ui.label(egui::RichText::new(match t.total { Some(n) => fmt_bytes(n), None => "—".into() }).color(theme::TEXT_MUTED).small()));
                        let sc = theme::status_color(t.status);
                        let w = nw(); cell(ui, w, |ui| ui.label(egui::RichText::new(format!("{:?}", t.status)).color(sc).small().strong()));
                        if !compact_table {
                            // ETA from THIS task's own rate (was global rate for all rows)
                            let eta_txt = if t.status == TaskStatus::Downloading { if let Some(total) = t.total { if task_rate > 1.0 && total > t.downloaded { eta((total - t.downloaded) as f64 / task_rate) } else { "—".into() } } else { "—".into() } } else { "—".into() };
                            let w = nw(); cell(ui, w, |ui| ui.label(egui::RichText::new(eta_txt).color(theme::TEXT_MUTED).small()));
                            let w = nw(); cell(ui, w, |ui| ui.label(egui::RichText::new(if t.status == TaskStatus::Downloading { fmt_rate(task_rate as f32) } else { "—".into() }).color(theme::TEXT_MUTED).small()));
                        }
                        let seg_view = self.seg_info.get(&t.id);
                        let frac_opt = display_fraction(t, seg_view);
                        let frac = frac_opt.unwrap_or(0.0);
                        let w = nw(); cell(ui, w, |ui| ui.vertical(|ui| {
                            if frac_opt.is_some() {
                                ui.add(egui::ProgressBar::new(frac).desired_width(170.0).fill(theme::PRIMARY).text(format!("{:.1}%", frac * 100.0)));
                            } else if t.status == TaskStatus::Downloading {
                                ui.add(egui::ProgressBar::new(0.0).desired_width(170.0).fill(theme::PRIMARY).animate(true).text("receiving".to_string()));
                            } else {
                                ui.add(egui::ProgressBar::new(frac).desired_width(170.0).fill(theme::PRIMARY).text(format!("{:.1}%", frac * 100.0)));
                            }
                            ui.label(egui::RichText::new(if t.total.is_some() { format!("{} of {}", fmt_bytes(t.downloaded), t.total.map(fmt_bytes).unwrap_or_else(|| "?".into())) } else if let Some(v) = seg_view.filter(|v| v.manifest) { format!("{} | {}", fmt_bytes(t.downloaded), v.label()) } else { format!("{} of ?", fmt_bytes(t.downloaded)) }).color(theme::TEXT_FAINT).small());
                            if t.status == TaskStatus::Downloading { if let Some(si) = self.seg_info.get(&t.id).cloned() { parts_strip(ui, &si, 170.0); ui.label(egui::RichText::new(si.label()).color(theme::TEXT_FAINT).small()); } }
                        }));
                        let w = nw(); cell(ui, w, |ui| {
                            if t.status == TaskStatus::Downloading {
                                if icon_btn(ui, Icon::Pause, "Stop", false, 56.0).clicked() { to_stop = Some(t.id.clone()); }
                            } else if t.status != TaskStatus::Complete && icon_btn(ui, Icon::Play, "Start", false, 58.0).clicked() { to_start = Some(t.id.clone()); }
                            if icon_btn(ui, Icon::Info, "Progress", false, 82.0).clicked() { to_progress = Some(t.id.clone()); }
                        });
                    });
                    if !t.error.is_empty() { ui.label(egui::RichText::new(&t.error).color(theme::RED_600).small()); }
                }).response;
                let resp = resp.interact(egui::Sense::click());
                if resp.clicked() { self.selected = Some(t.id.clone()); }
                if resp.double_clicked() { to_progress = Some(t.id.clone()); }
                resp.context_menu(|ui| {
                    if ui.button("Start / Resume").clicked() { to_start = Some(t.id.clone()); ui.close_menu(); }
                    if ui.button("Stop").clicked() { to_stop = Some(t.id.clone()); ui.close_menu(); }
                    if ui.button("Progress…").clicked() { to_progress = Some(t.id.clone()); ui.close_menu(); }
                    if ui.button("Open folder").clicked() { let _ = crate::platform::reveal_path(&t.dest_dir.join(&t.filename)); ui.close_menu(); }
                    if ui.button("Copy URL").clicked() { ui.output_mut(|o| o.copied_text = t.url.clone()); ui.close_menu(); }
                    ui.separator();
                    if ui.button("Remove").clicked() { to_remove = Some(t.id.clone()); ui.close_menu(); }
                });
                ui.add_space(3.0);
            }
        }
        });
        if let Some(id) = to_start { self.start_task(&id); }
        if let Some(id) = to_stop { stop_task(self, &id); }
        if let Some(id) = to_remove { remove_task(self, &id); }
        if let Some(id) = to_progress { self.show_progress_for = Some(id); }
    }

    fn ui_add_dialog(&mut self, ctx: &egui::Context) {
        egui::Window::new("Add download").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
            ui.set_min_width(460.0);
            ui.label(egui::RichText::new("URL").color(theme::TEXT_MUTED).small().strong());
            ui.add(egui::TextEdit::singleline(&mut self.add_url).hint_text("https://example.com/file.zip or .mpd/.m3u8").desired_width(440.0));
            ui.horizontal(|ui| {
                if ui.button("Paste").clicked() { if let Ok(mut cb) = arboard::Clipboard::new() { if let Ok(t) = cb.get_text() { self.add_url = t.trim().to_string(); } } }
                ui.label(egui::RichText::new("Supports segmented HTTP, DASH, HLS").color(theme::TEXT_FAINT).small());
            });
            ui.add_space(6.0);
            egui::Grid::new("add-grid").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Queue"); ui.add(egui::TextEdit::singleline(&mut self.add_queue).desired_width(200.0)); ui.end_row();
                ui.label("Save to"); ui.label(egui::RichText::new(self.rt.lock().unwrap().state.settings.download_dir.display().to_string()).monospace().small()); ui.end_row();
                ui.label("Referrer"); ui.add(egui::TextEdit::singleline(&mut self.add_referrer).desired_width(300.0)); ui.end_row();
                ui.label("Cookies"); ui.add(egui::TextEdit::singleline(&mut self.add_cookies).desired_width(300.0)); ui.end_row();
                ui.label("Mirrors"); ui.add(egui::TextEdit::singleline(&mut self.add_mirrors).hint_text("mirror URLs, comma separated (failover)").desired_width(300.0)); ui.end_row();
                ui.label("Description"); ui.add(egui::TextEdit::singleline(&mut self.add_desc).desired_width(300.0)); ui.end_row();
            });
            ui.add_space(4.0);
            // IDM Add URL: "Use authorization" + Login/Password
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Authorization (Basic)").color(theme::TEXT_MUTED).small().strong());
                ui.label("Login:"); ui.add(egui::TextEdit::singleline(&mut self.add_user).desired_width(130.0));
                ui.label("Password:"); ui.add(egui::TextEdit::singleline(&mut self.add_pass).password(true).desired_width(130.0));
            });
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if theme::primary_btn(ui, "Start download").clicked() {
                    let url = self.add_url.trim().to_string();
                    if !url.is_empty() {
                        let (referrer, cookies, queue) = (self.add_referrer.clone(), self.add_cookies.clone(), if self.add_queue.is_empty() { "main".into() } else { self.add_queue.clone() });
                        let (user, pass, mirrors, desc) = (self.add_user.clone(), self.add_pass.clone(), self.add_mirrors.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect::<Vec<_>>(), self.add_desc.clone());
                        let id = { let mut g = self.rt.lock().unwrap(); let id = g.add_url(&url, &queue, &referrer, &cookies); let segs = g.state.settings.max_connections; if let Some(t) = g.state.tasks.iter_mut().find(|t| t.id == id) { t.segments = segs; t.username = user; t.password = pass; t.mirrors = mirrors; t.description = desc; } g.save(); id };
                        self.start_task(&id);
                        self.add_url.clear(); self.show_add = false;
                    }
                }
                if theme::ghost_btn(ui, "Cancel").clicked() { self.show_add = false; }
            });
        });
    }

    fn ui_progress_dialog(&mut self, ctx: &egui::Context, id: &str) {
        let task = { let g = self.rt.lock().unwrap(); g.state.tasks.iter().find(|t| t.id == id).cloned() };
        let Some(t) = task else { self.show_progress_for = None; return; };
        let mut close = false; let mut toggle_pause = false;
        egui::Window::new("Download progress").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
            ui.set_min_width(420.0);
            ui.label(egui::RichText::new(&t.filename).strong().size(15.0));
            ui.label(egui::RichText::new(&t.url).color(theme::TEXT_FAINT).small());
            ui.add_space(8.0);
            let frac_opt = display_fraction(&t, self.seg_info.get(&t.id));
            let frac = frac_opt.unwrap_or(0.0);
            if frac_opt.is_some() {
                ui.add(egui::ProgressBar::new(frac).desired_width(400.0).fill(theme::PRIMARY).text(format!("{:.1}%", frac * 100.0)));
            } else if t.status == TaskStatus::Downloading {
                ui.add(egui::ProgressBar::new(0.0).desired_width(400.0).fill(theme::PRIMARY).animate(true).text("receiving".to_string()));
            } else {
                ui.add(egui::ProgressBar::new(frac).desired_width(400.0).fill(theme::PRIMARY).text(format!("{:.1}%", frac * 100.0)));
            }
            ui.add_space(6.0);
            let task_rate = self.task_rate(&t.id);
            let eta_txt = match t.total { Some(total) if t.status == TaskStatus::Downloading && task_rate > 1.0 && total > t.downloaded => eta((total - t.downloaded) as f64 / task_rate), _ => "—".into() };
            egui::Grid::new("prog-grid").num_columns(2).spacing([14.0, 4.0]).show(ui, |ui| {
                ui.label(egui::RichText::new("Status").color(theme::TEXT_MUTED)); ui.label(format!("{:?}", t.status)); ui.end_row();
                ui.label(egui::RichText::new("Transfer rate").color(theme::TEXT_MUTED)); ui.label(if t.status == TaskStatus::Downloading { fmt_rate(task_rate as f32) } else { "—".into() }); ui.end_row();
                ui.label(egui::RichText::new("Time left").color(theme::TEXT_MUTED)); ui.label(eta_txt); ui.end_row();
                ui.label(egui::RichText::new("Segments").color(theme::TEXT_MUTED)); ui.label(self.seg_info.get(&t.id).map(|v| v.label()).unwrap_or_else(|| format!("{} connections", t.segments))); ui.end_row();
                ui.label(egui::RichText::new("Downloaded").color(theme::TEXT_MUTED)); ui.label(format!("{} / {}", fmt_bytes(t.downloaded), t.total.map(fmt_bytes).unwrap_or_else(|| "—".into()))); ui.end_row();
                ui.label(egui::RichText::new("Save to").color(theme::TEXT_MUTED)); ui.label(t.dest_dir.display().to_string()); ui.end_row();
                ui.label(egui::RichText::new("Queue").color(theme::TEXT_MUTED)); ui.label(if t.queue_id.is_empty() { "main".into() } else { t.queue_id.clone() }); ui.end_row();
                if !t.error.is_empty() { ui.label(egui::RichText::new("Error").color(theme::RED_600)); ui.label(egui::RichText::new(&t.error).color(theme::RED_600)); ui.end_row(); }
            });
            if let Some(view) = self.seg_info.get(&t.id).cloned() {
                if !view.parts.is_empty() {
                    ui.add_space(8.0);
                    ui.label(egui::RichText::new("Parts").color(theme::TEXT_MUTED).small().strong());
                    parts_strip(ui, &view, 400.0);
                    if view.manifest {
                        let (d, tot) = view.parts.first().copied().unwrap_or((0, 0));
                        ui.label(egui::RichText::new(format!("{d} of {tot} segments downloaded")).color(theme::TEXT_FAINT).small());
                    } else {
                        egui::Grid::new("prog-parts").num_columns(3).spacing([12.0, 2.0]).show(ui, |ui| {
                            for (i, (done, exp)) in view.parts.iter().enumerate().take(32) {
                                let pct = if *exp > 0 { *done as f32 / *exp as f32 * 100.0 } else { 0.0 };
                                ui.label(egui::RichText::new(format!("Part {}", i + 1)).color(theme::TEXT_FAINT).small());
                                ui.label(egui::RichText::new(format!("{:.0}%", pct)).color(theme::TEXT).small());
                                ui.label(egui::RichText::new(format!("{} / {}", fmt_bytes(*done), fmt_bytes(*exp))).color(theme::TEXT_FAINT).small());
                                ui.end_row();
                            }
                        });
                        if view.parts.len() > 32 { ui.label(egui::RichText::new(format!("... and {} more parts", view.parts.len() - 32)).color(theme::TEXT_FAINT).small()); }
                    }
                }
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if t.status == TaskStatus::Downloading { if theme::ghost_btn(ui, "Pause").clicked() { toggle_pause = true; } }
                else if t.status != TaskStatus::Complete && theme::primary_btn(ui, "Resume").clicked() { toggle_pause = true; }
                if theme::ghost_btn(ui, "Open folder").clicked() { let _ = crate::platform::reveal_path(&t.dest_dir.join(&t.filename)); }
                if theme::ghost_btn(ui, "Close").clicked() { close = true; }
            });
        });
        if toggle_pause {
            if t.status == TaskStatus::Downloading { stop_task(self, id); } else { self.start_task(id); }
        }
        if close { self.show_progress_for = None; }
    }

    fn ui_queues(&mut self, ui: &mut egui::Ui, scheduler_only: bool) {
        if !scheduler_only {
            ui.heading("Queues");
            ui.add_space(4.0);
            theme::panel().show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label("New queue:");
                    ui.add(egui::TextEdit::singleline(&mut self.new_queue_name).desired_width(220.0));
                    if theme::ghost_btn(ui, "Add queue").clicked() && !self.new_queue_name.trim().is_empty() { let mut g = self.rt.lock().unwrap(); let id = crate::model::new_id(); g.state.queues.push(Queue { id, name: self.new_queue_name.clone(), max_concurrent: 1, auto_start: false, retry_count: 3, retry_interval_secs: 10, ..Default::default() }); g.save(); self.new_queue_name.clear(); }
                });
            });
            ui.add_space(6.0);
            let queues = self.rt.lock().unwrap().state.queues.clone();
            for q in queues {
                theme::panel().show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(&q.name).strong());
                        theme::chip(ui, &format!("max {}", q.max_concurrent), theme::TEXT_MUTED);
                        if q.auto_start { theme::chip(ui, "auto-start", theme::TEAL_500); }
                        theme::chip(ui, &format!("retry {}x/{}s", q.retry_count, q.retry_interval_secs), theme::TEXT_MUTED);
                        theme::chip(ui, &format!("on complete: {:?}", q.on_complete), theme::TEXT_MUTED);
                    });
                    ui.separator();
                    let tasks: Vec<DownloadTask> = { let g = self.rt.lock().unwrap(); let mut v: Vec<_> = g.state.tasks.iter().filter(|t| t.queue_id == q.id).cloned().collect(); v.sort_by_key(|t| t.order); v };
                    if tasks.is_empty() { ui.label(egui::RichText::new("No downloads in queue.").color(theme::TEXT_FAINT).small()); }
                    for t in &tasks {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(format!("#{} [{:?}]", t.order, t.priority)).color(theme::TEXT_MUTED).small().monospace());
                            ui.label(&t.filename);
                            let sc = theme::status_color(t.status);
                            ui.label(egui::RichText::new(format!("{:?}", t.status)).color(sc).small());
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if icon_btn(ui, Icon::Play, "Start", false, 58.0).clicked() { self.start_task(&t.id.clone()); }
                                if icon_btn(ui, Icon::Down, "Down", false, 62.0).clicked() { let mut g = self.rt.lock().unwrap(); move_task(&mut g.state.tasks, &t.id, 1); g.save(); }
                                if icon_btn(ui, Icon::Up, "Up", false, 48.0).clicked() { let mut g = self.rt.lock().unwrap(); move_task(&mut g.state.tasks, &t.id, -1); g.save(); }
                                for (label, pr) in [("Low", Priority::Low), ("Normal", Priority::Normal), ("High", Priority::High)] { if ui.small_button(label).clicked() { let mut g = self.rt.lock().unwrap(); if let Some(x) = g.state.tasks.iter_mut().find(|x| x.id == t.id) { x.priority = pr; } g.save(); } }
                            });
                        });
                    }
                    ui.separator();
                    ui.horizontal(|ui| {
                        let mut maxc = q.max_concurrent; let mut auto = q.auto_start; let mut retry = q.retry_count; let mut interval = q.retry_interval_secs;
                        ui.label("Max concurrent:"); ui.add(egui::DragValue::new(&mut maxc).range(1..=16));
                        ui.checkbox(&mut auto, "Auto-start");
                        ui.label("Retries:"); ui.add(egui::DragValue::new(&mut retry).range(0..=20));
                        ui.label("every (s):"); ui.add(egui::DragValue::new(&mut interval).range(1..=3600));
                        if maxc != q.max_concurrent || auto != q.auto_start || retry != q.retry_count || interval != q.retry_interval_secs { let mut g = self.rt.lock().unwrap(); if let Some(x) = g.state.queues.iter_mut().find(|x| x.id == q.id) { x.max_concurrent = maxc.max(1); x.auto_start = auto; x.retry_count = retry; x.retry_interval_secs = interval; } g.save(); }
                    });
                });
                ui.add_space(6.0);
            }
        } else {
            ui.heading("Scheduler");
            ui.add_space(4.0);
            theme::panel().show(ui, |ui| {
                ui.horizontal(|ui| {
                    if theme::primary_btn(ui, "+ Add schedule (main queue)").clicked() { let mut g = self.rt.lock().unwrap(); g.state.schedules.push(Schedule { id: crate::model::new_id(), queue_id: "main".into(), enabled: true, recurrence: Recurrence::Daily, start_secs: 3600, stop_secs: Some(7200), files_at_once: 3, ..Default::default() }); g.save(); }
                    ui.label(egui::RichText::new("Start/stop windows per queue · files at once · optional speed limit").color(theme::TEXT_FAINT).small());
                });
            });
            ui.add_space(6.0);
            let schedules = self.rt.lock().unwrap().state.schedules.clone();
            let (day, secs) = self.minutes_today;
            if schedules.is_empty() { ui.label(egui::RichText::new("No schedules.").color(theme::TEXT_FAINT)); }
            for s in schedules {
                theme::panel().show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(format!("Queue {}", s.queue_id)).strong());
                        theme::chip(ui, &format!("{:?}", s.recurrence), theme::TEXT_MUTED);
                        theme::chip(ui, &format!("start {:02}:{:02}", s.start_secs / 3600, (s.start_secs % 3600) / 60), theme::TEXT_MUTED);
                        if let Some(st) = s.stop_secs { theme::chip(ui, &format!("stop {:02}:{:02}", st / 3600, (st % 3600) / 60), theme::TEXT_MUTED); }
                        match next_run_in(&s, day, secs) { Some(n) => theme::chip(ui, &format!("next in {}", eta(n as f64)), theme::TEAL_500), None => theme::chip(ui, "no next run", theme::TEXT_FAINT) }
                    });
                    ui.separator();
                    ui.horizontal(|ui| {
                        let mut enabled = s.enabled;
                        if ui.checkbox(&mut enabled, "Enabled").changed() { let mut g = self.rt.lock().unwrap(); if let Some(x) = g.state.schedules.iter_mut().find(|x| x.id == s.id) { x.enabled = enabled; } g.save(); }
                        if icon_btn(ui, Icon::Play, "Start now", false, 92.0).clicked() { let ids: Vec<String> = self.rt.lock().unwrap().state.tasks.iter().filter(|t| t.queue_id == s.queue_id && t.status == TaskStatus::Queued).map(|t| t.id.clone()).collect(); for id in ids { self.start_task(&id); } }
                        if icon_btn(ui, Icon::Delete, "Delete", false, 76.0).clicked() { let mut g = self.rt.lock().unwrap(); g.state.schedules.retain(|x| x.id != s.id); g.save(); }
                    });
                    // IDM Scheduler: recurrence + weekdays + quota (no more than X MB every Y hours)
                    ui.horizontal(|ui| {
                        let mut rec = s.recurrence;
                        egui::ComboBox::from_id_salt(format!("sch-rec-{}", s.id)).selected_text(format!("{rec:?}")).show_ui(ui, |ui| { for r in [Recurrence::Daily, Recurrence::Weekdays, Recurrence::Weekly, Recurrence::Once] { ui.selectable_value(&mut rec, r, format!("{r:?}")); } });
                        if rec != s.recurrence { let mut g = self.rt.lock().unwrap(); if let Some(x) = g.state.schedules.iter_mut().find(|x| x.id == s.id) { x.recurrence = rec; } g.save(); }
                        for (i, d) in ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"].iter().enumerate() {
                            let bit = 1u8 << i; let mut on = s.weekdays_mask & bit != 0;
                            if ui.checkbox(&mut on, *d).changed() { let mut g = self.rt.lock().unwrap(); if let Some(x) = g.state.schedules.iter_mut().find(|x| x.id == s.id) { if on { x.weekdays_mask |= bit; } else { x.weekdays_mask &= !bit; } } g.save(); }
                        }
                    });
                    ui.horizontal(|ui| {
                        let (mut qmb, mut qh) = (s.quota_mb, s.quota_hours);
                        ui.label("Download no more than"); ui.add(egui::DragValue::new(&mut qmb).range(0..=1_000_000)); ui.label("MB every"); ui.add(egui::DragValue::new(&mut qh).range(0..=720)); ui.label("hours (0 = off)");
                        ui.label(egui::RichText::new(format!("used {} this window", fmt_bytes(s.quota_used_bytes))).color(theme::TEXT_FAINT).small());
                        if qmb != s.quota_mb || qh != s.quota_hours { let mut g = self.rt.lock().unwrap(); if let Some(x) = g.state.schedules.iter_mut().find(|x| x.id == s.id) { x.quota_mb = qmb; x.quota_hours = qh; } g.save(); }
                    });
                });
                ui.add_space(6.0);
            }
            ui.label(egui::RichText::new(format!("Clock: UTC day {day}, {secs}s into day (local-tz conversion not applied).")).color(theme::TEXT_FAINT).small());
        }
    }

    fn ui_grabber(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.horizontal(|ui| { ui.add_space(8.0); ui.heading("Site Grabber"); });
        ui.add_space(4.0);
        ui.add_space(4.0);
        theme::panel().show(ui, |ui| {
            ui.label(egui::RichText::new("Start URL").color(theme::TEXT_MUTED).small().strong());
            ui.add(egui::TextEdit::singleline(&mut self.grab_url).hint_text("https://example.com/").desired_width(ui.available_width()));
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new("Extensions").color(theme::TEXT_MUTED).small().strong());
                    ui.add(egui::TextEdit::singleline(&mut self.grab_exts).hint_text("zip,mp4,pdf (empty = all)").desired_width((ui.available_width() * 0.45).clamp(220.0, 520.0)));
                });
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new("Depth").color(theme::TEXT_MUTED).small().strong());
                    ui.add(egui::DragValue::new(&mut self.grab_depth).range(0..=10));
                });
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new("Scope").color(theme::TEXT_MUTED).small().strong());
                    egui::ComboBox::from_id_salt("grab-scope").selected_text(["Same host", "Same domain", "Any host"][self.grab_scope.min(2) as usize]).show_ui(ui, |ui| { for (i, l) in ["Same host", "Same domain", "Any host"].iter().enumerate() { ui.selectable_value(&mut self.grab_scope, i as u8, *l); } });
                });
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new("External levels").color(theme::TEXT_MUTED).small().strong());
                    ui.add(egui::DragValue::new(&mut self.grab_ext_depth).range(0..=10));
                });
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new("Parent dirs").color(theme::TEXT_MUTED).small().strong());
                    ui.checkbox(&mut self.grab_parent_dirs, "Don't explore");
                });
            });
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new("Only paths/domains (; separated)").color(theme::TEXT_MUTED).small().strong());
                    ui.add(egui::TextEdit::singleline(&mut self.grab_only_paths).hint_text("/files/;cdn.example.com").desired_width((ui.available_width() * 0.45).clamp(220.0, 520.0)));
                });
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new("Exclude paths/domains (; separated)").color(theme::TEXT_MUTED).small().strong());
                    ui.add(egui::TextEdit::singleline(&mut self.grab_exclude_paths).hint_text("/logout;/ads/").desired_width((ui.available_width() * 0.45).clamp(220.0, 520.0)));
                });
            });
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                if icon_btn_enabled(ui, Icon::Search, "Explore", true, 92.0, !self.grab_url.trim().is_empty()).clicked() || (self.grab_auto_explore && !self.grab_busy && !self.grab_url.trim().is_empty()) {
                    self.grab_auto_explore = false;
                    let proj = self.grab_project(false);
                    self.grab_project_json = serde_json::to_string_pretty(&proj).unwrap_or_default();
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
                        match res { Ok((pages, files)) => { out.message = format!("Done. {} pages explored, {} files found.", pages.len(), files.len()); out.files = files; rt.lock().unwrap().log("Grabber explore done"); } Err(e) => { out.message = format!("Explore failed: {e}"); rt.lock().unwrap().log(format!("Grabber error: {e}")); } }
                    });
                    self.grab_files = Vec::new();
                    GRAB_SLOT.with(|s| *s.borrow_mut() = Some(files_slot));
                }
                let _ = icon_btn_enabled(ui, Icon::Check, "Results update automatically", false, 188.0, false);
                if icon_btn_enabled(ui, Icon::Globe, "Mirror site", false, 104.0, !self.grab_url.trim().is_empty()).clicked() {
                    if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                        let proj = self.grab_project(true);
                        self.grab_busy = true;
                        self.grab_status = format!("Mirroring site to {}...", dir.display());
                        let slot = Arc::new(Mutex::new(GrabOutcome::default()));
                        let slot2 = slot.clone();
                        GRAB_SLOT.with(|s| *s.borrow_mut() = Some(slot));
                        let rt = self.rt.clone(); let rt2 = self.tokio_rt.clone();
                        std::thread::spawn(move || { let res = rt2.block_on(async move { crate::grabber::mirror_site(&engine::build_client(""), &proj, &dir).await }); let mut out = slot2.lock().unwrap(); out.done = true; out.apply_files = false; match res { Ok(v) => { out.message = format!("Mirror done: {} files written.", v.len()); rt.lock().unwrap().log(format!("Mirror done: {} files", v.len())); }, Err(e) => { out.message = format!("Mirror failed: {e}"); rt.lock().unwrap().log(format!("Mirror error: {e}")); } } });
                    }
                }
                ui.label(egui::RichText::new("Explore crawls pages; Enqueue adds selected files as downloads with page referrer.").color(theme::TEXT_FAINT).small());
                ui.label(egui::RichText::new(&self.grab_status).color(if self.grab_busy { theme::TEAL_500 } else { theme::TEXT_MUTED }).small());
            });
        });
        ui.add_space(6.0);
        theme::panel().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(format!("{} files found · {} selected", self.grab_files.len(), self.grab_files.iter().filter(|f| f.selected).count())).strong());
            });
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                action_group(ui, "Download", |ui| {
                    if icon_btn_enabled(ui, Icon::Download, "Enqueue selected", true, 140.0, self.grab_files.iter().any(|f| f.selected)).clicked() { for f in self.grab_files.iter().filter(|f| f.selected) { let id = self.rt.lock().unwrap().add_url(&f.url.clone(), "main", &f.page_url.clone(), ""); let _ = id; } }
                });
                action_group(ui, "Selection", |ui| {
                    if icon_btn_enabled(ui, Icon::Check, "Select all", false, 92.0, !self.grab_files.is_empty()).clicked() { for f in &mut self.grab_files { f.selected = true; } }
                    if icon_btn_enabled(ui, Icon::Delete, "Select none", false, 102.0, self.grab_files.iter().any(|f| f.selected)).clicked() { for f in &mut self.grab_files { f.selected = false; } }
                });
                action_group(ui, "Files", |ui| {
                    if icon_btn_enabled(ui, Icon::Document, "Export .txt", false, 102.0, !self.grab_files.is_empty()).clicked() { if let Some(p) = rfd::FileDialog::new().add_filter("txt", &["txt"]).save_file() { let _ = std::fs::write(p, self.grab_files.iter().map(|f| f.url.clone()).collect::<Vec<_>>().join("\n")); } }
                    if icon_btn_enabled(ui, Icon::Document, "Export CSV", false, 102.0, !self.grab_files.is_empty()).clicked() { if let Some(p) = rfd::FileDialog::new().add_filter("csv", &["csv"]).save_file() { let mut s = "url,size,mime,page\n".to_string(); for f in &self.grab_files { s.push_str(&format!("{},{},{},{}\n", f.url, f.size.map(|v| v.to_string()).unwrap_or_default(), f.mime, f.page_url)); } let _ = std::fs::write(p, s); } }
                    if icon_btn_enabled(ui, Icon::FolderOpen, "Save project", false, 112.0, !self.grab_files.is_empty()).clicked() { if let Some(p) = rfd::FileDialog::new().add_filter("json", &["json"]).save_file() { let _ = std::fs::write(p, &self.grab_project_json); } }
                    if icon_btn(ui, Icon::FolderOpen, "Load project", false, 112.0).clicked() { if let Some(p) = rfd::FileDialog::new().pick_file() { if let Ok(t) = std::fs::read_to_string(p) { self.grab_project_json = t.clone(); if let Ok(proj) = serde_json::from_str::<GrabberProject>(&t) { self.grab_url = proj.start_url; self.grab_depth = proj.depth; self.grab_ext_depth = proj.external_depth; self.grab_scope = proj.scope; self.grab_parent_dirs = proj.dont_explore_parent_dirs; self.grab_only_paths = proj.only_paths.join(";"); self.grab_exclude_paths = proj.exclude_paths.join(";"); } } } }
                });
            });
            ui.separator();
            // Captured links get remaining page height (was capped 320px).
            let list_h = ui.available_height().max(160.0);
            egui::ScrollArea::vertical().max_height(list_h).auto_shrink([false, false]).show(ui, |ui| {
                if self.grab_files.is_empty() { ui.label(egui::RichText::new(if self.grab_busy { "Working..." } else { "No files found yet." }).color(theme::TEXT_FAINT).small()); }
                egui::Grid::new("grab-files").num_columns(4).striped(false).spacing([10.0, 3.0]).show(ui, |ui| {
                    ui.label(egui::RichText::new("").strong());
                    ui.label(egui::RichText::new("URL").color(theme::TEXT_MUTED).small().strong());
                    ui.label(egui::RichText::new("Size").color(theme::TEXT_MUTED).small().strong());
                    ui.label(egui::RichText::new("Type").color(theme::TEXT_MUTED).small().strong());
                    ui.end_row();
                    for f in &mut self.grab_files {
                        ui.checkbox(&mut f.selected, "");
                        ui.add(egui::Label::new(egui::RichText::new(&f.url).color(theme::TEXT)).truncate()).on_hover_text(format!("{}\nfrom: {}", f.url, f.page_url));
                        ui.label(egui::RichText::new(f.size.map(fmt_bytes).unwrap_or_else(|| "—".into())).color(theme::TEXT_MUTED).small());
                        ui.label(egui::RichText::new(&f.mime).color(theme::TEXT_FAINT).small());
                        ui.end_row();
                    }
                });
            });
        });
        ui.add_space(6.0);
        ui.collapsing("Project JSON", |ui| { ui.add(egui::TextEdit::multiline(&mut self.grab_project_json).desired_width(f32::INFINITY).font(egui::TextStyle::Monospace)); });
    }

    fn ui_options(&mut self, ui: &mut egui::Ui) {
        ui.heading("Options");
        ui.label(egui::RichText::new(&self.options_notice).color(theme::TEAL_500).small());
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
                (7, "About", Icon::Info),
            ] {
                if icon_btn(ui, ic, name, self.options_tab == i, 104.0).clicked() { self.options_tab = i; }
            }
        });
        ui.add_space(8.0);

        let mut settings = { let g = self.rt.lock().unwrap(); g.state.settings.clone() };
        let before = serde_json::to_string(&settings).unwrap_or_default();
        let before_autostart = settings.autostart;
        let mut repair_hosts = false;
        let mut remove_hosts = false;

        theme::panel().show(ui, |ui| {
            ui.set_width(ui.available_width());
            match self.options_tab {
                0 => {
                    ui.label(egui::RichText::new("General").strong());
                    ui.separator();
                    ui.checkbox(&mut settings.start_minimized, "Start minimized");
                    ui.checkbox(&mut settings.close_to_tray, "Close to tray");
                    ui.checkbox(&mut settings.clipboard_monitor, "Start downloads for URLs copied to clipboard");
                    ui.checkbox(&mut settings.autostart, "Launch on OS startup");
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new("Download directory").color(theme::TEXT_MUTED).small().strong());
                    ui.horizontal_wrapped(|ui| {
                        ui.add(egui::Label::new(egui::RichText::new(settings.download_dir.display().to_string()).monospace().small()).truncate()).on_hover_text(settings.download_dir.display().to_string());
                        if icon_btn(ui, Icon::FolderOpen, "Browse", false, 96.0).clicked() { if let Some(path) = rfd::FileDialog::new().pick_folder() { settings.download_dir = path; } }
                    });
                    ui.label(egui::RichText::new(format!("Config directory: {}", platform::config_dir().display())).color(theme::TEXT_FAINT).small());
                }
                1 => {
                    ui.label(egui::RichText::new("File types").strong());
                    ui.separator();
                    for (cat, exts) in [("Compressed", "zip rar 7z tar gz bz2 xz"), ("Documents", "pdf doc docx xls xlsx ppt pptx txt epub"), ("Music", "mp3 flac ogg wav m4a aac"), ("Programs", "exe msi dmg pkg deb rpm appimage"), ("Video", "mp4 mkv avi mov webm ts m3u8 mpd")] {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(egui::RichText::new(cat).strong());
                            ui.label(egui::RichText::new(exts).color(theme::TEXT_MUTED).monospace().small());
                        });
                    }
                    ui.label(egui::RichText::new("Category comes from the file name when a download is added. The sidebar filters by category.").color(theme::TEXT_FAINT).small());
                }
                2 => {
                    ui.label(egui::RichText::new("Save to").strong());
                    ui.separator();
                    ui.label(egui::RichText::new("Default download directory").color(theme::TEXT_MUTED).small().strong());
                    ui.horizontal_wrapped(|ui| {
                        ui.add(egui::Label::new(egui::RichText::new(settings.download_dir.display().to_string()).monospace().small()).truncate()).on_hover_text(settings.download_dir.display().to_string());
                        if icon_btn(ui, Icon::FolderOpen, "Browse", false, 96.0).clicked() { if let Some(path) = rfd::FileDialog::new().pick_folder() { settings.download_dir = path; } }
                    });
                    ui.label(egui::RichText::new("Each download stores its destination when it is added.").color(theme::TEXT_FAINT).small());
                }
                3 => {
                    ui.label(egui::RichText::new("Downloads").strong());
                    ui.separator();
                    ui.add(egui::Slider::new(&mut settings.max_connections, 1..=32).text("Connections per file"));
                    ui.add(egui::Slider::new(&mut settings.global_max_concurrent, 1..=16).text("Simultaneous downloads"));
                    ui.add(egui::Slider::new(&mut settings.speed_limit_kbps, 0..=100_000).text("Speed limit KB/s (0 = unlimited)"));
                    ui.label(egui::RichText::new("Double-click a download to open its progress window.").color(theme::TEXT_FAINT).small());
                }
                4 => {
                    ui.label(egui::RichText::new("Connection").strong());
                    ui.separator();
                    ui.add(egui::Slider::new(&mut settings.max_connections, 1..=32).text("Connections per file"));
                    ui.label(egui::RichText::new("Connect timeout 15 seconds. Read timeout 60 seconds. A failed segment retries with backoff.").color(theme::TEXT_FAINT).small());
                }
                5 => {
                    ui.label(egui::RichText::new("Proxy / SOCKS").strong());
                    ui.separator();
                    ui.label(egui::RichText::new("Proxy").color(theme::TEXT_MUTED).small().strong());
                    ui.add(egui::TextEdit::singleline(&mut settings.proxy_url).hint_text("http://user:pass@host:port or socks5://host:port").desired_width(ui.available_width()));
                    ui.label(egui::RichText::new("Leave empty for a direct connection. New downloads use the saved proxy.").color(theme::TEXT_FAINT).small());
                }
                6 => {
                    ui.label(egui::RichText::new("Browser integration").strong());
                    ui.separator();
                    ui.horizontal_wrapped(|ui| {
                        action_group(ui, "Native host", |ui| {
                            if icon_btn(ui, Icon::Check, "Repair integration", false, 148.0).clicked() { repair_hosts = true; }
                            if icon_btn(ui, Icon::Delete, "Remove integration", false, 156.0).clicked() { remove_hosts = true; }
                        });
                    });
                    ui.label(egui::RichText::new(format!("Chromium extension ID: {}", downloadcraft_lib::platform::CHROME_EXT_ID)).color(theme::TEXT_FAINT).small().monospace());
                    ui.label(egui::RichText::new("Load extensions/chromium in Chromium browsers or extensions/firefox in Firefox. Native host: downloadcraft-host.").color(theme::TEXT_FAINT).small());
                }
                _ => {
                    ui.label(egui::RichText::new("About").strong());
                    ui.separator();
                    ui.label("downloadcraft 0.2.0");
                    ui.label("Cross-platform download manager written in Rust with egui.");
                    ui.label(egui::RichText::new("No proprietary code, binaries, icons, or licensing logic.").color(theme::TEXT_FAINT).small());
                }
            }
        });

        if repair_hosts {
            let mut g = self.rt.lock().unwrap();
            match platform::register_native_hosts(&crate::sibling_host_path()) {
                Ok(paths) => { self.options_notice = format!("Browser integration repaired. Registered {} host manifests.", paths.len()); g.log(self.options_notice.clone()); }
                Err(e) => { self.options_notice = format!("Browser integration failed: {e}"); g.log(self.options_notice.clone()); }
            }
            let _ = platform::register_protocol();
        }
        if remove_hosts {
            let _ = platform::unregister_native_hosts();
            let _ = platform::set_autostart(false);
            settings.autostart = false;
            self.options_notice = "Browser integration and autostart removed.".into();
            self.rt.lock().unwrap().log(self.options_notice.clone());
        }

        // Apply immediately. Options previously looked inert because edits only
        // persisted from a Save button that could sit below the visible page.
        if serde_json::to_string(&settings).unwrap_or_default() != before {
            if settings.autostart != before_autostart { let _ = platform::set_autostart(settings.autostart); }
            let speed_limit = settings.speed_limit_kbps;
            let limits: Vec<_> = {
                let mut g = self.rt.lock().unwrap();
                g.state.settings = settings;
                g.save();
                g.controls.values().cloned().collect::<Vec<_>>()
            };
            for c in limits { c.limit_kbps.store(speed_limit, Ordering::Relaxed); }
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
        if swap_with != p { tasks[sorted[p]].order = swap_with as i64; tasks[sorted[swap_with]].order = p as i64; }
    }
}

pub fn fmt_bytes(n: u64) -> String {
    const U: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64; let mut i = 0; while v >= 1024.0 && i < U.len() - 1 { v /= 1024.0; i += 1; } format!("{v:.1} {}", U[i])
}


#[cfg(test)]
mod progress_tests {
    use super::*;

    fn task(total: Option<u64>, downloaded: u64) -> DownloadTask {
        DownloadTask { total, downloaded, ..Default::default() }
    }

    #[test]
    fn byte_total_wins_and_single_part_is_not_manifest() {
        let view = SegView { parts: vec![(25, 100)], manifest: false };
        assert_eq!(display_fraction(&task(Some(200), 50), Some(&view)), Some(0.25));
        assert_eq!(view.fraction(), None);
    }

    #[test]
    fn manifest_segment_count_supplies_fraction_when_bytes_unknown() {
        let view = SegView { parts: vec![(3, 12)], manifest: true };
        assert_eq!(display_fraction(&task(None, 999), Some(&view)), Some(0.25));
    }

    #[test]
    fn unknown_total_without_manifest_count_stays_indeterminate() {
        let view = SegView { parts: vec![(10, 0)], manifest: false };
        assert_eq!(display_fraction(&task(None, 10), Some(&view)), None);
    }
}
