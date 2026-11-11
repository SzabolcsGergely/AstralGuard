//! Overclock tab: undervolt (a V/F curve offset), power limit and memory offset.
//!
//! The form always sends the whole `oc_*` group (Apply now saves it, then the service writes
//! the ticked settings to the GPU and puts the unticked, previously applied ones back). The
//! live preview runs the service's own `plan_undervolt` on the card's stock curve, so the
//! offset shown here is the one an apply would write.

use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, Align2, Color32, Layout, Pos2, Rect, RichText, Sense, Shape, Stroke, TextEdit, Ui, Vec2,
};

use super::backend::Job;
use super::theme::{self, Theme};
use super::widgets as w;
use crate::config::g;
use crate::overclock::curve::{plan_undervolt, RawPoint};
use crate::protocol::{Live, OcReport, OcStatus, Response, Settings, SettingsPatch};

/// The driver's per-point offset limit (the service uses the same figure).
const MAX_DELTA_MHZ: i32 = 1000;
/// How often the tab asks the service for the GPU's state while it is visible.
const POLL: Duration = Duration::from_secs(2);
/// How often the tiles' readings are asked for.
const LIVE_POLL: Duration = Duration::from_secs(1);
/// The tooltip on "Only under load".
const LOAD_ONLY_WHY: &str = "A raised XBAR clock keeps the core from dropping to its idle clock, so the card draws more at the desktop. With this on, the XBAR and SYS offsets are applied only after the GPU has drawn at least the given power for the given time (a game, a render), and put back after it has drawn less for the idle time.";

// Graph axes.
const MV: (f32, f32) = (700.0, 1240.0);
const MHZ: (f32, f32) = (0.0, 3400.0);

pub struct OverclockView {
    uv_on: bool,
    power_on: bool,
    mem_on: bool,
    delay: String,
    from: String,
    to: String,
    target: String,
    power: String,
    mem: String,
    xbar_on: bool,
    sys_on: bool,
    xbar: String,
    sys: String,
    load_only: bool,
    load_w: String,
    load_s: String,
    idle_s: String,
    base: Option<Settings>,
    status: Option<OcStatus>,
    status_err: Option<String>,
    report: Option<OcReport>,
    error: Option<String>,
    busy: bool,
    reload: bool,
    pending: bool,
    next_poll: Instant,
    /// Height of everything in the tab but the graph, measured on the last frame.
    fixed_h: f32,
    /// The tiles' readings, polled faster than `status` (no NVAPI in them).
    live: Option<crate::protocol::OcLive>,
    live_pending: bool,
    next_live: Instant,
    /// The tallest of the three bottom cards' content on the last frame: all three get it.
    cards_h: f32,
}

/// What an undervolt with the form's numbers would do on this card's curve.
#[derive(Debug, Clone, PartialEq)]
struct Preview {
    offset_mhz: i32,
    /// (mV, MHz) of every core point after the plan.
    curve: Vec<(f32, f32)>,
}

fn raw_points(status: &OcStatus) -> Vec<RawPoint> {
    status
        .stock_curve
        .iter()
        .map(|&(mv, mhz)| RawPoint {
            volt_uv: (mv * 1000.0).round() as u32,
            freq_khz: (mhz * 1000.0).round() as u32,
            delta_khz: 0,
            memory: false,
        })
        .collect()
}

fn preview(status: &OcStatus, from: f64, to: f64, target: f64) -> Result<Preview, String> {
    let points = raw_points(status);
    let plan = plan_undervolt(&points, from, to, target, MAX_DELTA_MHZ)?;
    let curve = plan
        .deltas_khz
        .iter()
        .map(|&(i, d)| {
            let p = &points[i];
            (p.volt_uv as f32 / 1000.0, (p.freq_khz as i64 + d as i64) as f32 / 1000.0)
        })
        .collect();
    Ok(Preview { offset_mhz: plan.offset_mhz, curve })
}

fn offset_text(p: &Preview, from: f64, to: f64) -> String {
    format!("offset {:+} MHz on {}–{} mV (limit ±{MAX_DELTA_MHZ})", p.offset_mhz, g(from), g(to))
}

fn parse(text: &str) -> Option<f64> {
    text.trim().replace(',', ".").parse::<f64>().ok().filter(|v| v.is_finite())
}

/// The `oc_*` group of a full settings value, for comparing with the form.
fn oc_patch(s: &Settings) -> SettingsPatch {
    SettingsPatch {
        oc_uv_enabled: Some(s.oc_uv_enabled),
        oc_uv_from_mv: Some(s.oc_uv_from_mv),
        oc_uv_to_mv: Some(s.oc_uv_to_mv),
        oc_uv_target_mhz: Some(s.oc_uv_target_mhz),
        oc_power_enabled: Some(s.oc_power_enabled),
        oc_power_w: Some(s.oc_power_w),
        oc_mem_enabled: Some(s.oc_mem_enabled),
        oc_mem_offset_mhz: Some(s.oc_mem_offset_mhz),
        oc_apply_delay_seconds: Some(s.oc_apply_delay_seconds),
        oc_xbar_enabled: Some(s.oc_xbar_enabled),
        oc_xbar_offset_mhz: Some(s.oc_xbar_offset_mhz),
        oc_sys_enabled: Some(s.oc_sys_enabled),
        oc_sys_offset_mhz: Some(s.oc_sys_offset_mhz),
        oc_clk_load_only: Some(s.oc_clk_load_only),
        oc_clk_load_watts: Some(s.oc_clk_load_watts),
        oc_clk_load_seconds: Some(s.oc_clk_load_seconds),
        oc_clk_idle_seconds: Some(s.oc_clk_idle_seconds),
        ..Default::default()
    }
}

