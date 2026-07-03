//! From-scratch arbitrary-precision decimal arithmetic — the engine's bcmath.
//!
//! Numbers are sign + little-endian base-10 digits + a decimal scale
//! (value = digits × 10^-scale). Pure integer digit math: schoolbook add/sub/
//! mul, long division truncated to the requested scale (PHP semantics — no
//! rounding), Newton's method for sqrt. Digit counts are capped so corpus
//! stress tests (huge bcpow operands) fail cleanly instead of eating RAM.
//!
//! Consumed by `eval.rs`'s bc* builtins (bcadd/bcsub/bcmul/bcdiv/bcmod/bcpow/
//! bcsqrt/bccomp/bcscale); parse errors surface as PHP ValueErrors there.

/// Hard ceiling on digit-vector length — far above any legitimate test value.
const MAX_DIGITS: usize = 100_000;

#[derive(Clone, Debug)]
pub struct Dec {
    pub neg: bool,
    /// base-10 digits, least-significant first; value = Σ d[i]·10^i × 10^-scale
    pub digits: Vec<u8>,
    pub scale: usize,
}

impl Dec {
    pub fn zero() -> Dec {
        Dec { neg: false, digits: vec![0], scale: 0 }
    }

    /// Parse a PHP bcmath numeric string. None = not well-formed.
    pub fn parse(s: &str) -> Option<Dec> {
        let t = s.trim();
        let b = t.as_bytes();
        if b.is_empty() {
            return None;
        }
        let (neg, rest) = match b[0] {
            b'-' => (true, &b[1..]),
            b'+' => (false, &b[1..]),
            _ => (false, b),
        };
        let mut int_part: Vec<u8> = Vec::new();
        let mut frac_part: Vec<u8> = Vec::new();
        let mut seen_dot = false;
        let mut any_digit = false;
        for &c in rest {
            match c {
                b'0'..=b'9' => {
                    any_digit = true;
                    if seen_dot {
                        frac_part.push(c - b'0');
                    } else {
                        int_part.push(c - b'0');
                    }
                }
                b'.' if !seen_dot => seen_dot = true,
                _ => return None,
            }
        }
        if !any_digit {
            return None;
        }
        if int_part.len() + frac_part.len() > MAX_DIGITS {
            return None;
        }
        // digits little-endian: frac (reversed) then int (reversed)
        let scale = frac_part.len();
        let mut digits: Vec<u8> = frac_part.iter().rev().cloned().collect();
        digits.extend(int_part.iter().rev());
        let mut d = Dec { neg, digits, scale };
        d.trim();
        Some(d)
    }

    /// Drop leading (most-significant) zeros; canonicalize -0 to 0.
    fn trim(&mut self) {
        while self.digits.len() > self.scale + 1 && *self.digits.last().unwrap() == 0 {
            self.digits.pop();
        }
        if self.digits.iter().all(|&d| d == 0) {
            self.neg = false;
        }
    }

    pub fn is_zero(&self) -> bool {
        self.digits.iter().all(|&d| d == 0)
    }

    /// Re-scale (pad or truncate the fractional digits) to exactly `scale`.
    pub fn with_scale(&self, scale: usize) -> Dec {
        let mut d = self.clone();
        while d.scale < scale {
            d.digits.insert(0, 0);
            d.scale += 1;
        }
        while d.scale > scale {
            d.digits.remove(0);
            d.scale -= 1;
        }
        if d.digits.is_empty() {
            d.digits.push(0);
        }
        d.trim();
        d
    }

    /// Render with exactly `scale` decimals (PHP keeps trailing zeros).
    pub fn to_string_scaled(&self, scale: usize) -> String {
        let d = self.with_scale(scale);
        let mut out = String::new();
        if d.neg && !d.is_zero() {
            out.push('-');
        }
        let n = d.digits.len();
        // integer digits: indices scale..n (most-significant last)
        for i in (d.scale..n).rev() {
            out.push((b'0' + d.digits[i]) as char);
        }
        if d.scale > 0 {
            out.push('.');
            for i in (0..d.scale).rev() {
                out.push((b'0' + d.digits[i]) as char);
            }
        }
        out
    }
}

/// Compare magnitudes of two aligned-scale digit vectors.
fn cmp_mag(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    if a.len() != b.len() {
        // caller aligns lengths; safety net
        return a.len().cmp(&b.len());
    }
    for i in (0..a.len()).rev() {
        match a[i].cmp(&b[i]) {
            Ordering::Equal => {}
            o => return o,
        }
    }
    Ordering::Equal
}

/// Align two numbers to a common scale and equal digit length.
fn align(a: &Dec, b: &Dec) -> (Vec<u8>, Vec<u8>, usize) {
    let scale = a.scale.max(b.scale);
    let aa = a.with_scale(scale);
    let bb = b.with_scale(scale);
    let len = aa.digits.len().max(bb.digits.len());
    let mut x = aa.digits;
    let mut y = bb.digits;
    x.resize(len, 0);
    y.resize(len, 0);
    (x, y, scale)
}

