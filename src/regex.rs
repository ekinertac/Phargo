#![allow(unused_imports)]
#![allow(clippy::all)]
use crate::*;
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

// ---- Regex engine (from-scratch, backtracking VM) --------------------------
//
// A small PCRE-ish engine compiled to a recursive backtracking bytecode VM.
// Supports: literals, `.`, char classes `[...]` (+ `\d \w \s` and negations),
// anchors `^ $`, word boundaries `\b \B`, groups `()` (capturing, `(?:)`
// non-capturing, named `(?P<n>)`/`(?<n>)`), alternation `|`, quantifiers
// `* + ? {n,m}` (greedy + lazy `?`), backreferences `\1`, and lookahead
// `(?=)`/`(?!)`. A global step budget guards against catastrophic backtracking.

#[derive(Clone)]
pub(crate) enum ClassItem {
    Ch(char),
    Range(char, char),
    Pre(char), // 'd' 'D' 'w' 'W' 's' 'S'
}

#[derive(Clone)]
pub(crate) enum Re {
    Empty,
    Char(char),
    Any,
    Class(bool, Vec<ClassItem>), // negated, items
    Start,
    End,
    WordB(bool), // true = \b, false = \B
    Backref(usize),
    Group(Option<usize>, Box<Re>),
    Look(bool, Box<Re>), // true = negative lookahead
    Alt(Box<Re>, Box<Re>),
    Concat(Vec<Re>),
    Star(Box<Re>, bool), // greedy
    Plus(Box<Re>, bool),
    Quest(Box<Re>, bool),
    Repeat(Box<Re>, usize, Option<usize>, bool),
}

#[derive(Clone)]
pub(crate) enum Inst {
    Char(char),
    Any,
    Class(bool, Vec<ClassItem>),
    Save(usize),
    Split(usize, usize),
    Jmp(usize),
    Start,
    End,
    WordB(bool),
    Backref(usize),
    Look(bool, Vec<Inst>),
    Match,
}

#[derive(Clone, Copy)]
pub(crate) struct RxFlags {
    ci: bool,
    dotall: bool,
    multiline: bool,
}

pub struct Rx {
    prog: Vec<Inst>,
    pub(crate) ngroups: usize,
    flags: RxFlags,
    pub(crate) names: Vec<(String, usize)>,
    anchored: bool, // pattern begins with ^ (non-multiline) — only try at start
}

pub(crate) struct ReParser {
    c: Vec<char>,
    i: usize,
    ngroups: usize,
    names: Vec<(String, usize)>,
    extended: bool,
}

impl ReParser {
    fn peek(&self) -> Option<char> {
        self.c.get(self.i).copied()
    }
    fn at(&self, k: usize) -> Option<char> {
        self.c.get(self.i + k).copied()
    }
    fn skip_x_ws(&mut self) {
        if !self.extended {
            return;
        }
        loop {
            match self.peek() {
                Some(ch) if ch.is_whitespace() => self.i += 1,
                Some('#') => {
                    while !matches!(self.peek(), None | Some('\n')) {
                        self.i += 1;
                    }
                }
                _ => break,
            }
        }
    }

    fn alt(&mut self) -> Re {
        let mut left = self.concat();
        while self.peek() == Some('|') {
            self.i += 1;
            let right = self.concat();
            left = Re::Alt(Box::new(left), Box::new(right));
        }
        left
    }

    fn concat(&mut self) -> Re {
        let mut items = Vec::new();
        loop {
            self.skip_x_ws();
            match self.peek() {
                None | Some('|') | Some(')') => break,
                _ => {}
            }
            if let Some(node) = self.quant() {
                items.push(node);
            } else {
                break;
            }
        }
        if items.is_empty() {
            Re::Empty
        } else if items.len() == 1 {
            items.pop().unwrap()
        } else {
            Re::Concat(items)
        }
    }

