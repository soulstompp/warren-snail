//! json's and jsonb's functions as `jsonfuncs.c`, `json.c` and `jsonb.c` compute them. A json is
//! read by the parser's callbacks as each function's own callbacks read it, de-escaping its strings
//! only where that function asks for them; a jsonb is read into its tokens and written again as
//! `jsonb_out` writes it, a container made or changed being laid out as `pushJsonbValue` lays it
//! out. json and jsonb are built from values as `datum_to_json` and `datum_to_jsonb` build them,
//! each value given as a [`JsonValue`]: what `json_categorize_type` makes of its type, and its
//! contents.

use super::{
    check_string, escape_json, parse, put_scalar, strtoint, syntax, written, Actions, Doc, Lexer,
    Tok, Token,
};
use crate::numeric::Numeric;
use crate::times;
use crate::{Error, Result};
use std::collections::HashSet;

/// A value given to the functions that build json and jsonb, as `json_categorize_type` sorts it by
/// its type (a domain as its base type) and `datum_to_json` and `datum_to_jsonb` write it.
#[derive(Clone, Debug, PartialEq)]
pub enum JsonValue {
    /// NULL, of any type.
    Null,
    /// A boolean.
    Bool(bool),
    /// A smallint, integer, bigint, real, double precision or numeric, as its type's output
    /// function writes it: `NaN`, `Infinity` and `-Infinity` among them.
    Number(String),
    /// A date: its days since 1970-01-01, its infinities at the ends of `i32`.
    Date(i32),
    /// A timestamp: its microseconds since 1970-01-01, its infinities at the ends of `i64`.
    Timestamp(i64),
    /// A timestamp with time zone: its microseconds since 1970-01-01 UTC.
    TimestampTz(i64),
    /// A json: its text as written.
    Json(String),
    /// A jsonb: its normal text.
    Jsonb(String),
    /// An array: the length of each of its dimensions, outermost first (none where it is empty),
    /// and its elements in the order it stores them.
    Array {
        dims: Vec<usize>,
        elements: Vec<JsonValue>,
    },
    /// A row: each field's name and value, in the order of its type, its dropped fields left out.
    Composite(Vec<(String, JsonValue)>),
    /// A value of any other type, as its type's output function writes it.
    Other(String),
}

impl JsonValue {
    fn is_null(&self) -> bool {
        matches!(self, JsonValue::Null)
    }

    /// Whether a key may not be of its kind: an array, a row, a json or a jsonb.
    fn is_structured(&self) -> bool {
        matches!(
            self,
            JsonValue::Array { .. }
                | JsonValue::Composite(_)
                | JsonValue::Json(_)
                | JsonValue::Jsonb(_)
        )
    }
}

fn key_not_scalar() -> Error {
    Error::new("key value must be scalar, not array, composite, or json")
}

fn null_key() -> Error {
    Error::new("null value not allowed for object key")
}

fn odd_arguments() -> Error {
    Error::new("argument list must have even number of elements")
}

fn subscripts() -> Error {
    Error::new("wrong number of array subscripts")
}

/// A date as `JsonEncodeDateTime` writes it: as `date_out` writes it under DateStyle ISO.
fn encode_date(d: i32) -> String {
    times::date_out(d)
}

/// An instant as `JsonEncodeDateTime` writes it: as ISO 8601 writes it, a `T` between its date and
/// its time, a timestamptz on the statement zone's clock with the zone's offset as `+05:30`, its
/// seconds where it has some, the infinities as words.
fn encode_instant(t: i64, zoned: bool) -> String {
    if t == i64::MIN || t == i64::MAX {
        return times::timestamp_out(t);
    }
    let (local, east) = match zoned {
        true => times::local_of(t).unwrap_or((t, 0)),
        false => (t, 0),
    };
    let s = times::timestamp_out(local);
    let (body, era) = match s.strip_suffix(" BC") {
        Some(b) => (b, " BC"),
        None => (s.as_str(), ""),
    };
    let mut out = body.replacen(' ', "T", 1);
    if zoned {
        let abs = east.unsigned_abs();
        out.push(if east >= 0 { '+' } else { '-' });
        out.push_str(&format!("{:02}:{:02}", abs / 3600, abs % 3600 / 60));
        if abs % 60 != 0 {
            out.push_str(&format!(":{:02}", abs % 60));
        }
    }
    out.push_str(era);
    out
}

/// A value written as json, as `datum_to_json_internal` writes it: a number bare where its text
/// starts as a JSON number does, quoted otherwise; a date or an instant quoted; a json or jsonb as
/// its text; any other type its text as a JSON string. As a key, everything is quoted, and an
/// array, a row, a json or a jsonb is refused.
fn put_json(out: &mut String, v: &JsonValue, key: bool) -> Result<()> {
    if key && v.is_structured() {
        return Err(key_not_scalar());
    }
    match v {
        JsonValue::Null => out.push_str("null"),
        JsonValue::Array { dims, elements } => put_json_array(out, dims, elements, false)?,
        JsonValue::Composite(fields) => put_json_row(out, fields, false)?,
        JsonValue::Bool(b) => {
            let word = if *b { "true" } else { "false" };
            if key {
                out.push('"');
                out.push_str(word);
                out.push('"');
            } else {
                out.push_str(word);
            }
        }
        JsonValue::Number(s) => {
            let b = s.as_bytes();
            let bare = !key
                && (b.first().is_some_and(u8::is_ascii_digit)
                    || (b.first() == Some(&b'-') && b.get(1).is_some_and(u8::is_ascii_digit)));
            if bare {
                out.push_str(s);
            } else {
                out.push('"');
                out.push_str(s);
                out.push('"');
            }
        }
        JsonValue::Date(d) => {
            out.push('"');
            out.push_str(&encode_date(*d));
            out.push('"');
        }
        JsonValue::Timestamp(t) | JsonValue::TimestampTz(t) => {
            out.push('"');
            out.push_str(&encode_instant(*t, matches!(v, JsonValue::TimestampTz(_))));
            out.push('"');
        }
        JsonValue::Json(t) | JsonValue::Jsonb(t) => out.push_str(t),
        JsonValue::Other(s) => escape_json(out, s),
    }
    Ok(())
}

/// How many elements dimensions of these lengths hold: none where there are none.
fn items(dims: &[usize]) -> usize {
    if dims.is_empty() {
        0
    } else {
        dims.iter().product()
    }
}

/// An array written as `array_to_json_internal` writes it: each dimension a JSON array, `,`
/// between elements, or where `pretty`, `,` and a new line between the outermost dimension's.
fn put_json_array(
    out: &mut String,
    dims: &[usize],
    elements: &[JsonValue],
    pretty: bool,
) -> Result<()> {
    if items(dims) == 0 {
        out.push_str("[]");
        return Ok(());
    }
    let mut next = 0;
    put_json_dim(out, 0, dims, elements, &mut next, pretty)
}

/// `array_dim_to_json`.
fn put_json_dim(
    out: &mut String,
    dim: usize,
    dims: &[usize],
    elements: &[JsonValue],
    next: &mut usize,
    pretty: bool,
) -> Result<()> {
    let sep = if pretty { ",\n " } else { "," };
    out.push('[');
    for i in 0..dims[dim] {
        if i > 0 {
            out.push_str(sep);
        }
        if dim + 1 == dims.len() {
            put_json(out, &elements[*next], false)?;
            *next += 1;
        } else {
            put_json_dim(out, dim + 1, dims, elements, next, false)?;
        }
    }
    out.push(']');
    Ok(())
}

/// A row written as `composite_to_json` writes it: each field's name, `:` and its value, `,`
/// between fields, or where `pretty`, `,` and a new line.
fn put_json_row(out: &mut String, fields: &[(String, JsonValue)], pretty: bool) -> Result<()> {
    let sep = if pretty { ",\n " } else { "," };
    out.push('{');
    for (i, (name, v)) in fields.iter().enumerate() {
        if i > 0 {
            out.push_str(sep);
        }
        escape_json(out, name);
        out.push(':');
        put_json(out, v, false)?;
    }
    out.push('}');
    Ok(())
}

/// A container open in a jsonb being built: an array's elements so far, or an object's pairs in
/// the order they came, with whether it refuses a key given twice and whether it drops a pair
/// whose value is a JSON null.
#[derive(Clone, Debug)]
enum Container {
    Array {
        count: usize,
        toks: Vec<Tok>,
    },
    Object {
        pairs: Vec<(String, Vec<Tok>)>,
        unique: bool,
        skip_nulls: bool,
    },
}

/// A jsonb being built, as a `JsonbInState` builds one through `pushJsonbValue`: the containers
/// still open, and the whole once the outermost closes.
#[derive(Clone, Debug, Default)]
struct Build {
    open: Vec<Container>,
    result: Option<Vec<Tok>>,
}

impl Build {
    fn begin_array(&mut self) {
        self.open.push(Container::Array {
            count: 0,
            toks: Vec::new(),
        });
    }

    fn begin_object(&mut self, unique: bool, skip_nulls: bool) {
        self.open.push(Container::Object {
            pairs: Vec::new(),
            unique,
            skip_nulls,
        });
    }

    /// A key of the object open innermost.
    fn key(&mut self, k: String) {
        if let Some(Container::Object { pairs, .. }) = self.open.last_mut() {
            pairs.push((k, Vec::new()));
        }
    }

    /// A whole value, its tokens: an element of the array open innermost, the value of the
    /// object's last key, or the whole.
    fn value(&mut self, toks: Vec<Tok>) {
        match self.open.last_mut() {
            Some(Container::Array { count, toks: t }) => {
                *count += 1;
                t.extend(toks);
            }
            Some(Container::Object { pairs, .. }) => {
                pairs.last_mut().expect("a value follows its key").1 = toks;
            }
            None => self.result = Some(toks),
        }
    }

