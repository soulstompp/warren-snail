//! A question read as a pipeline over one table in its order: each step of a WITH, and the question
//! itself, a SELECT over the table or a step before it. A window along the table's own order folds
//! as the rows come; a window on any other order holds its step's rows and reads them whole
//! (`windows`). A step keeps the order while it carries the order's key and groups nothing. The
//! expressions it reads are CASE, mod, abs, casts, comparisons, IS [NOT] DISTINCT FROM, and the
//! aggregates count, sum, min, max, bool_and and bool_or.

use crate::catalog::{Casing, ColumnType};
use crate::numeric::Numeric;
use crate::error::{NO_OPERATOR, NO_PREFIX_OPERATOR};
use crate::{Error, Result};
use sqlparser::ast::{
    BinaryOperator, CastKind, DataType, Expr, FunctionArg, FunctionArgExpr, FunctionArguments,
    GroupByExpr, Join, JoinOperator, ObjectNamePart, OrderByKind, OrderBySort, Query, SelectItem,
    SetExpr, TableFactor, TableWithJoins, UnaryOperator, Value as SqlValue, WindowFrameBound,
    WindowFrameUnits, WindowType,
};
use std::cmp::Ordering;
use std::collections::HashMap;

pub(super) mod parted;
mod sorted;
pub(super) mod windows;

/// One value of a row: a number whole or with its scale, text, bytes, a truth, or a uuid.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Value {
    Null,
    Int(i64),
    /// A numeric: its digits as one integer, and how many of them lie after the point.
    Num(i128, u32),
    Text(String),
    Bytes(Vec<u8>),
    Bool(bool),
    /// A uuid, as its key.
    Uuid(i128),
    /// A fixed scalar keyed as a number (an oid, an xid, a pg_lsn, a `"char"`, money): its type and
    /// its key.
    Scalar(ColumnType, i64),
    /// A real.
    Real(f32),
    /// A double precision.
    Double(f64),
    /// A value of a string type other than text, a bpchar's or a name's: its type and its own text.
    Str(ColumnType, String),
    /// A bit string: its type, bit(n) or bit varying, and its key.
    Bits(ColumnType, Vec<u8>),
    /// A numeric `Num` does not hold: NaN, an infinity, or one whose digits pass 128 bits.
    Numeric(crate::numeric::Numeric),
    /// A value of a composed type: its type, and the value held whole; an enum's is its rank.
    Composed(ColumnType, std::sync::Arc<crate::composed::Compound>),
    /// An interval, as its own fields.
    Interval(crate::times::Interval),
    /// A value of a type held as its text (json, jsonb, jsonpath, xml, inet, cidr, tsvector, tsquery,
    /// refcursor, pg_node_tree): its type and the text the snail holds.
    Typed(ColumnType, String),
    /// A geometric value.
    Geometric(crate::geo::Geo),
}

impl Value {
    pub(super) fn is_null(&self) -> bool {
        match self {
            Value::Null => true,
            // A composed column's NULL, which keeps its type.
            Value::Composed(_, c) => c.item.is_null(),
            _ => false,
        }
    }

    /// A numeric as a value: `Num` where its digits fit 128 bits.
    pub(crate) fn of_numeric(n: crate::numeric::Numeric) -> Value {
        match n.units() {
            Some((m, s)) => Value::Num(m, s),
            None => Value::Numeric(n),
        }
    }

    /// The value as a numeric, where it is an integer or a numeric.
    pub(crate) fn to_numeric(&self) -> Option<crate::numeric::Numeric> {
        match self {
            Value::Int(v) => Some(crate::numeric::Numeric::of_int(*v)),
            Value::Num(m, s) => Some(crate::numeric::Numeric::of_units(*m, *s)),
            Value::Numeric(n) => Some(n.clone()),
            _ => None,
        }
    }

    /// A numeric's digits at a scale at least its own.
    fn at_scale(m: i128, from: u32, to: u32) -> i128 {
        m * 10i128.pow(to - from)
    }

    /// A numeric's digits at a scale at least its own, where they fit 128 bits.
    fn at_scale_checked(m: i128, from: u32, to: u32) -> Option<i128> {
        10i128.checked_pow(to - from)?.checked_mul(m)
    }

    /// The value as a numeric: its digits and scale.
    pub(super) fn numeric(&self) -> Option<(i128, u32)> {
        match self {
            Value::Int(v) => Some((*v as i128, 0)),
            Value::Num(m, s) => Some((*m, *s)),
            _ => None,
        }
    }

    /// The value as PostgreSQL writes it as text.
    pub fn text(&self) -> Option<String> {
        if self.is_null() {
            return None;
        }
        Some(match self {
            Value::Null => return None,
            Value::Int(v) => v.to_string(),
            Value::Num(m, s) => numeric_text(*m, *s),
            Value::Text(t) => t.clone(),
            Value::Bytes(b) => {
                let mut out = String::from("\\x");
                for x in b {
                    out.push_str(&format!("{x:02x}"));
                }
                out
            }
            Value::Bool(b) => if *b { "true" } else { "false" }.to_string(),
            Value::Uuid(k) => crate::scalars::uuid_text(*k),
            Value::Scalar(ty, k) => crate::scalars::scalar_text(*ty, *k),
            Value::Real(f) => crate::scalars::float_text(u64::from(f.to_bits()), false),
            Value::Double(d) => crate::scalars::float_text(d.to_bits(), true),
            Value::Str(_, s) => s.clone(),
            Value::Bits(_, k) => crate::strings::bits_text(k),
            Value::Numeric(n) => n.to_string(),
            Value::Composed(ty, c) => crate::composed::text_out(*ty, &c.item),
            Value::Interval(v) => crate::times::interval_out(*v),
            Value::Typed(ty, s) => crate::textual::text_cast(*ty, s),
            Value::Geometric(g) => crate::geo::geo_out(g),
        })
    }

    /// The bytes a string is compared by, beside another string: text's and a name's own, a
    /// bpchar's without its trailing blanks.
    pub(super) fn string_key(&self) -> Option<&[u8]> {
        match self {
            Value::Text(t) => Some(t.as_bytes()),
            Value::Str(ty, s) => Some(crate::strings::text_of(*ty, s).as_bytes()),
            _ => None,
        }
    }
}

/// A numeric's digits and scale as PostgreSQL writes them: every digit of its scale after the point.
pub fn numeric_text(m: i128, s: u32) -> String {
    let sign = if m < 0 { "-" } else { "" };
    let digits = m.unsigned_abs().to_string();
    if s == 0 {
        return format!("{sign}{digits}");
    }
    let s = s as usize;
    let padded = format!("{:0>width$}", digits, width = s + 1);
    let (whole, frac) = padded.split_at(padded.len() - s);
    format!("{sign}{whole}.{frac}")
}

/// Each row of a block read along an order put in its place among the rows before it, by the
/// values at `at` in turn, a NULL after every value: the rows written after its sorted ones among
/// them.
pub(super) fn in_place(rows: &mut [Vec<Value>], at: &[usize]) -> Result<()> {
    let mut failed = None;
    rows.sort_by(|a, b| {
        for &i in at {
            let o = match (a[i].is_null(), b[i].is_null()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                _ => match compare_values(&a[i], &b[i], None) {
                    Ok(o) => o.unwrap_or(Ordering::Equal),
                    Err(e) => {
                        failed.get_or_insert(e);
                        Ordering::Equal
                    }
                },
            };
            if o != Ordering::Equal {
                return o;
            }
        }
        Ordering::Equal
    });
    match failed {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Two values compared as PostgreSQL compares values of one kind, two strings under `under`; none
/// where either is NULL.
pub(super) fn compare_values(
    a: &Value,
    b: &Value,
    under: Option<Casing>,
) -> Result<Option<Ordering>> {
    compare_under(under, a, b)
}

/// Two values in the order min and max keep them: a vector's as an array's, as `array_smaller` and
/// `array_larger` take it, whatever comparison its own type has.
fn extreme_order(under: Option<Casing>, a: &Value, b: &Value) -> Result<Option<Ordering>> {
    if let (Value::Composed(ty, x), Value::Composed(_, y)) = (a, b) {
        if crate::composed::is_vector(*ty) {
            let (x, y) = (
                crate::composed::array_order_key(*ty, x),
                crate::composed::array_order_key(*ty, y),
            );
            return Ok(Some(x.cmp(&y)));
        }
        if let Some(part) = crate::composed::compare_reaches(*ty, &x.item, &y.item) {
            return Err(Error::new(format!(
                "could not identify a comparison function for type {}",
                part.pg_name()
            )));
        }
    }
    compare_under(under, a, b)
}

/// Two values compared as PostgreSQL compares values of one kind; none where either is NULL.
fn compare(a: &Value, b: &Value) -> Result<Option<Ordering>> {
    if a.is_null() || b.is_null() {
        return Ok(None);
    }
    if let (Some((x, sx)), Some((y, sy))) = (a.numeric(), b.numeric()) {
        let s = sx.max(sy);
        if let (Some(p), Some(q)) = (
            Value::at_scale_checked(x, sx, s),
            Value::at_scale_checked(y, sy, s),
        ) {
            return Ok(Some(p.cmp(&q)));
        }
    }
    if let (Some(x), Some(y)) = (a.to_numeric(), b.to_numeric()) {
        return Ok(Some(x.cmp(&y)));
    }
    // Two reals compare as reals; a float beside any other number, as doubles.
    if let (Value::Real(x), Value::Real(y)) = (a, b) {
        return Ok(Some(
            crate::scalars::float4_key(x.to_bits()).cmp(&crate::scalars::float4_key(y.to_bits())),
        ));
    }
    if matches!(a, Value::Real(_) | Value::Double(_))
        || matches!(b, Value::Real(_) | Value::Double(_))
    {
        if let (Some(x), Some(y)) = (as_double(a), as_double(b)) {
            return Ok(Some(
                crate::scalars::float8_key(x.to_bits())
                    .cmp(&crate::scalars::float8_key(y.to_bits())),
            ));
        }
    }
    if let (Some(x), Some(y)) = (a.string_key(), b.string_key()) {
        return Ok(Some(x.cmp(y)));
    }
    if let Some(o) = super::clock::compare(a, b)? {
        return Ok(Some(o));
    }
    // Two geometric values, equal or not by their type's `=`, where it has one.
    if let (Value::Geometric(x), Value::Geometric(y)) = (a, b) {
        return match crate::geo::geo_binary("=", x, y) {
            Some(v) => Ok(Some(match v? {
                crate::geo::GeoValue::Bool(true) => Ordering::Equal,
                _ => Ordering::Less,
            })),
            None => Err(Error::new(format!(
                "operator does not exist: {} = {}{NO_OPERATOR}",
                x.kind().name(),
                y.kind().name()
            ))),
        };
    }
    // A value held as its text beside one of a type it compares with, by their classes' keys; a
    // node tree beside text, as text.
    match (a, b) {
        (Value::Typed(x, s), Value::Typed(y, t)) => {
            return crate::textual::compare(*x, s, *y, t).map(Some)
        }
        (Value::Typed(ColumnType::Internal(_), s), Value::Text(t)) => {
            return Ok(Some(s.as_bytes().cmp(t.as_bytes())))
        }
        (Value::Text(s), Value::Typed(ColumnType::Internal(_), t)) => {
            return Ok(Some(s.as_bytes().cmp(t.as_bytes())))
        }
        (Value::Typed(ty, _), other) | (other, Value::Typed(ty, _)) => {
            return Err(Error::new(format!(
                "operator does not exist: {} = {}{NO_OPERATOR}",
                ty.pg_name(),
                kind_name(other)
            )))
        }
        _ => {}
    }
    Ok(Some(match (a, b) {
        (Value::Bits(_, x), Value::Bits(_, y)) => x.cmp(y),
        (Value::Bytes(x), Value::Bytes(y)) => x.cmp(y),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (Value::Uuid(x), Value::Uuid(y)) => x.cmp(y),
        (Value::Composed(a, x), Value::Composed(b, y)) if a == b => {
            // An array's or a composite's = asks each part's equality first; two arrays' only
            // where they are of one shape.
            if let Some(part) = a.unequal_part().filter(|_| asks_parts(&x.item, &y.item)) {
                return Err(unequal(part));
            }
            x.key.cmp(&y.key)
        }
        (Value::Composed(a, _), Value::Composed(b, _)) => {
            return Err(Error::new(format!(
                "operator does not exist: {} = {}{NO_OPERATOR}",
                a.pg_name(),
                b.pg_name()
            )))
        }
        (Value::Composed(ty, _), other) | (other, Value::Composed(ty, _)) => {
            return Err(Error::new(format!(
                "operator does not exist: {} = {}{NO_OPERATOR}",
                ty.pg_name(),
                kind_name(other)
            )))
        }
        (Value::Interval(x), Value::Interval(y)) => x.total().cmp(&y.total()),
        (Value::Scalar(a, x), Value::Scalar(b, y)) if a == b || a.is_oid() && b.is_oid() => {
            x.cmp(y)
        }
        (Value::Scalar(ty, _), other) | (other, Value::Scalar(ty, _)) => {
            return Err(Error::new(format!(
                "operator does not exist: {} = {}{NO_OPERATOR}",
                ty.pg_name(),
                kind_name(other)
            )))
        }
        _ => return Err(Error::new(format!("{a:?} and {b:?} are not compared"))),
    }))
}

/// How two values compare under `under`: two strings under a library collation as it compares
/// them, any others as `compare` does.
fn compare_under(under: Option<Casing>, a: &Value, b: &Value) -> Result<Option<Ordering>> {
    // A "char" beside a string compares as the text it is cast to implicitly.
    let char_beside = |v: &Value, w: &Value| {
        matches!(v, Value::Scalar(ColumnType::Char, _)) && w.string_key().is_some()
    };
    if char_beside(a, b) || char_beside(b, a) {
        let (a, b) = (
            super::ops::char_as_text(a.clone()),
            super::ops::char_as_text(b.clone()),
        );
        return compare_under(under, &a, &b);
    }
    if let (Some(Casing::Library(id)), Some(x), Some(y)) = (under, a.string_key(), b.string_key()) {
        return Ok(Some(crate::collations::get(id).cmp(x, y)));
    }
    compare(a, b)
}

/// A value as `key` makes it, a string under a library collation by the collation's key, so that
/// values a nondeterministic collation calls equal are one key.
fn key_under(v: &Value, under: Option<Casing>) -> Key {
    match (under, v.string_key()) {
        (Some(Casing::Library(id)), Some(s)) => Key::Bytes(crate::collations::get(id).key(s)),
        _ => key(v),
    }
}

/// A value as a key that groups and partitions: equal values, at any scale, are one key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum Key {
    Null,
    Num(i128, u32),
    Text(String),
    Bytes(Vec<u8>),
    Bool(bool),
    Uuid(i128),
    Scalar(ColumnType, i64),
    Real(i64),
    Double(i64),
    /// A numeric no `Num` holds, by its class's key.
    Class(String),
    Composed(Box<[u8]>),
    Interval(i128),
}

pub(super) fn key(v: &Value) -> Key {
    match v {
        Value::Null => Key::Null,
        Value::Int(_) | Value::Num(..) => {
            let (mut m, mut s) = v.numeric().unwrap();
            while s > 0 && m % 10 == 0 {
                m /= 10;
                s -= 1;
            }
            Key::Num(m, s)
        }
        Value::Numeric(n) => match n.trimmed().units() {
            Some((m, s)) => Key::Num(m, s),
            None => Key::Class(n.class_key()),
        },
        Value::Text(t) => Key::Text(t.clone()),
        Value::Bytes(b) => Key::Bytes(b.clone()),
        Value::Bool(b) => Key::Bool(*b),
        Value::Uuid(k) => Key::Uuid(*k),
        Value::Scalar(ty, k) => Key::Scalar(*ty, *k),
        Value::Real(f) => Key::Real(crate::scalars::float4_key(f.to_bits())),
        Value::Double(d) => Key::Double(crate::scalars::float8_key(d.to_bits())),
        Value::Str(..) => Key::Text(String::from_utf8_lossy(v.string_key().unwrap()).into()),
        Value::Bits(_, k) => Key::Bytes(k.clone()),
        // A composed NULL keeps its type, and groups as a NULL.
        Value::Composed(_, c) if c.item.is_null() => Key::Null,
        Value::Composed(_, c) => Key::Composed(c.key.clone()),
        Value::Interval(v) => Key::Interval(v.total()),
        Value::Typed(ty, s) => {
            Key::Bytes(crate::strings::key_bytes(*ty, s.as_bytes()).into_owned())
        }
        // Never a key: grouping and partitioning refuse it first.
        Value::Geometric(g) => Key::Bytes(crate::geo::geo_bytes(g)),
    }
}

/// Why a value of a type PostgreSQL finds no equality for is compared by none.
fn unequal(part: ColumnType) -> Error {
    Error::new(format!(
        "could not identify an equality operator for type {}",
        part.pg_name()
    ))
}

/// The HINT under PostgreSQL's refusal of a sort key of no ordering.
const NO_ORDERING: &str = "\nHINT:  Use an explicit ordering operator or modify the query.";

/// PostgreSQL's refusal of a sort key of the type it names, which it finds no ordering for.
pub(super) fn unordered(name: &str) -> Error {
    Error::new(format!(
        "could not identify an ordering operator for type {name}{NO_ORDERING}"
    ))
}

/// Whether a type is the record a row constructor makes, of no declared type: PostgreSQL's parser
/// finds an ordering and an equality for every such record, and compares its fields only as two
/// rows meet.
pub(super) fn anonymous_record(ty: ColumnType) -> bool {
    matches!(ty, ColumnType::Composed(r)
        if matches!(r.def(), crate::composed::Composed::Record { name, .. } if name == "record"))
}

/// PostgreSQL's refusal of a comparison of two records that reaches a field of a type it compares
/// by no function.
pub(super) fn no_comparison(part: ColumnType) -> Error {
    Error::new(format!(
        "could not identify a comparison function for type {}",
        part.pg_name()
    ))
}

/// How far a comparison of two rows of these values reaches before a row constructor's field of
/// no comparison: the keys of the values and fields before it, and the field's type. Two rows
/// alike that far are compared there, as they meet.
fn reached_part(values: &[Value]) -> Option<(Vec<Key>, ColumnType)> {
    let mut reach = Vec::new();
    for v in values {
        if let Value::Composed(ty @ ColumnType::Composed(r), c) = v {
            if let (true, crate::composed::Composed::Record { fields, .. }, crate::composed::Item::Record(items)) =
                (anonymous_record(*ty), r.def(), &c.item)
            {
                for (f, x) in fields.iter().zip(items) {
                    if !f.ty.ordered() {
                        return Some((reach, f.ty));
                    }
                    let mut k = vec![u8::from(x.is_null())];
                    k.extend(crate::composed::key(f.ty, x));
                    reach.push(Key::Bytes(k));
                }
            }
        }
        reach.push(key(v));
    }
    None
}

/// The rows a DISTINCT or a GROUP BY has met, as far as comparing them reaches into a row
/// constructor's record: refused at the first two alike up to a field of no comparison.
#[derive(Default)]
pub(super) struct Met(std::collections::HashSet<Vec<Key>>);

impl Met {
    pub(super) fn meet(&mut self, values: &[Value]) -> Result<()> {
        match reached_part(values) {
            Some((reach, part)) => match self.0.insert(reach) {
                true => Ok(()),
                false => Err(no_comparison(part)),
            },
            None => Ok(()),
        }
    }
}

/// A sort key's type as PostgreSQL's parser reads it, before any row: refused where it finds no
/// ordering for it, a declared composite by its fields and an array by its elements.
pub(super) fn sorted_type(ty: Option<ColumnType>) -> Result<()> {
    match ty {
        Some(t) if !t.ordered() && !anonymous_record(t) => Err(unordered(&t.pg_name())),
        _ => Ok(()),
    }
}

/// A key that groups or finds DISTINCT values, read as PostgreSQL's parser reads its type before
/// any row: refused, naming the type itself, where it finds no equality for it.
pub(super) fn grouped_type(ty: Option<ColumnType>) -> Result<()> {
    match ty {
        Some(t) if !t.grouped() && !anonymous_record(t) => Err(unequal(t)),
        _ => Ok(()),
    }
}

/// An ORDER BY item of type `ty`, read as PostgreSQL's parser reads it before any row: by the
/// operator it names after USING, else by its type's ordering.
pub(super) fn sort_item(o: &sqlparser::ast::OrderByExpr, ty: Option<ColumnType>) -> Result<()> {
    match o.options.sort {
        Some(OrderBySort::Using(_)) => super::ordering::checked(&o.options, ty),
        _ => sorted_type(ty),
    }
}

/// An aggregate's DISTINCT arguments and its ORDER BY, read as PostgreSQL's parser reads them as
/// it reads the call, before any row: each ORDER BY item sorted, then each argument found
/// DISTINCT and sorted.
fn folded_keys(call: &Expr, typing: &mut super::typed::Typing) -> Result<()> {
    let Expr::Function(f) = call else {
        return Ok(());
    };
    if f.over.is_some() {
        return Ok(());
    }
    let mut by: Vec<&sqlparser::ast::OrderByExpr> = f.within_group.iter().collect();
    let mut distinct = Vec::new();
    if let FunctionArguments::List(l) = &f.args {
        for c in &l.clauses {
            if let sqlparser::ast::FunctionArgumentClause::OrderBy(o) = c {
                by.extend(o);
            }
        }
        if matches!(
            l.duplicate_treatment,
            Some(sqlparser::ast::DuplicateTreatment::Distinct)
        ) {
            distinct.extend(l.args.iter().filter_map(|a| match a {
                sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(e)) => {
                    Some(e)
                }
                _ => None,
            }));
        }
    }
    for o in by {
        sort_item(o, typing.type_of(&o.expr)?)?;
    }
    for e in distinct {
        let ty = typing.type_of(e)?;
        grouped_type(ty)?;
        if let Some(t) = ty.filter(|t| !t.ordered() && !anonymous_record(*t)) {
            return Err(Error::new(format!(
                "could not identify an ordering operator for type {}\nDETAIL:  Aggregates with \
                 DISTINCT must be able to sort their inputs.",
                t.pg_name()
            )));
        }
    }
    Ok(())
}

/// A value as a key that groups, refused where PostgreSQL has no equality to group it by.
fn grouped_key(v: &Value) -> Result<Key> {
    match v {
        Value::Typed(ty, _) | Value::Composed(ty, _) if !ty.grouped() => Err(Error::new(format!(
            "could not identify an equality operator for type {}",
            ty.pg_name()
        ))),
        Value::Geometric(g) => Err(Error::new(format!(
            "could not identify an equality operator for type {}",
            g.kind().name()
        ))),
        v => Ok(key(v)),
    }
}

/// A row as the keys a set operation meets rows by: each value as a group's key, refused where
/// PostgreSQL has no equality for it, a string under a library collation by that collation's key.
pub(super) fn row_key(row: &[Value], under: &[Option<Casing>]) -> Result<Vec<Key>> {
    row.iter()
        .enumerate()
        .map(|(i, v)| {
            grouped_key(v)?;
            Ok(key_under(v, under.get(i).copied().flatten()))
        })
        .collect()
}

/// A relation: its columns' names, each with the alias it was read under, and its rows; and which
/// of its columns holds the table's order key where its rows lie in that order.
#[derive(Clone, Debug, Default)]
pub struct Relation {
    pub names: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    pub order: Option<usize>,
}

/// The table a pipeline reads: its rows in its order, and the column holding its order's key, unique.
pub struct Table {
    pub name: String,
    pub relation: Relation,
}

/// What a step's expressions see: the columns of the relations its FROM reads, each under its
/// qualifier, and the parameters.
pub(super) struct Frame<'a> {
    pub(super) columns: Vec<(Option<String>, String)>,
    pub(super) params: &'a [String],
    /// The release whose catalog names a reg* value.
    pub(super) release: u8,
    /// How the columns map case, by their names.
    pub(super) casings: &'a Casings,
    /// Each expression's type, where the step resolves it.
    pub(super) types: &'a super::typed::Types,
    /// The statement's clock, in microseconds since 1970 UTC.
    pub(super) now: i64,
    /// What answers the step's subqueries, where it has any.
    pub(super) subqueries: Option<&'a dyn Subqueries>,
    /// The snail the statement reads, where its step is read over relations: a call of the file's
    /// function whose body reads relations asks it its body's statement.
    pub(super) reads: Option<&'a dyn super::relations::Reads>,
    /// The values a step makes below its fold and its windows, where it makes any (`Below`).
    pub(super) below: Option<&'a Below>,
}

/// The expressions a step makes below its fold and its windows, a GROUP BY's, a PARTITION BY's or
/// a window ORDER BY's holding a set-returning call: each row of its FROM made once for each value
/// their sets give, in levels and in lockstep, each expression's value after the FROM's columns.
/// `at` is each expression read from them, by its place in the text, and the value it reads; `n`
/// how many there are.
#[derive(Clone, Default)]
pub(super) struct Below {
    at: Vec<(*const Expr, usize)>,
    n: usize,
}

/// What answers a subquery an expression holds (a scalar subquery, EXISTS, IN, ANY or ALL over
/// one), for the row the expression is read on.
pub(super) trait Subqueries {
    fn value(&self, e: &Expr, row: &[Value], frame: &Frame, computed: &Computed) -> Result<Value>;
}

/// Whether an expression is one a subquery answers: a scalar subquery, EXISTS, IN or ANY or ALL
/// over one.
pub(super) fn is_subquery(e: &Expr) -> bool {
    match e {
        Expr::Subquery(_) | Expr::Exists { .. } | Expr::InSubquery { .. } => true,
        Expr::AnyOp { right, .. } | Expr::AllOp { right, .. } => {
            matches!(right.as_ref(), Expr::Subquery(_))
        }
        _ => false,
    }
}

/// How an expression's subqueries are found: those it holds outside any other, outermost first.
pub(super) fn subqueries_in<'q>(e: &'q Expr, out: &mut Vec<&'q Expr>) {
    if is_subquery(e) {
        if let Expr::InSubquery { expr, .. }
        | Expr::AnyOp { left: expr, .. }
        | Expr::AllOp { left: expr, .. } = e
        {
            subqueries_in(expr, out);
        }
        out.push(e);
        return;
    }
    let mut parts = parts_of(e);
    if parts.is_empty() {
        parts = super::collate::children(e);
    }
    for p in parts {
        subqueries_in(p, out);
    }
}

/// How the columns a statement reads and makes map case and order text, where not as the
/// database's default does: the collation each relation the statement makes carries on each of
/// its columns, by its place, and the collation each of its steps' expressions takes and is called
/// under, as PostgreSQL derives them; and the collations the DDL makes. An expression read alone (a
/// CHECK's, a DEFAULT's) finds its table's columns by their names. Beside them, the lc_monetary the
/// statement's money reads and prints under: the file's.
#[derive(Clone, Debug, Default)]
pub struct Casings {
    columns: HashMap<String, Casing>,
    declared: Vec<crate::catalog::Collation>,
    /// The columns under a library collation this build opens none of, each with its collation.
    unheld: HashMap<String, String>,
    /// Whether a column orders its text through a library, or names a collation that would.
    library: bool,
    /// The file's lc_monetary.
    monetary: crate::monetary::Monetary,
    /// The file's default_text_search_config, and whether tables of files naming two are met.
    text_search: crate::tsearch::TextSearch,
    text_search_split: bool,
    /// The collation each relation's columns carry, by the relation's name.
    relations: HashMap<String, Vec<(String, super::collate::Carried)>>,
    /// Each step expression's collation, and the one it is called under.
    nodes: HashMap<*const Expr, (super::collate::Derived, super::collate::Derived)>,
    /// The casing each collation the steps name maps case and orders text by, or why none.
    resolved: HashMap<String, std::result::Result<Casing, String>>,
}

impl Casings {
    /// The lc_monetary the statement's money reads and prints under.
    pub(crate) fn monetary(&self) -> crate::monetary::Monetary {
        self.monetary
    }

    /// The configuration a call naming none reads: refused by name across files naming two.
    pub(crate) fn text_search(&self) -> Result<crate::tsearch::TextSearch> {
        if self.text_search_split {
            return Err(Error::new(
                "default_text_search_config is read across snails recording different ones",
            ));
        }
        Ok(self.text_search)
    }

    /// No column's casing, money read and printed and a text search read under the table's file's:
    /// what an expression naming no column of a table reads, a domain's CHECK or a DEFAULT.
    pub(crate) fn under(table: &crate::catalog::Table) -> Casings {
        Casings {
            monetary: table.monetary,
            text_search: table.text_search,
            ..Casings::default()
        }
    }

    /// No column's casing, under a file's lc_monetary and default_text_search_config: what an
    /// expression naming no table reads.
    pub(crate) fn of_file(
        monetary: crate::monetary::Monetary,
        text_search: crate::tsearch::TextSearch,
    ) -> Casings {
        Casings {
            monetary,
            text_search,
            ..Casings::default()
        }
    }

    /// The casings of several tables' columns, each table's collations carried under its name, and
    /// the collations the DDL makes.
    pub(super) fn of_tables(
        tables: &[&crate::catalog::Table],
        declared: &[crate::catalog::Collation],
    ) -> Casings {
        let mut c = Casings {
            declared: declared.to_vec(),
            monetary: tables.first().map(|t| t.monetary).unwrap_or_default(),
            text_search: tables.first().map(|t| t.text_search).unwrap_or_default(),
            text_search_split: tables.windows(2).any(|w| w[0].text_search != w[1].text_search),
            ..Casings::default()
        };
        for t in tables {
            let one = Casings::of_table(t, declared);
            for (n, k) in one.columns {
                c.columns.entry(n).or_insert(k);
            }
            c.unheld.extend(one.unheld);
            c.library |= one.library;
            c.relations.extend(one.relations);
        }
        c
    }

    /// The collation each column of a relation carries, by the relation's name.
    pub(super) fn relation(&self, name: &str) -> Option<&[(String, super::collate::Carried)]> {
        self.relations.get(name).map(Vec::as_slice)
    }

    /// A relation's columns renamed, each keeping the collation it carries.
    pub(super) fn renamed(&mut self, name: &str, names: &[String]) {
        if let Some(r) = self.relations.get_mut(name) {
            for ((n, _), to) in r.iter_mut().zip(names) {
                *n = to.clone();
            }
        }
    }

    /// The casings of a table's columns, and the collations the DDL makes.
    pub fn of_table(
        table: &crate::catalog::Table,
        declared: &[crate::catalog::Collation],
    ) -> Casings {
        let mut c = Casings {
            columns: HashMap::new(),
            declared: declared.to_vec(),
            unheld: HashMap::new(),
            library: false,
            monetary: table.monetary,
            text_search: table.text_search,
            text_search_split: false,
            relations: HashMap::new(),
            nodes: HashMap::new(),
            resolved: HashMap::new(),
        };
        let carried = table
            .columns
            .iter()
            .map(|col| {
                let carried = match (&col.collation, col.ty) {
                    (_, ty) if !super::collate::collatable(ty) => super::collate::Carried::None,
                    (Some(n), _) => super::collate::Carried::Named(super::collate::named(n)),
                    (None, ColumnType::Name) => super::collate::Carried::Named("C".into()),
                    (None, _) => super::collate::Carried::Named(super::collate::DEFAULT.into()),
                };
                (col.name.clone(), carried)
            })
            .collect();
        c.relations.insert(table.name.clone(), carried);
        for col in &table.columns {
            let named = match (&col.collation, col.ty) {
                (Some(n), _) => match crate::catalog::collation_casing(n, declared) {
                    Ok(k) => Some(k),
                    Err(_) => {
                        c.unheld.insert(col.name.clone(), n.clone());
                        c.library = true;
                        None
                    }
                },
                (None, ColumnType::Name) => Some(Casing::Ascii),
                _ => None,
            };
            if let Some(k) = named {
                c.library |= matches!(k, Casing::Library(_));
                c.columns.insert(col.name.clone(), k);
            }
        }
        c
    }

    /// The library collation an expression's strings are compared under, where one is: as
    /// `implicit` finds it; at once none where no column and no COLLATE names one.
    fn ordering(&self, e: &Expr) -> Result<Option<Casing>> {
        if !self.library && !names_collate(e) {
            return Ok(None);
        }
        Ok(self
            .implicit(e)?
            .filter(|k| matches!(k, Casing::Library(_))))
    }

    /// How the collation named maps case, where the snail holds it.
    fn named(&self, name: &str) -> Result<Casing> {
        crate::catalog::collation_held(name, &self.declared)
            .ok_or_else(|| Error::new(format!("COLLATE {name}: {}", crate::catalog::NOT_HELD)))
    }

    /// The casing a collation's name maps case and orders text by, or why this build holds none,
    /// as `resolve` found it where a step names it.
    fn casing_named(&self, name: &str) -> Result<Casing> {
        match self.resolved.get(name) {
            Some(Ok(k)) => Ok(*k),
            Some(Err(e)) => Err(Error::new(e.clone())),
            None => self.resolve(name),
        }
    }

    /// The library collation a relation's column is compared under where rows are met by their
    /// values: the collation it carries, where that is one.
    pub(super) fn carried_under(&self, c: &super::collate::Carried) -> Result<Option<Casing>> {
        match c {
            super::collate::Carried::Named(n) => {
                Ok(Some(self.casing_named(n)?).filter(|k| matches!(k, Casing::Library(_))))
            }
            _ => Ok(None),
        }
    }

    /// The casing a collation's name maps case and orders text by, or why this build holds none:
    /// a column's under a library collation without the feature, refused by name.
    fn resolve(&self, name: &str) -> Result<Casing> {
        if self
            .unheld
            .values()
            .any(|n| super::collate::named(n) == name)
        {
            return Err(crate::collations::unbuilt(name));
        }
        if name == super::collate::DEFAULT {
            return Ok(Casing::Libc);
        }
        crate::catalog::collation_casing(name, &self.declared)
    }

    /// The casing a call or an operator is called under, as its inputs' collation settles it;
    /// refused in PostgreSQL's words for `what` where none is settled.
    fn input(&self, e: &Expr, what: &str) -> Result<Option<Casing>> {
        match self.nodes.get(&(e as *const Expr)) {
            Some((_, input)) => input
                .settled(what)?
                .map(|n| self.casing_named(n))
                .transpose(),
            None if self.nodes.is_empty() => self.implicit(e),
            None => Ok(None),
        }
    }

    /// The collation a call is made under, by its name, where its inputs settle one.
    fn called_under(&self, e: &Expr) -> Option<&str> {
        self.nodes
            .get(&(e as *const Expr))
            .and_then(|(_, input)| input.under())
    }

    /// A collation's name as PostgreSQL writes it: one the DDL makes in a schema other than public,
    /// by its schema and its name, each quoted where it must be; any other by its name alone.
    fn written_collation(&self, name: &str) -> String {
        let quote = super::calls::quote_ident;
        match name.split_once('.') {
            Some((schema, bare)) if self.declared.iter().any(|c| c.name == name) => match schema {
                "public" => quote(bare),
                _ => format!("{}.{}", quote(schema), quote(bare)),
            },
            _ => quote(name),
        }
    }

    /// Whether the inputs of a call or an operator settle no collation.
    fn unsettled(&self, e: &Expr) -> bool {
        self.nodes
            .get(&(e as *const Expr))
            .is_some_and(|(_, input)| input.settled("").is_err())
    }

    /// Whether a comparison is made under a library collation, or under none its inputs settle.
    pub(super) fn compared_under_library(&self, e: &Expr) -> bool {
        !matches!(self.compared(e, "string comparison"), Ok(None))
    }

    /// The library collation a comparison is made under, where one is, as `input` settles it.
    fn compared(&self, e: &Expr, what: &str) -> Result<Option<Casing>> {
        if self.nodes.is_empty() {
            return self.ordering(e);
        }
        Ok(self
            .input(e, what)?
            .filter(|k| matches!(k, Casing::Library(_))))
    }

    /// The library collation a key is compared under where a step sorts, groups or partitions by
    /// it, where one is: its own collation.
    fn keyed(&self, e: &Expr) -> Result<Option<Casing>> {
        match self.nodes.get(&(e as *const Expr)) {
            Some((out, _)) => Ok(out
                .keyed()?
                .map(|n| self.casing_named(n))
                .transpose()?
                .filter(|k| matches!(k, Casing::Library(_)))),
            None if self.nodes.is_empty() => self.ordering(e),
            None => Ok(None),
        }
    }

    /// The library collation each argument of an aggregate is compared under, where one is. Where
    /// the aggregate reads each value once, two implicit collations met in one refused at once, as
    /// PostgreSQL's parser sorts them; otherwise none, and the words min and max refuse it in at
    /// the first two values they compare, as PostgreSQL's comparison refuses it at the row.
    fn folded(&self, call: &Expr, args: &[&Expr]) -> Result<(Vec<Option<Casing>>, Option<String>)> {
        let distinct = matches!(call, Expr::Function(f) if matches!(&f.args,
            FunctionArguments::List(l) if l.duplicate_treatment == Some(sqlparser::ast::DuplicateTreatment::Distinct)));
        let mut refused = None;
        let mut under = Vec::with_capacity(args.len());
        for &x in args {
            let unsettled = match self.nodes.get(&(x as *const Expr)) {
                Some((out, _)) if !distinct => out.settled("string comparison").err(),
                _ => None,
            };
            match unsettled {
                Some(e) => {
                    refused.get_or_insert(e.to_string());
                    under.push(None);
                }
                None => under.push(self.keyed(x)?),
            }
        }
        Ok((under, refused))
    }

    /// The library collation a column a step makes is compared under, by what it carries.
    fn carried(&self, c: &super::collate::Carried) -> Result<Option<Casing>> {
        Ok(match c {
            super::collate::Carried::Named(n) => Some(self.casing_named(n)?),
            super::collate::Carried::Unsettled => {
                return Err(Error::new(
                    "could not determine which collation to use for string comparison",
                ))
            }
            super::collate::Carried::None => None,
        }
        .filter(|k| matches!(k, Casing::Library(_))))
    }

    /// The casing an expression's text takes: the collation it takes in its step, else, read alone,
    /// where a COLLATE or a column names one, as PostgreSQL carries a collation from a column or a
    /// COLLATE to what a function makes of it.
    fn implicit(&self, e: &Expr) -> Result<Option<Casing>> {
        if let Some((out, _)) = self.nodes.get(&(e as *const Expr)) {
            return out
                .settled("string comparison")?
                .map(|n| self.casing_named(n))
                .transpose();
        }
        if !self.nodes.is_empty() {
            return Ok(None);
        }
        Ok(match e {
            Expr::Collate { collation, .. } => {
                Some(self.named(&crate::catalog::object_name(collation))?)
            }
            Expr::Nested(x) => self.implicit(x)?,
            Expr::Identifier(_) | Expr::CompoundIdentifier(_) => match column_of(e) {
                Some((_, n)) => match self.unheld.get(&n) {
                    Some(k) => return Err(crate::collations::unbuilt(k)),
                    None => self.columns.get(&n).copied(),
                },
                None => None,
            },
            Expr::Function(f) => match args(f).ok().and_then(|a| a.first().copied()) {
                Some(x) => self.implicit(x)?,
                None => None,
            },
            // An explicit COLLATE on either side wins over a column's own.
            Expr::BinaryOp { left, right, .. } => {
                let (first, then) = if names_collate(right) && !names_collate(left) {
                    (right, left)
                } else {
                    (left, right)
                };
                match self.implicit(first)? {
                    Some(k) => Some(k),
                    None => self.implicit(then)?,
                }
            }
            // A pattern's, a distinctness's and an IN list's operands as a comparison's.
            Expr::Like { expr, pattern, .. }
            | Expr::ILike { expr, pattern, .. }
            | Expr::SimilarTo { expr, pattern, .. } => self.operands(&[expr, pattern])?,
            Expr::IsDistinctFrom(a, b) | Expr::IsNotDistinctFrom(a, b) => self.operands(&[a, b])?,
            Expr::InList { expr, list, .. } => {
                let xs: Vec<&Expr> = std::iter::once(&**expr).chain(list).collect();
                self.operands(&xs)?
            }
            _ => None,
        })
    }

    /// The casing an operator's operands take, read alone: an explicit COLLATE's on any of them,
    /// else the first a column or a function of one names.
    fn operands(&self, xs: &[&Expr]) -> Result<Option<Casing>> {
        for x in xs.iter().filter(|x| names_collate(x)) {
            if let Some(k) = self.implicit(x)? {
                return Ok(Some(k));
            }
        }
        for x in xs {
            if let Some(k) = self.implicit(x)? {
                return Ok(Some(k));
            }
        }
        Ok(None)
    }

    /// A step's collations derived before any row, over the columns its FROM reads: each of its
    /// expressions', the relation it makes carrying each item's under the step's name; a key it
    /// sorts, groups or partitions by, or returns once each, refused where two implicit collations
    /// meet in it, as PostgreSQL's parser refuses one. The collation each of its columns carries.
    pub(super) fn add_step(
        &mut self,
        name: &str,
        p: &Prepared,
        columns: &[(Option<String>, String, super::collate::Carried)],
    ) -> Result<Vec<super::collate::Carried>> {
        let mut d = super::collate::Deriving {
            columns,
            types: &p.types,
            nodes: std::mem::take(&mut self.nodes),
        };
        let mut carried = Vec::with_capacity(p.items.len());
        let mut outs = Vec::with_capacity(p.items.len());
        for (e, _) in &p.items {
            let out = d.derive(e)?;
            if p.distinct {
                out.keyed()?;
            }
            let collatable = d
                .types
                .get(&(*e as *const Expr))
                .is_none_or(|t| super::collate::collatable(*t));
            carried.push(out.carried(collatable));
            outs.push(out);
        }
        for w in p.selection.iter().chain(&p.having).chain(&p.joined) {
            d.derive(w)?;
        }
        for g in &p.grouping {
            d.derive(g)?.keyed()?;
        }
        // ORDER BY sorts by an item it names, keyed as the item was derived, or by an expression
        // over the FROM's columns.
        for &at in &p.sorted {
            match outs.get(at) {
                Some(out) => out.keyed()?,
                None => d.derive(p.hidden[at - outs.len()])?.keyed()?,
            };
        }
        self.nodes = d.nodes;
        // Each collation the step names, resolved once.
        let named: Vec<String> = self
            .nodes
            .values()
            .flat_map(|(out, input)| out.names().chain(input.names()))
            .map(str::to_string)
            .collect();
        for n in named {
            if !self.resolved.contains_key(&n) {
                let k = self.resolve(&n).map_err(|e| e.to_string());
                self.resolved.insert(n, k);
            }
        }
        self.library |= carried.iter().any(|c| {
            matches!(c, super::collate::Carried::Named(n)
                if matches!(self.casing_named(n), Ok(Casing::Library(_))))
        });
        self.relations.insert(
            name.to_string(),
            p.items
                .iter()
                .map(|(_, n)| n.clone())
                .zip(carried.iter().cloned())
                .collect(),
        );
        Ok(carried)
    }
}

/// Whether an expression names a COLLATE where `Casings::implicit` looks for one.
fn names_collate(e: &Expr) -> bool {
    match e {
        Expr::Collate { .. } => true,
        Expr::Nested(x) => names_collate(x),
        Expr::Function(f) => args(f)
            .ok()
            .and_then(|a| a.first().copied())
            .is_some_and(names_collate),
        Expr::BinaryOp { left, right, .. } => names_collate(left) || names_collate(right),
        _ => false,
    }
}

impl Frame<'_> {
    /// The type the step resolved an expression to, where it resolved one.
    pub(super) fn type_of(&self, e: &Expr) -> Option<ColumnType> {
        self.types.get(&(e as *const Expr)).copied()
    }

    /// An integer an operator or a function returns, held to the range of the type the step
    /// resolved it to.
    fn held(&self, e: &Expr, v: Value) -> Result<Value> {
        match (v, self.type_of(e)) {
            (Value::Int(x), Some(t @ (ColumnType::Int2 | ColumnType::Int4))) => {
                super::calls::held_to(Some(t), i128::from(x))
            }
            (v, _) => Ok(v),
        }
    }

    pub(super) fn find(&self, qualifier: Option<&str>, name: &str) -> Result<usize> {
        let found = naming(
            self.columns.iter().map(|(q, n)| (q.as_deref(), n.as_str())),
            qualifier,
            name,
        );
        match found.as_slice() {
            [i] => Ok(*i),
            [] => Err(Error::new(format!("{name} is no column the step reads"))),
            _ => Err(Error::new(format!(
                "column reference \"{name}\" is ambiguous"
            ))),
        }
    }
}

fn ident(i: &sqlparser::ast::Ident) -> String {
    crate::catalog::ident(i)
}

/// What marks a column's qualifier where a USING or NATURAL join has merged the column away from
/// a bare name: it answers only to its relation's name. A reference a star is read as names its
/// column by its place after the mark.
pub(super) const MERGED: char = '\0';

/// What marks the qualifier of the one column of a function in FROM returning a scalar: a bare
/// reference to the function's name reads that column, as PostgreSQL reads its whole row.
pub(super) const SCALAR: char = '\u{1}';

/// What marks the qualifier of a column of a query around a subquery, once for each query out: a
/// reference finds it only where no nearer query has a column it names.
pub(super) const OUTER: char = '\u{2}';

/// What follows the qualifier of a column of a table its FROM names without an alias: the table's
/// full name, which a reference by its schema and name answers to. Such a reference's qualifier is
/// the mark and the names before its column's, a database's among them where it writes one.
pub(super) const TABLE: char = '\u{7}';

/// A qualifier without its marks: the name of the relation it answers to.
pub(super) fn unmarked(q: &str) -> &str {
    let q = q.trim_start_matches([MERGED, SCALAR, OUTER]);
    q.split(TABLE).next().unwrap_or(q)
}

/// Whether a column of qualifier `q` answers to a reference's qualifier `want`: one by schema and
/// name only where the column's relation is the table of that full name named without an alias,
/// any other by the name its relation goes by.
pub(super) fn answers_to(q: Option<&str>, want: &str) -> bool {
    match want.strip_prefix(TABLE) {
        Some(table) => q
            .and_then(|q| q.split_once(TABLE))
            .is_some_and(|(_, t)| t == table),
        None => q.map(unmarked) == Some(want),
    }
}

/// The relation a reference's qualifier names, as PostgreSQL's refusals name it: its last name.
pub(super) fn relation_named(want: &str) -> &str {
    let q = want.strip_prefix(TABLE).unwrap_or(want);
    q.rsplit('.').next().unwrap_or(q)
}

/// The refusal of a reference by more names than a schema's and a table's before its column's, in
/// PostgreSQL's words: a database's other than the file's, or more names still.
pub(super) fn too_many_names(want: &str, name: &str) -> Option<Error> {
    let names = want.strip_prefix(TABLE)?;
    match names.split('.').count() {
        0..=2 => None,
        3 => Some(Error::new(format!(
            "cross-database references are not implemented: {names}.{name}"
        ))),
        _ => Some(Error::new(format!(
            "improper qualified name (too many dotted names): {names}.{name}"
        ))),
    }
}

/// How many queries out a column's relation stands.
pub(super) fn outer_level(q: Option<&str>) -> usize {
    q.map_or(0, |q| q.chars().take_while(|&c| c == OUTER).count())
}

/// The places of the columns of a FROM a reference names, in the nearest query that has one: the
/// one a star's reference names by its place; else each of that name, under the qualifier where
/// the reference has one, a bare name passing over the columns a USING or NATURAL join merged away;
/// else, for a bare name, the one column of the function in FROM of that name returning a scalar.
/// A qualified reference stays in the nearest query holding a relation of its qualifier.
pub(super) fn naming<'c>(
    columns: impl Iterator<Item = (Option<&'c str>, &'c str)> + Clone,
    qualifier: Option<&str>,
    name: &str,
) -> Vec<usize> {
    if let (None, Some(at)) = (qualifier, name.strip_prefix(MERGED)) {
        return at.parse().map(|i| vec![i]).unwrap_or_default();
    }
    let levels = columns
        .clone()
        .map(|(q, _)| outer_level(q))
        .max()
        .unwrap_or(0);
    for level in 0..=levels {
        let at = |q: Option<&str>| outer_level(q) == level;
        let found: Vec<usize> = columns
            .clone()
            .enumerate()
            .filter(|(_, (q, n))| {
                at(*q)
                    && *n == name
                    && match qualifier {
                        Some(want) => answers_to(*q, want),
                        None => !q.is_some_and(|q| q.trim_start_matches(OUTER).starts_with(MERGED)),
                    }
            })
            .map(|(i, _)| i)
            .collect();
        if !found.is_empty() {
            return found;
        }
        if let Some(want) = qualifier {
            if columns.clone().any(|(q, _)| at(q) && answers_to(q, want)) {
                return found;
            }
            continue;
        }
        let scalar: Vec<usize> = columns
            .clone()
            .enumerate()
            .filter(|(_, (q, _))| {
                at(*q)
                    && q.and_then(|q| q.trim_start_matches(OUTER).strip_prefix(SCALAR))
                        == Some(name)
            })
            .map(|(i, _)| i)
            .collect();
        if !scalar.is_empty() {
            return scalar;
        }
    }
    Vec::new()
}

/// The column an expression names, where it names one: by its name, its relation's and its own,
/// or its table's schema and name and its own (`TABLE`), the file's database's name before them
/// passed over.
pub(super) fn column_of(e: &Expr) -> Option<(Option<String>, String)> {
    match e {
        Expr::Identifier(i) => Some((None, ident(i))),
        Expr::CompoundIdentifier(p) if p.len() == 2 => Some((Some(ident(&p[0])), ident(&p[1]))),
        Expr::CompoundIdentifier(p) if p.len() > 2 => {
            let names: Vec<String> = p.iter().map(ident).collect();
            let (relation, column) = names.split_at(names.len() - 1);
            Some((Some(qualified(relation)), column[0].clone()))
        }
        Expr::Nested(e) => column_of(e),
        _ => None,
    }
}

/// The qualifier of a reference by a table's schema and name, the file's database's name before
/// them passed over.
pub(super) fn qualified(names: &[String]) -> String {
    let names = match names {
        [db, rest @ ..] if rest.len() == 2 && db == crate::pgcatalog::DATABASE => rest,
        n => n,
    };
    format!("{TABLE}{}", names.join("."))
}

/// The aggregates and windows an expression holds, found once each, by their place in the text.
#[derive(Clone, Default)]
struct Calls<'q> {
    aggregates: Vec<&'q Expr>,
    windows: Vec<&'q Expr>,
    /// Its GROUPING calls, each read from the set a group is made for.
    groupings: Vec<&'q Expr>,
    /// The set-returning calls of its select list.
    sets: Vec<&'q Expr>,
    /// The expressions it does not look inside, by their place in the text: those a step makes
    /// below its fold (`Below`).
    stops: Vec<*const Expr>,
}

const AGGREGATES: [&str; 37] = [
    "string_agg",
    "array_agg",
    "range_agg",
    "range_intersect_agg",
    "count",
    "sum",
    "min",
    "max",
    "avg",
    "bool_and",
    "bool_or",
    "every",
    "var_pop",
    "var_samp",
    "variance",
    "stddev",
    "stddev_pop",
    "stddev_samp",
    "covar_pop",
    "covar_samp",
    "corr",
    "regr_count",
    "regr_sxx",
    "regr_syy",
    "regr_sxy",
    "regr_avgx",
    "regr_avgy",
    "regr_r2",
    "regr_slope",
    "regr_intercept",
    "bit_and",
    "bit_or",
    "bit_xor",
    "any_value",
    "mode",
    "percentile_cont",
    "percentile_disc",
];

/// Whether a name is an aggregate a step folds.
pub(crate) fn is_aggregate(name: &str) -> bool {
    AGGREGATES.contains(&name) || super::json::AGGREGATES.contains(&name)
        || super::handed::is_aggregate(name)
}

pub(super) fn function_name(f: &sqlparser::ast::Function) -> String {
    match f.name.0.last() {
        Some(ObjectNamePart::Identifier(i)) => ident(i),
        _ => f.name.to_string().to_lowercase(),
    }
}

/// Whether a SELECT's list or HAVING calls an aggregate, or GROUPING.
pub(super) fn aggregates(s: &sqlparser::ast::Select) -> bool {
    let mut calls = Calls::default();
    for i in &s.projection {
        if let SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } = i {
            find_calls(e, &mut calls);
        }
    }
    if let Some(h) = &s.having {
        find_calls(h, &mut calls);
    }
    !calls.aggregates.is_empty() || !calls.groupings.is_empty()
}

/// Whether a call is GROUPING (…), a word of PostgreSQL's grammar: unquoted, in no schema.
pub(super) fn is_grouping_call(f: &sqlparser::ast::Function) -> bool {
    matches!(f.name.0.as_slice(), [ObjectNamePart::Identifier(i)]
        if i.quote_style.is_none() && i.value.eq_ignore_ascii_case("grouping"))
        && f.over.is_none()
}

/// What a window call reads: its arguments, its FILTER, its PARTITION BY and its ORDER BY.
fn window_parts(e: &Expr) -> Vec<&Expr> {
    let Expr::Function(f) = e else {
        return Vec::new();
    };
    let mut out = parts_of(e);
    out.extend(f.filter.as_deref());
    if let Some(WindowType::WindowSpec(spec)) = &f.over {
        out.extend(&spec.partition_by);
        out.extend(spec.order_by.iter().map(|o| &o.expr));
    }
    out
}

/// A GROUPING call's arguments.
fn grouping_args_of(e: &Expr) -> Vec<&Expr> {
    match e {
        Expr::Function(f) => parts_of(e)
            .into_iter()
            .filter(|_| is_grouping_call(f))
            .collect(),
        _ => Vec::new(),
    }
}

/// Whether an expression calls an aggregate, a window function or a set-returning function, as a
/// step reads its calls.
pub(super) fn calls_in(e: &Expr) -> (bool, bool, bool) {
    let mut c = Calls::default();
    find_calls(e, &mut c);
    (
        !c.aggregates.is_empty(),
        !c.windows.is_empty(),
        !c.sets.is_empty(),
    )
}

/// An aggregate of the step's own in a clause no aggregate stands in, or a window, refused as
/// PostgreSQL's parser refuses it, naming the clause: one reading none of the query around it, or a
/// column of the step's own FROM. One reading only the query around it is that query's.
pub(super) fn no_own_calls(
    e: &Expr,
    columns: &[super::typed::TypedColumn],
    clause: &str,
) -> Result<()> {
    let mut found = Calls::default();
    find_calls(e, &mut found);
    if !found.windows.is_empty() {
        return Err(Error::new(format!(
            "window functions are not allowed in {clause}"
        )));
    }
    let own = |a: &&Expr| {
        let mut read = Vec::new();
        columns_read(a, &mut read);
        let levels: Option<Vec<usize>> = read
            .iter()
            .map(|(q, n)| {
                let at = naming(
                    columns.iter().map(|(cq, cn, _)| (cq.as_deref(), cn.as_str())),
                    q.as_deref(),
                    n,
                );
                match at.as_slice() {
                    [i] => Some(outer_level(columns[*i].0.as_deref())),
                    _ => None,
                }
            })
            .collect();
        levels.is_some_and(|l| l.is_empty() || l.contains(&0))
    };
    if found.aggregates.iter().any(own) {
        return Err(Error::new(format!(
            "aggregate functions are not allowed in {clause}"
        )));
    }
    if found.groupings.iter().any(own) {
        return Err(Error::new(format!(
            "grouping operations are not allowed in {clause}"
        )));
    }
    Ok(())
}

/// A step's aggregates and GROUPING calls refused as PostgreSQL's parser refuses them: GROUPING of
/// more than 31 arguments; an aggregate or GROUPING in an aggregate's FILTER, or among an
/// aggregate's arguments, its ORDER BY or GROUPING's.
fn nested_calls(calls: &Calls) -> Result<()> {
    let holds = |xs: Vec<&Expr>| {
        let mut c = Calls::default();
        for x in xs {
            find_calls(x, &mut c);
        }
        (!c.aggregates.is_empty(), !c.groupings.is_empty())
    };
    let nested = || Error::new("aggregate function calls cannot be nested");
    for &g in &calls.groupings {
        let args = grouping_args_of(g);
        if args.len() > 31 {
            return Err(Error::new("GROUPING must have fewer than 32 arguments"));
        }
        if holds(args) != (false, false) {
            return Err(nested());
        }
    }
    for &c in &calls.aggregates {
        let Expr::Function(f) = c else {
            continue;
        };
        if let Some(w) = f.filter.as_deref() {
            match holds(vec![w]) {
                (true, _) => return Err(Error::new("aggregate functions are not allowed in FILTER")),
                (_, true) => return Err(Error::new("grouping operations are not allowed in FILTER")),
                _ => {}
            }
        }
        let mut parts = parts_of(c);
        if let FunctionArguments::List(l) = &f.args {
            for clause in &l.clauses {
                if let sqlparser::ast::FunctionArgumentClause::OrderBy(os) = clause {
                    parts.extend(os.iter().map(|o| &o.expr));
                }
            }
        }
        parts.extend(f.within_group.iter().map(|o| &o.expr));
        if holds(parts) != (false, false) {
            return Err(nested());
        }
    }
    Ok(())
}

fn find_calls<'q>(e: &'q Expr, out: &mut Calls<'q>) {
    if out.stops.iter().any(|&s| std::ptr::eq(s, e)) {
        return;
    }
    match e {
        Expr::Function(f) => {
            if f.over.is_some() {
                out.windows.push(e);
                // The aggregates and GROUPING calls a window reads are its step's, read from the
                // group each row of the window is.
                let mut read = Calls {
                    stops: out.stops.clone(),
                    ..Calls::default()
                };
                for x in window_parts(e) {
                    find_calls(x, &mut read);
                }
                out.aggregates.extend(read.aggregates);
                out.groupings.extend(read.groupings);
                return;
            }
            if is_grouping_call(f) {
                out.groupings.push(e);
                return;
            }
            if is_aggregate(&function_name(f)) || !f.within_group.is_empty() {
                out.aggregates.push(e);
                return;
            }
            if is_set_returning(&function_name(f)) {
                out.sets.push(e);
            }
            if let FunctionArguments::List(l) = &f.args {
                for a in &l.args {
                    if let FunctionArg::Unnamed(FunctionArgExpr::Expr(x)) = a {
                        find_calls(x, out);
                    }
                }
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            find_calls(left, out);
            find_calls(right, out);
        }
        Expr::UnaryOp { expr, .. }
        | Expr::Nested(expr)
        | Expr::Cast { expr, .. }
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr)
        | Expr::IsTrue(expr)
        | Expr::IsNotTrue(expr)
        | Expr::IsFalse(expr)
        | Expr::IsNotFalse(expr)
        | Expr::IsUnknown(expr)
        | Expr::IsNotUnknown(expr) => find_calls(expr, out),
        Expr::IsDistinctFrom(a, b) | Expr::IsNotDistinctFrom(a, b) => {
            find_calls(a, out);
            find_calls(b, out);
        }
        // A field of a composite an aggregate or a function gives, or an element of an array by
        // subscripts an aggregate gives.
        Expr::CompoundFieldAccess { root, access_chain } => {
            find_calls(root, out);
            for x in subscripts(access_chain) {
                find_calls(x, out);
            }
        }
        // What a subquery is compared with is the step's; the subquery's own calls are its own.
        Expr::InSubquery { expr, .. } => find_calls(expr, out),
        Expr::AnyOp { left, .. } | Expr::AllOp { left, .. } if is_subquery(e) => {
            find_calls(left, out)
        }
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            if let Some(o) = operand {
                find_calls(o, out);
            }
            for c in conditions {
                find_calls(&c.condition, out);
                find_calls(&c.result, out);
            }
            if let Some(x) = else_result {
                find_calls(x, out);
            }
        }
        // Any other expression's calls are those of the parts it reads: an IN list's, a BETWEEN's,
        // a LIKE's, an array's, a row's, a COLLATE's and the rest.
        e => {
            let mut parts = parts_of(e);
            if parts.is_empty() {
                parts = super::collate::children(e);
            }
            for p in parts {
                find_calls(p, out);
            }
        }
    }
}

impl<'q> Calls<'q> {
    /// Its set-returning calls by level, as PostgreSQL's planner splits them: the first those
    /// holding none among their arguments, each next those holding one of the level before.
    fn levels(&self) -> Vec<Vec<&'q Expr>> {
        let mut levels: Vec<Vec<&'q Expr>> = Vec::new();
        for &s in &self.sets {
            let at = depth(s, &self.stops);
            if levels.len() < at {
                levels.resize(at, Vec::new());
            }
            levels[at - 1].push(s);
        }
        levels
    }
}

/// An expression with its parentheses passed over.
fn unnested(e: &Expr) -> &Expr {
    match e {
        Expr::Nested(x) => unnested(x),
        e => e,
    }
}

/// Whether an expression is a set-returning call a step's select list makes, parentheses passed
/// over.
fn is_set_call(e: &Expr) -> bool {
    match e {
        Expr::Nested(x) => is_set_call(x),
        Expr::Function(f) => {
            f.over.is_none()
                && f.within_group.is_empty()
                && !is_aggregate(&function_name(f))
                && is_set_returning(&function_name(f))
        }
        _ => false,
    }
}

/// The level of an expression's deepest set-returning call: 0 where it calls none, 1 for a call
/// holding none among its arguments, and one more than the deepest among them for any other; the
/// calls inside `stops` none of its own.
fn depth(e: &Expr, stops: &[*const Expr]) -> usize {
    let mut inner = Calls {
        stops: stops.to_vec(),
        ..Calls::default()
    };
    match e {
        _ if stops.iter().any(|&s| std::ptr::eq(s, e)) => 0,
        Expr::Function(f) if is_set_call(e) => {
            if let FunctionArguments::List(l) = &f.args {
                for a in &l.args {
                    if let FunctionArg::Unnamed(FunctionArgExpr::Expr(x)) = a {
                        find_calls(x, &mut inner);
                    }
                }
            }
            1 + inner.sets.iter().map(|&s| depth(s, stops)).max().unwrap_or(0)
        }
        _ => {
            find_calls(e, &mut inner);
            inner.sets.iter().map(|&s| depth(s, stops)).max().unwrap_or(0)
        }
    }
}

/// The set-returning functions a step's select list reads.
const SET_RETURNING: [&str; 18] = [
    "unnest",
    "generate_series",
    "pg_prepared_statement",
    "range_minus_multi",
    "multirange_minus_multi",
    "generate_subscripts",
    "aclexplode",
    "pg_options_to_table",
    "pg_snapshot_xip",
    "txid_snapshot_xip",
    "pg_get_keywords",
    "regexp_matches",
    "regexp_split_to_table",
    "string_to_table",
    "pg_timezone_names",
    "pg_timezone_abbrevs_abbrevs",
    "pg_timezone_abbrevs_zone",
    "pg_timezone_abbrevs",
];

/// Whether a name is a set-returning function a step's select list reads.
pub(crate) fn is_set_returning(name: &str) -> bool {
    SET_RETURNING.contains(&name)
        || name == super::settings::ROWS
        || super::json::SETS.contains(&name)
        || super::tsearch::SETS.contains(&name)
        || super::handed::is_set_returning(name)
        || super::populate::SETS.contains(&name)
}

/// PostgreSQL's set-returning functions no step reads: refused where PostgreSQL refuses any set, in
/// its words for the clause.
const UNREAD_SETS: [&str; 57] = [
    "json_to_recordset",
    "jsonb_to_recordset",
    "pg_available_extension_versions",
    "pg_available_extensions",
    "pg_available_wal_summaries",
    "pg_config",
    "pg_cursor",
    "pg_event_trigger_ddl_commands",
    "pg_event_trigger_dropped_objects",
    "pg_extension_update_paths",
    "pg_get_aios",
    "pg_get_backend_memory_contexts",
    "pg_get_catalog_foreign_keys",
    "pg_get_loaded_modules",
    "pg_get_multixact_members",
    "pg_get_publication_tables",
    "pg_get_replication_slots",
    "pg_get_shmem_allocations",
    "pg_get_shmem_allocations_numa",
    "pg_get_wait_events",
    "pg_get_wal_resource_managers",
    "pg_hba_file_rules",
    "pg_ident_file_mappings",
    "pg_listening_channels",
    "pg_lock_status",
    "pg_logical_slot_get_binary_changes",
    "pg_logical_slot_get_changes",
    "pg_logical_slot_peek_binary_changes",
    "pg_logical_slot_peek_changes",
    "pg_ls_archive_statusdir",
    "pg_ls_dir",
    "pg_ls_logdir",
    "pg_ls_logicalmapdir",
    "pg_ls_logicalsnapdir",
    "pg_ls_replslotdir",
    "pg_ls_summariesdir",
    "pg_ls_tmpdir",
    "pg_ls_waldir",
    "pg_mcv_list_items",
    "pg_partition_ancestors",
    "pg_partition_tree",
    "pg_prepared_xact",
    "pg_show_all_file_settings",
    "pg_show_all_settings",
    "pg_show_replication_origin_status",
    "pg_stat_get_activity",
    "pg_stat_get_backend_idset",
    "pg_stat_get_backend_io",
    "pg_stat_get_io",
    "pg_stat_get_progress_info",
    "pg_stat_get_recovery_prefetch",
    "pg_stat_get_slru",
    "pg_stat_get_subscription",
    "pg_stat_get_wal_senders",
    "pg_tablespace_databases",
    "pg_wal_summary_contents",
    "ts_stat",
];

/// Whether a name is a set-returning function of PostgreSQL's, read or not.
fn returns_set(name: &str) -> bool {
    is_set_returning(name) || UNREAD_SETS.contains(&name)
}

/// The expressions an expression reads directly.
pub(super) fn parts_of(e: &Expr) -> Vec<&Expr> {
    match e {
        Expr::Function(f) => match &f.args {
            FunctionArguments::List(l) => l
                .args
                .iter()
                .filter_map(|a| match a {
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(x))
                    | FunctionArg::Named {
                        arg: FunctionArgExpr::Expr(x),
                        ..
                    } => Some(x),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        },
        Expr::BinaryOp { left, right, .. }
        | Expr::IsDistinctFrom(left, right)
        | Expr::IsNotDistinctFrom(left, right) => {
            vec![left, right]
        }
        Expr::UnaryOp { expr, .. }
        | Expr::Nested(expr)
        | Expr::Cast { expr, .. }
        | Expr::Collate { expr, .. }
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr)
        | Expr::IsTrue(expr)
        | Expr::IsNotTrue(expr)
        | Expr::IsFalse(expr)
        | Expr::IsNotFalse(expr)
        | Expr::IsUnknown(expr)
        | Expr::IsNotUnknown(expr) => vec![expr],
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => operand
            .iter()
            .map(|o| o.as_ref())
            .chain(conditions.iter().flat_map(|c| [&c.condition, &c.result]))
            .chain(else_result.iter().map(|x| x.as_ref()))
            .collect(),
        Expr::InList { expr, list, .. } => std::iter::once(expr.as_ref()).chain(list).collect(),
        Expr::Between {
            expr, low, high, ..
        } => vec![expr, low, high],
        Expr::Array(a) => a.elem.iter().collect(),
        Expr::Tuple(xs) => xs.iter().collect(),
        Expr::Like { expr, pattern, .. } | Expr::ILike { expr, pattern, .. } => vec![expr, pattern],
        // A subquery's own columns are its query's, read where it is answered.
        Expr::AnyOp { left, right, .. } | Expr::AllOp { left, right, .. }
            if matches!(right.as_ref(), Expr::Subquery(_)) =>
        {
            vec![left]
        }
        Expr::AnyOp { left, right, .. } | Expr::AllOp { left, right, .. } => vec![left, right],
        Expr::InSubquery { expr, .. } => vec![expr],
        Expr::CompoundFieldAccess { root, access_chain } => std::iter::once(root.as_ref())
            .chain(subscripts(access_chain))
            .collect(),
        _ => Vec::new(),
    }
}

/// The expressions an access chain's subscripts read: each index, and each slice's bounds.
pub(super) fn subscripts(chain: &[sqlparser::ast::AccessExpr]) -> Vec<&Expr> {
    use sqlparser::ast::{AccessExpr, Subscript};
    chain
        .iter()
        .flat_map(|a| match a {
            AccessExpr::Subscript(Subscript::Index { index }) => vec![index],
            AccessExpr::Subscript(Subscript::Slice {
                lower_bound,
                upper_bound,
                stride,
            }) => lower_bound
                .iter()
                .chain(upper_bound.iter())
                .chain(stride.iter())
                .collect(),
            AccessExpr::Dot(_) => Vec::new(),
        })
        .collect()
}

/// The columns an expression reads, by their qualifier and name, each as often as it is read.
/// The columns an expression reads, a parse tree handed to pg_get_expr apart: the one place a
/// catalog's tree is read.
pub(super) fn columns_read_but_trees(e: &Expr, out: &mut Vec<(Option<String>, String)>) {
    if let Expr::Function(f) = e {
        if function_name(f) == "pg_get_expr" {
            let a = args(f).unwrap_or_default();
            for (i, x) in a.iter().enumerate() {
                if i == 0 && column_of(x).is_some() {
                    continue;
                }
                columns_read_but_trees(x, out);
            }
            return;
        }
    }
    if let Some(c) = column_of(e) {
        out.push(c);
        return;
    }
    let mut parts = parts_of(e);
    if parts.is_empty() {
        parts = super::collate::children(e);
    }
    for p in parts {
        columns_read_but_trees(p, out);
    }
}

pub(super) fn columns_read(e: &Expr, out: &mut Vec<(Option<String>, String)>) {
    if let Some(c) = column_of(e) {
        out.push(c);
        return;
    }
    let mut parts = parts_of(e);
    if parts.is_empty() {
        parts = super::collate::children(e);
    }
    for p in parts {
        columns_read(p, out);
    }
}

/// The expressions a call sorts the rows it folds by: an aggregate's ORDER BY and WITHIN GROUP.
fn sorted_parts(f: &sqlparser::ast::Function) -> Vec<&Expr> {
    let mut out: Vec<&Expr> = Vec::new();
    if let FunctionArguments::List(l) = &f.args {
        for c in &l.clauses {
            if let sqlparser::ast::FunctionArgumentClause::OrderBy(o) = c {
                out.extend(o.iter().map(|x| &x.expr));
            }
        }
    }
    out.extend(f.within_group.iter().map(|o| &o.expr));
    out
}

/// Whether an expression calls a set-returning function of PostgreSQL's, a call's FILTER and the
/// expressions it sorts by among what it reads.
pub(super) fn holds_set(e: &Expr) -> bool {
    match e {
        Expr::Function(f) if f.over.is_none() && returns_set(&function_name(f)) => true,
        Expr::Function(f) => parts_of(e)
            .into_iter()
            .chain(f.filter.as_deref())
            .chain(sorted_parts(f))
            .any(holds_set),
        _ => {
            let mut parts = parts_of(e);
            if parts.is_empty() {
                parts = super::collate::children(e);
            }
            parts.into_iter().any(holds_set)
        }
    }
}

/// PostgreSQL's refusal of a set-returning function where a clause or a construct takes none.
fn not_allowed(what: &str) -> Error {
    Error::new(format!("set-returning functions are not allowed in {what}"))
}

/// A refusal with PostgreSQL's HINT naming a LATERAL FROM item, on a line of its own.
fn hinted(words: &str) -> Error {
    Error::new(format!(
        "{words}\nHINT:  You might be able to move the set-returning function into a LATERAL FROM \
         item."
    ))
}

/// PostgreSQL's refusal of a set where `construct` reads a truth or compares a value.
fn set_argument(construct: &str) -> Error {
    Error::new(format!("argument of {construct} must not return a set"))
}

/// A clause's expression refused as PostgreSQL's parser refuses a set-returning function in a
/// clause that takes none, `clause` its name for it.
pub(super) fn sets_refused(e: &Expr, clause: &str) -> Result<()> {
    match holds_set(e) {
        true => Err(not_allowed(clause)),
        false => Ok(()),
    }
}

/// A set-returning call where PostgreSQL's parser refuses one, refused in its words and in the
/// order it reads the expression, each part before what holds it: in a FILTER; among an
/// aggregate's or a window's arguments, in CASE or COALESCE, each with its HINT; where a truth is
/// read (AND, OR, NOT, a truth test, BETWEEN, a CASE's WHEN); compared as a row, or with a
/// subquery; in an IN compared alone, two or more items reading no column being one array.
fn sets_placed(e: &Expr) -> Result<()> {
    let each = |parts: Vec<&Expr>| parts.into_iter().try_for_each(sets_placed);
    let row_compared = || Err(Error::new("row comparison operator must not return a set"));
    match e {
        Expr::Function(f) => {
            let name = function_name(f);
            each(parts_of(e))?;
            if let Some(w) = f.filter.as_deref() {
                if holds_set(w) {
                    return Err(not_allowed("FILTER"));
                }
                sets_placed(w)?;
            }
            let sorted = sorted_parts(f);
            each(sorted.clone())?;
            let inner = parts_of(e).into_iter().chain(sorted).any(holds_set);
            if inner && f.over.is_some() {
                return Err(hinted(
                    "window function calls cannot contain set-returning function calls",
                ));
            }
            if inner && (is_aggregate(&name) || !f.within_group.is_empty()) {
                return Err(hinted(
                    "aggregate function calls cannot contain set-returning function calls",
                ));
            }
            if inner && name == "coalesce" {
                return Err(hinted(&not_allowed("COALESCE").to_string()));
            }
            // Its rows whole hold a plan cache's counts, which a snail has none of.
            if name == "pg_prepared_statement" && parts_of(e).is_empty() {
                return Err(Error::new(format!(
                    "pg_prepared_statement() in a select list gives its rows whole, {} among them, \
                     a plan cache's counts, and a snail has no plan cache: it is read in FROM, by \
                     its columns",
                    super::prepared::PLAN_CACHE.join(" and ")
                )));
            }
            Ok(())
        }
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            if let Some(o) = operand {
                sets_placed(o)?;
            }
            for c in conditions {
                sets_placed(&c.condition)?;
                if holds_set(&c.condition) {
                    return Err(set_argument("CASE/WHEN"));
                }
                sets_placed(&c.result)?;
            }
            if let Some(x) = else_result {
                sets_placed(x)?;
            }
            match parts_of(e).into_iter().any(holds_set) {
                true => Err(hinted(&not_allowed("CASE").to_string())),
                false => Ok(()),
            }
        }
        Expr::BinaryOp {
            left,
            op: op @ (BinaryOperator::And | BinaryOperator::Or),
            right,
        } => {
            let word = match op {
                BinaryOperator::And => "AND",
                _ => "OR",
            };
            for x in [left, right] {
                sets_placed(x)?;
                if holds_set(x) {
                    return Err(set_argument(word));
                }
            }
            Ok(())
        }
        Expr::UnaryOp {
            op: UnaryOperator::Not,
            expr: x,
        }
        | Expr::IsTrue(x)
        | Expr::IsNotTrue(x)
        | Expr::IsFalse(x)
        | Expr::IsNotFalse(x)
        | Expr::IsUnknown(x)
        | Expr::IsNotUnknown(x) => {
            sets_placed(x)?;
            let word = match e {
                Expr::IsTrue(_) => "IS TRUE",
                Expr::IsNotTrue(_) => "IS NOT TRUE",
                Expr::IsFalse(_) => "IS FALSE",
                Expr::IsNotFalse(_) => "IS NOT FALSE",
                Expr::IsUnknown(_) => "IS UNKNOWN",
                Expr::IsNotUnknown(_) => "IS NOT UNKNOWN",
                _ => "NOT",
            };
            match holds_set(x) {
                true => Err(set_argument(word)),
                false => Ok(()),
            }
        }
        // BETWEEN is read as two comparisons joined by AND, NOT BETWEEN by OR.
        Expr::Between {
            expr,
            negated,
            low,
            high,
        } => {
            let word = if *negated { "OR" } else { "AND" };
            sets_placed(expr)?;
            sets_placed(low)?;
            if holds_set(expr) || holds_set(low) {
                return Err(set_argument(word));
            }
            sets_placed(high)?;
            match holds_set(high) {
                true => Err(set_argument(word)),
                false => Ok(()),
            }
        }
        Expr::BinaryOp { left, op, right }
            if is_comparison(op) && row_parts(left).is_some() && row_parts(right).is_some() =>
        {
            sets_placed(left)?;
            sets_placed(right)?;
            match holds_set(left) || holds_set(right) {
                true => row_compared(),
                false => Ok(()),
            }
        }
        // A value compared with a subquery's rows is compared as a row.
        Expr::InSubquery { expr: x, .. }
        | Expr::AnyOp { left: x, .. }
        | Expr::AllOp { left: x, .. }
            if is_subquery(e) =>
        {
            sets_placed(x)?;
            match holds_set(x) {
                true => row_compared(),
                false => Ok(()),
            }
        }
        Expr::InList { expr, list, .. } => {
            sets_placed(expr)?;
            each(list.iter().collect())?;
            // Two or more items reading no column are one array, a row's apart; the rest are each
            // compared alone.
            let row = row_parts(expr).is_some();
            let free = list.iter().filter(|x| !reads_a_column(x)).count();
            for x in list {
                if free > 1 && !row && !reads_a_column(x) {
                    continue;
                }
                if holds_set(expr) || holds_set(x) {
                    return match row && row_parts(x).is_some() {
                        true => row_compared(),
                        false => Err(set_argument("IN")),
                    };
                }
            }
            Ok(())
        }
        _ => {
            let mut parts = parts_of(e);
            if parts.is_empty() {
                parts = super::collate::children(e);
            }
            each(parts)
        }
    }
}

/// The values a row's expressions see beyond its columns: each aggregate's and window's, by place.
pub(super) struct Computed<'q> {
    calls: HashMap<*const Expr, Value>,
    /// Where a function of the file's body is read: each of its parameters' values, read from
    /// the call's argument.
    bound: Option<&'q dyn Fn(&Expr) -> Option<Result<Value>>>,
    _q: std::marker::PhantomData<&'q ()>,
}

impl<'q> Computed<'q> {
    /// The value an aggregate or a window function computed, by its place.
    pub(super) fn of(&self, call: *const Expr) -> Option<Value> {
        self.calls.get(&call).cloned()
    }

    /// The same values, and each call's given beside them.
    fn and<'e>(&self, more: impl IntoIterator<Item = (&'e Expr, Value)>) -> Computed<'q> {
        let mut calls = self.calls.clone();
        calls.extend(more.into_iter().map(|(c, v)| (c as *const Expr, v)));
        Computed {
            calls,
            bound: self.bound,
            _q: std::marker::PhantomData,
        }
    }

    /// What a function's body sees: its parameters' values, as `bound` reads them.
    pub(super) fn bound_by(bound: &'q dyn Fn(&Expr) -> Option<Result<Value>>) -> Computed<'q> {
        Computed {
            calls: HashMap::new(),
            bound: Some(bound),
            _q: std::marker::PhantomData,
        }
    }
}

fn args(f: &sqlparser::ast::Function) -> Result<Vec<&Expr>> {
    // A built-in called by its parameters' names, its arguments put in their places.
    if super::named::is_named(f) {
        let release = crate::deparse::held().map_or(crate::regs::LATEST, |c| c.release);
        if let Some(args) = super::named::positional(f, release)? {
            return Ok(args);
        }
    }
    match &f.args {
        FunctionArguments::None => Ok(Vec::new()),
        FunctionArguments::List(l) => l
            .args
            .iter()
            .map(|a| match a {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(x)) => Ok(x),
                FunctionArg::Unnamed(FunctionArgExpr::Wildcard) => Err(Error::new("*")),
                _ => Err(Error::new(format!("{f}: its arguments are not read"))),
            })
            .collect(),
        _ => Err(Error::new(format!("{f}: its arguments are not read"))),
    }
}

fn is_count_star(f: &sqlparser::ast::Function) -> bool {
    matches!(&f.args, FunctionArguments::List(l)
        if matches!(l.args.as_slice(), [FunctionArg::Unnamed(FunctionArgExpr::Wildcard)]))
}

/// A number written out: an integer where it is digits alone and fits a bigint, else a numeric as
/// `numeric_in` reads it.
fn numeric_literal(s: &str) -> Result<Value> {
    let t = s.trim();
    let digits = t.strip_prefix(['-', '+']).unwrap_or(t);
    if !digits.is_empty() && digits.bytes().all(|c| c.is_ascii_digit()) {
        if let Ok(v) = t.parse::<i64>() {
            return Ok(Value::Int(v));
        }
    }
    Ok(Value::of_numeric(Numeric::parse(s)?))
}

/// A numeric's typmod, as a cast names it; none where it names none.
fn cast_typmod(to: &DataType) -> Result<Option<(u32, i32)>> {
    Ok(crate::catalog::held_type(to)?.numeric_typmod().flatten())
}

/// Whether an expression is a quoted literal or a parameter, given as text: of unknown type, which
/// a cast reads by the target's input and an operator by the type it takes on that side, as
/// PostgreSQL's parser types each.
pub(super) fn quoted(e: &Expr) -> bool {
    match e {
        Expr::Value(v) => matches!(
            v.value,
            SqlValue::SingleQuotedString(_) | SqlValue::Placeholder(_)
        ),
        Expr::Nested(e) => quoted(e),
        _ => false,
    }
}

/// Whether an expression is a string constant, written in any of PostgreSQL's quotings.
fn string_constant(e: &Expr) -> bool {
    match e {
        Expr::Value(v) => matches!(
            v.value,
            SqlValue::SingleQuotedString(_)
                | SqlValue::EscapedStringLiteral(_)
                | SqlValue::UnicodeStringLiteral(_)
                | SqlValue::DollarQuotedString(_)
        ),
        Expr::Nested(e) => string_constant(e),
        _ => false,
    }
}

fn cast(
    v: Value,
    to: &DataType,
    release: u8,
    monetary: crate::monetary::Monetary,
    literal: bool,
) -> Result<Value> {
    // A domain of the file's: its base type's cast, then its NOT NULL and its CHECKs, those of the
    // domain it is made from first.
    if let Some((dom, _)) = super::handed::domain_named(to) {
        let base = crate::ddl::parse_type(&dom.base)?;
        let v = cast(v, &base, release, monetary, literal)?;
        if dom.not_null && v.is_null() {
            return Err(Error::new(format!(
                "domain {} does not allow null values",
                dom.name
            )));
        }
        for k in &dom.checks {
            let held = eval_on(
                &k.check,
                &["value".to_string()],
                std::slice::from_ref(&v),
                release,
                &Casings {
                    monetary,
                    ..Casings::default()
                },
            )?;
            if matches!(held, Value::Bool(false)) {
                return Err(Error::new(format!(
                    "value for domain {} violates check constraint \"{}\"",
                    dom.name, k.name
                )));
            }
        }
        return Ok(v);
    }
    if v.is_null() {
        return Ok(Value::Null);
    }
    let name = to.to_string().to_lowercase();
    let base = name.split('(').next().unwrap_or("").trim().to_string();
    // A vector is cast to an array or a string, and to nothing else.
    if let Value::Composed(from, _) = &v {
        let to_string = matches!(crate::catalog::held_type(to), Ok(t) if t.is_string());
        if crate::composed::is_vector(*from) && !to_string && crate::composed::resolve(to).is_none() {
            return Err(Error::new(format!(
                "cannot cast type {} to {}",
                from.pg_name(),
                crate::catalog::format_type(to)
            )));
        }
    }
    // A geometric type by its cast from another, or by its input from text.
    if let Some(k) = crate::geo::GeoKind::of_name(base.as_str()) {
        return Ok(match v {
            Value::Geometric(g) => match crate::geo::geo_cast(&g, k) {
                Some(c) => Value::Geometric(c?),
                None => {
                    return Err(Error::new(format!(
                        "cannot cast type {} to {}",
                        g.kind().name(),
                        k.name()
                    )))
                }
            },
            Value::Text(t) => Value::Geometric(crate::geo::geo_in(k, &t)?),
            other => {
                return Err(Error::new(format!(
                    "cannot cast type {} to {}",
                    kind_name(&other),
                    k.name()
                )))
            }
        });
    }
    // A jsonb's number or boolean at its top, as its casts take one; a JSON null, NULL.
    if let Value::Typed(ColumnType::Textual(crate::textual::Textual::Jsonb), s) = &v {
        let target = match base.as_str() {
            "smallint" | "int2" => Some("smallint"),
            "integer" | "int" | "int4" => Some("integer"),
            "bigint" | "int8" => Some("bigint"),
            "numeric" | "decimal" => Some("numeric"),
            "real" | "float4" => Some("real"),
            "double precision" | "float8" => Some("double precision"),
            _ => None,
        };
        if let Some(target) = target {
            let Some(n) = crate::jsonb::jsonb_number(s, target)? else {
                return Ok(Value::Null);
            };
            return Ok(match target {
                "smallint" | "integer" | "bigint" => {
                    Value::Int(n.to_integer(i64::MIN, i64::MAX, target)?)
                }
                "real" => Value::Real(n.to_float(false)? as f32),
                "double precision" => Value::Double(n.to_float(true)?),
                _ => Value::of_numeric(match cast_typmod(to)? {
                    Some((p, s)) => n.typmod(p, s)?,
                    None => n,
                }),
            });
        }
        if matches!(base.as_str(), "boolean" | "bool") {
            return Ok(crate::jsonb::jsonb_bool(s)?.map_or(Value::Null, Value::Bool));
        }
    }
    Ok(match base.as_str() {
        // A bpchar's trailing blanks are taken off where it becomes text.
        // varchar(n) cut to its n characters, as an explicit cast cuts it.
        "text" | "varchar" | "character varying" => {
            let t = match v {
                Value::Str(ty, s) => crate::strings::text_of(ty, &s).to_string(),
                v => v.text().unwrap(),
            };
            match crate::catalog::held_type(to) {
                Ok(ColumnType::Text(Some(n))) => Value::Text(t.chars().take(n as usize).collect()),
                _ => Value::Text(t),
            }
        }
        "bigint" | "int8" | "integer" | "int" | "int4" | "smallint" | "int2" => match v {
            Value::Real(f) => float_integer(f64::from(f), &base)?,
            Value::Double(d) => float_integer(d, &base)?,
            Value::Scalar(ty, k) => scalar_integer(ty, k, &base)?,
            Value::Bits(_, k) if matches!(base.as_str(), "integer" | "int" | "int4") => {
                bits_integer(&k, 32)?
            }
            Value::Bits(_, k) if matches!(base.as_str(), "bigint" | "int8") => {
                bits_integer(&k, 64)?
            }
            // Held to the type's range, as int84, int82 and int42 refuse what passes it.
            Value::Int(x) => {
                let (name, lo, hi) = integer_range(&base);
                if !(lo..=hi).contains(&x) {
                    return Err(Error::new(format!("{name} out of range")));
                }
                Value::Int(x)
            }
            v @ (Value::Num(..) | Value::Numeric(_)) => {
                let (name, lo, hi) = integer_range(&base);
                Value::Int(v.to_numeric().unwrap().to_integer(lo, hi, name)?)
            }
            // Read by the type's input, which refuses a number past its range, and in its words
            // anything else that is not an integer.
            Value::Text(t) => {
                let (name, lo, hi) = integer_range(&base);
                let digits = t.trim().trim_start_matches(['+', '-']);
                match numeric_literal(&t) {
                    Ok(Value::Int(x)) if (lo..=hi).contains(&x) => Value::Int(x),
                    Ok(_) if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) => {
                        return Err(Error::new(format!(
                            "value \"{t}\" is out of range for type {name}"
                        )))
                    }
                    _ => {
                        return Err(Error::new(format!(
                            "invalid input syntax for type {name}: \"{t}\""
                        )))
                    }
                }
            }
            // As int4(boolean): true 1, false 0; to no other integer.
            Value::Bool(b) if matches!(base.as_str(), "integer" | "int" | "int4") => {
                Value::Int(i64::from(b))
            }
            other => {
                return Err(Error::new(format!(
                    "cannot cast type {} to {}",
                    kind_name(&other),
                    integer_range(&base).0
                )))
            }
        },
        "numeric" | "decimal" => {
            let n = match v {
                Value::Text(t) => Numeric::parse(&t)?,
                Value::Scalar(ColumnType::Money(m), k) => crate::monetary::to_numeric(k, m.conv()),
                Value::Real(f) => Numeric::of_float(f64::from(f), false),
                Value::Double(d) => Numeric::of_float(d, true),
                v => match v.to_numeric() {
                    Some(n) => n,
                    None => return Err(Error::new(format!("{v:?} cast to numeric"))),
                },
            };
            Value::of_numeric(match cast_typmod(to)? {
                Some((p, s)) => n.typmod(p, s)?,
                None => n,
            })
        }
        "bytea" => match v {
            Value::Bytes(b) => Value::Bytes(b),
            Value::Text(t) => Value::Bytes(crate::strings::bytea_in(&t)?),
            Value::Uuid(k) => Value::Bytes(crate::scalars::uuid_bytes(k).to_vec()),
            other => return Err(Error::new(format!("{other:?} cast to bytea"))),
        },
        "boolean" | "bool" => match v {
            Value::Bool(b) => Value::Bool(b),
            Value::Text(t) => boolean_of(&t)?,
            // As bool(integer): zero false, any other true; from no other integer.
            Value::Int(x) if i32::try_from(x).is_ok() => Value::Bool(x != 0),
            other => {
                return Err(Error::new(format!(
                    "cannot cast type {} to boolean",
                    kind_name(&other)
                )))
            }
        },
        "uuid" => match v {
            Value::Uuid(k) => Value::Uuid(k),
            Value::Text(t) => Value::Uuid(crate::scalars::uuid_parse(&t)?),
            // PostgreSQL 19 casts a uuid's 16 bytes to one, and back.
            Value::Bytes(b) => Value::Uuid(crate::scalars::uuid_key(
                b.as_slice().try_into().map_err(|_| {
                    Error::new(format!(
                        "invalid input length for type uuid: expected 16 bytes, got {}",
                        b.len()
                    ))
                })?,
            )),
            other => return Err(Error::new(format!("{other:?} cast to uuid"))),
        },
        _ if crate::composed::resolve(to).is_some() => {
            let ty = crate::composed::resolve(to)
                .unwrap()
                .with_monetary(monetary);
            match v {
                Value::Composed(from, c) if from == ty => Value::Composed(from, c),
                Value::Text(t) => composed_value(ty, &t)?,
                // An array of one type to an array of another, each element cast, its shape kept;
                // a vector is cast from, and to no other.
                Value::Composed(from, c)
                    if super::typed::element_of(from).is_some()
                        && super::typed::element_of(ty).is_some()
                        && !crate::composed::is_vector(ty) =>
                {
                    let (from_element, element) = (
                        super::typed::element_of(from).unwrap(),
                        super::typed::element_of(ty).unwrap(),
                    );
                    let crate::composed::Item::Array {
                        elements,
                        dims,
                        lower,
                    } = &c.item
                    else {
                        return Ok(Value::Null);
                    };
                    let elements = elements
                        .iter()
                        .map(|p| {
                            let v = cast(
                                part_value(from_element, p),
                                innermost(to),
                                release,
                                monetary,
                                false,
                            )?;
                            part_of(element, v, false)
                        })
                        .collect::<Result<Vec<_>>>()
                        .map_err(|e| {
                            if crate::composed::is_vector(from) {
                                Error::new(format!(
                                    "cannot cast type {} to {}",
                                    from.pg_name(),
                                    ty.pg_name()
                                ))
                            } else {
                                e
                            }
                        })?;
                    // A cast of no elements leaves an empty array, of no dimensions, but where a
                    // vector keeps its elements' type.
                    let (dims, lower) = if elements.is_empty() && from_element != element {
                        (Vec::new(), Vec::new())
                    } else {
                        (dims.clone(), lower.clone())
                    };
                    composed(
                        ty,
                        crate::composed::Item::Array {
                            elements,
                            dims,
                            lower,
                        },
                    )
                }
                other => {
                    return Err(Error::new(format!(
                        "cannot cast type {} to {}",
                        kind_name(&other),
                        ty.pg_name()
                    )))
                }
            }
        }
        _ if super::prng::is_void(to) => super::prng::to_void(v)?,
        _ => match crate::catalog::held_type(to) {
            // text to regclass is regclass(text), which reads a name alone; a literal is read by
            // regclass's input.
            Ok(ColumnType::Reg(crate::regs::RegKind::Class, _))
                if !literal && matches!(v, Value::Text(_)) =>
            {
                let Value::Text(t) = &v else { unreachable!() };
                let ty = ColumnType::Reg(crate::regs::RegKind::Class, release);
                Value::Scalar(
                    ty,
                    crate::scalars::unsigned_key(
                        u64::from(crate::regs::class_of_text(t, release)?),
                        32,
                    ),
                )
            }
            Ok(ColumnType::Reg(kind, _)) => {
                crate::regs::check_release(Some(kind), release)?;
                scalar_cast(v, ColumnType::Reg(kind, release))?
            }
            Ok(ty) if super::clock::is_time(ty) => super::clock::cast(v, ty)?,
            Ok(ty @ (ColumnType::Textual(_) | ColumnType::Internal(_))) => typed_cast(v, ty)?,
            Ok(ty) if crate::scalars::is_scalar(ty) => scalar_cast(v, ty.with_monetary(monetary))?,
            Ok(ty @ (ColumnType::Bpchar(_) | ColumnType::Name)) => string_cast(v, ty, true)?,
            Ok(ty) if ty.is_bits() => bits_cast(v, ty, true)?,
            Ok(ty @ (ColumnType::Float4 | ColumnType::Float8)) => {
                float_cast(v, ty == ColumnType::Float8)?
            }
            _ => {
                // A type that does not exist, refused as PostgreSQL refuses it: its schema first.
                if let DataType::Custom(..) = to {
                    crate::regs::type_oid(&to.to_string(), release)?;
                }
                return Err(Error::new(format!("a cast to {to} is not read")));
            }
        },
    })
}

/// A value cast to a type held as its text, as PostgreSQL's casts take each kind: a string's text
/// read by the type's input; a value of a type it casts from (json and jsonb, inet and cidr) by
/// that cast. A node tree and the others only a server makes are never cast to.
pub(crate) fn typed_cast(v: Value, ty: ColumnType) -> Result<Value> {
    let ColumnType::Textual(to) = ty else {
        let ColumnType::Internal(t) = ty else {
            unreachable!("a type held as its text")
        };
        return match v {
            Value::Null => Ok(Value::Null),
            Value::Typed(from, s) if from == ty => Ok(Value::Typed(from, s)),
            _ => Err(t.refusal()),
        };
    };
    Ok(match v {
        Value::Null => Value::Null,
        Value::Typed(ColumnType::Textual(from), s) => {
            Value::Typed(ty, crate::textual::cast(from, to, &s)?)
        }
        v @ (Value::Text(_) | Value::Str(..)) => {
            let s = match &v {
                Value::Str(t, s) => crate::strings::text_of(*t, s).to_string(),
                v => v.text().unwrap_or_default(),
            };
            Value::Typed(ty, crate::textual::text_in(to, &s)?)
        }
        other => {
            return Err(Error::new(format!(
                "cannot cast type {} to {}",
                kind_name(&other),
                ty.pg_name()
            )))
        }
    })
}

/// A value cast to bpchar or name, as PostgreSQL's casts take each kind: a string's text, a bpchar's
/// without its trailing blanks into a name, held to the type's length, cut where `explicit`; any
/// other value's text output, as its cast to a string type through its output takes it.
pub(crate) fn string_cast(v: Value, ty: ColumnType, explicit: bool) -> Result<Value> {
    let s = match &v {
        Value::Null => return Ok(Value::Null),
        Value::Str(from, s) if ty == ColumnType::Name => {
            crate::strings::text_of(*from, s).to_string()
        }
        other => other.text().unwrap_or_default(),
    };
    Ok(match ty {
        ColumnType::Bpchar(n) => Value::Str(ty, crate::strings::bpchar_in(&s, n, explicit)?),
        _ => Value::Str(ColumnType::Name, crate::strings::name_in(&s)),
    })
}

/// A value cast to a bit string of type `ty`, as PostgreSQL's casts take each kind: a bit string
/// cut or padded to bit(n)'s length and cut to bit varying(n)'s where `explicit`, refused otherwise;
/// text read by its input; an integer's bits, the rightmost n of its two's complement, into bit(n).
pub(crate) fn bits_cast(v: Value, ty: ColumnType, explicit: bool) -> Result<Value> {
    let (n, varying) = match ty {
        ColumnType::Bit(n) => (Some(n), false),
        ColumnType::Varbit(n) => (n, true),
        _ => unreachable!("a bit string's type"),
    };
    Ok(match v {
        Value::Null => Value::Null,
        Value::Bits(_, k) => Value::Bits(ty, crate::strings::bits_cast(&k, n, varying, explicit)?),
        Value::Text(t) => Value::Bits(ty, crate::strings::bits_parse(&t, n, varying)?),
        Value::Int(x) if !varying && explicit => {
            let n = n.unwrap_or(1);
            let wide = if i32::try_from(x).is_ok() { 32 } else { 64 };
            let mut bits = vec![0u8; (n as usize).div_ceil(8)];
            for i in 0..n {
                let from_right = n - 1 - i;
                let bit = if from_right >= wide {
                    x < 0
                } else {
                    (x >> from_right) & 1 == 1
                };
                if bit {
                    bits[i as usize / 8] |= 0x80 >> (i % 8);
                }
            }
            Value::Bits(ty, crate::strings::bits_key(n, &bits))
        }
        other => {
            return Err(Error::new(format!(
                "cannot cast type {} to {}",
                kind_name(&other),
                ty.pg_name()
            )))
        }
    })
}

/// A bit string cast to an integer of `bits` bits, as `bittoint4` and `bittoint8` take it: its
/// bits as an unsigned number, refused past the integer's width.
fn bits_integer(k: &[u8], bits: u32) -> Result<Value> {
    let (len, b) = crate::strings::bits_of_key(k);
    if len > bits {
        return Err(Error::new(if bits == 32 {
            "integer out of range"
        } else {
            "bigint out of range"
        }));
    }
    let mut v: u64 = 0;
    for x in &b {
        v = v << 8 | u64::from(*x);
    }
    v >>= b.len() as u32 * 8 - len;
    Ok(Value::Int(if bits == 32 {
        i64::from(v as u32 as i32)
    } else {
        v as i64
    }))
}

/// The name PostgreSQL gives a value's type: an integer's by its range.
pub(super) fn kind_name(v: &Value) -> String {
    match v {
        Value::Null => "unknown".into(),
        Value::Int(x) if i32::try_from(*x).is_ok() => "integer".into(),
        Value::Int(_) => "bigint".into(),
        Value::Num(..) | Value::Numeric(_) => "numeric".into(),
        Value::Text(_) => "text".into(),
        Value::Bytes(_) => "bytea".into(),
        Value::Bool(_) => "boolean".into(),
        Value::Uuid(_) => "uuid".into(),
        Value::Scalar(ty, _) => ty.pg_name(),
        Value::Real(_) => "real".into(),
        Value::Double(_) => "double precision".into(),
        Value::Str(ColumnType::Name, _) => "name".into(),
        Value::Str(..) => "character".into(),
        Value::Bits(ColumnType::Bit(_), _) => "bit".into(),
        Value::Bits(..) => "bit varying".into(),
        Value::Composed(ty, _) => ty.pg_name(),
        Value::Interval(_) => "interval".into(),
        Value::Typed(ty, _) => ty.pg_name(),
        Value::Geometric(g) => g.kind().name().into(),
    }
}

/// The type a value is of, where it says: an integer's the narrowest of integer and bigint that
/// holds it; none for NULL.
pub(super) fn type_of_value(v: &Value) -> Option<ColumnType> {
    Some(match v {
        Value::Null => return None,
        Value::Int(x) if i32::try_from(*x).is_ok() => ColumnType::Int4,
        Value::Int(_) => ColumnType::Int8,
        Value::Num(..) | Value::Numeric(_) => super::typed::NUMERIC,
        Value::Text(_) => ColumnType::Text(None),
        Value::Bytes(_) => ColumnType::Bytes,
        Value::Bool(_) => ColumnType::Boolean,
        Value::Uuid(_) => ColumnType::Uuid,
        Value::Real(_) => ColumnType::Float4,
        Value::Double(_) => ColumnType::Float8,
        Value::Scalar(ty, _)
        | Value::Str(ty, _)
        | Value::Bits(ty, _)
        | Value::Composed(ty, _)
        | Value::Typed(ty, _) => *ty,
        Value::Interval(_) => super::calls::full_interval(),
        Value::Geometric(g) => ColumnType::Geometric(g.kind()),
    })
}

/// An integer type's name, as PostgreSQL's errors write it, and its range, by the name a cast writes.
fn integer_range(base: &str) -> (&'static str, i64, i64) {
    match base {
        "smallint" | "int2" => ("smallint", i64::from(i16::MIN), i64::from(i16::MAX)),
        "integer" | "int" | "int4" => ("integer", i64::from(i32::MIN), i64::from(i32::MAX)),
        _ => ("bigint", i64::MIN, i64::MAX),
    }
}

/// A number rounded to whole units of `to` places from `from`, half away from zero.
fn rounded(m: i128, from: u32, to: u32) -> i128 {
    if to >= from {
        return m * 10i128.pow(to - from);
    }
    let d = 10i128.pow(from - to);
    let (q, r) = (m / d, (m % d).abs());
    if r * 2 >= d {
        q + m.signum()
    } else {
        q
    }
}

/// A value cast to a fixed scalar, as PostgreSQL's casts to it take each kind: text read by the
/// type's input; an integer to an oid with its bits, a bigint held to an oid's range; an integer or a
/// numeric to money in cents; an integer to a `"char"` held to a byte's signed range; an xid8 to its
/// xid.
fn scalar_cast(v: Value, ty: ColumnType) -> Result<Value> {
    use crate::scalars::{scalar_parse, unsigned_key, unsigned_of};
    let no = |v: &Value| {
        Error::new(format!(
            "cannot cast type {} to {}",
            kind_name(v),
            ty.pg_name()
        ))
    };
    let key = match (&v, ty) {
        (Value::Null, _) => return Ok(Value::Null),
        (Value::Scalar(from, k), _) if *from == ty => *k,
        // An oid and the reg* types are binary-coercible, as are a function's two and an operator's
        // two to each other.
        (Value::Scalar(ColumnType::Reg(a, _), k), ColumnType::Reg(b, _))
            if crate::regs::casts_between(*a, b) =>
        {
            *k
        }
        (Value::Scalar(ColumnType::Oid, k), ColumnType::Reg(..))
        | (Value::Scalar(ColumnType::Reg(..), k), ColumnType::Oid) => *k,
        (Value::Scalar(ColumnType::Oid, k), ColumnType::Oid8) => {
            unsigned_key(unsigned_of(*k, 32), 64)
        }
        // int8 and oid8 are binary-coercible; a smaller integer comes through int8.
        (Value::Int(x), ColumnType::Oid8) => unsigned_key(*x as u64, 64),
        (
            Value::Scalar(from @ (ColumnType::Macaddr | ColumnType::Macaddr8), k),
            ColumnType::Macaddr | ColumnType::Macaddr8,
        ) => crate::scalars::mac_cast(*from, ty, *k)?,
        (Value::Scalar(ColumnType::Xid8, k), ColumnType::Xid) => {
            unsigned_key(u64::from(unsigned_of(*k, 64) as u32), 32)
        }
        (Value::Text(t), _) => scalar_parse(ty, t)?,
        (Value::Int(x), ColumnType::Oid | ColumnType::Reg(..)) => {
            let u = match i32::try_from(*x) {
                Ok(i) => i as u32,
                Err(_) => u32::try_from(*x).map_err(|_| Error::new("OID out of range"))?,
            };
            unsigned_key(u64::from(u), 32)
        }
        (Value::Int(x), ColumnType::Money(m)) => crate::monetary::of_integer(*x, m.conv())?,
        // numeric_cash: its count, as numeric_int8 rounds it.
        (Value::Num(n, s), ColumnType::Money(m)) => {
            crate::monetary::of_numeric(&Numeric::of_units(*n, *s), m.conv())?
        }
        (Value::Numeric(n), ColumnType::Money(m)) => crate::monetary::of_numeric(n, m.conv())?,
        (Value::Int(x), ColumnType::Char) if i32::try_from(*x).is_ok() => {
            let b = i8::try_from(*x).map_err(|_| Error::new("\"char\" out of range"))?;
            unsigned_key(u64::from(b as u8), 8)
        }
        _ => return Err(no(&v)),
    };
    Ok(Value::Scalar(ty, key))
}

/// A fixed scalar cast to an integer type named `base`: an oid's bits as an integer, or as a
/// bigint unsigned; a `"char"`'s byte, signed, as an integer.
fn scalar_integer(ty: ColumnType, k: i64, base: &str) -> Result<Value> {
    use crate::scalars::unsigned_of;
    let int4 = matches!(base, "integer" | "int" | "int4");
    let int8 = matches!(base, "bigint" | "int8");
    Ok(Value::Int(match ty {
        ty if ty.is_oid() && int4 => i64::from(unsigned_of(k, 32) as u32 as i32),
        ty if ty.is_oid() && int8 => unsigned_of(k, 32) as i64,
        ColumnType::Oid8 if int8 => unsigned_of(k, 64) as i64,
        ColumnType::Char if int4 => i64::from(unsigned_of(k, 8) as u8 as i8),
        _ => {
            return Err(Error::new(format!(
                "cannot cast type {} to {base}",
                ty.pg_name()
            )))
        }
    }))
}

/// An integer beside a scalar it is compared with, as PostgreSQL takes it: cast to an oid, or to an
/// xid where it is an integer, as `xideqint4` reads one.
fn integer_beside(scalar: &Value, n: Value) -> Result<Value> {
    match scalar {
        Value::Scalar(ty, _) if ty.is_oid() => scalar_cast(n, ColumnType::Oid),
        Value::Scalar(ColumnType::Oid8, _) => scalar_cast(n, ColumnType::Oid8),
        Value::Scalar(ColumnType::Xid, _) if matches!(n, Value::Int(x) if i32::try_from(x).is_ok()) =>
        {
            let Value::Int(x) = n else { unreachable!() };
            Ok(Value::Scalar(
                ColumnType::Xid,
                crate::scalars::unsigned_key(u64::from(x as i32 as u32), 32),
            ))
        }
        _ => Ok(n),
    }
}

/// The comparisons a scalar's type has: xid only `=` and `<>`, cid only `=`, the rest all six.
fn compared(op: &BinaryOperator, x: &Value, y: &Value) -> Result<()> {
    // An array of a type with no B-tree is compared by = and <> alone: where both are values.
    if let (Value::Composed(ty, _), _) | (_, Value::Composed(ty, _)) = (x, y) {
        let both = !x.is_null() && !y.is_null();
        let equal = matches!(op, BinaryOperator::Eq | BinaryOperator::NotEq);
        // A composite is refused only where the two reach a field of no B-tree.
        if let (Value::Composed(ty, a), Value::Composed(_, b)) = (x, y) {
            if is_record(*ty) && !equal {
                return match crate::composed::compare_reaches(*ty, &a.item, &b.item) {
                    Some(part) => Err(Error::new(format!(
                        "could not identify a comparison function for type {}",
                        part.pg_name()
                    ))),
                    None => Ok(()),
                };
            }
        }
        if !ty.ordered() && both && !equal {
            return Err(Error::new(format!(
                "could not identify a comparison function for type {}",
                ty.pg_name()
            )));
        }
        return Ok(());
    }
    let ty = match (x, y) {
        (Value::Scalar(ty, _), _) | (_, Value::Scalar(ty, _)) => *ty,
        _ => return Ok(()),
    };
    let ok = match ty {
        ColumnType::Xid => matches!(op, BinaryOperator::Eq | BinaryOperator::NotEq),
        ColumnType::Cid => matches!(op, BinaryOperator::Eq),
        _ => true,
    };
    if ok {
        Ok(())
    } else {
        Err(Error::new(format!(
            "operator does not exist: {} {op} {}{NO_OPERATOR}",
            kind_name(x),
            kind_name(y)
        )))
    }
}

/// Whether comparing two composed values asks their parts' equality: always, but of two arrays of
/// different shapes, their dimensions or lower bounds apart, which PostgreSQL's `array_eq` finds
/// unequal before it asks.
fn asks_parts(a: &crate::composed::Item, b: &crate::composed::Item) -> bool {
    use crate::composed::Item;
    match (a, b) {
        (
            Item::Array {
                dims: d1,
                lower: l1,
                ..
            },
            Item::Array {
                dims: d2,
                lower: l2,
                ..
            },
        ) => d1 == d2 && l1 == l2,
        _ => true,
    }
}

/// An operator on a fixed scalar, as PostgreSQL defines it: money added, subtracted, multiplied by
/// an integer and divided by one; a pg_lsn less another, a numeric of bytes, and a numeric of bytes
/// added to one. None where neither side is a scalar.
fn scalar_arith(op: &BinaryOperator, x: &Value, y: &Value) -> Result<Option<Value>> {
    use crate::scalars::{unsigned_key, unsigned_of};
    use BinaryOperator as B;
    use ColumnType::{Money, PgLsn};
    if !matches!(x, Value::Scalar(..)) && !matches!(y, Value::Scalar(..)) {
        return Ok(None);
    }
    if x.is_null() || y.is_null() {
        return Ok(Some(Value::Null));
    }
    let money = |m, r: Option<i64>| {
        r.map(|v| Value::Scalar(Money(m), v))
            .ok_or_else(|| Error::new("money out of range"))
    };
    let lsn_plus = |l: i64, n: &Value, sign: i128| -> Result<Value> {
        let (m, s) = n.numeric().unwrap();
        let total = i128::from(unsigned_of(l, 64)) * 10i128.pow(s) + sign * m;
        let bytes = rounded(total, s, 0);
        let u = u64::try_from(bytes).map_err(|_| Error::new("pg_lsn out of range"))?;
        Ok(Value::Scalar(PgLsn, unsigned_key(u, 64)))
    };
    Ok(Some(match (op, x, y) {
        (B::Plus, Value::Scalar(Money(m), a), Value::Scalar(Money(_), b)) => {
            money(*m, a.checked_add(*b))?
        }
        (B::Minus, Value::Scalar(Money(m), a), Value::Scalar(Money(_), b)) => {
            money(*m, a.checked_sub(*b))?
        }
        (B::Multiply, Value::Scalar(Money(m), a), Value::Int(b))
        | (B::Multiply, Value::Int(b), Value::Scalar(Money(m), a)) => money(*m, a.checked_mul(*b))?,
        (B::Divide, Value::Scalar(Money(m), a), Value::Int(b)) => {
            if *b == 0 {
                return Err(Error::new("division by zero"));
            }
            money(*m, a.checked_div(*b))?
        }
        (B::BitwiseAnd | B::BitwiseOr, Value::Scalar(t, a), Value::Scalar(u, b))
            if t == u && t.fixed_bytes().is_some() =>
        {
            let bits = t.unsigned_bits();
            let (a, b) = (unsigned_of(*a, bits), unsigned_of(*b, bits));
            let v = if matches!(op, B::BitwiseAnd) {
                a & b
            } else {
                a | b
            };
            Value::Scalar(*t, unsigned_key(v, bits))
        }
        (B::Minus, Value::Scalar(PgLsn, a), Value::Scalar(PgLsn, b)) => Value::Num(
            i128::from(unsigned_of(*a, 64)) - i128::from(unsigned_of(*b, 64)),
            0,
        ),
        (B::Plus, Value::Scalar(PgLsn, l), n @ (Value::Int(_) | Value::Num(..)))
        | (B::Plus, n @ (Value::Int(_) | Value::Num(..)), Value::Scalar(PgLsn, l)) => {
            lsn_plus(*l, n, 1)?
        }
        (B::Minus, Value::Scalar(PgLsn, l), n @ (Value::Int(_) | Value::Num(..))) => {
            lsn_plus(*l, n, -1)?
        }
        _ => {
            return Err(Error::new(format!(
                "operator does not exist: {} {op} {}{NO_OPERATOR}",
                kind_name(x),
                kind_name(y)
            )))
        }
    }))
}

/// A value as text where it meets text in `||`: a string's own text, a bpchar's without its
/// trailing blanks, any other value as its type's output writes it.
fn text_part(v: &Value) -> String {
    match v {
        Value::Str(ty, s) => crate::strings::text_of(*ty, s).to_string(),
        other => other.text().unwrap_or_default(),
    }
}

/// A geometric operator's answer as a value: a geometric value, a truth, a double precision, an
/// integer or NULL.
fn of_geo(v: crate::geo::GeoValue) -> Value {
    use crate::geo::GeoValue as G;
    match v {
        G::Geo(g) => Value::Geometric(g),
        G::Bool(b) => Value::Bool(b),
        G::Float(f) => Value::Double(f),
        G::Int(i) => Value::Int(i64::from(i)),
        _ => Value::Null,
    }
}

/// An operator on geometric values, as PostgreSQL defines it on their types; a quoted literal
/// beside one read as a value of its type. None where neither side is geometric.
fn geo_op(
    op: &BinaryOperator,
    x: &Value,
    y: &Value,
    left: &Expr,
    right: &Expr,
    release: u8,
) -> Result<Option<Value>> {
    let read = |v: &Value, e: &Expr, beside: &Value| -> Result<Value> {
        Ok(match (v, beside) {
            (Value::Text(t), Value::Geometric(g)) if quoted(e) => {
                Value::Geometric(crate::geo::geo_in(g.kind(), t)?)
            }
            (v, _) => v.clone(),
        })
    };
    let name = op.to_string();
    if !matches!(x, Value::Geometric(_)) && !matches!(y, Value::Geometric(_)) {
        // An operator only geometric types have, over a NULL, is NULL.
        return Ok((matches!(
            name.as_str(),
            "~=" | "<^"
                | ">^"
                | "?||"
                | "?-|"
                | "?#"
                | "##"
                | "<->"
                | "&<|"
                | "|&>"
                | "<<|"
                | "|>>"
                | "&<"
                | "&>"
        ) && (x.is_null() || y.is_null()))
        .then_some(Value::Null));
    }
    let (x, y) = (read(x, left, y)?, read(y, right, x)?);
    let refused = || {
        Error::new(format!(
            "operator does not exist: {} {name} {}{NO_OPERATOR}",
            kind_name(&x),
            kind_name(&y)
        ))
    };
    match (&x, &y) {
        (Value::Geometric(a), Value::Geometric(b)) => {
            match crate::geo::at_release(release, || crate::geo::geo_binary(&name, a, b)) {
                Some(v) => Ok(Some(of_geo(v?))),
                None => Err(refused()),
            }
        }
        // Over a NULL, NULL: a NULL written out, where the type has the operator beside its own.
        (Value::Geometric(g), Value::Null) | (Value::Null, Value::Geometric(g)) => {
            let written = |e: &Expr| matches!(e, Expr::Value(v) if v.value == SqlValue::Null);
            if !written(left) && !written(right) {
                return Ok(Some(Value::Null));
            }
            match crate::geo::geo_binary(&name, g, g) {
                Some(_) => Ok(Some(Value::Null)),
                None => Err(refused()),
            }
        }
        _ => Err(refused()),
    }
}

/// Whether a value is of a type held as its text with a `||` of its own: jsonb, tsvector, tsquery.
fn concat_of_own(v: &Value) -> bool {
    matches!(
        v,
        Value::Typed(
            ColumnType::Textual(
                crate::textual::Textual::Jsonb
                    | crate::textual::Textual::Tsvector
                    | crate::textual::Textual::Tsquery
            ),
            _
        )
    )
}

/// Whether `||` between two values is text's, each side written as its type writes it: a literal
/// beside a type with no form of its own, or a string beside anything but an array.
fn concat_is_text(
    x: &Value,
    y: &Value,
    left: &Expr,
    right: &Expr,
    typed: [Option<ColumnType>; 2],
) -> bool {
    let array = |v: &Value, t: Option<ColumnType>| {
        matches!(v, Value::Composed(ty, _) if super::typed::element_of(*ty).is_some())
            || t.is_some_and(|t| super::typed::element_of(t).is_some())
    };
    let own = |v: &Value, t: Option<ColumnType>| {
        array(v, t)
            || concat_of_own(v)
            || matches!(v, Value::Bytes(_) | Value::Bits(..))
            || t.is_some_and(|t| super::typed::concat_form(t).is_some())
    };
    let string = |v: &Value, t: Option<ColumnType>| {
        matches!(
            v,
            Value::Text(_) | Value::Str(..) | Value::Scalar(ColumnType::Char, _)
        ) || t.is_some_and(|t| t.is_string() || t == ColumnType::Char)
    };
    let literal = |e: &Expr| super::typed::untyped(e).is_some();
    match (literal(left), literal(right)) {
        (true, false) => !own(y, typed[1]),
        (false, true) => !own(x, typed[0]),
        (false, false) => {
            string(x, typed[0]) && !array(y, typed[1]) || string(y, typed[1]) && !array(x, typed[0])
        }
        _ => true,
    }
}

/// An operator on a type held as its text, as PostgreSQL defines it on that type: inet's and
/// cidr's containment, overlap, bitwise and arithmetic. A comparison of a type PostgreSQL does not
/// order is refused, whatever stands beside it. None where neither side is such a value, or for a
/// comparison of one that orders, which `compare` answers.
fn typed_op(
    op: &BinaryOperator,
    x: &Value,
    y: &Value,
    left: &Expr,
    right: &Expr,
) -> Result<Option<Value>> {
    use crate::textual::Textual;
    let typed = |v: &Value| match v {
        Value::Typed(ty, _) => Some(*ty),
        _ => None,
    };
    let name = op.to_string();
    let Some(ty) = typed(x).or_else(|| typed(y)) else {
        // An operator only these types have, over a NULL, is NULL.
        return Ok((matches!(
            name.as_str(),
            "<<="
                | ">>="
                | "&&"
                | "->"
                | "->>"
                | "#>"
                | "#>>"
                | "@>"
                | "<@"
                | "?"
                | "?|"
                | "?&"
                | "#-"
                | "@@"
                | "@?"
                | "<->"
        ) && (x.is_null() || y.is_null()))
        .then_some(Value::Null));
    };
    let refused = || {
        Error::new(format!(
            "operator does not exist: {} {name} {}{NO_OPERATOR}",
            kind_name(x),
            kind_name(y)
        ))
    };
    if matches!(
        op,
        BinaryOperator::Eq
            | BinaryOperator::NotEq
            | BinaryOperator::Lt
            | BinaryOperator::LtEq
            | BinaryOperator::Gt
            | BinaryOperator::GtEq
    ) {
        let equal = matches!(op, BinaryOperator::Eq);
        return if ty.ordered() || equal && ty == ColumnType::Textual(Textual::Aclitem) {
            Ok(None)
        } else {
            Err(refused())
        };
    }
    // `||` of a type with none of its own is text's, of each side's text.
    if *op == BinaryOperator::StringConcat && !concat_of_own(x) && !concat_of_own(y) {
        return Ok(None);
    }
    // A quoted literal is read as the type the operator takes on its side.
    let read = |v: &Value, e: &Expr, as_: Textual| -> Result<Value> {
        Ok(match v {
            Value::Text(t) if quoted(e) => {
                Value::Typed(ColumnType::Textual(as_), crate::textual::text_in(as_, t)?)
            }
            v => v.clone(),
        })
    };
    let kind = |v: &Value| match v {
        Value::Typed(ColumnType::Textual(t), _) => Some(*t),
        _ => None,
    };
    let (lk, rk) = (kind(x), kind(y));
    // json's and jsonb's: a key or an index, a path or keys as text[], a document beside a
    // document.
    if let Some(doc @ (Textual::Json | Textual::Jsonb)) = lk {
        let jsonb = doc == Textual::Jsonb;
        let Value::Typed(_, a) = x else {
            unreachable!("a json document")
        };
        let texts = || -> Result<Option<(Vec<Option<String>>, usize)>> {
            match y {
                y if y.is_null() => Ok(None),
                Value::Text(t) if quoted(right) => super::ops::text_array_literal(t).map(Some),
                y => match super::ops::text_items(y) {
                    Some(items) => Ok(Some(items)),
                    None => Err(refused()),
                },
            }
        };
        let flat = |dims: usize| match dims > 1 {
            true => Err(Error::new("wrong number of array subscripts")),
            false => Ok(()),
        };
        let json_of = |s: Option<String>| {
            s.map_or(Value::Null, |s| Value::Typed(ColumnType::Textual(doc), s))
        };
        let text_of = |s: Option<String>| s.map_or(Value::Null, Value::Text);
        let doc_of = |s: String| Value::Typed(ColumnType::Textual(doc), s);
        // A path beside a jsonb, a quoted literal read as one.
        if jsonb && matches!(name.as_str(), "@?" | "@@") {
            let path = match read(y, right, Textual::Jsonpath)? {
                Value::Typed(ColumnType::Textual(Textual::Jsonpath), p) => p,
                v if v.is_null() => return Ok(Some(Value::Null)),
                _ => return Err(refused()),
            };
            let answer = if name == "@?" {
                crate::jsonpath::jsonb_path_exists_op(a, &path)?
            } else {
                crate::jsonpath::jsonb_path_match_op(a, &path)?
            };
            return Ok(Some(answer.map_or(Value::Null, Value::Bool)));
        }
        use crate::jsonb as j;
        return Ok(Some(match (name.as_str(), y) {
            (_, Value::Null) => Value::Null,
            ("->", Value::Text(k)) if !matches!(right, Expr::Array(_)) => json_of(if jsonb {
                j::jsonb_field(a, k)?
            } else {
                j::json_field(a, k)?
            }),
            ("->", Value::Int(i)) => json_of(if jsonb {
                j::jsonb_element(a, *i)?
            } else {
                j::json_element(a, *i)?
            }),
            ("->>", Value::Text(k)) if !matches!(right, Expr::Array(_)) => text_of(if jsonb {
                j::jsonb_field_text(a, k)?
            } else {
                j::json_field_text(a, k)?
            }),
            ("->>", Value::Int(i)) => text_of(if jsonb {
                j::jsonb_element_text(a, *i)?
            } else {
                j::json_element_text(a, *i)?
            }),
            ("#>" | "#>>", _) => {
                let Some((path, _)) = texts()? else {
                    return Ok(Some(Value::Null));
                };
                match (name.as_str(), jsonb) {
                    ("#>", true) => json_of(j::jsonb_path(a, &path)?),
                    ("#>", false) => json_of(j::json_path(a, &path)?),
                    (_, true) => text_of(j::jsonb_path_text(a, &path)?),
                    (_, false) => text_of(j::json_path_text(a, &path)?),
                }
            }
            ("?", Value::Text(k)) if jsonb && !matches!(right, Expr::Array(_)) => {
                Value::Bool(j::jsonb_exists(a, k)?)
            }
            ("?|" | "?&" | "#-", _) if jsonb => {
                let Some((keys, dims)) = texts()? else {
                    return Ok(Some(Value::Null));
                };
                match name.as_str() {
                    "?|" => Value::Bool(j::jsonb_exists_any(a, &keys)?),
                    "?&" => Value::Bool(j::jsonb_exists_all(a, &keys)?),
                    _ => {
                        flat(dims)?;
                        doc_of(j::jsonb_delete_path(a, &keys)?)
                    }
                }
            }
            ("-", Value::Int(i)) if jsonb => doc_of(j::jsonb_delete_index(a, *i)?),
            ("-", Value::Composed(..)) if jsonb => {
                let Some((keys, dims)) = texts()? else {
                    return Ok(Some(Value::Null));
                };
                flat(dims)?;
                doc_of(j::jsonb_delete_keys(a, &keys)?)
            }
            ("-", Value::Text(k)) if jsonb => doc_of(j::jsonb_delete_key(a, k)?),
            ("@>" | "<@" | "||", _) if jsonb => {
                let b = match read(y, right, Textual::Jsonb)? {
                    Value::Typed(ColumnType::Textual(Textual::Jsonb), b) => b,
                    _ => return Err(refused()),
                };
                match name.as_str() {
                    "@>" => Value::Bool(j::jsonb_contains(a, &b)?),
                    "<@" => Value::Bool(j::jsonb_contains(&b, a)?),
                    _ => doc_of(j::jsonb_concat(a, &b)?),
                }
            }
            _ => return Err(refused()),
        }));
    }
    // A document on the right of @>, <@ or ||, a literal on the left read as jsonb.
    if rk == Some(Textual::Jsonb) && matches!(name.as_str(), "@>" | "<@" | "||") {
        let (Value::Typed(_, a), Value::Typed(_, b)) = (read(x, left, Textual::Jsonb)?, y) else {
            return if x.is_null() {
                Ok(Some(Value::Null))
            } else {
                Err(refused())
            };
        };
        let doc_of = |s: String| Value::Typed(ColumnType::Textual(Textual::Jsonb), s);
        return Ok(Some(match name.as_str() {
            "@>" => Value::Bool(crate::jsonb::jsonb_contains(&a, b)?),
            "<@" => Value::Bool(crate::jsonb::jsonb_contains(b, &a)?),
            _ => doc_of(crate::jsonb::jsonb_concat(&a, b)?),
        }));
    }
    let net = |k: Option<Textual>| k.is_some_and(Textual::is_network);
    // The other side's type, where it is a literal: the type the operator reads beside this one.
    let (x, y) = match (lk, rk) {
        (Some(Textual::Tsvector), None) if name == "@@" => {
            (x.clone(), read(y, right, Textual::Tsquery)?)
        }
        (None, Some(Textual::Tsvector)) if name == "@@" => {
            (read(x, left, Textual::Tsquery)?, y.clone())
        }
        (Some(Textual::Tsquery), None) if name == "@@" => {
            (x.clone(), read(y, right, Textual::Tsvector)?)
        }
        (None, Some(Textual::Tsquery)) if name == "@@" => {
            (read(x, left, Textual::Tsvector)?, y.clone())
        }
        (Some(k), None) if k.is_network() && name != "+" && name != "-" => {
            (x.clone(), read(y, right, Textual::Inet)?)
        }
        (Some(k), None) if k.is_network() && name == "-" && quoted(right) => {
            (x.clone(), read(y, right, Textual::Inet)?)
        }
        (None, Some(k)) if k.is_network() && name != "+" => {
            (read(x, left, Textual::Inet)?, y.clone())
        }
        (Some(k), None) => (x.clone(), read(y, right, k)?),
        (None, Some(k)) => (read(x, left, k)?, y.clone()),
        _ => (x.clone(), y.clone()),
    };
    if x.is_null() || y.is_null() {
        return Ok(Some(Value::Null));
    }
    let typed_as = |t: Textual, s: String| Value::Typed(ColumnType::Textual(t), s);
    let (lk, rk) = (kind(&x), kind(&y));
    let (Value::Typed(_, a) | Value::Text(a)) = &x else {
        return match (name.as_str(), &x, &y) {
            ("+", Value::Int(n), Value::Typed(_, b)) if net(rk) => Ok(Some(typed_as(
                Textual::Inet,
                crate::inet::inet_plus(b, *n)?,
            ))),
            _ => Err(refused()),
        };
    };
    Ok(Some(match (name.as_str(), lk, rk, &y) {
        // inet's and cidr's, each an inet's result.
        ("<<", l, r, Value::Typed(_, b)) if net(l) && net(r) => {
            Value::Bool(crate::inet::inet_sub(a, b))
        }
        ("<<=", l, r, Value::Typed(_, b)) if net(l) && net(r) => {
            Value::Bool(crate::inet::inet_subeq(a, b))
        }
        (">>", l, r, Value::Typed(_, b)) if net(l) && net(r) => {
            Value::Bool(crate::inet::inet_sup(a, b))
        }
        (">>=", l, r, Value::Typed(_, b)) if net(l) && net(r) => {
            Value::Bool(crate::inet::inet_supeq(a, b))
        }
        ("&&", l, r, Value::Typed(_, b)) if net(l) && net(r) => {
            Value::Bool(crate::inet::inet_overlap(a, b))
        }
        ("&", l, r, Value::Typed(_, b)) if net(l) && net(r) => {
            typed_as(Textual::Inet, crate::inet::inet_and(a, b)?)
        }
        ("|", l, r, Value::Typed(_, b)) if net(l) && net(r) => {
            typed_as(Textual::Inet, crate::inet::inet_or(a, b)?)
        }
        ("+", l, None, Value::Int(n)) if net(l) => {
            typed_as(Textual::Inet, crate::inet::inet_plus(a, *n)?)
        }
        ("-", l, None, Value::Int(n)) if net(l) => {
            typed_as(Textual::Inet, crate::inet::inet_minus_int(a, *n)?)
        }
        ("-", l, r, Value::Typed(_, b)) if net(l) && net(r) => {
            Value::Int(crate::inet::inet_minus(a, b)?)
        }
        // tsvector's and tsquery's.
        ("@@", Some(Textual::Tsvector), Some(Textual::Tsquery), Value::Typed(_, b)) => {
            Value::Bool(crate::tsearch::ts_match(a, b))
        }
        ("@@", Some(Textual::Tsquery), Some(Textual::Tsvector), Value::Typed(_, b)) => {
            Value::Bool(crate::tsearch::ts_match(b, a))
        }
        ("||", Some(Textual::Tsvector), Some(Textual::Tsvector), Value::Typed(_, b)) => {
            typed_as(Textual::Tsvector, crate::tsearch::tsvector_concat(a, b)?)
        }
        ("||", Some(Textual::Tsquery), Some(Textual::Tsquery), Value::Typed(_, b)) => {
            typed_as(Textual::Tsquery, crate::tsearch::tsquery_or(a, b)?)
        }
        ("&&", Some(Textual::Tsquery), Some(Textual::Tsquery), Value::Typed(_, b)) => {
            typed_as(Textual::Tsquery, crate::tsearch::tsquery_and(a, b)?)
        }
        ("<->", Some(Textual::Tsquery), Some(Textual::Tsquery), Value::Typed(_, b)) => {
            typed_as(Textual::Tsquery, crate::tsearch::tsquery_phrase(a, b)?)
        }
        ("@>", Some(Textual::Tsquery), Some(Textual::Tsquery), Value::Typed(_, b)) => {
            Value::Bool(crate::tsearch::tsq_mcontains(a, b))
        }
        ("<@", Some(Textual::Tsquery), Some(Textual::Tsquery), Value::Typed(_, b)) => {
            Value::Bool(crate::tsearch::tsq_mcontains(b, a))
        }
        _ => return Err(refused()),
    }))
}

/// An operator on strings, bytea or bit strings, as PostgreSQL defines it: `||` of text and any
/// other value as its text, of bytea and of bit strings; `&`, `|` and `#` of bit strings of one
/// length, and `<<` and `>>` of one by an integer. None where neither side is one of them.
fn string_op(op: &BinaryOperator, x: &Value, y: &Value) -> Result<Option<Value>> {
    use crate::strings::{bits_cat, bits_each, bits_of_key, bits_shift};
    use BinaryOperator as B;
    let stringy = |v: &Value| {
        matches!(
            v,
            Value::Text(_) | Value::Str(..) | Value::Bytes(_) | Value::Bits(..)
        )
    };
    // Each of these is strict: over a NULL it is NULL, whatever the other side.
    if (x.is_null() || y.is_null())
        && matches!(
            op,
            B::StringConcat | B::PGBitwiseXor | B::PGBitwiseShiftLeft | B::PGBitwiseShiftRight
        )
    {
        return Ok(Some(Value::Null));
    }
    if !(stringy(x) || stringy(y))
        || !matches!(
            op,
            B::StringConcat
                | B::BitwiseAnd
                | B::BitwiseOr
                | B::PGBitwiseXor
                | B::PGBitwiseShiftLeft
                | B::PGBitwiseShiftRight
        )
    {
        return Ok(None);
    }
    if x.is_null() || y.is_null() {
        return Ok(Some(Value::Null));
    }
    // A bit string an operator makes is a bit of its own length, as PostgreSQL takes a bit varying
    // to bit for them.
    let bit = |k: Vec<u8>| Value::Bits(ColumnType::Bit(bits_of_key(&k).0), k);
    Ok(Some(match (op, x, y) {
        (B::StringConcat, Value::Bytes(a), Value::Bytes(b)) => {
            Value::Bytes(a.iter().chain(b).copied().collect())
        }
        (B::StringConcat, Value::Bits(_, a), Value::Bits(_, b)) => {
            Value::Bits(ColumnType::Varbit(None), bits_cat(a, b))
        }
        (B::BitwiseAnd, Value::Bits(_, a), Value::Bits(_, b)) => {
            bit(bits_each(a, b, "AND", |p, q| p && q)?)
        }
        (B::BitwiseOr, Value::Bits(_, a), Value::Bits(_, b)) => {
            bit(bits_each(a, b, "OR", |p, q| p || q)?)
        }
        (B::PGBitwiseXor, Value::Bits(_, a), Value::Bits(_, b)) => {
            bit(bits_each(a, b, "XOR", |p, q| p != q)?)
        }
        (B::PGBitwiseShiftLeft, Value::Bits(_, a), Value::Int(n)) if i32::try_from(*n).is_ok() => {
            bit(bits_shift(a, *n))
        }
        (B::PGBitwiseShiftRight, Value::Bits(_, a), Value::Int(n)) if i32::try_from(*n).is_ok() => {
            bit(bits_shift(a, -*n))
        }
        _ => {
            return Err(Error::new(format!(
                "operator does not exist: {} {op} {}{NO_OPERATOR}",
                kind_name(x),
                kind_name(y)
            )))
        }
    }))
}

/// A LIKE or an ILIKE, as PostgreSQL reads one: text, a name, a bpchar's own text with its blanks,
/// or bytea, matched by a pattern of its kind, an ILIKE's both in lower case under `casing`.
/// `like` and `notlike` by name: LIKE with its backslash escape, under the collation `under`.
pub(super) fn like_values(v: &Value, p: &Value, under: Option<Casing>) -> Result<bool> {
    like_of(v, p, Some('\\'), None, under)
}

fn like_of(
    v: &Value,
    p: &Value,
    escape: Option<char>,
    ilike: Option<Casing>,
    under: Option<Casing>,
) -> Result<bool> {
    let own = |v: &Value| match v {
        Value::Str(_, s) | Value::Text(s) => Some(s.clone()),
        _ => None,
    };
    match (v, p) {
        (Value::Bytes(a), Value::Bytes(b)) if ilike.is_none() => {
            let e = match escape {
                None => None,
                Some(c) => Some(u8::try_from(c).map_err(|_| Error::new("invalid escape string"))?),
            };
            crate::strings::like(a, b, b'%', b'_', e)
        }
        (a, b) if own(a).is_some() && b.string_key().is_some() => {
            let (mut t, mut q) = (
                own(a).unwrap(),
                std::str::from_utf8(b.string_key().unwrap())
                    .unwrap_or_default()
                    .to_string(),
            );
            if let Some(casing) = ilike {
                if let Casing::Library(id) = casing {
                    if !crate::collations::get(id).spec.deterministic {
                        return Err(Error::new(
                            "nondeterministic collations are not supported for ILIKE",
                        ));
                    }
                }
                t = crate::strings::case(&t, false, casing)?;
                q = crate::strings::case(&q, false, casing)?;
            }
            let (t, q): (Vec<char>, Vec<char>) = (t.chars().collect(), q.chars().collect());
            if let (None, Some(Casing::Library(id))) = (ilike, under) {
                let c = crate::collations::get(id);
                if !c.spec.deterministic {
                    return crate::strings::like_nondeterministic(&t, &q, escape, c);
                }
            }
            crate::strings::like(&t, &q, '%', '_', escape)
        }
        _ => Err(Error::new(format!(
            "operator does not exist: {} {} {}{NO_OPERATOR}",
            kind_name(v),
            if ilike.is_some() { "~~*" } else { "~~" },
            kind_name(p)
        ))),
    }
}

/// A value of the planner's statistics types where it meets a string or an untyped literal: its
/// text, as PostgreSQL casts it there through its output (the step's typing lets only
/// pg_mcv_list's, bytea's, through); beside anything else, its bytes as they are.
fn written_beside(frame: &Frame, e: &Expr, v: Value, other: &Expr) -> Value {
    let statistics = matches!(frame.type_of(e), Some(ColumnType::Internal(i)) if i.casts());
    let string = frame.type_of(other).is_none_or(|t| t.is_string());
    match v {
        Value::Bytes(_) if statistics && string => Value::Text(v.text().unwrap_or_default()),
        v => v,
    }
}

/// The casing a function of strings is called under: the collation its arguments settle, else the
/// database's default. Where they settle none, a function that maps case or searches for a
/// substring is refused in PostgreSQL's words, but where an argument is NULL, which it never reads;
/// any other needs none.
fn string_casing(frame: &Frame, e: &Expr, name: &str, values: &[Value]) -> Result<Casing> {
    let what = match name {
        "upper" | "lower" | "initcap" | "casefold" => format!("{name}() function"),
        "strpos" | "position" | "replace" | "split_part" | "starts_with" => {
            "string comparison".to_string()
        }
        _ => String::new(),
    };
    if frame.casings.unsettled(e) && (what.is_empty() || values.iter().any(Value::is_null)) {
        return Ok(Casing::Libc);
    }
    Ok(frame.casings.input(e, &what)?.unwrap_or(Casing::Libc))
}

/// The casing a regular expression is compiled under: the collation its text and its pattern
/// settle, else the database's default; refused where they settle none, but where either value
/// is NULL, which the match never reads.
fn regex_casing(frame: &Frame, e: &Expr, values: &[&Value]) -> Result<Casing> {
    if frame.casings.unsettled(e) && values.iter().any(|v| v.is_null()) {
        return Ok(Casing::Libc);
    }
    Ok(frame
        .casings
        .input(e, "regular expression")?
        .unwrap_or(Casing::Libc))
}

/// `s ~ p` and its kin, as PostgreSQL's textregexeq, nameregexeq, bpcharregexeq and their kin read
/// them: a text, a name or a bpchar against a text pattern.
fn regex_match(
    s: &Value,
    p: &Value,
    op: &BinaryOperator,
    icase: bool,
    not: bool,
    casing: Casing,
) -> Result<Value> {
    let string = |v: &Value| matches!(v, Value::Null | Value::Text(_) | Value::Str(..));
    if !string(s) || !string(p) {
        return Err(Error::new(format!(
            "operator does not exist: {} {op} {}{NO_OPERATOR}",
            kind_name(s),
            kind_name(p)
        )));
    }
    super::calls::regex_op(s, p, icase, not, casing)
}

/// A number as a double precision, as PostgreSQL widens it beside a float: an integer as itself
/// rounded, a numeric read from its text, a real exactly.
fn as_double(v: &Value) -> Option<f64> {
    Some(match v {
        Value::Int(x) => *x as f64,
        Value::Num(m, s) => numeric_text(*m, *s).parse().ok()?,
        Value::Numeric(n) => n.to_float(true).ok()?,
        Value::Real(f) => f64::from(*f),
        Value::Double(d) => *d,
        _ => return None,
    })
}

fn out_of_range(what: &str) -> Error {
    Error::new(format!("value out of range: {what}"))
}

/// A float's operator, as PostgreSQL's float4 and float8 operators give it: two reals in real, any
/// float beside a double, an integer or a numeric in double precision; refused where finite
/// numbers make an infinity, where a product or quotient of numbers not zero is zero, and where
/// it divides by zero. Money times or divided by a float, rounded to its smallest unit, and money
/// divided by money. None where neither side is a float.
fn float_arith(op: &BinaryOperator, x: &Value, y: &Value) -> Result<Option<Value>> {
    use BinaryOperator as B;
    let float = |v: &Value| matches!(v, Value::Real(_) | Value::Double(_));
    let money = |v: &Value| matches!(v, Value::Scalar(ColumnType::Money(_), _));
    if !(float(x) || float(y) || (money(x) && money(y) && matches!(op, B::Divide))) {
        return Ok(None);
    }
    let with_money = match (x, y) {
        (Value::Scalar(ty @ ColumnType::Money(_), c), f) if float(f) => {
            Some((*ty, *c, as_double(f), true))
        }
        (f, Value::Scalar(ty @ ColumnType::Money(_), c)) if float(f) => {
            Some((*ty, *c, as_double(f), false))
        }
        _ => None,
    };
    if let Some((ty, c, f, money_left)) = with_money {
        let f = f.expect("a float is a double");
        let r = match op {
            B::Multiply => c as f64 * f,
            B::Divide if money_left => {
                if f == 0.0 {
                    return Err(Error::new("division by zero"));
                }
                c as f64 / f
            }
            _ => {
                return Err(Error::new(format!(
                    "operator does not exist: {} {op} {}{NO_OPERATOR}",
                    kind_name(x),
                    kind_name(y)
                )))
            }
        };
        let r = r.round_ties_even();
        if !(-9.223_372_036_854_776e18..9.223_372_036_854_776e18).contains(&r) {
            return Err(Error::new("money out of range"));
        }
        return Ok(Some(Value::Scalar(ty, r as i64)));
    }
    if let (Value::Scalar(ColumnType::Money(_), a), Value::Scalar(ColumnType::Money(_), b)) = (x, y)
    {
        if *b == 0 {
            return Err(Error::new("division by zero"));
        }
        return Ok(Some(Value::Double(*a as f64 / *b as f64)));
    }
    let (Some(a), Some(b)) = (as_double(x), as_double(y)) else {
        return Err(Error::new(format!(
            "operator does not exist: {} {op} {}{NO_OPERATOR}",
            kind_name(x),
            kind_name(y)
        )));
    };
    let real = matches!((x, y), (Value::Real(_), Value::Real(_)));
    let (r, overflow, underflow) = match op {
        B::Plus | B::Minus => {
            let r = if real {
                let (p, q) = (a as f32, b as f32);
                f64::from(if matches!(op, B::Plus) { p + q } else { p - q })
            } else if matches!(op, B::Plus) {
                a + b
            } else {
                a - b
            };
            (
                r,
                r.is_infinite() && !a.is_infinite() && !b.is_infinite(),
                false,
            )
        }
        B::Multiply => {
            let r = if real {
                f64::from(a as f32 * b as f32)
            } else {
                a * b
            };
            (
                r,
                r.is_infinite() && !a.is_infinite() && !b.is_infinite(),
                r == 0.0 && a != 0.0 && b != 0.0,
            )
        }
        B::Divide => {
            if b == 0.0 && !a.is_nan() {
                return Err(Error::new("division by zero"));
            }
            let r = if real {
                f64::from(a as f32 / b as f32)
            } else {
                a / b
            };
            (
                r,
                r.is_infinite() && !a.is_infinite(),
                r == 0.0 && a != 0.0 && !b.is_infinite(),
            )
        }
        _ => {
            return Err(Error::new(format!(
                "operator does not exist: {} {op} {}{NO_OPERATOR}",
                kind_name(x),
                kind_name(y)
            )))
        }
    };
    if overflow {
        return Err(out_of_range("overflow"));
    }
    if underflow {
        return Err(out_of_range("underflow"));
    }
    Ok(Some(if real {
        Value::Real(r as f32)
    } else {
        Value::Double(r)
    }))
}

/// A value cast to a float, real where `wide` is not: an integer or a numeric rounded to it, text
/// read as float4in or float8in reads it; a double to a real refused where it overflows or falls
/// to zero.
pub(crate) fn float_cast(v: Value, wide: bool) -> Result<Value> {
    let bits = |t: &str| crate::scalars::float_parse(t, wide);
    let of = |b: u64| {
        if wide {
            Value::Double(f64::from_bits(b))
        } else {
            Value::Real(f32::from_bits(b as u32))
        }
    };
    Ok(match v {
        Value::Null => Value::Null,
        Value::Real(f) if wide => Value::Double(f64::from(f)),
        Value::Real(f) => Value::Real(f),
        Value::Double(d) if wide => Value::Double(d),
        Value::Double(d) => {
            let f = d as f32;
            if f.is_infinite() && !d.is_infinite() {
                return Err(out_of_range("overflow"));
            }
            if f == 0.0 && d != 0.0 {
                return Err(out_of_range("underflow"));
            }
            Value::Real(f)
        }
        Value::Int(x) => {
            if wide {
                Value::Double(x as f64)
            } else {
                Value::Real(x as f32)
            }
        }
        Value::Num(m, s) => of(bits(&numeric_text(m, s))?),
        Value::Numeric(n) => {
            let f = n.to_float(wide)?;
            if wide {
                Value::Double(f)
            } else {
                Value::Real(f as f32)
            }
        }
        Value::Text(t) => of(bits(&t)?),
        other => {
            return Err(Error::new(format!(
                "cannot cast type {} to {}",
                kind_name(&other),
                if wide { "double precision" } else { "real" }
            )))
        }
    })
}

/// A float rounded to an integer type named `base`, half to even, held to its range.
pub(crate) fn float_integer(f: f64, base: &str) -> Result<Value> {
    let (name, lo, hi) = match base {
        "smallint" | "int2" => ("smallint", i16::MIN as f64, -(i16::MIN as f64)),
        "integer" | "int" | "int4" => ("integer", i32::MIN as f64, -(i32::MIN as f64)),
        _ => ("bigint", i64::MIN as f64, -(i64::MIN as f64)),
    };
    let r = f.round_ties_even();
    if r.is_nan() || r < lo || r >= hi {
        return Err(Error::new(format!("{name} out of range")));
    }
    Ok(Value::Int(r as i64))
}

/// Text read as a boolean, as PostgreSQL's boolean input reads it.
fn boolean_of(t: &str) -> Result<Value> {
    super::question::truth(t)
        .map(Value::Bool)
        .ok_or_else(|| Error::new(format!("invalid input syntax for type boolean: \"{t}\"")))
}

/// An arithmetic operator on two numbers, as PostgreSQL's integer and numeric operators compute
/// it: two integers to an integer, any numeric to a numeric, at the scales numeric.c gives a sum, a
/// difference, a product, a quotient and a remainder.
pub(super) fn arith(op: &BinaryOperator, a: &Value, b: &Value) -> Result<Value> {
    use BinaryOperator as B;
    if a.is_null() || b.is_null() {
        return Ok(Value::Null);
    }
    if let (&Value::Int(x), &Value::Int(y)) = (a, b) {
        if matches!(op, B::Divide | B::Modulo) && y == 0 {
            return Err(Error::new("division by zero"));
        }
        let r = match op {
            B::Plus => x.checked_add(y),
            B::Minus => x.checked_sub(y),
            B::Multiply => x.checked_mul(y),
            B::Divide => x.checked_div(y),
            // As int8mod gives it, by -1 always 0.
            _ if y == -1 => Some(0),
            _ => x.checked_rem(y),
        };
        return r
            .map(Value::Int)
            .ok_or_else(|| Error::new("bigint out of range"));
    }
    if let (Some((x, sx)), Some((y, sy))) = (a.numeric(), b.numeric()) {
        let s = sx.max(sy);
        let aligned = || {
            Some((
                Value::at_scale_checked(x, sx, s)?,
                Value::at_scale_checked(y, sy, s)?,
            ))
        };
        let fast = match op {
            B::Plus => aligned()
                .and_then(|(p, q)| p.checked_add(q))
                .map(|r| (r, s)),
            B::Minus => aligned()
                .and_then(|(p, q)| p.checked_sub(q))
                .map(|r| (r, s)),
            B::Modulo if y != 0 => aligned()
                .and_then(|(p, q)| p.checked_rem(q))
                .map(|r| (r, s)),
            B::Multiply if sx + sy <= crate::numeric::DSCALE_MAX => {
                x.checked_mul(y).map(|r| (r, sx + sy))
            }
            _ => None,
        };
        if let Some((r, s)) = fast {
            return Ok(Value::Num(r, s));
        }
    }
    let (Some(x), Some(y)) = (a.to_numeric(), b.to_numeric()) else {
        return Err(Error::new(format!(
            "operator does not exist: {} {op} {}{NO_OPERATOR}",
            kind_name(a),
            kind_name(b)
        )));
    };
    Ok(Value::of_numeric(match op {
        B::Plus => x.add(&y),
        B::Minus => x.sub(&y),
        B::Multiply => x.mul(&y),
        B::Divide => x.div(&y),
        _ => x.rem(&y),
    }?))
}

pub(super) fn truth(v: &Value) -> Result<Option<bool>> {
    match v {
        v if v.is_null() => Ok(None),
        Value::Bool(b) => Ok(Some(*b)),
        other => Err(Error::new(format!("{other:?} is not a truth"))),
    }
}

/// A quoted literal or a parameter beside an expression the step resolved to an integer, read by
/// that integer's input at its width, as PostgreSQL gives an unknown the other side's type; any
/// other value as it is.
fn integer_read(frame: &Frame, e: &Expr, v: Value, other: &Expr) -> Result<Value> {
    let bits = match frame.type_of(other) {
        Some(ColumnType::Int2) => 16,
        Some(ColumnType::Int4) => 32,
        Some(ColumnType::Int8) => 64,
        _ => return Ok(v),
    };
    match &v {
        Value::Text(t) if quoted(e) => Ok(Value::Int(crate::composed::int_in(t, bits)?)),
        _ => Ok(v),
    }
}

/// A CASE's arm taken, in the type the step resolved the CASE to, as PostgreSQL's parser coerces
/// each arm to the type select_common_type finds: a quoted literal or a parameter read by that type's
/// input, a value of a type promoted to it (an integer to a numeric, a date to a timestamp) cast to
/// it. A string, a composite or an array, an arm of a type the CASE's is not found from, or a CASE
/// whose type is not resolved, as it is.
fn case_arm(frame: &Frame, case: &Expr, arm: &Expr, v: Value) -> Result<Value> {
    let Some(t) = frame.type_of(case) else {
        return Ok(v);
    };
    if v.is_null() || t.is_string() || matches!(t, ColumnType::Composed(_)) {
        return Ok(v);
    }
    let literal = quoted(arm) && matches!(v, Value::Text(_));
    let promoted = || {
        frame.type_of(arm).is_some_and(|a| {
            a != t && super::typed::common_type(&[Some(a), Some(t)], "CASE").is_ok_and(|c| c == t)
        })
    };
    if !literal && !promoted() {
        return Ok(v);
    }
    Ok(part_value(t, &part_of(t, v, literal)?))
}

/// A condition's truth on a row: a quoted literal or a parameter standing as the condition read by
/// boolean's input, as PostgreSQL's parser coerces an unknown to boolean where a truth stands
/// (WHERE, HAVING, ON, NOT, AND, OR, a CASE's WHEN, IS TRUE and its kin, an aggregate's FILTER);
/// else its value's.
pub(super) fn holds(e: &Expr, row: &[Value], frame: &Frame, computed: &Computed) -> Result<Option<bool>> {
    truth_of(e, eval(e, row, frame, computed)?)
}

/// The truth of a condition's value, `holds`' reading of it.
pub(super) fn truth_of(e: &Expr, v: Value) -> Result<Option<bool>> {
    match (&v, quoted(e)) {
        (Value::Text(t), true) => truth(&boolean_of(t)?),
        _ => truth(&v),
    }
}

/// The type a quoted literal beside a scalar of type `ty` is read as: an oid beside a reg* type,
/// whose comparisons are oid's; else `ty`.
fn literal_type(ty: ColumnType) -> ColumnType {
    if ty.is_oid() {
        ColumnType::Oid
    } else {
        // A time type's literal takes its type without the column's precision.
        super::clock::base(ty)
    }
}

/// Whether a type is a composite or a table's row type.
fn is_record(ty: ColumnType) -> bool {
    matches!(ty, ColumnType::Composed(r) if matches!(r.def(), crate::composed::Composed::Record { .. }))
}

/// The values a row constructor, `ROW(…)` or `(…, …)`, is made of; none where it is none.
pub(super) fn row_parts(e: &Expr) -> Option<Result<Vec<&Expr>>> {
    match e {
        Expr::Tuple(xs) => Some(Ok(xs.iter().collect())),
        Expr::Function(f) if crate::catalog::object_name(&f.name).eq_ignore_ascii_case("row") => {
            Some(args(f))
        }
        _ => None,
    }
}

/// A composed value read from its text by its type's input.
pub(super) fn composed_value(ty: ColumnType, t: &str) -> Result<Value> {
    Ok(match crate::composed::scalar_in(ty, t)? {
        crate::read::statement::Datum::Compound(c) => Value::Composed(ty, c),
        d => super::fill::value_of(&d, ty),
    })
}

/// A string type without its length: the type a literal beside one is read as.
fn unbounded(ty: ColumnType) -> ColumnType {
    match ty {
        ColumnType::Bpchar(_) => ColumnType::Bpchar(None),
        other => other,
    }
}

/// Two values compared, a quoted literal beside a boolean, a uuid or a numeric read as one, as
/// PostgreSQL gives its unknown type the other side's.
fn beside(x: Value, y: Value, left: &Expr, right: &Expr) -> Result<(Value, Value)> {
    let quoted = |e: &Expr| matches!(e, Expr::Value(v) if matches!(v.value, SqlValue::SingleQuotedString(_) | SqlValue::Placeholder(_)));
    Ok(match (&x, &y) {
        (Value::Bool(_), Value::Text(t)) if quoted(right) => {
            let t = boolean_of(t)?;
            (x, t)
        }
        (Value::Text(t), Value::Bool(_)) if quoted(left) => (boolean_of(t)?, y),
        (Value::Uuid(_), Value::Text(t)) if quoted(right) => {
            let t = Value::Uuid(crate::scalars::uuid_parse(t)?);
            (x, t)
        }
        (Value::Text(t), Value::Uuid(_)) if quoted(left) => {
            (Value::Uuid(crate::scalars::uuid_parse(t)?), y)
        }
        // A quoted literal beside a numeric is read as numeric_in reads it.
        (Value::Num(..) | Value::Numeric(_), Value::Text(t)) if quoted(right) => {
            let t = Value::of_numeric(Numeric::parse(t)?);
            (x, t)
        }
        (Value::Text(t), Value::Num(..) | Value::Numeric(_)) if quoted(left) => {
            (Value::of_numeric(Numeric::parse(t)?), y)
        }
        (Value::Real(_) | Value::Double(_), Value::Text(t)) if quoted(right) => {
            let t = float_cast(Value::Text(t.clone()), matches!(x, Value::Double(_)))?;
            (x, t)
        }
        (Value::Text(t), Value::Real(_) | Value::Double(_)) if quoted(left) => (
            float_cast(Value::Text(t.clone()), matches!(y, Value::Double(_)))?,
            y,
        ),
        (Value::Composed(ty, _), Value::Text(_)) if quoted(right) && is_record(*ty) => {
            return Err(Error::new(
                "input of anonymous composite types is not implemented",
            ))
        }
        (Value::Text(_), Value::Composed(ty, _)) if quoted(left) && is_record(*ty) => {
            return Err(Error::new(
                "input of anonymous composite types is not implemented",
            ))
        }
        (Value::Composed(ty, _), Value::Text(t)) if quoted(right) => {
            let t = composed_value(*ty, t)?;
            (x, t)
        }
        (Value::Text(t), Value::Composed(ty, _)) if quoted(left) => (composed_value(*ty, t)?, y),
        (Value::Scalar(ty, _), Value::Text(t)) if quoted(right) => {
            let ty = literal_type(*ty);
            let t = Value::Scalar(ty, crate::scalars::scalar_parse(ty, t)?);
            (x, t)
        }
        (Value::Text(t), Value::Scalar(ty, _)) if quoted(left) => {
            let ty = literal_type(*ty);
            (Value::Scalar(ty, crate::scalars::scalar_parse(ty, t)?), y)
        }
        // A quoted literal beside bytea, a bpchar, a name or a bit string is read by that type's
        // input.
        (Value::Bytes(_), Value::Text(t)) if quoted(right) => {
            let t = Value::Bytes(crate::strings::bytea_in(t)?);
            (x, t)
        }
        (Value::Text(t), Value::Bytes(_)) if quoted(left) => {
            (Value::Bytes(crate::strings::bytea_in(t)?), y)
        }
        (Value::Str(ty, _), Value::Text(t)) if quoted(right) => {
            let t = string_cast(Value::Text(t.clone()), unbounded(*ty), false)?;
            (x, t)
        }
        (Value::Text(t), Value::Str(ty, _)) if quoted(left) => (
            string_cast(Value::Text(t.clone()), unbounded(*ty), false)?,
            y,
        ),
        // Beside a geometric value, by its type's input.
        (Value::Geometric(g), Value::Text(t)) if quoted(right) => {
            let t = Value::Geometric(crate::geo::geo_in(g.kind(), t)?);
            (x, t)
        }
        (Value::Text(t), Value::Geometric(g)) if quoted(left) => {
            (Value::Geometric(crate::geo::geo_in(g.kind(), t)?), y)
        }
        // Beside a value held as its text, by its type's input; beside a node tree, as text.
        (Value::Typed(ColumnType::Textual(ty), _), Value::Text(t)) if quoted(right) => {
            let ty = ty.literal_type();
            let t = Value::Typed(ColumnType::Textual(ty), crate::textual::text_in(ty, t)?);
            (x, t)
        }
        (Value::Text(t), Value::Typed(ColumnType::Textual(ty), _)) if quoted(left) => (
            Value::Typed(
                ColumnType::Textual(ty.literal_type()),
                crate::textual::text_in(ty.literal_type(), t)?,
            ),
            y,
        ),
        (Value::Bits(..), Value::Text(t)) if quoted(right) => {
            let t = bits_cast(Value::Text(t.clone()), ColumnType::Varbit(None), false)?;
            (x, t)
        }
        (Value::Text(t), Value::Bits(..)) if quoted(left) => (
            bits_cast(Value::Text(t.clone()), ColumnType::Varbit(None), false)?,
            y,
        ),
        // A quoted literal beside an integer is read by the integer's input.
        (Value::Int(_), Value::Text(t)) if quoted(right) => {
            (x.clone(), Value::Int(crate::composed::int_in(t, 64)?))
        }
        (Value::Text(t), Value::Int(_)) if quoted(left) => {
            (Value::Int(crate::composed::int_in(t, 64)?), y.clone())
        }
        (Value::Interval(_), Value::Text(t)) if quoted(right) => {
            let t = super::clock::cast(Value::Text(t.clone()), literal_interval())?;
            (x, t)
        }
        (Value::Text(t), Value::Interval(_)) if quoted(left) => (
            super::clock::cast(Value::Text(t.clone()), literal_interval())?,
            y,
        ),
        // A date beside an integer is added to or taken from, its integer kept.
        (Value::Scalar(..), Value::Int(_)) if super::clock::time_of(&x).is_some() => (x, y),
        (Value::Int(_), Value::Scalar(..)) if super::clock::time_of(&y).is_some() => (x, y),
        (Value::Scalar(..), Value::Int(_)) => {
            let y = integer_beside(&x, y)?;
            (x, y)
        }
        // xid's comparisons with an integer take it on their right only.
        (Value::Int(_), Value::Scalar(ColumnType::Xid, _)) => {
            return Err(crate::error::no_operator(Some("integer"), "=", "xid"))
        }
        (Value::Int(_), Value::Scalar(..)) => {
            let x = integer_beside(&y, x)?;
            (x, y)
        }
        _ => (x, y),
    })
}

/// The interval a quoted literal beside an interval is read as.
fn literal_interval() -> ColumnType {
    ColumnType::Interval {
        range: crate::times::INTERVAL_FULL_RANGE,
        precision: None,
    }
}

/// Evaluates an expression on one row of a frame, its aggregates' and windows' values computed.
/// A binary operator's value on one row, its sides read where they stand: arithmetic, the
/// comparisons and the rest an operator reads past AND, OR, OVERLAPS and the regular expressions;
/// `e` the node whose type and collation settle it.
fn operated(
    e: &Expr,
    left: &Expr,
    op: &BinaryOperator,
    right: &Expr,
    row: &[Value],
    frame: &Frame,
    computed: &Computed,
) -> Result<Value> {
    let ev = |x: &Expr| eval(x, row, frame, computed);
    let x = ev(left)?;
    let y = ev(right)?;
    operated_on(e, left, op, right, x, y, frame)
}

/// A binary operator's value on its two sides' values, read from `left` and `right`.
fn operated_on(
    e: &Expr,
    left: &Expr,
    op: &BinaryOperator,
    right: &Expr,
    x: Value,
    y: Value,
    frame: &Frame,
) -> Result<Value> {
    let (x, y) = (
        written_beside(frame, left, x, right),
        written_beside(frame, right, y, left),
    );
    let typed = [frame.type_of(left), frame.type_of(right)];
    if let Some(v) = super::ops::of_integers(op, &x, &y, left, right, typed)? {
        return Ok(v);
    }
    if let Some(v) = super::ops::pattern_compared(op, &x, &y, left, right, typed)? {
        return Ok(v);
    }
    if let Some(v) = super::ops::text_matched(op, &x, &y, left, right, &frame.casings)? {
        return Ok(v);
    }
    if *op == BinaryOperator::PGExp {
        return super::ops::power(x, y, typed, frame.type_of(e));
    }
    if *op == BinaryOperator::PGStartsWith {
        if [&x, &y].into_iter().any(Value::is_null) {
            return Ok(Value::Null);
        }
        let values = [super::ops::char_as_text(x), super::ops::char_as_text(y)];
        let casing = string_casing(frame, e, "starts_with", &values)?;
        return Ok(super::calls::strings("starts_with", &values, casing)?
            .unwrap_or(Value::Null));
    }
    if *op == BinaryOperator::StringConcat && concat_is_text(&x, &y, left, right, typed) {
        return Ok(match x.is_null() || y.is_null() {
            true => Value::Null,
            false => Value::Text(text_part(&x) + &text_part(&y)),
        });
    }
    // An array's `||` is in the array of its sides' common type.
    let (x, y) = match frame.type_of(e) {
        Some(t) if *op == BinaryOperator::StringConcat => {
            (array_as(x, t)?, array_as(y, t)?)
        }
        _ => (x, y),
    };
    let x = super::ops::document_literal(op, x, left, frame.type_of(e))?;
    let y = super::ops::shift_count(op, &x, y, right)?;
    if !super::ops::deletes_keys(op, &x, &y) {
        if let Some(v) = composed_op(op, &x, &y, left, right, typed)? {
            return Ok(v);
        }
    }
    if let Some(v) = geo_op(op, &x, &y, left, right, frame.release)? {
        return Ok(v);
    }
    // An operator on a type held as its text reads a literal beside it as the operator
    // takes it.
    if let Some(v) = typed_op(op, &x, &y, left, right)? {
        return Ok(v);
    }
    let (x, y) = (
        integer_read(frame, left, x, right)?,
        integer_read(frame, right, y, left)?,
    );
    let (x, y) = beside(x, y, left, right)?;
    if let Some(v) = string_op(op, &x, &y)? {
        return Ok(v);
    }
    if matches!(
        op,
        BinaryOperator::Plus
            | BinaryOperator::Minus
            | BinaryOperator::Multiply
            | BinaryOperator::Divide
            | BinaryOperator::Modulo
            | BinaryOperator::BitwiseAnd
            | BinaryOperator::BitwiseOr
    ) {
        // Each of these operators is strict: over a NULL it is NULL.
        if x.is_null() || y.is_null() {
            return Ok(Value::Null);
        }
        if let Some(v) = super::clock::arith(op, &x, &y)? {
            return Ok(v);
        }
        if let Some(v) = float_arith(op, &x, &y)? {
            return Ok(v);
        }
        if let Some(v) = scalar_arith(op, &x, &y)? {
            return Ok(v);
        }
    } else {
        compared(op, &x, &y)?;
    }
    Ok(match op {
        BinaryOperator::Plus
        | BinaryOperator::Minus
        | BinaryOperator::Multiply
        | BinaryOperator::Divide
        | BinaryOperator::Modulo => frame.held(e, arith(op, &x, &y)?)?,
        BinaryOperator::Eq
        | BinaryOperator::NotEq
        | BinaryOperator::Lt
        | BinaryOperator::LtEq
        | BinaryOperator::Gt
        | BinaryOperator::GtEq => {
            // A NULL is compared under no collation.
            let under = match x.is_null() || y.is_null() {
                true => None,
                false => frame.casings.compared(e, "string comparison")?,
            };
            match compare_under(under, &x, &y)? {
                None => Value::Null,
                Some(o) => Value::Bool(match op {
                    BinaryOperator::Eq => o == Ordering::Equal,
                    BinaryOperator::NotEq => o != Ordering::Equal,
                    BinaryOperator::Lt => o == Ordering::Less,
                    BinaryOperator::LtEq => o != Ordering::Greater,
                    BinaryOperator::Gt => o == Ordering::Greater,
                    _ => o != Ordering::Less,
                }),
            }
        }
        other => return Err(Error::new(format!("the operator {other} is not read"))),
    })
}

/// Two rows of one width compared by `<`, `<=`, `>` or `>=` as PostgreSQL's executor compares
/// them: pair by pair, each side read once, the first pair not equal deciding by `op`, NULL where a
/// pair read before one decides holds a NULL; the last pair by `op` where every pair before it is
/// equal.
fn rows_by_order(
    e: &Expr,
    l: &[&Expr],
    op: &BinaryOperator,
    r: &[&Expr],
    row: &[Value],
    frame: &Frame,
    computed: &Computed,
) -> Result<Value> {
    let ev = |x: &Expr| eval(x, row, frame, computed);
    for (at, (a, b)) in l.iter().zip(r).enumerate() {
        let (x, y) = (ev(a)?, ev(b)?);
        if x.is_null() || y.is_null() {
            return Ok(Value::Null);
        }
        if at + 1 < l.len() {
            let eq = BinaryOperator::Eq;
            if matches!(
                operated_on(e, a, &eq, b, x.clone(), y.clone(), frame)?,
                Value::Bool(true)
            ) {
                continue;
            }
        }
        return operated_on(e, a, op, b, x, y, frame);
    }
    Ok(Value::Null)
}

/// Two truths joined by AND (`and`) or by OR, in SQL's three values.
fn joined(and: bool, x: Option<bool>, y: Option<bool>) -> Option<bool> {
    match (x, y) {
        (Some(a), Some(b)) => Some(if and { a && b } else { a || b }),
        (Some(v), None) | (None, Some(v)) if v != and => Some(v),
        _ => None,
    }
}

pub(super) fn eval(e: &Expr, row: &[Value], frame: &Frame, computed: &Computed) -> Result<Value> {
    if let Some(v) = computed.calls.get(&(e as *const Expr)) {
        return Ok(v.clone());
    }
    if let Some(v) = computed.bound.and_then(|b| b(e)) {
        return v;
    }
    // A value made below the fold, on a row that carries it.
    if let Some(b) = frame.below.filter(|b| b.n > 0 && row.len() == frame.columns.len() + b.n) {
        if let Some(&(_, k)) = b.at.iter().find(|(p, _)| std::ptr::eq(*p, e)) {
            return Ok(row[frame.columns.len() + k].clone());
        }
    }
    if is_subquery(e) {
        return match frame.subqueries {
            Some(s) => s.value(e, row, frame, computed),
            None => Err(Error::new(format!("{e} is not read here"))),
        };
    }
    // The forms calls of the file's functions inside one another pass through, each read in a
    // frame of its own, so that a level of them takes little of the stack.
    match e {
        Expr::Nested(x) => eval(x, row, frame, computed),
        Expr::Case {
            operand: None,
            conditions,
            else_result,
            ..
        } => searched_case(e, conditions, else_result.as_deref(), row, frame, computed),
        Expr::BinaryOp { left, op, right } if of_two_values(left, op, right) => {
            operated(e, left, op, right, row, frame, computed)
        }
        Expr::Function(f) if function_name(f) != "make_interval" => {
            // A function of the file's, its arguments by place or by name.
            match super::own::call(f, row, frame, computed)? {
                Some(v) => Ok(v),
                None => eval_whole(e, row, frame, computed),
            }
        }
        _ => eval_whole(e, row, frame, computed),
    }
}

/// A CASE with no operand: the result of its first condition that holds, else its ELSE's.
fn searched_case(
    e: &Expr,
    conditions: &[sqlparser::ast::CaseWhen],
    else_result: Option<&Expr>,
    row: &[Value],
    frame: &Frame,
    computed: &Computed,
) -> Result<Value> {
    let mut taken = else_result;
    for c in conditions {
        if holds(&c.condition, row, frame, computed)? == Some(true) {
            taken = Some(&c.result);
            break;
        }
    }
    match taken {
        Some(x) => case_arm(frame, e, x, eval(x, row, frame, computed)?),
        None => Ok(Value::Null),
    }
}

/// Whether a binary operator is arithmetic or a comparison read on its two sides' values alone,
/// as `operated` reads it.
fn of_two_values(left: &Expr, op: &BinaryOperator, right: &Expr) -> bool {
    matches!(
        op,
        BinaryOperator::Plus
            | BinaryOperator::Minus
            | BinaryOperator::Multiply
            | BinaryOperator::Divide
            | BinaryOperator::Modulo
            | BinaryOperator::Eq
            | BinaryOperator::NotEq
            | BinaryOperator::Lt
            | BinaryOperator::LtEq
            | BinaryOperator::Gt
            | BinaryOperator::GtEq
    ) && super::image::operator(op).is_none()
        && (row_parts(left).is_none() || row_parts(right).is_none())
}

/// Every form `eval` does not read in a frame of its own.
#[inline(never)]
fn eval_whole(e: &Expr, row: &[Value], frame: &Frame, computed: &Computed) -> Result<Value> {
    let ev = |x: &Expr| eval(x, row, frame, computed);
    Ok(match e {
        Expr::Identifier(_) | Expr::CompoundIdentifier(_) => {
            let (q, n) = column_of(e).ok_or_else(|| Error::new(format!("{e} is not read")))?;
            row[frame.find(q.as_deref(), &n)?].clone()
        }
        // Every collation the snail holds orders text by its bytes: a COLLATE changes no value.
        Expr::Collate { expr, collation } => {
            frame
                .casings
                .named(&crate::catalog::object_name(collation))?;
            ev(expr)?
        }
        Expr::Value(v) => match &v.value {
            SqlValue::Number(s, _) => numeric_literal(s)?,
            SqlValue::SingleQuotedString(s) | SqlValue::EscapedStringLiteral(s) => {
                Value::Text(s.clone())
            }
            // A bit-string constant is a bit of its own length.
            SqlValue::SingleQuotedByteStringLiteral(_) | SqlValue::HexStringLiteral(_) => {
                let s = super::question::bit_literal(e).unwrap();
                let k = crate::strings::bits_parse(&s, None, false)?;
                Value::Bits(ColumnType::Bit(crate::strings::bits_of_key(&k).0), k)
            }
            SqlValue::Null => Value::Null,
            SqlValue::Boolean(b) => Value::Bool(*b),
            SqlValue::Placeholder(p) => {
                let n: usize = p
                    .strip_prefix('$')
                    .and_then(|x| x.parse().ok())
                    .ok_or_else(|| Error::new(format!("the parameter {p} is not read")))?;
                Value::Text(
                    frame
                        .params
                        .get(n.wrapping_sub(1))
                        .ok_or_else(|| Error::new(format!("no value is given for {p}")))?
                        .clone(),
                )
            }
            other => return Err(Error::new(format!("the literal {other} is not read"))),
        },
        // An ARRAY cast to an array type takes that type, each element cast to its element type.
        Expr::Cast {
            kind: CastKind::Cast | CastKind::DoubleColon,
            expr,
            data_type,
            ..
        } if matches!(expr.as_ref(), Expr::Array(_))
            && crate::composed::resolve(data_type)
                .is_some_and(|t| super::typed::element_of(t).is_some()) =>
        {
            let Expr::Array(a) = expr.as_ref() else {
                unreachable!()
            };
            let ty = crate::composed::resolve(data_type).unwrap();
            array_cast(a, ty, data_type, row, frame, computed)?
        }
        // A row constructor cast to a row type, a composite's or a table's: each value in its
        // field's type.
        Expr::Cast {
            kind: CastKind::Cast | CastKind::DoubleColon,
            expr,
            data_type,
            ..
        } if row_parts(expr).is_some() && crate::composed::resolve(data_type).is_some_and(is_record) => {
            let ty = crate::composed::resolve(data_type).unwrap();
            let parts = row_parts(expr).unwrap()?;
            let crate::composed::Composed::Record { fields, .. } = (match ty {
                ColumnType::Composed(r) => r.def(),
                _ => unreachable!(),
            }) else {
                unreachable!()
            };
            if parts.len() != fields.len() {
                return Err(Error::new(format!(
                    "cannot cast type record to {}",
                    ty.pg_name()
                )));
            }
            let items = parts
                .iter()
                .zip(fields)
                .map(|(x, f)| part_of(f.ty, ev(x)?, quoted(x)))
                .collect::<Result<Vec<_>>>()?;
            composed(ty, crate::composed::Item::Record(items))
        }
        // A boolean casts to and from integer alone, refused from a smallint or a bigint whatever
        // its value.
        Expr::Cast {
            kind: CastKind::Cast | CastKind::DoubleColon,
            expr,
            data_type: DataType::Bool | DataType::Boolean,
            ..
        } if matches!(frame.type_of(expr), Some(ColumnType::Int2 | ColumnType::Int8)) => {
            let from = frame.type_of(expr).map(|t| t.pg_name()).unwrap_or_default();
            return Err(Error::new(format!("cannot cast type {from} to boolean")));
        }
        // A string constant cast to an interval of named fields is read by interval's input under
        // them, as PostgreSQL reads a constant of no type by its type's input and modifier: a bare
        // number in the last field's unit. A value of text, or a parameter, is read whole and cut.
        Expr::Cast {
            kind: CastKind::Cast | CastKind::DoubleColon,
            expr,
            data_type: data_type @ DataType::Interval { fields: Some(_), .. },
            ..
        } if string_constant(expr) => match (ev(expr)?, crate::catalog::held_type(data_type)?) {
            (Value::Text(s), ColumnType::Interval { range, precision }) => Value::Interval(
                crate::times::interval_in(&s, range, precision.map(u32::from))?,
            ),
            (v, _) => cast(v, data_type, frame.release, frame.casings.monetary(), true)?,
        },
        Expr::Cast {
            kind: CastKind::Cast | CastKind::DoubleColon,
            expr,
            data_type,
            ..
        } => cast(
            ev(expr)?,
            data_type,
            frame.release,
            frame.casings.monetary(),
            quoted(expr),
        )?,
        Expr::Array(a) => {
            let values = a
                .elem
                .iter()
                .map(|x| Ok((ev(x)?, quoted(x))))
                .collect::<Result<Vec<_>>>()?;
            let ty = match frame.type_of(e) {
                Some(t) => t,
                // Where nothing typed it, as a CHECK read alone: its elements' common type, as
                // PostgreSQL's transformArrayExpr finds it, text where every one is untyped.
                None if !a.elem.is_empty() && !a.elem.iter().any(|x| matches!(x, Expr::Array(_))) => {
                    let types: Vec<Option<ColumnType>> = values
                        .iter()
                        .map(|(v, q)| if *q { None } else { type_of_value(v) })
                        .collect();
                    crate::composed::type_of(crate::composed::Composed::Array {
                        element: super::typed::common_type(&types, "ARRAY")?,
                        domain: None,
                    })
                }
                None => return Err(Error::new("cannot determine type of empty array")),
            };
            array_value(ty, values, holds_arrays(a, frame))?
        }
        // An interval written as `interval '…'` with no fields named, read by its input.
        Expr::Interval(i)
            if i.leading_field.is_none()
                && i.last_field.is_none()
                && i.leading_precision.is_none()
                && i.fractional_seconds_precision.is_none() =>
        {
            match ev(&i.value)? {
                Value::Null => Value::Null,
                Value::Text(s) => Value::Interval(crate::times::interval_in(
                    &s,
                    crate::times::INTERVAL_FULL_RANGE,
                    None,
                )?),
                _ => return Err(Error::new(format!("{e} is not read"))),
            }
        }
        // A constant written after its type is read as that type's input reads it.
        Expr::TypedString(t) => match t.value.value.clone().into_string() {
            Some(s) => cast(
                Value::Text(s),
                &t.data_type,
                frame.release,
                frame.casings.monetary(),
                true,
            )?,
            None => return Err(Error::new(format!("{e} is not read"))),
        },
        // A number written with its minus is one constant, as PostgreSQL's grammar folds it.
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr,
        } if matches!(expr.as_ref(), Expr::Value(v) if matches!(v.value, SqlValue::Number(..))) => {
            let Expr::Value(v) = expr.as_ref() else {
                unreachable!()
            };
            let SqlValue::Number(s, _) = &v.value else {
                unreachable!()
            };
            numeric_literal(&format!("-{s}"))?
        }
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr,
        } => match ev(expr)? {
            Value::Null => Value::Null,
            Value::Int(x) => frame.held(
                e,
                Value::Int(
                    x.checked_neg()
                        .ok_or_else(|| Error::new("bigint out of range"))?,
                ),
            )?,
            Value::Num(m, s) => Value::Num(-m, s),
            Value::Numeric(n) => Value::Numeric(n.neg()),
            Value::Real(f) => Value::Real(-f),
            Value::Double(d) => Value::Double(-d),
            other @ Value::Interval(_) => super::clock::negate(&other)?.unwrap_or(Value::Null),
            other => return Err(Error::new(format!("-{other:?} is not a number"))),
        },
        Expr::UnaryOp {
            op:
                op @ (UnaryOperator::Plus
                | UnaryOperator::PGAbs
                | UnaryOperator::PGSquareRoot
                | UnaryOperator::PGCubeRoot),
            expr,
        } => super::ops::prefix(op, ev(expr)?, frame.type_of(expr), frame.type_of(e))?
            .unwrap_or(Value::Null),
        Expr::UnaryOp {
            op: UnaryOperator::BitwiseNot,
            expr,
        } => match ev(expr)? {
            Value::Null => Value::Null,
            Value::Int(x) => Value::Int(!x),
            Value::Bits(_, k) => {
                let k = crate::strings::bits_not(&k);
                Value::Bits(ColumnType::Bit(crate::strings::bits_of_key(&k).0), k)
            }
            Value::Scalar(t, k) if t.fixed_bytes().is_some() => {
                let bits = t.unsigned_bits();
                let v = !crate::scalars::unsigned_of(k, bits) & (u64::MAX >> (64 - bits));
                Value::Scalar(t, crate::scalars::unsigned_key(v, bits))
            }
            // An inet's or a cidr's bits inverted, an inet.
            Value::Typed(ColumnType::Textual(t), s) if t.is_network() => Value::Typed(
                ColumnType::Textual(crate::textual::Textual::Inet),
                crate::inet::inet_not(&s)?,
            ),
            other => return Err(Error::new(format!("~{other:?} is not read"))),
        },
        // A geometric value's prefix operators.
        Expr::UnaryOp {
            op:
                op @ (UnaryOperator::AtDashAt
                | UnaryOperator::Hash
                | UnaryOperator::DoubleAt
                | UnaryOperator::QuestionDash
                | UnaryOperator::QuestionPipe),
            expr,
        } => match ev(expr)? {
            Value::Null => Value::Null,
            Value::Geometric(g) => match crate::geo::geo_unary(&op.to_string(), &g) {
                Some(v) => of_geo(v?),
                None => {
                    return Err(Error::new(format!(
                        "operator does not exist: {op} {}{NO_PREFIX_OPERATOR}",
                        g.kind().name()
                    )))
                }
            },
            other => {
                return Err(Error::new(format!(
                    "operator does not exist: {op} {}{NO_PREFIX_OPERATOR}",
                    kind_name(&other)
                )))
            }
        },
        // A tsquery negated, a quoted literal read as one.
        Expr::UnaryOp {
            op: UnaryOperator::PGPrefixFactorial,
            expr,
        } => match ev(expr)? {
            Value::Null => Value::Null,
            Value::Typed(ColumnType::Textual(crate::textual::Textual::Tsquery), s) => Value::Typed(
                ColumnType::Textual(crate::textual::Textual::Tsquery),
                crate::tsearch::tsquery_not(&s)?,
            ),
            Value::Text(t) if quoted(expr) => Value::Typed(
                ColumnType::Textual(crate::textual::Textual::Tsquery),
                crate::tsearch::tsquery_not(&crate::tsearch::tsquery_in(&t)?)?,
            ),
            other => {
                return Err(Error::new(format!(
                    "operator does not exist: !! {}{NO_PREFIX_OPERATOR}",
                    kind_name(&other)
                )))
            }
        },
        Expr::UnaryOp {
            op: UnaryOperator::Not,
            expr,
        } => match holds(expr, row, frame, computed)? {
            None => Value::Null,
            Some(b) => Value::Bool(!b),
        },
        Expr::Like {
            negated,
            any: false,
            expr,
            pattern,
            escape_char,
        }
        | Expr::ILike {
            negated,
            any: false,
            expr,
            pattern,
            escape_char,
        } => {
            let escape = match escape_char {
                None => Some('\\'),
                Some(x) => match ev(x)? {
                    Value::Null => return Ok(Value::Null),
                    Value::Text(t) => crate::strings::like_escape(&t)?,
                    other => {
                        return Err(Error::new(format!(
                            "an ESCAPE of {} is not read",
                            kind_name(&other)
                        )))
                    }
                },
            };
            let (v, p) = (ev(expr)?, super::ops::char_as_text(ev(pattern)?));
            let v = super::ops::char_as_text(written_beside(frame, expr, v, pattern));
            if v.is_null() || p.is_null() {
                return Ok(Value::Null);
            }
            // A quoted pattern beside bytea is read as bytea.
            let p = match (&v, p) {
                (Value::Bytes(_), Value::Text(t)) if quoted(pattern) => {
                    Value::Bytes(crate::strings::bytea_in(&t)?)
                }
                (_, p) => p,
            };
            // Under the collation the text and the pattern settle.
            let ilike = match e {
                Expr::ILike { .. } => {
                    Some(frame.casings.input(e, "ILIKE")?.unwrap_or(Casing::Libc))
                }
                _ => None,
            };
            let under = frame.casings.compared(e, "LIKE")?;
            Value::Bool(like_of(&v, &p, escape, ilike, under)? != *negated)
        }
        // `a SIMILAR TO b [ESCAPE c]` is `a ~ similar_to_escape(b [, c])`, as PostgreSQL's grammar
        // writes it; NOT SIMILAR TO, `!~`.
        Expr::SimilarTo {
            negated,
            expr,
            pattern,
            escape_char,
        } => {
            let mut args = vec![ev(pattern)?];
            if let Some(c) = escape_char {
                args.push(ev(c)?);
            }
            let p = super::calls::regexps("similar_to_escape", &args, Casing::Libc)?
                .unwrap_or(Value::Null);
            let s = written_beside(frame, expr, ev(expr)?, pattern);
            let casing = regex_casing(frame, e, &[&s, &p])?;
            let op = if *negated {
                BinaryOperator::PGRegexNotMatch
            } else {
                BinaryOperator::PGRegexMatch
            };
            regex_match(&s, &p, &op, false, *negated, casing)?
        }
        Expr::AnyOp { left, right, .. } | Expr::AllOp { left, right, .. }
            if super::image::marked(right).is_some() =>
        {
            let (name, array) = super::image::marked(right).unwrap();
            let any = matches!(e, Expr::AnyOp { .. });
            super::image::over_array(name, ev(left)?, ev(array)?, left, array, any)?
        }
        Expr::AnyOp {
            left,
            compare_op,
            right,
            ..
        } => any_all(compare_op, ev(left)?, ev(right)?, left, true)?,
        Expr::AllOp {
            left,
            compare_op,
            right,
        } => any_all(compare_op, ev(left)?, ev(right)?, left, false)?,
        Expr::CompoundFieldAccess { root, access_chain } => {
            use sqlparser::ast::{AccessExpr, Subscript};
            let mut v = ev(root)?;
            let mut at: Vec<i64> = Vec::new();
            // Each subscript's lower and upper where any is a slice: a lone subscript the upper,
            // its lower 1.
            let mut slices: Vec<(Option<i64>, Option<i64>)> = Vec::new();
            let mut sliced = false;
            // A subscript as an integer of 32 bits, as PostgreSQL's parser coerces it by
            // assignment: a numeric or a float rounded, an oid or a reg* value taken as its bits,
            // a quoted literal or a parameter read by integer's input; none where it is NULL.
            let int = |e: &Expr| -> Result<Option<i64>> {
                let mut x = e;
                while let Expr::Nested(inner) = x {
                    x = inner;
                }
                let quoted = matches!(x, Expr::Value(v) if matches!(v.value,
                    SqlValue::SingleQuotedString(_)
                        | SqlValue::EscapedStringLiteral(_)
                        | SqlValue::Placeholder(_)));
                match ev(e)? {
                    Value::Int(i) if i32::try_from(i).is_ok() => Ok(Some(i)),
                    Value::Int(_) => Err(Error::new("integer out of range")),
                    Value::Null => Ok(None),
                    Value::Scalar(ty, k) if ty.is_oid() => match scalar_integer(ty, k, "integer")? {
                        Value::Int(i) => Ok(Some(i)),
                        _ => Ok(None),
                    },
                    v @ (Value::Num(..) | Value::Numeric(_) | Value::Real(_) | Value::Double(_)) => {
                        match part_value(ColumnType::Int4, &part_of(ColumnType::Int4, v, false)?) {
                            Value::Int(i) => Ok(Some(i)),
                            _ => Ok(None),
                        }
                    }
                    v @ Value::Text(_) if quoted => {
                        match part_value(ColumnType::Int4, &part_of(ColumnType::Int4, v, true)?) {
                            Value::Int(i) => Ok(Some(i)),
                            _ => Ok(None),
                        }
                    }
                    _ => Err(Error::new("array subscript must have type integer")),
                }
            };
            for a in access_chain {
                match a {
                    AccessExpr::Subscript(Subscript::Index { index }) => match int(index)? {
                        Some(i) => {
                            at.push(i);
                            slices.push((Some(1), Some(i)));
                        }
                        None => return Ok(Value::Null),
                    },
                    AccessExpr::Subscript(Subscript::Slice {
                        lower_bound,
                        upper_bound,
                        stride,
                    }) => {
                        if stride.is_some() {
                            return Err(Error::new(
                                "an array slice's stride is not read: PostgreSQL has none",
                            ));
                        }
                        let bound = |b: &Option<Expr>| -> Result<Option<Option<i64>>> {
                            match b {
                                None => Ok(Some(None)),
                                Some(e) => Ok(int(e)?.map(Some)),
                            }
                        };
                        let (Some(l), Some(u)) = (bound(lower_bound)?, bound(upper_bound)?) else {
                            return Ok(Value::Null);
                        };
                        sliced = true;
                        at.push(0);
                        slices.push((l, u));
                    }
                    AccessExpr::Dot(Expr::Identifier(i)) if at.is_empty() => {
                        v = match v {
                            Value::Null => Value::Null,
                            Value::Composed(ty, c) => {
                                let (fty, item) = crate::composed::field(ty, &c.item, &crate::catalog::ident(i))?;
                                part_value(fty, &item)
                            }
                            other => return Err(Error::new(format!("column notation .{} applied to type {}, which is not a composite type", i.value, kind_name(&other)))),
                        };
                    }
                    other => return Err(Error::new(format!("{other} is not read"))),
                }
            }
            if at.len() > 6 {
                return Err(Error::new(format!(
                    "number of array dimensions ({}) exceeds the maximum allowed (6)",
                    at.len()
                )));
            }
            if !at.is_empty() {
                v = match v {
                    Value::Null => Value::Null,
                    Value::Composed(ty, c) => match ty {
                        ColumnType::Composed(r) => match r.def() {
                            crate::composed::Composed::Array { .. } if sliced => Value::Composed(
                                ty,
                                std::sync::Arc::new(crate::composed::Compound::new(
                                    ty,
                                    crate::composed::slice(&c.item, &slices),
                                )),
                            ),
                            crate::composed::Composed::Vector { element } if sliced => {
                                let array = super::typed::array_of(*element);
                                Value::Composed(
                                    array,
                                    std::sync::Arc::new(crate::composed::Compound::new(
                                        array,
                                        crate::composed::slice(&c.item, &slices),
                                    )),
                                )
                            }
                            crate::composed::Composed::Array { element, .. }
                            | crate::composed::Composed::Vector { element } => {
                                part_value(*element, &crate::composed::subscript(&c.item, &at))
                            }
                            _ => {
                                return Err(Error::new(format!(
                                "cannot subscript type {} because it does not support subscripting",
                                ty.pg_name()
                            )))
                            }
                        },
                        _ => unreachable!(),
                    },
                    other => match raw_elements(&other) {
                        Some(_) if sliced => {
                            return Err(Error::coded(
                                "0A000",
                                "slices of fixed-length arrays not implemented",
                            ))
                        }
                        Some(elements) => match at.as_slice() {
                            [i] => usize::try_from(*i)
                                .ok()
                                .and_then(|i| elements.into_iter().nth(i))
                                .unwrap_or(Value::Null),
                            _ => Value::Null,
                        },
                        None => {
                            return Err(Error::new(format!(
                                "cannot subscript type {} because it does not support \
                                 subscripting",
                                kind_name(&other)
                            )))
                        }
                    },
                };
            }
            v
        }
        // True where some value of the list equals it, else NULL where it or a value is NULL.
        Expr::InList {
            expr,
            list,
            negated,
        } => {
            // A row IN a list of rows is the OR of its rows' comparisons, as PostgreSQL's parser
            // writes it; where it is not, refused in its words.
            if row_parts(expr).is_some() {
                if let Some(written) = super::spelled::rows_in(e) {
                    return eval(&written, row, frame, computed);
                }
                super::spelled::rows_in_refused(e, &mut |x, with| match with {
                    Some((op, field)) => {
                        let compared = Expr::BinaryOp {
                            left: Box::new(field.clone()),
                            op: op.clone(),
                            right: Box::new(x.clone()),
                        };
                        eval(&compared, row, frame, computed).map(|_| ())
                    }
                    None if super::image::unknown(x) => Err(Error::new(
                        "input of anonymous composite types is not implemented",
                    )),
                    None => Err(Error::new(format!(
                        "operator does not exist: record = {}{NO_OPERATOR}",
                        kind_name(&ev(x)?)
                    ))),
                })?;
                return Err(Error::new(format!("{expr} is not read")));
            }
            let v = ev(expr)?;
            if v.is_null() {
                return Ok(Value::Null);
            }
            let mut unknown = false;
            for x in list {
                let (a, b) = beside(written_beside(frame, expr, v.clone(), x), ev(x)?, expr, x)?;
                // Each value compared under the collation the list settles, a NULL under none.
                let under = match b.is_null() {
                    true => None,
                    false => frame.casings.compared(e, "string comparison")?,
                };
                match compare_under(under, &a, &b)? {
                    Some(Ordering::Equal) => return Ok(Value::Bool(!*negated)),
                    None => unknown = true,
                    Some(_) => {}
                }
            }
            if unknown {
                Value::Null
            } else {
                Value::Bool(*negated)
            }
        }
        Expr::IsNull(x) => Value::Bool(null_test(&ev(x)?, false)),
        Expr::IsNotNull(x) => Value::Bool(null_test(&ev(x)?, true)),
        // A truth tested is never NULL: an unknown truth is neither true nor false.
        Expr::IsTrue(x) => Value::Bool(holds(x, row, frame, computed)? == Some(true)),
        Expr::IsNotTrue(x) => Value::Bool(holds(x, row, frame, computed)? != Some(true)),
        Expr::IsFalse(x) => Value::Bool(holds(x, row, frame, computed)? == Some(false)),
        Expr::IsNotFalse(x) => Value::Bool(holds(x, row, frame, computed)? != Some(false)),
        Expr::IsUnknown(x) => Value::Bool(holds(x, row, frame, computed)?.is_none()),
        Expr::IsNotUnknown(x) => Value::Bool(holds(x, row, frame, computed)?.is_some()),
        Expr::IsDistinctFrom(a, b) | Expr::IsNotDistinctFrom(a, b) => {
            let (x, y) = (
                written_beside(frame, a, ev(a)?, b),
                written_beside(frame, b, ev(b)?, a),
            );
            let (x, y) = (integer_read(frame, a, x, b)?, integer_read(frame, b, y, a)?);
            let (x, y) = beside(x, y, a, b)?;
            let same = match (x.is_null(), y.is_null()) {
                (true, true) => true,
                (false, false) => {
                    compare_under(frame.casings.compared(e, "string comparison")?, &x, &y)?
                        == Some(Ordering::Equal)
                }
                _ => false,
            };
            Value::Bool(same == matches!(e, Expr::IsNotDistinctFrom(..)))
        }
        // As PostgreSQL's parser rewrites it: `x >= a AND x <= b`, NOT BETWEEN `x < a OR x > b`;
        // SYMMETRIC both orders of the bounds or'd, NOT BETWEEN SYMMETRIC both and'd. Each
        // comparison is read on the BETWEEN's own sides, a side settling it read alone.
        Expr::Between {
            expr,
            negated,
            low,
            high,
        } => {
            let (low, symmetric) = super::question::between_low(low);
            let one = |a: &Expr, b: &Expr| -> Result<Option<bool>> {
                let (first, second) = match negated {
                    false => (BinaryOperator::GtEq, BinaryOperator::LtEq),
                    true => (BinaryOperator::Lt, BinaryOperator::Gt),
                };
                let x = truth(&operated(e, expr, &first, a, row, frame, computed)?)?;
                if x == Some(*negated) {
                    return Ok(x);
                }
                let y = truth(&operated(e, expr, &second, b, row, frame, computed)?)?;
                Ok(joined(!*negated, x, y))
            };
            let v = match one(low, high)? {
                v if !symmetric || v == Some(!*negated) => v,
                v => joined(*negated, v, one(high, low)?),
            };
            v.map_or(Value::Null, Value::Bool)
        }
        Expr::BinaryOp { left, op, right } if super::image::operator(op).is_some() => {
            let name = super::image::operator(op).unwrap_or_default();
            super::image::answer(name, left, right, &ev)?
        }
        Expr::BinaryOp { left, op, right } => {
            if let (BinaryOperator::AtAt, Some(inner)) =
                (op, super::spelled::right_of_three_ats(right))
            {
                let (x, y) = (ev(left)?, ev(inner)?);
                let searched = |v: &Value| {
                    matches!(
                        v,
                        Value::Typed(
                            ColumnType::Textual(
                                crate::textual::Textual::Tsvector
                                    | crate::textual::Textual::Tsquery
                            ),
                            _
                        )
                    )
                };
                if !searched(&x) && !searched(&y) && !x.is_null() && !y.is_null() {
                    return Err(Error::new(format!(
                        "operator does not exist: {} @@@ {}{NO_OPERATOR}",
                        kind_name(&x),
                        kind_name(&y)
                    )));
                }
                let matched = Expr::BinaryOp {
                    left: left.clone(),
                    op: op.clone(),
                    right: Box::new(inner.clone()),
                };
                return eval(&matched, row, frame, computed);
            }
            if row_parts(left).is_some() && row_parts(right).is_some() && !matches!(op, BinaryOperator::Overlaps) {
                // Two rows compared by `=` or `<>` are their pairs' comparisons, as PostgreSQL's
                // parser writes them; of two widths, or of none, refused in its words.
                if let Some(written) = super::spelled::rows_compared(e) {
                    return eval(&written, row, frame, computed);
                }
                if let (Some(l), Some(r)) = (row_parts(left), row_parts(right)) {
                    let (l, r) = (l?, r?);
                    super::image::rows_agree(l.len(), r.len())?;
                    if matches!(
                        op,
                        BinaryOperator::Lt
                            | BinaryOperator::LtEq
                            | BinaryOperator::Gt
                            | BinaryOperator::GtEq
                    ) {
                        return rows_by_order(e, &l, op, &r, row, frame, computed);
                    }
                }
                return Err(Error::new(format!("{left} is not read")));
            }
            match op {
                BinaryOperator::And | BinaryOperator::Or => {
                    let and = matches!(op, BinaryOperator::And);
                    // Its right side read only where its left does not settle it, as the executor
                    // reads a list of conditions; a pattern written out PostgreSQL's planner reads
                    // before any row.
                    let x = holds(left, row, frame, computed)?;
                    if x == Some(!and) && !super::ops::pattern_written(right) {
                        return Ok(Value::Bool(!and));
                    }
                    let y = holds(right, row, frame, computed)?;
                    return Ok(match (x, y) {
                        (Some(a), Some(b)) => Value::Bool(if and { a && b } else { a || b }),
                        (Some(false), None) | (None, Some(false)) if and => Value::Bool(false),
                        (Some(true), None) | (None, Some(true)) if !and => Value::Bool(true),
                        _ => Value::Null,
                    });
                }
                BinaryOperator::Overlaps => {
                    let (Expr::Tuple(a), Expr::Tuple(b)) = (left.as_ref(), right.as_ref()) else {
                        return Err(Error::new(format!("{e}: OVERLAPS reads two pairs")));
                    };
                    let ([s1, e1], [s2, e2]) = (a.as_slice(), b.as_slice()) else {
                        return Err(Error::new(format!("{e}: OVERLAPS reads two pairs")));
                    };
                    return super::calls::overlaps([ev(s1)?, ev(e1)?, ev(s2)?, ev(e2)?], frame.now);
                }
                BinaryOperator::PGRegexMatch
                | BinaryOperator::PGRegexIMatch
                | BinaryOperator::PGRegexNotMatch
                | BinaryOperator::PGRegexNotIMatch => {
                    let icase = matches!(
                        op,
                        BinaryOperator::PGRegexIMatch | BinaryOperator::PGRegexNotIMatch
                    );
                    let not = matches!(
                        op,
                        BinaryOperator::PGRegexNotMatch | BinaryOperator::PGRegexNotIMatch
                    );
                    let (s, p) = (ev(left)?, ev(right)?);
                    let s = written_beside(frame, left, s, right);
                    let casing = regex_casing(frame, e, &[&s, &p])?;
                    return regex_match(&s, &p, op, icase, not, casing);
                }
                _ => {}
            }
            operated(e, left, op, right, row, frame, computed)?
        }
        Expr::AtTimeZone {
            timestamp,
            time_zone,
        } => super::clock::at_zone(ev(timestamp)?, ev(time_zone)?)?,
        Expr::Substring {
            expr,
            substring_from,
            substring_for,
            shorthand,
            ..
        } => {
            // Its form as PostgreSQL resolves it by its arguments' types: a string of text from a
            // text is substring(text, text), by a POSIX pattern, and with a text for its end too
            // substring(text, text, text), by a SIMILAR one; else a form of integers, a quoted
            // literal read as an integer. `SUBSTRING(s FOR e)` is substring(s, 1, e::int4) (gram.y,
            // substr_list), and SUBSTR a function of integers alone.
            let string = ev(expr)?;
            let parts: Vec<(Value, &Expr)> = [substring_from, substring_for]
                .into_iter()
                .flatten()
                .map(|x| Ok((ev(x)?, x.as_ref())))
                .collect::<Result<_>>()?;
            let texty = |v: &Value| matches!(v, Value::Text(_) | Value::Str(..));
            let named = |v: &Value, x: &Expr| {
                if quoted(x) {
                    "unknown".to_string()
                } else {
                    kind_name(v)
                }
            };
            let missing = || {
                let types: Vec<String> = std::iter::once(named(&string, expr))
                    .chain(parts.iter().map(|(v, x)| named(v, x)))
                    .collect();
                Error::new(format!(
                    "function {}({}) does not exist",
                    if *shorthand {
                        "substr"
                    } else {
                        "pg_catalog.substring"
                    },
                    types.join(", ")
                ))
            };
            let of_text = matches!(string, Value::Text(_) | Value::Str(..) | Value::Null);
            let integral = |v: &Value| matches!(v, Value::Int(x) if i32::try_from(*x).is_ok());
            let by_pattern = !*shorthand
                && of_text
                && substring_from.is_some()
                && parts.iter().all(|(v, _)| texty(v) || v.is_null())
                && parts.iter().any(|(v, _)| texty(v));
            if !by_pattern {
                let held = matches!(
                    string,
                    Value::Text(_)
                        | Value::Str(..)
                        | Value::Null
                        | Value::Bytes(_)
                        | Value::Bits(..)
                );
                if !held || (matches!(string, Value::Bits(..)) && *shorthand) {
                    return Err(missing());
                }
                // Each argument as an integer: a quoted literal read as int4 reads it, and FOR
                // alone cast to int4.
                let alone = substring_from.is_none();
                let mut args = vec![string.clone()];
                if alone {
                    args.push(Value::Int(1));
                }
                for (v, x) in &parts {
                    args.push(match v {
                        Value::Null => Value::Null,
                        v if integral(v) => v.clone(),
                        Value::Text(t) | Value::Str(_, t) if quoted(x) || alone => {
                            Value::Int(crate::composed::int_in(t, 32)?)
                        }
                        Value::Bool(b) if alone => Value::Int(i64::from(*b)),
                        v if alone => cast(
                            v.clone(),
                            &sqlparser::ast::DataType::Int4(None),
                            frame.release,
                            frame.casings.monetary(),
                            false,
                        )?,
                        _ => return Err(missing()),
                    });
                }
                // A bit string's, as bitsubstr takes it.
                let given: Vec<_> = args.iter().map(|v| (v.clone(), false, None)).collect();
                if let Some(v) = super::rest::call("substring", &given)? {
                    return Ok(v);
                }
                return super::calls::strings("substr", &args, Casing::Libc)?.ok_or_else(missing);
            }
            // A pattern alone is a regular expression, under the collation the text and the pattern
            // settle; with its escape it is SQL's, which PostgreSQL's function of SQL reads under
            // the database's default whatever its arguments' collations.
            let casing = match parts.get(1) {
                Some(_) => Casing::Libc,
                None => regex_casing(frame, e, &[&string, &parts[0].0])?,
            };
            super::calls::substring_by_pattern(
                &string,
                &parts[0].0,
                parts.get(1).map(|(v, _)| v),
                casing,
            )?
        }
        Expr::Position { expr, r#in } => {
            let bits = [(ev(r#in)?, false, None), (ev(expr)?, false, None)];
            if let Some(v) = super::rest::call("position", &bits)? {
                return Ok(v);
            }
            // Under the collation the string and the substring settle.
            let values = [bits[0].0.clone(), bits[1].0.clone()];
            let casing = string_casing(frame, e, "position", &values)?;
            super::calls::substring_search("position", casing)?;
            let [(s, ..), (sub, ..)] = bits;
            super::calls::position(s, sub)?
        }
        Expr::Trim {
            expr,
            trim_where,
            trim_what,
            trim_characters,
        } => {
            let name = match trim_where {
                Some(sqlparser::ast::TrimWhereField::Leading) => "ltrim",
                Some(sqlparser::ast::TrimWhereField::Trailing) => "rtrim",
                _ => "btrim",
            };
            let mut args = vec![ev(expr)?];
            match (trim_what, trim_characters.as_deref()) {
                (Some(w), _) => args.push(ev(w)?),
                (None, Some([c])) => args.push(ev(c)?),
                (None, None) => {}
                _ => return Err(Error::new(format!("{e} is not read"))),
            }
            super::calls::strings(name, &args, Casing::Libc)?
                .ok_or_else(|| Error::new(format!("{e} is not read")))?
        }
        Expr::Overlay {
            expr,
            overlay_what,
            overlay_from,
            overlay_for,
        } => {
            let (s, t, from) = (ev(expr)?, ev(overlay_what)?, ev(overlay_from)?);
            let count = overlay_for.as_deref().map(&ev).transpose()?;
            let given: Vec<_> = [&s, &t, &from]
                .into_iter()
                .chain(count.as_ref())
                .map(|v| (v.clone(), false, None))
                .collect();
            if let Some(v) = super::rest::call("overlay", &given)? {
                return Ok(v);
            }
            super::calls::overlay(s, t, from, count)?
        }
        Expr::Extract { field, expr, .. } => {
            let unit = super::calls::field_unit(field);
            super::calls::extract(&unit, &ev(expr)?)?
                .ok_or_else(|| Error::new(format!("{e} is not read")))?
        }
        Expr::Ceil {
            expr,
            field:
                sqlparser::ast::CeilFloorKind::DateTimeField(sqlparser::ast::DateTimeField::NoDateTime),
        }
        | Expr::Floor {
            expr,
            field:
                sqlparser::ast::CeilFloorKind::DateTimeField(sqlparser::ast::DateTimeField::NoDateTime),
        } => {
            let name = if matches!(e, Expr::Ceil { .. }) {
                "ceil"
            } else {
                "floor"
            };
            super::calls::numbers(name, &[ev(expr)?], &[frame.type_of(expr)], frame.type_of(e))?
                .ok_or_else(|| Error::new(format!("{e} is not read")))?
        }
        Expr::Tuple(_) | Expr::Function(_) if row_parts(e).is_some() => {
            let parts = row_parts(e).unwrap()?;
            let values = parts.iter().map(|x| ev(x)).collect::<Result<Vec<_>>>()?;
            let told: Vec<_> = parts
                .iter()
                .zip(&values)
                .map(|(x, v)| {
                    frame
                        .type_of(x)
                        .or_else(|| (!super::image::unknown(x)).then(|| type_of_value(v)).flatten())
                })
                .collect();
            let fields = super::image::field_types(&parts, &told).ok_or_else(|| Error::new(format!("{e} is not read")))?;
            let items = values
                .into_iter()
                .zip(&fields)
                .zip(&parts)
                .map(|((v, f), x)| part_of(f.0, v, quoted(x)))
                .collect::<Result<Vec<_>>>()?;
            // Its type as the step's typing made it, its fields' domains among it; a relation's
            // whole row's its row type.
            let ty = frame
                .type_of(e)
                .filter(|t| match e {
                    Expr::Tuple(_) => super::image::is_record_of(*t, &fields),
                    _ => super::image::is_anonymous(*t, &fields),
                })
                .unwrap_or_else(|| super::image::anonymous(&fields));
            composed(ty, crate::composed::Item::Record(items))
        }
        Expr::Function(f) => {
            let name = function_name(f);
            if name == "make_interval" {
                let values = super::calls::named_interval(f, |x| ev(x))?;
                return Ok(
                    super::calls::time(&name, &values, frame.type_of(e), frame.now)?
                        .unwrap_or(Value::Null),
                );
            }
            let a = args(f)?;
            if let ("record_image_eq" | "record_image_ne", [x, y]) = (name.as_str(), a.as_slice()) {
                return super::image::records_compared(name == "record_image_eq", ev(x)?, ev(y)?);
            }
            if super::handed::is_handed(f) {
                let given = a
                    .iter()
                    .map(|x| Ok((ev(x)?, quoted(x))))
                    .collect::<Result<Vec<_>>>()?;
                let types: Vec<_> = a.iter().map(|x| frame.type_of(x)).collect();
                if let Some(v) = super::handed::call(f, given, &types)? {
                    return Ok(v);
                }
            }
            if f.name.0.len() == 1 {
                if let Some(v) = chosen(e, &name, &a, row, frame, computed)? {
                    return Ok(v);
                }
            }
            match (name.as_str(), a.as_slice()) {
                // The collation its argument's call is made under, by its name; NULL for an
                // argument of no type, or one two implicit collations leave unsettled.
                ("pg_collation_for", [x]) => {
                    ev(x)?;
                    match frame.type_of(x) {
                        None => Value::Null,
                        Some(t) if !super::collate::collatable(t) => {
                            return Err(Error::new(format!(
                                "collations are not supported by type {}",
                                t.pg_name()
                            )))
                        }
                        Some(_) => match frame.casings.called_under(e) {
                            Some(n) => Value::Text(frame.casings.written_collation(n)),
                            None => Value::Null,
                        },
                    }
                }
                // Under the collation its argument carries, the database's default where none.
                ("upper" | "lower", [x]) => {
                    match ev(x)? {
                        Value::Null => Value::Null,
                        v @ (Value::Text(_) | Value::Str(..)) => Value::Text(crate::strings::case(
                            std::str::from_utf8(v.string_key().unwrap()).unwrap_or_default(),
                            name == "upper",
                            frame
                                .casings
                                .input(e, &format!("{name}() function"))?
                                .unwrap_or(Casing::Libc),
                        )?),
                        v @ Value::Scalar(ColumnType::Char, _) => {
                            Value::Text(crate::strings::case(
                                &v.text().unwrap_or_default(),
                                name == "upper",
                                Casing::Libc,
                            )?)
                        }
                        // A range's or a multirange's bound.
                        v @ Value::Composed(..) => {
                            match super::ranges::ranges(&name, &[(v.clone(), false)])? {
                                Some(b) => b,
                                None => {
                                    return Err(Error::new(format!(
                                        "function {name}({}) does not exist",
                                        kind_name(&v)
                                    )))
                                }
                            }
                        }
                        other => {
                            return Err(Error::new(format!(
                                "function {name}({}) does not exist",
                                kind_name(&other)
                            )))
                        }
                    }
                }
                ("timezone", [zone, x]) => super::clock::at_zone(ev(x)?, ev(zone)?)?,
                ("array_append" | "array_prepend" | "array_cat", [x, y]) => {
                    array_joined(&name, (ev(x)?, x), (ev(y)?, y), frame.type_of(e))?
                }
                _ => {
                    let values = a.iter().map(|x| ev(x)).collect::<Result<Vec<_>>>()?;
                    if let Some(v) = super::sequences::call(&name, &values)? {
                        return Ok(v);
                    }
                    if let Some(v) = super::prng::call(&name, &values, frame.type_of(e))? {
                        return Ok(v);
                    }
                    let types: Vec<Option<ColumnType>> =
                        a.iter().map(|x| frame.type_of(x)).collect();
                    let given: Vec<(Value, bool)> = values
                        .iter()
                        .cloned()
                        .zip(a.iter().map(|x| quoted(x)))
                        .collect();
                    if let Some(v) = super::tsearch::setting(&name, &given, &frame.casings)? {
                        return Ok(v);
                    }
                    let given = super::tsearch::named_setting(&name, given, &frame.casings)?;
                    let resolved: Vec<_> = given
                        .iter()
                        .cloned()
                        .zip(&types)
                        .map(|((v, q), t)| (v, q, *t))
                        .collect();
                    if let Some(v) = super::rest::call(&name, &resolved)? {
                        return Ok(v);
                    }
                    if let Some(v) = super::ranges::ranges(&name, &given)? {
                        return Ok(v);
                    }
                    if let Some(v) = super::tsearch::call(&name, &given)? {
                        return Ok(v);
                    }
                    if let Some(v) = super::arrays::arrays(&name, &given, &types)? {
                        return Ok(v);
                    }
                    if let Some(v) = super::populate::call(&name, &given, &types)? {
                        return Ok(v);
                    }
                    if let Some(v) = super::json::call(&name, &given)? {
                        return Ok(v);
                    }
                    if let Some(v) =
                        super::calls::numbers(&name, &values, &types, frame.type_of(e))?
                    {
                        return Ok(v);
                    }
                    if let Some(v) =
                        super::calls::time(&name, &values, frame.type_of(e), frame.now)?
                    {
                        return Ok(v);
                    }
                    // Under the collation its arguments settle.
                    let casing = string_casing(frame, e, &name, &values)?;
                    if let Some(v) = super::calls::strings(&name, &values, casing)? {
                        return Ok(v);
                    }
                    if super::calls::is_regexp_function(&name) {
                        let casing = regex_casing(frame, e, &values.iter().collect::<Vec<_>>())?;
                        if let Some(v) = super::calls::regexps(&name, &values, casing)? {
                            return Ok(v);
                        }
                    }
                    if let Some(v) = super::calls::formatting(
                        &name,
                        &values,
                        &types,
                        frame.casings.monetary().conv(),
                    )? {
                        return Ok(v);
                    }
                    if let Some(v) = super::updatable::call(&name, &values)? {
                        return Ok(v);
                    }
                    if let (Some(kind), [v]) =
                        (crate::regs::visibility_kind(&name), values.as_slice())
                    {
                        let visible = crate::deparse::oid_of(v)
                            .and_then(|oid| crate::regs::is_visible(kind, oid, frame.release));
                        return Ok(visible.map_or(Value::Null, Value::Bool));
                    }
                    if name == "obj_description" && matches!(values.len(), 1 | 2) {
                        if values.iter().any(Value::is_null) {
                            return Ok(Value::Null);
                        }
                        let class = values.get(1).map(|v| v.text().unwrap_or_default());
                        let found = crate::deparse::oid_of(&values[0]).and_then(|oid| {
                            crate::pgcatalog::description(oid, class.as_deref(), frame.release)
                        });
                        return Ok(found.map_or(Value::Null, Value::Text));
                    }
                    if name == "col_description" && values.len() == 2 {
                        let column = match &values[1] {
                            Value::Int(n) => i32::try_from(*n).ok(),
                            _ => None,
                        };
                        let found = crate::deparse::oid_of(&values[0])
                            .zip(column)
                            .and_then(|(oid, c)| {
                                crate::pgcatalog::column_description(oid, c, frame.release)
                            });
                        return Ok(found.map_or(Value::Null, Value::Text));
                    }
                    if let Some(v) = crate::deparse::call(&name, &values)? {
                        return Ok(v);
                    }
                    crate::functions::refuse_server(&name, frame.release)?;
                    return Err(Error::new(format!("{e} is not read")));
                }
            }
        }
        other => return Err(Error::new(format!("{other} is not read"))),
    })
}

/// A relation read by name: a step the WITH made before, or the table.
fn relation<'a>(
    name: &str,
    steps: &'a HashMap<String, Relation>,
    table: &'a Table,
) -> Result<&'a Relation> {
    if let Some(r) = steps.get(name) {
        return Ok(r);
    }
    if table.name == name || table.name.rsplit('.').next() == Some(name) {
        return Ok(&table.relation);
    }
    Err(Error::new(format!(
        "{name} is neither a step of the WITH nor the table"
    )))
}

/// The columns a FROM reads, each under its alias, with their types where known: each relation's
/// by its name in `typed`.
fn typed_from(
    items: &[TableWithJoins],
    typed: &HashMap<String, (Vec<String>, Vec<Option<ColumnType>>)>,
    table: &str,
) -> Vec<super::typed::TypedColumn> {
    let mut out = Vec::new();
    for item in items {
        for t in std::iter::once(&item.relation).chain(item.joins.iter().map(|j| &j.relation)) {
            let TableFactor::Table { name, alias, .. } = t else {
                continue;
            };
            let n = crate::catalog::object_name(name);
            let of = typed.get(&n).or_else(|| {
                (table == n || table.rsplit('.').next() == Some(n.as_str()))
                    .then(|| typed.get(table))
                    .flatten()
            });
            let Some((names, types)) = of else {
                continue;
            };
            // The table named without an alias answers by its schema and name too.
            let base = (n == table || table.rsplit('.').next() == Some(n.as_str()))
                && (!typed.contains_key(&n) || n == table);
            let q = match alias {
                Some(a) => ident(&a.name),
                None if base => format!(
                    "{}{TABLE}{}",
                    n.rsplit('.').next().unwrap_or(&n),
                    crate::oids::full(table)
                ),
                None => n.rsplit('.').next().unwrap_or(&n).to_string(),
            };
            for (c, t) in names.iter().zip(types) {
                out.push((Some(q.clone()), c.clone(), *t));
            }
        }
    }
    out
}

/// Each column a step reads of its FROM's, refused before any row in PostgreSQL's words where none
/// or two answer to it: a qualifier no relation goes by an invalid reference (the alias that hides
/// the table in a HINT, or the entry found) or a missing entry; a column its relation lacks, with
/// the HINT naming one spelled alike.
fn step_names(
    p: &Prepared,
    columns: &[super::typed::TypedColumn],
    items: &[TableWithJoins],
    typed: &HashMap<String, (Vec<String>, Vec<Option<ColumnType>>)>,
    table: &str,
) -> Result<()> {
    let mut relations: Vec<(String, Vec<String>)> = Vec::new();
    for (q, n, _) in columns {
        let name = q.as_deref().map(unmarked).unwrap_or_default().to_string();
        match relations.iter_mut().find(|(r, _)| *r == name) {
            Some((_, cs)) => cs.push(n.clone()),
            None => relations.push((name, vec![n.clone()])),
        }
    }
    let missing = |q: Option<&str>, n: &str| {
        let relation = q.map(relation_named);
        let words = match relation {
            Some(r) => format!("column {r}.{n} does not exist"),
            None => format!("column \"{n}\" does not exist"),
        };
        let hint = super::hint::column_hint(&relations, relation, n).unwrap_or_default();
        Error::new(format!("{words}{hint}"))
    };
    for (q, n) in p.reads() {
        let found = naming(
            columns.iter().map(|(q, n, _)| (q.as_deref(), n.as_str())),
            q.as_deref(),
            &n,
        );
        let Some(q) = q.filter(|_| found.is_empty()) else {
            match found.len() {
                // A relation's name alone is its whole row.
                0 if relations.iter().any(|(r, _)| *r == n) => continue,
                0 => return Err(missing(None, &n)),
                1 => continue,
                _ => return Err(Error::new(format!("column reference \"{n}\" is ambiguous"))),
            }
        };
        if let Some(e) = too_many_names(&q, &n) {
            return Err(e);
        }
        if columns.iter().any(|(c, _, _)| answers_to(c.as_deref(), &q)) {
            return Err(missing(Some(&q), &n));
        }
        return Err(no_entry_of_step(&q, items, typed, table));
    }
    Ok(())
}

/// The refusal of a qualifier no relation of a step's FROM goes by, as `relations.rs`'s
/// `no_entry` words it.
fn no_entry_of_step(
    q: &str,
    items: &[TableWithJoins],
    typed: &HashMap<String, (Vec<String>, Vec<Option<ColumnType>>)>,
    table: &str,
) -> Error {
    let relation = relation_named(q);
    let by_schema = q.strip_prefix(TABLE);
    let entry = |name: &str| {
        Error::new(format!(
            "invalid reference to FROM-clause entry for table \"{relation}\": There is an entry \
             for table \"{name}\", but it cannot be referenced from this part of the query."
        ))
    };
    let mut goes_by = false;
    for item in items {
        for t in std::iter::once(&item.relation).chain(item.joins.iter().map(|j| &j.relation)) {
            let TableFactor::Table { name, alias, .. } = t else {
                continue;
            };
            let n = crate::catalog::object_name(name);
            let base = (n == table || table.rsplit('.').next() == Some(n.as_str()))
                && (!typed.contains_key(&n) || n == table);
            let bare = n.rsplit('.').next().unwrap_or(&n).to_string();
            let finds = match by_schema {
                Some(full) => base && crate::oids::full(table) == full,
                None if base => crate::catalog::found_by_qualifier(table, q),
                None => n == q,
            };
            let named = alias.as_ref().map(|a| ident(&a.name));
            match named {
                Some(a) if finds && a != q && a == relation => return entry(&a),
                Some(a) if finds && a != q => {
                    return Error::new(format!(
                        "invalid reference to FROM-clause entry for table \"{relation}\"\nHINT:  \
                         Perhaps you meant to reference the table alias \"{a}\"."
                    ))
                }
                _ => {}
            }
            goes_by |= by_schema.is_some() && named.unwrap_or(bare) == relation;
        }
    }
    match goes_by {
        true => entry(relation),
        false => Error::new(format!(
            "missing FROM-clause entry for table \"{relation}\""
        )),
    }
}

/// The names a FROM reads the table under: each alias it gives the table, or the table's own.
fn base_qualifiers(
    items: &[TableWithJoins],
    typed: &HashMap<String, (Vec<String>, Vec<Option<ColumnType>>)>,
    table: &str,
) -> Vec<String> {
    let mut out = Vec::new();
    for item in items {
        for t in std::iter::once(&item.relation).chain(item.joins.iter().map(|j| &j.relation)) {
            let TableFactor::Table { name, alias, .. } = t else {
                continue;
            };
            let n = crate::catalog::object_name(name);
            let base = !typed.contains_key(&n) || n == table;
            if base && (table == n || table.rsplit('.').next() == Some(n.as_str())) {
                out.push(
                    alias
                        .as_ref()
                        .map(|a| ident(&a.name))
                        .unwrap_or_else(|| n.rsplit('.').next().unwrap_or(&n).to_string()),
                );
            }
        }
    }
    out
}

/// The columns a FROM reads, each under its alias, with the collation it carries: a step's by its
/// place among the step's columns, the table's by its name among the table's.
fn carried_from(
    items: &[TableWithJoins],
    typed: &HashMap<String, (Vec<String>, Vec<Option<ColumnType>>)>,
    relations: &HashMap<String, Vec<(String, super::collate::Carried)>>,
    table: &str,
) -> Vec<(Option<String>, String, super::collate::Carried)> {
    let mut out = Vec::new();
    for item in items {
        for t in std::iter::once(&item.relation).chain(item.joins.iter().map(|j| &j.relation)) {
            let TableFactor::Table { name, alias, .. } = t else {
                continue;
            };
            let n = crate::catalog::object_name(name);
            let key = if typed.contains_key(&n) {
                n.clone()
            } else if table == n || table.rsplit('.').next() == Some(n.as_str()) {
                table.to_string()
            } else {
                continue;
            };
            let (Some((names, _)), Some(carried)) = (typed.get(&key), relations.get(&key)) else {
                continue;
            };
            let q = alias
                .as_ref()
                .map(|a| ident(&a.name))
                .unwrap_or_else(|| n.rsplit('.').next().unwrap_or(&n).to_string());
            for (i, c) in names.iter().enumerate() {
                let found = match carried.len() == names.len() {
                    true => carried.get(i),
                    false => carried.iter().find(|(m, _)| m == c),
                };
                let collation = found.map_or(super::collate::Carried::None, |(_, k)| k.clone());
                out.push((Some(q.clone()), c.clone(), collation));
            }
        }
    }
    out
}

/// Columns by their qualifier and name.
pub(super) type Columns = Vec<(Option<String>, String)>;

/// The rows a FROM reads: one relation, or several crossed, each under its alias.
fn from(
    items: &[TableWithJoins],
    steps: &HashMap<String, Relation>,
    table: &Table,
) -> Result<(Relation, Columns)> {
    let mut parts: Vec<(Relation, String)> = Vec::new();
    let mut add = |t: &TableFactor| -> Result<()> {
        match t {
            TableFactor::Table { name, alias, .. } => {
                let n = crate::catalog::object_name(name);
                let r = relation(&n, steps, table)?.clone();
                let bare = n.rsplit('.').next().unwrap_or(&n).to_string();
                // The table named without an alias answers by its schema and name too.
                let q = match alias {
                    Some(a) => ident(&a.name),
                    None if !steps.contains_key(&n) => {
                        format!("{bare}{TABLE}{}", crate::oids::full(&table.name))
                    }
                    None => bare,
                };
                parts.push((r, q));
                Ok(())
            }
            _ => Err(Error::new("a step reads tables and steps by name")),
        }
    };
    for item in items {
        add(&item.relation)?;
        for Join {
            relation,
            join_operator,
            ..
        } in &item.joins
        {
            match join_operator {
                JoinOperator::CrossJoin(_) => add(relation)?,
                _ => return Err(Error::new("a step joins only with CROSS JOIN")),
            }
        }
    }
    let mut columns = Vec::new();
    for (r, q) in &parts {
        columns.extend(r.names.iter().map(|n| (Some(q.clone()), n.clone())));
    }
    let mut out = Relation {
        names: columns.iter().map(|c| c.1.clone()).collect(),
        rows: vec![Vec::new()],
        order: parts[0].0.order,
    };
    for (i, (r, _)) in parts.iter().enumerate() {
        if i > 0 && r.rows.len() != 1 {
            out.order = None;
        }
        let mut rows = Vec::with_capacity(out.rows.len() * r.rows.len());
        for left in &out.rows {
            for right in &r.rows {
                let mut row = left.clone();
                row.extend(right.iter().cloned());
                rows.push(row);
            }
        }
        out.rows = rows;
    }
    Ok((out, columns))
}

/// Each window's value on each row, read along the order: a lag, a running sum or count, partitioned
/// or not, each partition's running fold kept apart.
fn windows(
    calls: &[&Expr],
    rel: &Relation,
    frame: &Frame,
    state: &mut Running,
) -> Result<Vec<HashMap<*const Expr, Value>>> {
    let mut out: Vec<HashMap<*const Expr, Value>> = vec![HashMap::new(); rel.rows.len()];
    for &call in calls {
        let Expr::Function(f) = call else {
            unreachable!()
        };
        let spec = match &f.over {
            Some(WindowType::WindowSpec(s)) => s,
            _ => return Err(Error::new(format!("{call}: a named window is not read"))),
        };
        let order = rel.order.ok_or_else(|| {
            Error::new(format!(
                "{call}: a window is read only along the table's order"
            ))
        })?;
        match spec.order_by.as_slice() {
            [o] if super::ordering::ascending(&o.options)
                && column_of(&o.expr)
                    .is_some_and(|(q, n)| frame.find(q.as_deref(), &n).ok() == Some(order)) => {}
            _ => {
                return Err(Error::new(format!(
                    "{call}: a window is read only along the table's order, ascending, its \
                     NULLs last"
                )))
            }
        }
        if let Some(fr) = &spec.window_frame {
            let whole_start = matches!(fr.start_bound, WindowFrameBound::Preceding(None));
            let to_current = matches!(fr.end_bound, None | Some(WindowFrameBound::CurrentRow));
            if !(whole_start
                && to_current
                && matches!(fr.units, WindowFrameUnits::Rows | WindowFrameUnits::Range))
            {
                return Err(Error::new(format!(
                    "{call}: only the rows from the partition's first to the current are read"
                )));
            }
        }
        let name = function_name(f);
        let a = if is_count_star(f) {
            Vec::new()
        } else {
            args(f)?
        };
        let none = Computed {
            calls: HashMap::new(),
            bound: None,
            _q: std::marker::PhantomData,
        };
        let running = state.entry(call as *const Expr).or_default();
        let parted_under = spec
            .partition_by
            .iter()
            .map(|p| frame.casings.keyed(p))
            .collect::<Result<Vec<_>>>()?;
        for (r, row) in rel.rows.iter().enumerate() {
            let part: Vec<Key> = spec
                .partition_by
                .iter()
                .zip(&parted_under)
                .map(|(p, &u)| eval(p, row, frame, &none).map(|v| key_under(&v, u)))
                .collect::<Result<_>>()?;
            let state = running.entry(part).or_insert(Value::Null);
            let value = match (name.as_str(), a.as_slice()) {
                ("lag", [x]) => {
                    let now = eval(x, row, frame, &none)?;
                    std::mem::replace(state, now)
                }
                ("sum", [x]) => {
                    let v = eval(x, row, frame, &none)?;
                    // A bigint's sum is a numeric; a smallint's and an integer's a bigint, added
                    // as PostgreSQL's int2_sum and int4_sum add.
                    let v = match v {
                        Value::Int(i)
                            if frame.types.get(&(call as *const Expr))
                                == Some(&super::typed::NUMERIC) =>
                        {
                            Value::Num(i128::from(i), 0)
                        }
                        v => v,
                    };
                    if !v.is_null() {
                        *state = if state.is_null() {
                            v
                        } else if let (Value::Int(a), Value::Int(b)) = (&*state, &v) {
                            Value::Int(a.wrapping_add(*b))
                        } else if let Some(s) = float_arith(&BinaryOperator::Plus, state, &v)? {
                            s
                        } else if let Some(s) = scalar_arith(&BinaryOperator::Plus, state, &v)? {
                            s
                        } else {
                            arith(&BinaryOperator::Plus, state, &v)?
                        };
                    }
                    state.clone()
                }
                ("count", []) | ("row_number", []) => {
                    let n = match state {
                        Value::Int(n) => *n + 1,
                        _ => 1,
                    };
                    *state = Value::Int(n);
                    Value::Int(n)
                }
                _ => return Err(Error::new(format!("the window {call} is not read"))),
            };
            out[r].insert(call as *const Expr, value);
        }
    }
    Ok(out)
}

/// Whether a window call folds as the rows come: lag of one argument, sum, count(*) and row_number,
/// along the order, over the rows from the partition's first to the current, none left out.
fn streams(call: &Expr) -> bool {
    let Expr::Function(f) = call else {
        return false;
    };
    let Some(WindowType::WindowSpec(spec)) = &f.over else {
        return false;
    };
    let to_current = spec.window_frame.as_ref().is_none_or(|fr| {
        matches!(fr.start_bound, WindowFrameBound::Preceding(None))
            && matches!(fr.end_bound, None | Some(WindowFrameBound::CurrentRow))
            && matches!(fr.units, WindowFrameUnits::Rows | WindowFrameUnits::Range)
    });
    let name = function_name(f);
    !spec.order_by.is_empty()
        && spec.window_name.is_none()
        && to_current
        && f.filter.is_none()
        && f.null_treatment.is_none()
        && (is_count_star(f)
            || matches!(
                (name.as_str(), args(f).map(|a| a.len())),
                ("lag", Ok(1)) | ("sum", Ok(1)) | ("row_number", Ok(0))
            ))
}

/// The output name PostgreSQL gives an item it is not told.
pub(super) fn default_name(e: &Expr) -> String {
    match e {
        Expr::Identifier(i) => ident(i),
        Expr::CompoundIdentifier(p) => ident(p.last().unwrap()),
        Expr::Function(f) => function_name(f),
        Expr::Cast { expr, .. } => default_name(expr),
        Expr::Nested(x) => default_name(x),
        Expr::Case { .. } => "case".into(),
        // A subscript or a field: the last field's name, else what it is taken from.
        Expr::CompoundFieldAccess { root, access_chain } => access_chain
            .iter()
            .rev()
            .find_map(|a| match a {
                sqlparser::ast::AccessExpr::Dot(Expr::Identifier(i)) => Some(ident(i)),
                _ => None,
            })
            .unwrap_or_else(|| default_name(root)),
        _ => "?column?".into(),
    }
}

/// The elements a value of a fixed-length type is subscripted by, from 0, as PostgreSQL's raw
/// subscripting reads them: a point's x and y, a line's A, B and C, an lseg's two points, a box's
/// high and low corners, a name's 64 bytes, each a `"char"`; none for any other value.
fn raw_elements(v: &Value) -> Option<Vec<Value>> {
    use crate::geo::{Geo, Point};
    let point = |p: &Point| Value::Geometric(Geo::Point(*p));
    Some(match v {
        Value::Geometric(Geo::Point(p)) => vec![Value::Double(p.x), Value::Double(p.y)],
        Value::Geometric(Geo::Line(l)) => {
            vec![Value::Double(l.a), Value::Double(l.b), Value::Double(l.c)]
        }
        Value::Geometric(Geo::Lseg(s)) => vec![point(&s.p[0]), point(&s.p[1])],
        Value::Geometric(Geo::Box(b)) => vec![point(&b.high), point(&b.low)],
        Value::Str(ColumnType::Name, s) => (0..64)
            .map(|i| {
                let byte = s.as_bytes().get(i).copied().unwrap_or(0);
                Value::Scalar(ColumnType::Char, crate::scalars::unsigned_key(byte.into(), 8))
            })
            .collect(),
        _ => return None,
    })
}

/// One aggregate, folded a row at a time.
#[derive(Clone, Debug)]
enum Acc {
    Count(i64),
    Sum(Value),
    /// sum of a smallint or an integer: a bigint, added as PostgreSQL's int2_sum and int4_sum add.
    IntSum(Option<i64>),
    Least(Option<Value>),
    Greatest(Option<Value>),
    All(Option<bool>),
    Any(Option<bool>),
    /// avg of a float: its rows, their sum and their sum of squared deviations, as
    /// PostgreSQL's float8_accum folds them.
    Avg(f64, f64, f64),
    /// avg of an integer or a numeric, its sum and rows as numeric_avg_accum folds them.
    NumAvg(crate::numeric::Sum),
    /// sum, or avg where `avg`, of an interval, as PostgreSQL's interval_avg_accum folds it; of a
    /// time through its cast to an interval.
    Intervals(crate::times::IntervalAccum, bool),
    /// An aggregate past those the types carry.
    More(Box<super::aggregates::Agg>),
    /// An aggregate of each value once: the values seen, each a row of its arguments, folded in
    /// their order at the end, as PostgreSQL sorts a DISTINCT aggregate's input, each argument
    /// compared under its collation.
    Distinct {
        seen: std::collections::HashSet<Vec<Key>>,
        values: Vec<Vec<Value>>,
        under: Vec<Option<Casing>>,
        of: Box<Acc>,
    },
    /// An aggregate whose rows come in the order its ORDER BY gives: each row's arguments, then
    /// the values it is ordered by, folded in that order at the end; the arguments' collations.
    Ordered {
        by: Vec<super::aggregates::Direction>,
        args: usize,
        rows: Vec<Vec<Value>>,
        under: Vec<Option<Casing>>,
        of: Box<Acc>,
    },
}

/// What an aggregate call folds from each row: the expressions it folds, the FILTER that admits a
/// row, and an ordered-set aggregate's direct arguments.
struct Folds<'q> {
    args: Vec<&'q Expr>,
    filter: Option<&'q Expr>,
    direct: Vec<&'q Expr>,
}

impl Acc {
    /// The aggregate a call folds, and what it folds from each row: no expression for count(*).
    fn of<'e>(call: &'e Expr, types: &super::typed::Types) -> Result<(Acc, Folds<'e>)> {
        let Expr::Function(f) = call else {
            unreachable!()
        };
        let name = function_name(f);
        let filter = f.filter.as_deref();
        if name == "count" && is_count_star(f) {
            let folds = Folds {
                args: Vec::new(),
                filter,
                direct: Vec::new(),
            };
            return Ok((Acc::Count(0), folds));
        }
        let mut by: Vec<&sqlparser::ast::OrderByExpr> = Vec::new();
        let distinct = match &f.args {
            FunctionArguments::List(l) => {
                for c in &l.clauses {
                    match c {
                        sqlparser::ast::FunctionArgumentClause::OrderBy(o) => by.extend(o),
                        _ => {
                            return Err(Error::new(format!(
                                "{call}: a clause among an aggregate's arguments is not read"
                            )))
                        }
                    }
                }
                matches!(
                    l.duplicate_treatment,
                    Some(sqlparser::ast::DuplicateTreatment::Distinct)
                )
            }
            _ => false,
        };
        if distinct && !by.is_empty() {
            return Err(Error::new(format!(
                "{call}: an ORDER BY beside DISTINCT among an aggregate's arguments is not read"
            )));
        }
        let a = args(f)?;
        if !f.within_group.is_empty() {
            let by = f
                .within_group
                .iter()
                .map(|o| {
                    let desc = super::ordering::descends(&o.options);
                    super::aggregates::Direction {
                        desc,
                        nulls_first: o.options.nulls_first.unwrap_or(desc),
                        under: None,
                        bytes: super::ordering::by_bytes(&o.options),
                    }
                })
                .collect();
            let agg = super::aggregates::Agg::sorted(&name, by)
                .ok_or_else(|| Error::new(format!("{call} is not read")))?;
            let folds = Folds {
                args: f.within_group.iter().map(|o| &o.expr).collect(),
                filter,
                direct: a,
            };
            return Ok((Acc::More(Box::new(agg)), folds));
        }
        let arg_types: Vec<_> = a.iter().map(|x| types.get(&(*x as *const Expr)).copied()).collect();
        let handed = match super::handed::is_handed(f) {
            true => super::handed::aggregate(f, &arg_types)?,
            false => None,
        };
        let acc = match (name.as_str(), a.as_slice()) {
            _ if handed.is_some() => Acc::More(Box::new(super::aggregates::Agg::Handed {
                form: handed.unwrap(),
                rows: Vec::new(),
            })),
            ("count", [_]) => Acc::Count(0),
            ("sum", [_]) if types.get(&(call as *const Expr)) == Some(&ColumnType::Int8) => {
                Acc::IntSum(None)
            }
            ("sum", [_]) => Acc::Sum(Value::Null),
            ("min", [_]) => Acc::Least(None),
            ("max", [_]) => Acc::Greatest(None),
            ("bool_and" | "every", [_]) => Acc::All(None),
            ("bool_or", [_]) => Acc::Any(None),
            ("avg", [_]) => Acc::Avg(0.0, 0.0, 0.0),
            ("array_agg", [x]) => Acc::More(Box::new(super::aggregates::Agg::Collect {
                ty: types.get(&(call as *const Expr)).copied(),
                arrays: types
                    .get(&(*x as *const Expr))
                    .is_some_and(|t| super::typed::element_of(*t).is_some()),
                values: Vec::new(),
            })),
            (n, [_] | [_, _]) => match super::aggregates::Agg::of(n) {
                Some(agg) => Acc::More(Box::new(agg)),
                None => return Err(Error::new(format!("{call} is not read"))),
            },
            _ => return Err(Error::new(format!("{call} is not read"))),
        };
        // A DISTINCT sorts what it folds.
        if distinct {
            for x in &a {
                if let Some(t) = types.get(&(*x as *const Expr)).filter(|t| !t.ordered()) {
                    return Err(Error::new(format!(
                        "could not identify an ordering operator for type {}",
                        t.pg_name()
                    )));
                }
            }
        }
        let acc = if distinct {
            Acc::Distinct {
                seen: Default::default(),
                values: Vec::new(),
                under: Vec::new(),
                of: Box::new(acc),
            }
        } else if !by.is_empty() {
            Acc::Ordered {
                by: by
                    .iter()
                    .map(|o| {
                        let desc = super::ordering::descends(&o.options);
                        super::aggregates::Direction {
                            desc,
                            nulls_first: o.options.nulls_first.unwrap_or(desc),
                            under: None,
                            bytes: super::ordering::by_bytes(&o.options),
                        }
                    })
                    .collect(),
                args: a.len(),
                rows: Vec::new(),
                under: Vec::new(),
                of: Box::new(acc),
            }
        } else {
            acc
        };
        let mut args = a;
        args.extend(by.iter().map(|o| &o.expr));
        let folds = Folds {
            args,
            filter,
            direct: Vec::new(),
        };
        Ok((acc, folds))
    }

    /// Whether folding in a row's arguments compares its value with one folded before: min's or
    /// max's second value that is not NULL, in the order an ORDER BY gives them or not.
    fn compares(&self, values: &[Value]) -> bool {
        if values.first().is_none_or(Value::is_null) {
            return false;
        }
        match self {
            Acc::Least(Some(_)) | Acc::Greatest(Some(_)) => true,
            Acc::Ordered { of, rows, .. } => {
                matches!(**of, Acc::Least(_) | Acc::Greatest(_))
                    && rows.iter().any(|r| r.first().is_some_and(|v| !v.is_null()))
            }
            _ => false,
        }
    }

    /// Folds in one row's arguments, none for count(*), each compared under the collation `under`
    /// gives it.
    /// Whether it keeps the rows it is given until its group ends.
    fn keeps_rows(&self) -> bool {
        match self {
            Acc::Distinct { .. } | Acc::Ordered { .. } => true,
            Acc::More(agg) => agg.keeps_rows(),
            _ => false,
        }
    }

    fn add_row(&mut self, values: Vec<Value>, under: &[Option<Casing>]) -> Result<()> {
        match self {
            Acc::More(agg) => agg.add(&values, under),
            Acc::Ordered {
                by,
                args,
                rows,
                under: kept,
                ..
            } => {
                let (of_args, of_by) = under.split_at((*args).min(under.len()));
                kept.clear();
                kept.extend_from_slice(of_args);
                for (d, u) in by.iter_mut().zip(of_by) {
                    d.under = *u;
                }
                rows.push(values);
                Ok(())
            }
            Acc::Distinct {
                seen,
                values: kept,
                under: kept_under,
                of,
            } => {
                if values.iter().any(Value::is_null)
                    && !matches!(&**of, Acc::More(a) if a.keeps_nulls())
                {
                    return Ok(());
                }
                kept_under.clear();
                kept_under.extend_from_slice(under);
                let k = values
                    .iter()
                    .enumerate()
                    .map(|(i, v)| key_under(v, under.get(i).copied().flatten()))
                    .collect();
                if seen.insert(k) {
                    kept.push(values);
                }
                Ok(())
            }
            _ => self.add(values.into_iter().next(), under.first().copied().flatten()),
        }
    }

    /// An ordered-set aggregate's direct arguments, the first time they are given.
    fn direct(&mut self, values: Vec<Value>) {
        if let Acc::More(agg) = self {
            agg.direct(values);
        }
    }

    /// Folds in one row: its value, or none for count(*). A NULL is passed over. min and max compare
    /// strings under `under`.
    fn add(&mut self, v: Option<Value>, under: Option<Casing>) -> Result<()> {
        let v = match v {
            None => {
                if let Acc::Count(n) = self {
                    *n += 1;
                }
                return Ok(());
            }
            Some(v) if v.is_null() => return Ok(()),
            Some(v) => v,
        };
        // PostgreSQL has no sum, min or max of a boolean, a uuid or a bit string, and no sum of a
        // string or bytea.
        if matches!(
            (&v, &self),
            (
                Value::Bool(_) | Value::Uuid(_) | Value::Bits(..),
                Acc::Sum(_) | Acc::Least(_) | Acc::Greatest(_),
            ) | (
                Value::Text(_) | Value::Str(..) | Value::Bytes(_),
                Acc::Sum(_)
            )
        ) {
            let name = match self {
                Acc::Sum(_) => "sum",
                Acc::Least(_) => "min",
                _ => "max",
            };
            return Err(Error::new(format!(
                "function {name}({}) does not exist",
                kind_name(&v)
            )));
        }
        // No composed type has a sum; a range and a multirange have no min or max.
        if let Value::Composed(ty, _) = &v {
            let ranged = matches!(ty, ColumnType::Composed(r) if matches!(r.def(), crate::composed::Composed::Range { .. } | crate::composed::Composed::Multirange { .. }));
            let name = match self {
                Acc::Sum(_) => Some("sum"),
                Acc::Least(_) if ranged => Some("min"),
                Acc::Greatest(_) if ranged => Some("max"),
                _ => None,
            };
            if let Some(name) = name {
                return Err(Error::new(format!(
                    "function {name}({}) does not exist",
                    ty.pg_name()
                )));
            }
            // Of a type with no B-tree, refused where a second value meets the first, as
            // PostgreSQL's array_smaller and array_larger are first called on two.
            if !ty.ordered()
                && !is_record(*ty)
                && !v.is_null()
                && matches!(self, Acc::Least(Some(_)) | Acc::Greatest(Some(_)))
            {
                return Err(Error::new(format!(
                    "could not identify a comparison function for type {}",
                    ty.pg_name()
                )));
            }
        }
        // Of the scalars, sum is money's alone; min and max are an oid's, an xid8's, a pg_lsn's,
        // money's, a tid's and a time type's, and a `"char"`'s through its text.
        let v = match (&v, &self) {
            (Value::Scalar(ColumnType::Char, _), Acc::Least(_) | Acc::Greatest(_)) => {
                Value::Text(v.text().unwrap_or_default())
            }
            // Of the types held as their text only inet has min and max, which take a cidr as an
            // inet; none has a sum.
            (Value::Typed(ty, s), Acc::Least(_) | Acc::Greatest(_)) if matches!(ty, ColumnType::Textual(t) if t.is_network()) => {
                Value::Typed(
                    ColumnType::Textual(crate::textual::Textual::Inet),
                    crate::textual::cast(
                        match ty {
                            ColumnType::Textual(t) => *t,
                            _ => unreachable!(),
                        },
                        crate::textual::Textual::Inet,
                        s,
                    )?,
                )
            }
            (Value::Typed(ty, _), Acc::Sum(_) | Acc::Least(_) | Acc::Greatest(_)) => {
                let name = match self {
                    Acc::Sum(_) => "sum",
                    Acc::Least(_) => "min",
                    _ => "max",
                };
                return Err(Error::new(format!(
                    "function {name}({}) does not exist",
                    ty.pg_name()
                )));
            }
            (Value::Typed(..), Acc::All(_) | Acc::Any(_)) => {
                return Err(Error::new(format!("{v:?} is not a truth")))
            }
            // No geometric type has min, max or sum.
            (Value::Geometric(g), Acc::Sum(_) | Acc::Least(_) | Acc::Greatest(_)) => {
                let name = match self {
                    Acc::Sum(_) => "sum",
                    Acc::Least(_) => "min",
                    _ => "max",
                };
                return Err(Error::new(format!(
                    "function {name}({}) does not exist",
                    g.kind().name()
                )));
            }
            // A name's min and max are text's, as PostgreSQL reads a name through text.
            (Value::Str(ColumnType::Name, s), Acc::Least(_) | Acc::Greatest(_)) => {
                Value::Text(s.clone())
            }
            // A reg* type's min and max are oid's, an oid.
            (Value::Scalar(ColumnType::Reg(..), k), Acc::Least(_) | Acc::Greatest(_)) => {
                Value::Scalar(ColumnType::Oid, *k)
            }
            (Value::Scalar(ty, _), Acc::Sum(_) | Acc::Least(_) | Acc::Greatest(_))
                if !(matches!(self, Acc::Sum(_))
                    && matches!(ty, ColumnType::Money(_) | ColumnType::Time(_))
                    || !matches!(self, Acc::Sum(_))
                        && matches!(
                            ty,
                            ColumnType::Oid
                                | ColumnType::Oid8
                                | ColumnType::Xid8
                                | ColumnType::PgLsn
                                | ColumnType::Money(_)
                                | ColumnType::Tid
                                | ColumnType::Date
                                | ColumnType::Time(_)
                                | ColumnType::Timetz(_)
                                | ColumnType::Timestamp(_)
                                | ColumnType::Timestamptz
                        )) =>
            {
                let name = match self {
                    Acc::Sum(_) => "sum",
                    Acc::Least(_) => "min",
                    _ => "max",
                };
                return Err(Error::new(format!(
                    "function {name}({}) does not exist",
                    ty.pg_name()
                )));
            }
            (Value::Scalar(..), Acc::All(_) | Acc::Any(_)) => {
                return Err(Error::new(format!("{v:?} is not a truth")))
            }
            _ => v,
        };
        // An interval's sum or average, and a time's through its cast to an interval.
        let lapse = match &v {
            Value::Interval(x) => Some(*x),
            Value::Scalar(ColumnType::Time(_), t) => Some(crate::times::time_interval(*t)),
            _ => None,
        };
        if let Some(x) = lapse {
            match self {
                Acc::Sum(Value::Null) => *self = Acc::Intervals(Default::default(), false),
                Acc::Avg(n, ..) if *n == 0.0 => *self = Acc::Intervals(Default::default(), true),
                _ => {}
            }
            if let Acc::Intervals(s, avg) = self {
                s.add(x)?;
                if *avg {
                    s.avg()?;
                } else {
                    s.sum()?;
                }
                return Ok(());
            }
        }
        match self {
            Acc::Count(n) => *n += 1,
            Acc::IntSum(total) => match v {
                Value::Int(x) => *total = Some(total.map_or(x, |t| t.wrapping_add(x))),
                other => {
                    return Err(Error::new(format!(
                        "function sum({}) does not exist",
                        kind_name(&other)
                    )))
                }
            },
            // The sum of whole numbers is a numeric of scale 0, as PostgreSQL writes it.
            Acc::Sum(acc) => {
                *acc = if acc.is_null() {
                    match v {
                        Value::Int(x) => Value::Num(x as i128, 0),
                        other => other,
                    }
                } else if let Some(s) = float_arith(&BinaryOperator::Plus, acc, &v)? {
                    s
                } else if let Some(s) = scalar_arith(&BinaryOperator::Plus, acc, &v)? {
                    s
                } else {
                    arith(&BinaryOperator::Plus, acc, &v)?
                };
            }
            // Of two numerics equal, numeric_smaller and numeric_larger keep the later.
            Acc::Least(best) => {
                let later = v.to_numeric().is_some();
                let better = match best {
                    None => true,
                    Some(b) => match extreme_order(under, &v, b)? {
                        Some(Ordering::Less) => true,
                        Some(Ordering::Equal) => later,
                        _ => false,
                    },
                };
                if better {
                    *best = Some(v);
                }
            }
            Acc::Greatest(best) => {
                let later = v.to_numeric().is_some();
                let better = match best {
                    None => true,
                    Some(b) => match extreme_order(under, &v, b)? {
                        Some(Ordering::Greater) => true,
                        Some(Ordering::Equal) => later,
                        _ => false,
                    },
                };
                if better {
                    *best = Some(v);
                }
            }
            Acc::Avg(n, ..) if *n == 0.0 && v.to_numeric().is_some() => {
                let mut s = crate::numeric::Sum::default();
                s.add(&v.to_numeric().unwrap());
                *self = Acc::NumAvg(s);
            }
            Acc::NumAvg(s) => match v.to_numeric() {
                Some(n) => s.add(&n),
                None => return Err(Error::new(format!("avg of {} is not read", kind_name(&v)))),
            },
            Acc::All(a) => {
                let t = truth(&v)?.unwrap_or(true);
                *a = Some(a.map_or(t, |x| x && t));
            }
            Acc::Any(a) => {
                let t = truth(&v)?.unwrap_or(false);
                *a = Some(a.map_or(t, |x| x || t));
            }
            Acc::More(agg) => agg.add(&[v], &[under])?,
            Acc::Distinct { .. } | Acc::Ordered { .. } => self.add_row(vec![v], &[under])?,
            // Reached only by a value no interval is: the first was one.
            Acc::Intervals(..) => {
                return Err(Error::new(format!(
                    "{} beside an interval is not summed",
                    kind_name(&v)
                )))
            }
            Acc::Avg(n, sx, sxx) => {
                let x = match v {
                    Value::Real(f) => f64::from(f),
                    Value::Double(d) => d,
                    other => {
                        return Err(Error::new(format!(
                            "avg of {} is not read",
                            kind_name(&other)
                        )))
                    }
                };
                let (n0, sx0, sxx0) = (*n, *sx, *sxx);
                *n = n0 + 1.0;
                *sx = sx0 + x;
                if n0 > 0.0 {
                    let t = x * *n - *sx;
                    *sxx = sxx0 + t * t / (*n * n0);
                    if sx.is_infinite() || sxx.is_infinite() {
                        if !sx0.is_infinite() && !x.is_infinite() {
                            return Err(out_of_range("overflow"));
                        }
                        *sxx = f64::NAN;
                    }
                } else if x.is_nan() || x.is_infinite() {
                    *sxx = f64::NAN;
                }
            }
        }
        Ok(())
    }

    fn value(&self) -> Result<Value> {
        Ok(match self {
            Acc::Count(n) => Value::Int(*n),
            Acc::Sum(v) => v.clone(),
            Acc::IntSum(t) => t.map_or(Value::Null, Value::Int),
            Acc::Least(b) | Acc::Greatest(b) => b.clone().unwrap_or(Value::Null),
            Acc::All(a) | Acc::Any(a) => a.map_or(Value::Null, Value::Bool),
            Acc::Avg(n, sx, _) if *n > 0.0 => Value::Double(sx / n),
            Acc::Avg(..) => Value::Null,
            Acc::NumAvg(s) => match s.avg() {
                Some(a) => Value::of_numeric(a?),
                None => Value::Null,
            },
            Acc::Intervals(s, avg) => {
                (if *avg { s.avg()? } else { s.sum()? }).map_or(Value::Null, Value::Interval)
            }
            Acc::More(agg) => agg.value()?,
            Acc::Ordered {
                by,
                args,
                rows,
                under,
                of,
            } => {
                let mut rows = rows.clone();
                let mut failed = None;
                rows.sort_by(|a, b| {
                    super::aggregates::compare_rows(&a[*args..], &b[*args..], by).unwrap_or_else(
                        |e| {
                            failed.get_or_insert(e);
                            Ordering::Equal
                        },
                    )
                });
                if let Some(e) = failed {
                    return Err(e);
                }
                let mut acc = (**of).clone();
                for mut r in rows {
                    r.truncate(*args);
                    acc.add_row(r, under)?;
                }
                acc.value()?
            }
            Acc::Distinct {
                values, under, of, ..
            } => {
                let mut values = values.clone();
                let mut failed = None;
                values.sort_by(|a, b| {
                    for (i, (x, y)) in a.iter().zip(b).enumerate() {
                        // A NULL, which an aggregate that keeps NULLs sees, sorts last.
                        match (x.is_null(), y.is_null()) {
                            (true, true) => continue,
                            (true, false) => return Ordering::Greater,
                            (false, true) => return Ordering::Less,
                            _ => {}
                        }
                        match compare_under(under.get(i).copied().flatten(), x, y) {
                            Ok(Some(Ordering::Equal)) | Ok(None) => {}
                            Ok(Some(o)) => return o,
                            Err(e) => {
                                failed.get_or_insert(e);
                            }
                        }
                    }
                    Ordering::Equal
                });
                if let Some(e) = failed {
                    return Err(e);
                }
                let mut acc = (**of).clone();
                for v in values {
                    acc.add_row(v, under)?;
                }
                acc.value()?
            }
        })
    }
}

/// Each window call's running fold, for each of its partitions.
type Running = HashMap<*const Expr, HashMap<Vec<Key>, Value>>;

/// A SELECT of the pipeline as it is read: what it returns, its windows and aggregates, and how it
/// groups.
#[derive(Clone)]
pub(super) struct Prepared<'q> {
    from: &'q [TableWithJoins],
    selection: Option<&'q Expr>,
    /// The condition each group it makes is kept by.
    having: Option<&'q Expr>,
    /// The expressions its FROM reads its relations by: each join's ON, each function's call.
    pub(super) joined: Vec<&'q Expr>,
    order_by: Option<&'q sqlparser::ast::OrderBy>,
    /// The place, in each row it makes, of the value each ORDER BY item sorts by: a column it
    /// returns, or after them each expression of `hidden`, made beside them until they are sorted.
    sorted: Vec<usize>,
    hidden: Vec<&'q Expr>,
    items: Vec<(&'q Expr, String)>,
    /// The items made after the sort in a step that sorts or cuts its rows: those that draw from a
    /// sequence, and where `late_sets`, those holding a set. Each is made once the rows are sorted,
    /// for each row the cut reaches, from the row it came from and its aggregates' and windows'
    /// values, carried after the ORDER BY's values until then.
    drawn: Vec<usize>,
    /// Whether its select list's sets are made after the sort, each carried row becoming their rows.
    late_sets: bool,
    calls: Calls<'q>,
    /// Its select list's set-returning calls by level, as `project` makes them.
    levels: Vec<Vec<&'q Expr>>,
    /// Whether each set of the deepest level is an item, or an ORDER BY item, by itself: the items
    /// are then made beside them, and once more in the pass that finds every one ended.
    ends: bool,
    /// What it makes below its fold and its windows (`Below`): the expressions, their sets by level,
    /// and where each is read.
    below_exprs: Vec<&'q Expr>,
    below_levels: Vec<Vec<&'q Expr>>,
    pub(super) below: Below,
    /// The expressions it groups by, each once.
    grouping: Vec<&'q Expr>,
    /// Where it groups by more than one grouping set: each set by its members' places in
    /// `grouping`, and each place one of them stands whole in its select list, its ORDER BY, its
    /// HAVING or an aggregate's direct arguments, read from the set a group is made for.
    sets: Option<Vec<Vec<usize>>>,
    whole: Vec<(*const Expr, usize)>,
    /// Each GROUPING call's arguments, by their places in `grouping`.
    grouping_args: Vec<Vec<usize>>,
    grouped: bool,
    /// Whether it returns each row once (SELECT DISTINCT).
    distinct: bool,
    /// Where it returns the first row of each run its DISTINCT ON keys tie in: how many of the
    /// places `sorted` begins with hold the keys.
    distinct_on: Option<usize>,
    /// The rows it returns of those it makes, where it says.
    limit: Option<Limit<'q>>,
    /// Whether a window reads its partition whole: the step then holds its rows until they have
    /// all come.
    whole_windows: bool,
    /// Each expression's type, where the step resolves it, and the type of each column it returns.
    pub(super) types: super::typed::Types,
    pub(super) returns: Vec<Option<ColumnType>>,
    /// The collation each column it returns carries.
    pub(super) carried: Vec<super::collate::Carried>,
    /// The clock its statement reads.
    now: i64,
}

impl<'q> Prepared<'q> {
    /// The aggregates it folds.
    pub(super) fn aggregates(&self) -> &[&'q Expr] {
        &self.calls.aggregates
    }

    /// The windows it calls.
    pub(super) fn windows(&self) -> &[&'q Expr] {
        &self.calls.windows
    }

    /// The columns its expressions read of its FROM's: its select list's, its WHERE's, its HAVING's,
    /// its ORDER BY's own, its windows' partitions and orders, and those its FROM reads its
    /// relations by.
    /// The columns it reads, a parse tree handed to pg_get_expr apart.
    pub(super) fn reads_but_trees(&self) -> Vec<(Option<String>, String)> {
        let mut kept = Vec::new();
        for e in self.read_expressions() {
            columns_read_but_trees(e, &mut kept);
        }
        kept
    }

    pub(super) fn reads(&self) -> Vec<(Option<String>, String)> {
        let mut out = Vec::new();
        for e in self.read_expressions() {
            columns_read(e, &mut out);
        }
        out
    }

    /// The columns it reads outside the given expressions: a join's ONs, read on their own.
    pub(super) fn reads_beside(&self, beside: &[*const Expr]) -> Vec<(Option<String>, String)> {
        let mut out = Vec::new();
        for e in self.read_expressions() {
            if !beside.contains(&(e as *const Expr)) {
                columns_read(e, &mut out);
            }
        }
        out
    }

    /// The expressions its columns are read in.
    fn read_expressions(&self) -> Vec<&Expr> {
        let windowed = self.calls.windows.iter().flat_map(|w| match w {
            Expr::Function(f) => match &f.over {
                Some(WindowType::WindowSpec(s)) => s
                    .partition_by
                    .iter()
                    .chain(s.order_by.iter().map(|o| &o.expr))
                    .chain(f.filter.as_deref())
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            },
            _ => Vec::new(),
        });
        self.items
            .iter()
            .map(|(e, _)| *e)
            .chain(self.selection)
            .chain(self.having)
            .chain(self.hidden.iter().copied())
            .chain(self.joined.iter().copied())
            .chain(windowed)
            .collect()
    }

    /// The rows a row of its FROM becomes below its fold and its windows (`Below`): one for each
    /// value its sets there give, in levels and in lockstep, each with the expressions' values after
    /// the FROM's columns; none where a level gives none.
    fn below_rows(&self, row: Vec<Value>, frame: &Frame, out: &mut Vec<Vec<Value>>) -> Result<()> {
        each_level(&self.below_levels, &row, frame, &computed_none(), &mut |c| {
            let mut r = row.clone();
            for &e in &self.below_exprs {
                r.push(eval(e, &row, frame, c)?);
            }
            out.push(r);
            Ok(true)
        })?;
        Ok(())
    }

    /// The rows its select list makes of one row, each with its ORDER BY's expressions after them:
    /// one, or, where it calls set-returning functions, those its levels make, as PostgreSQL's
    /// ProjectSet makes them. Each level's sets are made over each row of the level beneath, the
    /// first's over the row itself, in lockstep (`zipped`); the items are made over the deepest
    /// level's rows, or beside its sets where it makes them (`passes`).
    fn project(&self, row: &[Value], frame: &Frame, computed: Computed) -> Result<Vec<Vec<Value>>> {
        // An item made after the sort is NULL until then.
        let before = |i: usize, e: &Expr, c: &Computed| match self.drawn.contains(&i) {
            true => Ok(Value::Null),
            false => eval(e, row, frame, c),
        };
        let one = |c: &Computed| {
            self.listed()
                .into_iter()
                .enumerate()
                .map(|(i, e)| before(i, e, c))
                .collect::<Result<Vec<_>>>()
        };
        // A row whose items wait for the sort and the cut carries the row it came from, and its
        // aggregates' and windows' values.
        let folds = match self.drawn.is_empty() {
            true => Vec::new(),
            false => self.folds_of(&computed),
        };
        let carry = |mut made: Vec<Value>| {
            if !self.drawn.is_empty() {
                made.extend_from_slice(row);
                made.extend(folds.iter().cloned());
            }
            made
        };
        let Some((deepest, beneath)) = self.levels.split_last().filter(|_| !self.late_sets) else {
            return Ok(vec![carry(one(&computed)?)]);
        };
        let mut made = Vec::new();
        let mut taking = Taking {
            cut: None,
            key: &[],
            out: &mut made,
        };
        each_level(beneath, row, frame, &computed, &mut |c| match self.ends {
            true => self.passes(deepest, row, frame, c, &|i, e| before(i, e, c), &mut taking),
            false => zipped(deepest, row, frame, c, &mut |c| {
                taking.take(one(&c)?);
                Ok(true)
            }),
        })?;
        Ok(made.into_iter().map(carry).collect())
    }

    /// Its items, then its ORDER BY's expressions beside them.
    fn listed(&self) -> Vec<&'q Expr> {
        self.items
            .iter()
            .map(|(e, _)| *e)
            .chain(self.hidden.iter().copied())
            .collect()
    }

    /// What a row is made with beside its row, in their order: each aggregate's, window's and
    /// GROUPING's value, and where it groups by sets, each grouped value in its place (`whole`).
    fn folded(&self) -> Vec<*const Expr> {
        self.calls
            .aggregates
            .iter()
            .chain(&self.calls.windows)
            .chain(&self.calls.groupings)
            .map(|&c| c as *const Expr)
            .chain(self.whole.iter().map(|&(p, _)| p))
            .collect()
    }

    /// Where a grouping expression is a column of its FROM or a value it makes below its fold, its
    /// place in a row: NULL in the groups of a set that lacks it.
    fn cleared(&self, i: usize, frame: &Frame) -> Option<usize> {
        let g = self.grouping[i];
        if let Some(&(_, k)) = self.below.at.iter().find(|(e, _)| std::ptr::eq(*e, g)) {
            return Some(frame.columns.len() + k);
        }
        let (q, n) = column_of(g)?;
        frame.find(q.as_deref(), &n).ok()
    }

    /// The rows a group returns, made of its row and the values its expressions read beyond it.
    fn group_rows(
        &self,
        row: &[Value],
        calls: HashMap<*const Expr, Value>,
        frame: &Frame,
    ) -> Result<Vec<Vec<Value>>> {
        let computed = Computed {
            calls,
            bound: None,
            _q: std::marker::PhantomData,
        };
        self.project(row, frame, computed)
    }

    /// The rows the groups return where windows read them, once every group is made: each window
    /// over the groups' rows, reading each group's values, then each group made into its rows.
    fn windowed_groups(
        &self,
        groups: Vec<(Vec<Value>, HashMap<*const Expr, Value>)>,
        frame: &Frame,
    ) -> Result<Vec<Vec<Value>>> {
        let (rows, calls): (Vec<_>, Vec<_>) = groups.into_iter().unzip();
        let given: Vec<Computed> = calls
            .into_iter()
            .map(|calls| Computed {
                calls,
                bound: None,
                _q: std::marker::PhantomData,
            })
            .collect();
        let rel = Relation {
            names: Vec::new(),
            rows,
            order: None,
        };
        let per_row = windows::values(&self.calls.windows, &rel, frame, Some(&given))?;
        let mut out = Vec::with_capacity(rel.rows.len());
        for ((row, mut g), w) in rel.rows.iter().zip(given).zip(per_row) {
            g.calls.extend(w);
            out.extend(self.project(row, frame, g)?);
        }
        Ok(out)
    }

    /// The values `folded` names a row is made with, in their order.
    fn folds_of(&self, computed: &Computed) -> Vec<Value> {
        self.folded()
            .into_iter()
            .map(|c| computed.calls.get(&c).cloned().unwrap_or(Value::Null))
            .collect()
    }

    /// The rows the deepest level makes over one row beneath it, where each of its sets is an item
    /// by itself: each pass the items in their order (`item` making any other than the sets), a set
    /// giving its next value, NULL past its end, while any gives one; and the items once more in the
    /// pass that finds every set ended, its row dropped. Each pass made only where `taking` wants a
    /// row; false where it stopped them.
    fn passes(
        &self,
        sets: &[&Expr],
        row: &[Value],
        frame: &Frame,
        computed: &Computed,
        item: &dyn Fn(usize, &Expr) -> Result<Value>,
        taking: &mut Taking,
    ) -> Result<bool> {
        let listed = self.listed();
        let mut values: Vec<Option<SetValues>> = (0..sets.len()).map(|_| None).collect();
        loop {
            if !taking.wants() {
                return Ok(false);
            }
            let mut made = Vec::with_capacity(listed.len());
            let mut given = false;
            for (i, &e) in listed.iter().enumerate() {
                let Some(at) = sets.iter().position(|&s| std::ptr::eq(s, unnested(e))) else {
                    // An INSERT's RETURNING call, after the sets, is made for a row written alone.
                    let returned = self
                        .items
                        .get(i)
                        .is_some_and(|(_, n)| n == super::change::RETURNED_CALL);
                    made.push(match returned && !given {
                        true => Value::Null,
                        false => item(i, e)?,
                    });
                    continue;
                };
                if values[at].is_none() {
                    values[at] = Some(set_iter(sets[at], row, frame, computed)?);
                }
                let v = values[at].as_mut().and_then(Iterator::next).transpose()?;
                given |= v.is_some();
                made.push(v.unwrap_or(Value::Null));
            }
            if !given {
                break;
            }
            taking.take(made);
        }
        Ok(true)
    }

    /// The rows a row carried past the sort becomes, each as `taking` wants it: its items made
    /// after the sort (`drawn`) made from the row it came from and its aggregates' and windows'
    /// values, which it carries no further, and where its sets wait for the sort, the rows its
    /// levels make, one at a time. A row carrying nothing is taken as it is.
    fn late_rows(&self, mut row: Vec<Value>, frame: &Frame, taking: &mut Taking) -> Result<()> {
        if self.drawn.is_empty() {
            if taking.wants() {
                taking.take(row);
            }
            return Ok(());
        }
        let mut from = row.split_off(self.items.len() + self.hidden.len());
        let folded = self.folded();
        let folds = from.split_off(from.len() - folded.len());
        let computed = Computed {
            calls: folded.into_iter().zip(folds).collect(),
            bound: None,
            _q: std::marker::PhantomData,
        };
        let base = row;
        let after = |i: usize, e: &Expr, c: &Computed| match self.drawn.contains(&i) {
            true => eval(e, &from, frame, c),
            false => Ok(base[i].clone()),
        };
        let one = |c: &Computed| {
            self.listed()
                .into_iter()
                .enumerate()
                .map(|(i, e)| after(i, e, c))
                .collect::<Result<Vec<_>>>()
        };
        let Some((deepest, beneath)) = self.levels.split_last().filter(|_| self.late_sets) else {
            if taking.wants() {
                taking.take(one(&computed)?);
            }
            return Ok(());
        };
        each_level(beneath, &from, frame, &computed, &mut |c| match self.ends {
            true => self.passes(deepest, &from, frame, c, &|i, e| after(i, e, c), taking),
            false => {
                if !taking.wants() {
                    return Ok(false);
                }
                zipped(deepest, &from, frame, c, &mut |c| {
                    taking.take(one(&c)?);
                    Ok(taking.wants())
                })
            }
        })?;
        Ok(())
    }
}

/// Over one row, each row the levels make handed to `each` as it is made: the first level's sets
/// made over the row itself, each next's over each row of the level before, in lockstep (`zipped`);
/// the row itself where there are none. False where `each` stopped them.
fn each_level<'q>(
    levels: &[Vec<&Expr>],
    row: &[Value],
    frame: &Frame,
    computed: &Computed<'q>,
    each: &mut dyn FnMut(&Computed<'q>) -> Result<bool>,
) -> Result<bool> {
    let Some((first, rest)) = levels.split_first() else {
        return each(computed);
    };
    zipped(first, row, frame, computed, &mut |c| {
        each_level(rest, row, frame, &c, each)
    })
}

/// A set-returning call's values over one row as they are wanted: a generate_series's made one at a
/// time, any other's made whole first.
enum SetValues {
    Whole(std::vec::IntoIter<Value>),
    Series(super::series::Series),
}

impl Iterator for SetValues {
    type Item = Result<Value>;
    fn next(&mut self) -> Option<Result<Value>> {
        match self {
            SetValues::Whole(v) => v.next().map(Ok),
            SetValues::Series(s) => s.next(),
        }
    }
}

/// A set-returning call's values over one row (`SetValues`), as `set_values` gives them.
fn set_iter(c: &Expr, row: &[Value], frame: &Frame, computed: &Computed) -> Result<SetValues> {
    if let Expr::Function(f) = c {
        if function_name(f) == "generate_series" && !super::handed::is_handed(f) {
            let given = args(f)?
                .iter()
                .map(|x| Ok((eval(x, row, frame, computed)?, quoted(x))))
                .collect::<Result<Vec<_>>>()?;
            return Ok(SetValues::Series(super::series::series(given, frame.type_of(c))?));
        }
    }
    Ok(SetValues::Whole(set_values(c, row, frame, computed)?.into_iter()))
}

impl Prepared<'_> {
    /// Of rows as they come in their order, each sorting by its value in `keys` where given, the
    /// rows they become past the sort (`late_rows`), through `cut` where there is one.
    fn after_sort(
        &self,
        rows: Vec<Vec<Value>>,
        keys: &[Vec<Value>],
        mut cut: Option<&mut sorted::Cut>,
        frame: &Frame,
    ) -> Result<Vec<Vec<Value>>> {
        // Rows carrying nothing, with no cut, go on as they came.
        if self.drawn.is_empty() && cut.is_none() {
            return Ok(rows);
        }
        let mut out = Vec::new();
        for (i, row) in rows.into_iter().enumerate() {
            let mut taking = Taking {
                cut: cut.as_deref_mut(),
                key: keys.get(i).map_or(&[][..], Vec::as_slice),
                out: &mut out,
            };
            if !taking.wants() {
                break;
            }
            self.late_rows(row, frame, &mut taking)?;
        }
        Ok(out)
    }
}

/// Where the rows a step makes go as they are made: through its cut where it has one, each sorting
/// by `key`, or all of them.
pub(super) struct Taking<'c> {
    cut: Option<&'c mut sorted::Cut>,
    key: &'c [Value],
    out: &'c mut Vec<Vec<Value>>,
}

impl Taking<'_> {
    /// Whether it reads one row more.
    fn wants(&self) -> bool {
        self.cut.as_ref().is_none_or(|c| c.wants())
    }

    fn take(&mut self, row: Vec<Value>) {
        match self.cut.as_deref_mut() {
            Some(c) => c.offer(row, self.key, self.out),
            None => self.out.push(row),
        }
    }
}

/// Whether an expression calls a function that moves a sequence or reads what the session drew,
/// or one that draws a random value or seeds the draws, each volatile in PostgreSQL's catalog.
pub(super) fn draws(e: &Expr) -> bool {
    let mut found = false;
    let _ = sqlparser::ast::visit_expressions(e, |x| {
        if let Expr::Function(f) = x {
            let name = function_name(f);
            found |= matches!(name.as_str(), "nextval" | "setval" | "currval" | "lastval")
                || super::prng::is_drawing(&name);
        }
        std::ops::ControlFlow::<()>::Continue(())
    });
    found
}

/// COALESCE, LEAST, GREATEST and NULLIF, each in the type the step resolved it to: COALESCE its
/// first argument not NULL, read in turn and no further; LEAST and GREATEST the least and the
/// greatest of those not NULL, under the collation their arguments settle; NULLIF its first
/// argument, NULL where = holds of the two. None for any other call.
fn chosen(
    e: &Expr,
    name: &str,
    a: &[&Expr],
    row: &[Value],
    frame: &Frame,
    computed: &Computed,
) -> Result<Option<Value>> {
    let ev = |x: &Expr| eval(x, row, frame, computed);
    let ty = frame.type_of(e);
    let typed = |v: Value, x: &Expr| -> Result<Value> {
        match ty {
            Some(t) if !v.is_null() => Ok(part_value(t, &part_of(t, v, quoted(x))?)),
            _ => Ok(v),
        }
    };
    Ok(Some(match (name, a) {
        ("coalesce", _) => {
            for &x in a {
                let v = ev(x)?;
                if !v.is_null() {
                    return Ok(Some(typed(v, x)?));
                }
            }
            Value::Null
        }
        ("least" | "greatest", _) => {
            let mut best: Option<Value> = None;
            for &x in a {
                let v = ev(x)?;
                if v.is_null() {
                    continue;
                }
                let v = typed(v, x)?;
                best = Some(match best {
                    None => v,
                    Some(b) => {
                        let under = frame.casings.compared(e, "string comparison")?;
                        let wanted = match name {
                            "least" => Ordering::Less,
                            _ => Ordering::Greater,
                        };
                        match compare_under(under, &v, &b)? == Some(wanted) {
                            true => v,
                            false => b,
                        }
                    }
                });
            }
            best.unwrap_or(Value::Null)
        }
        ("nullif", [x, y]) => {
            let v = ev(x)?;
            if v.is_null() {
                return Ok(Some(Value::Null));
            }
            let v = typed(v, x)?;
            let w = ev(y)?;
            if w.is_null() {
                return Ok(Some(v));
            }
            let w = match quoted(y) {
                true => typed(w, y)?,
                false => w,
            };
            let under = frame.casings.compared(e, "string comparison")?;
            match compare_under(under, &v, &w)? == Some(Ordering::Equal) {
                true => Value::Null,
                false => v,
            }
        }
        _ => return Ok(None),
    }))
}

/// What one level's sets make over a row beneath them, each handed to `each` as it is made: their
/// values side by side, a shorter set NULL past its end, until the longest ends; nothing where every
/// set gives none. False where `each` stopped them.
fn zipped<'q>(
    sets: &[&Expr],
    row: &[Value],
    frame: &Frame,
    computed: &Computed<'q>,
    each: &mut dyn FnMut(Computed<'q>) -> Result<bool>,
) -> Result<bool> {
    let mut values = sets
        .iter()
        .map(|&s| set_iter(s, row, frame, computed))
        .collect::<Result<Vec<_>>>()?;
    loop {
        let mut given = false;
        let mut now = Vec::with_capacity(sets.len());
        for v in &mut values {
            let x = v.next().transpose()?;
            given |= x.is_some();
            now.push(x.unwrap_or(Value::Null));
        }
        if !given {
            return Ok(true);
        }
        if !each(computed.and(sets.iter().copied().zip(now)))? {
            return Ok(false);
        }
    }
}

/// The values a set-returning call gives over one row, under the collation its arguments settle; a
/// regular expression's refused where they settle none.
fn set_values(c: &Expr, row: &[Value], frame: &Frame, computed: &Computed) -> Result<Vec<Value>> {
    let Expr::Function(f) = c else {
        return Err(Error::new(format!("{c} is not read")));
    };
    let a = args(f)?;
    let values = a
        .iter()
        .map(|x| Ok((eval(x, row, frame, computed)?, quoted(x))))
        .collect::<Result<Vec<_>>>()?;
    if super::handed::is_handed(f) {
        let types: Vec<_> = a.iter().map(|x| frame.type_of(x)).collect();
        if let Some(v) = super::handed::rows(f, values.clone(), &types)? {
            return Ok(v);
        }
    }
    let name = function_name(f);
    if let Some(v) = super::populate::sets(&name, &values, frame.type_of(c))? {
        return Ok(v);
    }
    if let Some(v) = super::clock::zone_sets(&name, frame.now)? {
        return Ok(v);
    }
    if name == super::settings::ROWS {
        let ty = super::settings::record();
        let columns = super::settings::columns();
        return super::settings::rows(frame.release)?
            .into_iter()
            .map(|r| {
                let items = r
                    .into_iter()
                    .zip(&columns)
                    .map(|(v, (_, t))| part_of(*t, v, false))
                    .collect::<Result<Vec<_>>>()?;
                Ok(composed(ty, crate::composed::Item::Record(items)))
            })
            .collect();
    }
    let casing = match super::calls::is_regexp_function(&name) {
        true => regex_casing(frame, c, &values.iter().map(|(v, _)| v).collect::<Vec<_>>())?,
        false => string_casing(frame, c, &name, &[])?,
    };
    let values = super::tsearch::named_setting(&name, values, &frame.casings)?;
    super::sets::values(&name, values, frame.type_of(c), casing)
}

/// Whether a function in FROM gives one row a call: a scalar function handed in, or one making a
/// value of a row type.
pub(super) fn gives_a_row(f: &sqlparser::ast::Function) -> bool {
    let name = function_name(f);
    !is_set_returning(&name)
        && (super::handed::is_handed(f) || super::populate::ROWS.contains(&name.as_str()))
}

/// The values a function in FROM gives for one row of the relations before it.
pub(super) fn call_rows(c: &Expr, row: &[Value], frame: &Frame) -> Result<Vec<Value>> {
    match c {
        Expr::Function(f) if gives_a_row(f) => Ok(vec![eval(c, row, frame, &computed_none())?]),
        _ => set_values(c, row, frame, &computed_none()),
    }
}

/// The values a function of a record of no type of its own gives in FROM for one row of the
/// relations before it, each a record of the type its column definition list makes.
pub(super) fn defined_rows(
    c: &Expr,
    row: &[Value],
    frame: &Frame,
    record: ColumnType,
) -> Result<Vec<Value>> {
    let Expr::Function(f) = c else {
        return Err(Error::new(format!("{c} is not read")));
    };
    let none = computed_none();
    let given = args(f)?
        .iter()
        .map(|x| Ok((eval(x, row, frame, &none)?, quoted(x))))
        .collect::<Result<Vec<_>>>()?;
    super::populate::defined(&function_name(f), &given, record)
}

/// The values a generate_series in FROM gives for one row of the relations before it, made one at
/// a time, `ty` the type its column takes; none where the call is another function's.
pub(super) fn call_series(
    c: &Expr,
    row: &[Value],
    frame: &Frame,
    ty: Option<ColumnType>,
) -> Result<Option<super::series::Series>> {
    let Expr::Function(f) = c else {
        return Ok(None);
    };
    if function_name(f) != "generate_series" {
        return Ok(None);
    }
    let none = computed_none();
    let given = args(f)?
        .iter()
        .map(|x| Ok((eval(x, row, frame, &none)?, quoted(x))))
        .collect::<Result<Vec<_>>>()?;
    super::series::series(given, ty).map(Some)
}

/// A record's fields, each a value of its own type; NULLs where the record is one.
pub(super) fn record_values(v: &Value, n: usize) -> Result<Vec<Value>> {
    use crate::composed::{Composed, Item};
    if let Value::Composed(ColumnType::Composed(r), c) = v {
        if let (Composed::Record { fields, .. }, Item::Record(items)) = (r.def(), &c.item) {
            return Ok(fields
                .iter()
                .zip(items)
                .map(|(f, i)| part_value(f.ty, i))
                .collect());
        }
        if c.item.is_null() {
            return Ok(vec![Value::Null; n]);
        }
    }
    Err(Error::new(format!(
        "{} is no record of {n} columns",
        kind_name(v)
    )))
}

/// Whether two values are equal, as = compares them; unknown where either is NULL.
pub(super) fn equal(a: &Value, b: &Value) -> Result<Option<bool>> {
    if a.is_null() || b.is_null() {
        return Ok(None);
    }
    Ok(Some(compare(a, b)? == Some(Ordering::Equal)))
}

/// The position in the select list a GROUP BY or ORDER BY item names, counted from 0, as
/// PostgreSQL's parser reads one: an integer constant, written with its minus or in parentheses,
/// names the item at that place, and any other constant is refused. None where it is an
/// expression.
fn ordinal(e: &Expr, clause: &str, items: usize) -> Result<Option<usize>> {
    let text = match e {
        Expr::Nested(x) => return ordinal(x, clause, items),
        Expr::Value(v) => match &v.value {
            SqlValue::Placeholder(_) => return Ok(None),
            SqlValue::Number(s, _) => s.clone(),
            _ => String::new(),
        },
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr,
        } => match expr.as_ref() {
            Expr::Value(v) => match &v.value {
                SqlValue::Number(s, _) => format!("-{s}"),
                _ => return Ok(None),
            },
            _ => return Ok(None),
        },
        _ => return Ok(None),
    };
    let Ok(n) = text.parse::<i32>() else {
        return Err(Error::new(format!("non-integer constant in {clause}")));
    };
    match usize::try_from(n) {
        Ok(at) if (1..=items).contains(&at) => Ok(Some(at - 1)),
        _ => Err(Error::new(format!(
            "{clause} position {n} is not in select list"
        ))),
    }
}

/// What a GROUP BY item groups by, as PostgreSQL's parser reads it: an ordinal the select list's
/// item at that place, where it holds no aggregate; a bare name no column of the FROM bears the
/// select list's item of that name; any other an expression over the FROM's columns.
fn grouped_by<'q>(
    g: &'q Expr,
    items: &[(&'q Expr, String)],
    columns: &[super::typed::TypedColumn],
) -> Result<&'q Expr> {
    let named = match ordinal(g, "GROUP BY", items.len())? {
        Some(at) => Some(items[at].0),
        None => match g {
            Expr::Identifier(i) if !columns.iter().any(|(_, n, _)| *n == ident(i)) => {
                let name = ident(i);
                let mut found = items.iter().filter(|(_, n)| *n == name).map(|(e, _)| *e);
                let first = found.next();
                if let Some(e) = first {
                    if found.any(|other| other != e) {
                        return Err(Error::new(format!("GROUP BY \"{name}\" is ambiguous")));
                    }
                }
                first
            }
            _ => None,
        },
    };
    let Some(e) = named else {
        // GROUPING written in GROUP BY is refused as it is read.
        let mut calls = Calls::default();
        find_calls(g, &mut calls);
        if !calls.groupings.is_empty() {
            return Err(Error::new("grouping operations are not allowed in GROUP BY"));
        }
        return Ok(g);
    };
    let mut calls = Calls::default();
    find_calls(e, &mut calls);
    if !calls.aggregates.is_empty() || !calls.groupings.is_empty() {
        return Err(Error::new(
            "aggregate functions are not allowed in GROUP BY",
        ));
    }
    if !calls.windows.is_empty() {
        return Err(Error::new("window functions are not allowed in GROUP BY"));
    }
    Ok(e)
}

/// What a step makes below its fold and its windows, as PostgreSQL's planner places a set-returning
/// call beneath them: each GROUP BY expression, PARTITION BY expression and window ORDER BY
/// expression holding a set, once for those written alike; and where each is read, itself and any
/// item, ORDER BY item or part of one written alike. A GROUP BY item refused is left to `grouped_by`.
fn below_fold<'q>(
    s: &'q sqlparser::ast::Select,
    items: &[(&'q Expr, String)],
    hidden: &[&'q Expr],
    columns: &[super::typed::TypedColumn],
) -> (Vec<&'q Expr>, Below) {
    let listed = || items.iter().map(|(e, _)| *e).chain(hidden.iter().copied());
    let mut found = Calls::default();
    for e in listed() {
        find_calls(e, &mut found);
    }
    let grouped: Vec<&'q Expr> = match &s.group_by {
        GroupByExpr::Expressions(g, m) if m.is_empty() => super::grouping::each_expr(g)
            .into_iter()
            .filter_map(|g| grouped_by(g, items, columns).ok())
            .collect(),
        _ => Vec::new(),
    };
    let keyed = found.windows.iter().flat_map(|w| match w {
        Expr::Function(f) => match &f.over {
            Some(WindowType::WindowSpec(spec)) => spec
                .partition_by
                .iter()
                .chain(spec.order_by.iter().map(|o| &o.expr))
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    });
    let keys: Vec<&'q Expr> = grouped.into_iter().chain(keyed).filter(|e| holds_set(e)).collect();
    let mut exprs: Vec<&'q Expr> = Vec::new();
    for &k in &keys {
        if !exprs.iter().any(|b| alike(b, k, columns, None)) {
            exprs.push(k);
        }
    }
    let mut at = Vec::new();
    if !exprs.is_empty() {
        let mut read = |x: &Expr| {
            if let Some(k) = exprs.iter().position(|b| alike(b, x, columns, None)) {
                at.push((x as *const Expr, k));
            }
        };
        for &k in &keys {
            read(k);
        }
        for e in listed() {
            let _ = sqlparser::ast::visit_expressions(e, |x| {
                read(x);
                std::ops::ControlFlow::<()>::Continue(())
            });
        }
    }
    let n = exprs.len();
    (exprs, Below { at, n })
}

/// The place in the select list an ORDER BY or DISTINCT ON item names, as PostgreSQL's parser
/// reads one, `clause` naming which: an ordinal's place; a bare name's item of that name, two
/// different ones refused; an expression the select list writes alike. None where it is an
/// expression over the FROM's columns.
fn sorted_by(
    e: &Expr,
    items: &[(&Expr, String)],
    columns: &[super::typed::TypedColumn],
    clause: &str,
) -> Result<Option<usize>> {
    if let Some(at) = ordinal(e, clause, items.len())? {
        return Ok(Some(at));
    }
    let mut bare = e;
    while let Expr::Nested(x) = bare {
        bare = x;
    }
    if let Expr::Identifier(i) = bare {
        let name = ident(i);
        let mut found = items.iter().enumerate().filter(|(_, (_, n))| *n == name);
        if let Some((at, (first, _))) = found.next() {
            if found.any(|(_, (other, _))| !alike(other, first, columns, None)) {
                return Err(Error::new(format!("{clause} \"{name}\" is ambiguous")));
            }
            return Ok(Some(at));
        }
    }
    Ok(items.iter().position(|(x, _)| alike(x, e, columns, None)))
}

/// The place of an ORDER BY or DISTINCT ON expression among those a step makes beside its select
/// list until they are sorted: one written alike, or a new one.
fn hidden_place<'q>(
    e: &'q Expr,
    hidden: &mut Vec<&'q Expr>,
    items: usize,
    columns: &[super::typed::TypedColumn],
) -> usize {
    match hidden.iter().position(|h| alike(h, e, columns, None)) {
        Some(at) => items + at,
        None => {
            hidden.push(e);
            items + hidden.len() - 1
        }
    }
}

/// Whether two expressions are one as PostgreSQL's parser compares them once it has read them.
fn alike(
    a: &Expr,
    b: &Expr,
    columns: &[super::typed::TypedColumn],
    types: Option<&super::typed::Types>,
) -> bool {
    a == b || as_read(a, columns, types) == as_read(b, columns, types)
}

/// An expression as PostgreSQL's parser leaves it: each column reference by the place of the
/// FROM's column it names, unquoted names folded, parentheses passed over; and where its types are
/// given, each cast to its operand's own type passed over.
fn as_read(
    e: &Expr,
    columns: &[super::typed::TypedColumn],
    types: Option<&super::typed::Types>,
) -> Expr {
    use sqlparser::ast::{Ident, Visit, Visitor};
    use std::ops::ControlFlow;
    // Whether each expression in it, in the order a visit leaves them, is a cast to its operand's
    // own type; its copy is visited in the same order.
    struct Casts<'t>(Option<&'t super::typed::Types>, Vec<bool>);
    impl Visitor for Casts<'_> {
        type Break = ();
        fn post_visit_expr(&mut self, e: &Expr) -> ControlFlow<()> {
            let passed = match (e, self.0) {
                (
                    Expr::Cast {
                        expr, data_type, ..
                    },
                    Some(t),
                ) => {
                    let to = t.get(&(e as *const Expr));
                    to.is_some()
                        && to == t.get(&(&**expr as *const Expr))
                        && super::handed::domain_named(data_type).is_none()
                }
                _ => false,
            };
            self.1.push(passed);
            ControlFlow::Continue(())
        }
    }
    let mut casts = Casts(types, Vec::new());
    let _ = e.visit(&mut casts);
    let folded = |i: &mut Ident| {
        if i.quote_style.is_none() {
            i.value = i.value.to_lowercase();
        }
    };
    let mut out = e.clone();
    let mut at = 0;
    let _ = sqlparser::ast::visit_expressions_mut(&mut out, |x| {
        let passed = casts.1.get(at).copied().unwrap_or(false);
        at += 1;
        if let Some(i) = resolved_place(x, columns) {
            *x = Expr::Identifier(Ident::with_quote('"', format!("{MERGED}{i}")));
            return ControlFlow::<()>::Continue(());
        }
        match x {
            Expr::Nested(inner) => {
                let inner = (**inner).clone();
                *x = inner;
            }
            Expr::Cast { expr, .. } if passed => {
                let inner = (**expr).clone();
                *x = inner;
            }
            Expr::Identifier(i) => folded(i),
            Expr::CompoundIdentifier(parts) => parts.iter_mut().for_each(folded),
            Expr::Function(f) => {
                for p in &mut f.name.0 {
                    if let ObjectNamePart::Identifier(i) = p {
                        folded(i);
                    }
                }
            }
            _ => {}
        }
        ControlFlow::Continue(())
    });
    out
}

/// The place of the column of the FROM a reference names, where it names one.
fn resolved_place(e: &Expr, columns: &[super::typed::TypedColumn]) -> Option<usize> {
    let (q, n) = column_of(e)?;
    match naming(
        columns
            .iter()
            .map(|(cq, cn, _)| (cq.as_deref(), cn.as_str())),
        q.as_deref(),
        &n,
    )
    .as_slice()
    {
        [i] => Some(*i),
        _ => None,
    }
}

/// The column of the FROM a reference names, where it names one.
fn resolved<'c>(
    e: &Expr,
    columns: &'c [super::typed::TypedColumn],
) -> Option<&'c super::typed::TypedColumn> {
    columns.get(resolved_place(e, columns)?)
}

/// The first column an expression of a grouped step reads that is neither grouped by, nor read
/// inside an aggregate, nor of a table read under a name whose primary key is grouped by: by its
/// relation's name and its own.
fn ungrouped(
    e: &Expr,
    grouping: &[&Expr],
    columns: &[super::typed::TypedColumn],
    decided: &[&str],
) -> Option<(String, String)> {
    let same = |g: &Expr| {
        g == e
            || matches!((resolved(g, columns), resolved(e, columns)),
                (Some(a), Some(b)) if std::ptr::eq(a, b))
    };
    if grouping.iter().any(|g| same(g)) {
        return None;
    }
    match e {
        Expr::Function(f)
            if f.over.is_none()
                && (is_aggregate(&function_name(f))
                    || !f.within_group.is_empty()
                    || is_grouping_call(f)) =>
        {
            None
        }
        Expr::Identifier(_) | Expr::CompoundIdentifier(_) => {
            let (q, n, _) = resolved(e, columns)?;
            // A column of the query around a subquery is one value for each of its groups.
            if outer_level(q.as_deref()) > 0 {
                return None;
            }
            let q = q.as_deref().map_or("", unmarked).to_string();
            (!decided.contains(&q.as_str())).then(|| (q, n.clone()))
        }
        // A window reads its arguments, FILTER, PARTITION BY and ORDER BY on each group.
        Expr::Function(f) if f.over.is_some() => window_parts(e)
            .into_iter()
            .find_map(|x| ungrouped(x, grouping, columns, decided)),
        _ => {
            let mut parts = parts_of(e);
            if parts.is_empty() {
                parts = super::collate::children(e);
            }
            parts
                .into_iter()
                .find_map(|x| ungrouped(x, grouping, columns, decided))
        }
    }
}

/// Each place in `e` one of the expressions a step groups by stands whole, by its place among them:
/// the whole before its parts, none inside an aggregate, GROUPING or a subquery, as PostgreSQL's
/// parser puts a group's value in its place; a window's in what it reads.
fn grouped_places(
    e: &Expr,
    grouping: &[&Expr],
    same: &dyn Fn(&Expr, &Expr) -> bool,
    out: &mut Vec<(*const Expr, usize)>,
) {
    if let Some(i) = grouping.iter().position(|g| same(g, e)) {
        out.push((e as *const Expr, i));
        return;
    }
    match e {
        Expr::Function(f) if f.over.is_some() => {
            for x in window_parts(e) {
                grouped_places(x, grouping, same, out);
            }
        }
        Expr::Function(f)
            if is_grouping_call(f)
                || is_aggregate(&function_name(f))
                || !f.within_group.is_empty() => {}
        // What a subquery is compared with is the step's; the subquery its own.
        Expr::InSubquery { expr, .. } => grouped_places(expr, grouping, same, out),
        Expr::AnyOp { left, .. } | Expr::AllOp { left, .. } if is_subquery(e) => {
            grouped_places(left, grouping, same, out)
        }
        e if is_subquery(e) => {}
        e => {
            let mut parts = parts_of(e);
            if parts.is_empty() {
                parts = super::collate::children(e);
            }
            for p in parts {
                grouped_places(p, grouping, same, out);
            }
        }
    }
}

/// The rows a step returns of those it makes: its LIMIT's or FETCH FIRST's count, its OFFSET, and
/// whether the rows that tie by its ORDER BY with the last it returns come too (WITH TIES).
#[derive(Clone)]
struct Limit<'q> {
    count: Count<'q>,
    offset: Option<&'q Expr>,
    ties: bool,
}

/// How many rows a step returns at most: all, one (a FETCH FIRST that gives no count), or as many
/// as an expression says.
#[derive(Clone)]
enum Count<'q> {
    All,
    One,
    Of(&'q Expr),
}

impl<'q> Limit<'q> {
    /// A step's LIMIT, FETCH FIRST and OFFSET, as PostgreSQL's grammar reads them; none where it
    /// has neither a count nor an offset.
    fn of(q: &'q Query) -> Result<Option<Limit<'q>>> {
        let (count, offset) = match &q.limit_clause {
            None => (Count::All, None),
            Some(sqlparser::ast::LimitClause::LimitOffset {
                limit,
                offset,
                limit_by,
            }) => {
                if !limit_by.is_empty() {
                    return Err(Error::new("syntax error at or near \"BY\""));
                }
                (
                    limit.as_ref().map_or(Count::All, Count::Of),
                    offset.as_ref().map(|o| &o.value),
                )
            }
            Some(sqlparser::ast::LimitClause::OffsetCommaLimit { .. }) => {
                return Err(Error::new("LIMIT #,# syntax is not supported"))
            }
        };
        let (count, ties) = match &q.fetch {
            None => (count, false),
            Some(_) if !matches!(count, Count::All) => {
                return Err(Error::new("syntax error at or near \"FETCH\""))
            }
            Some(f) if f.percent => return Err(Error::new("syntax error at or near \"PERCENT\"")),
            Some(f) => {
                if f.with_ties && q.order_by.is_none() {
                    return Err(Error::new(
                        "WITH TIES cannot be specified without ORDER BY clause",
                    ));
                }
                if f.with_ties
                    && matches!(&f.quantity, Some(Expr::Value(v)) if matches!(v.value, SqlValue::Null))
                {
                    return Err(Error::new(
                        "row count cannot be null in FETCH FIRST ... WITH TIES clause",
                    ));
                }
                (
                    f.quantity.as_ref().map_or(Count::One, Count::Of),
                    f.with_ties,
                )
            }
        };
        if matches!(count, Count::All) && offset.is_none() {
            return Ok(None);
        }
        Ok(Some(Limit {
            count,
            offset,
            ties,
        }))
    }

    /// Its expressions, each with the clause PostgreSQL names it by.
    fn exprs(&self) -> impl Iterator<Item = (&'q Expr, &'static str)> {
        let count = match self.count {
            Count::Of(e) => Some((e, "LIMIT")),
            _ => None,
        };
        self.offset.map(|e| (e, "OFFSET")).into_iter().chain(count)
    }

    /// Each of its expressions refused as PostgreSQL's parser refuses one: an aggregate, a window
    /// or a function returning a set in it; a type no assignment casts to bigint; a column.
    fn check(&self, typing: &mut super::typed::Typing) -> Result<()> {
        for (e, what) in self.exprs() {
            let mut calls = Calls::default();
            find_calls(e, &mut calls);
            if !calls.aggregates.is_empty() {
                return Err(Error::new(format!(
                    "aggregate functions are not allowed in {what}"
                )));
            }
            if !calls.groupings.is_empty() {
                return Err(Error::new(format!(
                    "grouping operations are not allowed in {what}"
                )));
            }
            if !calls.windows.is_empty() {
                return Err(Error::new(format!(
                    "window functions are not allowed in {what}"
                )));
            }
            sets_refused(e, what)?;
            match typing.type_of(e)? {
                None
                | Some(
                    ColumnType::Int2
                    | ColumnType::Int4
                    | ColumnType::Int8
                    | ColumnType::Numeric { .. }
                    | ColumnType::NumericText(_)
                    | ColumnType::Float4
                    | ColumnType::Float8
                    | ColumnType::Oid,
                ) => {}
                Some(t) => {
                    return Err(Error::new(format!(
                        "argument of {what} must be type bigint, not type {}",
                        t.pg_name()
                    )))
                }
            }
            if reads_a_column(e) {
                return Err(Error::new(format!(
                    "argument of {what} must not contain variables"
                )));
            }
        }
        Ok(())
    }

}

/// Whether two rows' sort keys tie: each pair NULL alike or equal.
fn tie(a: &[Value], b: &[Value]) -> bool {
    a.iter().zip(b).all(|(x, y)| {
        if x.is_null() || y.is_null() {
            x.is_null() == y.is_null()
        } else {
            matches!(compare(x, y), Ok(Some(Ordering::Equal)))
        }
    })
}

/// Whether an expression reads a column.
fn reads_a_column(e: &Expr) -> bool {
    if matches!(e, Expr::Identifier(_) | Expr::CompoundIdentifier(_)) {
        return true;
    }
    let mut parts = parts_of(e);
    if parts.is_empty() {
        parts = super::collate::children(e);
    }
    parts.into_iter().any(reads_a_column)
}

/// A step read before any row: its parts, each expression typed over the columns its FROM reads,
/// the expressions its FROM reads its relations by (`joined`) among them.
pub(super) fn prepare<'q>(
    q: &'q Query,
    columns: &[super::typed::TypedColumn],
    keyed: &[(String, Vec<String>)],
    release: u8,
    now: i64,
    joined: &[&'q Expr],
    seeded: super::typed::Types,
    around: &[usize],
) -> Result<Prepared<'q>> {
    let s = match q.body.as_ref() {
        SetExpr::Select(s) => s,
        _ => return Err(Error::new("a step is a SELECT")),
    };
    // SELECT ALL returns every row, as a SELECT that says neither.
    let distinct = matches!(s.distinct, Some(sqlparser::ast::Distinct::Distinct));
    let on = match &s.distinct {
        Some(sqlparser::ast::Distinct::On(keys)) => keys.as_slice(),
        _ => &[],
    };
    let limit = Limit::of(q)?;
    let items: Vec<(&Expr, String)> = s
        .projection
        .iter()
        .map(|i| match i {
            SelectItem::UnnamedExpr(e) => Ok((e, default_name(e))),
            SelectItem::ExprWithAlias { expr, alias } => Ok((expr, ident(alias))),
            _ => Err(Error::new("name each column a step returns")),
        })
        .collect::<Result<_>>()?;
    // ORDER BY sorts by the select list's column an item names by its place or its bare name, or
    // that it writes alike; any other is an expression over the FROM's columns, whose values the
    // step makes beside its own until they are sorted.
    let mut sorted = Vec::new();
    let mut hidden: Vec<&Expr> = Vec::new();
    if let Some(OrderByKind::Expressions(os)) = q.order_by.as_ref().map(|o| &o.kind) {
        for o in os {
            sorted.push(match sorted_by(&o.expr, &items, columns, "ORDER BY")? {
                Some(at) => at,
                None => hidden_place(&o.expr, &mut hidden, items.len(), columns),
            });
        }
    }
    let (below_exprs, below) = below_fold(s, &items, &hidden, columns);
    // Its sets are refused in PostgreSQL's parser's order: the select list's, WHERE's, HAVING's,
    // then the ORDER BY's.
    let mut calls = Calls {
        stops: below.at.iter().map(|&(p, _)| p).collect(),
        ..Calls::default()
    };
    for (e, _) in &items {
        sets_placed(e)?;
    }
    for e in items.iter().map(|(e, _)| *e).chain(hidden.iter().copied()) {
        find_calls(e, &mut calls);
    }
    if let Some(w) = s.selection.as_ref() {
        sets_refused(w, "WHERE")?;
    }
    if let Some(w) = s.selection.as_ref() {
        no_own_calls(w, columns, "WHERE")?;
    }
    // HAVING keeps the groups its condition holds of, its aggregates folded with the others'.
    let having = s.having.as_ref();
    let having_groupings = calls.groupings.len();
    if let Some(h) = having {
        sets_refused(h, "HAVING")?;
        let windows = calls.windows.len();
        find_calls(h, &mut calls);
        if calls.windows.len() > windows {
            return Err(Error::new("window functions are not allowed in HAVING"));
        }
    }
    // An aggregate a subquery read on each group calls over this step's columns alone is this
    // step's: computed on each group, and handed to the subquery.
    let own: Vec<(Option<&str>, &str)> = columns
        .iter()
        .filter(|(q, _, _)| outer_level(q.as_deref()) == 0)
        .map(|(q, n, _)| (q.as_deref(), n.as_str()))
        .collect();
    let of_step = |q: &str, n: &str| !naming(own.iter().copied(), Some(q), n).is_empty();
    let mut lifted: Vec<&'q Expr> = Vec::new();
    for e in items
        .iter()
        .map(|(e, _)| *e)
        .chain(hidden.iter().copied())
        .chain(having)
    {
        for sub in super::subqueries::read_on_groups(e) {
            if let Some(asked) = super::subqueries::asked_query(sub) {
                lifted.extend(super::subqueries::lifted(asked, &of_step));
            }
        }
    }
    calls.aggregates.extend(lifted.iter().copied());
    let having_groupings = having_groupings..calls.groupings.len();
    nested_calls(&calls)?;
    for e in &hidden {
        sets_placed(e)?;
    }
    let group_by = match &s.group_by {
        GroupByExpr::Expressions(g, m) if m.is_empty() => g.as_slice(),
        _ => return Err(Error::new("GROUP BY ALL is not read")),
    };
    // Its grouping expressions, each once, alike as a column read outside an aggregate is to one
    // of them (`ungrouped`), and its sets.
    let same = |g: &Expr, e: &Expr| {
        g == e
            || matches!((resolved(g, columns), resolved(e, columns)),
                (Some(a), Some(b)) if std::ptr::eq(a, b))
    };
    let read = super::grouping::expand(group_by, &mut |g| grouped_by(g, &items, columns), &same)?;
    let grouping = read.exprs.clone();
    // Each DISTINCT ON key is read as an ORDER BY item is, after the ORDER BY and the GROUP BY, as
    // PostgreSQL's parser reads it: the select list's column it names, or an expression the ORDER
    // BY makes beside it, or one more of its own.
    let mut keys = Vec::with_capacity(on.len());
    for k in on {
        keys.push(match sorted_by(k, &items, columns, "DISTINCT ON")? {
            Some(at) => at,
            None => {
                let before = hidden.len();
                let at = hidden_place(k, &mut hidden, items.len(), columns);
                if hidden.len() > before {
                    sets_placed(k)?;
                    find_calls(k, &mut calls);
                }
                at
            }
        });
    }
    let grouped = !grouping.is_empty()
        || !calls.aggregates.is_empty()
        || !calls.groupings.is_empty()
        || having.is_some()
        || read.written;
    let calls_windows_whole = calls.windows.iter().any(|w| !streams(w));
    // Each GROUPING call's arguments, each a grouping expression of the step's.
    let mut grouping_args = Vec::with_capacity(calls.groupings.len());
    for &g in &calls.groupings {
        let places: Option<Vec<usize>> = grouping_args_of(g)
            .iter()
            .map(|a| grouping.iter().position(|x| same(x, a)))
            .collect();
        grouping_args.push(places);
    }
    let mut typing = super::typed::Typing {
        columns,
        release,
        types: seeded,
    };
    let returns = items
        .iter()
        .map(|(e, _)| typing.type_of(e))
        .collect::<Result<Vec<_>>>()?;
    if let Some(w) = s.selection.as_ref() {
        typing.condition(w, "WHERE")?;
    }
    for g in &grouping {
        typing.type_of(g)?;
    }
    for e in &hidden {
        typing.type_of(e)?;
    }
    if let Some(h) = having {
        typing.condition(h, "HAVING")?;
    }
    for e in joined {
        typing.type_of(e)?;
    }
    for a in &lifted {
        typing.type_of(a)?;
    }
    // Each key's operators found before any row, in the order PostgreSQL's parser reads them: an
    // aggregate's as it reads the call, the select list's and HAVING's first; then each ORDER BY
    // item, an aggregate it holds before it; then GROUP BY's; then DISTINCT's other items.
    let folded = |e: &'q Expr| {
        let mut c = Calls::default();
        find_calls(e, &mut c);
        c.aggregates
    };
    for e in items.iter().map(|(e, _)| *e).chain(having) {
        for call in folded(e) {
            folded_keys(call, &mut typing)?;
        }
    }
    if let Some(OrderByKind::Expressions(os)) = q.order_by.as_ref().map(|o| &o.kind) {
        for (o, &at) in os.iter().zip(&sorted) {
            let ty = match returns.get(at) {
                Some(t) => *t,
                None => {
                    for call in folded(hidden[at - items.len()]) {
                        folded_keys(call, &mut typing)?;
                    }
                    typing.type_of(hidden[at - items.len()])?
                }
            };
            sort_item(o, ty)?;
        }
    }
    for g in &grouping {
        grouped_type(typing.type_of(g)?)?;
    }
    if distinct {
        // Under DISTINCT an ORDER BY item is one of the select list's, read alike once each cast
        // to its operand's own type is passed over; any other is refused.
        let typed = Some(&typing.types);
        for at in sorted.iter_mut() {
            let Some(own) = at.checked_sub(items.len()) else {
                continue;
            };
            *at = items
                .iter()
                .position(|(x, _)| alike(x, hidden[own], columns, typed))
                .ok_or_else(|| {
                    Error::new(
                        "for SELECT DISTINCT, ORDER BY expressions must appear in select list",
                    )
                })?;
        }
        for (i, ty) in returns.iter().enumerate() {
            if !sorted.contains(&i) {
                grouped_type(*ty)?;
            }
        }
    }
    if let Some(l) = &limit {
        l.check(&mut typing)?;
    }
    let listed: Vec<&Expr> = items.iter().map(|(e, _)| *e).collect();
    windows::checked(&calls.windows, &listed, &mut typing, release)?;
    // DISTINCT ON returns the first row of each run of rows its keys tie in, sorted by the ORDER BY,
    // whose items before its first other than a key hold every key; a key after them is read only
    // where an item before it sorts by it. Under an ORDER BY of keys alone, the keys it leaves out
    // sort after them, ascending, NULLs last, as PostgreSQL's parser adds them.
    let distinct_on = match on.is_empty() {
        true => None,
        false => {
            // An expression beside the select list, a key or the ORDER BY's, is read as an item or
            // an expression before it alike once each cast to its operand's own type is passed over.
            let typed = Some(&typing.types);
            let read_as = |at: usize| -> usize {
                let Some(own) = at.checked_sub(items.len()) else {
                    return at;
                };
                let e = hidden[own];
                if let Some(i) = items.iter().position(|(x, _)| alike(x, e, columns, typed)) {
                    return i;
                }
                hidden[..own]
                    .iter()
                    .position(|h| alike(h, e, columns, typed))
                    .map_or(at, |h| items.len() + h)
            };
            for at in sorted.iter_mut() {
                *at = read_as(*at);
            }
            for at in keys.iter_mut() {
                *at = read_as(*at);
            }
            let unmatched = || {
                Error::new("SELECT DISTINCT ON expressions must match initial ORDER BY expressions")
            };
            let mut lead = 0;
            let mut passed = false;
            for (i, &at) in sorted.iter().enumerate() {
                match (keys.contains(&at), passed) {
                    (false, _) => passed = true,
                    (true, false) => lead = i + 1,
                    (true, true) if sorted[..i].contains(&at) => {}
                    (true, true) => return Err(unmatched()),
                }
            }
            for &k in &keys {
                if sorted[..lead].contains(&k) {
                    continue;
                }
                if passed {
                    return Err(unmatched());
                }
                let ty = match returns.get(k) {
                    Some(t) => *t,
                    None => typing.type_of(hidden[k - items.len()])?,
                };
                grouped_type(ty)?;
                sorted.push(k);
                lead = sorted.len();
            }
            // A set the select list makes where no sort key makes one, PostgreSQL makes after the
            // rows DISTINCT ON keeps, under an ORDER BY.
            let at = |i: usize| {
                items
                    .get(i)
                    .map_or_else(|| hidden[i - items.len()], |(e, _)| *e)
            };
            let after = items
                .iter()
                .enumerate()
                .any(|(i, (e, _))| !sorted.contains(&i) && holds_set(e));
            if q.order_by.is_some() && after && !sorted.iter().any(|&i| holds_set(at(i))) {
                return Err(Error::new(
                    "a set-returning function in the select list beside DISTINCT ON and ORDER BY \
                     is not read",
                ));
            }
            Some(lead)
        }
    };
    let arguments_grouped = |range: &mut dyn Iterator<Item = usize>| -> Result<()> {
        for i in range {
            if grouping_args[i].is_none() {
                return Err(Error::new(
                    "arguments to GROUPING must be grouping expressions of the associated query \
                     level",
                ));
            }
        }
        Ok(())
    };
    // Where it groups, a column read outside an aggregate is one it groups by, or one its group by
    // the table's primary key decides where the key's columns are in every set, as PostgreSQL's
    // parser holds it: GROUPING's arguments and then the columns of its select list and ORDER BY,
    // then HAVING's, once the rest is read.
    if grouped {
        read.refused()?;
        let common: Vec<&Expr> = read.common().into_iter().map(|i| grouping[i]).collect();
        let decided: Vec<&str> = keyed
            .iter()
            .filter(|(q, key)| {
                !key.is_empty()
                    && key.iter().all(|k| {
                        common.iter().any(|g| {
                            resolved(g, columns).is_some_and(|(cq, cn, _)| {
                                cq.as_deref().map(unmarked) == Some(q.as_str()) && cn == k
                            })
                        })
                    })
            })
            .map(|(q, _)| q.as_str())
            .collect();
        let ungrouped_in = |e: &Expr| -> Result<()> {
            match ungrouped(e, &grouping, columns, &decided) {
                Some((q, n)) => Err(Error::new(format!(
                    "column \"{q}.{n}\" must appear in the GROUP BY clause or be used in an \
                     aggregate function"
                ))),
                None => Ok(()),
            }
        };
        arguments_grouped(
            &mut (0..calls.groupings.len()).filter(|i| !having_groupings.contains(i)),
        )?;
        for e in items.iter().map(|(e, _)| *e).chain(hidden.iter().copied()) {
            ungrouped_in(e)?;
        }
        arguments_grouped(&mut having_groupings.clone())?;
        if let Some(h) = having {
            ungrouped_in(h)?;
        }
        // A subquery read on each group reads only what is one value for it.
        for &at in around {
            let e = Expr::Identifier(sqlparser::ast::Ident::new(format!("{MERGED}{at}")));
            if let Some((q, n)) = ungrouped(&e, &grouping, columns, &decided) {
                return Err(Error::new(format!(
                    "subquery uses ungrouped column \"{q}.{n}\" from outer query"
                )));
            }
        }
    }
    // Where it groups by more than one set, each place an expression it groups by stands whole in
    // its select list, ORDER BY, HAVING and aggregates' direct arguments.
    let (sets, whole) = match read.plain() {
        true => (None, Vec::new()),
        false => {
            let mut whole = Vec::new();
            let direct = calls
                .aggregates
                .iter()
                .filter(|c| matches!(c, Expr::Function(f) if !f.within_group.is_empty()))
                .flat_map(|&c| parts_of(c));
            for e in items
                .iter()
                .map(|(e, _)| *e)
                .chain(hidden.iter().copied())
                .chain(having)
                .chain(direct)
            {
                grouped_places(e, &grouping, &same, &mut whole);
            }
            (Some(read.sets), whole)
        }
    };
    let grouping_args = grouping_args
        .into_iter()
        .map(Option::unwrap_or_default)
        .collect();
    // An item that draws, from a sequence or at random, and is no sort key is made above the sort
    // and below the cut, as PostgreSQL's planner places a volatile item: in the rows' sorted order,
    // for each row the LIMIT and OFFSET read, beside grouping, windows and sets too. Under DISTINCT
    // ON, only where it sorts by an ORDER BY.
    // So are its select list's sets, where no sort key and nothing below its fold holds one and it
    // has no DISTINCT nor DISTINCT ON: each row carried past the sort becomes their rows, one at a
    // time as the cut reads them. Where a sort key holds one, they are made before the sort, and the
    // drawing items alone after.
    let cuts = q.order_by.is_some() || limit.is_some();
    let open = cuts && !distinct && (on.is_empty() || q.order_by.is_some());
    let place = |i: usize| {
        items
            .get(i)
            .map_or_else(|| hidden[i - items.len()], |(e, _)| *e)
    };
    let set_keyed = !below_exprs.is_empty()
        || sorted.iter().any(|&i| depth(place(i), &calls.stops) > 0);
    let late_sets = open && on.is_empty() && !calls.sets.is_empty() && !set_keyed;
    let drawn = match open {
        true => items
            .iter()
            .enumerate()
            .filter(|(i, (e, _))| {
                !sorted.contains(i)
                    && match depth(e, &calls.stops) > 0 {
                        true => late_sets,
                        false => draws(e),
                    }
            })
            .map(|(i, _)| i)
            .collect(),
        false => Vec::new(),
    };
    // The deepest level makes the items where none of its sets stands inside an expression.
    let levels = calls.levels();
    let listed = || items.iter().map(|(e, _)| *e).chain(hidden.iter().copied());
    let deepest = listed().map(|e| depth(e, &calls.stops)).max().unwrap_or(0);
    let ends = listed()
        .filter(|e| depth(e, &calls.stops) == deepest)
        .all(is_set_call);
    // The sets below the fold, by level, as the select list's are.
    let mut beneath = Calls::default();
    for &e in &below_exprs {
        find_calls(e, &mut beneath);
    }
    Ok(Prepared {
        drawn,
        late_sets,
        levels,
        ends,
        below_levels: beneath.levels(),
        below_exprs,
        below,
        from: &s.from,
        selection: s.selection.as_ref(),
        having,
        joined: joined.to_vec(),
        order_by: q.order_by.as_ref(),
        sorted,
        hidden,
        items,
        calls,
        grouping,
        sets,
        whole,
        grouping_args,
        grouped,
        whole_windows: calls_windows_whole,
        distinct,
        distinct_on,
        limit,
        types: typing.types,
        returns,
        carried: Vec::new(),
        now,
    })
}

/// An expression on one row whose columns the names give, read alone: a CHECK's, a generated
/// column's, or a DEFAULT's on no row.
pub(crate) fn eval_on(
    e: &Expr,
    names: &[String],
    row: &[Value],
    release: u8,
    casings: &Casings,
) -> Result<Value> {
    eval_on_as(e, None, names, row, release, casings)
}

/// An expression on one row as `eval_on` reads it, a call of the file's function whose body reads
/// relations asking `reads` where it is given.
pub(super) fn eval_on_reading(
    e: &Expr,
    names: &[String],
    row: &[Value],
    release: u8,
    casings: &Casings,
    reads: Option<&dyn super::relations::Reads>,
) -> Result<Value> {
    eval_as_reading(e, None, names, row, release, casings, reads)
}

/// An expression on one row whose columns are named each with its own qualifier, in the types its
/// typing gave it and each expression in it.
pub(crate) fn eval_typed(
    e: &Expr,
    columns: Vec<(Option<String>, String)>,
    row: &[Value],
    release: u8,
    casings: &Casings,
    types: &super::typed::Types,
) -> Result<Value> {
    let frame = Frame {
        columns,
        params: &[],
        release,
        casings,
        types,
        now: crate::scalars::transaction_clock(),
        subqueries: None,
        reads: None,
        below: None,
    };
    eval(e, row, &frame, &computed_none())
}

/// An expression on one row as `eval_typed` reads it, a call of the file's function whose body
/// reads relations asking `reads`.
pub(super) fn eval_typed_reading(
    e: &Expr,
    columns: Vec<(Option<String>, String)>,
    row: &[Value],
    release: u8,
    casings: &Casings,
    types: &super::typed::Types,
    reads: &dyn super::relations::Reads,
) -> Result<Value> {
    let frame = Frame {
        columns,
        params: &[],
        release,
        casings,
        types,
        now: crate::scalars::transaction_clock(),
        subqueries: None,
        reads: Some(reads),
        below: None,
    };
    eval(e, row, &frame, &computed_none())
}

/// An expression over no row, in the types its typing gave it, a call of the file's function whose
/// body reads relations asking `reads` where it is given.
pub(super) fn eval_reading(
    e: &Expr,
    reads: Option<&dyn super::relations::Reads>,
    release: u8,
    casings: &Casings,
    types: &super::typed::Types,
) -> Result<Value> {
    let frame = Frame {
        columns: Vec::new(),
        params: &[],
        release,
        casings,
        types,
        now: crate::scalars::transaction_clock(),
        subqueries: None,
        reads,
        below: None,
    };
    eval(e, &[], &frame, &computed_none())
}

/// The types of an expression on one row whose columns are those given, and of each expression in
/// it, as a step types them; refused before any row where PostgreSQL's parser refuses it.
pub(crate) fn typed_on(
    e: &Expr,
    columns: &[(Option<String>, String, Option<ColumnType>)],
    release: u8,
) -> Result<super::typed::Types> {
    let mut typing = super::typed::Typing {
        columns,
        release,
        types: Default::default(),
    };
    typing.type_of(e)?;
    Ok(typing.types)
}

/// An expression on one row, its columns named as `by` names them, or by their names alone.
pub(crate) fn eval_on_as(
    e: &Expr,
    by: Option<&str>,
    names: &[String],
    row: &[Value],
    release: u8,
    casings: &Casings,
) -> Result<Value> {
    eval_as_reading(e, by, names, row, release, casings, None)
}

fn eval_as_reading(
    e: &Expr,
    by: Option<&str>,
    names: &[String],
    row: &[Value],
    release: u8,
    casings: &Casings,
    reads: Option<&dyn super::relations::Reads>,
) -> Result<Value> {
    let types = super::typed::Types::new();
    let frame = Frame {
        columns: names
            .iter()
            .map(|n| (by.map(str::to_string), n.clone()))
            .collect(),
        params: &[],
        release,
        casings,
        types: &types,
        now: crate::scalars::transaction_clock(),
        subqueries: None,
        reads,
        below: None,
    };
    eval(e, row, &frame, &computed_none())
}

/// The values of a row's aggregates and windows: none.
pub(super) fn computed_none<'q>() -> Computed<'q> {
    Computed {
        calls: HashMap::new(),
        bound: None,
        _q: std::marker::PhantomData,
    }
}

/// A row's partition, as its windows' PARTITION BY keys it.
fn partition_key(by: &[Expr], row: &[Value], frame: &Frame) -> Result<Vec<Key>> {
    let none = computed_none();
    by.iter()
        .map(|p| {
            let v = eval(p, row, frame, &none)?;
            grouped_key(&v)?;
            Ok(key_under(&v, frame.casings.keyed(p)?))
        })
        .collect()
}

/// One partition's rows folded by a step's windows, and the rows the step returns for them.
fn fold_partition(
    p: &Prepared,
    rows: Vec<Vec<Value>>,
    frame: &Frame,
    out: &mut Vec<Vec<Value>>,
) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let rel = Relation {
        names: Vec::new(),
        rows,
        order: None,
    };
    let per_row = windows::values(&p.calls.windows, &rel, frame, None)?;
    for (row, w) in rel.rows.iter().zip(per_row) {
        let computed = Computed {
            calls: w,
            bound: None,
            _q: std::marker::PhantomData,
        };
        out.extend(p.project(row, frame, computed)?);
    }
    Ok(())
}

/// A step's groups of one set: each one's first row and its aggregates' folds, its key's place
/// among them, and past the room, the rows of the groups none has come for yet, written for a pass
/// of their own. `set` is the places of its members among the step's grouping expressions; where
/// the step groups by sets, a group's first row carries each grouping expression's value after it.
struct Groups {
    set: Vec<usize>,
    at: HashMap<Vec<Key>, usize>,
    list: Vec<(Vec<Value>, Vec<Acc>)>,
    held: crate::room::Holding,
    past: Option<super::spill::Parts>,
    queue: super::spill::Queue,
    range: (u64, u64),
    expected: Option<u64>,
    rows_to_come: Option<u64>,
    closed: bool,
    /// What its aggregates keep of the rows they are given, in the round's room.
    kept: crate::room::Holding,
    met: Met,
}

/// How a step's rows are read for its groups, once a chunk: each aggregate's parts, the casing each
/// grouping expression keys under and each aggregate compares under.
struct Reading<'e> {
    folded: Vec<Folds<'e>>,
    grouped_under: Vec<Option<Casing>>,
    folded_under: Vec<(Vec<Option<Casing>>, Option<String>)>,
    group_bytes: u64,
}

/// What a row gives each aggregate it is folded into: its direct arguments' values and its
/// arguments', none where its FILTER passes the row over.
type Given = Vec<Option<(Vec<Value>, Vec<Value>)>>;

impl<'e> Reading<'e> {
    fn of(p: &'e Prepared, frame: &Frame) -> Result<Reading<'e>> {
        let folded: Vec<Folds> = p
            .calls
            .aggregates
            .iter()
            .map(|&c| Acc::of(c, &p.types).map(|(_, f)| f))
            .collect::<Result<_>>()?;
        let grouped_under = p
            .grouping
            .iter()
            .map(|g| frame.casings.keyed(g))
            .collect::<Result<Vec<_>>>()?;
        let folded_under = p
            .calls
            .aggregates
            .iter()
            .zip(&folded)
            .map(|(&c, f)| frame.casings.folded(c, &f.args))
            .collect::<Result<Vec<_>>>()?;
        Ok(Reading {
            folded,
            grouped_under,
            folded_under,
            group_bytes: 32 * p.calls.aggregates.len() as u64,
        })
    }

    /// A row's grouping expressions' keys and values.
    fn keyed(&self, p: &Prepared, row: &[Value], frame: &Frame) -> Result<(Vec<Key>, Vec<Value>)> {
        let none = computed_none();
        let mut keys = Vec::with_capacity(p.grouping.len());
        let mut values = Vec::with_capacity(p.grouping.len());
        for (g, &u) in p.grouping.iter().zip(&self.grouped_under) {
            let v = eval(g, row, frame, &none)?;
            // A row constructor's record is compared as rows meet (`met`).
            if !matches!(&v, Value::Composed(ty, _) if anonymous_record(*ty)) {
                grouped_key(&v)?;
            }
            keys.push(key_under(&v, u));
            values.push(v);
        }
        Ok((keys, values))
    }

    /// What a row gives each aggregate (`Given`).
    fn given(&self, row: &[Value], frame: &Frame) -> Result<Given> {
        let none = computed_none();
        let mut out = Vec::with_capacity(self.folded.len());
        for f in &self.folded {
            if let Some(w) = f.filter {
                if holds(w, row, frame, &none)? != Some(true) {
                    out.push(None);
                    continue;
                }
            }
            let direct = f
                .direct
                .iter()
                .map(|x| eval(x, row, frame, &none))
                .collect::<Result<_>>()?;
            let values = f
                .args
                .iter()
                .map(|x| eval(x, row, frame, &none))
                .collect::<Result<_>>()?;
            out.push(Some((direct, values)));
        }
        Ok(out)
    }
}

/// Rows folded into the groups of each of a step's sets, each row's keys read once for all of them,
/// and what it gives the aggregates once for those it is folded in (`Groups::place`). Where `along`
/// is given, rows come in the order of their groups, each ended as its key changes.
fn fold_rows(
    groups: &mut [Groups],
    p: &Prepared,
    rows: Vec<Vec<Value>>,
    frame: &Frame,
    mut along: Option<&mut dyn FnMut(Vec<Vec<Value>>) -> Result<()>>,
) -> Result<()> {
    let reading = Reading::of(p, frame)?;
    for row in rows {
        let (keys, values) = reading.keyed(p, &row, frame)?;
        let mut given: Option<Given> = None;
        for g in groups.iter_mut() {
            let Some(i) = g.place(p, &reading, &row, &keys, &values, frame, &mut along)? else {
                continue;
            };
            if given.is_none() {
                given = Some(reading.given(&row, frame)?);
            }
            g.add(i, &reading, given.clone().expect("just read"))?;
        }
    }
    Ok(())
}

impl Groups {
    fn new(set: Vec<usize>) -> Groups {
        Groups {
            set,
            at: HashMap::new(),
            list: Vec::new(),
            held: crate::room::holding(0),
            past: None,
            queue: Default::default(),
            range: (0, u64::MAX),
            expected: None,
            rows_to_come: None,
            closed: false,
            kept: crate::room::holding(0),
            met: Met::default(),
        }
    }

    fn fresh(p: &Prepared) -> Result<Vec<Acc>> {
        p.calls
            .aggregates
            .iter()
            .map(|&c| Acc::of(c, &p.types).map(|(a, _)| a))
            .collect()
    }

    /// The group a row is folded into, by its set's keys: a new one for a key none has come for, or
    /// none past the room, the row written.
    #[allow(clippy::too_many_arguments)]
    fn place(
        &mut self,
        p: &Prepared,
        reading: &Reading,
        row: &[Value],
        keys: &[Key],
        values: &[Value],
        frame: &Frame,
        along: &mut Option<&mut dyn FnMut(Vec<Vec<Value>>) -> Result<()>>,
    ) -> Result<Option<usize>> {
        let k: Vec<Key> = self.set.iter().map(|&i| keys[i].clone()).collect();
        // A row constructor's record is compared as rows meet (`met`), over this set's values.
        let met: Vec<Value> = self.set.iter().map(|&i| values[i].clone()).collect();
        self.met.meet(&met)?;
        if let Some(end) = along {
            if !self.list.is_empty() && !self.at.contains_key(&k) {
                self.project(p, frame, &mut |row, calls| end(p.group_rows(&row, calls, frame)?))?;
            }
        }
        if let Some(&i) = self.at.get(&k) {
            return Ok(Some(i));
        }
        let w = super::spill::row_width(row) + reading.group_bytes;
        if !self.list.is_empty() && (self.closed || !self.held.fits(w)) {
            self.closed = true;
            if self.expected.is_none() {
                self.expected = self.rows_to_come.map(|n| n * w);
            }
            let (lo, hi, expected) = (self.range.0, self.range.1, self.expected);
            self.past
                .get_or_insert_with(|| super::spill::Parts::new(lo, hi, expected))
                .push(parted::key_hash(&k, 0), row.to_vec())?;
            return Ok(None);
        }
        if self.list.is_empty() {
            self.held.more(w);
        }
        let mut first = row.to_vec();
        if p.sets.is_some() {
            first.extend_from_slice(values);
        }
        self.list.push((first, Self::fresh(p)?));
        self.at.insert(k, self.list.len() - 1);
        Ok(Some(self.list.len() - 1))
    }

    /// What a row gives the aggregates (`Given`) folded into its group.
    fn add(&mut self, i: usize, reading: &Reading, given: Given) -> Result<()> {
        for (((acc, f), (u, refused)), given) in self.list[i]
            .1
            .iter_mut()
            .zip(&reading.folded)
            .zip(&reading.folded_under)
            .zip(given)
        {
            let Some((direct, values)) = given else {
                continue;
            };
            if !f.direct.is_empty() {
                acc.direct(direct);
            }
            if let Some(e) = refused.as_ref().filter(|_| acc.compares(&values)) {
                return Err(Error::new(e.clone()));
            }
            if acc.keeps_rows() {
                self.kept.more(super::spill::row_width(&values));
            }
            acc.add_row(values, u)?;
        }
        Ok(())
    }

    /// Each group HAVING keeps handed to `out`, its row and the values its expressions read beyond
    /// it (`Computed`), then their room let go. Where the step groups by sets, each group's row has
    /// the columns and the values made below the fold its set lacks NULL, and each grouping
    /// expression standing whole reads its group's value, NULL where its set lacks it; each
    /// GROUPING call reads a bit for each argument its set lacks.
    fn project(
        &mut self,
        p: &Prepared,
        frame: &Frame,
        out: &mut dyn FnMut(Vec<Value>, HashMap<*const Expr, Value>) -> Result<()>,
    ) -> Result<()> {
        let lacking: Vec<usize> = match &p.sets {
            Some(_) => (0..p.grouping.len())
                .filter(|i| !self.set.contains(i))
                .flat_map(|i| p.cleared(i, frame))
                .collect(),
            None => Vec::new(),
        };
        let mut lacks: HashMap<*const Expr, Value> = HashMap::new();
        for (&g, args) in p.calls.groupings.iter().zip(&p.grouping_args) {
            let bits = args
                .iter()
                .fold(0i64, |b, a| (b << 1) | i64::from(!self.set.contains(a)));
            lacks.insert(g as *const Expr, Value::Int(bits));
        }
        for (first, accs) in std::mem::take(&mut self.list) {
            let (mut row, values) = match &p.sets {
                Some(_) => {
                    let mut row = first;
                    let values = row.split_off(row.len() - p.grouping.len());
                    (row, values)
                }
                None => (first, Vec::new()),
            };
            for &c in &lacking {
                row[c] = Value::Null;
            }
            let mut calls = lacks.clone();
            for &(at, i) in &p.whole {
                let v = match self.set.contains(&i) {
                    true => values[i].clone(),
                    false => Value::Null,
                };
                calls.insert(at, v);
            }
            let grouped = Computed {
                calls,
                bound: None,
                _q: std::marker::PhantomData,
            };
            let folds = p
                .calls
                .aggregates
                .iter()
                .zip(&accs)
                .map(|(&c, a)| {
                    // A direct argument is read once a group, from no row where none came.
                    let (_, f) = Acc::of(c, &p.types)?;
                    if f.direct.is_empty() {
                        return Ok((c, a.value()?));
                    }
                    let mut a = a.clone();
                    a.direct(
                        f.direct
                            .iter()
                            .map(|x| eval(x, &row, frame, &grouped))
                            .collect::<Result<_>>()?,
                    );
                    Ok((c, a.value()?))
                })
                .collect::<Result<Vec<_>>>()?;
            let computed = grouped.and(folds);
            if let Some(h) = p.having {
                if holds(h, &row, frame, &computed)? != Some(true) {
                    continue;
                }
            }
            out(row, computed.calls)?;
        }
        self.at.clear();
        self.closed = false;
        self.held = crate::room::holding(0);
        self.kept = crate::room::holding(0);
        Ok(())
    }
}

/// A step's rows on their way out: each DISTINCT row once, past the room those it cannot yet say
/// are new written for a pass of their own; then held for its ORDER BY or LIMIT, or handed on.
struct Going {
    distinct: bool,
    seen: std::collections::HashSet<Vec<Key>>,
    seen_held: crate::room::Holding,
    seen_past: Option<super::spill::Parts>,
    queue: super::spill::Queue,
    range: (u64, u64),
    expected: Option<u64>,
    rows_to_come: Option<u64>,
    closed: bool,
    sorter: Option<sorted::Sorter>,
    /// Whether it sorts them; where they come in their order, what its LIMIT keeps of them.
    sorts: bool,
    cut: Option<sorted::Cut>,
    met: Met,
}

impl Going {
    /// Whether its LIMIT keeps no row past those it has, rows that come in their order.
    fn ended(&mut self, p: &Prepared, frame: &Frame) -> Result<bool> {
        if let (None, Some(l)) = (&self.cut, &p.limit) {
            self.cut = Some(sorted::Cut::of(l, frame)?);
        }
        Ok(self.cut.as_ref().is_some_and(sorted::Cut::ended))
    }

    /// Of rows the step returns, those to hand on now.
    fn take(
        &mut self,
        p: &Prepared,
        frame: &Frame,
        rows: Vec<Vec<Value>>,
    ) -> Result<Vec<Vec<Value>>> {
        let casings = frame.casings;
        let rows = if self.distinct {
            let mut new = Vec::with_capacity(rows.len());
            for r in rows {
                self.met.meet(&r)?;
                let k: Vec<Key> = r.iter().map(key).collect();
                if self.seen.contains(&k) {
                    continue;
                }
                if self.closed || !self.seen_held.fits(super::spill::row_width(&r)) {
                    self.closed = true;
                    if self.expected.is_none() {
                        self.expected = self
                            .rows_to_come
                            .map(|n| n * super::spill::row_width(&r));
                    }
                    let (lo, hi, expected) = (self.range.0, self.range.1, self.expected);
                    self.seen_past
                        .get_or_insert_with(|| super::spill::Parts::new(lo, hi, expected))
                        .push(parted::key_hash(&k, 0), r)?;
                    continue;
                }
                self.seen.insert(k);
                new.push(r);
            }
            new
        } else {
            rows
        };
        if !self.sorts {
            let Some(l) = &p.limit else {
                // Rows that come in their order make what waits for the sort as they come.
                return p.after_sort(rows, &[], None, frame);
            };
            let keys = match l.ties {
                true => {
                    let under = sorted::sort_under(p, casings)?;
                    rows.iter()
                        .map(|r| sorted::sort_key(r, &p.sorted, &under))
                        .collect::<Result<Vec<_>>>()?
                }
                false => Vec::new(),
            };
            if self.cut.is_none() {
                self.cut = Some(sorted::Cut::of(l, frame)?);
            }
            return p.after_sort(rows, &keys, self.cut.as_mut(), frame);
        }
        if self.sorter.is_none() {
            let mut s = sorted::Sorter::new(p, casings)?;
            s.rows_to_come = self.rows_to_come;
            self.sorter = Some(s);
        }
        let s = self.sorter.as_mut().expect("just made");
        for r in rows {
            s.push(r)?;
        }
        Ok(Vec::new())
    }
}

/// One SELECT read over rows given a chunk at a time, in the order: its windows' running folds and
/// its groups kept from chunk to chunk. A step that neither groups nor sorts hands each chunk on.
pub(super) struct StepRun<'q> {
    p: Prepared<'q>,
    running: Running,
    /// Its groups, of each grouping set.
    groups: Vec<Groups>,
    going: Going,
    /// The rows its FROM gives, held for a window that reads its partition whole, and the column
    /// holding the table's order among them.
    held: parted::Parted,
    /// Where its rows come in the order of its windows' PARTITION BY and ORDER BY: the partition
    /// coming, its rows held in the round's room, and the rows the partitions before it returned.
    along_partitions: bool,
    partition: Option<Vec<Key>>,
    partition_rows: Vec<Vec<Value>>,
    partition_held: crate::room::Holding,
    partitions_out: Vec<Vec<Value>>,
    /// Whether its rows come in the order of its GROUP BY, each group ended as its key changes.
    along_groups: bool,
    /// Whether whoever reads it takes its rows as they are made, rather than all at the end.
    pub(super) hands_on: bool,
    /// The columns the step's FROM reads, as the last chunk gave them.
    columns: Columns,
    /// What answers its subqueries, where it has any.
    pub(super) subqueries: Option<&'q dyn Subqueries>,
    /// The snail the statement reads, where the step is read over relations.
    pub(super) reads: Option<&'q dyn super::relations::Reads>,
    /// The row a step that groups with no GROUP BY folds no row into reads its columns from: NULL,
    /// where none is given.
    pub(super) empty_row: Option<Vec<Value>>,
}

impl<'q> StepRun<'q> {
    pub(super) fn new(p: Prepared<'q>) -> StepRun<'q> {
        let going = Going {
            distinct: p.distinct,
            seen: Default::default(),
            seen_held: crate::room::holding(0),
            seen_past: None,
            queue: Default::default(),
            range: (0, u64::MAX),
            expected: None,
            rows_to_come: None,
            closed: false,
            sorter: None,
            sorts: p.order_by.is_some() || p.distinct_on.is_some(),
            cut: None,
            met: Met::default(),
        };
        let groups = match &p.sets {
            Some(sets) => sets.iter().map(|s| Groups::new(s.clone())).collect(),
            None => vec![Groups::new((0..p.grouping.len()).collect())],
        };
        StepRun {
            p,
            running: HashMap::new(),
            groups,
            going,
            held: parted::Parted::new(),
            along_partitions: false,
            partition: None,
            partition_rows: Vec::new(),
            partition_held: crate::room::holding(0),
            partitions_out: Vec::new(),
            along_groups: false,
            hands_on: false,
            columns: Vec::new(),
            subqueries: None,
            reads: None,
            empty_row: None,
        }
    }

    /// Told how many rows its FROM gives, at most, before the first comes.
    pub(super) fn expect_rows(&mut self, n: u64) {
        self.held.rows_to_come = Some(n);
        for g in &mut self.groups {
            g.rows_to_come = Some(n);
        }
        self.going.rows_to_come = Some(n);
    }

    /// Reads its rows as coming in the order a strand gives it: its windows' partitions each folded
    /// once its rows have come, its ORDER BY holding none, its groups ending as their keys change.
    pub(super) fn along(&mut self, led: super::strands::Led) {
        use super::strands::Led;
        match led {
            Led::Partitions => self.along_partitions = true,
            Led::Ordered => self.going.sorts = false,
            Led::Groups => self.along_groups = true,
        }
    }

    fn names(&self) -> Vec<String> {
        self.p.items.iter().map(|(_, n)| n.clone()).collect()
    }

    /// Reads one chunk of the rows its FROM gives: the rows the step returns for it, unless the step
    /// groups or sorts, which hold them until `finish`.
    pub(super) fn feed(
        &mut self,
        mut rel: Relation,
        columns: Columns,
        params: &[String],
        release: u8,
        casings: &Casings,
    ) -> Result<Option<Relation>> {
        let frame = Frame {
            columns,
            params,
            release,
            casings,
            types: &self.p.types,
            now: self.p.now,
            subqueries: self.subqueries,
            reads: self.reads,
            below: Some(&self.p.below),
        };
        let none = computed_none();
        // A WHERE that draws is read a row at a time, each row it keeps made before the next is
        // read, as PostgreSQL's scan reads its condition and then makes the row; where nothing
        // sorts between, each row is handed on as it is made, and none is read past the last the
        // LIMIT keeps.
        let by_row = self.p.selection.filter(|w| {
            draws(w) && !self.p.grouped && self.p.calls.windows.is_empty()
        });
        if let Some(w) = self.p.selection.filter(|_| by_row.is_none()) {
            let mut kept = Vec::with_capacity(rel.rows.len());
            for row in std::mem::take(&mut rel.rows) {
                if holds(w, &row, &frame, &none)? == Some(true) {
                    kept.push(row);
                }
            }
            rel.rows = kept;
        }
        // Its sets below the fold and the windows, each row made once for each of their values.
        if !self.p.below_exprs.is_empty() {
            let mut made = Vec::with_capacity(rel.rows.len());
            for row in std::mem::take(&mut rel.rows) {
                self.p.below_rows(row, &frame, &mut made)?;
            }
            rel.rows = made;
        }
        if self.p.grouped {
            let mut ended = Vec::new();
            let mut end = |rows: Vec<Vec<Value>>| -> Result<()> {
                ended.extend(rows);
                Ok(())
            };
            // Windows read every group, so none is ended before all have come.
            let along: Option<&mut dyn FnMut(Vec<Vec<Value>>) -> Result<()>> =
                (self.along_groups && self.p.calls.windows.is_empty()).then_some(&mut end);
            fold_rows(&mut self.groups, &self.p, rel.rows, &frame, along)?;
            let names = self.names();
            let handed = self.going.take(&self.p, &frame, ended)?;
            self.columns = frame.columns;
            if self.hands_on && !handed.is_empty() {
                return Ok(Some(Relation {
                    names,
                    rows: truncated(handed, self.p.items.len()),
                    order: None,
                }));
            }
            self.partitions_out.extend(handed);
            return Ok(None);
        }
        // Rows that come in its windows' order are folded a partition at a time.
        if self.along_partitions {
            let by = windows::shared_partition(&self.p.calls.windows);
            let mut made = Vec::new();
            for row in rel.rows {
                let k = partition_key(by, &row, &frame)?;
                if self.partition.as_ref() != Some(&k) && !self.partition_rows.is_empty() {
                    let rows = std::mem::take(&mut self.partition_rows);
                    fold_partition(&self.p, rows, &frame, &mut made)?;
                    self.partition_held = crate::room::holding(0);
                }
                self.partition = Some(k);
                self.partition_held.more(super::spill::row_width(&row));
                self.partition_rows.push(row);
            }
            let handed = self.going.take(&self.p, &frame, made)?;
            let names = self.names();
            self.columns = frame.columns;
            if self.hands_on {
                return Ok(Some(Relation {
                    names,
                    rows: truncated(handed, self.p.items.len()),
                    order: None,
                }));
            }
            self.partitions_out.extend(handed);
            return Ok(None);
        }
        // A window on another order than the one the rows come in reads them whole.
        if !self.p.whole_windows
            && self.running.is_empty()
            && !windows::along(&self.p.calls.windows, &rel, &frame)
        {
            self.p.whole_windows = true;
        }
        if self.p.whole_windows {
            self.held.order = self.held.order.or(rel.order);
            let by = windows::shared_partition(&self.p.calls.windows);
            for row in rel.rows {
                let h = parted::partition_hash(by, &row, &frame)?;
                self.held.push(h, row)?;
            }
            self.columns = frame.columns;
            return Ok(None);
        }
        let per_row = windows(&self.p.calls.windows, &rel, &frame, &mut self.running)?;
        let handed = by_row.is_some() && !self.going.sorts;
        let mut rows = Vec::with_capacity(rel.rows.len());
        for (row, w) in rel.rows.iter().zip(per_row) {
            if let Some(sel) = by_row {
                if handed && self.going.ended(&self.p, &frame)? {
                    break;
                }
                if holds(sel, row, &frame, &none)? != Some(true) {
                    continue;
                }
            }
            let computed = Computed {
                calls: w,
                bound: None,
                _q: std::marker::PhantomData,
            };
            let made = self.p.project(row, &frame, computed)?;
            match handed {
                true => rows.extend(self.going.take(&self.p, &frame, made)?),
                false => rows.extend(made),
            }
        }
        // The order holds where an item is the column that held it, and no set repeats a row.
        let order = rel
            .order
            .filter(|_| self.p.calls.sets.is_empty() && self.p.below_exprs.is_empty())
            .and_then(|o| {
                self.p.items.iter().position(|(e, _)| {
                    column_of(e).is_some_and(|(q, n)| frame.find(q.as_deref(), &n).ok() == Some(o))
                })
            });
        let names = self.names();
        let rows = match handed {
            true => rows,
            false => self.going.take(&self.p, &frame, rows)?,
        };
        self.columns = frame.columns;
        if self.going.sorts {
            return Ok(None);
        }
        if (self.p.distinct || self.p.limit.is_some() || self.p.order_by.is_some()) && !self.hands_on {
            self.partitions_out.extend(rows);
            return Ok(None);
        }
        Ok(Some(Relation {
            names,
            rows: truncated(rows, self.p.items.len()),
            order: order.filter(|_| !self.p.distinct || self.going.seen_past.is_none()),
        }))
    }

    /// Sorts the rows it holds and hands on those its LIMIT keeps, every row to come sorting after them.
    fn sort_so_far(&mut self, params: &[String], release: u8, casings: &Casings) -> Result<()> {
        let Some(sorter) = self.going.sorter.take() else {
            return Ok(());
        };
        let frame = Frame {
            columns: self.columns.clone(),
            params,
            release,
            casings,
            types: &self.p.types,
            now: self.p.now,
            subqueries: self.subqueries,
            reads: self.reads,
            below: Some(&self.p.below),
        };
        if let (None, Some(l)) = (&self.going.cut, &self.p.limit) {
            self.going.cut = Some(sorted::Cut::of(l, &frame)?);
        }
        let (cut, out, p) = (&mut self.going.cut, &mut self.partitions_out, &self.p);
        sorter.finish(&mut |rows, keys| {
            out.extend(p.after_sort(rows, &keys, cut.as_mut(), &frame)?);
            Ok(())
        })
    }

    /// The rows a step that groups or sorts returns, once every chunk is read, handed to `each` as
    /// they are made: its columns' names, and the column holding the table's order where they keep it.
    pub(super) fn finish_each(
        mut self,
        params: &[String],
        release: u8,
        casings: &Casings,
        each: &mut dyn FnMut(Vec<Vec<Value>>) -> Result<()>,
    ) -> Result<(Vec<String>, Option<usize>)> {
        let names = self.names();
        let width = self.p.items.len();
        let mut order_out = None;
        let frame = Frame {
            columns: std::mem::take(&mut self.columns),
            params,
            release,
            casings,
            types: &self.p.types,
            now: self.p.now,
            subqueries: self.subqueries,
            reads: self.reads,
            below: Some(&self.p.below),
        };
        if !self.partitions_out.is_empty() {
            each(truncated(
                std::mem::take(&mut self.partitions_out),
                width,
            ))?;
        }
        let p = &self.p;
        let going = &mut self.going;
        let frame_ref = &frame;
        let mut emit = |rows: Vec<Vec<Value>>,
                        going: &mut Going|
         -> Result<()> {
            let rows = going.take(p, frame_ref, rows)?;
            if !rows.is_empty() {
                each(truncated(rows, width))?;
            }
            Ok(())
        };
        if p.grouped {
            // Each set's groups, set after set: a set of none gives its row over no rows, each such
            // set its own, its grouping expressions' values NULL after it where the step groups by
            // sets.
            let empty = self
                .empty_row
                .take()
                .unwrap_or_else(|| vec![Value::Null; frame.columns.len()]);
            // Where windows read the groups, every group's row is held for them.
            let mut windowed = Vec::new();
            for groups in &mut self.groups {
                if groups.list.is_empty() && groups.set.is_empty() {
                    let mut row = empty.clone();
                    if p.sets.is_some() {
                        row.extend((0..p.grouping.len()).map(|_| Value::Null));
                    }
                    groups.list.push((row, Groups::fresh(p)?));
                }
                loop {
                    groups.project(p, &frame, &mut |row, calls| {
                        match p.calls.windows.is_empty() {
                            true => emit(p.group_rows(&row, calls, &frame)?, going),
                            false => {
                                windowed.push((row, calls));
                                Ok(())
                            }
                        }
                    })?;
                    if let Some(past) = groups.past.take() {
                        past.into_queue(groups.range.1, &mut groups.queue)?;
                    }
                    let Some((lo, hi, total, runs)) = groups.queue.next(0) else {
                        break;
                    };
                    groups.range = (lo, hi);
                    groups.expected = Some(total);
                    for run in runs {
                        run.each_chunk(&mut |c| {
                            fold_rows(std::slice::from_mut(groups), p, c, &frame, None)
                        })?;
                    }
                }
            }
            if !windowed.is_empty() {
                emit(p.windowed_groups(windowed, &frame)?, going)?;
            }
        }
        if self.along_partitions {
            let rows = std::mem::take(&mut self.partition_rows);
            let mut made = Vec::new();
            fold_partition(p, rows, &frame, &mut made)?;
            self.partition_held = crate::room::holding(0);
            emit(made, going)?;
        } else if p.whole_windows {
            let held = std::mem::replace(&mut self.held, parted::Parted::new());
            let by = windows::shared_partition(&p.calls.windows);
            let mut order = None;
            held.finish(&|r| parted::partition_hash(by, r, &frame), &mut |rel| {
                let per_row = windows::values(&p.calls.windows, &rel, &frame, None)?;
                let mut rows = Vec::with_capacity(rel.rows.len());
                for (row, w) in rel.rows.iter().zip(per_row) {
                    let computed = Computed {
                        calls: w,
                        bound: None,
                        _q: std::marker::PhantomData,
                    };
                    rows.extend(p.project(row, &frame, computed)?);
                }
                order = rel.order;
                emit(rows, going)
            })?;
            order_out = order
                .filter(|_| {
                    p.calls.sets.is_empty()
                        && p.below_exprs.is_empty()
                        && p.order_by.is_none() && p.distinct_on.is_none()
                })
                .and_then(|o| {
                    p.items.iter().position(|(e, _)| {
                        column_of(e)
                            .is_some_and(|(q, n)| frame.find(q.as_deref(), &n).ok() == Some(o))
                    })
                });
        }
        // Each pass of the DISTINCT rows past the room.
        loop {
            if let Some(past) = going.seen_past.take() {
                past.into_queue(going.range.1, &mut going.queue)?;
            }
            going.seen.clear();
            going.closed = false;
            going.seen_held = crate::room::holding(0);
            let free = crate::room::current().map_or(u64::MAX, |r| r.free_bytes());
            let Some((lo, hi, total, runs)) = going.queue.next(free) else {
                break;
            };
            going.range = (lo, hi);
            going.expected = Some(total);
            for run in runs {
                run.each_chunk(&mut |c| emit(c, going))?;
            }
        }
        if going.sorts {
            let sorter = match going.sorter.take() {
                Some(s) => s,
                None => sorted::Sorter::new(p, casings)?,
            };
            let mut cut = match &p.limit {
                Some(l) => Some(sorted::Cut::of(l, &frame)?),
                None => None,
            };
            // DISTINCT ON keeps its rows before the cut reads them.
            let mut first = p.distinct_on.map(sorted::First::of);
            sorter.finish(&mut |rows, keys| {
                let (rows, keys) = match &mut first {
                    Some(f) => f.take(rows, keys),
                    None => (rows, keys),
                };
                let rows = p.after_sort(rows, &keys, cut.as_mut(), frame_ref)?;
                if !rows.is_empty() {
                    each(truncated(rows, width))?;
                }
                Ok(())
            })?;
        }
        Ok((names, order_out))
    }

    /// The rows a step that groups or sorts returns, once every chunk is read.
    pub(super) fn finish(
        self,
        params: &[String],
        release: u8,
        casings: &Casings,
    ) -> Result<Relation> {
        let mut rows = Vec::new();
        let (names, order) = self.finish_each(params, release, casings, &mut |c| {
            rows.extend(c);
            Ok(())
        })?;
        Ok(Relation { names, rows, order })
    }
}

/// Rows with the ORDER BY's own expressions gone, now they are sorted and cut.
fn truncated(mut rows: Vec<Vec<Value>>, width: usize) -> Vec<Vec<Value>> {
    for r in &mut rows {
        r.truncate(width);
    }
    rows
}

/// One SELECT read whole, over the steps before it and the table.
fn select(
    p: Prepared<'_>,
    steps: &HashMap<String, Relation>,
    table: &Table,
    params: &[String],
    release: u8,
    casings: &Casings,
) -> Result<Relation> {
    let (rel, columns) = if p.from.is_empty() {
        (
            Relation {
                names: Vec::new(),
                rows: vec![Vec::new()],
                order: None,
            },
            Vec::new(),
        )
    } else {
        from(p.from, steps, table)?
    };
    let mut run = StepRun::new(p);
    match run.feed(rel, columns, params, release, casings)? {
        Some(out) => Ok(out),
        None => run.finish(params, release, casings),
    }
}

/// A step's rows kept in the round's room for a step read after it.
struct Place {
    names: Vec<String>,
    rows: super::spill::Rows,
}

/// One relation a step read once every row has come crosses: a step read whole, or one in the room.
enum Crossed {
    Whole(Relation),
    Kept(super::spill::Rows),
}

/// Each row of `left` with each of the relations from `at`, handed to `each` a chunk at a time.
fn crossed(
    parts: &[Crossed],
    at: usize,
    left: &[Vec<Value>],
    each: &mut dyn FnMut(Vec<Vec<Value>>) -> Result<()>,
) -> Result<()> {
    let Some(part) = parts.get(at) else {
        return each(left.to_vec());
    };
    let mut with = |right: &[Vec<Value>]| -> Result<()> {
        let mut rows = Vec::with_capacity(left.len() * right.len());
        for l in left {
            for r in right {
                let mut row = l.clone();
                row.extend(r.iter().cloned());
                rows.push(row);
            }
        }
        crossed(parts, at + 1, &rows, each)
    };
    match part {
        Crossed::Whole(r) => with(&r.rows),
        Crossed::Kept(rows) => rows.each_chunk(&mut |c| with(&c)),
    }
}

/// One SELECT read once every row has come, over the relations before it, a chunk at a time, the rows
/// it returns handed to `put`: its columns' names, and the column holding the table's order where they
/// keep it.
#[allow(clippy::too_many_arguments)]
fn select_each(
    p: Prepared<'_>,
    whole: &HashMap<String, Relation>,
    places: &HashMap<String, Place>,
    table: &Table,
    params: &[String],
    release: u8,
    casings: &Casings,
    put: &mut dyn FnMut(Vec<Vec<Value>>) -> Result<()>,
) -> Result<(Vec<String>, Option<usize>)> {
    let mut parts: Vec<Crossed> = Vec::new();
    let mut columns: Columns = Vec::new();
    let mut names: Vec<String> = Vec::new();
    {
        let mut add = |t: &TableFactor| -> Result<()> {
            let TableFactor::Table { name, alias, .. } = t else {
                return Err(Error::new("a step reads tables and steps by name"));
            };
            let n = crate::catalog::object_name(name);
            let q = alias
                .as_ref()
                .map(|a| ident(&a.name))
                .unwrap_or_else(|| n.rsplit('.').next().unwrap_or(&n).to_string());
            let (part, ns) = match places.get(&n) {
                Some(place) => (Crossed::Kept(place.rows.clone()), place.names.clone()),
                None => {
                    let r = relation(&n, whole, table)?.clone();
                    let ns = r.names.clone();
                    (Crossed::Whole(r), ns)
                }
            };
            columns.extend(ns.iter().map(|c| (Some(q.clone()), c.clone())));
            names.extend(ns);
            parts.push(part);
            Ok(())
        };
        for item in p.from {
            add(&item.relation)?;
            for j in &item.joins {
                match j.join_operator {
                    JoinOperator::CrossJoin(_) => add(&j.relation)?,
                    _ => return Err(Error::new("a step joins only with CROSS JOIN")),
                }
            }
        }
    }
    let mut rows_known = 1u64;
    for part in &parts {
        rows_known = rows_known.saturating_mul(match part {
            Crossed::Whole(r) => r.rows.len() as u64,
            Crossed::Kept(rows) => rows.len() as u64,
        });
    }
    let mut run = StepRun::new(p);
    run.hands_on = true;
    run.expect_rows(rows_known);
    let mut order = None;
    crossed(&parts, 0, &[Vec::new()], &mut |rows| {
        let rel = Relation {
            names: names.clone(),
            rows,
            order: None,
        };
        if let Some(out) = run.feed(rel, columns.clone(), params, release, casings)? {
            order = out.order;
            put(out.rows)?;
        }
        Ok(())
    })?;
    let (out_names, tail) = run.finish_each(params, release, casings, put)?;
    Ok((out_names, order.or(tail)))
}

/// The names a SELECT's FROM reads.
fn reads(q: &Query) -> Result<Vec<String>> {
    let s = match q.body.as_ref() {
        SetExpr::Select(s) => s,
        _ => return Err(Error::new("a step is a SELECT")),
    };
    let mut out = Vec::new();
    for item in &s.from {
        for t in std::iter::once(&item.relation).chain(item.joins.iter().map(|j| &j.relation)) {
            match t {
                TableFactor::Table { name, .. } => out.push(crate::catalog::object_name(name)),
                _ => return Err(Error::new("a step reads tables and steps by name")),
            }
        }
    }
    Ok(out)
}

/// How a step is read against the table's rows coming in the order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// It reads neither the table nor a step that does: read once, before the rows come.
    Fixed,
    /// It reads the rows as they come and hands them on, a chunk at a time.
    Streamed,
    /// It reads the rows as they come, and groups or sorts them: read whole once they have come.
    Folded,
    /// It reads a step read whole: read once every row has come.
    After,
}

/// A question read as the table's rows come, a chunk at a time in the table's order: each step
/// that reads them folds as they come, and holds only its windows' running folds, its groups, or
/// the rows it sorts. A step read after the rows have come gets the steps it reads, and the table,
/// gathered whole.
pub struct Stream<'q> {
    table: Table,
    params: Vec<String>,
    /// The release whose catalog names a reg* value.
    release: u8,
    casings: Casings,
    /// Each step, and last the question itself under no name.
    steps: Vec<(String, &'q Query, Kind)>,
    runs: Vec<Option<StepRun<'q>>>,
    /// Each step read once every row has come, prepared.
    later: Vec<Option<Prepared<'q>>>,
    /// The steps read whole, then each streamed step's chunk as it is read.
    view: HashMap<String, Relation>,
    /// The streamed relations a step read after the rows have come reads, gathered.
    kept: HashMap<String, Relation>,
    fed: bool,
    /// The most rows held at once.
    most: usize,
}

impl<'q> Stream<'q> {
    /// `names` are the table's columns, `types` their types, `order` the one holding its order's
    /// key, where the windows can read along it, and `primary` its primary key's columns.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        q: &'q Query,
        table: &str,
        names: Vec<String>,
        types: Vec<Option<ColumnType>>,
        order: Option<usize>,
        primary: &[String],
        params: &[String],
        release: u8,
        casings: Casings,
    ) -> Result<Stream<'q>> {
        let mut s = Stream {
            table: Table {
                name: table.to_string(),
                relation: Relation {
                    names,
                    rows: Vec::new(),
                    order,
                },
            },
            params: params.to_vec(),
            release,
            casings,
            steps: Vec::new(),
            runs: Vec::new(),
            later: Vec::new(),
            view: HashMap::new(),
            kept: HashMap::new(),
            fed: false,
            most: 0,
        };
        let mut queries: Vec<(String, &'q Query)> = Vec::new();
        if let Some(with) = &q.with {
            if with.recursive {
                return Err(Error::new("WITH RECURSIVE is not read"));
            }
            for cte in &with.cte_tables {
                queries.push((ident(&cte.alias.name), &cte.query));
            }
        }
        queries.push((String::new(), q));
        // One clock the whole statement reads: its transaction's.
        let now = crate::scalars::transaction_clock();
        let mut typed: HashMap<String, (Vec<String>, Vec<Option<ColumnType>>)> = HashMap::new();
        typed.insert(
            s.table.name.clone(),
            (s.table.relation.names.clone(), types),
        );
        // The column holding the table's order in each relation that streams in it.
        let mut orders: HashMap<String, Option<usize>> = HashMap::new();
        orders.insert(s.table.name.clone(), s.table.relation.order);
        for (name, query) in queries {
            let mut streamed = Vec::new();
            let mut after = false;
            for n in reads(query)? {
                match s.steps.iter().rev().find(|(m, _, _)| *m == n) {
                    Some((_, _, Kind::Streamed)) => streamed.push(n),
                    Some((_, _, Kind::Folded | Kind::After)) => after = true,
                    Some((_, _, Kind::Fixed)) => {}
                    None if s.is_table(&n) => streamed.push(s.table.name.clone()),
                    None => {
                        return Err(Error::new(format!(
                            "{n} is neither a step of the WITH nor the table"
                        )))
                    }
                }
            }
            let columns = match query.body.as_ref() {
                SetExpr::Select(sel) => typed_from(&sel.from, &typed, &s.table.name),
                _ => Vec::new(),
            };
            // The table's own columns of the domain its DDL gives each, for a row made of them.
            let domains = match query.body.as_ref() {
                SetExpr::Select(sel) => sel
                    .from
                    .iter()
                    .flat_map(|t| std::iter::once(&t.relation).chain(t.joins.iter().map(|j| &j.relation)))
                    .filter_map(|t| match t {
                        TableFactor::Table { name, .. } => Some(crate::catalog::object_name(name)),
                        _ => None,
                    })
                    .filter_map(|n| {
                        let (names, _) = typed.get(&n).or_else(|| {
                            s.is_table(&n).then(|| typed.get(&s.table.name)).flatten()
                        })?;
                        let of_table = n == s.table.name || s.is_table(&n) && !typed.contains_key(&n);
                        let table = s.table.name.clone();
                        let made = super::handed::step_domains(&n).filter(|_| !of_table);
                        Some(names.iter().enumerate().map(move |(i, c)| match of_table {
                            true => super::handed::column_domain(&table, c),
                            false => made.as_ref().and_then(|m| m.get(i).cloned().flatten()),
                        }))
                    })
                    .flatten()
                    .collect(),
                _ => Vec::new(),
            };
            let _domains = super::typed::domains_held(domains);
            // The table read under each name its FROM gives it, with its primary key.
            let keyed: Vec<(String, Vec<String>)> = match query.body.as_ref() {
                SetExpr::Select(sel) => base_qualifiers(&sel.from, &typed, &s.table.name)
                    .into_iter()
                    .map(|q| (q, primary.to_vec()))
                    .collect(),
                _ => Vec::new(),
            };
            let mut p = prepare(
                query,
                &columns,
                &keyed,
                release,
                now,
                &[],
                Default::default(),
                &[],
            )?;
            if let SetExpr::Select(sel) = query.body.as_ref() {
                step_names(&p, &columns, &sel.from, &typed, &s.table.name)?;
            }
            // Its windows fold as the rows come only along the order of the one relation it reads.
            let input = match (streamed.as_slice(), query.body.as_ref()) {
                ([only], SetExpr::Select(sel))
                    if sel.from.len() == 1 && sel.from[0].joins.is_empty() =>
                {
                    orders.get(only).copied().flatten()
                }
                _ => None,
            };
            if !windows::along_columns(&p.calls.windows, &columns, input) {
                p.whole_windows = true;
            }
            let order = input.filter(|_| p.calls.sets.is_empty()).and_then(|o| {
                p.items.iter().position(|(e, _)| {
                    column_of(e).is_some_and(|(q, n)| {
                        naming(
                            columns.iter().map(|(q, n, _)| (q.as_deref(), n.as_str())),
                            q.as_deref(),
                            &n,
                        ) == [o]
                    })
                })
            });
            typed.insert(
                name.clone(),
                (
                    p.items.iter().map(|(_, n)| n.clone()).collect(),
                    p.returns.clone(),
                ),
            );
            super::handed::step_made(
                &name,
                p.items
                    .iter()
                    .map(|(e, _)| super::typed::domain_in(e, &columns))
                    .collect(),
            );
            let carried = match query.body.as_ref() {
                SetExpr::Select(sel) => {
                    carried_from(&sel.from, &typed, &s.casings.relations, &s.table.name)
                }
                _ => Vec::new(),
            };
            p.carried = s.casings.add_step(&name, &p, &carried)?;
            let kind = if after {
                for n in streamed {
                    s.kept.entry(n).or_insert_with(|| Relation {
                        names: Vec::new(),
                        rows: Vec::new(),
                        order: None,
                    });
                }
                Kind::After
            } else if !streamed.is_empty() {
                if p.grouped
                    || p.order_by.is_some()
                    || p.whole_windows
                    || p.limit.is_some()
                    || p.distinct
                    || p.distinct_on.is_some()
                {
                    Kind::Folded
                } else {
                    Kind::Streamed
                }
            } else {
                Kind::Fixed
            };
            let mut p = Some(p);
            if kind == Kind::Fixed {
                let r = select(
                    p.take().unwrap(),
                    &s.view,
                    &s.table,
                    params,
                    release,
                    &s.casings,
                )?;
                s.view.insert(name.clone(), r);
            }
            s.runs.push(match kind {
                Kind::Streamed | Kind::Folded => p.take().map(StepRun::new),
                _ => None,
            });
            s.later.push(p.filter(|_| kind == Kind::After));
            orders.insert(name.clone(), order.filter(|_| kind == Kind::Streamed));
            s.steps.push((name, query, kind));
        }
        // The question's own rows are gathered where they stream.
        if let Some((_, _, Kind::Streamed)) = s.steps.last() {
            s.kept.insert(
                String::new(),
                Relation {
                    names: Vec::new(),
                    rows: Vec::new(),
                    order: None,
                },
            );
        }
        Ok(s)
    }

    /// Told how many rows the table may give, before the first comes.
    pub(super) fn expect_rows(&mut self, n: u64) {
        for r in self.runs.iter_mut().flatten() {
            r.expect_rows(n);
        }
    }

    /// Its question's own step reads the rows as coming in the order a strand gives it.
    pub(super) fn along(&mut self, led: super::strands::Led) {
        if let Some(Some(r)) = self.runs.last_mut() {
            r.along(led);
        }
    }

    /// Its question's own step, where it reads the table's rows as they come.
    fn own_step(&self) -> Option<&StepRun<'q>> {
        self.runs.last()?.as_ref()
    }

    /// Whether its question is one SELECT that sorts the table's rows and returns one row for each it
    /// keeps.
    pub(super) fn one_row_each(&self) -> bool {
        self.steps.len() == 1
            && self.own_step().is_some_and(|r| {
                let p = &r.p;
                p.order_by.is_some()
                    && !p.grouped
                    && !p.distinct
                    && p.distinct_on.is_none()
                    && !p.whole_windows
                    && p.calls.sets.is_empty()
                    && p.calls.windows.is_empty()
                    && p.calls.aggregates.is_empty()
            })
    }

    /// What its question's own step reads an expression by, on the table's columns.
    fn own_frame(&self) -> Result<Frame<'_>> {
        let run = self
            .own_step()
            .ok_or_else(|| Error::new("the question reads no table"))?;
        let table = Table {
            name: self.table.name.clone(),
            relation: Relation {
                names: self.table.relation.names.clone(),
                rows: Vec::new(),
                order: None,
            },
        };
        let (_, columns) = from(run.p.from, &self.view, &table)?;
        Ok(Frame {
            columns,
            params: &self.params,
            release: self.release,
            casings: &self.casings,
            types: &run.p.types,
            now: run.p.now,
            subqueries: run.subqueries,
            reads: run.reads,
            below: None,
        })
    }

    /// What an expression that reads no column comes to.
    pub(super) fn constant(&self, e: &Expr) -> Result<Value> {
        eval(e, &[], &self.own_frame()?, &computed_none())
    }

    /// The rows its LIMIT and OFFSET can reach, where they say; and whether it keeps the ties of its last.
    pub(super) fn rows_reached(&self) -> Result<(Option<u64>, bool)> {
        let frame = self.own_frame()?;
        match self.own_step().and_then(|r| r.p.limit.as_ref()) {
            Some(l) => Ok((sorted::Cut::of(l, &frame)?.reached(), l.ties)),
            None => Ok((None, false)),
        }
    }

    /// Of rows of the table, those its question's WHERE keeps, each with what `e` comes to on it.
    pub(super) fn kept_with(
        &self,
        rows: Vec<Vec<Value>>,
        e: &Expr,
    ) -> Result<Vec<(Value, Vec<Value>)>> {
        let frame = self.own_frame()?;
        let none = computed_none();
        let selection = self.own_step().and_then(|r| r.p.selection);
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            if let Some(w) = selection {
                if holds(w, &row, &frame, &none)? != Some(true) {
                    continue;
                }
            }
            out.push((eval(e, &row, &frame, &none)?, row));
        }
        Ok(out)
    }

    /// Sorts what it holds and hands that on: every row to come sorts after it.
    pub(super) fn sort_so_far(&mut self) -> Result<()> {
        let (params, release, casings) = (&self.params, self.release, &self.casings);
        match self.runs.last_mut() {
            Some(Some(run)) => run.sort_so_far(params, release, casings),
            _ => Ok(()),
        }
    }

    /// Whether a step read once every row has come reads the rows of a step or of the table, held
    /// until it is: for the pipeline over several relations, whose steps lie in the round's room.
    pub fn holds_steps(&self) -> bool {
        self.kept.keys().any(|n| !n.is_empty())
    }

    /// The most rows the stream held at once as the rows came.
    pub fn most_held(&self) -> usize {
        self.most
    }

    fn is_table(&self, n: &str) -> bool {
        self.table.name == n || self.table.name.rsplit('.').next() == Some(n)
    }

    /// The table's next rows, in its order.
    pub fn feed(&mut self, chunk: Relation) -> Result<()> {
        self.fed = true;
        let table = Table {
            name: self.table.name.clone(),
            relation: chunk,
        };
        if let Some(k) = self.kept.get_mut(&table.name) {
            gather(k, &table.relation);
        }
        for i in 0..self.steps.len() {
            let (name, _, kind) = &self.steps[i];
            if !matches!(kind, Kind::Streamed | Kind::Folded) {
                continue;
            }
            let run = self.runs[i].as_mut().unwrap();
            let (rel, columns) = from(run.p.from, &self.view, &table)?;
            if let Some(out) = run.feed(rel, columns, &self.params, self.release, &self.casings)? {
                if let Some(k) = self.kept.get_mut(name) {
                    gather(k, &out);
                }
                self.view.insert(name.clone(), out);
            }
        }
        let held = table.relation.rows.len()
            + self.view.values().map(|r| r.rows.len()).sum::<usize>()
            + self.kept.values().map(|r| r.rows.len()).sum::<usize>()
            + self
                .runs
                .iter()
                .flatten()
                .map(|r| {
                    r.groups.iter().map(|g| g.list.len()).sum::<usize>()
                        + r.going.sorter.as_ref().map_or(0, |s| s.held())
                        + r.partitions_out.len()
                        + r.running.values().map(HashMap::len).sum::<usize>()
                })
                .sum::<usize>();
        self.most = self.most.max(held);
        Ok(())
    }

    /// The question's answer, once every row has come.
    pub fn finish(mut self) -> Result<Relation> {
        if !self.fed {
            let empty = Relation {
                names: self.table.relation.names.clone(),
                rows: Vec::new(),
                order: self.table.relation.order,
            };
            self.feed(empty)?;
        }
        let mut whole: HashMap<String, Relation> = self
            .view
            .iter()
            .filter(|(n, _)| {
                self.steps
                    .iter()
                    .any(|(m, _, k)| m == *n && *k == Kind::Fixed)
            })
            .map(|(n, r)| (n.clone(), r.clone()))
            .collect();
        let table = Table {
            name: self.table.name.clone(),
            relation: self.kept.remove(&self.table.name).unwrap_or(Relation {
                names: self.table.relation.names.clone(),
                rows: Vec::new(),
                order: self.table.relation.order,
            }),
        };
        let steps = std::mem::take(&mut self.steps);
        let last = steps.len().saturating_sub(1);
        // A step a later step reads lies in the round's room, the rows past it written; the answer
        // is the caller's.
        let mut places: HashMap<String, Place> = HashMap::new();
        for (i, (name, _, kind)) in steps.into_iter().enumerate() {
            if kind == Kind::Fixed {
                continue;
            }
            if kind == Kind::Streamed {
                if let Some(k) = self.kept.remove(&name) {
                    whole.insert(name, k);
                }
                continue;
            }
            let run_out = (i != last).then(super::spill::Run::new);
            let mut gathered: Vec<Vec<Value>> = Vec::new();
            let mut put = |rows: Vec<Vec<Value>>| -> Result<()> {
                match &run_out {
                    Some(run) => rows.into_iter().try_for_each(|r| run.push(r)),
                    None => {
                        gathered.extend(rows);
                        Ok(())
                    }
                }
            };
            let (names, order) = match kind {
                Kind::Folded => self.runs[i].take().unwrap().finish_each(
                    &self.params,
                    self.release,
                    &self.casings,
                    &mut put,
                )?,
                _ => select_each(
                    self.later[i].take().unwrap(),
                    &whole,
                    &places,
                    &table,
                    &self.params,
                    self.release,
                    &self.casings,
                    &mut put,
                )?,
            };
            match run_out {
                Some(run) => {
                    run.seal()?;
                    places.insert(
                        name,
                        Place {
                            names,
                            rows: super::spill::Rows::one(run),
                        },
                    );
                }
                None => {
                    whole.insert(
                        name,
                        Relation {
                            names,
                            rows: gathered,
                            order,
                        },
                    );
                }
            }
        }
        whole
            .remove("")
            .ok_or_else(|| Error::new("the question returned nothing"))
    }
}

/// A chunk's rows added to the rows gathered of the same relation.
fn gather(into: &mut Relation, chunk: &Relation) {
    if into.names.is_empty() {
        into.names = chunk.names.clone();
        into.order = chunk.order;
    }
    into.rows.extend(chunk.rows.iter().cloned());
}

/// The question's answer: each step of its WITH read in turn, then the question itself, over the
/// table's rows given whole.
#[cfg(test)]
pub fn run(q: &Query, table: &Table, params: &[String]) -> Result<Relation> {
    let mut s = Stream::new(
        q,
        &table.name,
        table.relation.names.clone(),
        vec![None; table.relation.names.len()],
        table.relation.order,
        &[],
        params,
        crate::regs::LATEST,
        Casings::default(),
    )?;
    s.feed(table.relation.clone())?;
    s.finish()
}

/// Whether a SELECT returns each row once, by DISTINCT or DISTINCT ON: SELECT ALL returns every row,
/// as one that says neither.
pub(super) fn says_distinct(s: &sqlparser::ast::Select) -> bool {
    matches!(
        s.distinct,
        Some(sqlparser::ast::Distinct::Distinct | sqlparser::ast::Distinct::On(_))
    )
}

/// Whether a question the question path does not read is handed to the pipeline: it reads no
/// table; it returns each row once, or some of its rows (LIMIT, OFFSET, FETCH), or keeps the groups
/// HAVING holds of; it returns a star, or reads a relation other than a table, a join by USING or
/// NATURAL, or a RIGHT or FULL JOIN after the first join; it groups by a name its select list gives
/// or orders by what it does not return; or it calls a function, an aggregate of a column's values
/// counted, or over each value once, filtered or ordered, or an operator on numbers or on a document,
/// the question path does not read, or reads a CASE, a subscript, a COLLATE, a cast of what is no
/// constant or a parameter, or returns a cast or a truth; or calls an aggregate or a window function in WHERE, which the
/// pipeline refuses as PostgreSQL's parser does.
pub fn hands_over(q: &Query) -> bool {
    let SetExpr::Select(s) = q.body.as_ref() else {
        return false;
    };
    if q.with.is_some() {
        return false;
    }
    if s.from.is_empty()
        || says_distinct(s)
        || q.limit_clause.is_some()
        || q.fetch.is_some()
        || s.having.is_some()
        || s.projection.iter().any(|i| {
            matches!(
                i,
                SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(..)
            )
        })
        || matches!(q.order_by.as_ref().map(|o| &o.kind),
            Some(OrderByKind::Expressions(os))
                if os.iter().any(|o| matches!(o.options.sort, Some(OrderBySort::Using(_)))))
    {
        return true;
    }
    let mut on: Vec<&Expr> = Vec::new();
    for (n, item) in s.from.iter().enumerate() {
        if !matches!(item.relation, TableFactor::Table { args: None, .. }) {
            return true;
        }
        for (k, j) in item.joins.iter().enumerate() {
            if !matches!(j.relation, TableFactor::Table { args: None, .. }) {
                return true;
            }
            let (constraint, keeps_left) = match &j.join_operator {
                JoinOperator::Join(c)
                | JoinOperator::Inner(c)
                | JoinOperator::Left(c)
                | JoinOperator::LeftOuter(c)
                | JoinOperator::CrossJoin(c) => (c, false),
                JoinOperator::Right(c)
                | JoinOperator::RightOuter(c)
                | JoinOperator::FullOuter(c) => (c, true),
                _ => return false,
            };
            if matches!(
                constraint,
                sqlparser::ast::JoinConstraint::Using(_) | sqlparser::ast::JoinConstraint::Natural
            ) || keeps_left && (n > 0 || k > 0)
            {
                return true;
            }
            if let sqlparser::ast::JoinConstraint::On(e) = constraint {
                on.push(e);
            }
        }
    }
    let mut exprs: Vec<&Expr> = s
        .projection
        .iter()
        .filter_map(|i| match i {
            SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => Some(e),
            _ => None,
        })
        .collect();
    let returned = exprs.clone();
    let one_table = matches!(s.from.as_slice(), [only] if only.joins.is_empty());
    if returned.iter().any(|e| {
        compares_no_constant(e) || is_written_constant(e) || any_or_all(e) || holds_cast(e)
            || holds_truth(e)
    })
        || one_table && s.selection.as_ref().is_some_and(compares_no_constant)
        || s.selection
            .iter()
            .chain(on.iter().copied())
            .any(stands_constant)
        || s.selection.as_ref().is_some_and(|w| {
            let (aggregate, window, _) = calls_in(w);
            aggregate || window
        })
    {
        return true;
    }
    exprs.extend(s.selection.iter());
    exprs.extend(on);
    if let GroupByExpr::Expressions(g, _) = &s.group_by {
        // A GROUP BY item naming a column the select list names.
        let aliased = |e: &Expr| {
            matches!(e, Expr::Identifier(i) if s.projection.iter().any(|p| matches!(p,
                SelectItem::ExprWithAlias { alias, .. } if ident(alias) == ident(i))))
        };
        if g.iter().any(aliased) {
            return true;
        }
        exprs.extend(g.iter());
    }
    if let Some(OrderByKind::Expressions(o)) = q.order_by.as_ref().map(|o| &o.kind) {
        // An ORDER BY item neither a place, nor a name the select list gives, nor an item of it.
        let named: Vec<String> = s
            .projection
            .iter()
            .filter_map(|i| match i {
                SelectItem::ExprWithAlias { alias, .. } => Some(ident(alias)),
                SelectItem::UnnamedExpr(e) => Some(default_name(e)),
                _ => None,
            })
            .collect();
        let unreturned = o.iter().any(|x| {
            !matches!(&x.expr, Expr::Value(_))
                && !returned.contains(&&x.expr)
                && !matches!(&x.expr, Expr::Identifier(i) if named.contains(&ident(i)))
        });
        if unreturned {
            return true;
        }
        exprs.extend(o.iter().map(|x| &x.expr));
    }
    exprs.into_iter().any(calls_beyond_the_question)
}

/// Whether a condition holds a constant written out where a truth stands, which the question path
/// holds no level for: alone, compared with another or tested for NULL, or any of those under NOT,
/// AND, OR or a truth test.
fn stands_constant(e: &Expr) -> bool {
    match e {
        Expr::Nested(x)
        | Expr::UnaryOp {
            op: UnaryOperator::Not,
            expr: x,
        }
        | Expr::IsTrue(x)
        | Expr::IsNotTrue(x)
        | Expr::IsFalse(x)
        | Expr::IsNotFalse(x)
        | Expr::IsUnknown(x)
        | Expr::IsNotUnknown(x) => stands_constant(x),
        Expr::BinaryOp {
            left,
            op: BinaryOperator::And | BinaryOperator::Or,
            right,
        } => stands_constant(left) || stands_constant(right),
        Expr::BinaryOp { left, right, .. } => is_written_constant(left) && is_written_constant(right),
        Expr::IsNull(x) | Expr::IsNotNull(x) => is_written_constant(x),
        e => is_written_constant(e),
    }
}

/// Whether a select list's item holds a cast, which the question path holds a level for only of a
/// column it reads as one: where it refuses one, of an aggregate's value or of a column it reads as
/// no level, the pipeline casts it.
fn holds_cast(e: &Expr) -> bool {
    let mut found = false;
    let _ = sqlparser::ast::visit_expressions(e, |x| {
        found |= matches!(x, Expr::Cast { .. });
        std::ops::ControlFlow::<()>::Continue(())
    });
    found
}

/// Whether a select list's item holds a truth made of its values, a test for NULL or a comparison
/// among them, which the question path reads only as a condition: the pipeline returns it.
fn holds_truth(e: &Expr) -> bool {
    sqlparser::ast::visit_expressions(e, |x| match x {
        Expr::IsNull(_)
        | Expr::IsNotNull(_)
        | Expr::IsTrue(_)
        | Expr::IsNotTrue(_)
        | Expr::IsFalse(_)
        | Expr::IsNotFalse(_)
        | Expr::IsUnknown(_)
        | Expr::IsNotUnknown(_)
        | Expr::IsDistinctFrom(..)
        | Expr::IsNotDistinctFrom(..)
        | Expr::UnaryOp {
            op: UnaryOperator::Not,
            ..
        } => std::ops::ControlFlow::Break(()),
        Expr::BinaryOp { op, .. }
            if is_comparison(op) || matches!(op, BinaryOperator::And | BinaryOperator::Or) =>
        {
            std::ops::ControlFlow::Break(())
        }
        _ => std::ops::ControlFlow::Continue(()),
    })
    .is_break()
}

/// Whether a select list's item is a constant written out, which the question path holds no level
/// for: a literal, NULL, a typed literal, or a cast or a sign of one.
fn is_written_constant(e: &Expr) -> bool {
    match e {
        Expr::Value(v) => !matches!(v.value, SqlValue::Placeholder(_)),
        Expr::TypedString { .. } => true,
        Expr::Cast { expr, .. } | Expr::UnaryOp { expr, .. } | Expr::Nested(expr) => {
            is_written_constant(expr)
        }
        _ => false,
    }
}

/// Whether an expression calls a function, or an operator on numbers or on a document, the question
/// path does not read, or reads a CASE, an OR, an IN list, a subscript, a COLLATE or a parameter:
/// its aggregates are its own, and a function a DDL names it reads where it reads the question.
/// Whether an expression holds ANY or ALL, which the question path reads only as a WHERE's points.
fn any_or_all(e: &Expr) -> bool {
    sqlparser::ast::visit_expressions(e, |x| match x {
        Expr::AnyOp { .. } | Expr::AllOp { .. } => std::ops::ControlFlow::Break(()),
        _ => std::ops::ControlFlow::Continue(()),
    })
    .is_break()
}

fn calls_beyond_the_question(e: &Expr) -> bool {
    match e {
        // A set-returning function, named in its schema or not, is the pipeline's.
        Expr::Function(f) if returns_set(&function_name(f)) => true,
        Expr::Function(f) => {
            let own = matches!(function_name(f).as_str(), "sum" | "min" | "max" | "avg")
                || function_name(f) == "count" && is_count_star(f);
            // An aggregate of its own but over each value once, filtered or ordered, is not.
            let plain = f.filter.is_none()
                && f.within_group.is_empty()
                && match &f.args {
                    FunctionArguments::List(l) => {
                        l.duplicate_treatment.is_none() && l.clauses.is_empty()
                    }
                    _ => true,
                };
            !(own && plain) || args(f).is_ok_and(|a| a.into_iter().any(calls_beyond_the_question))
        }
        Expr::Ceil { .. }
        | Expr::Floor { .. }
        | Expr::Extract { .. }
        | Expr::SimilarTo { .. }
        | Expr::Like { .. }
        | Expr::ILike { .. }
        | Expr::Between { .. }
        | Expr::Substring { .. }
        | Expr::Tuple(_)
        | Expr::Case { .. } => true,
        // Grouping sets, and GROUP BY DISTINCT's mark: the question path holds no level for either.
        Expr::Rollup(_) | Expr::Cube(_) | Expr::GroupingSets(_) => true,
        e if super::grouping::is_distinct_mark(e) => true,
        Expr::Value(v) => matches!(v.value, SqlValue::Placeholder(_)),
        // An OR: the question path reads one only where its arms are keys or elements.
        Expr::BinaryOp { left, op, right } => {
            !(matches!(op, BinaryOperator::And) || is_comparison(op))
                || calls_beyond_the_question(left)
                || calls_beyond_the_question(right)
        }
        Expr::UnaryOp { op, expr: x } => {
            !matches!(op, UnaryOperator::Not) || calls_beyond_the_question(x)
        }
        // A cast: the question path holds a level for a constant's, and for a month's as the key
        // spells it; any other is read on each row.
        Expr::Cast { expr: x, .. } => !is_written_constant(x) || calls_beyond_the_question(x),
        Expr::Nested(x)
        | Expr::IsNull(x)
        | Expr::IsNotNull(x)
        | Expr::IsTrue(x)
        | Expr::IsNotTrue(x)
        | Expr::IsFalse(x)
        | Expr::IsNotFalse(x)
        | Expr::IsUnknown(x)
        | Expr::IsNotUnknown(x) => calls_beyond_the_question(x),
        // A COLLATE: the question path holds a level only for one that orders as its column does.
        Expr::Collate { .. } => true,
        Expr::IsDistinctFrom(a, b)
        | Expr::IsNotDistinctFrom(a, b)
        | Expr::AnyOp {
            left: a, right: b, ..
        }
        | Expr::AllOp {
            left: a, right: b, ..
        } => calls_beyond_the_question(a) || calls_beyond_the_question(b),
        // An IN list: the question path reads no level by one.
        Expr::InList { .. } => true,
        Expr::Array(a) => a.elem.iter().any(calls_beyond_the_question),
        // A subscript or a field: the question path holds no level for either.
        Expr::CompoundFieldAccess { .. } => true,
        // AT TIME ZONE: the question path holds a level only for a timestamptz column's UTC clock.
        Expr::AtTimeZone { .. } => true,
        _ => false,
    }
}

fn is_comparison(op: &BinaryOperator) -> bool {
    matches!(
        op,
        BinaryOperator::Eq
            | BinaryOperator::NotEq
            | BinaryOperator::Lt
            | BinaryOperator::LtEq
            | BinaryOperator::Gt
            | BinaryOperator::GtEq
    )
}

/// Whether an expression compares two values neither of them a constant: what the question path keys
/// on no level, and reads only between two tables' columns.
fn compares_no_constant(e: &Expr) -> bool {
    let constant = |e: &Expr| match e {
        Expr::Value(_) | Expr::TypedString(_) => true,
        Expr::Cast { expr, .. } | Expr::Nested(expr) | Expr::UnaryOp { expr, .. } => {
            matches!(expr.as_ref(), Expr::Value(_))
        }
        _ => false,
    };
    match e {
        Expr::BinaryOp { left, op, right } if is_comparison(op) => {
            !constant(left) && !constant(right)
        }
        Expr::BinaryOp { left, right, .. } => {
            compares_no_constant(left) || compares_no_constant(right)
        }
        Expr::Nested(x)
        | Expr::UnaryOp { expr: x, .. }
        | Expr::Cast { expr: x, .. }
        | Expr::IsNull(x)
        | Expr::IsNotNull(x)
        | Expr::IsTrue(x)
        | Expr::IsNotTrue(x)
        | Expr::IsFalse(x)
        | Expr::IsNotFalse(x)
        | Expr::IsUnknown(x)
        | Expr::IsNotUnknown(x) => compares_no_constant(x),
        _ => false,
    }
}

/// Whether a question over the snail's tables reads one's whole row, which only the pipeline reads:
/// a bare name none of their columns bears that names one of them, by its alias or its name, or a
/// star where an expression or a call's argument stands: of one of them, its whole row; of none,
/// refused there in PostgreSQL's words.
pub fn reads_whole_rows(catalog: &crate::catalog::Catalog, q: &Query) -> bool {
    let SetExpr::Select(s) = q.body.as_ref() else {
        return false;
    };
    let mut names: Vec<String> = Vec::new();
    let mut columns: Vec<&str> = Vec::new();
    for t in &s.from {
        for f in std::iter::once(&t.relation).chain(t.joins.iter().map(|j| &j.relation)) {
            let TableFactor::Table { name, alias, args: None, .. } = f else {
                return false;
            };
            let full = crate::catalog::object_name(name);
            let last = full.rsplit('.').next().unwrap_or(&full).to_string();
            if let Some(t) = catalog
                .tables
                .iter()
                .find(|t| t.name == full || t.name.rsplit('.').next() == Some(&last))
            {
                columns.extend(t.columns.iter().map(|c| c.name.as_str()));
            }
            names.push(alias.as_ref().map_or(last, |a| ident(&a.name)));
        }
    }
    sqlparser::ast::visit_expressions(q, |e| match e {
        Expr::Identifier(i)
            if names.contains(&ident(i)) && !columns.contains(&ident(i).as_str()) =>
        {
            std::ops::ControlFlow::Break(())
        }
        Expr::QualifiedWildcard(..) => std::ops::ControlFlow::Break(()),
        Expr::Function(f)
            if matches!(&f.args, FunctionArguments::List(l) if l.args.iter().any(|a| matches!(a,
                FunctionArg::Unnamed(FunctionArgExpr::QualifiedWildcard(_))))) =>
        {
            std::ops::ControlFlow::Break(())
        }
        _ => std::ops::ControlFlow::Continue(()),
    })
    .is_break()
}

/// Whether a question is read as a pipeline: a WITH, or a window in what it returns.
/// Whether a question over one table compares or computes in its WHERE a column PostgreSQL compares
/// a row at a time, which only the pipeline reads: the question path keys no geometric value, nor
/// an array whose elements' type has no equality; or takes min or max of an array or a composite
/// whose parts have no order, which PostgreSQL refuses only where a second value meets the first.
pub fn reads_per_row(catalog: &crate::catalog::Catalog, q: &Query) -> bool {
    let SetExpr::Select(s) = q.body.as_ref() else {
        return false;
    };
    let [from] = s.from.as_slice() else {
        return false;
    };
    let TableFactor::Table { name, .. } = &from.relation else {
        return false;
    };
    let name = crate::catalog::object_name(name);
    let Some(table) = catalog
        .tables
        .iter()
        .find(|t| t.name == name || t.name.rsplit('.').next() == Some(name.as_str()))
    else {
        return false;
    };
    let by_rows: Vec<&str> = table
        .columns
        .iter()
        .filter(|c| c.ty.compared_by_rows())
        .map(|c| c.name.as_str())
        .collect();
    fn names(e: &Expr, by_rows: &[&str]) -> bool {
        match e {
            Expr::Identifier(i) => by_rows.contains(&crate::catalog::ident(i).as_str()),
            Expr::CompoundIdentifier(parts) => parts
                .last()
                .is_some_and(|i| by_rows.contains(&crate::catalog::ident(i).as_str())),
            Expr::Nested(e) | Expr::Cast { expr: e, .. } | Expr::UnaryOp { expr: e, .. } => {
                names(e, by_rows)
            }
            Expr::BinaryOp { left, right, .. } => names(left, by_rows) || names(right, by_rows),
            _ => false,
        }
    }
    fn operated(e: &Expr, by_rows: &[&str], table: &crate::catalog::Table) -> bool {
        match e {
            Expr::BinaryOp {
                left,
                op: BinaryOperator::And,
                right,
            } => operated(left, by_rows, table) || operated(right, by_rows, table),
            Expr::BinaryOp {
                left,
                op: BinaryOperator::Or,
                right,
            } => names(left, by_rows) || names(right, by_rows),
            Expr::Nested(e) => operated(e, by_rows, table),
            Expr::UnaryOp { expr: e, .. } => names(e, by_rows),
            Expr::BinaryOp { .. } => {
                names(e, by_rows) && !super::question::plane_condition(table, e)
            }
            _ => false,
        }
    }
    let unordered: Vec<&str> = table
        .columns
        .iter()
        .filter(|c| (c.ty.is_nested() && !c.ty.ordered()) || crate::composed::is_vector(c.ty))
        .map(|c| c.name.as_str())
        .collect();
    fn called(e: &Expr) -> Option<&sqlparser::ast::Function> {
        match e {
            Expr::Function(f) => Some(f),
            Expr::Nested(e) | Expr::Cast { expr: e, .. } => called(e),
            _ => None,
        }
    }
    let extreme = s.projection.iter().any(|i| {
        let (SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. }) = i else {
            return false;
        };
        let Some(f) = called(e) else {
            return false;
        };
        let called = crate::catalog::object_name(&f.name).to_lowercase();
        let FunctionArguments::List(list) = &f.args else {
            return false;
        };
        matches!(called.as_str(), "min" | "max")
            && list.args.iter().any(|a| {
                matches!(a, FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) if names(e, &unordered))
            })
    });
    // What it returns computed from such a column, a distance among them.
    let computed = s.projection.iter().any(|i| match i {
        SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
            matches!(e, Expr::BinaryOp { .. }) && names(e, &by_rows)
        }
        _ => false,
    });
    extreme
        || computed
        || s.selection
            .as_ref()
            .is_some_and(|w| !by_rows.is_empty() && operated(w, &by_rows, table))
}

/// Whether a FROM of the query, or of a query its FROM or WITH holds, draws a sample of a table:
/// read as a step reads it, among the relations it meets.
pub(crate) fn samples(q: &Query) -> bool {
    fn body(b: &SetExpr) -> bool {
        match b {
            SetExpr::Select(s) => s.from.iter().any(|t| {
                std::iter::once(&t.relation)
                    .chain(t.joins.iter().map(|j| &j.relation))
                    .any(factor)
            }),
            SetExpr::Query(q) => samples(q),
            SetExpr::SetOperation { left, right, .. } => body(left) || body(right),
            _ => false,
        }
    }
    fn factor(f: &TableFactor) -> bool {
        match f {
            TableFactor::Table { sample, .. } => sample.is_some(),
            TableFactor::Derived { subquery, .. } => samples(subquery),
            TableFactor::NestedJoin {
                table_with_joins, ..
            } => std::iter::once(&table_with_joins.relation)
                .chain(table_with_joins.joins.iter().map(|j| &j.relation))
                .any(factor),
            _ => false,
        }
    }
    q.with
        .as_ref()
        .is_some_and(|w| w.cte_tables.iter().any(|c| samples(&c.query)))
        || body(&q.body)
}

pub fn is_pipeline(q: &Query) -> bool {
    if q.with.is_some() || samples(q) {
        return true;
    }
    let SetExpr::Select(s) = q.body.as_ref() else {
        return false;
    };
    s.projection.iter().any(|i| {
        let e = match i {
            SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => e,
            _ => return false,
        };
        let mut calls = Calls::default();
        find_calls(e, &mut calls);
        !calls.windows.is_empty()
    })
}

/// An answer's rows as values: integers, numerics with their scale, text, bytes and truths.
pub fn relation_of(batch: &arrow::array::RecordBatch) -> Result<Relation> {
    use arrow::array::{
        Array, BinaryArray, BooleanArray, Decimal128Array, FixedSizeBinaryArray, Int16Array,
        Int32Array, Int64Array, StringArray, TimestampMicrosecondArray,
    };
    use arrow::datatypes::DataType as A;
    let mut rows = vec![Vec::with_capacity(batch.num_columns()); batch.num_rows()];
    let schema = batch.schema();
    for (a, field) in batch.columns().iter().zip(schema.fields()) {
        let any = a.as_any();
        let scalar = crate::scalars::field_scalar(field);
        let string = crate::strings::field_string(field);
        let numeric = crate::numeric::is_numeric_field(field);
        let composed = crate::composed::field_composed(field);
        for (r, row) in rows.iter_mut().enumerate() {
            row.push(if a.is_null(r) {
                match composed {
                    // A composed column's NULL keeps its type, which its operators read.
                    Some(ty) if ty.is_nested() => Value::Composed(
                        ty,
                        std::sync::Arc::new(crate::composed::Compound::new(
                            ty,
                            crate::composed::Item::Null,
                        )),
                    ),
                    _ => Value::Null,
                }
            } else if let Some(ty) = composed {
                Value::Composed(
                    ty,
                    crate::composed::compound_at(ty, a.as_ref(), r).ok_or_else(|| {
                        Error::new(format!("{} is not of its type", field.name()))
                    })?,
                )
            } else if let Some(ty) = string.filter(|t| t.is_bits()) {
                Value::Bits(
                    ty,
                    crate::strings::bits_key_at(ty, a.as_ref(), r).ok_or_else(|| {
                        Error::new(format!("{} is not of its type", field.name()))
                    })?,
                )
            } else if let Some(ty) = string.filter(|t| t.is_held_tree()) {
                // A path, a polygon or a snapshot, its tree made into the value a row holds.
                let held = crate::composed::group_at(ty, a.as_ref(), r)
                    .ok_or_else(|| Error::new(format!("{} is not of its type", field.name())))?;
                super::fill::value_of(&held, ty)
            } else if let Some(ColumnType::Geometric(k)) = string {
                let s = any
                    .downcast_ref::<arrow::array::StructArray>()
                    .ok_or_else(|| Error::new(format!("{} is not of its type", field.name())))?;
                let leaves: Vec<f64> = s
                    .columns()
                    .iter()
                    .map(|l| {
                        l.as_any()
                            .downcast_ref::<arrow::array::Float64Array>()
                            .map(|f| f.value(r))
                    })
                    .collect::<Option<_>>()
                    .ok_or_else(|| Error::new(format!("{} is not of its type", field.name())))?;
                Value::Geometric(crate::geo::geo_from_leaves(k, &leaves, false)?)
            } else if let Some(ty) =
                string.filter(|t| matches!(t, ColumnType::Textual(_) | ColumnType::Internal(_)))
            {
                let wrong = || Error::new(format!("{} is not of its type", field.name()));
                if ty == ColumnType::Textual(crate::textual::Textual::Aclitem) {
                    Value::Typed(
                        ty,
                        crate::strings::aclitem_at(a.as_ref(), r).ok_or_else(wrong)?,
                    )
                } else if ty.held_as_bytes() {
                    Value::Bytes(
                        any.downcast_ref::<BinaryArray>()
                            .ok_or_else(wrong)?
                            .value(r)
                            .to_vec(),
                    )
                } else {
                    Value::Typed(
                        ty,
                        any.downcast_ref::<StringArray>()
                            .ok_or_else(wrong)?
                            .value(r)
                            .to_string(),
                    )
                }
            } else if let Some(ty) = string {
                Value::Str(
                    ty,
                    any.downcast_ref::<StringArray>()
                        .ok_or_else(|| Error::new(format!("{} is not of its type", field.name())))?
                        .value(r)
                        .to_string(),
                )
            } else if let Some(ColumnType::Interval { .. }) = scalar {
                Value::Interval(
                    crate::scalars::interval_at(a.as_ref(), r).ok_or_else(|| {
                        Error::new(format!("{} is not of its type", field.name()))
                    })?,
                )
            } else if let Some(ty) = scalar {
                Value::Scalar(
                    ty,
                    crate::scalars::scalar_key_at(ty, a.as_ref(), r).ok_or_else(|| {
                        Error::new(format!("{} is not of its type", field.name()))
                    })?,
                )
            } else {
                match a.data_type() {
                    A::Int16 => {
                        Value::Int(any.downcast_ref::<Int16Array>().unwrap().value(r) as i64)
                    }
                    A::Int32 => {
                        Value::Int(any.downcast_ref::<Int32Array>().unwrap().value(r) as i64)
                    }
                    A::Int64 => Value::Int(any.downcast_ref::<Int64Array>().unwrap().value(r)),
                    // An instant, and a timestamp of no zone, as the scalars they are.
                    A::Timestamp(_, zone) => Value::Scalar(
                        if zone.is_some() {
                            ColumnType::Timestamptz
                        } else {
                            ColumnType::Timestamp(None)
                        },
                        any.downcast_ref::<TimestampMicrosecondArray>()
                            .unwrap()
                            .value(r),
                    ),
                    A::Decimal128(_, s) => Value::Num(
                        any.downcast_ref::<Decimal128Array>().unwrap().value(r),
                        *s as u32,
                    ),
                    // A numeric(p, s), its decimal and whether it is NaN.
                    A::Struct(_) if numeric => match crate::numeric::decimal_at(a.as_ref(), r) {
                        Some((Some(m), _, s)) => Value::Num(m, s as u32),
                        Some((None, _, _)) => Value::Numeric(Numeric::NAN),
                        None => {
                            return Err(Error::new(format!("{} is not of its type", field.name())))
                        }
                    },
                    A::Utf8 if numeric => Value::of_numeric(Numeric::parse(
                        any.downcast_ref::<StringArray>().unwrap().value(r),
                    )?),
                    A::Utf8 => Value::Text(
                        any.downcast_ref::<StringArray>()
                            .unwrap()
                            .value(r)
                            .to_string(),
                    ),
                    A::Binary => {
                        Value::Bytes(any.downcast_ref::<BinaryArray>().unwrap().value(r).to_vec())
                    }
                    A::Boolean => Value::Bool(any.downcast_ref::<BooleanArray>().unwrap().value(r)),
                    A::Float32 => Value::Real(
                        any.downcast_ref::<arrow::array::Float32Array>()
                            .unwrap()
                            .value(r),
                    ),
                    A::Float64 => Value::Double(
                        any.downcast_ref::<arrow::array::Float64Array>()
                            .unwrap()
                            .value(r),
                    ),
                    A::FixedSizeBinary(16) => Value::Uuid(crate::scalars::uuid_key(
                        any.downcast_ref::<FixedSizeBinaryArray>()
                            .unwrap()
                            .value(r)
                            .try_into()
                            .unwrap(),
                    )),
                    other => return Err(Error::new(format!("a column of {other} is not read"))),
                }
            });
        }
    }
    Ok(Relation {
        names: batch
            .schema()
            .fields()
            .iter()
            .map(|f| f.name().clone())
            .collect(),
        rows,
        order: None,
    })
}

/// A relation as record batch: each column of the kind its values are, numerics at the greatest
/// scale among them.
pub fn batch_of(rel: &Relation) -> Result<arrow::array::RecordBatch> {
    use arrow::array::{
        ArrayRef, BinaryArray, BooleanArray, Decimal128Array, FixedSizeBinaryArray, Int64Array,
        RecordBatch, RecordBatchOptions, StringArray,
    };
    use arrow::datatypes::{Field, Schema};
    use std::sync::Arc;
    let mut fields = Vec::new();
    let mut arrays: Vec<ArrayRef> = Vec::new();
    for (i, name) in rel.names.iter().enumerate() {
        let col = || rel.rows.iter().map(move |r| &r[i]);
        let scale = col()
            .filter_map(|v| match v {
                Value::Num(_, s) => Some(*s),
                _ => None,
            })
            .max();
        let first = col().find(|v| !v.is_null());
        // A column of geometric values: its type's leaves, a group of float8s or a path's or a
        // polygon's tree, where every value is of one type; otherwise each value's text, named for
        // the first one's type.
        if let Some(Value::Geometric(g)) = first {
            let k = g.kind();
            let one = col().all(|v| !matches!(v, Value::Geometric(h) if h.kind() != k));
            if one {
                let ty = ColumnType::Geometric(k);
                let bytes: Vec<Option<Vec<u8>>> = col()
                    .map(|v| match v {
                        Value::Geometric(g) => Some(crate::geo::geo_bytes(g)),
                        _ => None,
                    })
                    .collect();
                arrays.push(crate::strings::string_array(
                    ty,
                    bytes.iter().map(Option::as_deref),
                ));
                fields.push(crate::strings::string_field(name, ty, true));
            } else {
                arrays.push(Arc::new(
                    col()
                        .map(|v| match v {
                            Value::Geometric(g) => Some(crate::geo::geo_out(g)),
                            _ => None,
                        })
                        .collect::<StringArray>(),
                ));
                fields.push(
                    Field::new(name, arrow::datatypes::DataType::Utf8, true).with_metadata(
                        [(crate::scalars::PG_TYPE.to_string(), k.name().to_string())].into(),
                    ),
                );
            }
            continue;
        }
        // A column of a type held as its text, named for it; of inet and cidr together, inet.
        if let Some(Value::Typed(ty, _)) = first {
            let ty = match *ty {
                ty if col().all(|v| !matches!(v, Value::Typed(t, _) if *t != ty)) => ty,
                _ => ColumnType::Textual(crate::textual::Textual::Inet),
            };
            arrays.push(crate::strings::string_array(
                ty,
                col().map(|v| match v {
                    Value::Typed(_, s) => Some(s.as_bytes()),
                    _ => None,
                }),
            ));
            fields.push(crate::strings::string_field(name, ty, true));
            continue;
        }
        if let Some(Value::Str(ty, _) | Value::Bits(ty, _)) = first {
            // A column of bit strings of more than one type is bit varying.
            let ty = match *ty {
                ty if col().all(|v| match v {
                    Value::Str(t, _) | Value::Bits(t, _) => *t == ty,
                    _ => true,
                }) =>
                {
                    ty
                }
                ty if ty.is_bits() => ColumnType::Varbit(None),
                _ => ColumnType::Bpchar(None),
            };
            arrays.push(crate::strings::string_array(
                ty,
                col().map(|v| match v {
                    Value::Str(_, s) => Some(s.as_bytes()),
                    Value::Bits(_, k) => Some(k.as_slice()),
                    _ => None,
                }),
            ));
            fields.push(crate::strings::string_field(name, ty, true));
            continue;
        }
        if let Some(Value::Interval(_)) = first {
            arrays.push(crate::scalars::interval_array(col().map(|v| match v {
                Value::Interval(x) => Some(*x),
                _ => None,
            })));
            fields.push(crate::scalars::scalar_field(
                name,
                ColumnType::Interval {
                    range: crate::times::INTERVAL_FULL_RANGE,
                    precision: None,
                },
                true,
            ));
            continue;
        }
        if let Some(Value::Scalar(ty, _)) = first {
            let ty = *ty;
            arrays.push(crate::scalars::scalar_array(
                ty,
                col().map(|v| match v {
                    Value::Scalar(_, k) => Some(*k),
                    _ => None,
                }),
            ));
            fields.push(crate::scalars::scalar_field(name, ty, true));
            continue;
        }
        // A numeric column whose values print at more than one scale, or one holding a value no
        // Decimal128 of 38 digits holds, is written as each value's text, named a numeric.
        let texted = |s: u32| {
            col().any(|v| match v {
                Value::Numeric(_) => true,
                Value::Int(x) => s > 0 || x.unsigned_abs() >= 10u64.pow(18),
                Value::Num(m, from) => *from != s || m.unsigned_abs() >= 10u128.pow(38),
                _ => false,
            })
        };
        if matches!(first, Some(Value::Numeric(_)))
            || (matches!(first, Some(Value::Int(_) | Value::Num(..)))
                && (scale.is_some_and(texted) || col().any(|v| matches!(v, Value::Numeric(_)))))
        {
            arrays.push(Arc::new(col().map(|v| v.text()).collect::<StringArray>()));
            fields.push(crate::numeric::text_field(name, true));
            continue;
        }
        if let Some(Value::Composed(ty, _)) = first {
            let ty = *ty;
            arrays.push(crate::composed::arrow_array(
                ty,
                col().map(|v| match v {
                    Value::Composed(_, c) => Some(&**c),
                    _ => None,
                }),
            )?);
            fields.push(crate::composed::arrow_field(name, ty, true));
            continue;
        }
        let a: ArrayRef = match (first, scale) {
            (Some(Value::Int(_)) | Some(Value::Num(..)), Some(s)) => Arc::new(
                col()
                    .map(|v| v.numeric().map(|(m, from)| Value::at_scale(m, from, s)))
                    .collect::<Decimal128Array>()
                    .with_precision_and_scale(38, s as i8)?,
            ),
            (Some(Value::Int(_)), None) => Arc::new(
                col()
                    .map(|v| match v {
                        Value::Int(x) => Some(*x),
                        _ => None,
                    })
                    .collect::<Int64Array>(),
            ),
            (Some(Value::Bytes(_)), _) => Arc::new(
                col()
                    .map(|v| match v {
                        Value::Bytes(b) => Some(b.as_slice()),
                        _ => None,
                    })
                    .collect::<BinaryArray>(),
            ),
            (Some(Value::Bool(_)), _) => Arc::new(
                col()
                    .map(|v| match v {
                        Value::Bool(b) => Some(*b),
                        _ => None,
                    })
                    .collect::<BooleanArray>(),
            ),
            (Some(Value::Real(_)), _) => Arc::new(
                col()
                    .map(|v| match v {
                        Value::Real(f) => Some(*f),
                        _ => None,
                    })
                    .collect::<arrow::array::Float32Array>(),
            ),
            (Some(Value::Double(_)), _) => Arc::new(
                col()
                    .map(|v| match v {
                        Value::Double(d) => Some(*d),
                        _ => None,
                    })
                    .collect::<arrow::array::Float64Array>(),
            ),
            (Some(Value::Uuid(_)), _) => {
                Arc::new(FixedSizeBinaryArray::try_from_sparse_iter_with_size(
                    col().map(|v| match v {
                        Value::Uuid(k) => Some(crate::scalars::uuid_bytes(*k)),
                        _ => None,
                    }),
                    16,
                )?)
            }
            _ => Arc::new(col().map(|v| v.text()).collect::<StringArray>()),
        };
        fields.push(Field::new(name, a.data_type().clone(), true));
        arrays.push(a);
    }
    Ok(RecordBatch::try_new_with_options(
        Arc::new(Schema::new(fields)),
        arrays,
        &RecordBatchOptions::new().with_row_count(Some(rel.rows.len())),
    )?)
}

/// Sorts a relation's rows on one of its columns, ascending, its NULLs last, so they lie in the
/// table's order.
pub fn in_order(rel: &mut Relation, column: usize) -> Result<()> {
    let mut failed = None;
    rel.rows
        .sort_by(|a, b| match compare(&a[column], &b[column]) {
            Ok(Some(o)) => o,
            Ok(None) => a[column].is_null().cmp(&b[column].is_null()),
            Err(e) => {
                failed.get_or_insert(e);
                Ordering::Equal
            }
        });
    rel.order = Some(column);
    failed.map_or(Ok(()), Err)
}

/// A WITH's queries refused where one's name is another's, before any of them is read, as
/// PostgreSQL refuses them.
pub(in crate::read) fn named_once(ctes: &[sqlparser::ast::Cte]) -> Result<()> {
    let names: Vec<String> = ctes.iter().map(|c| ident(&c.alias.name)).collect();
    match (0..names.len()).find(|&i| names[..i].contains(&names[i])) {
        Some(i) => Err(Error::new(format!(
            "WITH query name \"{}\" specified more than once",
            names[i]
        ))),
        None => Ok(()),
    }
}

/// The one table a pipeline reads, by name: every table its steps name that no step of the WITH is.
pub fn table_named(q: &Query) -> Result<String> {
    if let Some(with) = &q.with {
        named_once(&with.cte_tables)?;
    }
    if q.with.as_ref().is_some_and(|w| w.recursive) {
        return Err(Error::new("WITH RECURSIVE is not read"));
    }
    let mut steps: Vec<String> = Vec::new();
    let mut tables: Vec<String> = Vec::new();
    let mut note = |body: &SetExpr, steps: &[String]| -> Result<()> {
        let s = match body {
            SetExpr::Select(s) => s,
            SetExpr::SetOperation { .. } => {
                return Err(Error::new("set operations are not read"));
            }
            other => {
                return Err(Error::new(format!(
                    "{other} is not read: a step is a SELECT"
                )))
            }
        };
        for item in &s.from {
            let mut names = vec![&item.relation];
            names.extend(item.joins.iter().map(|j| &j.relation));
            for t in names {
                let TableFactor::Table { name, .. } = t else {
                    return Err(Error::new(format!(
                        "{t} is not read: a step reads tables and steps by name"
                    )));
                };
                let n = crate::catalog::object_name(name);
                if !steps.contains(&n) && !tables.contains(&n) {
                    tables.push(n);
                }
            }
        }
        Ok(())
    };
    if let Some(with) = &q.with {
        for cte in &with.cte_tables {
            note(cte.query.body.as_ref(), &steps)?;
            steps.push(ident(&cte.alias.name));
        }
    }
    note(q.body.as_ref(), &steps)?;
    match tables.as_slice() {
        [t] => Ok(t.clone()),
        [] => Err(Error::new("the question reads no table")),
        _ => Err(Error::new("a pipeline reads only one table")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_numeric_is_written_with_every_digit_of_its_scale() {
        assert_eq!(numeric_text(12345, 2), "123.45");
        assert_eq!(numeric_text(-50, 2), "-0.50");
        assert_eq!(numeric_text(7, 0), "7");
        assert_eq!(numeric_text(0, 2), "0.00");
        assert!(matches!(
            numeric_literal("9999999.99").unwrap(),
            Value::Num(999999999, 2)
        ));
        assert!(matches!(
            numeric_literal("100000").unwrap(),
            Value::Int(100000)
        ));
        assert_eq!(Value::Bytes(vec![0xc1, 0x01]).text().unwrap(), "\\xc101");
        assert_eq!(crate::strings::bytea_in("\\xc101").unwrap(), [0xc1, 0x01]);
    }

    fn query(sql: &str) -> Query {
        let st =
            sqlparser::parser::Parser::parse_sql(&sqlparser::dialect::PostgreSqlDialect {}, sql)
                .unwrap()
                .remove(0);
        let sqlparser::ast::Statement::Query(q) = st else {
            panic!("{sql} is no query")
        };
        *q
    }

    /// Five rows (seq, g, amount, k) in seq order, one amount at a scale of its own.
    fn table() -> Table {
        let text = |s: &str| Value::Text(s.into());
        Table {
            name: "t".into(),
            relation: Relation {
                names: ["seq", "g", "amount", "k"].map(String::from).to_vec(),
                rows: vec![
                    vec![Value::Int(1), text("a"), Value::Num(150, 2), Value::Null],
                    vec![Value::Int(2), text("a"), Value::Num(225, 2), text("x")],
                    vec![Value::Int(3), text("b"), Value::Num(-75, 2), Value::Null],
                    vec![Value::Int(4), text("a"), Value::Num(100, 2), text("x")],
                    vec![Value::Int(5), text("b"), Value::Num(15, 1), Value::Null],
                ],
                order: Some(0),
            },
        }
    }

    #[test]
    fn windows_fold_along_the_order_and_numbers_compare_at_any_scale() {
        let q = query(
            "WITH w AS (
                SELECT seq, g, amount, k,
                       lag(g) OVER (ORDER BY seq) AS before,
                       sum(amount) OVER (PARTITION BY g ORDER BY seq ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS prefix,
                       count(*) OVER (ORDER BY seq) AS n
                FROM t
            )
            SELECT seq, before, prefix, n,
                   k IS NOT DISTINCT FROM NULL AS k_null,
                   amount > 1.5 AS over,
                   amount = CAST($1 AS numeric) AS same,
                   CASE WHEN before IS DISTINCT FROM g THEN 1 ELSE 0 END AS starts
            FROM w ORDER BY seq",
        );
        let out = run(&q, &table(), &["1.5".into()]).unwrap();
        let rows: Vec<Vec<Option<String>>> = out
            .rows
            .iter()
            .map(|r| r.iter().map(Value::text).collect())
            .collect();
        let want = [
            ["1", "", "1.50", "1", "true", "false", "true", "1"],
            ["2", "a", "3.75", "2", "false", "true", "false", "0"],
            ["3", "a", "-0.75", "3", "true", "false", "false", "1"],
            ["4", "b", "4.75", "4", "false", "false", "false", "1"],
            ["5", "a", "0.75", "5", "true", "false", "true", "1"],
        ];
        let want: Vec<Vec<Option<String>>> = want
            .iter()
            .map(|r| {
                r.iter()
                    .map(|s| (!s.is_empty()).then(|| s.to_string()))
                    .collect()
            })
            .collect();
        assert_eq!(rows, want);
    }

    #[test]
    fn a_stream_fed_in_chunks_answers_as_one_fed_whole() {
        let text = |r: &Relation| -> Vec<Vec<Option<String>>> {
            r.rows
                .iter()
                .map(|row| row.iter().map(Value::text).collect())
                .collect()
        };
        for sql in [
            "SELECT seq, lag(g) OVER (ORDER BY seq) AS before, sum(amount) OVER (PARTITION BY g ORDER BY seq) AS prefix FROM t",
            "WITH w AS (SELECT seq, g, amount, sum(amount) OVER (PARTITION BY g ORDER BY seq) AS prefix FROM t) SELECT g, count(*) AS n, max(prefix) AS reach FROM w GROUP BY g ORDER BY g",
        ] {
            let q = query(sql);
            let t = table();
            let whole = run(&q, &t, &[]).unwrap();
            let mut s = Stream::new(
                &q,
                "t",
                t.relation.names.clone(),
                vec![None; t.relation.names.len()],
                t.relation.order,
                &[],
                &[],
                crate::regs::LATEST,
                Casings::default(),
            )
            .unwrap();
            for part in [&t.relation.rows[..2], &t.relation.rows[2..]] {
                s.feed(Relation {
                    names: t.relation.names.clone(),
                    rows: part.to_vec(),
                    order: t.relation.order,
                })
                .unwrap();
            }
            assert!(s.most_held() < 4 * t.relation.rows.len());
            assert_eq!(text(&s.finish().unwrap()), text(&whole), "{sql}");
            assert!(!whole.rows.is_empty());
        }
    }

    #[test]
    fn a_window_off_the_order_descending_or_offset_reads_its_rows_whole() {
        let text = |r: &Relation| -> Vec<Vec<Option<String>>> {
            r.rows
                .iter()
                .map(|row| row.iter().map(Value::text).collect())
                .collect()
        };
        let num = |m: i128| Value::Num(m, 2);
        for (sql, want) in [
            // Peers in the order the rows came: seq 1, 2 and 4 under 'a', then 3 and 5.
            (
                "SELECT seq, lag(seq) OVER (ORDER BY g) AS x FROM t",
                [Value::Null, Value::Int(1), Value::Int(4), Value::Int(2), Value::Int(3)],
            ),
            (
                "SELECT seq, lag(seq) OVER (ORDER BY seq DESC) AS x FROM t",
                [Value::Int(2), Value::Int(3), Value::Int(4), Value::Int(5), Value::Null],
            ),
            (
                "SELECT seq, sum(amount) OVER (ORDER BY seq ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) AS x FROM t",
                [num(150), num(375), num(150), num(25), num(250)],
            ),
        ] {
            let out = run(&query(sql), &table(), &[]).unwrap();
            let expected: Vec<Vec<Option<String>>> = want
                .iter()
                .enumerate()
                .map(|(i, v)| vec![Value::Int(i as i64 + 1).text(), v.text()])
                .collect();
            assert_eq!(text(&out), expected, "{sql}");
        }
    }
}

/// A part of a composed value as a value of its own type.
pub(super) fn part_value(ty: ColumnType, item: &crate::composed::Item) -> Value {
    super::fill::value_of(&crate::composed::datum_of(ty, item), ty)
}

/// A value as a part of a composed value of type `ty`: a quoted literal read by the type's input,
/// any other value assigned to it.
pub(super) fn part_of(ty: ColumnType, v: Value, quoted: bool) -> Result<crate::composed::Item> {
    use crate::composed::Item;
    use crate::read::statement::Datum;
    let d = match v {
        Value::Null => return Ok(Item::Null),
        v if v.is_null() => return Ok(Item::Null),
        Value::Text(s) if quoted => crate::composed::scalar_in(ty, &s)?,
        v => super::fill::assign_value(v, ty, "")?,
    };
    Ok(match d {
        Datum::Null => Item::Null,
        Datum::Compound(c) => c.item.clone(),
        d => Item::Scalar(d),
    })
}

/// A composed value of a type from its parts.
pub(super) fn composed(ty: ColumnType, item: crate::composed::Item) -> Value {
    Value::Composed(
        ty,
        std::sync::Arc::new(crate::composed::Compound::new(ty, item)),
    )
}

/// The element type an array's type names, past every dimension.
fn innermost(dt: &DataType) -> &DataType {
    use sqlparser::ast::ArrayElemTypeDef as A;
    match dt {
        DataType::Array(A::SquareBracket(inner, _) | A::AngleBracket(inner)) => innermost(inner),
        dt => dt,
    }
}

/// An ARRAY cast to the array type `ty`: each element cast to `element`, an ARRAY among them cast
/// so too, as PostgreSQL's parser hands the cast's type down to the elements.
fn array_cast(
    a: &sqlparser::ast::Array,
    ty: ColumnType,
    whole: &DataType,
    row: &[Value],
    frame: &Frame,
    computed: &Computed,
) -> Result<Value> {
    let mut values = Vec::with_capacity(a.elem.len());
    for x in &a.elem {
        values.push(match x {
            Expr::Array(inner) => (array_cast(inner, ty, whole, row, frame, computed)?, false),
            x => (
                cast(
                    eval(x, row, frame, computed)?,
                    match frame.type_of(x) {
                        Some(t) if super::typed::element_of(t).is_some() => whole,
                        _ => innermost(whole),
                    },
                    frame.release,
                    frame.casings.monetary(),
                    quoted(x),
                )?,
                false,
            ),
        });
    }
    array_value(ty, values, holds_arrays(a, frame))
}

/// An array value as the array type `ty`, each element assigned to its element type; any other
/// value, or an array already of `ty`, as it is.
fn array_as(v: Value, ty: ColumnType) -> Result<Value> {
    use crate::composed::Item;
    let (Value::Composed(from, c), Some(to)) = (&v, super::typed::element_of(ty)) else {
        return Ok(v);
    };
    let Some(was) = super::typed::element_of(*from).filter(|_| *from != ty) else {
        return Ok(v);
    };
    let Item::Array {
        elements,
        dims,
        lower,
    } = &c.item
    else {
        return Ok(composed(ty, c.item.clone()));
    };
    let elements = elements
        .iter()
        .map(|i| match i {
            Item::Scalar(d) => part_of(to, super::fill::value_of(d, was), false),
            other => Ok(other.clone()),
        })
        .collect::<Result<Vec<_>>>()?;
    // A cast of no elements leaves an empty array, of no dimensions, as a vector's may be not.
    let (dims, lower) = if elements.is_empty() && was != to {
        (&Vec::new(), &Vec::new())
    } else {
        (dims, lower)
    };
    Ok(composed(
        ty,
        Item::Array {
            elements,
            dims: dims.clone(),
            lower: lower.clone(),
        },
    ))
}

/// Whether an ARRAY's elements are arrays, each a dimension of it.
fn holds_arrays(a: &sqlparser::ast::Array, frame: &Frame) -> bool {
    a.elem.iter().any(|x| {
        matches!(x, Expr::Array(_))
            || frame
                .type_of(x)
                .is_some_and(|t| {
                    super::typed::element_of(t).is_some() && !crate::composed::is_vector(t)
                })
    })
}

/// An ARRAY of the array type `ty` from its elements' values, each with whether it is a quoted
/// literal: one dimension of the elements, or, where they are arrays, one dimension more than
/// theirs, a NULL or empty array among them passed over where every one is, as ExecEvalArrayExpr
/// makes it.
fn array_value(ty: ColumnType, values: Vec<(Value, bool)>, nested: bool) -> Result<Value> {
    use crate::composed::Item;
    let element = super::typed::element_of(ty)
        .ok_or_else(|| Error::new(format!("{} is no array type", ty.pg_name())))?;
    if !nested {
        let n = values.len() as i32;
        let elements = values
            .into_iter()
            .map(|(v, q)| part_of(element, v, q))
            .collect::<Result<Vec<_>>>()?;
        return Ok(composed(
            ty,
            Item::Array {
                elements,
                dims: vec![n],
                lower: vec![1],
            },
        ));
    }
    let mismatch = || {
        Error::new("multidimensional arrays must have array expressions with matching dimensions")
    };
    let mut shape: Option<(Vec<i32>, Vec<i32>)> = None;
    let mut empty = false;
    let mut outer = 0;
    let mut elements = Vec::new();
    for (v, _) in values {
        let Value::Composed(from, c) = v else {
            if v.is_null() {
                empty = true;
                continue;
            }
            return Err(Error::new(format!(
                "ARRAY types {} and {} cannot be matched",
                ty.pg_name(),
                kind_name(&v)
            )));
        };
        let Item::Array {
            elements: parts,
            dims,
            lower,
        } = &c.item
        else {
            empty = true;
            continue;
        };
        if dims.is_empty() {
            empty = true;
            continue;
        }
        if dims.len() + 1 > 6 {
            return Err(Error::new(format!(
                "number of array dimensions ({}) exceeds the maximum allowed (6)",
                dims.len() + 1
            )));
        }
        match &shape {
            None => shape = Some((dims.clone(), lower.clone())),
            Some((d, l)) if d == dims && l == lower => {}
            Some(_) => return Err(mismatch()),
        }
        let from_element = super::typed::element_of(from).unwrap_or(element);
        for p in parts {
            elements.push(if from_element == element {
                p.clone()
            } else {
                part_of(element, part_value(from_element, p), false)?
            });
        }
        outer += 1;
    }
    let Some((d, l)) = shape else {
        return Ok(composed(
            ty,
            Item::Array {
                elements: Vec::new(),
                dims: Vec::new(),
                lower: Vec::new(),
            },
        ));
    };
    if empty {
        return Err(mismatch());
    }
    Ok(composed(
        ty,
        Item::Array {
            elements,
            dims: std::iter::once(outer).chain(d).collect(),
            lower: std::iter::once(1).chain(l).collect(),
        },
    ))
}

/// The composed type a value is, or a quoted literal beside it is read as: a multirange's where
/// either side is one.
fn composed_beside(x: &Value, y: &Value) -> Option<ColumnType> {
    let multi = |ty: &ColumnType| matches!(ty, ColumnType::Composed(r) if matches!(r.def(), crate::composed::Composed::Multirange { .. }));
    match (x, y) {
        (Value::Composed(ty, _), _) | (_, Value::Composed(ty, _)) if multi(ty) => Some(*ty),
        (Value::Composed(ty, _), _) | (_, Value::Composed(ty, _)) if ty.is_nested() => Some(*ty),
        _ => None,
    }
}

/// A vector's element, where a value is a vector.
fn vector_of(v: &Value) -> Option<ColumnType> {
    match v {
        Value::Composed(ColumnType::Composed(r), _) => match r.def() {
            crate::composed::Composed::Vector { element } => Some(*element),
            _ => None,
        },
        _ => None,
    }
}

/// A value with a vector made its elements' array, counted from 0 as it is; one of no elements, an
/// array of no dimensions where the other side's type makes it cast.
fn array_of_vector(v: &Value, other: Option<ColumnType>) -> Value {
    match (vector_of(v), v) {
        (Some(element), Value::Composed(_, c)) => {
            let array = super::typed::array_of(element);
            let cast = other.is_some_and(|t| super::typed::element_of(t).unwrap_or(t) != element);
            match &c.item {
                crate::composed::Item::Array { elements, .. } if elements.is_empty() && cast => {
                    composed(
                        array,
                        crate::composed::Item::Array {
                            elements: Vec::new(),
                            dims: Vec::new(),
                            lower: Vec::new(),
                        },
                    )
                }
                item => composed(array, item.clone()),
            }
        }
        _ => v.clone(),
    }
}

/// An operator of the composed types, where either side is one: an array's `||`, `@>`, `<@` and
/// `&&`; a range's `@>`, `<@`, `&&`, `<<`, `>>`, `&<`, `&>`, `-|-`, `+`, `*` and `-`, beside a
/// range or, for `@>` and `<@`, an element of its subtype; a multirange's, beside a multirange, a
/// range or an element, `+`, `*` and `-` beside a multirange. None where neither side is composed.
/// array_append, array_prepend and array_cat in the array type the step resolved them to, as `||`
/// joins an element or an array: each array read as that type, each element as its element type, a
/// quoted literal by its input. array_cat of two NULLs is NULL.
fn array_joined(
    name: &str,
    (x, left): (Value, &Expr),
    (y, right): (Value, &Expr),
    ty: Option<ColumnType>,
) -> Result<Value> {
    let element = ty.and_then(super::typed::element_of);
    let (Some(ty), Some(element)) = (ty, element) else {
        return Err(Error::new(format!(
            "{name}: the type of its arguments is not read"
        )));
    };
    let whole = |v: Value, e: &Expr| -> Result<Option<crate::composed::Item>> {
        Ok(match v.is_null() {
            true => None,
            false => Some(part_of(ty, v, quoted(e))?),
        })
    };
    let joined = match name {
        "array_append" => crate::composed::concat(
            ty,
            whole(x, left)?.as_ref(),
            Some(&part_of(element, y, quoted(right))?),
            false,
            true,
        )?,
        "array_prepend" => crate::composed::concat(
            ty,
            Some(&part_of(element, x, quoted(left))?),
            whole(y, right)?.as_ref(),
            true,
            false,
        )?,
        _ => match (whole(x, left)?, whole(y, right)?) {
            (None, None) => return Ok(Value::Null),
            (a, b) => crate::composed::concat(ty, a.as_ref(), b.as_ref(), false, false)?,
        },
    };
    Ok(composed(ty, joined))
}

fn composed_op(
    op: &BinaryOperator,
    x: &Value,
    y: &Value,
    left: &Expr,
    right: &Expr,
    typed: [Option<ColumnType>; 2],
) -> Result<Option<Value>> {
    // A NULL the step typed as a value other than an array is an element.
    let element_null = |v: &Value, t: Option<ColumnType>| {
        v.is_null() && t.is_some_and(|t| super::typed::element_of(t).is_none())
    };
    use crate::composed::{Composed, Item};
    let Some(ty) = composed_beside(x, y) else {
        return Ok(None);
    };
    let name = match op {
        BinaryOperator::StringConcat => "||",
        BinaryOperator::AtArrow => "@>",
        BinaryOperator::ArrowAt => "<@",
        BinaryOperator::PGOverlap => "&&",
        BinaryOperator::PGBitwiseShiftLeft => "<<",
        BinaryOperator::PGBitwiseShiftRight => ">>",
        BinaryOperator::AndLt => "&<",
        BinaryOperator::AndGt => "&>",
        BinaryOperator::Custom(s) if s == "-|-" => "-|-",
        BinaryOperator::Plus => "+",
        BinaryOperator::Multiply => "*",
        BinaryOperator::Minus => "-",
        _ => return Ok(None),
    };
    let quoted = |e: &Expr| matches!(e, Expr::Value(v) if matches!(v.value, SqlValue::SingleQuotedString(_) | SqlValue::Placeholder(_)));
    let ColumnType::Composed(r) = ty else {
        return Ok(None);
    };
    // A vector's `||` is its elements' array's.
    if name == "||" && (vector_of(x).is_some() || vector_of(y).is_some()) {
        let (a, b) = (
            array_of_vector(x, typed[1].or(type_of_value(y))),
            array_of_vector(y, typed[0].or(type_of_value(x))),
        );
        return composed_op(op, &a, &b, left, right, typed);
    }
    // A value as the composed type, a quoted literal read by its input; or as an element of it.
    let whole = |v: &Value, e: &Expr| -> Result<Option<Item>> {
        Ok(match v {
            Value::Composed(t, c) if *t == ty => Some(c.item.clone()),
            Value::Text(t) if quoted(e) => match crate::composed::scalar_in(ty, t)? {
                crate::read::statement::Datum::Compound(c) => Some(c.item.clone()),
                _ => None,
            },
            _ => None,
        })
    };
    let element_of = |et: ColumnType, v: &Value| -> Result<Item> {
        let d = super::fill::assign_value(v.clone(), et, "")?;
        Ok(match d {
            crate::read::statement::Datum::Null => Item::Null,
            crate::read::statement::Datum::Compound(c) => c.item.clone(),
            d => Item::Scalar(d),
        })
    };
    let out = |item: Item| {
        Value::Composed(
            ty,
            std::sync::Arc::new(crate::composed::Compound::new(ty, item)),
        )
    };
    let no = || {
        Error::new(format!(
            "operator does not exist: {} {name} {}{NO_OPERATOR}",
            kind_name(x),
            kind_name(y)
        ))
    };
    match r.def() {
        // An aclitem array's own operators, beside an aclitem: `@>` as aclcontains, `+` and `-`,
        // which PostgreSQL no longer takes.
        Composed::Array { element, .. }
            if *element == ColumnType::Textual(crate::textual::Textual::Aclitem)
                && matches!(x, Value::Composed(..))
                && !matches!(y, Value::Composed(..))
                && matches!(name, "@>" | "+" | "-") =>
        {
            if x.is_null() || y.is_null() {
                return Ok(Some(Value::Null));
            }
            let item = match y {
                Value::Typed(ColumnType::Textual(crate::textual::Textual::Aclitem), s) => s.clone(),
                Value::Text(t) if quoted(right) => {
                    crate::textual::text_in(crate::textual::Textual::Aclitem, t)?
                }
                _ => return Err(no()),
            };
            match name {
                "+" => Err(Error::new("aclinsert is no longer supported")),
                "-" => Err(Error::new("aclremove is no longer supported")),
                _ => {
                    let Some(Item::Array { elements, dims, .. }) = whole(x, left)? else {
                        return Err(no());
                    };
                    Ok(Some(Value::Bool(crate::textual::aclcontains(
                        &elements,
                        dims.len(),
                        &item,
                    )?)))
                }
            }
        }
        Composed::Array { element, .. } | Composed::Vector { element } => {
            let (a, b) = (whole(x, left)?, whole(y, right)?);
            if name == "||" {
                let first = matches!(y, Value::Composed(..))
                    && a.is_none()
                    && (!x.is_null() || element_null(x, typed[0]))
                    && !quoted(left);
                let last = matches!(x, Value::Composed(..))
                    && b.is_none()
                    && (!y.is_null() || element_null(y, typed[1]))
                    && !quoted(right);
                if first {
                    let e = element_of(*element, x)?;
                    return Ok(Some(out(crate::composed::concat(
                        ty,
                        Some(&e),
                        b.as_ref(),
                        true,
                        false,
                    )?)));
                }
                if last {
                    let e = element_of(*element, y)?;
                    return Ok(Some(out(crate::composed::concat(
                        ty,
                        a.as_ref(),
                        Some(&e),
                        false,
                        true,
                    )?)));
                }
                if a.is_none() && b.is_none() {
                    return Ok(Some(Value::Null));
                }
                return Ok(Some(out(crate::composed::concat(
                    ty,
                    a.as_ref(),
                    b.as_ref(),
                    false,
                    false,
                )?)));
            }
            if !matches!(name, "@>" | "<@" | "&&") {
                return Err(no());
            }
            if x.is_null() || y.is_null() {
                return Ok(Some(Value::Null));
            }
            let (Some(a), Some(b)) = (a, b) else {
                return Err(no());
            };
            if let Some(part) = ty.unequal_part() {
                return Err(unequal(part));
            }
            Ok(Some(Value::Bool(crate::composed::array_op(
                ty, name, &a, &b,
            )?)))
        }
        Composed::Range { subtype, .. } => {
            if x.is_null() || y.is_null() {
                return Ok(Some(Value::Null));
            }
            let (a, b) = (whole(x, left)?, whole(y, right)?);
            match (a, b) {
                (Some(a), Some(b)) => {
                    Ok(Some(match crate::composed::range_op(ty, name, &a, &b)? {
                        crate::composed::RangeAnswer::Truth(t) => Value::Bool(t),
                        crate::composed::RangeAnswer::Range(i) => out(i),
                    }))
                }
                (Some(a), None) if name == "@>" => Ok(Some(Value::Bool(
                    crate::composed::range_holds(ty, &a, &element_of(*subtype, y)?)?,
                ))),
                (None, Some(b)) if name == "<@" => Ok(Some(Value::Bool(
                    crate::composed::range_holds(ty, &b, &element_of(*subtype, x)?)?,
                ))),
                _ => Err(no()),
            }
        }
        Composed::Multirange { range, .. } => {
            use crate::composed::Spanned;
            if x.is_null() || y.is_null() {
                return Ok(Some(Value::Null));
            }
            // A side as a multirange, a quoted literal read by its input, or as a range of its
            // ranges' type.
            let side = |v: &Value, e: &Expr| -> Result<Option<Spanned>> {
                Ok(match v {
                    Value::Composed(t, c) if *t == *range => Some(Spanned::Range(c.item.clone())),
                    _ => match whole(v, e)? {
                        Some(Item::Multirange(rs)) => Some(Spanned::Ranges(rs)),
                        _ => None,
                    },
                })
            };
            let subtype = crate::composed::range_of(ty).map_or(ColumnType::Int4, |r| r.0);
            let (a, b) = match (side(x, left)?, side(y, right)?) {
                (Some(a), Some(b)) => (a, b),
                (Some(a), None) if name == "@>" => (a, Spanned::Element(element_of(subtype, y)?)),
                (None, Some(b)) if name == "<@" => (Spanned::Element(element_of(subtype, x)?), b),
                _ => return Err(no()),
            };
            Ok(Some(
                match crate::composed::multirange_op(ty, name, &a, &b)? {
                    crate::composed::RangeAnswer::Truth(t) => Value::Bool(t),
                    crate::composed::RangeAnswer::Range(i) => out(i),
                },
            ))
        }
        _ => Err(no()),
    }
}

/// `x op ANY (array)` or `x op ALL (array)`: each element compared with `x`, true where one (or
/// each) holds, NULL where none (or not every one) holds and one compared NULL.
fn any_all(op: &BinaryOperator, x: Value, arr: Value, left: &Expr, any: bool) -> Result<Value> {
    use crate::composed::{Composed, Item};
    let (ty, c) = match arr {
        Value::Null => return Ok(Value::Null),
        Value::Composed(ty, c) => (ty, c),
        other => {
            return Err(Error::new(format!(
                "op ANY/ALL (array) requires array on right side, not {}",
                kind_name(&other)
            )))
        }
    };
    let ColumnType::Composed(r) = ty else {
        unreachable!()
    };
    let (Composed::Array { element, .. } | Composed::Vector { element }) = r.def() else {
        return Err(Error::new(
            "op ANY/ALL (array) requires array on right side",
        ));
    };
    let Item::Array { elements, .. } = &c.item else {
        return Ok(Value::Null);
    };
    compared_each(
        op,
        x,
        elements.iter().map(|e| part_value(*element, e)),
        left,
        any,
    )
}

/// `x op ANY` or `x op ALL` over values: each compared with `x`, true where one (or each) holds,
/// NULL where none (or not every one) holds and one compared NULL; over none, false (or true).
pub(super) fn compared_each(
    op: &BinaryOperator,
    x: Value,
    values: impl Iterator<Item = Value>,
    left: &Expr,
    any: bool,
) -> Result<Value> {
    let mut unknown = false;
    for v in values {
        let (a, b) = beside(x.clone(), v, left, &Expr::value(SqlValue::Null))?;
        let t = match compare(&a, &b)? {
            None => None,
            Some(o) => Some(match op {
                BinaryOperator::Eq => o == Ordering::Equal,
                BinaryOperator::NotEq => o != Ordering::Equal,
                BinaryOperator::Lt => o == Ordering::Less,
                BinaryOperator::LtEq => o != Ordering::Greater,
                BinaryOperator::Gt => o == Ordering::Greater,
                BinaryOperator::GtEq => o != Ordering::Less,
                other => return Err(Error::new(format!("the operator {other} is not read"))),
            }),
        };
        match t {
            Some(true) if any => return Ok(Value::Bool(true)),
            Some(false) if !any => return Ok(Value::Bool(false)),
            None => unknown = true,
            _ => {}
        }
    }
    Ok(if unknown {
        Value::Null
    } else {
        Value::Bool(!any)
    })
}

/// Whether a question tests a composite column for NULL, which PostgreSQL answers by its fields: a
/// pipeline reads it, a row at a time.
pub fn tests_composite_null(catalog: &crate::catalog::Catalog, q: &Query) -> bool {
    let SetExpr::Select(s) = q.body.as_ref() else {
        return false;
    };
    let Some(w) = &s.selection else {
        return false;
    };
    let tables: Vec<&crate::catalog::Table> = s
        .from
        .iter()
        .filter_map(|f| match &f.relation {
            sqlparser::ast::TableFactor::Table { name, .. } => {
                let n = crate::catalog::object_name(name);
                catalog.tables.iter().find(|t| t.name == n)
            }
            _ => None,
        })
        .collect();
    let composite = |e: &Expr| {
        column_of(e).is_some_and(|(_, n)| {
            tables.iter().any(|t| {
                t.column(&n).is_some_and(|c| {
                    matches!(t.columns[c].ty, ColumnType::Composed(r) if matches!(r.def(), crate::composed::Composed::Record { .. }))
                })
            })
        })
    };
    let mut found = false;
    let mut stack = vec![w];
    while let Some(e) = stack.pop() {
        match e {
            Expr::IsNull(x) | Expr::IsNotNull(x) => found |= composite(x),
            Expr::BinaryOp { left, right, .. } => {
                stack.push(left);
                stack.push(right);
            }
            Expr::Nested(x) | Expr::UnaryOp { expr: x, .. } => stack.push(x),
            _ => {}
        }
    }
    found
}

/// A value tested for NULL as PostgreSQL tests it: a composite by its fields, NULL where every one
/// is, and NOT NULL only where none is.
fn null_test(v: &Value, not: bool) -> bool {
    if let Value::Composed(_, c) = v {
        if let crate::composed::Item::Record(fields) = &c.item {
            return if not {
                fields.iter().all(|f| !f.is_null())
            } else {
                fields.iter().all(|f| f.is_null())
            };
        }
    }
    if not {
        !v.is_null()
    } else {
        v.is_null()
    }
}
