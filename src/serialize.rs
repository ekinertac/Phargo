#![allow(unused_imports)]
#![allow(clippy::all)]
use crate::*;
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

// ---- serialize / unserialize -----------------------------------------------

pub(crate) fn php_serialize(v: &Value, depth: usize) -> String {
    if depth > 256 {
        return "N;".into();
    }
    match v {
        Value::Null => "N;".into(),
        Value::Bool(b) => format!("b:{};", if *b { 1 } else { 0 }),
        Value::Int(n) => format!("i:{n};"),
        Value::Float(x) => {
            let s = if x.is_nan() {
                "NAN".into()
            } else if x.is_infinite() {
                if *x > 0.0 { "INF".into() } else { "-INF".into() }
            } else {
                format!("{x}")
            };
            format!("d:{s};")
        }
        Value::Str(s) => format!("s:{}:\"{}\";", s.len(), s),
        Value::Array(a) => {
            let mut out = format!("a:{}:{{", a.entries.len());
            for (k, val) in &a.entries {
                match k {
                    AKey::Int(i) => out.push_str(&format!("i:{i};")),
                    AKey::Str(s) => out.push_str(&format!("s:{}:\"{}\";", s.len(), s)),
                }
                out.push_str(&php_serialize(val, depth + 1));
            }
            out.push('}');
            out
        }
        Value::Object(o) => {
            let b = o.borrow();
            let mut out = format!("O:{}:\"{}\":{}:{{", b.class.len(), b.class, b.props.len());
            for (k, val) in &b.props {
                out.push_str(&format!("s:{}:\"{}\";", k.len(), k));
                out.push_str(&php_serialize(val, depth + 1));
            }
            out.push('}');
            out
        }
        Value::Closure(_) => "N;".into(),
    }
}

pub(crate) fn unser_read_until(b: &[u8], pos: &mut usize, end: u8) -> String {
    let start = *pos;
    while *pos < b.len() && b[*pos] != end {
        *pos += 1;
    }
    let s = String::from_utf8_lossy(&b[start..*pos]).to_string();
    if *pos < b.len() {
        *pos += 1; // consume the delimiter
    }
    s
}

pub(crate) fn unser_string(b: &[u8], pos: &mut usize) -> Option<String> {
    // <len>:"<bytes>";
    let len: usize = unser_read_until(b, pos, b':').parse().ok()?;
    if *pos >= b.len() || b[*pos] != b'"' {
        return None;
    }
    *pos += 1;
    if *pos + len > b.len() {
        return None;
    }
    let s = String::from_utf8_lossy(&b[*pos..*pos + len]).to_string();
    *pos += len;
    // expect "  ;
    if *pos < b.len() && b[*pos] == b'"' {
        *pos += 1;
    }
    if *pos < b.len() && b[*pos] == b';' {
        *pos += 1;
    }
    Some(s)
}

pub(crate) fn php_unserialize(b: &[u8], pos: &mut usize, depth: usize) -> Option<Value> {
    if depth > 256 || *pos >= b.len() {
        return None;
    }
    let t = b[*pos];
    *pos += 1;
    match t {
        b'N' => {
            if *pos < b.len() && b[*pos] == b';' {
                *pos += 1;
            }
            Some(Value::Null)
        }
        b'b' => {
            *pos += 1; // skip ':'
            let v = b.get(*pos).copied() == Some(b'1');
            *pos += 1;
            if *pos < b.len() && b[*pos] == b';' {
                *pos += 1;
            }
            Some(Value::Bool(v))
        }
        b':' => unreachable!(),
        b'i' => {
            *pos += 1; // skip ':'
            unser_read_until(b, pos, b';').parse().ok().map(Value::Int)
        }
        b'd' => {
            *pos += 1; // skip ':'
            let s = unser_read_until(b, pos, b';');
            let x = match s.as_str() {
                "NAN" => f64::NAN,
                "INF" => f64::INFINITY,
                "-INF" => f64::NEG_INFINITY,
                _ => s.parse().ok()?,
            };
            Some(Value::Float(x))
        }
        b's' => {
            *pos += 1; // skip ':'
            unser_string(b, pos).map(Value::Str)
        }
        b'a' => {
            *pos += 1; // skip ':'
            let count: usize = unser_read_until(b, pos, b':').parse().ok()?;
            if *pos < b.len() && b[*pos] == b'{' {
                *pos += 1;
            }
            let mut arr = PArray::default();
            for _ in 0..count {
                let key = php_unserialize(b, pos, depth + 1)?;
                let val = php_unserialize(b, pos, depth + 1)?;
                arr.set(key_from_value(&key), val);
            }
            if *pos < b.len() && b[*pos] == b'}' {
                *pos += 1;
            }
            Some(Value::Array(arr))
        }
        b'O' => {
            *pos += 1; // skip ':'
            let class = unser_string(b, pos)?;
            if *pos < b.len() && b[*pos] == b':' {
                *pos += 1; // separator between class name and property count
            }
            let count: usize = unser_read_until(b, pos, b':').parse().ok()?;
            if *pos < b.len() && b[*pos] == b'{' {
                *pos += 1;
            }
            let mut props: Vec<(String, Value)> = Vec::new();
            for _ in 0..count {
                let key = php_unserialize(b, pos, depth + 1)?;
                let val = php_unserialize(b, pos, depth + 1)?;
                props.push((key.to_php_string(), val));
            }
            if *pos < b.len() && b[*pos] == b'}' {
                *pos += 1;
            }
            Some(Value::Object(Rc::new(RefCell::new(Obj { class, props }))))
        }
        _ => None,
    }
}

