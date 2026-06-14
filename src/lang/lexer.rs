//! Byte-level lexer. Turns PHP source into a `Vec<Token>`, handling the
//! HTML/code mode switch itself so the parser only ever sees code tokens
//! (plus `InlineHtml`/`OpenTag`/`CloseTag` markers).

use super::token::{Kind, Span, StrPart, Token};

#[derive(Debug)]
pub struct LexError {
    pub msg: String,
    pub pos: usize,
}

type R<T> = Result<T, LexError>;

pub struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
    in_php: bool, // false = raw-HTML mode
}

#[inline]
fn is_ident_start(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphabetic() || b >= 0x80
}
#[inline]
fn is_ident_cont(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphanumeric() || b >= 0x80
}

impl<'a> Lexer<'a> {
    pub fn new(src: &'a [u8]) -> Self {
        Lexer {
            src,
            pos: 0,
            in_php: false,
        }
    }

    pub fn tokenize(src: &'a [u8]) -> R<Vec<Token>> {
        let mut lx = Lexer::new(src);
        let mut out = Vec::new();
        loop {
            let t = lx.next_token()?;
            let eof = t.kind == Kind::Eof;
            out.push(t);
            if eof {
                break;
            }
        }
        Ok(out)
    }

    #[inline]
    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }
    #[inline]
    fn at(&self, off: usize) -> Option<u8> {
        self.src.get(self.pos + off).copied()
    }
    #[inline]
    fn starts(&self, s: &[u8]) -> bool {
        self.src[self.pos..].starts_with(s)
    }
    fn span(&self, start: usize) -> Span {
        Span {
            start,
            end: self.pos,
        }
    }

    fn next_token(&mut self) -> R<Token> {
        if !self.in_php {
            return self.lex_html();
        }
        self.skip_trivia();
        let start = self.pos;
        let b = match self.peek() {
            None => return Ok(Token { kind: Kind::Eof, span: self.span(start) }),
            Some(b) => b,
        };

        // close tag → back to HTML mode
        if self.starts(b"?>") {
            self.pos += 2;
            // PHP swallows a single newline immediately after `?>`
            if self.peek() == Some(b'\n') {
                self.pos += 1;
            } else if self.starts(b"\r\n") {
                self.pos += 2;
            }
            self.in_php = false;
            return Ok(Token { kind: Kind::CloseTag, span: self.span(start) });
        }

        match b {
            b'$' => self.lex_variable(),
            b'\'' => self.lex_single_quoted(),
            b'"' => self.lex_double_quoted(),
            _ if self.starts(b"<<<") => self.lex_heredoc(),
            _ if b.is_ascii_digit() => self.lex_number(),
            b'.' if self.at(1).map(|c| c.is_ascii_digit()).unwrap_or(false) => self.lex_number(),
            _ if is_ident_start(b) => Ok(self.lex_ident()),
            _ => self.lex_operator(),
        }
    }

    // ---- HTML mode ------------------------------------------------------
    fn lex_html(&mut self) -> R<Token> {
        let start = self.pos;
        // accumulate raw bytes until an open tag or EOF
        let mut html = Vec::new();
        while let Some(b) = self.peek() {
            if b == b'<' && (self.starts(b"<?php") || self.starts(b"<?=") || self.starts(b"<?")) {
                break;
            }
            html.push(b);
            self.pos += 1;
        }
        if !html.is_empty() {
            return Ok(Token { kind: Kind::InlineHtml(html), span: self.span(start) });
        }
        // sitting on an open tag (or EOF)
        if self.peek().is_none() {
            return Ok(Token { kind: Kind::Eof, span: self.span(start) });
        }
        if self.starts(b"<?=") {
            self.pos += 3;
            self.in_php = true;
            return Ok(Token { kind: Kind::OpenEcho, span: self.span(start) });
        }
        if self.starts(b"<?php") {
            self.pos += 5;
            // consume one following whitespace char (PHP grammar)
            if matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
                self.pos += 1;
            }
        } else {
            // short open tag `<?`
            self.pos += 2;
        }
        self.in_php = true;
        Ok(Token { kind: Kind::OpenTag, span: self.span(start) })
    }

    // ---- trivia: whitespace + comments ---------------------------------
    fn skip_trivia(&mut self) {
        loop {
            match self.peek() {
                Some(b' ' | b'\t' | b'\r' | b'\n') => self.pos += 1,
                Some(b'/') if self.at(1) == Some(b'/') => self.skip_line_comment(),
                // `#` is a line comment, but `#[` opens an attribute (not trivia)
                Some(b'#') if self.at(1) != Some(b'[') => self.skip_line_comment(),
                Some(b'/') if self.at(1) == Some(b'*') => {
                    self.pos += 2;
                    while self.pos < self.src.len() && !self.starts(b"*/") {
                        self.pos += 1;
                    }
                    self.pos = (self.pos + 2).min(self.src.len());
                }
                _ => break,
            }
        }
    }

    fn skip_line_comment(&mut self) {
        // a line comment ends at end-of-line OR at `?>` (which returns to HTML)
        while let Some(b) = self.peek() {
            if b == b'\n' || self.starts(b"?>") {
                break;
            }
            self.pos += 1;
        }
    }

    // ---- variables ------------------------------------------------------
    fn lex_variable(&mut self) -> R<Token> {
        let start = self.pos;
        self.pos += 1; // `$`
        match self.peek() {
            Some(b) if is_ident_start(b) => {
                let s = self.take_ident_str();
                Ok(Token { kind: Kind::Variable(s), span: self.span(start) })
            }
            // `$$x` / `${…}` — emit a bare Dollar and let the parser recurse
            _ => Ok(Token { kind: Kind::Dollar, span: self.span(start) }),
        }
    }

    fn take_ident_str(&mut self) -> String {
        let s = self.pos;
        while let Some(b) = self.peek() {
            if is_ident_cont(b) {
                self.pos += 1;
            } else {
                break;
            }
        }
        String::from_utf8_lossy(&self.src[s..self.pos]).into_owned()
    }

    fn lex_ident(&mut self) -> Token {
        let start = self.pos;
        let s = self.take_ident_str();
        Token { kind: Kind::Ident(s), span: self.span(start) }
    }

    // ---- numbers --------------------------------------------------------
    fn lex_number(&mut self) -> R<Token> {
        let start = self.pos;
        // hex / octal / binary integer literals
        if self.peek() == Some(b'0') {
            match self.at(1) {
                Some(b'x' | b'X') => return Ok(self.lex_radix(start, 16, 2)),
                Some(b'b' | b'B') => return Ok(self.lex_radix(start, 2, 2)),
                Some(b'o' | b'O') => return Ok(self.lex_radix(start, 8, 2)),
                _ => {}
            }
        }
        let mut is_float = false;
        let mut digits = String::new();
        while let Some(b) = self.peek() {
            match b {
                b'0'..=b'9' => {
                    digits.push(b as char);
                    self.pos += 1;
                }
                b'_' => self.pos += 1, // numeric separator
                b'.' if !is_float && self.at(1) != Some(b'.') => {
                    is_float = true;
                    digits.push('.');
                    self.pos += 1;
                }
                b'e' | b'E' => {
                    is_float = true;
                    digits.push('e');
                    self.pos += 1;
                    if matches!(self.peek(), Some(b'+' | b'-')) {
                        digits.push(self.peek().unwrap() as char);
                        self.pos += 1;
                    }
                }
                _ => break,
            }
        }
        if is_float {
            let f: f64 = digits.parse().map_err(|_| self.err("bad float literal"))?;
            Ok(Token { kind: Kind::Float(f), span: self.span(start) })
        } else {
            // a leading-zero all-octal-digit literal is octal in PHP
            let kind = if digits.len() > 1
                && digits.starts_with('0')
                && digits.bytes().all(|d| (b'0'..=b'7').contains(&d))
            {
                i64::from_str_radix(&digits[1..], 8).map(Kind::Int)
            } else {
                digits.parse::<i64>().map(Kind::Int)
            };
            match kind {
                Ok(k) => Ok(Token { kind: k, span: self.span(start) }),
                // integer overflow → PHP promotes to float
                Err(_) => Ok(Token {
                    kind: Kind::Float(digits.parse().unwrap_or(0.0)),
                    span: self.span(start),
                }),
            }
        }
    }

    fn lex_radix(&mut self, start: usize, radix: u32, prefix: usize) -> Token {
        self.pos += prefix;
        let ds = self.pos;
        while let Some(b) = self.peek() {
            if b == b'_' {
                self.pos += 1;
            } else if (b as char).is_digit(radix) {
                self.pos += 1;
            } else {
                break;
            }
        }
        let raw: String = String::from_utf8_lossy(&self.src[ds..self.pos])
            .chars()
            .filter(|c| *c != '_')
            .collect();
        let n = i64::from_str_radix(&raw, radix).unwrap_or(0);
        Token { kind: Kind::Int(n), span: self.span(start) }
    }

    // ---- single-quoted: only \\ and \' are escapes ---------------------
    fn lex_single_quoted(&mut self) -> R<Token> {
        let start = self.pos;
        self.pos += 1; // opening '
        let mut out = Vec::new();
        loop {
            match self.peek() {
                None => return Err(self.err("unterminated single-quoted string")),
                Some(b'\'') => {
                    self.pos += 1;
                    break;
                }
                Some(b'\\') => match self.at(1) {
                    Some(b'\'') => {
                        out.push(b'\'');
                        self.pos += 2;
                    }
                    Some(b'\\') => {
                        out.push(b'\\');
                        self.pos += 2;
                    }
                    _ => {
                        out.push(b'\\');
                        self.pos += 1;
                    }
                },
                Some(b) => {
                    out.push(b);
                    self.pos += 1;
                }
            }
        }
        Ok(Token { kind: Kind::Str(out), span: self.span(start) })
    }

    // ---- double-quoted: escapes + interpolation ------------------------
    fn lex_double_quoted(&mut self) -> R<Token> {
        let start = self.pos;
        self.pos += 1; // opening "
        let parts = self.scan_interpolated(Terminator::DoubleQuote)?;
        Ok(Token { kind: Kind::Template(parts), span: self.span(start) })
    }

    // ---- heredoc / nowdoc ----------------------------------------------
    fn lex_heredoc(&mut self) -> R<Token> {
        let start = self.pos;
        self.pos += 3; // <<<
        while matches!(self.peek(), Some(b' ' | b'\t')) {
            self.pos += 1;
        }
        let nowdoc = self.peek() == Some(b'\'');
        let quoted = nowdoc || self.peek() == Some(b'"');
        if quoted {
            self.pos += 1;
        }
        let label = self.take_ident_str();
        if label.is_empty() {
            return Err(self.err("expected heredoc/nowdoc label"));
        }
        if quoted {
            self.pos += 1; // closing quote of the label
        }
        // skip to end of the opening line
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\r')) {
            self.pos += 1;
        }
        if self.peek() == Some(b'\n') {
            self.pos += 1;
        }
        let body_start = self.pos;
        // find the closing label: at line start, optional indentation, label,
        // then a non-identifier char (PHP 7.3 flexible heredoc).
        let (body_end, after, _indent) = self.find_heredoc_close(label.as_bytes())?;
        // an empty heredoc (label immediately on the next line) yields
        // body_end = body_start - 1; clamp so the slice can't underflow.
        let body = &self.src[body_start..body_end.max(body_start)];
        self.pos = after;
        if nowdoc {
            Ok(Token { kind: Kind::Str(body.to_vec()), span: self.span(start) })
        } else {
            // interpolate the heredoc body (reuse the double-quote scanner over a slice)
            let mut sub = Lexer::new(body);
            sub.in_php = true;
            let parts = sub.scan_interpolated(Terminator::Eof)?;
            Ok(Token { kind: Kind::Template(parts), span: self.span(start) })
        }
    }

    /// Returns (body_end, pos_after_label, indent_len).
    fn find_heredoc_close(&self, label: &[u8]) -> R<(usize, usize, usize)> {
        let mut i = self.pos;
        let src = self.src;
        loop {
            // i is at the start of a line
            let line_start = i;
            let mut j = i;
            while j < src.len() && (src[j] == b' ' || src[j] == b'\t') {
                j += 1;
            }
            if src[j..].starts_with(label) {
                let after_label = j + label.len();
                let next = src.get(after_label).copied();
                if !next.map(is_ident_cont).unwrap_or(false) {
                    // closing line: body ends at the newline before this line
                    let body_end = if line_start > 0 && src[line_start - 1] == b'\n' {
                        if line_start >= 2 && src[line_start - 2] == b'\r' {
                            line_start - 2
                        } else {
                            line_start - 1
                        }
                    } else {
                        line_start
                    };
                    return Ok((body_end, after_label, j - line_start));
                }
            }
            // advance to next line
            while i < src.len() && src[i] != b'\n' {
                i += 1;
            }
            if i >= src.len() {
                return Err(self.err("unterminated heredoc/nowdoc"));
            }
            i += 1;
        }
    }

    // ---- shared interpolation scanner ----------------------------------
    fn scan_interpolated(&mut self, term: Terminator) -> R<Vec<StrPart>> {
        let mut parts: Vec<StrPart> = Vec::new();
        let mut lit: Vec<u8> = Vec::new();
        macro_rules! flush {
            () => {
                if !lit.is_empty() {
                    parts.push(StrPart::Lit(std::mem::take(&mut lit)));
                }
            };
        }
        loop {
            let b = match self.peek() {
                None => match term {
                    Terminator::Eof => break,
                    Terminator::DoubleQuote => return Err(self.err("unterminated string")),
                },
                Some(b) => b,
            };
            if term == Terminator::DoubleQuote && b == b'"' {
                self.pos += 1;
                break;
            }
            match b {
                b'\\' if term == Terminator::DoubleQuote || term == Terminator::Eof => {
                    self.pos += 1;
                    self.read_escape(&mut lit);
                }
                b'$' if self.at(1).map(is_ident_start).unwrap_or(false) => {
                    flush!();
                    let e = self.scan_simple_interp();
                    parts.push(StrPart::Expr(e));
                }
                b'$' if self.at(1) == Some(b'{') => {
                    // ${ name } / ${ expr }
                    flush!();
                    self.pos += 2; // $ {
                    let inner = self.take_balanced(b'{', b'}')?;
                    // ${name} means variable `name`; prefix `$` so the parser reads it as such
                    let mut e = vec![b'$'];
                    e.extend_from_slice(&inner);
                    parts.push(StrPart::Expr(e));
                }
                b'{' if self.at(1) == Some(b'$') => {
                    // {$expr} complex interpolation
                    flush!();
                    self.pos += 1; // {
                    let inner = self.take_balanced(b'{', b'}')?;
                    parts.push(StrPart::Expr(inner));
                }
                _ => {
                    lit.push(b);
                    self.pos += 1;
                }
            }
        }
        flush!();
        if parts.is_empty() {
            parts.push(StrPart::Lit(Vec::new()));
        }
        Ok(parts)
    }

    /// Simple interpolation: `$name`, `$name->prop`, `$name[index]` (one level).
    fn scan_simple_interp(&mut self) -> Vec<u8> {
        let start = self.pos;
        self.pos += 1; // $
        while self.peek().map(is_ident_cont).unwrap_or(false) {
            self.pos += 1;
        }
        // one trailing accessor
        if self.starts(b"->") && self.at(2).map(is_ident_start).unwrap_or(false) {
            self.pos += 2;
            while self.peek().map(is_ident_cont).unwrap_or(false) {
                self.pos += 1;
            }
        } else if self.peek() == Some(b'[') {
            // up to the matching ]
            self.pos += 1;
            while let Some(c) = self.peek() {
                self.pos += 1;
                if c == b']' {
                    break;
                }
            }
        }
        self.src[start..self.pos].to_vec()
    }

    fn take_balanced(&mut self, open: u8, close: u8) -> R<Vec<u8>> {
        let start = self.pos;
        let mut depth = 1;
        while let Some(b) = self.peek() {
            if b == open {
                depth += 1;
            } else if b == close {
                depth -= 1;
                if depth == 0 {
                    let inner = self.src[start..self.pos].to_vec();
                    self.pos += 1; // consume close
                    return Ok(inner);
                }
            }
            self.pos += 1;
        }
        Err(self.err("unbalanced interpolation braces"))
    }

    fn read_escape(&mut self, out: &mut Vec<u8>) {
        let b = match self.peek() {
            None => return,
            Some(b) => b,
        };
        self.pos += 1;
        match b {
            b'n' => out.push(b'\n'),
            b't' => out.push(b'\t'),
            b'r' => out.push(b'\r'),
            b'v' => out.push(0x0b),
            b'f' => out.push(0x0c),
            b'e' => out.push(0x1b),
            b'\\' => out.push(b'\\'),
            b'$' => out.push(b'$'),
            b'"' => out.push(b'"'),
            b'x' => {
                let mut hex = String::new();
                while hex.len() < 2 && self.peek().map(|c| c.is_ascii_hexdigit()).unwrap_or(false) {
                    hex.push(self.peek().unwrap() as char);
                    self.pos += 1;
                }
                if hex.is_empty() {
                    out.push(b'\\');
                    out.push(b'x');
                } else {
                    out.push(u8::from_str_radix(&hex, 16).unwrap_or(0));
                }
            }
            b'u' if self.peek() == Some(b'{') => {
                self.pos += 1;
                let mut hex = String::new();
                while self.peek().map(|c| c != b'}').unwrap_or(false) {
                    hex.push(self.peek().unwrap() as char);
                    self.pos += 1;
                }
                self.pos += 1; // }
                if let Ok(cp) = u32::from_str_radix(&hex, 16) {
                    if let Some(c) = char::from_u32(cp) {
                        let mut buf = [0u8; 4];
                        out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                    }
                }
            }
            b'0'..=b'7' => {
                let mut oct = String::new();
                oct.push(b as char);
                while oct.len() < 3 && self.peek().map(|c| (b'0'..=b'7').contains(&c)).unwrap_or(false)
                {
                    oct.push(self.peek().unwrap() as char);
                    self.pos += 1;
                }
                out.push(u8::from_str_radix(&oct, 8).unwrap_or(0) as u8);
            }
            // unknown escape: PHP keeps the backslash and the char
            other => {
                out.push(b'\\');
                out.push(other);
            }
        }
    }

    // ---- operators & punctuation (longest match) -----------------------
    fn lex_operator(&mut self) -> R<Token> {
        let start = self.pos;
        // try 3-byte, then 2-byte, then 1-byte operators
        let three = self.src.get(self.pos..self.pos + 3);
        if let Some(s) = three {
            let k = match s {
                b"===" => Some(Kind::Identical),
                b"!==" => Some(Kind::NotIdentical),
                b"<=>" => Some(Kind::Spaceship),
                b"**=" => Some(Kind::PowEq),
                b"..." => Some(Kind::Ellipsis),
                b"<<=" => Some(Kind::ShlEq),
                b">>=" => Some(Kind::ShrEq),
                b"??=" => Some(Kind::CoalesceEq),
                b"?->" => Some(Kind::NullArrow),
                _ => None,
            };
            if let Some(kind) = k {
                self.pos += 3;
                return Ok(Token { kind, span: self.span(start) });
            }
        }
        let two = self.src.get(self.pos..self.pos + 2);
        if let Some(s) = two {
            let k = match s {
                b"==" => Some(Kind::EqEq),
                b"!=" => Some(Kind::NotEq),
                b"<>" => Some(Kind::NotEq),
                b"<=" => Some(Kind::Le),
                b">=" => Some(Kind::Ge),
                b"&&" => Some(Kind::AndAnd),
                b"||" => Some(Kind::OrOr),
                b"++" => Some(Kind::Inc),
                b"--" => Some(Kind::Dec),
                b"+=" => Some(Kind::PlusEq),
                b"-=" => Some(Kind::MinusEq),
                b"*=" => Some(Kind::StarEq),
                b"/=" => Some(Kind::SlashEq),
                b"%=" => Some(Kind::PercentEq),
                b".=" => Some(Kind::DotEq),
                b"&=" => Some(Kind::AndEq),
                b"|=" => Some(Kind::OrEq),
                b"|>" => Some(Kind::PipeArrow),
                b"^=" => Some(Kind::XorEq),
                b"**" => Some(Kind::Pow),
                b"->" => Some(Kind::Arrow),
                b"=>" => Some(Kind::FatArrow),
                b"::" => Some(Kind::DoubleColon),
                b"<<" => Some(Kind::Shl),
                b">>" => Some(Kind::Shr),
                b"??" => Some(Kind::Coalesce),
                b"#[" => Some(Kind::AttrStart),
                _ => None,
            };
            if let Some(kind) = k {
                self.pos += 2;
                return Ok(Token { kind, span: self.span(start) });
            }
        }
        let b = self.peek().unwrap();
        let kind = match b {
            b'(' => Kind::LParen,
            b')' => Kind::RParen,
            b'{' => Kind::LBrace,
            b'}' => Kind::RBrace,
            b'[' => Kind::LBracket,
            b']' => Kind::RBracket,
            b';' => Kind::Semi,
            b',' => Kind::Comma,
            b':' => Kind::Colon,
            b'?' => Kind::Question,
            b'@' => Kind::At,
            b'\\' => Kind::Backslash,
            b'+' => Kind::Plus,
            b'-' => Kind::Minus,
            b'*' => Kind::Star,
            b'/' => Kind::Slash,
            b'%' => Kind::Percent,
            b'.' => Kind::Dot,
            b'=' => Kind::Assign,
            b'<' => Kind::Lt,
            b'>' => Kind::Gt,
            b'!' => Kind::Not,
            b'&' => Kind::Amp,
            b'|' => Kind::Pipe,
            b'^' => Kind::Caret,
            b'~' => Kind::Tilde,
            _ => return Err(self.err(&format!("unexpected byte {:?}", b as char))),
        };
        self.pos += 1;
        Ok(Token { kind, span: self.span(start) })
    }

    fn err(&self, msg: &str) -> LexError {
        LexError { msg: msg.to_string(), pos: self.pos }
    }
}

#[derive(PartialEq, Clone, Copy)]
enum Terminator {
    DoubleQuote,
    Eof,
}