    /// A scalar: a value inside a container, or the whole as a raw scalar.
    fn scalar(&mut self, t: Tok) {
        if self.open.is_empty() {
            self.result = Some(vec![
                Tok::Array {
                    count: 1,
                    raw: true,
                },
                t,
                Tok::EndArray,
            ]);
        } else {
            self.value(vec![t]);
        }
    }

    /// A jsonb read whole: its scalar where it is one inside a container, else all of it.
    fn doc(&mut self, d: Doc) {
        if d.is_raw() && !self.open.is_empty() {
            let t = d.toks.into_iter().nth(1).expect("a raw scalar holds one");
            self.value(vec![t]);
        } else {
            self.value(d.toks);
        }
    }

    /// Closes the container open innermost, an object's pairs put in jsonb's order.
    fn end(&mut self) -> Result<()> {
        let toks = match self.open.pop().expect("a container closes what it opened") {
            Container::Array { count, toks } => {
                let mut out = Vec::with_capacity(toks.len() + 2);
                out.push(Tok::Array { count, raw: false });
                out.extend(toks);
                out.push(Tok::EndArray);
                out
            }
            Container::Object {
                pairs,
                unique,
                skip_nulls,
            } => close_object(pairs, unique, skip_nulls)?,
        };
        self.value(toks);
        Ok(())
    }

    /// The whole, written as `jsonb_out` writes it.
    fn text(&self) -> String {
        written(self.result.as_deref().expect("a jsonb built is closed"))
    }
}

/// An object of these pairs, as `uniqueifyJsonbObject` leaves it: its keys by their length, then
/// their bytes, of a key given twice its last pair, a pair whose value is a JSON null dropped where
/// `skip_nulls`, and a key given twice refused where `unique`. A pair after a dropped one is kept
/// even where its key is the dropped one's.
fn close_object(
    mut pairs: Vec<(String, Vec<Tok>)>,
    unique: bool,
    skip_nulls: bool,
) -> Result<Vec<Tok>> {
    let mut order: Vec<usize> = (0..pairs.len()).collect();
    order.sort_by(|&x, &y| {
        let (a, b) = (&pairs[x].0, &pairs[y].0);
        a.len()
            .cmp(&b.len())
            .then_with(|| a.as_bytes().cmp(b.as_bytes()))
            .then(y.cmp(&x))
    });
    let twice = order.windows(2).any(|w| pairs[w[0]].0 == pairs[w[1]].0);
    if twice && unique {
        return Err(Error::new("duplicate JSON object key value"));
    }
    let mut kept: Vec<usize> = Vec::with_capacity(order.len());
    for i in order {
        if twice && kept.last().is_some_and(|&k| pairs[k].0 == pairs[i].0) {
            continue;
        }
        if skip_nulls && matches!(pairs[i].1.as_slice(), [Tok::Null]) {
            continue;
        }
        kept.push(i);
    }
    let mut toks = vec![Tok::Object { count: kept.len() }];
    for i in kept {
        let (k, v) = std::mem::take(&mut pairs[i]);
        toks.push(Tok::Key(k));
        toks.extend(v);
    }
    toks.push(Tok::EndObject);
    Ok(toks)
}

/// A value added to a jsonb being built, as `datum_to_jsonb_internal` adds it: a number as
/// `numeric_in` reads its text, `NaN` and the infinities as strings; a date or an instant as the
/// string json writes; a json read as `jsonb_in` reads it; a jsonb as it is; any other type its
/// text as a string. As a key, everything is its text as a string, and an array, a row, a json or
/// a jsonb is refused.
fn push_jsonb(b: &mut Build, v: &JsonValue, key: bool) -> Result<()> {
    if key && v.is_structured() {
        return Err(key_not_scalar());
    }
    let t = match v {
        JsonValue::Null => Tok::Null,
        JsonValue::Array { dims, elements } => {
            if items(dims) == 0 {
                b.begin_array();
                b.end()?;
            } else {
                let mut next = 0;
                push_jsonb_dim(b, 0, dims, elements, &mut next)?;
            }
            return Ok(());
        }
        JsonValue::Composite(fields) => {
            b.begin_object(false, false);
            for (name, v) in fields {
                b.key(name.clone());
                push_jsonb(b, v, false)?;
            }
            return b.end();
        }
        JsonValue::Json(t) | JsonValue::Jsonb(t) => {
            b.doc(Doc::read(t)?);
            return Ok(());
        }
        JsonValue::Bool(x) if key => Tok::Str(if *x { "true" } else { "false" }.into()),
        JsonValue::Bool(x) => Tok::Bool(*x),
        JsonValue::Number(s) if key => Tok::Str(s.clone()),
        JsonValue::Number(s) => {
            let n = Numeric::parse(s)?;
            if n.is_nan() || n.is_infinite() {
                Tok::Str(s.clone())
            } else {
                Tok::Num(n)
            }
        }
        JsonValue::Date(d) => Tok::Str(encode_date(*d)),
        JsonValue::Timestamp(t) => Tok::Str(encode_instant(*t, false)),
        JsonValue::TimestampTz(t) => Tok::Str(encode_instant(*t, true)),
        JsonValue::Other(s) => {
            check_string(s)?;
            Tok::Str(s.clone())
        }
    };
    match t {
        Tok::Str(s) if key => b.key(s),
        t => b.scalar(t),
    }
    Ok(())
}

/// `array_dim_to_jsonb`.
fn push_jsonb_dim(
    b: &mut Build,
    dim: usize,
    dims: &[usize],
    elements: &[JsonValue],
    next: &mut usize,
) -> Result<()> {
    b.begin_array();
    for _ in 0..dims[dim] {
        if dim + 1 == dims.len() {
            push_jsonb(b, &elements[*next], false)?;
            *next += 1;
        } else {
            push_jsonb_dim(b, dim + 1, dims, elements, next)?;
        }
    }
    b.end()
}

/// `to_json(anyelement)`, as `datum_to_json` writes the value.
pub fn to_json(v: &JsonValue) -> Result<String> {
    let mut out = String::new();
    put_json(&mut out, v, false)?;
    Ok(out)
}

/// `to_jsonb(anyelement)`, as `datum_to_jsonb` builds the value.
pub fn to_jsonb(v: &JsonValue) -> Result<String> {
    let mut b = Build::default();
    push_jsonb(&mut b, v, false)?;
    Ok(b.text())
}

/// `array_to_json(anyarray [, pretty])`: the array as `to_json` writes it, or where `pretty`, a new
/// line after each `,` between the outermost dimension's elements. Any other value as `to_json`
/// writes it.
pub fn array_to_json(v: &JsonValue, pretty: bool) -> Result<String> {
    match v {
        JsonValue::Array { dims, elements } => {
            let mut out = String::new();
            put_json_array(&mut out, dims, elements, pretty)?;
            Ok(out)
        }
        _ => to_json(v),
    }
}

/// `row_to_json(record [, pretty])`: the row as `to_json` writes it, or where `pretty`, a new line
/// after each `,` between its fields. Any other value as `to_json` writes it.
pub fn row_to_json(v: &JsonValue, pretty: bool) -> Result<String> {
    match v {
        JsonValue::Composite(fields) => {
            let mut out = String::new();
            put_json_row(&mut out, fields, pretty)?;
            Ok(out)
        }
        _ => to_json(v),
    }
}

/// `json_build_array(VARIADIC "any")`, and with no arguments: each argument as `to_json` writes it,
/// `, ` between them.
pub fn json_build_array(args: &[JsonValue]) -> Result<String> {
    let mut out = String::from("[");
    for (i, v) in args.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        put_json(&mut out, v, false)?;
    }
    out.push(']');
    Ok(out)
}

/// `json_build_object(VARIADIC "any")`, and with no arguments: keys and values in turn, each key
/// written as a key (a NULL refused) and `" : "` before its value, `, ` between pairs; a key given
/// twice kept twice. An odd count of arguments refused.
pub fn json_build_object(args: &[JsonValue]) -> Result<String> {
    if !args.len().is_multiple_of(2) {
        return Err(odd_arguments());
    }
    let mut out = String::from("{");
    for (i, pair) in args.chunks(2).enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        if pair[0].is_null() {
            return Err(null_key());
        }
        put_json(&mut out, &pair[0], true)?;
        out.push_str(" : ");
        put_json(&mut out, &pair[1], false)?;
    }
    out.push('}');
    Ok(out)
}

/// `jsonb_build_array(VARIADIC "any")`, and with no arguments: each argument as `to_jsonb` builds
/// it, in an array.
pub fn jsonb_build_array(args: &[JsonValue]) -> Result<String> {
    let mut b = Build::default();
    b.begin_array();
    for v in args {
        push_jsonb(&mut b, v, false)?;
    }
    b.end()?;
    Ok(b.text())
}

/// `jsonb_build_object(VARIADIC "any")`, and with no arguments: keys and values in turn in an
/// object, each key its text, a NULL key refused by its argument's place; of a key given twice its
/// last value. An odd count of arguments refused.
pub fn jsonb_build_object(args: &[JsonValue]) -> Result<String> {
    if !args.len().is_multiple_of(2) {
        return Err(odd_arguments());
    }
    let mut b = Build::default();
    b.begin_object(false, false);
    for (i, pair) in args.chunks(2).enumerate() {
        if pair[0].is_null() {
            return Err(Error::new(format!(
                "argument {}: key must not be null",
                2 * i + 1
            )));
        }
        push_jsonb(&mut b, &pair[0], true)?;
        push_jsonb(&mut b, &pair[1], false)?;
    }
    b.end()?;
    Ok(b.text())
}

