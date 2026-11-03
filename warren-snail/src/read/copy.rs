//! COPY, as PostgreSQL's: a table's rows or a question's answer written out, and rows read in, in
//! the text and CSV formats with their options, each value as its type's output function writes it
//! and its input function reads it.

use super::Snail;
use crate::{Error, Result};

/// Where COPY writes or reads.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum End {
    Stdout,
    Stdin,
    File(String),
}

/// A COPY statement as the snail reads it.
#[derive(Clone, Debug)]
pub struct Copy {
    /// The table and the columns it names, or the question.
    table: Option<(usize, Vec<usize>)>,
    question: Option<String>,
    to: bool,
    end: End,
    csv: bool,
    binary: bool,
    delimiter: char,
    null: String,
    header: Header,
    quote: char,
    escape: char,
    force_quote: Option<Vec<usize>>,
    /// FORCE_QUOTE *: every column quoted, a question's too.
    force_all: bool,
    force_not_null: Vec<usize>,
    force_null: Vec<usize>,
    default: Option<String>,
    /// ON_ERROR ignore, LOG_VERBOSITY (0 silent, 1 default, 2 verbose), REJECT_LIMIT, WHERE.
    on_error_ignore: bool,
    log_verbosity: u8,
    reject_limit: Option<i64>,
    where_: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Header {
    No,
    Yes,
    Match,
}

impl Copy {
    /// Whether it writes rows out.
    pub(super) fn to(&self) -> bool {
        self.to
    }

