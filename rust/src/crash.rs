//! A panic must not pass silently: the message, location and backtrace go to the log
//! and to logs/crash_trace.log before the process dies (the watchdog then restarts it).

use std::io::Write;
use std::path::Path;

pub fn install(log_dir: &Path) {
    let trace = log_dir.join("crash_trace.log");
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("unnamed");
        let backtrace = std::backtrace::Backtrace::force_capture();
        let msg = format!("PANIC in thread '{name}': {info}");
        crate::logger::critical(&msg);
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&trace) {
            let _ = writeln!(
                f,
                "=== {} pid {} ===\n{msg}\n{backtrace}",
                chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                std::process::id()
            );
        }
        default(info);
    }));
}
