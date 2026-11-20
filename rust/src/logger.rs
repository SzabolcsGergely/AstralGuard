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
    /// Rotate before a line would take the file past this.
    max_bytes: u64,
}

impl Sink {
    fn open(path: &Path, min: Level, max_bytes: u64) -> Sink {
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let file = OpenOptions::new().create(true).append(true).open(path).ok();
        Sink { path: path.to_path_buf(), file, min, max_bytes }
    }

    fn write(&mut self, level: Level, msg: &str) {
        if level < self.min {
            return;
        }
        let line = format!("{} | {} | {}\n", crate::clock::human_ms(&crate::clock::now()), level.name(), msg);
        let size = self.file.as_ref().and_then(|f| f.metadata().ok()).map(|m| m.len()).unwrap_or(0);
        if size + line.len() as u64 > self.max_bytes {
            self.rotate();
        }
        if let Some(f) = self.file.as_mut() {
            let _ = f.write_all(line.as_bytes());
            let _ = f.flush();
        }
    }

    /// astralguard.log -> .1 -> .2 ... -> .5, the oldest dropped.
    fn rotate(&mut self) {
        self.file = None;
        let numbered = |n: u32| PathBuf::from(format!("{}.{n}", self.path.display()));
        let _ = fs::remove_file(numbered(BACKUPS));
        for n in (1..BACKUPS).rev() {
            let _ = fs::rename(numbered(n), numbered(n + 1));
        }
        let _ = fs::rename(&self.path, numbered(1));
        self.file = OpenOptions::new().create(true).append(true).open(&self.path).ok();
    }
}

static SINK: Mutex<Option<Sink>> = Mutex::new(None);

pub fn init(path: &Path, min: Level) {
    *SINK.lock().unwrap_or_else(|e| e.into_inner()) = Some(Sink::open(path, min, MAX_BYTES));
}

pub fn path() -> Option<PathBuf> {
    SINK.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|s| s.path.clone())
}

pub fn log(level: Level, msg: &str) {
    if let Some(sink) = SINK.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        sink.write(level, msg);
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ag-logger-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    fn read(p: impl AsRef<Path>) -> String {
        fs::read_to_string(p).unwrap_or_default()
    }

    #[test]
    fn lines_have_the_stamp_level_and_message_and_quiet_levels_are_dropped() {
        let dir = temp_dir("lines");
        let path = dir.join("logs").join("astralguard.log");
        let mut sink = Sink::open(&path, Level::Info, MAX_BYTES);
        sink.write(Level::Debug, "not shown");
        for (level, name) in
            [(Level::Info, "INFO"), (Level::Warning, "WARNING"), (Level::Error, "ERROR"), (Level::Critical, "CRITICAL")]
        {
            sink.write(level, "hello");
            let text = read(&path);
            let last = text.lines().last().unwrap();
            assert_eq!(&last[23..], format!(" | {name} | hello"), "{last}");
        }
        assert!(!read(&path).contains("not shown"));
        assert_eq!(read(&path).lines().count(), 4);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_full_log_rotates_and_keeps_five_old_files() {
        let dir = temp_dir("rotate");
        let path = dir.join("astralguard.log");
        // A line is 23 + " | INFO | " + 3 + newline = 37 bytes: two fit in 80.
        let mut sink = Sink::open(&path, Level::Debug, 80);
        for i in 0..14 {
            sink.write(Level::Info, &format!("{i:03}"));
        }
        // 14 lines, two per file: 012-013 current, then 010-011 in .1 ... 002-003 in .5.
        assert!(read(&path).contains("012") && read(&path).contains("013"));
        assert_eq!(read(&path).lines().count(), 2);
        let backup = |n: u32| read(format!("{}.{n}", path.display()));
        assert!(backup(1).contains("010") && backup(1).contains("011"), "{}", backup(1));
        assert!(backup(5).contains("002") && backup(5).contains("003"), "{}", backup(5));
        assert!(!Path::new(&format!("{}.6", path.display())).exists(), "five old files, not more");
        let all: String = (1..=5).map(backup).collect::<String>() + &read(&path);
        assert!(!all.contains("| 000") && !all.contains("| 001"), "the oldest dropped");
        let _ = fs::remove_dir_all(dir);
    }
}
