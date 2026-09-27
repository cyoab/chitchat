//! Small text helpers shared by tool results, hook digests and the CLI.

use crate::db::now_ms;

/// "just now", "5m ago", "3h ago", "2d ago".
pub fn ago(ms: i64) -> String {
    let secs = (now_ms() - ms).max(0) / 1000;
    match secs {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

/// Local wall-clock time as "HH:MM".
pub fn clock(ms: i64) -> String {
    let (h, m) = local_hour_minute(ms);
    format!("{h:02}:{m:02}")
}

#[cfg(unix)]
fn local_hour_minute(ms: i64) -> (i64, i64) {
    let secs = (ms / 1000) as libc::time_t;
    let mut tm = std::mem::MaybeUninit::<libc::tm>::zeroed();
    // SAFETY: localtime_r only writes into the provided tm.
    let ok = unsafe { !libc::localtime_r(&secs, tm.as_mut_ptr()).is_null() };
    if ok {
        // SAFETY: localtime_r succeeded, so tm is initialized.
        let tm = unsafe { tm.assume_init() };
        (tm.tm_hour as i64, tm.tm_min as i64)
    } else {
        utc_hour_minute(ms)
    }
}

#[cfg(not(unix))]
fn local_hour_minute(ms: i64) -> (i64, i64) {
    utc_hour_minute(ms)
}

fn utc_hour_minute(ms: i64) -> (i64, i64) {
    let secs_of_day = (ms / 1000).rem_euclid(86_400);
    (secs_of_day / 3600, secs_of_day % 3600 / 60)
}

/// RFC 3339 UTC timestamp, e.g. "2026-09-27T14:03:05Z".
pub fn iso_utc(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (days, secs_of_day) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, mo, d) = civil_from_days(days);
    format!(
        "{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}Z",
        secs_of_day / 3600,
        secs_of_day % 3600 / 60,
        secs_of_day % 60
    )
}

/// Days since 1970-01-01 to (year, month, day). Howard Hinnant's algorithm.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// Truncates to at most `max` characters, marking the cut with "…".
pub fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Indents every line after the first, for multi-line bodies in lists.
pub fn indent_continuation(text: &str, prefix: &str) -> String {
    text.trim_end()
        .lines()
        .enumerate()
        .map(|(i, line)| {
            if i == 0 {
                line.to_string()
            } else {
                format!("{prefix}{line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_utc_formats_known_instants() {
        assert_eq!(iso_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_utc(951_782_400_000), "2000-02-29T00:00:00Z");
        assert_eq!(iso_utc(1_790_517_785_000), "2026-09-27T14:03:05Z");
    }

    #[test]
    fn truncate_counts_characters() {
        assert_eq!(truncate("héllo", 10), "héllo");
        assert_eq!(truncate("héllo wörld", 6), "héllo…");
    }
}