/// `json_agg` and `json_agg_strict`, row by row: each value as `to_json` writes it, `, ` between
/// them and a new line before an array or a row after the first; the strict form passes over a
/// NULL. No rows give NULL; rows all passed over give `[]`.
#[derive(Clone, Debug)]
pub struct JsonAgg {
    strict: bool,
    text: Option<String>,
}

impl JsonAgg {
    pub fn new(strict: bool) -> JsonAgg {
        JsonAgg { strict, text: None }
    }

    /// Adds a row's value, as `json_agg_transfn` does.
    pub fn add(&mut self, v: &JsonValue) -> Result<()> {
        let out = self.text.get_or_insert_with(|| "[".to_string());
        if self.strict && v.is_null() {
            return Ok(());
        }
        if out.len() > 1 {
            out.push_str(", ");
            if matches!(v, JsonValue::Array { .. } | JsonValue::Composite(_)) {
                out.push_str("\n ");
            }
        }
        put_json(out, v, false)
    }

    /// The aggregate's value, as `json_agg_finalfn` gives it.
    pub fn finish(&self) -> Option<String> {
        self.text.as_ref().map(|t| format!("{t}]"))
    }
}

/// `json_object_agg` and its `_strict`, `_unique` and `_unique_strict` forms, row by row: each key
/// written as a key (a NULL refused), `" : "` and its value, `{ ` before the first, `, ` between
/// them and ` }` after the last. The strict forms pass over a NULL value; the unique forms refuse a
/// key written a second time, a passed-over pair's among them. No rows give NULL.
#[derive(Clone, Debug)]
pub struct JsonObjectAgg {
    strict: bool,
    unique: bool,
    text: Option<String>,
    keys: HashSet<String>,
}

impl JsonObjectAgg {
    pub fn new(strict: bool, unique: bool) -> JsonObjectAgg {
        JsonObjectAgg {
            strict,
            unique,
            text: None,
            keys: HashSet::new(),
        }
    }

    /// Adds a row's key and value, as `json_object_agg_transfn_worker` does.
    pub fn add(&mut self, key: &JsonValue, value: &JsonValue) -> Result<()> {
        let out = self.text.get_or_insert_with(|| "{ ".to_string());
        if key.is_null() {
            return Err(null_key());
        }
        let skip = self.strict && value.is_null();
        if skip && !self.unique {
            return Ok(());
        }
        let mut k = String::new();
        put_json(&mut k, key, true)?;
        if self.unique && !self.keys.insert(k.clone()) {
            return Err(Error::new(format!("duplicate JSON object key value: {k}")));
        }
        if skip {
            return Ok(());
        }
        if out.len() > 2 {
            out.push_str(", ");
        }
        out.push_str(&k);
        out.push_str(" : ");
        put_json(out, value, false)
    }

    /// The aggregate's value, as `json_object_agg_finalfn` gives it.
    pub fn finish(&self) -> Option<String> {
        self.text.as_ref().map(|t| format!("{t} }}"))
    }
}

/// `jsonb_agg` and `jsonb_agg_strict`, row by row: each value as `to_jsonb` builds it, in an
/// array; the strict form passes over a NULL. No rows give NULL; rows all passed over give `[]`.
#[derive(Clone, Debug)]
pub struct JsonbAgg {
    strict: bool,
    build: Option<Build>,
}

impl JsonbAgg {
    pub fn new(strict: bool) -> JsonbAgg {
        JsonbAgg {
            strict,
            build: None,
        }
    }

    /// Adds a row's value, as `jsonb_agg_transfn` does.
    pub fn add(&mut self, v: &JsonValue) -> Result<()> {
        let b = self.build.get_or_insert_with(|| {
            let mut b = Build::default();
            b.begin_array();
            b
        });
        if self.strict && v.is_null() {
            return Ok(());
        }
        push_jsonb(b, v, false)
    }

    /// The aggregate's value, as `jsonb_agg_finalfn` gives it.
    pub fn finish(&self) -> Result<Option<String>> {
        let Some(b) = &self.build else {
            return Ok(None);
        };
        let mut b = b.clone();
        b.end()?;
        Ok(Some(b.text()))
    }
}

/// `jsonb_object_agg` and its `_strict`, `_unique` and `_unique_strict` forms, row by row: each key
/// its text (a NULL refused) and its value, in an object; of a key given twice its last value. The
/// strict forms drop a NULL value and, as the object closes, every pair whose value is a JSON
/// null; the unique forms refuse, as the object closes, a key given twice, a dropped pair's among
/// them. No rows give NULL.
#[derive(Clone, Debug)]
pub struct JsonbObjectAgg {
    strict: bool,
    unique: bool,
    build: Option<Build>,
}

impl JsonbObjectAgg {
    pub fn new(strict: bool, unique: bool) -> JsonbObjectAgg {
        JsonbObjectAgg {
            strict,
            unique,
            build: None,
        }
    }

    /// Adds a row's key and value, as `jsonb_object_agg_transfn_worker` does.
    pub fn add(&mut self, key: &JsonValue, value: &JsonValue) -> Result<()> {
        let (strict, unique) = (self.strict, self.unique);
        let b = self.build.get_or_insert_with(|| {
            let mut b = Build::default();
            b.begin_object(unique, strict);
            b
        });
        if key.is_null() {
            return Err(Error::new("field name must not be null"));
        }
        if strict && value.is_null() && !unique {
            return Ok(());
        }
        push_jsonb(b, key, true)?;
        push_jsonb(b, value, false)
    }

    /// The aggregate's value, as `jsonb_object_agg_finalfn` gives it.
    pub fn finish(&self) -> Result<Option<String>> {
        let Some(b) = &self.build else {
            return Ok(None);
        };
        let mut b = b.clone();
        b.end()?;
        Ok(Some(b.text()))
    }
}

/// How many elements a text array of these dimensions and elements gives `json_object`, as it
/// reads them: none for no dimensions; one dimension of an even length; two, the second of 2.
fn object_pairs(elements: &[Option<String>], dims: &[usize]) -> Result<usize> {
    match dims.len() {
        0 => return Ok(0),
        1 if !dims[0].is_multiple_of(2) => {
            return Err(Error::new("array must have even number of elements"))
        }
        1 => {}
        2 if dims[1] != 2 => return Err(Error::new("array must have two columns")),
        2 => {}
        _ => return Err(subscripts()),
    }
    Ok(elements.len() / 2)
}

/// `json_object(text[])`: the array's elements taken in pairs, a key and its value, as strings, a
/// NULL value as a JSON null and a NULL key refused; `" : "` in each pair and `, ` between them.
pub fn json_object(elements: &[Option<String>], dims: &[usize]) -> Result<String> {
    let count = object_pairs(elements, dims)?;
    let pairs: Vec<(&Option<String>, &Option<String>)> = (0..count)
        .map(|i| (&elements[2 * i], &elements[2 * i + 1]))
        .collect();
    json_object_of(&pairs)
}

/// `json_object(keys text[], values text[])`: each key with the value at its place, as
/// `json_object(text[])` writes them; both arrays of one dimension, or both empty, and as long as
/// each other.
pub fn json_object_pairs(
    keys: &[Option<String>],
    key_dims: &[usize],
    values: &[Option<String>],
    value_dims: &[usize],
) -> Result<String> {
    let count = two_arrays(keys, key_dims, values, value_dims)?;
    let pairs: Vec<(&Option<String>, &Option<String>)> =
        (0..count).map(|i| (&keys[i], &values[i])).collect();
    json_object_of(&pairs)
}

/// How many pairs two text arrays give `json_object(text[], text[])`.
fn two_arrays(
    keys: &[Option<String>],
    key_dims: &[usize],
    values: &[Option<String>],
    value_dims: &[usize],
) -> Result<usize> {
    if key_dims.len() > 1 || key_dims.len() != value_dims.len() {
        return Err(subscripts());
    }
    if key_dims.is_empty() {
        return Ok(0);
    }
    if keys.len() != values.len() {
        return Err(Error::new("mismatched array dimensions"));
    }
    Ok(keys.len())
}

fn json_object_of(pairs: &[(&Option<String>, &Option<String>)]) -> Result<String> {
    let mut out = String::from("{");
    for (i, (k, v)) in pairs.iter().enumerate() {
        let Some(k) = k else {
            return Err(null_key());
        };
        if i > 0 {
            out.push_str(", ");
        }
        escape_json(&mut out, k);
        out.push_str(" : ");
        match v {
            Some(v) => escape_json(&mut out, v),
            None => out.push_str("null"),
        }
    }
    out.push('}');
    Ok(out)
}

/// `jsonb_object(text[])`: the array's elements taken in pairs as `json_object` takes them, in an
/// object; of a key given twice its last value.
pub fn jsonb_object(elements: &[Option<String>], dims: &[usize]) -> Result<String> {
    let count = object_pairs(elements, dims)?;
    let pairs: Vec<(&Option<String>, &Option<String>)> = (0..count)
        .map(|i| (&elements[2 * i], &elements[2 * i + 1]))
        .collect();
    jsonb_object_of(&pairs)
}

/// `jsonb_object(keys text[], values text[])`.
pub fn jsonb_object_pairs(
    keys: &[Option<String>],
    key_dims: &[usize],
    values: &[Option<String>],
    value_dims: &[usize],
) -> Result<String> {
    let count = two_arrays(keys, key_dims, values, value_dims)?;
    let pairs: Vec<(&Option<String>, &Option<String>)> =
        (0..count).map(|i| (&keys[i], &values[i])).collect();
    jsonb_object_of(&pairs)
}

