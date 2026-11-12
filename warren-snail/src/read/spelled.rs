//! A statement as PostgreSQL's grammar and analyser read it, where sqlparser reads its words
//! otherwise. Before sqlparser parses it: SQL's `SUBSTRING(s SIMILAR p ESCAPE e)` and
//! `SUBSTRING(s FOR e FROM p)` written in the words of the one call PostgreSQL's grammar makes of
//! each, and `BETWEEN SYMMETRIC x` and `BETWEEN ASYMMETRIC x` with their bound in parentheses, which
//! sqlparser reads as a call of the keyword; and a window frame's EXCLUDE, which sqlparser does not
//! read, carried in the window's name (`EXCLUDED`). After: each BETWEEN of a pipeline's statement
//! written as the comparisons PostgreSQL's analyser makes of it.

use crate::{Error, Result};
use sqlparser::ast::{
    AccessExpr, BinaryOperator, Expr, FunctionArg, FunctionArgExpr, FunctionArgumentClause,
    FunctionArguments, GroupByExpr, JoinConstraint, JoinOperator, OrderByKind, Query, SelectItem,
    SetExpr, Subscript, TableFactor, UnaryOperator, WindowType,
};
use sqlparser::keywords::Keyword;
use sqlparser::tokenizer::{Location, Token, TokenWithSpan, Tokenizer};
use std::borrow::Cow;

/// The statement with each rewrite made, one at a time until none is left.
pub(crate) fn spelled(sql: &str) -> Result<Cow<'_, str>> {
    at_at_spaced(sql)?;
    let lower = sql.to_ascii_lowercase();
    let mut out = Cow::Borrowed(sql);
    if lower.contains("exclude") {
        while let Some(next) = excluded_once(&out)? {
            out = Cow::Owned(next);
        }
    }
    if lower.contains("substring") || lower.contains("symmetric") {
        while let Some(next) = rewrite_once(&out)? {
            out = Cow::Owned(next);
        }
    }
    if let Some(next) = crate::grammar::bound(&out) {
        out = Cow::Owned(next);
    }
    Ok(out)
}


/// What marks a function name as a TABLESAMPLE clause's, its method and whether it has a REPEATABLE:
/// this character, `r` or `n`, this character again, the method as named. A REPEATABLE's seed is the
/// call's last argument.
pub(crate) const SAMPLED: char = '\u{2}';

/// What marks ROWS FROM's call (`ROWS_FROM`), and an alias that names nothing, standing where a
/// function in FROM takes a column definition list with no alias before it (`AS (…)`).
pub(crate) const ROWS_MARK: char = '\u{6}';

/// The name ROWS FROM (…) is called by: its functions are its arguments, a function's column
/// definition list named arguments after it, each a column's name and a NULL of its type.
pub(crate) const ROWS_FROM: &str = "\u{6}rows";

/// A column definition list's column written SETOF or with a COLLATE, as its name carries them:
/// the name, `ROWS_MARK`, `s` where SETOF, `ROWS_MARK` again, and the collation as named.
fn marked_column(name: &str, setof: bool, collation: Option<&str>) -> String {
    let s = if setof { "s" } else { "" };
    format!("{name}{ROWS_MARK}{s}{ROWS_MARK}{}", collation.unwrap_or_default())
}

/// A column definition list's column by the name `marked_column` writes: its own name, whether it
/// is written SETOF, and the collation it names.
pub(crate) fn column_marks(written: &str) -> (&str, bool, Option<&str>) {
    let Some((name, rest)) = written.split_once(ROWS_MARK) else {
        return (written, false, None);
    };
    let (setof, collation) = rest.split_once(ROWS_MARK).unwrap_or((rest, ""));
    (name, setof == "s", Some(collation).filter(|c| !c.is_empty()))
}

/// The statement as PostgreSQL's scanner and grammar read its words, where sqlparser's tokenizer
/// reads them otherwise, under `release`: a number in its bases and with its underscores from 16
/// on, a name written against it refused as trailing junk from 15 on; strings a newline apart as one
/// string; `x ISNULL` and `x NOTNULL`; an interval's literal as the cast its grammar makes of it, its
/// precision or its fields, and the fields of its type, refused where its grammar refuses them; a
/// TABLESAMPLE clause carried in a call's name (`SAMPLED`), with a seed drawn for the statement where
/// it names none; ROWS FROM (…) as a call (`ROWS_FROM`), the alias a function's column definition
/// list stands behind where none is written, and a column of such a list written SETOF or with a
/// COLLATE by its name (`marked_column`); and what sqlparser takes and PostgreSQL's grammar does not,
/// refused, a label written without the AS the release's keywords want among them.
pub(crate) fn scanned(sql: &str, release: u8) -> Result<Cow<'_, str>> {
    let out = scanned_seeding(sql, release, true, true, true)?;
    bare_labels(&out, release)?;
    Ok(out)
}

/// A DDL's text as `scanned` reads a statement's, a TABLESAMPLE that names no seed left to draw one
/// each time a statement reads it: a view's. ROWS FROM and a column definition list, with no alias
/// or with SETOF or a COLLATE, are left as written, so that statements split from a text read
/// again as PostgreSQL's; its labels are a statement's to read.
pub(crate) fn scanned_unseeded(sql: &str, release: u8) -> Result<Cow<'_, str>> {
    scanned_seeding(sql, release, false, true, false)
}

/// A text as `scanned_unseeded` reads it to split it into its statements, an interval's literal
/// left as written too: each statement split from it is the one written, read as the cast its
/// grammar makes of the literal where the statement is read.
pub(crate) fn scanned_split(sql: &str, release: u8) -> Result<Cow<'_, str>> {
    scanned_seeding(sql, release, false, false, false)
}

/// A CREATE VIEW's text as `scanned_unseeded` reads a DDL's, its body's ROWS FROM and column
/// definition lists read as a statement's are.
pub(crate) fn scanned_view(sql: &str, release: u8) -> Result<Cow<'_, str>> {
    scanned_seeding(sql, release, false, true, true)
}

fn scanned_seeding(
    sql: &str,
    release: u8,
    seeded: bool,
    casts: bool,
    rows: bool,
) -> Result<Cow<'_, str>> {
    let mut out = numbered(sql, release)?;
    if let Some(next) = worded(&out, seeded, casts, rows, release)? {
        out = Cow::Owned(next);
    }
    Ok(out)
}

fn ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c >= 0x80
}

fn ident_cont(c: u8) -> bool {
    ident_start(c) || c.is_ascii_digit() || c == b'$'
}

/// Where a string's whitespace_with_newline ends, where a quote there goes on with the string: past
/// the closing quote at `at`.
fn continued(b: &[u8], at: usize) -> Option<usize> {
    let comment = |mut i: usize| {
        while i < b.len() && !matches!(b[i], b'\n' | b'\r') {
            i += 1;
        }
        i
    };
    let mut i = at;
    let mut newline = false;
    while i < b.len() {
        match b[i] {
            b'\n' | b'\r' => {
                newline = true;
                i += 1;
            }
            b' ' | b'\t' | 0x0c | 0x0b => i += 1,
            b'-' if b.get(i + 1) == Some(&b'-') => {
                i = comment(i);
                // A comment there is whitespace only before a newline.
                if i >= b.len() {
                    return None;
                }
            }
            b'\'' if newline => return Some(i),
            _ => return None,
        }
    }
    None
}

/// The end of `{decdigit}(_?{decdigit})*` from `i`, where one starts there.
fn decimal_end(b: &[u8], i: usize) -> Option<usize> {
    if !b.get(i)?.is_ascii_digit() {
        return None;
    }
    let mut j = i + 1;
    loop {
        match b.get(j) {
            Some(c) if c.is_ascii_digit() => j += 1,
            Some(b'_') if b.get(j + 1).is_some_and(u8::is_ascii_digit) => j += 2,
            _ => return Some(j),
        }
    }
}

/// The end of `{numeric}` from `i`, where one starts there.
fn numeric_end(b: &[u8], i: usize) -> Option<usize> {
    match decimal_end(b, i) {
        Some(d) if b.get(d) == Some(&b'.') => Some(decimal_end(b, d + 1).unwrap_or(d + 1)),
        Some(_) => None,
        None if b.get(i) == Some(&b'.') => decimal_end(b, i + 1),
        None => None,
    }
}

/// The end of the digits of a base's integer after its `0x`, `0o` or `0b`, each maybe after one
/// underscore; and the end of its prefix and that underscore, what a prefix alone reads as.
fn radix_ends(b: &[u8], i: usize, letter: u8, digit: fn(u8) -> bool) -> Option<(Option<usize>, usize)> {
    if b.get(i) != Some(&b'0') || !b.get(i + 1)?.eq_ignore_ascii_case(&letter) {
        return None;
    }
    let mut j = i + 2;
    let mut end = None;
    loop {
        match b.get(j) {
            Some(&c) if digit(c) => {
                j += 1;
                end = Some(j);
            }
            Some(b'_') if b.get(j + 1).is_some_and(|&c| digit(c)) => j += 1,
            _ => break,
        }
    }
    let fail = i + 2 + usize::from(b.get(i + 2) == Some(&b'_'));
    Some((end, fail))
}

/// The digits of an integer in a base, as a decimal.
fn in_decimal(digits: &[u8], base: u32) -> String {
    const LIMB: u64 = 1_000_000_000;
    let mut limbs: Vec<u64> = vec![0];
    for &c in digits.iter().filter(|c| **c != b'_') {
        let mut carry = u64::from((c as char).to_digit(base).unwrap_or(0));
        for l in &mut limbs {
            let v = *l * u64::from(base) + carry;
            *l = v % LIMB;
            carry = v / LIMB;
        }
        if carry > 0 {
            limbs.push(carry);
        }
    }
    let mut out = limbs.last().unwrap().to_string();
    for l in limbs.iter().rev().skip(1) {
        out.push_str(&format!("{l:09}"));
    }
    out
}

