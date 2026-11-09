//! The literals PostgreSQL reads once, where their DDL statement is declared, and keeps as their
//! values: in a CHECK, a DEFAULT, a generated column, a domain's CHECK and DEFAULT, an index's keys
//! and predicate, and a view's query, a quoted literal coerced to a type whose input reads the
//! session's zone (timestamptz and timetz, and a domain, array, range, multirange or composite of
//! them), or one naming the clock (`now`, `today`, `tomorrow`, `yesterday`) coerced to a type whose
//! input reads the clock (those, date, time and timestamp). A snail keeps its DDL as text and reads
//! it again where each statement runs, so each such literal is written in the kept statement as its
//! value, as its type's output writes it under the zone and on the clock the statement runs under,
//! and every later read gives the value PostgreSQL keeps.
//!
//! A literal's type is the analyzer's (`analyze`), read as PostgreSQL's parser reads it: a copy of
//! the statement, each literal that may be one written as a mark, is declared after the statements
//! before it, and each mark's type taken where the analyzer coerces it.

use super::analyze::{mark, Marking};
use super::cat::Cat;
use crate::catalog::Catalog;
use crate::oids::{full, parts, Object};
use crate::{Error, Result};
use sqlparser::tokenizer::Token;
use std::borrow::Cow;
use std::collections::HashMap;
use std::ops::Range;

const DATE: u32 = 1082;
const TIME: u32 = 1083;
const TIMESTAMP: u32 = 1114;
const TIMESTAMPTZ: u32 = 1184;
const TIMETZ: u32 = 1266;
const TSRANGE: u32 = 3908;
const TSTZRANGE: u32 = 3910;
const DATERANGE: u32 = 3912;
const TSMULTIRANGE: u32 = 4533;
const TSTZMULTIRANGE: u32 = 4534;
const DATEMULTIRANGE: u32 = 4535;

/// What a type's input reads besides a literal's text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Reads {
    Nothing,
    Clock,
    Zone,
}

/// A literal of a statement that may be one PostgreSQL folds: its text, and where it stands in the
/// statement's scanned text, in bytes.
struct Literal {
    text: String,
    at: Range<usize>,
}

/// A DDL with each literal PostgreSQL keeps as its value written as that value, in its statements
/// from the `from`th on, each read after the statements before it, under the zone and on the clock
/// of the statement running: its first `kept` statements a file's own, as `Catalog::parse_kept`
/// reads them, and each statement's catalog made as the snail makes it by `prepare`. As written
/// where none is, and where the DDL is one the snail refuses, which its reading then says.
pub(crate) fn folded<'a>(
    ddl: &'a str,
    from: usize,
    kept: usize,
    prepare: &dyn Fn(&mut Catalog) -> Result<()>,
) -> Result<Cow<'a, str>> {
    if !ddl.contains(['\'', '$']) {
        return Ok(Cow::Borrowed(ddl));
    }
    let Ok(pieces) = statements(ddl) else {
        return Ok(Cow::Borrowed(ddl));
    };
    let mut out = String::with_capacity(ddl.len());
    let mut written = 0;
    let mut changed = false;
    for (n, piece) in pieces.into_iter().enumerate() {
        if n < from {
            continue;
        }
        let text = &ddl[piece.clone()];
        if !text.contains(['\'', '$']) {
            continue;
        }
        out.push_str(&ddl[written..piece.start]);
        written = piece.start;
        if let Some(f) = statement_folded(&out, text, kept, prepare)? {
            out.push_str(&f);
            written = piece.end;
            changed = true;
        }
    }
    if !changed {
        return Ok(Cow::Borrowed(ddl));
    }
    out.push_str(&ddl[written..]);
    Ok(Cow::Owned(out))
}

/// Each statement of a DDL that holds more than whitespace and comments, where it stands in the
/// text without its semicolon.
fn statements(ddl: &str) -> Result<Vec<Range<usize>>> {
    let mut out = Vec::new();
    let mut open: Option<Range<usize>> = None;
    for (t, a, b) in crate::read::statement::spans(ddl)? {
        match t {
            Token::SemiColon => out.extend(open.take()),
            Token::Whitespace(_) => {}
            _ => match &mut open {
                Some(r) => r.end = b,
                None => open = Some(a..b),
            },
        }
    }
    out.extend(open);
    Ok(out)
}

