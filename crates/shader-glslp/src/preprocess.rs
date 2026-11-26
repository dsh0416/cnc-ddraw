//! Pure-Rust GLSL preprocessor for RetroArch `.glsl` files.
//!
//! Handles `#include`, `#pragma parameter`, `#define` (object and function-like),
//! `#undef`, and `#if`/`#ifdef`/`#ifndef`/`#elif`/`#else`/`#endif` conditionals so that
//! vertex and fragment stages can be cleanly separated and macro-expanded before
//! GLSL 450 rewriting.
#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};
use std::path::Path;

const MAX_INCLUDE_DEPTH: usize = 16;
const MAX_MACRO_EXPANSION_STEPS: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShaderStage {
    Vertex,
    Fragment,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ShaderParameter {
    pub name: String,
    pub default: f32,
}

#[derive(Clone, Debug)]
pub struct PreprocessedSource {
    pub source: String,
    pub parameters: Vec<ShaderParameter>,
}

#[derive(Clone, Debug)]
enum MacroDef {
    Object(String),
    Function { params: Vec<String>, body: String },
}

#[derive(Clone, Copy, Debug)]
struct CondFrame {
    parent_active: bool,
    this_active: bool,
    any_taken: bool,
    seen_else: bool,
}

struct Preprocessor {
    macros: HashMap<String, MacroDef>,
    cond_stack: Vec<CondFrame>,
    parameters: Vec<ShaderParameter>,
    seen_params: HashSet<String>,
    output: String,
}

impl Preprocessor {
    fn new(stage: ShaderStage) -> Self {
        let mut macros = HashMap::new();
        macros.insert("__VERSION__".into(), MacroDef::Object("450".into()));
        macros.insert("PARAMETER_UNIFORM".into(), MacroDef::Object("1".into()));
        macros.insert(
            "GL_FRAGMENT_PRECISION_HIGH".into(),
            MacroDef::Object("1".into()),
        );
        match stage {
            ShaderStage::Vertex => {
                macros.insert("VERTEX".into(), MacroDef::Object("1".into()));
            }
            ShaderStage::Fragment => {
                macros.insert("FRAGMENT".into(), MacroDef::Object("1".into()));
            }
        }
        Self {
            macros,
            cond_stack: Vec::new(),
            parameters: Vec::new(),
            seen_params: HashSet::new(),
            output: String::new(),
        }
    }

    fn is_active(&self) -> bool {
        self.cond_stack.last().is_none_or(|f| f.this_active)
    }

    fn process_source(&mut self, raw: &str, base_dir: &Path, depth: usize) -> Result<(), String> {
        if depth > MAX_INCLUDE_DEPTH {
            return Err(format!(
                "exceeded maximum #include depth ({MAX_INCLUDE_DEPTH})"
            ));
        }
        let cleaned = strip_comments(&join_continuations(normalize_newlines(raw)));
        let mut active_chunk = String::new();
        for line in cleaned.lines() {
            let trimmed = line.trim_start();
            if let Some(after_hash) = trimmed.strip_prefix('#') {
                if !active_chunk.is_empty() {
                    let expanded = self.expand_line(&active_chunk);
                    self.output.push_str(&expanded);
                    active_chunk.clear();
                }
                self.handle_directive(after_hash.trim_start(), base_dir, depth)?;
                self.output.push('\n');
            } else if self.is_active() {
                active_chunk.push_str(line);
                active_chunk.push('\n');
            } else {
                if !active_chunk.is_empty() {
                    let expanded = self.expand_line(&active_chunk);
                    self.output.push_str(&expanded);
                    active_chunk.clear();
                }
                self.output.push('\n');
            }
        }
        if !active_chunk.is_empty() {
            let expanded = self.expand_line(&active_chunk);
            self.output.push_str(&expanded);
        }
        Ok(())
    }

    fn handle_directive(
        &mut self,
        rest: &str,
        base_dir: &Path,
        depth: usize,
    ) -> Result<(), String> {
        let (dir, arg) = split_ident(rest);
        match dir {
            "ifdef" => {
                let name = arg.split_whitespace().next().unwrap_or("");
                let parent = self.is_active();
                let cond = parent && self.macros.contains_key(name);
                self.cond_stack.push(CondFrame {
                    parent_active: parent,
                    this_active: cond,
                    any_taken: cond,
                    seen_else: false,
                });
            }
            "ifndef" => {
                let name = arg.split_whitespace().next().unwrap_or("");
                let parent = self.is_active();
                let cond = parent && !self.macros.contains_key(name);
                self.cond_stack.push(CondFrame {
                    parent_active: parent,
                    this_active: cond,
                    any_taken: cond,
                    seen_else: false,
                });
            }
            "if" => {
                let parent = self.is_active();
                let cond = parent && self.eval_if_expr(arg);
                self.cond_stack.push(CondFrame {
                    parent_active: parent,
                    this_active: cond,
                    any_taken: cond,
                    seen_else: false,
                });
            }
            "elif" => {
                let top = self
                    .cond_stack
                    .last()
                    .copied()
                    .ok_or("unexpected #elif without matching #if")?;
                if top.seen_else {
                    return Err("unexpected #elif after #else".into());
                }
                let cond = if top.parent_active && !top.any_taken {
                    self.eval_if_expr(arg)
                } else {
                    false
                };
                if let Some(frame) = self.cond_stack.last_mut() {
                    frame.this_active = cond;
                    frame.any_taken |= cond;
                }
            }
            "else" => {
                let frame = self
                    .cond_stack
                    .last_mut()
                    .ok_or("unexpected #else without matching #if")?;
                if frame.seen_else {
                    return Err("duplicate #else in conditional block".into());
                }
                frame.seen_else = true;
                frame.this_active = frame.parent_active && !frame.any_taken;
                frame.any_taken = true;
            }
            "endif" => {
                if self.cond_stack.pop().is_none() {
                    return Err("unexpected #endif without matching #if".into());
                }
            }
            "pragma" => {
                // RetroArch `#pragma parameter` is global metadata; collect it whenever active.
                if self.is_active() {
                    if let Some(param) = parse_pragma_parameter(arg.trim()) {
                        if self.seen_params.insert(param.name.clone()) {
                            self.parameters.push(param);
                        }
                    }
                }
            }
            "define" => {
                if self.is_active() {
                    self.parse_define(arg.trim_start());
                }
            }
            "undef" if self.is_active() => {
                let name = arg.split_whitespace().next().unwrap_or("");
                if !name.is_empty() {
                    self.macros.remove(name);
                }
            }
            "include" if self.is_active() => {
                let trimmed = arg.trim();
                let rel = trimmed
                    .strip_prefix('"')
                    .and_then(|s| s.strip_suffix('"'))
                    .or_else(|| trimmed.strip_prefix('<').and_then(|s| s.strip_suffix('>')))
                    .ok_or_else(|| format!("malformed #include directive: '{trimmed}'"))?;
                let normalized = rel.replace('\\', "/");
                let inc_path = base_dir.join(normalized);
                let inc_text = std::fs::read_to_string(&inc_path)
                    .map_err(|err| format!("reading #include {}: {err}", inc_path.display()))?;
                let inc_dir = inc_path.parent().unwrap_or(base_dir);
                self.process_source(&inc_text, inc_dir, depth + 1)?;
            }
            // Strip #version, #extension, #line, empty `#`, and unknown directives.
            _ => {}
        }
        Ok(())
    }

    fn parse_define(&mut self, rest: &str) {
        let (name, after_name) = split_ident(rest);
        if name.is_empty() {
            return;
        }
        // Function-like macro requires '(' immediately after the name with no whitespace.
        if let Some(after_lparen) = after_name.strip_prefix('(') {
            if let Some(rparen) = after_lparen.find(')') {
                let param_list = &after_lparen[..rparen];
                let params: Vec<String> = param_list
                    .split(',')
                    .map(|p| p.trim().to_string())
                    .filter(|p| !p.is_empty())
                    .collect();
                let body = after_lparen[rparen + 1..].trim().to_string();
                self.macros
                    .insert(name.to_string(), MacroDef::Function { params, body });
                return;
            }
        }
        let body = after_name.trim().to_string();
        self.macros.insert(name.to_string(), MacroDef::Object(body));
    }

    fn expand_line(&self, line: &str) -> String {
        let mut hidden = HashSet::new();
        let mut budget = MAX_MACRO_EXPANSION_STEPS;
        self.expand_text(line, &mut hidden, &mut budget)
    }

    fn expand_text(&self, input: &str, hidden: &mut HashSet<String>, budget: &mut usize) -> String {
        let bytes = input.as_bytes();
        let mut out = String::with_capacity(input.len());
        let mut i = 0;

        while i < bytes.len() {
            let b = bytes[i];
            if is_ident_start(b) {
                let start = i;
                i += 1;
                while i < bytes.len() && is_ident_cont(bytes[i]) {
                    i += 1;
                }
                let ident = &input[start..i];
                if *budget > 0 && !hidden.contains(ident) {
                    if let Some(def) = self.macros.get(ident) {
                        match def {
                            MacroDef::Object(body) => {
                                *budget -= 1;
                                hidden.insert(ident.to_string());
                                let expanded = self.expand_text(body, hidden, budget);
                                hidden.remove(ident);
                                out.push_str(&expanded);
                                continue;
                            }
                            MacroDef::Function { params, body } => {
                                let mut j = i;
                                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                                    j += 1;
                                }
                                if j < bytes.len() && bytes[j] == b'(' {
                                    if let Some((raw_args, end_idx)) =
                                        parse_macro_call_args(input, j)
                                    {
                                        if raw_args.len() == params.len()
                                            || (params.is_empty()
                                                && raw_args.len() == 1
                                                && raw_args[0].trim().is_empty())
                                        {
                                            *budget -= 1;
                                            let expanded_args: Vec<String> = raw_args
                                                .iter()
                                                .map(|a| self.expand_text(a.trim(), hidden, budget))
                                                .collect();
                                            let substituted =
                                                substitute_fn_macro(body, params, &expanded_args);
                                            hidden.insert(ident.to_string());
                                            let expanded =
                                                self.expand_text(&substituted, hidden, budget);
                                            hidden.remove(ident);
                                            out.push_str(&expanded);
                                            i = end_idx;
                                            continue;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                out.push_str(ident);
            } else {
                let ch = input[i..].chars().next().unwrap();
                out.push(ch);
                i += ch.len_utf8();
            }
        }
        out
    }

    fn eval_if_expr(&self, expr: &str) -> bool {
        // First replace `defined(X)` and `defined X` before macro expansion.
        let with_defined = resolve_defined_ops(expr, &self.macros);
        let expanded = self.expand_line(&with_defined);
        let tokens = tokenize_if_expr(&expanded);
        let mut parser = IfExprParser { tokens, pos: 0 };
        parser.parse_ternary() != 0
    }
}

pub fn preprocess_glsl(
    raw: &str,
    base_dir: &Path,
    stage: ShaderStage,
) -> Result<PreprocessedSource, String> {
    let mut pp = Preprocessor::new(stage);
    pp.process_source(raw, base_dir, 0)?;
    if !pp.cond_stack.is_empty() {
        return Err("unterminated #if conditional block in shader".into());
    }
    Ok(PreprocessedSource {
        source: pp.output,
        parameters: pp.parameters,
    })
}

fn normalize_newlines(raw: &str) -> String {
    let stripped = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    stripped.replace("\r\n", "\n").replace('\r', "\n")
}

fn join_continuations(src: String) -> String {
    let mut out = String::with_capacity(src.len());
    let mut chars = src.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.peek() == Some(&'\n') {
            chars.next();
            continue;
        }
        out.push(c);
    }
    out
}

fn strip_comments(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    let mut in_string = false;

    while i < bytes.len() {
        if in_string {
            let ch = src[i..].chars().next().unwrap();
            out.push(ch);
            if ch == '\\' && i + 1 < bytes.len() {
                i += 1;
                let esc = src[i..].chars().next().unwrap();
                out.push(esc);
                i += esc.len_utf8();
                continue;
            }
            if ch == '"' {
                in_string = false;
            }
            i += ch.len_utf8();
            continue;
        }
        if bytes[i] == b'"' {
            in_string = true;
            out.push('"');
            i += 1;
            continue;
        }
        if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                if bytes[i] == b'\n' {
                    out.push('\n');
                }
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            out.push(' ');
            continue;
        }
        let ch = src[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn split_ident(s: &str) -> (&str, &str) {
    let bytes = s.as_bytes();
    if bytes.is_empty() || !is_ident_start(bytes[0]) {
        return ("", s);
    }
    let mut end = 1;
    while end < bytes.len() && is_ident_cont(bytes[end]) {
        end += 1;
    }
    (&s[..end], &s[end..])
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident_cont(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn parse_pragma_parameter(arg: &str) -> Option<ShaderParameter> {
    let rest = arg.strip_prefix("parameter")?.trim_start();
    let (name, after_name) = split_ident(rest);
    if name.is_empty() {
        return None;
    }
    let after_desc = after_name.trim_start();
    let nums_part = if let Some(quoted) = after_desc.strip_prefix('"') {
        let close = quoted.find('"')?;
        &quoted[close + 1..]
    } else {
        // Fallback when description isn't quoted.
        after_desc.split_once(char::is_whitespace)?.1
    };
    let default_token = nums_part.split_whitespace().next()?;
    let default = default_token
        .trim_end_matches(['f', 'F'])
        .parse::<f32>()
        .ok()?;
    Some(ShaderParameter {
        name: name.to_string(),
        default,
    })
}

fn parse_macro_call_args(src: &str, lparen: usize) -> Option<(Vec<String>, usize)> {
    let bytes = src.as_bytes();
    if bytes.get(lparen) != Some(&b'(') {
        return None;
    }
    let mut depth = 1usize;
    let mut args = Vec::new();
    let mut arg_start = lparen + 1;
    let mut i = lparen + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    args.push(src[arg_start..i].to_string());
                    return Some((args, i + 1));
                }
            }
            b',' if depth == 1 => {
                args.push(src[arg_start..i].to_string());
                arg_start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn substitute_fn_macro(body: &str, params: &[String], args: &[String]) -> String {
    let bytes = body.as_bytes();
    let mut out = String::with_capacity(body.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'#' && i + 1 < bytes.len() && bytes[i + 1] == b'#' {
            // Token pasting: drop surrounding spaces.
            while out.ends_with([' ', '\t']) {
                out.pop();
            }
            i += 2;
            while i < bytes.len() && matches!(bytes[i], b' ' | b'\t') {
                i += 1;
            }
            continue;
        }
        if is_ident_start(bytes[i]) {
            let start = i;
            i += 1;
            while i < bytes.len() && is_ident_cont(bytes[i]) {
                i += 1;
            }
            let ident = &body[start..i];
            if let Some(idx) = params.iter().position(|p| p == ident) {
                if let Some(arg) = args.get(idx) {
                    out.push_str(arg);
                    continue;
                }
            }
            out.push_str(ident);
        } else {
            let ch = body[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

fn resolve_defined_ops(expr: &str, macros: &HashMap<String, MacroDef>) -> String {
    let bytes = expr.as_bytes();
    let mut out = String::with_capacity(expr.len());
    let mut i = 0;
    while i < bytes.len() {
        if is_ident_start(bytes[i]) {
            let start = i;
            i += 1;
            while i < bytes.len() && is_ident_cont(bytes[i]) {
                i += 1;
            }
            let ident = &expr[start..i];
            if ident == "defined" {
                let mut j = i;
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                let has_paren = j < bytes.len() && bytes[j] == b'(';
                if has_paren {
                    j += 1;
                    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                        j += 1;
                    }
                }
                let name_start = j;
                while j < bytes.len() && is_ident_cont(bytes[j]) {
                    j += 1;
                }
                let name = &expr[name_start..j];
                if has_paren {
                    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    if j < bytes.len() && bytes[j] == b')' {
                        j += 1;
                    }
                }
                out.push(if macros.contains_key(name) { '1' } else { '0' });
                i = j;
                continue;
            }
            out.push_str(ident);
        } else {
            let ch = expr[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum IfTok {
    Num(i64),
    LParen,
    RParen,
    Question,
    Colon,
    OrOr,
    AndAnd,
    BitOr,
    BitXor,
    BitAnd,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Shl,
    Shr,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Bang,
    Tilde,
}

fn tokenize_if_expr(s: &str) -> Vec<IfTok> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if b[i].is_ascii_digit() {
            let start = i;
            if b[i] == b'0' && i + 1 < b.len() && matches!(b[i + 1], b'x' | b'X') {
                i += 2;
                while i < b.len() && b[i].is_ascii_hexdigit() {
                    i += 1;
                }
                let val = i64::from_str_radix(&s[start + 2..i], 16).unwrap_or(0);
                while i < b.len() && matches!(b[i], b'u' | b'U' | b'l' | b'L') {
                    i += 1;
                }
                out.push(IfTok::Num(val));
            } else {
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
                let val = s[start..i].parse::<i64>().unwrap_or(0);
                while i < b.len() && matches!(b[i], b'u' | b'U' | b'l' | b'L') {
                    i += 1;
                }
                out.push(IfTok::Num(val));
            }
            continue;
        }
        if is_ident_start(b[i]) {
            while i < b.len() && is_ident_cont(b[i]) {
                i += 1;
            }
            // Per GLSL/C preprocessor rules, undefined identifiers in `#if` evaluate to 0.
            out.push(IfTok::Num(0));
            continue;
        }
        let two = if i + 1 < b.len() {
            Some((b[i], b[i + 1]))
        } else {
            None
        };
        let tok = match two {
            Some((b'|', b'|')) => {
                i += 2;
                IfTok::OrOr
            }
            Some((b'&', b'&')) => {
                i += 2;
                IfTok::AndAnd
            }
            Some((b'=', b'=')) => {
                i += 2;
                IfTok::Eq
            }
            Some((b'!', b'=')) => {
                i += 2;
                IfTok::Ne
            }
            Some((b'<', b'=')) => {
                i += 2;
                IfTok::Le
            }
            Some((b'>', b'=')) => {
                i += 2;
                IfTok::Ge
            }
            Some((b'<', b'<')) => {
                i += 2;
                IfTok::Shl
            }
            Some((b'>', b'>')) => {
                i += 2;
                IfTok::Shr
            }
            _ => {
                let c = b[i];
                i += 1;
                match c {
                    b'(' => IfTok::LParen,
                    b')' => IfTok::RParen,
                    b'?' => IfTok::Question,
                    b':' => IfTok::Colon,
                    b'|' => IfTok::BitOr,
                    b'^' => IfTok::BitXor,
                    b'&' => IfTok::BitAnd,
                    b'<' => IfTok::Lt,
                    b'>' => IfTok::Gt,
                    b'+' => IfTok::Plus,
                    b'-' => IfTok::Minus,
                    b'*' => IfTok::Star,
                    b'/' => IfTok::Slash,
                    b'%' => IfTok::Percent,
                    b'!' => IfTok::Bang,
                    b'~' => IfTok::Tilde,
                    _ => continue,
                }
            }
        };
        out.push(tok);
    }
    out
}

struct IfExprParser {
    tokens: Vec<IfTok>,
    pos: usize,
}

impl IfExprParser {
    fn peek(&self) -> Option<&IfTok> {
        self.tokens.get(self.pos)
    }

    fn next(&mut self) -> Option<IfTok> {
        let tok = self.tokens.get(self.pos).cloned();
        if tok.is_some() {
            self.pos += 1;
        }
        tok
    }

    fn parse_ternary(&mut self) -> i64 {
        let cond = self.parse_or();
        if self.peek() == Some(&IfTok::Question) {
            self.next();
            let then_v = self.parse_ternary();
            let else_v = if self.peek() == Some(&IfTok::Colon) {
                self.next();
                self.parse_ternary()
            } else {
                0
            };
            if cond != 0 {
                then_v
            } else {
                else_v
            }
        } else {
            cond
        }
    }

    fn parse_or(&mut self) -> i64 {
        let mut lhs = self.parse_and();
        while self.peek() == Some(&IfTok::OrOr) {
            self.next();
            let rhs = self.parse_and();
            lhs = i64::from(lhs != 0 || rhs != 0);
        }
        lhs
    }

    fn parse_and(&mut self) -> i64 {
        let mut lhs = self.parse_bitor();
        while self.peek() == Some(&IfTok::AndAnd) {
            self.next();
            let rhs = self.parse_bitor();
            lhs = i64::from(lhs != 0 && rhs != 0);
        }
        lhs
    }

    fn parse_bitor(&mut self) -> i64 {
        let mut lhs = self.parse_bitxor();
        while self.peek() == Some(&IfTok::BitOr) {
            self.next();
            lhs |= self.parse_bitxor();
        }
        lhs
    }

    fn parse_bitxor(&mut self) -> i64 {
        let mut lhs = self.parse_bitand();
        while self.peek() == Some(&IfTok::BitXor) {
            self.next();
            lhs ^= self.parse_bitand();
        }
        lhs
    }

    fn parse_bitand(&mut self) -> i64 {
        let mut lhs = self.parse_eq();
        while self.peek() == Some(&IfTok::BitAnd) {
            self.next();
            lhs &= self.parse_eq();
        }
        lhs
    }

    fn parse_eq(&mut self) -> i64 {
        let mut lhs = self.parse_rel();
        loop {
            match self.peek() {
                Some(IfTok::Eq) => {
                    self.next();
                    let rhs = self.parse_rel();
                    lhs = i64::from(lhs == rhs);
                }
                Some(IfTok::Ne) => {
                    self.next();
                    let rhs = self.parse_rel();
                    lhs = i64::from(lhs != rhs);
                }
                _ => break,
            }
        }
        lhs
    }

    fn parse_rel(&mut self) -> i64 {
        let mut lhs = self.parse_shift();
        loop {
            match self.peek() {
                Some(IfTok::Lt) => {
                    self.next();
                    let rhs = self.parse_shift();
                    lhs = i64::from(lhs < rhs);
                }
                Some(IfTok::Le) => {
                    self.next();
                    let rhs = self.parse_shift();
                    lhs = i64::from(lhs <= rhs);
                }
                Some(IfTok::Gt) => {
                    self.next();
                    let rhs = self.parse_shift();
                    lhs = i64::from(lhs > rhs);
                }
                Some(IfTok::Ge) => {
                    self.next();
                    let rhs = self.parse_shift();
                    lhs = i64::from(lhs >= rhs);
                }
                _ => break,
            }
        }
        lhs
    }

    fn parse_shift(&mut self) -> i64 {
        let mut lhs = self.parse_add();
        loop {
            match self.peek() {
                Some(IfTok::Shl) => {
                    self.next();
                    let rhs = self.parse_add();
                    lhs = lhs.wrapping_shl((rhs as u32) & 63);
                }
                Some(IfTok::Shr) => {
                    self.next();
                    let rhs = self.parse_add();
                    lhs = lhs.wrapping_shr((rhs as u32) & 63);
                }
                _ => break,
            }
        }
        lhs
    }

    fn parse_add(&mut self) -> i64 {
        let mut lhs = self.parse_mul();
        loop {
            match self.peek() {
                Some(IfTok::Plus) => {
                    self.next();
                    lhs = lhs.wrapping_add(self.parse_mul());
                }
                Some(IfTok::Minus) => {
                    self.next();
                    lhs = lhs.wrapping_sub(self.parse_mul());
                }
                _ => break,
            }
        }
        lhs
    }

    fn parse_mul(&mut self) -> i64 {
        let mut lhs = self.parse_unary();
        loop {
            match self.peek() {
                Some(IfTok::Star) => {
                    self.next();
                    lhs = lhs.wrapping_mul(self.parse_unary());
                }
                Some(IfTok::Slash) => {
                    self.next();
                    let rhs = self.parse_unary();
                    lhs = if rhs != 0 { lhs.wrapping_div(rhs) } else { 0 };
                }
                Some(IfTok::Percent) => {
                    self.next();
                    let rhs = self.parse_unary();
                    lhs = if rhs != 0 { lhs.wrapping_rem(rhs) } else { 0 };
                }
                _ => break,
            }
        }
        lhs
    }

    fn parse_unary(&mut self) -> i64 {
        match self.peek() {
            Some(IfTok::Plus) => {
                self.next();
                self.parse_unary()
            }
            Some(IfTok::Minus) => {
                self.next();
                self.parse_unary().wrapping_neg()
            }
            Some(IfTok::Bang) => {
                self.next();
                i64::from(self.parse_unary() == 0)
            }
            Some(IfTok::Tilde) => {
                self.next();
                !self.parse_unary()
            }
            _ => self.parse_primary(),
        }
    }

    fn parse_primary(&mut self) -> i64 {
        match self.next() {
            Some(IfTok::Num(n)) => n,
            Some(IfTok::LParen) => {
                let v = self.parse_ternary();
                if self.peek() == Some(&IfTok::RParen) {
                    self.next();
                }
                v
            }
            _ => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_stages_expands_macros_and_extracts_parameters() {
        let src = r#"
#pragma parameter GAIN "Brightness Gain" 1.25 0.5 2.0 0.05
#if __VERSION__ >= 130
#define STAGE_TEX(t, c) texture((t), (c))
#else
#define STAGE_TEX(t, c) texture2D((t), (c))
#endif

#if defined(VERTEX)
in vec4 VertexCoord;
void main() { gl_Position = VertexCoord; }
#elif defined(FRAGMENT)
uniform sampler2D Texture;
uniform float GAIN;
out vec4 FragColor;
void main() { FragColor = STAGE_TEX(Texture, vec2(0.5)) * GAIN; }
#endif
"#;
        let vs = preprocess_glsl(src, Path::new("."), ShaderStage::Vertex).unwrap();
        assert_eq!(vs.parameters.len(), 1);
        assert_eq!(vs.parameters[0].name, "GAIN");
        assert!((vs.parameters[0].default - 1.25).abs() < 1e-6);
        assert!(vs.source.contains("gl_Position = VertexCoord"));
        assert!(!vs.source.contains("FragColor"));

        let fs = preprocess_glsl(src, Path::new("."), ShaderStage::Fragment).unwrap();
        assert!(fs.source.contains("texture((Texture), (vec2(0.5)))"));
        assert!(!fs.source.contains("gl_Position"));
    }
}
