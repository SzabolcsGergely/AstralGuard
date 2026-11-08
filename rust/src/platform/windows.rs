use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows::Win32::System::EventLog::{
    DeregisterEventSource, RegisterEventSourceW, ReportEventW, EVENTLOG_ERROR_TYPE, EVENTLOG_INFORMATION_TYPE,
    EVENTLOG_WARNING_TYPE,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_SET_VALUE, REG_DWORD, REG_EXPAND_SZ,
    REG_OPTION_NON_VOLATILE,
};
use windows::Win32::System::Threading::{CreateMutexW, GetCurrentProcess, OpenProcessToken};

use super::EventKind;
use crate::protocol::APP_NAME;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// Held by the service: two guards would mean two shutdown commands.
const SINGLE_INSTANCE_MUTEX: &str = "Local\\AstralGuard_SingleInstance";
pub const TASK_NAME: &str = "AstralGuard";

fn system32(exe: &str) -> PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    PathBuf::from(root).join("System32").join(exe)
}

/// Asked of the process token, never answered by trying a privileged call: a safety
/// tool does not poke the GPU to learn about itself.
pub fn is_elevated() -> bool {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut returned = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut _),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    }
}

/// Held for the process lifetime. False when another guard already runs.
pub fn acquire_single_instance() -> bool {
    static HANDLE_KEEP: OnceLock<usize> = OnceLock::new();
    unsafe {
        let name = HSTRING::from(SINGLE_INSTANCE_MUTEX);
        match CreateMutexW(None, false, &name) {
            Ok(h) => {
                let already = GetLastError() == ERROR_ALREADY_EXISTS;
                let _ = HANDLE_KEEP.set(h.0 as usize);
                !already
            }
            Err(_) => true, // cannot tell; running twice beats not running at all
        }
    }
}

/// A Windows toast. Best effort, and on its own thread: a slow or missing notification
/// must never hold up the monitor loop.
pub fn toast(title: &str, msg: &str) {
    let (title, msg) = (title.to_string(), msg.to_string());
    let _ = std::thread::Builder::new().name("toast".into()).spawn(move || {
        let _ = show_toast(&title, &msg);
    });
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn show_toast(title: &str, msg: &str) -> windows::core::Result<()> {
    use windows::Data::Xml::Dom::XmlDocument;
    use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
    let xml = XmlDocument::new()?;
    xml.LoadXml(&HSTRING::from(format!(
        "<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual></toast>",
        xml_escape(title),
        xml_escape(msg)
    )))?;
    let toast = ToastNotification::CreateToastNotification(&xml)?;
    ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(APP_NAME))?.Show(&toast)
}

/// Register the Event Viewer source. Points it at .NET's EventLogMessages.dll, whose
/// message table is "%1" for every id, so Event Viewer shows the text instead of
/// "The description for Event ID ... cannot be found". Needs admin; safe to repeat.
pub fn register_event_source() -> Result<(), String> {
    let dll = PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into()))
        .join("Microsoft.NET\\Framework64\\v4.0.30319\\EventLogMessages.dll");
    unsafe {
        let key_path = HSTRING::from(format!("SYSTEM\\CurrentControlSet\\Services\\EventLog\\Application\\{APP_NAME}"));
        let mut key = HKEY::default();
        RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            &key_path,
            0,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut key,
            None,
        )
        .ok()
        .map_err(|e| e.to_string())?;
        if dll.exists() {
            let wide: Vec<u16> = dll.as_os_str().encode_wide_nul();
            let bytes = std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2);
            let _ = RegSetValueExW(key, w!("EventMessageFile"), 0, REG_EXPAND_SZ, Some(bytes));
        }
        let types: u32 = 7;
        let _ = RegSetValueExW(key, w!("TypesSupported"), 0, REG_DWORD, Some(&types.to_le_bytes()));
        let _ = RegCloseKey(key);
    }
    Ok(())
}

trait EncodeWideNul {
    fn encode_wide_nul(&self) -> Vec<u16>;
}

impl EncodeWideNul for std::ffi::OsStr {
    fn encode_wide_nul(&self) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        self.encode_wide().chain(std::iter::once(0)).collect()
    }
}

pub fn event_log(kind: EventKind, id: u32, msg: &str) {
    unsafe {
        let Ok(h) = RegisterEventSourceW(PCWSTR::null(), &HSTRING::from(APP_NAME)) else { return };
        let text = HSTRING::from(msg);
        let strings = [PCWSTR(text.as_ptr())];
        let kind = match kind {
            EventKind::Info => EVENTLOG_INFORMATION_TYPE,
            EventKind::Warning => EVENTLOG_WARNING_TYPE,
            EventKind::Error => EVENTLOG_ERROR_TYPE,
        };
        let _ = ReportEventW(h, kind, 0, id, None, 0, Some(&strings), None);
        let _ = DeregisterEventSource(h);
    }
}

/// shutdown.exe /s /t 5 /f, by absolute path.
pub fn shutdown_system() -> Result<(), String> {
    Command::new(system32("shutdown.exe"))
        .args(["/s", "/t", &super::shutdown_delay_sec().to_string(), "/f"])
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map_err(|e| e.to_string())
        .map(|_| ())
}

/// The logon task's Enabled flag: Some(true/false), None when the task does not exist.
/// Reads /xml, whose element names are the same on every Windows language.
pub fn autostart() -> Option<bool> {
    let out = Command::new(system32("schtasks.exe"))
        .args(["/query", "/tn", TASK_NAME, "/xml", "ONE"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    if !out.status.success() || out.stdout.is_empty() {
        return None;
    }
    Some(task_enabled(&decode_task_xml(&out.stdout)))
}

/// schtasks claims UTF-16 in its header but writes single-byte text into a pipe.
fn decode_task_xml(raw: &[u8]) -> String {
    if raw.starts_with(&[0xFF, 0xFE]) {
        let wide: Vec<u16> = raw[2..].as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
        String::from_utf16_lossy(&wide)
    } else {
        String::from_utf8_lossy(raw).trim_start_matches('\u{feff}').to_string()
    }
}

/// <Settings><Enabled> only: triggers have an <Enabled> of their own. Missing = true.
fn task_enabled(xml: &str) -> bool {
    let Some(start) = xml.find("<Settings>") else { return true };
    let end = xml[start..].find("</Settings>").map(|e| start + e).unwrap_or(xml.len());
    let settings = &xml[start..end];
    match settings.find("<Enabled>") {
        Some(i) => settings[i + 9..].trim_start().to_ascii_lowercase().starts_with("true"),
        None => true,
    }
}

pub fn set_autostart(enable: bool) -> bool {
    let ok = Command::new(system32("schtasks.exe"))
        .args(["/change", "/tn", TASK_NAME, if enable { "/enable" } else { "/disable" }])
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    ok && autostart() == Some(enable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enabled_flag_comes_from_settings_not_triggers() {
        let xml = "<Task><Triggers><LogonTrigger><Enabled>true</Enabled></LogonTrigger></Triggers>\
                   <Settings><Enabled>false</Enabled></Settings></Task>";
        assert!(!task_enabled(xml));
        assert!(task_enabled("<Task><Settings><Hidden>x</Hidden></Settings></Task>"));
    }
}