    fn quant(&mut self) -> Option<Re> {
        let atom = self.atom()?;
        self.skip_x_ws();
        let node = match self.peek() {
            Some('*') => {
                self.i += 1;
                Re::Star(Box::new(atom), self.greedy())
            }
            Some('+') => {
                self.i += 1;
                Re::Plus(Box::new(atom), self.greedy())
            }
            Some('?') => {
                self.i += 1;
                Re::Quest(Box::new(atom), self.greedy())
            }
            Some('{') => {
                if let Some((min, max)) = self.try_brace() {
                    Re::Repeat(Box::new(atom), min, max, self.greedy())
                } else {
                    atom
                }
            }
            _ => atom,
        };
        Some(node)
    }

    fn greedy(&mut self) -> bool {
        match self.peek() {
            Some('?') => {
                self.i += 1;
                false
            }
            Some('+') => {
                self.i += 1;
                true // possessive — approximate as greedy
            }
            _ => true,
        }
    }

    fn try_brace(&mut self) -> Option<(usize, Option<usize>)> {
        let save = self.i;
        self.i += 1; // consume {
        let min = self.read_int();
        let (min, max) = match self.peek() {
            Some('}') if min.is_some() => {
                let m = min.unwrap();
                (m, Some(m))
            }
            Some(',') => {
                self.i += 1;
                let max = self.read_int();
                (min.unwrap_or(0), max)
            }
            _ => {
                self.i = save;
                return None;
            }
        };
        if self.peek() != Some('}') {
            self.i = save;
            return None;
        }
        self.i += 1; // consume }
        let max = max.map(|m| m.min(min.max(1000)).max(min)); // cap explosion
        Some((min.min(1000), max))
    }

