//! PackCraft's own icon set, drawn with vector strokes (no image assets).

use eframe::egui::{Color32, Painter, Pos2, Rect, Stroke, StrokeKind, Vec2, pos2, vec2};

#[derive(Clone, Copy)]
pub enum Icon {
    Add,
    Extract,
    Test,
    View,
    Info,
    Up,
    Folder,
    File,
    Archive,
}

/// The PackCraft mark: a rounded crate with three stacked layers.
pub fn crate_mark(p: &Painter, r: Rect, fill: Color32, line: Color32) {
    let s = r.width();
    p.rect_filled(r.shrink(s * 0.06), s * 0.18, fill);
    let inner = r.shrink(s * 0.24);
    for i in 0..3 {
        let y = inner.top() + inner.height() * (0.18 + 0.32 * i as f32);
        let indent = s * 0.06 * i as f32;
        p.line_segment([pos2(inner.left() + indent, y), pos2(inner.right() - indent, y)], Stroke::new(s * 0.08, line));
    }
}

pub fn paint(p: &Painter, r: Rect, icon: Icon, color: Color32, accent: Color32) {
    let s = r.width().min(r.height());
    let c = r.center();
    let w = Stroke::new((s * 0.09).max(1.2), color);
    let a = Stroke::new((s * 0.11).max(1.4), accent);
    let at = |x: f32, y: f32| -> Pos2 { c + vec2(x * s, y * s) };
    let boxr = |inset: f32| Rect::from_center_size(c + vec2(0.0, s * 0.08), Vec2::splat(s * (0.78 - inset)));
    match icon {
        Icon::Add => {
            p.rect_stroke(boxr(0.1), s * 0.1, w, StrokeKind::Middle);
            p.line_segment([at(0.0, -0.08), at(0.0, 0.28)], a);
            p.line_segment([at(-0.18, 0.1), at(0.18, 0.1)], a);
            p.line_segment([at(-0.32, -0.3), at(0.32, -0.3)], w);
        }
        Icon::Extract => {
            p.rect_stroke(boxr(0.1), s * 0.1, w, StrokeKind::Middle);
            p.line_segment([at(0.0, 0.28), at(0.0, -0.44)], a);
            p.line_segment([at(-0.15, -0.3), at(0.0, -0.45)], a);
            p.line_segment([at(0.15, -0.3), at(0.0, -0.45)], a);
        }
        Icon::Test => {
            p.circle_stroke(c, s * 0.38, w);
            p.line_segment([at(-0.18, 0.0), at(-0.04, 0.15)], a);
            p.line_segment([at(-0.04, 0.15), at(0.2, -0.14)], a);
        }
        Icon::View => {
            let pts: Vec<Pos2> = (0..=24)
                .map(|i| {
                    let t = i as f32 / 24.0 * std::f32::consts::TAU;
                    at(0.42 * t.cos(), 0.22 * t.sin())
                })
                .collect();
            p.add(eframe::egui::Shape::closed_line(pts, w));
            p.circle_filled(c, s * 0.12, accent);
        }
        Icon::Info => {
            p.circle_stroke(c, s * 0.38, w);
            p.circle_filled(at(0.0, -0.18), s * 0.05, accent);
            p.line_segment([at(0.0, -0.04), at(0.0, 0.22)], a);
        }
        Icon::Up => {
            p.line_segment([at(0.0, 0.34), at(0.0, -0.3)], w);
            p.line_segment([at(-0.22, -0.08), at(0.0, -0.32)], w);
            p.line_segment([at(0.22, -0.08), at(0.0, -0.32)], w);
        }
        Icon::Folder => {
            let body = Rect::from_min_max(at(-0.42, -0.2), at(0.42, 0.32));
            p.rect_filled(Rect::from_min_max(at(-0.42, -0.32), at(-0.06, -0.12)), s * 0.06, accent);
            p.rect_filled(body, s * 0.08, accent);
        }
        Icon::File => {
            let r2 = Rect::from_min_max(at(-0.28, -0.4), at(0.28, 0.4));
            p.rect_stroke(r2, s * 0.06, Stroke::new(w.width * 0.8, color), StrokeKind::Middle);
            for k in 0..3 {
                let y = -0.14 + 0.16 * k as f32;
                p.line_segment([at(-0.14, y), at(0.14, y)], Stroke::new(w.width * 0.6, color));
            }
        }
        Icon::Archive => crate_mark(p, Rect::from_center_size(c, Vec2::splat(s * 0.92)), accent, Color32::WHITE),
    }
}

/// RGBA pixels of the window icon (the crate mark), drawn without any image file.
pub fn window_icon(size: u32, fill: [u8; 3]) -> Vec<u8> {
    let n = size as f32;
    let mut px = vec![0u8; (size * size * 4) as usize];
    let radius = n * 0.18;
    let (lo, hi) = (n * 0.06, n * 0.94);
    for y in 0..size {
        for x in 0..size {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            // Rounded-square coverage.
            let dx = (lo + radius - fx).max(fx - (hi - radius)).max(0.0);
            let dy = (lo + radius - fy).max(fy - (hi - radius)).max(0.0);
            let inside = (dx * dx + dy * dy).sqrt() <= radius && fx >= lo && fx <= hi && fy >= lo && fy <= hi;
            if !inside {
                continue;
            }
            let mut rgb = fill;
            for i in 0..3 {
                let yc = n * (0.24 + 0.52 * (0.18 + 0.32 * i as f32));
                let indent = n * 0.06 * i as f32;
                if (fy - yc).abs() <= n * 0.04 && fx >= n * 0.24 + indent && fx <= n * 0.76 - indent {
                    rgb = [255, 255, 255];
                }
            }
            let o = ((y * size + x) * 4) as usize;
            px[o..o + 4].copy_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
        }
    }
    px
}
