//! Protection and Settings tabs: every protection value the service lets the dashboard change, plus the
//! window's own preferences. Protection changes go Check -> (confirm warnings) -> Apply.

use std::time::Instant;

use eframe::egui::{self, Align, Layout, RichText, TextEdit, Ui, Vec2};

use super::backend::Job;
use super::prefs::Prefs;
use super::theme::{self, Theme};
use super::widgets as w;
use crate::protocol::{ImbalanceResponse, Response, ResponseMode, Settings, SettingsPatch};

struct Num {
    label: &'static str,
    unit: &'static str,
    get: fn(&Settings) -> f64,
    set: fn(&mut SettingsPatch, f64),
}

macro_rules! num {
    ($f:ident, $label:expr, $unit:expr) => {
        Num { label: $label, unit: $unit, get: |s| s.$f, set: |p, v| p.$f = Some(v) }
    };
}

// indexes into NUMS, grouped by card
/// Why the automatic sensor scan exists and when it runs; shown in the scan popup and as
/// the Protection tab checkbox's tooltip.
pub const AUTO_SCAN_WHY: &str = "The card's pin monitor sits on one of its I2C buses. A driver \
update, a kernel change or a different card can move it to another bus; the pins then stop \
reading and the connector is not protected. With this on, the guard scans by itself when that \
happens. It does not scan at every start: only when the pins stop giving valid readings, and \
only once each time.";

const OVERCURRENT: [usize; 5] = [0, 1, 2, 3, 4];
const IMBALANCE: [usize; 4] = [5, 6, 7, 8];
const SAG: [usize; 4] = [9, 10, 11, 12];
const SAG_WARN: usize = 9;
const SAG_SERIOUS: usize = 10;
const SAG_LENGTH: usize = 12;

const NUMS: [Num; 13] = [
    num!(threshold_amps, "Tier 1", "A"),
    num!(sustained_seconds_required, "Tier 1 for", "s"),
    num!(critical_amps, "Tier 2", "A"),
    num!(critical_seconds, "Tier 2 for", "s"),
    num!(catastrophic_amps, "Tier 3", "A · immediate"),
    num!(imbalance_percent, "Threshold", "% of max pin"),
    num!(imbalance_delta_amps, "Max delta", "A"),
    num!(imbalance_min_amps, "Only above", "A on max pin"),
    num!(imbalance_sustained_seconds, "Sustained for", "s"),
    num!(sag_warn_degrees, "Warning", "°"),
    num!(sag_serious_degrees, "Serious", "°"),
    num!(sag_sustained_seconds, "Sustained for", "s"),
    num!(sag_card_length_mm, "Card length", "mm · 0 = spec"),
];

#[derive(Clone, Copy, PartialEq)]
pub enum MsgKind {
    Ok,
    Err,
    Info,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Autostart {
    Unknown,
    Busy,
    Known(Option<bool>),
}

/// Things the settings tab wants the app to do.
pub enum Ask {
    Job(Job),
    ConfirmWarnings(Vec<String>, SettingsPatch),
    ConfirmReset,
    ConfirmCalibrate,
}

pub struct SettingsView {
    texts: Vec<String>,
    sag_enabled: bool,
    imbalance_enabled: bool,
    response_mode: ResponseMode,
    imbalance_response: ImbalanceResponse,
    sensor_auto_scan: bool,
    base: Option<Settings>,
    msg: Option<(String, MsgKind, Instant)>,
    busy: Option<&'static str>,
    /// Take the next snapshot's values even if the form was edited (after apply/reset).
    reload: bool,
    pub autostart: Autostart,
}

impl Default for SettingsView {
    fn default() -> Self {
        SettingsView {
            texts: vec![String::new(); NUMS.len()],
            sag_enabled: true,
            imbalance_enabled: true,
            response_mode: ResponseMode::Tiered,
            imbalance_response: ImbalanceResponse::Warn,
            sensor_auto_scan: false,
            base: None,
            msg: None,
            busy: None,
            reload: true,
            autostart: Autostart::Unknown,
        }
    }
}

fn parse(text: &str) -> Option<f64> {
    text.trim().replace(',', ".").parse::<f64>().ok().filter(|v| v.is_finite())
}

impl SettingsView {
    fn load(&mut self, s: &Settings) {
        for (i, n) in NUMS.iter().enumerate() {
            self.texts[i] = crate::config::g((n.get)(s));
        }
        self.sag_enabled = s.sag_enabled;
        self.imbalance_enabled = s.imbalance_enabled;
        self.response_mode = s.response_mode;
        self.imbalance_response = s.imbalance_response;
        self.sensor_auto_scan = s.sensor_auto_scan;
        self.base = Some(s.clone());
        self.reload = false;
    }

