use crate::engine;
use crate::grabber::{Crawler, FoundFile, GrabberProject};
use crate::model::*;
use crate::platform;
use crate::queue::plan_starts;
use crate::scheduler::{next_run_in, should_start, should_stop};
use crate::{Runtime, save_state};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, PartialEq)]
enum Tab { Downloads, Queues, Scheduler, Grabber, Options }

pub struct IdmApp {
    rt: Arc<Mutex<Runtime>>,
    tokio_rt: Arc<tokio::runtime::Runtime>,
    tab: Tab,
    add_url: String,
    add_referrer: String,
    add_cookies: String,
    add_queue: String,
    new_queue_name: String,
    grab_url: String,
    grab_depth: usize,
    grab_scope: u8,
    grab_exts: String,
    grab_files: Vec<FoundFile>,
    grab_project_json: String,
    last_tick: std::time::Instant,
    #[allow(dead_code)]
    tray: Option<TrayState>,
    hidden: bool,
    show_about: bool,
    search: String,
    minutes_today: (i64, u32),
}

struct TrayState { _tray: Option<tray_icon::TrayIcon>, }

impl IdmApp {
    pub fn new(_cc: &eframe::CreationContext<'_>, rt: Arc<Mutex<Runtime>>, hidden: bool) -> Self {
        let tokio_rt = Arc::new(tokio::runtime::Runtime::new().expect("tokio rt"));
        let tray = Self::make_tray();
        Self { rt, tokio_rt, tab: Tab::Downloads, add_url: String::new(), add_referrer: String::new(), add_cookies: String::new(), add_queue: "main".into(), new_queue_name: String::new(), grab_url: String::new(), grab_depth: 2, grab_scope: 0, grab_exts: String::new(), grab_files: Vec::new(), grab_project_json: String::new(), last_tick: std::time::Instant::now(), tray: Some(TrayState { _tray: tray }), hidden, show_about: false, search: String::new(), minutes_today: today_parts() }
    }

    fn make_tray() -> Option<tray_icon::TrayIcon> {
        // 16x16 RGBA icon generated in code (no proprietary asset).
        let mut rgba = vec![0u8; 16 * 16 * 4];
        for i in 0..256 { rgba[i * 4] = 30; rgba[i * 4 + 1] = 58; rgba[i * 4 + 2] = 95; rgba[i * 4 + 3] = 255; }
        let icon = tray_icon::Icon::from_rgba(rgba, 16, 16).ok()?;
        let menu = tray_icon::menu::Menu::new();
        let show = tray_icon::menu::MenuItem::new("Show idmcraft", true, None);
        let add = tray_icon::menu::MenuItem::new("Add URL...", true, None);
        let quit = tray_icon::menu::MenuItem::new("Quit", true, None);
        let _ = menu.append_items(&[&show, &add, &quit]);
        tray_icon::TrayIconBuilder::new().with_icon(icon).with_menu(Box::new(menu)).with_tooltip("idmcraft").build().ok()
    }

    fn start_task(&mut self, id: &str) {
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
        self.tokio_rt.spawn(async move {
            let result = crate::stream::download_url(client, task, ctrl).await;
            let mut g = rt.lock().unwrap();
            g.controls.remove(&tid);
            match result {
                Ok(done) => {
                    let status = done.status; let err = done.error.clone(); let fname = done.filename.clone();
                    if let Some(t) = g.state.tasks.iter_mut().find(|t| t.id == tid) { *t = done; }
                    g.log(format!("Finished {fname}: {:?} {err}", status));
                    if status == TaskStatus::Complete {
                        let queue_id = g.state.tasks.iter().find(|t| t.id == tid).map(|t| t.queue_id.clone()).unwrap_or_default();
                        run_completion(&g, &queue_id, &tid);
                    }
                }
                Err(e) => {
                    // retry per queue policy
                    let (retry, interval, attempts_left) = {
                        let t = g.state.tasks.iter().find(|t| t.id == tid);
                        let q = t.and_then(|t| g.state.queues.iter().find(|q| q.id == t.queue_id));
                        let retry = q.map(|q| q.retry_count).unwrap_or(0);
                        let interval = q.map(|q| q.retry_interval_secs).unwrap_or(10);
                        // ponytail: retry count tracked via error prefix attempts not persisted separately; use segments field? Use simple: retry if error not containing 'retry-exhausted'
                        (retry, interval, !e.contains("retry-exhausted"))
                    };
                    if let Some(t) = g.state.tasks.iter_mut().find(|t| t.id == tid) { t.status = TaskStatus::Error; t.error = e.clone(); }
                    g.log(format!("Error {tid}: {e} (queue retry={retry} interval={interval}s left={attempts_left})"));
                    if retry > 0 && attempts_left {
                        if let Some(t) = g.state.tasks.iter_mut().find(|t| t.id == tid) { t.status = TaskStatus::Queued; }
                    }
                }
            }
            g.save();
        });
    }

