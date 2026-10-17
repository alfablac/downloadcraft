use crate::engine;
use crate::grabber::{Crawler, FoundFile, GrabberProject};
use crate::model::*;
use crate::platform;
use crate::queue::plan_starts;
use crate::scheduler::{next_run_in, should_start, should_stop};
use crate::theme;
use crate::{Runtime, save_state};
use std::sync::atomic::Ordering;
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
    category: String,
    sort_col: u8,
    sort_asc: bool,
    options_tab: u8,
    show_add: bool,
    show_progress_for: Option<String>,
    selected: Option<String>,
    completed_notice: Option<String>,
    last_notified: Option<String>,
    speed_hist: Vec<f32>,
    last_dl_total: u64,
}

struct TrayState { _tray: Option<tray_icon::TrayIcon>, }

impl IdmApp {
    pub fn new(_cc: &eframe::CreationContext<'_>, rt: Arc<Mutex<Runtime>>, hidden: bool) -> Self {
        let tokio_rt = Arc::new(tokio::runtime::Runtime::new().expect("tokio rt"));
        let tray = Self::make_tray();
        Self { rt, tokio_rt, tab: Tab::Downloads, add_url: String::new(), add_referrer: String::new(), add_cookies: String::new(), add_queue: "main".into(), new_queue_name: String::new(), grab_url: String::new(), grab_depth: 2, grab_scope: 0, grab_exts: String::new(), grab_files: Vec::new(), grab_project_json: String::new(), last_tick: std::time::Instant::now(), tray: Some(TrayState { _tray: tray }), hidden, show_about: false, search: String::new(), minutes_today: today_parts(), category: "All Downloads".into(), sort_col: 0, sort_asc: true, options_tab: 0, show_add: false, show_progress_for: None, selected: None, completed_notice: None, last_notified: None, speed_hist: Vec::new(), last_dl_total: 0 }
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
                        // completion action only when queue drained (was: first task could exit/shutdown mid-queue)
                        let drained = !g.state.tasks.iter().any(|t| t.queue_id == queue_id && matches!(t.status, TaskStatus::Queued | TaskStatus::Downloading));
                        if drained { run_completion(&g, &queue_id, &tid); }
                    }
                }
                Err(e) => {
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
    }

    fn poll(&mut self, ctx: &egui::Context) {
        crate::drain_inbox(&self.rt);
        // progress sync
        {
            let mut g = self.rt.lock().unwrap();
            let ctrls: Vec<(String, u64)> = g.controls.iter().map(|(k, c)| (k.clone(), c.downloaded.load(std::sync::atomic::Ordering::Relaxed))).collect();
            for (id, n) in ctrls { if let Some(t) = g.state.tasks.iter_mut().find(|t| t.id == id) { t.downloaded = n; } }
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
                        .filter(|id| { g.state.tasks.iter().find(|t| &t.id == id).map(|t| t.next_retry_at <= now && g.state.queues.iter().find(|q| q.id == t.queue_id).map(|q| q.auto_start).unwrap_or(false)).unwrap_or(false) })
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
        theme::apply(ctx);
        self.poll(ctx);
        if self.hidden { return; }
        ctx.send_viewport_cmd(egui::ViewportCommand::Title("idmcraft — Download Manager".into()));

        // ── Top bar: brand + menus (IDM: Tasks/File/Downloads/View/Help) ──
        egui::TopBottomPanel::top("topbar").frame(egui::Frame::none().fill(theme::SURFACE).stroke(egui::Stroke::new(1.0f32, theme::BORDER)).inner_margin(egui::Margin::symmetric(8.0, 4.0))).show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("idmcraft").color(theme::TEXT).strong().size(17.0));
                theme::chip(ui, "v0.1.0", theme::TEAL_500);
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
                        if ui.button("About idmcraft").clicked() { self.show_about = true; ui.close_menu(); }
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
        egui::SidePanel::left("rail").resizable(false).exact_width(208.0)
            .frame(egui::Frame::none().fill(theme::SURFACE).stroke(egui::Stroke::new(1.0f32, theme::BORDER)).inner_margin(egui::Margin::same(6.0))).show(ctx, |ui| {
                ui.label(egui::RichText::new("WORKSPACE").color(theme::TEXT_FAINT).small().strong());
                ui.add_space(4.0);
                for (tab, label) in [(Tab::Downloads, "Downloads"), (Tab::Queues, "Queues"), (Tab::Scheduler, "Scheduler"), (Tab::Grabber, "Site Grabber"), (Tab::Options, "Options")] {
                    let on = self.tab == tab;
                    let b = egui::Button::new(egui::RichText::new(label).color(if on { egui::Color32::WHITE } else { theme::TEXT_MUTED })).fill(if on { theme::PRIMARY } else { egui::Color32::TRANSPARENT }).min_size(egui::vec2(188.0, 30.0));
                    if ui.add(b).clicked() { self.tab = tab; }
                }
                ui.add_space(14.0);
                ui.separator();
                ui.label(egui::RichText::new("CATEGORIES").color(theme::TEXT_FAINT).small().strong());
                ui.add_space(4.0);
                let cats = ["All Downloads", "Compressed", "Documents", "Music", "Programs", "Video", "Audio", "General", "Unfinished", "Finished", "Grabber projects", "Queues"];
                for c in cats {
                    let on = self.category == c;
                    let b = egui::Button::new(egui::RichText::new(c).color(if on { theme::TEAL_500 } else { theme::TEXT_MUTED })).fill(egui::Color32::TRANSPARENT).min_size(egui::vec2(188.0, 24.0));
                    if ui.add(b).clicked() { self.category = c.to_string(); self.tab = Tab::Downloads; }
                }
                ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                    ui.label(egui::RichText::new("ffmpeg: PATH/beside exe only").color(theme::TEXT_FAINT).small());
                    ui.label(egui::RichText::new("DRM content refused").color(theme::TEXT_FAINT).small());
                });
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

        egui::CentralPanel::default().frame(egui::Frame::none().fill(theme::BG).inner_margin(egui::Margin::same(8.0))).show(ctx, |ui| {
            match self.tab {
                Tab::Downloads => self.ui_downloads(ui),
                Tab::Queues => self.ui_queues(ui, false),
                Tab::Scheduler => self.ui_queues(ui, true),
                Tab::Grabber => self.ui_grabber(ui),
                Tab::Options => self.ui_options(ui),
            }
        });

        if self.show_about {
            egui::Window::new("About idmcraft").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
                ui.label("idmcraft 0.1.0 — clean-room cross-platform download manager.");
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
        // speed history sample (1/tick)
        let cur: u64 = { let g = self.rt.lock().unwrap(); g.state.tasks.iter().filter(|t| t.status == TaskStatus::Downloading).map(|t| t.downloaded).sum() };
        let dt = self.last_tick.elapsed().as_secs_f32().max(0.05);
        let rate = (cur.saturating_sub(self.last_dl_total)) as f32 / dt;
        self.last_dl_total = cur;
        if self.speed_hist.len() > 40 { self.speed_hist.remove(0); }
        self.speed_hist.push(rate);
    }
}