impl Default for OverclockView {
    fn default() -> Self {
        let mut v = OverclockView {
            uv_on: false,
            power_on: false,
            mem_on: false,
            delay: String::new(),
            from: String::new(),
            to: String::new(),
            target: String::new(),
            power: String::new(),
            mem: String::new(),
            xbar_on: false,
            sys_on: false,
            xbar: String::new(),
            sys: String::new(),
            load_only: true,
            load_w: String::new(),
            load_s: String::new(),
            idle_s: String::new(),
            base: None,
            status: None,
            status_err: None,
            report: None,
            error: None,
            busy: false,
            reload: true,
            pending: false,
            next_poll: Instant::now(),
            fixed_h: 0.0,
            live: None,
            live_pending: false,
            next_live: Instant::now(),
            cards_h: 0.0,
        };
        v.fill(&Settings::default());
        v
    }
}

impl OverclockView {
    fn fill(&mut self, s: &Settings) {
        self.uv_on = s.oc_uv_enabled;
        self.power_on = s.oc_power_enabled;
        self.mem_on = s.oc_mem_enabled;
        self.delay = g(s.oc_apply_delay_seconds);
        self.from = g(s.oc_uv_from_mv);
        self.to = g(s.oc_uv_to_mv);
        self.target = g(s.oc_uv_target_mhz);
        self.power = g(s.oc_power_w);
        self.mem = g(s.oc_mem_offset_mhz);
        self.xbar_on = s.oc_xbar_enabled;
        self.sys_on = s.oc_sys_enabled;
        self.xbar = g(s.oc_xbar_offset_mhz);
        self.sys = g(s.oc_sys_offset_mhz);
        self.load_only = s.oc_clk_load_only;
        self.load_w = g(s.oc_clk_load_watts);
        self.load_s = g(s.oc_clk_load_seconds);
        self.idle_s = g(s.oc_clk_idle_seconds);
    }

    fn load(&mut self, s: &Settings) {
        self.fill(s);
        self.base = Some(s.clone());
        self.reload = false;
    }

    /// The whole `oc_*` group from the form, or what could not be read as a number.
    fn form_patch(&self) -> Result<SettingsPatch, Vec<String>> {
        let mut errors = Vec::new();
        let mut num = |label: &str, text: &str| {
            let v = parse(text);
            if v.is_none() {
                errors.push(format!("{label}: '{}' is not a number", text.trim()));
            }
            v
        };
        let delay = num("Apply delay", &self.delay);
        let from = num("From", &self.from);
        let to = num("To", &self.to);
        let target = num("Target", &self.target);
        let power = num("Power limit", &self.power);
        let mem = num("Memory offset", &self.mem);
        let xbar = num("XBAR", &self.xbar);
        let sys = num("SYS", &self.sys);
        let load_w = num("Load", &self.load_w);
        let load_s = num("Load time", &self.load_s);
        let idle_s = num("Idle time", &self.idle_s);
        if !errors.is_empty() {
            return Err(errors);
        }
        Ok(SettingsPatch {
            oc_uv_enabled: Some(self.uv_on),
            oc_uv_from_mv: from,
            oc_uv_to_mv: to,
            oc_uv_target_mhz: target,
            oc_power_enabled: Some(self.power_on),
            oc_power_w: power,
            oc_mem_enabled: Some(self.mem_on),
            oc_mem_offset_mhz: mem,
            oc_apply_delay_seconds: delay,
            oc_xbar_enabled: Some(self.xbar_on),
            oc_xbar_offset_mhz: xbar,
            oc_sys_enabled: Some(self.sys_on),
            oc_sys_offset_mhz: sys,
            oc_clk_load_only: Some(self.load_only),
            oc_clk_load_watts: load_w,
            oc_clk_load_seconds: load_s,
            oc_clk_idle_seconds: idle_s,
            ..Default::default()
        })
    }

    fn apply_job(&self) -> Result<Job, Vec<String>> {
        self.form_patch().map(Job::ApplyOverclock)
    }

    fn dirty(&self) -> bool {
        match (self.form_patch(), &self.base) {
            (Err(_), _) => true,
            (Ok(p), Some(b)) => p != oc_patch(b),
            (Ok(_), None) => false,
        }
    }

    /// Keep the form in step with the saved settings while the user is not editing.
    fn sync(&mut self, saved: &Settings) {
        if self.reload || (self.base.as_ref() != Some(saved) && !self.dirty()) {
            self.load(saved);
        }
    }

