//! The composed types: arrays, composites and a table's row type, ranges and multiranges, and enums.
//!
//! A column of one names its definition by a `TypeRef`, so that `ColumnType` stays a small value
//! passed by copy; each definition is held once, the same one for every column that names it.
//!
//! A composed value is held as an `Item`, its parts the element type's values, and keyed as bytes
//! compared byte by byte: each part's key in turn, a part of varying length ended, a value that has
//! ended first. Its page form is a Parquet group or list of its parts, each part a leaf of its own:
//! an array its elements as one list in storage order, its dimensions and lower bounds beside them
//! where it is not one-dimensional from 1; a composite a leaf a field; a range its two bounds in the
//! subtype's own form and its flags; a multirange a list of its ranges. An enum is its label, keyed
//! by its rank among the labels its DDL declares.

use crate::catalog::ColumnType;
use crate::read::statement::Datum;
use crate::{Error, Result};
use std::sync::{Arc, OnceLock, RwLock};

/// A composed type's definition, by its place among those held.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TypeRef(u32);

impl std::fmt::Debug for TypeRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.def().pg_name())
    }
}

/// One field of a composite: its name, its type, and the domain it names, whose constraints its
/// values meet.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Field {
    pub name: String,
    pub ty: ColumnType,
    pub domain: Option<String>,
}

impl Field {
    /// A field of a type, naming no domain.
    pub fn new(name: &str, ty: ColumnType) -> Field {
        Field {
            name: name.into(),
            ty,
            domain: None,
        }
    }
}

/// A composed type.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Composed {
    /// An array of `element`, whose values meet `domain`'s constraints where it names one.
    Array {
        element: ColumnType,
        domain: Option<String>,
    },
    /// A composite the DDL declares, or a table's row type: its fields in order.
    Record { name: String, fields: Vec<Field> },
    /// A range of `subtype`; `discrete` where PostgreSQL makes each value canonical, its bounds
    /// written `[lower,upper)`, as int4range's, int8range's and daterange's canonical functions do.
    Range {
        name: String,
        subtype: ColumnType,
        discrete: bool,
    },
    /// A multirange of the range `range`.
    Multirange { name: String, range: ColumnType },
    /// An enum: its labels in the order its DDL gives them.
    Enum { name: String, labels: Vec<String> },
}

fn held() -> &'static RwLock<Vec<&'static Composed>> {
    static HELD: OnceLock<RwLock<Vec<&'static Composed>>> = OnceLock::new();
    HELD.get_or_init(|| RwLock::new(Vec::new()))
}

impl TypeRef {
    /// The definition's reference: the one held already where it is the same.
    pub fn of(c: Composed) -> TypeRef {
        let lock = held();
        if let Some(i) = lock.read().unwrap().iter().position(|d| **d == c) {
            return TypeRef(i as u32);
        }
        let mut all = lock.write().unwrap();
        if let Some(i) = all.iter().position(|d| **d == c) {
            return TypeRef(i as u32);
        }
        all.push(Box::leak(Box::new(c)));
        TypeRef(all.len() as u32 - 1)
    }

    pub fn def(self) -> &'static Composed {
        held().read().unwrap()[self.0 as usize]
    }
}

/// A composed column type from its definition.
pub fn type_of(c: Composed) -> ColumnType {
    ColumnType::Composed(TypeRef::of(c))
}

/// The range types PostgreSQL has built in over the element types the snail holds, each with its
/// multirange: their names, the subtype, and whether each value is made canonical.
pub const BUILT_IN_RANGES: [(&str, &str, ColumnType, bool); 6] = [
    ("int4range", "int4multirange", ColumnType::Int4, true),
    ("int8range", "int8multirange", ColumnType::Int8, true),
    (
        "numrange",
        "nummultirange",
        ColumnType::NumericText(None),
        false,
    ),
    (
        "tstzrange",
        "tstzmultirange",
        ColumnType::Timestamptz,
        false,
    ),
    ("daterange", "datemultirange", ColumnType::Date, true),
    (
        "tsrange",
        "tsmultirange",
        ColumnType::Timestamp(None),
        false,
    ),
];

/// A range type built in, by its name or its multirange's.
pub fn built_in(name: &str) -> Option<ColumnType> {
    let name = name.strip_prefix("pg_catalog.").unwrap_or(name);
    BUILT_IN_RANGES
        .iter()
        .find_map(|&(r, m, subtype, discrete)| {
            let range = type_of(Composed::Range {
                name: r.into(),
                subtype,
                discrete,
            });
            if name == r {
                Some(range)
            } else if name == m {
                Some(type_of(Composed::Multirange {
                    name: m.into(),
                    range,
                }))
            } else {
                None
            }
        })
}

impl Composed {
    /// The type as PostgreSQL's `format_type` names it.
    pub fn pg_name(&self) -> String {
        match self {
            Composed::Array {
                domain: Some(d), ..
            } => format!("{d}[]"),
            Composed::Array { element, .. } => format!("{}[]", element.pg_name()),
            Composed::Record { name, .. }
            | Composed::Range { name, .. }
            | Composed::Multirange { name, .. }
            | Composed::Enum { name, .. } => name.clone(),
        }
    }

    /// The B-tree operator class PostgreSQL takes for a key of this type where its index names none.
    pub fn default_opclass(&self) -> &'static str {
        match self {
            Composed::Array { .. } => "array_ops",
            Composed::Record { .. } => "record_ops",
            Composed::Range { .. } => "range_ops",
            Composed::Multirange { .. } => "multirange_ops",
            Composed::Enum { .. } => "enum_ops",
        }
    }
}

/// The range a multirange holds, its subtype and whether it is made canonical.
pub fn range_of(ty: ColumnType) -> Option<(ColumnType, bool)> {
    match ty {
        ColumnType::Composed(r) => match r.def() {
            Composed::Range {
                subtype, discrete, ..
            } => Some((*subtype, *discrete)),
            Composed::Multirange { range, .. } => range_of(*range),
            _ => None,
        },
        _ => None,
    }
}

/// A range's flags, as PostgreSQL's range type keeps them.
pub const EMPTY: u8 = 0x01;
pub const LB_INC: u8 = 0x02;
pub const UB_INC: u8 = 0x04;
pub const LB_INF: u8 = 0x08;
pub const UB_INF: u8 = 0x10;

/// A value of a composed type, or a part of one.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Item {
    Null,
    /// A value of an element type: its column's own form, an enum's its rank.
    Scalar(Datum),
    /// An array's elements in storage order, the last dimension's fastest; each dimension's length
    /// and lower bound, none for an array with no elements.
    Array {
        elements: Vec<Item>,
        dims: Vec<i32>,
        lower: Vec<i32>,
    },
    /// A composite's fields in order.
    Record(Vec<Item>),
    /// A range: its flags, and each bound it holds.
    Range {
        flags: u8,
        lower: Option<Box<Item>>,
        upper: Option<Box<Item>>,
    },
    /// A multirange's ranges, each an `Item::Range`, in order, none empty.
    Multirange(Vec<Item>),
}

impl Item {
    pub fn is_null(&self) -> bool {
        matches!(self, Item::Null)
    }

    /// Whether PostgreSQL's IS NULL holds of it: a NULL, or a composite whose every field is NULL.
    pub fn is_null_as_row(&self) -> bool {
        match self {
            Item::Null => true,
            Item::Record(fields) => fields.iter().all(Item::is_null),
            _ => false,
        }
    }
}

/// A composed value held whole: its parts, and its key, by which it is compared, grouped and
/// hashed. Its key is also written out as text, two hex digits a byte, which sorts as the key does.
#[derive(Clone, Debug)]
pub struct Compound {
    pub item: Item,
    pub key: Box<[u8]>,
    pub hex: Box<str>,
}

impl Compound {
    pub fn new(ty: ColumnType, item: Item) -> Compound {
        let key = key(ty, &item).into_boxed_slice();
        let hex = hex(&key).into_boxed_str();
        Compound { item, key, hex }
    }
}

impl PartialEq for Compound {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl Eq for Compound {}

impl std::hash::Hash for Compound {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.key.hash(state)
    }
}

impl PartialOrd for Compound {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Compound {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.key.cmp(&other.key)
    }
}

/// Bytes as text, two hex digits a byte.
pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// A key of 64 bits, written so that its bytes sort as the key does: its sign bit flipped,
/// big-endian.
fn key64(out: &mut Vec<u8>, k: i64) {
    out.extend_from_slice(&((k as u64) ^ (1 << 63)).to_be_bytes());
}

fn key128(out: &mut Vec<u8>, k: i128) {
    out.extend_from_slice(&((k as u128) ^ (1 << 127)).to_be_bytes());
}

fn key32(out: &mut Vec<u8>, k: i32) {
    out.extend_from_slice(&((k as u32) ^ (1 << 31)).to_be_bytes());
}

/// Bytes of varying length, ended: each 0x00 written 0x00 0xFF, then 0x00 0x00.
fn ended(out: &mut Vec<u8>, b: &[u8]) {
    for &x in b {
        out.push(x);
        if x == 0 {
            out.push(0xFF);
        }
    }
    out.extend_from_slice(&[0, 0]);
}

/// A value of an element type's key, as bytes.
fn scalar_key(out: &mut Vec<u8>, ty: ColumnType, d: &Datum) {
    match d {
        Datum::Int(v) => key64(out, *v),
        Datum::Wide(v) => key128(out, *v),
        Datum::Float4(b) => key64(out, crate::scalars::float4_key(*b)),
        Datum::Float8(b) => key64(out, crate::scalars::float8_key(*b)),
        Datum::Text(s) => ended(out, &crate::strings::key_bytes(ty, s.as_bytes())),
        Datum::Bytes(b) => ended(out, b),
        Datum::Interval(v) => key128(out, v.total()),
        Datum::Compound(c) => out.extend_from_slice(&c.key),
        Datum::Null => {
            debug_assert!(false, "a NULL of {} keyed", ty.pg_name());
        }
    }
}

/// A value's key: the bytes it is compared by, as PostgreSQL's comparison function for its type
/// orders it.
pub fn key(ty: ColumnType, item: &Item) -> Vec<u8> {
    let mut out = Vec::new();
    key_into(&mut out, ty, item);
    out
}

fn key_into(out: &mut Vec<u8>, ty: ColumnType, item: &Item) {
    let def = match ty {
        ColumnType::Composed(r) => Some(r.def()),
        _ => None,
    };
    match (def, item) {
        (_, Item::Null) => {}
        (_, Item::Scalar(d)) => scalar_key(out, ty, d),
        (
            Some(Composed::Array { element, .. }),
            Item::Array {
                elements,
                dims,
                lower,
            },
        ) => {
            for e in elements {
                if e.is_null() {
                    out.push(2);
                } else {
                    out.push(1);
                    key_into(out, *element, e);
                }
            }
            out.push(0);
            out.push(dims.len() as u8);
            for &d in dims {
                key32(out, d);
            }
            for &l in lower {
                key32(out, l);
            }
        }
        (Some(Composed::Record { fields, .. }), Item::Record(values)) => {
            for (f, v) in fields.iter().zip(values) {
                if v.is_null() {
                    out.push(2);
                } else {
                    out.push(1);
                    key_into(out, f.ty, v);
                }
            }
        }
        (
            Some(Composed::Range { subtype, .. }),
            Item::Range {
                flags,
                lower,
                upper,
            },
        ) => range_key(out, *subtype, *flags, lower.as_deref(), upper.as_deref()),
        (Some(Composed::Multirange { range, .. }), Item::Multirange(ranges)) => {
            let subtype = range_of(*range).map_or(ColumnType::Int4, |r| r.0);
            for r in ranges {
                if let Item::Range {
                    flags,
                    lower,
                    upper,
                } = r
                {
                    out.push(1);
                    bounds_key(out, subtype, *flags, lower.as_deref(), upper.as_deref());
                }
            }
            out.push(0);
        }
        (_, other) => {
            debug_assert!(false, "{other:?} is not a value of {}", ty.pg_name());
        }
    }
}

fn range_key(
    out: &mut Vec<u8>,
    subtype: ColumnType,
    flags: u8,
    lo: Option<&Item>,
    hi: Option<&Item>,
) {
    if flags & EMPTY != 0 {
        out.push(0);
        return;
    }
    out.push(1);
    bounds_key(out, subtype, flags, lo, hi);
}

/// A range's bounds: an unbounded lower before every value, an inclusive lower before an
/// exclusive one at the same value; an exclusive upper before an inclusive one, an unbounded upper
/// after every value.
fn bounds_key(
    out: &mut Vec<u8>,
    subtype: ColumnType,
    flags: u8,
    lo: Option<&Item>,
    hi: Option<&Item>,
) {
    match lo {
        Some(v) if flags & LB_INF == 0 => {
            out.push(1);
            key_into(out, subtype, v);
            out.push(if flags & LB_INC != 0 { 0 } else { 1 });
        }
        _ => out.push(0),
    }
    match hi {
        Some(v) if flags & UB_INF == 0 => {
            out.push(1);
            key_into(out, subtype, v);
            out.push(if flags & UB_INC != 0 { 1 } else { 0 });
        }
        _ => out.push(2),
    }
}

/// White space as PostgreSQL's scanner and C's `isspace` see it.
pub(crate) fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// An integer of `bits` bits read as PostgreSQL's `pg_strtoint` functions read it: white space
/// either side, a sign, decimal digits or `0x`, `0o` and `0b` with theirs, an underscore between
/// two digits.
pub fn int_in(s: &str, bits: u32) -> Result<i64> {
    let name = match bits {
        16 => "smallint",
        32 => "integer",
        _ => "bigint",
    };
    let invalid = || Error::new(format!("invalid input syntax for type {name}: \"{s}\""));
    let range = || Error::new(format!("value \"{s}\" is out of range for type {name}"));
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && is_space(b[i]) {
        i += 1;
    }
    let mut neg = false;
    if i < b.len() && (b[i] == b'-' || b[i] == b'+') {
        neg = b[i] == b'-';
        i += 1;
    }
    let (radix, digits_at) = match (b.get(i), b.get(i + 1)) {
        (Some(b'0'), Some(b'x' | b'X')) => (16, i + 2),
        (Some(b'0'), Some(b'o' | b'O')) => (8, i + 2),
        (Some(b'0'), Some(b'b' | b'B')) => (2, i + 2),
        _ => (10, i),
    };
    let limit: u128 = 1u128 << (bits - 1);
    let mut tmp: u128 = 0;
    i = digits_at;
    let digit = |c: u8| (c as char).to_digit(radix);
    loop {
        match b.get(i) {
            Some(&c) if digit(c).is_some() => {
                tmp = tmp * radix as u128 + digit(c).unwrap() as u128;
                if tmp > limit {
                    return Err(range());
                }
                i += 1;
            }
            Some(b'_') => {
                if radix == 10 && i == digits_at {
                    return Err(invalid());
                }
                i += 1;
                if !b.get(i).is_some_and(|&c| digit(c).is_some()) {
                    return Err(invalid());
                }
            }
            _ => break,
        }
    }
    if i == digits_at {
        return Err(invalid());
    }
    while i < b.len() && is_space(b[i]) {
        i += 1;
    }
    if i != b.len() {
        return Err(invalid());
    }
    if neg {
        Ok((tmp as i128).wrapping_neg() as i64)
    } else if tmp == limit {
        Err(range())
    } else {
        Ok(tmp as i64)
    }
}

