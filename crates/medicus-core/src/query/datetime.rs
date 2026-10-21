//! Interpretation of DA, TM and DT values "by meaning" (PS3.4 C.2.2.2.1.3, C.2.2.2.5).
//!
//! Every value is turned into an inclusive interval: a partial value such as the TM `"10"` covers
//! `10:00:00.000000 ..= 10:59:59.999999`. Single Value Matching compares lower bounds; Range
//! Matching uses the lower bound of the range start and the upper bound of the range end.
//!
//! Implementation-defined choices (to be listed in the Conformance Statement):
//! - DT values without a UTC offset are interpreted as UTC.
//! - The ACR-NEMA forms `YYYY.MM.DD` (DA) and `HH:MM:SS` (TM) are accepted.

use time::{Date, Month};

/// Microseconds per unit.
const US_PER_SEC: i64 = 1_000_000;
const US_PER_DAY: i64 = 86_400 * US_PER_SEC;
/// Julian day number of 1970-01-01.
const UNIX_EPOCH_JD: i64 = 2_440_588;

/// Inclusive interval in a VR-specific unit (days for DA, µs since midnight for TM,
/// µs since the Unix epoch for DT).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Interval {
    pub lo: i64,
    pub hi: i64,
}

fn digits(s: &str) -> Option<i64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

fn julian_day(year: i64, month: i64, day: i64) -> Option<i64> {
    let month = Month::try_from(u8::try_from(month).ok()?).ok()?;
    let date =
        Date::from_calendar_date(i32::try_from(year).ok()?, month, u8::try_from(day).ok()?).ok()?;
    Some(i64::from(date.to_julian_day()))
}

fn days_in_month(year: i64, month: i64) -> Option<i64> {
    let m = Month::try_from(u8::try_from(month).ok()?).ok()?;
    Some(i64::from(time::util::days_in_month(
        m,
        i32::try_from(year).ok()?,
    )))
}

/// DA: `YYYYMMDD` (or `YYYY.MM.DD`). Result in Julian days.
pub(crate) fn date(raw: &str) -> Option<Interval> {
    let s: String = raw.trim().chars().filter(|c| *c != '.').collect();
    // Byte-index slicing below requires ASCII; valid values are ASCII-only anyway.
    if s.len() != 8 || !s.is_ascii() {
        return None;
    }
    let jd = julian_day(digits(&s[0..4])?, digits(&s[4..6])?, digits(&s[6..8])?)?;
    Some(Interval { lo: jd, hi: jd })
}

/// Parses `.F{1,6}` fraction digits into (lower, upper) microseconds.
fn fraction(f: &str) -> Option<(i64, i64)> {
    if f.is_empty() {
        return Some((0, US_PER_SEC - 1));
    }
    if f.len() > 6 {
        return None;
    }
    let v = digits(f)?;
    let scale = 10_i64.pow(u32::try_from(6 - f.len()).ok()?);
    Some((v * scale, v * scale + scale - 1))
}

/// Time of day `HH[MM[SS[.F]]]` without separators. Result in µs since midnight.
fn time_of_day(s: &str) -> Option<Interval> {
    if !s.is_ascii() {
        return None;
    }
    let (main, frac) = match s.split_once('.') {
        Some((m, f)) => (m, Some(f)),
        None => (s, None),
    };
    if !matches!(main.len(), 2 | 4 | 6) || (frac.is_some() && main.len() != 6) {
        return None;
    }
    let h = digits(&main[0..2])?;
    let m = if main.len() >= 4 {
        Some(digits(&main[2..4])?)
    } else {
        None
    };
    let sec = if main.len() == 6 {
        Some(digits(&main[4..6])?)
    } else {
        None
    };
    if h > 23 || m.is_some_and(|m| m > 59) || sec.is_some_and(|s| s > 60) {
        return None;
    }
    let base =
        h * 3600 * US_PER_SEC + m.unwrap_or(0) * 60 * US_PER_SEC + sec.unwrap_or(0) * US_PER_SEC;
    let span = match (m, sec) {
        (None, _) => 3600 * US_PER_SEC - 1,
        (Some(_), None) => 60 * US_PER_SEC - 1,
        (Some(_), Some(_)) => {
            let (flo, fhi) = fraction(frac.unwrap_or(""))?;
            return Some(Interval {
                lo: base + flo,
                hi: base + fhi,
            });
        }
    };
    Some(Interval {
        lo: base,
        hi: base + span,
    })
}

