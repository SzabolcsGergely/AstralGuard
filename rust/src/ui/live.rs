//! Overview tab: the connector, the numbers that matter, five minutes of history.

use eframe::egui::{self, Align2, Color32, Pos2, Rect, Sense, Stroke, Vec2};

use super::theme::{self, Theme};
use super::thermal::{self, Thermal};
use super::widgets::{self as w, Tile};
use crate::protocol::{Live, ResponseMode, SagStatus, Settings, Status};

const HISTORY_SECONDS: f64 = 300.0;

/// Indexes of pins far below the busiest one, by the imbalance check's own rule. Tier
/// colours alone would paint a burnt-open 0 A pin as cool and fine.
pub fn low_pins(pins: &[(String, f64)], s: &Settings) -> Vec<bool> {
    let hi = pins.iter().map(|p| p.1).fold(f64::MIN, f64::max);
    if pins.is_empty() || hi < s.imbalance_min_amps || hi <= 0.0 {
        return vec![false; pins.len()];
    }
    pins.iter()
        .map(|&(_, a)| hi - a >= s.imbalance_delta_amps || (hi - a) / hi * 100.0 >= s.imbalance_percent)
        .collect()
}

/// The status tag in the thermal view's corner.
fn badge(l: &Live) -> (String, Color32) {
    let c = |r, g, b| Color32::from_rgb(r, g, b);
    if l.paused {
        return ("UNPROTECTED".into(), w::rgb(Status::Paused.color()));
    }
    match l.status {
        Status::Shutdown => ("DANGER".into(), c(255, 64, 64)),
        _ if l.engaged => (format!("PROTECTING · TIER {}", l.engaged_tier.unwrap_or(1)), c(255, 106, 60)),
        Status::Over => ("WARNING".into(), c(255, 170, 0)),
        _ if l.imbalance_active => ("IMBALANCE".into(), c(255, 190, 40)),
        Status::Error | Status::Missing => ("UNKNOWN".into(), c(180, 140, 255)),
        _ => ("SAFE".into(), c(90, 255, 140)),
    }
}

/// Colour for a current against the tiers: plain, pending, danger.
pub fn amps_color(t: &Theme, amps: f64, s: &Settings) -> Option<Color32> {
    if amps >= s.critical_amps {
        Some(t.danger)
    } else if amps >= s.threshold_amps {
        Some(t.pending)
    } else {
        None
    }
}