/// A statement read after `before` with each literal PostgreSQL keeps as its value written as that
/// value, in its scanned text; none where none is, or where the statement is one the snail refuses.
fn statement_folded(
    before: &str,
    statement: &str,
    kept: usize,
    prepare: &dyn Fn(&mut Catalog) -> Result<()>,
) -> Result<Option<String>> {
    let Ok(scanned) = crate::read::scanned_unseeded(statement, crate::regs::LATEST) else {
        return Ok(None);
    };
    let Ok(tokens) = crate::read::statement::spans(&scanned) else {
        return Ok(None);
    };
    let literals: Vec<Literal> = tokens
        .into_iter()
        .filter_map(|(t, a, b)| {
            let text = match t {
                Token::SingleQuotedString(s)
                | Token::EscapedStringLiteral(s)
                | Token::NationalStringLiteral(s)
                | Token::UnicodeStringLiteral(s) => s,
                Token::DollarQuotedString(d) => d.value,
                _ => return None,
            };
            may_fold(&text).then_some(Literal { text, at: a..b })
        })
        .collect();
    if literals.is_empty() {
        return Ok(None);
    }
    let copy = spliced(&scanned, &literals, |k, _| super::print::literal(&mark(k)));
    // Wherever the DDL's reader reads a literal's value, it reads the copy's by the literal each
    // mark stands for.
    let marking = Marking::of(literals.iter().map(|l| l.text.clone()).collect());
    let catalog = match declared(&format!("{before}{copy};\n"), kept, prepare) {
        Ok(c) => c,
        // Where the copy is refused, the statement is refused as the snail reads it, or refused
        // only as a copy, which nothing then folds.
        Err(e) => {
            return match declared(&format!("{before}{scanned};\n"), kept, prepare) {
                Err(_) => Ok(None),
                Ok(_) => Err(not_folded(&literals[0].text, &e)),
            }
        }
    };
    let values = values(&catalog, marking)?;
    if values.iter().all(Option::is_none) {
        return Ok(None);
    }
    Ok(Some(spliced(&scanned, &literals, |k, l| {
        values[k].clone().unwrap_or_else(|| scanned[l.at.clone()].to_string())
    })))
}

/// A text with each literal's place written by `with`.
fn spliced(text: &str, literals: &[Literal], with: impl Fn(usize, &Literal) -> String) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for (k, l) in literals.iter().enumerate() {
        out.push_str(&text[at..l.at.start]);
        out.push_str(&with(k, l));
        at = l.at.end;
    }
    out.push_str(&text[at..]);
    out
}

/// A DDL's catalog, its first `kept` statements a file's own, made as the snail makes it.
fn declared(ddl: &str, kept: usize, prepare: &dyn Fn(&mut Catalog) -> Result<()>) -> Result<Catalog> {
    let mut catalog = Catalog::parse_kept(ddl, kept)?;
    prepare(&mut catalog)?;
    Ok(catalog)
}

/// Whether a literal's text may be one PostgreSQL folds: an instant or a time naming no zone of
/// its own, `today`, `tomorrow` and `yesterday` among them, or `now`, in each bound or element
/// where it is a range's or an array's. A function's body never reads as one.
fn may_fold(text: &str) -> bool {
    let now = |p: &str| p.trim().trim_matches('"').trim().eq_ignore_ascii_case("now");
    let t = text.trim();
    let pieces = match t.as_bytes().first() {
        Some(b'[' | b'(' | b'{') => t[1..].trim_end_matches([']', ')', '}']).split(',').any(now),
        _ => now(t),
    };
    crate::times::reads_on_the_zone(text) || pieces
}

/// Whether a text names the clock, in any of its words: `now`, `today`, `tomorrow`, `yesterday`.
fn names_the_clock(text: &str) -> bool {
    text.split(|c: char| !c.is_ascii_alphabetic()).any(|w| {
        ["now", "today", "tomorrow", "yesterday"]
            .iter()
            .any(|k| w.eq_ignore_ascii_case(k))
    })
}

fn not_folded(text: &str, e: &Error) -> Error {
    Error::new(format!(
        "the literal '{text}' is one PostgreSQL reads once, where its statement is declared, and \
         keeps as its value, and the snail could not read its type to keep that value ({}); write \
         the value it stands for, with its offset where it has one",
        e.0
    ))
}

