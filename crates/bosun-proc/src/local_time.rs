//! The machine's local time zone (#67): the daily cap parks the queue until local midnight, and
//! the standard library knows no time zone. Here only because the OS calls are `unsafe`.

/// How far local time is ahead of UTC at `at_ms` (ms since the Unix epoch), in milliseconds,
/// by the time zone the OS has now (daylight saving included); 0 when the OS cannot say.
#[must_use]
pub fn utc_offset_ms(at_ms: i64) -> i64 {
    imp::offset_ms(at_ms).unwrap_or(0)
}

/// The first local midnight after `now_ms`, as ms since the Unix epoch.
#[must_use]
pub fn next_local_midnight(now_ms: i64) -> i64 {
    next_midnight(now_ms, utc_offset_ms)
}

/// The last local midnight at or before `now_ms`, as ms since the Unix epoch.
#[must_use]
pub fn last_local_midnight(now_ms: i64) -> i64 {
    last_midnight(now_ms, utc_offset_ms)
}

const DAY_MS: i64 = 24 * 3_600_000;

fn last_midnight(now_ms: i64, offset: impl Fn(i64) -> i64) -> i64 {
    let local = now_ms + offset(now_ms);
    let midnight_local = local - local.rem_euclid(DAY_MS);
    // The offset at midnight may differ from now's (a daylight-saving change in between).
    let guess = midnight_local - offset(now_ms);
    let at = midnight_local - offset(guess);
    if at > now_ms { at - DAY_MS } else { at }
}

fn next_midnight(now_ms: i64, offset: impl Fn(i64) -> i64) -> i64 {
    let last = last_midnight(now_ms, &offset);
    let local = last + offset(last) + DAY_MS;
    let guess = local - offset(last);
    let next = local - offset(guess);
    if next <= now_ms { next + DAY_MS } else { next }
}

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows_sys::Win32::System::Time::{
        FileTimeToSystemTime, SystemTimeToFileTime, SystemTimeToTzSpecificLocalTime,
    };

    /// 100-ns intervals from 1601-01-01 to the Unix epoch, in ms.
    const EPOCH_MS: i64 = 11_644_473_600_000;

    pub(super) fn offset_ms(at_ms: i64) -> Option<i64> {
        let ticks = u64::try_from(at_ms.checked_add(EPOCH_MS)?.checked_mul(10_000)?).ok()?;
        let utc = FILETIME {
            dwLowDateTime: (ticks & 0xffff_ffff) as u32,
            dwHighDateTime: (ticks >> 32) as u32,
        };
        let mut utc_st = SYSTEMTIME::default();
        let mut local_st = SYSTEMTIME::default();
        let mut local = FILETIME::default();
        // SAFETY: every pointer is to a live local of the type the call expects; a null time
        // zone is the documented "the active time zone".
        let ok = unsafe {
            FileTimeToSystemTime(&raw const utc, &raw mut utc_st) != 0
                && SystemTimeToTzSpecificLocalTime(
                    std::ptr::null(),
                    &raw const utc_st,
                    &raw mut local_st,
                ) != 0
                && SystemTimeToFileTime(&raw const local_st, &raw mut local) != 0
        };
        if !ok {
            return None;
        }
        let local_ticks = (u64::from(local.dwHighDateTime) << 32) | u64::from(local.dwLowDateTime);
        let diff = i128::from(local_ticks) - i128::from(ticks);
        i64::try_from(diff / 10_000).ok()
    }
}

#[cfg(unix)]
mod imp {
    // `time_t` and `c_long` are `i64` on most targets, not all.
    #[allow(clippy::useless_conversion, clippy::unnecessary_fallible_conversions)]
    pub(super) fn offset_ms(at_ms: i64) -> Option<i64> {
        let secs: libc::time_t = (at_ms.div_euclid(1000)).try_into().ok()?;
        // SAFETY: `tm` is plain data; zeroed is a valid value that `localtime_r` overwrites.
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        // SAFETY: both pointers are to live locals; `localtime_r` is the thread-safe variant.
        let done = unsafe { libc::localtime_r(&raw const secs, &raw mut tm) };
        if done.is_null() {
            return None;
        }
        Some(i64::from(tm.tm_gmtoff) * 1000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: i64 = 3_600_000;
    /// 2026-11-24T00:00:00Z.
    const DAY: i64 = 1_795_478_400_000;

    #[test]
    fn midnights_in_a_fixed_zone() {
        for offset in [0, 3 * H, -5 * H, 5 * H + H / 2] {
            let now = DAY + 10 * H;
            let last = last_midnight(now, |_| offset);
            let next = next_midnight(now, |_| offset);
            assert_eq!(next - last, DAY_MS, "{offset}");
            assert!(last <= now && now < next, "{offset}");
            assert_eq!((last + offset).rem_euclid(DAY_MS), 0, "{offset}");
            // At midnight exactly it is the last midnight, and the next is a day on.
            assert_eq!(last_midnight(last, |_| offset), last);
            assert_eq!(next_midnight(last, |_| offset), last + DAY_MS);
        }
    }

    #[test]
    fn a_daylight_saving_change_shortens_the_day() {
        // +1 h until 02:00 UTC of the day, +2 h after: the day after the change is 23 hours.
        let change = DAY + 2 * H;
        let offset = |at: i64| if at < change { H } else { 2 * H };
        let now = DAY + 12 * H;
        let next = next_midnight(now, offset);
        assert_eq!((next + offset(next)).rem_euclid(DAY_MS), 0);
        assert!(next > now && next - now <= DAY_MS);
    }

    #[test]
    fn the_os_names_an_offset() {
        let offset = utc_offset_ms(DAY);
        assert!(offset.abs() <= 14 * H, "{offset}");
        let next = next_local_midnight(DAY + H);
        assert!(next > DAY + H && next - (DAY + H) <= DAY_MS);
    }
}
