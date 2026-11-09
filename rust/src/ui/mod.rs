//! `astralguard ui`: the tray icon and the dashboard window.
//!
//! A separate process from the protection service, so nothing here can hurt protection.
//! Talks to the service only through `ipc::Client` (on a worker thread, never the UI
//! thread); works, and keeps retrying, while the service is not running.
//!
//! Threads: main (eframe window), `ui-ipc` (polls the service, runs commands), `ui-tray`
//! (tray icon with its own message loop, so it keeps working while the window is hidden
//! and eframe runs no frames).
//!
//! Options:
//!   --show                 open the window even if "Open on start" is off
//!   --demo                 simulated service instead of IPC (--phase <s>, --offline)
//!   --screenshot <png>     render, save a PNG, exit (--tab, --theme, --scale, --size WxH,
//!                          --confirm pause|exit|reset|calibrate|warnings)

mod backend;
mod demo;
mod imu;
mod live;
mod log;
mod prefs;
mod settings;
mod theme;
mod thermal;
mod tray;
mod update;
mod widgets;
#[cfg(windows)]
mod win;

use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, Color32, CornerRadius, Layout, Margin, Pos2, RichText, Sense, Stroke, Vec2, ViewportCommand,
};

use backend::{Backend, Job, Link};
use prefs::Prefs;
use settings::{Ask, SettingsView};
use theme::Theme;
use tray::{TrayAction, TrayHandle};
use widgets as w;

use crate::protocol::{Live, ResponseMode, SagStatus, SettingsPatch, Status, APP_NAME};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tab {
    Live,
    Imu,
    Settings,
    Log,
}

impl Tab {
    const ALL: [Tab; 4] = [Tab::Live, Tab::Imu, Tab::Settings, Tab::Log];
    fn key(self) -> &'static str {
        match self {
            Tab::Live => "live",
            Tab::Imu => "imu",
            Tab::Settings => "settings",
            Tab::Log => "log",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Tab::Live => "Live",
            Tab::Imu => "IMU",
            Tab::Settings => "Settings",
            Tab::Log => "Log",
        }
    }
    fn parse(s: &str) -> Tab {
        Tab::ALL.into_iter().find(|t| t.key() == s.to_ascii_lowercase()).unwrap_or(Tab::Live)
    }
}

enum Confirm {
    Pause,
    Exit,
    Calibrate,
    Reset,
    Warnings(Vec<String>, Box<SettingsPatch>),
}

#[derive(Default)]
struct Opts {
    show: bool,
    demo: bool,
    offline: bool,
    phase: Option<f64>,
    screenshot: Option<String>,
    tab: Option<String>,
    theme: Option<String>,
    scale: Option<f32>,
    size: Option<(f32, f32)>,
    confirm: Option<String>,
}

impl Opts {
    fn parse(args: &[String]) -> Opts {
        let mut o = Opts::default();
        let mut it = args.iter();
        while let Some(a) = it.next() {
            match a.as_str() {
                "--show" => o.show = true,
                "--demo" => o.demo = true,
                "--offline" => o.offline = true,
                "--phase" => o.phase = it.next().and_then(|v| v.parse().ok()),
                "--screenshot" => o.screenshot = it.next().cloned(),
                "--tab" => o.tab = it.next().cloned(),
                "--theme" => o.theme = it.next().cloned(),
                "--scale" => o.scale = it.next().and_then(|v| v.parse().ok()),
                "--size" => {
                    o.size = it.next().and_then(|v| {
                        let (a, b) = v.split_once('x')?;
                        Some((a.parse().ok()?, b.parse().ok()?))
                    })
                }
                "--confirm" => o.confirm = it.next().cloned(),
                _ => {}
            }
        }
        o
    }
}

/// Open a file with its default program.
pub(crate) fn open_path(path: &str) {
    #[cfg(windows)]
    win::open_path(path);
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    }
}

const DEFAULT_SIZE: Vec2 = Vec2::new(800.0, 820.0);
const MIN_SIZE: Vec2 = Vec2::new(660.0, 560.0);

fn icon_data(status: Status) -> Arc<egui::IconData> {
    Arc::new(egui::IconData { rgba: tray::dot_rgba(status.color(), 64), width: 64, height: 64 })
}