/// The number at `i`: where it ends, and what stands for it in the text.
fn number_at(b: &[u8], i: usize, text: &str) -> Result<(usize, Cow<'static, str>)> {
    let junk = |end: usize| {
        let mut j = end;
        if !b.get(j).copied().is_some_and(ident_start) {
            return None;
        }
        while b.get(j).copied().is_some_and(ident_cont) {
            j += 1;
        }
        Some(j)
    };
    let real = |from: Option<usize>| -> (Option<usize>, Option<usize>) {
        let Some(m) = from.filter(|&m| matches!(b.get(m), Some(b'e' | b'E'))) else {
            return (None, None);
        };
        let sign = usize::from(matches!(b.get(m + 1), Some(b'-' | b'+')));
        match decimal_end(b, m + 1 + sign) {
            Some(e) => (Some(e), None),
            None if sign == 1 => (None, Some(m + 2)),
            None => (None, None),
        }
    };
    let integer = decimal_end(b, i);
    let numeric = numeric_end(b, i);
    let (real_i, fail_i) = real(integer);
    let (real_n, fail_n) = real(numeric);
    let real_end = real_i.or(real_n);
    let hex = radix_ends(b, i, b'x', |c| c.is_ascii_hexdigit());
    let oct = radix_ends(b, i, b'o', |c| matches!(c, b'0'..=b'7'));
    let bin = radix_ends(b, i, b'b', |c| matches!(c, b'0' | b'1'));
    let numeric_fail = integer.filter(|&d| b.get(d) == Some(&b'.') && b.get(d + 1) == Some(&b'.'));
    // The rules of scan.l in their order: the longest match, the first of those.
    #[derive(Clone, Copy)]
    enum Rule {
        Plain,
        Radix(usize, u32),
        Failed(&'static str),
        Integer,
        Junk,
    }
    let mut rules: Vec<(Option<usize>, Rule)> = vec![
        (integer, Rule::Plain),
        (hex.and_then(|h| h.0), Rule::Radix(2, 16)),
        (oct.and_then(|h| h.0), Rule::Radix(2, 8)),
        (bin.and_then(|h| h.0), Rule::Radix(2, 2)),
        (hex.map(|h| h.1), Rule::Failed("hexadecimal")),
        (oct.map(|h| h.1), Rule::Failed("octal")),
        (bin.map(|h| h.1), Rule::Failed("binary")),
        (numeric, Rule::Plain),
        (numeric_fail.map(|d| d + 2), Rule::Integer),
        (real_end, Rule::Plain),
        (fail_i.or(fail_n), Rule::Junk),
    ];
    rules.push((integer.and_then(junk), Rule::Junk));
    rules.push((numeric.and_then(junk), Rule::Junk));
    rules.push((real_end.and_then(junk), Rule::Junk));
    let (mut end, mut rule) = (i, Rule::Plain);
    for (e, r) in rules {
        if let Some(e) = e.filter(|&e| e > end) {
            (end, rule) = (e, r);
        }
    }
    let spelled = &text[i..end];
    match rule {
        Rule::Plain if spelled.contains('_') => Ok((end, spelled.replace('_', "").into())),
        Rule::Plain => Ok((end, spelled.to_string().into())),
        Rule::Radix(skip, base) => Ok((end, in_decimal(&b[i + skip..end], base).into())),
        Rule::Failed(kind) => Err(Error::new(format!(
            "invalid {kind} integer at or near \"{spelled}\""
        ))),
        Rule::Integer => {
            let d = integer.unwrap_or(i);
            Ok((d, text[i..d].replace('_', "").into()))
        }
        Rule::Junk => Err(Error::new(format!(
            "trailing junk after numeric literal at or near \"{spelled}\""
        ))),
    }
}

/// The number at `i` as PostgreSQL before 16 reads it: its digits alone, a name after it its own.
fn number_before_16(b: &[u8], i: usize) -> usize {
    let digits = |mut j: usize| {
        while b.get(j).is_some_and(u8::is_ascii_digit) {
            j += 1;
        }
        j
    };
    let mut end = digits(i);
    if b.get(end) == Some(&b'.') && !(b.get(end + 1) == Some(&b'.')) {
        end = digits(end + 1);
    }
    if matches!(b.get(end), Some(b'e' | b'E')) {
        let sign = usize::from(matches!(b.get(end + 1), Some(b'-' | b'+')));
        if b.get(end + 1 + sign).is_some_and(u8::is_ascii_digit) {
            end = digits(end + 1 + sign);
        }
    }
    end
}

/// The statement with its numbers and its strings as PostgreSQL's scanner reads them.
fn numbered(sql: &str, release: u8) -> Result<Cow<'_, str>> {
    let b = sql.as_bytes();
    let mut out = String::new();
    let mut copied = 0;
    let mut i = 0;
    let swap = |out: &mut String, from: usize, to: usize, with: &str, copied: &mut usize| {
        out.push_str(&sql[*copied..from]);
        out.push_str(with);
        *copied = to;
    };
    while i < b.len() {
        let c = b[i];
        match c {
            b'\'' => {
                // A string's prefix `E` makes a backslash escape what follows it.
                let escapes = i > 0
                    && matches!(b[i - 1], b'e' | b'E')
                    && !(i > 1 && ident_cont(b[i - 2]));
                let mut j = i + 1;
                loop {
                    match b.get(j) {
                        None => return Ok(finish(sql, out, copied)),
                        Some(b'\\') if escapes => j += 2,
                        Some(b'\'') if b.get(j + 1) == Some(&b'\'') => j += 2,
                        Some(b'\'') => match continued(b, j + 1) {
                            Some(next) => {
                                swap(&mut out, j, next + 1, "", &mut copied);
                                j = next + 1;
                            }
                            None => break,
                        },
                        Some(_) => j += 1,
                    }
                }
                i = j + 1;
            }
            b'"' => {
                let mut j = i + 1;
                loop {
                    match b.get(j) {
                        None => return Ok(finish(sql, out, copied)),
                        Some(b'"') if b.get(j + 1) == Some(&b'"') => j += 2,
                        Some(b'"') => break,
                        Some(_) => j += 1,
                    }
                }
                i = j + 1;
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && !matches!(b[i], b'\n' | b'\r') {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let mut depth = 1;
                i += 2;
                while i < b.len() && depth > 0 {
                    if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                        depth += 1;
                        i += 2;
                    } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
            }
            b'$' => {
                let mut j = i + 1;
                if b.get(j).is_some_and(u8::is_ascii_digit) {
                    while b.get(j).is_some_and(u8::is_ascii_digit) {
                        j += 1;
                    }
                    if b.get(j).copied().is_some_and(ident_start) {
                        let digits = j;
                        while b.get(j).copied().is_some_and(ident_cont) {
                            j += 1;
                        }
                        if release >= 16 {
                            return Err(Error::new(format!(
                                "trailing junk after parameter at or near \"{}\"",
                                &sql[i..j]
                            )));
                        }
                        swap(&mut out, digits, digits, " ", &mut copied);
                    }
                    i = j;
                } else {
                    while b.get(j).copied().is_some_and(|c| ident_cont(c) && c != b'$') {
                        j += 1;
                    }
                    if b.get(j) == Some(&b'$') {
                        let tag = &sql[i..=j];
                        match sql[j + 1..].find(tag) {
                            Some(at) => i = j + 1 + at + tag.len(),
                            None => return Ok(finish(sql, out, copied)),
                        }
                    } else {
                        i += 1;
                    }
                }
            }
            c if c.is_ascii_digit() || (c == b'.' && b.get(i + 1).is_some_and(u8::is_ascii_digit)) => {
                if release >= 16 {
                    let (end, with) = number_at(b, i, sql)?;
                    if with != sql[i..end] {
                        swap(&mut out, i, end, &with, &mut copied);
                    }
                    i = end;
                } else {
                    let end = number_before_16(b, i);
                    if b.get(end).copied().is_some_and(ident_start) {
                        // 15 refuses a name written against a number, 14 reads it as the next word.
                        if release == 15 {
                            let mut j = end;
                            while b.get(j).copied().is_some_and(ident_cont) {
                                j += 1;
                            }
                            return Err(Error::new(format!(
                                "trailing junk after numeric literal at or near \"{}\"",
                                &sql[i..j]
                            )));
                        }
                        swap(&mut out, end, end, " ", &mut copied);
                    }
                    i = end;
                }
            }
            c if ident_start(c) => {
                while b.get(i).copied().is_some_and(ident_cont) {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    Ok(finish(sql, out, copied))
}

/// A statement's text with what was swapped in it, and the rest after.
fn finish(sql: &str, mut out: String, copied: usize) -> Cow<'_, str> {
    if copied == 0 && out.is_empty() {
        return Cow::Borrowed(sql);
    }
    out.push_str(&sql[copied..]);
    Cow::Owned(out)
}

/// The statement with its words as PostgreSQL's grammar reads them; none where it needs none, or
/// where sqlparser's tokenizer refuses it, which its parser says. Without `casts` (a text split),
/// what is written for sqlparser alone is left as written: an interval's literal, its fields
/// refused where its grammar refuses them all the same, and the label before ON CONFLICT;
/// without `rows`, ROWS FROM and a column definition list are left as written.
fn worded(
    sql: &str,
    seeded: bool,
    casts: bool,
    rows: bool,
    release: u8,
) -> Result<Option<String>> {
    let lower = sql.to_ascii_lowercase();
    // `AS (`, whatever stands between the two.
    let as_paren = lower.match_indices("as").any(|(at, _)| {
        lower[at + 2..].trim_start().starts_with('(')
    });
    if !as_paren
        && ![
            "isnull", "notnull", "interval", "tablesample", "==", "[", "format", "*<", "*>", "rows",
            "setof", "collate", "conflict",
        ]
        .iter()
        .any(|w| lower.contains(w))
    {
        return Ok(None);
    }
    let Ok(tokens) = super::statement::spans(sql) else {
        return Ok(None);
    };
    let solid: Vec<&(Token, usize, usize)> = tokens
        .iter()
        .filter(|t| !matches!(t.0, Token::Whitespace(_)))
        .collect();
    let word = |i: usize| match solid.get(i).map(|t| &t.0) {
        Some(Token::Word(w)) if w.quote_style.is_none() => Some(w.value.to_ascii_uppercase()),
        _ => None,
    };
    let near = |i: usize| match solid.get(i).filter(|t| !matches!(t.0, Token::EOF)) {
        Some(t) => Error::new(format!("syntax error at or near \"{}\"", &sql[t.1..t.2])),
        None => Error::new("syntax error at end of input"),
    };
    let string = |i: usize| {
        matches!(
            solid.get(i).map(|t| &t.0),
            Some(
                Token::SingleQuotedString(_)
                    | Token::EscapedStringLiteral(_)
                    | Token::NationalStringLiteral(_)
                    | Token::UnicodeStringLiteral(_)
                    | Token::HexStringLiteral(_)
            )
        )
    };
    // Past a parenthesis or bracket opened at `i`, the place after the one that closes it.
    let closed = |i: usize, open: Token, close: Token| -> Option<usize> {
        let mut depth = 0i32;
        for (j, t) in solid.iter().enumerate().skip(i) {
            if t.0 == open {
                depth += 1;
            } else if t.0 == close {
                depth -= 1;
                if depth == 0 {
                    return Some(j + 1);
                }
            }
        }
        None
    };
    let token = |i: usize| solid.get(i).map(|t| &t.0);
    // One of the fields an interval's type or literal names after it.
    let field = |i: usize| {
        matches!(
            word(i).as_deref(),
            Some("YEAR" | "MONTH" | "DAY" | "HOUR" | "MINUTE" | "SECOND")
        )
    };
    // Past an interval's fields from `i`, as PostgreSQL's grammar reads them: one field, YEAR TO
    // MONTH, DAY, HOUR or MINUTE TO a later field, and SECOND with its precision or without;
    // refused at the word its grammar stops at.
    let fields_end = |i: usize| -> Result<usize> {
        let first = word(i).unwrap_or_default();
        let mut j = i + 1;
        let mut last = first.clone();
        if word(j).as_deref() == Some("TO") {
            let later: &[&str] = match first.as_str() {
                "YEAR" => &["MONTH"],
                "DAY" => &["HOUR", "MINUTE", "SECOND"],
                "HOUR" => &["MINUTE", "SECOND"],
                "MINUTE" => &["SECOND"],
                _ => return Err(near(j)),
            };
            last = word(j + 1)
                .filter(|w| later.contains(&w.as_str()))
                .ok_or_else(|| near(j + 1))?;
            j += 2;
        }
        if last == "SECOND" && token(j) == Some(&Token::LParen) {
            let integer =
                |n: &str| n.bytes().all(|b| b.is_ascii_digit()) && n.parse::<i32>().is_ok();
            if !matches!(token(j + 1), Some(Token::Number(n, _)) if integer(n)) {
                return Err(near(j + 1));
            }
            if token(j + 2) != Some(&Token::RParen) {
                return Err(near(j + 2));
            }
            j += 3;
        }
        if token(j) == Some(&Token::LParen) || word(j).as_deref() == Some("TO") || field(j) {
            return Err(near(j));
        }
        Ok(j)
    };
    // A word sqlparser reads as a unit after an interval's literal, where PostgreSQL's grammar
    // reads the literal without it and the word as what follows: a label in a select list.
    let unit_otherwise = |i: usize| {
        matches!(token(i), Some(Token::Word(w)) if w.quote_style.is_none() && matches!(
            w.keyword,
            Keyword::YEARS
                | Keyword::MONTHS
                | Keyword::WEEK
                | Keyword::WEEKS
                | Keyword::DAYS
                | Keyword::HOURS
                | Keyword::MINUTES
                | Keyword::SECONDS
                | Keyword::CENTURY
                | Keyword::DECADE
                | Keyword::DOW
                | Keyword::DOY
                | Keyword::EPOCH
                | Keyword::ISODOW
                | Keyword::ISOYEAR
                | Keyword::JULIAN
                | Keyword::MICROSECOND
                | Keyword::MICROSECONDS
                | Keyword::MILLENIUM
                | Keyword::MILLENNIUM
                | Keyword::MILLISECOND
                | Keyword::MILLISECONDS
                | Keyword::NANOSECOND
                | Keyword::NANOSECONDS
                | Keyword::QUARTER
                | Keyword::TIMEZONE
                | Keyword::TIMEZONE_HOUR
                | Keyword::TIMEZONE_MINUTE
        ))
    };
    // INTERVAL as SET TIME ZONE's value, which the setting reads off its own words.
    let zone_set = |i: usize| {
        word(i.wrapping_sub(1)).as_deref() == Some("ZONE")
            && word(i.wrapping_sub(2)).as_deref() == Some("TIME")
            && word(i.wrapping_sub(3)).as_deref() != Some("AT")
    };
    // Whether the parenthesis opened at `o` is a call's, `ROWS FROM (` among them.
    let called = |o: usize| {
        o > 0
            && match &solid[o - 1].0 {
                Token::Word(w) if w.quote_style.is_some() => true,
                Token::Word(w) => {
                    !matches!(keyword(&w.value, release), Some(('R', _)))
                        || (w.value.eq_ignore_ascii_case("from")
                            && word(o.wrapping_sub(2)).as_deref() == Some("ROWS"))
                }
                _ => false,
            }
    };
    // Whether the parenthesis closed last, at `c` and opened at `o`, is a call's that ends, with
    // its WITH ORDINALITY if any, just before `i`.
    let call_before = |i: usize, last: Option<(usize, usize)>| {
        last.is_some_and(|(c, o)| {
            let after_call = c + 1 == i
                || (c + 3 == i
                    && word(c + 1).as_deref() == Some("WITH")
                    && word(c + 2).as_deref() == Some("ORDINALITY"));
            after_call && called(o)
        })
    };
    let mut swaps: Vec<(usize, usize, String)> = Vec::new();
    let mut brackets = 0i32;
    let mut array_closes: Vec<i32> = Vec::new();
    // Whether each depth of parentheses stands in a FROM list, as the clauses met at it say; the
    // places of the parentheses open, and of the last one closed and where it opened.
    let mut in_from: Vec<bool> = vec![false];
    let mut opens: Vec<usize> = Vec::new();
    let mut last_closed: Option<(usize, usize)> = None;
    // Whether each depth of parentheses stands in a select list or a RETURNING list, where an item
    // takes a label; and the CASEs and BETWEENs open at it, inside which an item cannot end.
    let mut in_list: Vec<bool> = vec![false];
    let mut unended: Vec<(i32, i32)> = vec![(0, 0)];
    for i in 0..solid.len() {
        let prev_as = i > 0 && (word(i - 1).as_deref() == Some("AS") || solid[i - 1].0 == Token::Period);
        match &solid[i].0 {
            Token::LParen => {
                in_from.push(false);
                in_list.push(false);
                unended.push((0, 0));
                opens.push(i);
            }
            Token::RParen => {
                if in_from.len() > 1 {
                    in_from.pop();
                }
                if in_list.len() > 1 {
                    in_list.pop();
                    unended.pop();
                }
                if let Some(o) = opens.pop() {
                    last_closed = Some((i, o));
                }
            }
            _ => {}
        }
        let (cases, betweens) = unended.last_mut().unwrap();
        match word(i).as_deref() {
            Some("SELECT" | "RETURNING") if !prev_as => *in_list.last_mut().unwrap() = true,
            Some("FROM") if matches!(word(i.wrapping_sub(1)).as_deref(), Some("ROWS" | "DISTINCT")) => {}
            Some(
                "FROM" | "INTO" | "WHERE" | "GROUP" | "HAVING" | "WINDOW" | "ORDER" | "LIMIT"
                | "OFFSET" | "FETCH" | "UNION" | "INTERSECT" | "EXCEPT" | "VALUES" | "SET",
            ) if !prev_as => *in_list.last_mut().unwrap() = false,
            Some("CASE") if !prev_as => *cases += 1,
            Some("END") if !prev_as && *cases > 0 => *cases -= 1,
            Some("BETWEEN") if !prev_as => *betweens += 1,
            Some("AND") if *betweens > 0 => *betweens -= 1,
            _ => {}
        }
        let from_here = *in_from.last().unwrap_or(&false);
        match word(i).as_deref() {
            Some("FROM") if !prev_as && !matches!(word(i.wrapping_sub(1)).as_deref(), Some("ROWS" | "DISTINCT")) => {
                if let Some(f) = in_from.last_mut() {
                    *f = true;
                }
            }
            Some(
                "SELECT" | "WHERE" | "GROUP" | "HAVING" | "WINDOW" | "ORDER" | "LIMIT" | "OFFSET"
                | "FETCH" | "UNION" | "INTERSECT" | "EXCEPT" | "RETURNING" | "VALUES" | "SET",
            ) if !prev_as => {
                if let Some(f) = in_from.last_mut() {
                    *f = false;
                }
            }
            // ROWS FROM, where a FROM item stands.
            Some("ROWS")
                if rows
                    && word(i + 1).as_deref() == Some("FROM")
                    && matches!(solid.get(i + 2).map(|t| &t.0), Some(Token::LParen))
                    && i > 0
                    && (matches!(word(i - 1).as_deref(), Some("FROM" | "JOIN" | "LATERAL"))
                        || (solid[i - 1].0 == Token::Comma && from_here)
                        || (solid[i - 1].0 == Token::LParen
                            && in_from.len() > 1
                            && in_from[in_from.len() - 2])) =>
            {
                swaps.extend(rows_from(sql, &solid, i, &word, &near, &closed)?);
            }
            // A function's column definition list with no alias before it.
            Some("AS")
                if rows
                    && from_here
                    && matches!(solid.get(i + 1).map(|t| &t.0), Some(Token::LParen))
                    && call_before(i, last_closed) =>

            {
                swaps.push((solid[i].1, solid[i].2, format!("AS \"{ROWS_MARK}\"")));
            }
            _ => {}
        }
        // A function's column definition list, after its alias or after AS alone, holding SETOF
        // or a COLLATE: each such column written by its name marked with them (`marked_column`),
        // which sqlparser reads.
        if rows
            && solid[i].0 == Token::LParen
            && in_from.len() > 1
            && in_from[in_from.len() - 2]
        {
            let column_id = |j: usize| {
                matches!(&solid[j].0, Token::Word(w) if w.quote_style.is_some()
                    || !matches!(keyword(&w.value, release), Some(('R' | 'T', _))))
            };
            let before = match i.checked_sub(1) {
                Some(j) if word(j).as_deref() == Some("AS") => Some(j),
                Some(j) if column_id(j) => match j.checked_sub(1) {
                    Some(k) if word(k).as_deref() == Some("AS") => Some(k),
                    _ => Some(j),
                },
                _ => None,
            };
            if let (Some(b), Some(after)) = (before, closed(i, Token::LParen, Token::RParen)) {
                let marked = (i + 1..after - 1)
                    .any(|j| matches!(word(j).as_deref(), Some("SETOF" | "COLLATE")));
                if marked && call_before(b, last_closed) {
                    swaps.extend(defined_list(sql, &solid, (i + 1, after - 1), &word, &near)?);
                }
            }
        }
        match &solid[i].0 {
            // Touching an operator's characters before it, it ends a longer operator, the parser's.
            Token::DoubleEq
                if !(i > 0
                    && solid[i - 1].2 == solid[i].1
                    && !matches!(
                        solid[i - 1].0,
                        Token::Word(_)
                            | Token::Number(..)
                            | Token::RParen
                            | Token::RBracket
                            | Token::SingleQuotedString(_)
                            | Token::Placeholder(_)
                    )) =>
            {
                return Err(Error::new("operator does not exist: =="));
            }
            Token::LBracket => {
                if brackets == 0 && i > 0 && !ends_a_value(&solid[i - 1].0, release) {
                    return Err(near(i));
                }
                brackets += 1;
                if word(i.wrapping_sub(1)).as_deref() == Some("ARRAY") {
                    array_closes.push(brackets);
                }
            }
            Token::RBracket => {
                if array_closes.last() == Some(&brackets) {
                    array_closes.pop();
                    // An array's constructor takes a subscript only in parentheses.
                    if matches!(solid.get(i + 1).map(|t| &t.0), Some(Token::LBracket)) {
                        return Err(near(i + 1));
                    }
                }
                brackets -= 1;
            }
            Token::Mul
                if i > 0
                    && word(i - 1).as_deref() == Some("USING")
                    && matches!(solid.get(i + 1).map(|t| &t.0), Some(Token::Lt | Token::Gt))
                    && solid[i + 1].1 == solid[i].2 =>
            {
                return Err(Error::new(format!(
                    "ORDER BY … USING *{} is not read: it orders a row by the bytes PostgreSQL's \
                     heap holds it in",
                    solid[i + 1].0
                )));
            }
            _ => {}
        }
        match word(i).as_deref() {
            Some(w @ ("ISNULL" | "NOTNULL"))
                if !prev_as && !matches!(solid.get(i + 1).map(|t| &t.0), Some(Token::LParen)) =>
            {
                let with = if w == "ISNULL" { "IS NULL" } else { "IS NOT NULL" };
                swaps.push((solid[i].1, solid[i].2, with.to_string()));
            }
            // ON CONFLICT after a select list with no FROM, which sqlparser reads as the last
            // item's label: an alias that names nothing given that item first, where it has none.
            Some("ON")
                if casts
                    && word(i + 1).as_deref() == Some("CONFLICT")
                    && in_list.last().copied().unwrap_or(false)
                    && i >= 2
                    && word(i - 2).as_deref() != Some("AS")
                    && !(matches!(solid[i - 1].0, Token::Word(_))
                        && (ends_a_value(&solid[i - 2].0, release)
                            || matches!(word(i - 2).as_deref(), Some("NULL" | "TRUE" | "FALSE")))) =>
            {
                swaps.push((solid[i].1, solid[i].1, format!("AS \"{ROWS_MARK}\" ")));
            }
            // An interval's type or literal, its fields read as PostgreSQL's grammar reads them. A
            // literal is written as the cast its grammar makes of it, `'…'::interval` before its
            // fields or `'…'::interval(p)`, whose type sqlparser reads with its fields; a
            // precision takes no fields after it.
            Some("INTERVAL") if !zone_set(i) => {
                let precision = token(i + 1) == Some(&Token::LParen)
                    && matches!(token(i + 2), Some(Token::Number(..)))
                    && token(i + 3) == Some(&Token::RParen);
                let at = if precision { i + 4 } else { i + 1 };
                // A word sqlparser reads as a unit after the literal is the item's label only in a
                // select list or a RETURNING list, before what may follow an item there.
                if string(at) && unit_otherwise(at + 1) {
                    let ends = in_list.last().copied().unwrap_or(false)
                        && unended.last() == Some(&(0, 0))
                        && brackets == 0;
                    if !ends {
                        return Err(near(at + 1));
                    }
                    let follows = matches!(
                        token(at + 2),
                        None | Some(Token::EOF | Token::SemiColon | Token::Comma | Token::RParen)
                    ) || matches!(
                        word(at + 2).as_deref(),
                        Some(
                            "FROM" | "INTO" | "WHERE" | "GROUP" | "HAVING" | "WINDOW" | "UNION"
                                | "INTERSECT" | "EXCEPT" | "ORDER" | "LIMIT" | "OFFSET" | "FETCH"
                                | "FOR" | "ON" | "WITH" | "RETURNING"
                        )
                    );
                    if !follows {
                        return Err(near(at + 2));
                    }
                }
                let cast = match (string(at), precision) {
                    (false, _) if matches!(token(at), Some(Token::Number(..))) => {
                        return Err(near(at))
                    }
                    (false, true) if field(at) => return Err(near(at)),
                    (false, false) if field(at) => {
                        fields_end(at)?;
                        None
                    }
                    (false, _) => None,
                    (true, true) if field(at + 1) => return Err(near(at + 1)),
                    (true, true) => Some(format!("::interval({})", &sql[solid[i + 2].1..solid[i + 2].2])),
                    (true, false) if field(at + 1) => {
                        fields_end(at + 1)?;
                        Some("::interval".to_string())
                    }
                    (true, false) if unit_otherwise(at + 1) => Some("::interval".to_string()),
                    (true, false) => None,
                };
                if let Some(cast) = cast.filter(|_| casts) {
                    let literal = &sql[solid[at].1..solid[at].2];
                    swaps.push((solid[i].1, solid[at].2, format!("{literal}{cast}")));
                }
            }
            Some("FORMAT")
                if string(i + 1)
                    && i > 0
                    && !matches!(solid[i - 1].0, Token::Comma | Token::LParen) =>
            {
                return Err(near(i));
            }
            Some("TABLESAMPLE") if !prev_as => {
                swaps.push(sampled_clause(sql, &solid, i, &near, &closed, seeded)?);
            }
            _ => {}
        }
    }
    if swaps.is_empty() {
        return Ok(None);
    }
    swaps.sort_by_key(|s| s.0);
    let mut out = String::with_capacity(sql.len());
    let mut copied = 0;
    for (from, to, with) in swaps {
        out.push_str(&sql[copied..from]);
        out.push_str(&with);
        copied = to;
    }
    out.push_str(&sql[copied..]);
    Ok(Some(out))
}

/// PostgreSQL's keywords under a release, a line each: the word, its category, and `t` where it may
/// stand as a label without AS. 19's list from 19 on; 18's for 18 and every release before it.
pub(super) fn keyword_list(release: u8) -> &'static str {
    match release {
        19.. => include_str!("../kwlist.txt"),
        _ => include_str!("../kwlist18.txt"),
    }
}

/// A word of PostgreSQL's grammar under a release: its category, and whether it may stand as a
/// label without AS.
pub(super) fn keyword(word: &str, release: u8) -> Option<(char, bool)> {
    type Words = std::collections::HashMap<&'static str, (char, bool)>;
    static WORDS: [std::sync::OnceLock<Words>; 2] = [std::sync::OnceLock::new(), std::sync::OnceLock::new()];
    WORDS[usize::from(release >= 19)]
        .get_or_init(|| {
            keyword_list(release)
                .lines()
                .filter_map(|l| {
                    let mut f = l.split('\t');
                    let (w, c, b) = (f.next()?, f.next()?, f.next()?);
                    Some((w, (c.chars().next()?, b == "t")))
                })
                .collect()
        })
        .get(word.to_ascii_lowercase().as_str())
        .copied()
}

/// Whether a value can end at a token, so that a bracket after it is a subscript or a type's.
fn ends_a_value(t: &Token, release: u8) -> bool {
    match t {
        Token::RParen
        | Token::RBracket
        | Token::SingleQuotedString(_)
        | Token::Number(..)
        | Token::Placeholder(_) => true,
        Token::Word(w) => {
            w.quote_style.is_some()
                || w.value.eq_ignore_ascii_case("array")
                || !matches!(keyword(&w.value, release), Some(('R', _)))
        }
        _ => false,
    }
}

/// A select list's or RETURNING list's label written without AS where PostgreSQL's grammar takes it
/// only after AS: a keyword the release's list says requires it (`SELECT 1 year`), refused at the
/// label as the grammar refuses it. sqlparser takes such a word for the label.
fn bare_labels(sql: &str, release: u8) -> Result<()> {
    labels_written_bare(sql, &|w| wants_as(w, release), false).map(|_| ())
}

/// Whether a word the release's list holds stands as a label only after AS.
fn wants_as(word: &str, release: u8) -> bool {
    matches!(keyword(word, release), Some((c, false)) if c != 'R')
}

/// The labels a DDL's statement writes without AS, which sqlparser prints after one: refused at the
/// first that stands only after AS under every release the snail reads, as `bare_labels` refuses
/// it; else how many there are. Each release's own word is the statement's, read where its file's
/// release is known (`labels_at_release`).
pub(crate) fn ddl_labels(sql: &str) -> Result<usize> {
    labels_written_bare(sql, &|w| wants_as(w, 18) && wants_as(w, crate::regs::LATEST), true)
}

/// A DDL statement's labels and those of its bodies of LANGUAGE sql after AS, as `bare_labels`
/// reads them under `release`, where CREATE FUNCTION checks a body as a statement.
pub(crate) fn labels_at_release(sql: &str, release: u8) -> Result<()> {
    bare_labels(sql, release)?;
    for body in sql_bodies(sql) {
        bare_labels(&body, release)?;
    }
    Ok(())
}

/// The bodies after AS of a CREATE FUNCTION or PROCEDURE of LANGUAGE sql, as their strings hold
/// them; none for any other statement.
pub(crate) fn sql_bodies(sql: &str) -> Vec<String> {
    let Ok(tokens) = super::statement::spans(sql) else {
        return Vec::new();
    };
    let solid: Vec<&Token> = tokens
        .iter()
        .map(|t| &t.0)
        .filter(|t| !matches!(t, Token::Whitespace(_) | Token::EOF))
        .collect();
    let word = |i: usize| match solid.get(i) {
        Some(Token::Word(w)) if w.quote_style.is_none() => Some(w.value.to_ascii_uppercase()),
        _ => None,
    };
    let routine = word(0).as_deref() == Some("CREATE")
        && (matches!(word(1).as_deref(), Some("FUNCTION" | "PROCEDURE"))
            || word(1).as_deref() == Some("OR")
                && word(2).as_deref() == Some("REPLACE")
                && matches!(word(3).as_deref(), Some("FUNCTION" | "PROCEDURE")));
    let in_sql = (0..solid.len()).any(|i| {
        word(i).as_deref() == Some("LANGUAGE")
            && match solid.get(i + 1) {
                Some(Token::Word(w)) => w.value.eq_ignore_ascii_case("sql"),
                Some(Token::SingleQuotedString(s)) => s.eq_ignore_ascii_case("sql"),
                _ => false,
            }
    });
    if !(routine && in_sql) {
        return Vec::new();
    }
    (0..solid.len())
        .filter(|&i| word(i).as_deref() == Some("AS"))
        .filter_map(|i| string_of(solid.get(i + 1).copied()))
        .collect()
}

/// The labels of a statement's select lists and RETURNING lists that sqlparser took where no AS is
/// written before them: refused at the first in the order written that `wants` says stands only
/// after AS; else how many of them there are, each counted where `all`, else only those `wants`
/// names (none: they are refused).
fn labels_written_bare(sql: &str, wants: &dyn Fn(&str) -> bool, all: bool) -> Result<usize> {
    let Ok(tokens) = super::image::tokens(sql) else {
        return Ok(0);
    };
    let mut before: Option<&Token> = None;
    let mut bare = Vec::new();
    for t in &tokens {
        if matches!(t.token, Token::Whitespace(_)) {
            continue;
        }
        if let Token::Word(w) = &t.token {
            let after_as = matches!(before, Some(Token::Word(b)) if b.quote_style.is_none() && b.keyword == Keyword::AS);
            let wanted = w.quote_style.is_none() && wants(&w.value);
            if (all || wanted) && !after_as {
                bare.push(t.span.start);
            }
        }
        before = Some(&t.token);
    }
    if bare.is_empty() {
        return Ok(0);
    }
    let Ok(statements) = sqlparser::parser::Parser::new(&sqlparser::dialect::PostgreSqlDialect {})
        .with_tokens_with_locations(tokens)
        .parse_statements()
    else {
        return Ok(0);
    };
    struct Labels<'b> {
        bare: &'b [Location],
        found: Vec<(Location, String, bool)>,
    }
    impl Labels<'_> {
        fn items(&mut self, items: &[SelectItem]) {
            for item in items {
                if let SelectItem::ExprWithAlias { alias, .. } = item {
                    if self.bare.contains(&alias.span.start) {
                        self.found.push((
                            alias.span.start,
                            alias.value.clone(),
                            alias.quote_style.is_none(),
                        ));
                    }
                }
            }
        }
        fn body(&mut self, body: &SetExpr) {
            match body {
                SetExpr::Select(s) => self.items(&s.projection),
                SetExpr::SetOperation { left, right, .. } => {
                    self.body(left);
                    self.body(right);
                }
                _ => {}
            }
        }
    }
    impl sqlparser::ast::Visitor for Labels<'_> {
        type Break = ();
        fn pre_visit_query(&mut self, q: &Query) -> std::ops::ControlFlow<()> {
            self.body(&q.body);
            std::ops::ControlFlow::Continue(())
        }
        fn pre_visit_statement(&mut self, s: &sqlparser::ast::Statement) -> std::ops::ControlFlow<()> {
            use sqlparser::ast::Statement as S;
            let returning = match s {
                S::Insert(i) => i.returning.as_deref(),
                S::Update(u) => u.returning.as_deref(),
                S::Delete(d) => d.returning.as_deref(),
                _ => None,
            };
            self.items(returning.unwrap_or_default());
            std::ops::ControlFlow::Continue(())
        }
    }
    let mut labels = Labels { bare: &bare, found: Vec::new() };
    let _ = sqlparser::ast::Visit::visit(&statements, &mut labels);
    labels.found.sort_by_key(|(at, _, _)| (at.line, at.column));
    labels.found.dedup_by_key(|(at, _, _)| (at.line, at.column));
    let refused = labels
        .found
        .iter()
        .filter(|(_, word, unquoted)| *unquoted && wants(word))
        .min_by_key(|(at, _, _)| (at.line, at.column));
    match refused {
        Some((_, word, _)) => Err(Error::new(format!("syntax error at or near \"{word}\""))),
        None => Ok(labels.found.len()),
    }
}

