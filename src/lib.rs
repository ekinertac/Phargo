//! FerroPHP — a from-scratch, memory-safe PHP engine written in Rust.
//!
//! **v1.** Building on v0 (inline HTML, `echo`, string/int literals, `.`
//! concatenation), this adds the foundation everything else needs:
//!   * a PHP value model — [`Value`] (the "zval": null/bool/int/float/string)
//!   * variables: assignment (`$x = …;`) and lookup (`$x`)
//!   * simple `"…$var…"` interpolation inside double-quoted strings
//!
//! Anything else returns an [`EngineError`], which makes the corresponding
//! `.phpt` test fail *honestly*. See PROGRESS.md for the climb.

use std::collections::HashMap;
use std::fmt;

#[derive(Debug)]
pub struct EngineError(pub String);

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for EngineError {}

type R<T> = Result<T, EngineError>;

/// A PHP value — the engine's "zval". Only the scalar types so far.
#[derive(Clone, Debug)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
}

impl Value {
    /// Convert to the exact string PHP would print for this value.
    pub fn to_php_string(&self) -> String {
        match self {
            Value::Null => String::new(),
            Value::Bool(true) => "1".to_string(),
            Value::Bool(false) => String::new(),
            Value::Int(n) => n.to_string(),
            Value::Float(x) => format_php_float(*x),
            Value::Str(s) => s.clone(),
        }
    }
}

/// PHP prints whole-valued floats without a decimal point (`1.0` -> "1"),
/// and otherwise with up to 14 significant digits, trailing zeros trimmed.
fn format_php_float(x: f64) -> String {
    if x.is_finite() && x == x.trunc() && x.abs() < 1e15 {
        return format!("{}", x as i64);
    }
    if x.is_infinite() {
        return if x > 0.0 { "INF".into() } else { "-INF".into() };
    }
    if x.is_nan() {
        return "NAN".into();
    }
    let mut s = format!("{x:.14}");
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    s
}

/// Execute PHP `source` and return everything it would have printed to stdout.
pub fn run(source: &str) -> R<String> {
    let mut engine = Engine {
        src: source.chars().collect(),
        pos: 0,
        out: String::new(),
        vars: HashMap::new(),
    };
    engine.program()?;
    Ok(engine.out)
}

struct Engine {
    src: Vec<char>,
    pos: usize,
    out: String,
    vars: HashMap<String, Value>,
}

impl Engine {
    fn peek(&self) -> Option<char> {
        self.src.get(self.pos).copied()
    }

    fn peek_at(&self, off: usize) -> Option<char> {
        self.src.get(self.pos + off).copied()
    }