fn jsonb_object_of(pairs: &[(&Option<String>, &Option<String>)]) -> Result<String> {
    let mut b = Build::default();
    b.begin_object(false, false);
    for (k, v) in pairs {
        let Some(k) = k else {
            return Err(null_key());
        };
        b.key(k.clone());
        b.scalar(match v {
            Some(v) => Tok::Str(v.clone()),
            None => Tok::Null,
        });
    }
    b.end()?;
    Ok(b.text())
}

/// `json_typeof`: the kind of the value at the top, by its first token.
pub fn json_typeof(doc: &str) -> Result<&'static str> {
    let mut lex = Lexer::new(doc, false);
    lex.next()?;
    Ok(match lex.token {
        Token::ObjectStart => "object",
        Token::ArrayStart => "array",
        Token::String => "string",
        Token::Number => "number",
        Token::True | Token::False => "boolean",
        Token::Null => "null",
        _ => return Err(syntax()),
    })
}

/// A scalar's kind as `JsonbTypeName` names it.
fn type_name(t: &Tok) -> &'static str {
    match t {
        Tok::Null => "null",
        Tok::Str(_) => "string",
        Tok::Num(_) => "number",
        Tok::Bool(_) => "boolean",
        Tok::Array { .. } => "array",
        _ => "object",
    }
}

/// `jsonb_typeof`: the kind of the value at the top.
pub fn jsonb_typeof(doc: &str) -> Result<&'static str> {
    let d = Doc::read(doc)?;
    Ok(if d.is_raw() {
        type_name(&d.toks[1])
    } else {
        type_name(&d.toks[0])
    })
}

/// `json_array_length`'s callbacks: the elements of the array at the top counted, anything else
/// there refused.
#[derive(Default)]
struct Length {
    count: i32,
}

impl Actions for Length {
    fn object_start(&mut self, lex: &Lexer) -> Result<()> {
        if lex.level == 0 {
            return Err(Error::new("cannot get array length of a non-array"));
        }
        Ok(())
    }

    fn scalar(&mut self, lex: &Lexer, _: &str, _: Token) -> Result<()> {
        if lex.level == 0 {
            return Err(Error::new("cannot get array length of a scalar"));
        }
        Ok(())
    }

    fn element_start(&mut self, lex: &Lexer, _: bool) -> Result<()> {
        if lex.level == 1 {
            self.count += 1;
        }
        Ok(())
    }
}

/// `json_array_length`: how many elements the array at the top holds; an object or a scalar there
/// refused. Its strings are not de-escaped.
pub fn json_array_length(doc: &str) -> Result<i32> {
    let mut a = Length::default();
    parse(&mut Lexer::new(doc, false), &mut a)?;
    Ok(a.count)
}

/// `jsonb_array_length`: how many elements the array at the top holds; a scalar or an object there
/// refused.
pub fn jsonb_array_length(doc: &str) -> Result<i32> {
    let d = Doc::read(doc)?;
    if d.is_raw() {
        return Err(Error::new("cannot get array length of a scalar"));
    }
    if d.is_object() {
        return Err(Error::new("cannot get array length of a non-array"));
    }
    Ok(d.count(0) as i32)
}

/// `json_object_keys`'s callbacks: the keys of the object at the top, anything else there refused.
#[derive(Default)]
struct Keys {
    keys: Vec<String>,
}

impl Actions for Keys {
    fn field_start(&mut self, lex: &Lexer, name: &str, _: bool) -> Result<()> {
        if lex.level == 1 {
            self.keys.push(name.to_string());
        }
        Ok(())
    }

    fn array_start(&mut self, lex: &Lexer) -> Result<()> {
        if lex.level == 0 {
            return Err(Error::new("cannot call json_object_keys on an array"));
        }
        Ok(())
    }

    fn scalar(&mut self, lex: &Lexer, _: &str, _: Token) -> Result<()> {
        if lex.level == 0 {
            return Err(Error::new("cannot call json_object_keys on a scalar"));
        }
        Ok(())
    }
}

/// `json_object_keys`: the keys of the object at the top in the order written, a key written twice
/// given twice; an array or a scalar there refused.
pub fn json_object_keys(doc: &str) -> Result<Vec<String>> {
    let mut k = Keys::default();
    parse(&mut Lexer::new(doc, true), &mut k)?;
    Ok(k.keys)
}

/// `jsonb_object_keys`: the keys of the object at the top in the order jsonb stores them; a scalar
/// or an array there refused.
pub fn jsonb_object_keys(doc: &str) -> Result<Vec<String>> {
    let d = Doc::read(doc)?;
    if d.is_raw() {
        return Err(Error::new("cannot call jsonb_object_keys on a scalar"));
    }
    if !d.is_object() {
        return Err(Error::new("cannot call jsonb_object_keys on an array"));
    }
    Ok(d.children(0)
        .into_iter()
        .map(|k| match &d.toks[k] {
            Tok::Key(s) => s.clone(),
            _ => unreachable!("an object's child is a key"),
        })
        .collect())
}

/// The callbacks of `json_each` and `json_array_elements`: each field of the object at the top, or
/// each element of the array there, its value sliced from the text as written, or where text is
/// asked for a string de-escaped and a JSON null none.
struct Rows {
    fields: bool,
    as_text: bool,
    name: &'static str,
    rows: Vec<(String, Option<String>)>,
    start: usize,
    next_scalar: bool,
    normalized: String,
}

impl Rows {
    fn new(fields: bool, as_text: bool, name: &'static str) -> Rows {
        Rows {
            fields,
            as_text,
            name,
            rows: Vec::new(),
            start: 0,
            next_scalar: false,
            normalized: String::new(),
        }
    }

    fn begin(&mut self, lex: &Lexer) {
        if lex.level == 1 {
            if self.as_text && lex.token == Token::String {
                self.next_scalar = true;
            } else {
                self.start = lex.start.unwrap_or(lex.end);
            }
        }
    }

    fn finish(&mut self, lex: &Lexer, name: &str, isnull: bool) {
        if lex.level != 1 {
            return;
        }
        let value = if isnull && self.as_text {
            None
        } else if self.next_scalar {
            self.next_scalar = false;
            Some(std::mem::take(&mut self.normalized))
        } else {
            Some(lex.text[self.start..lex.prev_end].to_string())
        };
        self.rows.push((name.to_string(), value));
    }
}

impl Actions for Rows {
    fn object_start(&mut self, lex: &Lexer) -> Result<()> {
        if !self.fields && lex.level == 0 {
            return Err(Error::new(format!(
                "cannot call {} on a non-array",
                self.name
            )));
        }
        Ok(())
    }

    fn array_start(&mut self, lex: &Lexer) -> Result<()> {
        if self.fields && lex.level == 0 {
            return Err(Error::new("cannot deconstruct an array as an object"));
        }
        Ok(())
    }

    fn field_start(&mut self, lex: &Lexer, _: &str, _: bool) -> Result<()> {
        if self.fields {
            self.begin(lex);
        }
        Ok(())
    }

    fn field_end(&mut self, lex: &Lexer, name: &str, isnull: bool) -> Result<()> {
        if self.fields {
            self.finish(lex, name, isnull);
        }
        Ok(())
    }

    fn element_start(&mut self, lex: &Lexer, _: bool) -> Result<()> {
        if !self.fields {
            self.begin(lex);
        }
        Ok(())
    }

    fn element_end(&mut self, lex: &Lexer, isnull: bool) -> Result<()> {
        if !self.fields {
            self.finish(lex, "", isnull);
        }
        Ok(())
    }

    fn scalar(&mut self, lex: &Lexer, value: &str, _: Token) -> Result<()> {
        if lex.level == 0 {
            return Err(Error::new(if self.fields {
                "cannot deconstruct a scalar".to_string()
            } else {
                format!("cannot call {} on a scalar", self.name)
            }));
        }
        if self.next_scalar {
            self.normalized = value.to_string();
        }
        Ok(())
    }
}

fn json_rows(
    doc: &str,
    fields: bool,
    as_text: bool,
    name: &'static str,
) -> Result<Vec<(String, Option<String>)>> {
    let mut r = Rows::new(fields, as_text, name);
    parse(&mut Lexer::new(doc, fields || as_text), &mut r)?;
    Ok(r.rows)
}

/// `json_each`: each field of the object at the top, its key and its value as written, a key
/// written twice given twice; an array or a scalar there refused.
pub fn json_each(doc: &str) -> Result<Vec<(String, String)>> {
    Ok(json_rows(doc, true, false, "json_each")?
        .into_iter()
        .map(|(k, v)| (k, v.unwrap_or_default()))
        .collect())
}

/// `json_each_text`: as `json_each`, a string value de-escaped and a JSON null none.
pub fn json_each_text(doc: &str) -> Result<Vec<(String, Option<String>)>> {
    json_rows(doc, true, true, "json_each_text")
}

/// `json_array_elements`: each element of the array at the top as written; an object or a scalar
/// there refused. Its strings are not de-escaped.
pub fn json_array_elements(doc: &str) -> Result<Vec<String>> {
    Ok(json_rows(doc, false, false, "json_array_elements")?
        .into_iter()
        .map(|(_, v)| v.unwrap_or_default())
        .collect())
}