/// A TABLESAMPLE clause as PostgreSQL's grammar has it, written as a call of a name that carries its
/// method: `TABLESAMPLE m (a, b) REPEATABLE (s)` as `TABLESAMPLE "\u{2}r\u{2}m"(a, b, (s))`; one naming
/// no seed as `"\u{2}s\u{2}m"(a, b, n)`, `n` the seed drawn for the statement, or unseeded `\u{2}n`.
fn sampled_clause(
    sql: &str,
    solid: &[&(Token, usize, usize)],
    i: usize,
    near: &dyn Fn(usize) -> Error,
    closed: &dyn Fn(usize, Token, Token) -> Option<usize>,
    seeded: bool,
) -> Result<(usize, usize, String)> {
    let part = |j: usize| match solid.get(j).map(|t| &t.0) {
        Some(Token::Word(w)) => Some(match w.quote_style {
            Some(_) => w.value.clone(),
            None => w.value.to_lowercase(),
        }),
        _ => None,
    };
    // Written so already.
    if part(i + 1).is_some_and(|p| p.starts_with(SAMPLED)) {
        return Ok((solid[i].1, solid[i].2, sql[solid[i].1..solid[i].2].to_string()));
    }
    let Some(mut method) = part(i + 1) else {
        return Err(near(i + 1));
    };
    let mut j = i + 2;
    while matches!(solid.get(j).map(|t| &t.0), Some(Token::Period)) {
        let Some(p) = part(j + 1) else {
            return Err(near(j + 1));
        };
        method.push('.');
        method.push_str(&p);
        j += 2;
    }
    if !matches!(solid.get(j).map(|t| &t.0), Some(Token::LParen)) {
        return Err(near(j));
    }
    // `ROW` opens a row, and takes no parenthesis as a function's name.
    if method == "row" && j == i + 2 {
        return Err(near(j));
    }
    if matches!(solid.get(j + 1).map(|t| &t.0), Some(Token::RParen)) {
        return Err(near(j + 1));
    }
    let Some(after) = closed(j, Token::LParen, Token::RParen) else {
        return Err(near(solid.len()));
    };
    let args = sql[solid[j + 1].1..solid[after - 2].2].to_string();
    let repeatable = matches!(
        solid.get(after).map(|t| &t.0),
        Some(Token::Word(w)) if w.quote_style.is_none() && w.value.eq_ignore_ascii_case("repeatable")
    );
    let (kind, seed, end) = if repeatable {
        if !matches!(solid.get(after + 1).map(|t| &t.0), Some(Token::LParen)) {
            return Err(near(after + 1));
        }
        let Some(last) = closed(after + 1, Token::LParen, Token::RParen) else {
            return Err(near(solid.len()));
        };
        if last == after + 3 {
            return Err(near(after + 2));
        }
        (
            'r',
            format!(", ({})", &sql[solid[after + 2].1..solid[last - 2].2]),
            solid[last - 1].2,
        )
    } else if seeded {
        (
            's',
            format!(", {}", super::tablesample::drawn_seed()),
            solid[after - 1].2,
        )
    } else {
        ('n', String::new(), solid[after - 1].2)
    };
    let name = format!("{SAMPLED}{kind}{SAMPLED}{method}").replace('"', "\"\"");
    Ok((
        solid[i].1,
        end,
        format!("TABLESAMPLE \"{name}\"({args}{seed})"),
    ))
}

