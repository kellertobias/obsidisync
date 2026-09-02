use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn unix_now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// Formats a unix timestamp (seconds) as an RFC 3339 UTC string such as `2026-09-02T10:15:00Z`.
pub fn rfc3339_from_unix(seconds: u64) -> String {
    let days = seconds / 86_400;
    let remainder = seconds % 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        remainder / 3600,
        (remainder % 3600) / 60,
        remainder % 60
    )
}

/// Formats a unix timestamp (seconds) as an HTTP date such as `Wed, 02 Sep 2026 10:15:00 GMT`.
pub fn http_date_from_unix(seconds: u64) -> String {
    httpdate::fmt_http_date(UNIX_EPOCH + Duration::from_secs(seconds))
}

pub fn unix_seconds_from_millis(millis: i64) -> u64 {
    if millis <= 0 {
        0
    } else {
        (millis / 1000) as u64
    }
}

// Howard Hinnant's days-to-civil algorithm, so the server does not need a time crate.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_rfc3339_dates() {
        assert_eq!(rfc3339_from_unix(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_from_unix(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339_from_unix(1_788_689_700), "2026-09-06T10:15:00Z");
    }

    #[test]
    fn formats_http_dates() {
        assert_eq!(http_date_from_unix(0), "Thu, 01 Jan 1970 00:00:00 GMT");
    }
}
