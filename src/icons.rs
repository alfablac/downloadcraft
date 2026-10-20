//! Vector icons drawn with egui primitives only. No emoji, no fonts, no images.

use egui::epaint::PathStroke;
use egui::{Color32, Painter, Pos2, Rect, Shape, Stroke};

/// Count of [`Icon`] variants. Kept in sync by exhaustive test below.
pub const ICON_COUNT: usize = 25;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
#[allow(dead_code)]
pub enum Icon {
    Download,
    File,
    Folder,
    Video,
    Music,
    Document,
    Archive,
    Program,
    Play,
    Pause,
    Plus,
    Delete,
    FolderOpen,
    Settings,
    Globe,
    Queue,
    Clock,
    Search,
    Link,
    Check,
    Error,
    Info,
    Up,
    Down,
    Cloud,
}

/// Stable short name per variant.
pub fn name(kind: &Icon) -> &'static str {
    match kind {
        Icon::Download => "download",
        Icon::File => "file",
        Icon::Folder => "folder",
        Icon::Video => "video",
        Icon::Music => "music",
        Icon::Document => "document",
        Icon::Archive => "archive",
        Icon::Program => "program",
        Icon::Play => "play",
        Icon::Pause => "pause",
        Icon::Plus => "plus",
        Icon::Delete => "delete",
        Icon::FolderOpen => "folder-open",
        Icon::Settings => "settings",
        Icon::Globe => "globe",
        Icon::Queue => "queue",
        Icon::Clock => "clock",
        Icon::Search => "search",
        Icon::Link => "link",
        Icon::Check => "check",
        Icon::Error => "error",
        Icon::Info => "info",
        Icon::Up => "up",
        Icon::Down => "down",
        Icon::Cloud => "cloud",
    }
}

/// All variants, for tests and iteration.
pub const ALL: [Icon; ICON_COUNT] = [
    Icon::Download,
    Icon::File,
    Icon::Folder,
    Icon::Video,
    Icon::Music,
    Icon::Document,
    Icon::Archive,
    Icon::Program,
    Icon::Play,
    Icon::Pause,
    Icon::Plus,
    Icon::Delete,
    Icon::FolderOpen,
    Icon::Settings,
    Icon::Globe,
    Icon::Queue,
    Icon::Clock,
    Icon::Search,
    Icon::Link,
    Icon::Check,
    Icon::Error,
    Icon::Info,
    Icon::Up,
    Icon::Down,
    Icon::Cloud,
];

// --- geometry helpers (pure, unit-testable without a Painter) ---

fn pt(rect: Rect, u: f32, v: f32) -> Pos2 {
    Pos2::new(
        rect.left() + u * rect.width(),
        rect.top() + v * rect.height(),
    )
}

fn seg(p: &Painter, a: Pos2, b: Pos2, s: Stroke) {
    p.line_segment([a, b], s);
}

fn poly(p: &Painter, pts: &[Pos2], s: Stroke) {
    for w in pts.windows(2) {
        p.line_segment([w[0], w[1]], s);
    }
}

fn poly_shape(pts: Vec<Pos2>, fill: Color32, stroke: Stroke) -> Shape {
    Shape::convex_polygon(pts, fill, PathStroke::from(stroke))
}

fn stroke_for(rect: Rect, color: Color32) -> Stroke {
    Stroke::new(
        (rect.width().min(rect.height()) * 0.08).clamp(1.0, 2.5),
        color,
    )
}