/// ROWS FROM (…) at `i` as a call of `ROWS_FROM`, its functions its arguments, each function's
/// column definition list the named arguments after it: `ROWS FROM (f(a) AS (x t), g(b))` as
/// `"\u{6}rows"(f(a) , "x" => CAST(NULL AS t), g(b))`, a column written SETOF or with a COLLATE
/// named as `defined_item` marks it. Each swap made, the functions' own text left to the rest;
/// refused as PostgreSQL's grammar refuses a list it does not read.
fn rows_from(
    sql: &str,
    solid: &[&(Token, usize, usize)],
    i: usize,
    word: &dyn Fn(usize) -> Option<String>,
    near: &dyn Fn(usize) -> Error,
    closed: &dyn Fn(usize, Token, Token) -> Option<usize>,
) -> Result<Vec<(usize, usize, String)>> {
    let open = i + 2;
    let Some(after) = closed(open, Token::LParen, Token::RParen) else {
        return Err(near(solid.len()));
    };
    if after == open + 2 {
        return Err(near(open + 1));
    }
    let items = |from: usize, to: usize| items_between(solid, from, to);
    let mut swaps = vec![(solid[i].1, solid[open].2, format!("\"{ROWS_FROM}\"("))];
    for (a, b) in items(open + 1, after - 1) {
        if a == b {
            return Err(near(a));
        }
        if !matches!(solid[a].0, Token::Word(_)) {
            return Err(near(a));
        }
        let mut depth = 0i32;
        let mut listed = None;
        for k in a..b {
            match solid[k].0 {
                Token::LParen | Token::LBracket => depth += 1,
                Token::RParen | Token::RBracket => depth -= 1,
                _ if depth == 0 && k > a && word(k).as_deref() == Some("AS") => {
                    listed = Some(k);
                    break;
                }
                _ => {}
            }
        }
        let Some(k) = listed else {
            continue;
        };
        if k + 1 >= b || solid[k + 1].0 != Token::LParen {
            return Err(near(k + 1));
        }
        let Some(end) = closed(k + 1, Token::LParen, Token::RParen) else {
            return Err(near(solid.len()));
        };
        if end != b {
            return Err(near(end));
        }
        if end == k + 3 {
            return Err(near(k + 2));
        }
        let mut named = Vec::new();
        for (ea, eb) in items(k + 2, end - 1) {
            let (name, ty) = defined_item(sql, solid, (ea, eb), word, near)?;
            named.push(format!(
                ", \"{}\" => CAST(NULL AS {ty})",
                name.replace('"', "\"\"")
            ));
        }
        swaps.push((solid[k].1, solid[end - 1].2, named.concat()));
    }
    Ok(swaps)
}