    /// (patch of changed fields, parse errors)
    fn diff(&self) -> (SettingsPatch, Vec<String>) {
        let mut p = SettingsPatch::default();
        let mut errors = Vec::new();
        let Some(base) = &self.base else { return (p, errors) };
        for (i, n) in NUMS.iter().enumerate() {
            match parse(&self.texts[i]) {
                Some(v) if (v - (n.get)(base)).abs() > 1e-9 => (n.set)(&mut p, v),
                Some(_) => {}
                None => errors.push(format!("{}: '{}' is not a number", n.label, self.texts[i].trim())),
            }
        }
        if self.sag_enabled != base.sag_enabled {
            p.sag_enabled = Some(self.sag_enabled);
        }
        if self.imbalance_enabled != base.imbalance_enabled {
            p.imbalance_enabled = Some(self.imbalance_enabled);
        }
        if self.response_mode != base.response_mode {
            p.response_mode = Some(self.response_mode);
        }
        if self.imbalance_response != base.imbalance_response {
            p.imbalance_response = Some(self.imbalance_response);
        }
        if self.sensor_auto_scan != base.sensor_auto_scan {
            p.sensor_auto_scan = Some(self.sensor_auto_scan);
        }
        (p, errors)
    }

    pub fn dirty(&self) -> bool {
        let (p, e) = self.diff();
        !p.is_empty() || !e.is_empty()
    }

    fn say(&mut self, text: impl Into<String>, kind: MsgKind) {
        self.msg = Some((text.into(), kind, Instant::now()));
    }

    /// Keep the form in step with the service while the user is not editing.
    pub fn sync(&mut self, live: Option<&Settings>) {
        if let Some(s) = live {
            if self.reload || (self.base.as_ref() != Some(s) && !self.dirty()) {
                self.load(s);
            }
        }
    }

    pub fn apply_confirmed(&mut self) {
        self.busy = Some("Applying…");
    }

    /// A finished job that belongs to this tab. Returns a follow-up for the app.
    pub fn on_done(&mut self, job: &Job, result: &Result<Response, String>) -> Option<Ask> {
        match (job, result) {
            (Job::Check(patch), Ok(Response::Check { errors, warnings })) => {
                if !errors.is_empty() {
                    self.busy = None;
                    self.say(errors.join("; "), MsgKind::Err);
                    None
                } else if !warnings.is_empty() {
                    self.busy = None;
                    Some(Ask::ConfirmWarnings(warnings.clone(), patch.clone()))
                } else {
                    self.busy = Some("Applying…");
                    Some(Ask::Job(Job::Apply(patch.clone())))
                }
            }
            (Job::Apply(patch), Ok(Response::Applied { failures })) => {
                self.busy = None;
                self.reload = true;
                if failures.is_empty() {
                    let n = crate::config::patch_keys(patch).len();
                    self.say(format!("{n} setting{} applied", if n == 1 { "" } else { "s" }), MsgKind::Ok);
                } else {
                    self.say(format!("Applied, but: {}", failures.join("; ")), MsgKind::Err);
                }
                None
            }
            (Job::ResetDefaults, Ok(_)) => {
                self.reload = true;
                self.say("Defaults restored", MsgKind::Ok);
                None
            }
            (Job::Calibrate, Ok(_)) => {
                self.say("Calibrating · takes about 10 s", MsgKind::Info);
                None
            }
            (Job::AutostartGet, Ok(Response::Autostart { state })) => {
                self.autostart = Autostart::Known(*state);
                None
            }
            (Job::AutostartSet(_), Ok(Response::Bool { value })) => {
                if !value {
                    self.say("Could not change autostart: needs administrator rights", MsgKind::Err);
                }
                self.autostart = Autostart::Busy;
                Some(Ask::Job(Job::AutostartGet))
            }
            (Job::AutostartGet | Job::AutostartSet(_), Err(e)) => {
                self.autostart = Autostart::Known(None);
                self.say(e.clone(), MsgKind::Err);
                None
            }
            (Job::Check(_) | Job::Apply(_) | Job::ResetDefaults | Job::Calibrate, Err(e)) => {
                self.busy = None;
                self.say(e.clone(), MsgKind::Err);
                None
            }
            (Job::Check(_) | Job::Apply(_), Ok(other)) => {
                self.busy = None;
                self.say(format!("Unexpected answer: {other:?}"), MsgKind::Err);
                None
            }
            _ => None,
        }
    }