/// A value of an element type read from its text, as the type's input function reads it.
pub fn scalar_in(ty: ColumnType, s: &str) -> Result<Datum> {
    Ok(match ty {
        ColumnType::Int2 => Datum::Int(int_in(s, 16)?),
        ColumnType::Int4 => Datum::Int(int_in(s, 32)?),
        ColumnType::Int8 => Datum::Int(int_in(s, 64)?),
        ColumnType::Boolean => Datum::Int(
            crate::read::truth(s)
                .ok_or_else(|| {
                    Error::new(format!("invalid input syntax for type boolean: \"{s}\""))
                })?
                .into(),
        ),
        ColumnType::Timestamptz => Datum::Int(crate::read::instant(s)?),
        ColumnType::Text(n) => crate::read::fill::assign(
            Datum::Text(s.to_string()),
            ColumnType::Text(None),
            ColumnType::Text(n),
            "",
        )?,
        ColumnType::Bytes => Datum::Bytes(crate::strings::bytea_in(s)?),
        ColumnType::Bpchar(n) => Datum::Text(crate::strings::bpchar_in(s, n, false)?),
        ColumnType::Name => Datum::Text(crate::strings::name_in(s)),
        ColumnType::Numeric { .. } | ColumnType::NumericText(_) => {
            crate::read::fill::numeric_datum(crate::numeric::Numeric::parse(s)?, ty)?
        }
        ColumnType::Uuid => Datum::Wide(crate::scalars::uuid_parse(s)?),
        ColumnType::Float4 => Datum::Float4(crate::scalars::float_parse(s, false)? as u32),
        ColumnType::Float8 => Datum::Float8(crate::scalars::float_parse(s, true)?),
        ColumnType::Composed(r) => match r.def() {
            Composed::Enum { name, labels } => {
                Datum::Int(labels.iter().position(|l| l == s).ok_or_else(|| {
                    Error::new(format!("invalid input value for enum {name}: \"{s}\""))
                })? as i64)
            }
            _ => Datum::Compound(Arc::new(Compound::new(ty, text_in(ty, s)?))),
        },
        // A type held as its text, by its input.
        ColumnType::Textual(t) => Datum::Text(crate::textual::text_in(t, s)?),
        ColumnType::Geometric(k) => Datum::Bytes(crate::geo::geo_bytes(&crate::geo::geo_in(k, s)?)),
        ColumnType::Interval { range, precision } => Datum::Interval(crate::times::interval_in(
            s,
            range,
            precision.map(u32::from),
        )?),
        ColumnType::Bit(n) => Datum::Bytes(crate::strings::bits_parse(s, Some(n), false)?),
        ColumnType::Varbit(n) => Datum::Bytes(crate::strings::bits_parse(s, n, true)?),
        ColumnType::TimestamptzRounded(_) => Datum::Int(crate::scalars::scalar_parse(ty, s)?),
        ty if crate::scalars::is_scalar(ty) => Datum::Int(crate::scalars::scalar_parse(ty, s)?),
        other => {
            return Err(Error::new(format!(
                "{} is not read as a part of a composed value",
                other.pg_name()
            )))
        }
    })
}

/// A value of an element type as its type's output function writes it.
pub fn scalar_out(ty: ColumnType, d: &Datum) -> String {
    match (ty, d) {
        (_, Datum::Compound(c)) => text_out(ty, &c.item),
        (ColumnType::Boolean, Datum::Int(v)) => if *v != 0 { "t" } else { "f" }.into(),
        (ColumnType::Textual(t), Datum::Text(s)) => crate::textual::text_out(t, s).into_owned(),
        (ColumnType::Geometric(k), Datum::Bytes(b)) => crate::geo::geo_of_bytes(k, b)
            .map(|g| crate::geo::geo_out(&g))
            .unwrap_or_default(),
        (ColumnType::Composed(r), Datum::Int(k)) => match r.def() {
            Composed::Enum { labels, .. } => labels[*k as usize].clone(),
            _ => k.to_string(),
        },
        _ => crate::read::fill::text_of(d, ty).unwrap_or_default(),
    }
}

/// A part of a composed value from a value of its element type: held whole where the element is
/// composed itself.
fn part(ty: ColumnType, d: Datum) -> Item {
    match (ty, d) {
        (_, Datum::Null) => Item::Null,
        (ColumnType::Composed(r), Datum::Compound(c))
            if !matches!(r.def(), Composed::Enum { .. }) =>
        {
            c.item.clone()
        }
        (_, d) => Item::Scalar(d),
    }
}

/// A part of a composed value as a value of its element type.
pub fn datum_of(ty: ColumnType, item: &Item) -> Datum {
    match item {
        Item::Null => Datum::Null,
        Item::Scalar(d) => d.clone(),
        other => Datum::Compound(Arc::new(Compound::new(ty, other.clone()))),
    }
}

fn malformed(kind: &str, s: &str, why: &str) -> Error {
    Error::new(format!("malformed {kind} literal: \"{s}\" ({why})"))
}

/// A composed value read from its text, as its type's input function reads it.
pub fn text_in(ty: ColumnType, s: &str) -> Result<Item> {
    let ColumnType::Composed(r) = ty else {
        return Err(Error::new(format!(
            "{} is not a composed type",
            ty.pg_name()
        )));
    };
    match r.def() {
        Composed::Array { element, .. } => array_in(*element, s),
        Composed::Record { fields, .. } => record_in(fields, s),
        Composed::Range {
            subtype, discrete, ..
        } => range_in(*subtype, *discrete, s),
        Composed::Multirange { range, .. } => multirange_in(*range, s),
        Composed::Enum { .. } => Ok(Item::Scalar(scalar_in(ty, s)?)),
    }
}

/// An array's text read as `array_in` reads it: its dimensions written `[l:u]` before `=`, or found
/// from its braces; each element unquoted, its white space taken off either side, or quoted; an
/// unquoted NULL in any case a NULL; a backslash before any character that character.
fn array_in(element: ColumnType, s: &str) -> Result<Item> {
    let b = s.as_bytes();
    let delim = delimiter(element);
    let bad = |why: &str| malformed("array", s, why);
    let mut p = 0;
    let mut dims: Vec<i32> = Vec::new();
    let mut lower: Vec<i32> = Vec::new();
    loop {
        while p < b.len() && is_space(b[p]) {
            p += 1;
        }
        if b.get(p) != Some(&b'[') {
            break;
        }
        p += 1;
        if dims.len() >= 6 {
            return Err(Error::new(
                "number of array dimensions exceeds the maximum allowed (6)",
            ));
        }
        let read_int = |p: &mut usize| -> Result<Option<i64>> {
            let start = *p;
            if *p < b.len() && (b[*p] == b'-' || b[*p] == b'+') {
                *p += 1;
            }
            while *p < b.len() && b[*p].is_ascii_digit() {
                *p += 1;
            }
            if *p == start || (*p == start + 1 && !b[start].is_ascii_digit()) {
                *p = start;
                return Ok(None);
            }
            let v: i64 = s[start..*p]
                .parse()
                .map_err(|_| Error::new("array bound is out of integer range"))?;
            if i32::try_from(v).is_err() {
                return Err(Error::new("array bound is out of integer range"));
            }
            Ok(Some(v))
        };
        let first = read_int(&mut p)?
            .ok_or_else(|| bad("\"[\" must introduce explicitly-specified array dimensions."))?;
        let (lb, ub) = if b.get(p) == Some(&b':') {
            p += 1;
            let ub = read_int(&mut p)?.ok_or_else(|| bad("Missing array dimension value."))?;
            (first, ub)
        } else {
            (1, first)
        };
        if b.get(p) != Some(&b']') {
            return Err(bad("Missing \"]\" after array dimensions."));
        }
        p += 1;
        if ub < lb {
            return Err(Error::new("upper bound cannot be less than lower bound"));
        }
        if ub == i32::MAX as i64 {
            return Err(Error::new(format!("array upper bound is too large: {ub}")));
        }
        lower.push(lb as i32);
        dims.push((ub - lb + 1) as i32);
    }
    let specified = !dims.is_empty();
    if specified {
        if b.get(p) != Some(&b'=') {
            return Err(bad("Missing \"=\" after array dimensions."));
        }
        p += 1;
        while p < b.len() && is_space(b[p]) {
            p += 1;
        }
        if b.get(p) != Some(&b'{') {
            return Err(bad("Array contents must start with \"{\"."));
        }
    } else if b.get(p) != Some(&b'{') {
        return Err(bad(
            "Array value must start with \"{\" or dimension information.",
        ));
    }
    let mut found: Vec<i32> = if specified { dims.clone() } else { Vec::new() };
    let mut ndim = dims.len();
    let mut frozen = specified;
    let mut nelems = [0i32; 6];
    let mut level = 0usize;
    let mut expect_delim = false;
    let mut elements: Vec<Item> = Vec::new();
    let dimension_error = || {
        if specified {
            bad("Specified array dimensions do not match array contents.")
        } else {
            bad("Multidimensional arrays must have sub-arrays with matching dimensions.")
        }
    };
    let end = || bad("Unexpected end of input.");
    loop {
        // The next token.
        while p < b.len() && is_space(b[p]) && b[p] != delim {
            p += 1;
        }
        let Some(&c) = b.get(p) else {
            return Err(end());
        };
        match c {
            b'{' => {
                p += 1;
                if expect_delim {
                    return Err(bad("Unexpected \"{\" character."));
                }
                if level >= 6 {
                    return Err(Error::new(
                        "number of array dimensions exceeds the maximum allowed (6)",
                    ));
                }
                nelems[level] = 0;
                level += 1;
                if level > ndim {
                    if frozen {
                        return Err(dimension_error());
                    }
                    ndim = level;
                    found.resize(ndim, -1);
                }
            }
            b'}' => {
                p += 1;
                if nelems[level - 1] > 0 && !expect_delim {
                    return Err(bad("Unexpected \"}\" character."));
                }
                level -= 1;
                if level > 0 {
                    nelems[level - 1] += 1;
                }
                if found[level] < 0 {
                    found[level] = nelems[level];
                } else if nelems[level] != found[level] {
                    return Err(dimension_error());
                }
                expect_delim = true;
                if level == 0 {
                    break;
                }
            }
            c if c == delim => {
                p += 1;
                if !expect_delim {
                    return Err(bad(&format!("Unexpected \"{}\" character.", delim as char)));
                }
                expect_delim = false;
            }
            _ => {
                // An element, quoted or not.
                let mut buf: Vec<u8> = Vec::new();
                let mut null = false;
                if c == b'"' {
                    p += 1;
                    loop {
                        match b.get(p) {
                            None => return Err(end()),
                            Some(b'\\') => {
                                p += 1;
                                let x = *b.get(p).ok_or_else(end)?;
                                buf.push(x);
                                p += 1;
                            }
                            Some(b'"') => {
                                p += 1;
                                loop {
                                    match b.get(p) {
                                        None => return Err(end()),
                                        Some(&x) if x == delim || x == b'}' || x == b'{' => break,
                                        Some(&x) if is_space(x) => p += 1,
                                        Some(_) => {
                                            return Err(bad("Incorrectly quoted array element."))
                                        }
                                    }
                                }
                                break;
                            }
                            Some(&x) => {
                                buf.push(x);
                                p += 1;
                            }
                        }
                    }
                } else {
                    let mut kept = 0;
                    let mut escaped = false;
                    loop {
                        match b.get(p) {
                            None => return Err(end()),
                            Some(b'{') => return Err(bad("Unexpected \"{\" character.")),
                            Some(b'"') => return Err(bad("Incorrectly quoted array element.")),
                            Some(b'\\') => {
                                p += 1;
                                let x = *b.get(p).ok_or_else(end)?;
                                buf.push(x);
                                kept = buf.len();
                                escaped = true;
                                p += 1;
                            }
                            Some(&x) if x == delim || x == b'}' => break,
                            Some(&x) => {
                                buf.push(x);
                                if !is_space(x) {
                                    kept = buf.len();
                                }
                                p += 1;
                            }
                        }
                    }
                    buf.truncate(kept);
                    null = !escaped && buf.eq_ignore_ascii_case(b"NULL");
                }
                if expect_delim {
                    return Err(bad("Unexpected array element."));
                }
                let text = String::from_utf8(buf).map_err(|_| bad("an element is not UTF-8"))?;
                elements.push(if null {
                    Item::Null
                } else {
                    part(element, scalar_in(element, &text)?)
                });
                frozen = true;
                if level != ndim {
                    return Err(dimension_error());
                }
                nelems[level - 1] += 1;
                expect_delim = true;
            }
        }
    }
    while p < b.len() {
        if !is_space(b[p]) {
            return Err(bad("Junk after closing right brace."));
        }
        p += 1;
    }
    if elements.is_empty() {
        return Ok(Item::Array {
            elements,
            dims: Vec::new(),
            lower: Vec::new(),
        });
    }
    let lower = if specified { lower } else { vec![1; ndim] };
    Ok(Item::Array {
        elements,
        dims: found,
        lower,
    })
}

/// A composite's text read as `record_in` reads it: its fields between parentheses, each as written
/// or quoted, a doubled quote inside quotes a quote, a backslash before any character that
/// character; a field written as nothing a NULL.
fn record_in(fields: &[Field], s: &str) -> Result<Item> {
    let b = s.as_bytes();
    let bad = |why: &str| malformed("record", s, why);
    let mut p = 0;
    while p < b.len() && is_space(b[p]) {
        p += 1;
    }
    if b.get(p) != Some(&b'(') {
        return Err(bad("Missing left parenthesis."));
    }
    p += 1;
    let mut values = Vec::with_capacity(fields.len());
    for (i, f) in fields.iter().enumerate() {
        if i > 0 {
            if b.get(p) == Some(&b',') {
                p += 1;
            } else {
                return Err(bad("Too few columns."));
            }
        }
        if matches!(b.get(p), Some(b',' | b')')) {
            values.push(Item::Null);
            continue;
        }
        let mut buf: Vec<u8> = Vec::new();
        let mut quoted = false;
        loop {
            let Some(&c) = b.get(p) else {
                return Err(bad("Unexpected end of input."));
            };
            if !quoted && (c == b',' || c == b')') {
                break;
            }
            p += 1;
            match c {
                b'\\' => {
                    let x = *b.get(p).ok_or_else(|| bad("Unexpected end of input."))?;
                    buf.push(x);
                    p += 1;
                }
                b'"' if !quoted => quoted = true,
                b'"' if b.get(p) == Some(&b'"') => {
                    buf.push(b'"');
                    p += 1;
                }
                b'"' => quoted = false,
                _ => buf.push(c),
            }
        }
        let text = String::from_utf8(buf).map_err(|_| bad("a field is not UTF-8"))?;
        values.push(part(f.ty, scalar_in(f.ty, &text)?));
    }
    if b.get(p) != Some(&b')') {
        return Err(bad("Too many columns."));
    }
    p += 1;
    while p < b.len() && is_space(b[p]) {
        p += 1;
    }
    if p != b.len() {
        return Err(bad("Junk after right parenthesis."));
    }
    Ok(Item::Record(values))
}

/// One bound of a range's text, as `range_parse_bound` reads it, and where its reading stopped:
/// none where it is written as nothing.
fn range_bound(b: &[u8], mut p: usize, s: &str) -> Result<(Option<String>, usize)> {
    if matches!(b.get(p), Some(b',' | b')' | b']')) {
        return Ok((None, p));
    }
    let end = || malformed("range", s, "Unexpected end of input.");
    let mut buf: Vec<u8> = Vec::new();
    let mut quoted = false;
    loop {
        let Some(&c) = b.get(p) else {
            return Err(end());
        };
        if !quoted && matches!(c, b',' | b')' | b']') {
            break;
        }
        p += 1;
        match c {
            b'\\' => {
                buf.push(*b.get(p).ok_or_else(end)?);
                p += 1;
            }
            b'"' if !quoted => quoted = true,
            b'"' if b.get(p) == Some(&b'"') => {
                buf.push(b'"');
                p += 1;
            }
            b'"' => quoted = false,
            _ => buf.push(c),
        }
    }
    let text = String::from_utf8(buf).map_err(|_| malformed("range", s, "a bound is not UTF-8"))?;
    Ok((Some(text), p))
}

