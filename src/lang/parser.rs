//! Recursive-descent parser with precedence climbing for expressions.
//! Tokens in → AST out. Operator precedence is encoded as binding powers
//! (`infix_bp`) following PHP 8's table, so `1 + 2 * 3`, `$a . $b == $c`, and
//! `$x = $a ?: $b ?? $c` all associate correctly by construction.
#![allow(dead_code)]

use super::ast::*;
use super::token::{Kind, StrPart, Token};

#[derive(Debug)]
pub struct ParseError {
    pub msg: String,
    pub pos: usize,
}
type R<T> = Result<T, ParseError>;

pub struct Parser {
    toks: Vec<Token>,
    pos: usize,
    depth: usize,
}

const MAX_DEPTH: usize = 2500;

impl Parser {
    pub fn new(toks: Vec<Token>) -> Self {
        Parser { toks, pos: 0, depth: 0 }
    }

    pub fn parse(toks: Vec<Token>) -> R<Vec<Stmt>> {
        let mut p = Parser::new(toks);
        p.program()
    }

    // ---- token helpers --------------------------------------------------
    fn kind(&self) -> &Kind {
        &self.toks[self.pos].kind
    }
    fn at(&self, off: usize) -> &Kind {
        self.toks
            .get(self.pos + off)
            .map(|t| &t.kind)
            .unwrap_or(&Kind::Eof)
    }
    fn bump(&mut self) -> Kind {
        let k = self.toks[self.pos].kind.clone();
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        k
    }
    fn is_eof(&self) -> bool {
        matches!(self.kind(), Kind::Eof)
    }
    fn eat(&mut self, k: &Kind) -> bool {
        if self.kind() == k {
            self.bump();
            true
        } else {
            false
        }
    }
    fn expect(&mut self, k: &Kind) -> R<()> {
        if self.kind() == k {
            self.bump();
            Ok(())
        } else {
            Err(self.err(&format!("expected {:?}, found {:?}", k, self.kind())))
        }
    }
    fn err(&self, msg: &str) -> ParseError {
        ParseError { msg: msg.to_string(), pos: self.toks[self.pos].span.start }
    }

    /// Current token is identifier `kw` (case-insensitive)?
    fn at_kw(&self, kw: &str) -> bool {
        matches!(self.kind(), Kind::Ident(s) if s.eq_ignore_ascii_case(kw))
    }
    fn kw_at(&self, off: usize, kw: &str) -> bool {
        matches!(self.at(off), Kind::Ident(s) if s.eq_ignore_ascii_case(kw))
    }
    fn eat_kw(&mut self, kw: &str) -> bool {
        if self.at_kw(kw) {
            self.bump();
            true
        } else {
            false
        }
    }
    fn ident(&mut self) -> R<String> {
        match self.bump() {
            Kind::Ident(s) => Ok(s),
            other => Err(ParseError {
                msg: format!("expected identifier, found {other:?}"),
                pos: self.toks[self.pos.saturating_sub(1)].span.start,
            }),
        }
    }

    /// A statement terminator: `;`, or an implicit one before `?>` / EOF.
    fn semi(&mut self) -> R<()> {
        if self.eat(&Kind::Semi) {
            return Ok(());
        }
        if matches!(self.kind(), Kind::CloseTag | Kind::Eof) {
            return Ok(()); // `?>` and EOF act as implicit `;`
        }
        Err(self.err(&format!("expected `;`, found {:?}", self.kind())))
    }

    // ---- program / statement layer -------------------------------------
    fn program(&mut self) -> R<Vec<Stmt>> {
        let mut out = Vec::new();
        while !self.is_eof() {
            match self.kind() {
                Kind::InlineHtml(_) => {
                    if let Kind::InlineHtml(b) = self.bump() {
                        out.push(Stmt::InlineHtml(b));
                    }
                }
                Kind::OpenTag | Kind::CloseTag => {
                    self.bump(); // mode markers — transparent to statements
                }
                Kind::OpenEcho => {
                    self.bump();
                    let e = self.expr()?;
                    let mut items = vec![e];
                    while self.eat(&Kind::Comma) {
                        items.push(self.expr()?);
                    }
                    self.semi()?;
                    out.push(Stmt::Echo(items));
                }
                _ => out.push(self.statement()?),
            }
        }
        Ok(out)
    }

    /// Parse statements until a closing `}` (consumed) — a `{ … }` block body.
    fn block(&mut self) -> R<Vec<Stmt>> {
        self.expect(&Kind::LBrace)?;
        let mut out = Vec::new();
        while !matches!(self.kind(), Kind::RBrace | Kind::Eof) {
            out.push(self.statement()?);
        }
        self.expect(&Kind::RBrace)?;
        Ok(out)
    }

    /// A statement, OR a `{ block }`, OR a `: … end___;` alternative body.
    fn body_until(&mut self, ends: &[&str]) -> R<Vec<Stmt>> {
        if self.eat(&Kind::Colon) {
            let mut out = Vec::new();
            while !ends.iter().any(|e| self.at_kw(e)) && !self.is_eof() {
                // allow embedded close/open tags inside alternative syntax
                match self.kind() {
                    Kind::InlineHtml(_) => {
                        if let Kind::InlineHtml(b) = self.bump() {
                            out.push(Stmt::InlineHtml(b));
                        }
                    }
                    Kind::OpenTag | Kind::CloseTag => {
                        self.bump();
                    }
                    _ => out.push(self.statement()?),
                }
            }
            Ok(out)
        } else if matches!(self.kind(), Kind::LBrace) {
            self.block()
        } else {
            Ok(vec![self.statement()?])
        }
    }

