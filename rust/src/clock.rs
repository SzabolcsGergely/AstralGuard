//! Local wall-clock time for log lines, file names and the sag history.
//!
//! On jiff (chrono is unmaintained, RUSTSEC-2026-0543). Only formatting and calendar dates
//! live here; the guard's timers run on `crate::now()`, seconds since the epoch, and never
//! on the local clock, so a time-zone or DST change can't stretch or cut a protection window.

use jiff::civil::Date;
use jiff::tz::TimeZone;
use jiff::{Timestamp, ZonedDateTime as Zoned};

/// The local time now.
pub fn now() -> Zoned {
    Zoned::now()
}

/// The local time of a Unix timestamp in seconds; None outside jiff's range.
pub fn local(secs: i64) -> Option<Zoned> {
    Timestamp::from_second(secs).ok().map(|t| t.to_zoned_datetime(TimeZone::system()))
}

/// "2026-11-20 14:05:09".
pub fn human(t: &Zoned) -> String {
    t.strftime("%Y-%m-%d %H:%M:%S").to_string()
}

/// "2026-11-20 14:05:09,123", the log line's stamp.
pub fn human_ms(t: &Zoned) -> String {
    format!("{},{:03}", human(t), t.millisecond())
}

/// A calendar date as the sag history writes it ("2026-11-20").
pub fn day(t: &Zoned) -> String {
    t.date().to_string()
}

/// Parse a date written by `day`.
pub fn parse_day(s: &str) -> Option<Date> {
    s.trim().parse().ok()
}

/// Whole days from `earlier` to `later` (negative when `earlier` is the later one).
pub fn days_between(earlier: Date, later: Date) -> i64 {
    later.since(earlier).map(|span| i64::from(span.get_days())).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_round_trip_and_count() {
        let a = parse_day("2026-10-25").unwrap();
        let b = parse_day("2026-11-20").unwrap();
        assert_eq!(days_between(a, b), 26);
        assert_eq!(days_between(b, a), -26);
        assert_eq!(parse_day("2026-02-30"), None);
        assert_eq!(parse_day("garbage"), None);
        let t = local(1_795_000_000).unwrap();
        assert_eq!(parse_day(&day(&t)), Some(t.date()));
    }

    #[test]
    fn stamps_have_the_old_shape() {
        let t = local(1_795_000_000).unwrap();
        let h = human(&t);
        assert_eq!(h.len(), 19, "{h}");
        assert_eq!(&h[4..5], "-");
        assert_eq!(&h[10..11], " ");
        let ms = human_ms(&t);
        assert_eq!(ms.len(), 23, "{ms}");
        assert!(ms.ends_with(",000"), "{ms}");
    }
}
