//! The guard's log file, one line per event:
//! `2026-11-02 11:04:04,609 | INFO | message`. Rotates at 2 MB, keeping 5 old files.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const MAX_BYTES: u64 = 2 * 1024 * 1024;
const BACKUPS: u32 = 5;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Level {
    Debug,
    Info,
    Warning,
    Error,
    Critical,
}

impl Level {
    fn name(self) -> &'static str {
        match self {
            Level::Debug => "DEBUG",
            Level::Info => "INFO",
            Level::Warning => "WARNING",
            Level::Error => "ERROR",
            Level::Critical => "CRITICAL",
        }
    }
}

struct Sink {
    path: PathBuf,
    file: Option<File>,
    min: Level,
}

static SINK: Mutex<Option<Sink>> = Mutex::new(None);

pub fn init(path: &Path, min: Level) {
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let file = OpenOptions::new().create(true).append(true).open(path).ok();
    *SINK.lock().unwrap_or_else(|e| e.into_inner()) = Some(Sink { path: path.to_path_buf(), file, min });
}

pub fn path() -> Option<PathBuf> {
    SINK.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|s| s.path.clone())
}

pub fn log(level: Level, msg: &str) {
    let mut guard = SINK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(sink) = guard.as_mut() else { return };
    if level < sink.min {
        return;
    }
    let now = chrono::Local::now();
    let line = format!("{} | {} | {}\n", now.format("%Y-%m-%d %H:%M:%S,%3f"), level.name(), msg);
    if sink.file.as_ref().and_then(|f| f.metadata().ok()).map(|m| m.len()).unwrap_or(0) + line.len() as u64 > MAX_BYTES
    {
        rotate(sink);
    }
    if let Some(f) = sink.file.as_mut() {
        let _ = f.write_all(line.as_bytes());
        let _ = f.flush();
    }
}

fn rotate(sink: &mut Sink) {
    sink.file = None;
    let numbered = |n: u32| PathBuf::from(format!("{}.{n}", sink.path.display()));
    let _ = fs::remove_file(numbered(BACKUPS));
    for n in (1..BACKUPS).rev() {
        let _ = fs::rename(numbered(n), numbered(n + 1));
    }
    let _ = fs::rename(&sink.path, numbered(1));
    sink.file = OpenOptions::new().create(true).append(true).open(&sink.path).ok();
}

pub fn debug(msg: impl AsRef<str>) {
    log(Level::Debug, msg.as_ref())
}
pub fn info(msg: impl AsRef<str>) {
    log(Level::Info, msg.as_ref())
}
pub fn warn(msg: impl AsRef<str>) {
    log(Level::Warning, msg.as_ref())
}
pub fn error(msg: impl AsRef<str>) {
    log(Level::Error, msg.as_ref())
}
pub fn critical(msg: impl AsRef<str>) {
    log(Level::Critical, msg.as_ref())
}
