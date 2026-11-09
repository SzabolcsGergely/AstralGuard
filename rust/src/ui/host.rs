//! Linux: `astralguard ui` is a window-less tray process; the dashboard is a child process,
//! `astralguard ui --window`, started when it is wanted and gone when it is closed.
//!
//! Why: Wayland gives a client no way to hide its window (winit's set_visible is
//! unsupported there, and so is positioning it off-screen), and winit cannot create a
//! second event loop in one process. So "Hide to tray" closes the window for real, and the
//! tray, the desktop notifications and the update check live on here.
//! VERIFIED (2026-11-09, KDE Plasma 6.8 / KWin Wayland): close did nothing, and a window
//! meant to start hidden at login stayed on screen.
//!
//! The tray's clicks reach a running window as lines on its stdin ("open", "log", "pause",
//! "exit"); a new window gets the click as `--action`. A window that stopped the guard
//! (Exit) ends with EXIT_GUARD_STOPPED, and the tray goes with it.

use std::io::Write;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui;

use super::backend::{self, Link};
use super::prefs::Prefs;
use super::tray::{self, TrayAction};

/// The window's exit code after the user stopped the guard from it.
pub const EXIT_GUARD_STOPPED: i32 = 3;

pub fn action_key(a: TrayAction) -> &'static str {
    match a {
        TrayAction::Open => "open",
        TrayAction::OpenLogTab => "log",
        TrayAction::ConfirmPause => "pause",
        TrayAction::ConfirmExit => "exit",
    }
}

pub fn parse_action(s: &str) -> Option<TrayAction> {
    match s.trim() {
        "open" => Some(TrayAction::Open),
        "log" => Some(TrayAction::OpenLogTab),
        "pause" => Some(TrayAction::ConfirmPause),
        "exit" => Some(TrayAction::ConfirmExit),
        _ => None,
    }
}

/// The window side: read the tray's clicks from stdin into the window's action queue.
pub fn listen_for_actions(link: Arc<Link>, actions: tray::Actions) {
    let _ = std::thread::Builder::new().name("ui-stdin".into()).spawn(move || {
        let stdin = std::io::stdin();
        let mut line = String::new();
        while stdin.read_line(&mut line).is_ok_and(|n| n > 0) {
            if let Some(a) = parse_action(&line) {
                actions.lock().unwrap().push(a);
                link.repaint();
            }
            line.clear();
        }
    });
}

struct Window {
    child: Child,
    stdin: Option<ChildStdin>,
}

/// After a package upgrade replaces the binary, Linux reports the running tray's own path as
/// "/usr/bin/astralguard (deleted)". The window must start from the new file at the original
/// path, or the tray can never open it again until the next login.
fn window_exe(exe: std::path::PathBuf) -> std::path::PathBuf {
    match exe.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
        Some(original) => std::path::PathBuf::from(original),
        None => exe,
    }
}

fn spawn_window(action: TrayAction) -> Option<Window> {
    let exe = window_exe(std::env::current_exe().ok()?);
    let mut child = Command::new(exe)
        .args(["ui", "--window", "--action", action_key(action)])
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| eprintln!("astralguard ui: cannot start the dashboard window ({e})"))
        .ok()?;
    let stdin = child.stdin.take();
    Some(Window { child, stdin })
}

pub fn run(show: bool) -> i32 {
    let prefs = Prefs::load();
    let link = Link::new();
    // Polls the service for the tray icon and raises the service's notices on the desktop.
    backend::start(link.clone(), Box::new(crate::ipc::Client::new()));
    let actions: tray::Actions = Arc::new(Mutex::new(Vec::new()));
    let tray = tray::spawn(link.clone(), actions.clone(), Arc::new(AtomicIsize::new(0)));
    // The daily check notifies from here, so it happens with no window open; nothing
    // repaints, the window runs its own check for its banner.
    let _updates = super::update::Updates::spawn(egui::Context::default(), prefs.check_updates, super::notify_update);

    let mut window: Option<Window> = None;
    if show || prefs.show_on_start {
        window = spawn_window(TrayAction::Open);
    }
    let mut code = 0;
    while !link.quit.load(Ordering::Relaxed) {
        if let Some(w) = window.as_mut() {
            if let Ok(Some(status)) = w.child.try_wait() {
                window = None;
                if status.code() == Some(EXIT_GUARD_STOPPED) {
                    break;
                }
            }
        }
        let pending = std::mem::take(&mut *actions.lock().unwrap());
        for a in pending {
            // A running window gets the click; if its stdin is gone it is on its way out.
            let delivered = window
                .as_mut()
                .and_then(|w| w.stdin.as_mut())
                .is_some_and(|s| writeln!(s, "{}", action_key(a)).and_then(|_| s.flush()).is_ok());
            if !delivered {
                if let Some(mut old) = window.take() {
                    let _ = old.child.wait();
                }
                window = spawn_window(a);
                if window.is_none() {
                    code = 1;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    link.quit.store(true, Ordering::Relaxed);
    tray.join();
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_starts_from_the_upgraded_binary() {
        use std::path::PathBuf;
        assert_eq!(window_exe(PathBuf::from("/usr/bin/astralguard (deleted)")), PathBuf::from("/usr/bin/astralguard"));
        assert_eq!(window_exe(PathBuf::from("/usr/bin/astralguard")), PathBuf::from("/usr/bin/astralguard"));
    }

    #[test]
    fn every_tray_action_round_trips() {
        for a in [TrayAction::Open, TrayAction::OpenLogTab, TrayAction::ConfirmPause, TrayAction::ConfirmExit] {
            assert_eq!(parse_action(&format!("{}\n", action_key(a))), Some(a));
        }
        assert_eq!(parse_action("reboot"), None);
    }
}
