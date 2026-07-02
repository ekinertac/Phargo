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

/// Format a Unix timestamp with a PHP `date()` format string, UTC (gmdate & the
/// default-UTC fast path).
pub(crate) fn php_date(fmt: &str, ts: i64) -> String {
    php_date_tz(fmt, ts, None)
}

/// ISO-8601 week number and week-year (`W` / `o` format codes).
fn iso_week(y: i64, m: i64, d: i64) -> (i64, i64) {
    let days = days_from_civil(y, m, d);
    let wday = (days.rem_euclid(7) + 3) % 7; // 0 = Monday
    let thursday = days - wday + 3;
    let (wy, _, _) = civil_from_days(thursday);
    let jan1 = days_from_civil(wy, 1, 1);
    let week = (thursday - jan1) / 7 + 1;
    (week, wy)
}

/// Format a Unix timestamp with a PHP `date()` format string in the given zone
/// (`None` = UTC). Zone-dependent codes (e/T/O/P/p/Z/I) come from the TZif data.
pub(crate) fn php_date_tz(fmt: &str, ts: i64, zone: Option<&crate::tz::TzData>) -> String {
    let (off, isdst, abbr, zname) = match zone {
        Some(z) => {
            let (o, d, a) = z.offset_at(ts);
            (o as i64, d, a.to_string(), z.name.clone())
        }
        None => (0, false, "UTC".to_string(), "UTC".to_string()),
    };
    let local = ts + off;
    let days = local.div_euclid(86400);
    let secs = local.rem_euclid(86400);
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
    let off_hm = |sep: &str| {
        let sign = if off < 0 { '-' } else { '+' };
        let a = off.abs();
        format!("{sign}{:02}{sep}{:02}", a / 3600, (a % 3600) / 60)
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
            'u' => out.push_str("000000"),
            'v' => out.push_str("000"),
            'W' => out.push_str(&format!("{:02}", iso_week(y, m, d).0)),
            'o' => out.push_str(&iso_week(y, m, d).1.to_string()),
            // Swatch Internet Time — always relative to UTC+1, zone-independent
            'B' => {
                let beat = ((ts + 3600).rem_euclid(86400)) * 1000 / 86400;
                out.push_str(&format!("{beat:03}"));
            }
            // timezone-dependent
            'e' => out.push_str(&zname),
            'T' => out.push_str(&abbr),
            'O' => out.push_str(&off_hm("")),
            'P' => out.push_str(&off_hm(":")),
            'p' => out.push_str(&if off == 0 { "Z".to_string() } else { off_hm(":") }),
            'Z' => out.push_str(&off.to_string()),
            'I' => out.push_str(if isdst { "1" } else { "0" }),
            'c' => {
                out.push_str(&format!("{y:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}"));
                out.push_str(&off_hm(":"));
            }
            'r' => {
                out.push_str(&format!(
                    "{}, {d:02} {} {y} {hour:02}:{minute:02}:{second:02} {}",
                    &DAYS[wday as usize][..3],
                    &MONTHS[(m - 1) as usize][..3],
                    off_hm("")
                ));
            }
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

// ---- DateTime::createFromFormat ------------------------------------------

/// Fields extracted by [`php_parse_from_format`]. `None` = not present in the
/// format; the caller fills defaults (now, or the epoch after `!`/`|`).
#[derive(Default)]
pub(crate) struct ParsedFmt {
    pub y: Option<i64>,
    pub mo: Option<i64>,
    pub d: Option<i64>,
    pub h: Option<i64>,
    pub mi: Option<i64>,
    pub s: Option<i64>,
    pub pm: Option<bool>,      // a/A: seen "pm"?
    pub epoch: Option<i64>,    // U: absolute timestamp — overrides everything
    pub off: Option<i64>,      // O/P: fixed UTC offset in seconds
    pub tzname: Option<String>, // e/T: parsed zone identifier
    pub default_epoch: bool,   // saw ! or | → unset fields default to the epoch
}

/// `DateTime::createFromFormat` format matcher. Returns None on any mismatch
/// (PHP returns false there). Mirrors PHP's parsing codes; microseconds are
/// consumed but dropped (we don't store sub-second precision).
pub(crate) fn php_parse_from_format(fmt: &str, input: &str) -> Option<ParsedFmt> {
    let f: Vec<char> = fmt.chars().collect();
    let s: Vec<char> = input.chars().collect();
    let mut out = ParsedFmt::default();
    let (mut i, mut j) = (0usize, 0usize);
    let mut allow_trailing = false;

    // read 1..=max digits (PHP is lenient about leading zeros)
    fn digits(s: &[char], j: &mut usize, max: usize) -> Option<i64> {
        let start = *j;
        while *j < s.len() && *j - start < max && s[*j].is_ascii_digit() {
            *j += 1;
        }
        if *j == start {
            return None;
        }
        s[start..*j].iter().collect::<String>().parse().ok()
    }
    // case-insensitive literal word match
    fn word(s: &[char], j: &mut usize, w: &str) -> bool {
        let wc: Vec<char> = w.chars().collect();
        if *j + wc.len() > s.len() {
            return false;
        }
        for (k, c) in wc.iter().enumerate() {
            if !s[*j + k].eq_ignore_ascii_case(c) {
                return false;
            }
        }
        *j += wc.len();
        true
    }
    // signed HH:MM / HHMM offset, with optional GMT/UTC prefix
    fn tz_offset(s: &[char], j: &mut usize) -> Option<i64> {
        let save = *j;
        if word(s, j, "GMT") || word(s, j, "UTC") {
            if *j >= s.len() || (s[*j] != '+' && s[*j] != '-') {
                // bare "GMT"/"UTC" = +00:00
                return Some(0);
            }
        }
        if *j >= s.len() || (s[*j] != '+' && s[*j] != '-') {
            *j = save;
            return None;
        }
        let neg = s[*j] == '-';
        *j += 1;
        let h = digits(s, j, 2)?;
        if *j < s.len() && s[*j] == ':' {
            *j += 1;
        }
        let m = digits(s, j, 2).unwrap_or(0);
        let v = h * 3600 + m * 60;
        Some(if neg { -v } else { v })
    }

    while i < f.len() {
        let c = f[i];
        i += 1;
        match c {
            '\\' => {
                // escaped literal
                if i < f.len() {
                    if j >= s.len() || s[j] != f[i] {
                        return None;
                    }
                    i += 1;
                    j += 1;
                }
            }
            'd' | 'j' => out.d = Some(digits(&s, &mut j, 2)?),
            'm' | 'n' => out.mo = Some(digits(&s, &mut j, 2)?),
            'Y' => out.y = Some(digits(&s, &mut j, 4)?),
            'y' => out.y = Some(norm_year(digits(&s, &mut j, 2)?)),
            'H' | 'G' => out.h = Some(digits(&s, &mut j, 2)?),
            'h' | 'g' => out.h = Some(digits(&s, &mut j, 2)?),
            'i' => out.mi = Some(digits(&s, &mut j, 2)?),
            's' => out.s = Some(digits(&s, &mut j, 2)?),
            'u' | 'v' => {
                // consume fraction digits, drop (no sub-second storage)
                digits(&s, &mut j, 6)?;
            }
            'z' => {
                digits(&s, &mut j, 3)?; // day-of-year: consumed, unused
            }
            'a' | 'A' => {
                if word(&s, &mut j, "am") || word(&s, &mut j, "a.m.") {
                    out.pm = Some(false);
                } else if word(&s, &mut j, "pm") || word(&s, &mut j, "p.m.") {
                    out.pm = Some(true);
                } else {
                    return None;
                }
            }
            'F' | 'M' => {
                // month name (full or 3-letter)
                let names = [
                    "January", "February", "March", "April", "May", "June", "July",
                    "August", "September", "October", "November", "December",
                ];
                let mut found = None;
                for (k, n) in names.iter().enumerate() {
                    if word(&s, &mut j, n) {
                        found = Some(k as i64 + 1);
                        break;
                    }
                    if word(&s, &mut j, &n[..3]) {
                        found = Some(k as i64 + 1);
                        break;
                    }
                }
                out.mo = Some(found?);
            }
            'D' | 'l' => {
                // day name: matched and discarded (date fields decide the day)
                let mut ok = false;
                for n in DAYS {
                    if word(&s, &mut j, n) || word(&s, &mut j, &n[..3]) {
                        ok = true;
                        break;
                    }
                }
                if !ok {
                    return None;
                }
            }
            'S' => {
                // ordinal suffix
                if !(word(&s, &mut j, "st")
                    || word(&s, &mut j, "nd")
                    || word(&s, &mut j, "rd")
                    || word(&s, &mut j, "th"))
                {
                    return None;
                }
            }
            'N' | 'w' => {
                digits(&s, &mut j, 1)?; // weekday number: consumed, unused
            }
            'U' => {
                let neg = j < s.len() && s[j] == '-';
                if neg {
                    j += 1;
                }
                let v = digits(&s, &mut j, 19)?;
                out.epoch = Some(if neg { -v } else { v });
            }
            'O' | 'P' | 'p' => out.off = Some(tz_offset(&s, &mut j)?),
            'e' | 'T' => {
                // timezone identifier / abbreviation — or a numeric offset
                if let Some(off) = tz_offset(&s, &mut j) {
                    out.off = Some(off);
                } else {
                    let start = j;
                    while j < s.len()
                        && (s[j].is_ascii_alphanumeric() || matches!(s[j], '/' | '_' | '-' | '+'))
                    {
                        j += 1;
                    }
                    if j == start {
                        return None;
                    }
                    out.tzname = Some(s[start..j].iter().collect());
                }
            }
            '!' => {
                // reset everything (parsed so far included) to the epoch
                out = ParsedFmt { default_epoch: true, ..Default::default() };
            }
            '|' => out.default_epoch = true,
            '?' => {
                if j >= s.len() {
                    return None;
                }
                j += 1;
            }
            '*' => {
                // skip input until the next separator byte
                while j < s.len() && !matches!(s[j], ';' | ':' | '/' | '.' | ',' | '-' | '(' | ')' | ' ') {
                    j += 1;
                }
            }
            '+' => allow_trailing = true,
            '#' => {
                if j >= s.len() || !matches!(s[j], ';' | ':' | '/' | '.' | ',' | '-') {
                    return None;
                }
                j += 1;
            }
            ' ' => {
                if j < s.len() && (s[j] == ' ' || s[j] == '\t') {
                    j += 1;
                }
            }
            lit => {
                if j >= s.len() || s[j] != lit {
                    return None;
                }
                j += 1;
            }
        }
    }
    if j < s.len() && !allow_trailing {
        return None; // unconsumed input
    }
    Some(out)
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

/// Timezone-aware `strtotime`: wall-clock strings are interpreted in `zone`.
/// The parser itself works in "wall seconds"; we shift the base into local wall
/// time, parse there, and convert the result back to UTC. `@ts` stays absolute,
/// and so does any string that carries its own explicit offset/abbreviation
/// (ISO `+HH:MM`, RFC-2822 `+HHMM`, or a zone abbreviation like `GMT`/`PST`) —
/// those bypass the wall→UTC conversion entirely since they're already UTC-exact.
pub(crate) fn php_strtotime_tz(s: &str, base: i64, zone: Option<&crate::tz::TzData>) -> Option<i64> {
    let t = s.trim();
    if let Some(rest) = t.strip_prefix('@') {
        return rest.trim().split('.').next().unwrap_or("").parse::<i64>().ok();
    }
    match zone {
        None => php_strtotime(s, base),
        Some(z) => {
            let (off, _, _) = z.offset_at(base);
            let (wall, absolute) = php_strtotime2(s, base + off as i64)?;
            if absolute {
                Some(wall)
            } else {
                Some(crate::tz::ts_from_local(wall, z))
            }
        }
    }
}

/// Core `strtotime`. Returns `(timestamp, absolute)`: `absolute` is true when the
/// input carried its own explicit UTC offset/abbreviation, in which case the
/// timestamp is already a true UTC instant (the tz wrapper must not re-convert
/// it as wall-clock local time). Everything else stays in "wall seconds", the
/// contract the tz wrapper depends on.
pub(crate) fn php_strtotime2(s: &str, base: i64) -> Option<(i64, bool)> {
    let t = s.trim();
    if t.is_empty() || t.eq_ignore_ascii_case("now") {
        return Some((base, false));
    }
    if let Some(rest) = t.strip_prefix('@') {
        return rest.trim().parse::<i64>().ok().map(|ts| (ts, true));
    }
    let lower = t.to_ascii_lowercase();

    // ---- whole-string keywords ----
    let (by, bm, bd) = civil_from_days(base.div_euclid(86400));
    let midnight = make_ts(0, 0, 0, bm, bd, by);
    match lower.as_str() {
        "today" | "midnight" => return Some((midnight, false)),
        "noon" => return Some((midnight + 12 * 3600, false)),
        "tomorrow" => return Some((midnight + 86400, false)),
        "yesterday" => return Some((midnight - 86400, false)),
        _ => {}
    }

    // ---- ISO / numeric absolute: Y-m-d or m/d/Y, optional time [+zone] ----
    if let Some(r) = parse_absolute(t) {
        return Some(r);
    }

    // ---- time-only (applies to the base date) ----
    if let Some((h, mi, se)) = parse_time(t) {
        return Some((midnight + h * 3600 + mi * 60 + se, false));
    }

    // ---- textual / relative, token by token ----
    parse_relative_or_textual(&lower, base, midnight)
}

/// Thin wrapper over [`php_strtotime2`] for callers that only need the
/// timestamp (wall-domain contract unchanged from before the tz/offset work).
pub(crate) fn php_strtotime(s: &str, base: i64) -> Option<i64> {
    php_strtotime2(s, base).map(|(ts, _)| ts)
}

/// Absolute `YYYY-MM-DD`, `M/D/Y`, `DD-Mon-YYYY`, optionally with a time after a
/// space or `T`, and an optional trailing UTC offset/zone abbreviation on the
/// time (`+02:00`, ` GMT`, ` PST`, …). Returns `(ts, absolute)` — see
/// [`php_strtotime2`] for what `absolute` means.
fn parse_absolute(t: &str) -> Option<(i64, bool)> {
    let (date_part, time_part) = match t.split_once(['T', ' ']) {
        Some((d, tm)) => (d, Some(tm.trim())),
        None => (t, None),
    };
    let mut absolute = false;
    let mut offset = 0i64;
    let (h, mi, se) = match time_part {
        Some(tp) if !tp.is_empty() => {
            let (tp2, off, abs) = strip_zone_suffix(tp);
            if abs {
                absolute = true;
                offset = off;
            }
            parse_time(tp2.split_whitespace().next().unwrap_or("")).unwrap_or((0, 0, 0))
        }
        _ => (0, 0, 0),
    };
    // dashes → Y-m-d (or D-Mon-Y); slashes → m/d/Y (US)
    let ts = if date_part.contains('-') {
        let p: Vec<&str> = date_part.split('-').collect();
        if p.len() != 3 {
            return None;
        }
        if let Some(mon) = month_from_name(p[1]) {
            // D-Mon-Y
            let d = p[0].parse().ok()?;
            let y = norm_year(p[2].parse().ok()?);
            make_ts(h, mi, se, mon, d, y)
        } else {
            let y = p[0].parse::<i64>().ok()?;
            let mo = p[1].parse::<i64>().ok()?;
            let d = p[2].parse::<i64>().ok()?;
            make_ts(h, mi, se, mo, d, y)
        }
    } else if date_part.contains('/') {
        let p: Vec<&str> = date_part.split('/').collect();
        if p.len() != 3 {
            return None;
        }
        let mo = p[0].parse::<i64>().ok()?;
        let d = p[1].parse::<i64>().ok()?;
        let y = norm_year(p[2].parse::<i64>().ok()?);
        make_ts(h, mi, se, mo, d, y)
    } else {
        return None;
    };
    Some((if absolute { ts - offset } else { ts }, absolute))
}

fn norm_year(y: i64) -> i64 {
    if y < 70 { 2000 + y } else if y < 100 { 1900 + y } else { y }
}

/// Strip a trailing timezone offset or abbreviation from a time-of-day string
/// like `"22:30:41+02:00"`, `"22:30:41 GMT"`, or `"22:30:41 PST"`. Returns the
/// remaining time text, the offset in seconds east of UTC, and whether a
/// zone marker was actually found (vs. a bare time with nothing to strip).
fn strip_zone_suffix(tp: &str) -> (String, i64, bool) {
    let tp = tp.trim();
    // space-separated trailing token, e.g. "22:30:41 GMT" / "22:30:41 +0200"
    if let Some(idx) = tp.rfind(' ') {
        let (rest, last) = tp.split_at(idx);
        let last = last.trim();
        if let Some(off) = parse_offset_str(last) {
            return (rest.trim().to_string(), off, true);
        }
        if let Some(off) = abbrev_offset(last) {
            return (rest.trim().to_string(), off, true);
        }
    }
    // offset glued directly onto the time, e.g. "22:30:41+02:00" (ISO 8601)
    if let Some((rest, off)) = strip_attached_offset(tp) {
        return (rest.to_string(), off, true);
    }
    (tp.to_string(), 0, false)
}

/// Find a `+HH:MM`/`-HHMM`/`+HH` offset glued onto the end of `tp` with no
/// separating space (the ISO-8601 `T22:30:41+02:00` shape).
fn strip_attached_offset(tp: &str) -> Option<(&str, i64)> {
    let bytes = tp.as_bytes();
    for i in 1..bytes.len() {
        if bytes[i] == b'+' || bytes[i] == b'-' {
            if let Some(off) = parse_offset_str(&tp[i..]) {
                return Some((&tp[..i], off));
            }
        }
    }
    None
}

/// Parse a standalone signed offset token: `+02:00`, `-0500`, `+02`, or `Z`.
/// Returns seconds east of UTC.
fn parse_offset_str(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("z") {
        return Some(0);
    }
    let mut chars = s.chars();
    let sign = match chars.next()? {
        '+' => 1i64,
        '-' => -1i64,
        _ => return None,
    };
    let rest: String = chars.collect::<String>().replace(':', "");
    if rest.is_empty() || !rest.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let (h, m) = match rest.len() {
        2 => (rest[0..2].parse::<i64>().ok()?, 0i64),
        4 => (rest[0..2].parse::<i64>().ok()?, rest[2..4].parse::<i64>().ok()?),
        _ => return None,
    };
    Some(sign * (h * 3600 + m * 60))
}

/// Zone-abbreviation → seconds-east-of-UTC lookup (case-insensitive). `IST` is
/// deliberately omitted — it's ambiguous across India/Ireland/Israel.
fn abbrev_offset(s: &str) -> Option<i64> {
    let u = s.to_ascii_uppercase();
    const TABLE: &[(&str, i64)] = &[
        ("GMT", 0), ("UT", 0), ("UTC", 0), ("Z", 0),
        ("EST", -18000), ("EDT", -14400),
        ("CST", -21600), ("CDT", -18000),
        ("MST", -25200), ("MDT", -21600),
        ("PST", -28800), ("PDT", -25200),
        ("AKST", -32400), ("AKDT", -28800),
        ("HST", -36000),
        ("CET", 3600), ("CEST", 7200),
        ("WET", 0), ("WEST", 3600),
        ("BST", 3600),
        ("EET", 7200), ("EEST", 10800),
        ("MSK", 10800),
        ("SAST", 7200),
        ("JST", 32400), ("KST", 32400),
        ("AEST", 36000), ("AEDT", 39600),
        ("NZST", 43200), ("NZDT", 46800),
    ];
    TABLE.iter().find(|(k, _)| *k == u).map(|(_, v)| *v)
}

/// Relative offsets (`+1 day`, `2 weeks ago`, `next monday`) and textual dates
/// (`1 January 2020`, `Jan 1 2020`, `January 1, 2020`, and RFC-2822-shaped
/// strings like `Thu, 14 Jul 2005 22:30:41 +0200` — the leading weekday name
/// is harmless noise here since the month-name scan below runs first and
/// simply never matches it). Returns `(ts, absolute)` — see
/// [`php_strtotime2`] for what `absolute` means.
fn parse_relative_or_textual(lower: &str, base: i64, midnight: i64) -> Option<(i64, bool)> {
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
        // trailing clock time, e.g. the "10:30" in "14 July 2005 10:30"
        // (defaults to midnight — the pre-existing, still-correct behavior
        // for pure textual dates with no time component)
        let mut h = 0i64;
        let mut mi = 0i64;
        let mut se = 0i64;
        for w in &toks {
            if let Some((th, tm, ts_)) = parse_time(w) {
                h = th;
                mi = tm;
                se = ts_;
                break;
            }
        }
        // trailing zone offset/abbreviation, e.g. RFC-2822's "+0200"
        let mut absolute = false;
        let mut offset = 0i64;
        if let Some(last) = toks.last() {
            if let Some(off) = parse_offset_str(last) {
                offset = off;
                absolute = true;
            } else if let Some(off) = abbrev_offset(last) {
                offset = off;
                absolute = true;
            }
        }
        let ts = make_ts(h, mi, se, mon, day, norm_year(year));
        return Some((if absolute { ts - offset } else { ts }, absolute));
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
            return Some((midnight + delta * 86400, false));
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
    if matched { Some((ts, false)) } else { None }
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

