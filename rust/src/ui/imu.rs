//! IMU tab: bubble level, per-axis angles, sag, a minute of strip chart.

use eframe::egui::{self, Align2, Color32, Pos2, Rect, Sense, Stroke, Vec2};

use super::theme::{self, Theme};
use super::widgets::{self as w, Tile};
use crate::protocol::{fmt_tilt, Live, SagStatus};

pub const FAST_SECONDS: f64 = 60.0;
const WINDOW_SEC: f64 = 60.0;

pub enum ImuAction {
    Fast(f64),
    Calibrate,
}

/// Each axis's angle to the horizontal plane, degrees: 0 = level, ±90 = straight down/up.
pub fn axis_angles(v: [f64; 3]) -> [f64; 3] {
    let g = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt().max(1e-9);
    v.map(|c| (c / g).clamp(-1.0, 1.0).asin().to_degrees())
}

/// Gravity's tilt away from the dominant ("down") axis as two angles, degrees. Works for
/// a horizontal card (down = Z) and a vertical mount (down = X or Y) alike.
/// Returns (down, u, w, tilt_u, tilt_w).
pub fn tilt_components(v: [f64; 3]) -> (usize, usize, usize, f64, f64) {
    let d = (0..3).max_by(|&a, &b| v[a].abs().total_cmp(&v[b].abs())).unwrap_or(2);
    let others: Vec<usize> = (0..3).filter(|&i| i != d).collect();
    let (u, wi) = (others[0], others[1]);
    let down = v[d].abs().max(1e-9);
    (d, u, wi, v[u].atan2(down).to_degrees(), v[wi].atan2(down).to_degrees())
}

pub fn axis_colors(t: &Theme) -> [Color32; 3] {
    [t.red, t.green, t.blue]
}

const NAMES: [&str; 3] = ["X", "Y", "Z"];

pub fn show(ui: &mut egui::Ui, t: &Theme, live: Option<&Live>) -> Option<ImuAction> {
    let empty = Live::default();
    let l = live.unwrap_or(&empty);
    let mut action = None;
    let now = w::now();

    // controls
    ui.horizontal(|ui| {
        let left = l.sag_fast_until - now;
        let fast_on = left > 0.0;
        let label = if fast_on { "Stop fast polling" } else { "Fast polling · 60 s" };
        if ui.add_enabled(live.is_some(), egui::Button::new(label).min_size(Vec2::new(0.0, 28.0))).clicked() {
            action = Some(ImuAction::Fast(if fast_on { 0.0 } else { FAST_SECONDS }));
        }
        let status = if fast_on && l.sag_fast {
            format!("10 Hz · {left:.0} s left")
        } else if fast_on {
            format!("held while limited · {left:.0} s")
        } else {
            "every 5 s".into()
        };
        ui.label(egui::RichText::new(status).color(t.muted()).size(12.0));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .add_enabled(
                    live.is_some() && l.sag_status != SagStatus::Unavailable,
                    egui::Button::new("Calibrate…").min_size(Vec2::new(0.0, 28.0)),
                )
                .clicked()
            {
                action = Some(ImuAction::Calibrate);
            }
        });
    });
    ui.add_space(w::GAP);

    // level + readouts
    let level = 248.0;
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = w::GAP;
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(level), Sense::hover());
        draw_level(ui, t, rect, l);
        let rest = ui.available_width();
        let (grid, _) = ui.allocate_exact_size(Vec2::new(rest, level), Sense::hover());
        let p = ui.painter_at(grid.expand(1.0));
        let cw = ((grid.width() - w::GAP) / 2.0).floor();
        let rh = ((grid.height() - 2.0 * w::GAP) / 3.0).floor();
        let cell = |c: usize, r: usize| {
            Rect::from_min_size(
                Pos2::new(grid.min.x + c as f32 * (cw + w::GAP), grid.min.y + r as f32 * (rh + w::GAP)),
                Vec2::new(cw, rh),
            )
        };
        let colors = axis_colors(t);
        let angles = l.sag_xyz.map(axis_angles);
        for i in 0..3 {
            let v = angles.map(|a| format!("{:+.2}°", a[i])).unwrap_or("—".into());
            let tile = Tile::new(NAMES[i], v).color(Some(colors[i])).sub("to horizontal");
            w::paint_tile(&p, t, cell(0, i), &tile);
        }
        let down = l
            .sag_xyz
            .map(|x| {
                let (d, ..) = tilt_components(x);
                format!("{} {}", NAMES[d], if x[d] > 0.0 { "+" } else { "−" })
            })
            .unwrap_or("—".into());
        w::paint_tile(&p, t, cell(1, 0), &Tile::new("Down axis", down));
        let abs = l.sag_offaxis.map(|d| fmt_tilt(d, l.card_length_mm)).unwrap_or("—".into());
        w::paint_tile(&p, t, cell(1, 1), &Tile::new("Tilt", abs).sub("from level"));
        let warn = matches!(l.sag_status, SagStatus::Warning | SagStatus::Serious);
        let sag = match (l.sag_deg, l.sag_status) {
            (Some(d), _) => Tile::new("Sag", fmt_tilt(d, l.card_length_mm))
                .color(warn.then_some(if l.sag_status == SagStatus::Serious { t.danger } else { t.pending }))
                .sub("since calibration"),
            (None, SagStatus::Uncalibrated) => Tile::new("Sag", "—").sub("not calibrated"),
            (None, SagStatus::Disabled) => Tile::new("Sag", "Off"),
            (None, SagStatus::Unavailable) => Tile::new("Sag", "n/a").sub("no G-sensor"),
            _ => Tile::new("Sag", "—"),
        };
        w::paint_tile(&p, t, cell(1, 2), &sag);
    });
    ui.add_space(w::GAP);

    strips(ui, t, l, now);
    action
}

