//! The functions of arrays a pipeline answers by name, over `crate::arrays`: each argument read as
//! the type the function takes it as, an element beside an array as the array's element type.

use super::pipeline::{composed, part_of, Value};
use crate::catalog::ColumnType;
use crate::composed::{Composed, Field, Item};
use crate::read::statement::Datum;
use crate::{Error, Result};

/// The names of the functions of arrays the snail answers, the sets among them.
pub(super) fn is_array_function(name: &str) -> bool {
    matches!(
        name,
        "array_dims"
            | "array_ndims"
            | "array_length"
            | "array_lower"
            | "array_upper"
            | "cardinality"
            | "array_fill"
            | "array_position"
            | "array_positions"
            | "array_remove"
            | "array_replace"
            | "array_reverse"
            | "array_sort"
            | "trim_array"
            | "array_shuffle"
            | "array_sample"
            | "array_to_string"
            | "string_to_array"
            | "parse_ident"
            | "acldefault"
            | "oidvectortypes"
    )
}

/// The record type of a set-returning function's rows, where they are records.
fn record(name: &str) -> Option<ColumnType> {
    let fields = match name {
        "aclexplode" => vec![
            Field::new("grantor", ColumnType::Oid),
            Field::new("grantee", ColumnType::Oid),
            Field::new("privilege_type", ColumnType::Text(None)),
            Field::new("is_grantable", ColumnType::Boolean),
        ],
        "pg_options_to_table" => vec![
            Field::new("option_name", ColumnType::Text(None)),
            Field::new("option_value", ColumnType::Text(None)),
        ],
        _ => return None,
    };
    Some(crate::composed::type_of(Composed::Record {
        name: "record".into(),
        fields,
    }))
}

fn aclitem_array() -> ColumnType {
    super::typed::array_of(ColumnType::Textual(crate::textual::Textual::Aclitem))
}

/// The type a function of arrays returns over its arguments' types, or its refusal where it has no
/// form for them; none where the name is not one of them, or the step leaves the type open.
pub(super) fn typed(name: &str, args: &[Option<ColumnType>]) -> Result<Option<Option<ColumnType>>> {
    use super::typed::{array_of, element_of};
    let int4 = Some(ColumnType::Int4);
    let text_array = Some(array_of(ColumnType::Text(None)));
    let first = args.first().copied().flatten();
    let array = first.filter(|t| element_of(*t).is_some());
    let no_form = || Err(super::typed::no_form(name, args));
    Ok(Some(match name {
        "string_to_array" | "parse_ident" => text_array,
        "string_to_table" if (2..=3).contains(&args.len()) => Some(ColumnType::Text(None)),
        "oidvectortypes" if args.len() == 1 => Some(ColumnType::Text(None)),
        "acldefault" => Some(aclitem_array()),
        "aclexplode" | "pg_options_to_table" => record(name),
        "generate_subscripts"
        | "array_position"
        | "array_ndims"
        | "array_length"
        | "array_lower"
        | "array_upper"
        | "cardinality"
            if array.is_some() || first.is_none() =>
        {
            int4
        }
        "array_positions" if array.is_some() || first.is_none() => Some(array_of(ColumnType::Int4)),
        "array_dims" | "array_to_string" if array.is_some() || first.is_none() => {
            Some(ColumnType::Text(None))
        }
        "array_fill" => first.map(array_of),
        // A vector's is its elements' array's, as anycompatiblearray finds it.
        "array_remove" | "array_replace" if array.is_some() || first.is_none() => {
            array.map(crate::composed::array_type)
        }
        "array_reverse" | "array_sort" | "trim_array" | "array_shuffle" | "array_sample"
            if array.is_some() || first.is_none() =>
        {
            array
        }
        "width_bucket" if args.len() == 2 => int4,
        _ if is_array_function(name) => return no_form(),
        _ => return Ok(None),
    }))
}

fn oidvector() -> ColumnType {
    crate::composed::built_in("oidvector").expect("oidvector is built in")
}

/// `oidvectortypes`: each OID's type named as `format_type` names it, 0 `-`, an OID the release's
/// catalog does not name `???`.
fn oid_types(item: &Item, release: u8) -> String {
    let Item::Array { elements, .. } = item else {
        return String::new();
    };
    elements
        .iter()
        .map(|e| match e {
            Item::Scalar(Datum::Int(k)) => {
                let oid = crate::scalars::unsigned_of(*k, 32) as u32;
                let text = crate::regs::reg_text(crate::regs::RegKind::Type, oid, release);
                if oid != 0 && text == oid.to_string() {
                    "???".to_string()
                } else {
                    text
                }
            }
            _ => String::new(),
        })
        .collect::<Vec<String>>()
        .join(", ")
}