/// Draw `kind` inside `rect` in `color`. Nothing outside `rect`.
pub fn icon(p: &Painter, kind: &Icon, rect: Rect, color: Color32) {
    let s = stroke_for(rect, color);
    let c = rect.center();
    match kind {
        Icon::Download => {
            seg(p, pt(rect, 0.50, 0.12), pt(rect, 0.50, 0.58), s);
            poly(
                p,
                &[
                    pt(rect, 0.30, 0.40),
                    pt(rect, 0.50, 0.62),
                    pt(rect, 0.70, 0.40),
                ],
                s,
            );
            seg(p, pt(rect, 0.18, 0.82), pt(rect, 0.82, 0.82), s);
            seg(p, pt(rect, 0.18, 0.82), pt(rect, 0.18, 0.68), s);
            seg(p, pt(rect, 0.82, 0.82), pt(rect, 0.82, 0.68), s);
        }
        Icon::File => {
            let r = Rect::from_min_max(pt(rect, 0.26, 0.12), pt(rect, 0.74, 0.88));
            p.rect_stroke(r, 0.0, s);
            poly(
                p,
                &[
                    pt(rect, 0.58, 0.12),
                    pt(rect, 0.58, 0.30),
                    pt(rect, 0.74, 0.30),
                ],
                s,
            );
            seg(p, pt(rect, 0.36, 0.55), pt(rect, 0.64, 0.55), s);
            seg(p, pt(rect, 0.36, 0.70), pt(rect, 0.64, 0.70), s);
        }
        Icon::Folder => {
            let body = Rect::from_min_max(pt(rect, 0.12, 0.30), pt(rect, 0.88, 0.82));
            p.rect_stroke(body, 0.0, s);
            // tab
            poly(
                p,
                &[
                    pt(rect, 0.12, 0.30),
                    pt(rect, 0.12, 0.18),
                    pt(rect, 0.42, 0.18),
                    pt(rect, 0.50, 0.30),
                ],
                s,
            );
        }
        Icon::FolderOpen => {
            // open folder: slanted front panel polygon + back edge
            seg(p, pt(rect, 0.12, 0.24), pt(rect, 0.42, 0.24), s);
            seg(p, pt(rect, 0.42, 0.24), pt(rect, 0.50, 0.34), s);
            seg(p, pt(rect, 0.12, 0.24), pt(rect, 0.12, 0.76), s);
            seg(p, pt(rect, 0.50, 0.34), pt(rect, 0.88, 0.34), s);
            p.add(poly_shape(
                vec![
                    pt(rect, 0.20, 0.52),
                    pt(rect, 0.90, 0.52),
                    pt(rect, 0.80, 0.82),
                    pt(rect, 0.12, 0.82),
                ],
                Color32::TRANSPARENT,
                s,
            ));
        }
        Icon::Video => {
            let r = Rect::from_min_max(pt(rect, 0.12, 0.22), pt(rect, 0.88, 0.78));
            p.rect_stroke(r, 0.0, s);
            p.add(poly_shape(
                vec![
                    pt(rect, 0.42, 0.38),
                    pt(rect, 0.42, 0.62),
                    pt(rect, 0.64, 0.50),
                ],
                color,
                Stroke::NONE,
            ));
        }
        Icon::Music => {
            // note heads
            let r1 = rect.width() * 0.11;
            p.circle_filled(pt(rect, 0.34, 0.74), r1, color);
            p.circle_filled(pt(rect, 0.72, 0.66), r1, color);
            // stems + beam
            seg(p, pt(rect, 0.44, 0.72), pt(rect, 0.44, 0.28), s);
            seg(p, pt(rect, 0.82, 0.64), pt(rect, 0.82, 0.20), s);
            seg(p, pt(rect, 0.44, 0.28), pt(rect, 0.82, 0.20), s);
        }
        Icon::Document => {
            let r = Rect::from_min_max(pt(rect, 0.24, 0.10), pt(rect, 0.76, 0.90));
            p.rect_stroke(r, 0.0, s);
            poly(
                p,
                &[
                    pt(rect, 0.56, 0.10),
                    pt(rect, 0.56, 0.28),
                    pt(rect, 0.76, 0.28),
                ],
                s,
            );
            for v in [0.44, 0.58, 0.72] {
                seg(p, pt(rect, 0.34, v), pt(rect, 0.66, v), s);
            }
        }
        Icon::Archive => {
            let r = Rect::from_min_max(pt(rect, 0.16, 0.16), pt(rect, 0.84, 0.84));
            p.rect_stroke(r, 0.0, s);
            seg(p, pt(rect, 0.16, 0.34), pt(rect, 0.84, 0.34), s);
            // zipper: short vertical dashes down middle above band
            seg(p, pt(rect, 0.50, 0.18), pt(rect, 0.50, 0.24), s);
            p.rect_filled(
                Rect::from_min_max(pt(rect, 0.44, 0.42), pt(rect, 0.56, 0.52)),
                0.0,
                color,
            );
        }
        Icon::Program => {
            let r = Rect::from_min_max(pt(rect, 0.12, 0.16), pt(rect, 0.88, 0.84));
            p.rect_stroke(r, 0.0, s);
            seg(p, pt(rect, 0.12, 0.32), pt(rect, 0.88, 0.32), s);
            let br = rect.width() * 0.045;
            p.circle_filled(pt(rect, 0.20, 0.24), br, color);
            p.circle_filled(pt(rect, 0.30, 0.24), br, color);
            p.circle_filled(pt(rect, 0.40, 0.24), br, color);
        }
        Icon::Play => {
            p.add(poly_shape(
                vec![
                    pt(rect, 0.30, 0.18),
                    pt(rect, 0.30, 0.82),
                    pt(rect, 0.78, 0.50),
                ],
                color,
                Stroke::NONE,
            ));
        }
        Icon::Pause => {
            let w = rect.width() * 0.14;
            let (x0, x1) = (
                rect.left() + rect.width() * 0.30,
                rect.left() + rect.width() * 0.56,
            );
            let (t, b) = (
                rect.top() + rect.height() * 0.20,
                rect.top() + rect.height() * 0.80,
            );
            p.rect_filled(
                Rect::from_min_max(Pos2::new(x0, t), Pos2::new(x0 + w, b)),
                0.0,
                color,
            );
            p.rect_filled(
                Rect::from_min_max(Pos2::new(x1, t), Pos2::new(x1 + w, b)),
                0.0,
                color,
            );
        }
        Icon::Plus => {
            seg(p, pt(rect, 0.50, 0.18), pt(rect, 0.50, 0.82), s);
            seg(p, pt(rect, 0.18, 0.50), pt(rect, 0.82, 0.50), s);
        }
        Icon::Delete => {
            // bin body
            poly(
                p,
                &[
                    pt(rect, 0.26, 0.32),
                    pt(rect, 0.30, 0.84),
                    pt(rect, 0.70, 0.84),
                    pt(rect, 0.74, 0.32),
                ],
                s,
            );
            seg(p, pt(rect, 0.20, 0.32), pt(rect, 0.80, 0.32), s); // lid
            seg(p, pt(rect, 0.42, 0.32), pt(rect, 0.42, 0.22), s); // handle
            seg(p, pt(rect, 0.58, 0.32), pt(rect, 0.58, 0.22), s);
            seg(p, pt(rect, 0.42, 0.22), pt(rect, 0.58, 0.22), s);
            seg(p, pt(rect, 0.42, 0.44), pt(rect, 0.43, 0.72), s);
            seg(p, pt(rect, 0.58, 0.44), pt(rect, 0.57, 0.72), s);
        }
        Icon::Settings => {
            let rad = rect.width().min(rect.height()) * 0.16;
            p.circle_stroke(c, rad, s);
            // 6 spokes + rim ticks
            for i in 0..6 {
                let a = std::f32::consts::TAU * (i as f32) / 6.0;
                let (sin, cos) = a.sin_cos();
                let r0 = rect.width() * 0.20;
                let r1 = rect.width() * 0.34;
                seg(
                    p,
                    Pos2::new(c.x + cos * r0, c.y + sin * r0),
                    Pos2::new(c.x + cos * r1, c.y + sin * r1),
                    s,
                );
            }
            p.circle_stroke(c, rect.width() * 0.38, s);
        }
        Icon::Globe => {
            let r = rect.width().min(rect.height()) * 0.38;
            p.circle_stroke(c, r, s);
            // meridian ellipse approximated by two arcs of lines
            let mut prev: Option<Pos2> = None;
            for i in 0..=12 {
                let a = std::f32::consts::TAU * (i as f32) / 12.0;
                let q = Pos2::new(c.x + a.cos() * r * 0.45, c.y + a.sin() * r);
                if let Some(pr) = prev {
                    seg(p, pr, q, s);
                }
                prev = Some(q);
            }
            seg(p, Pos2::new(c.x - r, c.y), Pos2::new(c.x + r, c.y), s);
            // parallels
            for dy in [-0.45f32, 0.45] {
                let yy = c.y + dy * r;
                let hw = r * (1.0 - dy * dy).sqrt();
                seg(p, Pos2::new(c.x - hw, yy), Pos2::new(c.x + hw, yy), s);
            }
        }
        Icon::Queue => {
            for (row, v) in [0.24f32, 0.50, 0.76].iter().enumerate() {
                let dot_c = pt(rect, 0.24, *v);
                p.circle_filled(dot_c, rect.width() * 0.05, color);
                let x_end = if row == 2 { 0.62 } else { 0.82 };
                seg(p, pt(rect, 0.40, *v), pt(rect, x_end, *v), s);
            }
        }
        Icon::Clock => {
            let r = rect.width().min(rect.height()) * 0.38;
            p.circle_stroke(c, r, s);
            seg(p, c, pt(rect, 0.50, 0.26), s);
            seg(p, c, pt(rect, 0.66, 0.56), s);
            p.circle_filled(c, rect.width() * 0.04, color);
        }
        Icon::Search => {
            let cc = pt(rect, 0.42, 0.42);
            p.circle_stroke(cc, rect.width() * 0.26, s);
            seg(p, pt(rect, 0.62, 0.62), pt(rect, 0.84, 0.84), s);
        }
        Icon::Link => {
            // two chain links: diagonal bars + end arcs approximated by polylines
            poly(p, &[pt(rect, 0.36, 0.64), pt(rect, 0.58, 0.42)], s);
            // lower-left link
            poly(
                p,
                &[
                    pt(rect, 0.38, 0.30),
                    pt(rect, 0.22, 0.46),
                    pt(rect, 0.30, 0.70),
                    pt(rect, 0.54, 0.62),
                ],
                s,
            );
            // upper-right link
            poly(
                p,
                &[
                    pt(rect, 0.62, 0.70),
                    pt(rect, 0.78, 0.54),
                    pt(rect, 0.70, 0.30),
                    pt(rect, 0.46, 0.38),
                ],
                s,
            );
        }
        Icon::Check => {
            poly(
                p,
                &[
                    pt(rect, 0.18, 0.52),
                    pt(rect, 0.42, 0.76),
                    pt(rect, 0.84, 0.24),
                ],
                s,
            );
        }
        Icon::Error => {
            let r = rect.width().min(rect.height()) * 0.38;
            p.circle_stroke(c, r, s);
            seg(p, pt(rect, 0.36, 0.36), pt(rect, 0.64, 0.64), s);
            seg(p, pt(rect, 0.64, 0.36), pt(rect, 0.36, 0.64), s);
        }
        Icon::Info => {
            let r = rect.width().min(rect.height()) * 0.38;
            p.circle_stroke(c, r, s);
            p.circle_filled(pt(rect, 0.50, 0.34), rect.width() * 0.05, color);
            seg(p, pt(rect, 0.50, 0.48), pt(rect, 0.50, 0.70), s);
        }
        Icon::Up => {
            poly(
                p,
                &[
                    pt(rect, 0.22, 0.62),
                    pt(rect, 0.50, 0.30),
                    pt(rect, 0.78, 0.62),
                ],
                s,
            );
            seg(p, pt(rect, 0.50, 0.30), pt(rect, 0.50, 0.78), s);
        }
        Icon::Down => {
            poly(
                p,
                &[
                    pt(rect, 0.22, 0.38),
                    pt(rect, 0.50, 0.70),
                    pt(rect, 0.78, 0.38),
                ],
                s,
            );
            seg(p, pt(rect, 0.50, 0.22), pt(rect, 0.50, 0.70), s);
        }
        Icon::Cloud => {
            let r = rect.width();
            p.circle_stroke(pt(rect, 0.38, 0.58), r * 0.15, s);
            p.circle_stroke(pt(rect, 0.58, 0.48), r * 0.18, s);
            p.circle_stroke(pt(rect, 0.74, 0.60), r * 0.12, s);
            seg(p, pt(rect, 0.27, 0.68), pt(rect, 0.83, 0.68), s);
            seg(p, pt(rect, 0.55, 0.68), pt(rect, 0.55, 0.86), s);
            poly(
                p,
                &[
                    pt(rect, 0.46, 0.78),
                    pt(rect, 0.55, 0.90),
                    pt(rect, 0.64, 0.78),
                ],
                s,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile-time exhaustiveness: match with no wildcard arm.
    fn exhaustive(kind: &Icon) -> bool {
        match kind {
            Icon::Download
            | Icon::File
            | Icon::Folder
            | Icon::Video
            | Icon::Music
            | Icon::Document
            | Icon::Archive
            | Icon::Program
            | Icon::Play
            | Icon::Pause
            | Icon::Plus
            | Icon::Delete
            | Icon::FolderOpen
            | Icon::Settings
            | Icon::Globe
            | Icon::Queue
            | Icon::Clock
            | Icon::Search
            | Icon::Link
            | Icon::Check
            | Icon::Error
            | Icon::Info
            | Icon::Up
            | Icon::Cloud
            | Icon::Down => true,
        }
    }

    #[test]
    fn all_variants_named_nonempty_and_unique() {
        assert_eq!(ALL.len(), ICON_COUNT);
        let mut names: Vec<&str> = ALL.iter().map(name).collect();
        for n in &names {
            assert!(!n.is_empty());
        }
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), ICON_COUNT, "names must be unique");
        for k in &ALL {
            assert!(exhaustive(k));
        }
    }

    #[test]
    fn geometry_helpers_stay_inside_rect() {
        let rect = Rect::from_min_max(Pos2::new(10.0, 20.0), Pos2::new(50.0, 80.0));
        for u in [0.0, 0.25, 0.5, 1.0] {
            for v in [0.0, 0.5, 1.0] {
                let q = pt(rect, u, v);
                assert!(rect.contains(q), "{q:?} outside {rect:?}");
            }
        }
        // stroke width bounded
        let s = stroke_for(rect, Color32::WHITE);
        assert!((1.0..=2.5).contains(&s.width));
        let tiny = stroke_for(
            Rect::from_min_size(Pos2::ZERO, egui::vec2(4.0, 4.0)),
            Color32::WHITE,
        );
        assert_eq!(tiny.width, 1.0);
    }

    #[test]
    fn polygon_shapes_are_convex_helper_ok() {
        // poly_shape builds a Shape without panicking for triangle input
        let sh = poly_shape(
            vec![Pos2::ZERO, Pos2::new(10.0, 0.0), Pos2::new(5.0, 8.0)],
            Color32::RED,
            Stroke::NONE,
        );
        let _ = format!("{sh:?}");
    }
}