    fn statement(&mut self) -> R<Stmt> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            self.depth -= 1;
            return Err(self.err("nesting too deep"));
        }
        let r = self.statement_inner();
        self.depth -= 1;
        r
    }

    fn statement_inner(&mut self) -> R<Stmt> {
        // `#[Attr]` before a declaration (class/function/enum/...) — skip it.
        if matches!(self.kind(), Kind::AttrStart) {
            self.skip_attributes();
            if matches!(self.kind(), Kind::Semi | Kind::Eof) {
                self.eat(&Kind::Semi);
                return Ok(Stmt::Nop);
            }
        }
        // keyword-led statements
        if let Kind::Ident(s) = self.kind() {
            let kw = s.to_ascii_lowercase();
            match kw.as_str() {
                "echo" => return self.stmt_echo(),
                "if" => return self.stmt_if(),
                "while" => return self.stmt_while(),
                "do" => return self.stmt_do_while(),
                "for" => return self.stmt_for(),
                "foreach" => return self.stmt_foreach(),
                "switch" => return self.stmt_switch(),
                "break" => return self.stmt_break_continue(true),
                "continue" => return self.stmt_break_continue(false),
                "return" => {
                    self.bump();
                    let e = if matches!(self.kind(), Kind::Semi | Kind::CloseTag | Kind::Eof) {
                        None
                    } else {
                        Some(self.expr()?)
                    };
                    self.semi()?;
                    return Ok(Stmt::Return(e));
                }
                "function" => {
                    // `function name(...)` decl vs `function(...)`/`function &(...)` closure expr
                    let is_decl = matches!(self.at(1), Kind::Ident(_))
                        || (matches!(self.at(1), Kind::Amp) && matches!(self.at(2), Kind::Ident(_)));
                    if is_decl {
                        return self.stmt_function();
                    }
                }
                "abstract" | "final" | "class" | "interface" | "trait" | "enum" => {
                    return self.stmt_class();
                }
                "readonly" if self.kw_at(1, "class") => {
                    return self.stmt_class();
                }
                "global" => {
                    self.bump();
                    let mut names = Vec::new();
                    loop {
                        if let Kind::Variable(v) = self.kind().clone() {
                            names.push(v);
                            self.bump();
                        }
                        if !self.eat(&Kind::Comma) {
                            break;
                        }
                    }
                    self.semi()?;
                    return Ok(Stmt::Global(names));
                }
                "static" if matches!(self.at(1), Kind::Variable(_)) => {
                    self.bump();
                    let mut vars = Vec::new();
                    loop {
                        if let Kind::Variable(v) = self.kind().clone() {
                            self.bump();
                            let def = if self.eat(&Kind::Assign) {
                                Some(self.expr()?)
                            } else {
                                None
                            };
                            vars.push((v, def));
                        }
                        if !self.eat(&Kind::Comma) {
                            break;
                        }
                    }
                    self.semi()?;
                    return Ok(Stmt::StaticVar(vars));
                }
                "unset" => {
                    self.bump();
                    self.expect(&Kind::LParen)?;
                    let mut items = Vec::new();
                    while !matches!(self.kind(), Kind::RParen) {
                        items.push(self.expr()?);
                        if !self.eat(&Kind::Comma) {
                            break;
                        }
                    }
                    self.expect(&Kind::RParen)?;
                    self.semi()?;
                    return Ok(Stmt::Unset(items));
                }
                "const" => {
                    self.bump();
                    let mut decls = Vec::new();
                    loop {
                        let name = self.ident()?;
                        self.expect(&Kind::Assign)?;
                        let v = self.expr()?;
                        decls.push((name, v));
                        if !self.eat(&Kind::Comma) {
                            break;
                        }
                    }
                    self.semi()?;
                    return Ok(Stmt::ConstDecl(decls));
                }
                "throw" => {
                    self.bump();
                    let e = self.expr()?;
                    self.semi()?;
                    return Ok(Stmt::Throw(e));
                }
                "try" => return self.stmt_try(),
                "namespace" => return self.stmt_namespace(),
                "use" => return self.stmt_use(),
                "declare" => {
                    self.bump();
                    self.expect(&Kind::LParen)?;
                    let mut depth = 1;
                    while depth > 0 && !self.is_eof() {
                        match self.bump() {
                            Kind::LParen => depth += 1,
                            Kind::RParen => depth -= 1,
                            _ => {}
                        }
                    }
                    if matches!(self.kind(), Kind::LBrace) {
                        let body = self.block()?;
                        return Ok(Stmt::Block(body));
                    }
                    self.semi()?;
                    return Ok(Stmt::Declare);
                }
                _ => {}
            }
        }
        // block
        if matches!(self.kind(), Kind::LBrace) {
            return Ok(Stmt::Block(self.block()?));
        }
        // empty statement
        if self.eat(&Kind::Semi) {
            return Ok(Stmt::Nop);
        }
        // expression statement
        let e = self.expr()?;
        self.semi()?;
        Ok(Stmt::Expr(e))
    }

    fn stmt_echo(&mut self) -> R<Stmt> {
        self.bump();
        let mut items = vec![self.expr()?];
        while self.eat(&Kind::Comma) {
            items.push(self.expr()?);
        }
        self.semi()?;
        Ok(Stmt::Echo(items))
    }

    fn stmt_if(&mut self) -> R<Stmt> {
        self.bump();
        let cond = self.paren_expr()?;
        let then = self.body_until(&["elseif", "else", "endif"])?;
        let mut elseifs = Vec::new();
        let mut els = None;
        loop {
            if self.at_kw("elseif") {
                self.bump();
                let c = self.paren_expr()?;
                let b = self.body_until(&["elseif", "else", "endif"])?;
                elseifs.push((c, b));
            } else if self.at_kw("else") && self.kw_at(1, "if") {
                // `else if`
                self.bump();
                self.bump();
                let c = self.paren_expr()?;
                let b = self.body_until(&["elseif", "else", "endif"])?;
                elseifs.push((c, b));
            } else if self.at_kw("else") {
                self.bump();
                els = Some(self.body_until(&["endif"])?);
                break;
            } else {
                break;
            }
        }
        if self.eat_kw("endif") {
            self.semi()?;
        }
        Ok(Stmt::If { cond, then, elseifs, els })
    }

    fn stmt_while(&mut self) -> R<Stmt> {
        self.bump();
        let cond = self.paren_expr()?;
        let body = self.body_until(&["endwhile"])?;
        if self.eat_kw("endwhile") {
            self.semi()?;
        }
        Ok(Stmt::While { cond, body })
    }

    fn stmt_do_while(&mut self) -> R<Stmt> {
        self.bump();
        let body = if matches!(self.kind(), Kind::LBrace) {
            self.block()?
        } else {
            vec![self.statement()?]
        };
        if !self.eat_kw("while") {
            return Err(self.err("expected `while` after `do` body"));
        }
        let cond = self.paren_expr()?;
        self.semi()?;
        Ok(Stmt::DoWhile { body, cond })
    }

    fn stmt_for(&mut self) -> R<Stmt> {
        self.bump();
        self.expect(&Kind::LParen)?;
        let init = self.expr_list_until(&Kind::Semi)?;
        self.expect(&Kind::Semi)?;
        let cond = self.expr_list_until(&Kind::Semi)?;
        self.expect(&Kind::Semi)?;
        let step = self.expr_list_until(&Kind::RParen)?;
        self.expect(&Kind::RParen)?;
        let body = self.body_until(&["endfor"])?;
        if self.eat_kw("endfor") {
            self.semi()?;
        }
        Ok(Stmt::For { init, cond, step, body })
    }

    fn expr_list_until(&mut self, end: &Kind) -> R<Vec<Expr>> {
        let mut out = Vec::new();
        if self.kind() == end {
            return Ok(out);
        }
        loop {
            out.push(self.expr()?);
            if !self.eat(&Kind::Comma) {
                break;
            }
        }
        Ok(out)
    }

    fn stmt_foreach(&mut self) -> R<Stmt> {
        self.bump();
        self.expect(&Kind::LParen)?;
        let array = self.expr()?;
        if !self.eat_kw("as") {
            return Err(self.err("expected `as` in foreach"));
        }
        let mut by_ref = self.eat(&Kind::Amp);
        let first = self.expr()?;
        let (key, value, vref) = if self.eat(&Kind::FatArrow) {
            let vref = self.eat(&Kind::Amp);
            let v = self.expr()?;
            (Some(first), v, vref)
        } else {
            (None, first, false)
        };
        if vref {
            by_ref = true;
        }
        self.expect(&Kind::RParen)?;
        let body = self.body_until(&["endforeach"])?;
        if self.eat_kw("endforeach") {
            self.semi()?;
        }
        Ok(Stmt::Foreach { array, key, value, by_ref, body })
    }

    fn stmt_switch(&mut self) -> R<Stmt> {
        self.bump();
        let subject = self.paren_expr()?;
        let alt = self.eat(&Kind::Colon);
        if !alt {
            self.expect(&Kind::LBrace)?;
        }
        let mut cases = Vec::new();
        loop {
            if self.at_kw("case") {
                self.bump();
                let test = self.expr()?;
                if !self.eat(&Kind::Colon) {
                    self.eat(&Kind::Semi);
                }
                let body = self.case_body()?;
                cases.push(SwitchCase { test: Some(test), body });
            } else if self.at_kw("default") {
                self.bump();
                if !self.eat(&Kind::Colon) {
                    self.eat(&Kind::Semi);
                }
                let body = self.case_body()?;
                cases.push(SwitchCase { test: None, body });
            } else {
                break;
            }
        }
        if alt {
            self.eat_kw("endswitch");
            self.semi()?;
        } else {
            self.expect(&Kind::RBrace)?;
        }
        Ok(Stmt::Switch { subject, cases })
    }

    fn case_body(&mut self) -> R<Vec<Stmt>> {
        let mut out = Vec::new();
        while !self.at_kw("case")
            && !self.at_kw("default")
            && !self.at_kw("endswitch")
            && !matches!(self.kind(), Kind::RBrace | Kind::Eof)
        {
            out.push(self.statement()?);
        }
        Ok(out)
    }

    fn stmt_break_continue(&mut self, is_break: bool) -> R<Stmt> {
        self.bump();
        let n = if let Kind::Int(n) = self.kind() {
            let n = *n as u32;
            self.bump();
            n
        } else {
            1
        };
        self.semi()?;
        Ok(if is_break {
            Stmt::Break(n)
        } else {
            Stmt::Continue(n)
        })
    }

    fn stmt_try(&mut self) -> R<Stmt> {
        self.bump();
        let body = self.block()?;
        let mut catches = Vec::new();
        while self.at_kw("catch") {
            self.bump();
            self.expect(&Kind::LParen)?;
            let mut types = vec![self.parse_name()?];
            while self.eat(&Kind::Pipe) {
                types.push(self.parse_name()?);
            }
            let var = if let Kind::Variable(v) = self.kind().clone() {
                self.bump();
                Some(v)
            } else {
                None
            };
            self.expect(&Kind::RParen)?;
            let cbody = self.block()?;
            catches.push(Catch { types, var, body: cbody });
        }
        let finally = if self.eat_kw("finally") {
            Some(self.block()?)
        } else {
            None
        };
        Ok(Stmt::Try { body, catches, finally })
    }

    fn stmt_namespace(&mut self) -> R<Stmt> {
        self.bump();
        let name = if matches!(self.kind(), Kind::Ident(_) | Kind::Backslash) {
            Some(self.parse_name()?)
        } else {
            None
        };
        if matches!(self.kind(), Kind::LBrace) {
            let body = self.block()?;
            Ok(Stmt::Namespace { name, body: Some(body) })
        } else {
            self.semi()?;
            Ok(Stmt::Namespace { name, body: None })
        }
    }

    fn stmt_use(&mut self) -> R<Stmt> {
        self.bump();
        // skip optional `function`/`const`
        self.eat_kw("function");
        self.eat_kw("const");
        let mut items = Vec::new();
        loop {
            let name = self.parse_name()?;
            let alias = if self.eat_kw("as") {
                Some(self.ident()?)
            } else {
                None
            };
            items.push(UseItem { name, alias });
            if !self.eat(&Kind::Comma) {
                break;
            }
        }
        self.semi()?;
        Ok(Stmt::Use(items))
    }

    fn stmt_function(&mut self) -> R<Stmt> {
        self.bump(); // `function`
        let by_ref_return = self.eat(&Kind::Amp);
        let name = self.ident()?;
        let params = self.parse_params()?;
        let ret_type = self.skip_return_type();
        let body = self.block()?;
        Ok(Stmt::Func(FuncDecl { name, params, body, by_ref_return, ret_type }))
    }

    // ---- parameters -----------------------------------------------------
    fn parse_params(&mut self) -> R<Vec<Param>> {
        self.expect(&Kind::LParen)?;
        let mut out = Vec::new();
        while !matches!(self.kind(), Kind::RParen) {
            self.skip_attributes();
            let mut promote = None;
            let mut readonly = false;
            loop {
                if self.at_kw("public") {
                    self.bump();
                    self.skip_set_visibility();
                    promote = Some(Visibility::Public);
                } else if self.at_kw("protected") {
                    self.bump();
                    self.skip_set_visibility();
                    promote = Some(Visibility::Protected);
                } else if self.at_kw("private") {
                    self.bump();
                    self.skip_set_visibility();
                    promote = Some(Visibility::Private);
                } else if self.at_kw("readonly") {
                    self.bump();
                    readonly = true;
                } else {
                    break;
                }
            }
            let type_hint = self.parse_type_opt();
            let by_ref = self.eat(&Kind::Amp);
            let variadic = self.eat(&Kind::Ellipsis);
            let name = match self.bump() {
                Kind::Variable(v) => v,
                other => return Err(self.errk("expected parameter variable", &other)),
            };
            let default = if self.eat(&Kind::Assign) {
                Some(self.expr()?)
            } else {
                None
            };
            out.push(Param { name, default, by_ref, variadic, type_hint, promote, readonly });
            if !self.eat(&Kind::Comma) {
                break;
            }
        }
        self.expect(&Kind::RParen)?;
        Ok(out)
    }

    fn errk(&self, msg: &str, k: &Kind) -> ParseError {
        ParseError { msg: format!("{msg}, found {k:?}"), pos: self.toks[self.pos.saturating_sub(1)].span.start }
    }

    /// Parse an optional type hint (`?Foo`, `int|string`, `\A\B`, `Foo&Bar`),
    /// returning its text. Stops before a `$var`, `&$var`, `...`, `)`, or a bare
    /// following identifier (e.g. the NAME in `const int NAME = …`). A type is a
    /// sequence of name-segments joined by `|`/`&`; two bare identifiers in a row
    /// end the type (the second begins the next grammar element).
    fn parse_type_opt(&mut self) -> Option<String> {
        let mut t = String::new();
        if matches!(self.kind(), Kind::Question) {
            self.bump();
            t.push('?');
        }
        if !matches!(self.kind(), Kind::Ident(_) | Kind::Backslash) {
            return if t.is_empty() { None } else { Some(t) };
        }
        loop {
            // one segment: leading `\`, then Name(\Name)*
            let mut got = false;
            while matches!(self.kind(), Kind::Backslash) {
                t.push('\\');
                self.bump();
            }
            loop {
                if let Kind::Ident(s) = self.kind().clone() {
                    t.push_str(&s);
                    self.bump();
                    got = true;
                } else {
                    break;
                }
                if matches!(self.kind(), Kind::Backslash) {
                    t.push('\\');
                    self.bump();
                } else {
                    break;
                }
            }
            if !got {
                break;
            }
            // a `|` or `&` continues into another segment; anything else ends the type
            match self.kind() {
                Kind::Pipe => {
                    t.push('|');
                    self.bump();
                }
                Kind::Amp if matches!(self.at(1), Kind::Ident(_) | Kind::Backslash) => {
                    t.push('&');
                    self.bump();
                }
                _ => break,
            }
        }
        Some(t)
    }

    fn skip_return_type(&mut self) -> Option<String> {
        if self.eat(&Kind::Colon) {
            return self.parse_type_opt();
        }
        None
    }

    /// PHP 8.4 asymmetric visibility: a visibility keyword may be followed by
    /// `(set)`, e.g. `public private(set) int $x`. We don't model set-visibility
    /// separately yet, so just consume the parenthesised modifier.
    fn skip_set_visibility(&mut self) {
        if matches!(self.kind(), Kind::LParen) {
            self.bump();
            while !matches!(self.kind(), Kind::RParen | Kind::Eof) {
                self.bump();
            }
            self.eat(&Kind::RParen);
        }
    }

    /// Consume a balanced `{ … }` block (used to skip property-hook bodies).
    fn skip_braced_block(&mut self) {
        if !matches!(self.kind(), Kind::LBrace) {
            return;
        }
        let mut depth = 0;
        loop {
            match self.bump() {
                Kind::LBrace => depth += 1,
                Kind::RBrace => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                Kind::Eof => break,
                _ => {}
            }
        }
    }

    fn skip_attributes(&mut self) {
        while matches!(self.kind(), Kind::AttrStart) {
            self.bump();
            let mut depth = 1;
            while depth > 0 && !self.is_eof() {
                match self.bump() {
                    Kind::LBracket => depth += 1,
                    Kind::RBracket => depth -= 1,
                    _ => {}
                }
            }
        }
    }

    // ---- classes --------------------------------------------------------
    fn stmt_class(&mut self) -> R<Stmt> {
        let mut is_abstract = false;
        let mut is_final = false;
        loop {
            if self.eat_kw("abstract") {
                is_abstract = true;
            } else if self.eat_kw("final") {
                is_final = true;
            } else if self.at_kw("readonly") {
                self.bump();
            } else {
                break;
            }
        }
        let kind = if self.eat_kw("class") {
            ClassKind::Class
        } else if self.eat_kw("interface") {
            ClassKind::Interface
        } else if self.eat_kw("trait") {
            ClassKind::Trait
        } else if self.eat_kw("enum") {
            ClassKind::Enum
        } else {
            return Err(self.err("expected class/interface/trait/enum"));
        };
        let name = self.ident()?;
        let mut enum_backing = None;
        if kind == ClassKind::Enum && self.eat(&Kind::Colon) {
            enum_backing = self.parse_type_opt();
        }
        let mut parent = None;
        let mut interfaces = Vec::new();
        if self.eat_kw("extends") {
            parent = Some(self.parse_name()?);
            // interfaces can `extends A, B`
            while self.eat(&Kind::Comma) {
                interfaces.push(self.parse_name()?);
            }
        }
        if self.eat_kw("implements") {
            loop {
                interfaces.push(self.parse_name()?);
                if !self.eat(&Kind::Comma) {
                    break;
                }
            }
        }
        let mut decl = ClassDecl {
            kind,
            name,
            parent,
            interfaces,
            is_abstract,
            is_final,
            enum_backing,
            consts: Vec::new(),
            props: Vec::new(),
            methods: Vec::new(),
            uses_traits: Vec::new(),
            cases: Vec::new(),
        };
        self.parse_class_body(&mut decl)?;
        Ok(Stmt::Class(decl))
    }

    fn parse_class_body(&mut self, decl: &mut ClassDecl) -> R<()> {
        self.expect(&Kind::LBrace)?;
        'members: while !matches!(self.kind(), Kind::RBrace | Kind::Eof) {
            self.skip_attributes();
            // `use TraitName;`
            if self.at_kw("use") {
                self.bump();
                loop {
                    decl.uses_traits.push(self.parse_name()?);
                    if !self.eat(&Kind::Comma) {
                        break;
                    }
                }
                if matches!(self.kind(), Kind::LBrace) {
                    // trait adaptation block — skip it
                    let mut depth = 0;
                    loop {
                        match self.bump() {
                            Kind::LBrace => depth += 1,
                            Kind::RBrace => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            Kind::Eof => break,
                            _ => {}
                        }
                    }
                } else {
                    self.semi()?;
                }
                continue;
            }
            // enum case
            if decl.kind == ClassKind::Enum && self.at_kw("case") {
                self.bump();
                let cname = self.ident()?;
                let value = if self.eat(&Kind::Assign) {
                    Some(self.expr()?)
                } else {
                    None
                };
                self.semi()?;
                decl.cases.push(EnumCase { name: cname, value });
                continue;
            }
            // modifiers
            let mut visibility = Visibility::Public;
            let mut is_static = false;
            let mut is_abstract = false;
            let mut is_final = false;
            let mut readonly = false;
            loop {
                if self.at_kw("public") {
                    self.bump();
                    self.skip_set_visibility();
                    visibility = Visibility::Public;
                } else if self.at_kw("protected") {
                    self.bump();
                    self.skip_set_visibility();
                    visibility = Visibility::Protected;
                } else if self.at_kw("private") {
                    self.bump();
                    self.skip_set_visibility();
                    visibility = Visibility::Private;
                } else if self.at_kw("static") {
                    self.bump();
                    is_static = true;
                } else if self.at_kw("abstract") {
                    self.bump();
                    is_abstract = true;
                } else if self.at_kw("final") {
                    self.bump();
                    is_final = true;
                } else if self.at_kw("readonly") {
                    self.bump();
                    readonly = true;
                } else {
                    break;
                }
            }
            // const
            if self.at_kw("const") {
                self.bump();
                // PHP 8.3 typed constants: `const TYPE NAME = …`. A type is present
                // unless it's `NAME =` (untyped) — i.e. a `?`/`\` lead, or an
                // identifier NOT immediately followed by `=`.
                let typed = matches!(self.kind(), Kind::Question | Kind::Backslash)
                    || (matches!(self.kind(), Kind::Ident(_)) && !matches!(self.at(1), Kind::Assign));
                if typed {
                    let _ = self.parse_type_opt();
                }
                loop {
                    let cname = self.ident()?;
                    self.expect(&Kind::Assign)?;
                    let v = self.expr()?;
                    decl.consts.push(ClassConstDecl { name: cname, value: v, visibility });
                    if !self.eat(&Kind::Comma) {
                        break;
                    }
                }
                self.semi()?;
                continue;
            }
            // method
            if self.at_kw("function") {
                self.bump();
                let by_ref_return = self.eat(&Kind::Amp);
                let mname = self.member_name()?;
                let params = self.parse_params()?;
                let ret_type = self.skip_return_type();
                let body = if matches!(self.kind(), Kind::LBrace) {
                    Some(self.block()?)
                } else {
                    self.semi()?;
                    None
                };
                decl.methods.push(MethodDecl {
                    name: mname,
                    params,
                    body,
                    visibility,
                    is_static,
                    is_abstract,
                    is_final,
                    by_ref_return,
                    ret_type,
                });
                continue;
            }
            // property (optional type, then $name [= default] [, ...])
            let type_hint = self.parse_type_opt();
            loop {
                let pname = match self.bump() {
                    Kind::Variable(v) => v,
                    other => return Err(self.errk("expected property variable", &other)),
                };
                let default = if self.eat(&Kind::Assign) {
                    Some(self.expr()?)
                } else {
                    None
                };
                decl.props.push(PropDecl {
                    name: pname,
                    default,
                    visibility,
                    is_static,
                    readonly,
                    type_hint: type_hint.clone(),
                });
                // PHP 8.4 property hooks: `$x { get => …; set { … } }` — parse-skip
                // the hook block (the property itself is recorded; hooks are no-ops).
                if matches!(self.kind(), Kind::LBrace) {
                    self.skip_braced_block();
                    continue 'members;
                }
                if !self.eat(&Kind::Comma) {
                    break;
                }
            }
            self.semi()?;
        }
        self.expect(&Kind::RBrace)?;
        Ok(())
    }

    /// A method name: an identifier, possibly a keyword (e.g. `list`, `print`).
    fn member_name(&mut self) -> R<String> {
        match self.bump() {
            Kind::Ident(s) => Ok(s),
            other => Err(self.errk("expected method name", &other)),
        }
    }

    // ---- names ----------------------------------------------------------
    fn parse_name(&mut self) -> R<Name> {
        let fq = self.eat(&Kind::Backslash);
        let mut parts = Vec::new();
        match self.bump() {
            Kind::Ident(s) => parts.push(s),
            other => return Err(self.errk("expected name", &other)),
        }
        while matches!(self.kind(), Kind::Backslash) {
            self.bump();
            match self.bump() {
                Kind::Ident(s) => parts.push(s),
                other => return Err(self.errk("expected name segment", &other)),
            }
        }
        Ok(Name { parts, fully_qualified: fq })
    }

    // ---- expressions: precedence climbing ------------------------------
    fn paren_expr(&mut self) -> R<Expr> {
        self.expect(&Kind::LParen)?;
        let e = self.expr()?;
        self.expect(&Kind::RParen)?;
        Ok(e)
    }

    pub fn expr(&mut self) -> R<Expr> {
        self.expr_bp(0)
    }

    fn expr_bp(&mut self, min_bp: u8) -> R<Expr> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            self.depth -= 1;
            return Err(self.err("expression nesting too deep"));
        }
        let r = self.expr_bp_inner(min_bp);
        self.depth -= 1;
        r
    }

    fn expr_bp_inner(&mut self, min_bp: u8) -> R<Expr> {
        let mut lhs = self.prefix()?;
        loop {
            // ternary  a ? b : c   and   a ?: c
            if matches!(self.kind(), Kind::Question) {
                if 12 < min_bp {
                    break;
                }
                self.bump();
                let mid = if matches!(self.kind(), Kind::Colon) {
                    None
                } else {
                    Some(Box::new(self.expr_bp(0)?))
                };
                self.expect(&Kind::Colon)?;
                let els = self.expr_bp(12)?;
                lhs = Expr::Ternary(Box::new(lhs), mid, Box::new(els));
                continue;
            }
            // assignment (right-assoc, bp 10)
            if let Some(op) = self.assign_op() {
                if 10 < min_bp {
                    break;
                }
                self.bump();
                match op {
                    AssignKind::Eq => {
                        // `=&` reference assignment
                        if self.eat(&Kind::Amp) {
                            let rhs = self.expr_bp(10)?;
                            lhs = Expr::AssignRef(Box::new(lhs), Box::new(rhs));
                        } else {
                            let rhs = self.expr_bp(10)?;
                            lhs = Expr::Assign(Box::new(lhs), Box::new(rhs));
                        }
                    }
                    AssignKind::Op(b) => {
                        let rhs = self.expr_bp(10)?;
                        lhs = Expr::AssignOp(b, Box::new(lhs), Box::new(rhs));
                    }
                }
                continue;
            }
            // `instanceof`
            if self.at_kw("instanceof") {
                if 40 < min_bp {
                    break;
                }
                self.bump();
                let rhs = self.instanceof_rhs()?;
                lhs = Expr::InstanceOf(Box::new(lhs), Box::new(rhs));
                continue;
            }
            // PHP 8.5 pipe operator  `lhs |> callable`  ==  callable(lhs).
            // Binds tighter than comparison, looser than concat; left-assoc.
            if matches!(self.kind(), Kind::PipeArrow) {
                if 30 < min_bp {
                    break;
                }
                self.bump();
                let rhs = self.expr_bp(31)?;
                lhs = Expr::Call(
                    Box::new(rhs),
                    vec![Arg { value: lhs, spread: false, by_ref: false, name: None }],
                );
                continue;
            }
            // keyword logical operators and / or / xor
            if let Some((b, lbp, rbp)) = self.kw_logical() {
                if lbp < min_bp {
                    break;
                }
                self.bump();
                let rhs = self.expr_bp(rbp)?;
                lhs = Expr::Binary(b, Box::new(lhs), Box::new(rhs));
                continue;
            }
            // regular binary operators
            if let Some((b, lbp, rbp)) = infix_bp(self.kind()) {
                if lbp < min_bp {
                    break;
                }
                self.bump();
                let rhs = self.expr_bp(rbp)?;
                lhs = Expr::Binary(b, Box::new(lhs), Box::new(rhs));
                continue;
            }
            break;
        }
        Ok(lhs)
    }

    fn assign_op(&self) -> Option<AssignKind> {
        Some(match self.kind() {
            Kind::Assign => AssignKind::Eq,
            Kind::PlusEq => AssignKind::Op(BinOp::Add),
            Kind::MinusEq => AssignKind::Op(BinOp::Sub),
            Kind::StarEq => AssignKind::Op(BinOp::Mul),
            Kind::SlashEq => AssignKind::Op(BinOp::Div),
            Kind::PercentEq => AssignKind::Op(BinOp::Mod),
            Kind::PowEq => AssignKind::Op(BinOp::Pow),
            Kind::DotEq => AssignKind::Op(BinOp::Concat),
            Kind::AndEq => AssignKind::Op(BinOp::BitAnd),
            Kind::OrEq => AssignKind::Op(BinOp::BitOr),
            Kind::XorEq => AssignKind::Op(BinOp::BitXor),
            Kind::ShlEq => AssignKind::Op(BinOp::Shl),
            Kind::ShrEq => AssignKind::Op(BinOp::Shr),
            Kind::CoalesceEq => AssignKind::Op(BinOp::Coalesce),
            _ => return None,
        })
    }

    fn kw_logical(&self) -> Option<(BinOp, u8, u8)> {
        if self.at_kw("and") {
            Some((BinOp::And, 8, 9))
        } else if self.at_kw("xor") {
            Some((BinOp::Xor, 6, 7))
        } else if self.at_kw("or") {
            Some((BinOp::Or, 4, 5))
        } else {
            None
        }
    }

    fn instanceof_rhs(&mut self) -> R<Expr> {
        // a class name, or an expression yielding a class/object
        if matches!(self.kind(), Kind::Ident(_) | Kind::Backslash) {
            Ok(Expr::ConstFetch(self.parse_name()?))
        } else {
            self.prefix()
        }
    }

    // ---- prefix (unary / cast / new / clone / primary) -----------------
    fn prefix(&mut self) -> R<Expr> {
        match self.kind().clone() {
            Kind::Minus => {
                self.bump();
                Ok(Expr::Unary(UnOp::Neg, Box::new(self.expr_bp(42)?)))
            }
            Kind::Plus => {
                self.bump();
                Ok(Expr::Unary(UnOp::Pos, Box::new(self.expr_bp(42)?)))
            }
            Kind::Not => {
                self.bump();
                Ok(Expr::Unary(UnOp::Not, Box::new(self.expr_bp(41)?)))
            }
            Kind::Tilde => {
                self.bump();
                Ok(Expr::Unary(UnOp::BitNot, Box::new(self.expr_bp(42)?)))
            }
            Kind::At => {
                self.bump();
                Ok(Expr::ErrorSuppress(Box::new(self.expr_bp(42)?)))
            }
            Kind::Inc => {
                self.bump();
                Ok(Expr::PreInc(Box::new(self.expr_bp(42)?)))
            }
            Kind::Dec => {
                self.bump();
                Ok(Expr::PreDec(Box::new(self.expr_bp(42)?)))
            }
            Kind::Amp => {
                // reference in expression position (e.g. `&$x` in array/uses); treat operand
                self.bump();
                self.expr_bp(42)
            }
            Kind::LParen => {
                // cast?  ( type )
                if let Some(ct) = self.peek_cast() {
                    self.bump(); // (
                    self.bump(); // type ident
                    self.bump(); // )
                    let e = self.expr_bp(42)?;
                    return Ok(Expr::Cast(ct, Box::new(e)));
                }
                self.bump();
                let e = self.expr_bp(0)?;
                self.expect(&Kind::RParen)?;
                self.postfix(e)
            }
            _ => {
                let p = self.primary()?;
                self.postfix(p)
            }
        }
    }

    fn peek_cast(&self) -> Option<CastType> {
        if let (Kind::Ident(s), Kind::RParen) = (self.at(1), self.at(2)) {
            let ct = match s.to_ascii_lowercase().as_str() {
                "int" | "integer" => CastType::Int,
                "float" | "double" | "real" => CastType::Float,
                "string" | "binary" => CastType::String,
                "bool" | "boolean" => CastType::Bool,
                "array" => CastType::Array,
                "object" => CastType::Object,
                "unset" => CastType::Unset,
                _ => return None,
            };
            Some(ct)
        } else {
            None
        }
    }

    // ---- primary --------------------------------------------------------
    fn primary(&mut self) -> R<Expr> {
        match self.kind().clone() {
            Kind::Int(n) => {
                self.bump();
                Ok(Expr::Int(n))
            }
            Kind::Float(f) => {
                self.bump();
                Ok(Expr::Float(f))
            }
            Kind::Str(s) => {
                self.bump();
                Ok(Expr::Str(s))
            }
            Kind::Template(parts) => {
                self.bump();
                self.build_template(parts)
            }
            Kind::Variable(v) => {
                self.bump();
                Ok(Expr::Var(v))
            }
            Kind::Dollar => {
                self.bump();
                // $$x  or  ${expr}
                if self.eat(&Kind::LBrace) {
                    let e = self.expr()?;
                    self.expect(&Kind::RBrace)?;
                    Ok(Expr::VarVar(Box::new(e)))
                } else {
                    let inner = self.primary()?;
                    Ok(Expr::VarVar(Box::new(inner)))
                }
            }
            Kind::LBracket => self.array_literal(Kind::RBracket),
            Kind::Backslash => {
                let name = self.parse_name()?;
                self.name_expr(name)
            }
            Kind::Ident(_) => {
                let kw = if let Kind::Ident(s) = self.kind() {
                    s.to_ascii_lowercase()
                } else {
                    unreachable!()
                };
                match kw.as_str() {
                    "true" => {
                        self.bump();
                        Ok(Expr::Bool(true))
                    }
                    "false" => {
                        self.bump();
                        Ok(Expr::Bool(false))
                    }
                    "null" => {
                        self.bump();
                        Ok(Expr::Null)
                    }
                    "array" if matches!(self.at(1), Kind::LParen) => {
                        self.bump();
                        self.bump(); // (
                        self.array_items_rest(Kind::RParen)
                    }
                    "new" => self.parse_new(),
                    "clone" => {
                        self.bump();
                        Ok(Expr::Clone(Box::new(self.expr_bp(42)?)))
                    }
                    "print" => {
                        self.bump();
                        Ok(Expr::Print(Box::new(self.expr_bp(10)?)))
                    }
                    "throw" => {
                        self.bump();
                        Ok(Expr::Throw(Box::new(self.expr_bp(0)?)))
                    }
                    "yield" => {
                        self.bump();
                        if self.eat_kw("from") {
                            let e = self.expr_bp(10)?;
                            Ok(Expr::YieldFrom(Box::new(e)))
                        } else if matches!(
                            self.kind(),
                            Kind::Semi | Kind::RParen | Kind::RBracket | Kind::Eof | Kind::Comma
                        ) {
                            Ok(Expr::Yield(None, None))
                        } else {
                            let first = self.expr_bp(10)?;
                            if self.eat(&Kind::FatArrow) {
                                let v = self.expr_bp(10)?;
                                Ok(Expr::Yield(Some(Box::new(first)), Some(Box::new(v))))
                            } else {
                                Ok(Expr::Yield(None, Some(Box::new(first))))
                            }
                        }
                    }
                    "isset" => {
                        self.bump();
                        self.expect(&Kind::LParen)?;
                        let mut items = Vec::new();
                        while !matches!(self.kind(), Kind::RParen) {
                            items.push(self.expr()?);
                            if !self.eat(&Kind::Comma) {
                                break;
                            }
                        }
                        self.expect(&Kind::RParen)?;
                        Ok(Expr::Isset(items))
                    }
                    "empty" => {
                        self.bump();
                        Ok(Expr::Empty(Box::new(self.paren_expr()?)))
                    }
                    "list" => {
                        self.bump();
                        self.expect(&Kind::LParen)?;
                        self.list_rest()
                    }
                    "match" => self.parse_match(),
                    "function" => self.parse_closure(false),
                    "fn" => self.parse_arrow(false),
                    "static" if self.kw_at(1, "function") => {
                        self.bump();
                        self.parse_closure(true)
                    }
                    "static" if self.kw_at(1, "fn") => {
                        self.bump();
                        self.parse_arrow(true)
                    }
                    "include" | "include_once" | "require" | "require_once" | "eval" | "exit"
                    | "die" => {
                        // treat as a call-like construct: name + optional ( expr )
                        let nm = self.ident()?;
                        if matches!(self.kind(), Kind::LParen) {
                            let args = self.parse_args()?;
                            Ok(Expr::Call(Box::new(Expr::ConstFetch(Name::simple(nm))), args))
                        } else if matches!(
                            self.kind(),
                            Kind::Semi | Kind::CloseTag | Kind::Eof | Kind::RParen
                        ) {
                            Ok(Expr::Call(Box::new(Expr::ConstFetch(Name::simple(nm))), vec![]))
                        } else {
                            let e = self.expr_bp(10)?;
                            Ok(Expr::Call(
                                Box::new(Expr::ConstFetch(Name::simple(nm))),
                                vec![Arg { value: e, spread: false, by_ref: false, name: None }],
                            ))
                        }
                    }
                    _ => {
                        let name = self.parse_name()?;
                        self.name_expr(name)
                    }
                }
            }
            other => Err(self.errk("unexpected token in expression", &other)),
        }
    }

    /// A bareword name in value position — magic constant, or a plain constant
    /// fetch (the postfix layer turns `name(` into a call and `name::` into a
    /// static access).
    fn name_expr(&mut self, name: Name) -> R<Expr> {
        if !name.fully_qualified && name.parts.len() == 1 {
            let u = name.parts[0].to_ascii_uppercase();
            if u.starts_with("__") && u.ends_with("__") {
                return Ok(Expr::MagicConst(name.parts[0].clone()));
            }
        }
        Ok(Expr::ConstFetch(name))
    }

    fn parse_new(&mut self) -> R<Expr> {
        self.bump(); // new
        if self.at_kw("class") {
            // anonymous class
            self.bump();
            let args = if matches!(self.kind(), Kind::LParen) {
                self.parse_args()?
            } else {
                vec![]
            };
            let mut parent = None;
            let mut interfaces = Vec::new();
            if self.eat_kw("extends") {
                parent = Some(self.parse_name()?);
            }
            if self.eat_kw("implements") {
                loop {
                    interfaces.push(self.parse_name()?);
                    if !self.eat(&Kind::Comma) {
                        break;
                    }
                }
            }
            let mut decl = ClassDecl {
                kind: ClassKind::Class,
                name: String::new(),
                parent,
                interfaces,
                is_abstract: false,
                is_final: false,
                enum_backing: None,
                consts: vec![],
                props: vec![],
                methods: vec![],
                uses_traits: vec![],
                cases: vec![],
            };
            self.parse_class_body(&mut decl)?;
            return Ok(Expr::NewAnon(Box::new(decl), args));
        }
        // class reference: a name, a variable, `static`/`self`/`parent`, or (expr)
        let class = if matches!(self.kind(), Kind::Ident(_) | Kind::Backslash) {
            Expr::ConstFetch(self.parse_name()?)
        } else {
            // variable / dynamic
            let p = self.primary()?;
            self.member_chain(p)?
        };
        let args = if matches!(self.kind(), Kind::LParen) {
            self.parse_args()?
        } else {
            vec![]
        };
        Ok(Expr::New(Box::new(class), args))
    }

    /// Like `postfix` but only `->`/`::`/`[]` access (used for `new $a->b`).
    fn member_chain(&mut self, mut e: Expr) -> R<Expr> {
        loop {
            match self.kind() {
                Kind::Arrow | Kind::NullArrow => {
                    let nullsafe = matches!(self.kind(), Kind::NullArrow);
                    self.bump();
                    let name = self.prop_name()?;
                    e = Expr::Prop(Box::new(e), name, nullsafe);
                }
                Kind::DoubleColon => {
                    self.bump();
                    let name = self.member_name()?;
                    e = Expr::ClassConst(Box::new(e), name);
                }
                Kind::LBracket => {
                    self.bump();
                    let idx = if matches!(self.kind(), Kind::RBracket) {
                        None
                    } else {
                        Some(Box::new(self.expr()?))
                    };
                    self.expect(&Kind::RBracket)?;
                    e = Expr::Index(Box::new(e), idx);
                }
                _ => break,
            }
        }
        Ok(e)
    }

    fn parse_match(&mut self) -> R<Expr> {
        self.bump(); // match
        let subject = self.paren_expr()?;
        self.expect(&Kind::LBrace)?;
        let mut arms = Vec::new();
        while !matches!(self.kind(), Kind::RBrace | Kind::Eof) {
            if self.eat_kw("default") {
                self.expect(&Kind::FatArrow)?;
                let body = self.expr()?;
                arms.push(MatchArm { conditions: None, body });
            } else {
                let mut conds = vec![self.expr()?];
                while self.eat(&Kind::Comma) {
                    if matches!(self.kind(), Kind::FatArrow) {
                        break;
                    }
                    conds.push(self.expr()?);
                }
                self.expect(&Kind::FatArrow)?;
                let body = self.expr()?;
                arms.push(MatchArm { conditions: Some(conds), body });
            }
            if !self.eat(&Kind::Comma) {
                break;
            }
        }
        self.expect(&Kind::RBrace)?;
        Ok(Expr::Match(Box::new(subject), arms))
    }

    fn parse_closure(&mut self, is_static: bool) -> R<Expr> {
        self.bump(); // function
        let by_ref_return = self.eat(&Kind::Amp);
        let params = self.parse_params()?;
        let mut uses = Vec::new();
        if self.eat_kw("use") {
            self.expect(&Kind::LParen)?;
            while !matches!(self.kind(), Kind::RParen) {
                let by_ref = self.eat(&Kind::Amp);
                if let Kind::Variable(v) = self.kind().clone() {
                    self.bump();
                    uses.push(ClosureUse { name: v, by_ref });
                }
                if !self.eat(&Kind::Comma) {
                    break;
                }
            }
            self.expect(&Kind::RParen)?;
        }
        self.skip_return_type();
        let body = self.block()?;
        Ok(Expr::Closure(Box::new(Closure {
            params,
            uses,
            body,
            is_static,
            by_ref_return,
        })))
    }

    fn parse_arrow(&mut self, is_static: bool) -> R<Expr> {
        self.bump(); // fn
        let _ = self.eat(&Kind::Amp);
        let params = self.parse_params()?;
        self.skip_return_type();
        self.expect(&Kind::FatArrow)?;
        let body = self.expr_bp(10)?;
        Ok(Expr::ArrowFn(Box::new(ArrowFn { params, body, is_static })))
    }

    // ---- postfix: ->  ?->  ::  []  {}  (call)  ++  -- -------------------
    fn postfix(&mut self, mut e: Expr) -> R<Expr> {
        loop {
            match self.kind() {
                Kind::Arrow | Kind::NullArrow => {
                    let nullsafe = matches!(self.kind(), Kind::NullArrow);
                    self.bump();
                    let name = self.prop_name()?;
                    if matches!(self.kind(), Kind::LParen) {
                        let args = self.parse_args()?;
                        e = Expr::MethodCall(Box::new(e), name, args, nullsafe);
                    } else {
                        e = Expr::Prop(Box::new(e), name, nullsafe);
                    }
                }
                Kind::DoubleColon => {
                    self.bump();
                    // ::$prop  | ::CONST | ::method(...) | ::class | ::{expr}
                    if let Kind::Variable(v) = self.kind().clone() {
                        self.bump();
                        e = Expr::StaticProp(Box::new(e), v);
                    } else if matches!(self.kind(), Kind::LBrace) {
                        self.bump();
                        let inner = self.expr()?;
                        self.expect(&Kind::RBrace)?;
                        if matches!(self.kind(), Kind::LParen) {
                            let args = self.parse_args()?;
                            e = Expr::StaticCall(Box::new(e), PropName::Expr(Box::new(inner)), args);
                        } else {
                            e = Expr::ClassConst(Box::new(e), String::new());
                        }
                    } else {
                        let name = self.member_name()?;
                        if matches!(self.kind(), Kind::LParen) {
                            let args = self.parse_args()?;
                            e = Expr::StaticCall(Box::new(e), PropName::Id(name), args);
                        } else {
                            e = Expr::ClassConst(Box::new(e), name);
                        }
                    }
                }
                Kind::LBracket => {
                    self.bump();
                    let idx = if matches!(self.kind(), Kind::RBracket) {
                        None
                    } else {
                        Some(Box::new(self.expr()?))
                    };
                    self.expect(&Kind::RBracket)?;
                    e = Expr::Index(Box::new(e), idx);
                }
                Kind::LParen => {
                    let args = self.parse_args()?;
                    e = Expr::Call(Box::new(e), args);
                }
                Kind::Inc => {
                    self.bump();
                    e = Expr::PostInc(Box::new(e));
                }
                Kind::Dec => {
                    self.bump();
                    e = Expr::PostDec(Box::new(e));
                }
                _ => break,
            }
        }
        Ok(e)
    }

    fn prop_name(&mut self) -> R<PropName> {
        match self.kind().clone() {
            Kind::Ident(s) => {
                self.bump();
                Ok(PropName::Id(s))
            }
            Kind::Variable(v) => {
                self.bump();
                Ok(PropName::Expr(Box::new(Expr::Var(v))))
            }
            Kind::LBrace => {
                self.bump();
                let e = self.expr()?;
                self.expect(&Kind::RBrace)?;
                Ok(PropName::Expr(Box::new(e)))
            }
            Kind::Dollar => {
                let e = self.primary()?;
                Ok(PropName::Expr(Box::new(e)))
            }
            other => Err(self.errk("expected property/method name", &other)),
        }
    }

    fn parse_args(&mut self) -> R<Vec<Arg>> {
        self.expect(&Kind::LParen)?;
        // first-class callable: foo(...)
        if matches!(self.kind(), Kind::Ellipsis) && matches!(self.at(1), Kind::RParen) {
            self.bump();
            self.bump();
            return Ok(vec![Arg {
                value: Expr::Null,
                spread: false,
                by_ref: false,
                name: Some("...".to_string()), // sentinel: first-class callable
            }]);
        }
        let mut out = Vec::new();
        while !matches!(self.kind(), Kind::RParen) {
            let spread = self.eat(&Kind::Ellipsis);
            // named arg:  label : value   (label is an ident, not `::`)
            let name = if let Kind::Ident(s) = self.kind().clone() {
                if matches!(self.at(1), Kind::Colon) && !matches!(self.at(1), Kind::DoubleColon) {
                    self.bump(); // ident
                    self.bump(); // :
                    Some(s)
                } else {
                    None
                }
            } else {
                None
            };
            let by_ref = self.eat(&Kind::Amp);
            let value = self.expr()?;
            out.push(Arg { value, spread, by_ref, name });
            if !self.eat(&Kind::Comma) {
                break;
            }
        }
        self.expect(&Kind::RParen)?;
        Ok(out)
    }

    // ---- arrays & list --------------------------------------------------
    fn array_literal(&mut self, close: Kind) -> R<Expr> {
        self.bump(); // [
        self.array_items_rest(close)
    }

    fn array_items_rest(&mut self, close: Kind) -> R<Expr> {
        let mut items = Vec::new();
        while self.kind() != &close {
            if self.eat(&Kind::Comma) {
                continue; // skip stray/elided commas defensively
            }
            let spread = self.eat(&Kind::Ellipsis);
            let by_ref0 = self.eat(&Kind::Amp);
            let first = self.expr()?;
            let item = if self.eat(&Kind::FatArrow) {
                let by_ref = self.eat(&Kind::Amp);
                let value = self.expr()?;
                ArrayItem { key: Some(first), value, by_ref, spread: false }
            } else {
                ArrayItem { key: None, value: first, by_ref: by_ref0, spread }
            };
            items.push(item);
            if !self.eat(&Kind::Comma) {
                break;
            }
        }
        self.expect(&close)?;
        Ok(Expr::Array(items))
    }

    fn list_rest(&mut self) -> R<Expr> {
        let mut items = Vec::new();
        while !matches!(self.kind(), Kind::RParen) {
            if self.eat(&Kind::Comma) {
                items.push(None); // skipped element
                continue;
            }
            let first = self.expr()?;
            let item = if self.eat(&Kind::FatArrow) {
                let value = self.expr()?;
                ArrayItem { key: Some(first), value, by_ref: false, spread: false }
            } else {
                ArrayItem { key: None, value: first, by_ref: false, spread: false }
            };
            items.push(Some(item));
            if !self.eat(&Kind::Comma) {
                break;
            }
        }
        self.expect(&Kind::RParen)?;
        Ok(Expr::List(items))
    }

    // ---- templates (interpolated strings) ------------------------------
    fn build_template(&mut self, parts: Vec<StrPart>) -> R<Expr> {
        // A template that is a single literal collapses to a plain Str.
        if parts.len() == 1 {
            if let StrPart::Lit(b) = &parts[0] {
                return Ok(Expr::Str(b.clone()));
            }
        }
        let mut out = Vec::new();
        for p in parts {
            match p {
                StrPart::Lit(b) => out.push(TplPart::Lit(b)),
                StrPart::Expr(src) => {
                    // re-lex & parse the captured expression source in code mode
                    let e = parse_embedded(&src)?;
                    out.push(TplPart::Expr(Box::new(e)));
                }
            }
        }
        Ok(Expr::Template(out))
    }
}

