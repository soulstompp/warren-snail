//! jsonpath run over a jsonb, as PostgreSQL 19's jsonpath_exec.c runs it: the functions
//! jsonb_path_exists, jsonb_path_match, jsonb_path_query, jsonb_path_query_array and
//! jsonb_path_query_first, their `_tz` forms, and the operators `@?` and `@@`. A jsonb crosses this
//! module as its normal text, the text `jsonb_out` writes, and a jsonpath as its text; each is read
//! here into a tree of its own, and every item a path gives is written back as `jsonb_out` writes
//! it. Lax and strict modes, the arithmetic, every item method, filters, `exists`, `starts with`,
//! `is unknown`, `last`, ranges, `**` and variables are run as PostgreSQL runs them, each error in
//! its words, under the settings the snail answers under: TimeZone UTC, the database's default
//! collation libc's C.UTF-8. `like_regex` is refused by name, waiting for the regular expressions.

use super::{Chain, Item, Op};
use crate::catalog::Casing;
use crate::formatting::{parse_datetime, Datetime};
use crate::numeric::Numeric;
use crate::{Error, Result};
use std::cmp::Ordering;
use std::rc::Rc;

// ---- The functions ----

/// `jsonb_path_exists(target, path, vars, silent)`, `jsonb_path_exists_tz` where `tz`: whether
/// the path gives any item for the jsonb; none (NULL) where `silent` passed over an error. The
/// function is strict: a NULL argument is the caller's to answer.
pub fn jsonb_path_exists(
    target: &str,
    path: &str,
    vars: &str,
    silent: bool,
    tz: bool,
) -> Result<Option<bool>> {
    exists(target, path, Some(vars), silent, tz)
}

/// `jsonb @? jsonpath`: `jsonb_path_exists` with no variables, its errors passed over.
pub fn jsonb_path_exists_op(target: &str, path: &str) -> Result<Option<bool>> {
    exists(target, path, None, true, false)
}

fn exists(
    target: &str,
    path: &str,
    vars: Option<&str>,
    silent: bool,
    tz: bool,
) -> Result<Option<bool>> {
    run(target, path, vars, !silent, false, tz, |res, _| {
        Ok(match res {
            Jper::Error => None,
            res => Some(res == Jper::Ok),
        })
    })
}

/// `jsonb_path_match(target, path, vars, silent)`, `jsonb_path_match_tz` where `tz`: the one
/// boolean the path gives; none (NULL) where it gives a JSON null, or anything but one boolean
/// under `silent`, refused without it.
pub fn jsonb_path_match(
    target: &str,
    path: &str,
    vars: &str,
    silent: bool,
    tz: bool,
) -> Result<Option<bool>> {
    matches(target, path, Some(vars), silent, tz)
}

/// `jsonb @@ jsonpath`: `jsonb_path_match` with no variables, its errors passed over.
pub fn jsonb_path_match_op(target: &str, path: &str) -> Result<Option<bool>> {
    matches(target, path, None, true, false)
}

fn matches(
    target: &str,
    path: &str,
    vars: Option<&str>,
    silent: bool,
    tz: bool,
) -> Result<Option<bool>> {
    run(target, path, vars, !silent, true, tz, |_, found| {
        match found.as_slice() {
            [Val::Bool(b)] => return Ok(Some(*b)),
            [Val::Null] => return Ok(None),
            _ => {}
        }
        if !silent {
            return Err(Error::new("single boolean result is expected"));
        }
        Ok(None)
    })
}

/// `jsonb_path_query(target, path, vars, silent)`, `jsonb_path_query_tz` where `tz`: the items
/// the path gives, in order, each as `jsonb_out` writes it; the rows of the set it returns. Under
/// `silent` an error is passed over and the items found before it are kept.
pub fn jsonb_path_query(
    target: &str,
    path: &str,
    vars: &str,
    silent: bool,
    tz: bool,
) -> Result<Vec<String>> {
    run(target, path, Some(vars), !silent, true, tz, |_, found| {
        found.iter().map(text_of).collect()
    })
}

/// `jsonb_path_query_array(target, path, vars, silent)`, `jsonb_path_query_array_tz` where `tz`:
/// the items the path gives as one jsonb array.
pub fn jsonb_path_query_array(
    target: &str,
    path: &str,
    vars: &str,
    silent: bool,
    tz: bool,
) -> Result<String> {
    run(target, path, Some(vars), !silent, true, tz, |_, found| {
        let mut out = vec![b'['];
        for (i, v) in found.iter().enumerate() {
            if i > 0 {
                out.extend_from_slice(b", ");
            }
            write(&mut out, v)?;
        }
        out.push(b']');
        Ok(String::from_utf8_lossy(&out).into_owned())
    })
}

/// `jsonb_path_query_first(target, path, vars, silent)`, `jsonb_path_query_first_tz` where `tz`:
/// the first item the path gives; none (NULL) where it gives none.
pub fn jsonb_path_query_first(
    target: &str,
    path: &str,
    vars: &str,
    silent: bool,
    tz: bool,
) -> Result<Option<String>> {
    run(target, path, Some(vars), !silent, true, tz, |_, found| {
        found.first().map(text_of).transpose()
    })
}

// ---- The values: JsonbValue ----

/// A value as the executor holds one (`JsonbValue`): a scalar, a date or time a method made, or an
/// array or object of a jsonb (`jbvBinary`) with where it begins in its document, in bytes from the
/// document's first container as jsonb's binary form lays them out.
#[derive(Clone, Debug)]
enum Val {
    Null,
    Bool(bool),
    Num(Numeric),
    Str(Rc<str>),
    Datetime(Datetime),
    Box(Rc<Cont>, u64),
}

/// An array or an object.
#[derive(Debug)]
struct Cont {
    /// An object's keys, in the order jsonb stores them; none for an array.
    keys: Option<Vec<Rc<str>>>,
    /// Its elements, or its values; each container among them with where it begins in this one.
    vals: Vec<Val>,
    /// Its bytes in jsonb's binary form, from its header to its end.
    size: u64,
}

impl Cont {
    fn is_object(&self) -> bool {
        self.keys.is_some()
    }

    fn len(&self) -> usize {
        self.vals.len()
    }

    /// Its `i`th element or value, a container placed in the document this one begins `at`.
    fn child(&self, i: usize, at: u64) -> Val {
        match &self.vals[i] {
            Val::Box(c, rel) => Val::Box(c.clone(), at + rel),
            v => v.clone(),
        }
    }

    /// The value of the key, as `findJsonbValueFromContainer` finds it.
    fn find(&self, key: &[u8]) -> Option<usize> {
        self.keys.as_ref()?.iter().position(|k| k.as_bytes() == key)
    }
}

fn align(p: u64) -> u64 {
    (p + 3) & !3
}

/// A container as `convertJsonbArray` and `convertJsonbObject` lay it out: its header, a JEntry
/// for each element or for each key and each value, an object's keys, then the values, a number
/// and a container each after padding to four bytes.
fn container(keys: Option<Vec<Rc<str>>>, mut vals: Vec<Val>) -> Rc<Cont> {
    let n = vals.len() as u64;
    let mut pos = 4 + 4 * if keys.is_some() { 2 * n } else { n };
    for k in keys.iter().flatten() {
        pos += k.len() as u64;
    }
    for v in &mut vals {
        match v {
            Val::Str(s) => pos += s.len() as u64,
            Val::Num(x) => pos = align(pos) + numeric_bytes(x),
            Val::Box(c, at) => {
                pos = align(pos);
                *at = pos;
                pos += c.size;
            }
            Val::Null | Val::Bool(_) | Val::Datetime(_) => {}
        }
    }
    Rc::new(Cont {
        keys,
        vals,
        size: pos,
    })
}

/// The bytes a number takes in a jsonb: the varlena `make_result` makes of it, its digits in base
/// 10000 from the first that is not zero to the last, under the short header where its display
/// scale and weight fit one.
fn numeric_bytes(n: &Numeric) -> u64 {
    let text = n.to_string();
    let t = text.trim_start_matches('-');
    let (int, frac) = t.split_once('.').unwrap_or((t, ""));
    let int_groups = int.len().div_ceil(4);
    let mut digits = "0".repeat(int_groups * 4 - int.len());
    digits.push_str(int);
    digits.push_str(frac);
    digits.push_str(&"0".repeat(frac.len().div_ceil(4) * 4 - frac.len()));
    let nonzero: Vec<bool> = digits
        .as_bytes()
        .chunks(4)
        .map(|g| g.iter().any(|&c| c != b'0'))
        .collect();
    let (ndigits, weight) = match (
        nonzero.iter().position(|&z| z),
        nonzero.iter().rposition(|&z| z),
    ) {
        (Some(f), Some(l)) => ((l - f + 1) as u64, int_groups as i64 - 1 - f as i64),
        _ => (0, 0),
    };
    let short = frac.len() <= 63 && (-64..=63).contains(&weight);
    4 + if short { 2 } else { 4 } + 2 * ndigits
}

// ---- Reading a jsonb's text ----