/// A function's column definition list from `from` to `to`: each column written SETOF or with a
/// COLLATE by its name marked with them (`defined_item`). A list naming its first column alone
/// refused as PostgreSQL's grammar refuses it, at the first type it meets.
fn defined_list(
    sql: &str,
    solid: &[&(Token, usize, usize)],
    (from, to): (usize, usize),
    word: &dyn Fn(usize) -> Option<String>,
    near: &dyn Fn(usize) -> Error,
) -> Result<Vec<(usize, usize, String)>> {
    let items = items_between(solid, from, to);
    let lone = |&(a, b): &(usize, usize)| b == a + 1;
    if items.first().is_some_and(lone) {
        if let Some(&(a, _)) = items.iter().find(|x| !lone(x)) {
            return Err(near(a + 1));
        }
    }
    let mut swaps = Vec::new();
    for (a, b) in items {
        let (name, ty) = defined_item(sql, solid, (a, b), word, near)?;
        if name.contains(ROWS_MARK) {
            let written = format!("\"{}\" {ty}", name.replace('"', "\"\""));
            swaps.push((solid[a].1, solid[b - 1].2, written));
        }
    }
    Ok(swaps)
}

/// The places of a list's items between `from` and `to`, split at its commas.
fn items_between(solid: &[&(Token, usize, usize)], from: usize, to: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let (mut depth, mut start) = (0i32, from);
    for k in from..to {
        match solid[k].0 {
            Token::LParen | Token::LBracket => depth += 1,
            Token::RParen | Token::RBracket => depth -= 1,
            Token::Comma if depth == 0 => {
                out.push((start, k));
                start = k + 1;
            }
            _ => {}
        }
    }
    out.push((start, to));
    out
}

/// One column of a column definition list, from `a` to `b`, as PostgreSQL's grammar reads it: a
/// name, a type, SETOF perhaps before it, and a COLLATE perhaps after it. Its name, marked where it
/// is written SETOF or with a COLLATE (`marked_column`), and its type's text; refused at the word
/// the grammar stops at.
fn defined_item<'s>(
    sql: &'s str,
    solid: &[&(Token, usize, usize)],
    (a, b): (usize, usize),
    word: &dyn Fn(usize) -> Option<String>,
    near: &dyn Fn(usize) -> Error,
) -> Result<(String, &'s str)> {
    let part = |j: usize| match &solid[j].0 {
        Token::Word(w) if w.quote_style.is_some() => Some(w.value.clone()),
        Token::Word(w) => Some(w.value.to_lowercase()),
        _ => None,
    };
    let name = part(a).ok_or_else(|| near(a))?;
    if b <= a + 1 {
        return Err(near(b));
    }
    let setof = word(a + 1).as_deref() == Some("SETOF");
    let typed_from = if setof { a + 2 } else { a + 1 };
    let collate = (typed_from..b).find(|&c| word(c).as_deref() == Some("COLLATE"));
    let typed_to = collate.unwrap_or(b);
    if typed_to == typed_from {
        return Err(near(typed_to));
    }
    let ty = &sql[solid[typed_from].1..solid[typed_to - 1].2];
    // A collation's name, in its schema perhaps: `pg_catalog."C"`.
    let collation = match collate {
        Some(c) => {
            let mut parts = vec![part(c + 1).filter(|_| c + 1 < b).ok_or_else(|| near(c + 1))?];
            let mut j = c + 2;
            while j < b && solid[j].0 == Token::Period {
                parts.push(part(j + 1).filter(|_| j + 1 < b).ok_or_else(|| near(j + 1))?);
                j += 2;
            }
            if j < b {
                return Err(near(j));
            }
            Some(parts.join("."))
        }
        None => None,
    };
    let name = match setof || collation.is_some() {
        true => marked_column(&name, setof, collation.as_deref()),
        false => name,
    };
    Ok((name, ty))
}

/// What stands on the right of `@@@`, which sqlparser reads as `@@` and a prefix `@`.
pub(super) fn right_of_three_ats(right: &Expr) -> Option<&Expr> {
    match right {
        Expr::UnaryOp {
            op: UnaryOperator::PGAbs,
            expr,
        } => Some(expr),
        _ => None,
    }
}

/// `@@` and `@` written apart are two operators to PostgreSQL, and no prefix `@` gives what a `@@`
/// reads; together they are `@@@`, and sqlparser reads the two alike. Refused here, where they can
/// be told apart: a statement printed from what sqlparser read writes `@@@` as `@@ @x`.
pub(super) fn at_at_spaced(sql: &str) -> Result<()> {
    if !sql.contains("@@") {
        return Ok(());
    }
    let tokens = Tokenizer::new(&sqlparser::dialect::PostgreSqlDialect {}, sql).tokenize()?;
    for (i, _) in tokens
        .iter()
        .enumerate()
        .filter(|(_, t)| **t == Token::AtAt)
    {
        let next = tokens[i + 1..]
            .iter()
            .position(|t| !matches!(t, Token::Whitespace(_)))
            .map(|n| n + i + 1);
        let apart = |&n: &usize| {
            n > i + 1
                && tokens[n] == Token::AtSign
                && matches!(tokens.get(n + 1), Some(Token::Whitespace(_)))
        };
        if let Some(n) = next.filter(apart) {
            let operand = tokens[n + 1..]
                .iter()
                .find(|t| !matches!(t, Token::Whitespace(_)));
            return Err(Error::new(match operand {
                Some(Token::SingleQuotedString(_)) => {
                    "operator is not unique: @ unknown".to_string()
                }
                _ => "operator does not exist: @@ with a prefix @ on its right".to_string(),
            }));
        }
    }
    Ok(())
}

/// What marks a window's name as carrying its frame's EXCLUDE: the name is this character, the
/// exclusion's words, this character again, and the window it names, if any.
pub(super) const EXCLUDED: char = '\u{1}';