pub fn run(args: &[String]) -> i32 {
    let opts = Opts::parse(args);
    let shot = opts.screenshot.is_some();

    #[cfg(windows)]
    if !opts.demo && !shot && win::another_instance_running() {
        return 0;
    }

    let mut prefs = if shot { Prefs::default() } else { Prefs::load() };
    if let Some(th) = &opts.theme {
        prefs.theme = th.clone();
    }
    if let Some(tab) = &opts.tab {
        prefs.last_tab = tab.clone();
    }
    let visible = shot || opts.show || opts.demo || prefs.show_on_start;

    let link = Link::new();
    let backend: Box<dyn Backend> = if opts.demo {
        Box::new(demo::Demo::new(opts.phase.unwrap_or(52.0), opts.offline))
    } else {
        Box::new(crate::ipc::Client::new())
    };

    let size = opts
        .size
        .map(|(w, h)| Vec2::new(w, h))
        .or(prefs.window.filter(|_| !shot).map(|g| Vec2::new(g[2], g[3])))
        .unwrap_or(DEFAULT_SIZE)
        .max(MIN_SIZE);
    let saved_pos = prefs
        .window
        .filter(|g| !shot && g[0] > -10000.0 && g[1] > -10000.0 && g[0] < 20000.0 && g[1] < 20000.0)
        .map(|g| Pos2::new(g[0], g[1]));
    let mut vp = egui::ViewportBuilder::default()
        .with_title(APP_NAME)
        .with_app_id("astralguard")
        .with_inner_size(size)
        .with_min_inner_size(MIN_SIZE)
        .with_icon(icon_data(Status::Missing));
    if prefs.always_on_top && !shot {
        vp = vp.with_always_on_top();
    }
    // eframe shows the window after its first frame no matter what, so a window that
    // starts hidden is created off-screen and hidden on the second frame.
    if !visible {
        vp = vp.with_position(Pos2::new(-32000.0, -32000.0)).with_active(false);
    } else if let Some(p) = saved_pos {
        vp = vp.with_position(p);
    }
    let options =
        eframe::NativeOptions { viewport: vp, centered: saved_pos.is_none() && visible, ..Default::default() };

    let tray_handle: Arc<Mutex<Option<TrayHandle>>> = Arc::new(Mutex::new(None));
    let th = tray_handle.clone();
    let link2 = link.clone();
    let result = eframe::run_native(
        APP_NAME,
        options,
        Box::new(move |cc| {
            let ctx = &cc.egui_ctx;
            theme::install_fonts(ctx);
            if let Some(s) = opts.scale {
                ctx.set_zoom_factor(s);
            }
            link2.set_ctx(ctx);
            let hwnd = Arc::new(AtomicIsize::new(window_handle(cc)));
            #[cfg(windows)]
            if !shot && !opts.demo {
                win::listen_for_show(hwnd.clone(), ctx.clone());
            }
            backend::start(link2.clone(), backend);
            let actions: tray::Actions = Arc::new(Mutex::new(Vec::new()));
            if !shot {
                *th.lock().unwrap() = Some(tray::spawn(link2.clone(), actions.clone(), hwnd.clone()));
            }
            let check = prefs.check_updates && !opts.demo && !shot;
            let updates = update::Updates::spawn(ctx.clone(), check, notify_update);
            let mut app = App::new(ctx, link2, actions, hwnd, prefs, opts, visible, saved_pos);
            app.updates = Some(updates);
            Ok(Box::new(app))
        }),
    );
    link.quit.store(true, Ordering::Relaxed);
    if let Some(t) = tray_handle.lock().unwrap().take() {
        t.join();
    }
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("astralguard ui: {e}");
            1
        }
    }
}

/// A desktop notification for a newly found release.
fn notify_update(version: &str) {
    let title = format!("{APP_NAME} update");
    let msg = format!("Version {version} is available. Open the dashboard to download it.");
    #[cfg(windows)]
    crate::platform::toast(&title, &msg);
    #[cfg(not(windows))]
    {
        let _ = notify_rust::Notification::new().summary(&title).body(&msg).show();
    }
}

fn window_handle(cc: &eframe::CreationContext) -> isize {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match cc.window_handle().map(|h| h.as_raw()) {
        Ok(RawWindowHandle::Win32(h)) => h.hwnd.get(),
        _ => 0,
    }
}