    fn read_int(&mut self) -> Option<usize> {
        let start = self.i;
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            self.i += 1;
        }
        if self.i == start {
            None
        } else {
            self.c[start..self.i]
                .iter()
                .collect::<String>()
                .parse()
                .ok()
        }
    }

    fn atom(&mut self) -> Option<Re> {
        self.skip_x_ws();
        let ch = self.peek()?;
        match ch {
            '(' => self.group(),
            '[' => self.class(),
            '.' => {
                self.i += 1;
                Some(Re::Any)
            }
            '^' => {
                self.i += 1;
                Some(Re::Start)
            }
            '$' => {
                self.i += 1;
                Some(Re::End)
            }
            '\\' => self.escape(),
            ')' | '|' => None,
            _ => {
                self.i += 1;
                Some(Re::Char(ch))
            }
        }
    }

    fn group(&mut self) -> Option<Re> {
        self.i += 1; // consume (
        let mut capturing = true;
        let mut name: Option<String> = None;
        let mut look: Option<bool> = None; // Some(neg) for lookahead
        let mut lookbehind = false;
        if self.peek() == Some('?') {
            self.i += 1;
            match self.peek() {
                Some(':') => {
                    self.i += 1;
                    capturing = false;
                }
                Some('=') => {
                    self.i += 1;
                    capturing = false;
                    look = Some(false);
                }
                Some('!') => {
                    self.i += 1;
                    capturing = false;
                    look = Some(true);
                }
                Some('P') => {
                    self.i += 1;
                    if self.peek() == Some('<') {
                        self.i += 1;
                        name = Some(self.read_name('>'));
                    }
                }
                Some('<') => {
                    // (?<name>) named, OR (?<= / (?<! lookbehind
                    if matches!(self.at(1), Some('=') | Some('!')) {
                        self.i += 2; // consume < and =/!
                        capturing = false;
                        lookbehind = true; // approximated as always-pass zero-width
                    } else {
                        self.i += 1;
                        name = Some(self.read_name('>'));
                    }
                }
                Some('\'') => {
                    self.i += 1;
                    name = Some(self.read_name('\''));
                }
                _ => {
                    // unknown construct — treat as non-capturing
                    capturing = false;
                }
            }
        }
        let idx = if capturing {
            self.ngroups += 1;
            if let Some(n) = &name {
                self.names.push((n.clone(), self.ngroups));
            }
            Some(self.ngroups)
        } else {
            None
        };
        let inner = self.alt();
        if self.peek() == Some(')') {
            self.i += 1;
        }
        if lookbehind {
            // approximate: zero-width assertion that always passes
            return Some(Re::Empty);
        }
        if let Some(neg) = look {
            return Some(Re::Look(neg, Box::new(inner)));
        }
        Some(Re::Group(idx, Box::new(inner)))
    }

    fn read_name(&mut self, close: char) -> String {
        let start = self.i;
        while let Some(ch) = self.peek() {
            if ch == close {
                break;
            }
            self.i += 1;
        }
        let nm: String = self.c[start..self.i].iter().collect();
        if self.peek() == Some(close) {
            self.i += 1;
        }
        nm
    }

    fn class(&mut self) -> Option<Re> {
        self.i += 1; // consume [
        let mut neg = false;
        if self.peek() == Some('^') {
            neg = true;
            self.i += 1;
        }
        let mut items: Vec<ClassItem> = Vec::new();
        // leading ] is a literal
        if self.peek() == Some(']') {
            items.push(ClassItem::Ch(']'));
            self.i += 1;
        }
        while let Some(ch) = self.peek() {
            if ch == ']' {
                self.i += 1;
                break;
            }
            if ch == '\\' {
                self.i += 1;
                if let Some(e) = self.peek() {
                    self.i += 1;
                    match e {
                        'd' | 'D' | 'w' | 'W' | 's' | 'S' => {
                            items.push(ClassItem::Pre(e));
                            continue;
                        }
                        _ => {
                            let lo = class_escape_char(e);
                            // possible range with escaped lo
                            if self.peek() == Some('-')
                                && self.at(1).is_some()
                                && self.at(1) != Some(']')
                            {
                                self.i += 1;
                                let hi = self.read_class_char();
                                items.push(ClassItem::Range(lo, hi));
                            } else {
                                items.push(ClassItem::Ch(lo));
                            }
                            continue;
                        }
                    }
                }
                continue;
            }
            self.i += 1;
            if self.peek() == Some('-') && self.at(1).is_some() && self.at(1) != Some(']') {
                self.i += 1; // consume -
                let hi = self.read_class_char();
                items.push(ClassItem::Range(ch, hi));
            } else {
                items.push(ClassItem::Ch(ch));
            }
        }
        Some(Re::Class(neg, items))
    }

    fn read_class_char(&mut self) -> char {
        match self.peek() {
            Some('\\') => {
                self.i += 1;
                let e = self.peek().unwrap_or('\\');
                self.i += 1;
                class_escape_char(e)
            }
            Some(ch) => {
                self.i += 1;
                ch
            }
            None => '\0',
        }
    }

    fn escape(&mut self) -> Option<Re> {
        self.i += 1; // consume backslash
        let e = self.peek()?;
        self.i += 1;
        let node = match e {
            'd' => Re::Class(false, vec![ClassItem::Pre('d')]),
            'D' => Re::Class(false, vec![ClassItem::Pre('D')]),
            'w' => Re::Class(false, vec![ClassItem::Pre('w')]),
            'W' => Re::Class(false, vec![ClassItem::Pre('W')]),
            's' => Re::Class(false, vec![ClassItem::Pre('s')]),
            'S' => Re::Class(false, vec![ClassItem::Pre('S')]),
            'b' => Re::WordB(true),
            'B' => Re::WordB(false),
            'A' => Re::Start,
            'z' | 'Z' => Re::End,
            'n' => Re::Char('\n'),
            't' => Re::Char('\t'),
            'r' => Re::Char('\r'),
            'f' => Re::Char('\x0c'),
            'v' => Re::Char('\x0b'),
            '0' => Re::Char('\0'),
            '1'..='9' => {
                // backreference (read full number)
                let mut n = e as usize - '0' as usize;
                while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                    n = n * 10 + (self.peek().unwrap() as usize - '0' as usize);
                    self.i += 1;
                }
                Re::Backref(n)
            }
            'x' => {
                // \xHH
                let mut hex = String::new();
                while hex.len() < 2 && matches!(self.peek(), Some(c) if c.is_ascii_hexdigit()) {
                    hex.push(self.peek().unwrap());
                    self.i += 1;
                }
                let code = u32::from_str_radix(&hex, 16).unwrap_or(0);
                Re::Char(char::from_u32(code).unwrap_or('\0'))
            }
            other => Re::Char(other),
        };
        Some(node)
    }
}

