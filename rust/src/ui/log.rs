//! Log tab: the tail of the guard's log, and the flight recorder dumps.

use std::time::{Duration, Instant};

use eframe::egui::{
    self, text::LayoutJob, Align, Align2, Color32, Layout, Pos2, Rect, RichText, Sense, Stroke, TextFormat, Vec2,
};

use super::backend::Job;
use super::theme::{self, Theme};
use super::widgets as w;
use crate::protocol::Response;

const REFRESH: Duration = Duration::from_secs(2);
const FLIGHTS_REFRESH: Duration = Duration::from_secs(10);
const MAX_LINES: usize = 500;

struct Flight {
    name: String,
    /// per pin: amps per sample
    pins: Vec<Vec<f64>>,
    first: String,
    last: String,
    raw: String,
}

#[derive(Default)]
pub struct LogView {
    text: String,
    job: Option<(bool, bool, LayoutJob)>,
    warn_only: bool,
    next: Option<Instant>,
    next_flights: Option<Instant>,
    pending: bool,
    flights: Vec<String>,
    flight: Option<Flight>,
    flight_error: String,
    show_raw: bool,
    error: String,
    flight_pending: bool,
    auto_open: Option<String>,
}

fn level(line: &str) -> Option<&'static str> {
    ["CRITICAL", "ERROR", "WARNING"].into_iter().find(|lv| line.contains(&format!("| {lv} |")))
}

fn parse_flight(name: &str, text: &str) -> Flight {
    let mut lines = text.lines();
    let header: Vec<&str> = lines.next().unwrap_or("").split(',').collect();
    let cols: Vec<usize> = header
        .iter()
        .enumerate()
        .filter(|(_, h)| h.trim().to_ascii_lowercase().starts_with("pin"))
        .map(|(i, _)| i)
        .collect();
    let mut pins = vec![Vec::new(); cols.len()];
    let (mut first, mut last) = (String::new(), String::new());
    for line in lines {
        let f: Vec<&str> = line.split(',').collect();
        if f.len() < header.len().min(2) {
            continue;
        }
        if first.is_empty() {
            first = f[0].to_string();
        }
        last = f[0].to_string();
        for (k, &c) in cols.iter().enumerate() {
            pins[k].push(f.get(c).and_then(|v| v.trim().parse().ok()).unwrap_or(f64::NAN));
        }
    }
    Flight { name: name.to_string(), pins, first, last, raw: text.to_string() }
}

/// "flight_20261109_101500_engaged.csv" -> ("2026-11-09 10:15:00", "engaged")
fn pretty_name(name: &str) -> (String, String) {
    let stem = name.trim_start_matches("flight_").trim_end_matches(".csv");
    let parts: Vec<&str> = stem.splitn(3, '_').collect();
    if parts.len() == 3 && parts[0].len() == 8 && parts[1].len() == 6 {
        let (d, t) = (parts[0], parts[1]);
        (format!("{}-{}-{} {}:{}:{}", &d[..4], &d[4..6], &d[6..], &t[..2], &t[2..4], &t[4..]), parts[2].to_string())
    } else {
        (name.to_string(), String::new())
    }
}

impl LogView {
    /// While the tab is visible: refresh every 2 s.
    pub fn tick(&mut self, send: &mut dyn FnMut(Job)) {
        let now = Instant::now();
        if !self.pending && self.next.is_none_or(|n| now >= n) {
            self.pending = true;
            self.next = Some(now + REFRESH);
            send(Job::LogTail);
        }
        if self.next_flights.is_none_or(|n| now >= n) {
            self.next_flights = Some(now + FLIGHTS_REFRESH);
            send(Job::FlightDumps);
        }
        if let Some(name) = self.auto_open.take() {
            send(Job::ReadFlight(name));
        }
    }

    pub fn force_refresh(&mut self) {
        self.next = None;
        self.next_flights = None;
    }

