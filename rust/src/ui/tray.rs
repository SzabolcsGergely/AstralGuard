//! The tray icon. It lives on its own thread with its own message loop (Win32 on Windows,
//! GTK on Linux), so it keeps working while the dashboard window is hidden and eframe is
//! not running frames. It reads the worker's snapshot and talks to the window through
//! the action queue plus a direct show (Windows) and a repaint request.

use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use super::backend::{Job, Link};
use crate::protocol::{fmt_tilt, Live, SagStatus, Status, APP_NAME, APP_VERSION};

/// What the tray asks of the dashboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayAction {
    Open,
    OpenLogTab,
    /// Pausing asks for confirmation in the window.
    ConfirmPause,
    ConfirmExit,
}

pub type Actions = Arc<Mutex<Vec<TrayAction>>>;

/// The Windows tray tooltip holds 127 characters.
const TOOLTIP_MAX: usize = 127;

fn short_pin(label: &str) -> String {
    label.replace("GPU 12VHPWR ", "").replace(" Current", "")
}

fn imbalance_tooltip(l: &Live) -> String {
    // only while warning: the spread at idle is noise and would crowd the tooltip
    match (l.imbalance_active, l.imbalance_pct) {
        (true, Some(p)) => format!(" | IMBALANCE {p:.0}%"),
        _ => String::new(),
    }
}

fn sag_tooltip(l: &Live) -> String {
    let mut parts = Vec::new();
    if let Some(dev) = l.sag_deg {
        let tag = if matches!(l.sag_status, SagStatus::Warning | SagStatus::Serious) { "SAG" } else { "sag" };
        parts.push(format!("{tag} {}", fmt_tilt(dev, l.card_length_mm)));
    }
    if let Some(off) = l.sag_offaxis {
        parts.push(format!("abs {}", fmt_tilt(off, l.card_length_mm)));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" | {}", parts.join(", "))
    }
}

/// The tray tooltip, as the Python guard wrote it.
pub fn tray_title(live: Option<&Live>, connected: bool) -> String {
    let Some(l) = live.filter(|_| connected) else {
        return format!("{APP_NAME}: Unknown (service not running)");
    };
    let mut named = format!("Max {} {:.2}A", short_pin(&l.max_label), l.max_val);
    let mut unnamed = format!("Max {:.2}A", l.max_val);
    if let Some(min) = l.min_val {
        named += &format!(", Min {} {:.2}A", short_pin(&l.min_label), min);
        unnamed += &format!(", Min {min:.2}A");
    }
    let st = if l.paused { Status::Paused } else { l.status };
    let label = st.text();
    match st {
        Status::Mitigated | Status::Over | Status::Shutdown => format!(
            "{APP_NAME}: {label} ({}) | {named}{}",
            st.detail(),
            if st == Status::Mitigated { String::new() } else { imbalance_tooltip(l) }
        ),
        Status::Missing | Status::Error | Status::Paused => format!("{APP_NAME}: {label} ({})", st.detail()),
        Status::Ok => {
            // protection must never weaken silently
            let mode = if l.effective_shutdown_only { " | shutdown-only" } else { "" };
            format!("{APP_NAME}: {label} | {unnamed}{mode}{}{}", imbalance_tooltip(l), sag_tooltip(l))
        }
    }
}

pub fn tray_tooltip(live: Option<&Live>, connected: bool) -> String {
    let title = tray_title(live, connected);
    if title.chars().count() <= TOOLTIP_MAX {
        title
    } else {
        title.chars().take(TOOLTIP_MAX - 3).collect::<String>() + "..."
    }
}

/// The colour the tray and window icon show.
pub fn icon_status(live: Option<&Live>, connected: bool) -> Status {
    match live {
        Some(l) if connected => {
            if l.paused {
                Status::Paused
            } else {
                l.status
            }
        }
        _ => Status::Missing,
    }
}