    /// Time to ask the service for the GPU's state (the app calls this only while the tab is
    /// visible). At most one request in flight, and none while an apply is running.
    pub fn poll_due(&mut self) -> bool {
        if self.pending || self.busy || Instant::now() < self.next_poll {
            return false;
        }
        self.pending = true;
        self.next_poll = Instant::now() + POLL;
        true
    }

    /// Time to ask for the tiles' readings (the app calls this only while the tab is visible).
    pub fn live_due(&mut self) -> bool {
        if self.live_pending || Instant::now() < self.next_live {
            return false;
        }
        self.live_pending = true;
        self.next_live = Instant::now() + LIVE_POLL;
        true
    }

    /// The tab was just opened: ask at once.
    pub fn opened(&mut self) {
        self.next_poll = Instant::now();
        self.next_live = Instant::now();
    }

    pub fn on_done(&mut self, job: &Job, r: &Result<Response, String>) {
        match (job, r) {
            (Job::OverclockLive, Ok(Response::OverclockLive { live })) => {
                self.live_pending = false;
                self.live = Some(live.clone());
            }
            (Job::OverclockLive, _) => {
                self.live_pending = false;
                self.live = None;
            }
            (Job::OverclockStatus, Ok(Response::OverclockStatus { status })) => {
                self.pending = false;
                self.status = Some(status.clone());
                self.status_err = None;
            }
            (Job::OverclockStatus, Ok(other)) => {
                self.pending = false;
                self.status_err = Some(format!("Unexpected answer: {other:?}"));
            }
            (Job::OverclockStatus, Err(e)) => {
                self.pending = false;
                self.status_err = Some(e.clone());
            }
            (Job::ApplyOverclock(_), Ok(Response::Overclock { report })) => {
                self.busy = false;
                self.report = Some(report.clone());
                self.error = None;
                self.reload = true;
                self.next_poll = Instant::now();
            }
            (Job::ApplyOverclock(_), Ok(other)) => {
                self.busy = false;
                self.error = Some(format!("Unexpected answer: {other:?}"));
            }
            (Job::ApplyOverclock(_), Err(e)) => {
                self.busy = false;
                self.error = Some(e.clone());
                self.next_poll = Instant::now();
            }
            _ => {}
        }
    }