/// The statement with its first frame's EXCLUDE moved into its window's name, or none where no
/// frame excludes: `OVER (w ORDER BY x ROWS … EXCLUDE TIES)` is written
/// `OVER ("\u{1}ties\u{1}w" ORDER BY x ROWS …)`, and `EXCLUDE NO OTHERS`, the default, is dropped.
fn excluded_once(sql: &str) -> Result<Option<String>> {
    let tokens =
        Tokenizer::new(&sqlparser::dialect::PostgreSqlDialect {}, sql).tokenize_with_location()?;
    let starts: Vec<usize> = std::iter::once(0)
        .chain(sql.match_indices('\n').map(|(i, _)| i + 1))
        .collect();
    let offset = |l: Location| -> usize {
        let start = starts[l.line as usize - 1];
        start
            + sql[start..]
                .chars()
                .take(l.column as usize - 1)
                .map(char::len_utf8)
                .sum::<usize>()
    };
    let solid: Vec<&TokenWithSpan> = tokens
        .iter()
        .filter(|t| !matches!(t.token, Token::Whitespace(_)))
        .collect();
    let word = |i: usize| match solid.get(i).map(|t| &t.token) {
        Some(Token::Word(w)) if w.quote_style.is_none() => Some(w.value.to_ascii_uppercase()),
        _ => None,
    };
    let start = |i: usize| offset(solid[i].span.start);
    let end = |i: usize| offset(solid[i].span.end);
    let mut opened: Vec<usize> = Vec::new();
    for i in 0..solid.len() {
        match solid[i].token {
            Token::LParen => opened.push(i),
            Token::RParen => {
                opened.pop();
            }
            _ => {}
        }
        if word(i).as_deref() != Some("EXCLUDE")
            || !matches!(
                i.checked_sub(1).and_then(word).as_deref(),
                Some("PRECEDING" | "FOLLOWING" | "ROW")
            )
        {
            continue;
        }
        let (kind, last) = match (word(i + 1).as_deref(), word(i + 2).as_deref()) {
            (Some("CURRENT"), Some("ROW")) => (Some("current row"), i + 2),
            (Some("GROUP"), _) => (Some("group"), i + 1),
            (Some("TIES"), _) => (Some("ties"), i + 1),
            (Some("NO"), Some("OTHERS")) => (None, i + 2),
            _ => continue,
        };
        let Some(&open) = opened.last() else {
            continue;
        };
        if !matches!(solid.get(last + 1).map(|t| &t.token), Some(Token::RParen)) {
            continue;
        }
        // The window the spelling names, its identifier folded as PostgreSQL's parser folds it.
        let named = match &solid[open + 1].token {
            Token::Word(w) if w.keyword == Keyword::NoKeyword => Some(match w.quote_style {
                None => w.value.to_ascii_lowercase(),
                Some(_) => w.value.clone(),
            }),
            _ => None,
        };
        let after = if named.is_some() {
            end(open + 1)
        } else {
            end(open)
        };
        let mark = match (kind, named) {
            (Some(k), n) => format!(
                " \"{EXCLUDED}{k}{EXCLUDED}{}\"",
                n.unwrap_or_default().replace('"', "\"\"")
            ),
            (None, Some(n)) => format!(" \"{}\"", n.replace('"', "\"\"")),
            (None, None) => String::new(),
        };
        return Ok(Some(format!(
            "{}{mark}{}{}",
            &sql[..end(open)],
            &sql[after..start(i)],
            &sql[end(last)..]
        )));
    }
    Ok(None)
}

/// A group the words open: a parenthesis, a SUBSTRING call's among them, a bracket, or a CASE.
enum Group {
    Paren,
    Substring(Vec<(Keyword, usize)>),
    Bracket,
    Case,
}

/// The statement with its first rewrite made, or none where it needs none.
fn rewrite_once(sql: &str) -> Result<Option<String>> {
    let tokens =
        Tokenizer::new(&sqlparser::dialect::PostgreSqlDialect {}, sql).tokenize_with_location()?;
    let starts: Vec<usize> = std::iter::once(0)
        .chain(sql.match_indices('\n').map(|(i, _)| i + 1))
        .collect();
    let offset = |l: Location| -> usize {
        let start = starts[l.line as usize - 1];
        start
            + sql[start..]
                .chars()
                .take(l.column as usize - 1)
                .map(char::len_utf8)
                .sum::<usize>()
    };
    let solid: Vec<&TokenWithSpan> = tokens
        .iter()
        .filter(|t| !matches!(t.token, Token::Whitespace(_)))
        .collect();
    let keyword = |i: usize| match solid.get(i).map(|t| &t.token) {
        Some(Token::Word(w)) if w.quote_style.is_none() => Some(w.keyword),
        _ => None,
    };
    let start = |i: usize| offset(solid[i].span.start);
    let end = |i: usize| offset(solid[i].span.end);
    let near = |i: usize| match solid.get(i) {
        Some(t) => Error::new(format!("syntax error at or near \"{}\"", t.token)),
        None => Error::new("syntax error at end of input"),
    };
    let mut groups: Vec<Group> = Vec::new();
    for i in 0..solid.len() {
        match &solid[i].token {
            Token::LParen => {
                let call = i > 0 && keyword(i - 1) == Some(Keyword::SUBSTRING);
                groups.push(if call {
                    Group::Substring(Vec::new())
                } else {
                    Group::Paren
                });
            }
            Token::RParen => {
                if let Some(Group::Substring(words)) = groups.pop() {
                    match words.as_slice() {
                        // `SUBSTRING(s SIMILAR p ESCAPE e)` is `SUBSTRING(s FROM p FOR e)`, the
                        // same call of substring of three texts.
                        [(Keyword::SIMILAR, s), rest @ ..] => match rest.first() {
                            Some(&(Keyword::ESCAPE, e)) => {
                                return Ok(Some(format!(
                                    "{}FROM{}FOR{}",
                                    &sql[..start(*s)],
                                    &sql[end(*s)..start(e)],
                                    &sql[end(e)..]
                                )))
                            }
                            Some(&(_, k)) => return Err(near(k)),
                            None => return Err(near(i)),
                        },
                        // `SUBSTRING(s FOR e FROM p)` is `SUBSTRING(s FROM p FOR e)`.
                        [(Keyword::FOR, f), (Keyword::FROM, m), ..] => {
                            return Ok(Some(format!(
                                "{}FROM {} FOR {}{}",
                                &sql[..start(*f)],
                                sql[end(*m)..start(i)].trim(),
                                sql[end(*f)..start(*m)].trim(),
                                &sql[start(i)..]
                            )))
                        }
                        _ => {}
                    }
                }
            }
            Token::LBracket => groups.push(Group::Bracket),
            Token::RBracket => {
                groups.pop();
            }
            _ => {}
        }
        match keyword(i) {
            Some(Keyword::CASE) => groups.push(Group::Case),
            Some(Keyword::END) if matches!(groups.last(), Some(Group::Case)) => {
                groups.pop();
            }
            Some(k @ (Keyword::SIMILAR | Keyword::ESCAPE | Keyword::FOR | Keyword::FROM)) => {
                if let Some(Group::Substring(words)) = groups.last_mut() {
                    if !(k == Keyword::SIMILAR && keyword(i + 1) == Some(Keyword::TO)) {
                        words.push((k, i));
                    }
                }
            }
            Some(Keyword::BETWEEN)
                if matches!(
                    keyword(i + 1),
                    Some(Keyword::SYMMETRIC | Keyword::ASYMMETRIC)
                ) =>
            {
                // The bound runs to the first AND outside any group it opens.
                let first = i + 2;
                let mut depth = 0i32;
                let mut and = None;
                for j in first..solid.len() {
                    match (&solid[j].token, keyword(j)) {
                        (Token::LParen | Token::LBracket, _) | (_, Some(Keyword::CASE)) => {
                            depth += 1
                        }
                        (Token::RParen | Token::RBracket, _) | (_, Some(Keyword::END)) => {
                            depth -= 1;
                            if depth < 0 {
                                break;
                            }
                        }
                        (_, Some(Keyword::AND)) if depth == 0 => {
                            and = Some(j);
                            break;
                        }
                        _ => {}
                    }
                }
                let Some(and) = and.filter(|&a| a > first) else {
                    return Err(near(and.unwrap_or(first)));
                };
                // Already one parenthesis, read as the call; a row's, whose fields the call would
                // read as its arguments, given one more.
                let whole = matches!(solid[first].token, Token::LParen)
                    && matches!(solid[and - 1].token, Token::RParen)
                    && {
                        let mut d = 0;
                        (first..and).all(|j| {
                            match solid[j].token {
                                Token::LParen => d += 1,
                                Token::RParen => d -= 1,
                                Token::Comma if d == 1 => return false,
                                _ => {}
                            }
                            d > 0 || j == and - 1
                        })
                    };
                if !whole {
                    return Ok(Some(format!(
                        "{}({}) {}",
                        &sql[..start(first)],
                        sql[start(first)..start(and)].trim_end(),
                        &sql[start(and)..]
                    )));
                }
            }
            _ => {}
        }
    }
    Ok(None)
}

/// A pipeline's statement with each BETWEEN written as PostgreSQL's `transformAExprBetween` writes
/// it: `x >= a AND x <= b`; NOT BETWEEN, `x < a OR x > b`; SYMMETRIC, both orders of the bounds
/// or'd; NOT BETWEEN SYMMETRIC, both orders' NOT BETWEEN and'd. Each in parentheses. And each
/// `CASE x WHEN v` whose `x` calls no function and holds no subquery written `CASE WHEN (x = v)`,
/// the comparison `transformCaseExpr` makes, an `x` written as a quoted literal read as text. And
/// two rows of one length compared by IS DISTINCT FROM written as `make_row_distinct_op` writes
/// them, each pair's IS DISTINCT FROM or'd in turn, FALSE for two rows of none; IS NOT DISTINCT FROM
/// the NOT of that.
pub(super) fn between_written(q: &Query) -> Query {
    let mut q = q.clone();
    query(&mut q);
    q
}

/// An expression written as `between_written` writes a statement's: an UPDATE's SET item's.
pub(super) fn expr_written(e: &Expr) -> Expr {
    let mut e = e.clone();
    expr(&mut e);
    e
}

fn query(q: &mut Query) {
    if let Some(w) = &mut q.with {
        for c in &mut w.cte_tables {
            query(&mut c.query);
        }
    }
    set_expr(&mut q.body);
    if let Some(o) = &mut q.order_by {
        if let OrderByKind::Expressions(es) = &mut o.kind {
            for x in es {
                expr(&mut x.expr);
            }
        }
    }
}

fn set_expr(s: &mut SetExpr) {
    match s {
        SetExpr::Select(s) => {
            for item in &mut s.projection {
                if let SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } = item
                {
                    expr(e);
                }
            }
            for t in &mut s.from {
                table_factor(&mut t.relation);
                for j in &mut t.joins {
                    table_factor(&mut j.relation);
                    if let Some(JoinConstraint::On(e)) = join_constraint(&mut j.join_operator) {
                        expr(e);
                    }
                }
            }
            for e in s.selection.iter_mut().chain(s.having.iter_mut()) {
                expr(e);
            }
            if let GroupByExpr::Expressions(es, _) = &mut s.group_by {
                es.iter_mut().for_each(expr);
            }
        }
        SetExpr::Query(q) => query(q),
        SetExpr::SetOperation { left, right, .. } => {
            set_expr(left);
            set_expr(right);
        }
        _ => {}
    }
}

fn table_factor(t: &mut TableFactor) {
    if let TableFactor::Derived { subquery, .. } = t {
        query(subquery);
    }
}

fn join_constraint(j: &mut JoinOperator) -> Option<&mut JoinConstraint> {
    match j {
        JoinOperator::Join(c)
        | JoinOperator::Inner(c)
        | JoinOperator::Left(c)
        | JoinOperator::LeftOuter(c)
        | JoinOperator::Right(c)
        | JoinOperator::RightOuter(c)
        | JoinOperator::FullOuter(c)
        | JoinOperator::CrossJoin(c) => Some(c),
        _ => None,
    }
}

fn function_arg(a: &mut FunctionArg) {
    let (FunctionArg::Unnamed(x)
    | FunctionArg::Named { arg: x, .. }
    | FunctionArg::ExprNamed { arg: x, .. }) = a;
    if let FunctionArgExpr::Expr(e) = x {
        expr(e);
    }
}

fn function_arguments(a: &mut FunctionArguments) {
    match a {
        FunctionArguments::List(l) => {
            l.args.iter_mut().for_each(function_arg);
            for c in &mut l.clauses {
                match c {
                    FunctionArgumentClause::OrderBy(o) => {
                        o.iter_mut().for_each(|x| expr(&mut x.expr))
                    }
                    FunctionArgumentClause::Where(e) | FunctionArgumentClause::Limit(e) => expr(e),
                    _ => {}
                }
            }
        }
        FunctionArguments::Subquery(q) => query(q),
        FunctionArguments::None => {}
    }
}

/// Whether an expression reads the same written in several places as once: it calls no function
/// and holds no subquery.
fn written_once(e: &Expr) -> bool {
    let mut once = true;
    let _ = sqlparser::ast::visit_expressions(e, |x| {
        if matches!(
            x,
            Expr::Function(_) | Expr::Subquery(_) | Expr::Exists { .. } | Expr::InSubquery { .. }
        ) {
            once = false;
        }
        std::ops::ControlFlow::<()>::Continue(())
    });
    once
}