fn draw_level(ui: &egui::Ui, t: &Theme, rect: Rect, l: &Live) {
    let p = ui.painter_at(rect.expand(1.0));
    p.rect(rect, 8.0, t.surface, Stroke::new(1.0_f32, t.border), egui::StrokeKind::Inside);
    let c = rect.center() + Vec2::new(0.0, 6.0);
    let r = rect.width() / 2.0 - 36.0;
    w::paint_caption(&p, rect.min + Vec2::new(12.0, 10.0), Align2::LEFT_TOP, "Level", t.muted());
    let Some(xyz) = l.sag_xyz else {
        p.text(c, Align2::CENTER_CENTER, "No G-sensor data", theme::regular(12.0), t.muted());
        return;
    };
    let s = &l.settings;
    let (warn, serious) = (s.sag_warn_degrees, s.sag_serious_degrees);
    let (_d, u, wi, tu, tw) = tilt_components(xyz);
    let (bu, bw) = l
        .sag_baseline
        .map(|b| {
            let (.., a, b) = tilt_components(b);
            (a, b)
        })
        .unwrap_or((0.0, 0.0));
    let scale =
        [3.0, serious * 1.6, tu.abs() * 1.2, tw.abs() * 1.2, bu.abs() + serious * 1.2, bw.abs() + serious * 1.2]
            .into_iter()
            .fold(0.0, f64::max);
    let k = r as f64 / scale;
    let pt = |a: f64, b: f64| Pos2::new(c.x + (a * k) as f32, c.y - (b * k) as f32);

    let grid = Stroke::new(1.0_f32, t.grid());
    p.circle_stroke(c, r, grid);
    p.line_segment([c - Vec2::new(r, 0.0), c + Vec2::new(r, 0.0)], grid);
    p.line_segment([c - Vec2::new(0.0, r), c + Vec2::new(0.0, r)], grid);
    p.text(c + Vec2::new(r + 4.0, 0.0), Align2::LEFT_CENTER, format!("+{}", NAMES[u]), theme::regular(10.5), t.faint());
    p.text(
        c - Vec2::new(0.0, r + 3.0),
        Align2::CENTER_BOTTOM,
        format!("+{}", NAMES[wi]),
        theme::regular(10.5),
        t.faint(),
    );
    p.text(
        rect.max - Vec2::new(10.0, 8.0),
        Align2::RIGHT_BOTTOM,
        format!("edge {scale:.1}°"),
        theme::regular(10.5),
        t.faint(),
    );

    if l.sag_baseline.is_some() {
        let b = pt(bu, bw);
        for (ang, col) in [(warn, t.pending), (serious, t.danger)] {
            let rr = (ang * k) as f32;
            let n = 48;
            let pts: Vec<Pos2> = (0..=n)
                .map(|i| {
                    let a = i as f32 / n as f32 * std::f32::consts::TAU;
                    b + Vec2::new(a.cos(), a.sin()) * rr
                })
                .collect();
            p.extend(egui::Shape::dashed_line(&pts, Stroke::new(1.2_f32, col.gamma_multiply(0.85)), 4.0, 3.0));
        }
        let m = Stroke::new(1.0_f32, t.muted());
        p.line_segment([b - Vec2::new(5.0, 0.0), b + Vec2::new(5.0, 0.0)], m);
        p.line_segment([b - Vec2::new(0.0, 5.0), b + Vec2::new(0.0, 5.0)], m);
    }
    // recent trail, then the current position
    let n = l.imu_history.len();
    for h in &l.imu_history[n.saturating_sub(40)..] {
        let (.., a, b) = tilt_components([h.1, h.2, h.3]);
        p.circle_filled(pt(a, b), 1.6, t.faint());
    }
    let cur = pt(tu, tw);
    p.circle_filled(cur, 7.5, t.accent.gamma_multiply(0.35));
    p.circle_filled(cur, 5.5, t.accent);
    p.circle_stroke(cur, 5.5, Stroke::new(1.0_f32, t.text.gamma_multiply(0.8)));
}