    pub fn ui(&mut self, ui: &mut Ui, t: &Theme, live: Option<&Live>) -> Option<Job> {
        if let Some(l) = live {
            self.sync(&l.settings);
        }
        let mut job = None;
        let Some(status) = self.status.clone() else {
            let text = self.status_err.as_deref().unwrap_or("Reading the GPU…");
            let color = if self.status_err.is_some() { t.danger } else { t.muted() };
            ui.label(RichText::new(text).color(color));
            return None;
        };
        if !status.available {
            w::card(ui, t, "Overclock", |ui| {
                let reason =
                    if status.reason.is_empty() { "The GPU's overclock is not available." } else { &status.reason };
                ui.label(RichText::new(reason).color(t.danger));
            });
            return None;
        }
        let patch = self.form_patch();
        let dirty = self.dirty();
        // The graph takes what the rest of the tab leaves of the window, so the tab does not
        // scroll at the default size (the rest is measured on the frame before).
        let (top, room) = (ui.cursor().top(), ui.available_height());
        let graph_h = (room - self.fixed_h).clamp(GRAPH_MIN_H, GRAPH_H);

        ui.horizontal(|ui| {
            ui.set_min_height(32.0);
            label_row_text(ui, t, "Apply delay");
            self.delay_field(ui, t);
            ui.label(RichText::new("s after the guard starts").color(t.muted()).size(12.0));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let can = live.is_some() && patch.is_ok() && !self.busy;
                if w::primary_button(ui, t, "Apply now", can).clicked() {
                    if let Ok(j) = self.apply_job() {
                        self.busy = true;
                        self.error = None;
                        self.report = None;
                        job = Some(j);
                    }
                }
                if ui
                    .add_enabled(dirty && !self.busy, egui::Button::new("Revert").min_size(Vec2::new(0.0, 28.0)))
                    .clicked()
                {
                    if let Some(b) = self.base.clone() {
                        self.fill(&b);
                    }
                    self.error = None;
                }
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    if self.busy {
                        ui.spinner();
                        ui.label(RichText::new("Applying…").color(t.muted()));
                    } else if let Some(e) = &self.error {
                        ui.add(egui::Label::new(RichText::new(e.as_str()).color(t.danger)).truncate());
                    } else if let Err(errs) = &patch {
                        ui.add(egui::Label::new(RichText::new(errs.join("; ")).color(t.danger)).truncate());
                    } else if dirty {
                        ui.label(RichText::new("Unsaved changes").color(t.muted()));
                    }
                });
            });
        });
        ui.add_space(4.0);

        live_tiles(ui, t, &status, live, self.live.as_ref());
        ui.add_space(w::GAP);

        let deferred = self.report.as_ref().is_some_and(|r| r.deferred);
        self.undervolt_card(ui, t, &status, deferred, graph_h);
        ui.add_space(w::GAP);
        // One height for the three cards: the tallest content of the last frame.
        let target = self.cards_h;
        let mut tallest = 0.0f32;
        ui.columns(3, |cols| {
            let [a, b, c] = cols else { return };
            // Pad a card by what it is short of the tallest one.
            let mut fit = |ui: &mut Ui, natural: f32| {
                tallest = tallest.max(natural);
                // add_space brings its own item spacing with it
                let pad = target - natural - ui.spacing().item_spacing.y;
                if pad > 0.0 {
                    ui.add_space(pad);
                }
            };
            a.with_layout(Layout::top_down(Align::LEFT), |ui| self.power_card(ui, t, &status, deferred, &mut fit));
            b.with_layout(Layout::top_down(Align::LEFT), |ui| self.memory_card(ui, t, &status, deferred, &mut fit));
            c.with_layout(Layout::top_down(Align::LEFT), |ui| self.clocks_card(ui, t, &status, deferred, &mut fit));
        });
        if (tallest - self.cards_h).abs() > 0.5 {
            self.cards_h = tallest;
            ui.ctx().request_repaint();
        }
        if let Some(e) = &self.status_err {
            ui.add_space(4.0);
            ui.label(RichText::new(format!("GPU state: {e}")).color(t.pending).size(12.0));
        }
        self.fixed_h = ui.cursor().top() - top - graph_h;
        job
    }

    fn delay_field(&mut self, ui: &mut Ui, t: &Theme) {
        let base = self.base.as_ref().map(|b| b.oc_apply_delay_seconds);
        field(ui, t, &mut self.delay, base);
    }

    fn undervolt_card(&mut self, ui: &mut Ui, t: &Theme, status: &OcStatus, deferred: bool, graph_h: f32) {
        w::card(ui, t, "Voltage/frequency curve", |ui| {
            let b = self.base.clone().unwrap_or_default();
            let c = changed_color(t, self.uv_on != b.oc_uv_enabled);
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.uv_on, RichText::new("Enabled").color(c));
                ui.add_space(16.0);
                ui.label(RichText::new("From").color(t.text));
                field(ui, t, &mut self.from, Some(b.oc_uv_from_mv));
                ui.label(RichText::new("mV").color(t.muted()).size(12.0));
                ui.add_space(8.0);
                ui.label(RichText::new("To").color(t.text));
                field(ui, t, &mut self.to, Some(b.oc_uv_to_mv));
                ui.label(RichText::new("mV").color(t.muted()).size(12.0));
                ui.add_space(8.0);
                ui.label(RichText::new("Target").color(t.text));
                field(ui, t, &mut self.target, Some(b.oc_uv_target_mhz));
                ui.label(RichText::new("MHz").color(t.muted()).size(12.0));
            });
            let form = (parse(&self.from), parse(&self.to), parse(&self.target));
            let plan = match form {
                (Some(f), Some(to), Some(tg)) => Some((f, to, preview(status, f, to, tg))),
                _ => None,
            };
            // The plan and what is applied, on one line.
            ui.horizontal_wrapped(|ui| {
                match &plan {
                    Some((f, to, Ok(p))) => {
                        ui.label(RichText::new(offset_text(p, *f, *to)).color(t.text));
                    }
                    Some((_, _, Err(e))) => {
                        ui.label(RichText::new(e.as_str()).color(t.danger));
                    }
                    None => {
                        ui.label(RichText::new("Enter numbers in From, To and Target").color(t.danger));
                    }
                }
                ui.label(RichText::new(format!("· {}", applied_curve_text(status))).color(t.muted()).size(12.0));
            });
            if status.foreign_core_offset_mhz != 0 {
                ui.label(
                    RichText::new(format!(
                        "Another tool set a graphics clock offset of {:+} MHz; the undervolt is refused until it is removed",
                        status.foreign_core_offset_mhz
                    ))
                    .color(t.danger)
                    .size(12.0),
                );
            }
            if status.curve_compatible == Some(false) {
                ui.label(
                    RichText::new("The curve write check failed on this driver; the undervolt is refused")
                        .color(t.danger)
                        .size(12.0),
                );
            }
            self.result_lines(ui, t, "undervolt", self.uv_on, deferred);
            ui.add_space(6.0);
            let p = plan.as_ref().and_then(|(_, _, r)| r.as_ref().ok());
            graph(ui, t, status, p, self.uv_on, graph_h);
        });
    }

    fn power_card(
        &mut self,
        ui: &mut Ui,
        t: &Theme,
        status: &OcStatus,
        deferred: bool,
        fit: &mut dyn FnMut(&mut Ui, f32),
    ) {
        w::card(ui, t, "Power limit", |ui| {
            let top = ui.min_rect().top();
            let b = self.base.clone().unwrap_or_default();
            let c = changed_color(t, self.power_on != b.oc_power_enabled);
            ui.checkbox(&mut self.power_on, RichText::new("Enabled").color(c));
            ui.horizontal(|ui| {
                field(ui, t, &mut self.power, Some(b.oc_power_w));
                ui.label(RichText::new("W").color(t.muted()).size(12.0));
            });
            let range = status.power_range_w.map(|(lo, hi)| (lo as f64, hi as f64));
            let bad = matches!((parse(&self.power), range), (Some(v), Some((lo, hi))) if v < lo || v > hi);
            match status.power_range_w {
                Some((lo, hi)) => range_line(ui, t, &format!("Allowed {lo}–{hi} W"), bad),
                None => range_line(ui, t, "Allowed range not read", false),
            }
            self.result_lines(ui, t, "power", self.power_on, deferred);
            fit(ui, ui.min_rect().bottom() - top);
        });
    }

    fn memory_card(
        &mut self,
        ui: &mut Ui,
        t: &Theme,
        status: &OcStatus,
        deferred: bool,
        fit: &mut dyn FnMut(&mut Ui, f32),
    ) {
        w::card(ui, t, "Memory offset", |ui| {
            let top = ui.min_rect().top();
            let b = self.base.clone().unwrap_or_default();
            let c = changed_color(t, self.mem_on != b.oc_mem_enabled);
            ui.checkbox(&mut self.mem_on, RichText::new("Enabled").color(c));
            ui.horizontal(|ui| {
                field(ui, t, &mut self.mem, Some(b.oc_mem_offset_mhz));
                ui.label(RichText::new("MHz").color(t.muted()).size(12.0));
            });
            let range = status.mem_range_ab.map(|(lo, hi)| (lo as f64, hi as f64));
            let bad = matches!((parse(&self.mem), range), (Some(v), Some((lo, hi))) if v < lo || v > hi);
            match status.mem_range_ab {
                Some((lo, hi)) => range_line(ui, t, &format!("Allowed {lo:+} to {hi:+} MHz"), bad),
                None => range_line(ui, t, "Allowed range not read", false),
            }
            self.result_lines(ui, t, "memory", self.mem_on, deferred);
            fit(ui, ui.min_rect().bottom() - top);
        });
    }

    fn clocks_card(
        &mut self,
        ui: &mut Ui,
        t: &Theme,
        status: &OcStatus,
        deferred: bool,
        fit: &mut dyn FnMut(&mut Ui, f32),
    ) {
        w::card(ui, t, "XBAR / SYS offset · MHz", |ui| {
            let top = ui.min_rect().top();
            let b = self.base.clone().unwrap_or_default();
            let available = status.xbar_khz.is_some() || status.sys_khz.is_some();
            ui.horizontal(|ui| {
                for (on, text, base_on, base_v, name) in [
                    (&mut self.xbar_on, &mut self.xbar, b.oc_xbar_enabled, b.oc_xbar_offset_mhz, "XBAR"),
                    (&mut self.sys_on, &mut self.sys, b.oc_sys_enabled, b.oc_sys_offset_mhz, "SYS"),
                ] {
                    let c = changed_color(t, *on != base_on);
                    ui.add_enabled(available || *on, egui::Checkbox::new(on, RichText::new(name).color(c)));
                    field_w(ui, t, text, Some(base_v), 36.0);
                }
            });
            let c = changed_color(t, self.load_only != b.oc_clk_load_only);
            ui.checkbox(&mut self.load_only, RichText::new("Only under load").color(c)).on_hover_text(LOAD_ONLY_WHY);
            ui.add_enabled_ui(self.load_only, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 3.0;
                    field_w(ui, t, &mut self.load_w, Some(b.oc_clk_load_watts), 36.0);
                    ui.label(RichText::new("W for").color(t.muted()).size(12.0));
                    field_w(ui, t, &mut self.load_s, Some(b.oc_clk_load_seconds), 26.0);
                    ui.label(RichText::new("s · idle").color(t.muted()).size(12.0));
                    field_w(ui, t, &mut self.idle_s, Some(b.oc_clk_idle_seconds), 26.0);
                    ui.label(RichText::new("s").color(t.muted()).size(12.0));
                });
            });
            self.result_lines(ui, t, "xbar", self.xbar_on, deferred);
            self.result_lines(ui, t, "sys", self.sys_on, deferred);
            fit(ui, ui.min_rect().bottom() - top);
        });
    }

    /// What the last Apply now said about one setting.
    fn result_lines(&self, ui: &mut Ui, t: &Theme, setting: &str, ticked: bool, deferred: bool) {
        let Some(report) = &self.report else { return };
        if deferred && ticked {
            ui.label(RichText::new("Waiting: the GPU is limited by the guard").color(t.pending).size(12.0));
        }
        for r in report.results.iter().filter(|r| r.setting == setting) {
            let color = if r.ok { t.green } else { t.danger };
            ui.add(egui::Label::new(RichText::new(r.message.as_str()).color(color).size(12.0)).wrap());
        }
    }
}