/// Returns true when the person asked to edit the protection settings.
pub fn show(ui: &mut egui::Ui, t: &Theme, live: Option<&Live>, mirror: bool, thermal: &mut Thermal) -> bool {
    let empty = Live::default();
    let l = live.unwrap_or(&empty);
    let s = &l.settings;

    // the connector
    let width = ui.available_width();
    let h = (width * 0.37).clamp(200.0, 300.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, h), Sense::hover());
    let mut order: Vec<usize> = (0..l.pins.len()).collect();
    if mirror {
        order.reverse();
    }
    let limits = if live.is_some() {
        (s.threshold_amps, s.critical_amps, s.catastrophic_amps)
    } else {
        let d = Settings::default();
        (d.threshold_amps, d.critical_amps, d.catastrophic_amps)
    };
    let input = thermal::Input {
        pins: &l.pins,
        order,
        low: low_pins(&l.pins, s),
        limits,
        badge: if live.is_some() { badge(l) } else { ("OFFLINE".into(), Color32::from_gray(150)) },
    };
    thermal.show(ui, rect, &input);
    ui.painter().rect_stroke(rect, 8.0, Stroke::new(1.0_f32, t.border), egui::StrokeKind::Inside);
    ui.add_space(w::GAP);

    // the numbers
    let dash = "—";
    let have = !l.pins.is_empty();
    let hi = l.pins.iter().cloned().fold(None::<(String, f64)>, |m, p| match m {
        Some(m) if m.1 >= p.1 => Some(m),
        _ => Some(p),
    });
    let lo = l.pins.iter().cloned().fold(None::<(String, f64)>, |m, p| match m {
        Some(m) if m.1 <= p.1 => Some(m),
        _ => Some(p),
    });
    let low = low_pins(&l.pins, s);
    let lo_flag = lo.as_ref().is_some_and(|lo| l.pins.iter().position(|p| p.0 == lo.0).is_some_and(|i| low[i]));

    let max_tile = match &hi {
        Some((n, a)) => Tile::new("Max pin", format!("{a:.2} A")).color(amps_color(t, *a, s)).sub(n.clone()),
        None => Tile::new("Max pin", dash),
    };
    let min_tile = match &lo {
        Some((n, a)) => Tile::new("Min pin", format!("{a:.2} A"))
            .color(lo_flag.then_some(t.danger))
            .sub(if lo_flag { format!("{n} · low") } else { n.clone() })
            .sub_color(lo_flag.then_some(t.danger)),
        None => Tile::new("Min pin", dash),
    };
    let delta_tile = match (&hi, &lo) {
        (Some(h), Some(lw)) => {
            let d = h.1 - lw.1;
            let pct = if h.1 > 0.0 { d / h.1 * 100.0 } else { 0.0 };
            Tile::new("Delta", format!("{d:.2} A"))
                .color(l.imbalance_active.then_some(t.pending))
                .sub(format!("{pct:.0} % of max"))
        }
        _ => Tile::new("Delta", dash),
    };
    let amps_sum: f64 = l.pins.iter().map(|p| p.1).sum();
    let avg_v = l.volts.as_ref().filter(|v| !v.is_empty()).map(|v| v.iter().sum::<f64>() / v.len() as f64);
    let total_tile = if have {
        let (watts, approx) = match avg_v {
            Some(v) => (amps_sum * v, ""),
            None => (amps_sum * 12.0, "≈ "),
        };
        Tile::new("Total", format!("{approx}{watts:.0} W")).sub(format!("{amps_sum:.1} A"))
    } else {
        Tile::new("Total", dash)
    };
    w::tiles(ui, t, &[max_tile, min_tile, delta_tile, total_tile]);
    ui.add_space(w::GAP);

    let volts_tile = match (&l.volts, avg_v) {
        (Some(v), Some(avg)) => {
            let min = v.iter().cloned().fold(f64::MAX, f64::min);
            let max = v.iter().cloned().fold(f64::MIN, f64::max);
            let tile = Tile::new("Avg V", format!("{avg:.2} V"));
            if max - min >= 0.05 {
                tile.sub(format!("min {min:.2} V"))
            } else {
                tile.sub(format!("{} pins", v.len()))
            }
        }
        _ => Tile::new("Avg V", dash).sub(if have { "not reported" } else { "" }),
    };
    let (imb, sag) = if live.is_some() {
        (imbalance_tile(t, l), sag_tile(t, l))
    } else {
        (Tile::new("Imbalance", dash), Tile::new("Sag", dash))
    };
    w::tiles(ui, t, &[volts_tile, response_tile(t, l, live.is_some()), imb, sag]);
    ui.add_space(w::GAP);

    let then = if s.response_mode == crate::protocol::ResponseMode::Tiered {
        "limit GPU, then shut down"
    } else {
        "shut down"
    };
    let text = format!(
        "{} A for {} s  ·  {} A for {} s  ·  {} A at once  ·  {then}",
        s.threshold_amps, s.sustained_seconds_required, s.critical_amps, s.critical_seconds, s.catastrophic_amps
    );
    let edit = w::summary_line(ui, t, "Protection", &text);
    ui.add_space(w::GAP);

    history_chart(ui, t, l);
    edit
}

fn response_tile(t: &Theme, l: &Live, have: bool) -> Tile {
    if !have {
        return Tile::new("Response", "—");
    }
    if l.engaged {
        let since = l.engaged_at.map(|a| w::mmss(w::now() - a)).unwrap_or_default();
        return Tile::new("Response", format!("Tier {} · {since}", l.engaged_tier.unwrap_or(1)))
            .color(Some(t.danger))
            .sub("GPU limited");
    }
    let chosen_tiered = l.settings.response_mode == ResponseMode::Tiered;
    let value = if l.effective_shutdown_only { "Shutdown only" } else { "Tiered" };
    let weakened = l.effective_shutdown_only && chosen_tiered;
    let sub = match l.mitigation.as_str() {
        "" => "limit: checking…".to_string(),
        "ready" => {
            if chosen_tiered {
                "GPU limit ready".into()
            } else {
                "no GPU limiting".into()
            }
        }
        why => format!("no limit: {why}"),
    };
    Tile::new("Response", value).color(weakened.then_some(t.pending)).sub(sub).sub_color(weakened.then_some(t.pending))
}

fn imbalance_tile(t: &Theme, l: &Live) -> Tile {
    let s = &l.settings;
    if !s.imbalance_enabled {
        return Tile::new("Imbalance", "Off").color(Some(t.muted()));
    }
    let on_trip = match s.imbalance_response {
        crate::protocol::ImbalanceResponse::Warn => "on trip: warn",
        crate::protocol::ImbalanceResponse::Act => "on trip: act",
    };
    match (l.imbalance_delta, l.imbalance_pct) {
        (Some(_), Some(pct)) if l.imbalance_active => {
            Tile::new("Imbalance", format!("Warning {pct:.0} %")).color(Some(t.pending)).sub(on_trip)
        }
        (Some(_), _) => Tile::new("Imbalance", "Balanced").sub(on_trip),
        _ => Tile::new("Imbalance", "—").sub("waiting for data"),
    }
}