/// A jsonb's text read into its values: a scalar at the top bare (`JsonbExtractScalar`), a
/// container at the start of its document.
fn read_jsonb(text: &str) -> Result<Val> {
    let b = text.as_bytes();
    let mut at = 0;
    let mut stack: Vec<Open> = Vec::new();
    let bad = || Error::new(format!("invalid input syntax for type json: \"{text}\""));
    let skip = |at: &mut usize| {
        while *at < b.len() && matches!(b[*at], b' ' | b'\t' | b'\n' | b'\r') {
            *at += 1;
        }
    };
    loop {
        skip(&mut at);
        // An object's key comes first where an object is open and its keys outnumber its values
        // by none; an empty object closes there.
        let mut empty_object = false;
        if let Some((Some(keys), vals)) = stack.last_mut() {
            if keys.len() == vals.len() {
                if b.get(at) == Some(&b'}') {
                    at += 1;
                    empty_object = true;
                } else {
                    let k = read_string(b, &mut at).ok_or_else(bad)?;
                    skip(&mut at);
                    if b.get(at) != Some(&b':') {
                        return Err(bad());
                    }
                    at += 1;
                    keys.push(k.into());
                    continue;
                }
            }
        }
        let v = if empty_object {
            let (keys, vals) = stack.pop().expect("an open object");
            Val::Box(container(keys, vals), 0)
        } else {
            match b.get(at) {
                Some(b'{') => {
                    at += 1;
                    stack.push((Some(Vec::new()), Vec::new()));
                    continue;
                }
                Some(b'[') => {
                    at += 1;
                    skip(&mut at);
                    if b.get(at) == Some(&b']') {
                        at += 1;
                        Val::Box(container(None, Vec::new()), 0)
                    } else {
                        stack.push((None, Vec::new()));
                        continue;
                    }
                }
                Some(b'"') => Val::Str(read_string(b, &mut at).ok_or_else(bad)?.into()),
                Some(b't') if b[at..].starts_with(b"true") => {
                    at += 4;
                    Val::Bool(true)
                }
                Some(b'f') if b[at..].starts_with(b"false") => {
                    at += 5;
                    Val::Bool(false)
                }
                Some(b'n') if b[at..].starts_with(b"null") => {
                    at += 4;
                    Val::Null
                }
                Some(_) => {
                    let start = at;
                    while at < b.len()
                        && matches!(b[at], b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E')
                    {
                        at += 1;
                    }
                    if at == start {
                        return Err(bad());
                    }
                    Val::Num(Numeric::parse(&text[start..at])?)
                }
                None => return Err(bad()),
            }
        };
        let mut v = v;
        loop {
            if let Some(root) = place(&mut stack, v) {
                return finish(b, at, root, bad);
            }
            skip(&mut at);
            match b.get(at) {
                Some(b',') => {
                    at += 1;
                    break;
                }
                Some(b']') if matches!(stack.last(), Some((None, _))) => {
                    at += 1;
                    let (_, vals) = stack.pop().expect("an open array");
                    v = Val::Box(container(None, vals), 0);
                }
                Some(b'}') if matches!(stack.last(), Some((Some(_), _))) => {
                    at += 1;
                    let (keys, vals) = stack.pop().expect("an open object");
                    v = Val::Box(container(keys, vals), 0);
                }
                _ => return Err(bad()),
            }
        }
    }
}

/// A container being read: an object's keys so far (none for an array), and its values.
type Open = (Option<Vec<Rc<str>>>, Vec<Val>);

/// A value read put into the container open last; the value itself where none is open.
fn place(stack: &mut [Open], v: Val) -> Option<Val> {
    match stack.last_mut() {
        Some((_, vals)) => {
            vals.push(v);
            None
        }
        None => Some(v),
    }
}

fn finish(b: &[u8], mut at: usize, root: Val, bad: impl Fn() -> Error) -> Result<Val> {
    while at < b.len() && matches!(b[at], b' ' | b'\t' | b'\n' | b'\r') {
        at += 1;
    }
    if at != b.len() {
        return Err(bad());
    }
    Ok(root)
}

/// A JSON string at `at`, its escapes taken.
fn read_string(b: &[u8], at: &mut usize) -> Option<String> {
    if b.get(*at) != Some(&b'"') {
        return None;
    }
    *at += 1;
    let mut out: Vec<u8> = Vec::new();
    loop {
        let c = *b.get(*at)?;
        *at += 1;
        match c {
            b'"' => break,
            b'\\' => {
                let e = *b.get(*at)?;
                *at += 1;
                match e {
                    b'"' | b'\\' | b'/' => out.push(e),
                    b'b' => out.push(b'\x08'),
                    b'f' => out.push(b'\x0c'),
                    b'n' => out.push(b'\n'),
                    b'r' => out.push(b'\r'),
                    b't' => out.push(b'\t'),
                    b'u' => {
                        let hex = |at: usize| -> Option<u32> {
                            u32::from_str_radix(std::str::from_utf8(b.get(at..at + 4)?).ok()?, 16)
                                .ok()
                        };
                        let mut cp = hex(*at)?;
                        *at += 4;
                        if (0xD800..0xDC00).contains(&cp)
                            && b.get(*at..*at + 2) == Some(b"\\u".as_slice())
                        {
                            let lo = hex(*at + 2)?;
                            if (0xDC00..0xE000).contains(&lo) {
                                cp = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                                *at += 6;
                            }
                        }
                        let ch = char::from_u32(cp)?;
                        let mut buf = [0u8; 4];
                        out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                    }
                    _ => return None,
                }
            }
            _ => out.push(c),
        }
    }
    String::from_utf8(out).ok()
}

// ---- Writing a value: JsonbValueToJsonb and jsonb_out ----

/// An item as `jsonb_out` writes the jsonb `JsonbValueToJsonb` makes of it.
fn text_of(v: &Val) -> Result<String> {
    let mut out = Vec::new();
    write(&mut out, v)?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// A value as `jsonb_out` writes it: `, ` between elements, `": "` after a key, a date or time
/// as the string `JsonEncodeDateTime` writes. Walked in a loop, so a deep document is written
/// without deep recursion.
fn write(out: &mut Vec<u8>, v: &Val) -> Result<()> {
    let mut stack: Vec<(Rc<Cont>, usize)> = Vec::new();
    let mut cur = Some(v.clone());
    loop {
        if let Some(v) = cur.take() {
            match v {
                Val::Null => out.extend_from_slice(b"null"),
                Val::Bool(b) => out.extend_from_slice(if b { b"true" } else { b"false" }),
                Val::Num(n) => out.extend_from_slice(n.to_string().as_bytes()),
                Val::Str(s) => super::escape_json(out, s.as_bytes()),
                Val::Datetime(d) => super::escape_json(out, datetime_text(&d)?.as_bytes()),
                Val::Box(c, _) => {
                    out.push(if c.is_object() { b'{' } else { b'[' });
                    stack.push((c, 0));
                }
            }
        }
        let Some((c, i)) = stack.last_mut() else {
            return Ok(());
        };
        if *i == c.len() {
            out.push(if c.is_object() { b'}' } else { b']' });
            stack.pop();
            continue;
        }
        if *i > 0 {
            out.extend_from_slice(b", ");
        }
        if let Some(keys) = &c.keys {
            super::escape_json(out, keys[*i].as_bytes());
            out.extend_from_slice(b": ");
        }
        cur = Some(c.vals[*i].clone());
        *i += 1;
    }
}

// ---- Dates and times ----

const USECS_PER_SEC: i64 = 1_000_000;

/// A date or time as `JsonEncodeDateTime` writes it: ISO 8601 with a `T`, a zone as `+hh:mm`.
fn datetime_text(d: &Datetime) -> Result<String> {
    let year = |y: i64| format!("{:04}", if y > 0 { y } else { -(y - 1) });
    let bc = |out: &mut String, y: i64| {
        if y <= 0 {
            out.push_str(" BC");
        }
    };
    Ok(match *d {
        Datetime::Date(i32::MIN) => "-infinity".into(),
        Datetime::Date(i32::MAX) => "infinity".into(),
        Datetime::Date(days) => {
            let (y, m, dd) = crate::times::j2date(i64::from(days) + 2_440_588);
            let mut out = format!("{}-{m:02}-{dd:02}", year(y));
            bc(&mut out, y);
            out
        }
        Datetime::Time(t) => time_text(t),
        Datetime::TimeTz(t, zone) => {
            let mut out = time_text(t);
            zone_text(&mut out, zone);
            out
        }
        Datetime::Timestamp(i64::MIN) | Datetime::TimestampTz(i64::MIN, _) => "-infinity".into(),
        Datetime::Timestamp(i64::MAX) | Datetime::TimestampTz(i64::MAX, _) => "infinity".into(),
        Datetime::Timestamp(t) => instant_text(t, None),
        Datetime::TimestampTz(t, tz) => instant_text(t - i64::from(tz) * USECS_PER_SEC, Some(tz)),
    })
}

fn seconds_text(out: &mut String, sec: i64, frac: i64) {
    out.push_str(&format!("{sec:02}"));
    if frac != 0 {
        let f = format!("{frac:06}");
        out.push('.');
        out.push_str(f.trim_end_matches('0'));
    }
}

fn time_text(t: i64) -> String {
    let mut out = format!(
        "{:02}:{:02}:",
        t / 3_600_000_000,
        t % 3_600_000_000 / 60_000_000
    );
    seconds_text(&mut out, t % 60_000_000 / USECS_PER_SEC, t % USECS_PER_SEC);
    out
}

/// A zone, `tz` seconds west of UTC, as `EncodeTimezone` writes it in the XSD style.
fn zone_text(out: &mut String, tz: i32) {
    let sec = tz.unsigned_abs();
    let (hour, min, s) = (sec / 3600, sec / 60 % 60, sec % 60);
    out.push(if tz <= 0 { '+' } else { '-' });
    out.push_str(&format!("{hour:02}:{min:02}"));
    if s != 0 {
        out.push_str(&format!(":{s:02}"));
    }
}

/// An instant on its own clock, microseconds since 1970-01-01, as `EncodeDateTime` writes it in
/// the XSD style, with the zone where one is given.
fn instant_text(t: i64, tz: Option<i32>) -> String {
    let (y, mo, d, h, mi, s, us) = crate::times::civil(t);
    let mut out = format!("{}-{mo:02}-{d:02}T{h:02}:{mi:02}:", {
        format!("{:04}", if y > 0 { y } else { -(y - 1) })
    });
    seconds_text(&mut out, i64::from(s), i64::from(us));
    if let Some(tz) = tz {
        zone_text(&mut out, tz);
    }
    if y <= 0 {
        out.push_str(" BC");
    }
    out
}

fn type_name(v: &Val) -> &'static str {
    match v {
        Val::Null => "null",
        Val::Bool(_) => "boolean",
        Val::Num(_) => "number",
        Val::Str(_) => "string",
        Val::Box(c, _) => {
            if c.is_object() {
                "object"
            } else {
                "array"
            }
        }
        Val::Datetime(d) => match d {
            Datetime::Date(_) => "date",
            Datetime::Time(_) => "time without time zone",
            Datetime::TimeTz(..) => "time with time zone",
            Datetime::Timestamp(_) => "timestamp without time zone",
            Datetime::TimestampTz(..) => "timestamp with time zone",
        },
    }
}

/// `checkTimezoneIsUsedForCast`: a cast that needs the session's zone refused outside the `_tz`
/// functions.
fn need_tz(use_tz: bool, from: &str, to: &str) -> Result<()> {
    if !use_tz {
        return Err(Error::new(format!(
            "cannot convert value from {from} to {to} without time zone usage"
        )));
    }
    Ok(())
}

/// `timetz_cmp`: by the instant each names, then by its zone.
fn timetz_cmp(a: (i64, i32), b: (i64, i32)) -> Ordering {
    let ta = a.0 + i64::from(a.1) * USECS_PER_SEC;
    let tb = b.0 + i64::from(b.1) * USECS_PER_SEC;
    ta.cmp(&tb).then(a.1.cmp(&b.1))
}

/// `compareDatetime`: two dates or times compared across their types, a cast that needs a zone
/// refused outside the `_tz` functions; none where the two cannot be compared.
fn compare_datetime(a: &Datetime, b: &Datetime, use_tz: bool) -> Result<Option<Ordering>> {
    use crate::times::{date_cmp_timestamp, date_cmp_timestamptz, timestamp_cmp_timestamptz};
    use Datetime::*;
    Ok(Some(match (*a, *b) {
        (Date(x), Date(y)) => x.cmp(&y),
        (Date(x), Timestamp(y)) => date_cmp_timestamp(x, y),
        (Date(x), TimestampTz(y, _)) => {
            need_tz(use_tz, "date", "timestamptz")?;
            date_cmp_timestamptz(x, y)
        }
        (Date(_), Time(_) | TimeTz(..)) => return Ok(None),
        (Time(x), Time(y)) => x.cmp(&y),
        (Time(x), TimeTz(t, z)) => {
            need_tz(use_tz, "time", "timetz")?;
            timetz_cmp(crate::times::time_timetz(x), (t, z))
        }
        (Time(_), Date(_) | Timestamp(_) | TimestampTz(..)) => return Ok(None),
        (TimeTz(t, z), Time(y)) => {
            need_tz(use_tz, "time", "timetz")?;
            timetz_cmp((t, z), crate::times::time_timetz(y))
        }
        (TimeTz(t, z), TimeTz(u, w)) => timetz_cmp((t, z), (u, w)),
        (TimeTz(..), Date(_) | Timestamp(_) | TimestampTz(..)) => return Ok(None),
        (Timestamp(x), Date(y)) => date_cmp_timestamp(y, x).reverse(),
        (Timestamp(x), Timestamp(y)) => x.cmp(&y),
        (Timestamp(x), TimestampTz(y, _)) => {
            need_tz(use_tz, "timestamp", "timestamptz")?;
            timestamp_cmp_timestamptz(x, y)
        }
        (Timestamp(_), Time(_) | TimeTz(..)) => return Ok(None),
        (TimestampTz(x, _), Date(y)) => {
            need_tz(use_tz, "date", "timestamptz")?;
            date_cmp_timestamptz(y, x).reverse()
        }
        (TimestampTz(x, _), Timestamp(y)) => {
            need_tz(use_tz, "timestamp", "timestamptz")?;
            timestamp_cmp_timestamptz(y, x).reverse()
        }
        (TimestampTz(x, _), TimestampTz(y, _)) => x.cmp(&y),
        (TimestampTz(..), Time(_) | TimeTz(..)) => return Ok(None),
    }))
}

// ---- The executor ----

/// What running a path gives (`JsonPathExecResult`): an item, none, or an error passed over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Jper {
    Ok,
    NotFound,
    Error,
}

/// What a predicate gives (`JsonPathBool`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Jpb {
    False,
    True,
    Unknown,
}

/// The list the items found go to, where the caller keeps them.
type Found<'f> = Option<&'f mut Vec<Val>>;

/// How many levels a run may take on the caller's stack; a deeper one takes a thread of its own,
/// with a stack of `LEVEL_BYTES` for each level.
const INLINE_LEVELS: usize = 512;
const LEVEL_BYTES: usize = 4 << 10;
/// How many levels a run may take before it is refused, in PostgreSQL's words for a stack too
/// deep: past where PostgreSQL's own stack, at its default depth, refuses a path or a document.
const LEVELS_MAX: usize = 40_000;

/// The executor's state (`JsonPathExecContext`).
struct Cx<'a> {
    vars: Option<&'a Val>,
    root: Val,
    current: Val,
    /// The id of the object `.keyvalue()` counts its ids from: `$` 0, the variables 1, each object
    /// `.keyvalue()` makes one of its own.
    base_id: i64,
    last_generated_id: i64,
    innermost_array_size: i32,
    lax: bool,
    ignore_structural_errors: bool,
    throw_errors: bool,
    use_tz: bool,
    depth: usize,
    /// The most levels this run may take.
    levels: usize,
}

impl Cx<'_> {
    fn enter(&mut self) -> Result<()> {
        self.depth += 1;
        if self.depth > self.levels {
            return Err(Error::new("stack depth limit exceeded"));
        }
        Ok(())
    }
}

/// `RETURN_ERROR`: an error thrown where the context throws, otherwise passed back as one.
macro_rules! return_error {
    ($cx:expr, $($arg:tt)*) => {
        return if $cx.throw_errors {
            Err(Error::new(format!($($arg)*)))
        } else {
            Ok(Jper::Error)
        }
    };
}

/// `executeJsonPath`: the path run over the jsonb, its items kept where `collect`, and `then` given
/// what the run gave and its items. A path in strict mode asked only whether it gives an item is
/// run to its end, to find every error. A run deeper than the caller's stack holds is run on a
/// thread of its own.
fn run<T: Send>(
    target: &str,
    path: &str,
    vars: Option<&str>,
    throw_errors: bool,
    collect: bool,
    use_tz: bool,
    then: impl FnOnce(Jper, Vec<Val>) -> Result<T> + Send,
) -> Result<T> {
    let (lax, chain) = super::read(path)?;
    let levels = levels(&chain, [Some(target), vars]);
    if levels > LEVELS_MAX {
        return Err(Error::new("stack depth limit exceeded"));
    }
    let go = move || -> Result<T> {
        let mut found = Vec::new();
        let res = execute(
            target,
            &chain,
            lax,
            vars,
            throw_errors,
            collect.then_some(&mut found),
            use_tz,
            levels,
        )?;
        then(res, found)
    };
    if levels <= INLINE_LEVELS {
        return go();
    }
    std::thread::scope(|s| {
        std::thread::Builder::new()
            .stack_size(levels * LEVEL_BYTES + (1 << 20))
            .spawn_scoped(s, go)
            .map_err(|e| Error::new(format!("could not start the jsonpath executor: {e}")))?
            .join()
            .unwrap_or_else(|p| std::panic::resume_unwind(p))
    })
}