// free helpers used by menus
fn stop_task(app: &mut IdmApp, id: &str) { let mut g = app.rt.lock().unwrap(); if let Some(c) = g.controls.get(id) { c.paused.store(true, Ordering::Relaxed); }
    if let Some(t) = g.state.tasks.iter_mut().find(|t| t.id == id) { t.status = TaskStatus::Paused; } g.save(); }
fn remove_task(app: &mut IdmApp, id: &str) { let mut g = app.rt.lock().unwrap(); g.state.tasks.retain(|t| t.id != id); g.controls.remove(id); app.selected = None; g.save(); }
fn resume_all(app: &mut IdmApp) { let ids: Vec<String> = { let g = app.rt.lock().unwrap(); g.state.tasks.iter().filter(|t| matches!(t.status, TaskStatus::Paused | TaskStatus::Error | TaskStatus::Queued)).map(|t| t.id.clone()).collect() }; for id in ids { app.start_task(&id); } }
fn stop_all(app: &mut IdmApp) { let ids: Vec<String> = { let g = app.rt.lock().unwrap(); g.state.tasks.iter().filter(|t| t.status == TaskStatus::Downloading).map(|t| t.id.clone()).collect() }; for id in ids { stop_task(app, &id); } }
fn delete_completed(app: &mut IdmApp) { let mut g = app.rt.lock().unwrap(); g.state.tasks.retain(|t| t.status != TaskStatus::Complete); g.save(); }
fn toggle_limiter(app: &mut IdmApp) { let mut g = app.rt.lock().unwrap(); g.state.settings.speed_limit_kbps = if g.state.settings.speed_limit_kbps == 0 { 512 } else { 0 }; let lim = g.state.settings.speed_limit_kbps; for c in g.controls.values() { c.limit_kbps.store(lim, Ordering::Relaxed); } g.save(); }
fn export_json(app: &mut IdmApp) { let g = app.rt.lock().unwrap(); let txt = serde_json::to_string_pretty(&g.state.tasks.iter().map(|t| serde_json::json!({"url":t.url,"filename":t.filename,"queue":t.queue_id})).collect::<Vec<_>>()).unwrap_or_default(); let path = g.state.settings.download_dir.join("idmcraft-export.json"); let _ = std::fs::write(path, txt); }
fn add_batch_clipboard(app: &mut IdmApp) {
    if let Ok(mut cb) = arboard::Clipboard::new() { if let Ok(txt) = cb.get_text() { for line in txt.lines() { let u = line.trim(); if u.starts_with("http://") || u.starts_with("https://") { add_url_task(app, u); } } } }
}
fn add_url_task(app: &mut IdmApp, url: &str) {
    let id = { let mut g = app.rt.lock().unwrap(); g.add_url(url, "main", "", "") };
    app.start_task(&id);
}
fn fmt_rate(bps: f32) -> String { if bps <= 0.0 { "0 B/s".into() } else if bps < 1024.0 { format!("{bps:.0} B/s") } else if bps < 1024.0 * 1024.0 { format!("{:.1} KB/s", bps / 1024.0) } else { format!("{:.2} MB/s", bps / 1048576.0) } }
fn eta(secs: f64) -> String { if !secs.is_finite() || secs < 0.0 { "—".into() } else if secs < 60.0 { format!("{secs:.0}s") } else if secs < 3600.0 { format!("{:.0}m {:.0}s", secs / 60.0, secs % 60.0) } else { format!("{:.0}h {:.0}m", secs / 3600.0, (secs % 3600.0) / 60.0) } }