/// Each BETWEEN inside `e`, and `e` where it is one, written as its comparisons.
fn expr(e: &mut Expr) {
    let each = |es: &mut [Expr]| es.iter_mut().for_each(expr);
    let boxed = |x: &mut Option<Box<Expr>>| {
        if let Some(x) = x {
            expr(x)
        }
    };
    match e {
        Expr::BinaryOp { left, right, .. }
        | Expr::AnyOp { left, right, .. }
        | Expr::AllOp { left, right, .. }
        | Expr::IsDistinctFrom(left, right)
        | Expr::IsNotDistinctFrom(left, right) => {
            expr(left);
            expr(right);
        }
        Expr::UnaryOp { expr: x, .. }
        | Expr::Nested(x)
        | Expr::Cast { expr: x, .. }
        | Expr::Collate { expr: x, .. }
        | Expr::IsNull(x)
        | Expr::IsNotNull(x)
        | Expr::IsTrue(x)
        | Expr::IsFalse(x)
        | Expr::IsNotTrue(x)
        | Expr::IsNotFalse(x)
        | Expr::IsUnknown(x)
        | Expr::IsNotUnknown(x)
        | Expr::Extract { expr: x, .. }
        | Expr::Ceil { expr: x, .. }
        | Expr::Floor { expr: x, .. } => expr(x),
        Expr::InList { expr: x, list, .. } => {
            expr(x);
            each(list);
        }
        Expr::InSubquery {
            expr: x, subquery, ..
        } => {
            expr(x);
            query(subquery);
        }
        Expr::Subquery(q) | Expr::Exists { subquery: q, .. } => query(q),
        Expr::Like {
            expr: x,
            pattern,
            escape_char,
            ..
        }
        | Expr::ILike {
            expr: x,
            pattern,
            escape_char,
            ..
        }
        | Expr::SimilarTo {
            expr: x,
            pattern,
            escape_char,
            ..
        } => {
            expr(x);
            expr(pattern);
            boxed(escape_char);
        }
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            boxed(operand);
            for c in conditions.iter_mut() {
                expr(&mut c.condition);
                expr(&mut c.result);
            }
            boxed(else_result);
            if operand.as_deref().is_some_and(written_once) {
                let x = *operand.take().unwrap();
                let x = match super::typed::untyped(&x) {
                    Some(Some(_)) => Expr::Cast {
                        kind: sqlparser::ast::CastKind::DoubleColon,
                        expr: Box::new(x),
                        data_type: sqlparser::ast::DataType::Text,
                        format: None,
                    },
                    _ => x,
                };
                for c in conditions {
                    let v = std::mem::replace(
                        &mut c.condition,
                        Expr::Value(sqlparser::ast::Value::Null.into()),
                    );
                    c.condition = Expr::Nested(Box::new(Expr::BinaryOp {
                        left: Box::new(x.clone()),
                        op: BinaryOperator::Eq,
                        right: Box::new(v),
                    }));
                }
            }
        }
        Expr::Function(f) => {
            function_arguments(&mut f.args);
            function_arguments(&mut f.parameters);
            boxed(&mut f.filter);
            f.within_group.iter_mut().for_each(|x| expr(&mut x.expr));
            if let Some(WindowType::WindowSpec(w)) = &mut f.over {
                each(&mut w.partition_by);
                w.order_by.iter_mut().for_each(|x| expr(&mut x.expr));
            }
        }
        Expr::Substring {
            expr: x,
            substring_from,
            substring_for,
            ..
        } => {
            expr(x);
            boxed(substring_from);
            boxed(substring_for);
        }
        Expr::Trim {
            expr: x,
            trim_what,
            trim_characters,
            ..
        } => {
            expr(x);
            boxed(trim_what);
            if let Some(cs) = trim_characters {
                each(cs);
            }
        }
        Expr::Position { expr: x, r#in } => {
            expr(x);
            expr(r#in);
        }
        Expr::Overlay {
            expr: x,
            overlay_what,
            overlay_from,
            overlay_for,
        } => {
            expr(x);
            expr(overlay_what);
            expr(overlay_from);
            boxed(overlay_for);
        }
        Expr::AtTimeZone {
            timestamp,
            time_zone,
        } => {
            expr(timestamp);
            expr(time_zone);
        }
        Expr::Tuple(es) => each(es),
        Expr::Array(a) => each(&mut a.elem),
        Expr::CompoundFieldAccess { root, access_chain } => {
            // A name and the names dotted after it, before a subscript, are one column's name, as
            // PostgreSQL's grammar reads `c.conkey[1]`.
            if let Expr::Identifier(first) = root.as_ref() {
                let named = access_chain
                    .iter()
                    .take_while(|a| matches!(a, AccessExpr::Dot(Expr::Identifier(_))))
                    .count();
                if named > 0 && named < access_chain.len() {
                    let mut parts = vec![first.clone()];
                    for a in access_chain.drain(..named) {
                        if let AccessExpr::Dot(Expr::Identifier(i)) = a {
                            parts.push(i);
                        }
                    }
                    **root = Expr::CompoundIdentifier(parts);
                }
            }
            expr(root);
            for a in access_chain {
                match a {
                    AccessExpr::Dot(x) => expr(x),
                    AccessExpr::Subscript(Subscript::Index { index }) => expr(index),
                    AccessExpr::Subscript(Subscript::Slice {
                        lower_bound,
                        upper_bound,
                        stride,
                    }) => {
                        for x in [lower_bound, upper_bound, stride].into_iter().flatten() {
                            expr(x);
                        }
                    }
                }
            }
        }
        Expr::Interval(i) => expr(&mut i.value),
        Expr::Between {
            expr: x, low, high, ..
        } => {
            expr(x);
            expr(low);
            expr(high);
        }
        _ => {}
    }
    if let Expr::Between {
        expr: x,
        negated,
        low,
        high,
    } = e
    {
        let (low, symmetric) = super::question::between_low(low);
        let cmp = |op: BinaryOperator, bound: &Expr| Expr::BinaryOp {
            left: x.clone(),
            op,
            right: Box::new(bound.clone()),
        };
        let join = |a: Expr, op: BinaryOperator, b: Expr| Expr::BinaryOp {
            left: Box::new(Expr::Nested(Box::new(a))),
            op,
            right: Box::new(Expr::Nested(Box::new(b))),
        };
        use BinaryOperator::{And, Gt, GtEq, Lt, LtEq, Or};
        let within = |a: &Expr, b: &Expr| join(cmp(GtEq, a), And, cmp(LtEq, b));
        let outside = |a: &Expr, b: &Expr| join(cmp(Lt, a), Or, cmp(Gt, b));
        let written = match (*negated, symmetric) {
            (false, false) => within(low, high),
            (true, false) => outside(low, high),
            (false, true) => join(within(low, high), Or, within(high, low)),
            (true, true) => join(outside(low, high), And, outside(high, low)),
        };
        *e = Expr::Nested(Box::new(written));
    }
    if let Some(written) = rows_distinct(e) {
        *e = written;
    }
    // Rows compared holding a set-returning function stay as written, refused where they are
    // read.
    if super::pipeline::holds_set(e) {
        return;
    }
    if let Some(written) = rows_in(e) {
        *e = written;
    }
    if let Some(written) = rows_compared(e) {
        *e = written;
    }
}

/// A row IN a list, written as `transformAExprIn` writes it where every element is a row of the
/// left's width or NULL: each element's pairs compared by `=` and and'd, NOT IN's by `<>` and or'd,
/// a row of one field its one comparison, an element NULL a NULL; the elements or'd, NOT IN's and'd.
/// None for anything else, which stays as written (`rows_in_refused`).
pub(super) fn rows_in(e: &Expr) -> Option<Expr> {
    let Expr::InList {
        expr: left,
        list,
        negated,
    } = e
    else {
        return None;
    };
    let Some(Ok(l)) = super::pipeline::row_parts(left) else {
        return None;
    };
    let (op, among) = match negated {
        false => (BinaryOperator::Eq, BinaryOperator::Or),
        true => (BinaryOperator::NotEq, BinaryOperator::And),
    };
    let mut arms = Vec::with_capacity(list.len());
    for x in list {
        match super::pipeline::row_parts(x) {
            Some(Ok(r)) if r.len() == l.len() => arms.push(paired(&l, &r, &op)?),
            None if is_null(x) => arms.push(Expr::Value(sqlparser::ast::Value::Null.into())),
            _ => return None,
        }
    }
    let written = arms.into_iter().reduce(|a, b| Expr::BinaryOp {
        left: Box::new(a),
        op: among.clone(),
        right: Box::new(b),
    })?;
    Some(Expr::Nested(Box::new(written)))
}

/// Two rows of one width compared by `=` or `<>`, written as `make_row_comparison_op` writes them:
/// the pairs' `=`s and'd, their `<>`s or'd; none for anything else.
pub(super) fn rows_compared(e: &Expr) -> Option<Expr> {
    let Expr::BinaryOp {
        left,
        op: op @ (BinaryOperator::Eq | BinaryOperator::NotEq),
        right,
    } = e
    else {
        return None;
    };
    let (Some(Ok(l)), Some(Ok(r))) = (
        super::pipeline::row_parts(left),
        super::pipeline::row_parts(right),
    ) else {
        return None;
    };
    (l.len() == r.len()).then(|| paired(&l, &r, op))?
}

/// Each pair of two rows' fields compared by `op`, in parentheses: and'd for `=`, or'd for `<>`.
/// None for rows of no fields.
fn paired(l: &[&Expr], r: &[&Expr], op: &BinaryOperator) -> Option<Expr> {
    let joined = match op {
        BinaryOperator::NotEq => BinaryOperator::Or,
        _ => BinaryOperator::And,
    };
    let grouped = |x: &Expr| match x {
        Expr::BinaryOp { .. } | Expr::UnaryOp { .. } => Expr::Nested(Box::new(x.clone())),
        x => x.clone(),
    };
    let written = l
        .iter()
        .zip(r)
        .map(|(x, y)| Expr::BinaryOp {
            left: Box::new(grouped(x)),
            op: op.clone(),
            right: Box::new(grouped(y)),
        })
        .reduce(|a, b| Expr::BinaryOp {
            left: Box::new(a),
            op: joined.clone(),
            right: Box::new(b),
        })?;
    Some(Expr::Nested(Box::new(written)))
}

/// Whether an expression is NULL written out, inside any parentheses.
fn is_null(e: &Expr) -> bool {
    match e {
        Expr::Nested(x) => is_null(x),
        Expr::Value(v) => v.value == sqlparser::ast::Value::Null,
        _ => false,
    }
}

/// What PostgreSQL's parser says of a row IN a list that stays as written, each element in turn: a
/// row of another width, or a row of none; `said` says it of each field of a row of the left's
/// width, given with the operator and the left's field it is compared with, and of an element that
/// is no row, given alone.
pub(super) fn rows_in_refused(
    e: &Expr,
    said: &mut dyn FnMut(&Expr, Option<(&BinaryOperator, &Expr)>) -> Result<()>,
) -> Result<()> {
    let Expr::InList {
        expr: left,
        list,
        negated,
    } = e
    else {
        return Ok(());
    };
    let Some(l) = super::pipeline::row_parts(left) else {
        return Ok(());
    };
    let l = l?;
    let op = match negated {
        false => BinaryOperator::Eq,
        true => BinaryOperator::NotEq,
    };
    for x in list {
        match super::pipeline::row_parts(x) {
            Some(r) => {
                let r = r?;
                super::image::rows_agree(l.len(), r.len())?;
                for (a, b) in l.iter().zip(&r) {
                    said(b, Some((&op, a)))?;
                }
            }
            None if is_null(x) => {}
            None => said(x, None)?,
        }
    }
    Ok(())
}