/// A coloured dot with a small white highlight, RGBA, `size` x `size`. 4x4 supersampled.
pub fn dot_rgba(rgb: [u8; 3], size: u32) -> Vec<u8> {
    let s = size as f32;
    let (cx, cy, r) = (s / 2.0, s / 2.0, s * 0.375);
    // highlight: the Python icon's ellipse (18,16)-(30,28) on 64 px
    let (hx, hy, hr) = (s * 24.0 / 64.0, s * 22.0 / 64.0, s * 6.0 / 64.0);
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    const N: u32 = 4;
    for y in 0..size {
        for x in 0..size {
            let (mut cov, mut hl) = (0.0f32, 0.0f32);
            for sy in 0..N {
                for sx in 0..N {
                    let px = x as f32 + (sx as f32 + 0.5) / N as f32;
                    let py = y as f32 + (sy as f32 + 0.5) / N as f32;
                    if (px - cx).powi(2) + (py - cy).powi(2) <= r * r {
                        cov += 1.0;
                        if (px - hx).powi(2) + (py - hy).powi(2) <= hr * hr {
                            hl += 1.0;
                        }
                    }
                }
            }
            let n = (N * N) as f32;
            let a = cov / n;
            let h = if cov > 0.0 { hl / cov * 90.0 / 255.0 } else { 0.0 };
            let mix = |c: u8| (c as f32 * (1.0 - h) + 255.0 * h).round() as u8;
            out.extend_from_slice(&[mix(rgb[0]), mix(rgb[1]), mix(rgb[2]), (a * 255.0).round() as u8]);
        }
    }
    out
}

fn icon_for(status: Status) -> Option<Icon> {
    Icon::from_rgba(dot_rgba(status.color(), 32), 32, 32).ok()
}

pub struct TrayHandle {
    thread: Option<thread::JoinHandle<()>>,
}

impl TrayHandle {
    /// The worker's `link.quit` ends the loop; wait for the icon to be removed.
    pub fn join(mut self) {
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Start the tray thread. `hwnd` is the dashboard window (Windows), 0 if unknown.
pub fn spawn(link: Arc<Link>, actions: Actions, hwnd: Arc<AtomicIsize>) -> TrayHandle {
    let thread = thread::Builder::new().name("ui-tray".into()).spawn(move || run(link, actions, hwnd)).ok();
    TrayHandle { thread }
}

struct Items {
    tray: TrayIcon,
    pause: MenuItem,
    last_status: Option<Status>,
    last_tip: String,
    last_pause_text: String,
    ticks: u32,
}

fn build(link: &Arc<Link>, actions: &Actions, hwnd: &Arc<AtomicIsize>) -> Option<Items> {
    let menu = Menu::new();
    let version = MenuItem::new(format!("{APP_NAME} v{APP_VERSION}"), false, None);
    let open = MenuItem::new("Open Dashboard", true, None);
    let log = MenuItem::new("Open Log", true, None);
    let pause = MenuItem::new("Pause", true, None);
    let exit = MenuItem::new("Exit", true, None);
    menu.append_items(&[
        &version,
        &PredefinedMenuItem::separator(),
        &open,
        &log,
        &PredefinedMenuItem::separator(),
        &pause,
        &exit,
    ])
    .ok()?;

    let ids = (open.id().clone(), log.id().clone(), pause.id().clone(), exit.id().clone());
    {
        let (link, actions, hwnd) = (link.clone(), actions.clone(), hwnd.clone());
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            let id = e.id();
            if *id == ids.0 {
                act(&link, &actions, &hwnd, TrayAction::Open);
            } else if *id == ids.1 {
                open_log(&link, &actions, &hwnd);
            } else if *id == ids.2 {
                let paused = link.live().0.is_some_and(|l| l.paused);
                if paused {
                    link.send(Job::SetPaused(false));
                } else {
                    act(&link, &actions, &hwnd, TrayAction::ConfirmPause);
                }
            } else if *id == ids.3 {
                act(&link, &actions, &hwnd, TrayAction::ConfirmExit);
            }
        }));
    }
    {
        let (link, actions, hwnd) = (link.clone(), actions.clone(), hwnd.clone());
        TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| match e {
            TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }
            | TrayIconEvent::DoubleClick { button: MouseButton::Left, .. } => {
                act(&link, &actions, &hwnd, TrayAction::Open)
            }
            _ => {}
        }));
    }

    let (live, connected) = link.live();
    let status = icon_status(live.as_deref(), connected);
    let tip = tray_tooltip(live.as_deref(), connected);
    let mut b = TrayIconBuilder::new().with_menu(Box::new(menu)).with_menu_on_left_click(false).with_tooltip(&tip);
    if let Some(icon) = icon_for(status) {
        b = b.with_icon(icon);
    }
    let tray = b.build().ok()?;
    Some(Items { tray, pause, last_status: Some(status), last_tip: tip, last_pause_text: "Pause".into(), ticks: 0 })
}

