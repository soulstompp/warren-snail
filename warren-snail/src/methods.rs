//! The index methods other than the B-tree, as a DDL names them: each method's operator classes and
//! the types they take, as PostgreSQL 19's catalog holds them, its storage parameters, and its
//! operator classes' own.

use crate::catalog::{ColumnType, Method};
use crate::composed::Composed;
use crate::geo::GeoKind;
use crate::textual::Textual;
use crate::{Error, Result};

/// Whether the snail holds an index of the method with a key of the operator class: a B-tree and a
/// hash on any; a GiST or an SP-GiST on a range, a multirange, an inet, or text; a key held as an
/// element strand.
pub fn held(method: Method, opclass: &str) -> bool {
    match method {
        Method::Btree | Method::Hash => true,
        _ if holds_elements(method, opclass) => true,
        Method::Gist => matches!(opclass, "range_ops" | "multirange_ops" | "inet_ops"),
        Method::Spgist => matches!(opclass, "range_ops" | "inet_ops" | "text_ops"),
        _ => false,
    }
}

/// Whether a key of the operator class is held as an element strand: a GIN index's, and a GiST
/// index's on a tsvector or a tsquery.
pub fn holds_elements(method: Method, opclass: &str) -> bool {
    method == Method::Gin
        || (method == Method::Gist && matches!(opclass, "tsvector_ops" | "tsquery_ops"))
}

/// Why a GIN index on an array refuses its elements' type: it has no B-tree to order them.
pub fn elements_refused(method: Method, opclass: &str, ty: ColumnType) -> Option<Error> {
    match composed(ty) {
        Some(Composed::Array { element, .. })
            if method == Method::Gin && opclass == "array_ops" && !element.ordered() =>
        {
            Some(Error::new(format!(
                "could not identify a comparison function for type {}",
                bare_name(*element)
            )))
        }
        _ => None,
    }
}

fn composed(ty: ColumnType) -> Option<&'static Composed> {
    match ty {
        ColumnType::Composed(r) => Some(r.def()),
        _ => None,
    }
}

fn is_range(ty: ColumnType) -> bool {
    matches!(composed(ty), Some(Composed::Range { .. }))
}

fn is_multirange(ty: ColumnType) -> bool {
    matches!(composed(ty), Some(Composed::Multirange { .. }))
}

fn is_array(ty: ColumnType) -> bool {
    matches!(composed(ty), Some(Composed::Array { .. }))
}

fn is_inet(ty: ColumnType) -> bool {
    matches!(
        ty,
        ColumnType::Textual(Textual::Inet) | ColumnType::Textual(Textual::Cidr)
    )
}

fn geo(ty: ColumnType, kind: GeoKind) -> bool {
    ty == ColumnType::Geometric(kind)
}

