//! Shared CLI time-argument parsing: `slice`'s strict RFC 3339, plus the wider
//! `--since`/`--until` form — RFC 3339, a bare `YYYY-MM-DD` date, or a duration like `2d`.

use chrono::{DateTime, Duration, FixedOffset, Local, NaiveDate, NaiveDateTime, TimeZone};
use once_cell::sync::Lazy;
use regex::Regex;

use crate::output::{py_repr, usage_error, CliExit};

static DATE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^\d{4}-\d{2}-\d{2}$").expect("date pattern compiles"));
static DURATION_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^(\d+)([smhdw])$").expect("duration pattern compiles"));

pub fn parse_rfc3339(
    option: &str,
    value: &str,
    usage: &str,
    help_path: &str,
) -> Result<DateTime<FixedOffset>, CliExit> {
    if let Ok(stamp) = DateTime::parse_from_rfc3339(value) {
        return Ok(stamp);
    }
    let naive = NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f")
        .or_else(|_| NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S%.f"))
        .is_ok()
        || NaiveDate::parse_from_str(value, "%Y-%m-%d").is_ok();
    if naive {
        return Err(usage_error(
            usage,
            help_path,
            &format!(
                "invalid {option} {}; RFC 3339 requires a UTC offset",
                py_repr(value)
            ),
        ));
    }
    Err(usage_error(
        usage,
        help_path,
        &format!(
            "invalid {option} {}; expected an RFC 3339 timestamp",
            py_repr(value)
        ),
    ))
}

fn unit_seconds(unit: char) -> i64 {
    match unit {
        's' => 1,
        'm' => 60,
        'h' => 3_600,
        'd' => 86_400,
        'w' => 604_800,
        _ => unreachable!("duration regex only matches [smhdw]"),
    }
}

fn no_match_message(option: &str, value: &str) -> String {
    format!(
        "invalid {option} {}; expected an RFC 3339 timestamp, a YYYY-MM-DD date, or a relative duration like 2d",
        py_repr(value)
    )
}

pub fn parse_time(
    option: &str,
    value: &str,
    now: DateTime<Local>,
    usage: &str,
    help_path: &str,
) -> Result<DateTime<FixedOffset>, CliExit> {
    if let Ok(stamp) = DateTime::parse_from_rfc3339(value) {
        return Ok(stamp);
    }
    if DATE_RE.is_match(value) {
        if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
            let midnight = date.and_hms_opt(0, 0, 0).expect("midnight is a valid time");
            if let Some(local) = Local.from_local_datetime(&midnight).earliest() {
                return Ok(local.fixed_offset());
            }
        }
    }
    if let Some(caps) = DURATION_RE.captures(value) {
        let unit = caps[2]
            .chars()
            .next()
            .expect("duration regex captures one unit char");
        if let Some(resolved) = caps[1]
            .parse::<i64>()
            .ok()
            .and_then(|n| n.checked_mul(unit_seconds(unit)))
            .and_then(Duration::try_seconds)
            .and_then(|duration| now.checked_sub_signed(duration))
        {
            return Ok(resolved.fixed_offset());
        }
    }
    Err(usage_error(
        usage,
        help_path,
        &no_match_message(option, value),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const USAGE: &str = "cc-transcript test [OPTIONS]";
    const HELP_PATH: &str = "cc-transcript test";

    fn fixed_now() -> DateTime<Local> {
        DateTime::parse_from_rfc3339("2026-01-01T12:00:00Z")
            .unwrap()
            .with_timezone(&Local)
    }

    #[test]
    fn rfc3339_offset_is_preserved() {
        let stamp = parse_time(
            "--since",
            "2020-06-01T10:00:00+05:30",
            fixed_now(),
            USAGE,
            HELP_PATH,
        )
        .unwrap();
        assert_eq!(stamp.offset().local_minus_utc(), 5 * 3_600 + 1_800);
        assert_eq!(
            stamp.naive_local(),
            NaiveDate::from_ymd_opt(2020, 6, 1)
                .unwrap()
                .and_hms_opt(10, 0, 0)
                .unwrap()
        );
    }

    #[test]
    fn bare_date_resolves_to_local_midnight_independent_of_now() {
        let resolved = parse_time("--since", "2024-03-15", fixed_now(), USAGE, HELP_PATH).unwrap();
        assert_eq!(
            resolved.naive_local(),
            NaiveDate::from_ymd_opt(2024, 3, 15)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap()
        );
    }

    #[test]
    fn leading_zero_date_parses() {
        let resolved = parse_time("--since", "2026-07-05", fixed_now(), USAGE, HELP_PATH).unwrap();
        assert_eq!(
            resolved.naive_local(),
            NaiveDate::from_ymd_opt(2026, 7, 5)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap()
        );
    }

    #[test]
    fn each_duration_unit_subtracts_from_now() {
        let now = fixed_now();
        for (input, seconds) in [
            ("5s", 5),
            ("3m", 180),
            ("2h", 7_200),
            ("4d", 345_600),
            ("1w", 604_800),
        ] {
            let expected = (now - Duration::seconds(seconds)).fixed_offset();
            let resolved = parse_time("--since", input, now, USAGE, HELP_PATH).unwrap();
            assert_eq!(resolved, expected, "{input:?}");
        }
    }

    #[test]
    fn garbage_input_reports_exit_2_with_the_expected_message() {
        assert_eq!(
            no_match_message("--since", "not-a-time"),
            "invalid --since 'not-a-time'; expected an RFC 3339 timestamp, a YYYY-MM-DD date, or a relative duration like 2d"
        );
        let CliExit(code) =
            parse_time("--since", "not-a-time", fixed_now(), USAGE, HELP_PATH).unwrap_err();
        assert_eq!(code, 2);
    }

    #[test]
    fn oversized_duration_reports_exit_2_instead_of_panicking() {
        // "999999999999999w" overflows the `n * unit_seconds(unit)` multiply; "1000000000000w"
        // fits the multiply but overflows chrono's `Duration` range on the subtract.
        for input in ["999999999999999w", "1000000000000w"] {
            assert_eq!(
                no_match_message("--since", input),
                format!(
                    "invalid --since {}; expected an RFC 3339 timestamp, a YYYY-MM-DD date, or a relative duration like 2d",
                    py_repr(input)
                ),
                "{input:?}"
            );
            let CliExit(code) =
                parse_time("--since", input, fixed_now(), USAGE, HELP_PATH).unwrap_err();
            assert_eq!(code, 2, "{input:?}");
        }
    }
}
