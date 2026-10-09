//! Converting the daemon's epoch-millisecond timestamps for display.
//!
//! Rendering takes a [`Clock`] instead of calling `Local::now()` so that
//! output is reproducible in tests.

use chrono::{DateTime, FixedOffset, Local, NaiveDate, NaiveDateTime, TimeZone};
use puzuz_core::model::Timestamp;

/// Time format for timestamps of today (pueue's default `status_time_format`).
pub const TIME_FORMAT: &str = "%H:%M:%S";
/// Time format for older timestamps (pueue's default `status_datetime_format`).
pub const DATETIME_FORMAT: &str = "%Y-%m-%d\n%H:%M:%S";

#[derive(Debug, Clone, Copy)]
enum Zone {
    Local,
    /// Only constructed by tests.
    #[cfg_attr(not(test), allow(dead_code))]
    Fixed(FixedOffset),
}

/// "Now" plus the time zone used for display.
#[derive(Debug, Clone, Copy)]
pub struct Clock {
    zone: Zone,
    now: DateTime<FixedOffset>,
}

impl Clock {
    /// The real clock in the local time zone.
    pub fn system() -> Self {
        Self {
            zone: Zone::Local,
            now: Local::now().fixed_offset(),
        }
    }

    /// A fixed clock in a fixed time zone (for tests).
    #[cfg(test)]
    pub fn fixed(now: DateTime<FixedOffset>) -> Self {
        Self {
            zone: Zone::Fixed(*now.offset()),
            now,
        }
    }

    pub fn now(&self) -> DateTime<FixedOffset> {
        self.now
    }

    /// Converts a daemon timestamp to a date in the display time zone.
    pub fn at(&self, millis: Timestamp) -> DateTime<FixedOffset> {
        let utc = DateTime::from_timestamp_millis(millis).unwrap_or_default();
        match self.zone {
            Zone::Local => utc.with_timezone(&Local).fixed_offset(),
            Zone::Fixed(offset) => utc.with_timezone(&offset),
        }
    }

    /// Interprets a wall-clock time in the display time zone.
    pub fn localize(&self, naive: NaiveDateTime) -> Option<DateTime<FixedOffset>> {
        match self.zone {
            Zone::Local => Local
                .from_local_datetime(&naive)
                .earliest()
                .map(|dt| dt.fixed_offset()),
            Zone::Fixed(offset) => offset.from_local_datetime(&naive).earliest(),
        }
    }

    pub fn today(&self) -> NaiveDate {
        self.now.date_naive()
    }

    pub fn start_of_day(&self, date: NaiveDate) -> Option<DateTime<FixedOffset>> {
        self.localize(date.and_hms_opt(0, 0, 0)?)
    }

    /// Only the time for timestamps of today, the date and time otherwise
    /// (as pueue's status table does).
    pub fn format_short(&self, millis: Timestamp) -> String {
        let at = self.at(millis);
        if at.date_naive() == self.today() {
            at.format(TIME_FORMAT).to_string()
        } else {
            at.format(DATETIME_FORMAT).to_string()
        }
    }

    /// A full date and time on one line.
    pub fn format_long(&self, millis: Timestamp) -> String {
        self.at(millis).format("%Y-%m-%d %H:%M:%S").to_string()
    }
}

/// Formats a byte count with binary units, e.g. `1.5 GiB`.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_format_hides_todays_date() {
        let now = DateTime::parse_from_rfc3339("2026-10-09T15:00:00+09:00").unwrap();
        let clock = Clock::fixed(now);
        let today = DateTime::parse_from_rfc3339("2026-10-09T08:30:00+09:00").unwrap();
        let earlier = DateTime::parse_from_rfc3339("2026-10-01T08:30:00+09:00").unwrap();
        assert_eq!(clock.format_short(today.timestamp_millis()), "08:30:00");
        assert_eq!(
            clock.format_short(earlier.timestamp_millis()),
            "2026-10-01\n08:30:00"
        );
    }

    #[test]
    fn human_bytes_uses_binary_units() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(80 * 1024 * 1024 * 1024), "80.0 GiB");
    }
}