/// `json_array_elements_text`: as `json_array_elements`, a string de-escaped and a JSON null none.
pub fn json_array_elements_text(doc: &str) -> Result<Vec<Option<String>>> {
    Ok(json_rows(doc, false, true, "json_array_elements_text")?
        .into_iter()
        .map(|(_, v)| v)
        .collect())
}

fn jsonb_fields(doc: &str, name: &str, as_text: bool) -> Result<Vec<(String, Option<String>)>> {
    let d = Doc::read(doc)?;
    if !d.is_object() {
        return Err(Error::new(format!("cannot call {name} on a non-object")));
    }
    Ok(d.children(0)
        .into_iter()
        .map(|k| {
            let key = match &d.toks[k] {
                Tok::Key(s) => s.clone(),
                _ => unreachable!("an object's child is a key"),
            };
            let v = if as_text {
                d.as_text(k + 1)
            } else {
                Some(d.text(k + 1))
            };
            (key, v)
        })
        .collect())
}

/// `jsonb_each`: each pair of the object at the top in the order jsonb stores them, its value as a
/// jsonb; anything else there refused.
pub fn jsonb_each(doc: &str) -> Result<Vec<(String, String)>> {
    Ok(jsonb_fields(doc, "jsonb_each", false)?
        .into_iter()
        .map(|(k, v)| (k, v.unwrap_or_default()))
        .collect())
}

/// `jsonb_each_text`: as `jsonb_each`, each value as `->>` gives it.
pub fn jsonb_each_text(doc: &str) -> Result<Vec<(String, Option<String>)>> {
    jsonb_fields(doc, "jsonb_each_text", true)
}

fn jsonb_elements(doc: &str, as_text: bool) -> Result<Vec<Option<String>>> {
    let d = Doc::read(doc)?;
    if d.is_raw() {
        return Err(Error::new("cannot extract elements from a scalar"));
    }
    if d.is_object() {
        return Err(Error::new("cannot extract elements from an object"));
    }
    Ok(d.children(0)
        .into_iter()
        .map(|e| {
            if as_text {
                d.as_text(e)
            } else {
                Some(d.text(e))
            }
        })
        .collect())
}

/// `jsonb_array_elements`: each element of the array at the top as a jsonb; a scalar or an object
/// there refused.
pub fn jsonb_array_elements(doc: &str) -> Result<Vec<String>> {
    Ok(jsonb_elements(doc, false)?
        .into_iter()
        .map(Option::unwrap_or_default)
        .collect())
}

/// `jsonb_array_elements_text`: as `jsonb_array_elements`, each element as `->>` gives it.
pub fn jsonb_array_elements_text(doc: &str) -> Result<Vec<Option<String>>> {
    jsonb_elements(doc, true)
}

/// `json_strip_nulls`'s callbacks: the text written again without whitespace, each field whose
/// value is null left out, and where `strip_in_arrays`, each null element; strings escaped again
/// as `escape_json` escapes them, numbers as written.
struct Strip {
    out: String,
    skip_next_null: bool,
    strip_in_arrays: bool,
}

impl Actions for Strip {
    fn object_start(&mut self, _: &Lexer) -> Result<()> {
        self.out.push('{');
        Ok(())
    }

    fn object_end(&mut self, _: &Lexer) -> Result<()> {
        self.out.push('}');
        Ok(())
    }

    fn array_start(&mut self, _: &Lexer) -> Result<()> {
        self.out.push('[');
        Ok(())
    }

    fn array_end(&mut self, _: &Lexer) -> Result<()> {
        self.out.push(']');
        Ok(())
    }

    fn field_start(&mut self, _: &Lexer, name: &str, isnull: bool) -> Result<()> {
        if isnull {
            self.skip_next_null = true;
            return Ok(());
        }
        if !self.out.ends_with('{') {
            self.out.push(',');
        }
        escape_json(&mut self.out, name);
        self.out.push(':');
        Ok(())
    }

    fn element_start(&mut self, _: &Lexer, isnull: bool) -> Result<()> {
        if isnull && self.strip_in_arrays {
            self.skip_next_null = true;
            return Ok(());
        }
        if !self.out.is_empty() && !self.out.ends_with('[') {
            self.out.push(',');
        }
        Ok(())
    }

    fn scalar(&mut self, _: &Lexer, value: &str, token: Token) -> Result<()> {
        if self.skip_next_null {
            self.skip_next_null = false;
            return Ok(());
        }
        if token == Token::String {
            escape_json(&mut self.out, value);
        } else {
            self.out.push_str(value);
        }
        Ok(())
    }
}

/// `json_strip_nulls(json [, strip_in_arrays])`: every field whose value is null left out at every
/// level, and where `strip_in_arrays`, every null element; the rest written again without
/// whitespace, a key written twice kept twice.
pub fn json_strip_nulls(doc: &str, strip_in_arrays: bool) -> Result<String> {
    let mut s = Strip {
        out: String::new(),
        skip_next_null: false,
        strip_in_arrays,
    };
    parse(&mut Lexer::new(doc, true), &mut s)?;
    Ok(s.out)
}

/// `jsonb_strip_nulls(jsonb [, strip_in_arrays])`: every pair whose value is a JSON null left out
/// at every level, and where `strip_in_arrays`, every null element; a scalar at the top as it is.
pub fn jsonb_strip_nulls(doc: &str, strip_in_arrays: bool) -> Result<String> {
    let d = Doc::read(doc)?;
    if d.is_raw() {
        return Ok(d.out());
    }
    let mut toks = Vec::with_capacity(d.toks.len());
    // Whether each container open is an object.
    let mut objects: Vec<bool> = Vec::new();
    let mut i = 0;
    while i < d.toks.len() {
        let t = &d.toks[i];
        match t {
            Tok::Key(_) if matches!(d.toks[i + 1], Tok::Null) => {
                i += 2;
                continue;
            }
            Tok::Null if strip_in_arrays && objects.last() == Some(&false) => {
                i += 1;
                continue;
            }
            Tok::Key(_) => {
                toks.push(t.clone());
                toks.push(d.toks[i + 1].clone());
                if let Tok::Object { .. } = d.toks[i + 1] {
                    objects.push(true);
                } else if let Tok::Array { .. } = d.toks[i + 1] {
                    objects.push(false);
                }
                i += 2;
                continue;
            }
            Tok::Array { .. } => objects.push(false),
            Tok::Object { .. } => objects.push(true),
            Tok::EndArray | Tok::EndObject => {
                objects.pop();
            }
            _ => {}
        }
        toks.push(t.clone());
        i += 1;
    }
    Ok(written(&toks))
}

/// `jsonb_pretty`: the jsonb as `JsonbToCStringIndent` writes it: each element, pair and closing
/// bracket on a line of its own, indented four spaces a level, `,` ending a line and `": "` after a
/// key, a key's container opened on the key's line; a scalar at the top bare.
pub fn jsonb_pretty(doc: &str) -> Result<String> {
    let d = Doc::read(doc)?;
    let toks = &d.toks;
    let mut out = String::new();
    let indent = |out: &mut String, on: bool, level: usize| {
        if on {
            out.push('\n');
            out.push_str(&" ".repeat(4 * level));
        }
    };
    let (mut first, mut level, mut use_indent) = (true, 0usize, false);
    let (mut raw, mut last_was_key) = (false, false);
    let mut i = 0;
    let mut redo: Option<&Tok> = None;
    loop {
        let t = match redo.take() {
            Some(t) => t,
            None => {
                let Some(t) = toks.get(i) else { break };
                i += 1;
                t
            }
        };
        let mut again = None;
        match t {
            Tok::Array { raw: r, .. } => {
                if !first {
                    out.push(',');
                }
                if *r {
                    raw = true;
                } else {
                    indent(&mut out, use_indent && !last_was_key, level);
                    out.push('[');
                }
                first = true;
                level += 1;
            }
            Tok::Object { .. } => {
                if !first {
                    out.push(',');
                }
                indent(&mut out, use_indent && !last_was_key, level);
                out.push('{');
                first = true;
                level += 1;
            }
            Tok::Key(k) => {
                if !first {
                    out.push(',');
                }
                first = true;
                indent(&mut out, use_indent, level);
                escape_json(&mut out, k);
                out.push_str(": ");
                let v = &toks[i];
                i += 1;
                if v.is_scalar() {
                    first = false;
                    put_scalar(&mut out, v);
                } else {
                    again = Some(v);
                }
            }
            Tok::EndArray => {
                level -= 1;
                if !raw {
                    indent(&mut out, use_indent, level);
                    out.push(']');
                }
                first = false;
            }
            Tok::EndObject => {
                level -= 1;
                indent(&mut out, use_indent, level);
                out.push('}');
                first = false;
            }
            t => {
                if !first {
                    out.push(',');
                }
                first = false;
                if !raw {
                    indent(&mut out, use_indent, level);
                }
                put_scalar(&mut out, t);
            }
        }
        use_indent = true;
        last_was_key = again.is_some();
        redo = again;
    }
    Ok(out)
}

/// What `setPath` does at the end of the path: `jsonb_set`'s create and replace, `jsonb_insert`'s
/// before and after.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SetOp {
    Create,
    Replace,
    Before,
    After,
}

impl SetOp {
    fn creates(self) -> bool {
        self != SetOp::Replace
    }

    fn inserts(self) -> bool {
        matches!(self, SetOp::Before | SetOp::After)
    }
}