pub(crate) fn class_escape_char(e: char) -> char {
    match e {
        'n' => '\n',
        't' => '\t',
        'r' => '\r',
        'f' => '\x0c',
        'v' => '\x0b',
        '0' => '\0',
        other => other,
    }
}

pub(crate) fn rx_is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

pub(crate) fn rx_ceq(a: char, b: char, ci: bool) -> bool {
    a == b || (ci && a.to_ascii_lowercase() == b.to_ascii_lowercase())
}

pub(crate) fn class_item_match(it: &ClassItem, c: char, ci: bool) -> bool {
    match it {
        ClassItem::Ch(x) => rx_ceq(c, *x, ci),
        ClassItem::Range(lo, hi) => {
            (*lo <= c && c <= *hi)
                || (ci && {
                    let cl = c.to_ascii_lowercase();
                    let cu = c.to_ascii_uppercase();
                    (*lo <= cl && cl <= *hi) || (*lo <= cu && cu <= *hi)
                })
        }
        ClassItem::Pre(p) => match p {
            'd' => c.is_ascii_digit(),
            'D' => !c.is_ascii_digit(),
            'w' => rx_is_word(c),
            'W' => !rx_is_word(c),
            's' => c.is_ascii_whitespace(),
            'S' => !c.is_ascii_whitespace(),
            _ => false,
        },
    }
}

pub(crate) fn class_matches(neg: bool, items: &[ClassItem], c: char, ci: bool) -> bool {
    let mut m = false;
    for it in items {
        if class_item_match(it, c, ci) {
            m = true;
            break;
        }
    }
    m ^ neg
}

pub(crate) const RX_STEP_BUDGET: usize = 2_000_000;
pub(crate) const RX_DEPTH_CAP: usize = 40_000;

pub(crate) struct RxCtx<'a> {
    text: &'a [char],
    flags: RxFlags,
    steps: usize,
}

pub(crate) fn rx_run(prog: &[Inst], pc: usize, sp: usize, slots: &mut Vec<usize>, ctx: &mut RxCtx, depth: usize) -> bool {
    ctx.steps += 1;
    if ctx.steps > RX_STEP_BUDGET || depth > RX_DEPTH_CAP {
        return false;
    }
    let text = ctx.text;
    let flags = ctx.flags;
    let d = depth + 1;
    match &prog[pc] {
        Inst::Match => true,
        Inst::Char(c) => {
            sp < text.len()
                && rx_ceq(text[sp], *c, flags.ci)
                && rx_run(prog, pc + 1, sp + 1, slots, ctx, d)
        }
        Inst::Any => {
            sp < text.len()
                && (flags.dotall || text[sp] != '\n')
                && rx_run(prog, pc + 1, sp + 1, slots, ctx, d)
        }
        Inst::Class(neg, items) => {
            sp < text.len()
                && class_matches(*neg, items, text[sp], flags.ci)
                && rx_run(prog, pc + 1, sp + 1, slots, ctx, d)
        }
        Inst::Save(n) => {
            let n = *n;
            let old = if n < slots.len() {
                let o = slots[n];
                slots[n] = sp;
                o
            } else {
                usize::MAX
            };
            if rx_run(prog, pc + 1, sp, slots, ctx, d) {
                true
            } else {
                if n < slots.len() {
                    slots[n] = old;
                }
                false
            }
        }
        Inst::Jmp(x) => rx_run(prog, *x, sp, slots, ctx, d),
        Inst::Split(x, y) => {
            let (x, y) = (*x, *y);
            rx_run(prog, x, sp, slots, ctx, d) || rx_run(prog, y, sp, slots, ctx, d)
        }
        Inst::Start => {
            let ok = sp == 0 || (flags.multiline && sp > 0 && text[sp - 1] == '\n');
            ok && rx_run(prog, pc + 1, sp, slots, ctx, d)
        }
        Inst::End => {
            let ok = sp == text.len()
                || (flags.multiline && text[sp] == '\n')
                || (!flags.multiline && sp + 1 == text.len() && text[sp] == '\n');
            ok && rx_run(prog, pc + 1, sp, slots, ctx, d)
        }
        Inst::WordB(want) => {
            let before = sp > 0 && rx_is_word(text[sp - 1]);
            let after = sp < text.len() && rx_is_word(text[sp]);
            let boundary = before != after;
            (boundary == *want) && rx_run(prog, pc + 1, sp, slots, ctx, d)
        }
        Inst::Backref(n) => {
            let (gs, ge) = if 2 * n + 1 < slots.len() {
                (slots[2 * n], slots[2 * n + 1])
            } else {
                (usize::MAX, usize::MAX)
            };
            if gs == usize::MAX || ge == usize::MAX {
                return rx_run(prog, pc + 1, sp, slots, ctx, d);
            }
            let len = ge - gs;
            if sp + len <= text.len() && (0..len).all(|k| rx_ceq(text[sp + k], text[gs + k], flags.ci))
            {
                rx_run(prog, pc + 1, sp + len, slots, ctx, d)
            } else {
                false
            }
        }
        Inst::Look(neg, sub) => {
            let neg = *neg;
            let snapshot = slots.clone();
            let ok = rx_run(sub, 0, sp, slots, ctx, d);
            if ok != neg {
                if neg {
                    *slots = snapshot;
                }
                rx_run(prog, pc + 1, sp, slots, ctx, d)
            } else {
                *slots = snapshot;
                false
            }
        }
    }
}