/// A range's text read as `range_in` reads it, then made as `make_range` makes it.
fn range_in(subtype: ColumnType, discrete: bool, s: &str) -> Result<Item> {
    let b = s.as_bytes();
    let bad = |why: &str| malformed("range", s, why);
    let mut p = 0;
    while p < b.len() && is_space(b[p]) {
        p += 1;
    }
    if b.len() >= p + 5 && b[p..p + 5].eq_ignore_ascii_case(b"empty") {
        p += 5;
        while p < b.len() && is_space(b[p]) {
            p += 1;
        }
        if p != b.len() {
            return Err(bad("Junk after \"empty\" key word."));
        }
        return Ok(Item::Range {
            flags: EMPTY,
            lower: None,
            upper: None,
        });
    }
    let mut flags = 0u8;
    match b.get(p) {
        Some(b'[') => flags |= LB_INC,
        Some(b'(') => {}
        _ => return Err(bad("Missing left parenthesis or bracket.")),
    }
    p += 1;
    let (lo, at) = range_bound(b, p, s)?;
    p = at;
    if b.get(p) != Some(&b',') {
        return Err(bad("Missing comma after lower bound."));
    }
    p += 1;
    let (hi, at) = range_bound(b, p, s)?;
    p = at;
    match b.get(p) {
        Some(b']') => flags |= UB_INC,
        Some(b')') => {}
        _ => return Err(bad("Too many commas.")),
    }
    p += 1;
    while p < b.len() && is_space(b[p]) {
        p += 1;
    }
    if p != b.len() {
        return Err(bad("Junk after right parenthesis or bracket."));
    }
    let lo = lo
        .map(|t| scalar_in(subtype, &t).map(|d| part(subtype, d)))
        .transpose()?;
    let hi = hi
        .map(|t| scalar_in(subtype, &t).map(|d| part(subtype, d)))
        .transpose()?;
    make_range(subtype, discrete, flags, lo, hi)
}

/// A range made as PostgreSQL's `make_range` makes it: a lower bound above its upper refused, equal
/// bounds not both inclusive empty, an infinite bound never inclusive; then, for a discrete range,
/// each bound written `[lower,upper)`.
pub fn make_range(
    subtype: ColumnType,
    discrete: bool,
    mut flags: u8,
    lo: Option<Item>,
    hi: Option<Item>,
) -> Result<Item> {
    if flags & EMPTY != 0 {
        return Ok(Item::Range {
            flags: EMPTY,
            lower: None,
            upper: None,
        });
    }
    if lo.is_none() {
        flags = (flags | LB_INF) & !LB_INC;
    }
    if hi.is_none() {
        flags = (flags | UB_INF) & !UB_INC;
    }
    let serialize = |flags: u8, lo: Option<Item>, hi: Option<Item>| -> Result<Item> {
        if let (Some(l), Some(h)) = (&lo, &hi) {
            let (kl, kh) = (key(subtype, l), key(subtype, h));
            if kl > kh {
                return Err(Error::new(
                    "range lower bound must be less than or equal to range upper bound",
                ));
            }
            if kl == kh && !(flags & LB_INC != 0 && flags & UB_INC != 0) {
                return Ok(Item::Range {
                    flags: EMPTY,
                    lower: None,
                    upper: None,
                });
            }
        }
        Ok(Item::Range {
            flags,
            lower: lo.map(Box::new),
            upper: hi.map(Box::new),
        })
    };
    let made = serialize(flags, lo, hi)?;
    if !discrete {
        return Ok(made);
    }
    let Item::Range {
        flags,
        lower,
        upper,
    } = made
    else {
        unreachable!()
    };
    if flags & EMPTY != 0 {
        return Ok(Item::Range {
            flags,
            lower,
            upper,
        });
    }
    // The value after a bound's: an integer's next, a date's next day; none for an infinite date,
    // which keeps its bound as it is.
    let next = |v: &Item| -> Result<Option<Item>> {
        let Item::Scalar(Datum::Int(x)) = v else {
            return Err(Error::new("a discrete range's bound is not an integer"));
        };
        Ok(Some(Item::Scalar(Datum::Int(match subtype {
            ColumnType::Date if *x == i64::from(i32::MIN) || *x == i64::from(i32::MAX) => {
                return Ok(None)
            }
            ColumnType::Date => i64::from(crate::times::date_pli(*x as i32, 1)?),
            ColumnType::Int4 if *x == i64::from(i32::MAX) => {
                return Err(Error::new("integer out of range"))
            }
            _ if *x == i64::MAX => return Err(Error::new("bigint out of range")),
            _ => x + 1,
        }))))
    };
    let mut f = flags;
    let lo = match lower {
        Some(l) if f & LB_INC == 0 => match next(&l)? {
            Some(n) => {
                f |= LB_INC;
                Some(n)
            }
            None => Some(*l),
        },
        l => l.map(|b| *b),
    };
    let hi = match upper {
        Some(h) if f & UB_INC != 0 => match next(&h)? {
            Some(n) => {
                f &= !UB_INC;
                Some(n)
            }
            None => Some(*h),
        },
        h => h.map(|b| *b),
    };
    serialize(f, lo, hi)
}

/// A multirange's text read as `multirange_in` reads it, then made canonical: its ranges sorted,
/// the empty ones dropped, those that overlap or meet merged.
fn multirange_in(range: ColumnType, s: &str) -> Result<Item> {
    let b = s.as_bytes();
    let bad = |why: &str| malformed("multirange", s, why);
    let mut p = 0;
    while p < b.len() && is_space(b[p]) {
        p += 1;
    }
    if b.get(p) != Some(&b'{') {
        return Err(bad("Missing left brace."));
    }
    p += 1;
    let mut ranges: Vec<Item> = Vec::new();
    let mut seen = 0;
    // 0 before a range, 1 in one, 2 escaped in one, 3 quoted in one, 4 escaped in quotes, 5 after.
    let mut state = 0;
    let mut begin = 0;
    loop {
        let Some(&c) = b.get(p) else {
            return Err(bad("Unexpected end of input."));
        };
        if is_space(c) {
            p += 1;
            continue;
        }
        match state {
            0 => {
                if c == b'[' || c == b'(' {
                    begin = p;
                    state = 1;
                } else if c == b'}' && seen == 0 {
                    p += 1;
                    break;
                } else if b.len() >= p + 5 && b[p..p + 5].eq_ignore_ascii_case(b"empty") {
                    seen += 1;
                    p += 4;
                    state = 5;
                } else {
                    return Err(bad("Expected range start."));
                }
            }
            1 => {
                if c == b']' || c == b')' {
                    seen += 1;
                    let r = text_in(range, &s[begin..=p])?;
                    if !matches!(r, Item::Range { flags, .. } if flags & EMPTY != 0) {
                        ranges.push(r);
                    }
                    state = 5;
                } else if c == b'"' {
                    state = 3;
                } else if c == b'\\' {
                    state = 2;
                }
            }
            2 => state = 1,
            3 => {
                if c == b'"' {
                    if b.get(p + 1) == Some(&b'"') {
                        p += 1;
                    } else {
                        state = 1;
                    }
                } else if c == b'\\' {
                    state = 4;
                }
            }
            4 => state = 3,
            _ => {
                if c == b',' {
                    state = 0;
                } else if c == b'}' {
                    p += 1;
                    break;
                } else {
                    return Err(bad("Expected comma or end of multirange."));
                }
            }
        }
        p += 1;
    }
    while p < b.len() && is_space(b[p]) {
        p += 1;
    }
    if p != b.len() {
        return Err(bad("Junk after closing right brace."));
    }
    Ok(Item::Multirange(canonical_ranges(range, ranges)))
}

/// A range's upper bound as a key of its own, comparable with another range's.
fn upper_key(subtype: ColumnType, r: &Item) -> Vec<u8> {
    let mut out = Vec::new();
    if let Item::Range { flags, upper, .. } = r {
        bounds_key(&mut out, subtype, *flags | LB_INF, None, upper.as_deref());
        out.remove(0);
    }
    out
}

/// A range's lower bound as a key of its own, comparable with another range's.
fn lower_key(subtype: ColumnType, r: &Item) -> Vec<u8> {
    let mut out = Vec::new();
    if let Item::Range { flags, lower, .. } = r {
        bounds_key(&mut out, subtype, *flags | UB_INF, lower.as_deref(), None);
    }
    out
}

/// Whether range `b`, which starts no earlier than `a`, overlaps `a` or meets it with no value
/// between: its lower bound at or below `a`'s upper, or at the same value, one of the two inclusive.
fn joins(subtype: ColumnType, a: &Item, b: &Item) -> bool {
    let (
        Item::Range {
            flags: fa,
            upper: ua,
            ..
        },
        Item::Range {
            flags: fb,
            lower: lb,
            ..
        },
    ) = (a, b)
    else {
        return false;
    };
    if fa & UB_INF != 0 || fb & LB_INF != 0 {
        return true;
    }
    let (Some(u), Some(l)) = (ua, lb) else {
        return true;
    };
    let (ku, kl) = (key(subtype, u), key(subtype, l));
    ku > kl || (ku == kl && (fa & UB_INC != 0 || fb & LB_INC != 0))
}

/// Ranges sorted, the empty ones dropped, and those that overlap or meet merged, as
/// `multirange_canonicalize` makes them: a merged range's bound the later range's where the two
/// are equal, as `range_union_internal` takes it.
pub fn canonical_ranges(range: ColumnType, ranges: Vec<Item>) -> Vec<Item> {
    let Some((subtype, _)) = range_of(range) else {
        return ranges;
    };
    let mut ranges: Vec<Item> = ranges
        .into_iter()
        .filter(|r| !matches!(r, Item::Range { flags, .. } if flags & EMPTY != 0))
        .collect();
    ranges.sort_by_key(|r| key(range, r));
    let mut out: Vec<Item> = Vec::new();
    for r in ranges {
        match out.last_mut() {
            Some(last) if joins(subtype, last, &r) => {
                let later_upper = upper_key(subtype, &r) >= upper_key(subtype, last);
                let same_lower = lower_key(subtype, &r) == lower_key(subtype, last);
                let (
                    Item::Range {
                        flags: fl,
                        lower: ll,
                        upper: ul,
                    },
                    Item::Range {
                        flags: fr,
                        lower: lr,
                        upper: ur,
                    },
                ) = (last, r)
                else {
                    unreachable!()
                };
                if later_upper {
                    *ul = ur;
                    *fl = (*fl & !(UB_INC | UB_INF)) | (fr & (UB_INC | UB_INF));
                }
                if same_lower {
                    *ll = lr;
                }
            }
            _ => out.push(r),
        }
    }
    out
}

/// Whether a part's text must be quoted in an array: empty, NULL in any case, or holding a quote, a
/// backslash, a brace, a comma or white space.
fn array_quoted(t: &str, delim: u8) -> bool {
    t.is_empty()
        || t.eq_ignore_ascii_case("NULL")
        || t.bytes()
            .any(|c| matches!(c, b'"' | b'\\' | b'{' | b'}') || c == delim || is_space(c))
}

/// The character an array of a type writes between its elements: box's `;`, every other's `,`.
fn delimiter(element: ColumnType) -> u8 {
    if element == ColumnType::Geometric(crate::geo::GeoKind::Box) {
        b';'
    } else {
        b','
    }
}