    fn starts_with(&self, s: &str) -> bool {
        s.chars()
            .enumerate()
            .all(|(i, c)| self.src.get(self.pos + i).copied() == Some(c))
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(c) if c.is_whitespace()) {
            self.pos += 1;
        }
    }

    /// Top level: copy text verbatim until a PHP open tag, then run PHP.
    fn program(&mut self) -> R<()> {
        while self.pos < self.src.len() {
            if self.starts_with("<?php") {
                self.pos += 5;
                self.php_body()?;
            } else {
                self.out.push(self.src[self.pos]);
                self.pos += 1;
            }
        }
        Ok(())
    }

    /// Inside a `<?php … ?>` block: run statements until `?>` or EOF.
    fn php_body(&mut self) -> R<()> {
        loop {
            self.skip_ws();
            if self.pos >= self.src.len() {
                return Ok(()); // an unclosed tag is allowed in PHP
            }
            if self.starts_with("?>") {
                self.pos += 2;
                if self.peek() == Some('\n') {
                    self.pos += 1; // PHP swallows one newline after `?>`
                }
                return Ok(());
            }
            self.statement()?;
        }
    }

    fn statement(&mut self) -> R<()> {
        self.skip_ws();
        if self.peek() == Some(';') {
            self.pos += 1; // empty statement
            return Ok(());
        }

        // Assignment or bare expression statement starting with a variable.
        if self.peek() == Some('$') {
            let save = self.pos;
            let name = self.parse_variable_name()?;
            self.skip_ws();
            if self.peek() == Some('=') && self.peek_at(1) != Some('=') {
                self.pos += 1; // consume '='
                self.skip_ws();
                let value = self.expression()?;
                self.vars.insert(name, value);
                return self.end_statement();
            }
            // not an assignment: re-parse from the start as an expression
            self.pos = save;
            let _ = self.expression()?;
            return self.end_statement();
        }

        if let Some(word) = self.try_identifier() {
            if word.eq_ignore_ascii_case("echo") {
                return self.echo_statement();
            }
            return Err(EngineError(format!(
                "v1 does not support the `{word}` statement yet"
            )));
        }

        Err(EngineError(format!(
            "v1 cannot parse statement near {:?}",
            self.snippet()
        )))
    }

    /// Consume the `;` (or close tag) that terminates a statement.
    fn end_statement(&mut self) -> R<()> {
        self.skip_ws();
        match self.peek() {
            Some(';') => {
                self.pos += 1;
                Ok(())
            }
            _ if self.starts_with("?>") => Ok(()),
            other => Err(EngineError(format!("expected `;`, found {other:?}"))),
        }
    }

    fn echo_statement(&mut self) -> R<()> {
        loop {
            let value = self.expression()?;
            self.out.push_str(&value.to_php_string());
            self.skip_ws();
            if self.peek() == Some(',') {
                self.pos += 1;
                self.skip_ws();
                continue;
            }
            break;
        }
        self.end_statement()
    }

    /// Expression = primary ( '.' primary )*   — concatenation only, for now.
    fn expression(&mut self) -> R<Value> {
        let mut value = self.primary()?;
        loop {
            self.skip_ws();
            if self.peek() == Some('.') && self.peek_at(1) != Some('=') {
                self.pos += 1;
                self.skip_ws();
                let rhs = self.primary()?;
                let joined = format!("{}{}", value.to_php_string(), rhs.to_php_string());
                value = Value::Str(joined);
            } else {
                break;
            }
        }
        Ok(value)
    }

    fn primary(&mut self) -> R<Value> {
        self.skip_ws();
        match self.peek() {
            Some('"') => Ok(Value::Str(self.double_quoted()?)),
            Some('\'') => Ok(Value::Str(self.single_quoted()?)),
            Some('$') => self.variable(),
            Some(c) if c.is_ascii_digit() => Ok(self.number()),
            other => Err(EngineError(format!(
                "v1 cannot parse expression near {other:?} ({:?})",
                self.snippet()
            ))),
        }
    }

    fn variable(&mut self) -> R<Value> {
        let name = self.parse_variable_name()?;
        // Undefined variables read as NULL (PHP emits a warning; v1 stays quiet).
        Ok(self.vars.get(&name).cloned().unwrap_or(Value::Null))
    }

    /// Parse `$name`, returning the name without the leading `$`.
    fn parse_variable_name(&mut self) -> R<String> {
        debug_assert_eq!(self.peek(), Some('$'));
        self.pos += 1; // consume '$'
        if !matches!(self.peek(), Some(c) if c.is_ascii_alphabetic() || c == '_') {
            return Err(EngineError("expected a variable name after `$`".into()));
        }
        let start = self.pos;
        while matches!(self.peek(), Some(c) if c.is_ascii_alphanumeric() || c == '_') {
            self.pos += 1;
        }
        Ok(self.src[start..self.pos].iter().collect())
    }

    fn number(&mut self) -> Value {
        let start = self.pos;
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            self.pos += 1;
        }
        let text: String = self.src[start..self.pos].iter().collect();
        match text.parse::<i64>() {
            Ok(n) => Value::Int(n),
            Err(_) => Value::Float(text.parse::<f64>().unwrap_or(0.0)), // overflow -> float, like PHP
        }
    }

    fn single_quoted(&mut self) -> R<String> {
        self.pos += 1; // opening '
        let mut s = String::new();
        while let Some(c) = self.peek() {
            match c {
                // In single quotes, only \' and \\ are real escapes.
                '\\' => match self.peek_at(1) {
                    Some('\'') => {
                        s.push('\'');
                        self.pos += 2;
                    }
                    Some('\\') => {
                        s.push('\\');
                        self.pos += 2;
                    }
                    _ => {
                        s.push('\\');
                        self.pos += 1;
                    }
                },
                '\'' => {
                    self.pos += 1;
                    return Ok(s);
                }
                _ => {
                    s.push(c);
                    self.pos += 1;
                }
            }
        }
        Err(EngineError("unterminated single-quoted string".into()))
    }

    fn double_quoted(&mut self) -> R<String> {
        self.pos += 1; // opening "
        let mut s = String::new();
        while let Some(c) = self.peek() {
            if c == '\\' {
                let (ch, adv) = match self.peek_at(1) {
                    Some('n') => ('\n', 2),
                    Some('t') => ('\t', 2),
                    Some('r') => ('\r', 2),
                    Some('"') => ('"', 2),
                    Some('\\') => ('\\', 2),
                    Some('$') => ('$', 2),
                    _ => ('\\', 1), // unknown escape: PHP keeps the backslash
                };
                s.push(ch);
                self.pos += adv;
                continue;
            }
            if c == '"' {
                self.pos += 1;
                return Ok(s);
            }
            // Simple "$var" interpolation (no "{$expr}" or "$arr[k]" yet).
            if c == '$' && matches!(self.peek_at(1), Some(d) if d.is_ascii_alphabetic() || d == '_') {
                let name = self.parse_variable_name()?;
                let value = self.vars.get(&name).cloned().unwrap_or(Value::Null);
                s.push_str(&value.to_php_string());
                continue;
            }
            s.push(c);
            self.pos += 1;
        }
        Err(EngineError("unterminated double-quoted string".into()))
    }

    /// Read an identifier/keyword if one starts here. Consumes it on a hit.
    fn try_identifier(&mut self) -> Option<String> {
        let first = self.peek()?;
        if !(first.is_ascii_alphabetic() || first == '_') {
            return None;
        }
        let start = self.pos;
        while matches!(self.peek(), Some(c) if c.is_ascii_alphanumeric() || c == '_') {
            self.pos += 1;
        }
        Some(self.src[start..self.pos].iter().collect())
    }

    fn snippet(&self) -> String {
        let end = (self.pos + 16).min(self.src.len());
        self.src[self.pos..end].iter().collect()
    }
}