fn add_mag(x: &[u8], y: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(x.len() + 1);
    let mut carry = 0u8;
    for i in 0..x.len() {
        let s = x[i] + y[i] + carry;
        out.push(s % 10);
        carry = s / 10;
    }
    if carry > 0 {
        out.push(carry);
    }
    out
}

/// x - y, requires x >= y in magnitude.
fn sub_mag(x: &[u8], y: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(x.len());
    let mut borrow = 0i8;
    for i in 0..x.len() {
        let mut d = x[i] as i8 - y[i] as i8 - borrow;
        if d < 0 {
            d += 10;
            borrow = 1;
        } else {
            borrow = 0;
        }
        out.push(d as u8);
    }
    out
}

pub fn add(a: &Dec, b: &Dec) -> Dec {
    let (x, y, scale) = align(a, b);
    let mut r = if a.neg == b.neg {
        Dec { neg: a.neg, digits: add_mag(&x, &y), scale }
    } else {
        match cmp_mag(&x, &y) {
            std::cmp::Ordering::Less => Dec { neg: b.neg, digits: sub_mag(&y, &x), scale },
            _ => Dec { neg: a.neg, digits: sub_mag(&x, &y), scale },
        }
    };
    r.trim();
    r
}

pub fn sub(a: &Dec, b: &Dec) -> Dec {
    let mut nb = b.clone();
    nb.neg = !nb.neg;
    add(a, &nb)
}

pub fn cmp(a: &Dec, b: &Dec) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let d = sub(a, b);
    if d.is_zero() {
        Ordering::Equal
    } else if d.neg {
        Ordering::Less
    } else {
        Ordering::Greater
    }
}

pub fn mul(a: &Dec, b: &Dec) -> Option<Dec> {
    if a.digits.len() + b.digits.len() > MAX_DIGITS {
        return None;
    }
    let mut out = vec![0u32; a.digits.len() + b.digits.len()];
    for (i, &da) in a.digits.iter().enumerate() {
        if da == 0 {
            continue;
        }
        for (j, &db) in b.digits.iter().enumerate() {
            out[i + j] += da as u32 * db as u32;
        }
    }
    // carry pass
    let mut digits = Vec::with_capacity(out.len());
    let mut carry = 0u32;
    for v in out {
        let s = v + carry;
        digits.push((s % 10) as u8);
        carry = s / 10;
    }
    while carry > 0 {
        digits.push((carry % 10) as u8);
        carry /= 10;
    }
    let mut r = Dec { neg: a.neg != b.neg, digits, scale: a.scale + b.scale };
    r.trim();
    Some(r)
}

/// Truncating division to `scale` fractional digits. None on division by zero
/// or digit overflow.
pub fn div(a: &Dec, b: &Dec, scale: usize) -> Option<Dec> {
    if b.is_zero() {
        return None;
    }
    // Scale numerator so integer division yields `scale` fraction digits:
    // shift a left by (b.scale + scale - a.scale) decimal places.
    let shift = (b.scale + scale) as isize - a.scale as isize;
    let mut num: Vec<u8> = a.digits.clone();
    if shift >= 0 {
        if num.len() + shift as usize > MAX_DIGITS {
            return None;
        }
        let mut shifted = vec![0u8; shift as usize];
        shifted.extend(num);
        num = shifted;
    } else {
        let cut = (-shift) as usize;
        if cut >= num.len() {
            num = vec![0];
        } else {
            num.drain(0..cut);
        }
    }
    // long division of num by b.digits (both little-endian magnitudes)
    let den: Vec<u8> = b.digits.clone();
    let q = div_mag(&num, &den)?;
    let mut r = Dec { neg: a.neg != b.neg, digits: q, scale };
    if r.digits.is_empty() {
        r.digits.push(0);
    }
    r.trim();
    Some(r)
}

/// Magnitude long division: num / den (little-endian digit vectors).
fn div_mag(num: &[u8], den: &[u8]) -> Option<Vec<u8>> {
    // strip leading zeros of den
    let mut d = den.to_vec();
    while d.len() > 1 && *d.last().unwrap() == 0 {
        d.pop();
    }
    if d == [0] {
        return None;
    }
    let mut quotient = vec![0u8; num.len()];
    let mut rem: Vec<u8> = Vec::new(); // little-endian remainder
    for i in (0..num.len()).rev() {
        // rem = rem * 10 + num[i]
        rem.insert(0, num[i]);
        while rem.len() > 1 && *rem.last().unwrap() == 0 {
            rem.pop();
        }
        // find q in 0..=9 with q*d <= rem
        let mut q = 0u8;
        loop {
            if q == 9 {
                break;
            }
            // try (q+1)*d <= rem
            let trial = mul_small(&d, q + 1);
            if mag_le(&trial, &rem) {
                q += 1;
            } else {
                break;
            }
        }
        if q > 0 {
            let prod = mul_small(&d, q);
            rem = sub_aligned(&rem, &prod);
        }
        quotient[i] = q;
    }
    Some(quotient)
}