/// A part's text quoted where it must be: a quote and a backslash each doubled inside a record's or
/// a range's quotes, each escaped by a backslash inside an array's.
fn quoted(t: &str, doubled: bool) -> String {
    let mut out = String::with_capacity(t.len() + 2);
    out.push('"');
    for c in t.chars() {
        if c == '"' || c == '\\' {
            out.push(if doubled { c } else { '\\' });
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// A composed value as its type's output function writes it.
pub fn text_out(ty: ColumnType, item: &Item) -> String {
    let ColumnType::Composed(r) = ty else {
        return match item {
            Item::Scalar(d) => scalar_out(ty, d),
            _ => String::new(),
        };
    };
    let part_text = |t: ColumnType, i: &Item| match i {
        Item::Scalar(d) => scalar_out(t, d),
        other => text_out(t, other),
    };
    match (r.def(), item) {
        (_, Item::Null) => String::new(),
        (Composed::Enum { .. }, Item::Scalar(d)) => scalar_out(ty, d),
        (
            Composed::Array { element, .. },
            Item::Array {
                elements,
                dims,
                lower,
            },
        ) => {
            if elements.is_empty() {
                return "{}".into();
            }
            let mut out = String::new();
            if lower.iter().any(|&l| l != 1) {
                for (d, l) in dims.iter().zip(lower) {
                    out.push_str(&format!("[{}:{}]", l, l + d - 1));
                }
                out.push('=');
            }
            let texts: Vec<String> = elements
                .iter()
                .map(|e| {
                    if e.is_null() {
                        "NULL".to_string()
                    } else {
                        let t = part_text(*element, e);
                        if array_quoted(&t, delimiter(*element)) {
                            quoted(&t, false)
                        } else {
                            t
                        }
                    }
                })
                .collect();
            nest(&mut out, &texts, dims, delimiter(*element) as char);
            out
        }
        (Composed::Record { fields, .. }, Item::Record(values)) => {
            let mut out = String::from("(");
            for (i, (f, v)) in fields.iter().zip(values).enumerate() {
                if i > 0 {
                    out.push(',');
                }
                if v.is_null() {
                    continue;
                }
                let t = part_text(f.ty, v);
                let q = t.is_empty()
                    || t.bytes()
                        .any(|c| matches!(c, b'"' | b'\\' | b'(' | b')' | b',') || is_space(c));
                out.push_str(&if q { quoted(&t, true) } else { t });
            }
            out.push(')');
            out
        }
        (
            Composed::Range { subtype, .. },
            Item::Range {
                flags,
                lower,
                upper,
            },
        ) => range_text(*subtype, *flags, lower.as_deref(), upper.as_deref()),
        (Composed::Multirange { range, .. }, Item::Multirange(ranges)) => {
            let subtype = range_of(*range).map_or(ColumnType::Int4, |r| r.0);
            let texts: Vec<String> = ranges
                .iter()
                .map(|r| match r {
                    Item::Range {
                        flags,
                        lower,
                        upper,
                    } => range_text(subtype, *flags, lower.as_deref(), upper.as_deref()),
                    _ => String::new(),
                })
                .collect();
            format!("{{{}}}", texts.join(","))
        }
        (_, other) => format!("{other:?}"),
    }
}

/// An array's element texts in braces, a pair of them a dimension, the last dimension innermost.
fn nest(out: &mut String, texts: &[String], dims: &[i32], delim: char) {
    match dims {
        [] | [_] => {
            out.push('{');
            out.push_str(&texts.join(&delim.to_string()));
            out.push('}');
        }
        [first, rest @ ..] => {
            let step = texts.len() / (*first).max(1) as usize;
            out.push('{');
            for (i, chunk) in texts.chunks(step.max(1)).enumerate() {
                if i > 0 {
                    out.push(delim);
                }
                nest(out, chunk, rest, delim);
            }
            out.push('}');
        }
    }
}

fn range_text(subtype: ColumnType, flags: u8, lo: Option<&Item>, hi: Option<&Item>) -> String {
    if flags & EMPTY != 0 {
        return "empty".into();
    }
    let bound = |b: Option<&Item>| match b {
        Some(v) => {
            let t = match v {
                Item::Scalar(d) => scalar_out(subtype, d),
                other => text_out(subtype, other),
            };
            let q = t.is_empty()
                || t.bytes().any(|c| {
                    matches!(c, b'"' | b'\\' | b'(' | b')' | b'[' | b']' | b',') || is_space(c)
                });
            if q {
                quoted(&t, true)
            } else {
                t
            }
        }
        None => String::new(),
    };
    format!(
        "{}{},{}{}",
        if flags & LB_INC != 0 { '[' } else { '(' },
        if flags & LB_INF != 0 {
            String::new()
        } else {
            bound(lo)
        },
        if flags & UB_INF != 0 {
            String::new()
        } else {
            bound(hi)
        },
        if flags & UB_INC != 0 { ']' } else { ')' },
    )
}

/// The name of the one leaf a composite of no fields holds.
pub const NO_FIELDS: &str = "none";

/// One node of a composed column's Parquet type tree.
#[derive(Clone, Debug)]
enum Node {
    /// A value of an element type.
    Leaf {
        name: String,
        ty: ColumnType,
        /// The bits of the unsigned number it holds, 0 where it holds none.
        bits: u32,
        optional: bool,
        /// Whether the cut passes over it: an array's dimensions and lower bounds.
        thin: bool,
    },
    /// A group of fields, each a node of its own.
    Group {
        name: String,
        optional: bool,
        children: Vec<Node>,
    },
    /// A list, written as Parquet's LIST: a group holding a repeated group `list` holding its
    /// `element`.
    List {
        name: String,
        optional: bool,
        element: Box<Node>,
    },
}

impl Node {
    /// The tree of a value of type `ty` named `name`.
    fn of(name: &str, ty: ColumnType, optional: bool) -> Node {
        let leaf = |name: &str, ty: ColumnType, optional: bool, thin: bool| Node::Leaf {
            name: name.into(),
            ty,
            bits: ty.unsigned_bits(),
            optional,
            thin,
        };
        let def = match ty {
            ColumnType::Composed(r) => r.def(),
            // A value held as a group of leaves, a tid, a timetz, an interval or a bit varying: a group
            // of its own.
            _ if ty.leaf_count() > 1 => {
                return Node::Group {
                    name: name.into(),
                    optional,
                    children: ty
                        .leaves()
                        .into_iter()
                        .map(|l| Node::Leaf {
                            name: l.field.unwrap_or_default().into(),
                            ty: l.ty,
                            bits: l.bits,
                            optional: false,
                            thin: false,
                        })
                        .collect(),
                }
            }
            _ => return leaf(name, ty, optional, false),
        };
        match def {
            Composed::Array { element, .. } => Node::Group {
                name: name.into(),
                optional,
                children: vec![
                    Node::List {
                        name: "elements".into(),
                        optional: false,
                        element: Box::new(Node::of("element", *element, true)),
                    },
                    Node::List {
                        name: "dims".into(),
                        optional: true,
                        element: Box::new(leaf("element", ColumnType::Int4, false, true)),
                    },
                    Node::List {
                        name: "lower".into(),
                        optional: true,
                        element: Box::new(leaf("element", ColumnType::Int4, false, true)),
                    },
                ],
            },
            // A composite of no fields holds one leaf, a boolean always NULL, which carries the
            // composite's own NULL in its levels: Parquet has no group of no fields.
            Composed::Record { fields, .. } if fields.is_empty() => Node::Group {
                name: name.into(),
                optional,
                children: vec![leaf(NO_FIELDS, ColumnType::Boolean, true, false)],
            },
            Composed::Record { fields, .. } => Node::Group {
                name: name.into(),
                optional,
                children: fields
                    .iter()
                    .map(|f| Node::of(&f.name, f.ty, true))
                    .collect(),
            },
            Composed::Range { subtype, .. } => Node::Group {
                name: name.into(),
                optional,
                children: vec![
                    Node::of("lower", *subtype, true),
                    Node::of("upper", *subtype, true),
                    leaf("flags", ColumnType::Int4, false, false),
                ],
            },
            Composed::Multirange { range, .. } => Node::List {
                name: name.into(),
                optional,
                element: Box::new(Node::of("element", *range, false)),
            },
            Composed::Enum { .. } => leaf(name, ty, optional, false),
        }
    }

    fn optional(&self) -> bool {
        match self {
            Node::Leaf { optional, .. }
            | Node::Group { optional, .. }
            | Node::List { optional, .. } => *optional,
        }
    }

    /// How many leaves lie under it.
    fn leaf_count(&self) -> usize {
        match self {
            Node::Leaf { .. } => 1,
            Node::Group { children, .. } => children.iter().map(Node::leaf_count).sum(),
            Node::List { element, .. } => element.leaf_count(),
        }
    }

    /// Its leaves in order: each one's path below the column, its type and whether the cut passes
    /// over it.
    fn leaves(
        &self,
        path: &mut Vec<String>,
        out: &mut Vec<(Vec<String>, ColumnType, u32, bool, bool)>,
        top: bool,
        listed: bool,
    ) {
        let name = match self {
            Node::Leaf { name, .. } | Node::Group { name, .. } | Node::List { name, .. } => name,
        };
        if !top {
            path.push(name.clone());
        }
        match self {
            Node::Leaf { ty, bits, thin, .. } => {
                out.push((path.clone(), *ty, *bits, *thin, listed))
            }
            Node::Group { children, .. } => {
                for c in children {
                    c.leaves(path, out, false, listed);
                }
            }
            Node::List { element, .. } => {
                path.push("list".into());
                element.leaves(path, out, false, true);
                path.pop();
            }
        }
        if !top {
            path.pop();
        }
    }

    /// Its Parquet type.
    fn parquet(&self) -> Result<parquet::schema::types::Type> {
        use parquet::basic::{LogicalType, Repetition};
        use parquet::schema::types::Type;
        let rep = |optional: bool| {
            if optional {
                Repetition::OPTIONAL
            } else {
                Repetition::REQUIRED
            }
        };
        Ok(match self {
            Node::Leaf {
                name,
                ty,
                bits,
                optional,
                ..
            } => crate::encode::primitive(name, *ty, *bits, rep(*optional))?,
            Node::Group {
                name,
                optional,
                children,
            } => Type::group_type_builder(name)
                .with_repetition(rep(*optional))
                .with_fields(
                    children
                        .iter()
                        .map(|c| c.parquet().map(Arc::new))
                        .collect::<Result<Vec<_>>>()?,
                )
                .build()?,
            Node::List {
                name,
                optional,
                element,
            } => Type::group_type_builder(name)
                .with_repetition(rep(*optional))
                .with_logical_type(Some(LogicalType::List))
                .with_fields(vec![Arc::new(
                    Type::group_type_builder("list")
                        .with_repetition(Repetition::REPEATED)
                        .with_fields(vec![Arc::new(element.parquet()?)])
                        .build()?,
                )])
                .build()?,
        })
    }
}

/// A composed value laid out on its Parquet type tree.
#[derive(Clone, Debug)]
enum Tree {
    Null,
    Leaf(Datum),
    Group(Vec<Tree>),
    List(Vec<Tree>),
}

/// A value of an element type held as a group of leaves, on its leaves: a tid's block number and
/// offset, each keyed in its own bits; a timetz's time and zone; an interval's months, days and
/// microseconds; a bit varying's length and bits; an aclitem's grantee, grantor and privileges, each
/// keyed in its own bits; a geometric value's float8s.
fn grouped(ty: ColumnType, d: &Datum) -> Tree {
    let int = |v: i64| Tree::Leaf(Datum::Int(v));
    match (ty, d) {
        (ColumnType::Tid, Datum::Int(k)) => {
            let (b, o) = crate::scalars::tid_parts(*k);
            Tree::Group(vec![
                int(crate::scalars::unsigned_key(u64::from(b), 32)),
                int(crate::scalars::unsigned_key(u64::from(o), 16)),
            ])
        }
        (ColumnType::Timetz(_), Datum::Int(k)) => {
            let (t, z) = crate::scalars::timetz_parts(*k);
            Tree::Group(vec![int(t), int(i64::from(z))])
        }
        (ColumnType::Interval { .. }, Datum::Interval(v)) => Tree::Group(vec![
            int(i64::from(v.months)),
            int(i64::from(v.days)),
            int(v.micros),
        ]),
        (ColumnType::Varbit(_), Datum::Bytes(k)) => {
            let (len, bits) = crate::strings::bits_of_key(k);
            Tree::Group(vec![int(i64::from(len)), Tree::Leaf(Datum::Bytes(bits))])
        }
        (ColumnType::Textual(crate::textual::Textual::Aclitem), Datum::Text(s)) => {
            match crate::textual::aclitem_parse(s) {
                Ok((grantee, grantor, privileges)) => Tree::Group(vec![
                    int(crate::scalars::unsigned_key(u64::from(grantee), 32)),
                    int(crate::scalars::unsigned_key(u64::from(grantor), 32)),
                    int(crate::scalars::unsigned_key(privileges, 64)),
                ]),
                Err(_) => Tree::Null,
            }
        }
        (ColumnType::Geometric(_), Datum::Bytes(b)) => Tree::Group(
            b.chunks(8)
                .map(|x| Tree::Leaf(Datum::Float8(u64::from_be_bytes(x.try_into().unwrap()))))
                .collect(),
        ),
        _ => Tree::Null,
    }
}

/// A value of an element type held as a group of leaves, from its leaves.
fn ungrouped(ty: ColumnType, parts: &[Tree]) -> Option<Datum> {
    use Tree::Leaf;
    Some(match (ty, parts) {
        (ColumnType::Tid, [Leaf(Datum::Int(b)), Leaf(Datum::Int(o))]) => {
            Datum::Int(crate::scalars::tid_key(
                crate::scalars::unsigned_of(*b, 32) as u32,
                crate::scalars::unsigned_of(*o, 16) as u16,
            ))
        }
        (ColumnType::Timetz(_), [Leaf(Datum::Int(t)), Leaf(Datum::Int(z))]) => {
            Datum::Int(crate::scalars::timetz_key(*t, *z as i32))
        }
        (
            ColumnType::Interval { .. },
            [Leaf(Datum::Int(m)), Leaf(Datum::Int(d)), Leaf(Datum::Int(u))],
        ) => Datum::Interval(crate::times::Interval {
            months: *m as i32,
            days: *d as i32,
            micros: *u,
        }),
        (ColumnType::Varbit(_), [Leaf(Datum::Int(len)), Leaf(Datum::Bytes(bits))]) => {
            Datum::Bytes(crate::strings::bits_key(*len as u32, bits))
        }
        (
            ColumnType::Textual(crate::textual::Textual::Aclitem),
            [Leaf(Datum::Int(grantee)), Leaf(Datum::Int(grantor)), Leaf(Datum::Int(privileges))],
        ) => Datum::Text(crate::textual::aclitem_text(
            crate::scalars::unsigned_of(*grantee, 32) as u32,
            crate::scalars::unsigned_of(*grantor, 32) as u32,
            crate::scalars::unsigned_of(*privileges, 64),
        )),
        (ColumnType::Geometric(_), parts) => {
            let mut b = Vec::with_capacity(8 * parts.len());
            for p in parts {
                let Leaf(Datum::Float8(x)) = p else {
                    return None;
                };
                b.extend_from_slice(&x.to_be_bytes());
            }
            Datum::Bytes(b)
        }
        _ => return None,
    })
}

/// A value of type `ty` on its type's tree.
fn tree_of(ty: ColumnType, item: &Item) -> Tree {
    let def = match ty {
        ColumnType::Composed(r) => Some(r.def()),
        _ => None,
    };
    match (def, item) {
        (_, Item::Null) => Tree::Null,
        (None, Item::Scalar(d)) if ty.leaf_count() > 1 => grouped(ty, d),
        (_, Item::Scalar(d)) => Tree::Leaf(d.clone()),
        (
            Some(Composed::Array { element, .. }),
            Item::Array {
                elements,
                dims,
                lower,
            },
        ) => {
            // The shape is written only where the array is not one-dimensional from 1.
            let plain = dims.len() <= 1 && lower.iter().all(|&l| l == 1);
            let ints = |v: &[i32]| {
                Tree::List(
                    v.iter()
                        .map(|&x| Tree::Leaf(Datum::Int(x as i64)))
                        .collect(),
                )
            };
            Tree::Group(vec![
                Tree::List(elements.iter().map(|e| tree_of(*element, e)).collect()),
                if plain { Tree::Null } else { ints(dims) },
                if plain { Tree::Null } else { ints(lower) },
            ])
        }
        (Some(Composed::Record { fields, .. }), Item::Record(_)) if fields.is_empty() => {
            Tree::Group(vec![Tree::Null])
        }
        (Some(Composed::Record { fields, .. }), Item::Record(values)) => Tree::Group(
            fields
                .iter()
                .zip(values)
                .map(|(f, v)| tree_of(f.ty, v))
                .collect(),
        ),
        (
            Some(Composed::Range { subtype, .. }),
            Item::Range {
                flags,
                lower,
                upper,
            },
        ) => Tree::Group(vec![
            lower
                .as_deref()
                .map_or(Tree::Null, |v| tree_of(*subtype, v)),
            upper
                .as_deref()
                .map_or(Tree::Null, |v| tree_of(*subtype, v)),
            Tree::Leaf(Datum::Int(*flags as i64)),
        ]),
        (Some(Composed::Multirange { range, .. }), Item::Multirange(ranges)) => {
            Tree::List(ranges.iter().map(|r| tree_of(*range, r)).collect())
        }
        _ => Tree::Null,
    }
}

/// A value of type `ty` from its type's tree.
fn item_of(ty: ColumnType, t: Tree) -> Item {
    let def = match ty {
        ColumnType::Composed(r) => Some(r.def()),
        _ => None,
    };
    match (def, t) {
        (_, Tree::Null) => Item::Null,
        (None, Tree::Group(parts)) => ungrouped(ty, &parts).map_or(Item::Null, Item::Scalar),
        (_, Tree::Leaf(d)) => Item::Scalar(d),
        (Some(Composed::Array { element, .. }), Tree::Group(parts)) => {
            let mut parts = parts.into_iter();
            let elements: Vec<Item> = match parts.next() {
                Some(Tree::List(v)) => v.into_iter().map(|e| item_of(*element, e)).collect(),
                _ => Vec::new(),
            };
            let ints = |t: Option<Tree>| match t {
                Some(Tree::List(v)) => Some(
                    v.into_iter()
                        .map(|x| match x {
                            Tree::Leaf(Datum::Int(n)) => n as i32,
                            _ => 0,
                        })
                        .collect::<Vec<i32>>(),
                ),
                _ => None,
            };
            let (dims, lower) = match (ints(parts.next()), ints(parts.next())) {
                (Some(d), Some(l)) => (d, l),
                _ if elements.is_empty() => (Vec::new(), Vec::new()),
                _ => (vec![elements.len() as i32], vec![1]),
            };
            Item::Array {
                elements,
                dims,
                lower,
            }
        }
        (Some(Composed::Record { fields, .. }), Tree::Group(parts)) => Item::Record(
            fields
                .iter()
                .zip(parts)
                .map(|(f, p)| item_of(f.ty, p))
                .collect(),
        ),
        (Some(Composed::Range { subtype, .. }), Tree::Group(parts)) => {
            let mut parts = parts.into_iter();
            let lo = parts.next().map(|p| item_of(*subtype, p));
            let hi = parts.next().map(|p| item_of(*subtype, p));
            let flags = match parts.next() {
                Some(Tree::Leaf(Datum::Int(f))) => f as u8,
                _ => 0,
            };
            let bound = |b: Option<Item>| b.filter(|i| !i.is_null()).map(Box::new);
            Item::Range {
                flags,
                lower: bound(lo),
                upper: bound(hi),
            }
        }
        (Some(Composed::Multirange { range, .. }), Tree::List(v)) => {
            Item::Multirange(v.into_iter().map(|r| item_of(*range, r)).collect())
        }
        _ => Item::Null,
    }
}

/// One leaf's levels and values as a column's rows are striped onto it: a value an entry, NULL where
/// the entry holds none; row `r`'s entries from `starts[r]` to `starts[r + 1]`.
#[derive(Clone, Debug, Default)]
struct Stripe {
    defs: Vec<i16>,
    reps: Vec<i16>,
    values: Vec<Option<Datum>>,
    starts: Vec<u32>,
}

impl Stripe {
    fn push(&mut self, def: i16, rep: i16, v: Option<Datum>) {
        self.defs.push(def);
        self.reps.push(rep);
        self.values.push(v);
    }
}

/// Writes one value onto the stripes of the leaves under `n`, from leaf `at` on: `def` the level its
/// parent defines, `rep` the repetition level of its first entry and `depth` its parent's.
fn shred(n: &Node, t: &Tree, def: i16, rep: i16, depth: i16, out: &mut [Stripe], at: usize) {
    let opt = n.optional() as i16;
    let none = |out: &mut [Stripe], def: i16| {
        for s in &mut out[at..at + n.leaf_count()] {
            s.push(def, rep, None);
        }
    };
    match (n, t) {
        (Node::Leaf { .. }, Tree::Leaf(d)) if !matches!(d, Datum::Null) => {
            out[at].push(def + opt, rep, Some(d.clone()))
        }
        (Node::Group { children, .. }, Tree::Group(parts)) => {
            let mut a = at;
            for (c, p) in children.iter().zip(parts) {
                shred(c, p, def + opt, rep, depth, out, a);
                a += c.leaf_count();
            }
        }
        (Node::List { element, .. }, Tree::List(items)) if !items.is_empty() => {
            for (i, e) in items.iter().enumerate() {
                let r = if i == 0 { rep } else { depth + 1 };
                shred(element, e, def + opt + 1, r, depth + 1, out, at);
            }
        }
        (Node::List { .. }, Tree::List(_)) => none(out, def + opt),
        _ => none(out, def),
    }
}

/// Reads one value off the stripes of the leaves under `n`, from leaf `at` on, each stripe's next
/// entry at `cur`.
fn assemble(
    n: &Node,
    def: i16,
    depth: i16,
    stripes: &[Stripe],
    cur: &mut [usize],
    at: usize,
) -> Tree {
    let opt = n.optional() as i16;
    let d = stripes[at].defs[cur[at]];
    let skip = |cur: &mut [usize]| {
        for c in &mut cur[at..at + n.leaf_count()] {
            *c += 1;
        }
    };
    if d < def + opt {
        skip(cur);
        return Tree::Null;
    }
    match n {
        Node::Leaf { .. } => {
            let v = stripes[at].values[cur[at]].clone();
            cur[at] += 1;
            v.map_or(Tree::Null, Tree::Leaf)
        }
        Node::Group { children, .. } => {
            let mut a = at;
            let mut parts = Vec::with_capacity(children.len());
            for c in children {
                parts.push(assemble(c, def + opt, depth, stripes, cur, a));
                a += c.leaf_count();
            }
            Tree::Group(parts)
        }
        Node::List { element, .. } => {
            if d < def + opt + 1 {
                skip(cur);
                return Tree::List(Vec::new());
            }
            let mut items = Vec::new();
            loop {
                items.push(assemble(
                    element,
                    def + opt + 1,
                    depth + 1,
                    stripes,
                    cur,
                    at,
                ));
                let s = &stripes[at];
                if cur[at] < s.reps.len() && s.reps[cur[at]] == depth + 1 {
                    continue;
                }
                break;
            }
            Tree::List(items)
        }
    }
}

/// What a composed type's leaves are, held once a type.
struct Shape {
    node: Node,
    leaves: Vec<crate::catalog::Leaf>,
    paths: Vec<Vec<String>>,
}

fn shape_of(r: TypeRef) -> &'static Shape {
    static SHAPES: OnceLock<RwLock<std::collections::HashMap<TypeRef, &'static Shape>>> =
        OnceLock::new();
    let lock = SHAPES.get_or_init(Default::default);
    if let Some(s) = lock.read().unwrap().get(&r) {
        return s;
    }
    let ty = ColumnType::Composed(r);
    let node = Node::of("v", ty, true);
    let mut found = Vec::new();
    node.leaves(&mut Vec::new(), &mut found, true, false);
    let paths: Vec<Vec<String>> = found.iter().map(|f| f.0.clone()).collect();
    let leaves = found
        .into_iter()
        .enumerate()
        .map(|(i, (path, lt, bits, thin, listed))| {
            let field: &'static str = Box::leak(path.join(".").into_boxed_str());
            crate::catalog::Leaf::nested(field, lt, bits, r, i as u16, thin, listed)
        })
        .collect();
    let shape: &'static Shape = Box::leak(Box::new(Shape {
        node,
        leaves,
        paths,
    }));
    lock.write().unwrap().entry(r).or_insert(shape)
}

