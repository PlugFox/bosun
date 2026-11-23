//! The time arguments `--since` and `--until`, and the display of a millisecond timestamp.
//!
//! A bound is either
//!
//! - an RFC 3339 date-time, `2026-11-01T09:30:00Z` or `2026-11-01T12:30:00.250+03:00` (the
//!   separator may be `T`, `t` or a space; seconds may carry a fraction, kept to the
//!   millisecond and truncated beyond it; a leap second `:60` is refused), or a bare date
//!   `2026-11-01`, which is midnight UTC; or
//! - a duration before now: a whole number and a unit, `s`, `m`, `h`, `d` (24 h) or `w`
//!   (7 d): `90m`, `36h`, `7d`, `2w`.
//!
//! "Now" is read once per command, so `--since 7d --until 1d` is an exact six-day range.

const SECOND: i64 = 1000;
const MINUTE: i64 = 60 * SECOND;
const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;

/// A parsed `--since` or `--until`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Bound {
    /// An instant, in milliseconds UTC.
    At(i64),
    /// This many milliseconds before now.
    Ago(i64),
}

impl Bound {
    /// The instant this bound names, `now_ms` being now.
    pub(crate) fn resolve(self, now_ms: i64) -> i64 {
        match self {
            Self::At(ms) => ms,
            Self::Ago(ms) => now_ms.saturating_sub(ms),
        }
    }
}

/// Parses a bound; see the module documentation.
///
/// # Errors
///
/// A text that is neither form, with what was expected.
pub(crate) fn parse_bound(text: &str) -> Result<Bound, String> {
    if let Some(ago) = parse_ago(text) {
        return ago.map(Bound::Ago);
    }
    parse_rfc3339(text).map(Bound::At).ok_or_else(|| {
        format!(
            "`{text}` is neither an RFC 3339 date-time (2026-11-01T09:30:00Z), a date \
             (2026-11-01) nor a duration before now (90m, 36h, 7d, 2w)"
        )
    })
}

/// `Some` when `text` has the shape of a duration (digits and a unit letter).
fn parse_ago(text: &str) -> Option<Result<i64, String>> {
    let unit = text.chars().last()?;
    let digits = &text[..text.len() - unit.len_utf8()];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let scale = match unit {
        's' => SECOND,
        'm' => MINUTE,
        'h' => HOUR,
        'd' => DAY,
        'w' => 7 * DAY,
        _ => return None,
    };
    Some(
        digits
            .parse::<i64>()
            .ok()
            .and_then(|n| n.checked_mul(scale))
            .ok_or_else(|| format!("`{text}` is too long a duration")),
    )
}

/// Days since 1970-01-01 of a proleptic Gregorian date (H. Hinnant's `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let doy = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The date of a day number of [`days_from_civil`].
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// A fixed-width decimal field of `text` at `at`, or `None`.
fn number(text: &[u8], at: usize, width: usize) -> Option<i64> {
    let field = text.get(at..at + width)?;
    field.iter().try_fold(0, |n, &b| {
        b.is_ascii_digit().then(|| n * 10 + i64::from(b - b'0'))
    })
}

fn parse_rfc3339(text: &str) -> Option<i64> {
    let b = text.as_bytes();
    let year = number(b, 0, 4)?;
    let month = number(b, 5, 2)?;
    let day = number(b, 8, 2)?;
    if b.get(4) != Some(&b'-') || b.get(7) != Some(&b'-') {
        return None;
    }
    if !(1..=12).contains(&month) || !(1..=days_in_month(year, month)).contains(&day) {
        return None;
    }
    let date = days_from_civil(year, month, day) * DAY;
    if b.len() == 10 {
        return Some(date);
    }
    if !matches!(b.get(10), Some(b'T' | b't' | b' '))
        || b.get(13) != Some(&b':')
        || b.get(16) != Some(&b':')
    {
        return None;
    }
    let (hour, minute, second) = (number(b, 11, 2)?, number(b, 14, 2)?, number(b, 17, 2)?);
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let mut at = 19;
    let mut millis = 0;
    if b.get(at) == Some(&b'.') {
        at += 1;
        let digits = b[at..].iter().take_while(|c| c.is_ascii_digit()).count();
        if digits == 0 {
            return None;
        }
        let kept = digits.min(3);
        let scale = [100, 10, 1];
        for (i, weight) in scale.iter().enumerate().take(kept) {
            millis += i64::from(b[at + i] - b'0') * weight;
        }
        at += digits;
    }
    let offset = match b.get(at..)? {
        [b'Z' | b'z'] => 0,
        [sign @ (b'+' | b'-'), rest @ ..] if rest.len() == 5 && rest[2] == b':' => {
            let (oh, om) = (number(rest, 0, 2)?, number(rest, 3, 2)?);
            if oh > 23 || om > 59 {
                return None;
            }
            let offset = oh * HOUR + om * MINUTE;
            if *sign == b'-' { -offset } else { offset }
        }
        _ => return None,
    };
    Some(date + hour * HOUR + minute * MINUTE + second * SECOND + millis - offset)
}

