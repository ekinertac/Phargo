#![allow(unused_imports)]
#![allow(clippy::all)]
use crate::*;
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

// ---- JSON ------------------------------------------------------------------

pub(crate) fn json_encode_value(v: &Value, depth: usize) -> String {
    if depth > 512 {
        return "null".to_string();
    }
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => if *b { "true".into() } else { "false".into() },
        Value::Int(n) => n.to_string(),
        Value::Float(x) => {
            if x.is_finite() {
                let s = format_php_float(*x);
                if s.contains(['.', 'e', 'E']) {
                    s
                } else {
                    format!("{s}.0")
                }
            } else {
                "0".to_string()
            }
        }
        Value::Str(s) => json_encode_string(s),
        Value::Array(a) => {
            let is_list = a
                .entries
                .iter()
                .enumerate()
                .all(|(i, (k, _))| matches!(k, AKey::Int(n) if *n == i as i64));
            if is_list {
                let parts: Vec<String> = a
                    .entries
                    .iter()
                    .map(|(_, v)| json_encode_value(v, depth + 1))
                    .collect();
                format!("[{}]", parts.join(","))
            } else {
                let parts: Vec<String> = a
                    .entries
                    .iter()
                    .map(|(k, v)| {
                        let key = match k {
                            AKey::Int(n) => n.to_string(),
                            AKey::Str(s) => s.clone(),
                        };
                        format!("{}:{}", json_encode_string(&key), json_encode_value(v, depth + 1))
                    })
                    .collect();
                format!("{{{}}}", parts.join(","))
            }
        }
        Value::Object(o) => {
            let ob = o.borrow();
            let parts: Vec<String> = ob
                .props
                .iter()
                .map(|(k, v)| format!("{}:{}", json_encode_string(k), json_encode_value(v, depth + 1)))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Closure(_) => "null".to_string(),
    }
}

pub(crate) fn json_encode_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '/' => out.push_str("\\/"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub(crate) fn json_decode_str(s: &str, assoc: bool) -> Option<Value> {
    let chars: Vec<char> = s.chars().collect();
    let mut p = 0usize;
    let v = json_parse(&chars, &mut p, assoc, 0)?;
    json_ws(&chars, &mut p);
    if p >= chars.len() {
        Some(v)
    } else {
        None
    }
}

pub(crate) fn json_ws(c: &[char], p: &mut usize) {
    while *p < c.len() && c[*p].is_whitespace() {
        *p += 1;
    }
}

pub(crate) fn json_parse(c: &[char], p: &mut usize, assoc: bool, depth: usize) -> Option<Value> {
    if depth > 512 {
        return None;
    }
    json_ws(c, p);
    let ch = *c.get(*p)?;
    match ch {
        '{' => {
            *p += 1;
            let mut arr = PArray::default();
            let mut props: Vec<(String, Value)> = Vec::new();
            json_ws(c, p);
            if c.get(*p) == Some(&'}') {
                *p += 1;
            } else {
                loop {
                    json_ws(c, p);
                    let key = json_string(c, p)?;
                    json_ws(c, p);
                    if c.get(*p) != Some(&':') {
                        return None;
                    }
                    *p += 1;
                    let val = json_parse(c, p, assoc, depth + 1)?;
                    if assoc {
                        arr.set(key_from_value(&Value::Str(key)), val);
                    } else {
                        props.push((key, val));
                    }
                    json_ws(c, p);
                    match c.get(*p) {
                        Some(',') => {
                            *p += 1;
                            continue;
                        }
                        Some('}') => {
                            *p += 1;
                            break;
                        }
                        _ => return None,
                    }
                }
            }
            if assoc {
                Some(Value::Array(arr))
            } else {
                Some(Value::Object(Rc::new(RefCell::new(Obj {
                    class: "stdClass".to_string(),
                    props,
                }))))
            }
        }
        '[' => {
            *p += 1;
            let mut arr = PArray::default();
            json_ws(c, p);
            if c.get(*p) == Some(&']') {
                *p += 1;
            } else {
                loop {
                    let val = json_parse(c, p, assoc, depth + 1)?;
                    arr.push(val);
                    json_ws(c, p);
                    match c.get(*p) {
                        Some(',') => {
                            *p += 1;
                            continue;
                        }
                        Some(']') => {
                            *p += 1;
                            break;
                        }
                        _ => return None,
                    }
                }
            }
            Some(Value::Array(arr))
        }
        '"' => Some(Value::Str(json_string(c, p)?)),
        't' => {
            if c[*p..].starts_with(&['t', 'r', 'u', 'e']) {
                *p += 4;
                Some(Value::Bool(true))
            } else {
                None
            }
        }
        'f' => {
            if c[*p..].starts_with(&['f', 'a', 'l', 's', 'e']) {
                *p += 5;
                Some(Value::Bool(false))
            } else {
                None
            }
        }
        'n' => {
            if c[*p..].starts_with(&['n', 'u', 'l', 'l']) {
                *p += 4;
                Some(Value::Null)
            } else {
                None
            }
        }
        _ => {
            let start = *p;
            if c.get(*p) == Some(&'-') {
                *p += 1;
            }
            while *p < c.len() && (c[*p].is_ascii_digit() || matches!(c[*p], '.' | 'e' | 'E' | '+' | '-')) {
                *p += 1;
            }
            let numstr: String = c[start..*p].iter().collect();
            if let Ok(n) = numstr.parse::<i64>() {
                Some(Value::Int(n))
            } else if let Ok(f) = numstr.parse::<f64>() {
                Some(Value::Float(f))
            } else {
                None
            }
        }
    }
}

pub(crate) fn json_string(c: &[char], p: &mut usize) -> Option<String> {
    if c.get(*p) != Some(&'"') {
        return None;
    }
    *p += 1;
    let mut s = String::new();
    while let Some(&ch) = c.get(*p) {
        match ch {
            '"' => {
                *p += 1;
                return Some(s);
            }
            '\\' => {
                *p += 1;
                let e = *c.get(*p)?;
                match e {
                    '"' => s.push('"'),
                    '\\' => s.push('\\'),
                    '/' => s.push('/'),
                    'n' => s.push('\n'),
                    'r' => s.push('\r'),
                    't' => s.push('\t'),
                    'b' => s.push('\u{8}'),
                    'f' => s.push('\u{c}'),
                    'u' => {
                        let hex: String = c.get(*p + 1..*p + 5)?.iter().collect();
                        let code = u32::from_str_radix(&hex, 16).ok()?;
                        s.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                        *p += 4;
                    }
                    _ => return None,
                }
                *p += 1;
            }
            _ => {
                s.push(ch);
                *p += 1;
            }
        }
    }
    None
}

