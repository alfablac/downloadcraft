// TEMP repro: replicate progress dialog widgets in raw egui, click around.
use egui::{CentralPanel, Context, RawInput, Window};

fn input(pos: egui::Pos2, pressed: bool) -> RawInput {
    let mut input = RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1200.0, 800.0),
        )),
        ..Default::default()
    };
    input.events.push(egui::Event::PointerMoved(pos));
    if pressed {
        input.events.push(egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: Default::default(),
        });
    }
    input
}

fn frame(ctx: &Context, pos: egui::Pos2, pressed: bool) {
    let _ = ctx.run(input(pos, pressed), |ctx| {
        CentralPanel::default().show(ctx, |ui| {
            ui.label("main");
        });
        Window::new("Download progress")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .default_size([484.0, 0.0])
            .show(ctx, |ui| {
                ui.set_min_width(460.0);
                ui.label(egui::RichText::new(
                    "Bonuses, MULTi8) [FitGirl Repack, Selective Download - from 1.3 GB]",
                ).strong().size(14.0));
                let bar_w = ui.available_width();
                ui.add(egui::ProgressBar::new(0.25).desired_width(bar_w));
                egui::Grid::new("prog-grid")
                    .num_columns(2)
                    .spacing([12.0, 6.0])
                    .min_col_width(120.0)
                    .max_col_width(1024.0)
                    .show(ui, |ui| {
                        ui.label("Speed graph");
                        let (rect, _) = ui.allocate_exact_size(
                            egui::vec2(ui.available_width(), 48.0),
                            egui::Sense::hover(),
                        );
                        ui.painter().rect_filled(rect, 2.0, egui::Color32::BLACK);
                        ui.end_row();
                        ui.label("Limit (KB/s, 0=off)");
                        let mut lim: i64 = 0;
                        ui.add_sized([56.0, 32.0], egui::DragValue::new(&mut lim).range(0..=1_000_000));
                        ui.end_row();
                        ui.label("URL");
                        let mut url = "magnet:?xt=urn:btih:15A7DB908759C576A2A0724055DF460A8EC4E3A3".to_string();
                        ui.add(egui::TextEdit::singleline(&mut url).desired_width(f32::INFINITY));
                        ui.end_row();
                    });
                ui.add_space(8.0);
                egui::Grid::new("prog-swarm")
                    .num_columns(2)
                    .spacing([12.0, 6.0])
                    .min_col_width(120.0)
                    .max_col_width(1024.0)
                    .show(ui, |ui| {
                        ui.label("Peers");
                        ui.label("3 connected");
                        ui.end_row();
                        ui.label("Trackers");
                        ui.vertical(|ui| {
                            for i in 0..21 {
                                ui.add(
                                    egui::Label::new(format!("udp://tracker{i}.example.org:1337/announce"))
                                        .truncate(),
                                );
                            }
                        });
                        ui.end_row();
                    });
                ui.button("Close").clicked();
                ui.button("Open folder").clicked();
                ui.button("Pause").clicked();
            });
    });
}

#[test]
fn dialog_clicks_no_panic() {
    let ctx = Context::default();
    // warm frames
    for _ in 0..5 {
        frame(&ctx, egui::pos2(600.0, 400.0), false);
    }
    // click grid across dialog incl title-bar drag area + buttons
    for y in (180..620).step_by(20) {
        for x in (380..820).step_by(20) {
            frame(&ctx, egui::pos2(x as f32, y as f32), false);
            frame(&ctx, egui::pos2(x as f32, y as f32), true);
            frame(&ctx, egui::pos2(x as f32, y as f32), false);
        }
    }
}