fn sag_tile(t: &Theme, l: &Live) -> Tile {
    let mm = |d: f64| crate::protocol::sag_mm(d, l.card_length_mm);
    match l.sag_status {
        SagStatus::Disabled => Tile::new("Sag", "Off").color(Some(t.muted())),
        SagStatus::Unavailable => Tile::new("Sag", "n/a").color(Some(t.muted())).sub("no G-sensor"),
        SagStatus::Uncalibrated => Tile::new("Sag", "—").sub("not calibrated"),
        SagStatus::Starting => Tile::new("Sag", "—").sub("starting"),
        st => match l.sag_deg {
            Some(d) => {
                let warn = matches!(st, SagStatus::Warning | SagStatus::Serious);
                let tag = match st {
                    SagStatus::Warning => " · warning",
                    SagStatus::Serious => " · serious",
                    _ => "",
                };
                Tile::new("Sag", format!("{d:.1}°"))
                    .color(warn.then_some(if st == SagStatus::Serious { t.danger } else { t.pending }))
                    .sub(format!("{:.1} mm at the end{tag}", mm(d)))
            }
            None => Tile::new("Sag", "—").sub("measuring"),
        },
    }
}

fn history_chart(ui: &mut egui::Ui, t: &Theme, l: &Live) {
    let width = ui.available_width();
    // what the window leaves, so the tab does not scroll at the default size
    let height = ui.available_height().clamp(100.0, 150.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
    let p = ui.painter_at(rect.expand(1.0));
    p.rect(rect, 8.0, t.surface, Stroke::new(1.0_f32, t.border), egui::StrokeKind::Inside);
    w::paint_caption(&p, rect.min + Vec2::new(12.0, 10.0), Align2::LEFT_TOP, "Pin history · 5 min", t.muted());

    // legend, top right
    let mut x = rect.max.x - 12.0;
    for (label, c) in [("min", t.blue), ("max", t.red)] {
        let r = p.text(Pos2::new(x, rect.min.y + 16.0), Align2::RIGHT_CENTER, label, theme::regular(11.5), t.muted());
        x = r.min.x - 6.0;
        p.line_segment([Pos2::new(x - 14.0, r.center().y), Pos2::new(x, r.center().y)], Stroke::new(2.0_f32, c));
        x -= 26.0;
    }

    let s = &l.settings;
    let thr =
        if l.pins.is_empty() && l.history.is_empty() { Settings::default().threshold_amps } else { s.threshold_amps };
    let now = w::now();
    let hist: Vec<&(f64, Vec<f64>)> =
        l.history.iter().filter(|(ts, a)| now - ts <= HISTORY_SECONDS && !a.is_empty()).collect();
    let top = hist.iter().map(|(_, a)| a.iter().cloned().fold(f64::MIN, f64::max) * 1.1).fold(thr * 1.2, f64::max);
    let plot = Rect::from_min_max(rect.min + Vec2::new(40.0, 32.0), rect.max - Vec2::new(14.0, 22.0));
    let px = |ts: f64| plot.max.x - ((now - ts) / HISTORY_SECONDS) as f32 * plot.width();
    let py = |a: f64| plot.max.y - (a / top) as f32 * plot.height();

    for a in [0.0, thr / 2.0, thr] {
        p.text(
            Pos2::new(plot.min.x - 6.0, py(a)),
            Align2::RIGHT_CENTER,
            crate::config::g((a * 10.0).round() / 10.0),
            theme::regular(10.5),
            t.faint(),
        );
        if a > 0.0 && a < thr {
            p.line_segment(
                [Pos2::new(plot.min.x, py(a)), Pos2::new(plot.max.x, py(a))],
                Stroke::new(1.0_f32, t.grid().gamma_multiply(0.6)),
            );
        }
    }
    p.line_segment(
        [Pos2::new(plot.min.x, plot.max.y), Pos2::new(plot.max.x, plot.max.y)],
        Stroke::new(1.0_f32, t.grid()),
    );
    w::dashed_h(&p, plot.min.x, plot.max.x, py(thr), Stroke::new(1.0_f32, t.pending.gamma_multiply(0.8)));
    for m in 0..=5 {
        let ts = now - m as f64 * 60.0;
        let label = if m == 0 { "now".to_string() } else { format!("-{m}m") };
        let anchor = if m == 0 {
            Align2::RIGHT_TOP
        } else if m == 5 {
            Align2::LEFT_TOP
        } else {
            Align2::CENTER_TOP
        };
        p.text(Pos2::new(px(ts), plot.max.y + 5.0), anchor, label, theme::regular(10.5), t.faint());
    }
    if hist.len() < 2 {
        return;
    }
    for (pick_max, c) in [(true, t.red), (false, t.blue)] {
        let pts: Vec<Pos2> = hist
            .iter()
            .map(|(ts, a)| {
                let v = if pick_max {
                    a.iter().cloned().fold(f64::MIN, f64::max)
                } else {
                    a.iter().cloned().fold(f64::MAX, f64::min)
                };
                Pos2::new(px(*ts), py(v))
            })
            .collect();
        p.add(egui::Shape::line(pts, Stroke::new(1.6_f32, c)));
    }
}