/// What the card runs now: two rows of tiles, from the fast readings (`OcLive`), the
/// connector's pins and, for the voltage and the offsets, the slower status.
fn live_tiles(ui: &mut Ui, t: &Theme, status: &OcStatus, live: Option<&Live>, oc: Option<&crate::protocol::OcLive>) {
    use w::Tile;
    let dash = || "—".to_string();
    let mhz = |v: Option<u32>| v.map_or_else(dash, |v| format!("{v} MHz"));
    let o = oc.cloned().unwrap_or_default();
    let limited = live.is_some_and(|l| l.engaged);

    // Row 1, power: what the card draws, how hot and at what voltage, and the connector.
    let power = Tile::new("GPU power", o.power_draw_w.map_or_else(dash, |w| format!("{w} W")));
    let power = match (limited, o.power_limit_w) {
        (true, _) => power.sub("limited by the guard").sub_color(Some(t.pending)),
        (false, Some(w)) => power.sub(format!("limit {w} W")),
        _ => power,
    };
    let temp = Tile::new("GPU temperature", o.temp_c.map_or_else(dash, |c| format!("{c} °C")))
        .color(o.temp_c.and_then(|c| match c {
            90.. => Some(t.danger),
            83.. => Some(t.pending),
            _ => None,
        }))
        .sub("core");
    let volts = Tile::new("Core voltage", status.live_mv.map_or_else(dash, |mv| format!("{mv:.0} mV")));
    let pin = match live.filter(|l| !l.pins.is_empty()) {
        Some(l) => {
            let total: f64 = l.pins.iter().map(|p| p.1).sum();
            Tile::new("Max pin", format!("{:.2} A", l.max_val))
                .color(super::live::amps_color(t, l.max_val, &l.settings))
                .sub(format!("{} · total {total:.1} A", l.max_label))
        }
        None => Tile::new("Max pin", dash()),
    };
    w::tiles(ui, t, &[power, temp, volts, pin]);
    ui.add_space(w::GAP);

    // Row 2, clocks: each as measured, with the offset applied to it.
    let offset = |khz: Option<(i32, i32, i32)>| match khz {
        Some((0, _, _)) => "no offset".to_string(),
        Some((k, _, _)) => format!("offset {:+} MHz", k / 1000),
        None => "not available".to_string(),
    };
    // The load gate's state goes with the offsets, when "only under load" is on.
    let gate = match live.map(|l| l.settings.oc_clk_load_only) {
        Some(true) if status.clk_loaded => " · under load",
        Some(true) => " · idle",
        _ => "",
    };
    let core = Tile::new("Core clock", mhz(o.core_mhz)).sub(match status.uv_offset_mhz {
        Some(off) if status.managed.iter().any(|m| m == "undervolt") => format!("curve {off:+} MHz"),
        _ => "stock curve".to_string(),
    });
    let xbar = Tile::new("XBAR clock", mhz(o.xbar_mhz)).sub(offset(status.xbar_khz) + gate);
    let sys = Tile::new("SYS clock", mhz(o.sys_mhz)).sub(offset(status.sys_khz));
    let mem = Tile::new("Memory clock", mhz(o.mem_mhz)).sub(match status.mem_ab {
        Some(0) => "no offset".to_string(),
        Some(ab) => format!("offset {ab:+} MHz"),
        None => String::new(),
    });
    w::tiles(ui, t, &[core, xbar, sys, mem]);
}