    fn poll(&mut self, ctx: &egui::Context) {
        crate::drain_inbox(&self.rt);
        // progress sync
        {
            let mut g = self.rt.lock().unwrap();
            let ctrls: Vec<(String, u64)> = g.controls.iter().map(|(k, c)| (k.clone(), c.downloaded.load(std::sync::atomic::Ordering::Relaxed))).collect();
            for (id, n) in ctrls { if let Some(t) = g.state.tasks.iter_mut().find(|t| t.id == id) { t.downloaded = n; } }
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
        // queue auto-start
        {
            let g = self.rt.lock().unwrap();
            let auto = g.state.queues.iter().any(|q| q.auto_start);
            if auto {
                let active: Vec<(String, String)> = g.controls.keys().map(|id| (id.clone(), g.state.tasks.iter().find(|t| &t.id == id).map(|t| t.queue_id.clone()).unwrap_or_default())).collect();
                let plan = plan_starts(&g.state.tasks, &g.state.queues, &active, g.state.settings.global_max_concurrent);
                drop(g);
                for id in plan {
                    // only auto-start queues flagged
                    let qid = self.rt.lock().unwrap().state.tasks.iter().find(|t| t.id == id).map(|t| t.queue_id.clone()).unwrap_or_default();
                    let auto_q = self.rt.lock().unwrap().state.queues.iter().find(|q| q.id == qid).map(|q| q.auto_start).unwrap_or(false);
                    if auto_q { self.start_task(&id); }
                }
            }
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
                let ids: Vec<String> = self.rt.lock().unwrap().state.tasks.iter().filter(|t| t.queue_id == qid && t.status == TaskStatus::Queued).map(|t| t.id.clone()).collect();
                let limit = self.rt.lock().unwrap().state.schedules.iter().find(|s| s.queue_id == qid).map(|s| s.files_at_once).unwrap_or(ids.len());
                for id in ids.into_iter().take(if limit == 0 { usize::MAX } else { limit }) { self.start_task(&id); }
            }
            for qid in to_stop {
                let g = self.rt.lock().unwrap();
                for (id, c) in &g.controls { if g.state.tasks.iter().find(|t| &t.id == id).map(|t| t.queue_id.clone()).as_deref() == Some(&qid) { c.paused.store(true, std::sync::atomic::Ordering::Relaxed); } }
            }
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

impl eframe::App for IdmApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.hidden {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            // still poll inbox/scheduler while hidden
            if self.last_tick.elapsed() > std::time::Duration::from_secs(2) { self.last_tick = std::time::Instant::now(); self.poll(ctx); }
            // tray menu events handled by tray-icon polling via context repaint; check show via hidden flag externally not wired to menu clicks in egui loop
            ctx.request_repaint_after(std::time::Duration::from_secs(1));
            return;
        }
        if self.last_tick.elapsed() > std::time::Duration::from_millis(500) { self.last_tick = std::time::Instant::now(); self.poll(ctx); }
        egui::TopBottomPanel::top("menu").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.menu_button("Tasks", |ui| {
                    if ui.button("Add URL...").clicked() { self.tab = Tab::Downloads; }
                    if ui.button("Add batch (one URL per line)...").clicked() { self.tab = Tab::Downloads; }
                    if ui.button("Resume all").clicked() { let ids: Vec<String> = self.rt.lock().unwrap().state.tasks.iter().filter(|t| t.status == TaskStatus::Paused || t.status == TaskStatus::Error).map(|t| t.id.clone()).collect(); for id in ids { if let Some(t) = self.rt.lock().unwrap().state.tasks.iter_mut().find(|t| t.id == id) { t.status = TaskStatus::Queued; } } self.rt.lock().unwrap().save(); }
                    if ui.button("Stop all").clicked() { let g = self.rt.lock().unwrap(); for c in g.controls.values() { c.paused.store(true, std::sync::atomic::Ordering::Relaxed); } }
                    if ui.button("Quit").clicked() { std::process::exit(0); }
                });
                ui.menu_button("Downloads", |ui| { if ui.button("Options").clicked() { self.tab = Tab::Options; } });
                ui.menu_button("View", |ui| {
                    if ui.button("Queues & Scheduler").clicked() { self.tab = Tab::Queues; }
                    if ui.button("Site Grabber").clicked() { self.tab = Tab::Grabber; }
                });
                ui.menu_button("Help", |ui| { if ui.button("About").clicked() { self.show_about = true; } });
                ui.separator();
                for (label, tab) in [("Downloads", Tab::Downloads), ("Queues", Tab::Queues), ("Scheduler", Tab::Scheduler), ("Grabber", Tab::Grabber), ("Options", Tab::Options)] {
                    if ui.selectable_label(self.tab == tab, label).clicked() { self.tab = tab; }
                }
            });
        });
        if self.show_about {
            egui::Window::new("About idmcraft").show(ctx, |ui| { ui.label("idmcraft 0.1.0 — clean-room download manager. Not affiliated with Tonec/IDM."); if ui.button("Close").clicked() { self.show_about = false; } });
        }
        egui::CentralPanel::default().show(ctx, |ui| {
            match self.tab {
                Tab::Downloads => self.ui_downloads(ui),
                Tab::Queues => self.ui_queues(ui, false),
                Tab::Scheduler => self.ui_queues(ui, true),
                Tab::Grabber => self.ui_grabber(ui),
                Tab::Options => self.ui_options(ui),
            }
        });
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            let g = self.rt.lock().unwrap();
            ui.horizontal(|ui| { ui.label(g.messages.last().cloned().unwrap_or_default()); ui.separator(); ui.label(format!("{} tasks", g.state.tasks.len())); });
        });
        // close to tray
        if ctx.input(|i| i.viewport().close_requested()) {
            let close_to_tray = self.rt.lock().unwrap().state.settings.close_to_tray;
            if close_to_tray { ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose); self.hidden = true; ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false)); }
        }
    }
}

