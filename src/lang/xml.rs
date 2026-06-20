//! A small, from-scratch XML parser for `DOMDocument`/`simplexml`.
//!
//! It turns XML text into a nested-array tree of `Value`s (the engine's own
//! value type), which the DOM prelude classes then build a mutable object tree
//! from. Keeping the parser in Rust and the DOM semantics in PHP keeps each side
//! small. Handles elements, attributes, text, CDATA, comments, the `<?xml?>`
//! prolog, `<!DOCTYPE>`, self-closing tags, and the five predefined entities
//! plus numeric character references.
//!
//! Node shape (string-keyed arrays):
//!   element: { t:1, name:Str, attrs:[name=>value...], kids:[node...] }
//!   text:    { t:3, text:Str }
//!   cdata:   { t:4, text:Str }
//!   comment: { t:8, text:Str }

use super::value::{Arr, Key, Value};

pub fn parse(src: &[u8]) -> Result<Value, String> {
    let mut p = XmlP { s: src, i: 0 };
    p.parse_document()
}

struct XmlP<'a> {
    s: &'a [u8],
    i: usize,
}

impl<'a> XmlP<'a> {
    fn starts(&self, pat: &[u8]) -> bool {
        self.s[self.i..].starts_with(pat)
    }

    fn skip_ws(&mut self) {
        while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }

    /// Advance to just past the next occurrence of `pat` (or end).
    fn skip_past(&mut self, pat: &[u8]) {
        match find(&self.s[self.i..], pat) {
            Some(off) => self.i += off + pat.len(),
            None => self.i = self.s.len(),
        }
    }

    fn read_name(&mut self) -> Vec<u8> {
        let start = self.i;
        while self.i < self.s.len() {
            let c = self.s[self.i];
            if c.is_ascii_whitespace() || matches!(c, b'>' | b'/' | b'=' | b'<') {
                break;
            }
            self.i += 1;
        }
        self.s[start..self.i].to_vec()
    }

    fn parse_document(&mut self) -> Result<Value, String> {
        loop {
            self.skip_ws();
            if self.i >= self.s.len() {
                return Err("no root element".into());
            }
            if self.starts(b"<?") {
                self.skip_past(b"?>");
                continue;
            }
            if self.starts(b"<!--") {
                self.skip_past(b"-->");
                continue;
            }
            if self.starts(b"<!") {
                // DOCTYPE (we don't model internal subsets / nested brackets deeply)
                self.skip_past(b">");
                continue;
            }
            break;
        }
        self.parse_element()
    }

    fn parse_element(&mut self) -> Result<Value, String> {
        if !self.starts(b"<") {
            return Err("expected element".into());
        }
        self.i += 1;
        let name = self.read_name();
        if name.is_empty() {
            return Err("empty tag name".into());
        }
        let mut attrs = Arr::new();
        loop {
            self.skip_ws();
            if self.i >= self.s.len() {
                return Err("unterminated tag".into());
            }
            if self.starts(b"/>") {
                self.i += 2;
                return Ok(element(name, attrs, Arr::new()));
            }
            if self.starts(b">") {
                self.i += 1;
                break;
            }
            let aname = self.read_name();
            if aname.is_empty() {
                return Err("bad attribute".into());
            }
            self.skip_ws();
            let mut aval = Vec::new();
            if self.starts(b"=") {
                self.i += 1;
                self.skip_ws();
                if let Some(&q) = self.s.get(self.i) {
                    if q == b'"' || q == b'\'' {
                        self.i += 1;
                        let start = self.i;
                        while self.i < self.s.len() && self.s[self.i] != q {
                            self.i += 1;
                        }
                        aval = decode_entities(&self.s[start..self.i]);
                        self.i += 1; // closing quote
                    }
                }
            }
            attrs.insert(Key::Str(aname), Value::Str(aval));
        }
        // children
        let mut kids = Arr::new();
        loop {
            if self.i >= self.s.len() {
                break;
            }
            if self.starts(b"</") {
                self.i += 2;
                let _close = self.read_name();
                self.skip_ws();
                if self.starts(b">") {
                    self.i += 1;
                }
                break;
            }
            if self.starts(b"<!--") {
                let start = self.i + 4;
                let end = find(&self.s[start..], b"-->").map(|o| start + o).unwrap_or(self.s.len());
                kids.push(text_like(8, self.s[start..end].to_vec()));
                self.i = (end + 3).min(self.s.len());
                continue;
            }
            if self.starts(b"<![CDATA[") {
                let start = self.i + 9;
                let end = find(&self.s[start..], b"]]>").map(|o| start + o).unwrap_or(self.s.len());
                kids.push(text_like(4, self.s[start..end].to_vec()));
                self.i = (end + 3).min(self.s.len());
                continue;
            }
            if self.starts(b"<?") {
                self.skip_past(b"?>");
                continue;
            }
            if self.starts(b"<") {
                kids.push(self.parse_element()?);
                continue;
            }
            // text run
            let start = self.i;
            while self.i < self.s.len() && self.s[self.i] != b'<' {
                self.i += 1;
            }
            kids.push(text_like(3, decode_entities(&self.s[start..self.i])));
        }
        Ok(element(name, attrs, kids))
    }
}

fn element(name: Vec<u8>, attrs: Arr, kids: Arr) -> Value {
    let mut a = Arr::new();
    a.insert(Key::Str(b"t".to_vec()), Value::Int(1));
    a.insert(Key::Str(b"name".to_vec()), Value::Str(name));
    a.insert(Key::Str(b"attrs".to_vec()), Value::Array(attrs));
    a.insert(Key::Str(b"kids".to_vec()), Value::Array(kids));
    Value::Array(a)
}

fn text_like(t: i64, text: Vec<u8>) -> Value {
    let mut a = Arr::new();
    a.insert(Key::Str(b"t".to_vec()), Value::Int(t));
    a.insert(Key::Str(b"text".to_vec()), Value::Str(text));
    Value::Array(a)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Decode the five predefined entities and numeric character references.
pub fn decode_entities(s: &[u8]) -> Vec<u8> {
    if !s.contains(&b'&') {
        return s.to_vec();
    }
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'&' {
            if let Some(off) = s[i + 1..].iter().position(|&c| c == b';') {
                let ent = &s[i + 1..i + 1 + off];
                let rep: Option<Vec<u8>> = match ent {
                    b"lt" => Some(b"<".to_vec()),
                    b"gt" => Some(b">".to_vec()),
                    b"amp" => Some(b"&".to_vec()),
                    b"quot" => Some(b"\"".to_vec()),
                    b"apos" => Some(b"'".to_vec()),
                    _ if ent.starts_with(b"#x") || ent.starts_with(b"#X") => std::str::from_utf8(&ent[2..])
                        .ok()
                        .and_then(|h| u32::from_str_radix(h, 16).ok())
                        .and_then(char::from_u32)
                        .map(|c| c.to_string().into_bytes()),
                    _ if ent.starts_with(b"#") => std::str::from_utf8(&ent[1..])
                        .ok()
                        .and_then(|d| d.parse::<u32>().ok())
                        .and_then(char::from_u32)
                        .map(|c| c.to_string().into_bytes()),
                    _ => None,
                };
                if let Some(r) = rep {
                    out.extend_from_slice(&r);
                    i += 2 + off;
                    continue;
                }
            }
        }
        out.push(s[i]);
        i += 1;
    }
    out
}