/// An operator class of a method: its name, whether it is its type's default, and whether it takes a
/// key of the type.
type Class = (&'static str, bool, fn(ColumnType) -> bool);

const GIST: [Class; 9] = [
    ("box_ops", true, |t| geo(t, GeoKind::Box)),
    ("circle_ops", true, |t| geo(t, GeoKind::Circle)),
    ("inet_ops", true, is_inet),
    ("multirange_ops", true, is_multirange),
    ("point_ops", true, |t| geo(t, GeoKind::Point)),
    ("poly_ops", true, |t| geo(t, GeoKind::Polygon)),
    ("range_ops", true, is_range),
    ("tsquery_ops", true, |t| {
        t == ColumnType::Textual(Textual::Tsquery)
    }),
    ("tsvector_ops", true, |t| {
        t == ColumnType::Textual(Textual::Tsvector)
    }),
];

const SPGIST: [Class; 7] = [
    ("box_ops", true, |t| geo(t, GeoKind::Box)),
    ("inet_ops", true, is_inet),
    ("kd_point_ops", false, |t| geo(t, GeoKind::Point)),
    ("poly_ops", true, |t| geo(t, GeoKind::Polygon)),
    ("quad_point_ops", true, |t| geo(t, GeoKind::Point)),
    ("range_ops", true, is_range),
    ("text_ops", true, |t| t.is_text()),
];

const GIN: [Class; 4] = [
    ("array_ops", true, is_array),
    ("jsonb_ops", true, |t| {
        t == ColumnType::Textual(Textual::Jsonb)
    }),
    ("jsonb_path_ops", false, |t| {
        t == ColumnType::Textual(Textual::Jsonb)
    }),
    ("tsvector_ops", true, |t| {
        t == ColumnType::Textual(Textual::Tsvector)
    }),
];

/// The hash operator classes besides each type's default: the pattern ones, varchar's, cidr's.
const HASH_NAMED: [(&str, fn(ColumnType) -> bool); 5] = [
    ("text_pattern_ops", |t| t.is_text()),
    ("varchar_ops", |t| t.is_text()),
    ("varchar_pattern_ops", |t| t.is_text()),
    ("bpchar_pattern_ops", |t| matches!(t, ColumnType::Bpchar(_))),
    ("cidr_ops", is_inet),
];

/// Every hash operator class PostgreSQL 19 holds, by name.
const HASH: [&str; 42] = [
    "aclitem_ops",
    "array_ops",
    "bool_ops",
    "bpchar_ops",
    "bpchar_pattern_ops",
    "bytea_ops",
    "char_ops",
    "cid_ops",
    "cidr_ops",
    "date_ops",
    "enum_ops",
    "float4_ops",
    "float8_ops",
    "inet_ops",
    "int2_ops",
    "int4_ops",
    "int8_ops",
    "interval_ops",
    "jsonb_ops",
    "macaddr8_ops",
    "macaddr_ops",
    "multirange_ops",
    "name_ops",
    "numeric_ops",
    "oid8_ops",
    "oid_ops",
    "oidvector_ops",
    "pg_lsn_ops",
    "range_ops",
    "record_ops",
    "text_ops",
    "text_pattern_ops",
    "tid_ops",
    "time_ops",
    "timestamp_ops",
    "timestamptz_ops",
    "timetz_ops",
    "uuid_ops",
    "varchar_ops",
    "varchar_pattern_ops",
    "xid8_ops",
    "xid_ops",
];

/// The kinds of BRIN summary a prefix of operator class names has: (prefix, minmax-multi, bloom).
const BRIN_PREFIXES: [(&str, bool, bool); 26] = [
    ("bit", false, false),
    ("bpchar", false, true),
    ("bytea", false, true),
    ("char", false, true),
    ("date", true, true),
    ("float4", true, true),
    ("float8", true, true),
    ("inet", true, true),
    ("int2", true, true),
    ("int4", true, true),
    ("int8", true, true),
    ("interval", true, true),
    ("macaddr", true, true),
    ("macaddr8", true, true),
    ("name", false, true),
    ("numeric", true, true),
    ("oid", true, true),
    ("pg_lsn", true, true),
    ("text", false, true),
    ("tid", true, true),
    ("time", true, true),
    ("timestamp", true, true),
    ("timestamptz", true, true),
    ("timetz", true, true),
    ("uuid", true, true),
    ("varbit", false, false),
];

/// The prefix of a type's BRIN operator classes' names.
fn brin_prefix(ty: ColumnType) -> Option<&'static str> {
    Some(match ty {
        ColumnType::Bit(_) => "bit",
        ColumnType::Varbit(_) => "varbit",
        ColumnType::Bpchar(_) => "bpchar",
        ColumnType::Bytes => "bytea",
        ColumnType::Char => "char",
        ColumnType::Date => "date",
        ColumnType::Float4 => "float4",
        ColumnType::Float8 => "float8",
        t if is_inet(t) => "inet",
        ColumnType::Int2 => "int2",
        ColumnType::Int4 => "int4",
        ColumnType::Int8 => "int8",
        ColumnType::Interval { .. } => "interval",
        ColumnType::Macaddr => "macaddr",
        ColumnType::Macaddr8 => "macaddr8",
        ColumnType::Name => "name",
        ColumnType::Numeric { .. } | ColumnType::NumericText(_) => "numeric",
        ColumnType::Oid | ColumnType::Reg(..) => "oid",
        ColumnType::PgLsn => "pg_lsn",
        ColumnType::Text(_) => "text",
        ColumnType::Tid => "tid",
        ColumnType::Time(_) => "time",
        ColumnType::Timestamp(_) => "timestamp",
        ColumnType::Timestamptz | ColumnType::TimestamptzRounded(_) => "timestamptz",
        ColumnType::Timetz(_) => "timetz",
        ColumnType::Uuid => "uuid",
        _ => return None,
    })
}