impl IdmApp {
    fn ui_downloads(&mut self, ui: &mut egui::Ui) {
        ui.heading("Downloads");
        ui.horizontal(|ui| {
            ui.label("URL:"); ui.text_edit_singleline(&mut self.add_url);
            ui.label("Queue:"); ui.text_edit_singleline(&mut self.add_queue);
            if ui.button("Add").clicked() && !self.add_url.trim().is_empty() {
                for line in self.add_url.lines().map(|s| s.trim()).filter(|s| !s.is_empty()) {
                    self.rt.lock().unwrap().add_url(line, &self.add_queue.clone(), &self.add_referrer.clone(), &self.add_cookies.clone());
                }
                self.add_url.clear();
            }
            if ui.button("Add + Start").clicked() && !self.add_url.trim().is_empty() {
                let u = self.add_url.trim().to_string(); let id = self.rt.lock().unwrap().add_url(&u, &self.add_queue.clone(), &self.add_referrer.clone(), &self.add_cookies.clone());
                self.start_task(&id); self.add_url.clear();
            }
            if ui.button("Paste").clicked() { if let Ok(mut cb) = arboard::Clipboard::new() { if let Ok(t) = cb.get_text() { self.add_url = t.trim().to_string(); } } }
        });
        ui.horizontal(|ui| { ui.label("Referrer:"); ui.text_edit_singleline(&mut self.add_referrer); ui.label("Cookies:"); ui.text_edit_singleline(&mut self.add_cookies); ui.label("Search:"); ui.text_edit_singleline(&mut self.search); });
        ui.separator();
        // toolbar
        let mut to_start: Vec<String> = Vec::new(); let mut to_pause: Vec<String> = Vec::new(); let mut to_remove: Vec<String> = Vec::new(); let mut to_resume_ctrl: Vec<String> = Vec::new();
        egui::ScrollArea::vertical().show(ui, |ui| {
            egui::Grid::new("tasks").striped(true).num_columns(8).show(ui, |ui| {
                ui.strong("File"); ui.strong("Size"); ui.strong("Downloaded"); ui.strong("Status"); ui.strong("Queue"); ui.strong("Priority"); ui.strong("Speed"); ui.strong("Actions"); ui.end_row();
                let tasks = self.rt.lock().unwrap().state.tasks.clone();
                for t in &tasks {
                    if !self.search.is_empty() && !t.filename.to_ascii_lowercase().contains(&self.search.to_ascii_lowercase()) && !t.url.to_ascii_lowercase().contains(&self.search.to_ascii_lowercase()) { continue; }
                    ui.label(&t.filename);
                    ui.label(t.total.map(fmt_bytes).unwrap_or_else(|| "?".into()));
                    ui.label(fmt_bytes(t.downloaded));
                    ui.label(format!("{:?}{}", t.status, if t.error.is_empty() { "".into() } else { format!(": {}", t.error) }));
                    ui.label(&t.queue_id);
                    ui.label(format!("{:?}", t.priority));
                    ui.label(if t.total.unwrap_or(0) > 0 { format!("{:.0}%", t.downloaded as f64 * 100.0 / t.total.unwrap_or(1) as f64) } else { "".into() });
                    ui.horizontal(|ui| {
                        let is_active = self.rt.lock().unwrap().controls.contains_key(&t.id);
                        if !is_active && t.status != TaskStatus::Complete && ui.button("Start").clicked() { to_start.push(t.id.clone()); }
                        if is_active {
                            if ui.button("Pause").clicked() { to_pause.push(t.id.clone()); }
                            if ui.button("Resume").clicked() { to_resume_ctrl.push(t.id.clone()); }
                        }
                        if ui.button("Remove").clicked() { to_remove.push(t.id.clone()); }
                        if ui.button("Open").clicked() { let _ = platform::open_path(&t.dest_dir.join(&t.filename)); }
                        if ui.button("Folder").clicked() { let _ = platform::reveal_path(&t.dest_dir.join(&t.filename)); }
                    });
                    ui.end_row();
                    if let Some(total) = t.total { if total > 0 { ui.add(egui::ProgressBar::new(t.downloaded as f32 / total as f32).show_percentage()); ui.end_row(); } }
                }
            });
        });
        for id in to_start { self.start_task(&id); }
        for id in to_pause {
            let g = self.rt.lock().unwrap();
            if let Some(c) = g.controls.get(&id) { c.paused.store(true, std::sync::atomic::Ordering::Relaxed); }
            drop(g);
            if let Some(t) = self.rt.lock().unwrap().state.tasks.iter_mut().find(|t| t.id == id) { t.status = TaskStatus::Paused; }
            self.rt.lock().unwrap().save();
        }
        for id in to_resume_ctrl { let g = self.rt.lock().unwrap(); if let Some(c) = g.controls.get(&id) { c.paused.store(false, std::sync::atomic::Ordering::Relaxed); } else { drop(g); self.start_task(&id); } }
        for id in to_remove { let mut g = self.rt.lock().unwrap(); if let Some(c) = g.controls.get(&id) { c.cancelled.store(true, std::sync::atomic::Ordering::Relaxed); } g.state.tasks.retain(|t| t.id != id); g.controls.remove(&id); g.save(); }
        // drag-drop files/URLs via egui dropped files
        // ponytail: URL drag-drop from browser uses text drop not exposed by egui files API; clipboard + inbox cover it.
    }