/// An enum's labels, in their order.
pub fn labels(ty: ColumnType) -> Option<&'static [String]> {
    match ty {
        ColumnType::Composed(r) => match r.def() {
            Composed::Enum { labels, .. } => Some(labels),
            _ => None,
        },
        _ => None,
    }
}

/// Whether a composed type is held as a tree of leaves, each with its levels, rather than as one
/// flat leaf: every one but an enum.
pub fn is_nested(r: TypeRef) -> bool {
    !matches!(r.def(), Composed::Enum { .. })
}

/// The path of leaf `i` of a composed column below the column.
pub fn leaf_path(r: TypeRef, i: usize) -> &'static [String] {
    &shape_of(r).paths[i]
}

/// The leaves of a composed column's Parquet type tree, in order.
pub fn leaves(r: TypeRef) -> &'static [crate::catalog::Leaf] {
    &shape_of(r).leaves
}

/// A composed column's Parquet type, named `name`, OPTIONAL where it takes NULL.
pub fn parquet_type(
    name: &str,
    ty: ColumnType,
    nullable: bool,
) -> Result<parquet::schema::types::Type> {
    Node::of(name, ty, nullable).parquet()
}

/// The descriptor of leaf `index` of a composed column named `column`.
pub fn leaf_descriptor(
    column: &str,
    r: TypeRef,
    index: usize,
    nullable: bool,
) -> Result<parquet::schema::types::ColumnDescPtr> {
    use parquet::schema::types::{SchemaDescriptor, Type};
    let _ = nullable;
    let t = parquet_type(column, ColumnType::Composed(r), true)?;
    let root = Type::group_type_builder("schema")
        .with_fields(vec![Arc::new(t)])
        .build()?;
    let schema = SchemaDescriptor::new(Arc::new(root));
    Ok(schema.column(index))
}

/// A leaf's values as its column's rows are striped onto it: one an entry of its levels, NULL where
/// the entry holds none, and the entry each row starts at.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct Nested {
    pub values: crate::values::Values,
    pub defs: Vec<i16>,
    pub reps: Vec<i16>,
    /// Where each row's entries start, and one past the last.
    pub starts: Vec<u32>,
}

impl Nested {
    pub fn rows(&self) -> usize {
        self.starts.len().saturating_sub(1)
    }

    /// The entries of row `r`.
    pub fn entries(&self, r: usize) -> std::ops::Range<usize> {
        self.starts[r] as usize..self.starts[r + 1] as usize
    }

    /// The rows given, in that order.
    pub fn gather(&self, rows: &[usize]) -> Nested {
        let picked: Vec<usize> = rows.iter().flat_map(|&r| self.entries(r)).collect();
        let mut starts = Vec::with_capacity(rows.len() + 1);
        starts.push(0u32);
        for &r in rows {
            let n = self.entries(r).len() as u32;
            starts.push(starts.last().unwrap() + n);
        }
        Nested {
            values: self.values.gather(&picked),
            defs: picked.iter().map(|&e| self.defs[e]).collect(),
            reps: if self.reps.is_empty() {
                Vec::new()
            } else {
                picked.iter().map(|&e| self.reps[e]).collect()
            },
            starts,
        }
    }

    /// Appends row `r` of `other`.
    pub fn push_from(&mut self, other: &Nested, r: usize) {
        for e in other.entries(r) {
            self.values.push_from(&other.values, e);
            self.defs.push(other.defs[e]);
            if !other.reps.is_empty() {
                self.reps.push(other.reps[e]);
            }
        }
        if self.starts.is_empty() {
            self.starts.push(0);
        }
        self.starts.push(self.defs.len() as u32);
    }

    /// No rows yet, of the kind `like` holds.
    pub fn empty(like: &Nested) -> Nested {
        Nested {
            values: crate::values::Values::new_like(&like.values),
            defs: Vec::new(),
            reps: Vec::new(),
            starts: vec![0],
        }
    }

    /// The leaf's values from its pages: a value an entry of its levels, and the levels; each row
    /// starting at an entry repeated at no level, or one row an entry where none repeats.
    pub fn of_levels(values: crate::values::Values, defs: Vec<i16>, reps: Vec<i16>) -> Nested {
        let mut starts: Vec<u32> = if reps.is_empty() {
            (0..defs.len() as u32).collect()
        } else {
            reps.iter()
                .enumerate()
                .filter(|(_, &r)| r == 0)
                .map(|(i, _)| i as u32)
                .collect()
        };
        starts.push(defs.len() as u32);
        Nested {
            values,
            defs,
            reps,
            starts,
        }
    }
}

/// A composed column's values as each of its leaves holds them.
pub fn stripe(r: TypeRef, values: &[Option<Arc<Compound>>]) -> Vec<Nested> {
    let shape = shape_of(r);
    let ty = ColumnType::Composed(r);
    let node = &shape.node;
    let mut stripes = vec![Stripe::default(); shape.leaves.len()];
    for v in values {
        for s in &mut stripes {
            s.starts.push(s.defs.len() as u32);
        }
        let t = v.as_ref().map_or(Tree::Null, |c| tree_of(ty, &c.item));
        shred(node, &t, 0, 0, 0, &mut stripes, 0);
    }
    stripes
        .into_iter()
        .zip(&shape.leaves)
        .map(|(mut s, leaf)| {
            s.starts.push(s.defs.len() as u32);
            let mut v = crate::values::Values::new(leaf.ty);
            for d in &s.values {
                crate::values::push_datum(&mut v, d.as_ref().unwrap_or(&Datum::Null));
            }
            let repeated = leaf.repeated;
            Nested {
                values: v,
                defs: s.defs,
                reps: if repeated { s.reps } else { Vec::new() },
                starts: s.starts,
            }
        })
        .collect()
}

/// A composed column's values from its leaves', each of the same rows.
pub fn assemble_rows(r: TypeRef, leaves: &[&Nested]) -> Vec<Option<Arc<Compound>>> {
    let shape = shape_of(r);
    let ty = ColumnType::Composed(r);
    let node = &shape.node;
    let stripes: Vec<Stripe> = leaves
        .iter()
        .zip(&shape.leaves)
        .map(|(l, leaf)| Stripe {
            defs: l.defs.clone(),
            reps: if l.reps.is_empty() {
                vec![0; l.defs.len()]
            } else {
                l.reps.clone()
            },
            values: (0..l.values.len())
                .map(|i| match crate::values::datum_of(&l.values, i, leaf.ty) {
                    Datum::Null => None,
                    d => Some(d),
                })
                .collect(),
            starts: l.starts.clone(),
        })
        .collect();
    let rows = leaves.first().map_or(0, |l| l.rows());
    let mut cur: Vec<usize> = vec![0; stripes.len()];
    (0..rows)
        .map(|_| match assemble(node, 0, 0, &stripes, &mut cur, 0) {
            Tree::Null => None,
            t => Some(Arc::new(Compound::new(ty, item_of(ty, t)))),
        })
        .collect()
}

/// A value of type `ty` written in a statement: quoted text its type's input reads; an array as
/// `ARRAY[…]`, a composite as `ROW(…)` or `(…)`, a range as its type's constructor, `int4range(1,
/// 5, '[]')`, a multirange as its, each part a value written out; or NULL.
pub fn literal(ty: ColumnType, e: &sqlparser::ast::Expr) -> Result<Datum> {
    use sqlparser::ast::{Expr, FunctionArg, FunctionArgExpr, FunctionArguments, Value};
    let wrong = || Error::new(format!("{e} is not a value of {}", ty.pg_name()));
    let e = match e {
        Expr::Cast {
            expr, data_type, ..
        } => {
            let named = crate::catalog::format_type(data_type);
            if named != ty.pg_name() && !matches!(&**expr, Expr::Value(_)) {
                return Err(wrong());
            }
            &**expr
        }
        Expr::Nested(x) => &**x,
        other => other,
    };
    if let Expr::Value(v) = e {
        return match &v.value {
            Value::Null => Ok(Datum::Null),
            Value::SingleQuotedString(s) => scalar_in(ty, s),
            Value::Number(s, _) if !ty.is_nested() && !matches!(ty, ColumnType::Composed(_)) => {
                scalar_in(ty, s)
            }
            Value::Boolean(b) if ty == ColumnType::Boolean => Ok(Datum::Int(*b as i64)),
            _ => Err(wrong()),
        };
    }
    if let Expr::UnaryOp {
        op: sqlparser::ast::UnaryOperator::Minus,
        expr,
    } = e
    {
        if let Expr::Value(v) = &**expr {
            if let Value::Number(s, _) = &v.value {
                return scalar_in(ty, &format!("-{s}"));
            }
        }
    }
    let def = match ty {
        ColumnType::Composed(r) => r.def(),
        _ => return Err(wrong()),
    };
    let whole = |item: Item| Ok(Datum::Compound(Arc::new(Compound::new(ty, item))));
    match (def, e) {
        (Composed::Array { element, .. }, Expr::Array(a)) => {
            // Each element written out, or an array of them a dimension deeper.
            let mut elements = Vec::new();
            let mut inner: Option<(Vec<i32>, usize)> = None;
            for x in &a.elem {
                if let Expr::Array(_) = x {
                    match literal(ty, x)? {
                        Datum::Compound(c) => match &c.item {
                            Item::Array {
                                elements: e, dims, ..
                            } => {
                                let shape = (dims.clone(), e.len());
                                if inner.as_ref().is_some_and(|i| *i != shape) {
                                    return Err(Error::new(
                                        "multidimensional arrays must have array expressions with matching dimensions",
                                    ));
                                }
                                inner = Some(shape);
                                elements.extend(e.iter().cloned());
                            }
                            _ => return Err(wrong()),
                        },
                        _ => return Err(wrong()),
                    }
                } else {
                    if inner.is_some() {
                        return Err(wrong());
                    }
                    elements.push(part(*element, literal(*element, x)?));
                }
            }
            if elements.is_empty() {
                return whole(Item::Array {
                    elements,
                    dims: Vec::new(),
                    lower: Vec::new(),
                });
            }
            let dims: Vec<i32> = match inner {
                Some((d, _)) => std::iter::once(a.elem.len() as i32).chain(d).collect(),
                None => vec![elements.len() as i32],
            };
            let lower = vec![1; dims.len()];
            whole(Item::Array {
                elements,
                dims,
                lower,
            })
        }
        (Composed::Record { fields, .. }, Expr::Tuple(xs)) => record_of(ty, fields, xs),
        (Composed::Record { fields, .. }, Expr::Function(f))
            if crate::catalog::object_name(&f.name).eq_ignore_ascii_case("row") =>
        {
            let FunctionArguments::List(list) = &f.args else {
                return record_of(ty, fields, &[]);
            };
            let xs: Vec<Expr> = list
                .args
                .iter()
                .map(|a| match a {
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(x)) => Ok(x.clone()),
                    _ => Err(wrong()),
                })
                .collect::<Result<_>>()?;
            record_of(ty, fields, &xs)
        }
        (
            Composed::Range {
                name,
                subtype,
                discrete,
            },
            Expr::Function(f),
        ) if crate::catalog::object_name(&f.name) == *name => {
            let args = function_args(f).ok_or_else(wrong)?;
            if !(2..=3).contains(&args.len()) {
                return Err(wrong());
            }
            let bound = |x: &Expr| -> Result<Option<Item>> {
                Ok(match literal(*subtype, x)? {
                    Datum::Null => None,
                    d => Some(part(*subtype, d)),
                })
            };
            let flags = match args.get(2) {
                None => LB_INC,
                Some(x) => match literal(ColumnType::Text(None), x)? {
                    Datum::Text(t) => match t.as_str() {
                        "[]" => LB_INC | UB_INC,
                        "[)" => LB_INC,
                        "(]" => UB_INC,
                        "()" => 0,
                        _ => return Err(Error::new("invalid range bound flags")),
                    },
                    _ => {
                        return Err(Error::new(
                            "range constructor flags argument must not be null",
                        ))
                    }
                },
            };
            let item = make_range(
                *subtype,
                *discrete,
                flags,
                bound(&args[0])?,
                bound(&args[1])?,
            )?;
            whole(item)
        }
        (Composed::Multirange { name, range }, Expr::Function(f))
            if crate::catalog::object_name(&f.name) == *name =>
        {
            let args = function_args(f).ok_or_else(wrong)?;
            let mut ranges = Vec::new();
            for x in &args {
                match literal(*range, x)? {
                    Datum::Compound(c) => ranges.push(c.item.clone()),
                    Datum::Null => {
                        return Err(Error::new("multirange values cannot contain null members"))
                    }
                    _ => return Err(wrong()),
                }
            }
            whole(Item::Multirange(canonical_ranges(*range, ranges)))
        }
        _ => Err(wrong()),
    }
}