    /// The bar under the tab: message, Reset, Revert, Apply.
    pub fn bottom_bar(&mut self, ui: &mut Ui, t: &Theme, connected: bool) -> Option<Ask> {
        let mut ask = None;
        ui.horizontal(|ui| {
            ui.set_min_height(32.0);
            let dirty = self.dirty();
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let can_apply = connected && dirty && self.busy.is_none();
                if w::primary_button(ui, t, "Apply", can_apply).clicked() {
                    let (patch, errors) = self.diff();
                    if !errors.is_empty() {
                        self.say(errors.join("; "), MsgKind::Err);
                    } else if !patch.is_empty() {
                        self.busy = Some("Checking…");
                        self.msg = None;
                        ask = Some(Ask::Job(Job::Check(patch)));
                    }
                }
                if ui.add_enabled(dirty, egui::Button::new("Revert").min_size(Vec2::new(0.0, 28.0))).clicked() {
                    if let Some(b) = self.base.clone() {
                        self.load(&b);
                    }
                    self.msg = None;
                }
                if ui
                    .add_enabled(
                        connected && self.busy.is_none(),
                        egui::Button::new("Reset to defaults…").min_size(Vec2::new(0.0, 28.0)),
                    )
                    .clicked()
                {
                    ask = Some(Ask::ConfirmReset);
                }
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    if let Some(b) = self.busy {
                        ui.spinner();
                        ui.label(RichText::new(b).color(t.muted()));
                    } else if let Some((text, kind, at)) = &self.msg {
                        let c = match kind {
                            MsgKind::Ok => t.green,
                            MsgKind::Err => t.danger,
                            MsgKind::Info => t.muted(),
                        };
                        if *kind != MsgKind::Err && at.elapsed().as_secs() >= 5 {
                            self.msg = None;
                        } else {
                            ui.add(egui::Label::new(RichText::new(text.as_str()).color(c)).wrap());
                            ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
                        }
                    } else if dirty {
                        ui.label(RichText::new("Unsaved changes").color(t.muted()));
                    } else if !connected {
                        ui.label(RichText::new("Service not running").color(t.muted()));
                    }
                });
            });
        });
        ask
    }

    pub fn show(
        &mut self,
        ui: &mut Ui,
        t: &Theme,
        prefs: &mut Prefs,
        card_mm: f64,
        updates: Option<&super::update::Updates>,
        protection: bool,
    ) -> (Option<Ask>, bool) {
        let mut ask = None;
        let mut prefs_changed = false;
        let base = self.base.clone().unwrap_or_default();
        let card_mm = match parse(&self.texts[SAG_LENGTH]) {
            Some(v) if v > 0.0 => v,
            _ => card_mm,
        };

        ui.columns(2, |cols| {
            let (left, right) = cols.split_at_mut(1);
            let (l, r) = (&mut left[0], &mut right[0]);
            // columns are justified; cards lay out their rows left-aligned
            l.with_layout(Layout::top_down(Align::LEFT), |l| {
                if protection {
                    w::card(l, t, "Overcurrent & tiers", |ui| {
                        for i in OVERCURRENT {
                            self.num_row(ui, t, &base, i, None);
                        }
                        ui.add_space(8.0);
                        w::caption(ui, t, "Response");
                        ui.add_space(2.0);
                        let changed = self.response_mode != base.response_mode;
                        for (v, text) in [
                            (ResponseMode::Tiered, "Limit GPU, then shut down"),
                            (ResponseMode::ShutdownOnly, "Shut down only"),
                        ] {
                            let rt = RichText::new(text).color(if changed && self.response_mode == v {
                                t.accent
                            } else {
                                t.text
                            });
                            ui.radio_value(&mut self.response_mode, v, rt);
                        }
                    });
                    l.add_space(w::GAP);
                    w::card(l, t, "What this does", |ui| {
                        for line in self.ladder_text(&base) {
                            ui.add(egui::Label::new(RichText::new(line).color(t.muted()).size(12.5)).wrap());
                            ui.add_space(2.0);
                        }
                    });
                    // Linux only: on Windows the driver addresses the pin monitor directly.
                    if cfg!(target_os = "linux") {
                        l.add_space(w::GAP);
                        w::card(l, t, "Pin sensor", |ui| {
                            let ch = self.sensor_auto_scan != base.sensor_auto_scan;
                            ui.checkbox(
                                &mut self.sensor_auto_scan,
                                RichText::new("Scan for the pin sensor automatically").color(if ch {
                                    t.accent
                                } else {
                                    t.text
                                }),
                            )
                            .on_hover_text(AUTO_SCAN_WHY);
                            ui.label(
                                RichText::new("Only when it stops reading, once each time").color(t.muted()).size(12.0),
                            );
                        });
                    }
                } else {
                    w::card(l, t, "Window", |ui| {
                        label_row(ui, t, "Theme", |ui| {
                            let label =
                                theme::PRESETS.iter().find(|p| p.0 == prefs.theme).map(|p| p.1).unwrap_or("Dark");
                            egui::ComboBox::from_id_salt("theme").selected_text(label).width(150.0).show_ui(ui, |ui| {
                                for (key, name) in theme::PRESETS {
                                    if ui.selectable_label(prefs.theme == key, name).clicked() {
                                        prefs.theme = key.into();
                                        prefs_changed = true;
                                    }
                                }
                            });
                        });
                        label_row(ui, t, "Close button", |ui| {
                            let label = if prefs.close_action == "exit" { "Exit the guard" } else { "Hide to tray" };
                            egui::ComboBox::from_id_salt("close").selected_text(label).width(150.0).show_ui(ui, |ui| {
                                for (key, name) in [("tray", "Hide to tray"), ("exit", "Exit the guard")] {
                                    if ui.selectable_label(prefs.close_action == key, name).clicked() {
                                        prefs.close_action = key.into();
                                        prefs_changed = true;
                                    }
                                }
                            });
                        });
                        ui.add_space(2.0);
                        for (v, text) in [
                            (&mut prefs.minimize_to_tray, "Minimize to tray"),
                            (&mut prefs.show_on_start, "Open on start"),
                            (&mut prefs.always_on_top, "Always on top"),
                            (&mut prefs.connector_mirror, "Mirror connector pin order"),
                        ] {
                            if text == "Always on top" && !super::always_on_top_supported() {
                                continue;
                            }
                            if ui.checkbox(v, text).changed() {
                                prefs_changed = true;
                            }
                        }
                    });
                }
            });

            r.with_layout(Layout::top_down(Align::LEFT), |r| {
                if protection {
                    w::card(r, t, "Pin imbalance", |ui| {
                        let rt =
                            |changed: bool, s: &str| RichText::new(s).color(if changed { t.accent } else { t.text });
                        let ch = self.imbalance_enabled != base.imbalance_enabled;
                        ui.checkbox(&mut self.imbalance_enabled, rt(ch, "Check enabled"));
                        ui.horizontal(|ui| {
                            let ch = self.imbalance_response != base.imbalance_response;
                            for (v, s) in [
                                (ImbalanceResponse::Warn, "Warn only"),
                                (ImbalanceResponse::Act, "Act like overcurrent"),
                            ] {
                                let sel = ch && self.imbalance_response == v;
                                ui.radio_value(&mut self.imbalance_response, v, rt(sel, s));
                            }
                        });
                        ui.add_space(2.0);
                        for i in IMBALANCE {
                            self.num_row(ui, t, &base, i, None);
                        }
                    });
                    r.add_space(w::GAP);
                    w::card(r, t, "GPU sag", |ui| {
                        let ch = self.sag_enabled != base.sag_enabled;
                        ui.checkbox(
                            &mut self.sag_enabled,
                            RichText::new("Detection enabled").color(if ch { t.accent } else { t.text }),
                        );
                        ui.add_space(2.0);
                        for i in SAG {
                            let hint = (i == SAG_WARN || i == SAG_SERIOUS)
                                .then(|| {
                                    parse(&self.texts[i])
                                        .map(|d| format!("° → {:.1} mm", crate::protocol::sag_mm(d, card_mm)))
                                })
                                .flatten();
                            self.num_row(ui, t, &base, i, hint);
                        }
                        ui.add_space(4.0);
                        if w::button(ui, "Calibrate baseline…").clicked() {
                            ask = Some(Ask::ConfirmCalibrate);
                        }
                    });
                } else {
                    w::card(r, t, "System", |ui| {
                        let label = if cfg!(windows) { "Start with Windows" } else { "Start at boot" };
                        ui.horizontal(|ui| match self.autostart {
                            Autostart::Known(Some(mut on)) => {
                                if ui.checkbox(&mut on, label).changed() {
                                    self.autostart = Autostart::Busy;
                                    ask = Some(Ask::Job(Job::AutostartSet(on)));
                                }
                            }
                            Autostart::Known(None) => {
                                ui.add_enabled(false, egui::Checkbox::new(&mut false, label));
                                ui.label(RichText::new("· not installed").color(t.muted()).size(12.0));
                            }
                            Autostart::Busy | Autostart::Unknown => {
                                ui.add_enabled(false, egui::Checkbox::new(&mut false, label));
                                ui.spinner();
                            }
                        });
                        ui.label(RichText::new("Applies at once · needs admin").color(t.muted()).size(12.0));
                        ui.add_space(6.0);
                        if ui.checkbox(&mut prefs.check_updates, "Check for updates daily").changed() {
                            prefs_changed = true;
                        }
                        if let Some(u) = updates {
                            use super::update::UpdateState;
                            ui.horizontal(|ui| {
                                if w::button(ui, "Check now").clicked() {
                                    u.check_now();
                                }
                                let current = crate::protocol::APP_VERSION;
                                let (text, color) = match u.state() {
                                    UpdateState::Idle => (format!("v{current}"), t.muted()),
                                    UpdateState::Checking => ("checking…".into(), t.muted()),
                                    UpdateState::UpToDate { checked } => {
                                        (format!("v{current} is the latest · {}", checked.format("%H:%M")), t.muted())
                                    }
                                    UpdateState::Available { version, .. } => {
                                        (format!("v{version} available"), t.accent)
                                    }
                                    UpdateState::Failed { error } => (format!("check failed: {error}"), t.pending),
                                };
                                ui.label(RichText::new(text).color(color).size(12.0));
                            });
                        }
                    });
                }
            });
        });
        (ask, prefs_changed)
    }

    /// The response ladder in words, from the values in the form (the saved one where a field
    /// does not parse), so an edit shows what it will mean before it is applied.
    fn ladder_text(&self, base: &Settings) -> Vec<String> {
        let v = |i: usize| parse(&self.texts[i]).unwrap_or((NUMS[i].get)(base));
        let (t1, t1s, t2, t2s, t3) = (v(0), v(1), v(2), v(3), v(4));
        if self.response_mode == ResponseMode::Tiered {
            vec![
                format!("A pin at {t1} A or more for {t1s} s: the GPU is limited."),
                format!("{t2} A or more for {t2s} s: the GPU is limited, with less time to recover."),
                format!("{t3} A or more: the PC shuts down at once."),
                format!("If limiting does not bring the pin under {t1} A, the PC shuts down."),
            ]
        } else {
            vec![
                format!("A pin at {t1} A or more for {t1s} s: the PC shuts down."),
                format!("{t2} A or more for {t2s} s: the PC shuts down."),
                format!("{t3} A or more: the PC shuts down at once."),
                "The GPU is never limited.".to_string(),
            ]
        }
    }

    fn num_row(&mut self, ui: &mut Ui, t: &Theme, base: &Settings, i: usize, hint: Option<String>) {
        let n = &NUMS[i];
        let parsed = parse(&self.texts[i]);
        let changed = parsed.is_some_and(|v| (v - (n.get)(base)).abs() > 1e-9);
        let color = if parsed.is_none() {
            t.danger
        } else if changed {
            t.accent
        } else {
            t.text
        };
        label_row(ui, t, n.label, |ui| {
            ui.add(
                TextEdit::singleline(&mut self.texts[i])
                    .desired_width(62.0)
                    .horizontal_align(Align::RIGHT)
                    .text_color(color)
                    .margin(egui::Margin::symmetric(6, 3)),
            );
            let unit = hint.unwrap_or_else(|| n.unit.to_string());
            ui.label(RichText::new(unit).color(t.muted()).size(12.0));
        });
    }
}

