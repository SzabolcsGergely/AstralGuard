//! Thermal-camera rendering of the 12V-2x6 plug, straight onto egui's painter: the heat
//! field is computed per texel into a small texture (bilinear-filtered when scaled up),
//! everything sharp (outlines, labels, markers) is drawn on top as shapes.
//!
//! The heat is the measured current mapped onto an ironbow ramp, not a temperature: the
//! card has no per-pin temperature sensors. The ramp is anchored to the guard's own
//! thresholds, so the limit is orange, tier 2 yellow and tier 3 white-hot.
//!
//! Layout follows the 12V-2x6 pinout (latch over the ground row, sense pins on the far
//! side). Which socket each Pin number really is has not been verified; the caller picks
//! the left-to-right order (Settings: mirror).

use eframe::egui::{
    self, Align2, Color32, ColorImage, CornerRadius, FontId, Painter, Pos2, Rect, Shape, Stroke, StrokeKind,
    TextureHandle, TextureOptions, Vec2,
};

use super::theme;

// Design space; everything is laid out in these units and scaled to fit.
const DW: f32 = 1000.0;
const DH: f32 = 440.0;

const STOPS: [(f32, [u8; 3]); 8] = [
    (0.00, [4, 4, 18]),
    (0.12, [18, 12, 72]),
    (0.30, [88, 14, 128]),
    (0.48, [172, 24, 112]),
    (0.62, [226, 66, 38]),
    (0.76, [250, 140, 18]),
    (0.88, [255, 210, 60]),
    (1.00, [255, 255, 236]),
];

const CYAN: Color32 = Color32::from_rgb(90, 230, 255);
const DIM: Color32 = Color32::from_rgba_premultiplied(120, 132, 141, 150); // (200,220,235) @ 150
const WHITE: Color32 = Color32::from_rgb(240, 248, 255);
const ALARM: Color32 = Color32::from_rgb(255, 90, 90);
const COLD: Color32 = Color32::from_rgb(90, 200, 255);

const HOUSING: [f32; 4] = [175.0, 72.0, 715.0, 318.0];
const LATCH: [f32; 4] = [385.0, 48.0, 505.0, 72.0];
const SOCKET_W: f32 = 70.0;
const SOCKET_H: f32 = 62.0;
const SCALE: [f32; 4] = [790.0, 76.0, 806.0, 314.0];
/// Below this a flagged pin is labelled "NO CURRENT" rather than "LOW".
const DEAD_AMPS: f64 = 1.0;

fn pitch() -> f32 {
    (HOUSING[2] - HOUSING[0] - 40.0) / 6.0
}

fn sense(i: usize) -> [f32; 4] {
    let x = 300.0 + i as f32 * 80.0;
    [x, 262.0, x + 48.0, 284.0]
}

/// Socket box; `gnd` row on top (under the latch), +12V row below it.
fn socket(gnd: bool, slot: usize) -> [f32; 4] {
    let x = HOUSING[0] + 20.0 + slot as f32 * pitch() + (pitch() - SOCKET_W) / 2.0;
    let y = if gnd { 92.0 } else { 172.0 };
    [x, y, x + SOCKET_W, y + SOCKET_H]
}

pub fn ramp(t: f32) -> [u8; 3] {
    let t = t.clamp(0.0, 1.0);
    for w in STOPS.windows(2) {
        let ((t0, c0), (t1, c1)) = (w[0], w[1]);
        if t <= t1 {
            let f = (t - t0) / (t1 - t0);
            let m = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * f).round() as u8;
            return [m(c0[0], c1[0]), m(c0[1], c1[1]), m(c0[2], c1[2])];
        }
    }
    STOPS[7].1
}

fn ramp32(t: f32) -> Color32 {
    let c = ramp(t);
    Color32::from_rgb(c[0], c[1], c[2])
}

/// Amps -> 0..1 on the ramp; normal load lands in the magenta-red band.
pub fn heat(amps: f64, limit: f64, tier2: f64, tier3: f64) -> f32 {
    let pts = [(0.0, 0.0), (limit, 0.64), (tier2, 0.86), (tier3, 1.0)];
    for w in pts.windows(2) {
        let ((a0, t0), (a1, t1)) = (w[0], w[1]);
        if amps <= a1 {
            return (t0 + (t1 - t0) * (amps - a0) / (a1 - a0).max(1e-6)).max(0.0) as f32;
        }
    }
    1.0
}

