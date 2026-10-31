//! The functions of ranges and multiranges a pipeline answers by name, as rangetypes.c and
//! multirangetypes.c compute them: their constructors, their bounds and the tests of them, the
//! smallest range holding a multirange, the subtype's difference and canonical form, and the sets
//! and aggregates of ranges.

use super::pipeline::{composed, part_of, part_value, Value};
use crate::catalog::ColumnType;
use crate::composed::{self as c, Composed, Item};
use crate::{Error, Result};

/// The names of the functions of ranges the snail answers.
pub(super) fn is_range_function(name: &str) -> bool {
    matches!(
        name,
        "lower"
            | "upper"
            | "isempty"
            | "lower_inc"
            | "upper_inc"
            | "lower_inf"
            | "upper_inf"
            | "multirange"
            | "range_merge"
    ) || constructed(name).is_some()
        || subdiff(name).is_some()
        || canonical(name).is_some()
}

/// The range or multirange type a constructor of that name makes.
pub(super) fn constructed(name: &str) -> Option<ColumnType> {
    let name = name.strip_prefix("pg_catalog.").unwrap_or(name);
    c::BUILT_IN_RANGES
        .iter()
        .any(|&(r, m, _, _)| name == r || name == m)
        .then(|| c::built_in(name))
        .flatten()
}

/// The range type of a subtype difference function: `int4range_subdiff` is int4range's.
fn subdiff(name: &str) -> Option<ColumnType> {
    name.strip_suffix("_subdiff").and_then(constructed)
}

/// The range type of a canonical function: int4range's, int8range's and daterange's.
fn canonical(name: &str) -> Option<ColumnType> {
    name.strip_suffix("_canonical").and_then(constructed)
}

/// What a range or multirange is: its ranges' type, and whether it is a multirange.
fn ranged(ty: ColumnType) -> Option<(ColumnType, bool)> {
    let ColumnType::Composed(r) = ty else {
        return None;
    };
    match r.def() {
        Composed::Range { .. } => Some((ty, false)),
        Composed::Multirange { range, .. } => Some((*range, true)),
        _ => None,
    }
}

/// A value's ranges and its range type: a range's own, none where it is empty; a multirange's.
fn ranges_of(v: &Value) -> Option<(ColumnType, bool, Vec<Item>)> {
    let Value::Composed(ty, cv) = v else {
        return None;
    };
    let (range, multi) = ranged(*ty)?;
    let ranges = match &cv.item {
        Item::Multirange(rs) => rs.clone(),
        Item::Range { flags, .. } if flags & c::EMPTY != 0 => Vec::new(),
        r @ Item::Range { .. } => vec![r.clone()],
        _ => return None,
    };
    Some((range, multi, ranges))
}

/// The type a function of ranges returns over its arguments' types, or its refusal where it has no
/// form for them; none where the name is no function of ranges, or the step leaves the type open.
pub(super) fn typed(name: &str, args: &[Option<ColumnType>]) -> Result<Option<Option<ColumnType>>> {
    if !is_range_function(name) {
        return Ok(None);
    }
    let no_form = || Err(super::typed::no_form(name, args));
    if let Some(ty) = constructed(name) {
        return Ok(Some(Some(ty)));
    }
    if subdiff(name).is_some() {
        return Ok(Some(Some(ColumnType::Float8)));
    }
    if let Some(ty) = canonical(name) {
        return Ok(Some(Some(ty)));
    }
    let [t] = args else {
        return Ok(None);
    };
    let Some((range, multi)) = t.and_then(ranged) else {
        // lower and upper of a string are the strings' functions.
        return if matches!(name, "lower" | "upper") || t.is_none() {
            Ok(None)
        } else {
            no_form()
        };
    };
    let subtype = c::range_of(range).map(|(s, _)| s);
    Ok(Some(match name {
        "lower" | "upper" => subtype,
        "multirange" if !multi => c::multirange_of(range),
        "range_merge" if multi => Some(range),
        "multirange" | "range_merge" => return no_form(),
        _ => Some(ColumnType::Boolean),
    }))
}

/// The bound flags a constructor's third argument names.
fn bound_flags(s: &str) -> Result<u8> {
    Ok(match s {
        "()" => 0,
        "(]" => c::UB_INC,
        "[)" => c::LB_INC,
        "[]" => c::LB_INC | c::UB_INC,
        _ => {
            return Err(Error::new(
                "invalid range bound flags\nHINT:  Valid values are \"[]\", \"[)\", \"(]\", and \"()\".",
            ))
        }
    })
}

