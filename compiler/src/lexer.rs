//! Tokenizer. Newlines are not tokens; each token records whether a newline
//! preceded it so the parser can refuse to continue a postfix call/index
//! across lines (`a\n(b)` is two statements, not a call).

use crate::Diag;

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    Ident(String),
    Int(i64),
    Str(String),
    /// Duration literal in milliseconds.
    Dur(i64),
    Kw(&'static str),
    Sym(&'static str),
    Eof,
}

#[derive(Clone, Debug)]
pub struct Token {
    pub tok: Tok,
    pub line: u32,
    pub col: u32,
    pub nl_before: bool,
}

pub const KEYWORDS: &[&str] = &[
    "module", "import", "type", "enum", "fn", "task", "workflow", "extern", "requires", "budget",
    "let", "mut", "if", "else", "while", "for", "in", "return", "match", "parallel", "race",
    "timeout", "retry", "spawn", "await", "ensure", "true", "false", "assert", "break",
    "continue",
];

// Longest first so `==` wins over `=`.
const SYMBOLS: &[&str] = &[
    "->", "=>", "==", "!=", "<=", ">=", "&&", "||", "(", ")", "{", "}", "[", "]", "<", ">", ",",
    ":", ";", ".", "=", "+", "-", "*", "/", "%", "!", "?",
];

pub fn lex(src: &str, file: &str) -> Result<Vec<Token>, Diag> {
    let chars: Vec<char> = src.chars().collect();
    let mut toks = Vec::new();
    let (mut i, mut line, mut col) = (0usize, 1u32, 1u32);
    let mut nl = true;
    let err = |line, col, msg: String| Diag { file: file.to_string(), line, col, msg };

    while i < chars.len() {
        let c = chars[i];
        if c == '\n' {
            i += 1;
            line += 1;
            col = 1;
            nl = true;
            continue;
        }
        if c.is_whitespace() {
            i += 1;
            col += 1;
            continue;
        }
        if c == '/' && chars.get(i + 1) == Some(&'/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        let (sl, sc) = (line, col);
        let tok = if c.is_ascii_digit() {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '_') {
                i += 1;
            }
            let digits: String = chars[start..i].iter().filter(|c| **c != '_').collect();
            let n: i64 = digits
                .parse()
                .map_err(|_| err(sl, sc, format!("integer literal out of range: {digits}")))?;
            // A unit suffix glued to the number makes it a duration.
            let us = i;
            while i < chars.len() && chars[i].is_ascii_alphabetic() {
                i += 1;
            }
            let unit: String = chars[us..i].iter().collect();
            let scale = match unit.as_str() {
                "" => None,
                "ms" => Some(1),
                "s" => Some(1_000),
                "min" => Some(60_000),
                "h" => Some(3_600_000),
                u => return Err(err(sl, sc, format!("unknown duration unit `{u}` (use ms, s, min, h)"))),
            };
            let t = match scale {
                None => Tok::Int(n),
                Some(k) => Tok::Dur(n.checked_mul(k).ok_or_else(|| err(sl, sc, format!("duration literal out of range: {digits}{unit}")))?),
            };
            col += (i - start) as u32;
            t
        } else if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            col += (i - start) as u32;
            match KEYWORDS.iter().find(|k| **k == word) {
                Some(k) => Tok::Kw(k),
                None => Tok::Ident(word),
            }
        } else if c == '"' {
            i += 1;
            col += 1;
            let mut s = String::new();
            loop {
                match chars.get(i) {
                    None | Some('\n') => return Err(err(sl, sc, "unterminated string".into())),
                    Some('"') => {
                        i += 1;
                        col += 1;
                        break;
                    }
                    Some('\\') => {
                        let e = chars.get(i + 1).copied();
                        s.push(match e {
                            Some('n') => '\n',
                            Some('t') => '\t',
                            Some('"') => '"',
                            Some('\\') => '\\',
                            _ => return Err(err(line, col, "unknown escape".into())),
                        });
                        i += 2;
                        col += 2;
                    }
                    Some(ch) => {
                        s.push(*ch);
                        i += 1;
                        col += 1;
                    }
                }
            }
            Tok::Str(s)
        } else {
            let rest: String = chars[i..chars.len().min(i + 2)].iter().collect();
            match SYMBOLS.iter().find(|s| rest.starts_with(**s)) {
                Some(s) => {
                    i += s.len();
                    col += s.len() as u32;
                    Tok::Sym(s)
                }
                None => return Err(err(sl, sc, format!("unexpected character `{c}`"))),
            }
        };
        toks.push(Token { tok, line: sl, col: sc, nl_before: nl });
        nl = false;
    }
    toks.push(Token { tok: Tok::Eof, line, col, nl_before: true });
    Ok(toks)
}