/// "Applied now: ..." for the curve: the stock curve, or the offset where it differs.
fn applied_curve_text(status: &OcStatus) -> String {
    if status.stock_curve.is_empty() || status.current_curve.len() != status.stock_curve.len() {
        return "Applied now: curve not read".into();
    }
    let diffs: Vec<f32> = status.current_curve.iter().zip(&status.stock_curve).map(|(c, s)| c.1 - s.1).collect();
    let top = diffs.iter().cloned().fold(0.0_f32, |a, d| if d.abs() > a.abs() { d } else { a });
    if top.abs() < 0.5 {
        "Applied now: stock curve".into()
    } else {
        format!("Applied now: curve changed (largest step {:+} MHz)", top.round() as i32)
    }
}

/// The curve's highest frequency and the first voltage that reaches it, as (mV, MHz).
fn curve_peak(points: &[(f32, f32)]) -> Option<(f32, f32)> {
    let mut best: Option<(f32, f32)> = None;
    for &(mv, mhz) in points {
        if best.is_none_or(|(_, top)| mhz > top) {
            best = Some((mv, mhz));
        }
    }
    best
}

fn changed_color(t: &Theme, changed: bool) -> Color32 {
    if changed {
        t.accent
    } else {
        t.text
    }
}

fn range_line(ui: &mut Ui, t: &Theme, text: &str, bad: bool) {
    ui.label(RichText::new(text).color(if bad { t.danger } else { t.muted() }).size(12.0));
}

fn label_row_text(ui: &mut Ui, t: &Theme, label: &str) {
    ui.label(RichText::new(label).color(t.text));
}

/// A number box: danger when it does not parse, accent when it differs from the saved value.
fn field(ui: &mut Ui, t: &Theme, text: &mut String, base: Option<f64>) {
    field_w(ui, t, text, base, 62.0);
}

fn field_w(ui: &mut Ui, t: &Theme, text: &mut String, base: Option<f64>, width: f32) {
    let parsed = parse(text);
    let color = match (parsed, base) {
        (None, _) => t.danger,
        (Some(v), Some(b)) if (v - b).abs() > 1e-9 => t.accent,
        _ => t.text,
    };
    ui.add(
        TextEdit::singleline(text)
            // `width` is the text's; egui counts the margin in (since 0.37)
            .desired_width(width + FIELD_MARGIN.sum().x)
            .align(egui::Align2::RIGHT_CENTER)
            .text_color(color)
            .margin(FIELD_MARGIN),
    );
}

