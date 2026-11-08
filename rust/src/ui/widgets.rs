//! Small building blocks: captions, cards, value tiles, buttons.

use std::sync::Arc;

use eframe::egui::{
    self, text::LayoutJob, Align2, Color32, CornerRadius, FontId, Galley, Painter, Pos2, Rect, Response, RichText,
    Sense, Stroke, StrokeKind, TextFormat, Ui, Vec2,
};

use super::theme::{self, Theme};

pub const GAP: f32 = 8.0;
pub const RADIUS: u8 = 8;

/// "1:05"
pub fn mmss(seconds: f64) -> String {
    let s = seconds.max(0.0) as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

pub fn now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

pub fn caption_job(text: &str, color: Color32) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.append(
        &text.to_uppercase(),
        0.0,
        TextFormat { font_id: theme::semibold(10.5), color, extra_letter_spacing: 0.7, ..Default::default() },
    );
    job
}

/// Small muted UPPERCASE label.
pub fn caption(ui: &mut Ui, t: &Theme, text: &str) -> Response {
    ui.label(caption_job(text, t.muted()))
}

pub fn paint_caption(painter: &Painter, pos: Pos2, anchor: Align2, text: &str, color: Color32) -> Rect {
    let galley = painter.layout_job(caption_job(text, color));
    let rect = anchor.anchor_size(pos, galley.size());
    painter.galley(rect.min, galley, color);
    rect
}

/// Lay out `text` at the first size that fits `max_w`.
pub fn fit(
    painter: &Painter,
    text: &str,
    font: fn(f32) -> FontId,
    sizes: &[f32],
    max_w: f32,
    color: Color32,
) -> Arc<Galley> {
    let mut last = None;
    for &s in sizes {
        let g = painter.layout_no_wrap(text.to_owned(), font(s), color);
        if g.size().x <= max_w {
            return g;
        }
        last = Some(g);
    }
    last.unwrap_or_else(|| painter.layout_no_wrap(text.to_owned(), font(12.0), color))
}

pub fn surface_frame(t: &Theme) -> egui::Frame {
    egui::Frame::new()
        .fill(t.surface)
        .stroke(Stroke::new(1.0_f32, t.border))
        .corner_radius(CornerRadius::same(RADIUS))
        .inner_margin(egui::Margin::same(12))
}

/// A surface with a caption on top.
pub fn card<R>(ui: &mut Ui, t: &Theme, title: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    surface_frame(t)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            caption(ui, t, title);
            ui.add_space(4.0);
            add(ui)
        })
        .inner
}

pub struct Tile {
    pub caption: &'static str,
    pub value: String,
    pub value_color: Option<Color32>,
    pub sub: String,
    pub sub_color: Option<Color32>,
}

impl Tile {
    pub fn new(caption: &'static str, value: impl Into<String>) -> Tile {
        Tile { caption, value: value.into(), value_color: None, sub: String::new(), sub_color: None }
    }
    pub fn color(mut self, c: Option<Color32>) -> Tile {
        self.value_color = c;
        self
    }
    pub fn sub(mut self, s: impl Into<String>) -> Tile {
        self.sub = s.into();
        self
    }
    pub fn sub_color(mut self, c: Option<Color32>) -> Tile {
        self.sub_color = c;
        self
    }
}

pub const TILE_H: f32 = 76.0;

/// A row of equal-width tiles: caption, big value, small line under it.
pub fn tiles(ui: &mut Ui, t: &Theme, row: &[Tile]) {
    let n = row.len().max(1) as f32;
    let w = ((ui.available_width() - GAP * (n - 1.0)) / n).floor();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), TILE_H), Sense::hover());
    let painter = ui.painter_at(rect.expand(1.0));
    for (i, tile) in row.iter().enumerate() {
        let r = Rect::from_min_size(Pos2::new(rect.min.x + i as f32 * (w + GAP), rect.min.y), Vec2::new(w, TILE_H));
        paint_tile(&painter, t, r, tile);
    }
}

pub fn paint_tile(painter: &Painter, t: &Theme, r: Rect, tile: &Tile) {
    painter.rect(r, CornerRadius::same(RADIUS), t.surface, Stroke::new(1.0_f32, t.border), StrokeKind::Inside);
    let pad = 12.0;
    let inner_w = r.width() - 2.0 * pad;
    paint_caption(painter, r.min + Vec2::new(pad, 10.0), Align2::LEFT_TOP, tile.caption, t.muted());
    let vc = tile.value_color.unwrap_or(t.text);
    let g = fit(painter, &tile.value, theme::semibold, &[22.0, 19.0, 16.0, 14.0], inner_w, vc);
    let vy = r.min.y + 27.0 + (25.0 - g.size().y).max(0.0) / 2.0;
    painter.galley(Pos2::new(r.min.x + pad, vy), g, vc);
    if !tile.sub.is_empty() {
        let sc = tile.sub_color.unwrap_or(t.muted());
        let g = fit(painter, &tile.sub, theme::regular, &[11.5, 10.5, 9.5], inner_w, sc);
        painter.galley(Pos2::new(r.min.x + pad, r.max.y - 9.0 - g.size().y), g, sc);
    }
}

/// Accent-filled button for the main action of a view.
pub fn primary_button(ui: &mut Ui, t: &Theme, text: &str, enabled: bool) -> Response {
    let fg = Color32::WHITE;
    let b = egui::Button::new(RichText::new(text).color(if enabled { fg } else { t.muted() }))
        .fill(if enabled { t.accent } else { t.raised() })
        .stroke(Stroke::NONE)
        .min_size(Vec2::new(84.0, 28.0));
    ui.add_enabled(enabled, b)
}

pub fn button(ui: &mut Ui, text: &str) -> Response {
    ui.add(egui::Button::new(text).min_size(Vec2::new(0.0, 28.0)))
}

/// Dashed horizontal line.
pub fn dashed_h(painter: &Painter, x0: f32, x1: f32, y: f32, stroke: Stroke) {
    painter.extend(egui::Shape::dashed_line(&[Pos2::new(x0, y), Pos2::new(x1, y)], stroke, 5.0, 4.0));
}

pub fn rgb(c: [u8; 3]) -> Color32 {
    Color32::from_rgb(c[0], c[1], c[2])
}