/// A draw of a uniform integer below `bound`, from the machine's own random source.
fn draw(bound: u64) -> u64 {
    use std::io::Read;
    let mut b = [0u8; 8];
    let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b));
    if bound == 0 {
        return 0;
    }
    let zone = u64::MAX - u64::MAX % bound;
    let mut x = u64::from_le_bytes(b);
    while x >= zone {
        let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b));
        x = u64::from_le_bytes(b);
    }
    x % bound
}

/// An array argument: its type, its element type and its value; none where it is NULL.
fn array_arg<'v>(name: &str, v: &'v Value) -> Result<Option<(ColumnType, ColumnType, &'v Item)>> {
    match v {
        v if v.is_null() => Ok(None),
        Value::Composed(ty, c) => match super::typed::element_of(*ty) {
            Some(e) => Ok(Some((*ty, e, &c.item))),
            None => Err(no_such(name, std::slice::from_ref(v))),
        },
        Value::Text(_) => Err(Error::new(
            "could not determine polymorphic type because input has type unknown",
        )),
        other => Err(no_such(name, std::slice::from_ref(other))),
    }
}

fn no_such(name: &str, args: &[Value]) -> Error {
    Error::new(format!(
        "function {name}({}) does not exist",
        args.iter()
            .map(super::pipeline::kind_name)
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

/// An argument as an integer, NULL as none.
fn int(v: &Value) -> Result<Option<i32>> {
    Ok(match v {
        v if v.is_null() => None,
        Value::Int(x) => Some(i32::try_from(*x).map_err(|_| Error::new("integer out of range"))?),
        Value::Text(s) => {
            Some(s.trim().parse().map_err(|_| {
                Error::new(format!("invalid input syntax for type integer: \"{s}\""))
            })?)
        }
        other => return Err(Error::new(format!("{other:?} is no integer"))),
    })
}

/// An argument as text, NULL as none.
fn text(v: &Value) -> Option<String> {
    (!v.is_null()).then(|| v.text()).flatten()
}

/// An argument as a boolean, NULL as none.
fn boolean(v: &Value) -> Result<Option<bool>> {
    Ok(match v {
        v if v.is_null() => None,
        Value::Bool(b) => Some(*b),
        Value::Text(s) => Some(super::question::truth(s).ok_or_else(|| {
            Error::new(format!("invalid input syntax for type boolean: \"{s}\""))
        })?),
        other => return Err(Error::new(format!("{other:?} is no boolean"))),
    })
}

/// A value of the int4 array type, or of any array type, as an item.
fn int_array(v: &Value) -> Result<Item> {
    Ok(match v {
        v if v.is_null() => Item::Null,
        Value::Composed(_, c) => c.item.clone(),
        Value::Text(s) => crate::composed::text_in(super::typed::array_of(ColumnType::Int4), s)?,
        other => return Err(Error::new(format!("{other:?} is no integer[]"))),
    })
}

/// The value of a function of arrays over its arguments' values, each with whether it is a quoted
/// literal; none where the snail answers no function of arrays by that name.
pub(super) fn arrays(
    name: &str,
    args: &[(Value, bool)],
    types: &[Option<ColumnType>],
) -> Result<Option<Value>> {
    use crate::arrays as a;
    if !is_array_function(name) && !(name == "width_bucket" && args.len() == 2) {
        return Ok(None);
    }
    let values: Vec<Value> = args.iter().map(|(v, _)| v.clone()).collect();
    let int4 = |x: Option<i32>| x.map_or(Value::Null, |x| Value::Int(i64::from(x)));
    let text_array = super::typed::array_of(ColumnType::Text(None));
    let out = |ty: ColumnType, item: Item| {
        if item.is_null() {
            Value::Null
        } else {
            composed(ty, item)
        }
    };
    Ok(Some(match (name, args) {
        ("string_to_array", [(s, _), (d, _)]) | ("string_to_array", [(s, _), (d, _), _]) => {
            let Some(s) = text(s) else {
                return Ok(Some(Value::Null));
            };
            let null = args.get(2).and_then(|(v, _)| text(v));
            out(
                text_array,
                a::string_to_array(&s, text(d).as_deref(), null.as_deref()),
            )
        }
        ("parse_ident", [(s, _)]) | ("parse_ident", [(s, _), _]) => {
            let strict = match args.get(1) {
                Some((v, _)) => match boolean(v)? {
                    Some(b) => b,
                    None => return Ok(Some(Value::Null)),
                },
                None => true,
            };
            let Some(s) = text(s) else {
                return Ok(Some(Value::Null));
            };
            let parts = a::parse_ident(&s, strict)?;
            let n = parts.len() as i32;
            out(
                text_array,
                Item::Array {
                    elements: parts
                        .into_iter()
                        .map(|p| Item::Scalar(Datum::Text(p)))
                        .collect(),
                    dims: vec![n],
                    lower: vec![1],
                },
            )
        }
        ("acldefault", [(k, _), (o, _)]) => {
            if k.is_null() || o.is_null() {
                return Ok(Some(Value::Null));
            }
            let kind = k.text().unwrap_or_default();
            let owner = match o {
                Value::Scalar(_, key) => crate::scalars::unsigned_of(*key, 32) as u32,
                Value::Int(x) => u32::try_from(*x).map_err(|_| Error::new("OID out of range"))?,
                other => other
                    .text()
                    .and_then(|t| t.trim().parse().ok())
                    .ok_or_else(|| {
                        Error::new(format!("invalid input syntax for type oid: {other:?}"))
                    })?,
            };
            out(
                aclitem_array(),
                a::acldefault(kind.bytes().next().unwrap_or(0), owner)?,
            )
        }
        ("oidvectortypes", [(v, _)]) => {
            let v = match v {
                Value::Text(s) => super::pipeline::composed_value(oidvector(), s)?,
                v => v.clone(),
            };
            match (&v, v.is_null()) {
                (_, true) => Value::Null,
                (Value::Composed(ty, c), _) if *ty == oidvector() => {
                    Value::Text(oid_types(&c.item, super::handed::release_held()))
                }
                _ => return Err(no_such(name, std::slice::from_ref(&v))),
            }
        }
        ("array_fill", [(v, q), (d, _)]) | ("array_fill", [(v, q), (d, _), _]) => {
            // The element's type is the step's, a NULL's too; an untyped literal has none.
            let elem = match types.first().copied().flatten() {
                Some(t) => t,
                None if !*q && !v.is_null() => super::pipeline::type_of_value(v).unwrap(),
                None => {
                    return Err(Error::new(
                        "could not determine polymorphic type because input has type unknown",
                    ))
                }
            };
            let value = part_of(elem, v.clone(), false)?;
            let lower = match args.get(2) {
                Some((l, _)) => Some(int_array(l)?),
                None => None,
            };
            out(
                super::typed::array_of(elem),
                a::fill(elem, &value, &int_array(d)?, lower.as_ref())?,
            )
        }
        ("width_bucket", [(x, qx), (t, _)]) => {
            let Some((_, elem, item)) = array_arg(name, t)? else {
                return Ok(Some(Value::Null));
            };
            let operand = part_of(elem, x.clone(), *qx)?;
            int4(a::width_bucket(elem, &operand, item)?)
        }
        _ => {
            let Some((v0, _)) = args.first() else {
                return Err(no_such(name, &values));
            };
            let Some((ty, elem, item)) = array_arg(name, v0)? else {
                return Ok(Some(Value::Null));
            };
            let element = |i: usize| -> Result<Item> {
                let (v, q) = &args[i];
                part_of(elem, v.clone(), *q)
            };
            match (name, args.len()) {
                ("array_dims", 1) => a::dims_text(item).map_or(Value::Null, Value::Text),
                ("array_ndims", 1) => int4(a::ndims(item)),
                ("cardinality", 1) => int4(a::cardinality(item)),
                ("array_length" | "array_lower" | "array_upper", 2) => {
                    let Some(d) = int(&values[1])? else {
                        return Ok(Some(Value::Null));
                    };
                    int4(match name {
                        "array_length" => a::length(item, d),
                        "array_lower" => a::lower(item, d),
                        _ => a::upper(item, d),
                    })
                }
                ("array_position", 2) => int4(a::position(elem, item, &element(1)?, None)?),
                ("array_position", 3) => int4(a::position(
                    elem,
                    item,
                    &element(1)?,
                    Some(int(&values[2])?),
                )?),
                ("array_positions", 2) => out(
                    super::typed::array_of(ColumnType::Int4),
                    a::positions(elem, item, &element(1)?)?,
                ),
                ("array_remove", 2) => out(
                    crate::composed::array_type(ty),
                    a::remove(elem, item, &element(1)?)?,
                ),
                ("array_replace", 3) => out(
                    crate::composed::array_type(ty),
                    a::replace(elem, item, &element(1)?, &element(2)?)?,
                ),
                ("array_reverse", 1) => out(ty, a::reverse(item)),
                ("array_sort", 1..=3) => {
                    let descending = match values.get(1) {
                        Some(v) => match boolean(v)? {
                            Some(b) => b,
                            None => return Ok(Some(Value::Null)),
                        },
                        None => false,
                    };
                    let nulls_first = match values.get(2) {
                        Some(v) => match boolean(v)? {
                            Some(b) => b,
                            None => return Ok(Some(Value::Null)),
                        },
                        None => descending,
                    };
                    out(ty, a::sort(elem, item, descending, nulls_first)?)
                }
                ("trim_array", 2) => match int(&values[1])? {
                    Some(n) => out(ty, a::trim(item, n)?),
                    None => Value::Null,
                },
                ("array_shuffle", 1) => out(ty, a::shuffle(item, &mut draw)),
                ("array_sample", 2) => match int(&values[1])? {
                    Some(n) => out(ty, a::sample(item, n, &mut draw)?),
                    None => Value::Null,
                },
                ("array_to_string", 2 | 3) => {
                    let Some(d) = text(&values[1]) else {
                        return Ok(Some(Value::Null));
                    };
                    let null = values.get(2).and_then(text);
                    a::to_string(elem, item, &d, null.as_deref()).map_or(Value::Null, Value::Text)
                }
                _ => return Err(no_such(name, &values)),
            }
        }
    }))
}

/// The values of a set-returning function of arrays over its arguments' values; none where it is
/// not one of them.
pub(super) fn sets(name: &str, args: &[Value]) -> Result<Option<Vec<Value>>> {
    let rows = |ty: ColumnType, rows: Vec<Vec<Item>>| {
        rows.into_iter()
            .map(|r| composed(ty, Item::Record(r)))
            .collect()
    };
    Ok(Some(match (name, args) {
        // string_to_array's elements, one a row; none of a NULL string.
        ("string_to_table", [s, d]) | ("string_to_table", [s, d, _]) => {
            let Some(s) = text(s) else {
                return Ok(Some(Vec::new()));
            };
            let null = args.get(2).and_then(text);
            match crate::arrays::string_to_array(&s, text(d).as_deref(), null.as_deref()) {
                Item::Array { elements, .. } => elements
                    .iter()
                    .map(|e| super::pipeline::part_value(ColumnType::Text(None), e))
                    .collect(),
                _ => Vec::new(),
            }
        }
        ("generate_subscripts", [a, d]) | ("generate_subscripts", [a, d, _]) => {
            let Some((_, _, item)) = array_arg(name, a)? else {
                return Ok(Some(Vec::new()));
            };
            let reverse = match args.get(2) {
                Some(v) => match boolean(v)? {
                    Some(b) => b,
                    None => return Ok(Some(Vec::new())),
                },
                None => false,
            };
            match int(d)? {
                Some(d) => crate::arrays::subscripts(item, d, reverse)
                    .into_iter()
                    .map(|s| Value::Int(i64::from(s)))
                    .collect(),
                None => Vec::new(),
            }
        }
        ("aclexplode", [a]) => {
            let Some((_, _, item)) = array_arg(name, a)? else {
                return Ok(Some(Vec::new()));
            };
            let oid =
                |o: u32| Item::Scalar(Datum::Int(crate::scalars::unsigned_key(u64::from(o), 32)));
            rows(
                record(name).unwrap(),
                crate::arrays::aclexplode(item)?
                    .into_iter()
                    .map(|(grantor, grantee, privilege, grantable)| {
                        vec![
                            oid(grantor),
                            oid(grantee),
                            Item::Scalar(Datum::Text(privilege.into())),
                            Item::Scalar(Datum::Int(i64::from(grantable))),
                        ]
                    })
                    .collect(),
            )
        }
        ("pg_options_to_table", [a]) => {
            let item = match a {
                a if a.is_null() => return Ok(Some(Vec::new())),
                Value::Composed(_, c) => c.item.clone(),
                Value::Text(s) => {
                    crate::composed::text_in(super::typed::array_of(ColumnType::Text(None)), s)?
                }
                other => return Err(no_such(name, std::slice::from_ref(other))),
            };
            rows(
                record(name).unwrap(),
                crate::arrays::options_to_table(&item)?
                    .into_iter()
                    .map(|(k, v)| {
                        vec![
                            Item::Scalar(Datum::Text(k)),
                            v.map_or(Item::Null, |v| Item::Scalar(Datum::Text(v))),
                        ]
                    })
                    .collect(),
            )
        }
        _ => return Ok(None),
    }))
}
