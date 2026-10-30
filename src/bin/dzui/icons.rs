//! Line icons drawn with the painter (egui's bundled fonts lack most emoji),
//! in the spirit of Bandizip's toolbar: thin strokes, document/folder shapes.

use eframe::egui;
use egui::{pos2, Color32, CornerRadius, Painter, Pos2, Rect, Shape, Stroke, StrokeKind, Vec2};

#[derive(Clone, Copy)]
pub enum Icon {
    Open,
    Extract,
    New,
    Auto,
    Test,
    Folder,
    File,
    Archive,
    Lock,
}

/// Draws `icon` inside `r` (square-ish) with stroke `c`.
pub fn draw(p: &Painter, r: Rect, icon: Icon, c: Color32) {
    let w = r.width().min(r.height());
    let s = Stroke::new((w / 22.0).max(1.2), c);
    let at = |x: f32, y: f32| pos2(r.left() + x * r.width(), r.top() + y * r.height());
    let line = |pts: &[Pos2]| p.add(Shape::line(pts.to_vec(), s));
    // A page with a folded corner; most icons start from it.
    let page = |x0: f32, y0: f32, x1: f32, y1: f32| {
        let f = (x1 - x0) * 0.28;
        line(&[at(x0, y0), at(x1 - f, y0), at(x1, y0 + f * r.width() / r.height()), at(x1, y1), at(x0, y1), at(x0, y0)]);
    };
    match icon {
        Icon::Open => {
            page(0.18, 0.1, 0.72, 0.78);
            p.rect_stroke(Rect::from_min_max(at(0.32, 0.26), at(0.86, 0.92)), CornerRadius::ZERO, s, StrokeKind::Middle);
            line(&[at(0.42, 0.6), at(0.74, 0.6)]);
            line(&[at(0.64, 0.5), at(0.75, 0.6), at(0.64, 0.7)]);
        }
        Icon::Extract => {
            page(0.2, 0.08, 0.8, 0.92);
            zipper(p, at, s, 0.5, 0.12, 0.46);
            line(&[at(0.5, 0.58), at(0.5, 0.82)]);
            line(&[at(0.4, 0.72), at(0.5, 0.83), at(0.6, 0.72)]);
        }
        Icon::New => {
            page(0.2, 0.08, 0.8, 0.92);
            zipper(p, at, s, 0.5, 0.12, 0.62);
            p.rect_stroke(Rect::from_min_max(at(0.43, 0.62), at(0.57, 0.78)), CornerRadius::same(1), s, StrokeKind::Middle);
        }
        Icon::Auto => {
            // Four-point sparkle.
            let c0 = at(0.5, 0.5);
            let (a, b) = (w * 0.4, w * 0.11);
            let pts: Vec<Pos2> = (0..8)
                .map(|i| {
                    let ang = i as f32 * std::f32::consts::FRAC_PI_4 - std::f32::consts::FRAC_PI_2;
                    let rr = if i % 2 == 0 { a } else { b };
                    c0 + Vec2::angled(ang) * rr
                })
                .collect();
            let mut closed = pts.clone();
            closed.push(pts[0]);
            line(&closed);
        }
        Icon::Test => {
            line(&[at(0.58, 0.06), at(0.26, 0.54), at(0.5, 0.54), at(0.4, 0.94), at(0.76, 0.42), at(0.52, 0.42), at(0.58, 0.06)]);
        }
        Icon::Folder => {
            let body = Rect::from_min_max(at(0.06, 0.28), at(0.94, 0.86));
            p.rect_filled(Rect::from_min_max(at(0.06, 0.16), at(0.44, 0.34)), CornerRadius::same(1), Color32::from_rgb(240, 190, 60));
            p.rect_filled(body, CornerRadius::same(1), Color32::from_rgb(252, 211, 85));
        }
        Icon::File => {
            let fill = Color32::from_gray(252);
            p.rect_filled(Rect::from_min_max(at(0.2, 0.08), at(0.8, 0.92)), CornerRadius::ZERO, fill);
            page(0.2, 0.08, 0.8, 0.92);
        }
        Icon::Archive => {
            p.rect_filled(Rect::from_min_max(at(0.18, 0.08), at(0.82, 0.92)), CornerRadius::same(2), Color32::from_rgb(30, 136, 229));
            zipper(p, at, Stroke::new(s.width, Color32::WHITE), 0.5, 0.12, 0.8);
        }
        Icon::Lock => {
            p.rect_filled(Rect::from_min_max(at(0.22, 0.46), at(0.78, 0.9)), CornerRadius::same(2), c);
            p.add(Shape::line(
                (0..=12)
                    .map(|i| {
                        let t = std::f32::consts::PI * i as f32 / 12.0;
                        at(0.5 - 0.17 * t.cos(), 0.46 - 0.3 * t.sin())
                    })
                    .collect(),
                s,
            ));
        }
    }
}

/// Alternating teeth down the middle of a page.
fn zipper(p: &Painter, at: impl Fn(f32, f32) -> Pos2, s: Stroke, x: f32, y0: f32, y1: f32) {
    let n = 5;
    for i in 0..n {
        let y = y0 + (y1 - y0) * i as f32 / n as f32;
        let (a, b) = if i % 2 == 0 { (x - 0.1, x) } else { (x, x + 0.1) };
        p.line_segment([at(a, y + 0.03), at(b, y + 0.03)], s);
    }
}

/// A small inline icon for list rows and the tree.
pub fn inline(ui: &mut egui::Ui, icon: Icon) {
    // As tall as a selectable label so the icon sits on the text's centre line.
    let h = ui.spacing().interact_size.y;
    let (r, _) = ui.allocate_exact_size(Vec2::new(18.0, h), egui::Sense::hover());
    let r = Rect::from_center_size(r.center(), Vec2::splat(18.0));
    draw(ui.painter(), r.shrink(1.0), icon, Color32::from_gray(110));
}