/// A BRIN operator class's name read: whether it exists, and whether it takes a key of `ty`; and the
/// type's default.
fn brin(ty: ColumnType, named: Option<&str>) -> (bool, bool, Option<String>) {
    let inclusion = |t: ColumnType| -> Option<&'static str> {
        if geo(t, GeoKind::Box) {
            Some("box_inclusion_ops")
        } else if is_inet(t) {
            Some("inet_inclusion_ops")
        } else if is_range(t) {
            Some("range_inclusion_ops")
        } else {
            None
        }
    };
    // inet's default is its inclusion, every other type's with a minmax its minmax.
    let default = inclusion(ty)
        .map(str::to_string)
        .or_else(|| brin_prefix(ty).map(|p| format!("{p}_minmax_ops")));
    let Some(name) = named else {
        return (true, true, default);
    };
    let exists = [
        "box_inclusion_ops",
        "inet_inclusion_ops",
        "range_inclusion_ops",
    ]
    .contains(&name)
        || BRIN_PREFIXES.iter().any(|&(p, multi, bloom)| {
            name == format!("{p}_minmax_ops")
                || (multi && name == format!("{p}_minmax_multi_ops"))
                || (bloom && name == format!("{p}_bloom_ops"))
        });
    let takes = inclusion(ty) == Some(name)
        || brin_prefix(ty).is_some_and(|p| {
            name.strip_prefix(p).is_some_and(|rest| {
                matches!(rest, "_minmax_ops" | "_minmax_multi_ops" | "_bloom_ops")
            })
        });
    (exists, exists && takes, default)
}

/// The operator class a key of type `ty` takes in an index of `method`, other than the B-tree, as
/// PostgreSQL's `ResolveOpClass` resolves it: the one named, or the type's default.
pub fn opclass(method: Method, ty: ColumnType, named: Option<&str>) -> Result<String> {
    let am = method.name();
    let (exists, takes, default) = match method {
        Method::Hash => {
            let exists = named.is_none_or(|n| HASH.contains(&n));
            let takes = match named {
                None => true,
                Some(n) => {
                    ty.hash_opclass() == Some(n) || HASH_NAMED.iter().any(|&(o, f)| o == n && f(ty))
                }
            };
            (exists, takes, ty.hash_opclass().map(str::to_string))
        }
        Method::Brin => brin(ty, named),
        Method::Gist | Method::Spgist | Method::Gin => {
            let classes: &[Class] = match method {
                Method::Gist => &GIST,
                Method::Spgist => &SPGIST,
                _ => &GIN,
            };
            let exists = named.is_none_or(|n| classes.iter().any(|c| c.0 == n));
            let takes = named.is_none_or(|n| classes.iter().any(|c| c.0 == n && (c.2)(ty)));
            let default = classes
                .iter()
                .find(|c| c.1 && (c.2)(ty))
                .map(|c| c.0.to_string());
            (exists, takes, default)
        }
        Method::Btree => unreachable!("a B-tree's operator class is read beside its order"),
    };
    match named {
        None => default.ok_or_else(|| {
            Error::new(format!(
                "data type {} has no default operator class for access method \"{am}\"",
                bare_name(ty)
            ))
        }),
        Some(n) if !exists => Err(Error::new(format!(
            "operator class \"{n}\" does not exist for access method \"{am}\""
        ))),
        Some(n) if !takes => Err(Error::new(format!(
            "operator class \"{n}\" does not accept data type {}",
            bare_name(ty)
        ))),
        Some(n) => Ok(n.to_string()),
    }
}

/// What a parameter reads, and its bounds.
#[derive(Clone, Copy)]
enum Param {
    Int(i64, i64),
    Real(f64, f64),
    Bool,
    Buffering,
}

/// A value as written in a parameter list: unquoted, its doubled quotes made one.
fn unquoted(v: &str) -> String {
    let v = v.trim();
    match v.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')) {
        Some(x) => x.replace("''", "'"),
        None => v.to_string(),
    }
}

/// A parameter's value read as PostgreSQL's `parse_one_reloption` reads it, refused in its words.
fn param(name: &str, kind: Param, v: &str) -> Result<()> {
    match kind {
        Param::Int(lo, hi) => {
            let n: i64 = v.parse().map_err(|_| {
                Error::new(format!("invalid value for integer option \"{name}\": {v}"))
            })?;
            if !(lo..=hi).contains(&n) {
                return Err(Error::new(format!(
                    "value {v} out of bounds for option \"{name}\""
                )));
            }
        }
        Param::Real(lo, hi) => {
            let x: f64 = v.parse().map_err(|_| {
                Error::new(format!(
                    "invalid value for floating point option \"{name}\": {v}"
                ))
            })?;
            if !(lo..=hi).contains(&x) {
                return Err(Error::new(format!(
                    "value {v} out of bounds for option \"{name}\""
                )));
            }
        }
        Param::Bool => {
            if crate::read::truth(v).is_none() || v.trim() != v {
                return Err(Error::new(format!(
                    "invalid value for boolean option \"{name}\": {v}"
                )));
            }
        }
        Param::Buffering => {
            if !["on", "off", "auto"].contains(&v.to_lowercase().as_str()) {
                return Err(Error::new(format!(
                    "invalid value for enum option \"{name}\": {v}"
                )));
            }
        }
    }
    Ok(())
}