/// `setPath` for `jsonb_set` and `jsonb_insert`: the path followed down from the top, a key in an
/// object, an integer in an array (a negative one counting from its end), and at its end the new
/// value set or put in. A NULL in the path is refused where the walk reaches it, and an element
/// that is not an integer where it meets an array; a path that leaves the document, or meets a
/// scalar before its end, changes nothing.
fn set_path(d: &Doc, path: &[Option<String>], new: &Doc, op: SetOp) -> Result<String> {
    let newval: &[Tok] = if new.is_raw() {
        &new.toks[1..2]
    } else {
        &new.toks
    };
    let last = path.len() - 1;
    let mut at = 0;
    for (level, p) in path.iter().enumerate() {
        let Some(p) = p else {
            return Err(Error::new(format!(
                "path element at position {} is null",
                level + 1
            )));
        };
        // The container changed and its tokens, where the walk ends here.
        let changed: Vec<Tok> = match d.toks[at] {
            Tok::Object { .. } => {
                let found = d.field(at, p);
                if level < last {
                    match found {
                        Some(v) => {
                            at = v;
                            continue;
                        }
                        None => return Ok(d.out()),
                    }
                }
                match found {
                    Some(_) if op.inserts() => {
                        return Err(Error::new("cannot replace existing key"));
                    }
                    Some(v) => {
                        let mut toks = d.toks[at..v].to_vec();
                        toks.extend_from_slice(newval);
                        toks.extend_from_slice(&d.toks[d.end[v]..d.end[at]]);
                        toks
                    }
                    None if op.creates() => {
                        let mut pairs: Vec<(String, Vec<Tok>)> = d
                            .children(at)
                            .into_iter()
                            .map(|k| match &d.toks[k] {
                                Tok::Key(s) => (s.clone(), d.value(k + 1).to_vec()),
                                _ => unreachable!("an object's child is a key"),
                            })
                            .collect();
                        pairs.push((p.clone(), newval.to_vec()));
                        close_object(pairs, false, false)?
                    }
                    None => return Ok(d.out()),
                }
            }
            Tok::Array { count, .. } => {
                let idx = strtoint(p).ok_or_else(|| {
                    Error::new(format!(
                        "path element at position {} is not an integer: \"{p}\"",
                        level + 1
                    ))
                })?;
                let n = count as i64;
                let mut idx = idx as i64;
                if idx < 0 {
                    idx = if -idx > n { i32::MIN as i64 } else { n + idx };
                }
                if idx > n {
                    idx = n;
                }
                let elems = d.children(at);
                if level < last {
                    match usize::try_from(idx).ok().and_then(|i| elems.get(i)) {
                        Some(&e) => {
                            at = e;
                            continue;
                        }
                        None => return Ok(d.out()),
                    }
                }
                let mut toks = Vec::with_capacity(d.end[at] - at + newval.len());
                let mut count = 0;
                let mut done = false;
                if (idx == i32::MIN as i64 || n == 0) && op.creates() {
                    toks.extend_from_slice(newval);
                    count += 1;
                    done = true;
                }
                for (i, &e) in elems.iter().enumerate() {
                    let old = d.value(e);
                    if i as i64 == idx {
                        done = true;
                        if matches!(op, SetOp::Before | SetOp::Create) {
                            toks.extend_from_slice(newval);
                            count += 1;
                        }
                        if op.inserts() {
                            toks.extend_from_slice(old);
                            count += 1;
                        }
                        if matches!(op, SetOp::After | SetOp::Replace) {
                            toks.extend_from_slice(newval);
                            count += 1;
                        }
                    } else {
                        toks.extend_from_slice(old);
                        count += 1;
                    }
                }
                if op.creates() && !done {
                    toks.extend_from_slice(newval);
                    count += 1;
                }
                toks.insert(0, Tok::Array { count, raw: false });
                toks.push(Tok::EndArray);
                toks
            }
            _ => return Ok(d.out()),
        };
        let mut toks = d.toks[..at].to_vec();
        toks.extend(changed);
        toks.extend_from_slice(&d.toks[d.end[at]..]);
        return Ok(written(&toks));
    }
    Ok(d.out())
}

/// `jsonb_set(target, path, new [, create_if_missing])`: the value at the end of the path replaced
/// by `new`, or where it is missing and `create_if_missing`, a key added to the object there, or
/// `new` put first or last in the array there for an index past its start or its end. A scalar at
/// the top refused; an empty path, or an empty target not to be added to, gives the target.
pub fn jsonb_set(
    target: &str,
    path: &[Option<String>],
    new: &str,
    create_if_missing: bool,
) -> Result<String> {
    let d = Doc::read(target)?;
    let n = Doc::read(new)?;
    if d.is_raw() {
        return Err(Error::new("cannot set path in scalar"));
    }
    if (d.count(0) == 0 && !create_if_missing) || path.is_empty() {
        return Ok(d.out());
    }
    let op = if create_if_missing {
        SetOp::Create
    } else {
        SetOp::Replace
    };
    set_path(&d, path, &n, op)
}

fn null_treatment() -> Error {
    Error::new(
        "null_value_treatment must be \"delete_key\", \"return_target\", \"use_json_null\", or \"raise_exception\"",
    )
}

/// `jsonb_set_lax(target, path, new [, create_if_missing [, null_value_treatment]])`, its
/// arguments as given, NULLs among them: NULL where the target, the path or `create_if_missing`
/// is; `jsonb_set` where `new` is not NULL; else as `null_value_treatment` says (`use_json_null`
/// by default): `raise_exception` refuses, `use_json_null` sets a JSON null, `delete_key` takes
/// the path out as `#-` does, `return_target` gives the target. Any other treatment, or a NULL
/// one, refused.
pub fn jsonb_set_lax(
    target: Option<&str>,
    path: Option<&[Option<String>]>,
    new: Option<&str>,
    create_if_missing: Option<bool>,
    null_value_treatment: Option<&str>,
) -> Result<Option<String>> {
    let (Some(target), Some(path), Some(create)) = (target, path, create_if_missing) else {
        return Ok(None);
    };
    let Some(treatment) = null_value_treatment else {
        return Err(null_treatment());
    };
    if let Some(new) = new {
        return jsonb_set(target, path, new, create).map(Some);
    }
    match treatment {
        "raise_exception" => Err(Error::new("JSON value must not be null")),
        "use_json_null" => jsonb_set(target, path, "null", create).map(Some),
        "delete_key" => super::jsonb_delete_path(target, path).map(Some),
        "return_target" => Ok(Some(target.to_string())),
        _ => Err(null_treatment()),
    }
}

/// `jsonb_insert(target, path, new [, insert_after])`: `new` put into the array at the end of the
/// path before the element there, or where `insert_after`, after it (first or last for an index
/// past its start or its end), or added to the object there under the path's last key, a key it
/// already has refused. A scalar at the top refused; an empty path gives the target.
pub fn jsonb_insert(
    target: &str,
    path: &[Option<String>],
    new: &str,
    insert_after: bool,
) -> Result<String> {
    let d = Doc::read(target)?;
    let n = Doc::read(new)?;
    if d.is_raw() {
        return Err(Error::new("cannot set path in scalar"));
    }
    if path.is_empty() {
        return Ok(d.out());
    }
    let op = if insert_after {
        SetOp::After
    } else {
        SetOp::Before
    };
    set_path(&d, path, &n, op)
}

#[cfg(test)]
mod tests {
    use super::*;

    type Answer<'a> = std::result::Result<&'a str, &'a str>;

    fn got(r: Result<String>) -> std::result::Result<String, String> {
        r.map_err(|e| e.to_string())
    }

    fn want(a: Answer) -> std::result::Result<String, String> {
        a.map(str::to_string).map_err(str::to_string)
    }

    fn num(s: &str) -> JsonValue {
        JsonValue::Number(s.into())
    }

    fn txt(s: &str) -> JsonValue {
        JsonValue::Other(s.into())
    }

    fn row(fields: &[(&str, JsonValue)]) -> JsonValue {
        JsonValue::Composite(
            fields
                .iter()
                .map(|(n, v)| (n.to_string(), v.clone()))
                .collect(),
        )
    }

    fn ints(dims: &[usize], els: &[i64]) -> JsonValue {
        JsonValue::Array {
            dims: dims.to_vec(),
            elements: els.iter().map(|n| num(&n.to_string())).collect(),
        }
    }

    fn day(y: i64, m: i64, d: i64) -> i64 {
        times::date2j(y, m, d) - times::UNIX_EPOCH_JDATE
    }

    fn instant(y: i64, m: i64, d: i64, secs: i64, micros: i64) -> i64 {
        day(y, m, d) * times::MICROS_PER_DAY + secs * 1_000_000 + micros
    }

    fn path(p: &[Option<&str>]) -> Vec<Option<String>> {
        p.iter().map(|e| e.map(str::to_string)).collect()
    }

    fn texts(p: &[Option<&str>]) -> Vec<Option<String>> {
        path(p)
    }