struct Shot {
    path: String,
    ready_frames: u32,
    requested: bool,
}

struct App {
    link: Arc<Link>,
    actions: tray::Actions,
    // The native window: titlebar theme and show/hide go through it on Windows.
    #[cfg_attr(not(windows), allow(dead_code))]
    hwnd: Arc<AtomicIsize>,
    prefs: Prefs,
    theme: Theme,
    theme_applied: Option<(String, Theme)>,
    next_theme_poll: Instant,
    tab: Tab,
    thermal: thermal::Thermal,
    settings: SettingsView,
    log: log::LogView,
    confirm: Option<Confirm>,
    frame: u64,
    start_hidden: bool,
    restore_pos: Option<Pos2>,
    shown: bool,
    quitting: bool,
    exit_pending: bool,
    icon_status: Option<Status>,
    flash: Option<(String, Instant)>,
    shot: Option<Shot>,
    live: Option<Arc<Live>>,
    connected: bool,
    started: Instant,
    updates: Option<update::Updates>,
}

impl App {
    #[allow(clippy::too_many_arguments)]
    fn new(
        ctx: &egui::Context,
        link: Arc<Link>,
        actions: tray::Actions,
        hwnd: Arc<AtomicIsize>,
        prefs: Prefs,
        opts: Opts,
        visible: bool,
        saved_pos: Option<Pos2>,
    ) -> App {
        let theme = theme::preset(&prefs.theme);
        let tab = Tab::parse(&prefs.last_tab);
        let confirm = match opts.confirm.as_deref() {
            Some("pause") => Some(Confirm::Pause),
            Some("exit") => Some(Confirm::Exit),
            Some("reset") => Some(Confirm::Reset),
            Some("calibrate") => Some(Confirm::Calibrate),
            Some("warnings") => Some(Confirm::Warnings(
                crate::config::check(
                    &Default::default(),
                    &SettingsPatch { threshold_amps: Some(11.0), ..Default::default() },
                )
                .1,
                Box::default(),
            )),
            _ => None,
        };
        let mut app = App {
            link,
            actions,
            hwnd,
            prefs,
            theme,
            theme_applied: None,
            next_theme_poll: Instant::now(),
            tab,
            thermal: Default::default(),
            settings: SettingsView::default(),
            log: Default::default(),
            confirm,
            frame: 0,
            start_hidden: !visible,
            restore_pos: saved_pos,
            shown: true,
            quitting: false,
            exit_pending: false,
            icon_status: None,
            flash: None,
            shot: opts.screenshot.map(|path| Shot { path, ready_frames: 0, requested: false }),
            live: None,
            connected: false,
            started: Instant::now(),
            updates: None,
        };
        app.apply_theme(ctx);
        app
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    fn hwnd(&self) -> isize {
        self.hwnd.load(Ordering::Relaxed)
    }

    /// Re-resolve the theme when the preference changed, or (for "system") every 3 s.
    fn apply_theme(&mut self, ctx: &egui::Context) {
        let key_changed = self.theme_applied.as_ref().is_none_or(|(k, _)| *k != self.prefs.theme);
        let poll = self.prefs.theme == "system" && Instant::now() >= self.next_theme_poll;
        if !key_changed && !poll {
            return;
        }
        self.next_theme_poll = Instant::now() + Duration::from_secs(3);
        let t = theme::preset(&self.prefs.theme);
        if !key_changed && self.theme_applied.as_ref().is_some_and(|(_, th)| *th == t) {
            return;
        }
        self.theme = t;
        theme::apply(ctx, &t);
        #[cfg(windows)]
        win::set_dark_titlebar(self.hwnd(), t.dark);
        self.theme_applied = Some((self.prefs.theme.clone(), t));
    }

    fn save_geometry(&mut self, ctx: &egui::Context) {
        if self.shot.is_some() {
            return;
        }
        let (outer, inner, minimized) =
            ctx.input(|i| (i.viewport().outer_rect, i.viewport().inner_rect, i.viewport().minimized));
        if minimized == Some(true) {
            return;
        }
        if let (Some(o), Some(i)) = (outer, inner) {
            if o.min.x > -10000.0 && o.min.y > -10000.0 {
                self.prefs.window = Some([o.min.x, o.min.y, i.width(), i.height()]);
            }
        }
        self.prefs.last_tab = self.tab.key().into();
    }

    fn save_prefs(&self) {
        if self.shot.is_none() {
            self.prefs.save();
        }
    }

    fn show_window(&mut self, ctx: &egui::Context) {
        #[cfg(windows)]
        win::show_window(self.hwnd());
        ctx.send_viewport_cmd(ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(ViewportCommand::Focus);
        self.shown = true;
    }

    fn hide_window(&mut self, ctx: &egui::Context) {
        self.save_geometry(ctx);
        self.save_prefs();
        ctx.send_viewport_cmd(ViewportCommand::Visible(false));
        self.shown = false;
    }

    fn quit(&mut self, ctx: &egui::Context) {
        if self.shown {
            self.save_geometry(ctx);
        }
        self.save_prefs();
        self.quitting = true;
        ctx.send_viewport_cmd(ViewportCommand::Close);
    }

    fn send(&self, job: Job) {
        self.link.send(job);
    }

    fn handle_ask(&mut self, ask: Ask) {
        match ask {
            Ask::Job(j) => self.send(j),
            Ask::ConfirmWarnings(ws, p) => self.confirm = Some(Confirm::Warnings(ws, Box::new(p))),
            Ask::ConfirmReset => self.confirm = Some(Confirm::Reset),
            Ask::ConfirmCalibrate => self.confirm = Some(Confirm::Calibrate),
        }
    }

    fn handle_done(&mut self, ctx: &egui::Context) {
        for (job, result) in self.link.take_done() {
            if let Some(ask) = self.settings.on_done(&job, &result) {
                self.handle_ask(ask);
            }
            self.log.on_done(&job, &result);
            match (&job, &result) {
                (Job::Exit, _) => self.quit(ctx),
                (Job::SetPaused(_) | Job::ImuFast(_) | Job::Calibrate, Err(e)) => {
                    self.flash = Some((e.clone(), Instant::now()));
                }
                (Job::Calibrate, Ok(_)) => {
                    self.flash = Some(("Calibrating · takes about 10 s".into(), Instant::now()));
                }
                _ => {}
            }
        }
    }

    fn handle_tray(&mut self, ctx: &egui::Context) {
        let actions = std::mem::take(&mut *self.actions.lock().unwrap());
        for a in actions {
            self.show_window(ctx);
            match a {
                TrayAction::Open => {}
                TrayAction::OpenLogTab => self.tab = Tab::Log,
                TrayAction::ConfirmPause => self.confirm = Some(Confirm::Pause),
                TrayAction::ConfirmExit => self.confirm = Some(Confirm::Exit),
            }
        }
    }

    fn window_events(&mut self, ctx: &egui::Context) {
        // Shown behind eframe's back (tray thread, second instance): tell winit, so a
        // later hide is not a no-op.
        #[cfg(windows)]
        if !self.shown && win::is_visible(self.hwnd()) {
            self.shown = true;
            ctx.send_viewport_cmd(ViewportCommand::Visible(true));
        }
        if self.start_hidden && self.frame == 2 {
            self.start_hidden = false;
            ctx.send_viewport_cmd(ViewportCommand::Visible(false));
            self.shown = false;
            match self.restore_pos {
                Some(p) => ctx.send_viewport_cmd(ViewportCommand::OuterPosition(p)),
                None => {
                    // centre on the primary monitor when first shown
                    if let Some(m) = ctx.input(|i| i.viewport().monitor_size) {
                        let s = ctx.input(|i| i.viewport().inner_rect.map(|r| r.size())).unwrap_or(DEFAULT_SIZE);
                        let p = ((m - s) / 2.0).max(Vec2::ZERO);
                        ctx.send_viewport_cmd(ViewportCommand::OuterPosition(p.to_pos2()));
                    }
                }
            }
        }
        if ctx.input(|i| i.viewport().close_requested()) && !self.quitting {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            if self.prefs.close_action == "exit" {
                self.confirm = Some(Confirm::Exit);
            } else if self.shot.is_none() {
                self.hide_window(ctx);
            }
        }
        if self.prefs.minimize_to_tray && self.shown && ctx.input(|i| i.viewport().minimized) == Some(true) {
            self.hide_window(ctx);
        }
        self.link.window_visible.store(self.shown, Ordering::Relaxed);
        self.link.fast.store(self.tab == Tab::Imu, Ordering::Relaxed);
    }

    fn screenshot(&mut self, ctx: &egui::Context) {
        let Some(shot) = &mut self.shot else { return };
        ctx.request_repaint();
        // live data in, and a moment for slower answers (autostart, log)
        let ready = (self.live.is_some() && self.started.elapsed() > Duration::from_millis(1200))
            || self.started.elapsed() > Duration::from_secs(3);
        if ready {
            shot.ready_frames += 1;
        }
        if shot.ready_frames == 8 && !shot.requested {
            shot.requested = true;
            ctx.send_viewport_cmd(ViewportCommand::Screenshot(Default::default()));
        }
        let image = ctx.input(|i| {
            i.raw.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(img) = image {
            let path = shot.path.clone();
            if let Err(e) = save_png(&path, &img) {
                eprintln!("screenshot: {e}");
            }
            self.quit(ctx);
        }
    }
}

fn save_png(path: &str, img: &egui::ColorImage) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), img.size[0] as u32, img.size[1] as u32);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().map_err(|e| e.to_string())?;
    let data: Vec<u8> = img.pixels.iter().flat_map(|c| [c.r(), c.g(), c.b()]).collect();
    writer.write_image_data(&data).map_err(|e| e.to_string())
}

impl eframe::App for App {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        self.theme.background.to_normalized_gamma_f32()
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.frame += 1;
        self.apply_theme(ctx);