pub(crate) fn rx_emit(re: &Re, prog: &mut Vec<Inst>) {
    match re {
        Re::Empty => {}
        Re::Char(c) => prog.push(Inst::Char(*c)),
        Re::Any => prog.push(Inst::Any),
        Re::Class(neg, items) => prog.push(Inst::Class(*neg, items.clone())),
        Re::Start => prog.push(Inst::Start),
        Re::End => prog.push(Inst::End),
        Re::WordB(b) => prog.push(Inst::WordB(*b)),
        Re::Backref(n) => prog.push(Inst::Backref(*n)),
        Re::Concat(v) => {
            for r in v {
                rx_emit(r, prog);
            }
        }
        Re::Group(idx, inner) => {
            if let Some(i) = idx {
                prog.push(Inst::Save(2 * i));
                rx_emit(inner, prog);
                prog.push(Inst::Save(2 * i + 1));
            } else {
                rx_emit(inner, prog);
            }
        }
        Re::Look(neg, inner) => {
            let mut sub = Vec::new();
            rx_emit(inner, &mut sub);
            sub.push(Inst::Match);
            prog.push(Inst::Look(*neg, sub));
        }
        Re::Alt(a, b) => {
            let split = prog.len();
            prog.push(Inst::Split(0, 0));
            rx_emit(a, prog);
            let jmp = prog.len();
            prog.push(Inst::Jmp(0));
            let l2 = prog.len();
            rx_emit(b, prog);
            let l3 = prog.len();
            prog[split] = Inst::Split(split + 1, l2);
            prog[jmp] = Inst::Jmp(l3);
        }
        Re::Star(inner, greedy) => {
            let l1 = prog.len();
            prog.push(Inst::Split(0, 0));
            rx_emit(inner, prog);
            prog.push(Inst::Jmp(l1));
            let l3 = prog.len();
            prog[l1] = if *greedy {
                Inst::Split(l1 + 1, l3)
            } else {
                Inst::Split(l3, l1 + 1)
            };
        }
        Re::Plus(inner, greedy) => {
            let l1 = prog.len();
            rx_emit(inner, prog);
            let sp = prog.len();
            prog.push(Inst::Split(0, 0));
            let l3 = prog.len();
            prog[sp] = if *greedy {
                Inst::Split(l1, l3)
            } else {
                Inst::Split(l3, l1)
            };
        }
        Re::Quest(inner, greedy) => {
            let sp = prog.len();
            prog.push(Inst::Split(0, 0));
            rx_emit(inner, prog);
            let l3 = prog.len();
            prog[sp] = if *greedy {
                Inst::Split(sp + 1, l3)
            } else {
                Inst::Split(l3, sp + 1)
            };
        }
        Re::Repeat(inner, min, max, greedy) => {
            for _ in 0..*min {
                rx_emit(inner, prog);
            }
            match max {
                None => rx_emit(&Re::Star(inner.clone(), *greedy), prog),
                Some(mx) => {
                    for _ in *min..*mx {
                        rx_emit(&Re::Quest(inner.clone(), *greedy), prog);
                    }
                }
            }
        }
    }
}