/// Each literal's value, where it is one PostgreSQL folds, as its type's output writes it, quoted:
/// the catalog's expressions read by the analyzer, each holding a mark.
fn values(catalog: &Catalog, marking: Marking) -> Result<Vec<Option<String>>> {
    let n = marking.len();
    let marks: Vec<String> = (0..n).map(mark).collect();
    let marked_in = |s: &str| -> Vec<usize> { (0..n).filter(|&k| s.contains(&marks[k])).collect() };
    let cat = Cat::of(catalog);
    // Each expression read: the marks it holds, and what its reading gave.
    let mut read: Vec<(Vec<usize>, Result<()>)> = Vec::new();
    let mut tree = |s: &str, f: &dyn Fn() -> Result<()>| {
        let held = marked_in(s);
        if !held.is_empty() {
            read.push((held, f()));
        }
    };
    for t in catalog.tables.iter().filter(|t| t.declared()) {
        let Some(rel) = catalog.oids.of(&Object::Class(full(&t.name))) else {
            continue;
        };
        for (i, c) in t.columns.iter().enumerate() {
            let expr = c.default.as_ref().or(c.generated.as_ref().map(|g| &g.expr));
            if let Some(e) = expr {
                tree(&e.to_string(), &|| {
                    super::stored_tree(&cat, "attrdef", rel, i as i64 + 1).map(drop)
                });
            }
        }
        for (i, k) in t.checks.iter().enumerate() {
            tree(&k.expr.to_string(), &|| {
                super::stored_tree(&cat, "check", rel, i as i64).map(drop)
            });
        }
        let schema = parts(&full(&t.name)).0.to_string();
        let index = |name: &str| catalog.oids.of(&Object::Class(format!("{schema}.{name}")));
        let indexes = t
            .orders
            .iter()
            .map(|o| (&o.name, o.keys.join(" "), &o.predicate))
            .chain(t.element_keys.iter().map(|e| (&e.index, String::new(), &e.predicate)))
            .chain(t.brins.iter().map(|b| (&b.name, b.keys.join(" "), &b.predicate)));
        for (name, keys, predicate) in indexes {
            let Some(oid) = index(name) else { continue };
            let text = format!("{keys} {}", predicate.as_deref().unwrap_or_default());
            tree(&text, &|| super::defs::index_trees(&cat, oid).map(drop));
        }
    }
    for d in &catalog.domains {
        let Some(oid) = catalog.oids.of(&Object::Type(full(&d.name))) else {
            continue;
        };
        for (i, k) in d.checks.iter().enumerate() {
            tree(&k.check.to_string(), &|| {
                super::stored_tree(&cat, "domaincheck", oid, i as i64).map(drop)
            });
        }
        if let Some(e) = &d.default {
            tree(&e.to_string(), &|| {
                super::stored_tree(&cat, "domaindefault", oid, 0).map(drop)
            });
        }
    }
    for v in &catalog.views {
        tree(&v.query, &|| super::query::view_query(&cat, v).map(drop));
    }
    let typed = marking.typed();
    if typed.len() != n {
        return Err(Error::new("a fold's literals were let go before they were typed"));
    }
    let mut ranges: Option<HashMap<u32, u32>> = None;
    let mut out = Vec::with_capacity(n);
    for (k, m) in typed.iter().enumerate() {
        // A literal in an expression the analyzer could not read, before it was typed, has no
        // type to fold it by.
        if m.typed.is_empty() {
            if let Some((_, Err(e))) = read.iter().find(|(held, r)| held.contains(&k) && r.is_err()) {
                return Err(not_folded(&m.text, e));
            }
            out.push(None);
            continue;
        }
        let (ty, value) = &m.typed[0];
        if m.typed.iter().any(|(t, v)| t != ty || v != value) {
            return Err(Error::new(format!(
                "the literal '{}' is read as two types in one statement, which PostgreSQL never \
                 reads it as",
                m.text
            )));
        }
        let ranges = ranges.get_or_insert_with(|| subtypes(catalog));
        let folds = match reads(&cat, ranges, *ty, 0) {
            Reads::Zone => true,
            Reads::Clock => names_the_clock(&m.text),
            Reads::Nothing => false,
        };
        out.push(folds.then(|| super::print::literal(value)));
    }
    Ok(out)
}

