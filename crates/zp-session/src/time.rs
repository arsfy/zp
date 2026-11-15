//! UTC timestamp formatting without a date-time dependency.

/// `(year, month, day, hour, minute, second, millisecond)` in UTC.
fn civil(ms: i64) -> (i64, i64, i64, i64, i64, i64, i64) {
    let (seconds, millis) = (ms.div_euclid(1000), ms.rem_euclid(1000));
    let (days, rest) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (
        year,
        month,
        day,
        rest / 3600,
        rest % 3600 / 60,
        rest % 60,
        millis,
    )
}

/// ISO 8601, e.g. `2026-11-15T08:43:21.123Z`.
pub fn format_iso(ms: i64) -> String {
    let (y, mo, d, h, mi, s, ml) = civil(ms);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{ml:03}Z")
}

/// File-name safe, e.g. `2026-11-15T08-43-21-123Z`.
pub fn format_for_file_name(ms: i64) -> String {
    let (y, mo, d, h, mi, s, ml) = civil(ms);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}-{mi:02}-{s:02}-{ml:03}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_utc() {
        assert_eq!(format_iso(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(format_iso(1_763_196_201_123), "2025-11-15T08:43:21.123Z");
        assert_eq!(
            format_for_file_name(951_782_400_000),
            "2000-02-29T00-00-00-000Z"
        );
    }
}
