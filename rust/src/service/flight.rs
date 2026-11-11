//! The last minute of per-pin samples, written to logs/ only when the guard acts.
//!
//! Memory only in normal operation. The dump is what a bug report or an RMA claim
//! needs: what every pin was doing in the minute before the GPU was limited or the
//! machine shut down.

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{Local, TimeZone};

use crate::config::log_dir;
use crate::logger;
use crate::sensor::Pin;

/// 60 s at the 0.5 s poll interval.
const SAMPLES: usize = 120;
const KEEP_FILES: usize = 10;

pub struct FlightRecorder {
    buf: VecDeque<(f64, Vec<f64>, Option<f64>)>,
    /// logs/; a temporary folder in the tests.
    dir: PathBuf,
}

impl FlightRecorder {
    pub fn new() -> Self {
        Self::in_dir(log_dir())
    }

    pub fn in_dir(dir: PathBuf) -> Self {
        FlightRecorder { buf: VecDeque::with_capacity(SAMPLES), dir }
    }

    pub fn record(&mut self, now: f64, pins: &[Pin], sag_deg: Option<f64>) {
        if self.buf.len() >= SAMPLES {
            self.buf.pop_front();
        }
        self.buf.push_back((now, pins.iter().map(|p| (p.amps * 1000.0).round() / 1000.0).collect(), sag_deg));
    }

    /// Write logs/flight_<stamp>_<event>.csv. Returns the path.
    pub fn dump(&self, event: &str) -> Option<PathBuf> {
        let first = self.buf.front()?;
        let n = first.1.len();
        let path = self.dir.join(format!("flight_{}_{event}.csv", Local::now().format("%Y%m%d_%H%M%S")));
        let mut out = String::from("time,");
        out += &(1..=n).map(|i| format!("pin{i}")).collect::<Vec<_>>().join(",");
        out += ",sag_deg\n";
        for (ts, vals, sag) in &self.buf {
            let secs = ts.floor() as i64;
            let ms = ((ts - ts.floor()) * 1000.0) as u32;
            let when = Local
                .timestamp_opt(secs, 0)
                .single()
                .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
                .unwrap_or_default();
            out += &format!(
                "{when}.{ms:03},{},{}\n",
                vals.iter().map(|v| format!("{v:.3}")).collect::<Vec<_>>().join(","),
                sag.map(|s| format!("{s:.2}")).unwrap_or_default()
            );
        }
        match fs::write(&path, out) {
            Ok(()) => {
                logger::info(format!("Flight recorder: {} samples written to {}", self.buf.len(), path.display()));
                prune(&self.dir);
                Some(path)
            }
            Err(e) => {
                logger::warn(format!("Flight recorder: could not write dump ({e})"));
                None
            }
        }
    }
}

/// Newest first; the stamp sorts chronologically.
pub fn dumps() -> Vec<String> {
    dumps_in(&log_dir())
}

fn dumps_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .map(|it| {
            it.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with("flight_") && n.ends_with(".csv"))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names.reverse();
    names
}

/// A dump's content by bare file name; anything that looks like a path is refused.
pub fn read(name: &str) -> Option<String> {
    read_in(&log_dir(), name)
}

fn read_in(dir: &Path, name: &str) -> Option<String> {
    let safe = name.starts_with("flight_")
        && name.ends_with(".csv")
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
        && !name.contains("..");
    if !safe {
        return None;
    }
    fs::read_to_string(dir.join(name)).ok()
}

fn prune(dir: &Path) {
    let mut names = dumps_in(dir);
    names.reverse(); // oldest first
    if names.len() > KEEP_FILES {
        for old in &names[..names.len() - KEEP_FILES] {
            let _ = fs::remove_file(dir.join(old));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh folder per test, removed at the end.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(name: &str) -> Self {
            let d = std::env::temp_dir().join(format!("ag-flight-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&d);
            fs::create_dir_all(&d).unwrap();
            TempDir(d)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn pins(amps: &[f64]) -> Vec<Pin> {
        amps.iter().enumerate().map(|(i, &a)| Pin { name: format!("Pin{}", i + 1), amps: a, volts: 12.0 }).collect()
    }

    #[test]
    fn keeps_exactly_the_last_minute() {
        let d = TempDir::new("minute");
        let mut f = FlightRecorder::in_dir(d.0.clone());
        for i in 0..(SAMPLES + 30) {
            f.record(1000.0 + i as f64 * 0.5, &pins(&[i as f64; 6]), None);
        }
        assert_eq!(f.buf.len(), SAMPLES, "60 s at 0.5 s");
        assert_eq!(f.buf.front().unwrap().1[0], 30.0, "the oldest 30 samples were dropped");
        assert_eq!(f.buf.back().unwrap().1[0], (SAMPLES + 29) as f64);
    }

    #[test]
    fn nothing_recorded_writes_nothing() {
        let d = TempDir::new("empty");
        assert!(FlightRecorder::in_dir(d.0.clone()).dump("engaged").is_none());
        assert!(dumps_in(&d.0).is_empty());
    }

    #[test]
    fn the_dump_is_a_csv_of_every_pin_and_the_sag() {
        let d = TempDir::new("csv");
        let mut f = FlightRecorder::in_dir(d.0.clone());
        f.record(1_000_000.25, &pins(&[1.0, 2.0, 3.0, 4.0, 5.0, 9.87654]), Some(0.5));
        f.record(1_000_000.75, &pins(&[1.0; 6]), None);
        let path = f.dump("engaged").unwrap();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("flight_") && name.ends_with("_engaged.csv"), "{name}");
        let text = read_in(&d.0, &name).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "time,pin1,pin2,pin3,pin4,pin5,pin6,sag_deg");
        assert_eq!(lines.len(), 3);
        assert!(lines[1].ends_with(".250,1.000,2.000,3.000,4.000,5.000,9.877,0.50"), "{}", lines[1]);
        assert!(lines[2].ends_with(".750,1.000,1.000,1.000,1.000,1.000,1.000,"), "no sag: an empty column");
    }

    #[test]
    fn keeps_the_newest_ten_dumps() {
        let d = TempDir::new("prune");
        for i in 0..14 {
            fs::write(d.0.join(format!("flight_20261101_0000{i:02}_engaged.csv")), "x").unwrap();
        }
        fs::write(d.0.join("astralguard.log"), "not a dump").unwrap();
        prune(&d.0);
        let left = dumps_in(&d.0);
        assert_eq!(left.len(), KEEP_FILES);
        assert_eq!(left.first().unwrap(), "flight_20261101_000013_engaged.csv", "newest first");
        assert_eq!(left.last().unwrap(), "flight_20261101_000004_engaged.csv", "the oldest four went");
        assert!(d.0.join("astralguard.log").exists(), "other files are left alone");
    }

    #[test]
    fn reading_refuses_anything_but_a_dump_name() {
        let d = TempDir::new("read");
        fs::write(d.0.join("flight_x.csv"), "ok").unwrap();
        assert_eq!(read_in(&d.0, "flight_x.csv").as_deref(), Some("ok"));
        for bad in ["../config.ini", "flight_../../etc/passwd.csv", "/etc/passwd", "config.ini", "flight_a/b.csv"] {
            assert!(read_in(&d.0, bad).is_none(), "{bad} must be refused");
        }
    }
}