/// The subtype of each range the file declares, by the range's OID and its multirange's.
fn subtypes(catalog: &Catalog) -> HashMap<u32, u32> {
    let num = |r: &HashMap<&'static str, Option<String>>, k: &str| -> u32 {
        r.get(k)
            .and_then(|v| v.as_deref())
            .and_then(|v| v.parse().ok())
            .unwrap_or_default()
    };
    let mut out = HashMap::new();
    for r in crate::pgcatalog::file_rows(catalog, "pg_range") {
        let sub = num(&r, "rngsubtype");
        out.insert(num(&r, "rngtypid"), sub);
        out.insert(num(&r, "rngmultitypid"), sub);
    }
    out
}

/// What a type's input reads besides a literal's text: the zone where it or a type it is made of
/// reads it, else the clock where one reads that.
fn reads(cat: &Cat, ranges: &HashMap<u32, u32>, ty: u32, depth: u8) -> Reads {
    if depth > 16 {
        return Reads::Nothing;
    }
    match ty {
        TIMESTAMPTZ | TIMETZ | TSTZRANGE | TSTZMULTIRANGE => return Reads::Zone,
        DATE | TIME | TIMESTAMP | TSRANGE | DATERANGE | TSMULTIRANGE | DATEMULTIRANGE => {
            return Reads::Clock
        }
        _ => {}
    }
    let Some(t) = cat.ty(ty) else {
        return Reads::Nothing;
    };
    let inner = |of: u32| reads(cat, ranges, of, depth + 1);
    match t.typtype {
        'd' => inner(t.base),
        'r' | 'm' => ranges.get(&ty).map_or(Reads::Nothing, |&s| inner(s)),
        'c' if t.relid != 0 => composite_fields(cat.catalog, t.relid)
            .into_iter()
            .map(inner)
            .max()
            .unwrap_or(Reads::Nothing),
        _ => match cat.element(ty) {
            0 => Reads::Nothing,
            e => inner(e),
        },
    }
}

/// The types of a composite's fields, by its relation's OID: a table's or a type's the file declares.
fn composite_fields(catalog: &Catalog, relid: u32) -> Vec<u32> {
    crate::pgcatalog::file_rows(catalog, "pg_attribute")
        .into_iter()
        .filter(|r| {
            let num = |k: &str| r.get(k).and_then(|v| v.as_deref()).unwrap_or_default().to_string();
            num("attrelid") == relid.to_string()
                && num("attnum").parse::<i32>().is_ok_and(|a| a > 0)
                && num("attisdropped") != "t"
        })
        .filter_map(|r| r.get("atttypid").and_then(|v| v.as_deref()).and_then(|v| v.parse().ok()))
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::catalog::Catalog;

    #[test]
    fn a_files_ddl_keeps_each_literal_its_zone_reads_as_its_value_under_utc() {
        let c = Catalog::parse(
            "CREATE SCHEMA s; CREATE TABLE s.t (id integer PRIMARY KEY, \
             d timestamptz CHECK (d >= '2026-01-01 00:00'), a date CHECK (a >= '2026-01-01'), \
             n text DEFAULT 'now', o timestamptz CHECK (o > '2026-01-01 00:00+05'));",
        )
        .unwrap();
        assert_eq!(
            c.ddl,
            "CREATE SCHEMA s; CREATE TABLE s.t (id integer PRIMARY KEY, d timestamptz CHECK (d >= \
             '2026-01-01 00:00:00+00'), a date CHECK (a >= '2026-01-01'), n text DEFAULT 'now', o \
             timestamptz CHECK (o > '2026-01-01 00:00+05'));"
        );
        // A DDL holding none is kept as written, a function's body whatever it names.
        let plain = "CREATE SCHEMA s;\nCREATE TABLE s.u (id integer PRIMARY KEY);\n";
        assert_eq!(Catalog::parse(plain).unwrap().ddl, plain);
        let body = "CREATE SCHEMA s;\nCREATE FUNCTION s.f() RETURNS timestamptz LANGUAGE sql STABLE \
                    AS 'SELECT now()';\n";
        assert_eq!(Catalog::parse(body).unwrap().ddl, body);
    }
}
