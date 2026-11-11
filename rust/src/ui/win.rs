//! The few Win32 calls the UI needs. Window handles travel between threads as isize.

use windows::core::{w, BOOL, HSTRING, PCWSTR};
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, HANDLE, HWND};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWINDOWATTRIBUTE};
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, OpenEventW, SetEvent, WaitForSingleObject, EVENT_MODIFY_STATE, INFINITE,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, IsIconic, IsWindowVisible, SetForegroundWindow, ShowWindowAsync, SW_RESTORE, SW_SHOW, SW_SHOWNORMAL,
};

fn hwnd(h: isize) -> HWND {
    HWND(h as *mut core::ffi::c_void)
}

/// Bring the dashboard up from another thread (the tray's). Works while eframe is not
/// running frames for the hidden window, which is exactly when it is needed.
pub fn show_window(h: isize) {
    if h == 0 {
        return;
    }
    unsafe {
        let w = hwnd(h);
        let cmd = if IsIconic(w).as_bool() { SW_RESTORE } else { SW_SHOW };
        let _ = ShowWindowAsync(w, cmd);
        let _ = SetForegroundWindow(w);
    }
}

pub fn is_visible(h: isize) -> bool {
    h != 0 && unsafe { IsWindowVisible(hwnd(h)).as_bool() }
}

/// Dark or light title bar to match the theme (Windows 10 2004+ / 11).
pub fn set_dark_titlebar(h: isize, dark: bool) {
    if h == 0 {
        return;
    }
    let v = BOOL::from(dark);
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd(h),
            DWMWINDOWATTRIBUTE(20), // DWMWA_USE_IMMERSIVE_DARK_MODE
            &v as *const BOOL as *const core::ffi::c_void,
            std::mem::size_of::<BOOL>() as u32,
        );
    }
}

pub fn apps_use_light_theme() -> Option<bool> {
    let mut data: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut data as *mut u32 as *mut core::ffi::c_void),
            Some(&mut size),
        )
    };
    r.is_ok().then_some(data != 0)
}

const SHOW_EVENT: PCWSTR = w!("Local\\AstralGuardShowDashboard");

/// One dashboard per session: true (after bringing the other one up) if a UI is already
/// running. The mutex is held until this process exits.
///
/// `wait`: started by the watchdog (not by the user), so another dashboard is most likely
/// one that is still exiting after a restart: wait for it a while before giving up,
/// rather than leaving the session with no tray icon at all.
pub fn another_instance_running(wait: std::time::Duration) -> bool {
    let give_up = std::time::Instant::now() + wait;
    loop {
        let running = unsafe {
            match CreateMutexW(None, true, w!("Local\\AstralGuardUI")) {
                Ok(h) if GetLastError() == ERROR_ALREADY_EXISTS => {
                    // Closed, or this handle alone would keep the name alive.
                    let _ = windows::Win32::Foundation::CloseHandle(h);
                    true
                }
                // Kept: the OS releases it when the process ends.
                Ok(_) => false,
                // Access denied: it exists, created by the elevated dashboard.
                Err(e) => e.code() == windows::Win32::Foundation::E_ACCESSDENIED,
            }
        };
        if !running {
            return false;
        }
        if std::time::Instant::now() >= give_up {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    if wait.is_zero() && !signal_show() {
        if let Ok(other) = unsafe { FindWindowW(PCWSTR::null(), w!("AstralGuard")) } {
            show_window(other.0 as isize);
        }
    }
    true
}

/// Ask the running dashboard to show itself. Works from a non-elevated process (the Start
/// menu shortcut), where window messages to the elevated dashboard are blocked.
fn signal_show() -> bool {
    unsafe {
        match OpenEventW(EVENT_MODIFY_STATE, false, SHOW_EVENT) {
            Ok(ev) => {
                let ok = SetEvent(ev).is_ok();
                let _ = windows::Win32::Foundation::CloseHandle(ev);
                ok
            }
            Err(_) => false,
        }
    }
}

/// The other half of signal_show: a thread that shows the window whenever the event fires.
/// The event lets any interactive user signal it; that can only ever open the dashboard.
pub fn listen_for_show(hwnd: std::sync::Arc<std::sync::atomic::AtomicIsize>, ctx: eframe::egui::Context) {
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    let ev = unsafe {
        let mut sd = PSECURITY_DESCRIPTOR::default();
        // SYNCHRONIZE | EVENT_MODIFY_STATE for interactive users, all for admins and SYSTEM.
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            w!("D:(A;;0x100002;;;IU)(A;;GA;;;BA)(A;;GA;;;SY)"),
            SDDL_REVISION_1,
            &mut sd,
            None,
        )
        .is_err()
        {
            return;
        }
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: false.into(),
        };
        // The descriptor stays allocated for the life of the process; it is tiny.
        match CreateEventW(Some(&sa), false, false, SHOW_EVENT) {
            Ok(ev) => ev.0 as isize,
            Err(_) => return,
        }
    };
    let _ = std::thread::Builder::new().name("show-event".into()).spawn(move || loop {
        let ev = HANDLE(ev as *mut core::ffi::c_void);
        if unsafe { WaitForSingleObject(ev, INFINITE) }.0 != 0 {
            return;
        }
        show_window(hwnd.load(std::sync::atomic::Ordering::Relaxed));
        ctx.request_repaint();
    });
}

/// Open a file or folder with its default program.
pub fn open_path(path: &str) {
    let file = HSTRING::from(path);
    unsafe {
        ShellExecuteW(None, w!("open"), &file, PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
    }
}