/// The storage parameters an index of a method takes.
fn storage_params(method: Method) -> &'static [(&'static str, Param)] {
    const FILL: Param = Param::Int(10, 100);
    match method {
        Method::Btree => &[("fillfactor", FILL), ("deduplicate_items", Param::Bool)],
        Method::Hash | Method::Spgist => &[("fillfactor", FILL)],
        Method::Gist => &[("fillfactor", FILL), ("buffering", Param::Buffering)],
        Method::Gin => &[
            ("fastupdate", Param::Bool),
            ("gin_pending_list_limit", Param::Int(64, i32::MAX as i64)),
        ],
        Method::Brin => &[
            ("pages_per_range", Param::Int(1, 131072)),
            ("autosummarize", Param::Bool),
        ],
    }
}

/// A storage parameter of an index of `method`, `name = value` as written: its value as
/// PostgreSQL keeps it, or why it refuses it.
pub fn storage_param(method: Method, name: &str, value: &str) -> Result<String> {
    let v = unquoted(value);
    let (_, kind) = storage_params(method)
        .iter()
        .find(|(n, _)| *n == name)
        .ok_or_else(|| Error::new(format!("unrecognized parameter \"{name}\"")))?;
    param(name, *kind, &v)?;
    Ok(v)
}

/// An operator class's parameters, `name = value, …` as written, read as its options function reads
/// them: GiST tsvector_ops' signature length, a BRIN minmax-multi's values a range, a BRIN bloom's
/// distinct values a range and its false positive rate.
pub fn opclass_params(method: Method, opclass: &str, written: &str) -> Result<()> {
    let params: &[(&str, Param)] = if method == Method::Gist && opclass == "tsvector_ops" {
        &[("siglen", Param::Int(1, 2024))]
    } else if opclass.ends_with("_minmax_multi_ops") {
        &[("values_per_range", Param::Int(8, 256))]
    } else if opclass.ends_with("_bloom_ops") {
        &[
            ("n_distinct_per_range", Param::Real(-1.0, i32::MAX as f64)),
            ("false_positive_rate", Param::Real(0.0001, 0.25)),
        ]
    } else {
        return Err(Error::new(format!(
            "operator class {opclass} has no options"
        )));
    };
    let mut seen: Vec<String> = Vec::new();
    for p in written.split(',') {
        let (name, value) = p
            .split_once('=')
            .ok_or_else(|| Error::new(format!("{opclass}: ({written}) is not read")))?;
        let name = name.trim().to_lowercase();
        let (_, kind) = params
            .iter()
            .find(|(n, _)| *n == name)
            .ok_or_else(|| Error::new(format!("unrecognized parameter \"{name}\"")))?;
        if seen.contains(&name) {
            return Err(Error::new(format!(
                "parameter \"{name}\" specified more than once"
            )));
        }
        param(&name, *kind, &unquoted(value))?;
        seen.push(name);
    }
    Ok(())
}

/// A type as PostgreSQL's `format_type_be` names it in a refusal: without its modifier.
pub(crate) fn bare_name(ty: ColumnType) -> String {
    match ty {
        ColumnType::Bpchar(_) => return "character".into(),
        ColumnType::Interval { .. } => return "interval".into(),
        _ => {}
    }
    let mut out = String::new();
    let mut depth = 0;
    for ch in ty.pg_name().chars() {
        match ch {
            '(' => depth += 1,
            ')' => depth -= 1,
            c if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

/// Whether an EXCLUDE's element may name an operator, as PostgreSQL's `ComputeIndexAttrs` asks:
/// commutative, and of the element's operator class's family.
pub fn exclusion_operator(method: Method, opclass: &str, ty: ColumnType, op: &str) -> Result<()> {
    let members: &[&str] = match (method, opclass) {
        (Method::Btree | Method::Hash, _) => &["="],
        (_, "range_ops" | "multirange_ops") => &["&&", "=", "-|-"],
        (_, "inet_ops") => &["&&", "=", "<>"],
        (_, "text_ops") => &["="],
        _ => &[],
    };
    // A range's and a multirange's operators are the polymorphic ones.
    let t = if is_range(ty) {
        "anyrange".to_string()
    } else if is_multirange(ty) {
        "anymultirange".to_string()
    } else {
        bare_name(ty)
    };
    if !["=", "<>", "&&", "-|-", "~="].contains(&op) {
        return Err(Error::new(format!(
            "operator {op}({t},{t}) is not commutative"
        )));
    }
    if !members.contains(&op) {
        let family = match opclass {
            "int2_ops" | "int4_ops" | "int8_ops" => "integer_ops",
            "inet_ops" => "network_ops",
            o => o,
        };
        return Err(Error::new(format!(
            "operator {op}({t},{t}) is not a member of operator family \"{family}\""
        )));
    }
    Ok(())
}