/// A function's arguments, each an expression written without a name.
fn function_args(f: &sqlparser::ast::Function) -> Option<Vec<sqlparser::ast::Expr>> {
    use sqlparser::ast::{FunctionArg, FunctionArgExpr, FunctionArguments};
    match &f.args {
        FunctionArguments::List(list) => list
            .args
            .iter()
            .map(|a| match a {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(x)) => Some(x.clone()),
                _ => None,
            })
            .collect(),
        FunctionArguments::None => Some(Vec::new()),
        _ => None,
    }
}

fn record_of(ty: ColumnType, fields: &[Field], xs: &[sqlparser::ast::Expr]) -> Result<Datum> {
    if xs.len() != fields.len() {
        return Err(Error::new(format!(
            "a row of {} values for {}, a composite of {} fields",
            xs.len(),
            ty.pg_name(),
            fields.len()
        )));
    }
    let values = fields
        .iter()
        .zip(xs)
        .map(|(f, x)| literal(f.ty, x).map(|d| part(f.ty, d)))
        .collect::<Result<Vec<_>>>()?;
    Ok(Datum::Compound(Arc::new(Compound::new(
        ty,
        Item::Record(values),
    ))))
}

/// The key of an answer's field metadata that names a composed column's type by its place among
/// the types this process holds.
pub const COMPOSED_KEY: &str = "warren-snail.composed";

/// The Arrow type a value of an element type takes in an answer: what arrow's Parquet reader gives
/// its page form.
fn leaf_arrow(ty: ColumnType) -> arrow::datatypes::DataType {
    use arrow::datatypes::{DataType as A, TimeUnit};
    match ty {
        ColumnType::Int2 => A::Int16,
        ColumnType::Int4 => A::Int32,
        ColumnType::Int8 => A::Int64,
        ColumnType::Boolean => A::Boolean,
        ColumnType::Timestamptz => A::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
        ColumnType::Text(_)
        | ColumnType::Bpchar(_)
        | ColumnType::Name
        | ColumnType::NumericText(_)
        | ColumnType::Textual(_) => A::Utf8,
        ColumnType::Bytes | ColumnType::Composed(_) => A::Binary,
        ColumnType::Bit(_) => crate::strings::string_arrow(ty),
        ColumnType::Numeric { precision, scale } => A::Decimal128(precision, scale as i8),
        ColumnType::Uuid => A::FixedSizeBinary(16),
        ColumnType::Float4 => A::Float32,
        ColumnType::Float8 => A::Float64,
        ty => crate::scalars::scalar_arrow(ty),
    }
}

impl Node {
    fn name(&self) -> &str {
        match self {
            Node::Leaf { name, .. } | Node::Group { name, .. } | Node::List { name, .. } => name,
        }
    }

    fn arrow(&self) -> arrow::datatypes::DataType {
        use arrow::datatypes::{DataType as A, Field};
        match self {
            Node::Leaf { ty, bits: 16, .. } if *ty == ColumnType::Oid => {
                arrow::datatypes::DataType::UInt16
            }
            Node::Leaf { ty, .. } => leaf_arrow(*ty),
            Node::Group { children, .. } => A::Struct(
                children
                    .iter()
                    .map(|c| Field::new(c.name(), c.arrow(), c.optional()))
                    .collect::<Vec<_>>()
                    .into(),
            ),
            Node::List { element, .. } => A::List(Arc::new(Field::new(
                element.name(),
                element.arrow(),
                element.optional(),
            ))),
        }
    }

    /// The values on this node of the rows given, as an Arrow array.
    fn array(&self, trees: &[&Tree]) -> Result<arrow::array::ArrayRef> {
        use arrow::array::{ListArray, StructArray};
        use arrow::buffer::{NullBuffer, OffsetBuffer};
        let nulls = |trees: &[&Tree]| -> Option<NullBuffer> {
            trees.iter().any(|t| matches!(t, Tree::Null)).then(|| {
                NullBuffer::from(
                    trees
                        .iter()
                        .map(|t| !matches!(t, Tree::Null))
                        .collect::<Vec<_>>(),
                )
            })
        };
        Ok(match self {
            Node::Leaf { ty, bits: 16, .. } if *ty == ColumnType::Oid => Arc::new(
                trees
                    .iter()
                    .map(|t| match t {
                        Tree::Leaf(Datum::Int(k)) => {
                            Some(crate::scalars::unsigned_of(*k, 16) as u16)
                        }
                        _ => None,
                    })
                    .collect::<arrow::array::UInt16Array>(),
            ),
            Node::Leaf { ty, .. } => leaf_array(
                *ty,
                trees
                    .iter()
                    .map(|t| match t {
                        Tree::Leaf(d) => Some(d),
                        _ => None,
                    })
                    .collect(),
            )?,
            Node::Group { children, .. } => {
                let arrow::datatypes::DataType::Struct(fields) = self.arrow() else {
                    unreachable!()
                };
                let mut columns = Vec::with_capacity(children.len());
                for (i, c) in children.iter().enumerate() {
                    let parts: Vec<&Tree> = trees
                        .iter()
                        .map(|t| match t {
                            Tree::Group(v) => &v[i],
                            _ => &Tree::Null,
                        })
                        .collect();
                    columns.push(c.array(&parts)?);
                }
                Arc::new(StructArray::try_new(fields, columns, nulls(trees))?)
            }
            Node::List { element, .. } => {
                let arrow::datatypes::DataType::List(field) = self.arrow() else {
                    unreachable!()
                };
                let mut offsets: Vec<i32> = vec![0];
                let mut flat: Vec<&Tree> = Vec::new();
                for t in trees {
                    if let Tree::List(items) = t {
                        flat.extend(items.iter());
                    }
                    offsets.push(flat.len() as i32);
                }
                let values = element.array(&flat)?;
                Arc::new(ListArray::try_new(
                    field,
                    OffsetBuffer::new(offsets.into()),
                    values,
                    nulls(trees),
                )?)
            }
        })
    }

    /// The value on this node at row `r` of an Arrow array.
    fn tree_at(&self, a: &dyn arrow::array::Array, r: usize) -> Option<Tree> {
        use arrow::array::{ListArray, StructArray};
        if a.is_null(r) {
            return Some(Tree::Null);
        }
        Some(match self {
            Node::Leaf { ty, bits: 16, .. } if *ty == ColumnType::Oid => {
                Tree::Leaf(Datum::Int(crate::scalars::unsigned_key(
                    u64::from(
                        a.as_any()
                            .downcast_ref::<arrow::array::UInt16Array>()?
                            .value(r),
                    ),
                    16,
                )))
            }
            Node::Leaf { ty, .. } => Tree::Leaf(leaf_at(*ty, a, r)?),
            Node::Group { children, .. } => {
                let s = a.as_any().downcast_ref::<StructArray>()?;
                Tree::Group(
                    children
                        .iter()
                        .enumerate()
                        .map(|(i, c)| c.tree_at(s.column(i).as_ref(), r))
                        .collect::<Option<Vec<_>>>()?,
                )
            }
            Node::List { element, .. } => {
                let l = a.as_any().downcast_ref::<ListArray>()?;
                let values = l.value(r);
                Tree::List(
                    (0..values.len())
                        .map(|i| element.tree_at(values.as_ref(), i))
                        .collect::<Option<Vec<_>>>()?,
                )
            }
        })
    }
}

/// Values of an element type, as an Arrow array of the type `leaf_arrow` gives.
fn leaf_array(ty: ColumnType, values: Vec<Option<&Datum>>) -> Result<arrow::array::ArrayRef> {
    use arrow::array::{
        BinaryArray, BooleanArray, Decimal128Array, FixedSizeBinaryArray, Float32Array,
        Float64Array, Int16Array, Int32Array, Int64Array, StringArray, TimestampMicrosecondArray,
    };
    let int = |d: &Option<&Datum>| match d {
        Some(Datum::Int(v)) => Some(*v),
        _ => None,
    };
    Ok(match ty {
        ColumnType::Int2 => Arc::new(
            values
                .iter()
                .map(|d| int(d).map(|v| v as i16))
                .collect::<Int16Array>(),
        ),
        ColumnType::Int4 => Arc::new(
            values
                .iter()
                .map(|d| int(d).map(|v| v as i32))
                .collect::<Int32Array>(),
        ),
        ColumnType::Int8 => Arc::new(values.iter().map(int).collect::<Int64Array>()),
        ColumnType::Boolean => Arc::new(
            values
                .iter()
                .map(|d| int(d).map(|v| v != 0))
                .collect::<BooleanArray>(),
        ),
        ColumnType::Timestamptz => Arc::new(
            values
                .iter()
                .map(int)
                .collect::<TimestampMicrosecondArray>()
                .with_timezone("UTC"),
        ),
        ColumnType::Text(_)
        | ColumnType::Bpchar(_)
        | ColumnType::Name
        | ColumnType::NumericText(_)
        | ColumnType::Textual(_) => Arc::new(
            values
                .iter()
                .map(|d| match d {
                    Some(Datum::Text(s)) => Some(s.as_str()),
                    _ => None,
                })
                .collect::<StringArray>(),
        ),
        ColumnType::Bytes => Arc::new(
            values
                .iter()
                .map(|d| match d {
                    Some(Datum::Bytes(b)) => Some(b.as_slice()),
                    _ => None,
                })
                .collect::<BinaryArray>(),
        ),
        ColumnType::Bit(_) => crate::strings::string_array(
            ty,
            values.iter().map(|d| match d {
                Some(Datum::Bytes(b)) => Some(b.as_slice()),
                _ => None,
            }),
        ),
        ColumnType::Composed(_) => {
            let labels = labels(ty).unwrap_or(&[]);
            Arc::new(
                values
                    .iter()
                    .map(|d| int(d).map(|k| labels[k as usize].as_bytes()))
                    .collect::<BinaryArray>(),
            )
        }
        ColumnType::Numeric { precision, scale } => Arc::new(
            values
                .iter()
                .map(|d| d.and_then(Datum::wide))
                .collect::<Decimal128Array>()
                .with_precision_and_scale(precision, scale as i8)?,
        ),
        ColumnType::Uuid => Arc::new(FixedSizeBinaryArray::try_from_sparse_iter_with_size(
            values.iter().map(|d| match d {
                Some(Datum::Wide(k)) => Some(crate::scalars::uuid_bytes(*k)),
                _ => None,
            }),
            16,
        )?),
        ColumnType::Float4 => Arc::new(
            values
                .iter()
                .map(|d| match d {
                    Some(Datum::Float4(b)) => Some(f32::from_bits(*b)),
                    _ => None,
                })
                .collect::<Float32Array>(),
        ),
        ColumnType::Float8 => Arc::new(
            values
                .iter()
                .map(|d| match d {
                    Some(Datum::Float8(b)) => Some(f64::from_bits(*b)),
                    _ => None,
                })
                .collect::<Float64Array>(),
        ),
        ty => crate::scalars::scalar_array(ty, values.iter().map(int)),
    })
}

/// Row `r`'s value of an element type in an Arrow array of the type `leaf_arrow` gives.
fn leaf_at(ty: ColumnType, a: &dyn arrow::array::Array, r: usize) -> Option<Datum> {
    use arrow::array::{
        BinaryArray, BooleanArray, Decimal128Array, FixedSizeBinaryArray, Float32Array,
        Float64Array, Int16Array, Int32Array, Int64Array, StringArray, TimestampMicrosecondArray,
    };
    let any = a.as_any();
    Some(match ty {
        ColumnType::Int2 => Datum::Int(any.downcast_ref::<Int16Array>()?.value(r) as i64),
        ColumnType::Int4 => Datum::Int(any.downcast_ref::<Int32Array>()?.value(r) as i64),
        ColumnType::Int8 => Datum::Int(any.downcast_ref::<Int64Array>()?.value(r)),
        ColumnType::Boolean => Datum::Int(any.downcast_ref::<BooleanArray>()?.value(r) as i64),
        ColumnType::Timestamptz => {
            Datum::Int(any.downcast_ref::<TimestampMicrosecondArray>()?.value(r))
        }
        ColumnType::Text(_)
        | ColumnType::Bpchar(_)
        | ColumnType::Name
        | ColumnType::NumericText(_)
        | ColumnType::Textual(_) => {
            Datum::Text(any.downcast_ref::<StringArray>()?.value(r).to_string())
        }
        ColumnType::Bytes => Datum::Bytes(any.downcast_ref::<BinaryArray>()?.value(r).to_vec()),
        ColumnType::Bit(_) => Datum::Bytes(crate::strings::bits_key_at(ty, a, r)?),
        ColumnType::Composed(_) => {
            let b = any.downcast_ref::<BinaryArray>()?.value(r);
            Datum::Int(labels(ty)?.iter().position(|l| l.as_bytes() == b)? as i64)
        }
        ColumnType::Numeric { precision, .. } => {
            let v = any.downcast_ref::<Decimal128Array>()?.value(r);
            if precision > 18 {
                Datum::Wide(v)
            } else {
                Datum::Int(v as i64)
            }
        }
        ColumnType::Uuid => Datum::Wide(crate::scalars::uuid_key(
            any.downcast_ref::<FixedSizeBinaryArray>()?
                .value(r)
                .try_into()
                .ok()?,
        )),
        ColumnType::Float4 => Datum::Float4(any.downcast_ref::<Float32Array>()?.value(r).to_bits()),
        ColumnType::Float8 => Datum::Float8(any.downcast_ref::<Float64Array>()?.value(r).to_bits()),
        ty => Datum::Int(crate::scalars::scalar_key_at(ty, a, r)?),
    })
}

/// The Arrow type an answer holds a composed column as.
pub fn arrow_type(ty: ColumnType) -> arrow::datatypes::DataType {
    match ty {
        ColumnType::Composed(r) if is_nested(r) => shape_of(r).node.arrow(),
        _ => leaf_arrow(ty),
    }
}

