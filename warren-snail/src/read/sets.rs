//! The set-returning functions a step's select list reads: each call's values, one a row it makes.

use super::pipeline::{part_value, Value};
use crate::catalog::ColumnType;
use crate::composed::Item;
use crate::{Error, Result};

/// The values a set-returning call gives over its arguments' values, `ty` the type the step
/// resolved it to: none for a NULL argument where the function is strict.
pub(super) fn values(
    name: &str,
    given: Vec<(Value, bool)>,
    ty: Option<ColumnType>,
    casing: crate::catalog::Casing,
) -> Result<Vec<Value>> {
    let _ = ty;
    if let Some(v) = regexps(name, &given, casing)? {
        return Ok(v);
    }
    if let Some(v) = super::json::sets(name, &given)? {
        return Ok(v);
    }
    if let Some(v) = super::tsearch::sets(name, &given)? {
        return Ok(v);
    }
    let resolved: Vec<_> = given.iter().map(|(v, q)| (v.clone(), *q, None)).collect();
    if let Some(v) = super::rest::set(name, &resolved)? {
        return Ok(v);
    }
    if let Some(rows) = super::rest::rows(name, &resolved)? {
        let ty = crate::composed::type_of(crate::composed::Composed::Record {
            name: "record".into(),
            fields: super::rest::KEYWORD_COLUMNS
                .iter()
                .map(|(n, t)| crate::composed::Field::new(n, *t))
                .collect(),
        });
        return rows
            .into_iter()
            .map(|r| {
                let items = r
                    .into_iter()
                    .zip(super::rest::KEYWORD_COLUMNS)
                    .map(|(v, (_, t))| super::pipeline::part_of(t, v, false))
                    .collect::<Result<Vec<_>>>()?;
                Ok(super::pipeline::composed(ty, Item::Record(items)))
            })
            .collect();
    }
    // A quoted literal beside a range or multirange is read as its type.
    let first = given.first().and_then(|(v, _)| match v {
        Value::Composed(t, _) => Some(*t),
        _ => None,
    });
    let args = given
        .into_iter()
        .map(|(v, q)| match (&v, first) {
            (Value::Text(s), Some(t)) if q && super::typed::element_of(t).is_none() => {
                super::pipeline::composed_value(t, s)
            }
            _ => Ok(v),
        })
        .collect::<Result<Vec<_>>>()?;
    if let Some(v) = super::arrays::sets(name, &args)? {
        return Ok(v);
    }
    match (name, args.as_slice()) {
        ("unnest", [v]) if v.is_null() => Ok(Vec::new()),
        ("range_minus_multi" | "multirange_minus_multi", [a, b]) => minus_multi(name, a, b),
        ("unnest", [Value::Composed(t, c)]) => match (super::typed::element_of(*t), &c.item) {
            (Some(element), Item::Array { elements, .. }) => {
                Ok(elements.iter().map(|e| part_value(element, e)).collect())
            }
            // A multirange's ranges, in order.
            (None, Item::Multirange(ranges)) => {
                let range = super::typed::multirange_range(*t).unwrap();
                Ok(ranges.iter().map(|r| part_value(range, r)).collect())
            }
            _ => Err(Error::new(format!(
                "function unnest({}) does not exist",
                t.pg_name()
            ))),
        },
        _ => Err(Error::new(format!(
            "{name}({}) is not read",
            args.iter()
                .map(super::pipeline::kind_name)
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// What range_minus_multi and multirange_minus_multi give: the ranges left of the first range
/// once the second is taken from it, none empty; the multirange left of the first, none where it
/// is empty.
fn minus_multi(name: &str, a: &Value, b: &Value) -> Result<Vec<Value>> {
    use crate::composed::{self as c, Composed};
    if a.is_null() || b.is_null() {
        return Ok(Vec::new());
    }
    let (Value::Composed(ty, x), Value::Composed(tb, y)) = (a, b) else {
        return Err(Error::new(format!(
            "function {name}({}, {}) does not exist",
            super::pipeline::kind_name(a),
            super::pipeline::kind_name(b)
        )));
    };
    let ColumnType::Composed(r) = ty else {
        unreachable!()
    };
    let ranges = |i: &Item| match i {
        Item::Multirange(rs) => rs.clone(),
        Item::Range { flags, .. } if flags & c::EMPTY != 0 => Vec::new(),
        r => vec![r.clone()],
    };
    let range = match (name, r.def()) {
        ("range_minus_multi", Composed::Range { .. }) if ty == tb => *ty,
        ("multirange_minus_multi", Composed::Multirange { range, .. }) if ty == tb => *range,
        _ => {
            return Err(Error::new(format!(
                "function {name}({}, {}) does not exist",
                ty.pg_name(),
                tb.pg_name()
            )))
        }
    };
    let (xs, ys) = (ranges(&x.item), ranges(&y.item));
    let left = if xs.is_empty() || ys.is_empty() {
        xs
    } else {
        c::multirange_minus(range, &xs, &ys)?
    };
    Ok(if name == "range_minus_multi" {
        left.iter().map(|r| part_value(range, r)).collect()
    } else if left.is_empty() {
        Vec::new()
    } else {
        vec![super::pipeline::composed(*ty, Item::Multirange(left))]
    })
}

/// The rows of regexp_matches and regexp_split_to_table, under the collation the call's argument
/// carries; none where the name is neither.
fn regexps(
    name: &str,
    given: &[(Value, bool)],
    casing: crate::catalog::Casing,
) -> Result<Option<Vec<Value>>> {
    if !matches!(name, "regexp_matches" | "regexp_split_to_table") {
        return Ok(None);
    }
    let text = |v: &Value| -> Result<Option<String>> {
        Ok(match v {
            v if v.is_null() => None,
            // A bpchar as it casts to text, its trailing blanks gone.
            Value::Str(ColumnType::Bpchar(_), s) => Some(s.trim_end_matches(' ').to_string()),
            Value::Text(s) | Value::Str(_, s) => Some(s.clone()),
            other => {
                return Err(Error::new(format!(
                    "function {name}({}) does not exist",
                    super::pipeline::kind_name(other)
                )))
            }
        })
    };
    let (s, p, flags) = match given {
        [(s, _), (p, _)] => (text(s)?, text(p)?, None),
        [(s, _), (p, _), (f, _)] => match text(f)? {
            Some(f) => (text(s)?, text(p)?, Some(f)),
            None => return Ok(Some(Vec::new())),
        },
        _ => {
            return Err(Error::new(format!(
                "function {name} takes two or three arguments"
            )))
        }
    };
    let (Some(s), Some(p)) = (s, p) else {
        return Ok(Some(Vec::new()));
    };
    let ctype = super::calls::ctype_of(casing)?;
    Ok(Some(if name == "regexp_matches" {
        crate::regexp::regexp_matches(&s, &p, flags.as_deref(), ctype)?
            .into_iter()
            .map(|m| {
                let n = m.len() as i32;
                super::pipeline::composed(
                    super::calls::text_array(),
                    Item::Array {
                        elements: m
                            .into_iter()
                            .map(|x| {
                                x.map_or(Item::Null, |x| {
                                    Item::Scalar(crate::read::statement::Datum::Text(x))
                                })
                            })
                            .collect(),
                        dims: if n == 0 { Vec::new() } else { vec![n] },
                        lower: if n == 0 { Vec::new() } else { vec![1] },
                    },
                )
            })
            .collect()
    } else {
        crate::regexp::regexp_split_to_table(&s, &p, flags.as_deref(), ctype)?
            .into_iter()
            .map(Value::Text)
            .collect()
    }))
}