    /// Whether it reads rows from the caller.
    pub(super) fn from_stdin(&self) -> bool {
        !self.to && self.end == End::Stdin
    }
}

fn syntax_at(t: Option<&sqlparser::tokenizer::Token>) -> Error {
    Error::new(match t {
        Some(t) => format!("syntax error at or near \"{t}\""),
        None => "syntax error at end of input".to_string(),
    })
}

/// COPY's words, as PostgreSQL's grammar reads them, its options in either of its spellings: none
/// where the words are no COPY.
pub(super) fn parse_copy(
    catalog: &crate::catalog::Catalog,
    sql: &str,
) -> Option<Result<Copy>> {
    use sqlparser::tokenizer::Token;
    let spans = super::statement::spans(sql).ok()?;
    let w: Vec<&(Token, usize, usize)> = spans
        .iter()
        .filter(|(t, _, _)| !matches!(t, Token::Whitespace(_) | Token::EOF))
        .collect();
    let kw = |i: usize| match w.get(i).map(|x| &x.0) {
        Some(Token::Word(x)) if x.quote_style.is_none() => x.value.to_uppercase(),
        _ => String::new(),
    };
    if kw(0) != "COPY" {
        return None;
    }
    let release = catalog.release;
    Some((|| {
        let mut end_at = w.len();
        if matches!(w.last().map(|x| &x.0), Some(Token::SemiColon)) {
            end_at -= 1;
        }
        let tok = |i: usize| w.get(i).filter(|_| i < end_at).map(|x| &x.0);
        let mut i = 1;
        let mut binary = false;
        if kw(i) == "BINARY" {
            binary = true;
            i += 1;
        }
        let name_at = |i: usize| -> Option<String> {
            match tok(i) {
                Some(Token::Word(x)) => Some(match x.quote_style {
                    Some(_) => x.value.clone(),
                    None => x.value.to_lowercase(),
                }),
                _ => None,
            }
        };
        // The relation: a table and its columns, or a question in parentheses.
        let mut table = None;
        let mut question = None;
        let mut columns_named = Vec::new();
        if matches!(tok(i), Some(Token::LParen)) {
            let mut depth = 0;
            let start = w[i].2;
            let mut j = i;
            loop {
                match tok(j) {
                    Some(Token::LParen) => depth += 1,
                    Some(Token::RParen) => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    None => return Err(syntax_at(None)),
                    _ => {}
                }
                j += 1;
            }
            question = Some(sql[start..w[j].1].trim().to_string());
            i = j + 1;
        } else {
            let mut parts = Vec::new();
            loop {
                parts.push(name_at(i).ok_or_else(|| syntax_at(tok(i)))?);
                i += 1;
                if matches!(tok(i), Some(Token::Period)) {
                    i += 1;
                    continue;
                }
                break;
            }
            let n = parts.join(".");
            let ti = catalog
                .tables
                .iter()
                .position(|t| t.name == n)
                .ok_or_else(|| Error::new(format!("{n} is not in the snail")))?;
            if matches!(tok(i), Some(Token::LParen)) {
                i += 1;
                loop {
                    columns_named.push(name_at(i).ok_or_else(|| syntax_at(tok(i)))?);
                    i += 1;
                    match tok(i) {
                        Some(Token::Comma) => i += 1,
                        Some(Token::RParen) => {
                            i += 1;
                            break;
                        }
                        other => return Err(syntax_at(other)),
                    }
                }
            }
            let t = &catalog.tables[ti];
            let cols = if columns_named.is_empty() {
                (0..t.columns.len())
                    .filter(|&c| !t.columns[c].is_virtual())
                    .collect()
            } else {
                columns_named
                    .iter()
                    .map(|n| {
                        t.columns.iter().position(|c| c.name == *n).ok_or_else(|| {
                            Error::new(format!(
                                "column \"{n}\" of relation \"{}\" does not exist",
                                t.name.rsplit('.').next().unwrap_or_default()
                            ))
                        })
                    })
                    .collect::<Result<Vec<_>>>()?
            };
            table = Some((ti, cols));
        }
        let to = match kw(i).as_str() {
            "TO" => true,
            "FROM" => false,
            _ => return Err(syntax_at(tok(i))),
        };
        i += 1;
        let end = match (kw(i).as_str(), tok(i)) {
            ("STDOUT", _) => End::Stdout,
            ("STDIN", _) => End::Stdin,
            ("PROGRAM", _) => {
                return Err(Error::new(
                    "COPY … PROGRAM is refused: it runs a program as the server, and a snail has \
                     no server",
                ))
            }
            (_, Some(Token::SingleQuotedString(f))) => End::File(f.clone()),
            _ => return Err(syntax_at(tok(i))),
        };
        if question.is_some() && !to {
            return Err(syntax_at(tok(i - 1)));
        }
        if to && end == End::Stdin || !to && end == End::Stdout {
            return Err(syntax_at(tok(i)));
        }
        i += 1;
        let mut c = Copy {
            table,
            question,
            to,
            end,
            csv: false,
            binary,
            delimiter: '\t',
            null: "\\N".into(),
            header: Header::No,
            quote: '"',
            escape: '"',
            force_quote: None,
            force_all: false,
            force_not_null: Vec::new(),
            force_null: Vec::new(),
            default: None,
            on_error_ignore: false,
            log_verbosity: 1,
            reject_limit: None,
            where_: None,
        };
        let mut delimiter = None;
        let mut null = None;
        let mut quote = None;
        let mut escape = None;
        let mut force_quote_named: Option<Option<Vec<String>>> = None;
        let mut force_not_null_named = Vec::new();
        let mut force_null_named = Vec::new();
        let string_at = |i: usize| -> Result<String> {
            match tok(i) {
                Some(Token::SingleQuotedString(s)) | Some(Token::EscapedStringLiteral(s)) => {
                    Ok(s.clone())
                }
                other => Err(syntax_at(other)),
            }
        };
        if kw(i) == "WITH" {
            i += 1;
        }
        if matches!(tok(i), Some(Token::LParen)) {
            // The options in parentheses: each a name and its value.
            i += 1;
            loop {
                let opt = name_at(i).ok_or_else(|| syntax_at(tok(i)))?;
                i += 1;
                let mut value: Option<String> = None;
                let mut list: Option<Vec<String>> = None;
                match tok(i) {
                    Some(Token::Comma) | Some(Token::RParen) => {}
                    Some(Token::SingleQuotedString(s)) => {
                        value = Some(s.clone());
                        i += 1
                    }
                    Some(Token::Number(n, _)) => {
                        value = Some(n.clone());
                        i += 1
                    }
                    Some(Token::Mul) => {
                        value = Some("*".into());
                        i += 1
                    }
                    Some(Token::LParen) => {
                        i += 1;
                        let mut l = Vec::new();
                        loop {
                            l.push(name_at(i).ok_or_else(|| syntax_at(tok(i)))?);
                            i += 1;
                            match tok(i) {
                                Some(Token::Comma) => i += 1,
                                Some(Token::RParen) => {
                                    i += 1;
                                    break;
                                }
                                other => return Err(syntax_at(other)),
                            }
                        }
                        list = Some(l);
                    }
                    Some(Token::Word(_)) => {
                        value = name_at(i);
                        i += 1
                    }
                    other => return Err(syntax_at(other)),
                }
                let truth = |v: &Option<String>| -> Result<bool> {
                    match v.as_deref() {
                        None => Ok(true),
                        Some(v) => crate::read::truth(v).ok_or_else(|| {
                            Error::new(format!("{opt} requires a Boolean value"))
                        }),
                    }
                };
                // An option the file's release does not know is none of its options.
                if matches!(opt.as_str(), "on_error" | "log_verbosity") && release < 17
                    || opt == "reject_limit" && release < 18
                {
                    return Err(Error::new(format!("option \"{opt}\" not recognized")));
                }
                match opt.as_str() {
                    "format" => match value.as_deref() {
                        Some("text") => {}
                        Some("csv") => c.csv = true,
                        Some("binary") => c.binary = true,
                        Some(f) => {
                            return Err(Error::new(format!("COPY format \"{f}\" not recognized")))
                        }
                        None => return Err(Error::new("format requires a parameter")),
                    },
                    "freeze" => {
                        truth(&value)?;
                    }
                    "delimiter" => delimiter = value,
                    "null" => null = value,
                    "default" => c.default = value,
                    "header" => {
                        c.header = match value.as_deref() {
                            Some("match") if !c.to => Header::Match,
                            Some("match") => {
                                return Err(Error::new(
                                    "cannot use \"match\" with HEADER in COPY TO",
                                ))
                            }
                            _ if truth(&value)? => Header::Yes,
                            _ => Header::No,
                        }
                    }
                    "quote" => quote = value,
                    "escape" => escape = value,
                    "force_quote" => {
                        force_quote_named = Some(match value.as_deref() {
                            Some("*") => None,
                            _ => Some(list.unwrap_or_default()),
                        })
                    }
                    "force_not_null" => force_not_null_named = list.unwrap_or_default(),
                    "force_null" => force_null_named = list.unwrap_or_default(),
                    "encoding" => match value.as_deref().map(|v| v.to_ascii_lowercase().replace(['-', '_'], "")) {
                        Some(v) if v == "utf8" || v == "unicode" => {}
                        _ => {
                            return Err(Error::new(
                                "COPY … ENCODING is refused but for UTF8: the snail holds its text \
                                 in UTF8",
                            ))
                        }
                    },
                    "on_error" => match value.as_deref() {
                        Some("stop") => {}
                        Some("ignore") => c.on_error_ignore = true,
                        Some(v) => {
                            return Err(Error::new(format!(
                                "COPY ON_ERROR \"{v}\" not recognized"
                            )))
                        }
                        None => return Err(Error::new("on_error requires a parameter")),
                    },
                    "log_verbosity" => {
                        c.log_verbosity = match value.as_deref() {
                            Some("silent") if release >= 18 => 0,
                            Some("default") => 1,
                            Some("verbose") => 2,
                            Some(v) => {
                                return Err(Error::new(format!(
                                    "COPY LOG_VERBOSITY \"{v}\" not recognized"
                                )))
                            }
                            None => {
                                return Err(Error::new("log_verbosity requires a parameter"))
                            }
                        }
                    }
                    "reject_limit" => {
                        let n = value
                            .as_deref()
                            .and_then(|v| v.parse::<i64>().ok())
                            .ok_or_else(|| Error::new("reject_limit requires a numeric value"))?;
                        if n <= 0 {
                            return Err(Error::new(format!(
                                "REJECT_LIMIT ({n}) must be greater than zero"
                            )));
                        }
                        c.reject_limit = Some(n);
                    }
                    other => {
                        return Err(Error::new(format!("option \"{other}\" not recognized")))
                    }
                }
                match tok(i) {
                    Some(Token::Comma) => i += 1,
                    Some(Token::RParen) => {
                        i += 1;
                        break;
                    }
                    other => return Err(syntax_at(other)),
                }
            }
        } else {
            // The options as PostgreSQL before 9.0 wrote them.
            loop {
                match kw(i).as_str() {
                    "BINARY" => {
                        c.binary = true;
                        i += 1
                    }
                    "CSV" => {
                        c.csv = true;
                        i += 1
                    }
                    "HEADER" => {
                        c.header = Header::Yes;
                        i += 1
                    }
                    "DELIMITER" | "NULL" | "QUOTE" | "ESCAPE" => {
                        let which = kw(i);
                        i += 1;
                        if kw(i) == "AS" {
                            i += 1;
                        }
                        let v = Some(string_at(i)?);
                        i += 1;
                        match which.as_str() {
                            "DELIMITER" => delimiter = v,
                            "NULL" => null = v,
                            "QUOTE" => quote = v,
                            _ => escape = v,
                        }
                    }
                    "FORCE" => {
                        i += 1;
                        let which = kw(i);
                        let mut j = i + 1;
                        if which == "NOT" {
                            j += 1;
                        }
                        let mut l = Vec::new();
                        if matches!(tok(j), Some(Token::Mul)) {
                            force_quote_named = Some(None);
                            i = j + 1;
                            continue;
                        }
                        loop {
                            l.push(name_at(j).ok_or_else(|| syntax_at(tok(j)))?);
                            j += 1;
                            if matches!(tok(j), Some(Token::Comma)) {
                                j += 1;
                                continue;
                            }
                            break;
                        }
                        i = j;
                        match which.as_str() {
                            "QUOTE" => force_quote_named = Some(Some(l)),
                            "NOT" => force_not_null_named = l,
                            _ => return Err(syntax_at(tok(i))),
                        }
                    }
                    _ => break,
                }
            }
        }
        if kw(i) == "WHERE" && !c.to {
            let start = w[i].2;
            let stop = w.get(end_at).map_or(sql.len(), |x| x.1);
            c.where_ = Some(sql[start..stop].trim().to_string());
            i = end_at;
        }
        if i != end_at {
            return Err(syntax_at(tok(i)));
        }
        let one_char = |what: &str, v: Option<String>, default: char| -> Result<char> {
            match v {
                None => Ok(default),
                Some(s) if s.chars().count() == 1 => Ok(s.chars().next().unwrap()),
                Some(_) => Err(Error::new(format!(
                    "COPY {what} must be a single one-byte character"
                ))),
            }
        };
        if c.binary {
            for (what, given) in [
                ("DELIMITER", delimiter.is_some()),
                ("NULL", null.is_some()),
                ("DEFAULT", c.default.is_some()),
                ("HEADER", c.header != Header::No),
            ] {
                if given {
                    return Err(Error::new(format!("cannot specify {what} in BINARY mode")));
                }
            }
        }
        c.delimiter = one_char("delimiter", delimiter, if c.csv { ',' } else { '\t' })?;
        c.null = null.unwrap_or_else(|| if c.csv { String::new() } else { "\\N".into() });
        if !c.csv && (quote.is_some() || escape.is_some()) {
            return Err(Error::new(format!(
                "COPY {} requires CSV mode",
                if quote.is_some() { "QUOTE" } else { "ESCAPE" }
            )));
        }
        c.quote = one_char("quote", quote, '"')?;
        c.escape = one_char("escape", escape, c.quote)?;
        if c.delimiter == '\r' || c.delimiter == '\n' {
            return Err(Error::new(
                "COPY delimiter cannot be newline or carriage return",
            ));
        }
        if c.csv && c.delimiter == c.quote {
            return Err(Error::new(
                "COPY delimiter and quote must be different",
            ));
        }
        if c.null.contains(c.delimiter) {
            return Err(Error::new(
                "COPY delimiter character must not appear in the NULL specification",
            ));
        }
        let column_of = |n: &String| -> Result<usize> {
            let (ti, cols) = c.table.as_ref().ok_or_else(|| {
                Error::new("COPY FORCE names columns of a table")
            })?;
            let t = &catalog.tables[*ti];
            cols.iter()
                .copied()
                .find(|&k| t.columns[k].name == *n)
                .ok_or_else(|| {
                    Error::new(format!("column \"{n}\" is not referenced by COPY"))
                })
        };
        if let Some(named) = force_quote_named {
            if !c.csv {
                return Err(Error::new("COPY FORCE_QUOTE requires CSV mode"));
            }
            if !c.to {
                return Err(Error::new("COPY FORCE_QUOTE cannot be used with COPY FROM"));
            }
            c.force_all = named.is_none();
            c.force_quote = Some(match named {
                None => c.table.as_ref().map_or(Vec::new(), |(_, cols)| cols.clone()),
                Some(l) => l.iter().map(column_of).collect::<Result<_>>()?,
            });
        }
        if !force_not_null_named.is_empty() || !force_null_named.is_empty() {
            if !c.csv {
                return Err(Error::new("COPY FORCE_NOT_NULL requires CSV mode"));
            }
            if c.to {
                return Err(Error::new(
                    "COPY FORCE_NOT_NULL cannot be used with COPY TO",
                ));
            }
            c.force_not_null = force_not_null_named.iter().map(column_of).collect::<Result<_>>()?;
            c.force_null = force_null_named.iter().map(column_of).collect::<Result<_>>()?;
        }
        if c.default.is_some() && c.to {
            return Err(Error::new("COPY DEFAULT cannot be used with COPY TO"));
        }
        if c.on_error_ignore && c.to {
            return Err(Error::new("COPY ON_ERROR cannot be used with COPY TO"));
        }
        if c.on_error_ignore && c.binary {
            return Err(Error::new("only ON_ERROR STOP is allowed in BINARY mode"));
        }
        if c.reject_limit.is_some() && !c.on_error_ignore {
            return Err(Error::new(
                "COPY REJECT_LIMIT requires ON_ERROR to be set to IGNORE",
            ));
        }
        Ok(c)
    })())
}

/// A value written in COPY's text format: a backslash before each byte that would end it.
fn text_field(v: &str, delimiter: char, out: &mut String) {
    for ch in v.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' if delimiter == '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\u{b}' => out.push_str("\\v"),
            c if c == delimiter => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
}

/// A value written in COPY's CSV format: quoted where it holds the delimiter, the quote, a line's
/// end, or reads as the NULL string or the end of data, or where `force`.
fn csv_field(v: &str, c: &Copy, force: bool, out: &mut String) {
    let needs = force
        || v.is_empty() && c.null.is_empty()
        || v == c.null
        || v == "\\."
        || v.chars().any(|ch| ch == c.delimiter || ch == c.quote || ch == '\n' || ch == '\r');
    if !needs {
        out.push_str(v);
        return;
    }
    out.push(c.quote);
    for ch in v.chars() {
        if ch == c.quote || ch == c.escape {
            out.push(c.escape);
        }
        out.push(ch);
    }
    out.push(c.quote);
}

/// One line of COPY's text format read into its fields: each unescaped, or none where it is the
/// NULL string as written.
fn text_line(line: &str, c: &Copy) -> Result<Vec<Option<String>>> {
    let mut fields = Vec::new();
    for raw in split_unescaped(line, c.delimiter) {
        if raw == c.null {
            fields.push(None);
            continue;
        }
        let mut v = String::new();
        let mut chars = raw.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch != '\\' {
                v.push(ch);
                continue;
            }
            match chars.next() {
                Some('n') => v.push('\n'),
                Some('r') => v.push('\r'),
                Some('t') => v.push('\t'),
                Some('b') => v.push('\u{8}'),
                Some('f') => v.push('\u{c}'),
                Some('v') => v.push('\u{b}'),
                Some(d @ '0'..='7') => {
                    let mut n = d.to_digit(8).unwrap();
                    for _ in 0..2 {
                        match chars.peek() {
                            Some(&e @ '0'..='7') => {
                                n = n * 8 + e.to_digit(8).unwrap();
                                chars.next();
                            }
                            _ => break,
                        }
                    }
                    v.push(char::from_u32(n & 0xff).unwrap_or('\u{fffd}'));
                }
                Some('x') => {
                    let mut n = 0;
                    let mut k = 0;
                    while k < 2 {
                        match chars.peek().and_then(|c| c.to_digit(16)) {
                            Some(d) => {
                                n = n * 16 + d;
                                chars.next();
                                k += 1;
                            }
                            None => break,
                        }
                    }
                    if k == 0 {
                        v.push('x');
                    } else {
                        v.push(char::from_u32(n).unwrap_or('\u{fffd}'));
                    }
                }
                Some(o) => v.push(o),
                None => {}
            }
        }
        fields.push(Some(v));
    }
    Ok(fields)
}

