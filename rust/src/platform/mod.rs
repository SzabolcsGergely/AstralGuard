//! What differs between Windows and Linux. Each function has one meaning on both.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "linux")]
pub use linux::*;
#[cfg(windows)]
pub use windows::*;

/// Severity for the system log (Windows Event Log / journald via stderr).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    Info,
    Warning,
    Error,
}

/// Shut the machine down after `delay_sec`, closing applications without asking.
/// Absolute paths only: the one safety-critical action must not depend on PATH.
pub fn shutdown_delay_sec() -> u32 {
    5
}