impl IdmApp {
    fn ui_downloads(&mut self, ui: &mut egui::Ui) {
        // Header + actions (IDM toolbar: Add URL, Resume, Stop, Stop All, Delete, Options…)
        ui.horizontal(|ui| {
            ui.heading("Downloads");
            theme::chip(ui, &self.category.clone(), theme::TEXT_MUTED);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if theme::primary_btn(ui, "+ Add URL").clicked() { self.show_add = true; }
                if theme::ghost_btn(ui, "Resume").clicked() { if let Some(id) = self.selected.clone() { self.start_task(&id); } }
                if theme::ghost_btn(ui, "Stop").clicked() { if let Some(id) = self.selected.clone() { stop_task(self, &id); } }
                if theme::ghost_btn(ui, "Stop all").clicked() { stop_all(self); }
                if theme::ghost_btn(ui, "Delete").clicked() { if let Some(id) = self.selected.clone() { remove_task(self, &id); } }
                if theme::ghost_btn(ui, "Delete completed").clicked() { delete_completed(self); }
            });
        });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Filter").color(theme::TEXT_MUTED).small());
            ui.add(egui::TextEdit::singleline(&mut self.search).hint_text("Search filename or URL…").desired_width(280.0));
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
        let rate_now = self.speed_hist.last().copied().unwrap_or(0.0) as f64;
        match self.sort_col {
            0 => tasks.sort_by_key(|a| a.filename.to_lowercase()),
            1 => tasks.sort_by_key(|t| t.queue_id.clone()),
            2 => tasks.sort_by_key(|t| t.total.unwrap_or(0)),
            3 => tasks.sort_by_key(|t| format!("{:?}", t.status)),
            _ => tasks.sort_by_key(|t| t.order),
        }
        if !self.sort_asc { tasks.reverse(); }

        // Table header (IDM columns)
        egui::Frame::none().fill(theme::SURFACE_RAISED).stroke(egui::Stroke::new(1.0f32, theme::BORDER)).inner_margin(egui::Margin::symmetric(8.0, 4.0)).show(ui, |ui| {
            egui::Grid::new("dl-head").num_columns(8).spacing([10.0, 4.0]).min_col_width(60.0).show(ui, |ui| {
                for (i, h) in ["File Name", "Q", "Size", "Status", "Time left", "Transfer rate", "Progress", ""].iter().enumerate() {
                    if ui.add(egui::Button::new(egui::RichText::new(*h).color(theme::TEXT_MUTED).small().strong()).fill(egui::Color32::TRANSPARENT)).clicked() {
                        if self.sort_col == i as u8 { self.sort_asc = !self.sort_asc; } else { self.sort_col = i as u8; self.sort_asc = true; }
                    }
                }
                ui.end_row();
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

        let mut to_start: Option<String> = None;
        let mut to_stop: Option<String> = None;
        let mut to_remove: Option<String> = None;
        let mut to_progress: Option<String> = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for t in &tasks {
                let sel = self.selected.as_deref() == Some(t.id.as_str());
                let fill = if sel { theme::STEEL_DIM } else { theme::SURFACE };
                let resp = egui::Frame::none().fill(fill).stroke(egui::Stroke::new(1.0f32, theme::BORDER)).inner_margin(egui::Margin::symmetric(8.0, 4.0)).show(ui, |ui| {
                    egui::Grid::new(format!("dl-{}", t.id)).num_columns(8).spacing([10.0, 4.0]).min_col_width(60.0).show(ui, |ui| {
                        ui.label(egui::RichText::new(&t.filename).color(theme::TEXT).strong());
                        ui.label(egui::RichText::new(if t.queue_id.is_empty() { "main" } else { &t.queue_id }).color(theme::TEXT_MUTED).small());
                        ui.label(egui::RichText::new(match t.total { Some(n) => fmt_bytes(n), None => "—".into() }).color(theme::TEXT_MUTED).small());
                        let sc = theme::status_color(t.status);
                        ui.label(egui::RichText::new(format!("{:?}", t.status)).color(sc).small().strong());
                        // ETA
                        let eta_txt = if t.status == TaskStatus::Downloading { if let Some(total) = t.total { if rate_now > 0.0 && total > t.downloaded { eta((total - t.downloaded) as f64 / rate_now) } else { "—".into() } } else { "—".into() } } else { "—".into() };
                        ui.label(egui::RichText::new(eta_txt).color(theme::TEXT_MUTED).small());
                        ui.label(egui::RichText::new(if t.status == TaskStatus::Downloading { fmt_rate(rate_now as f32) } else { "—".into() }).color(theme::TEXT_MUTED).small());
                        let frac = match t.total { Some(total) if total > 0 => (t.downloaded as f32 / total as f32).clamp(0.0, 1.0), _ => 0.0 };
                        ui.add(egui::ProgressBar::new(frac).desired_width(140.0).fill(theme::PRIMARY).text(format!("{:.0}%", frac * 100.0)));
                        ui.horizontal(|ui| {
                            if t.status == TaskStatus::Downloading {
                                if ui.small_button("Stop").clicked() { to_stop = Some(t.id.clone()); }
                            } else if t.status != TaskStatus::Complete && ui.small_button("Start").clicked() { to_start = Some(t.id.clone()); }
                            if ui.small_button("Progress").clicked() { to_progress = Some(t.id.clone()); }
                        });
                        ui.end_row();
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
                ui.label("Referrer"); ui.add(egui::TextEdit::singleline(&mut self.add_referrer).desired_width(300.0)); ui.end_row();
                ui.label("Cookies"); ui.add(egui::TextEdit::singleline(&mut self.add_cookies).desired_width(300.0)); ui.end_row();
            });
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if theme::primary_btn(ui, "Start download").clicked() {
                    let url = self.add_url.trim().to_string();
                    if !url.is_empty() {
                        let (referrer, cookies, queue) = (self.add_referrer.clone(), self.add_cookies.clone(), if self.add_queue.is_empty() { "main".into() } else { self.add_queue.clone() });
                        let id = { let mut g = self.rt.lock().unwrap(); let id = g.add_url(&url, &queue, &referrer, &cookies); let segs = g.state.settings.max_connections; if let Some(t) = g.state.tasks.iter_mut().find(|t| t.id == id) { t.segments = segs; } g.save(); id };
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
            let frac = match t.total { Some(total) if total > 0 => (t.downloaded as f32 / total as f32).clamp(0.0, 1.0), _ => 0.0 };
            ui.add(egui::ProgressBar::new(frac).desired_width(400.0).fill(theme::PRIMARY).text(format!("{:.1}%", frac * 100.0)));
            ui.add_space(6.0);
            egui::Grid::new("prog-grid").num_columns(2).spacing([14.0, 4.0]).show(ui, |ui| {
                ui.label(egui::RichText::new("Status").color(theme::TEXT_MUTED)); ui.label(format!("{:?}", t.status)); ui.end_row();
                ui.label(egui::RichText::new("Downloaded").color(theme::TEXT_MUTED)); ui.label(format!("{} / {}", fmt_bytes(t.downloaded), t.total.map(fmt_bytes).unwrap_or_else(|| "—".into()))); ui.end_row();
                ui.label(egui::RichText::new("Save to").color(theme::TEXT_MUTED)); ui.label(t.dest_dir.display().to_string()); ui.end_row();
                ui.label(egui::RichText::new("Queue").color(theme::TEXT_MUTED)); ui.label(if t.queue_id.is_empty() { "main".into() } else { t.queue_id.clone() }); ui.end_row();
                if !t.error.is_empty() { ui.label(egui::RichText::new("Error").color(theme::RED_600)); ui.label(egui::RichText::new(&t.error).color(theme::RED_600)); ui.end_row(); }
            });
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
                                if ui.small_button("Start").clicked() { self.start_task(&t.id.clone()); }
                                if ui.small_button("Down").clicked() { let mut g = self.rt.lock().unwrap(); move_task(&mut g.state.tasks, &t.id, 1); g.save(); }
                                if ui.small_button("Up").clicked() { let mut g = self.rt.lock().unwrap(); move_task(&mut g.state.tasks, &t.id, -1); g.save(); }
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
                        if theme::ghost_btn(ui, "Start now").clicked() { let ids: Vec<String> = self.rt.lock().unwrap().state.tasks.iter().filter(|t| t.queue_id == s.queue_id && t.status == TaskStatus::Queued).map(|t| t.id.clone()).collect(); for id in ids { self.start_task(&id); } }
                        if theme::ghost_btn(ui, "Delete").clicked() { let mut g = self.rt.lock().unwrap(); g.state.schedules.retain(|x| x.id != s.id); g.save(); }
                    });
                });
                ui.add_space(6.0);
            }
            ui.label(egui::RichText::new(format!("Clock: UTC day {day}, {secs}s into day (local-tz conversion not applied).")).color(theme::TEXT_FAINT).small());
        }
    }

    fn ui_grabber(&mut self, ui: &mut egui::Ui) {
        ui.heading("Site Grabber");
        ui.add_space(4.0);
        theme::panel().show(ui, |ui| {
            egui::Grid::new("grab-grid").num_columns(4).spacing([8.0, 6.0]).show(ui, |ui| {
                ui.label("Start URL:"); ui.add(egui::TextEdit::singleline(&mut self.grab_url).desired_width(420.0));
                ui.label("Depth:"); ui.add(egui::DragValue::new(&mut self.grab_depth).range(0..=10)); ui.end_row();
                ui.label("Extensions:"); ui.add(egui::TextEdit::singleline(&mut self.grab_exts).hint_text("zip,mp4,pdf (empty = all)").desired_width(420.0));
                ui.label("Scope:"); egui::ComboBox::from_id_salt("grab-scope").selected_text(["Same host", "Same domain", "Any host"][self.grab_scope.min(2) as usize]).show_ui(ui, |ui| { for (i, l) in ["Same host", "Same domain", "Any host"].iter().enumerate() { ui.selectable_value(&mut self.grab_scope, i as u8, *l); } }); ui.end_row();
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if theme::primary_btn(ui, "Explore").clicked() && !self.grab_url.trim().is_empty() {
                    let proj = GrabberProject { start_url: self.grab_url.clone(), depth: self.grab_depth, scope: self.grab_scope, extensions: self.grab_exts.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(), max_pages: 200, ..Default::default() };
                    self.grab_project_json = serde_json::to_string_pretty(&proj).unwrap_or_default();
                    let rt = self.rt.clone(); let client = engine::build_client("");
                    let crawler = Crawler { project: proj };
                    let files_slot = Arc::new(Mutex::new(Vec::new()));
                    let slot2 = files_slot.clone();
                    let rt2 = self.tokio_rt.clone();
                    std::thread::spawn(move || {
                        let res = rt2.block_on(async move { crawler.crawl(&client).await });
                        match res { Ok((_pages, files)) => { *slot2.lock().unwrap() = files; rt.lock().unwrap().log("Grabber explore done"); } Err(e) => { rt.lock().unwrap().log(format!("Grabber error: {e}")); } }
                    });
                    self.grab_files = Vec::new();
                    GRAB_SLOT.with(|s| *s.borrow_mut() = Some(files_slot));
                }
                if theme::ghost_btn(ui, "Refresh results").clicked() { GRAB_SLOT.with(|s| { if let Some(slot) = &*s.borrow() { self.grab_files = slot.lock().unwrap().clone(); } }); }
                if theme::ghost_btn(ui, "Mirror site…").clicked() {
                    if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                        let proj = GrabberProject { start_url: self.grab_url.clone(), depth: self.grab_depth, scope: self.grab_scope, extensions: self.grab_exts.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(), max_pages: 200, mirror: true, ..Default::default() };
                        let rt = self.rt.clone(); let rt2 = self.tokio_rt.clone();
                        std::thread::spawn(move || { let res = rt2.block_on(async move { crate::grabber::mirror_site(&engine::build_client(""), &proj, &dir).await }); match res { Ok(v) => rt.lock().unwrap().log(format!("Mirror done: {} files", v.len())), Err(e) => rt.lock().unwrap().log(format!("Mirror error: {e}")) } });
                    }
                }
                ui.label(egui::RichText::new("Explore crawls pages; Enqueue adds selected files as downloads with page referrer.").color(theme::TEXT_FAINT).small());
            });
        });
        ui.add_space(6.0);
        theme::panel().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(format!("{} files found · {} selected", self.grab_files.len(), self.grab_files.iter().filter(|f| f.selected).count())).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::primary_btn(ui, "Enqueue selected").clicked() { for f in self.grab_files.iter().filter(|f| f.selected) { let id = self.rt.lock().unwrap().add_url(&f.url.clone(), "main", &f.page_url.clone(), ""); let _ = id; } }
                    if theme::ghost_btn(ui, "Select none").clicked() { for f in &mut self.grab_files { f.selected = false; } }
                    if theme::ghost_btn(ui, "Select all").clicked() { for f in &mut self.grab_files { f.selected = true; } }
                    if theme::ghost_btn(ui, "Export .txt").clicked() { if let Some(p) = rfd::FileDialog::new().add_filter("txt", &["txt"]).save_file() { let _ = std::fs::write(p, self.grab_files.iter().map(|f| f.url.clone()).collect::<Vec<_>>().join("\n")); } }
                    if theme::ghost_btn(ui, "Export CSV").clicked() { if let Some(p) = rfd::FileDialog::new().add_filter("csv", &["csv"]).save_file() { let mut s = "url,size,mime,page\n".to_string(); for f in &self.grab_files { s.push_str(&format!("{},{},{},{}\n", f.url, f.size.map(|v| v.to_string()).unwrap_or_default(), f.mime, f.page_url)); } let _ = std::fs::write(p, s); } }
                    if theme::ghost_btn(ui, "Save project").clicked() { if let Some(p) = rfd::FileDialog::new().add_filter("json", &["json"]).save_file() { let _ = std::fs::write(p, &self.grab_project_json); } }
                    if theme::ghost_btn(ui, "Load project").clicked() { if let Some(p) = rfd::FileDialog::new().pick_file() { if let Ok(t) = std::fs::read_to_string(p) { self.grab_project_json = t.clone(); if let Ok(proj) = serde_json::from_str::<GrabberProject>(&t) { self.grab_url = proj.start_url; self.grab_depth = proj.depth; self.grab_scope = proj.scope; } } } }
                });
            });
            ui.separator();
            egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                if self.grab_files.is_empty() { ui.label(egui::RichText::new("No results yet. Explore a site, then Refresh results.").color(theme::TEXT_FAINT).small()); }
                for f in &mut self.grab_files {
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut f.selected, "");
                        ui.label(egui::RichText::new(&f.url).color(theme::TEXT));
                        ui.label(egui::RichText::new(f.size.map(fmt_bytes).unwrap_or_default()).color(theme::TEXT_MUTED).small());
                        ui.label(egui::RichText::new(&f.mime).color(theme::TEXT_FAINT).small());
                    });
                }
            });
        });
        ui.add_space(6.0);
        ui.collapsing("Project JSON", |ui| { ui.add(egui::TextEdit::multiline(&mut self.grab_project_json).desired_width(f32::INFINITY).font(egui::TextStyle::Monospace)); });
    }

    fn ui_options(&mut self, ui: &mut egui::Ui) {
        ui.heading("Configuration");
        ui.add_space(4.0);
        // IDM Configuration has 9 tabs: General/File types/Save To/Downloads/Connection/Proxy/Site logins/Dial-Up/Sounds.
        // Clone groups into left section list + right form (desktop property sheet feel).
        ui.horizontal_top(|ui| {
            theme::panel().show(ui, |ui| {
                ui.set_min_width(150.0);
                for (i, name) in ["General", "File types", "Save to", "Downloads", "Connection", "Proxy", "Browser", "About"].iter().enumerate() {
                    let on = self.options_tab == i as u8;
                    let b = egui::Button::new(egui::RichText::new(*name).color(if on { theme::TEXT } else { theme::TEXT_MUTED })).fill(if on { theme::STEEL_DIM } else { egui::Color32::TRANSPARENT }).min_size(egui::vec2(140.0, 24.0));
                    if ui.add(b).clicked() { self.options_tab = i as u8; }
                }
            });
            ui.add_space(6.0);
            theme::panel().show(ui, |ui| {
                ui.set_min_width(520.0);
                let mut g = self.rt.lock().unwrap();
                match self.options_tab {
                    0 => {
                        ui.label(egui::RichText::new("General").strong());
                        ui.separator();
                        ui.checkbox(&mut g.state.settings.start_minimized, "Start minimized");
                        ui.checkbox(&mut g.state.settings.close_to_tray, "Close to tray");
                        ui.checkbox(&mut g.state.settings.clipboard_monitor, "Automatically start downloading URLs placed to clipboard");
                        let mut autostart = g.state.settings.autostart;
                        if ui.checkbox(&mut autostart, "Launch on OS startup").changed() { let _ = platform::set_autostart(autostart); g.state.settings.autostart = autostart; }
                        ui.separator();
                        ui.horizontal(|ui| { ui.label("Download directory:"); ui.label(egui::RichText::new(g.state.settings.download_dir.display().to_string()).monospace().small()); if theme::ghost_btn(ui, "Browse…").clicked() { if let Some(p) = rfd::FileDialog::new().pick_folder() { g.state.settings.download_dir = p; } } });
                        ui.label(egui::RichText::new(format!("Config directory: {}", platform::config_dir().display())).color(theme::TEXT_FAINT).small());
                    }
                    1 => {
                        ui.label(egui::RichText::new("File types (categories)").strong());
                        ui.separator();
                        for (cat, exts) in [("Compressed", "zip rar 7z tar gz bz2 xz"), ("Documents", "pdf doc docx xls xlsx ppt pptx txt epub"), ("Music", "mp3 flac ogg wav m4a aac"), ("Programs", "exe msi dmg pkg deb rpm appimage"), ("Video", "mp4 mkv avi mov webm ts m3u8 mpd")] {
                            ui.horizontal(|ui| { ui.label(egui::RichText::new(cat).strong()); ui.label(egui::RichText::new(exts).color(theme::TEXT_MUTED).monospace().small()); });
                        }
                        ui.label(egui::RichText::new("Category assigned from filename at Add time; sidebar filters by category.").color(theme::TEXT_FAINT).small());
                    }
                    2 => {
                        ui.label(egui::RichText::new("Save to").strong());
                        ui.separator();
                        ui.horizontal(|ui| { ui.label("Default download directory:"); if theme::ghost_btn(ui, "Browse…").clicked() { if let Some(p) = rfd::FileDialog::new().pick_folder() { g.state.settings.download_dir = p; } } });
                        ui.label(egui::RichText::new(g.state.settings.download_dir.display().to_string()).monospace().small());
                        ui.label(egui::RichText::new("Per-category folders + remember-last-path: stored per task dest_dir at Add time.").color(theme::TEXT_FAINT).small());
                    }
                    3 => {
                        ui.label(egui::RichText::new("Downloads").strong());
                        ui.separator();
                        ui.add(egui::Slider::new(&mut g.state.settings.max_connections, 1..=32).text("Connections per file"));
                        ui.add(egui::Slider::new(&mut g.state.settings.global_max_concurrent, 1..=16).text("Simultaneous downloads"));
                        ui.add(egui::Slider::new(&mut g.state.settings.speed_limit_kbps, 0..=100_000).text("Speed limit KB/s (0 = unlimited)"));
                        ui.label(egui::RichText::new("Progress dialog: double-click a download. Download-complete dialog can be dismissed; “Don’t show again” = close it (state not persisted per-dialog).").color(theme::TEXT_FAINT).small());
                    }
                    4 => {
                        ui.label(egui::RichText::new("Connection").strong());
                        ui.separator();
                        ui.label(egui::RichText::new("Default max connections number: 8 (per-file connections in Downloads tab).").color(theme::TEXT_MUTED));
                        ui.label(egui::RichText::new("Connect timeout 15s · read timeout 60s · per-segment retry with backoff.").color(theme::TEXT_FAINT).small());
                    }
                    5 => {
                        ui.label(egui::RichText::new("Proxy / SOCKS").strong());
                        ui.separator();
                        ui.horizontal(|ui| { ui.label("Proxy:"); ui.add(egui::TextEdit::singleline(&mut g.state.settings.proxy_url).hint_text("http://user:pass@host:port or socks5://host:port").desired_width(360.0)); });
                        ui.label(egui::RichText::new("Empty = direct. Applied to HTTP engine + streams. Grabber uses direct client.").color(theme::TEXT_FAINT).small());
                    }
                    6 => {
                        ui.label(egui::RichText::new("Browser integration").strong());
                        ui.separator();
                        if theme::ghost_btn(ui, "Repair browser integration (register native hosts + protocol)").clicked() { match platform::register_native_hosts(&crate::sibling_host_path()) { Ok(p) => g.log(format!("Registered {} host manifests", p.len())), Err(e) => g.log(format!("Register failed: {e}")) } let _ = platform::register_protocol(); }
                        if theme::ghost_btn(ui, "Remove browser integration + autostart").clicked() { let _ = platform::unregister_native_hosts(); let _ = platform::set_autostart(false); g.state.settings.autostart = false; }
                        ui.label(egui::RichText::new(format!("Chromium extension ID (pinned): {}", idmcraft_lib::platform::CHROME_EXT_ID)).color(theme::TEXT_FAINT).small().monospace());
                        ui.label(egui::RichText::new("Load unpacked: extensions/chromium (MV3) · extensions/firefox (MV2). Native host: idmcraft-host.").color(theme::TEXT_FAINT).small());
                    }
                    _ => {
                        ui.label(egui::RichText::new("About").strong());
                        ui.separator();
                        ui.label("idmcraft 0.1.0 — clean-room cross-platform download manager.");
                        ui.label("Rust + egui. No proprietary IDM code, binaries, icons, or licensing logic.");
                        ui.label(egui::RichText::new("Sounds / Dial-Up / VPN tabs from IDM not cloned: desktop notifications use OS defaults; dial-up/VPN obsolete on target OSes.").color(theme::TEXT_FAINT).small());
                    }
                }
                ui.separator();
                ui.horizontal(|ui| {
                    if theme::primary_btn(ui, "Save").clicked() { let a = g.state.settings.autostart; save_state(&g.state); drop(g); let _ = platform::set_autostart(a); return; }
                    ui.label(egui::RichText::new("Changes apply after Save (autostart applies on toggle).").color(theme::TEXT_FAINT).small());
                });
            });
        });
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