/// An answer's field holding a composed column, named for its PostgreSQL type in its metadata.
pub fn arrow_field(name: &str, ty: ColumnType, nullable: bool) -> arrow::datatypes::Field {
    let ColumnType::Composed(r) = ty else {
        return arrow::datatypes::Field::new(name, leaf_arrow(ty), nullable);
    };
    arrow::datatypes::Field::new(name, arrow_type(ty), nullable).with_metadata(
        [
            (crate::scalars::PG_TYPE.to_string(), ty.pg_name()),
            (COMPOSED_KEY.to_string(), r.0.to_string()),
        ]
        .into_iter()
        .collect(),
    )
}

/// The composed type an answer's field holds, by its metadata.
pub fn field_composed(f: &arrow::datatypes::Field) -> Option<ColumnType> {
    let i: u32 = f.metadata().get(COMPOSED_KEY)?.parse().ok()?;
    let r = TypeRef(i);
    (held().read().unwrap().len() > i as usize
        && arrow_type(ColumnType::Composed(r)) == *f.data_type())
    .then_some(ColumnType::Composed(r))
}

/// A composed column's values as an answer holds them; an enum's ranks as its labels.
pub fn arrow_array<'a>(
    ty: ColumnType,
    values: impl Iterator<Item = Option<&'a Compound>>,
) -> Result<arrow::array::ArrayRef> {
    let values: Vec<Option<&Compound>> = values.collect();
    match ty {
        ColumnType::Composed(r) if is_nested(r) => {
            let trees: Vec<Tree> = values
                .iter()
                .map(|v| v.map_or(Tree::Null, |c| tree_of(ty, &c.item)))
                .collect();
            let refs: Vec<&Tree> = trees.iter().collect();
            shape_of(r).node.array(&refs)
        }
        _ => {
            let datums: Vec<Option<Datum>> = values
                .iter()
                .map(|v| match v.map(|c| &c.item) {
                    Some(Item::Scalar(d)) => Some(d.clone()),
                    _ => None,
                })
                .collect();
            leaf_array(ty, datums.iter().map(Option::as_ref).collect())
        }
    }
}

/// Row `r`'s value of a composed column in an answer's array.
pub fn compound_at(ty: ColumnType, a: &dyn arrow::array::Array, r: usize) -> Option<Arc<Compound>> {
    if a.is_null(r) {
        return None;
    }
    let item = match ty {
        ColumnType::Composed(t) if is_nested(t) => item_of(ty, shape_of(t).node.tree_at(a, r)?),
        _ => Item::Scalar(leaf_at(ty, a, r)?),
    };
    Some(Arc::new(Compound::new(ty, item)))
}

/// An enum column's ranks as an answer holds them: its labels.
pub fn enum_array(
    ty: ColumnType,
    ranks: impl Iterator<Item = Option<i64>>,
) -> Result<arrow::array::ArrayRef> {
    let datums: Vec<Option<Datum>> = ranks.map(|k| k.map(Datum::Int)).collect();
    leaf_array(ty, datums.iter().map(Option::as_ref).collect())
}

/// The composed type a data type names outside a DDL, as a cast names it: an array of a type held,
/// a range built in, or a type a DDL this process read declares, by its name, where one alone has
/// it.
pub fn resolve(dt: &sqlparser::ast::DataType) -> Option<ColumnType> {
    use sqlparser::ast::{ArrayElemTypeDef, DataType};
    match dt {
        DataType::Array(
            ArrayElemTypeDef::SquareBracket(inner, _)
            | ArrayElemTypeDef::AngleBracket(inner)
            | ArrayElemTypeDef::Parenthesis(inner)
            | ArrayElemTypeDef::Qualified(inner, _),
        ) => {
            let element = crate::catalog::held_type(inner)
                .ok()
                .or_else(|| resolve(inner))?;
            if let ColumnType::Composed(r) = element {
                if matches!(r.def(), Composed::Array { .. }) {
                    return Some(element);
                }
            }
            Some(type_of(Composed::Array {
                element,
                domain: None,
            }))
        }
        DataType::Custom(name, args) if args.is_empty() => {
            let n = crate::catalog::object_name(name);
            if let Some(t) = built_in(&n) {
                return Some(t);
            }
            let all = held().read().unwrap();
            let mut found = all
                .iter()
                .enumerate()
                .filter(|(_, d)| !matches!(d, Composed::Array { .. }) && d.pg_name() == n);
            let first = found.next()?;
            found
                .next()
                .is_none()
                .then(|| ColumnType::Composed(TypeRef(first.0 as u32)))
        }
        _ => None,
    }
}

/// A value of type `ty` a comparison is given: as `literal` reads it, but a quoted literal beside a
/// composite refused, as PostgreSQL types it an anonymous record, whose input it does not read.
pub fn compared(ty: ColumnType, e: &sqlparser::ast::Expr) -> Result<Datum> {
    use sqlparser::ast::{Expr, Value};
    if let ColumnType::Composed(r) = ty {
        if matches!(r.def(), Composed::Record { .. })
            && matches!(e, Expr::Value(v) if matches!(v.value, Value::SingleQuotedString(_)))
        {
            return Err(Error::new(
                "input of anonymous composite types is not implemented",
            ));
        }
    }
    literal(ty, e)
}

/// Whether an expression writes out a value of type `ty` by its constructor: an array as
/// `ARRAY[…]`, a composite as `ROW(…)` or `(…)`, a range or a multirange as its type's function of
/// its name.
pub fn constructs(ty: ColumnType, e: &sqlparser::ast::Expr) -> bool {
    use sqlparser::ast::Expr;
    let ColumnType::Composed(r) = ty else {
        return false;
    };
    match (r.def(), e) {
        (_, Expr::Nested(x)) | (_, Expr::Cast { expr: x, .. }) => constructs(ty, x),
        (Composed::Array { .. }, Expr::Array(_)) => true,
        (Composed::Record { .. }, Expr::Tuple(_)) => true,
        (Composed::Record { .. }, Expr::Function(f)) => {
            crate::catalog::object_name(&f.name).eq_ignore_ascii_case("row")
        }
        (Composed::Range { name, .. } | Composed::Multirange { name, .. }, Expr::Function(f)) => {
            crate::catalog::object_name(&f.name) == *name
        }
        _ => false,
    }
}

/// One bound of a range, as `range_cmp_bounds` compares them: its value's key, none where it is
/// unbounded, whether it is inclusive, and whether it is the lower.
#[derive(Clone, Debug)]
struct Bound {
    key: Option<Vec<u8>>,
    value: Option<Item>,
    inclusive: bool,
    lower: bool,
}

/// A range's bounds; none for an empty range.
fn bounds(subtype: ColumnType, r: &Item) -> Option<(Bound, Bound)> {
    let Item::Range {
        flags,
        lower,
        upper,
    } = r
    else {
        return None;
    };
    if flags & EMPTY != 0 {
        return None;
    }
    let b = |v: &Option<Box<Item>>, inf: u8, inc: u8, lower: bool| {
        let v = v.as_deref().filter(|_| flags & inf == 0);
        Bound {
            key: v.map(|v| key(subtype, v)),
            value: v.cloned(),
            inclusive: flags & inc != 0,
            lower,
        }
    };
    Some((
        b(lower, LB_INF, LB_INC, true),
        b(upper, UB_INF, UB_INC, false),
    ))
}

fn cmp_bounds(a: &Bound, b: &Bound) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    match (&a.key, &b.key) {
        (None, None) if a.lower == b.lower => Equal,
        (None, None) => {
            if a.lower {
                Less
            } else {
                Greater
            }
        }
        (None, _) => {
            if a.lower {
                Less
            } else {
                Greater
            }
        }
        (_, None) => {
            if b.lower {
                Greater
            } else {
                Less
            }
        }
        (Some(x), Some(y)) => match x.cmp(y) {
            Equal => match (a.inclusive, b.inclusive) {
                (false, false) if a.lower == b.lower => Equal,
                (false, false) => {
                    if a.lower {
                        Greater
                    } else {
                        Less
                    }
                }
                (false, true) => {
                    if a.lower {
                        Greater
                    } else {
                        Less
                    }
                }
                (true, false) => {
                    if b.lower {
                        Less
                    } else {
                        Greater
                    }
                }
                (true, true) => Equal,
            },
            o => o,
        },
    }
}

/// Whether two bounds at one value touch with no value between: an upper and a lower, one of the
/// two inclusive, or, for a discrete range, values one apart as its canonical form writes them.
fn adjacent(upper: &Bound, lower: &Bound) -> bool {
    match (&upper.key, &lower.key) {
        (Some(x), Some(y)) => x == y && upper.inclusive != lower.inclusive,
        _ => false,
    }
}

fn range_item(subtype: ColumnType, discrete: bool, lo: &Bound, hi: &Bound) -> Result<Item> {
    let mut flags = 0;
    if lo.inclusive {
        flags |= LB_INC;
    }
    if hi.inclusive {
        flags |= UB_INC;
    }
    make_range(subtype, discrete, flags, lo.value.clone(), hi.value.clone())
}

fn empty_range() -> Item {
    Item::Range {
        flags: EMPTY,
        lower: None,
        upper: None,
    }
}

/// What a range operator gives two ranges of type `ty`: a truth, or a range.
pub enum RangeAnswer {
    Truth(bool),
    Range(Item),
}

/// A range operator on two ranges: `@>`, `<@`, `&&`, `<<`, `>>`, `&<`, `&>`, `-|-`, `+`, `*` and
/// `-`, as PostgreSQL's range functions answer them.
pub fn range_op(ty: ColumnType, op: &str, a: &Item, b: &Item) -> Result<RangeAnswer> {
    use std::cmp::Ordering::*;
    use RangeAnswer::{Range, Truth};
    let (subtype, discrete) = range_of(ty).ok_or_else(|| Error::new("not a range"))?;
    let (x, y) = (bounds(subtype, a), bounds(subtype, b));
    Ok(match op {
        "@>" => Truth(match (&x, &y) {
            (_, None) => true,
            (None, _) => false,
            (Some((al, au)), Some((bl, bu))) => {
                cmp_bounds(al, bl) != Greater && cmp_bounds(au, bu) != Less
            }
        }),
        "<@" => return range_op(ty, "@>", b, a),
        "&&" => Truth(match (&x, &y) {
            (Some((al, au)), Some((bl, bu))) => {
                (cmp_bounds(al, bl) != Less && cmp_bounds(al, bu) != Greater)
                    || (cmp_bounds(bl, al) != Less && cmp_bounds(bl, au) != Greater)
            }
            _ => false,
        }),
        "<<" => Truth(match (&x, &y) {
            (Some((_, au)), Some((bl, _))) => cmp_bounds(au, bl) == Less,
            _ => false,
        }),
        ">>" => Truth(match (&x, &y) {
            (Some((al, _)), Some((_, bu))) => cmp_bounds(al, bu) == Greater,
            _ => false,
        }),
        "&<" => Truth(match (&x, &y) {
            (Some((_, au)), Some((_, bu))) => cmp_bounds(au, bu) != Greater,
            _ => false,
        }),
        "&>" => Truth(match (&x, &y) {
            (Some((al, _)), Some((bl, _))) => cmp_bounds(al, bl) != Less,
            _ => false,
        }),
        "-|-" => Truth(match (&x, &y) {
            (Some((al, au)), Some((bl, bu))) => adjacent(au, bl) || adjacent(bu, al),
            _ => false,
        }),
        "+" => Range(match (&x, &y) {
            (None, _) => b.clone(),
            (_, None) => a.clone(),
            (Some((al, au)), Some((bl, bu))) => {
                let overlap = matches!(range_op(ty, "&&", a, b)?, Truth(true));
                let touch = adjacent(au, bl) || adjacent(bu, al);
                if !overlap && !touch {
                    return Err(Error::new("result of range union would not be contiguous"));
                }
                let lo = if cmp_bounds(al, bl) == Greater {
                    bl
                } else {
                    al
                };
                let hi = if cmp_bounds(au, bu) == Less { bu } else { au };
                range_item(subtype, discrete, lo, hi)?
            }
        }),
        "*" => Range(match (&x, &y) {
            (Some((al, au)), Some((bl, bu)))
                if matches!(range_op(ty, "&&", a, b)?, Truth(true)) =>
            {
                let lo = if cmp_bounds(al, bl) == Less { bl } else { al };
                let hi = if cmp_bounds(au, bu) == Greater {
                    bu
                } else {
                    au
                };
                range_item(subtype, discrete, lo, hi)?
            }
            _ => empty_range(),
        }),
        "-" => Range(match (&x, &y) {
            (None, _) => empty_range(),
            (_, None) => a.clone(),
            (Some((al, au)), Some((bl, bu))) => {
                let (l1l2, l1u2, u1l2, u1u2) = (
                    cmp_bounds(al, bl),
                    cmp_bounds(al, bu),
                    cmp_bounds(au, bl),
                    cmp_bounds(au, bu),
                );
                if l1l2 == Less && u1u2 == Greater {
                    return Err(Error::new(
                        "result of range difference would not be contiguous",
                    ));
                }
                if l1u2 == Greater || u1l2 == Less {
                    a.clone()
                } else if l1l2 != Less && u1u2 != Greater {
                    empty_range()
                } else if l1l2 != Greater && u1l2 != Less && u1u2 != Greater {
                    let mut hi = bl.clone();
                    hi.inclusive = !bl.inclusive;
                    hi.lower = false;
                    range_item(subtype, discrete, al, &hi)?
                } else {
                    let mut lo = bu.clone();
                    lo.inclusive = !bu.inclusive;
                    lo.lower = true;
                    range_item(subtype, discrete, &lo, au)?
                }
            }
        }),
        other => {
            return Err(Error::new(format!(
                "the operator {other} on a range is not read"
            )))
        }
    })
}

/// Whether a range holds a value of its subtype: `@>` and `<@` with an element.
pub fn range_holds(ty: ColumnType, r: &Item, v: &Item) -> Result<bool> {
    let (subtype, _) = range_of(ty).ok_or_else(|| Error::new("not a range"))?;
    let Some((lo, hi)) = bounds(subtype, r) else {
        return Ok(false);
    };
    let point = |lower: bool| Bound {
        key: Some(key(subtype, v)),
        value: Some(v.clone()),
        inclusive: true,
        lower,
    };
    Ok(cmp_bounds(&lo, &point(true)) != std::cmp::Ordering::Greater
        && cmp_bounds(&hi, &point(false)) != std::cmp::Ordering::Less)
}

/// One side of a multirange's operator: a multirange's ranges, a range, or a value of its subtype.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Spanned {
    Ranges(Vec<Item>),
    Range(Item),
    Element(Item),
}

impl Spanned {
    /// Its ranges: a range's own, none where it is empty.
    fn ranges(&self) -> Option<Vec<Item>> {
        match self {
            Spanned::Ranges(v) => Some(v.clone()),
            Spanned::Range(Item::Range { flags, .. }) if flags & EMPTY != 0 => Some(Vec::new()),
            Spanned::Range(r) => Some(vec![r.clone()]),
            Spanned::Element(_) => None,
        }
    }
}

/// The range from the lower bound of a multirange's first range to the upper bound of its last.
fn span_of(ranges: &[Item]) -> Item {
    match (ranges.first(), ranges.last()) {
        (
            Some(Item::Range {
                flags: fl,
                lower: lo,
                ..
            }),
            Some(Item::Range {
                flags: fu,
                upper: hi,
                ..
            }),
        ) => Item::Range {
            flags: (fl & (LB_INC | LB_INF)) | (fu & (UB_INC | UB_INF)),
            lower: lo.clone(),
            upper: hi.clone(),
        },
        _ => empty_range(),
    }
}