/// A text-format line cut at each delimiter no backslash escapes.
fn split_unescaped(line: &str, delimiter: char) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut escaped = false;
    for (i, ch) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
        } else if ch == delimiter {
            out.push(&line[start..i]);
            start = i + ch.len_utf8();
        }
    }
    out.push(&line[start..]);
    out
}

/// COPY's CSV rows read into their fields, a quoted field across lines among them: each with
/// whether it was quoted.
fn csv_rows(data: &str, c: &Copy) -> Result<Vec<Vec<(String, bool)>>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut in_quotes = false;
    let mut any = false;
    let mut chars = data.chars().peekable();
    while let Some(ch) = chars.next() {
        if in_quotes {
            // The escape before a quote, or a doubled quote where they are one, is a quote; a lone
            // quote ends the field.
            if ch == c.escape && chars.peek() == Some(&c.quote) {
                chars.next();
                field.push(c.quote);
                continue;
            }
            if ch == c.escape && c.escape != c.quote && chars.peek() == Some(&c.escape) {
                chars.next();
                field.push(c.escape);
                continue;
            }
            if ch == c.quote {
                in_quotes = false;
                continue;
            }
            field.push(ch);
            continue;
        }
        any = true;
        match ch {
            q if q == c.quote => {
                in_quotes = true;
                quoted = true;
            }
            d if d == c.delimiter => {
                row.push((std::mem::take(&mut field), quoted));
                quoted = false;
            }
            '\r' | '\n' => {
                if ch == '\r' && chars.peek() == Some(&'\n') {
                    chars.next();
                }
                row.push((std::mem::take(&mut field), quoted));
                quoted = false;
                if !(row.len() == 1 && row[0].0 == "\\." && !row[0].1) {
                    rows.push(std::mem::take(&mut row));
                } else {
                    row.clear();
                    break;
                }
                any = false;
            }
            other => field.push(other),
        }
    }
    if in_quotes {
        return Err(Error::new("unterminated CSV quoted field"));
    }
    if any || !field.is_empty() || !row.is_empty() {
        row.push((field, quoted));
        if !(row.len() == 1 && row[0].0 == "\\." && !row[0].1) {
            rows.push(row);
        }
    }
    Ok(rows)
}

    /// COPY TO: the rows, as COPY writes them, written where it names, or handed back.
    pub(super) fn copy_out(
        c: &Copy,
        catalog: &crate::catalog::Catalog,
        ask: &mut dyn FnMut(&str) -> Result<arrow::array::RecordBatch>,
    ) -> Result<super::Done> {
        let (question, names) = match (&c.table, &c.question) {
            (Some((ti, cols)), _) => {
                let t = &catalog.tables[*ti];
                let by = t.name.rsplit('.').next().unwrap_or_default();
                let list: Vec<String> = cols
                    .iter()
                    .map(|&k| format!("\"{by}\".\"{}\"", t.columns[k].name.replace('"', "\"\"")))
                    .collect();
                (
                    format!("SELECT {} FROM {}", list.join(", "), t.name),
                    cols.iter().map(|&k| t.columns[k].name.clone()).collect(),
                )
            }
            (None, Some(q)) => (q.clone(), Vec::new()),
            _ => unreachable!("COPY names a table or a question"),
        };
        let batch = ask(&question)?;
        // Each column's type as the answer holds it, an integer's as PostgreSQL types the question.
        let stated = super::change::stated_types(catalog, &question);
        let types: Vec<crate::catalog::ColumnType> = super::change::answer_types(&batch)?
            .into_iter()
            .enumerate()
            .map(|(k, held)| match (held, stated.get(k).copied().flatten()) {
                // An integer the answer holds in 64 bits is written in its own width.
                (
                    crate::catalog::ColumnType::Int8,
                    Some(s @ (crate::catalog::ColumnType::Int2 | crate::catalog::ColumnType::Int4)),
                ) => s,
                _ => held,
            })
            .collect();
        let rows = super::change::rows_of(&batch)?;
        let names: Vec<String> = match names.is_empty() {
            true => batch.schema().fields().iter().map(|f| f.name().clone()).collect(),
            false => names,
        };
        if c.binary {
            let mut out = super::binary::HEADER.to_vec();
            for r in &rows {
                out.extend((r.len() as i16).to_be_bytes());
                for (k, d) in r.iter().enumerate() {
                    match d {
                        super::statement::Datum::Null => out.extend((-1i32).to_be_bytes()),
                        d => {
                            let b = super::binary::send(types[k], d)?;
                            out.extend((b.len() as i32).to_be_bytes());
                            out.extend(b);
                        }
                    }
                }
            }
            out.extend((-1i16).to_be_bytes());
            return copied(c, out, rows.len() as u64);
        }
        let forced: Vec<bool> = match (&c.force_quote, &c.table) {
            _ if c.force_all => vec![true; names.len()],
            (Some(f), Some((_, cols))) => cols.iter().map(|k| f.contains(k)).collect(),
            _ => vec![false; names.len()],
        };
        let mut out = String::new();
        let write = |v: &str, k: usize, out: &mut String| match c.csv {
            true => csv_field(v, c, forced.get(k).copied().unwrap_or(false), out),
            false => text_field(v, c.delimiter, out),
        };
        if c.header == Header::Yes {
            for (k, n) in names.iter().enumerate() {
                if k > 0 {
                    out.push(c.delimiter);
                }
                write(n, k, &mut out);
            }
            out.push('\n');
        }
        for r in &rows {
            for (k, d) in r.iter().enumerate() {
                if k > 0 {
                    out.push(c.delimiter);
                }
                match d {
                    super::statement::Datum::Null => out.push_str(&c.null),
                    d => write(&crate::composed::scalar_out(types[k], d), k, &mut out),
                }
            }
            out.push('\n');
        }
        copied(c, out.into_bytes(), rows.len() as u64)
    }

    /// What COPY TO wrote: into the file it names, or handed back.
    fn copied(c: &Copy, out: Vec<u8>, n: u64) -> Result<super::Done> {
        match &c.end {
            End::File(f) => {
                std::fs::write(f, &out).map_err(|e| {
                    Error::new(format!("could not open file \"{f}\" for writing: {e}"))
                })?;
                Ok(super::Done {
                    statement: "COPY",
                    rows: n,
                    ..Default::default()
                })
            }
            _ => Ok(super::Done {
                statement: "COPY",
                rows: n,
                copied: Some(out),
                ..Default::default()
            }),
        }
    }

    /// COPY FROM: the rows read from the caller's bytes or the file, each value by its column's
    /// input function, written as one INSERT of them all would write them.
    pub(super) fn copy_in(
        c: &Copy,
        catalog: &crate::catalog::Catalog,
        given: Option<&[u8]>,
        exec: &mut dyn FnMut(&str) -> Result<super::Done>,
    ) -> Result<super::Done> {
        let bytes = match (&c.end, given) {
            (End::File(f), _) => std::fs::read(f).map_err(|e| {
                Error::new(format!("could not open file \"{f}\" for reading: {e}"))
            })?,
            (End::Stdin, Some(b)) => b.to_vec(),
            _ => {
                return Err(Error::new(
                    "COPY … FROM STDIN takes its rows from the caller: exec reads them after the \
                     statement, up to a line \\., as psql does",
                ))
            }
        };
        let (ti, cols) = c.table.clone().expect("COPY FROM names a table");
        let t = &catalog.tables[ti];
        // Each row's fields: the NULL string, or a value.
        let mut rows: Vec<Vec<Option<String>>> = Vec::new();
        let data = match c.binary {
            true => {
                rows = binary_rows(&bytes, &cols.iter().map(|&k| t.columns[k].ty).collect::<Vec<_>>())?;
                String::new()
            }
            false => String::from_utf8(bytes)
                .map_err(|_| Error::new("invalid byte sequence for encoding \"UTF8\""))?,
        };
        if c.binary {
        } else if c.csv {
            for r in csv_rows(&data, c)? {
                rows.push(
                    r.into_iter()
                        .enumerate()
                        .map(|(k, (v, quoted))| {
                            let col = cols.get(k).copied();
                            let not_null = col.is_some_and(|x| c.force_not_null.contains(&x));
                            let force_null = col.is_some_and(|x| c.force_null.contains(&x));
                            if (!quoted && v == c.null && !not_null) || (force_null && v == c.null) {
                                None
                            } else {
                                Some(v)
                            }
                        })
                        .collect(),
                );
            }
        } else {
            for line in data.split('\n') {
                let line = line.strip_suffix('\r').unwrap_or(line);
                if line == "\\." {
                    break;
                }
                rows.push(text_line(line, c)?);
            }
            if rows.last().is_some_and(|r| r.len() == 1 && r[0].as_deref() == Some("")) {
                rows.pop();
            }
        }
        let mut header = c.header != Header::No;
        let mut values = Vec::new();
        let (mut skipped, mut notices) = (0u64, Vec::new());
        let where_ = match &c.where_ {
            None => None,
            Some(w) => match super::image::parse_sql(&format!("SELECT 1 WHERE {w}"))?.as_slice() {
                [sqlparser::ast::Statement::Query(q)] => match q.body.as_ref() {
                    sqlparser::ast::SetExpr::Select(s) => s.selection.clone(),
                    _ => None,
                },
                _ => None,
            },
        };
        for (n, r) in rows.into_iter().enumerate() {
            if header {
                header = false;
                if c.header == Header::Match {
                    let names: Vec<&str> = cols.iter().map(|&k| t.columns[k].name.as_str()).collect();
                    let got: Vec<&str> = r.iter().map(|f| f.as_deref().unwrap_or("")).collect();
                    if got != names {
                        return Err(Error::new(format!(
                            "column name mismatch in header line field {}: got \"{}\", expected \"{}\"",
                            got.iter().zip(&names).position(|(a, b)| a != b).unwrap_or(got.len().min(names.len())) + 1,
                            got.iter().zip(&names).find(|(a, b)| a != b).map_or("", |x| x.0),
                            got.iter().zip(&names).find(|(a, b)| a != b).map_or("", |x| x.1)
                        )));
                    }
                }
                continue;
            }
            if r.len() > cols.len() {
                return Err(Error::new(format!(
                    "extra data after last expected column (line {})",
                    n + 1
                )));
            }
            if r.len() < cols.len() {
                return Err(Error::new(format!(
                    "missing data for column \"{}\"",
                    t.columns[cols[r.len()]].name
                )));
            }
            // Each value read by its column's input: under ON_ERROR ignore a row one fails is
            // skipped, said where LOG_VERBOSITY is verbose.
            let mut typed = Vec::with_capacity(r.len());
            let mut failed = None;
            let checked = c.on_error_ignore || where_.is_some();
            for (f, &k) in r.iter().zip(&cols).filter(|_| checked) {
                let Some(v) = f.as_deref().filter(|v| c.default.as_deref() != Some(*v)) else {
                    typed.push(super::pipeline::Value::Null);
                    continue;
                };
                let literal = sqlparser::ast::Expr::value(sqlparser::ast::Value::SingleQuotedString(
                    v.to_string(),
                ));
                match super::statement::datum(&literal, t, k) {
                    Ok(_) if where_.is_some() => typed.push(cast_value(v, t.columns[k].ty, t)?),
                    Ok(_) => {}
                    Err(e) => {
                        failed = Some((k, v.to_string(), e));
                        break;
                    }
                }
            }
            if let Some((k, v, e)) = failed {
                if !c.on_error_ignore {
                    return Err(e);
                }
                skipped += 1;
                if c.log_verbosity == 2 {
                    notices.push(format!(
                        "NOTICE:  skipping row due to data type incompatibility at line {} for column \"{}\": \"{v}\"",
                        n + 1,
                        t.columns[k].name
                    ));
                }
                if c.reject_limit.is_some_and(|l| skipped > l as u64) {
                    return Err(Error::new(format!(
                        "skipped more than REJECT_LIMIT ({}) rows due to data type incompatibility",
                        c.reject_limit.unwrap()
                    )));
                }
                continue;
            }
            if let Some(w) = &where_ {
                let names: Vec<String> = cols.iter().map(|&k| t.columns[k].name.clone()).collect();
                let casings = super::pipeline::Casings::under(t);
                let kept = super::pipeline::eval_on(w, &names, &typed, t.release, &casings)?;
                if !matches!(kept, super::pipeline::Value::Bool(true)) {
                    continue;
                }
            }
            let row: Vec<String> = r
                .iter()
                .map(|f| match f {
                    None => "NULL".to_string(),
                    Some(v) if c.default.as_deref() == Some(v.as_str()) => "DEFAULT".to_string(),
                    Some(v) => format!("'{}'", v.replace('\'', "''")),
                })
                .collect();
            values.push(format!("({})", row.join(", ")));
        }
        if skipped > 0 && c.log_verbosity >= 1 {
            notices.push(match skipped {
                1 => "NOTICE:  1 row was skipped due to data type incompatibility".to_string(),
                k => format!("NOTICE:  {k} rows were skipped due to data type incompatibility"),
            });
        }
        let n = values.len() as u64;
        if n > 0 {
            let names: Vec<String> = cols
                .iter()
                .map(|&k| format!("\"{}\"", t.columns[k].name.replace('"', "\"\"")))
                .collect();
            let insert = format!(
                "INSERT INTO {} ({}) VALUES {}",
                t.name,
                names.join(", "),
                values.join(", ")
            );
            let d = exec(&insert)?;
            return Ok(super::Done {
                statement: "COPY",
                rows: d.rows,
                notices,
                ..d
            });
        }
        Ok(super::Done {
            statement: "COPY",
            notices,
            ..Default::default()
        })
    }