enum AssignKind {
    Eq,
    Op(BinOp),
}

/// Binding powers for the regular binary operators (lbp, rbp). Left-assoc:
/// `(L, L+1)`; right-assoc: `(L+1, L)`. Matches PHP 8's precedence table.
fn infix_bp(k: &Kind) -> Option<(BinOp, u8, u8)> {
    Some(match k {
        Kind::Coalesce => (BinOp::Coalesce, 15, 14), // right-assoc
        Kind::OrOr => (BinOp::Or, 16, 17),
        Kind::AndAnd => (BinOp::And, 18, 19),
        Kind::Pipe => (BinOp::BitOr, 20, 21),
        Kind::Caret => (BinOp::BitXor, 22, 23),
        Kind::Amp => (BinOp::BitAnd, 24, 25),
        Kind::EqEq => (BinOp::Eq, 26, 27),
        Kind::NotEq => (BinOp::NotEq, 26, 27),
        Kind::Identical => (BinOp::Identical, 26, 27),
        Kind::NotIdentical => (BinOp::NotIdentical, 26, 27),
        Kind::Spaceship => (BinOp::Spaceship, 26, 27),
        Kind::Lt => (BinOp::Lt, 28, 29),
        Kind::Gt => (BinOp::Gt, 28, 29),
        Kind::Le => (BinOp::Le, 28, 29),
        Kind::Ge => (BinOp::Ge, 28, 29),
        // (30/31 is the PHP 8.5 pipe operator `|>`, handled specially in the
        // expression loop — it sits between comparison and concat.)
        Kind::Dot => (BinOp::Concat, 32, 33),
        Kind::Shl => (BinOp::Shl, 34, 35),
        Kind::Shr => (BinOp::Shr, 34, 35),
        Kind::Plus => (BinOp::Add, 36, 37),
        Kind::Minus => (BinOp::Sub, 36, 37),
        Kind::Star => (BinOp::Mul, 38, 39),
        Kind::Slash => (BinOp::Div, 38, 39),
        Kind::Percent => (BinOp::Mod, 38, 39),
        Kind::Pow => (BinOp::Pow, 45, 44), // right-assoc, very tight
        _ => return None,
    })
}

/// Parse an interpolated-expression fragment (e.g. `$user->name` or the inside
/// of `{$...}`) captured by the lexer, in code mode.
fn parse_embedded(src: &[u8]) -> R<Expr> {
    use super::lexer::Lexer;
    let mut full = Vec::with_capacity(src.len() + 6);
    full.extend_from_slice(b"<?php ");
    full.extend_from_slice(src);
    let toks = Lexer::tokenize(&full).map_err(|e| ParseError { msg: e.msg, pos: e.pos })?;
    let mut p = Parser::new(toks);
    // skip the synthetic open tag
    if matches!(p.kind(), Kind::OpenTag) {
        p.bump();
    }
    p.expr()
}
