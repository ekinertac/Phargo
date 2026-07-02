//! From-scratch IANA TZif (RFC 8536) reader — named-timezone support for ext/date.
//!
//! Loads compiled zone files from the host's /usr/share/zoneinfo (the same tzdata
//! PHP's own timezonedb is generated from), so offsets/DST/abbreviations match
//! PHP's answers for the historical dates the corpus tests use. Parsed zones are
//! cached per thread. Zones resolve through `offset_at(ts)` → (utc offset seconds,
//! is-DST, abbreviation).
//!
//! Used by `datetime.rs` (tz-aware `date()`/`mktime()`/`strtotime`) and by the
//! DateTime/DateTimeZone prelude classes via internal builtins in `eval.rs`.
//!
//! Limitations (deliberate): the v2+ footer TZ string (rules beyond the last
//! precompiled transition, ~2037 on fat tzdata) is not evaluated — timestamps past
//! the table use the last known type. Leap-second tables are ignored (PHP does the
//! same: Unix time is POSIX time).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

pub struct TzData {
    /// Zone name as canonically capitalized in the request (PHP echoes it back).
    pub name: String,
    /// Transition instants (UTC seconds), ascending.
    trans: Vec<i64>,
    /// Index into `types` for the interval starting at the same-index transition.
    idx: Vec<u8>,
    /// (utoff seconds, is_dst, abbreviation)
    types: Vec<(i32, bool, String)>,
}

impl TzData {
    /// Offset info in effect at UTC timestamp `ts`.
    pub fn offset_at(&self, ts: i64) -> (i32, bool, &str) {
        let ti = match self.trans.binary_search(&ts) {
            Ok(i) => Some(i),
            Err(0) => None,          // before the first transition
            Err(i) => Some(i - 1),
        };
        let ty = match ti {
            Some(i) => self.idx.get(i).copied().unwrap_or(0) as usize,
            // RFC 8536: before the first transition use the first non-DST type
            None => self.types.iter().position(|t| !t.1).unwrap_or(0),
        };
        match self.types.get(ty) {
            Some((off, dst, ab)) => (*off, *dst, ab.as_str()),
            None => (0, false, "UTC"),
        }
    }
}

thread_local! {
    static CACHE: RefCell<HashMap<String, Option<Rc<TzData>>>> = RefCell::new(HashMap::new());
    /// Per-test default-zone override (a .phpt `--INI--` `date.timezone=` line).
    /// Read once by each fresh evaluator; None → "UTC".
    static DEFAULT_TZ: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Set (or clear) the default timezone the next evaluator starts with — the
/// scoreboard runner calls this per test from the `--INI--` section.
pub fn set_default_tz(tz: Option<String>) {
    DEFAULT_TZ.with(|d| *d.borrow_mut() = tz);
}

pub fn default_tz() -> String {
    DEFAULT_TZ.with(|d| d.borrow().clone()).unwrap_or_else(|| "UTC".to_string())
}

/// Look up a zone by IANA name (case-corrected lookups are NOT attempted — PHP is
/// case-sensitive-ish here; we accept the name as given and try the file).
pub fn lookup(name: &str) -> Option<Rc<TzData>> {
    let key = name.to_string();
    CACHE.with(|c| {
        if let Some(hit) = c.borrow().get(&key) {
            return hit.clone();
        }
        let loaded = load(name).map(Rc::new);
        c.borrow_mut().insert(key, loaded.clone());
        loaded
    })
}

/// Is this a UTC-equivalent spelling PHP treats as the trivial zone?
pub fn is_utc_name(name: &str) -> bool {
    matches!(name.to_ascii_uppercase().as_str(), "UTC" | "GMT" | "Z" | "GMT+0" | "GMT-0" | "GMT0" | "+00:00")
}

fn load(name: &str) -> Option<TzData> {
    // Path safety: zone names are letter/digit/slash/underscore/dash/plus/dot,
    // no leading slash, no "..".
    if name.is_empty()
        || name.starts_with('/')
        || name.contains("..")
        || !name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'_' | b'-' | b'+' | b'.'))
    {
        return None;
    }
    let bytes = std::fs::read(format!("/usr/share/zoneinfo/{name}")).ok()?;
    parse_tzif(&bytes, name)
}

fn be32(b: &[u8], at: usize) -> Option<i64> {
    let s: [u8; 4] = b.get(at..at + 4)?.try_into().ok()?;
    Some(i32::from_be_bytes(s) as i64)
}

fn be64(b: &[u8], at: usize) -> Option<i64> {
    let s: [u8; 8] = b.get(at..at + 8)?.try_into().ok()?;
    Some(i64::from_be_bytes(s))
}