impl Snail {
    pub(super) fn copy_to(&mut self, c: &Copy) -> Result<super::Done> {
        let catalog = self.catalog.clone();
        copy_out(c, &catalog, &mut |q| self.ask(q, None).map(|(b, _)| b))
    }

    pub(super) fn copy_from(&mut self, c: &Copy, given: Option<&[u8]>) -> Result<super::Done> {
        let catalog = self.catalog.clone();
        copy_in(c, &catalog, given, &mut |q| self.execute(q))
    }

    /// Runs COPY … FROM STDIN with the rows `input` holds, as a client's COPY data hands them.
    pub fn execute_copy(&mut self, sql: &str, input: &[u8]) -> Result<super::Done> {
        self.session.copy_input = Some(input.to_vec());
        let done = self.execute(sql);
        self.session.copy_input = None;
        done
    }
}

/// A value read in by its column's type, as a step's expression holds it.
fn cast_value(
    v: &str,
    ty: crate::catalog::ColumnType,
    t: &crate::catalog::Table,
) -> Result<super::pipeline::Value> {
    let e = match super::image::parse_sql(&format!(
        "SELECT CAST('{}' AS {})",
        v.replace('\'', "''"),
        ty.pg_name()
    ))?
    .as_slice()
    {
        [sqlparser::ast::Statement::Query(q)] => match q.body.as_ref() {
            sqlparser::ast::SetExpr::Select(s) => match s.projection.first() {
                Some(sqlparser::ast::SelectItem::UnnamedExpr(e)) => e.clone(),
                _ => return Err(Error::new("a value COPY reads is no expression")),
            },
            _ => return Err(Error::new("a value COPY reads is no expression")),
        },
        _ => return Err(Error::new("a value COPY reads is no expression")),
    };
    super::pipeline::eval_on(&e, &[], &[], t.release, &super::pipeline::Casings::under(t))
}