fn act(link: &Link, actions: &Actions, hwnd: &AtomicIsize, a: TrayAction) {
    actions.lock().unwrap().push(a);
    #[cfg(windows)]
    super::win::show_window(hwnd.load(Ordering::Relaxed));
    #[cfg(not(windows))]
    let _ = hwnd;
    link.repaint();
}

fn open_log(link: &Link, actions: &Actions, hwnd: &AtomicIsize) {
    // The file if this user can see it, the dashboard's Log tab otherwise.
    let path = link.live().0.map(|l| l.log_path.clone()).unwrap_or_default();
    if !path.is_empty() && std::path::Path::new(&path).is_file() {
        super::open_path(&path);
    } else {
        act(link, actions, hwnd, TrayAction::OpenLogTab);
    }
}

/// Once per tick: icon, tooltip and the Pause/Resume label from the latest snapshot.
fn refresh(link: &Link, it: &mut Items) {
    let (live, connected) = link.live();
    let status = icon_status(live.as_deref(), connected);
    it.ticks += 1;
    // The image is re-pushed every 10 s, not just on change: a one-shot update can be
    // lost while Explorer is still building the taskbar at logon.
    if it.last_status != Some(status) || it.ticks >= 10 {
        let _ = it.tray.set_icon(icon_for(status));
        it.last_status = Some(status);
        it.ticks = 0;
    }
    let tip = tray_tooltip(live.as_deref(), connected);
    if tip != it.last_tip {
        let _ = it.tray.set_tooltip(Some(&tip));
        it.last_tip = tip;
    }
    let pause_text = if live.as_ref().is_some_and(|l| l.paused) { "Resume" } else { "Pause" };
    if pause_text != it.last_pause_text {
        it.pause.set_text(pause_text);
        it.last_pause_text = pause_text.into();
    }
    it.pause.set_enabled(connected);
}

#[cfg(windows)]
fn run(link: Arc<Link>, actions: Actions, hwnd: Arc<AtomicIsize>) {
    use windows::Win32::Foundation::{BOOL, HWND};
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, MsgWaitForMultipleObjects, PeekMessageW, TranslateMessage, MSG, PM_REMOVE, QS_ALLINPUT,
        WM_QUIT,
    };
    let Some(mut items) = build(&link, &actions, &hwnd) else { return };
    let mut last = std::time::Instant::now();
    'outer: loop {
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, HWND::default(), 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    break 'outer;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        if link.quit.load(Ordering::Relaxed) {
            break;
        }
        if last.elapsed().as_millis() >= 1000 {
            refresh(&link, &mut items);
            last = std::time::Instant::now();
        }
        unsafe {
            MsgWaitForMultipleObjects(None, BOOL::from(false), 250, QS_ALLINPUT);
        }
    }
    MenuEvent::set_event_handler(None::<fn(MenuEvent)>);
    TrayIconEvent::set_event_handler(None::<fn(TrayIconEvent)>);
    drop(items); // removes the icon
}

/// Linux: tray-icon needs GTK initialised and its main loop running on this thread.
/// Not built or tested on Linux yet.
#[cfg(target_os = "linux")]
fn run(link: Arc<Link>, actions: Actions, hwnd: Arc<AtomicIsize>) {
    use gtk::glib;
    if gtk::init().is_err() {
        return;
    }
    let Some(items) = build(&link, &actions, &hwnd) else { return };
    let items = std::rc::Rc::new(std::cell::RefCell::new(items));
    glib::timeout_add_local(std::time::Duration::from_millis(1000), move || {
        if link.quit.load(Ordering::Relaxed) {
            gtk::main_quit();
            return glib::ControlFlow::Break;
        }
        refresh(&link, &mut items.borrow_mut());
        glib::ControlFlow::Continue
    });
    gtk::main();
}

#[cfg(not(any(windows, target_os = "linux")))]
fn run(_link: Arc<Link>, _actions: Actions, _hwnd: Arc<AtomicIsize>) {
    // TODO: macOS needs the tray on the main thread; not a target for now.
}