/// Parse a PHP regex literal of the form `/pattern/flags` into a compiled `Rx`.
pub(crate) fn rx_compile(raw: &str) -> Option<Rx> {
    let chars: Vec<char> = raw.trim().chars().collect();
    if chars.is_empty() {
        return None;
    }
    let open = chars[0];
    let close = match open {
        '(' => ')',
        '[' => ']',
        '{' => '}',
        '<' => '>',
        c if c.is_ascii_alphanumeric() || c == '\\' || c == ' ' => return None,
        c => c,
    };
    // find the last unescaped closing delimiter
    let end = (1..chars.len()).rev().find(|&i| chars[i] == close)?;
    let pattern: String = chars[1..end].iter().collect();
    let flag_str: String = chars[end + 1..].iter().collect();
    let mut flags = RxFlags {
        ci: false,
        dotall: false,
        multiline: false,
    };
    let mut extended = false;
    for f in flag_str.chars() {
        match f {
            'i' => flags.ci = true,
            's' => flags.dotall = true,
            'm' => flags.multiline = true,
            'x' => extended = true,
            'u' | 'U' | 'D' | 'A' | 'X' | 'S' => {} // ignore/no-op
            _ => {}
        }
    }
    let mut p = ReParser {
        c: pattern.chars().collect(),
        i: 0,
        ngroups: 0,
        names: Vec::new(),
        extended,
    };
    let root = p.alt();
    let ngroups = p.ngroups;
    let names = p.names;
    let anchored = matches!(&root, Re::Start)
        || matches!(&root, Re::Concat(v) if matches!(v.first(), Some(Re::Start)));
    let mut prog = Vec::new();
    prog.push(Inst::Save(0));
    rx_emit(&root, &mut prog);
    prog.push(Inst::Save(1));
    prog.push(Inst::Match);
    Some(Rx {
        prog,
        ngroups,
        flags,
        names,
        anchored: anchored && !flags.multiline,
    })
}

impl Rx {
    /// Find the leftmost match at or after `start`. Returns capture slots.
    pub(crate) fn exec(&self, text: &[char], start: usize, steps: &mut usize) -> Option<Vec<usize>> {
        let mut ctx = RxCtx {
            text,
            flags: self.flags,
            steps: *steps,
        };
        let mut from = start;
        let result = loop {
            let mut slots = vec![usize::MAX; 2 * (self.ngroups + 1)];
            if rx_run(&self.prog, 0, from, &mut slots, &mut ctx, 0) {
                break Some(slots);
            }
            if self.anchored || from >= text.len() || ctx.steps > RX_STEP_BUDGET {
                break None;
            }
            from += 1;
        };
        *steps = ctx.steps;
        result
    }
}

/// Extract the substring for capture group `g` (returns empty string if unset).
pub(crate) fn rx_group_str(text: &[char], slots: &[usize], g: usize) -> String {
    let (s, e) = (slots[2 * g], slots[2 * g + 1]);
    if s == usize::MAX || e == usize::MAX || s > e || e > text.len() {
        String::new()
    } else {
        text[s..e].iter().collect()
    }
}