/// `ms` as an RFC 3339 date-time in UTC, with milliseconds only when there are some.
pub(crate) fn format(ms: i64) -> String {
    let (days, rest) = (ms.div_euclid(DAY), ms.rem_euclid(DAY));
    let (year, month, day) = civil_from_days(days);
    let (hour, minute) = (rest / HOUR, rest % HOUR / MINUTE);
    let (second, millis) = (rest % MINUTE / SECOND, rest % SECOND);
    let fraction = if millis == 0 {
        String::new()
    } else {
        format!(".{millis:03}")
    };
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}{fraction}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_and_dates() {
        let at = |t: &str| match parse_bound(t) {
            Ok(Bound::At(ms)) => ms,
            other => panic!("{t}: {other:?}"),
        };
        assert_eq!(at("1970-01-01T00:00:00Z"), 0);
        assert_eq!(at("1970-01-01"), 0);
        assert_eq!(at("2026-11-01T09:30:00Z"), 1_793_525_400_000);
        assert_eq!(at("2026-11-01t12:30:00.25+03:00"), 1_793_525_400_250);
        assert_eq!(at("2026-11-01 09:30:00.123456z"), 1_793_525_400_123);
        assert_eq!(at("2026-11-01T04:00:00-05:30"), 1_793_525_400_000);
        assert_eq!(at("2024-02-29"), at("2024-03-01") - DAY);
        assert_eq!(at("1969-12-31T23:59:59.999Z"), -1);
        for bad in [
            "",
            "2026-11-01T09:30:00",
            "2026-13-01",
            "2026-02-29",
            "2026-11-01T24:00:00Z",
            "2026-11-01T09:30:60Z",
            "2026-11-01T09:30:00.Z",
            "2026-11-01T09:30:00+0300",
            "2026-11-01T09:30:00Zjunk",
            "2026/11/01",
            "yesterday",
        ] {
            assert!(parse_bound(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn durations_before_now() {
        assert_eq!(parse_bound("7d"), Ok(Bound::Ago(7 * DAY)));
        assert_eq!(parse_bound("2w"), Ok(Bound::Ago(14 * DAY)));
        assert_eq!(parse_bound("36h"), Ok(Bound::Ago(36 * HOUR)));
        assert_eq!(parse_bound("90m"), Ok(Bound::Ago(90 * MINUTE)));
        assert_eq!(parse_bound("0s"), Ok(Bound::Ago(0)));
        assert_eq!(Bound::Ago(DAY).resolve(10 * DAY), 9 * DAY);
        assert_eq!(Bound::At(5).resolve(10 * DAY), 5);
        for bad in ["d", "7", "-7d", "7 d", "7y", "99999999999999999999d"] {
            assert!(parse_bound(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn format_round_trips() {
        for ms in [0, -1, 1_793_525_400_000, 1_793_525_400_250, 951_782_400_000] {
            let text = format(ms);
            assert_eq!(parse_bound(&text), Ok(Bound::At(ms)), "{text}");
        }
        assert_eq!(format(1_793_525_400_000), "2026-11-01T09:30:00Z");
        assert_eq!(format(-1), "1969-12-31T23:59:59.999Z");
    }
}