/// The number fields' padding, shared with the Protection tab's fields.
pub(super) const FIELD_MARGIN: egui::Margin = egui::Margin::symmetric(6, 3);

const GRAPH_H: f32 = 200.0;
const GRAPH_MIN_H: f32 = 90.0;

/// A small accent label with a surface-colored backing.
fn tag(p: &egui::Painter, t: &Theme, pos: Pos2, align: Align2, text: String) {
    let galley = p.layout_no_wrap(text, theme::regular(10.0), t.accent);
    let r = align.anchor_size(pos, galley.size());
    p.rect_filled(r.expand(1.5), 2.0, t.surface);
    p.galley(r.min, galley, t.accent);
}

/// The V/F curves: stock dashed gray, the preview solid accent, the card's live point a dot.
fn graph(ui: &mut Ui, t: &Theme, status: &OcStatus, preview: Option<&Preview>, ticked: bool, height: f32) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
    let p = ui.painter().clone();
    let plot = Rect::from_min_max(
        Pos2::new(rect.min.x + 40.0, rect.min.y + 8.0),
        Pos2::new(rect.max.x - 10.0, rect.max.y - 18.0),
    );
    let at = |mv: f32, mhz: f32| {
        Pos2::new(
            plot.min.x + (mv - MV.0) / (MV.1 - MV.0) * plot.width(),
            plot.max.y - (mhz - MHZ.0) / (MHZ.1 - MHZ.0) * plot.height(),
        )
    };
    let label = theme::regular(10.0);
    for mhz in [0.0_f32, 1000.0, 2000.0, 3000.0] {
        let y = at(MV.0, mhz).y;
        p.line_segment([Pos2::new(plot.min.x, y), Pos2::new(plot.max.x, y)], Stroke::new(1.0_f32, t.grid()));
        p.text(Pos2::new(plot.min.x - 6.0, y), Align2::RIGHT_CENTER, format!("{mhz:.0}"), label.clone(), t.faint());
    }
    for mv in [700.0_f32, 800.0, 900.0, 1000.0, 1100.0, 1200.0] {
        let x = at(mv, 0.0).x;
        p.line_segment([Pos2::new(x, plot.min.y), Pos2::new(x, plot.max.y)], Stroke::new(1.0_f32, t.grid()));
        p.text(Pos2::new(x, plot.max.y + 3.0), Align2::CENTER_TOP, format!("{mv:.0}"), label.clone(), t.faint());
    }
    p.text(Pos2::new(plot.max.x, plot.max.y + 3.0), Align2::RIGHT_TOP, "mV", label.clone(), t.muted());
    // above the axis labels, clear of the curves and the target label
    p.text(Pos2::new(rect.min.x, rect.min.y), Align2::LEFT_TOP, "MHz", label.clone(), t.muted());

    let clipped = p.with_clip_rect(plot.expand(6.0));
    let stock: Vec<Pos2> = status.stock_curve.iter().map(|&(mv, mhz)| at(mv, mhz)).collect();
    let gray = t.muted();
    clipped.extend(Shape::dashed_line(&stock, Stroke::new(1.5_f32, gray), 6.0, 4.0));
    let accent = if ticked { t.accent } else { theme::lerp(t.accent, t.surface, 0.5) };
    if let Some(pv) = preview {
        let pts: Vec<Pos2> = pv.curve.iter().map(|&(mv, mhz)| at(mv, mhz)).collect();
        clipped.add(Shape::line(pts, Stroke::new(2.0_f32, accent)));
    }
    if let (Some(mv), Some(mhz)) = (status.live_mv, status.live_mhz) {
        let c = at(mv, mhz);
        clipped.circle(c, 5.0, t.surface, Stroke::new(2.0_f32, t.text));
        clipped.circle_filled(c, 2.5, t.text);
    }

    // The previewed curve's top: its frequency and the voltage that first reaches it. When the
    // undervolt is off or refused, the stock curve's top.
    let peak = match preview {
        Some(pv) if ticked => curve_peak(&pv.curve),
        _ => curve_peak(&status.stock_curve),
    };
    if let Some((mv, mhz)) = peak {
        let c = at(mv.clamp(MV.0, MV.1), mhz.clamp(MHZ.0, MHZ.1));
        let guide = Stroke::new(1.0_f32, t.muted());
        let inside = p.with_clip_rect(plot);
        inside.extend(Shape::dashed_line(&[Pos2::new(plot.min.x, c.y), c], guide, 4.0, 3.0));
        inside.extend(Shape::dashed_line(&[Pos2::new(c.x, plot.max.y), c], guide, 4.0, 3.0));
        inside.circle(c, 4.0, t.surface, Stroke::new(1.5_f32, accent));
        inside.circle_filled(c, 2.0, accent);
        // labels hug the axes, on the surface colour so gridlines do not cut through them
        let y_tag = Pos2::new(plot.min.x + 3.0, (c.y - 2.0).max(plot.min.y + 12.0));
        tag(&inside, t, y_tag, Align2::LEFT_BOTTOM, format!("{mhz:.0} MHz"));
        let flip = c.x + 60.0 > plot.max.x;
        let (x, align) = if flip { (c.x - 3.0, Align2::RIGHT_BOTTOM) } else { (c.x + 3.0, Align2::LEFT_BOTTOM) };
        tag(&inside, t, Pos2::new(x, plot.max.y - 2.0), align, format!("{mv:.0} mV"));
    }

    // legend
    let (leg, _) = ui.allocate_exact_size(Vec2::new(width, 16.0), Sense::hover());
    let y = leg.center().y;
    let mut x = leg.min.x + 40.0;
    p.extend(Shape::dashed_line(&[Pos2::new(x, y), Pos2::new(x + 22.0, y)], Stroke::new(1.5_f32, gray), 5.0, 3.0));
    let r = p.text(Pos2::new(x + 28.0, y), Align2::LEFT_CENTER, "stock", theme::regular(11.0), t.muted());
    x = r.max.x + 16.0;
    p.line_segment([Pos2::new(x, y), Pos2::new(x + 22.0, y)], Stroke::new(2.0_f32, accent));
    let name = if ticked { "preview" } else { "preview (not ticked)" };
    let r = p.text(Pos2::new(x + 28.0, y), Align2::LEFT_CENTER, name, theme::regular(11.0), t.muted());
    x = r.max.x + 16.0;
    p.circle(Pos2::new(x + 5.0, y), 4.0, t.surface, Stroke::new(1.5_f32, t.text));
    let live = match (status.live_mv, status.live_mhz) {
        (Some(mv), Some(mhz)) => format!("live {mv:.0} mV · {mhz:.0} MHz"),
        (Some(mv), None) => format!("live {mv:.0} mV"),
        _ => "live point not read".into(),
    };
    p.text(Pos2::new(x + 14.0, y), Align2::LEFT_CENTER, live, theme::regular(11.0), t.muted());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner_status() -> OcStatus {
        let stock = [
            (700.0, 225.0),
            (770.0, 247.0),
            (800.0, 675.0),
            (810.0, 757.0),
            (815.0, 847.0),
            (820.0, 930.0),
            (850.0, 1365.0),
            (895.0, 1965.0),
            (900.0, 2047.0),
            (950.0, 2655.0),
            (1000.0, 2820.0),
            (1100.0, 3022.0),
            (1240.0, 3232.0),
        ];
        OcStatus { available: true, stock_curve: stock.to_vec(), ..Default::default() }
    }

    #[test]
    fn the_form_builds_only_the_overclock_patch() {
        let mut v = OverclockView { power_on: true, power: "520".into(), ..Default::default() };
        let Ok(Job::ApplyOverclock(p)) = v.apply_job() else { panic!("no job") };
        assert_eq!(p.oc_power_enabled, Some(true));
        assert_eq!(p.oc_power_w, Some(520.0));
        assert_eq!(p.oc_uv_enabled, Some(false));
        assert_eq!(p.threshold_amps, None);
        assert_eq!(p.sag_enabled, None);
        // nothing outside the oc_ group: blank those and the patch is empty
        let mut rest = p.clone();
        rest.oc_uv_enabled = None;
        rest.oc_uv_from_mv = None;
        rest.oc_uv_to_mv = None;
        rest.oc_uv_target_mhz = None;
        rest.oc_power_enabled = None;
        rest.oc_power_w = None;
        rest.oc_mem_enabled = None;
        rest.oc_mem_offset_mhz = None;
        rest.oc_apply_delay_seconds = None;
        rest.oc_xbar_enabled = None;
        rest.oc_xbar_offset_mhz = None;
        rest.oc_sys_enabled = None;
        rest.oc_sys_offset_mhz = None;
        rest.oc_clk_load_only = None;
        rest.oc_clk_load_watts = None;
        rest.oc_clk_load_seconds = None;
        rest.oc_clk_idle_seconds = None;
        assert!(rest.is_empty());
        v.mem = "abc".into();
        assert!(v.apply_job().is_err());
    }

    #[test]
    fn preview_offset_matches_the_service_math() {
        let p = preview(&owner_status(), 820.0, 895.0, 2827.0).unwrap();
        assert_eq!(p.offset_mhz, 862);
        let text = offset_text(&p, 820.0, 895.0);
        assert!(text.contains("+862 MHz"), "{text}");
        assert!(text.contains("820–895 mV"), "{text}");
        assert!(preview(&owner_status(), 895.0, 820.0, 2827.0).is_err());
    }

    #[test]
    fn the_curve_peak_is_the_first_voltage_at_the_top_frequency() {
        let p = preview(&owner_status(), 820.0, 895.0, 2827.0).unwrap();
        // points above To are capped to the target, so 900 mV and up repeat 2827 MHz
        assert_eq!(curve_peak(&p.curve), Some((895.0, 2827.0)));
        assert_eq!(curve_peak(&owner_status().stock_curve), Some((1240.0, 3232.0)));
        assert_eq!(curve_peak(&[]), None);
    }
}