/// TM: `HH[MM[SS[.F{1,6}]]]` (or `HH:MM:SS.F`).
pub(crate) fn time(raw: &str) -> Option<Interval> {
    let s: String = raw.trim().chars().filter(|c| *c != ':').collect();
    time_of_day(&s)
}

/// DT: `YYYY[MM[DD[HH[MM[SS[.F{1,6}]]]]]][&ZZXX]`. Result in µs since the Unix epoch (UTC).
pub(crate) fn datetime(raw: &str) -> Option<Interval> {
    let s = raw.trim();
    if !s.is_ascii() {
        return None;
    }
    // A UTC offset is a trailing `+HHMM` / `-HHMM`.
    let (body, offset_us) = match s.rfind(['+', '-']) {
        Some(i) if s.len() - i == 5 => {
            let sign = if &s[i..=i] == "-" { -1 } else { 1 };
            let oh = digits(&s[i + 1..i + 3])?;
            let om = digits(&s[i + 3..i + 5])?;
            if oh > 14 || om > 59 {
                return None;
            }
            (&s[..i], sign * (oh * 3600 + om * 60) * US_PER_SEC)
        }
        Some(_) => return None,
        None => (s, 0),
    };
    let (main, frac) = match body.split_once('.') {
        Some((m, f)) => (m, Some(f)),
        None => (body, None),
    };
    if !matches!(main.len(), 4 | 6 | 8 | 10 | 12 | 14) || (frac.is_some() && main.len() != 14) {
        return None;
    }
    let year = digits(&main[0..4])?;
    let (lo_day, hi_day) = match main.len() {
        4 => (julian_day(year, 1, 1)?, julian_day(year, 12, 31)?),
        6 => {
            let month = digits(&main[4..6])?;
            let last = days_in_month(year, month)?;
            (julian_day(year, month, 1)?, julian_day(year, month, last)?)
        }
        _ => {
            let jd = julian_day(year, digits(&main[4..6])?, digits(&main[6..8])?)?;
            (jd, jd)
        }
    };
    let (tod_lo, tod_hi) = if main.len() <= 8 {
        (0, US_PER_DAY - 1)
    } else {
        let mut t = main[8..].to_owned();
        if let Some(f) = frac {
            t.push('.');
            t.push_str(f);
        }
        let i = time_of_day(&t)?;
        (i.lo, i.hi)
    };
    Some(Interval {
        lo: (lo_day - UNIX_EPOCH_JD) * US_PER_DAY + tod_lo - offset_us,
        hi: (hi_day - UNIX_EPOCH_JD) * US_PER_DAY + tod_hi - offset_us,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(date("20240229").unwrap().lo, date("2024.02.29").unwrap().lo);
        assert!(date("20230229").is_none());
        assert!(date("2024").is_none());
        assert_eq!(date("19700101").unwrap().lo, UNIX_EPOCH_JD);
    }

    #[test]
    fn times_are_intervals() {
        let h = time("10").unwrap();
        assert_eq!(h.lo, 10 * 3600 * US_PER_SEC);
        assert_eq!(h.hi, 11 * 3600 * US_PER_SEC - 1);
        // "2230" matches "223000" by meaning (PS3.4 C.2.2.2.1.3 Note 1)
        assert_eq!(time("2230").unwrap().lo, time("223000").unwrap().lo);
        assert_eq!(time("22:30:00").unwrap().lo, time("223000").unwrap().lo);
        let f = time("101010.5").unwrap();
        assert_eq!(f.hi - f.lo, 99_999);
        assert!(time("2460").is_none());
        assert!(time("1010.5").is_none());
    }

    #[test]
    fn datetimes_respect_offsets() {
        // "19980128103000" (no offset → UTC) matches "19980128073000-0300" (PS3.4 Note 1)
        assert_eq!(
            datetime("19980128103000").unwrap().lo,
            datetime("19980128073000-0300").unwrap().lo
        );
        assert_eq!(
            datetime("19980128103000.0000").unwrap().lo,
            datetime("19980128103000").unwrap().lo
        );
        let y = datetime("2024").unwrap();
        assert_eq!(y.hi + 1 - y.lo, 366 * US_PER_DAY);
        let m = datetime("202402").unwrap();
        assert_eq!(m.hi + 1 - m.lo, 29 * US_PER_DAY);
        assert!(datetime("2024021").is_none());
        assert!(datetime("20240230").is_none());
    }

    #[test]
    fn non_ascii_is_rejected_not_panicking() {
        assert!(date("2024€101").is_none());
        assert!(time("1€").is_none());
        assert!(datetime("2€24").is_none());
        assert!(datetime("20240101€0500").is_none());
    }
}