        let (live, connected) = self.link.live();
        self.live = live;
        self.connected = connected;

        self.handle_tray(ctx);
        self.handle_done(ctx);
        self.window_events(ctx);

        let status = tray::icon_status(self.live.as_deref(), self.connected);
        if self.icon_status != Some(status) {
            self.icon_status = Some(status);
            ctx.send_viewport_cmd(ViewportCommand::Icon(Some(icon_data(status))));
        }
        self.settings.sync(self.live.as_ref().map(|l| &l.settings));
        if self.tab == Tab::Settings && self.connected && self.settings.autostart == settings::Autostart::Unknown {
            self.settings.autostart = settings::Autostart::Busy;
            self.send(Job::AutostartGet);
        }

        self.ui(ctx);
        self.modals(ctx);
        self.screenshot(ctx);
        if self.tab == Tab::Log && self.shown {
            let link = self.link.clone();
            self.log.tick(&mut |j| link.send(j));
        }
        ctx.request_repaint_after(Duration::from_millis(1000));
    }
}

// ---------------------------------------------------------------------------- drawing

impl App {
    fn ui(&mut self, ctx: &egui::Context) {
        let t = self.theme;
        let live = self.live.clone();
        let l = live.as_deref();

        egui::TopBottomPanel::top("header")
            .frame(egui::Frame::new().fill(t.background).inner_margin(Margin {
                left: 16,
                right: 16,
                top: 12,
                bottom: 0,
            }))
            .show_separator_line(false)
            .show(ctx, |ui| {
                self.header(ui, &t, l);
                self.banners(ui, &t, l);
                self.update_banner(ui, &t);
                self.tab_bar(ui, &t);
            });

        if self.tab == Tab::Settings {
            egui::TopBottomPanel::bottom("settings_bar")
                .frame(egui::Frame::new().fill(t.background).inner_margin(Margin {
                    left: 16,
                    right: 16,
                    top: 8,
                    bottom: 10,
                }))
                .show_separator_line(false)
                .show(ctx, |ui| {
                    let r = ui.max_rect();
                    ui.painter().line_segment(
                        [Pos2::new(r.min.x - 16.0, r.min.y - 8.0), Pos2::new(r.max.x + 16.0, r.min.y - 8.0)],
                        Stroke::new(1.0_f32, t.border),
                    );
                    if let Some(ask) = self.settings.bottom_bar(ui, &t, self.connected) {
                        self.handle_ask(ask);
                    }
                });
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(t.background).inner_margin(Margin {
                left: 16,
                right: 16,
                top: 12,
                bottom: 12,
            }))
            .show(ctx, |ui| match self.tab {
                Tab::Live => {
                    egui::ScrollArea::vertical().id_salt("live").auto_shrink([false, false]).show(ui, |ui| {
                        live::show(ui, &t, l, self.prefs.connector_mirror, &mut self.thermal);
                    });
                }
                Tab::Imu => {
                    let action = egui::ScrollArea::vertical()
                        .id_salt("imu")
                        .auto_shrink([false, false])
                        .show(ui, |ui| imu::show(ui, &t, l))
                        .inner;
                    match action {
                        Some(imu::ImuAction::Fast(s)) => self.send(Job::ImuFast(s)),
                        Some(imu::ImuAction::Calibrate) => self.confirm = Some(Confirm::Calibrate),
                        None => {}
                    }
                }
                Tab::Settings => {
                    let card_mm = l.map(|l| l.card_length_mm).unwrap_or(0.0);
                    let (ask, changed) = egui::ScrollArea::vertical()
                        .id_salt("settings")
                        .auto_shrink([false, false])
                        .show(ui, |ui| self.settings.show(ui, &t, &mut self.prefs, card_mm, self.updates.as_ref()))
                        .inner;
                    if let Some(ask) = ask {
                        self.handle_ask(ask);
                    }
                    if changed {
                        ctx.send_viewport_cmd(ViewportCommand::WindowLevel(if self.prefs.always_on_top {
                            egui::WindowLevel::AlwaysOnTop
                        } else {
                            egui::WindowLevel::Normal
                        }));
                        self.apply_theme(ctx);
                        self.save_prefs();
                        if let Some(u) = &self.updates {
                            u.set_enabled(self.prefs.check_updates);
                        }
                    }
                }
                Tab::Log => {
                    let path = l.map(|l| l.log_path.clone()).unwrap_or_default();
                    let link = self.link.clone();
                    self.log.show(ui, &t, &path, &mut |j| link.send(j));
                }
            });
    }

