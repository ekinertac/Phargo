#![allow(unused_imports)]
#![allow(clippy::all)]
use crate::*;
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

// ---- bcmath: arbitrary-precision decimal arithmetic (from scratch) ---------
// Magnitudes are Vec<u8> of decimal digits, least-significant first, trimmed.

/// Cap on the working scale so a pathological `bcscale(1e15)` can't try to build
/// quadrillion-digit numbers (PHP itself rejects such scales with a ValueError).
pub(crate) const BC_SCALE_MAX: usize = 32768;

pub(crate) fn mag_trim(mut a: Vec<u8>) -> Vec<u8> {
    while a.len() > 1 && *a.last().unwrap() == 0 {
        a.pop();
    }
    a
}
pub(crate) fn mag_is_zero(a: &[u8]) -> bool {
    a.iter().all(|&d| d == 0)
}
pub(crate) fn mag_cmp(a: &[u8], b: &[u8]) -> Ordering {
    if a.len() != b.len() {
        return a.len().cmp(&b.len());
    }
    for i in (0..a.len()).rev() {
        if a[i] != b[i] {
            return a[i].cmp(&b[i]);
        }
    }
    Ordering::Equal
}
pub(crate) fn mag_add(a: &[u8], b: &[u8]) -> Vec<u8> {
    let mut r = Vec::with_capacity(a.len().max(b.len()) + 1);
    let mut carry = 0u8;
    for i in 0..a.len().max(b.len()) {
        let s = a.get(i).copied().unwrap_or(0) + b.get(i).copied().unwrap_or(0) + carry;
        r.push(s % 10);
        carry = s / 10;
    }
    if carry > 0 {
        r.push(carry);
    }
    mag_trim(r)
}
pub(crate) fn mag_sub(a: &[u8], b: &[u8]) -> Vec<u8> {
    // assumes a >= b
    let mut r = Vec::with_capacity(a.len());
    let mut borrow = 0i16;
    for i in 0..a.len() {
        let mut d = a[i] as i16 - b.get(i).copied().unwrap_or(0) as i16 - borrow;
        if d < 0 {
            d += 10;
            borrow = 1;
        } else {
            borrow = 0;
        }
        r.push(d as u8);
    }
    mag_trim(r)
}
pub(crate) fn mag_mul(a: &[u8], b: &[u8]) -> Vec<u8> {
    if mag_is_zero(a) || mag_is_zero(b) {
        return vec![0];
    }
    let mut r = vec![0u8; a.len() + b.len()];
    for i in 0..a.len() {
        let mut carry = 0u32;
        for j in 0..b.len() {
            let cur = r[i + j] as u32 + a[i] as u32 * b[j] as u32 + carry;
            r[i + j] = (cur % 10) as u8;
            carry = cur / 10;
        }
        let mut k = i + b.len();
        while carry > 0 {
            let cur = r[k] as u32 + carry;
            r[k] = (cur % 10) as u8;
            carry = cur / 10;
            k += 1;
        }
    }
    mag_trim(r)
}
/// Long division: returns (quotient, remainder) magnitudes, or None on /0.
pub(crate) fn mag_divmod(num: &[u8], den: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    if mag_is_zero(den) {
        return None;
    }
    let mut rem: Vec<u8> = vec![0];
    let mut quot = vec![0u8; num.len()];
    for i in (0..num.len()).rev() {
        rem.insert(0, num[i]); // rem = rem*10 + num[i]
        rem = mag_trim(rem);
        let mut d = 0u8;
        for cand in (1..=9u8).rev() {
            if mag_cmp(&mag_mul(den, &[cand]), &rem) != Ordering::Greater {
                d = cand;
                break;
            }
        }
        quot[i] = d;
        if d > 0 {
            rem = mag_sub(&rem, &mag_mul(den, &[d]));
        }
    }
    Some((mag_trim(quot), mag_trim(rem)))
}
pub(crate) fn mag_isqrt(n: &[u8]) -> Vec<u8> {
    if mag_is_zero(n) {
        return vec![0];
    }
    // initial guess 10^ceil(len/2) is guaranteed >= sqrt(n), so Newton descends.
    let mut x = vec![0u8; n.len().div_ceil(2)];
    x.push(1);
    let mut guard = 0;
    loop {
        guard += 1;
        if guard > 10_000 {
            break;
        }
        let (q, _) = mag_divmod(n, &x).unwrap();
        let half = mag_divmod(&mag_add(&x, &q), &[2]).unwrap().0;
        if mag_cmp(&half, &x) != Ordering::Less {
            break;
        }
        x = half;
    }
    while mag_cmp(&mag_mul(&x, &x), n) == Ordering::Greater {
        x = mag_sub(&x, &[1]);
    }
    x
}

/// Parse a decimal string → (negative, magnitude LSF, fraction-digit count).
pub(crate) fn bc_parse(s: &str) -> (bool, Vec<u8>, usize) {
    let s = s.trim();
    let neg = s.starts_with('-');
    let body = s.trim_start_matches(['+', '-']);
    let (ip, fp) = body.split_once('.').unwrap_or((body, ""));
    let ipd: String = ip.chars().filter(|c| c.is_ascii_digit()).collect();
    let fpd: String = fp.chars().filter(|c| c.is_ascii_digit()).collect();
    let scale = fpd.len();
    let mag: Vec<u8> = format!("{ipd}{fpd}")
        .chars()
        .rev()
        .map(|c| c.to_digit(10).unwrap() as u8)
        .collect();
    let mag = if mag.is_empty() { vec![0] } else { mag_trim(mag) };
    let neg = neg && !mag_is_zero(&mag);
    (neg, mag, scale)
}