    pub fn on_done(&mut self, job: &Job, result: &Result<Response, String>) {
        match (job, result) {
            (Job::LogTail, r) => {
                self.pending = false;
                match r {
                    Ok(Response::Text { text }) => {
                        if *text != self.text {
                            self.text = text.clone();
                            self.job = None;
                        }
                        self.error.clear();
                    }
                    Ok(other) => self.error = format!("unexpected answer: {other:?}"),
                    Err(e) => self.error = e.clone(),
                }
            }
            (Job::FlightDumps, Ok(Response::Names { names })) => {
                self.flights = names.clone();
                self.flights.sort();
                self.flights.reverse();
                // show the newest dump straight away
                if self.flight.is_none() && !self.flight_pending {
                    if let Some(first) = self.flights.first() {
                        self.flight_pending = true;
                        self.auto_open = Some(first.clone());
                    }
                }
            }
            (Job::ReadFlight(name), Ok(Response::Text { text })) => {
                self.flight = Some(parse_flight(name, text));
                self.flight_error.clear();
            }
            (Job::ReadFlight(_), Err(e)) => self.flight_error = e.clone(),
            _ => {}
        }
    }

    fn layout(&mut self, t: &Theme) -> LayoutJob {
        let dark = t.dark;
        if let Some((wo, d, job)) = &self.job {
            if *wo == self.warn_only && *d == dark {
                return job.clone();
            }
        }
        let mut lines: Vec<&str> = self.text.lines().collect();
        // a tail usually starts mid-line
        if self.text.len() >= 65536 && !lines.is_empty() {
            lines.remove(0);
        }
        if self.warn_only {
            lines.retain(|l| level(l).is_some());
        }
        let start = lines.len().saturating_sub(MAX_LINES);
        let mut job = LayoutJob::default();
        let font = theme::mono(12.0);
        for line in &lines[start..] {
            let c = match level(line) {
                Some("WARNING") => t.pending,
                Some(_) => t.danger,
                None => t.text,
            };
            job.append(line, 0.0, TextFormat { font_id: font.clone(), color: c, ..Default::default() });
            job.append("\n", 0.0, TextFormat { font_id: font.clone(), color: c, ..Default::default() });
        }
        if lines.is_empty() {
            let msg = if self.text.is_empty() { "Log is empty" } else { "No warnings or errors" };
            job.append(msg, 0.0, TextFormat { font_id: theme::regular(12.5), color: t.muted(), ..Default::default() });
        }
        self.job = Some((self.warn_only, dark, job.clone()));
        job
    }

    pub fn show(&mut self, ui: &mut egui::Ui, t: &Theme, log_path: &str, send: &mut dyn FnMut(Job)) {
        ui.horizontal(|ui| {
            if ui.checkbox(&mut self.warn_only, "Warnings and errors only").changed() {
                self.job = None;
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if w::button(ui, "Refresh").clicked() {
                    self.force_refresh();
                }
                let local = !log_path.is_empty() && std::path::Path::new(log_path).is_file();
                if local && w::button(ui, "Open file").clicked() {
                    super::open_path(log_path);
                }
                if !self.error.is_empty() {
                    ui.label(RichText::new(&self.error).color(t.danger).size(12.0));
                }
            });
        });
        ui.add_space(4.0);