const LABEL_W: f32 = 104.0;

fn label_row(ui: &mut Ui, t: &Theme, label: &str, add: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        ui.allocate_ui_with_layout(Vec2::new(LABEL_W, 24.0), Layout::left_to_right(Align::Center), |ui| {
            ui.set_min_width(LABEL_W);
            ui.label(RichText::new(label).color(t.text));
        });
        add(ui);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ladder_text_follows_the_form_and_the_response_mode() {
        let base = Settings::default();
        let mut v = SettingsView::default();
        v.load(&base);
        let lines = v.ladder_text(&base);
        assert_eq!(lines[0], "A pin at 9.5 A or more for 15 s: the GPU is limited.");
        assert_eq!(lines[2], "16 A or more: the PC shuts down at once.");

        // an edit shows before it is applied; a field that does not parse keeps the saved value
        v.texts[0] = "11".into();
        v.texts[2] = "x".into();
        v.response_mode = ResponseMode::ShutdownOnly;
        let lines = v.ladder_text(&base);
        assert_eq!(lines[0], "A pin at 11 A or more for 15 s: the PC shuts down.");
        assert_eq!(lines[1], "13 A or more for 3 s: the PC shuts down.");
        assert!(lines.iter().all(|l| !l.contains("is limited")), "{lines:?}");
    }
}