fn strips(ui: &mut egui::Ui, t: &Theme, l: &Live, now: f64) {
    let width = ui.available_width();
    let height = ui.available_height().max(186.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
    let p = ui.painter_at(rect.expand(1.0));
    p.rect(rect, 8.0, t.surface, Stroke::new(1.0_f32, t.border), egui::StrokeKind::Inside);
    w::paint_caption(&p, rect.min + Vec2::new(12.0, 10.0), Align2::LEFT_TOP, "Axes · 60 s", t.muted());

    let hist: Vec<(f64, [f64; 3])> =
        l.imu_history.iter().filter(|h| now - h.0 <= WINDOW_SEC).map(|h| (h.0, axis_angles([h.1, h.2, h.3]))).collect();
    let left = rect.min.x + 34.0;
    let right = rect.max.x - 112.0;
    let top0 = rect.min.y + 30.0;
    let strip_h = (rect.max.y - 22.0 - top0) / 3.0;
    let colors = axis_colors(t);
    let px = |ts: f64| right - ((now - ts) / WINDOW_SEC) as f32 * (right - left);
    for i in 0..3 {
        let top = top0 + i as f32 * strip_h;
        let bot = top + strip_h - 8.0;
        let mid_y = (top + bot) / 2.0;
        p.text(Pos2::new(rect.min.x + 14.0, mid_y), Align2::LEFT_CENTER, NAMES[i], theme::semibold(12.5), colors[i]);
        p.line_segment([Pos2::new(left, bot), Pos2::new(right, bot)], Stroke::new(1.0_f32, t.grid()));
        let vals: Vec<f64> = hist.iter().map(|h| h.1[i]).collect();
        if vals.is_empty() {
            continue;
        }
        let lo = vals.iter().cloned().fold(f64::MAX, f64::min);
        let hi = vals.iter().cloned().fold(f64::MIN, f64::max);
        // degrees: noise-level changes stay flat, not huge
        let span = (hi - lo).max(0.2);
        let mid = (hi + lo) / 2.0;
        let (lo_s, hi_s) = (mid - span * 0.6, mid + span * 0.6);
        let py = |v: f64| bot - ((v - lo_s) / (hi_s - lo_s)) as f32 * (bot - top);
        if hist.len() >= 2 {
            let pts: Vec<Pos2> = hist.iter().map(|h| Pos2::new(px(h.0), py(h.1[i]))).collect();
            p.add(egui::Shape::line(pts, Stroke::new(1.6_f32, colors[i])));
        } else {
            p.circle_filled(Pos2::new(px(hist[0].0), py(vals[0])), 2.0, colors[i]);
        }
        let last = vals[vals.len() - 1];
        p.text(
            Pos2::new(right + 12.0, mid_y - 7.0),
            Align2::LEFT_CENTER,
            format!("{last:+.2}°"),
            theme::semibold(13.0),
            t.text,
        );
        p.text(
            Pos2::new(right + 12.0, mid_y + 8.0),
            Align2::LEFT_CENTER,
            format!("Δ {:.2}°", hi - lo),
            theme::regular(10.5),
            t.muted(),
        );
    }
    p.text(Pos2::new(left, rect.max.y - 8.0), Align2::LEFT_BOTTOM, "-60 s", theme::regular(10.5), t.faint());
    p.text(Pos2::new(right, rect.max.y - 8.0), Align2::RIGHT_BOTTOM, "now", theme::regular(10.5), t.faint());
}
