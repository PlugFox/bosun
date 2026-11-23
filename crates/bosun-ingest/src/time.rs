//! RFC 3339 timestamps to milliseconds since the Unix epoch, UTC (#3: every timestamp is UTC
//! integer milliseconds).

/// Parses `YYYY-MM-DDTHH:MM:SS[.fraction](Z|+HH:MM|-HH:MM)`. Digits of the fraction past
/// milliseconds are dropped. Returns `None` for anything else.
pub(crate) fn parse_ms(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    let number = |from: usize, to: usize| -> Option<i64> {
        let digits = text.get(from..to)?;
        if digits.bytes().all(|b| b.is_ascii_digit()) {
            digits.parse().ok()
        } else {
            None
        }
    };
    let separators = [(4, b'-'), (7, b'-'), (13, b':'), (16, b':')];
    if bytes.len() < 20
        || separators.iter().any(|&(at, c)| bytes[at] != c)
        || !matches!(bytes[10], b'T' | b't' | b' ')
    {
        return None;
    }
    let (year, month, day) = (number(0, 4)?, number(5, 7)?, number(8, 10)?);
    let (hour, minute, second) = (number(11, 13)?, number(14, 16)?, number(17, 19)?);
    if !(1..=12).contains(&month)
        || !(1..=days_in_month(year, month)).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }

    let mut rest = &text[19..];
    let mut millis = 0;
    if let Some(fraction) = rest.strip_prefix('.') {
        let digits = fraction.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        let padded = format!("{:0<3}", &fraction[..digits.min(3)]);
        millis = padded.parse::<i64>().ok()?;
        rest = &fraction[digits..];
    }
    let offset_minutes = match rest.as_bytes() {
        [b'Z' | b'z'] => 0,
        [sign @ (b'+' | b'-'), h1, h2, b':', m1, m2] => {
            let digit = |b: &u8| b.is_ascii_digit().then(|| i64::from(b - b'0'));
            let minutes = (digit(h1)? * 10 + digit(h2)?) * 60 + digit(m1)? * 10 + digit(m2)?;
            if *sign == b'-' { -minutes } else { minutes }
        }
        _ => return None,
    };

    let days = days_from_civil(year, month, day);
    let seconds = ((days * 24 + hour) * 60 + minute - offset_minutes) * 60 + second;
    Some(seconds * 1000 + millis)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date (H. Hinnant's `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_from_march = (month + 9) % 12;
    let day_of_year = (153 * month_from_march + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_parse_to_utc_milliseconds() {
        assert_eq!(parse_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_ms("2000-01-01T00:00:00.000Z"), Some(946_684_800_000));
        assert_eq!(parse_ms("2000-01-03T00:00:01.5Z"), Some(946_857_601_500));
        assert_eq!(
            parse_ms("2000-02-29T00:00:00.123456Z"),
            Some(951_782_400_123)
        );
        assert_eq!(parse_ms("2000-01-01T02:00:00+02:00"), Some(946_684_800_000));
        assert_eq!(parse_ms("1999-12-31T23:00:00-01:00"), Some(946_684_800_000));
    }

    #[test]
    fn malformed_timestamps_are_refused() {
        for bad in [
            "",
            "2000-01-01",
            "2000-01-01T00:00:00",
            "2000-13-01T00:00:00Z",
            "2001-02-29T00:00:00Z",
            "2000-01-01T24:00:00Z",
            "2000-01-01T00:00:00.Z",
            "2000-01-01T00:00:00+0200",
            "2000-01-01X00:00:00Z",
            "20x0-01-01T00:00:00Z",
        ] {
            assert_eq!(parse_ms(bad), None, "{bad}");
        }
    }
}
