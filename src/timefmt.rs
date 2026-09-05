//! Conversion of ChatGPT's floating-point Unix timestamps into displayable time.

use chrono::{DateTime, Local, SecondsFormat, Utc};

/// How timestamps are rendered in human-facing output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TimeZoneMode {
    /// RFC 3339 in UTC, e.g. `2026-09-05T11:32:04Z`. The default: stable,
    /// sortable, and reproducible across machines.
    #[default]
    Utc,
    /// RFC 3339 in the machine's local zone. Opt-in only.
    Local,
}

/// Convert a ChatGPT timestamp (fractional Unix seconds) to a UTC datetime.
///
/// Returns `None` for absent, non-finite, or out-of-range values rather than
/// panicking — exports in the wild do contain `null`.
pub fn to_datetime(timestamp: Option<f64>) -> Option<DateTime<Utc>> {
    let raw = timestamp?;
    if !raw.is_finite() {
        return None;
    }
    let secs = raw.trunc();
    if secs < i64::MIN as f64 || secs > i64::MAX as f64 {
        return None;
    }
    let nanos = ((raw - secs) * 1e9).round().clamp(0.0, 999_999_999.0) as u32;
    DateTime::from_timestamp(secs as i64, nanos)
}

/// Full RFC 3339 rendering, or `-` when the timestamp is missing/invalid.
pub fn format(timestamp: Option<f64>, mode: TimeZoneMode) -> String {
    match to_datetime(timestamp) {
        None => "-".to_string(),
        Some(dt) => match mode {
            TimeZoneMode::Utc => dt.to_rfc3339_opts(SecondsFormat::Secs, true),
            TimeZoneMode::Local => dt
                .with_timezone(&Local)
                .to_rfc3339_opts(SecondsFormat::Secs, false),
        },
    }
}

/// Compact `YYYY-MM-DD HH:MM` rendering for table columns.
pub fn format_short(timestamp: Option<f64>, mode: TimeZoneMode) -> String {
    match to_datetime(timestamp) {
        None => "-".to_string(),
        Some(dt) => match mode {
            TimeZoneMode::Utc => dt.format("%Y-%m-%d %H:%M").to_string(),
            TimeZoneMode::Local => dt
                .with_timezone(&Local)
                .format("%Y-%m-%d %H:%M")
                .to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_fractional_seconds() {
        assert_eq!(
            format(Some(1_757_071_924.421), TimeZoneMode::Utc),
            "2025-09-05T11:32:04Z"
        );
    }

    #[test]
    fn missing_and_invalid_timestamps_do_not_panic() {
        assert_eq!(format(None, TimeZoneMode::Utc), "-");
        assert_eq!(format(Some(f64::NAN), TimeZoneMode::Utc), "-");
        assert_eq!(format(Some(f64::INFINITY), TimeZoneMode::Utc), "-");
        assert_eq!(format(Some(1e300), TimeZoneMode::Utc), "-");
        assert_eq!(format(Some(-1e300), TimeZoneMode::Utc), "-");
    }

    #[test]
    fn epoch_zero_is_a_real_time_not_an_error() {
        assert_eq!(format(Some(0.0), TimeZoneMode::Utc), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn short_format_is_table_friendly() {
        assert_eq!(
            format_short(Some(1_757_071_924.0), TimeZoneMode::Utc),
            "2025-09-05 11:32"
        );
    }
}
