#![allow(unused_imports)]
#![allow(clippy::all)]
use crate::*;
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

// ---- XML parser (from scratch) ---------------------------------------------

#[derive(Clone)]
pub(crate) struct XmlNode {
    name: String,
    attrs: Vec<(String, String)>,
    children: Vec<XmlNode>,
    text: String,
}

pub(crate) fn xml_decode_entities(s: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '&' {
            if let Some(semi) = chars[i + 1..].iter().position(|&c| c == ';') {
                let ent: String = chars[i + 1..i + 1 + semi].iter().collect();
                let rep = match ent.as_str() {
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "amp" => Some('&'),
                    "quot" => Some('"'),
                    "apos" => Some('\''),
                    _ if ent.starts_with("#x") || ent.starts_with("#X") => {
                        u32::from_str_radix(&ent[2..], 16).ok().and_then(char::from_u32)
                    }
                    _ if ent.starts_with('#') => {
                        ent[1..].parse::<u32>().ok().and_then(char::from_u32)
                    }
                    _ => None,
                };
                if let Some(c) = rep {
                    out.push(c);
                    i += semi + 2;
                    continue;
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

pub(crate) struct XmlParser {
    c: Vec<char>,
    i: usize,
}
impl XmlParser {
    fn skip_ws(&mut self) {
        while matches!(self.c.get(self.i), Some(ch) if ch.is_whitespace()) {
            self.i += 1;
        }
    }
    fn starts(&self, s: &str) -> bool {
        self.c[self.i..].iter().take(s.chars().count()).copied().eq(s.chars())
    }
    /// Skip prologs/comments/PIs/doctype until the next real element start.
    fn skip_misc(&mut self) {
        loop {
            self.skip_ws();
            if self.starts("<?") {
                while self.i < self.c.len() && !self.starts("?>") {
                    self.i += 1;
                }
                self.i += 2;
            } else if self.starts("<!--") {
                self.i += 4;
                while self.i < self.c.len() && !self.starts("-->") {
                    self.i += 1;
                }
                self.i += 3;
            } else if self.starts("<!") {
                // DOCTYPE etc. — skip to matching '>'
                while self.i < self.c.len() && self.c[self.i] != '>' {
                    self.i += 1;
                }
                self.i += 1;
            } else {
                break;
            }
        }
    }
    fn read_name(&mut self) -> String {
        let start = self.i;
        while matches!(self.c.get(self.i), Some(ch) if !ch.is_whitespace() && !matches!(ch, '>' | '/' | '=')) {
            self.i += 1;
        }
        self.c[start..self.i].iter().collect()
    }
    fn parse_element(&mut self) -> Option<XmlNode> {
        self.skip_misc();
        if self.c.get(self.i) != Some(&'<') {
            return None;
        }
        self.i += 1; // consume '<'
        let name = self.read_name();
        if name.is_empty() {
            return None;
        }
        let mut attrs = Vec::new();
        loop {
            self.skip_ws();
            match self.c.get(self.i) {
                Some('/') => {
                    self.i += 1;
                    self.skip_ws();
                    if self.c.get(self.i) == Some(&'>') {
                        self.i += 1;
                    }
                    return Some(XmlNode { name, attrs, children: Vec::new(), text: String::new() });
                }
                Some('>') => {
                    self.i += 1;
                    break;
                }
                None => return Some(XmlNode { name, attrs, children: Vec::new(), text: String::new() }),
                _ => {
                    let aname = self.read_name();
                    self.skip_ws();
                    let mut aval = String::new();
                    if self.c.get(self.i) == Some(&'=') {
                        self.i += 1;
                        self.skip_ws();
                        let q = self.c.get(self.i).copied();
                        if q == Some('"') || q == Some('\'') {
                            let quote = q.unwrap();
                            self.i += 1;
                            let s = self.i;
                            while self.i < self.c.len() && self.c[self.i] != quote {
                                self.i += 1;
                            }
                            aval = xml_decode_entities(&self.c[s..self.i].iter().collect::<String>());
                            self.i += 1;
                        }
                    }
                    if aname.is_empty() {
                        self.i += 1; // avoid infinite loop on malformed input
                    } else {
                        attrs.push((aname, aval));
                    }
                }
            }
        }
        // children + text until </name>
        let mut children = Vec::new();
        let mut text = String::new();
        loop {
            if self.i >= self.c.len() {
                break;
            }
            if self.starts("</") {
                self.i += 2;
                let _ = self.read_name();
                self.skip_ws();
                if self.c.get(self.i) == Some(&'>') {
                    self.i += 1;
                }
                break;
            } else if self.starts("<!--") {
                self.i += 4;
                while self.i < self.c.len() && !self.starts("-->") {
                    self.i += 1;
                }
                self.i += 3;
            } else if self.starts("<![CDATA[") {
                self.i += 9;
                let s = self.i;
                while self.i < self.c.len() && !self.starts("]]>") {
                    self.i += 1;
                }
                text.push_str(&self.c[s..self.i].iter().collect::<String>());
                self.i += 3;
            } else if self.c.get(self.i) == Some(&'<') {
                if let Some(child) = self.parse_element() {
                    children.push(child);
                } else {
                    break;
                }
            } else {
                let s = self.i;
                while self.i < self.c.len() && self.c[self.i] != '<' {
                    self.i += 1;
                }
                text.push_str(&xml_decode_entities(&self.c[s..self.i].iter().collect::<String>()));
            }
        }
        Some(XmlNode { name, attrs, children, text })
    }
}

pub(crate) fn xml_parse(s: &str) -> Option<XmlNode> {
    let mut p = XmlParser { c: s.chars().collect(), i: 0 };
    p.parse_element()
}

pub(crate) fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Serialize a SimpleXMLElement object back to an XML string.
pub(crate) fn xml_dump_value(v: &Value) -> String {
    let o = match v {
        Value::Object(o) => o,
        _ => return String::new(),
    };
    let b = o.borrow();
    let name = b.get("__name").map(|x| x.to_php_string()).unwrap_or_default();
    let text = b.get("__text").map(|x| x.to_php_string()).unwrap_or_default();
    let mut s = format!("<{name}");
    if let Some(Value::Array(attrs)) = b.get("__attrs") {
        for (k, av) in &attrs.entries {
            let kn = match k {
                AKey::Str(s) => s.clone(),
                AKey::Int(i) => i.to_string(),
            };
            s.push_str(&format!(" {}=\"{}\"", kn, xml_escape(&av.to_php_string())));
        }
    }
    let children = match b.get("__children") {
        Some(Value::Array(c)) => c.entries,
        _ => Vec::new(),
    };
    if children.is_empty() && text.is_empty() {
        s.push_str("/>");
    } else {
        s.push('>');
        s.push_str(&xml_escape(&text));
        for (_, ch) in &children {
            s.push_str(&xml_dump_value(ch));
        }
        s.push_str(&format!("</{name}>"));
    }
    s
}

/// Convert a parsed XML tree into a SimpleXMLElement object graph.
pub(crate) fn xml_to_simplexml(node: &XmlNode) -> Value {
    let mut attrs = PArray::default();
    for (k, v) in &node.attrs {
        attrs.set(AKey::Str(k.clone()), Value::Str(v.clone()));
    }
    let mut children = PArray::default();
    for ch in &node.children {
        children.push(xml_to_simplexml(ch));
    }
    // direct text: only the node's own text, trimmed of surrounding whitespace
    let text = node.text.trim().to_string();
    let props = vec![
        ("__name".to_string(), Value::Str(node.name.clone())),
        ("__text".to_string(), Value::Str(text)),
        ("__attrs".to_string(), Value::Array(attrs)),
        ("__children".to_string(), Value::Array(children)),
    ];
    Value::Object(Rc::new(RefCell::new(Obj {
        class: "SimpleXMLElement".to_string(),
        props,
    })))
}