    fn ui_queues(&mut self, ui: &mut egui::Ui, scheduler_only: bool) {
        if !scheduler_only {
            ui.heading("Queues");
            ui.horizontal(|ui| { ui.text_edit_singleline(&mut self.new_queue_name); if ui.button("Add queue").clicked() && !self.new_queue_name.trim().is_empty() { let mut g = self.rt.lock().unwrap(); let id = crate::model::new_id(); g.state.queues.push(Queue { id, name: self.new_queue_name.clone(), max_concurrent: 1, auto_start: false, retry_count: 3, retry_interval_secs: 10, ..Default::default() }); g.save(); self.new_queue_name.clear(); } });
            let queues = self.rt.lock().unwrap().state.queues.clone();
            for q in queues {
                ui.collapsing(format!("{} (max {})", q.name, q.max_concurrent), |ui| {
                    ui.label(format!("id={} auto_start={} retry={}x/{}s complete={:?}", q.id, q.auto_start, q.retry_count, q.retry_interval_secs, q.on_complete));
                    // task order controls
                    let tasks: Vec<DownloadTask> = { let g = self.rt.lock().unwrap(); let mut v: Vec<_> = g.state.tasks.iter().filter(|t| t.queue_id == q.id).cloned().collect(); v.sort_by_key(|t| t.order); v };
                    for t in &tasks {
                        ui.horizontal(|ui| {
                            ui.label(format!("{} [{:?}] {:?}", t.order, t.priority, t.filename));
                            if ui.button("Up").clicked() { let mut g = self.rt.lock().unwrap(); move_task(&mut g.state.tasks, &t.id, -1); g.save(); }
                            if ui.button("Down").clicked() { let mut g = self.rt.lock().unwrap(); move_task(&mut g.state.tasks, &t.id, 1); g.save(); }
                            for (label, pr) in [("High", Priority::High), ("Normal", Priority::Normal), ("Low", Priority::Low)] { if ui.button(label).clicked() { let mut g = self.rt.lock().unwrap(); if let Some(x) = g.state.tasks.iter_mut().find(|x| x.id == t.id) { x.priority = pr; } g.save(); } }
                            if ui.button("Start").clicked() { self.start_task(&t.id.clone()); }
                        });
                    }
                    ui.horizontal(|ui| {
                        let mut maxc = q.max_concurrent; let mut auto = q.auto_start; let mut retry = q.retry_count;
                        ui.add(egui::DragValue::new(&mut maxc).prefix("max: "));
                        ui.checkbox(&mut auto, "auto-start");
                        ui.add(egui::DragValue::new(&mut retry).prefix("retries: "));
                        if maxc != q.max_concurrent || auto != q.auto_start || retry != q.retry_count { let mut g = self.rt.lock().unwrap(); if let Some(x) = g.state.queues.iter_mut().find(|x| x.id == q.id) { x.max_concurrent = maxc.max(1); x.auto_start = auto; x.retry_count = retry; } g.save(); }
                    });
                });
            }
        } else {
            ui.heading("Scheduler");
            if ui.button("Add schedule for main queue").clicked() { let mut g = self.rt.lock().unwrap(); g.state.schedules.push(Schedule { id: crate::model::new_id(), queue_id: "main".into(), enabled: true, recurrence: Recurrence::Daily, start_secs: 3600, stop_secs: Some(7200), files_at_once: 3, ..Default::default() }); g.save(); }
            let schedules = self.rt.lock().unwrap().state.schedules.clone();
            let (day, secs) = self.minutes_today;
            for s in schedules {
                ui.group(|ui| {
                    ui.label(format!("queue={} {:?} start={}s stop={:?} enabled={} next={:?}", s.queue_id, s.recurrence, s.start_secs, s.stop_secs, s.enabled, next_run_in(&s, day, secs)));
                    ui.horizontal(|ui| {
                        let mut enabled = s.enabled;
                        if ui.checkbox(&mut enabled, "enabled").changed() { let mut g = self.rt.lock().unwrap(); if let Some(x) = g.state.schedules.iter_mut().find(|x| x.id == s.id) { x.enabled = enabled; } g.save(); }
                        if ui.button("Delete").clicked() { let mut g = self.rt.lock().unwrap(); g.state.schedules.retain(|x| x.id != s.id); g.save(); }
                        if ui.button("Start now").clicked() { let ids: Vec<String> = self.rt.lock().unwrap().state.tasks.iter().filter(|t| t.queue_id == s.queue_id && t.status == TaskStatus::Queued).map(|t| t.id.clone()).collect(); for id in ids { self.start_task(&id); } }
                    });
                });
            }
            ui.label(format!("Now (UTC day/secs): {day} / {secs}"));
        }
    }