pub struct Input<'a> {
    pub pins: &'a [(String, f64)],
    /// Pin index per slot, left to right.
    pub order: Vec<usize>,
    /// Pins flagged by the imbalance rule.
    pub low: Vec<bool>,
    pub limits: (f64, f64, f64),
    pub badge: (String, Color32),
}

#[derive(PartialEq)]
struct Key {
    w: usize,
    h: usize,
    hot: Vec<(usize, i32, bool)>,
}

#[derive(Default)]
pub struct Thermal {
    tex: Option<TextureHandle>,
    key: Option<Key>,
}

struct Fit {
    origin: Pos2,
    k: f32,
    ox: f32,
    oy: f32,
}

impl Fit {
    fn new(r: Rect) -> Fit {
        let k = (r.width() / DW).min(r.height() / DH);
        Fit { origin: r.min, k, ox: (r.width() - DW * k) / 2.0, oy: (r.height() - DH * k) / 2.0 }
    }
    fn pt(&self, x: f32, y: f32) -> Pos2 {
        Pos2::new(self.origin.x + self.ox + x * self.k, self.origin.y + self.oy + y * self.k)
    }
    fn rect(&self, b: [f32; 4]) -> Rect {
        Rect::from_min_max(self.pt(b[0], b[1]), self.pt(b[2], b[3]))
    }
    fn chamfer(&self, b: [f32; 4], c: f32) -> Vec<Pos2> {
        let [x0, y0, x1, y1] = b;
        [(x0 + c, y0), (x1 - c, y0), (x1, y0 + c), (x1, y1 - c), (x1 - c, y1), (x0 + c, y1), (x0, y1 - c), (x0, y0 + c)]
            .iter()
            .map(|&(x, y)| self.pt(x, y))
            .collect()
    }
}

fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