    #[test]
    fn json_writes_each_kind_of_value_as_datum_to_json_does() {
        let cases: &[(JsonValue, Answer)] = &[
            (num("-0"), Ok("-0")),
            (num("NaN"), Ok("\"NaN\"")),
            (num("-Infinity"), Ok("\"-Infinity\"")),
            (JsonValue::Date(i32::MIN), Ok("\"-infinity\"")),
            (
                JsonValue::Date(day(-43, 3, 15) as i32),
                Ok("\"0044-03-15 BC\""),
            ),
            (
                JsonValue::Timestamp(instant(2020, 1, 2, 3 * 3600 + 4 * 60 + 5, 500_000)),
                Ok("\"2020-01-02T03:04:05.5\""),
            ),
            (
                JsonValue::TimestampTz(instant(2020, 1, 2, 3 * 3600 + 4 * 60 + 5, 0)),
                Ok("\"2020-01-02T03:04:05+00:00\""),
            ),
            (
                JsonValue::TimestampTz(instant(0, 1, 1, 0, 0)),
                Ok("\"0001-01-01T00:00:00+00:00 BC\""),
            ),
            (txt("a\"b\\"), Ok("\"a\\\"b\\\\\"")),
            (txt("\u{1}\n"), Ok("\"\\u0001\\n\"")),
            (ints(&[2, 2], &[1, 2, 3, 4]), Ok("[[1,2],[3,4]]")),
        ];
        for (v, w) in cases {
            assert_eq!(got(to_json(v)), want(*w), "{v:?}");
        }
        assert_eq!(
            got(array_to_json(&ints(&[2, 2], &[1, 2, 3, 4]), true)),
            want(Ok("[[1,2],\n [3,4]]"))
        );
        assert_eq!(
            got(array_to_json(&ints(&[2, 1, 2], &[1, 2, 3, 4]), true)),
            want(Ok("[[[1,2]],\n [[3,4]]]"))
        );
        assert_eq!(got(array_to_json(&ints(&[], &[]), true)), want(Ok("[]")));
        assert_eq!(
            got(row_to_json(
                &row(&[("f1", num("1")), ("f2", txt("x"))]),
                true
            )),
            want(Ok("{\"f1\":1,\n \"f2\":\"x\"}"))
        );
    }

    #[test]
    fn jsonb_builds_each_kind_of_value_as_datum_to_jsonb_does() {
        let cases: &[(JsonValue, Answer)] = &[
            (num("-0"), Ok("0")),
            (num("-Infinity"), Ok("\"-Infinity\"")),
            (num("1.50"), Ok("1.50")),
            (JsonValue::TimestampTz(i64::MAX), Ok("\"infinity\"")),
            (
                JsonValue::Array {
                    dims: vec![2],
                    elements: vec![
                        row(&[("f1", num("1")), ("f2", txt("a"))]),
                        row(&[("f1", num("2")), ("f2", txt("b"))]),
                    ],
                },
                Ok("[{\"f1\": 1, \"f2\": \"a\"}, {\"f1\": 2, \"f2\": \"b\"}]"),
            ),
        ];
        for (v, w) in cases {
            assert_eq!(got(to_jsonb(v)), want(*w), "{v:?}");
        }
        assert_eq!(
            got(jsonb_build_array(&[
                JsonValue::Json("{\"b\":1,\"a\":2,\"b\":3}".into()),
                JsonValue::Json("\"\\u0041\"".into()),
                JsonValue::Jsonb("[1, 2]".into()),
            ])),
            want(Ok("[{\"a\": 2, \"b\": 3}, \"A\", [1, 2]]"))
        );
    }

    #[test]
    fn the_builders_answer_as_postgresql_s() {
        let args = [
            num("1.50"),
            JsonValue::Bool(true),
            JsonValue::Bool(true),
            txt("x"),
            JsonValue::Date(day(2020, 1, 1) as i32),
            JsonValue::Null,
        ];
        assert_eq!(
            got(json_build_object(&args)),
            want(Ok(
                "{\"1.50\" : true, \"true\" : \"x\", \"2020-01-01\" : null}"
            ))
        );
        assert_eq!(
            got(jsonb_build_object(&args)),
            want(Ok(
                "{\"1.50\": true, \"true\": \"x\", \"2020-01-01\": null}"
            ))
        );
        assert_eq!(
            got(jsonb_build_object(&[
                txt("b"),
                num("1"),
                txt("a"),
                num("2"),
                txt("b"),
                num("3")
            ])),
            want(Ok("{\"a\": 2, \"b\": 3}"))
        );
        let null_key = [txt("a"), num("1"), JsonValue::Null, num("2")];
        assert_eq!(
            got(json_build_object(&null_key)),
            want(Err("null value not allowed for object key"))
        );
        assert_eq!(
            got(jsonb_build_object(&null_key)),
            want(Err("argument 3: key must not be null"))
        );
        assert_eq!(
            got(json_build_object(&[txt("a")])),
            want(Err("argument list must have even number of elements"))
        );
        assert_eq!(
            got(json_build_object(&[ints(&[1], &[1]), num("2")])),
            want(Err(
                "key value must be scalar, not array, composite, or json"
            ))
        );
        assert_eq!(
            got(jsonb_build_object(&[
                num("1e+16"),
                num("1"),
                num("-0"),
                num("2")
            ])),
            want(Ok("{\"-0\": 2, \"1e+16\": 1}"))
        );
        assert_eq!(
            got(json_build_object(&[num("1e+16"), num("1")])),
            want(Ok("{\"1e+16\" : 1}"))
        );
        assert_eq!(got(json_build_array(&[])), want(Ok("[]")));
        assert_eq!(got(jsonb_build_object(&[])), want(Ok("{}")));
    }

    #[test]
    fn the_aggregates_answer_as_postgresql_s() {
        let mut a = JsonAgg::new(false);
        for v in [ints(&[1], &[1]), JsonValue::Null, ints(&[1], &[2])] {
            a.add(&v).unwrap();
        }
        assert_eq!(a.finish().as_deref(), Some("[[1], null, \n [2]]"));
        let mut a = JsonAgg::new(true);
        for v in [JsonValue::Null, num("5")] {
            a.add(&v).unwrap();
        }
        assert_eq!(a.finish().as_deref(), Some("[5]"));
        let mut a = JsonAgg::new(true);
        a.add(&JsonValue::Null).unwrap();
        assert_eq!(a.finish().as_deref(), Some("[]"));
        assert_eq!(JsonAgg::new(false).finish(), None);

        let rows = [
            (txt("a"), num("1")),
            (txt("b"), JsonValue::Null),
            (txt("a"), num("3")),
        ];
        let mut a = JsonObjectAgg::new(false, false);
        for (k, v) in &rows {
            a.add(k, v).unwrap();
        }
        assert_eq!(
            a.finish().as_deref(),
            Some("{ \"a\" : 1, \"b\" : null, \"a\" : 3 }")
        );
        let mut a = JsonObjectAgg::new(true, false);
        for (k, v) in &rows[..2] {
            a.add(k, v).unwrap();
        }
        assert_eq!(a.finish().as_deref(), Some("{ \"a\" : 1 }"));
        let mut a = JsonObjectAgg::new(true, true);
        a.add(&txt("a"), &JsonValue::Null).unwrap();
        assert_eq!(
            a.add(&txt("a"), &num("1")).map_err(|e| e.to_string()),
            Err("duplicate JSON object key value: \"a\"".to_string())
        );
        assert_eq!(
            JsonObjectAgg::new(false, false)
                .add(&JsonValue::Null, &num("1"))
                .map_err(|e| e.to_string()),
            Err("null value not allowed for object key".to_string())
        );

        let mut a = JsonbObjectAgg::new(false, false);
        for (k, v) in [
            (txt("b"), num("1")),
            (txt("a"), num("2")),
            (txt("b"), num("3")),
        ] {
            a.add(&k, &v).unwrap();
        }
        assert_eq!(a.finish().unwrap().as_deref(), Some("{\"a\": 2, \"b\": 3}"));
        let mut a = JsonbObjectAgg::new(true, false);
        a.add(&txt("a"), &JsonValue::Jsonb("1".into())).unwrap();
        a.add(&txt("a"), &JsonValue::Jsonb("null".into())).unwrap();
        assert_eq!(a.finish().unwrap().as_deref(), Some("{\"a\": 1}"));
        let mut a = JsonbObjectAgg::new(false, false);
        a.add(&num("1.50"), &txt("x")).unwrap();
        a.add(&num("NaN"), &txt("y")).unwrap();
        assert_eq!(
            a.finish().unwrap().as_deref(),
            Some("{\"NaN\": \"y\", \"1.50\": \"x\"}")
        );
        let mut a = JsonbObjectAgg::new(true, true);
        a.add(&txt("a"), &JsonValue::Null).unwrap();
        a.add(&txt("a"), &num("1")).unwrap();
        assert_eq!(
            a.finish().map_err(|e| e.to_string()),
            Err("duplicate JSON object key value".to_string())
        );
        let mut a = JsonbObjectAgg::new(true, true);
        a.add(&txt("a"), &JsonValue::Null).unwrap();
        a.add(&txt("b"), &num("1")).unwrap();
        assert_eq!(a.finish().unwrap().as_deref(), Some("{\"b\": 1}"));
        assert_eq!(
            JsonbObjectAgg::new(false, false)
                .add(&JsonValue::Null, &num("1"))
                .map_err(|e| e.to_string()),
            Err("field name must not be null".to_string())
        );
        assert_eq!(JsonbAgg::new(false).finish().unwrap(), None);
    }

    #[test]
    fn json_object_reads_its_text_arrays_as_postgresql_does() {
        let six = texts(&[Some("a"), Some("1"), Some("b"), None, Some("a"), Some("2")]);
        assert_eq!(
            got(json_object(&six, &[6])),
            want(Ok("{\"a\" : \"1\", \"b\" : null, \"a\" : \"2\"}"))
        );
        assert_eq!(
            got(jsonb_object(&six, &[6])),
            want(Ok("{\"a\": \"2\", \"b\": null}"))
        );
        let four = texts(&[Some("a"), Some("1"), Some("b"), Some("2")]);
        assert_eq!(
            got(json_object(&four, &[2, 2])),
            want(Ok("{\"a\" : \"1\", \"b\" : \"2\"}"))
        );
        assert_eq!(
            got(json_object(&four[..3], &[3])),
            want(Err("array must have even number of elements"))
        );
        assert_eq!(
            got(json_object(&four[..3], &[1, 3])),
            want(Err("array must have two columns"))
        );
        assert_eq!(
            got(json_object(&four[..2], &[1, 1, 2])),
            want(Err("wrong number of array subscripts"))
        );
        assert_eq!(
            got(json_object_pairs(&four[..2], &[2], &four[..1], &[1])),
            want(Err("mismatched array dimensions"))
        );
        assert_eq!(
            got(json_object(&texts(&[None, Some("1")]), &[2])),
            want(Err("null value not allowed for object key"))
        );
        assert_eq!(got(json_object(&[], &[])), want(Ok("{}")));
    }