/// Two rows of one length compared by IS DISTINCT FROM or IS NOT DISTINCT FROM, written as
/// `make_row_distinct_op` writes them; none for anything else.
fn rows_distinct(e: &Expr) -> Option<Expr> {
    let (Expr::IsDistinctFrom(a, b) | Expr::IsNotDistinctFrom(a, b)) = e else {
        return None;
    };
    let (Some(Ok(x)), Some(Ok(y))) = (
        super::pipeline::row_parts(a),
        super::pipeline::row_parts(b),
    ) else {
        return None;
    };
    if x.len() != y.len() {
        return None;
    }
    // Where a row holds a set-returning function, which an OR or a NOT refuses, the pairs' truths
    // are the greatest of them, or for IS NOT DISTINCT FROM the least: none is ever NULL.
    if super::pipeline::holds_set(e) && x.len() > 1 {
        let (name, pair): (&str, fn(Box<Expr>, Box<Expr>) -> Expr) = match e {
            Expr::IsNotDistinctFrom(..) => ("least", Expr::IsNotDistinctFrom),
            _ => ("greatest", Expr::IsDistinctFrom),
        };
        let args = x
            .iter()
            .zip(&y)
            .map(|(l, r)| {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(pair(
                    Box::new((*l).clone()),
                    Box::new((*r).clone()),
                )))
            })
            .collect();
        return Some(Expr::Function(sqlparser::ast::Function {
            name: sqlparser::ast::ObjectName::from(vec![sqlparser::ast::Ident::new(name)]),
            uses_odbc_syntax: false,
            parameters: FunctionArguments::None,
            args: FunctionArguments::List(sqlparser::ast::FunctionArgumentList {
                duplicate_treatment: None,
                args,
                clauses: Vec::new(),
            }),
            filter: None,
            null_treatment: None,
            over: None,
            within_group: Vec::new(),
        }));
    }
    let any = x
        .iter()
        .zip(&y)
        .map(|(l, r)| Expr::IsDistinctFrom(Box::new((*l).clone()), Box::new((*r).clone())))
        .reduce(|acc, d| Expr::BinaryOp {
            left: Box::new(acc),
            op: BinaryOperator::Or,
            right: Box::new(d),
        })
        .unwrap_or(Expr::Value(sqlparser::ast::Value::Boolean(false).into()));
    let written = match e {
        Expr::IsNotDistinctFrom(..) => Expr::UnaryOp {
            op: UnaryOperator::Not,
            expr: Box::new(Expr::Nested(Box::new(any))),
        },
        _ => any,
    };
    Some(Expr::Nested(Box::new(written)))
}

/// PostgreSQL's WARNINGs where a statement names a time type of a precision past 6, which its type's
/// modifier input takes as 6 (`catalog::time_precision`): each in the order its analyser reads the
/// type names, then each its planner reads in the bodies of the file's functions it calls.
pub(crate) fn precisions_reduced(functions: &[crate::catalog::Function], sql: &str) -> Vec<String> {
    let mut out = precisions_named(sql);
    let first = super::statement::spans(sql).ok().and_then(|t| {
        t.into_iter().find_map(|(t, _, _)| match t {
            Token::Word(w) if w.quote_style.is_none() => Some(w.value.to_ascii_uppercase()),
            Token::Whitespace(_) => None,
            _ => Some(String::new()),
        })
    });
    // A statement the planner reads; a parenthesis opens a question.
    let plans = matches!(
        first.as_deref(),
        Some(
            "SELECT" | "INSERT" | "UPDATE" | "DELETE" | "MERGE" | "VALUES" | "WITH" | "TABLE"
                | "EXPLAIN" | ""
        )
    );
    if plans {
        out.extend(precisions_called(functions, sql, 0));
    }
    out
}

/// The words of each time type a statement names of a precision past 6, in the order written: a
/// CREATE TABLE's columns' types twice, as its columns are read and then laid out, before what
/// their expressions name; a CREATE FUNCTION's body of LANGUAGE sql after AS read after its head.
fn precisions_named(sql: &str) -> Vec<String> {
    let Ok(sql) = scanned_unseeded(sql, crate::regs::LATEST) else {
        return Vec::new();
    };
    let Ok(tokens) = super::statement::spans(&sql) else {
        return Vec::new();
    };
    let solid: Vec<&Token> = tokens
        .iter()
        .map(|t| &t.0)
        .filter(|t| !matches!(t, Token::Whitespace(_) | Token::EOF))
        .collect();
    let word = |i: usize| match solid.get(i) {
        Some(Token::Word(w)) if w.quote_style.is_none() => Some(w.value.to_ascii_uppercase()),
        _ => None,
    };
    let precision = |i: usize| match (solid.get(i), solid.get(i + 1), solid.get(i + 2)) {
        (Some(Token::LParen), Some(Token::Number(n, _)), Some(Token::RParen)) => {
            n.parse::<i64>().ok()
        }
        _ => None,
    };
    let zoned = |i: usize| {
        word(i).as_deref() == Some("WITH")
            && word(i + 1).as_deref() == Some("TIME")
            && word(i + 2).as_deref() == Some("ZONE")
    };
    let mut named: Vec<(usize, String)> = Vec::new();
    for i in 0..solid.len() {
        let w = word(i);
        let typed = match w.as_deref() {
            Some(k @ ("TIMESTAMP" | "TIME")) => precision(i + 1).map(|p| (k, p, zoned(i + 4))),
            Some("TIMESTAMPTZ" | "CURRENT_TIMESTAMP") => {
                precision(i + 1).map(|p| ("TIMESTAMP", p, true))
            }
            Some("LOCALTIMESTAMP") => precision(i + 1).map(|p| ("TIMESTAMP", p, false)),
            Some("TIMETZ" | "CURRENT_TIME") => precision(i + 1).map(|p| ("TIME", p, true)),
            Some("LOCALTIME") => precision(i + 1).map(|p| ("TIME", p, false)),
            // INTERVAL(p), or SECOND(p) last of its fields.
            Some("INTERVAL") => {
                let mut j = i + 1;
                while matches!(
                    word(j).as_deref(),
                    Some("YEAR" | "MONTH" | "DAY" | "HOUR" | "MINUTE" | "SECOND" | "TO")
                ) {
                    j += 1;
                }
                match j == i + 1 || word(j - 1).as_deref() == Some("SECOND") {
                    true => precision(j).map(|p| ("INTERVAL", p, false)),
                    false => None,
                }
            }
            _ => None,
        };
        if let Some((kind, p, zone)) = typed.filter(|t| t.1 > 6) {
            let zone = if zone { " WITH TIME ZONE" } else { "" };
            named.push((
                i,
                format!("WARNING:  {kind}({p}){zone} precision reduced to maximum allowed, 6"),
            ));
        }
    }
    let columns = column_types(&solid, &word);
    let (types, rest): (Vec<_>, Vec<_>) =
        named.into_iter().partition(|(i, _)| columns.contains(i));
    let mut out: Vec<String> = types
        .iter()
        .chain(&types)
        .chain(&rest)
        .map(|(_, w)| w.clone())
        .collect();
    // CREATE FUNCTION checks a body of LANGUAGE sql after AS as a statement.
    for body in sql_bodies(&sql) {
        out.extend(precisions_named(&body));
    }
    out
}

/// A string constant's text, where the token is one.
fn string_of(t: Option<&Token>) -> Option<String> {
    match t? {
        Token::SingleQuotedString(s) | Token::EscapedStringLiteral(s) => Some(s.clone()),
        Token::DollarQuotedString(d) => Some(d.value.clone()),
        _ => None,
    }
}

/// Where each column's type begins in a CREATE TABLE's list of its columns and constraints.
fn column_types(solid: &[&Token], word: &dyn Fn(usize) -> Option<String>) -> Vec<usize> {
    let mut i = 1;
    while matches!(
        word(i).as_deref(),
        Some("GLOBAL" | "LOCAL" | "TEMP" | "TEMPORARY" | "UNLOGGED")
    ) {
        i += 1;
    }
    if word(0).as_deref() != Some("CREATE") || word(i).as_deref() != Some("TABLE") {
        return Vec::new();
    }
    i += 1;
    if word(i).as_deref() == Some("IF") {
        i += 3;
    }
    // The table's name, qualified or not.
    i += 1;
    while solid.get(i) == Some(&&Token::Period) {
        i += 2;
    }
    if solid.get(i) != Some(&&Token::LParen) {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut depth = 0;
    let mut start = true;
    for (j, t) in solid.iter().enumerate().skip(i) {
        match t {
            Token::LParen => {
                depth += 1;
                if depth == 1 {
                    start = true;
                    continue;
                }
            }
            Token::RParen => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            Token::Comma if depth == 1 => {
                start = true;
                continue;
            }
            _ => {}
        }
        if start && depth == 1 {
            let constraint = matches!(
                word(j).as_deref(),
                Some("CONSTRAINT" | "PRIMARY" | "UNIQUE" | "CHECK" | "FOREIGN" | "EXCLUDE" | "LIKE")
            );
            if !constraint {
                out.push(j + 1);
            }
        }
        start = false;
    }
    out
}

/// The WARNINGs of each call of a file's function whose body is a string after AS, in the order
/// written: its body's own, then its body's calls', as the planner reads the body where it puts it
/// in the call's place.
fn precisions_called(
    functions: &[crate::catalog::Function],
    sql: &str,
    depth: usize,
) -> Vec<String> {
    let quoted = |f: &&crate::catalog::Function| f.form == crate::catalog::BodyForm::Quoted;
    if depth > 16 || !functions.iter().any(|f| quoted(&f)) {
        return Vec::new();
    }
    let Ok(tokens) = super::statement::spans(sql) else {
        return Vec::new();
    };
    let solid: Vec<&Token> = tokens
        .iter()
        .map(|t| &t.0)
        .filter(|t| !matches!(t, Token::Whitespace(_) | Token::EOF))
        .collect();
    let part = |i: usize| match solid.get(i) {
        Some(Token::Word(w)) if w.quote_style.is_some() => Some(w.value.clone()),
        Some(Token::Word(w)) => Some(w.value.to_lowercase()),
        _ => None,
    };
    let mut out = Vec::new();
    for i in 0..solid.len() {
        if solid.get(i + 1) != Some(&&Token::LParen) {
            continue;
        }
        let Some(name) = part(i) else { continue };
        let schema = (i >= 2 && solid[i - 1] == &Token::Period)
            .then(|| part(i - 2))
            .flatten();
        // A routine named where a statement names it, not called.
        let before = if schema.is_some() { i.wrapping_sub(3) } else { i.wrapping_sub(1) };
        if matches!(
            part(before).as_deref(),
            Some("function" | "procedure" | "routine" | "aggregate")
        ) {
            continue;
        }
        // Its arguments, counted by the commas outside any parenthesis within the call.
        let (mut inside, mut args, mut any) = (0, 1, false);
        for t in &solid[i + 2..] {
            match t {
                Token::LParen => inside += 1,
                Token::RParen if inside == 0 => break,
                Token::RParen => inside -= 1,
                Token::Comma if inside == 0 => args += 1,
                _ => {}
            }
            any = true;
        }
        let args = if any { args } else { 0 };
        let named = |f: &crate::catalog::Function| match &schema {
            Some(s) => f.name == format!("{s}.{name}"),
            None => f.name == name || f.name == format!("public.{name}"),
        };
        let Some(f) = functions.iter().filter(quoted).find(|f| {
            named(f) && args <= f.args.len() && args + f.defaults.len() >= f.args.len()
        }) else {
            continue;
        };
        out.extend(precisions_named(&f.source));
        out.extend(precisions_called(functions, &f.source, depth + 1));
    }
    out
}