/// COPY's binary rows read into their fields: each NULL, or the text its column's input reads, as
/// its type's receive function reads its bytes.
fn binary_rows(b: &[u8], types: &[crate::catalog::ColumnType]) -> Result<Vec<Vec<Option<String>>>> {
    let bad = || Error::new("COPY file signature not recognized");
    let sig = &super::binary::HEADER[..11];
    if b.len() < 19 || &b[..11] != sig {
        return Err(bad());
    }
    let flags = u32::from_be_bytes(b[11..15].try_into().unwrap());
    if flags & 0xffff_0000 != 0 {
        return Err(Error::new("unrecognized critical flags in COPY file header"));
    }
    let ext = u32::from_be_bytes(b[15..19].try_into().unwrap()) as usize;
    let mut at = 19 + ext;
    let early = || Error::new("unexpected EOF in COPY data");
    let mut rows = Vec::new();
    loop {
        let n = i16::from_be_bytes(b.get(at..at + 2).ok_or_else(early)?.try_into().unwrap());
        at += 2;
        if n == -1 {
            return Ok(rows);
        }
        if n as usize != types.len() {
            return Err(Error::new(format!(
                "row field count is {n}, expected {}",
                types.len()
            )));
        }
        let mut row = Vec::with_capacity(types.len());
        for &ty in types {
            let len = i32::from_be_bytes(b.get(at..at + 4).ok_or_else(early)?.try_into().unwrap());
            at += 4;
            if len < 0 {
                row.push(None);
                continue;
            }
            let v = b.get(at..at + len as usize).ok_or_else(early)?;
            at += len as usize;
            row.push(Some(super::binary::recv(ty, v)?));
        }
        rows.push(row);
    }
}