    fn header(&mut self, ui: &mut egui::Ui, t: &Theme, l: Option<&Live>) {
        let (status_text, detail, color) = match l {
            Some(l) if self.connected => {
                let st = if l.paused { Status::Paused } else { l.status };
                (st.text().to_string(), Some(st.detail()), w::rgb(st.color()))
            }
            _ if self.started.elapsed() < Duration::from_millis(1500) && !self.connected => {
                ("Connecting…".to_string(), None, t.muted())
            }
            _ => ("Unknown".to_string(), Some("service not running"), t.muted()),
        };
        ui.horizontal(|ui| {
            ui.set_min_height(40.0);
            let (r, _) = ui.allocate_exact_size(Vec2::new(14.0, 14.0), Sense::hover());
            ui.painter().circle_filled(r.center(), 9.0, color.gamma_multiply(0.22));
            ui.painter().circle_filled(r.center(), 5.5, color);
            ui.add_space(2.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.label(RichText::new(&status_text).font(theme::semibold(18.0)).color(t.text));
                let card = l.map(|l| l.card.as_str()).filter(|c| !c.is_empty()).unwrap_or("No card");
                let ver =
                    l.map(|l| l.version.as_str()).filter(|v| !v.is_empty()).unwrap_or(crate::protocol::APP_VERSION);
                let sub = match detail {
                    Some(d) => format!("{d} · {card} · v{ver}"),
                    None => format!("{card} · v{ver}"),
                };
                ui.label(RichText::new(sub).size(12.0).color(t.muted()));
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let paused = l.is_some_and(|l| l.paused);
                let label = if paused { "Resume" } else { "Pause" };
                let b = if paused {
                    egui::Button::new(RichText::new(label).color(Color32::WHITE))
                        .fill(w::rgb(Status::Paused.color()))
                        .stroke(Stroke::NONE)
                } else {
                    egui::Button::new(label)
                };
                if ui.add_enabled(self.connected, b.min_size(Vec2::new(86.0, 30.0))).clicked() {
                    if paused {
                        self.send(Job::SetPaused(false));
                    } else {
                        self.confirm = Some(Confirm::Pause);
                    }
                }
                if let Some((msg, at)) = &self.flash {
                    if at.elapsed() < Duration::from_secs(6) {
                        ui.label(RichText::new(msg).size(12.0).color(t.muted()));
                    } else {
                        self.flash = None;
                    }
                }
            });
        });
        ui.add_space(8.0);
    }

    /// "Update available" with Download / Skip, unless the user skipped that version.
    fn update_banner(&mut self, ui: &mut egui::Ui, t: &Theme) {
        let Some(update::UpdateState::Available { version, url }) = self.updates.as_ref().map(|u| u.state()) else {
            return;
        };
        if version == self.prefs.skipped_update {
            return;
        }
        let c = t.accent;
        egui::Frame::new()
            .fill(t.tint_bg(c))
            .stroke(Stroke::new(1.0_f32, c.gamma_multiply(0.45)))
            .corner_radius(CornerRadius::same(6))
            .inner_margin(Margin { left: 10, right: 10, top: 4, bottom: 4 })
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(Vec2::new(3.0, 16.0), Sense::hover());
                    ui.painter().rect_filled(r, 1.5, c);
                    ui.label(RichText::new("Update available").font(theme::semibold(13.0)).color(t.text));
                    ui.label(
                        RichText::new(format!("· v{version} (running v{})", crate::protocol::APP_VERSION))
                            .size(13.0)
                            .color(t.muted()),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.small_button("Skip").clicked() {
                            self.prefs.skipped_update = version.clone();
                            self.save_prefs();
                        }
                        if ui.small_button("Download").clicked() {
                            update::open_url(&url);
                        }
                    });
                });
            });
        ui.add_space(6.0);
    }

    fn banners(&self, ui: &mut egui::Ui, t: &Theme, l: Option<&Live>) {
        let mut list: Vec<(Color32, String, &str)> = Vec::new();
        match l {
            Some(l) if self.connected => {
                if l.status == Status::Shutdown {
                    list.push((t.danger, "Shutting down".into(), ""));
                }
                if l.paused {
                    list.push((w::rgb(Status::Paused.color()), "Paused".into(), "the connector is not protected"));
                } else if l.engaged {
                    let since = l.engaged_at.map(|a| w::mmss(w::now() - a)).unwrap_or_default();
                    list.push((
                        t.danger,
                        format!("GPU limited · tier {} · {since}", l.engaged_tier.unwrap_or(1)),
                        "save your work and check the cable",
                    ));
                }
                if l.imbalance_active && !l.paused {
                    list.push((t.pending, "Pin imbalance".into(), "check the plug is fully seated"));
                }
                if matches!(l.sag_status, SagStatus::Warning | SagStatus::Serious) {
                    list.push((t.pending, "GPU sag".into(), "add a support bracket, or recalibrate if moved"));
                }
                if l.effective_shutdown_only && l.settings.response_mode == ResponseMode::Tiered {
                    list.push((t.pending, "GPU limiting unavailable".into(), "tiers go straight to shutdown"));
                }
            }
            _ if self.started.elapsed() >= Duration::from_millis(1500) => {
                list.push((t.danger, "Guard service not running".into(), "retrying"));
            }
            _ => {}
        }
        for (c, head, tail) in list {
            egui::Frame::new()
                .fill(t.tint_bg(c))
                .stroke(Stroke::new(1.0_f32, c.gamma_multiply(0.45)))
                .corner_radius(CornerRadius::same(6))
                .inner_margin(Margin { left: 10, right: 10, top: 6, bottom: 6 })
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        let (r, _) = ui.allocate_exact_size(Vec2::new(3.0, 16.0), Sense::hover());
                        ui.painter().rect_filled(r, 1.5, c);
                        ui.label(RichText::new(head).font(theme::semibold(13.0)).color(t.text));
                        if !tail.is_empty() {
                            ui.label(RichText::new(format!("· {tail}")).size(13.0).color(t.muted()));
                        }
                    });
                });
            ui.add_space(6.0);
        }
    }

    fn tab_bar(&mut self, ui: &mut egui::Ui, t: &Theme) {
        ui.add_space(2.0);
        let full = ui.available_width();
        let (bar, _) = ui.allocate_exact_size(Vec2::new(full, 34.0), Sense::hover());
        let p = ui.painter().clone();
        p.line_segment(
            [Pos2::new(bar.min.x - 16.0, bar.max.y - 0.5), Pos2::new(bar.max.x + 16.0, bar.max.y - 0.5)],
            Stroke::new(1.0_f32, t.border),
        );
        let mut x = bar.min.x;
        for tab in Tab::ALL {
            let selected = self.tab == tab;
            let g = p.layout_no_wrap(
                tab.label().into(),
                if selected { theme::semibold(14.0) } else { theme::regular(14.0) },
                t.text,
            );
            let r = egui::Rect::from_min_size(Pos2::new(x, bar.min.y), Vec2::new(g.size().x + 24.0, bar.height()));
            let resp = ui.interact(r, ui.id().with(tab.key()), Sense::click());
            let color = if selected {
                t.text
            } else if resp.hovered() {
                theme::lerp(t.muted(), t.text, 0.5)
            } else {
                t.muted()
            };
            p.galley(Pos2::new(r.center().x - g.size().x / 2.0, r.center().y - g.size().y / 2.0 - 1.0), g, color);
            if selected {
                p.rect_filled(
                    egui::Rect::from_min_max(
                        Pos2::new(r.min.x + 8.0, r.max.y - 2.5),
                        Pos2::new(r.max.x - 8.0, r.max.y),
                    ),
                    1.0,
                    t.accent,
                );
            }
            if resp.clicked() && !selected {
                self.tab = tab;
                self.prefs.last_tab = tab.key().into();
                match tab {
                    Tab::Settings => self.settings.autostart = settings::Autostart::Unknown,
                    Tab::Log => self.log.force_refresh(),
                    _ => {}
                }
            }
            x = r.max.x;
        }
    }

    fn modals(&mut self, ctx: &egui::Context) {
        let Some(confirm) = &self.confirm else { return };
        let t = self.theme;
        let (title, body, ok, danger): (&str, Vec<String>, &str, bool) = match confirm {
            Confirm::Pause => (
                "Pause protection?",
                vec!["The connector is not protected while paused, and a GPU limit in place is released.".into()],
                "Pause",
                true,
            ),
            Confirm::Exit => (
                "Exit AstralGuard?",
                vec!["The connector is not protected until the guard is started again.".into()],
                "Exit",
                true,
            ),
            Confirm::Calibrate => (
                "Calibrate sag baseline?",
                vec!["Uses the card's current position as the no-sag reference. Leave the PC where it normally stands and untouched; it takes about 10 s.".into()],
                "Calibrate",
                false,
            ),
            Confirm::Reset => (
                "Reset to defaults?",
                vec!["Every protection setting returns to its default.".into()],
                "Reset",
                false,
            ),
            Confirm::Warnings(ws, _) => ("Reduced protection", ws.clone(), "Apply anyway", true),
        };
        let mut result: Option<bool> = None;
        let modal = egui::Modal::new(egui::Id::new("confirm"))
            .frame(
                egui::Frame::new()
                    .fill(t.surface)
                    .stroke(Stroke::new(1.0_f32, t.border))
                    .corner_radius(10)
                    .inner_margin(20),
            )
            .backdrop_color(Color32::from_black_alpha(140))
            .show(ctx, |ui| {
                ui.set_width(380.0);
                ui.label(RichText::new(title).font(theme::semibold(16.0)).color(t.text));
                ui.add_space(6.0);
                for line in &body {
                    ui.label(RichText::new(line).color(t.muted()));
                    ui.add_space(2.0);
                }
                ui.add_space(12.0);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let fill = if danger { t.danger } else { t.accent };
                    if ui
                        .add(
                            egui::Button::new(RichText::new(ok).color(Color32::WHITE))
                                .fill(fill)
                                .stroke(Stroke::NONE)
                                .min_size(Vec2::new(92.0, 30.0)),
                        )
                        .clicked()
                    {
                        result = Some(true);
                    }
                    if ui.add(egui::Button::new("Cancel").min_size(Vec2::new(80.0, 30.0))).clicked() {
                        result = Some(false);
                    }
                });
            });
        if modal.should_close() && result.is_none() {
            result = Some(false);
        }
        let Some(yes) = result else { return };
        let confirm = self.confirm.take();
        if !yes {
            return;
        }
        match confirm {
            Some(Confirm::Pause) => self.send(Job::SetPaused(true)),
            Some(Confirm::Exit) => {
                if self.connected && !self.exit_pending {
                    self.exit_pending = true;
                    self.send(Job::Exit);
                } else {
                    self.quit(ctx);
                }
            }
            Some(Confirm::Calibrate) => self.send(Job::Calibrate),
            Some(Confirm::Reset) => self.send(Job::ResetDefaults),
            Some(Confirm::Warnings(_, patch)) => {
                self.settings.apply_confirmed();
                self.send(Job::Apply(*patch));
            }
            None => {}
        }
    }
}