    #[test]
    fn the_readers_answer_as_postgresql_s() {
        assert_eq!(json_typeof(" [1]").unwrap(), "array");
        assert_eq!(json_typeof("\"\\u0000\"").unwrap(), "string");
        assert_eq!(jsonb_typeof("1.5").unwrap(), "number");
        assert_eq!(json_array_length("[1,[2,3],{\"a\":4}]").unwrap(), 3);
        let refused = |r: Result<i32>| r.map_err(|e| e.to_string()).unwrap_err();
        assert_eq!(
            refused(json_array_length("{}")),
            "cannot get array length of a non-array"
        );
        assert_eq!(
            refused(jsonb_array_length("\"x\"")),
            "cannot get array length of a scalar"
        );
        let doc = "{\"a\":\"\\u0041\",\"a\":null, \"b\" : [1, 2]}";
        assert_eq!(
            json_each(doc).unwrap(),
            [("a", "\"\\u0041\""), ("a", "null"), ("b", "[1, 2]")]
                .map(|(k, v)| (k.to_string(), v.to_string()))
        );
        assert_eq!(
            json_each_text(doc).unwrap(),
            [("a", Some("A")), ("a", None), ("b", Some("[1, 2]"))]
                .map(|(k, v)| (k.to_string(), v.map(str::to_string)))
        );
        assert_eq!(
            json_array_elements("[\"\\u0000\", null, {\"a\" : 1}]").unwrap(),
            ["\"\\u0000\"", "null", "{\"a\" : 1}"]
        );
        assert_eq!(
            json_array_elements_text("[\"\\u0000\"]")
                .map_err(|e| e.to_string())
                .unwrap_err(),
            "unsupported Unicode escape sequence"
        );
        assert_eq!(
            json_array_elements_text("[\"\\u00e9\", null, 1.50]").unwrap(),
            [Some("é".to_string()), None, Some("1.50".to_string())]
        );
        assert_eq!(
            json_object_keys("{\"b\":1,\"a\":{\"c\":2},\"b\":3}").unwrap(),
            ["b", "a", "b"]
        );
        assert_eq!(
            json_object_keys("[{\"a\":1}]")
                .map_err(|e| e.to_string())
                .unwrap_err(),
            "cannot call json_object_keys on an array"
        );
        assert_eq!(
            jsonb_object_keys("{\"a\": 2, \"b\": 1, \"aa\": 3}").unwrap(),
            ["a", "b", "aa"]
        );
        assert_eq!(
            jsonb_each_text("{\"a\": null, \"b\": \"x\", \"c\": {\"d\": 1.0}}").unwrap(),
            [("a", None), ("b", Some("x")), ("c", Some("{\"d\": 1.0}"))]
                .map(|(k, v)| (k.to_string(), v.map(str::to_string)))
        );
        assert_eq!(
            jsonb_array_elements("[null, \"x\", {\"a\": 2, \"b\": 1}]").unwrap(),
            ["null", "\"x\"", "{\"a\": 2, \"b\": 1}"]
        );
        let refused =
            |r: Result<Vec<(String, Option<String>)>>| r.map_err(|e| e.to_string()).unwrap_err();
        assert_eq!(refused(json_each_text("1")), "cannot deconstruct a scalar");
        assert_eq!(
            refused(json_each_text("[]")),
            "cannot deconstruct an array as an object"
        );
        assert_eq!(
            refused(jsonb_each_text("[]")),
            "cannot call jsonb_each_text on a non-object"
        );
        let refused = |r: Result<Vec<Option<String>>>| r.map_err(|e| e.to_string()).unwrap_err();
        assert_eq!(
            refused(jsonb_array_elements_text("{}")),
            "cannot extract elements from an object"
        );
        assert_eq!(
            refused(json_array_elements_text("2")),
            "cannot call json_array_elements_text on a scalar"
        );
        assert_eq!(
            json_array_elements("{}")
                .map_err(|e| e.to_string())
                .unwrap_err(),
            "cannot call json_array_elements on a non-array"
        );
    }

    #[test]
    fn strip_nulls_and_pretty_answer_as_postgresql_s() {
        assert_eq!(
            got(json_strip_nulls(
                "{\"a\":null,\"b\":[null,{\"c\":null,\"d\":1}],\"e\":\"\\u0041\"}",
                false
            )),
            want(Ok("{\"b\":[null,{\"d\":1}],\"e\":\"A\"}"))
        );
        assert_eq!(
            got(json_strip_nulls("[null,1,null,{\"a\":null}]", true)),
            want(Ok("[1,{}]"))
        );
        assert_eq!(
            got(jsonb_strip_nulls(
                "{\"a\": null, \"b\": [null, {\"c\": null}]}",
                true
            )),
            want(Ok("{\"b\": [{}]}"))
        );
        assert_eq!(
            got(jsonb_pretty("{\"a\": [1, {\"b\": []}, {}], \"c\": {}}")),
            want(Ok("{\n    \"a\": [\n        1,\n        {\n            \"b\": [\n            ]\n        },\n        {\n        }\n    ],\n    \"c\": {\n    }\n}"))
        );
        assert_eq!(got(jsonb_pretty("[]")), want(Ok("[\n]")));
        assert_eq!(got(jsonb_pretty("\"x\"")), want(Ok("\"x\"")));
    }

    #[test]
    fn jsonb_set_and_insert_answer_as_postgresql_s() {
        let set = |doc: &str, p: &[Option<&str>], new: &str, create: bool| {
            got(jsonb_set(doc, &path(p), new, create))
        };
        type Case<'a> = (&'a str, &'a [Option<&'a str>], &'a str, bool, Answer<'a>);
        let cases: &[Case] = &[
            (
                "{\"a\": 1}",
                &[Some("b")],
                "2",
                true,
                Ok("{\"a\": 1, \"b\": 2}"),
            ),
            ("{\"a\": 1}", &[Some("a")], "[2]", true, Ok("{\"a\": [2]}")),
            ("[1, 2, 3]", &[Some("-10")], "0", true, Ok("[0, 1, 2, 3]")),
            ("[1, 2, 3]", &[Some("10")], "4", true, Ok("[1, 2, 3, 4]")),
            ("[1, 2, 3]", &[Some("10")], "4", false, Ok("[1, 2, 3]")),
            (
                "[1, [2, 3]]",
                &[Some("1"), Some("-1")],
                "\"x\"",
                true,
                Ok("[1, [2, \"x\"]]"),
            ),
            ("[]", &[Some("0")], "1", false, Ok("[]")),
            (
                "{\"a\": {\"b\": 1}}",
                &[Some("a"), None],
                "2",
                true,
                Err("path element at position 2 is null"),
            ),
            (
                "{\"a\": 1}",
                &[Some("b"), None],
                "2",
                true,
                Ok("{\"a\": 1}"),
            ),
            (
                "[1]",
                &[Some("x")],
                "2",
                true,
                Err("path element at position 1 is not an integer: \"x\""),
            ),
            (
                "1",
                &[Some("a")],
                "2",
                true,
                Err("cannot set path in scalar"),
            ),
        ];
        for (doc, p, new, create, w) in cases {
            assert_eq!(set(doc, p, new, *create), want(*w), "{doc} {p:?}");
        }
        let insert = |doc: &str, p: &[Option<&str>], new: &str, after: bool| {
            got(jsonb_insert(doc, &path(p), new, after))
        };
        assert_eq!(
            insert("[1, 2, 3]", &[Some("1")], "\"n\"", false),
            want(Ok("[1, \"n\", 2, 3]"))
        );
        assert_eq!(
            insert("[1, 2, 3]", &[Some("1")], "\"n\"", true),
            want(Ok("[1, 2, \"n\", 3]"))
        );
        assert_eq!(
            insert("{\"a\": 1}", &[Some("a")], "2", false),
            want(Err("cannot replace existing key"))
        );
        assert_eq!(
            insert("{\"a\": 1}", &[Some("b")], "2", false),
            want(Ok("{\"a\": 1, \"b\": 2}"))
        );
        let a = path(&[Some("a")]);
        let lax = |new: Option<&str>, create: Option<bool>, h: Option<&str>| {
            jsonb_set_lax(Some("{\"a\": 1}"), Some(&a), new, create, h).map_err(|e| e.to_string())
        };
        let treatment = "null_value_treatment must be \"delete_key\", \"return_target\", \"use_json_null\", or \"raise_exception\"";
        assert_eq!(
            lax(None, Some(true), Some("use_json_null")),
            Ok(Some("{\"a\": null}".into()))
        );
        assert_eq!(
            lax(None, Some(true), Some("delete_key")),
            Ok(Some("{}".into()))
        );
        assert_eq!(
            lax(None, Some(true), Some("raise_exception")),
            Err("JSON value must not be null".into())
        );
        assert_eq!(lax(None, Some(true), Some("bogus")), Err(treatment.into()));
        assert_eq!(lax(Some("2"), Some(true), None), Err(treatment.into()));
        assert_eq!(lax(None, None, Some("use_json_null")), Ok(None));
    }
}