/// How many levels a run takes at most: four for each item of the path, and for each `**` and the
/// path itself the depth of the deepest document and two more.
fn levels(chain: &Chain, docs: [Option<&str>; 2]) -> usize {
    let (mut items, mut anys) = (0, 0);
    let mut todo = vec![chain];
    while let Some(c) = todo.pop() {
        for item in c {
            items += 1;
            match item {
                Item::Any(..) => anys += 1,
                Item::Binary(_, l, r) | Item::Call2(_, l, r) => todo.extend([l, r]),
                Item::Decimal(l, r) => todo.extend(l.iter().chain(r.iter())),
                Item::Call(_, a) => todo.extend(a.iter()),
                Item::Not(a)
                | Item::IsUnknown(a)
                | Item::Plus(a)
                | Item::Minus(a)
                | Item::Filter(a)
                | Item::Exists(a)
                | Item::LikeRegex { expr: a, .. } => todo.push(a),
                Item::Index(elems) => {
                    for (from, to) in elems {
                        todo.push(from);
                        todo.extend(to.iter());
                    }
                }
                _ => {}
            }
        }
    }
    let depth = docs.iter().flatten().map(|d| nesting(d)).max().unwrap_or(0);
    4 * items + (anys + 1) * (depth + 2) + 16
}