        let flight_h = if self.flights.is_empty() { 0.0 } else { 220.0 };
        let log_h = (ui.available_height() - flight_h - if flight_h > 0.0 { w::GAP } else { 0.0 }).max(140.0);
        let job = self.layout(t);
        w::surface_frame(t).inner_margin(egui::Margin::symmetric(10, 8)).fill(t.field()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::ScrollArea::both()
                .id_salt("log")
                .max_height(log_h - 18.0)
                .min_scrolled_height(log_h - 18.0)
                .auto_shrink([false, false])
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    ui.add(egui::Label::new(job).extend().selectable(true));
                });
        });

        if self.flights.is_empty() {
            return;
        }
        ui.add_space(w::GAP);
        w::card(ui, t, "Flight recorder · the minute before the guard acted", |ui| {
            ui.horizontal_top(|ui| {
                ui.allocate_ui_with_layout(Vec2::new(210.0, 168.0), Layout::top_down(Align::LEFT), |ui| {
                    egui::ScrollArea::vertical().id_salt("flights").max_height(168.0).auto_shrink([false, true]).show(
                        ui,
                        |ui| {
                            for name in self.flights.clone() {
                                let (when, what) = pretty_name(&name);
                                let sel = self.flight.as_ref().is_some_and(|f| f.name == name);
                                let text = RichText::new(format!("{when}  {what}")).size(12.5);
                                if ui.selectable_label(sel, text).clicked() {
                                    send(Job::ReadFlight(name.clone()));
                                }
                            }
                        },
                    );
                });
                ui.separator();
                ui.vertical(|ui| {
                    if !self.flight_error.is_empty() {
                        ui.label(RichText::new(&self.flight_error).color(t.danger));
                    }
                    match &self.flight {
                        None => {
                            ui.add_space(60.0);
                            ui.label(RichText::new("Select a dump").color(t.muted()));
                        }
                        Some(f) if self.show_raw => {
                            if ui.small_button("Chart").clicked() {
                                self.show_raw = false;
                            }
                            egui::ScrollArea::both().id_salt("raw").max_height(140.0).show(ui, |ui| {
                                ui.label(RichText::new(&f.raw).font(theme::mono(11.5)));
                            });
                        }
                        Some(f) => {
                            let toggle = flight_chart(ui, t, f);
                            if toggle {
                                self.show_raw = true;
                            }
                        }
                    }
                });
            });
        });
    }
}

/// Every pin across the dump. Returns true when "CSV" was clicked.
fn flight_chart(ui: &mut egui::Ui, t: &Theme, f: &Flight) -> bool {
    let n = f.pins.first().map_or(0, |p| p.len());
    let peak = f
        .pins
        .iter()
        .enumerate()
        .map(|(i, p)| (i, p.iter().cloned().filter(|v| v.is_finite()).fold(0.0, f64::max)))
        .fold((0, 0.0), |m, x| if x.1 > m.1 { x } else { m });
    let mut raw = false;
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("{n} samples · peak Pin{} {:.2} A", peak.0 + 1, peak.1)).size(12.5));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            raw = ui.small_button("CSV").clicked();
        });
    });
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 132.0), Sense::hover());
    let p = ui.painter_at(rect.expand(1.0));
    let plot = Rect::from_min_max(rect.min + Vec2::new(30.0, 4.0), rect.max - Vec2::new(4.0, 16.0));
    let top = (peak.1 * 1.1).max(1.0);
    p.line_segment([plot.left_bottom(), plot.right_bottom()], Stroke::new(1.0_f32, t.grid()));
    for a in [0.0, top / 2.0, top] {
        let y = plot.max.y - (a / top) as f32 * plot.height();
        p.text(
            Pos2::new(plot.min.x - 5.0, y),
            Align2::RIGHT_CENTER,
            format!("{a:.0}"),
            theme::regular(10.0),
            t.faint(),
        );
    }
    p.text(
        plot.left_bottom() + Vec2::new(0.0, 3.0),
        Align2::LEFT_TOP,
        short_time(&f.first),
        theme::regular(10.0),
        t.faint(),
    );
    p.text(
        plot.right_bottom() + Vec2::new(0.0, 3.0),
        Align2::RIGHT_TOP,
        short_time(&f.last),
        theme::regular(10.0),
        t.faint(),
    );
    let palette = [t.red, t.pending, t.green, t.blue, t.accent, Color32::from_rgb(200, 120, 230)];
    if n >= 2 {
        for (k, series) in f.pins.iter().enumerate() {
            let pts: Vec<Pos2> = series
                .iter()
                .enumerate()
                .filter(|(_, v)| v.is_finite())
                .map(|(i, v)| {
                    Pos2::new(
                        plot.min.x + i as f32 / (n - 1) as f32 * plot.width(),
                        plot.max.y - (*v / top) as f32 * plot.height(),
                    )
                })
                .collect();
            p.add(egui::Shape::line(pts, Stroke::new(1.3_f32, palette[k % palette.len()])));
        }
    }
    raw
}

fn short_time(s: &str) -> String {
    s.split(' ').nth(1).unwrap_or(s).split('.').next().unwrap_or("").to_string()
}
