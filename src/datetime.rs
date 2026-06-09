#![allow(unused_imports)]
#![allow(clippy::all)]
use crate::*;
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

// ---- date / time (from-scratch civil-calendar math, UTC) -------------------

/// Days since the Unix epoch for a civil (proleptic Gregorian) date.
pub(crate) fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Civil (year, month, day) from days since the Unix epoch.
pub(crate) fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub(crate) fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

pub(crate) fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap(y) {
                29
            } else {
                28
            }
        }
        _ => 30,
    }
}

pub(crate) fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub(crate) const MONTHS: [&str; 12] = [
    "January", "February", "March", "April", "May", "June", "July", "August", "September",
    "October", "November", "December",
];
pub(crate) const DAYS: [&str; 7] = [
    "Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday",
];

/// Format a Unix timestamp with a PHP `date()` format string (UTC).
pub(crate) fn php_date(fmt: &str, ts: i64) -> String {
    let days = ts.div_euclid(86400);
    let secs = ts.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    let (hour, minute, second) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    let wday = (days.rem_euclid(7) + 4) % 7; // 1970-01-01 = Thursday
    let yday = days - days_from_civil(y, 1, 1);
    let h12 = {
        let h = hour % 12;
        if h == 0 {
            12
        } else {
            h
        }
    };
    let mut out = String::new();
    let chars: Vec<char> = fmt.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' {
            if i + 1 < chars.len() {
                out.push(chars[i + 1]);
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        match c {
            'Y' => out.push_str(&y.to_string()),
            'y' => out.push_str(&format!("{:02}", y.rem_euclid(100))),
            'm' => out.push_str(&format!("{m:02}")),
            'n' => out.push_str(&m.to_string()),
            'd' => out.push_str(&format!("{d:02}")),
            'j' => out.push_str(&d.to_string()),
            'H' => out.push_str(&format!("{hour:02}")),
            'G' => out.push_str(&hour.to_string()),
            'h' => out.push_str(&format!("{h12:02}")),
            'g' => out.push_str(&h12.to_string()),
            'i' => out.push_str(&format!("{minute:02}")),
            's' => out.push_str(&format!("{second:02}")),
            'A' => out.push_str(if hour < 12 { "AM" } else { "PM" }),
            'a' => out.push_str(if hour < 12 { "am" } else { "pm" }),
            'D' => out.push_str(&DAYS[wday as usize][..3]),
            'l' => out.push_str(DAYS[wday as usize]),
            'N' => out.push_str(&(if wday == 0 { 7 } else { wday }).to_string()),
            'w' => out.push_str(&wday.to_string()),
            'F' => out.push_str(MONTHS[(m - 1) as usize]),
            'M' => out.push_str(&MONTHS[(m - 1) as usize][..3]),
            't' => out.push_str(&days_in_month(y, m).to_string()),
            'L' => out.push_str(if is_leap(y) { "1" } else { "0" }),
            'z' => out.push_str(&yday.to_string()),
            'U' => out.push_str(&ts.to_string()),
            'S' => out.push_str(match d % 10 {
                _ if (11..=13).contains(&(d % 100)) => "th",
                1 => "st",
                2 => "nd",
                3 => "rd",
                _ => "th",
            }),
            other => out.push(other),
        }
        i += 1;
    }
    out
}

/// Compose a UTC timestamp from civil components (used by mktime/gmmktime).
pub(crate) fn make_ts(h: i64, mi: i64, s: i64, mon: i64, day: i64, year: i64) -> i64 {
    days_from_civil(year, mon, day) * 86400 + h * 3600 + mi * 60 + s
}

/// A small `strtotime`: `@<ts>`, `now`, and `YYYY-MM-DD[ HH:MM:SS]` / `YYYY/MM/DD`.
pub(crate) fn php_strtotime(s: &str, base: i64) -> Option<i64> {
    let t = s.trim();
    if t.eq_ignore_ascii_case("now") || t.is_empty() {
        return Some(base);
    }
    if let Some(rest) = t.strip_prefix('@') {
        return rest.trim().parse::<i64>().ok();
    }
    // YYYY-MM-DD or YYYY/MM/DD optionally followed by time
    let (date_part, time_part) = match t.split_once([' ', 'T']) {
        Some((d, tm)) => (d, Some(tm)),
        None => (t, None),
    };
    let ds: Vec<&str> = date_part.split(['-', '/']).collect();
    if ds.len() == 3 {
        let y = ds[0].parse::<i64>().ok()?;
        let mo = ds[1].parse::<i64>().ok()?;
        let d = ds[2].parse::<i64>().ok()?;
        let (mut h, mut mi, mut se) = (0i64, 0i64, 0i64);
        if let Some(tp) = time_part {
            let ts: Vec<&str> = tp.trim().split(':').collect();
            if !ts.is_empty() {
                h = ts[0].parse().unwrap_or(0);
            }
            if ts.len() > 1 {
                mi = ts[1].parse().unwrap_or(0);
            }
            if ts.len() > 2 {
                se = ts[2].split('.').next().unwrap_or("0").parse().unwrap_or(0);
            }
        }
        return Some(make_ts(h, mi, se, mo, d, y));
    }
    None
}