/// One TZif header + data block. `wide` selects 8-byte transition times (v2+).
/// Returns the parsed zone and the byte offset just past the block.
fn parse_block(b: &[u8], start: usize, wide: bool, name: &str) -> Option<(TzData, usize)> {
    if b.get(start..start + 4)? != b"TZif" {
        return None;
    }
    let isutcnt = be32(b, start + 20)? as usize;
    let isstdcnt = be32(b, start + 24)? as usize;
    let leapcnt = be32(b, start + 28)? as usize;
    let timecnt = be32(b, start + 32)? as usize;
    let typecnt = be32(b, start + 36)? as usize;
    let charcnt = be32(b, start + 40)? as usize;
    // sanity caps: a real zone file is a few KB
    if timecnt > 100_000 || typecnt > 1000 || charcnt > 10_000 {
        return None;
    }
    let tsize = if wide { 8 } else { 4 };
    let mut at = start + 44;
    let mut trans = Vec::with_capacity(timecnt);
    for i in 0..timecnt {
        trans.push(if wide { be64(b, at + i * tsize)? } else { be32(b, at + i * tsize)? });
    }
    at += timecnt * tsize;
    let idx = b.get(at..at + timecnt)?.to_vec();
    at += timecnt;
    let mut types = Vec::with_capacity(typecnt);
    let abbr_base = at + typecnt * 6;
    let abbrs = b.get(abbr_base..abbr_base + charcnt)?;
    for i in 0..typecnt {
        let off = be32(b, at + i * 6)? as i32;
        let isdst = *b.get(at + i * 6 + 4)? != 0;
        let di = *b.get(at + i * 6 + 5)? as usize;
        let end = abbrs[di.min(abbrs.len())..]
            .iter()
            .position(|&c| c == 0)
            .map(|p| di + p)
            .unwrap_or(abbrs.len());
        let ab = String::from_utf8_lossy(abbrs.get(di..end)?).into_owned();
        types.push((off, isdst, ab));
    }
    at = abbr_base + charcnt;
    // skip leap seconds, isstd, isut
    at += leapcnt * (tsize + 4) + isstdcnt + isutcnt;
    Some((TzData { name: name.to_string(), trans, idx, types }, at))
}

fn parse_tzif(b: &[u8], name: &str) -> Option<TzData> {
    if b.get(..4)? != b"TZif" {
        return None;
    }
    let version = *b.get(4)?;
    let (v1, end) = parse_block(b, 0, false, name)?;
    if version == 0 {
        return Some(v1);
    }
    // v2+: a second block with 64-bit times follows; prefer it
    parse_block(b, end, true, name).map(|(z, _)| z).or(Some(v1))
}

impl TzData {
    /// Transitions within `[begin, end]` for DateTimeZone::getTransitions:
    /// (ts, offset, isdst, abbrev), plus the in-effect state AT `begin` first.
    pub fn transitions(&self, begin: i64, end: i64) -> Vec<(i64, i32, bool, String)> {
        let mut out = Vec::new();
        let (off, dst, ab) = self.offset_at(begin);
        out.push((begin, off, dst, ab.to_string()));
        for (i, &t) in self.trans.iter().enumerate() {
            if t > begin && t <= end {
                let ty = self.idx.get(i).copied().unwrap_or(0) as usize;
                if let Some((o, d, a)) = self.types.get(ty) {
                    out.push((t, *o, *d, a.clone()));
                }
            }
        }
        out
    }
}

/// Convert a wall-clock instant expressed in `tz` local seconds to UTC.
/// Standard two-pass fixup: guess with the offset at the wall time interpreted
/// as UTC, then correct with the offset at the guess. Ambiguous/skipped times
/// resolve the way PHP does most of the time (forward bias).
pub fn ts_from_local(wall: i64, tz: &TzData) -> i64 {
    let (o1, _, _) = tz.offset_at(wall);
    let guess = wall - o1 as i64;
    let (o2, _, _) = tz.offset_at(guess);
    wall - o2 as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nyc_offsets() {
        let Some(tz) = lookup("America/New_York") else { return }; // skip if no zoneinfo
        // 2005-07-14 12:00 UTC → EDT (-4h)
        let (off, dst, ab) = tz.offset_at(1121342400);
        assert_eq!(off, -14400);
        assert!(dst);
        assert_eq!(ab, "EDT");
        // 2005-01-14 12:00 UTC → EST (-5h)
        let (off, dst, ab) = tz.offset_at(1105704000);
        assert_eq!(off, -18000);
        assert!(!dst);
        assert_eq!(ab, "EST");
    }
}