fn mul_small(d: &[u8], m: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(d.len() + 1);
    let mut carry = 0u32;
    for &x in d {
        let s = x as u32 * m as u32 + carry;
        out.push((s % 10) as u8);
        carry = s / 10;
    }
    while carry > 0 {
        out.push((carry % 10) as u8);
        carry /= 10;
    }
    out
}

fn mag_le(a: &[u8], b: &[u8]) -> bool {
    let la = a.iter().rposition(|&x| x != 0).map(|i| i + 1).unwrap_or(0);
    let lb = b.iter().rposition(|&x| x != 0).map(|i| i + 1).unwrap_or(0);
    if la != lb {
        return la < lb;
    }
    for i in (0..la).rev() {
        if a[i] != b[i] {
            return a[i] < b[i];
        }
    }
    true
}

/// a - b for little-endian magnitudes with a >= b (lengths may differ).
fn sub_aligned(a: &[u8], b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(a.len());
    let mut borrow = 0i8;
    for i in 0..a.len() {
        let bv = if i < b.len() { b[i] as i8 } else { 0 };
        let mut d = a[i] as i8 - bv - borrow;
        if d < 0 {
            d += 10;
            borrow = 1;
        } else {
            borrow = 0;
        }
        out.push(d as u8);
    }
    out
}

/// PHP bcmod: remainder of truncating division, sign follows the dividend.
pub fn modulo(a: &Dec, b: &Dec, scale: usize) -> Option<Dec> {
    if b.is_zero() {
        return None;
    }
    // q = trunc(a / b) at scale 0, then r = a - q*b, rendered at `scale`
    let q = div(a, b, 0)?;
    let qb = mul(&q, b)?;
    let mut r = sub(a, &qb);
    r = r.with_scale(scale.max(a.scale.max(b.scale)));
    Some(r)
}

/// a^n for integer n >= 0 (repeated squaring); negative handled by caller.
pub fn pow(a: &Dec, mut n: u64, scale: usize) -> Option<Dec> {
    let mut base = a.clone();
    let mut acc = Dec::parse("1").unwrap();
    if n == 0 {
        return Some(acc.with_scale(scale));
    }
    // guard: result digit estimate
    if a.digits.len().saturating_mul(n as usize) > MAX_DIGITS {
        return None;
    }
    while n > 0 {
        if n & 1 == 1 {
            acc = mul(&acc, &base)?;
        }
        n >>= 1;
        if n > 0 {
            base = mul(&base, &base)?;
        }
    }
    Some(acc.with_scale(scale.max(acc.scale.min(scale.max(a.scale)))))
}

/// Integer-scaled Newton's method square root, truncated to `scale`.
pub fn sqrt(a: &Dec, scale: usize) -> Option<Dec> {
    if a.neg && !a.is_zero() {
        return None;
    }
    if a.is_zero() {
        return Some(Dec::zero().with_scale(scale));
    }
    // work at extra precision then truncate
    let work = scale + 2;
    let mut x = div(a, &Dec::parse("2").unwrap(), work)?; // initial guess a/2
    if x.is_zero() {
        x = a.with_scale(work);
    }
    let two = Dec::parse("2").unwrap();
    for _ in 0..200 {
        // x' = (x + a/x) / 2
        let ax = div(a, &x, work)?;
        let sum = add(&x, &ax);
        let nx = div(&sum, &two, work)?;
        if cmp(&nx, &x) == std::cmp::Ordering::Equal {
            break;
        }
        x = nx;
    }
    Some(x.with_scale(scale))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn s(x: &str) -> Dec {
        Dec::parse(x).unwrap()
    }
    #[test]
    fn basics() {
        assert_eq!(add(&s("1.1"), &s("2.2")).to_string_scaled(1), "3.3");
        assert_eq!(sub(&s("1"), &s("2.5")).to_string_scaled(1), "-1.5");
        assert_eq!(mul(&s("12345678901234567890"), &s("98765432109876543210")).unwrap().to_string_scaled(0), "1219326311370217952237463801111263526900");
        assert_eq!(div(&s("10"), &s("3"), 5).unwrap().to_string_scaled(5), "3.33333");
        assert_eq!(div(&s("1"), &s("7"), 20).unwrap().to_string_scaled(20), "0.14285714285714285714");
        assert_eq!(modulo(&s("10"), &s("3"), 0).unwrap().to_string_scaled(0), "1");
        assert_eq!(pow(&s("2"), 64, 0).unwrap().to_string_scaled(0), "18446744073709551616");
        assert_eq!(sqrt(&s("9"), 3).unwrap().to_string_scaled(3), "3.000");
        assert_eq!(sqrt(&s("2"), 10).unwrap().to_string_scaled(10), "1.4142135623");
        assert_eq!(cmp(&s("1.0"), &s("1")), std::cmp::Ordering::Equal);
    }
}
