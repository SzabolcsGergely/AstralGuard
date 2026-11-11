use std::fs::OpenOptions;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

use super::EventKind;

pub const UNIT_NAME: &str = "astralguard.service";
const LOCK_PATH: &str = "/run/astralguard/service.lock";

/// Root can set NVML clocks/power limits and open /dev/i2c-*.
pub fn is_elevated() -> bool {
    unsafe { libc::geteuid() == 0 }
}

/// An exclusive flock held for the process lifetime. False when another guard runs.
pub fn acquire_single_instance() -> bool {
    static FILE: OnceLock<std::fs::File> = OnceLock::new();
    if let Some(dir) = Path::new(LOCK_PATH).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(file) = OpenOptions::new().create(true).write(true).truncate(false).open(LOCK_PATH) else {
        return true; // cannot tell; running twice beats not running at all
    };
    let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    if locked {
        let _ = FILE.set(file);
    }
    locked
}

/// A root service has no desktop to toast on; the UI process shows its notices.
pub fn toast(_title: &str, _msg: &str) {}

pub fn register_event_source() -> Result<(), String> {
    Ok(())
}

/// systemd captures stderr into the journal; the <N> prefix sets the priority.
pub fn event_log(kind: EventKind, id: u32, msg: &str) {
    let prio = match kind {
        EventKind::Info => 6,
        EventKind::Warning => 4,
        EventKind::Error => 3,
    };
    eprintln!("<{prio}>[{id}] {msg}");
}

pub fn shutdown_system() -> Result<(), String> {
    let delay = super::shutdown_delay_sec();
    for (exe, args) in
        [("/usr/bin/systemctl", vec!["poweroff".to_string()]), ("/sbin/shutdown", vec!["-P".into(), "now".into()])]
    {
        if Path::new(exe).exists() {
            std::thread::sleep(std::time::Duration::from_secs(delay as u64));
            return Command::new(exe).args(&args).status().map(|_| ()).map_err(|e| e.to_string());
        }
    }
    Err("no systemctl or shutdown binary found".into())
}

fn systemctl(args: &[&str]) -> Option<std::process::Output> {
    Command::new("/usr/bin/systemctl").args(args).output().ok()
}

pub fn autostart() -> Option<bool> {
    let out = systemctl(&["is-enabled", UNIT_NAME])?;
    match String::from_utf8_lossy(&out.stdout).trim() {
        "enabled" => Some(true),
        "disabled" => Some(false),
        _ => None,
    }
}

pub fn set_autostart(enable: bool) -> bool {
    systemctl(&[if enable { "enable" } else { "disable" }, UNIT_NAME]).map(|o| o.status.success()).unwrap_or(false)
        && autostart() == Some(enable)
}

// ------------------------------------------------------------- system description

/// "Arch Linux · kernel 6.12.1-arch1-1".
pub fn os_version() -> String {
    let kernel =
        std::fs::read_to_string("/proc/sys/kernel/osrelease").map(|k| k.trim().to_string()).unwrap_or_default();
    let distro = std::fs::read_to_string("/etc/os-release").ok().and_then(|t| super::os_release_name(&t));
    match (distro, kernel.is_empty()) {
        (Some(d), false) => format!("{d} · kernel {kernel}"),
        (Some(d), true) => d,
        (None, false) => format!("Linux · kernel {kernel}"),
        (None, true) => "Linux".into(),
    }
}

/// "NVIDIA 580.95.05 open" (or "proprietary"), from the loaded kernel module; "" when none.
pub fn gpu_driver() -> String {
    std::fs::read_to_string("/proc/driver/nvidia/version")
        .ok()
        .and_then(|t| super::nvidia_proc_version(&t))
        .map(|(version, flavour)| format!("NVIDIA {version} {flavour}"))
        .unwrap_or_default()
}
