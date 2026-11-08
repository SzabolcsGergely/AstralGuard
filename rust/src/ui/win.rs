//! The few Win32 calls the UI needs. Window handles travel between threads as isize.

use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{GetLastError, BOOL, ERROR_ALREADY_EXISTS, HANDLE, HWND};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWINDOWATTRIBUTE};
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows::Win32::System::Threading::CreateMutexW;
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

/// One dashboard per session: true (after bringing the other one up) if a UI is already
/// running. The mutex is held until this process exits.
pub fn another_instance_running() -> bool {
    unsafe {
        let Ok(h) = CreateMutexW(None, true, w!("Local\\AstralGuardUI")) else {
            return false;
        };
        if GetLastError() == ERROR_ALREADY_EXISTS {
            if let Ok(other) = FindWindowW(PCWSTR::null(), w!("AstralGuard")) {
                show_window(other.0 as isize);
            }
            return true;
        }
        let _: HANDLE = h; // never closed: the OS releases it when the process ends
        false
    }
}

/// Open a file or folder with its default program.
pub fn open_path(path: &str) {
    let file = HSTRING::from(path);
    unsafe {
        ShellExecuteW(HWND::default(), w!("open"), &file, PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
    }
}
