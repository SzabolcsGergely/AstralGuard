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

// ------------------------------------------------------------- stop signals
//
// systemd stops the service with SIGTERM: `systemctl stop` and `restart`, and every reboot.
// Without this the process died on the spot and skipped the loop's own exit, so a GPU limit
// stayed on until the next start and the day's sag history row was lost. SIGTERM and SIGINT
// (Ctrl-C in a terminal) are blocked in every thread and taken by one thread with sigwait:
// nothing runs inside a signal handler.

fn stop_signal_set() -> libc::sigset_t {
    // SAFETY: sigemptyset / sigaddset only fill in the set they are given.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::sigaddset(&mut set, libc::SIGINT);
        set
    }
}

/// Block SIGTERM and SIGINT in this thread and every thread started from it afterwards.
/// Call it first, before any thread exists: a thread that does not block them dies of them.
pub fn block_stop_signals() {
    let set = stop_signal_set();
    // SAFETY: changes only this thread's signal mask.
    unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) };
}

/// Call `f` with the signal's name on the first SIGTERM or SIGINT, from a thread of its own.
pub fn on_stop_signal(f: impl FnOnce(&'static str) + Send + 'static) -> std::io::Result<()> {
    stop_signal_thread(f).map(drop)
}

fn stop_signal_thread(f: impl FnOnce(&'static str) + Send + 'static) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new().name("stop-signals".into()).spawn(move || {
        block_stop_signals(); // already inherited when called as intended; never wait unblocked
        let set = stop_signal_set();
        let mut sig = 0;
        // SAFETY: waits for one of the blocked signals in `set`.
        if unsafe { libc::sigwait(&set, &mut sig) } == 0 {
            f(if sig == libc::SIGTERM { "SIGTERM" } else { "SIGINT" });
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::thread::JoinHandleExt;
    use std::sync::mpsc;
    use std::time::Duration;

    /// The signal goes to the waiting thread only (pthread_kill), which was born with it
    /// blocked, so no other thread of the test binary can die of it.
    fn delivered(sig: libc::c_int) -> &'static str {
        block_stop_signals(); // this test's thread, inherited by the waiter
        let (tx, rx) = mpsc::channel();
        let waiter = stop_signal_thread(move |name| tx.send(name).unwrap()).unwrap();
        // SAFETY: the target thread is alive until it has received the signal.
        assert_eq!(unsafe { libc::pthread_kill(waiter.as_pthread_t(), sig) }, 0);
        let name = rx.recv_timeout(Duration::from_secs(5)).expect("the signal was not taken");
        waiter.join().unwrap();
        name
    }

    #[test]
    fn systemds_stop_signal_is_handed_on_not_fatal() {
        assert_eq!(delivered(libc::SIGTERM), "SIGTERM");
    }

    #[test]
    fn ctrl_c_in_a_terminal_is_handed_on_too() {
        assert_eq!(delivered(libc::SIGINT), "SIGINT");
    }
}