/// The value of a function of ranges over its arguments' values, each with whether it is a quoted
/// literal; none where the snail answers no function of ranges by that name and those arguments.
pub(super) fn ranges(name: &str, args: &[(Value, bool)]) -> Result<Option<Value>> {
    if !is_range_function(name) {
        return Ok(None);
    }
    if let Some(ty) = constructed(name) {
        return construct(ty, args).map(Some);
    }
    if let Some(ty) = subdiff(name) {
        let (subtype, _) = c::range_of(ty).unwrap();
        let [(a, qa), (b, qb)] = args else {
            return Ok(None);
        };
        if a.is_null() || b.is_null() {
            return Ok(Some(Value::Null));
        }
        let (a, b) = (
            part_value(subtype, &part_of(subtype, a.clone(), *qa)?),
            part_value(subtype, &part_of(subtype, b.clone(), *qb)?),
        );
        return Ok(Some(Value::Double(match (&a, &b) {
            (Value::Scalar(t, x), Value::Scalar(_, y)) if !matches!(t, ColumnType::Date) => {
                (*x as f64 - *y as f64) / 1_000_000.0
            }
            (Value::Scalar(_, x), Value::Scalar(_, y)) => *x as f64 - *y as f64,
            (Value::Int(x), Value::Int(y)) => *x as f64 - *y as f64,
            _ => {
                let d = super::pipeline::arith(&sqlparser::ast::BinaryOperator::Minus, &a, &b)?;
                match super::pipeline::float_cast(d, true)? {
                    Value::Double(f) => f,
                    other => return Err(Error::new(format!("{other:?} is no double precision"))),
                }
            }
        })));
    }
    if let Some(ty) = canonical(name) {
        let [(v, q)] = args else {
            return Ok(None);
        };
        return Ok(Some(match v {
            v if v.is_null() => Value::Null,
            Value::Composed(t, _) if *t == ty => v.clone(),
            Value::Text(s) if *q => super::pipeline::composed_value(ty, s)?,
            other => {
                return Err(Error::new(format!(
                    "function {name}({}) does not exist",
                    super::pipeline::kind_name(other)
                )))
            }
        }));
    }
    let [(v, _)] = args else {
        return Ok(None);
    };
    if v.is_null() {
        return Ok(Some(Value::Null));
    }
    let Some((range, multi, rs)) = ranges_of(v) else {
        return Ok(None);
    };
    let (subtype, _) = c::range_of(range).unwrap();
    let flags = |r: Option<&Item>| match r {
        Some(Item::Range { flags, .. }) => *flags,
        _ => c::EMPTY,
    };
    let (first, last) = (rs.first(), rs.last());
    let bound = |r: Option<&Item>, upper: bool| match r {
        Some(Item::Range {
            lower, upper: hi, ..
        }) => match if upper { hi } else { lower } {
            Some(b) => part_value(subtype, b),
            None => Value::Null,
        },
        _ => Value::Null,
    };
    Ok(Some(match name {
        "lower" => bound(first, false),
        "upper" => bound(last, true),
        "isempty" => Value::Bool(rs.is_empty()),
        "lower_inc" => Value::Bool(flags(first) & c::LB_INC != 0),
        "upper_inc" => Value::Bool(flags(last) & c::UB_INC != 0),
        "lower_inf" => Value::Bool(flags(first) & c::LB_INF != 0),
        "upper_inf" => Value::Bool(flags(last) & c::UB_INF != 0),
        "multirange" if !multi => {
            let ty = c::multirange_of(range)
                .ok_or_else(|| Error::new(format!("{} has no multirange", range.pg_name())))?;
            composed(ty, Item::Multirange(rs))
        }
        "range_merge" if multi => composed(range, c::span_of(&rs)),
        _ => return Ok(None),
    }))
}

/// A range or multirange its constructor makes: a range of two bounds, a NULL one unbounded, its
/// flags `[)` or those named; a multirange of the ranges given, none NULL.
fn construct(ty: ColumnType, args: &[(Value, bool)]) -> Result<Value> {
    let (range, multi) = ranged(ty).expect("a constructor makes a range or a multirange");
    if multi {
        // Of one range, the strict multirange_constructor1.
        if let [(v, _)] = args {
            if v.is_null() {
                return Ok(Value::Null);
            }
        }
        let mut rs = Vec::with_capacity(args.len());
        for (v, q) in args {
            let item = match v {
                v if v.is_null() => {
                    return Err(Error::new("multirange values cannot contain null members"))
                }
                Value::Composed(t, cv) if *t == range => cv.item.clone(),
                Value::Text(_) if *q => part_of(range, v.clone(), true)?,
                other => {
                    return Err(Error::new(format!(
                        "function {}({}) does not exist",
                        ty.pg_name(),
                        super::pipeline::kind_name(other)
                    )))
                }
            };
            rs.push(item);
        }
        return Ok(composed(
            ty,
            Item::Multirange(c::canonical_ranges(range, rs)),
        ));
    }
    let (subtype, discrete) = c::range_of(range).unwrap();
    let flags = match args {
        [_, _] => c::LB_INC,
        [_, _, (f, _)] => match f {
            Value::Text(s) => bound_flags(s)?,
            v if v.is_null() => {
                return Err(Error::new(
                    "range constructor flags argument must not be null",
                ))
            }
            other => bound_flags(&other.text().unwrap_or_default())?,
        },
        _ => {
            return Err(Error::new(format!(
                "function {}({} arguments) does not exist",
                ty.pg_name(),
                args.len()
            )))
        }
    };
    let bound = |(v, q): &(Value, bool)| -> Result<Option<Item>> {
        Ok(match part_of(subtype, v.clone(), *q)? {
            Item::Null => None,
            i => Some(i),
        })
    };
    let item = c::make_range(subtype, discrete, flags, bound(&args[0])?, bound(&args[1])?)?;
    Ok(composed(ty, item))
}