/// An operator of a multirange of type `ty`, beside a multirange, a range of its ranges' type or,
/// for `@>` and `<@`, a value of their subtype: `@>`, `<@`, `&&`, `<<`, `>>`, `&<`, `&>` and `-|-`;
/// `+`, `*` and `-` of two multiranges. As PostgreSQL's multirange functions answer them, an empty
/// multirange or range overlapping, adjoining and lying beside nothing.
pub fn multirange_op(ty: ColumnType, op: &str, a: &Spanned, b: &Spanned) -> Result<RangeAnswer> {
    use RangeAnswer::{Range, Truth};
    let Some(Composed::Multirange { range, .. }) = (match ty {
        ColumnType::Composed(r) => Some(r.def()),
        _ => None,
    }) else {
        return Err(Error::new("not a multirange"));
    };
    let range = *range;
    let truth = |o: &str, x: &Item, y: &Item| -> Result<bool> {
        Ok(matches!(range_op(range, o, x, y)?, Truth(true)))
    };
    let holds = |rs: &[Item], v: &Item| -> Result<bool> {
        for r in rs {
            if range_holds(range, r, v)? {
                return Ok(true);
            }
        }
        Ok(false)
    };
    if let ("<@", Spanned::Element(v)) = (op, a) {
        return Ok(Truth(holds(&b.ranges().unwrap_or_default(), v)?));
    }
    if let ("@>", Spanned::Element(v)) = (op, b) {
        return Ok(Truth(holds(&a.ranges().unwrap_or_default(), v)?));
    }
    let (Some(x), Some(y)) = (a.ranges(), b.ranges()) else {
        return Err(Error::new(format!(
            "the operator {op} on a multirange and a value of its subtype is not read"
        )));
    };
    let both = matches!((a, b), (Spanned::Ranges(_), Spanned::Ranges(_)));
    let either_empty = x.is_empty() || y.is_empty();
    Ok(match op {
        "@>" | "<@" => {
            let (big, small) = if op == "@>" { (&x, &y) } else { (&y, &x) };
            let mut every = true;
            for s in small {
                let mut held = false;
                for g in big {
                    if truth("@>", g, s)? {
                        held = true;
                        break;
                    }
                }
                every &= held;
            }
            Truth(every)
        }
        _ if either_empty && matches!(op, "&&" | "<<" | ">>" | "&<" | "&>" | "-|-") => Truth(false),
        "&&" => {
            let mut any = false;
            for r in &x {
                for s in &y {
                    any |= truth("&&", r, s)?;
                }
            }
            Truth(any)
        }
        "<<" => Truth(truth("<<", x.last().unwrap(), y.first().unwrap())?),
        ">>" => Truth(truth(">>", x.first().unwrap(), y.last().unwrap())?),
        "&<" => Truth(truth("&<", x.last().unwrap(), y.last().unwrap())?),
        "&>" => Truth(truth("&>", x.first().unwrap(), y.first().unwrap())?),
        "-|-" => Truth(truth("-|-", &span_of(&x), &span_of(&y))?),
        "+" | "*" | "-" if !both => {
            return Err(Error::new(format!(
                "the operator {op} on a multirange and a range is not read: PostgreSQL has none"
            )))
        }
        "+" if x.is_empty() => Range(Item::Multirange(y)),
        "+" if y.is_empty() => Range(Item::Multirange(x)),
        "+" => Range(Item::Multirange(canonical_ranges(
            range,
            x.into_iter().chain(y).collect(),
        ))),
        "*" if either_empty => Range(Item::Multirange(Vec::new())),
        "*" => Range(Item::Multirange(multirange_intersect(range, &x, &y)?)),
        "-" if either_empty => Range(Item::Multirange(x)),
        "-" => Range(Item::Multirange(multirange_minus(range, &x, &y)?)),
        other => {
            return Err(Error::new(format!(
                "the operator {other} on a multirange is not read"
            )))
        }
    })
}

/// The ranges of `x` less those of `y`, as `multirange_minus_internal` walks the two in order.
fn multirange_minus(range: ColumnType, x: &[Item], y: &[Item]) -> Result<Vec<Item>> {
    use std::cmp::Ordering::{Greater, Less};
    let (subtype, discrete) = range_of(range).ok_or_else(|| Error::new("not a range"))?;
    let truth = |o: &str, a: &Item, b: &Item| -> Result<bool> {
        Ok(matches!(
            range_op(range, o, a, b)?,
            RangeAnswer::Truth(true)
        ))
    };
    let mut out = Vec::new();
    let mut i2 = 0;
    for r in x {
        let mut r1 = r.clone();
        while i2 < y.len() && truth("<<", &y[i2], &r1)? {
            i2 += 1;
        }
        while i2 < y.len() {
            let r2 = &y[i2];
            let split = match (bounds(subtype, &r1), bounds(subtype, r2)) {
                (Some((l1, u1)), Some((l2, u2)))
                    if cmp_bounds(&l1, &l2) == Less && cmp_bounds(&u1, &u2) == Greater =>
                {
                    let mut hi = l2.clone();
                    hi.inclusive = !l2.inclusive;
                    hi.lower = false;
                    let mut lo = u2.clone();
                    lo.inclusive = !u2.inclusive;
                    lo.lower = true;
                    Some((
                        range_item(subtype, discrete, &l1, &hi)?,
                        range_item(subtype, discrete, &lo, &u1)?,
                    ))
                }
                _ => None,
            };
            if let Some((left, right)) = split {
                out.push(left);
                r1 = right;
                i2 += 1;
            } else if truth("&&", &r1, r2)? {
                r1 = match range_op(range, "-", &r1, r2)? {
                    RangeAnswer::Range(i) => i,
                    RangeAnswer::Truth(_) => unreachable!(),
                };
                let gone = matches!(r1, Item::Range { flags, .. } if flags & EMPTY != 0);
                if gone || truth("<<", &r1, r2)? {
                    break;
                }
                i2 += 1;
            } else {
                break;
            }
        }
        out.push(r1);
    }
    Ok(canonical_ranges(range, out))
}

/// The ranges `x` and `y` both hold, as `multirange_intersect_internal` walks the two in order.
fn multirange_intersect(range: ColumnType, x: &[Item], y: &[Item]) -> Result<Vec<Item>> {
    let truth = |o: &str, a: &Item, b: &Item| -> Result<bool> {
        Ok(matches!(
            range_op(range, o, a, b)?,
            RangeAnswer::Truth(true)
        ))
    };
    let mut out = Vec::new();
    let mut i2 = 0;
    for r1 in x {
        while i2 < y.len() && truth("<<", &y[i2], r1)? {
            i2 += 1;
        }
        while i2 < y.len() {
            let r2 = &y[i2];
            if !truth("&&", r1, r2)? {
                break;
            }
            if let RangeAnswer::Range(i) = range_op(range, "*", r1, r2)? {
                out.push(i);
            }
            if truth("&<", r2, r1)? {
                i2 += 1;
            } else {
                break;
            }
        }
        if i2 >= y.len() {
            break;
        }
    }
    Ok(canonical_ranges(range, out))
}

/// An array's slice, as `array_get_slice` takes it: each dimension from its lower subscript to its
/// upper, one not given or past the array's own its bound, the dimensions past those given whole;
/// its lower bounds 1. Empty where the subscripts are more than its dimensions or a dimension's
/// lower passes its upper.
pub fn slice(a: &Item, at: &[(Option<i64>, Option<i64>)]) -> Item {
    let empty = Item::Array {
        elements: Vec::new(),
        dims: Vec::new(),
        lower: Vec::new(),
    };
    let Item::Array {
        elements,
        dims,
        lower,
    } = a
    else {
        return Item::Null;
    };
    let n = dims.len();
    if n == 0 || n < at.len() {
        return empty;
    }
    let mut lo = vec![0i64; n];
    let mut hi = vec![0i64; n];
    for i in 0..n {
        let (l, u) = at.get(i).copied().unwrap_or((None, None));
        let first = i64::from(lower[i]);
        let last = first + i64::from(dims[i]) - 1;
        lo[i] = l.filter(|&l| l >= first).unwrap_or(first);
        hi[i] = u.filter(|&u| u <= last).unwrap_or(last);
        if lo[i] > hi[i] {
            return empty;
        }
    }
    let mut out = Vec::new();
    let mut at = lo.clone();
    loop {
        let mut offset = 0usize;
        for i in 0..n {
            offset = offset * dims[i] as usize + (at[i] - i64::from(lower[i])) as usize;
        }
        out.push(elements[offset].clone());
        let mut d = n;
        loop {
            if d == 0 {
                return Item::Array {
                    elements: out,
                    dims: (0..n).map(|i| (hi[i] - lo[i] + 1) as i32).collect(),
                    lower: vec![1; n],
                };
            }
            d -= 1;
            at[d] += 1;
            if at[d] <= hi[d] {
                break;
            }
            at[d] = lo[d];
        }
    }
}

/// An array's elements' keys, its NULLs none.
fn element_keys(element: ColumnType, a: &Item) -> Vec<Option<Vec<u8>>> {
    match a {
        Item::Array { elements, .. } => elements
            .iter()
            .map(|e| (!e.is_null()).then(|| key(element, e)))
            .collect(),
        _ => Vec::new(),
    }
}

/// An array operator on two arrays of type `ty`: `@>`, `<@` and `&&`, as `array_contains`,
/// `array_contained` and `arrayoverlap` answer them, a NULL element matching nothing.
pub fn array_op(ty: ColumnType, op: &str, a: &Item, b: &Item) -> Result<bool> {
    let Some(Composed::Array { element, .. }) = (match ty {
        ColumnType::Composed(r) => Some(r.def()),
        _ => None,
    }) else {
        return Err(Error::new("not an array"));
    };
    let (x, y) = (element_keys(*element, a), element_keys(*element, b));
    let contains = |big: &[Option<Vec<u8>>], small: &[Option<Vec<u8>>]| {
        small.iter().all(|s| {
            s.as_ref()
                .is_some_and(|s| big.iter().any(|b| b.as_ref() == Some(s)))
        })
    };
    Ok(match op {
        "@>" => contains(&x, &y),
        "<@" => contains(&y, &x),
        "&&" => x
            .iter()
            .flatten()
            .any(|k| y.iter().any(|j| j.as_ref() == Some(k))),
        other => {
            return Err(Error::new(format!(
                "the operator {other} on an array is not read"
            )))
        }
    })
}

/// Two arrays of type `ty` concatenated as `array_cat` does, or an element put before or after one
/// as `array_prepend` and `array_append` do.
pub fn concat(
    ty: ColumnType,
    a: Option<&Item>,
    b: Option<&Item>,
    element_first: bool,
    element_last: bool,
) -> Result<Item> {
    let shape = |i: Option<&Item>| match i {
        Some(Item::Array {
            elements,
            dims,
            lower,
        }) => (elements.clone(), dims.clone(), lower.clone()),
        _ => (Vec::new(), Vec::new(), Vec::new()),
    };
    if element_first {
        let (mut e, dims, lower) = shape(b);
        let x = a.cloned().unwrap_or(Item::Null);
        if dims.len() > 1 {
            return Err(Error::new(
                "argument must be empty or one-dimensional array",
            ));
        }
        e.insert(0, x);
        let lb = lower.first().copied().unwrap_or(1);
        return Ok(Item::Array {
            dims: vec![e.len() as i32],
            lower: vec![lb],
            elements: e,
        });
    }
    if element_last {
        let (mut e, dims, lower) = shape(a);
        let x = b.cloned().unwrap_or(Item::Null);
        if dims.len() > 1 {
            return Err(Error::new(
                "argument must be empty or one-dimensional array",
            ));
        }
        e.push(x);
        let lb = lower.first().copied().unwrap_or(1);
        return Ok(Item::Array {
            dims: vec![e.len() as i32],
            lower: vec![lb],
            elements: e,
        });
    }
    let _ = ty;
    let (ea, da, la) = shape(a);
    let (eb, db, lb) = shape(b);
    if ea.is_empty() {
        return Ok(b.cloned().unwrap_or(Item::Array {
            elements: eb,
            dims: db,
            lower: lb,
        }));
    }
    if eb.is_empty() {
        return Ok(a.cloned().unwrap_or(Item::Array {
            elements: ea,
            dims: da,
            lower: la,
        }));
    }
    let mut elements = ea.clone();
    elements.extend(eb.iter().cloned());
    if da.len() == db.len() {
        if da[1..] != db[1..] || la[1..] != lb[1..] {
            return Err(Error::new("cannot concatenate incompatible arrays"));
        }
        let mut dims = da.clone();
        dims[0] += db[0];
        return Ok(Item::Array {
            elements,
            dims,
            lower: la,
        });
    }
    if da.len() == db.len() + 1 && da[1..] == db[..] {
        let mut dims = da.clone();
        dims[0] += 1;
        return Ok(Item::Array {
            elements,
            dims,
            lower: la,
        });
    }
    if db.len() == da.len() + 1 && db[1..] == da[..] {
        let mut dims = db.clone();
        dims[0] += 1;
        return Ok(Item::Array {
            elements,
            dims,
            lower: lb,
        });
    }
    Err(Error::new("cannot concatenate incompatible arrays"))
}

/// An array's element at the subscripts given, each counted from its dimension's lower bound; NULL
/// where they fall outside it or are fewer or more than its dimensions.
pub fn subscript(a: &Item, at: &[i64]) -> Item {
    let Item::Array {
        elements,
        dims,
        lower,
    } = a
    else {
        return Item::Null;
    };
    if at.len() != dims.len() {
        return Item::Null;
    }
    let mut offset = 0usize;
    for ((&i, &d), &l) in at.iter().zip(dims).zip(lower) {
        let k = i - l as i64;
        if k < 0 || k >= d as i64 {
            return Item::Null;
        }
        offset = offset * d as usize + k as usize;
    }
    elements.get(offset).cloned().unwrap_or(Item::Null)
}

/// A composite's field by its name, and the field's type.
pub fn field(ty: ColumnType, item: &Item, name: &str) -> Result<(ColumnType, Item)> {
    let Some(Composed::Record { fields, .. }) = (match ty {
        ColumnType::Composed(r) => Some(r.def()),
        _ => None,
    }) else {
        return Err(Error::new(format!(
            "column notation .{name} applied to type {}, which is not a composite type",
            ty.pg_name()
        )));
    };
    let i = fields.iter().position(|f| f.name == name).ok_or_else(|| {
        Error::new(format!(
            "could not identify column \"{name}\" in record data type"
        ))
    })?;
    let v = match item {
        Item::Record(values) => values[i].clone(),
        _ => Item::Null,
    };
    Ok((fields[i].ty, v))
}

/// A type with every part of type `from` made of type `to`: the labels an enum's ADD VALUE or RENAME
/// VALUE gives it, in every type that holds it.
pub fn replace(ty: ColumnType, from: ColumnType, to: ColumnType) -> ColumnType {
    if ty == from {
        return to;
    }
    let ColumnType::Composed(r) = ty else {
        return ty;
    };
    type_of(match r.def().clone() {
        Composed::Array { element, domain } => Composed::Array {
            element: replace(element, from, to),
            domain,
        },
        Composed::Record { name, fields } => Composed::Record {
            name,
            fields: fields
                .into_iter()
                .map(|f| Field {
                    ty: replace(f.ty, from, to),
                    ..f
                })
                .collect(),
        },
        Composed::Range {
            name,
            subtype,
            discrete,
        } => Composed::Range {
            name,
            subtype: replace(subtype, from, to),
            discrete,
        },
        Composed::Multirange { name, range } => Composed::Multirange {
            name,
            range: replace(range, from, to),
        },
        e @ Composed::Enum { .. } => e,
    })
}
