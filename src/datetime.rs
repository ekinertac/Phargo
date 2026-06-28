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

/// Format a Unix timestamp with a (deprecated) `strftime()` `%`-code format, UTC.
pub(crate) fn php_strftime(fmt: &str, ts: i64) -> String {
    let days = ts.div_euclid(86400);
    let secs = ts.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    let (hour, minute, second) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    let wday = (days.rem_euclid(7) + 4) % 7; // 1970-01-01 = Thursday
    let yday = days - days_from_civil(y, 1, 1); // 0-based
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
        if chars[i] != '%' || i + 1 >= chars.len() {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let code = chars[i + 1];
        match code {
            'a' => out.push_str(&DAYS[wday as usize][..3]),
            'A' => out.push_str(DAYS[wday as usize]),
            'b' | 'h' => out.push_str(&MONTHS[(m - 1) as usize][..3]),
            'B' => out.push_str(MONTHS[(m - 1) as usize]),
            'd' => out.push_str(&format!("{d:02}")),
            'e' => out.push_str(&format!("{d:2}")),
            'j' => out.push_str(&format!("{:03}", yday + 1)),
            'm' => out.push_str(&format!("{m:02}")),
            'u' => out.push_str(&(if wday == 0 { 7 } else { wday }).to_string()),
            'w' => out.push_str(&wday.to_string()),
            'y' => out.push_str(&format!("{:02}", y.rem_euclid(100))),
            'Y' => out.push_str(&y.to_string()),
            'C' => out.push_str(&format!("{:02}", y.div_euclid(100))),
            'H' => out.push_str(&format!("{hour:02}")),
            'k' => out.push_str(&format!("{hour:2}")),
            'I' => out.push_str(&format!("{h12:02}")),
            'l' => out.push_str(&format!("{h12:2}")),
            'M' => out.push_str(&format!("{minute:02}")),
            'S' => out.push_str(&format!("{second:02}")),
            'p' => out.push_str(if hour < 12 { "AM" } else { "PM" }),
            'P' => out.push_str(if hour < 12 { "am" } else { "pm" }),
            'D' => out.push_str(&format!("{m:02}/{d:02}/{:02}", y.rem_euclid(100))),
            'F' => out.push_str(&format!("{y}-{m:02}-{d:02}")),
            'T' | 'X' => out.push_str(&format!("{hour:02}:{minute:02}:{second:02}")),
            'R' => out.push_str(&format!("{hour:02}:{minute:02}")),
            'r' => out.push_str(&format!(
                "{h12:02}:{minute:02}:{second:02} {}",
                if hour < 12 { "AM" } else { "PM" }
            )),
            'x' => out.push_str(&format!("{m:02}/{d:02}/{:02}", y.rem_euclid(100))),
            'z' => out.push_str("+0000"),
            'Z' => out.push_str("UTC"),
            's' => out.push_str(&ts.to_string()),
            'n' => out.push('\n'),
            't' => out.push('\t'),
            '%' => out.push('%'),
            other => {
                out.push('%');
                out.push(other);
            }
        }
        i += 2;
    }
    out
}

/// Compose a UTC timestamp from civil components (used by mktime/gmmktime).
pub(crate) fn make_ts(h: i64, mi: i64, s: i64, mon: i64, day: i64, year: i64) -> i64 {
    days_from_civil(year, mon, day) * 86400 + h * 3600 + mi * 60 + s
}

/// A small `strtotime`: `@<ts>`, `now`, and `YYYY-MM-DD[ HH:MM:SS]` / `YYYY/MM/DD`.
fn month_from_name(s: &str) -> Option<i64> {
    let l = s.to_ascii_lowercase();
    let names = [
        "january", "february", "march", "april", "may", "june", "july", "august",
        "september", "october", "november", "december",
    ];
    for (i, n) in names.iter().enumerate() {
        if l == *n || (l.len() >= 3 && n.starts_with(&l[..3.min(l.len())]) && l.len() <= n.len() && n.starts_with(&l)) {
            return Some(i as i64 + 1);
        }
    }
    None
}

fn weekday_from_name(s: &str) -> Option<i64> {
    // 0 = Sunday … 6 = Saturday
    let l = s.to_ascii_lowercase();
    let names = ["sunday", "monday", "tuesday", "wednesday", "thursday", "friday", "saturday"];
    for (i, n) in names.iter().enumerate() {
        if n.starts_with(&l) && l.len() >= 3 {
            return Some(i as i64);
        }
    }
    None
}