/// Expand a `preg_replace` replacement template (`$1`, `${1}`, `\1`) using slots.
pub(crate) fn rx_expand_repl(repl: &str, text: &[char], slots: &[usize], ngroups: usize) -> String {
    let rc: Vec<char> = repl.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    let group_str = |n: usize| {
        if n <= ngroups {
            rx_group_str(text, slots, n)
        } else {
            String::new()
        }
    };
    while i < rc.len() {
        let ch = rc[i];
        if (ch == '$' || ch == '\\') && i + 1 < rc.len() {
            if rc[i + 1] == '{' {
                // ${n}
                let mut j = i + 2;
                let mut num = String::new();
                while j < rc.len() && rc[j].is_ascii_digit() {
                    num.push(rc[j]);
                    j += 1;
                }
                if j < rc.len() && rc[j] == '}' && !num.is_empty() {
                    out.push_str(&group_str(num.parse().unwrap_or(0)));
                    i = j + 1;
                    continue;
                }
            } else if rc[i + 1].is_ascii_digit() {
                let mut j = i + 1;
                let mut num = String::new();
                while j < rc.len() && rc[j].is_ascii_digit() && num.len() < 2 {
                    num.push(rc[j]);
                    j += 1;
                }
                out.push_str(&group_str(num.parse().unwrap_or(0)));
                i = j;
                continue;
            }
        }
        if ch == '\\' && i + 1 < rc.len() && rc[i + 1] == '\\' {
            out.push('\\');
            i += 2;
            continue;
        }
        out.push(ch);
        i += 1;
    }
    out
}

/// preg_replace on a single subject string. Returns None on a bad pattern.
pub(crate) fn rx_replace_str(rx: &Rx, repl: &str, subject: &str, limit: i64, count: &mut i64) -> String {
    let text: Vec<char> = subject.chars().collect();
    let mut out = String::new();
    let mut pos = 0usize;
    let mut steps = 0usize;
    let max = if limit < 0 { i64::MAX } else { limit };
    let mut done = 0i64;
    while pos <= text.len() && done < max {
        match rx.exec(&text, pos, &mut steps) {
            Some(slots) => {
                let (ms, me) = (slots[0], slots[1]);
                out.extend(&text[pos..ms]);
                out.push_str(&rx_expand_repl(repl, &text, &slots, rx.ngroups));
                *count += 1;
                done += 1;
                if me > ms {
                    pos = me;
                } else {
                    // empty match — emit one char to avoid looping
                    if me < text.len() {
                        out.push(text[me]);
                    }
                    pos = me + 1;
                }
            }
            None => break,
        }
    }
    if pos < text.len() {
        out.extend(&text[pos..]);
    }
    out
}

/// preg_replace_callback on a single subject. Calls `f` with each match array.
pub(crate) fn rx_replace_cb<F: FnMut(&[usize], &[char]) -> R<String>>(
    rx: &Rx,
    subject: &str,
    limit: i64,
    count: &mut i64,
    mut f: F,
) -> R<String> {
    let text: Vec<char> = subject.chars().collect();
    let mut out = String::new();
    let mut pos = 0usize;
    let mut steps = 0usize;
    let max = if limit < 0 { i64::MAX } else { limit };
    let mut done = 0i64;
    while pos <= text.len() && done < max {
        match rx.exec(&text, pos, &mut steps) {
            Some(slots) => {
                let (ms, me) = (slots[0], slots[1]);
                out.extend(&text[pos..ms]);
                out.push_str(&f(&slots, &text)?);
                *count += 1;
                done += 1;
                if me > ms {
                    pos = me;
                } else {
                    if me < text.len() {
                        out.push(text[me]);
                    }
                    pos = me + 1;
                }
            }
            None => break,
        }
    }
    if pos < text.len() {
        out.extend(&text[pos..]);
    }
    Ok(out)
}

pub(crate) fn rx_quote(s: &str, delim: Option<char>) -> String {
    let mut out = String::new();
    for c in s.chars() {
        let special = matches!(
            c,
            '.' | '\\'
                | '+'
                | '*'
                | '?'
                | '['
                | '^'
                | ']'
                | '$'
                | '('
                | ')'
                | '{'
                | '}'
                | '='
                | '!'
                | '<'
                | '>'
                | '|'
                | ':'
                | '-'
                | '#'
        ) || Some(c) == delim;
        if special {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