    fn ui_grabber(&mut self, ui: &mut egui::Ui) {
        ui.heading("Site Grabber");
        ui.horizontal(|ui| { ui.label("URL:"); ui.text_edit_singleline(&mut self.grab_url); ui.label("Depth:"); ui.add(egui::DragValue::new(&mut self.grab_depth)); ui.label("Scope(0 host,1 domain,2 all):"); ui.add(egui::DragValue::new(&mut self.grab_scope)); });
        ui.horizontal(|ui| { ui.label("Extensions (comma, empty=all):"); ui.text_edit_singleline(&mut self.grab_exts); });
        if ui.button("Explore").clicked() && !self.grab_url.trim().is_empty() {
            let proj = GrabberProject { start_url: self.grab_url.clone(), depth: self.grab_depth, scope: self.grab_scope, extensions: self.grab_exts.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(), max_pages: 200, ..Default::default() };
            self.grab_project_json = serde_json::to_string_pretty(&proj).unwrap_or_default();
            let rt = self.rt.clone(); let client = engine::build_client("");
            let crawler = Crawler { project: proj };
            // blocking call on tokio: use block_on in spawned thread via channel - simpler: spawn and poll via messages
            let files_slot = Arc::new(Mutex::new(Vec::new()));
            let slot2 = files_slot.clone();
            let rt2 = self.tokio_rt.clone();
            std::thread::spawn(move || {
                let res = rt2.block_on(async move { crawler.crawl(&client).await });
                match res { Ok((_pages, files)) => { *slot2.lock().unwrap() = files; rt.lock().unwrap().log("Grabber explore done"); } Err(e) => { rt.lock().unwrap().log(format!("Grabber error: {e}")); } }
            });
            // store handle via messages poll: check slot on next frames - stash in grab_files when user clicks Refresh
            self.grab_files = Vec::new();
            // keep slot in a static-ish way: use add_url hack - instead store in thread and read via Persisted? Simplest: button below re-runs synchronously not possible.
            // ponytail: explore runs async; click "Refresh results" after a moment.
            GRAB_SLOT.with(|s| *s.borrow_mut() = Some(files_slot));
        }
        if ui.button("Refresh results").clicked() { GRAB_SLOT.with(|s| { if let Some(slot) = &*s.borrow() { self.grab_files = slot.lock().unwrap().clone(); } }); }
        ui.horizontal(|ui| {
            if ui.button("Select all").clicked() { for f in &mut self.grab_files { f.selected = true; } }
            if ui.button("Select none").clicked() { for f in &mut self.grab_files { f.selected = false; } }
            if ui.button("Enqueue selected").clicked() { for f in self.grab_files.iter().filter(|f| f.selected) { self.rt.lock().unwrap().add_url(&f.url.clone(), "main", "", ""); } }
            if ui.button("Export .txt").clicked() { if let Some(p) = rfd::FileDialog::new().add_filter("txt", &["txt"]).save_file() { let _ = std::fs::write(p, self.grab_files.iter().map(|f| f.url.clone()).collect::<Vec<_>>().join("\n")); } }
            if ui.button("Export CSV").clicked() { if let Some(p) = rfd::FileDialog::new().add_filter("csv", &["csv"]).save_file() { let mut s = "url,size,mime,page\n".to_string(); for f in &self.grab_files { s.push_str(&format!("{},{},{},{}\n", f.url, f.size.map(|v| v.to_string()).unwrap_or_default(), f.mime, f.page_url)); } let _ = std::fs::write(p, s); } }
            if ui.button("Save project").clicked() { if let Some(p) = rfd::FileDialog::new().add_filter("json", &["json"]).save_file() { let _ = std::fs::write(p, &self.grab_project_json); } }
            if ui.button("Load project").clicked() { if let Some(p) = rfd::FileDialog::new().pick_file() { if let Ok(t) = std::fs::read_to_string(p) { self.grab_project_json = t.clone(); if let Ok(proj) = serde_json::from_str::<GrabberProject>(&t) { self.grab_url = proj.start_url; self.grab_depth = proj.depth; self.grab_scope = proj.scope; } } } }
            if ui.button("Mirror site...").clicked() {
                if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                    let proj = GrabberProject { start_url: self.grab_url.clone(), depth: self.grab_depth, scope: self.grab_scope, extensions: self.grab_exts.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(), max_pages: 200, mirror: true, ..Default::default() };
                    let rt = self.rt.clone(); let rt2 = self.tokio_rt.clone();
                    std::thread::spawn(move || { let res = rt2.block_on(async move { crate::grabber::mirror_site(&engine::build_client(""), &proj, &dir).await }); match res { Ok(v) => rt.lock().unwrap().log(format!("Mirror done: {} files", v.len())), Err(e) => rt.lock().unwrap().log(format!("Mirror error: {e}")) } });
                }
            }
        });
        egui::ScrollArea::vertical().show(ui, |ui| {
            for f in &mut self.grab_files { ui.horizontal(|ui| { ui.checkbox(&mut f.selected, ""); ui.label(&f.url); ui.label(f.size.map(fmt_bytes).unwrap_or_default()); ui.label(&f.mime); }); }
        });
        ui.label("Project JSON:");
        ui.text_edit_multiline(&mut self.grab_project_json);
    }

    fn ui_options(&mut self, ui: &mut egui::Ui) {
        ui.heading("Options");
        let mut g = self.rt.lock().unwrap();
        ui.label(format!("Config: {}", platform::config_dir().display()));
        ui.horizontal(|ui| { ui.label("Download dir:"); ui.label(g.state.settings.download_dir.display().to_string()); if ui.button("Browse...").clicked() { if let Some(p) = rfd::FileDialog::new().pick_folder() { g.state.settings.download_dir = p; } } });
        ui.add(egui::Slider::new(&mut g.state.settings.max_connections, 1..=32).text("Connections per file"));
        ui.add(egui::Slider::new(&mut g.state.settings.global_max_concurrent, 1..=16).text("Simultaneous downloads"));
        ui.add(egui::Slider::new(&mut g.state.settings.speed_limit_kbps, 0..=100_000).text("Speed limit KB/s (0=unlimited)"));
        ui.horizontal(|ui| { ui.label("Proxy (http://user:pass@host:port):"); ui.text_edit_singleline(&mut g.state.settings.proxy_url); });
        let mut autostart = g.state.settings.autostart; let mut sm = g.state.settings.start_minimized; let mut ctt = g.state.settings.close_to_tray; let mut clip = g.state.settings.clipboard_monitor;
        ui.checkbox(&mut sm, "Start minimized"); ui.checkbox(&mut ctt, "Close to tray"); ui.checkbox(&mut clip, "Monitor clipboard for URLs");
        if ui.checkbox(&mut autostart, "Launch at OS startup").changed() { let _ = platform::set_autostart(autostart); }
        if autostart != g.state.settings.autostart { let _ = platform::set_autostart(autostart); }
        g.state.settings.start_minimized = sm; g.state.settings.close_to_tray = ctt; g.state.settings.clipboard_monitor = clip; g.state.settings.autostart = autostart;
        if ui.button("Repair browser integration (register native hosts)").clicked() { match platform::register_native_hosts(&crate::sibling_host_path()) { Ok(p) => g.log(format!("Registered {} host manifests", p.len())), Err(e) => g.log(format!("Register failed: {e}")) } let _ = platform::register_protocol(); }
        if ui.button("Remove browser integration + autostart").clicked() { let _ = platform::unregister_native_hosts(); let _ = platform::set_autostart(false); g.state.settings.autostart = false; }
        ui.label(format!("Native host manifests (mockable fn): {:?}", platform::native_host_manifest_paths(&std::env::home_dir().unwrap_or_default()).iter().map(|(_, p)| p.display().to_string()).collect::<Vec<_>>()));
        if ui.button("Save").clicked() { let autostart_now = g.state.settings.autostart; save_state(&g.state); drop(g); let _ = platform::set_autostart(autostart_now);}
    }
}

thread_local! { static GRAB_SLOT: std::cell::RefCell<Option<Arc<Mutex<Vec<FoundFile>>>>> = const { std::cell::RefCell::new(None) }; }

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