/// Parse `HH:MM[:SS][am|pm]` from a token list; returns (h, m, s) if it looks like a time.
fn parse_time(tok: &str) -> Option<(i64, i64, i64)> {
    let (body, ampm) = {
        let lower = tok.to_ascii_lowercase();
        if let Some(b) = lower.strip_suffix("am") {
            (b.trim().to_string(), Some(false))
        } else if let Some(b) = lower.strip_suffix("pm") {
            (b.trim().to_string(), Some(true))
        } else {
            (lower, None)
        }
    };
    let parts: Vec<&str> = body.split(':').collect();
    if parts.is_empty() || parts[0].is_empty() || !parts[0].chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if parts.len() < 2 && ampm.is_none() {
        return None; // a bare number isn't a time
    }
    let mut h: i64 = parts[0].parse().ok()?;
    let mi: i64 = parts.get(1).and_then(|p| p.parse().ok()).unwrap_or(0);
    let se: i64 = parts.get(2).and_then(|p| p.split('.').next().unwrap_or("0").parse().ok()).unwrap_or(0);
    if let Some(pm) = ampm {
        if pm && h < 12 { h += 12; }
        if !pm && h == 12 { h = 0; }
    }
    Some((h, mi, se))
}

pub(crate) fn php_strtotime(s: &str, base: i64) -> Option<i64> {
    let t = s.trim();
    if t.is_empty() || t.eq_ignore_ascii_case("now") {
        return Some(base);
    }
    if let Some(rest) = t.strip_prefix('@') {
        return rest.trim().parse::<i64>().ok();
    }
    let lower = t.to_ascii_lowercase();

    // ---- whole-string keywords ----
    let (by, bm, bd) = civil_from_days(base.div_euclid(86400));
    let midnight = make_ts(0, 0, 0, bm, bd, by);
    match lower.as_str() {
        "today" | "midnight" => return Some(midnight),
        "noon" => return Some(midnight + 12 * 3600),
        "tomorrow" => return Some(midnight + 86400),
        "yesterday" => return Some(midnight - 86400),
        _ => {}
    }

    // ---- ISO / numeric absolute: Y-m-d or m/d/Y, optional time ----
    if let Some(ts) = parse_absolute(t) {
        return Some(ts);
    }

    // ---- time-only (applies to the base date) ----
    if let Some((h, mi, se)) = parse_time(t) {
        return Some(midnight + h * 3600 + mi * 60 + se);
    }

    // ---- textual / relative, token by token ----
    parse_relative_or_textual(&lower, base, midnight)
}

/// Absolute `YYYY-MM-DD`, `M/D/Y`, `DD-Mon-YYYY`, optionally with a time after a
/// space or `T`.
fn parse_absolute(t: &str) -> Option<i64> {
    let (date_part, time_part) = match t.split_once(['T', ' ']) {
        Some((d, tm)) => (d, Some(tm.trim())),
        None => (t, None),
    };
    let (h, mi, se) = match time_part {
        Some(tp) if !tp.is_empty() => parse_time(tp.split_whitespace().next().unwrap_or("")).unwrap_or((0, 0, 0)),
        _ => (0, 0, 0),
    };
    // dashes → Y-m-d (or D-Mon-Y); slashes → m/d/Y (US)
    if date_part.contains('-') {
        let p: Vec<&str> = date_part.split('-').collect();
        if p.len() == 3 {
            if let Some(mon) = month_from_name(p[1]) {
                // D-Mon-Y
                let d = p[0].parse().ok()?;
                let y = norm_year(p[2].parse().ok()?);
                return Some(make_ts(h, mi, se, mon, d, y));
            }
            let y = p[0].parse::<i64>().ok()?;
            let mo = p[1].parse::<i64>().ok()?;
            let d = p[2].parse::<i64>().ok()?;
            return Some(make_ts(h, mi, se, mo, d, y));
        }
    } else if date_part.contains('/') {
        let p: Vec<&str> = date_part.split('/').collect();
        if p.len() == 3 {
            let mo = p[0].parse::<i64>().ok()?;
            let d = p[1].parse::<i64>().ok()?;
            let y = norm_year(p[2].parse::<i64>().ok()?);
            return Some(make_ts(h, mi, se, mo, d, y));
        }
    }
    None
}

