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

// ------------------------------------------------------------- system description
//
// Shown under the dashboard's header (and in bug reports): the OS and the NVIDIA driver.
// Read from the registry / procfs, not NVML: the protection path touches NVML only when it
// limits the GPU (safety rule 5), and this must also work in shutdown-only mode.

/// "Windows 10 Pro" is what ProductName still says on Windows 11; build 22000 is 11's first.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn windows_name(product: &str, build: u32) -> String {
    let product = product.trim();
    match product.strip_prefix("Windows 10") {
        Some(rest) if build >= 22000 => format!("Windows 11{rest}"),
        _ => product.to_string(),
    }
}

/// The display driver's file version as NVIDIA's release number: the last five digits of the
/// last two parts, "32.0.16.2114" -> "621.14".
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn nvidia_release(file_version: &str) -> Option<String> {
    let parts: Vec<&str> = file_version.trim().split('.').collect();
    let [.., a, b] = parts.as_slice() else { return None };
    if a.is_empty() || b.is_empty() || !a.chars().chain(b.chars()).all(|c| c.is_ascii_digit()) {
        return None;
    }
    let digits = format!("{a}{b:0>4}");
    let tail = &digits[digits.len().checked_sub(5)?..];
    Some(format!("{}.{}", &tail[..3], &tail[3..]))
}

/// /proc/driver/nvidia/version's first line -> (version, flavour): "open" for NVIDIA's open
/// kernel modules, "proprietary" for the closed ones.
#[cfg_attr(windows, allow(dead_code))]
pub(crate) fn nvidia_proc_version(text: &str) -> Option<(String, &'static str)> {
    let line = text.lines().find(|l| l.starts_with("NVRM version:"))?;
    let version = line
        .split_whitespace()
        .find(|t| t.contains('.') && t.split('.').all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())))?;
    let flavour = if line.contains("Open Kernel Module") { "open" } else { "proprietary" };
    Some((version.to_string(), flavour))
}

/// PRETTY_NAME from /etc/os-release ("Arch Linux", "Ubuntu 24.04.1 LTS").
#[cfg_attr(windows, allow(dead_code))]
pub(crate) fn os_release_name(text: &str) -> Option<String> {
    let v = text.lines().find_map(|l| l.strip_prefix("PRETTY_NAME="))?;
    let v = v.trim().trim_matches('"').trim();
    (!v.is_empty()).then(|| v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_11_is_named_by_its_build() {
        assert_eq!(windows_name("Windows 10 Pro", 28000), "Windows 11 Pro");
        assert_eq!(windows_name("Windows 10 Pro", 19045), "Windows 10 Pro");
        assert_eq!(windows_name("Windows Server 2025 Standard", 26100), "Windows Server 2025 Standard");
    }

    #[test]
    fn nvidia_release_from_the_file_version() {
        assert_eq!(nvidia_release("32.0.16.2114").as_deref(), Some("621.14"));
        assert_eq!(nvidia_release("32.0.15.6614").as_deref(), Some("566.14"));
        // a short last part keeps its leading zeros
        assert_eq!(nvidia_release("31.0.15.0302").as_deref(), Some("503.02"));
        assert_eq!(nvidia_release("31.0.15.302").as_deref(), Some("503.02"));
        assert_eq!(nvidia_release("garbage"), None);
        assert_eq!(nvidia_release("1.x"), None);
    }

    #[test]
    fn nvidia_flavour_from_procfs() {
        let open = "NVRM version: NVIDIA UNIX Open Kernel Module for x86_64  580.95.05  Release Build  (dvs-builder@U22-I3-AM02-24-3)  Thu Sep 25 2025\nGCC version:  gcc version 15.2.1";
        assert_eq!(nvidia_proc_version(open), Some(("580.95.05".into(), "open")));
        let closed = "NVRM version: NVIDIA UNIX x86_64 Kernel Module  565.77  Wed Nov 27 23:33:08 UTC 2024\n";
        assert_eq!(nvidia_proc_version(closed), Some(("565.77".into(), "proprietary")));
        assert_eq!(nvidia_proc_version("nothing here"), None);
    }

    #[test]
    fn distribution_name_from_os_release() {
        assert_eq!(
            os_release_name("NAME=\"Arch Linux\"\nPRETTY_NAME=\"Arch Linux\"\nID=arch").as_deref(),
            Some("Arch Linux")
        );
        assert_eq!(os_release_name("ID=x"), None);
    }
}