/// Soft inside-mask of a chamfered box: 1 inside, 0 outside, `soft` design units of edge.
fn chamfer_mask(x: f32, y: f32, b: [f32; 4], c: f32, soft: f32) -> f32 {
    let (cx, cy) = ((b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0);
    let (hw, hh) = ((b[2] - b[0]) / 2.0, (b[3] - b[1]) / 2.0);
    let (dx, dy) = ((x - cx).abs(), (y - cy).abs());
    let d_rect = (dx - hw).max(dy - hh);
    let d_ch = (dx + dy - (hw + hh - c)) / std::f32::consts::SQRT_2;
    sigmoid(-d_rect.max(d_ch) / soft)
}

/// Soft inside-mask of a rounded box.
fn rounded_mask(x: f32, y: f32, b: [f32; 4], r: f32, soft: f32) -> f32 {
    let (cx, cy) = ((b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0);
    let (hw, hh) = ((b[2] - b[0]) / 2.0 - r, (b[3] - b[1]) / 2.0 - r);
    let (qx, qy) = ((x - cx).abs() - hw, (y - cy).abs() - hh);
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    let d = outside + qx.max(qy).min(0.0) - r;
    sigmoid(-d / soft)
}

/// Soft ellipse disc: radius rx, ry; `soft` units of blur at the edge.
fn disc(x: f32, y: f32, cx: f32, cy: f32, rx: f32, ry: f32, soft: f32) -> f32 {
    let rho = (((x - cx) / rx).powi(2) + ((y - cy) / ry).powi(2)).sqrt();
    sigmoid((1.0 - rho) * rx.min(ry) / soft)
}

fn field(x: f32, y: f32, slots: &[(usize, f32, bool)]) -> f32 {
    // the housing's resting warmth
    let mut v = 10.0 / 255.0
        + (24.0 / 255.0) * rounded_mask(x, y, HOUSING, 14.0, 6.0)
        + (20.0 / 255.0) * chamfer_mask(x, y, LATCH, 0.0, 4.0);
    for &(slot, t, _) in slots {
        if t <= 0.001 {
            continue;
        }
        let b = socket(false, slot);
        let (cx, cy) = ((b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0);
        // quick reject: far from this pin, nothing to add
        if (x - cx).abs() > 140.0 || (y - cy).abs() > 160.0 {
            continue;
        }
        let r = 34.0 + 26.0 * t;
        v = v.max(t * 0.92 * disc(x, y, cx, cy, r, r * 0.9, 13.0));
        let g = socket(true, slot);
        let (gx, gy) = ((g[0] + g[2]) / 2.0, (g[1] + g[3]) / 2.0);
        // the ground return runs warm too
        v = v.max(t * 0.55 * disc(x, y, gx, gy, 29.0, 25.0, 12.0));
        // the socket itself: a cooler rim around a hot core
        let mut core = 0.80 * chamfer_mask(x, y, b, 12.0, 2.5);
        for (inset, f) in [(8.0f32, 0.88f32), (16.0, 0.95), (23.0, 1.0)] {
            let rx = SOCKET_W / 2.0 - inset;
            let ry = SOCKET_H / 2.0 - inset * 0.8;
            core = core.max(f * disc(x, y, cx, cy, rx, ry, 2.5));
        }
        v = v.max(t * core);
    }
    // a dead pin is a cold hole in its warm neighbours' glow
    for &(slot, t, low) in slots {
        if low {
            let m = chamfer_mask(x, y, socket(false, slot), 12.0, 2.0);
            v = v * (1.0 - m) + t * m;
        }
    }
    v
}

impl Thermal {
    pub fn show(&mut self, ui: &mut egui::Ui, rect: Rect, input: &Input) {
        let painter = ui.painter_at(rect);
        let fit = Fit::new(rect);
        let (limit, tier2, tier3) = input.limits;
        let slots: Vec<(usize, usize, f64)> = input
            .order
            .iter()
            .take(6)
            .enumerate()
            .filter(|(_, &i)| i < input.pins.len())
            .map(|(slot, &i)| (slot, i, input.pins[i].1))
            .collect();
        let hot: Vec<(usize, f32, bool)> = slots
            .iter()
            .map(|&(slot, i, a)| (slot, heat(a, limit, tier2, tier3), input.low.get(i).copied().unwrap_or(false)))
            .collect();

        // The heat field: blurry by nature, so a low-resolution texture scaled up.
        let tw = (rect.width() * 0.42).round().clamp(80.0, 480.0) as usize;
        let th = ((tw as f32) * rect.height() / rect.width()).round().max(20.0) as usize;
        let key = Key { w: tw, h: th, hot: hot.iter().map(|&(s, t, l)| (s, (t * 400.0) as i32, l)).collect() };
        if self.key.as_ref() != Some(&key) || self.tex.is_none() {
            let mut pixels = Vec::with_capacity(tw * th);
            for j in 0..th {
                for i in 0..tw {
                    let sx = (i as f32 + 0.5) / tw as f32 * rect.width();
                    let sy = (j as f32 + 0.5) / th as f32 * rect.height();
                    let x = (sx - fit.ox) / fit.k;
                    let y = (sy - fit.oy) / fit.k;
                    pixels.push(ramp32(field(x, y, &hot)));
                }
            }
            let img = ColorImage::new([tw, th], pixels);
            match &mut self.tex {
                Some(t) => t.set(img, TextureOptions::LINEAR),
                None => self.tex = Some(ui.ctx().load_texture("thermal", img, TextureOptions::LINEAR)),
            }
            self.key = Some(key);
        }
        if let Some(tex) = &self.tex {
            let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
            painter.add(
                egui::epaint::RectShape::filled(rect, CornerRadius::same(8), Color32::WHITE).with_texture(tex.id(), uv),
            );
        }
        self.overlay(&painter, &fit, rect, input, &slots, &hot);
    }

    fn overlay(
        &self,
        d: &Painter,
        f: &Fit,
        rect: Rect,
        input: &Input,
        slots: &[(usize, usize, f64)],
        hot: &[(usize, f32, bool)],
    ) {
        let k = f.k;
        let (limit, tier2, tier3) = input.limits;
        let line = Color32::from_white_alpha(70);
        let lw = (1.6 * k).max(1.0);

        // housing, latch, sockets, sense pins
        d.rect_stroke(f.rect(HOUSING), CornerRadius::same((14.0 * k) as u8), Stroke::new(lw, line), StrokeKind::Middle);
        d.rect_stroke(f.rect(LATCH), CornerRadius::ZERO, Stroke::new(lw, line), StrokeKind::Middle);
        d.line_segment([f.pt(LATCH[0] + 14.0, 60.0), f.pt(LATCH[2] - 14.0, 60.0)], Stroke::new(lw * 0.6, line));
        for slot in 0..6 {
            d.add(Shape::closed_line(f.chamfer(socket(true, slot), 12.0), Stroke::new(lw, line)));
            let b = socket(false, slot);
            d.add(Shape::closed_line(f.chamfer(b, 12.0), Stroke::new(lw, Color32::from_white_alpha(110))));
            d.rect_stroke(
                f.rect([b[0] + 22.0, b[1] + 18.0, b[2] - 22.0, b[3] - 18.0]),
                CornerRadius::same((4.0 * k) as u8),
                Stroke::new(lw * 0.6, Color32::from_white_alpha(45)),
                StrokeKind::Middle,
            );
        }
        for i in 0..4 {
            d.rect_stroke(f.rect(sense(i)), CornerRadius::ZERO, Stroke::new(lw * 0.6, line), StrokeKind::Middle);
        }

        let sans = |px: f32, min: f32| theme::semibold((px * k).max(min));
        let mono = |px: f32, min: f32| FontId::monospace((px * k).max(min));

        // row labels
        for (label, y) in [("GND", 123.0), ("+12V", 203.0), ("SENSE", 273.0)] {
            d.text(f.pt(HOUSING[0] - 18.0, y), Align2::RIGHT_CENTER, label, sans(12.0, 9.0), DIM);
            d.line_segment([f.pt(HOUSING[0] - 12.0, y), f.pt(HOUSING[0] - 2.0, y)], Stroke::new(1.0_f32, DIM));
        }

        // the colour scale, like a camera's temperature bar
        let s = f.rect(SCALE);
        let n = 48;
        for i in 0..n {
            let y0 = s.min.y + s.height() * i as f32 / n as f32;
            let y1 = s.min.y + s.height() * (i + 1) as f32 / n as f32;
            let tt = 1.0 - (i as f32 + 0.5) / n as f32;
            d.rect_filled(Rect::from_x_y_ranges(s.x_range(), y0..=y1 + 0.5), 0.0, ramp32(tt));
        }
        d.rect_stroke(s, 0.0, Stroke::new(1.0_f32, Color32::from_white_alpha(80)), StrokeKind::Outside);
        for (amps, label) in [(tier3, "TIER 3"), (tier2, "TIER 2"), (limit, "LIMIT"), (0.0, "")] {
            let y = s.max.y - s.height() * heat(amps, limit, tier2, tier3);
            d.line_segment([Pos2::new(s.min.x - 4.0, y), Pos2::new(s.max.x + 4.0, y)], Stroke::new(1.0_f32, WHITE));
            let r = d.text(
                Pos2::new(s.max.x + 9.0, y),
                Align2::LEFT_CENTER,
                format!("{} A", crate::config::g(amps)),
                mono(13.0, 10.0),
                WHITE,
            );
            if !label.is_empty() {
                d.text(
                    Pos2::new(f.pt(SCALE[2] + 9.0 + 56.0, 0.0).x.max(r.max.x + 6.0), y),
                    Align2::LEFT_CENTER,
                    label,
                    sans(11.0, 8.5),
                    DIM,
                );
            }
        }

        // title and viewfinder corners
        let m = 7.0;
        let l = (22.0 * k).max(12.0);
        for (x, y, sx, sy) in [
            (rect.min.x + m, rect.min.y + m, 1.0, 1.0),
            (rect.max.x - m, rect.min.y + m, -1.0, 1.0),
            (rect.min.x + m, rect.max.y - m, 1.0, -1.0),
            (rect.max.x - m, rect.max.y - m, -1.0, -1.0),
        ] {
            let st = Stroke::new(1.5_f32, CYAN);
            d.line_segment([Pos2::new(x, y), Pos2::new(x + sx * l, y)], st);
            d.line_segment([Pos2::new(x, y), Pos2::new(x, y + sy * l)], st);
        }
        let tr =
            d.text(rect.min + Vec2::new(18.0, 13.0), Align2::LEFT_TOP, "12V-2x6  ·  THERMAL", sans(15.0, 11.0), CYAN);
        d.text(
            Pos2::new(tr.min.x, tr.max.y + 2.0),
            Align2::LEFT_TOP,
            "pin order as reported",
            theme::regular((11.0 * k).max(9.5)),
            DIM,
        );

        // status badge
        let (badge, bc) = (&input.badge.0, input.badge.1);
        if !badge.is_empty() {
            let g = d.layout_no_wrap(badge.clone(), theme::semibold((14.0 * k).max(10.5)), bc);
            let h = g.size().y + 8.0;
            let x1 = rect.max.x - 30.0;
            let r = Rect::from_min_max(
                Pos2::new(x1 - g.size().x - 20.0, rect.min.y + 12.0),
                Pos2::new(x1, rect.min.y + 12.0 + h),
            );
            d.rect_filled(r, 4.0, Color32::from_black_alpha(110));
            d.rect_stroke(r, 4.0, Stroke::new(1.5_f32, bc), StrokeKind::Inside);
            d.galley(r.center() - g.size() / 2.0, g, bc);
        }

        if slots.is_empty() {
            d.text(f.pt(445.0, 370.0), Align2::CENTER_CENTER, "NO PIN DATA", sans(22.0, 13.0), DIM);
            return;
        }

        // MAX / MIN markers
        let (mut hi, mut lo) = (slots[0], slots[0]);
        for &s in slots {
            if s.2 > hi.2 {
                hi = s;
            }
            if s.2 < lo.2 {
                lo = s;
            }
        }
        let mut marks = vec![(hi.0, WHITE, "MAX")];
        if lo.1 != hi.1 && lo.2 < hi.2 {
            marks.push((lo.0, COLD, "MIN"));
        }
        for (slot, col, tag) in marks {
            let b = socket(false, slot);
            let c = f.pt((b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0);
            let (a, bb) = ((7.0 * k).max(4.0), (17.0 * k).max(9.0));
            for (dx, dy) in [(1.0, 0.0), (-1.0, 0.0), (0.0, 1.0), (0.0, -1.0)] {
                d.line_segment(
                    [c + Vec2::new(dx * a, dy * a), c + Vec2::new(dx * bb, dy * bb)],
                    Stroke::new(1.6_f32, col),
                );
            }
            d.text(f.pt(b[2] - 4.0, b[1] - 3.0), Align2::RIGHT_BOTTOM, tag, sans(11.0, 9.0), col);
        }

        // per-pin readout under the plug
        for (n, &(slot, i, amps)) in slots.iter().enumerate() {
            let b = socket(false, slot);
            let cx = (b[0] + b[2]) / 2.0;
            let flagged = hot[n].2;
            if flagged {
                d.add(Shape::closed_line(f.chamfer(b, 12.0), Stroke::new(2.0_f32, ALARM)));
            }
            let name = input.pins[i].0.to_uppercase();
            d.text(f.pt(cx, 336.0), Align2::CENTER_CENTER, name, sans(12.0, 9.0), if flagged { ALARM } else { DIM });
            d.text(f.pt(cx, 357.0), Align2::CENTER_CENTER, format!("{amps:.2}"), mono(21.0, 12.0), WHITE);
            d.text(f.pt(cx, 377.0), Align2::CENTER_CENTER, "A", theme::regular((10.0 * k).max(8.5)), DIM);
            let bar = Rect::from_min_max(f.pt(b[0] - 4.0, 394.0), f.pt(b[2] + 4.0, 400.0));
            d.rect_filled(bar, 1.0, Color32::from_white_alpha(28));
            let frac = (amps / tier3).clamp(0.0, 1.0) as f32;
            if frac > 0.0 {
                let fill = Rect::from_min_max(bar.min, Pos2::new(bar.min.x + bar.width() * frac, bar.max.y));
                d.rect_filled(fill, 1.0, ramp32(hot[n].1.max(0.35)));
            }
            for (lim, c) in [(limit, Color32::from_rgb(255, 190, 40)), (tier2, Color32::from_rgb(255, 120, 30))] {
                let tx = bar.min.x + bar.width() * (lim / tier3).clamp(0.0, 1.0) as f32;
                d.line_segment(
                    [Pos2::new(tx, bar.min.y - 3.0), Pos2::new(tx, bar.max.y + 3.0)],
                    Stroke::new(1.0_f32, c),
                );
            }
            if flagged {
                let txt = if amps < DEAD_AMPS { "NO CURRENT" } else { "LOW" };
                d.text(f.pt(cx, 420.0), Align2::CENTER_CENTER, txt, sans(11.0, 9.0), ALARM);
            }
        }
    }
}