fn norm_year(y: i64) -> i64 {
    if y < 70 { 2000 + y } else if y < 100 { 1900 + y } else { y }
}

/// Relative offsets (`+1 day`, `2 weeks ago`, `next monday`) and textual dates
/// (`1 January 2020`, `Jan 1 2020`, `January 1, 2020`).
fn parse_relative_or_textual(lower: &str, base: i64, midnight: i64) -> Option<i64> {
    let toks: Vec<String> = lower
        .replace(',', " ")
        .split_whitespace()
        .map(|s| s.to_string())
        .collect();
    if toks.is_empty() {
        return None;
    }

    // textual date: find a month-name token and surrounding day/year numbers
    let mut month = None;
    let mut nums: Vec<i64> = Vec::new();
    for w in &toks {
        if let Some(m) = month_from_name(w) {
            month = Some(m);
        } else if let Ok(n) = w.trim_end_matches(|c: char| !c.is_ascii_digit()).parse::<i64>() {
            if w.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
                nums.push(n);
            }
        }
    }
    if let Some(mon) = month {
        // day = a number ≤ 31, year = a number > 31 (or the remaining one)
        let day = nums.iter().copied().find(|&n| (1..=31).contains(&n)).unwrap_or(1);
        let year = nums.iter().copied().find(|&n| n > 31).unwrap_or_else(|| {
            let (y, _, _) = civil_from_days(base.div_euclid(86400));
            y
        });
        return Some(make_ts(0, 0, 0, mon, day, norm_year(year)));
    }

    // weekday navigation: "next monday" / "last friday" / "monday"
    for (i, w) in toks.iter().enumerate() {
        if let Some(target) = weekday_from_name(w) {
            let dir = match toks.get(i.wrapping_sub(1)).map(|s| s.as_str()) {
                Some("last") | Some("previous") => -1,
                _ => 1,
            };
            let cur_dow = (base.div_euclid(86400) + 4).rem_euclid(7); // 1970-01-01 = Thursday(4)
            let mut delta = (target - cur_dow).rem_euclid(7);
            if dir > 0 && delta == 0 { delta = 7; }
            if dir < 0 { delta = -((cur_dow - target).rem_euclid(7)); if delta == 0 { delta = -7; } }
            return Some(midnight + delta * 86400);
        }
    }

    // relative offsets: "[+|-]N unit [ago]" possibly repeated
    let mut ts = base;
    let mut matched = false;
    let mut i = 0;
    while i < toks.len() {
        let (sign, numstr) = {
            let w = &toks[i];
            if let Some(r) = w.strip_prefix('+') {
                (1i64, r.to_string())
            } else if let Some(r) = w.strip_prefix('-') {
                (-1i64, r.to_string())
            } else {
                (1i64, w.clone())
            }
        };
        if let Ok(mut n) = numstr.parse::<i64>() {
            n *= sign;
            if let Some(unit) = toks.get(i + 1) {
                let ago = toks.get(i + 2).map(|s| s == "ago").unwrap_or(false);
                if ago { n = -n; }
                if apply_unit(&mut ts, n, unit) {
                    matched = true;
                    i += if ago { 3 } else { 2 };
                    continue;
                }
            }
        }
        i += 1;
    }
    if matched { Some(ts) } else { None }
}

fn apply_unit(ts: &mut i64, n: i64, unit: &str) -> bool {
    let u = unit.trim_end_matches('s');
    match u {
        "sec" | "second" => *ts += n,
        "min" | "minute" => *ts += n * 60,
        "hour" => *ts += n * 3600,
        "day" => *ts += n * 86400,
        "week" => *ts += n * 7 * 86400,
        "fortnight" => *ts += n * 14 * 86400,
        "month" | "year" => {
            let days = ts.div_euclid(86400);
            let secs = ts.rem_euclid(86400);
            let (mut y, mut m, d) = civil_from_days(days);
            if u == "year" {
                y += n;
            } else {
                let total = (y * 12 + (m - 1)) + n;
                y = total.div_euclid(12);
                m = total.rem_euclid(12) + 1;
            }
            let dd = d.min(days_in_month(y, m));
            *ts = make_ts(0, 0, 0, m, dd, y) + secs;
        }
        _ => return false,
    }
    true
}

