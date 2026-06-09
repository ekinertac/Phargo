#![allow(unused_imports)]
#![allow(clippy::all)]
use crate::*;
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

// ---- built-in output / formatting helpers ----------------------------------

pub(crate) fn php_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "NULL",
        Value::Bool(_) => "boolean",
        Value::Int(_) => "integer",
        Value::Float(_) => "double",
        Value::Str(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
        Value::Closure(_) => "object",
    }
}

/// `var_dump` output (with trailing newline). `indent` is the leading space
/// count for this value's line — arrays recurse with `indent + 2`.
pub(crate) fn obj_ptr(o: &ObjRef) -> usize {
    Rc::as_ptr(o) as *const () as usize
}

pub(crate) fn var_dump_str(v: &Value, indent: usize) -> String {
    var_dump_seen(v, indent, &mut Vec::new())
}

pub(crate) fn var_dump_seen(v: &Value, indent: usize, seen: &mut Vec<usize>) -> String {
    if indent > 4096 {
        return format!("{}*RECURSION*\n", " ".repeat(indent));
    }
    let pad = " ".repeat(indent);
    match v {
        Value::Int(n) => format!("{pad}int({n})\n"),
        Value::Float(x) => format!("{pad}float({})\n", format_php_float(*x)),
        Value::Bool(b) => format!("{pad}bool({})\n", if *b { "true" } else { "false" }),
        Value::Str(s) => format!("{pad}string({}) \"{}\"\n", s.len(), s),
        Value::Null => format!("{pad}NULL\n"),
        Value::Array(a) => {
            let mut out = format!("{pad}array({}) {{\n", a.entries.len());
            let kp = " ".repeat(indent + 2);
            for (k, val) in &a.entries {
                let ks = match k {
                    AKey::Int(i) => format!("[{i}]"),
                    AKey::Str(s) => format!("[\"{s}\"]"),
                };
                out.push_str(&format!("{kp}{ks}=>\n"));
                out.push_str(&var_dump_seen(val, indent + 2, seen));
            }
            out.push_str(&format!("{pad}}}\n"));
            out
        }
        Value::Object(o) => {
            let ptr = obj_ptr(o);
            if seen.contains(&ptr) {
                return format!("{pad}*RECURSION*\n");
            }
            seen.push(ptr);
            let ob = o.borrow();
            let mut out = format!("{pad}object({})#1 ({}) {{\n", ob.class, ob.props.len());
            let kp = " ".repeat(indent + 2);
            for (n, v) in &ob.props {
                out.push_str(&format!("{kp}[\"{n}\"]=>\n"));
                out.push_str(&var_dump_seen(v, indent + 2, seen));
            }
            out.push_str(&format!("{pad}}}\n"));
            seen.pop();
            out
        }
        Value::Closure(_) => format!("{pad}object(Closure)#1 (0) {{\n{pad}}}\n"),
    }
}

pub(crate) fn print_r_str(v: &Value) -> String {
    print_r_inner(v, 0, &mut Vec::new())
}

pub(crate) fn print_r_inner(v: &Value, depth: usize, seen: &mut Vec<usize>) -> String {
    if depth > 4096 {
        return " *RECURSION*".to_string();
    }
    match v {
        Value::Array(a) => {
            let paren = " ".repeat(depth * 8);
            let item = " ".repeat(depth * 8 + 4);
            let mut s = String::from("Array\n");
            s.push_str(&format!("{paren}(\n"));
            for (k, val) in &a.entries {
                let ks = match k {
                    AKey::Int(i) => i.to_string(),
                    AKey::Str(st) => st.clone(),
                };
                s.push_str(&format!("{item}[{ks}] => {}\n", print_r_inner(val, depth + 1, seen)));
            }
            s.push_str(&format!("{paren})\n"));
            s
        }
        Value::Object(o) => {
            let ptr = obj_ptr(o);
            if seen.contains(&ptr) {
                return format!("{} Object\n *RECURSION*", o.borrow().class);
            }
            seen.push(ptr);
            let ob = o.borrow();
            let paren = " ".repeat(depth * 8);
            let item = " ".repeat(depth * 8 + 4);
            let mut s = format!("{} Object\n", ob.class);
            s.push_str(&format!("{paren}(\n"));
            for (n, v) in &ob.props {
                s.push_str(&format!("{item}[{n}] => {}\n", print_r_inner(v, depth + 1, seen)));
            }
            s.push_str(&format!("{paren})\n"));
            seen.pop();
            s
        }
        _ => v.to_php_string(),
    }
}

pub(crate) fn var_export_str(v: &Value) -> String {
    var_export_inner(v, 0, &mut Vec::new())
}

pub(crate) fn var_export_inner(v: &Value, indent: usize, seen: &mut Vec<usize>) -> String {
    if indent > 4096 {
        return "NULL".to_string();
    }
    match v {
        Value::Null => "NULL".to_string(),
        Value::Bool(b) => if *b { "true".into() } else { "false".into() },
        Value::Int(n) => n.to_string(),
        Value::Float(x) => {
            let s = format_php_float(*x);
            if s.contains(['.', 'e', 'E', 'N', 'I']) {
                s
            } else {
                format!("{s}.0") // var_export keeps floats float-looking: 1 -> 1.0
            }
        }
        Value::Str(s) => format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'")),
        Value::Array(a) => {
            let pad = " ".repeat(indent);
            let ipad = " ".repeat(indent + 2);
            let mut s = String::from("array (\n");
            for (k, val) in &a.entries {
                let ks = match k {
                    AKey::Int(i) => i.to_string(),
                    AKey::Str(st) => format!("'{}'", st.replace('\\', "\\\\").replace('\'', "\\'")),
                };
                match val {
                    Value::Array(_) => s.push_str(&format!(
                        "{ipad}{ks} => \n{ipad}{},\n",
                        var_export_inner(val, indent + 2, seen)
                    )),
                    _ => s.push_str(&format!(
                        "{ipad}{ks} => {},\n",
                        var_export_inner(val, indent + 2, seen)
                    )),
                }
            }
            s.push_str(&format!("{pad})"));
            s
        }
        Value::Object(o) => {
            let ptr = obj_ptr(o);
            if seen.contains(&ptr) {
                return "NULL".to_string();
            }
            seen.push(ptr);
            let ob = o.borrow();
            let pad = " ".repeat(indent);
            let ipad = " ".repeat(indent + 2);
            let mut s = format!("\\{}::__set_state(array(\n", ob.class);
            for (n, v) in &ob.props {
                s.push_str(&format!("{ipad}'{n}' => {},\n", var_export_inner(v, indent + 2, seen)));
            }
            s.push_str(&format!("{pad}))"));
            seen.pop();
            s
        }
        Value::Closure(_) => "NULL".to_string(),
    }
}