/// How deep a JSON text's arrays and objects nest.
fn nesting(text: &str) -> usize {
    let (mut depth, mut most, mut in_string, mut escaped) = (0usize, 0, false, false);
    for &c in text.as_bytes() {
        if in_string {
            match c {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            b'"' => in_string = true,
            b'[' | b'{' => {
                depth += 1;
                most = most.max(depth);
            }
            b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    most
}

/// The path run over the jsonb read from its text, the variables from theirs.
#[allow(clippy::too_many_arguments)]
fn execute(
    target: &str,
    chain: &Chain,
    lax: bool,
    vars: Option<&str>,
    throw_errors: bool,
    found: Found,
    use_tz: bool,
    levels: usize,
) -> Result<Jper> {
    let root = read_jsonb(target)?;
    let vars = vars.map(read_jsonb).transpose()?;
    let count = match &vars {
        None => 0,
        Some(Val::Box(c, _)) if c.is_object() => 1,
        Some(_) => return Err(Error::new("\"vars\" argument is not an object")),
    };
    let mut cx = Cx {
        vars: vars.as_ref(),
        current: root.clone(),
        root: root.clone(),
        base_id: 0,
        last_generated_id: 1 + count,
        innermost_array_size: -1,
        lax,
        ignore_structural_errors: lax,
        throw_errors,
        use_tz,
        depth: 0,
        levels,
    };
    if !lax && found.is_none() {
        let mut vals = Vec::new();
        let res = execute_item(&mut cx, chain, &root, Some(&mut vals))?;
        if res == Jper::Error {
            return Ok(res);
        }
        return Ok(if vals.is_empty() {
            Jper::NotFound
        } else {
            Jper::Ok
        });
    }
    execute_item(&mut cx, chain, &root, found)
}

fn is_array(v: &Val) -> bool {
    matches!(v, Val::Box(c, _) if !c.is_object())
}

/// `JsonbArraySize`: an array's size, -1 for anything else.
fn array_size(v: &Val) -> i32 {
    match v {
        Val::Box(c, _) if !c.is_object() => c.len() as i32,
        _ => -1,
    }
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// `executeItem`: an item run over a value, an array unwrapped in lax mode.
fn execute_item(cx: &mut Cx, jsp: &[Item], jb: &Val, found: Found) -> Result<Jper> {
    let unwrap = cx.lax;
    execute_item_opt_unwrap_target(cx, jsp, jb, found, unwrap)
}

/// `executeNextItem`: the items after one run over its value; the value itself into `found`
/// where none follow.
fn execute_next_item(cx: &mut Cx, next: &[Item], v: &Val, found: Found) -> Result<Jper> {
    if !next.is_empty() {
        return execute_item(cx, next, v, found);
    }
    if let Some(f) = found {
        f.push(v.clone());
    }
    Ok(Jper::Ok)
}

/// `executeItemUnwrapTargetArray`: the items run over each element of an array.
fn execute_item_unwrap_target_array(
    cx: &mut Cx,
    jsp: Option<&[Item]>,
    jb: &Val,
    found: Found,
    unwrap_elements: bool,
) -> Result<Jper> {
    let Val::Box(c, at) = jb else {
        return Err(Error::new("invalid jsonb array value type"));
    };
    execute_any_item(cx, jsp, c, *at, found, 1, 1, 1, false, unwrap_elements)
}

/// `executeItemOptUnwrapResult`: an item run, each array it gives unwrapped where `unwrap` asks
/// in lax mode.
fn execute_item_opt_unwrap_result(
    cx: &mut Cx,
    jsp: &[Item],
    jb: &Val,
    unwrap: bool,
    found: Found,
) -> Result<Jper> {
    if unwrap && cx.lax {
        let mut seq = Vec::new();
        let res = execute_item(cx, jsp, jb, Some(&mut seq))?;
        if res == Jper::Error {
            return Ok(res);
        }
        let mut found = found;
        for item in seq {
            if is_array(&item) {
                execute_item_unwrap_target_array(cx, None, &item, found.as_deref_mut(), false)?;
            } else if let Some(f) = found.as_deref_mut() {
                f.push(item);
            }
        }
        return Ok(Jper::Ok);
    }
    execute_item(cx, jsp, jb, found)
}

/// `executeItemOptUnwrapResultNoThrow`: as `execute_item_opt_unwrap_result`, its errors passed
/// back.
fn execute_item_opt_unwrap_result_no_throw(
    cx: &mut Cx,
    jsp: &[Item],
    jb: &Val,
    unwrap: bool,
    found: Found,
) -> Result<Jper> {
    let throw = cx.throw_errors;
    cx.throw_errors = false;
    let res = execute_item_opt_unwrap_result(cx, jsp, jb, unwrap, found)?;
    cx.throw_errors = throw;
    Ok(res)
}

/// `executeItemOptUnwrapTarget`: one item of a path run over a value, and the items after it over
/// what it gives; an array the item does not apply to unwrapped where `unwrap`.
fn execute_item_opt_unwrap_target(
    cx: &mut Cx,
    jsp: &[Item],
    jb: &Val,
    found: Found,
    unwrap: bool,
) -> Result<Jper> {
    cx.enter()?;
    let res = item_over(cx, jsp, jb, found, unwrap);
    cx.depth -= 1;
    res
}

fn item_over(cx: &mut Cx, jsp: &[Item], jb: &Val, mut found: Found, unwrap: bool) -> Result<Jper> {
    let next = &jsp[1..];
    let has_next = !next.is_empty();
    let mut res = Jper::NotFound;
    match &jsp[0] {
        item @ (Item::Null | Item::Bool(_) | Item::Num(_) | Item::Str(_) | Item::Variable(_)) => {
            if !has_next && found.is_none() && !matches!(item, Item::Variable(_)) {
                return Ok(Jper::Ok);
            }
            let base = cx.base_id;
            let v = get_item(cx, item)?;
            res = execute_next_item(cx, next, &v, found)?;
            cx.base_id = base;
        }
        Item::Binary(Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Mod, ..) => {
            return execute_binary_arith(cx, jsp, jb, found);
        }
        Item::Binary(..)
        | Item::Not(_)
        | Item::IsUnknown(_)
        | Item::Exists(_)
        | Item::LikeRegex { .. } => {
            let st = execute_bool_item(cx, jsp, jb, true)?;
            res = append_bool_result(cx, next, found, st)?;
        }
        Item::Plus(arg) => return execute_unary_arith(cx, false, arg, next, jb, found),
        Item::Minus(arg) => return execute_unary_arith(cx, true, arg, next, jb, found),
        Item::AnyArray => {
            if is_array(jb) {
                let lax = cx.lax;
                res =
                    execute_item_unwrap_target_array(cx, has_next.then_some(next), jb, found, lax)?;
            } else if cx.lax {
                res = execute_next_item(cx, next, jb, found)?;
            } else if !cx.ignore_structural_errors {
                return_error!(
                    cx,
                    "jsonpath wildcard array accessor can only be applied to an array"
                );
            }
        }
        Item::AnyKey => {
            if let Val::Box(c, at) = jb {
                if c.is_object() {
                    let lax = cx.lax;
                    return execute_any_item(
                        cx,
                        has_next.then_some(next),
                        c,
                        *at,
                        found,
                        1,
                        1,
                        1,
                        false,
                        lax,
                    );
                }
            }
            if unwrap && is_array(jb) {
                return execute_item_unwrap_target_array(cx, Some(jsp), jb, found, false);
            } else if !cx.ignore_structural_errors {
                return_error!(
                    cx,
                    "jsonpath wildcard member accessor can only be applied to an object"
                );
            }
        }
        Item::Index(elems) => {
            if is_array(jb) || cx.lax {
                let innermost = cx.innermost_array_size;
                let mut size = array_size(jb);
                let singleton = size < 0;
                if singleton {
                    size = 1;
                }
                cx.innermost_array_size = size;
                for (from, to) in elems {
                    let index_from = match get_array_index(cx, from, jb)? {
                        Ok(i) => i,
                        Err(e) => {
                            res = e;
                            break;
                        }
                    };
                    let index_to = match to {
                        Some(to) => match get_array_index(cx, to, jb)? {
                            Ok(i) => i,
                            Err(e) => {
                                res = e;
                                break;
                            }
                        },
                        None => index_from,
                    };
                    if !cx.ignore_structural_errors
                        && (index_from < 0 || index_from > index_to || index_to >= size)
                    {
                        return_error!(cx, "jsonpath array subscript is out of bounds");
                    }
                    let index_from = index_from.max(0);
                    let index_to = if index_to >= size { size - 1 } else { index_to };
                    res = Jper::NotFound;
                    let mut index = index_from;
                    while index <= index_to {
                        let v = if singleton {
                            jb.clone()
                        } else {
                            match jb {
                                Val::Box(c, at) if (index as usize) < c.len() => {
                                    c.child(index as usize, *at)
                                }
                                _ => {
                                    index += 1;
                                    continue;
                                }
                            }
                        };
                        if !has_next && found.is_none() {
                            return Ok(Jper::Ok);
                        }
                        res = execute_next_item(cx, next, &v, found.as_deref_mut())?;
                        if res == Jper::Error || (res == Jper::Ok && found.is_none()) {
                            break;
                        }
                        index += 1;
                    }
                    if res == Jper::Error || (res == Jper::Ok && found.is_none()) {
                        break;
                    }
                }
                cx.innermost_array_size = innermost;
            } else if !cx.ignore_structural_errors {
                return_error!(
                    cx,
                    "jsonpath array accessor can only be applied to an array"
                );
            }
        }
        Item::Any(first, last) => {
            if *first == 0 {
                let saved = cx.ignore_structural_errors;
                cx.ignore_structural_errors = true;
                res = execute_next_item(cx, next, jb, found.as_deref_mut())?;
                cx.ignore_structural_errors = saved;
                if res == Jper::Ok && found.is_none() {
                    return Ok(res);
                }
            }
            if let Val::Box(c, at) = jb {
                let lax = cx.lax;
                res = execute_any_item(
                    cx,
                    has_next.then_some(next),
                    c,
                    *at,
                    found,
                    1,
                    *first,
                    *last,
                    true,
                    lax,
                )?;
            }
        }
        Item::Key(key) => {
            if let Val::Box(c, at) = jb {
                if c.is_object() {
                    match c.find(key) {
                        Some(i) => {
                            res = execute_next_item(cx, next, &c.child(i, *at), found)?;
                        }
                        None if !cx.ignore_structural_errors => {
                            if !cx.throw_errors {
                                return Ok(Jper::Error);
                            }
                            return Err(Error::new(format!(
                                "JSON object does not contain key \"{}\"",
                                lossy(key)
                            )));
                        }
                        None => {}
                    }
                    return Ok(res);
                }
            }
            if unwrap && is_array(jb) {
                return execute_item_unwrap_target_array(cx, Some(jsp), jb, found, false);
            } else if !cx.ignore_structural_errors {
                return_error!(
                    cx,
                    "jsonpath member accessor can only be applied to an object"
                );
            }
        }
        Item::Current => {
            let v = cx.current.clone();
            res = execute_next_item(cx, next, &v, found)?;
        }
        Item::Root => {
            let v = cx.root.clone();
            let base = cx.base_id;
            cx.base_id = 0;
            res = execute_next_item(cx, next, &v, found)?;
            cx.base_id = base;
        }
        Item::Filter(pred) => {
            if unwrap && is_array(jb) {
                return execute_item_unwrap_target_array(cx, Some(jsp), jb, found, false);
            }
            let st = execute_nested_bool_item(cx, pred, jb)?;
            if st != Jpb::True {
                res = Jper::NotFound;
            } else {
                res = execute_next_item(cx, next, jb, found)?;
            }
        }
        Item::Last => {
            if cx.innermost_array_size < 0 {
                return Err(Error::new(
                    "evaluating jsonpath LAST outside of array subscript",
                ));
            }
            if !has_next && found.is_none() {
                res = Jper::Ok;
            } else {
                let last = Numeric::of_int(i64::from(cx.innermost_array_size) - 1);
                res = execute_next_item(cx, next, &Val::Num(last), found)?;
            }
        }
        Item::Method(name) => return method(cx, jsp, name, jb, found, unwrap),
        Item::Call(name, _) | Item::Call2(name, ..) => {
            if unwrap && is_array(jb) {
                return execute_item_unwrap_target_array(cx, Some(jsp), jb, found, false);
            }
            return match *name {
                "datetime" | "time" | "time_tz" | "timestamp" | "timestamp_tz" => {
                    execute_datetime_method(cx, jsp, name, jb, found)
                }
                _ => execute_string_method(cx, jsp, name, jb, found),
            };
        }
        Item::Decimal(..) => return execute_number_method(cx, jsp, "decimal", jb, found, unwrap),
    }
    Ok(res)
}

/// The methods without arguments.
fn method(
    cx: &mut Cx,
    jsp: &[Item],
    name: &str,
    jb: &Val,
    found: Found,
    unwrap: bool,
) -> Result<Jper> {
    let next = &jsp[1..];
    match name {
        "type" => {
            let v = Val::Str(type_name(jb).into());
            execute_next_item(cx, next, &v, found)
        }
        "size" => {
            let mut size = array_size(jb);
            if size < 0 {
                if !cx.lax {
                    if !cx.ignore_structural_errors {
                        return_error!(
                            cx,
                            "jsonpath item method .size() can only be applied to an array"
                        );
                    }
                    return Ok(Jper::NotFound);
                }
                size = 1;
            }
            let v = Val::Num(Numeric::of_int(i64::from(size)));
            execute_next_item(cx, next, &v, found)
        }
        "abs" | "floor" | "ceiling" => {
            execute_numeric_item_method(cx, jsp, name, jb, found, unwrap)
        }
        "number" => execute_number_method(cx, jsp, name, jb, found, unwrap),
        _ => {
            if unwrap && is_array(jb) {
                return execute_item_unwrap_target_array(cx, Some(jsp), jb, found, false);
            }
            match name {
                "double" => execute_double(cx, next, jb, found),
                "date" => execute_datetime_method(cx, jsp, name, jb, found),
                "keyvalue" => execute_keyvalue_method(cx, next, jb, found),
                "bigint" | "integer" => execute_integer_method(cx, next, name, jb, found),
                "boolean" => execute_boolean_method(cx, next, jb, found),
                "string" => execute_string_func(cx, next, jb, found),
                _ => execute_string_method(cx, jsp, name, jb, found),
            }
        }
    }
}

/// `getJsonPathItem`: a literal's value, or a variable's, which becomes the base object.
fn get_item(cx: &mut Cx, item: &Item) -> Result<Val> {
    Ok(match item {
        Item::Null => Val::Null,
        Item::Bool(b) => Val::Bool(*b),
        Item::Num(n) => Val::Num(n.clone()),
        Item::Str(s) => Val::Str(lossy(s).into()),
        Item::Variable(name) => {
            let v = cx.vars.and_then(|vars| match vars {
                Val::Box(c, at) => c.find(name).map(|i| c.child(i, *at)),
                _ => None,
            });
            let Some(v) = v else {
                return Err(Error::new(format!(
                    "could not find jsonpath variable \"{}\"",
                    lossy(name)
                )));
            };
            cx.base_id = 1;
            v
        }
        _ => return Err(Error::new("unexpected jsonpath item type")),
    })
}

/// `getArrayIndex`: a subscript's one number, truncated to an integer; the result passed back
/// where it is an error.
fn get_array_index(cx: &mut Cx, jsp: &[Item], jb: &Val) -> Result<std::result::Result<i32, Jper>> {
    let mut found = Vec::new();
    let res = execute_item(cx, jsp, jb, Some(&mut found))?;
    if res == Jper::Error {
        return Ok(Err(res));
    }
    let n = match found.as_slice() {
        [Val::Num(n)] => n.trunc(0),
        _ => {
            if cx.throw_errors {
                return Err(Error::new(
                    "jsonpath array subscript is not a single numeric value",
                ));
            }
            return Ok(Err(Jper::Error));
        }
    };
    match n.to_integer(i64::from(i32::MIN), i64::from(i32::MAX), "integer") {
        Ok(i) => Ok(Ok(i as i32)),
        Err(_) => {
            if cx.throw_errors {
                return Err(Error::new(
                    "jsonpath array subscript is out of integer range",
                ));
            }
            Ok(Err(Jper::Error))
        }
    }
}

/// `executeAnyItem`: `.**`, `.*` and `[*]` over a container's values, from level `first` to level
/// `last` down (`u32::MAX` for the last), the leaves alone where both are `last`; the items after
/// run over each, or each into `found` where none follow.
#[allow(clippy::too_many_arguments)]
fn execute_any_item(
    cx: &mut Cx,
    jsp: Option<&[Item]>,
    c: &Rc<Cont>,
    at: u64,
    mut found: Found,
    level: u32,
    first: u32,
    last: u32,
    ignore_structural_errors: bool,
    unwrap_next: bool,
) -> Result<Jper> {
    let mut res = Jper::NotFound;
    if level > last {
        return Ok(res);
    }
    cx.enter()?;
    for i in 0..c.len() {
        let v = c.child(i, at);
        let is_box = matches!(v, Val::Box(..));
        if level >= first || (first == u32::MAX && last == u32::MAX && !is_box) {
            if let Some(jsp) = jsp {
                if ignore_structural_errors {
                    let saved = cx.ignore_structural_errors;
                    cx.ignore_structural_errors = true;
                    res = execute_item_opt_unwrap_target(
                        cx,
                        jsp,
                        &v,
                        found.as_deref_mut(),
                        unwrap_next,
                    )?;
                    cx.ignore_structural_errors = saved;
                } else {
                    res = execute_item_opt_unwrap_target(
                        cx,
                        jsp,
                        &v,
                        found.as_deref_mut(),
                        unwrap_next,
                    )?;
                }
                if res == Jper::Error || (res == Jper::Ok && found.is_none()) {
                    break;
                }
            } else if let Some(f) = found.as_deref_mut() {
                f.push(v.clone());
            } else {
                cx.depth -= 1;
                return Ok(Jper::Ok);
            }
        }
        if level < last {
            if let Val::Box(cc, cat) = &v {
                res = execute_any_item(
                    cx,
                    jsp,
                    cc,
                    *cat,
                    found.as_deref_mut(),
                    level + 1,
                    first,
                    last,
                    ignore_structural_errors,
                    unwrap_next,
                )?;
                if res == Jper::Error || (res == Jper::Ok && found.is_none()) {
                    break;
                }
            }
        }
    }
    cx.depth -= 1;
    Ok(res)
}

// ---- Predicates ----

/// `executeBoolItem`: a predicate's truth over a value.
fn execute_bool_item(cx: &mut Cx, jsp: &[Item], jb: &Val, can_have_next: bool) -> Result<Jpb> {
    if !can_have_next && jsp.len() > 1 {
        return Err(Error::new("boolean jsonpath item cannot have next item"));
    }
    cx.enter()?;
    let res = bool_over(cx, &jsp[0], jb);
    cx.depth -= 1;
    res
}

fn bool_over(cx: &mut Cx, item: &Item, jb: &Val) -> Result<Jpb> {
    Ok(match item {
        Item::Binary(Op::And, l, r) => {
            let res = execute_bool_item(cx, l, jb, false)?;
            if res == Jpb::False {
                return Ok(Jpb::False);
            }
            let res2 = execute_bool_item(cx, r, jb, false)?;
            if res2 == Jpb::True {
                res
            } else {
                res2
            }
        }
        Item::Binary(Op::Or, l, r) => {
            let res = execute_bool_item(cx, l, jb, false)?;
            if res == Jpb::True {
                return Ok(Jpb::True);
            }
            let res2 = execute_bool_item(cx, r, jb, false)?;
            if res2 == Jpb::False {
                res
            } else {
                res2
            }
        }
        Item::Not(a) => match execute_bool_item(cx, a, jb, false)? {
            Jpb::Unknown => Jpb::Unknown,
            Jpb::True => Jpb::False,
            Jpb::False => Jpb::True,
        },
        Item::IsUnknown(a) => {
            if execute_bool_item(cx, a, jb, false)? == Jpb::Unknown {
                Jpb::True
            } else {
                Jpb::False
            }
        }
        Item::Binary(Op::StartsWith, l, r) => {
            execute_predicate(cx, Pred::StartsWith, l, Some(r), jb, false)?
        }
        Item::Binary(
            op @ (Op::Equal
            | Op::NotEqual
            | Op::Less
            | Op::Greater
            | Op::LessOrEqual
            | Op::GreaterOrEqual),
            l,
            r,
        ) => execute_predicate(cx, Pred::Compare(*op), l, Some(r), jb, true)?,
        Item::LikeRegex { .. } => {
            return Err(Error::new(
                "like_regex is not read yet: it waits for PostgreSQL's regular expressions",
            ))
        }
        Item::Exists(a) => {
            if !cx.lax {
                let mut vals = Vec::new();
                let res =
                    execute_item_opt_unwrap_result_no_throw(cx, a, jb, false, Some(&mut vals))?;
                if res == Jper::Error {
                    return Ok(Jpb::Unknown);
                }
                if vals.is_empty() {
                    Jpb::False
                } else {
                    Jpb::True
                }
            } else {
                let res = execute_item_opt_unwrap_result_no_throw(cx, a, jb, false, None)?;
                match res {
                    Jper::Error => Jpb::Unknown,
                    Jper::Ok => Jpb::True,
                    Jper::NotFound => Jpb::False,
                }
            }
        }
        _ => return Err(Error::new("invalid boolean jsonpath item type")),
    })
}

/// `executeNestedBoolItem`: a filter's predicate, `@` the value it filters.
fn execute_nested_bool_item(cx: &mut Cx, jsp: &[Item], jb: &Val) -> Result<Jpb> {
    let prev = std::mem::replace(&mut cx.current, jb.clone());
    let res = execute_bool_item(cx, jsp, jb, false)?;
    cx.current = prev;
    Ok(res)
}

/// The predicates over pairs of items.
#[derive(Clone, Copy)]
enum Pred {
    Compare(Op),
    StartsWith,
}

/// `executePredicate`: true where some pair of an item of the left and one of the right satisfies
/// the predicate; unknown where an error came first, or in strict mode anywhere.
fn execute_predicate(
    cx: &mut Cx,
    pred: Pred,
    larg: &[Item],
    rarg: Option<&[Item]>,
    jb: &Val,
    unwrap_right: bool,
) -> Result<Jpb> {
    let mut lseq = Vec::new();
    let mut rseq = Vec::new();
    let mut error = false;
    let mut found = false;
    'exit: {
        if execute_item_opt_unwrap_result_no_throw(cx, larg, jb, true, Some(&mut lseq))?
            == Jper::Error
        {
            error = true;
            break 'exit;
        }
        if let Some(r) = rarg {
            if execute_item_opt_unwrap_result_no_throw(cx, r, jb, unwrap_right, Some(&mut rseq))?
                == Jper::Error
            {
                error = true;
                break 'exit;
            }
        }
        for lval in &lseq {
            for rval in &rseq {
                let res = match pred {
                    Pred::Compare(op) => compare_items(op, lval, rval, cx.use_tz)?,
                    Pred::StartsWith => starts_with(lval, rval),
                };
                if res == Jpb::Unknown {
                    error = true;
                    if !cx.lax {
                        found = false;
                        break 'exit;
                    }
                } else if res == Jpb::True {
                    found = true;
                    if cx.lax {
                        break 'exit;
                    }
                }
            }
        }
    }
    Ok(if found {
        Jpb::True
    } else if error {
        Jpb::Unknown
    } else {
        Jpb::False
    })
}

/// `executeStartsWith`: whether the whole string begins with the initial one.
fn starts_with(whole: &Val, initial: &Val) -> Jpb {
    match (whole, initial) {
        (Val::Str(w), Val::Str(i)) => {
            if w.as_bytes().starts_with(i.as_bytes()) {
                Jpb::True
            } else {
                Jpb::False
            }
        }
        _ => Jpb::Unknown,
    }
}

/// `compareItems`: two items compared as their type compares them; a null equal to a null alone,
/// items of two types or two containers unknown.
fn compare_items(op: Op, a: &Val, b: &Val, use_tz: bool) -> Result<Jpb> {
    let cmp = match (a, b) {
        (Val::Null, Val::Null) => Ordering::Equal,
        (Val::Null, _) | (_, Val::Null) => {
            return Ok(if op == Op::NotEqual {
                Jpb::True
            } else {
                Jpb::False
            })
        }
        (Val::Bool(x), Val::Bool(y)) => x.cmp(y),
        (Val::Num(x), Val::Num(y)) => x.cmp(y),
        (Val::Str(x), Val::Str(y)) => {
            if op == Op::Equal {
                return Ok(if x == y { Jpb::True } else { Jpb::False });
            }
            x.as_bytes().cmp(y.as_bytes())
        }
        (Val::Datetime(x), Val::Datetime(y)) => match compare_datetime(x, y, use_tz)? {
            Some(o) => o,
            None => return Ok(Jpb::Unknown),
        },
        _ => return Ok(Jpb::Unknown),
    };
    let res = match op {
        Op::Equal => cmp == Ordering::Equal,
        Op::NotEqual => cmp != Ordering::Equal,
        Op::Less => cmp == Ordering::Less,
        Op::Greater => cmp == Ordering::Greater,
        Op::LessOrEqual => cmp != Ordering::Greater,
        _ => cmp != Ordering::Less,
    };
    Ok(if res { Jpb::True } else { Jpb::False })
}

/// `appendBoolResult`: a predicate's truth as an item, unknown as a JSON null.
fn append_bool_result(cx: &mut Cx, next: &[Item], found: Found, res: Jpb) -> Result<Jper> {
    if next.is_empty() && found.is_none() {
        return Ok(Jper::Ok);
    }
    let v = match res {
        Jpb::Unknown => Val::Null,
        b => Val::Bool(b == Jpb::True),
    };
    execute_next_item(cx, next, &v, found)
}

// ---- Arithmetic ----

/// `executeBinaryArithmExpr`: an operation over the one number each operand gives, an array
/// unwrapped in lax mode.
fn execute_binary_arith(cx: &mut Cx, jsp: &[Item], jb: &Val, found: Found) -> Result<Jper> {
    let Item::Binary(op, l, r) = &jsp[0] else {
        return Err(Error::new("invalid jsonpath arithmetic item"));
    };
    let next = &jsp[1..];
    let mut lseq = Vec::new();
    let mut rseq = Vec::new();
    if execute_item_opt_unwrap_result(cx, l, jb, true, Some(&mut lseq))? == Jper::Error {
        return Ok(Jper::Error);
    }
    if execute_item_opt_unwrap_result(cx, r, jb, true, Some(&mut rseq))? == Jper::Error {
        return Ok(Jper::Error);
    }
    let [Val::Num(lval)] = lseq.as_slice() else {
        return_error!(
            cx,
            "left operand of jsonpath operator {} is not a single numeric value",
            op.name()
        );
    };
    let [Val::Num(rval)] = rseq.as_slice() else {
        return_error!(
            cx,
            "right operand of jsonpath operator {} is not a single numeric value",
            op.name()
        );
    };
    let res = match op {
        Op::Add => lval.add(rval),
        Op::Sub => lval.sub(rval),
        Op::Mul => lval.mul(rval),
        Op::Div => lval.div(rval),
        _ => lval.rem(rval),
    };
    let res = match res {
        Ok(v) => v,
        Err(e) => {
            if cx.throw_errors {
                return Err(e);
            }
            return Ok(Jper::Error);
        }
    };
    if next.is_empty() && found.is_none() {
        return Ok(Jper::Ok);
    }
    execute_next_item(cx, next, &Val::Num(res), found)
}

/// `executeUnaryArithmExpr`: `+` or `-` over each number its operand gives.
fn execute_unary_arith(
    cx: &mut Cx,
    minus: bool,
    arg: &[Item],
    next: &[Item],
    jb: &Val,
    mut found: Found,
) -> Result<Jper> {
    let mut seq = Vec::new();
    if execute_item_opt_unwrap_result(cx, arg, jb, true, Some(&mut seq))? == Jper::Error {
        return Ok(Jper::Error);
    }
    let mut jper = Jper::NotFound;
    let has_next = !next.is_empty();
    for val in seq {
        let Val::Num(n) = val else {
            if found.is_none() && !has_next {
                continue;
            }
            return_error!(
                cx,
                "operand of unary jsonpath operator {} is not a numeric value",
                if minus { "-" } else { "+" }
            );
        };
        if found.is_none() && !has_next {
            return Ok(Jper::Ok);
        }
        let v = Val::Num(if minus { n.neg() } else { n });
        let jper2 = execute_next_item(cx, next, &v, found.as_deref_mut())?;
        if jper2 == Jper::Error {
            return Ok(jper2);
        }
        if jper2 == Jper::Ok {
            jper = Jper::Ok;
            if found.is_none() {
                return Ok(jper);
            }
        }
    }
    Ok(jper)
}

// ---- The item methods ----

/// `executeNumericItemMethod`: `.abs()`, `.floor()`, `.ceiling()` of a number.
fn execute_numeric_item_method(
    cx: &mut Cx,
    jsp: &[Item],
    name: &str,
    jb: &Val,
    found: Found,
    unwrap: bool,
) -> Result<Jper> {
    if unwrap && is_array(jb) {
        return execute_item_unwrap_target_array(cx, Some(jsp), jb, found, false);
    }
    let Val::Num(n) = jb else {
        return_error!(
            cx,
            "jsonpath item method .{name}() can only be applied to a numeric value"
        );
    };
    let v = match name {
        "abs" => n.abs(),
        "floor" => n.floor()?,
        _ => n.ceil()?,
    };
    let next = &jsp[1..];
    if next.is_empty() && found.is_none() {
        return Ok(Jper::Ok);
    }
    execute_next_item(cx, next, &Val::Num(v), found)
}

/// `.double()`: a string read as a double and given as the number it makes; a number checked as
/// a double and given unchanged.
fn execute_double(cx: &mut Cx, next: &[Item], jb: &Val, found: Found) -> Result<Jper> {
    let read = |s: &str| crate::scalars::float_parse(s, true).map(f64::from_bits);
    let v = match jb {
        Val::Num(n) => {
            let tmp = n.to_string();
            let Ok(val) = read(&tmp) else {
                return_error!(
                    cx,
                    "argument \"{tmp}\" of jsonpath item method .double() is invalid for type \
                     double precision"
                );
            };
            if val.is_infinite() || val.is_nan() {
                return_error!(
                    cx,
                    "NaN or Infinity is not allowed for jsonpath item method .double()"
                );
            }
            jb.clone()
        }
        Val::Str(s) => {
            let Ok(val) = read(s) else {
                return_error!(
                    cx,
                    "argument \"{s}\" of jsonpath item method .double() is invalid for type \
                     double precision"
                );
            };
            if val.is_infinite() || val.is_nan() {
                return_error!(
                    cx,
                    "NaN or Infinity is not allowed for jsonpath item method .double()"
                );
            }
            Val::Num(Numeric::of_float(val, true))
        }
        _ => return_error!(
            cx,
            "jsonpath item method .double() can only be applied to a string or numeric value"
        ),
    };
    execute_next_item(cx, next, &v, found)
}

/// `.bigint()` and `.integer()`: a number rounded to the integer type, or a string read as one.
fn execute_integer_method(
    cx: &mut Cx,
    next: &[Item],
    name: &str,
    jb: &Val,
    found: Found,
) -> Result<Jper> {
    let (bits, ty, lo, hi) = if name == "bigint" {
        (64, "bigint", i64::MIN, i64::MAX)
    } else {
        (32, "integer", i64::from(i32::MIN), i64::from(i32::MAX))
    };
    let v = match jb {
        Val::Num(n) => match n.to_integer(lo, hi, ty) {
            Ok(v) => v,
            Err(_) => return_error!(
                cx,
                "argument \"{n}\" of jsonpath item method .{name}() is invalid for type {ty}"
            ),
        },
        Val::Str(s) => match crate::composed::int_in(s, bits) {
            Ok(v) => v,
            Err(_) => return_error!(
                cx,
                "argument \"{s}\" of jsonpath item method .{name}() is invalid for type {ty}"
            ),
        },
        _ => return_error!(
            cx,
            "jsonpath item method .{name}() can only be applied to a string or numeric value"
        ),
    };
    execute_next_item(cx, next, &Val::Num(Numeric::of_int(v)), found)
}

/// `parse_bool`: a truth spelt whole or by the start of its word, in either case.
fn parse_bool(s: &str) -> Option<bool> {
    let l = s.to_ascii_lowercase();
    let prefix = |word: &str, least: usize| l.len() >= least && word.starts_with(l.as_str());
    match l.as_bytes().first()? {
        b't' if prefix("true", 1) => Some(true),
        b'f' if prefix("false", 1) => Some(false),
        b'y' if prefix("yes", 1) => Some(true),
        b'n' if prefix("no", 1) => Some(false),
        b'o' if prefix("on", 2) => Some(true),
        b'o' if prefix("off", 2) => Some(false),
        b'1' if l.len() == 1 => Some(true),
        b'0' if l.len() == 1 => Some(false),
        _ => None,
    }
}

/// `.boolean()`: a boolean as it is, a number read as an integer and true unless zero, a string
/// read as `parse_bool` reads it.
fn execute_boolean_method(cx: &mut Cx, next: &[Item], jb: &Val, found: Found) -> Result<Jper> {
    let b = match jb {
        Val::Bool(b) => *b,
        Val::Num(n) => {
            let tmp = n.to_string();
            match crate::composed::int_in(&tmp, 32) {
                Ok(v) => v != 0,
                Err(_) => return_error!(
                    cx,
                    "argument \"{tmp}\" of jsonpath item method .boolean() is invalid for type \
                     boolean"
                ),
            }
        }
        Val::Str(s) => match parse_bool(s) {
            Some(b) => b,
            None => return_error!(
                cx,
                "argument \"{s}\" of jsonpath item method .boolean() is invalid for type boolean"
            ),
        },
        _ => return_error!(
            cx,
            "jsonpath item method .boolean() can only be applied to a boolean, string, or \
             numeric value"
        ),
    };
    execute_next_item(cx, next, &Val::Bool(b), found)
}

/// `.number()` and `.decimal(precision, scale)`: a number, or a string read as one, NaN and the
/// infinities refused; under `.decimal`'s precision and scale, held to them as `numeric_in` holds
/// a value to its typmod.
fn execute_number_method(
    cx: &mut Cx,
    jsp: &[Item],
    name: &str,
    jb: &Val,
    found: Found,
    unwrap: bool,
) -> Result<Jper> {
    if unwrap && is_array(jb) {
        return execute_item_unwrap_target_array(cx, Some(jsp), jb, found, false);
    }
    let decimal = name == "decimal";
    let special = |n: &Numeric| n.is_nan() || n.is_infinite();
    let (mut num, numstr) = match jb {
        Val::Num(n) => {
            if special(n) {
                return_error!(
                    cx,
                    "NaN or Infinity is not allowed for jsonpath item method .{name}()"
                );
            }
            (n.clone(), decimal.then(|| n.to_string()))
        }
        Val::Str(s) => {
            let Ok(n) = Numeric::parse(s) else {
                return_error!(
                    cx,
                    "argument \"{s}\" of jsonpath item method .{name}() is invalid for type numeric"
                );
            };
            if special(&n) {
                return_error!(
                    cx,
                    "NaN or Infinity is not allowed for jsonpath item method .{name}()"
                );
            }
            (n, Some(s.to_string()))
        }
        _ => return_error!(
            cx,
            "jsonpath item method .{name}() can only be applied to a string or numeric value"
        ),
    };
    if let Item::Decimal(Some(p), s) = &jsp[0] {
        let int4 = |c: &Chain| match c.as_slice() {
            [Item::Num(n)] => Ok(n
                .to_integer(i64::from(i32::MIN), i64::from(i32::MAX), "integer")
                .ok()),
            _ => Err(Error::new(
                "invalid jsonpath item type for .decimal() precision",
            )),
        };
        let Some(precision) = int4(p)? else {
            return_error!(
                cx,
                "precision of jsonpath item method .decimal() is out of range for type integer"
            );
        };
        let mut scale = 0;
        if let Some(s) = s {
            let Some(v) = int4(s)? else {
                return_error!(
                    cx,
                    "scale of jsonpath item method .decimal() is out of range for type integer"
                );
            };
            scale = v;
        }
        let refusal = if !(1..=i64::from(crate::numeric::PRECISION_MAX)).contains(&precision) {
            Some(format!(
                "NUMERIC precision {precision} must be between 1 and {}",
                crate::numeric::PRECISION_MAX
            ))
        } else if !(i64::from(crate::numeric::SCALE_MIN)..=i64::from(crate::numeric::SCALE_MAX))
            .contains(&scale)
        {
            Some(format!(
                "NUMERIC scale {scale} must be between {} and {}",
                crate::numeric::SCALE_MIN,
                crate::numeric::SCALE_MAX
            ))
        } else {
            None
        };
        if let Some(r) = refusal {
            if cx.throw_errors {
                return Err(Error::new(r));
            }
            return Ok(Jper::Error);
        }
        let numstr = numstr.unwrap_or_default();
        match Numeric::parse(&numstr).and_then(|n| n.typmod(precision as u32, scale as i32)) {
            Ok(n) => num = n,
            Err(_) => return_error!(
                cx,
                "argument \"{numstr}\" of jsonpath item method .{name}() is invalid for type \
                 numeric"
            ),
        }
    }
    execute_next_item(cx, &jsp[1..], &Val::Num(num), found)
}

/// `.string()`: a string, a number, a boolean, or a date or time as text.
fn execute_string_func(cx: &mut Cx, next: &[Item], jb: &Val, found: Found) -> Result<Jper> {
    let s: String = match jb {
        Val::Str(s) => s.to_string(),
        Val::Num(n) => n.to_string(),
        Val::Bool(b) => if *b { "true" } else { "false" }.into(),
        Val::Datetime(d) => datetime_text(d)?,
        Val::Null | Val::Box(..) => return_error!(
            cx,
            "jsonpath item method .string() can only be applied to a boolean, string, numeric, \
             or datetime value"
        ),
    };
    execute_next_item(cx, next, &Val::Str(s.into()), found)
}

/// The text functions the string methods call: `replace`, `lower`, `upper`, `initcap`, the trims
/// and `split_part`, under the database's default collation.
fn execute_string_method(
    cx: &mut Cx,
    jsp: &[Item],
    name: &str,
    jb: &Val,
    found: Found,
) -> Result<Jper> {
    let Val::Str(s) = jb else {
        return_error!(
            cx,
            "jsonpath item method .{name}() can only be applied to a string"
        );
    };
    let string = |c: &Chain| match c.as_slice() {
        [Item::Str(v)] => Ok(lossy(v)),
        _ => Err(Error::new(format!(
            "invalid jsonpath item type for .{name}() argument"
        ))),
    };
    let out: String = match &jsp[0] {
        Item::Method("lower") => crate::strings::case(s, false, Casing::Libc)?,
        Item::Method("upper") => crate::strings::case(s, true, Casing::Libc)?,
        Item::Method(_) => crate::strings::title(s, Casing::Libc)?,
        Item::Call2("replace", from, to) => {
            let (from, to) = (string(from)?, string(to)?);
            if from.is_empty() {
                s.to_string()
            } else {
                s.replace(&from, &to)
            }
        }
        Item::Call2(_, delimiter, field) => {
            let delimiter = string(delimiter)?;
            let n = match field.as_slice() {
                [Item::Num(n)] => n.to_integer(i64::from(i32::MIN), i64::from(i32::MAX), "integer"),
                _ => return Err(Error::new("invalid jsonpath item type for .split_part()")),
            };
            let Ok(n) = n else {
                return_error!(
                    cx,
                    "field position of jsonpath item method .split_part() is out of range for \
                     type integer"
                );
            };
            if n == 0 {
                return_error!(
                    cx,
                    "field position of jsonpath item method .split_part() must not be zero"
                );
            }
            split_part(s, &delimiter, n)
        }
        Item::Call(_, arg) => {
            let set: Vec<char> = match arg {
                Some(a) => string(a)?.chars().collect(),
                None => vec![' '],
            };
            let c: Vec<char> = s.chars().collect();
            let (mut lo, mut hi) = (0, c.len());
            if name != "rtrim" {
                while lo < hi && set.contains(&c[lo]) {
                    lo += 1;
                }
            }
            if name != "ltrim" {
                while hi > lo && set.contains(&c[hi - 1]) {
                    hi -= 1;
                }
            }
            c[lo..hi].iter().collect()
        }
        _ => return Err(Error::new("unsupported jsonpath item type")),
    };
    let next = &jsp[1..];
    if next.is_empty() && found.is_none() {
        return Ok(Jper::Ok);
    }
    execute_next_item(cx, next, &Val::Str(out.into()), found)
}

/// `split_part`: the `n`th field of the text between the delimiters, counted from the end where
/// `n` is negative; the empty string past the last.
fn split_part(s: &str, delimiter: &str, n: i64) -> String {
    if s.is_empty() {
        return String::new();
    }
    if delimiter.is_empty() {
        return if n == 1 || n == -1 {
            s.to_string()
        } else {
            String::new()
        };
    }
    let parts: Vec<&str> = s.split(delimiter).collect();
    let at = if n > 0 { n - 1 } else { parts.len() as i64 + n };
    usize::try_from(at)
        .ok()
        .and_then(|at| parts.get(at))
        .map_or(String::new(), |p| p.to_string())
}

/// The formats `.datetime()` and the other date and time methods try, in order, without a
/// template.
const ISO_FORMATS: [&str; 13] = [
    "yyyy-mm-dd",
    "HH24:MI:SS.USTZ",
    "HH24:MI:SSTZ",
    "HH24:MI:SS.US",
    "HH24:MI:SS",
    "yyyy-mm-dd HH24:MI:SS.USTZ",
    "yyyy-mm-dd HH24:MI:SSTZ",
    "yyyy-mm-dd\"T\"HH24:MI:SS.USTZ",
    "yyyy-mm-dd\"T\"HH24:MI:SSTZ",
    "yyyy-mm-dd HH24:MI:SS.US",
    "yyyy-mm-dd HH24:MI:SS",
    "yyyy-mm-dd\"T\"HH24:MI:SS.US",
    "yyyy-mm-dd\"T\"HH24:MI:SS",
];

/// `executeDateTimeMethod`: `.datetime()` with its template or the first ISO format that reads
/// the string, and `.date()`, `.time()`, `.time_tz()`, `.timestamp()` and `.timestamp_tz()`, each
/// casting what the ISO formats read to its type, its precision where it is given.
fn execute_datetime_method(
    cx: &mut Cx,
    jsp: &[Item],
    name: &str,
    jb: &Val,
    found: Found,
) -> Result<Jper> {
    let Val::Str(s) = jb else {
        return_error!(
            cx,
            "jsonpath item method .{name}() can only be applied to a string"
        );
    };
    let arg = match &jsp[0] {
        Item::Call(_, a) => a.as_ref(),
        _ => None,
    };
    let not_recognized = |n: &str| format!("{n} format is not recognized: \"{s}\"");
    let mut time_precision: i32 = -1;
    let mut value = None;
    if let (Some(a), "datetime") = (arg, name) {
        let [Item::Str(template)] = a.as_slice() else {
            return Err(Error::new(
                "invalid jsonpath item type for .datetime() argument",
            ));
        };
        match parse_datetime(s, &lossy(template))? {
            Ok(v) => value = Some(v),
            Err(e) => {
                if cx.throw_errors {
                    return Err(e);
                }
                return Ok(Jper::Error);
            }
        }
    } else {
        if let Some(a) = arg {
            let [Item::Num(n)] = a.as_slice() else {
                return Err(Error::new(format!(
                    "invalid jsonpath item type for {name} argument"
                )));
            };
            match n.to_integer(i64::from(i32::MIN), i64::from(i32::MAX), "integer") {
                Ok(p) => time_precision = p as i32,
                Err(_) => return_error!(
                    cx,
                    "time precision of jsonpath item method .{name}() is out of range for type \
                     integer"
                ),
            }
        }
        for fmt in ISO_FORMATS {
            if let Ok(v) = parse_datetime(s, fmt)? {
                value = Some(v);
                break;
            }
        }
        if value.is_none() {
            return_error!(cx, "{}", not_recognized(name));
        }
    }
    let mut value = value.expect("a value read");
    let precision = |istz: bool, timestamp: bool| -> Result<u32> {
        if time_precision < 0 {
            return Err(Error::new(format!(
                "{}({time_precision}){} precision must not be negative",
                if timestamp { "TIMESTAMP" } else { "TIME" },
                if istz { " WITH TIME ZONE" } else { "" }
            )));
        }
        Ok(time_precision.min(6) as u32)
    };
    use crate::times;
    match name {
        "date" => {
            value = Datetime::Date(match value {
                Datetime::Date(d) => d,
                Datetime::Time(_) | Datetime::TimeTz(..) => {
                    return_error!(cx, "{}", not_recognized("date"))
                }
                Datetime::Timestamp(t) => times::timestamp_date(t)?,
                Datetime::TimestampTz(t, _) => {
                    need_tz(cx.use_tz, "timestamptz", "date")?;
                    times::timestamptz_date(t)?
                }
            });
        }
        "time" => {
            let mut t = match value {
                Datetime::Date(_) => return_error!(cx, "{}", not_recognized("time")),
                Datetime::Time(t) => t,
                Datetime::TimeTz(t, zone) => {
                    need_tz(cx.use_tz, "timetz", "time")?;
                    times::timetz_time(t, zone)
                }
                Datetime::Timestamp(t) => times::timestamp_time(t)?.unwrap_or(0),
                Datetime::TimestampTz(t, _) => {
                    need_tz(cx.use_tz, "timestamptz", "time")?;
                    times::timestamptz_time(t)?.unwrap_or(0)
                }
            };
            if time_precision != -1 {
                t = times::round_time(t, precision(false, false)?);
            }
            value = Datetime::Time(t);
        }
        "time_tz" => {
            let (mut t, zone) = match value {
                Datetime::Date(_) | Datetime::Timestamp(_) => {
                    return_error!(cx, "{}", not_recognized("time_tz"))
                }
                Datetime::Time(t) => {
                    need_tz(cx.use_tz, "time", "timetz")?;
                    times::time_timetz(t)
                }
                Datetime::TimeTz(t, zone) => (t, zone),
                Datetime::TimestampTz(t, _) => times::timestamptz_timetz(t)?.unwrap_or((0, 0)),
            };
            if time_precision != -1 {
                t = times::round_time(t, precision(true, false)?);
            }
            value = Datetime::TimeTz(t, zone);
        }
        "timestamp" => {
            let mut t = match value {
                Datetime::Date(d) => times::date_timestamp(d)?,
                Datetime::Time(_) | Datetime::TimeTz(..) => {
                    return_error!(cx, "{}", not_recognized("timestamp"))
                }
                Datetime::Timestamp(t) => t,
                Datetime::TimestampTz(t, _) => {
                    need_tz(cx.use_tz, "timestamptz", "timestamp")?;
                    times::timestamptz_timestamp(t)
                }
            };
            if time_precision != -1 {
                match times::round_instant(t, precision(false, true)?) {
                    Ok(r) => t = r,
                    Err(_) => return_error!(
                        cx,
                        "time precision of jsonpath item method .{name}() is invalid"
                    ),
                }
            }
            value = Datetime::Timestamp(t);
        }
        "timestamp_tz" => {
            let (mut t, tz) = match value {
                Datetime::Date(d) => {
                    need_tz(cx.use_tz, "date", "timestamptz")?;
                    (times::date_timestamptz(d)?, 0)
                }
                Datetime::Time(_) | Datetime::TimeTz(..) => {
                    return_error!(cx, "{}", not_recognized("timestamp_tz"))
                }
                Datetime::Timestamp(t) => {
                    need_tz(cx.use_tz, "timestamp", "timestamptz")?;
                    (times::timestamp_timestamptz(t), 0)
                }
                Datetime::TimestampTz(t, tz) => (t, tz),
            };
            if time_precision != -1 {
                match times::round_instant(t, precision(true, true)?) {
                    Ok(r) => t = r,
                    Err(_) => return_error!(
                        cx,
                        "time precision of jsonpath item method .{name}() is invalid"
                    ),
                }
            }
            value = Datetime::TimestampTz(t, tz);
        }
        _ => {}
    }
    let next = &jsp[1..];
    if next.is_empty() && found.is_none() {
        return Ok(Jper::Ok);
    }
    execute_next_item(cx, next, &Val::Datetime(value), found)
}

/// `.keyvalue()`: an object's pairs, each as an object of its key, its value and an id: where the
/// object begins in its base object's jsonb, plus 10^10 times that base object's id.
fn execute_keyvalue_method(cx: &mut Cx, next: &[Item], jb: &Val, mut found: Found) -> Result<Jper> {
    let (c, at) = match jb {
        Val::Box(c, at) if c.is_object() => (c.clone(), *at),
        _ => return_error!(
            cx,
            "jsonpath item method .keyvalue() can only be applied to an object"
        ),
    };
    if c.len() == 0 {
        return Ok(Jper::NotFound);
    }
    let has_next = !next.is_empty();
    let id = Numeric::of_int(at as i64 + cx.base_id * 10_000_000_000);
    let names: Vec<Rc<str>> = vec!["id".into(), "key".into(), "value".into()];
    let mut res = Jper::NotFound;
    let keys = c.keys.as_ref().expect("an object's keys");
    for (i, key) in keys.iter().enumerate() {
        res = Jper::Ok;
        if !has_next && found.is_none() {
            break;
        }
        let pair = container(
            Some(names.clone()),
            vec![Val::Num(id.clone()), Val::Str(key.clone()), c.child(i, at)],
        );
        let base = cx.base_id;
        cx.base_id = cx.last_generated_id;
        cx.last_generated_id += 1;
        res = execute_next_item(cx, next, &Val::Box(pair, 0), found.as_deref_mut())?;
        cx.base_id = base;
        if res == Jper::Error {
            return Ok(res);
        }
        if res == Jper::Ok && found.is_none() {
            break;
        }
    }
    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the module answers for a case, written as the oracle writes PostgreSQL's answer.
    fn answer(
        f: &str,
        target: &str,
        path: &str,
        vars: Option<&str>,
        silent: bool,
        tz: bool,
    ) -> String {
        let truth = |b: Option<bool>| b.map_or("<NULL>".to_string(), |b| b.to_string());
        let vars = vars.unwrap_or("{}");
        let got = match f {
            "exists_op" => jsonb_path_exists_op(target, path).map(truth),
            "match_op" => jsonb_path_match_op(target, path).map(truth),
            "exists" => jsonb_path_exists(target, path, vars, silent, tz).map(truth),
            "match" => jsonb_path_match(target, path, vars, silent, tz).map(truth),
            "query" => jsonb_path_query(target, path, vars, silent, tz).map(|r| r.join("\n")),
            "query_array" => jsonb_path_query_array(target, path, vars, silent, tz),
            _ => jsonb_path_query_first(target, path, vars, silent, tz)
                .map(|r| r.unwrap_or_else(|| "<NULL>".into())),
        };
        got.unwrap_or_else(|e| format!("ERROR: {e}"))
    }

    /// The walk: each case of the file `JSONPATH_CASES` names (COPY text: id, function, target,
    /// path, vars, silent, tz, PostgreSQL's answer) answered here and compared; the differences
    /// and the refusals written to `JSONPATH_DIFFS`.
    /// the path, the variables, silent, tz, and the answer.
    type Case = (
        &'static str,
        &'static str,
        &'static str,
        Option<&'static str>,
        bool,
        bool,
        &'static str,
    );

    fn answers_as_postgresql(cases: &[Case]) {
        for &(f, target, path, vars, silent, tz, want) in cases {
            let got = answer(f, target, path, vars, silent, tz);
            assert_eq!(got, want, "{f}({target}, {path}, {vars:?}, {silent}, {tz})");
        }
    }

    /// keyvalue: each pair an object of its key, its value and its id, where its object begins in its base object's binary form plus 10^10 times that object's id.
    #[test]
    fn keyvalue_ids() {
        answers_as_postgresql(&[
            ("query", "{\"a\": 1000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000, \"b\": {\"c\": 1}}", "$.b.keyvalue().id", Some("{}"), false, false, "36"),
            ("query", "{\"a\": 0.000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001, \"b\": {\"c\": 1}}", "$.b.keyvalue().id", Some("{}"), false, false, "36"),
            ("query", "{\"a\": 0.00000000000000000000000000000000000000000000000000000000000000000000001, \"b\": {\"c\": 1}}", "$.b.keyvalue().id", Some("{}"), false, false, "36"),
            ("query", "{\"a\": 123456789012345678901234567890, \"b\": {\"c\": [1, {\"d\": 2}]}}", "$.** ? (@.type() == \"object\").keyvalue().id", Some("{}"), false, false, "0\n0\n48\n84\n84"),
            ("query", "{\"a\": \"x\", \"aa\": \"yy\", \"aaa\": {\"b\": [1.5, -2, 30000, \"z\", null, true, {\"q\": 1}]}}", "$.** ? (@.type() == \"object\").keyvalue().id", Some("{}"), false, false, "0\n0\n0\n40\n120\n120"),
            ("query", "[1, {\"a\": {\"b\": 1}}, [2, {\"c\": 3}], {\"a\": [true, null, \"é\", 1, [], {}], \"b\": {\"c\": {}}}]", "$.** ? (@.type() == \"object\").keyvalue()", Some("{}"), false, false, "{\"id\": 28, \"key\": \"a\", \"value\": {\"b\": 1}}\n{\"id\": 112, \"key\": \"a\", \"value\": [true, null, \"é\", 1, [], {}]}\n{\"id\": 112, \"key\": \"b\", \"value\": {\"c\": {}}}\n{\"id\": 28, \"key\": \"a\", \"value\": {\"b\": 1}}\n{\"id\": 44, \"key\": \"b\", \"value\": 1}\n{\"id\": 88, \"key\": \"c\", \"value\": 3}\n{\"id\": 88, \"key\": \"c\", \"value\": 3}\n{\"id\": 112, \"key\": \"a\", \"value\": [true, null, \"é\", 1, [], {}]}\n{\"id\": 112, \"key\": \"b\", \"value\": {\"c\": {}}}\n{\"id\": 184, \"key\": \"c\", \"value\": {}}"),
            ("query", "{\"a\": {\"b\": {\"c\": 1}}, \"aa\": 1}", "$.keyvalue() ? (@.key == \"a\").value.keyvalue().value.keyvalue()", Some("{}"), false, false, "{\"id\": 30000000056, \"key\": \"c\", \"value\": 1}"),
            ("query", "{\"a\": 12345.678, \"b\": [0.00001, {\"c\": 1}]}", "$.b[1].keyvalue().id", Some("{}"), false, false, "56"),
            ("query", "{\"x\": 1}", "$o.keyvalue()", Some("{\"o\": {\"p\": {\"q\": 1}}, \"aaaa\": [1, 2, 3]}"), false, false, "{\"id\": 10000000028, \"key\": \"p\", \"value\": {\"q\": 1}}"),
            ("query", "{\"x\": 1}", "$o.p.keyvalue()", Some("{\"o\": {\"p\": {\"q\": 1}}, \"aaaa\": [1, 2, 3]}"), false, false, "{\"id\": 10000000044, \"key\": \"q\", \"value\": 1}"),
            ("query", "{\"x\": {\"y\": {\"z\": 1}}}", "$.keyvalue().value.keyvalue().value.keyvalue()", Some("{}"), false, false, "{\"id\": 30000000056, \"key\": \"z\", \"value\": 1}"),
            ("exists_op", "{\"x\": {\"y\": 1}}", "$.keyvalue() ? (@.id == 0)", None, true, false, "true"),
            ("exists_op", "{\"x\": {\"y\": 1}}", "$.x.keyvalue() ? (@.id == 1000000000012)", None, true, false, "false"),
            ("query", "[[1, 2]]", "lax $.keyvalue()", Some("{}"), false, false, "ERROR: jsonpath item method .keyvalue() can only be applied to an object"),
            ("query", "{}", "$.keyvalue()", Some("{}"), false, false, ""),
            ("query", "{\"a\": {}}", "$.a.keyvalue()", Some("{}"), false, false, ""),
            ("query", "[{\"a\": 1}, {\"b\": 2}]", "$.keyvalue()", Some("{}"), false, false, "{\"id\": 12, \"key\": \"a\", \"value\": 1}\n{\"id\": 36, \"key\": \"b\", \"value\": 2}"),
            ("query", "[{\"a\": 1}, {\"b\": 2}]", "$[*].keyvalue().id", Some("{}"), false, false, "12\n36"),
        ]);
    }

    /// The accessors in lax and strict mode: keys, `*`, `**` and its levels, subscripts, ranges and `last`, an array unwrapped or a value wrapped in lax mode.
    #[test]
    fn accessors_in_both_modes() {
        answers_as_postgresql(&[
            ("query", "[]", "$[last]", Some("{}"), false, false, ""),
            (
                "query",
                "[]",
                "strict $[last]",
                Some("{}"),
                false,
                false,
                "ERROR: jsonpath array subscript is out of bounds",
            ),
            (
                "query",
                "[[1, 2], [3]]",
                "$[*][last]",
                Some("{}"),
                false,
                false,
                "2\n3",
            ),
            (
                "query",
                "[1, 2, 3]",
                "$[last - 1 to last]",
                Some("{}"),
                false,
                false,
                "2\n3",
            ),
            (
                "query",
                "[1, 2, 3]",
                "$[$.size() - 1]",
                Some("{}"),
                false,
                false,
                "3",
            ),
            (
                "query",
                "{\"a\": 1}",
                "strict $.b",
                Some("{}"),
                false,
                false,
                "ERROR: JSON object does not contain key \"b\"",
            ),
            (
                "query",
                "{\"a\\\"b\": 1}",
                "strict $.\"c\\\"d\"",
                Some("{}"),
                false,
                false,
                "ERROR: JSON object does not contain key \"c\"d\"",
            ),
            (
                "query",
                "[1, 2, 3]",
                "strict $[*].a",
                Some("{}"),
                true,
                false,
                "",
            ),
            (
                "query_array",
                "[{\"a\": 1}, 2, {\"a\": 3}]",
                "strict $[*].a",
                Some("{}"),
                true,
                false,
                "[1]",
            ),
            (
                "query",
                "[1, 2]",
                "$[1.9999999999]",
                Some("{}"),
                false,
                false,
                "2",
            ),
            ("query", "[1, 2]", "$[-0.5]", Some("{}"), false, false, "1"),
            (
                "query",
                "[1, 2]",
                "strict $[2147483648]",
                Some("{}"),
                false,
                false,
                "ERROR: jsonpath array subscript is out of integer range",
            ),
            (
                "query",
                "[1, 2]",
                "$[$x]",
                Some("{\"x\": \"1\"}"),
                false,
                false,
                "ERROR: jsonpath array subscript is not a single numeric value",
            ),
            (
                "query",
                "[1, 2]",
                "$[$x]",
                Some("{\"x\": 1.5}"),
                false,
                false,
                "2",
            ),
            (
                "query",
                "[1, 2]",
                "$.**{0}",
                Some("{}"),
                false,
                false,
                "[1, 2]",
            ),
            (
                "query",
                "[1, [2]]",
                "strict $.**{1 to 1}.a",
                Some("{}"),
                false,
                false,
                "",
            ),
            (
                "query",
                "{\"a\": {\"b\": 1}}",
                "$.**{1 to 2}",
                Some("{}"),
                false,
                false,
                "{\"b\": 1}\n1",
            ),
            (
                "query",
                "{\"a\": {\"b\": 1}}",
                "$.**{2 to 1}",
                Some("{}"),
                false,
                false,
                "",
            ),
            (
                "query",
                "{\"a\": [1, {\"b\": 2}]}",
                "lax $.**.b",
                Some("{}"),
                false,
                false,
                "2\n2",
            ),
            (
                "query",
                "{\"a\": [1, {\"b\": 2}]}",
                "strict $.**.b",
                Some("{}"),
                false,
                false,
                "2",
            ),
            (
                "query",
                "{\"a\": [1, {\"b\": 2}]}",
                "strict $.*[*].b",
                Some("{}"),
                false,
                false,
                "ERROR: jsonpath member accessor can only be applied to an object",
            ),
            (
                "query",
                "[{\"a\": 1}, {\"b\": 2}]",
                "strict $.*",
                Some("{}"),
                false,
                false,
                "ERROR: jsonpath wildcard member accessor can only be applied to an object",
            ),
            (
                "query",
                "[{\"a\": 1}, {\"b\": 2}]",
                "lax $.*",
                Some("{}"),
                false,
                false,
                "1\n2",
            ),
            ("query", "\"x\"", "$.*", Some("{}"), false, false, ""),
            (
                "query",
                "\"x\"",
                "strict $[*]",
                Some("{}"),
                false,
                false,
                "ERROR: jsonpath wildcard array accessor can only be applied to an array",
            ),
            (
                "query",
                "\"x\"",
                "strict $[0]",
                Some("{}"),
                false,
                false,
                "ERROR: jsonpath array accessor can only be applied to an array",
            ),
            (
                "query",
                "\"x\"",
                "lax $[0, 0 to 0, last]",
                Some("{}"),
                false,
                false,
                "\"x\"\n\"x\"\n\"x\"",
            ),
        ]);
    }

    /// The methods over numbers and the strings that read as numbers: double, bigint, integer, number, decimal with its precision and scale, boolean, abs, floor, ceiling, size and type.
    #[test]
    fn number_methods() {
        answers_as_postgresql(&[
            ("query", "1", "$.double()", Some("{}"), false, false, "1"),
            ("query", "\"1e400\"", "$.double()", Some("{}"), false, false, "ERROR: argument \"1e400\" of jsonpath item method .double() is invalid for type double precision"),
            ("query", "10000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000", "$.double()", Some("{}"), false, false, "ERROR: argument \"10000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000\" of jsonpath item method .double() is invalid for type double precision"),
            ("query", "\"0x1A\"", "$.integer()", Some("{}"), false, false, "26"),
            ("query", "\"1_000\"", "$.bigint()", Some("{}"), false, false, "1000"),
            ("query", "\"  12  \"", "$.integer()", Some("{}"), false, false, "12"),
            ("query", "\" true\"", "$.boolean()", Some("{}"), false, false, "ERROR: argument \" true\" of jsonpath item method .boolean() is invalid for type boolean"),
            ("query", "\"of\"", "$.boolean()", Some("{}"), false, false, "false"),
            ("query", "1.0", "$.boolean()", Some("{}"), false, false, "ERROR: argument \"1.0\" of jsonpath item method .boolean() is invalid for type boolean"),
            ("query", "-0.5", "$.integer()", Some("{}"), false, false, "-1"),
            ("query", "2.5", "$.integer()", Some("{}"), false, false, "3"),
            ("query", "-2.5", "$.bigint()", Some("{}"), false, false, "-3"),
            ("query", "12.345", "$.decimal(4,2)", Some("{}"), false, false, "12.35"),
            ("query", "12.345", "$.decimal(3,2)", Some("{}"), true, false, ""),
            ("query", "\"12.3450\"", "$.decimal()", Some("{}"), false, false, "12.3450"),
            ("query", "\"12.3450\"", "$.number()", Some("{}"), false, false, "12.3450"),
            ("query", "1234", "$.decimal(2,-2)", Some("{}"), false, false, "1200"),
            ("query", "1", "$.decimal(0)", Some("{}"), true, false, ""),
            ("query", "1", "$.decimal(1, 1001)", Some("{}"), false, false, "ERROR: NUMERIC scale 1001 must be between -1000 and 1000"),
            ("query", "\"1e1000\"", "$.double()", Some("{}"), true, false, ""),
            ("query", "{\"a\": [1, 2]}", "$.a.size()", Some("{}"), false, false, "2"),
            ("query", "{\"a\": 1}", "strict $.a.size()", Some("{}"), false, false, "ERROR: jsonpath item method .size() can only be applied to an array"),
            ("query", "{\"a\": 1}", "strict $.a.size()", Some("{}"), true, false, ""),
            ("query", "{\"a\": \"x\"}", "$.a.type().size()", Some("{}"), false, false, "1"),
            ("query", "[1, 2]", "$.type()", Some("{}"), false, false, "\"array\""),
            ("query", "[1, 2]", "strict $.abs()", Some("{}"), false, false, "ERROR: jsonpath item method .abs() can only be applied to a numeric value"),
            ("query", "[1, [2]]", "lax $.abs()", Some("{}"), false, false, "ERROR: jsonpath item method .abs() can only be applied to a numeric value"),
            ("query", "[1, [2]]", "lax $.floor()", Some("{}"), true, false, "1"),
            ("query", "[-1, 0, 7]", "$[*].boolean()", Some("{}"), false, false, "true\nfalse\ntrue"),
            ("query", "1234.5", "$.decimal(5)", Some("{}"), false, false, "1235"),
            ("query", "[\"1.5e2\", \" 12.25 \", \"0x1p3\", \"1e-400\", \"-0\"]", "$[*].double()", Some("{}"), true, false, "150\n12.25\n8"),
        ]);
    }

    /// The string methods under the default collation, and strings compared by their bytes.
    #[test]
    fn string_methods() {
        answers_as_postgresql(&[
            (
                "query",
                "[\"é\", \"e\", \"z\", \"É\", \"日本\"]",
                "$[*] ? (@ > \"e\")",
                Some("{}"),
                false,
                false,
                "\"é\"\n\"z\"\n\"É\"\n\"日本\"",
            ),
            (
                "query",
                "[\"ǅ\", \"ß\", \"İ\", \"ﬀ\", \"Σσς\"]",
                "$[*].upper()",
                Some("{}"),
                false,
                false,
                "\"Ǆ\"\n\"ß\"\n\"İ\"\n\"ﬀ\"\n\"ΣΣΣ\"",
            ),
            (
                "query",
                "[\"ǅ\", \"ß\", \"İ\", \"ﬀ\", \"ΣΑΣ\"]",
                "$[*].lower()",
                Some("{}"),
                false,
                false,
                "\"ǆ\"\n\"ß\"\n\"i\"\n\"ﬀ\"\n\"σασ\"",
            ),
            (
                "query",
                "[\"hello wORLD\", \"o''neil\", \"x1y2\", \"ǆa ǅb\"]",
                "$[*].initcap()",
                Some("{}"),
                false,
                false,
                "\"Hello World\"\n\"O''Neil\"\n\"X1y2\"\n\"Ǆa Ǆb\"",
            ),
            (
                "query",
                "\"a~@~b~@~c\"",
                "$.split_part(\"~@~\", -4)",
                Some("{}"),
                false,
                false,
                "\"\"",
            ),
            (
                "query",
                "\"a~@~b~@~c\"",
                "$.split_part(\"~@~\", -3)",
                Some("{}"),
                false,
                false,
                "\"a\"",
            ),
            (
                "query",
                "\"xxaxx\"",
                "$.btrim(\"x\")",
                Some("{}"),
                false,
                false,
                "\"a\"",
            ),
            (
                "query",
                "\"äöäxä\"",
                "$.ltrim(\"ä\")",
                Some("{}"),
                false,
                false,
                "\"öäxä\"",
            ),
            (
                "query",
                "\"abc\"",
                "$ starts with \"ab\"",
                Some("{}"),
                false,
                false,
                "true",
            ),
            (
                "query",
                "[\"abc\", 1, \"ab\"]",
                "strict $[*] starts with \"ab\"",
                Some("{}"),
                false,
                false,
                "null",
            ),
            (
                "query",
                "[\"abc\", 1, \"ab\"]",
                "lax $[*] starts with \"ab\"",
                Some("{}"),
                false,
                false,
                "true",
            ),
            (
                "query",
                "[\"abc\", \"xab\"]",
                "$[*] ? (@ starts with $y)",
                Some("{\"y\": \"x\"}"),
                false,
                false,
                "\"xab\"",
            ),
        ]);
    }

    /// The date and time methods: the ISO formats tried in order, templates read in standard mode, each cast and its precision, zones written as `+hh:mm`, the casts that need a zone refused outside the `_tz` functions.
    #[test]
    fn datetime_methods() {
        answers_as_postgresql(&[
            ("query", "\"2023-08-15\"", "$.datetime().string()", Some("{}"), false, false, "\"2023-08-15\""),
            ("query", "\"0010-01-01 BC\"", "$.datetime(\"yyyy-mm-dd BC\")", Some("{}"), false, false, "\"0010-01-01 BC\""),
            ("query", "\"0010-01-01 BC\"", "$.datetime(\"yyyy-mm-dd BC\").string()", Some("{}"), false, false, "\"0010-01-01 BC\""),
            ("query", "\"10-01-01 12:00 BC\"", "$.datetime(\"yyyy-mm-dd HH24:MI BC\")", Some("{}"), false, false, "\"0010-01-01T12:00:00 BC\""),
            ("query", "\"2023-08-15 12:34:56.789+05:30\"", "$.timestamp_tz().string()", Some("{}"), false, false, "\"2023-08-15T12:34:56.789+05:30\""),
            ("query", "\"2023-08-15 12:34:56.789+05:30\"", "$.timestamp_tz(1)", Some("{}"), false, false, "\"2023-08-15T12:34:56.8+05:30\""),
            ("query", "\"2023-08-15 12:34:56.789+05:30\"", "$.timestamp_tz(0)", Some("{}"), false, false, "\"2023-08-15T12:34:57+05:30\""),
            ("query", "\"2023-08-15 12:34:56.789+05:30\"", "$.timestamp()", Some("{}"), false, true, "\"2023-08-15T07:04:56.789\""),
            ("query", "\"2023-08-15 12:34:56.789+05:30\"", "$.time_tz()", Some("{}"), false, false, "\"07:04:56.789+00:00\""),
            ("query", "\"2023-08-15 12:34:56.789+05:30\"", "$.time()", Some("{}"), false, true, "\"07:04:56.789\""),
            ("query", "\"2023-08-15 12:34:56.789+05:30\"", "$.date()", Some("{}"), false, true, "\"2023-08-15\""),
            ("query", "\"12:34:56.789+05:30\"", "$.time_tz(0)", Some("{}"), false, false, "\"12:34:57+05:30\""),
            ("query", "\"23:59:59.9\"", "$.time(0)", Some("{}"), false, false, "\"24:00:00\""),
            ("query", "\"2023-08-15\"", "$.timestamp_tz()", Some("{}"), false, true, "\"2023-08-15T00:00:00+00:00\""),
            ("query", "\"2023-08-15 12:00:00\"", "$.timestamp_tz()", Some("{}"), false, true, "\"2023-08-15T12:00:00+00:00\""),
            ("query", "\"12:00:00\"", "$.time_tz()", Some("{}"), false, true, "\"12:00:00+00:00\""),
            ("query", "[\"12:00:00\", \"12:00:00+01\", \"11:00:00+00\"]", "$[*].datetime() ? (@ < \"12:00:00+00\".time_tz())", Some("{}"), false, true, "\"12:00:00+01:00\"\n\"11:00:00+00:00\""),
            ("query", "[\"12:00:00+01\", \"11:00:00+00\"]", "$[*].datetime() ? (@ == \"11:00:00+00\".time_tz())", Some("{}"), false, false, "\"11:00:00+00:00\""),
            ("query", "[\"2023-08-15\", \"2023-08-15 00:00:00\", \"2023-08-15 00:00:00+00\"]", "$[*].datetime() ? (@ == \"2023-08-15\".date())", Some("{}"), false, true, "\"2023-08-15\"\n\"2023-08-15T00:00:00\"\n\"2023-08-15T00:00:00+00:00\""),
            ("query", "[\"2023-08-15\", \"2023-08-15 00:00:00\", \"2023-08-15 00:00:00+00\"]", "$[*].datetime() ? (@ == \"2023-08-15\".date())", Some("{}"), false, false, "ERROR: cannot convert value from date to timestamptz without time zone usage"),
            ("query", "[\"2023-08-15\", \"12:00:00\"]", "$[*].datetime() ? (@ == \"2023-08-15\".date())", Some("{}"), false, false, "\"2023-08-15\""),
            ("query", "\"2023-08-15 12:34:56\"", "$.datetime(\"yyyy-mm-dd HH24:MI:SS\").type()", Some("{}"), false, false, "\"timestamp without time zone\""),
            ("query", "\"12:34:56 +05:30\"", "$.datetime(\"HH24:MI:SS TZH:TZM\").type()", Some("{}"), false, false, "\"time with time zone\""),
            ("query", "\"2023-08-15\"", "$.datetime(\"yyyy-mm-dd#\")", Some("{}"), true, false, "ERROR: invalid datetime format separator: \"#\""),
            ("query", "\"2023-08-15\"", "$.datetime(\"yyyy-mm-dd\\\\\")", Some("{}"), true, false, "ERROR: invalid datetime format separator: \"\\\""),
            ("query", "\"5874897-12-31\"", "$.datetime(\"yyyy-mm-dd\")", Some("{}"), false, false, "\"5874897-12-31\""),
            ("query", "\"5874898-01-01\"", "$.datetime(\"yyyy-mm-dd\")", Some("{}"), false, false, "ERROR: date out of range: \"5874898-01-01\""),
            ("query", "\"294277-01-01 00:00:00\"", "$.datetime(\"yyyy-mm-dd HH24:MI:SS\")", Some("{}"), false, false, "ERROR: timestamp out of range"),
            ("query", "\"2023-08-15 12:34:56.123456789\"", "$.datetime()", Some("{}"), false, false, "ERROR: datetime format is not recognized: \"2023-08-15 12:34:56.123456789\""),
            ("query", "\"2023-08-15 12:34:56.1234567\"", "$.datetime(\"yyyy-mm-dd HH24:MI:SS.US\")", Some("{}"), false, false, "ERROR: date/time field value out of range: \"2023-08-15 12:34:56.1234567\""),
            ("query", "\"2023-08-15 12:34:56.12\"", "$.datetime(\"yyyy-mm-dd HH24:MI:SS.FF1\")", Some("{}"), false, false, "\"2023-08-15T12:34:56.1\""),
            ("query", "\"2023-08-15 12:34:56.16\"", "$.datetime(\"yyyy-mm-dd HH24:MI:SS.FF2\").string()", Some("{}"), false, false, "\"2023-08-15T12:34:56.16\""),
            ("query", "1", "$x.type()", Some("{\"x\": \"2023-08-15\"}"), false, false, "\"string\""),
            ("query", "1", "$x.datetime().type()", Some("{\"x\": \"2023-08-15\"}"), false, false, "\"date\""),
            ("query", "[\"2023-08-15T12:34:56+05:30\", \"2023-08-15T12:34:56\", \"2023-08-15T12:34:56.5\"]", "$[*].datetime()", Some("{}"), false, false, "\"2023-08-15T12:34:56+05:30\"\n\"2023-08-15T12:34:56\"\n\"2023-08-15T12:34:56.5\""),
        ]);
    }

    /// The arithmetic over single numbers, unary operators over each number, errors thrown, passed over under silent with the items found before them kept, and the missing variables and keys.
    #[test]
    fn arithmetic_and_errors() {
        answers_as_postgresql(&[
            ("query", "{\"a\": 1}", "$x", Some("{\"x y\": 1}"), false, false, "ERROR: could not find jsonpath variable \"x\""),
            ("query", "{\"a\": 1}", "$\"x y\"", Some("{\"x y\": [1, 2]}"), false, false, "[1, 2]"),
            ("query", "[1, \"a\", 2]", "lax $[*] + 1", Some("{}"), true, false, ""),
            ("query", "[1, [2, [3]]]", "-$[*]", Some("{}"), false, false, "ERROR: operand of unary jsonpath operator - is not a numeric value"),
            ("query", "[1, \"a\"]", "-$[*]", Some("{}"), false, false, "ERROR: operand of unary jsonpath operator - is not a numeric value"),
            ("exists_op", "[1, \"a\"]", "-$[*]", None, true, false, "true"),
            ("exists_op", "[1, \"a\"]", "strict -$[*]", None, true, false, "<NULL>"),
            ("query", "1", "$ ? (@ == 1).abs().floor()", Some("{}"), false, false, "1"),
            ("query", "1", "$x", Some("{\"x\": null}"), false, false, "null"),
            ("query", "{\"a\": 1}", "($.a + 1) * 2 / 3 % 1", Some("{}"), false, false, "0.3333333333333333"),
            ("query", "{\"a\": 1}", "$.a / 0", Some("{}"), true, false, ""),
            ("query", "{\"a\": 1}", "$.a / 0", Some("{}"), false, false, "ERROR: division by zero"),
            ("query", "{\"a\": 10000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000}", "$.a * $.a", Some("{}"), false, false, "ERROR: value overflows numeric format"),
            ("exists", "1", "$x", Some("[]"), false, false, "ERROR: \"vars\" argument is not an object"),
            ("exists", "1", "$x", Some("\"x\""), true, false, "ERROR: \"vars\" argument is not an object"),
        ]);
    }

    /// Predicates: comparisons over every pair, nulls, booleans, containers and mixed types, starts with, exists, is unknown, && and ||, and jsonb_path_match's one boolean.
    #[test]
    fn predicates() {
        answers_as_postgresql(&[
            (
                "match_op",
                "[1, \"a\"]",
                "$[*] > 0",
                None,
                true,
                false,
                "true",
            ),
            (
                "match_op",
                "[1, \"a\"]",
                "strict $[*] > 0",
                None,
                true,
                false,
                "<NULL>",
            ),
            (
                "query",
                "{\"a\": [1, 2], \"b\": [2, 3]}",
                "$ ? (@.a[*] == @.b[*]).a",
                Some("{}"),
                false,
                false,
                "[1, 2]",
            ),
            (
                "query",
                "{\"a\": [1, 2], \"b\": [[2], 3]}",
                "lax $.a[*] == $.b[*]",
                Some("{}"),
                false,
                false,
                "true",
            ),
            (
                "query",
                "{\"a\": {}, \"b\": {}}",
                "$.a == $.b",
                Some("{}"),
                false,
                false,
                "null",
            ),
            (
                "query",
                "{\"a\": null, \"b\": 1}",
                "$.a != $.b",
                Some("{}"),
                false,
                false,
                "true",
            ),
            (
                "query",
                "{\"a\": null, \"b\": 1}",
                "$.a < $.b",
                Some("{}"),
                false,
                false,
                "false",
            ),
            (
                "query",
                "{\"a\": true, \"b\": false}",
                "$.a > $.b",
                Some("{}"),
                false,
                false,
                "true",
            ),
            (
                "query",
                "1",
                "exists($.a) || $.b == 1",
                Some("{}"),
                false,
                false,
                "false",
            ),
            (
                "query",
                "1",
                "strict exists($.a)",
                Some("{}"),
                false,
                false,
                "null",
            ),
            (
                "query",
                "{\"a\": 1}",
                "strict exists($.a.b) is unknown",
                Some("{}"),
                false,
                false,
                "ERROR: syntax error at or near \" \" of jsonpath input",
            ),
            ("match", "1", "$ == 1", Some("{}"), false, false, "true"),
            (
                "match",
                "1",
                "$",
                Some("{}"),
                false,
                false,
                "ERROR: single boolean result is expected",
            ),
            ("match", "1", "$", Some("{}"), true, false, "<NULL>"),
            (
                "match",
                "[true, true]",
                "$[*]",
                Some("{}"),
                false,
                false,
                "ERROR: single boolean result is expected",
            ),
            ("match", "null", "$", Some("{}"), false, false, "<NULL>"),
            (
                "query",
                "{\"a\": 1}",
                "strict exists($.a)",
                Some("{}"),
                false,
                false,
                "true",
            ),
            (
                "query",
                "{\"a\": 1}",
                "strict exists($.a ? (@ > 5))",
                Some("{}"),
                false,
                false,
                "false",
            ),
        ]);
    }

    /// A run deeper than the caller's stack holds runs on a thread of its own: a path of 1000 keys
    /// and `**` down a document 5000 deep, each as PostgreSQL answers them.
    #[test]
    fn deep_runs_take_their_own_stack() {
        let doc = |n: usize| "{\"a\": ".repeat(n) + "1" + &"}".repeat(n);
        let chain = "$".to_string() + &".a".repeat(1000);
        assert_eq!(
            jsonb_path_query(&doc(1000), &chain, "{}", false, false).unwrap(),
            vec!["1".to_string()]
        );
        assert_eq!(
            jsonb_path_query(&doc(5000), "$.**", "{}", false, false)
                .unwrap()
                .len(),
            5001
        );
    }
}