/// Format (neg, magnitude with `scale` frac digits) to `out_scale` frac digits.
pub(crate) fn bc_format(neg: bool, mag: &[u8], scale: usize, out_scale: usize) -> String {
    let mut m = mag.to_vec();
    if out_scale < scale {
        m.drain(0..(scale - out_scale).min(m.len()));
    } else {
        let mut nm = vec![0u8; out_scale - scale];
        nm.extend_from_slice(&m);
        m = nm;
    }
    let mut m = mag_trim(m);
    while m.len() <= out_scale {
        m.push(0);
    }
    let frac: String = m[0..out_scale].iter().rev().map(|d| (b'0' + d) as char).collect();
    let intp: String = m[out_scale..].iter().rev().map(|d| (b'0' + d) as char).collect();
    let intp = intp.trim_start_matches('0');
    let intp = if intp.is_empty() { "0" } else { intp };
    let is_zero = intp == "0" && frac.chars().all(|c| c == '0');
    let sign = if neg && !is_zero { "-" } else { "" };
    if out_scale == 0 {
        format!("{sign}{intp}")
    } else {
        format!("{sign}{intp}.{frac}")
    }
}

/// Signed add (sub = add with b negated). Returns (neg, mag, scale).
pub(crate) fn bc_addsub(
    (na, ma, sa): (bool, Vec<u8>, usize),
    (nb, mb, sb): (bool, Vec<u8>, usize),
) -> (bool, Vec<u8>, usize) {
    let cs = sa.max(sb);
    let pad = |m: &[u8], s: usize| -> Vec<u8> {
        let mut v = vec![0u8; cs - s];
        v.extend_from_slice(m);
        mag_trim(v)
    };
    let (ma, mb) = (pad(&ma, sa), pad(&mb, sb));
    if na == nb {
        (na, mag_add(&ma, &mb), cs)
    } else {
        match mag_cmp(&ma, &mb) {
            Ordering::Greater => (na, mag_sub(&ma, &mb), cs),
            Ordering::Less => (nb, mag_sub(&mb, &ma), cs),
            Ordering::Equal => (false, vec![0], cs),
        }
    }
}

pub(crate) fn bc_add(a: &str, b: &str, scale: usize) -> String {
    let (n, m, s) = bc_addsub(bc_parse(a), bc_parse(b));
    bc_format(n, &m, s, scale)
}
pub(crate) fn bc_sub(a: &str, b: &str, scale: usize) -> String {
    let (nb, mb, sb) = bc_parse(b);
    let (n, m, s) = bc_addsub(bc_parse(a), (!nb && !mag_is_zero(&mb), mb, sb));
    bc_format(n, &m, s, scale)
}
pub(crate) fn bc_mul(a: &str, b: &str, scale: usize) -> String {
    let (na, ma, sa) = bc_parse(a);
    let (nb, mb, sb) = bc_parse(b);
    let m = mag_mul(&ma, &mb);
    let neg = (na != nb) && !mag_is_zero(&m);
    bc_format(neg, &m, sa + sb, scale)
}
pub(crate) fn bc_div(a: &str, b: &str, scale: usize) -> Option<String> {
    let (na, ma, sa) = bc_parse(a);
    let (nb, mb, sb) = bc_parse(b);
    if mag_is_zero(&mb) {
        return None;
    }
    // numerator = ma * 10^(sb + scale), denom = mb * 10^sa
    let mut num = ma;
    for _ in 0..(sb + scale) {
        num.insert(0, 0);
    }
    let mut den = mb;
    for _ in 0..sa {
        den.insert(0, 0);
    }
    let (q, _) = mag_divmod(&mag_trim(num), &mag_trim(den))?;
    let neg = (na != nb) && !mag_is_zero(&q);
    Some(bc_format(neg, &q, scale, scale))
}
pub(crate) fn bc_mod(a: &str, b: &str, scale: usize) -> Option<String> {
    // a - (floor-toward-zero(a/b)) * b, computed at integer scale then formatted
    let q = bc_div(a, b, 0)?;
    let prod = bc_mul(&q, b, scale.max(bc_parse(a).2).max(bc_parse(b).2));
    Some(bc_sub(a, &prod, scale))
}
pub(crate) fn bc_comp(a: &str, b: &str, scale: usize) -> i64 {
    let r = bc_sub(a, b, scale + 1);
    let (neg, mag, _) = bc_parse(&r);
    if mag_is_zero(&mag) {
        0
    } else if neg {
        -1
    } else {
        1
    }
}
pub(crate) fn bc_pow(a: &str, exp: &str, scale: usize) -> String {
    let e = bc_format(false, &bc_parse(exp).1, bc_parse(exp).2, 0)
        .parse::<i64>()
        .unwrap_or(0)
        .clamp(0, 10_000);
    let mut result = "1".to_string();
    for _ in 0..e {
        result = bc_mul(&result, a, scale + 4);
    }
    bc_format(
        bc_parse(&result).0,
        &bc_parse(&result).1,
        bc_parse(&result).2,
        scale,
    )
}
pub(crate) fn bc_sqrt(a: &str, scale: usize) -> Option<String> {
    let (na, ma, sa) = bc_parse(a);
    if na {
        return None;
    }
    if mag_is_zero(&ma) {
        return Some(bc_format(false, &[0], 0, scale));
    }
    // n = ma * 10^(2*scale - sa), then result = isqrt(n) with `scale` frac digits
    let shift = 2 * scale as i64 - sa as i64;
    let mut n = ma;
    if shift >= 0 {
        for _ in 0..shift {
            n.insert(0, 0);
        }
    } else {
        n.drain(0..((-shift) as usize).min(n.len()));
        if n.is_empty() {
            n = vec![0];
        }
    }
    let r = mag_isqrt(&mag_trim(n));
    Some(bc_format(false, &r, scale, scale))
}

