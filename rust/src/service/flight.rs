//! The last minute of per-pin samples, written to logs/ only when the guard acts.
//!
//! Memory only in normal operation. The dump is what a bug report or an RMA claim
//! needs: what every pin was doing in the minute before the GPU was limited or the
//! machine shut down.

use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;

use chrono::{Local, TimeZone};

use crate::config::log_dir;
use crate::logger;
use crate::sensor::Pin;

/// 60 s at the 0.5 s poll interval.
const SAMPLES: usize = 120;
const KEEP_FILES: usize = 10;

pub struct FlightRecorder {
    buf: VecDeque<(f64, Vec<f64>, Option<f64>)>,
}

impl FlightRecorder {
    pub fn new() -> Self {
        FlightRecorder { buf: VecDeque::with_capacity(SAMPLES) }
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
        let path = log_dir().join(format!("flight_{}_{event}.csv", Local::now().format("%Y%m%d_%H%M%S")));
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
                prune();
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
    let mut names: Vec<String> = fs::read_dir(log_dir())
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
    let safe = name.starts_with("flight_")
        && name.ends_with(".csv")
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
        && !name.contains("..");
    if !safe {
        return None;
    }
    fs::read_to_string(log_dir().join(name)).ok()
}

fn prune() {
    let mut names = dumps();
    names.reverse(); // oldest first
    if names.len() > KEEP_FILES {
        for old in &names[..names.len() - KEEP_FILES] {
            let _ = fs::remove_file(log_dir().join(old));
        }
    }
}
