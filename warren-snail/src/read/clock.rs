//! The time types in a pipeline: their operators, their casts and their comparisons across types,
//! as PostgreSQL resolves each under the settings the snail answers under.

use super::pipeline::Value;
use crate::catalog::ColumnType;
use crate::times;
use crate::{Error, Result};
use sqlparser::ast::BinaryOperator;
use std::cmp::Ordering;

/// A time type without its precision: the type its operators and comparisons take.
pub(super) fn base(ty: ColumnType) -> ColumnType {
    match ty {
        ColumnType::Time(_) => ColumnType::Time(None),
        ColumnType::Timetz(_) => ColumnType::Timetz(None),
        ColumnType::Timestamp(_) => ColumnType::Timestamp(None),
        ColumnType::TimestamptzRounded(_) => ColumnType::Timestamptz,
        ColumnType::Interval { .. } => full_interval(),
        ty => ty,
    }
}

fn full_interval() -> ColumnType {
    ColumnType::Interval {
        range: times::INTERVAL_FULL_RANGE,
        precision: None,
    }
}

/// A value's time type, without its precision, where it is of one.
pub(super) fn time_of(v: &Value) -> Option<ColumnType> {
    match v {
        Value::Interval(_) => Some(full_interval()),
        Value::Scalar(ty, _) => match base(*ty) {
            t @ (ColumnType::Date
            | ColumnType::Time(_)
            | ColumnType::Timetz(_)
            | ColumnType::Timestamp(_)
            | ColumnType::Timestamptz) => Some(t),
            _ => None,
        },
        _ => None,
    }
}

/// An instant of either kind, a date taken as one where the other side is one, as PostgreSQL's
/// implicit casts take it: a date at midnight, and a timestamp as a timestamptz, each on the statement zone's clock.
fn instants(x: &Value, y: &Value) -> Result<Option<(ColumnType, i64, i64)>> {
    let (Some(a), Some(b)) = (time_of(x), time_of(y)) else {
        return Ok(None);
    };
    let (Value::Scalar(_, p), Value::Scalar(_, q)) = (x, y) else {
        return Ok(None);
    };
    let wide = |t: ColumnType| match t {
        ColumnType::Date => 0,
        ColumnType::Timestamp(_) => 1,
        ColumnType::Timestamptz => 2,
        _ => 3,
    };
    let to = match (wide(a), wide(b)) {
        (3, _) | (_, 3) | (0, 0) => return Ok(None),
        (m, n) => m.max(n),
    };
    let lift = |t: ColumnType, k: i64| -> Result<i64> {
        Ok(match (t, to) {
            (ColumnType::Date, 1) => times::date_timestamp(k as i32)?,
            (ColumnType::Date, _) => times::date_timestamptz(k as i32)?,
            (ColumnType::Timestamp(_), 2) => times::timestamp_timestamptz(k)?,
            _ => k,
        })
    };
    let ty = if to == 1 {
        ColumnType::Timestamp(None)
    } else {
        ColumnType::Timestamptz
    };
    Ok(Some((ty, lift(a, *p)?, lift(b, *q)?)))
}

/// Two values compared where either is of a time type: none where PostgreSQL has no operator for
/// them, so the caller refuses as it would.
pub(super) fn compare(x: &Value, y: &Value) -> Result<Option<Ordering>> {
    match (x, y) {
        (Value::Interval(a), Value::Interval(b)) => Ok(Some(a.total().cmp(&b.total()))),
        (Value::Scalar(a, p), Value::Scalar(b, q)) if base(*a) == base(*b) => Ok(Some(p.cmp(q))),
        _ => match (time_of(x), time_of(y), x, y) {
            (
                Some(ColumnType::Date),
                Some(ColumnType::Timestamp(_)),
                Value::Scalar(_, d),
                Value::Scalar(_, t),
            ) => Ok(Some(times::date_cmp_timestamp(*d as i32, *t))),
            (
                Some(ColumnType::Timestamp(_)),
                Some(ColumnType::Date),
                Value::Scalar(_, t),
                Value::Scalar(_, d),
            ) => Ok(Some(times::timestamp_cmp_date(*t, *d as i32))),
            (
                Some(ColumnType::Date),
                Some(ColumnType::Timestamptz),
                Value::Scalar(_, d),
                Value::Scalar(_, t),
            ) => Ok(Some(times::date_cmp_timestamptz(*d as i32, *t)?)),
            (
                Some(ColumnType::Timestamptz),
                Some(ColumnType::Date),
                Value::Scalar(_, t),
                Value::Scalar(_, d),
            ) => Ok(Some(times::timestamptz_cmp_date(*t, *d as i32)?)),
            (
                Some(ColumnType::Timestamp(_)),
                Some(ColumnType::Timestamptz),
                Value::Scalar(_, a),
                Value::Scalar(_, b),
            ) => Ok(Some(times::timestamp_cmp_timestamptz(*a, *b)?)),
            (
                Some(ColumnType::Timestamptz),
                Some(ColumnType::Timestamp(_)),
                Value::Scalar(_, a),
                Value::Scalar(_, b),
            ) => Ok(Some(times::timestamptz_cmp_timestamp(*a, *b)?)),
            _ => Ok(None),
        },
    }
}

/// A number beside an interval's `*` or `/`, as the double precision PostgreSQL casts it to.
fn factor(v: &Value) -> Option<f64> {
    match v {
        Value::Int(x) => Some(*x as f64),
        Value::Num(m, s) => super::pipeline::numeric_text(*m, *s).parse().ok(),
        Value::Real(f) => Some(f64::from(*f)),
        Value::Double(d) => Some(*d),
        _ => None,
    }
}

fn scalar(ty: ColumnType, k: i64) -> Value {
    Value::Scalar(ty, k)
}

fn timetz(t: (i64, i32)) -> Value {
    scalar(
        ColumnType::Timetz(None),
        crate::scalars::timetz_key(t.0, t.1),
    )
}

/// An operator where either side is of a time type, as PostgreSQL resolves it; none where neither
/// is, and a refusal in PostgreSQL's words where it has no such operator.
pub(super) fn arith(op: &BinaryOperator, x: &Value, y: &Value) -> Result<Option<Value>> {
    use BinaryOperator::{Divide, Minus, Multiply, Plus};
    use ColumnType as T;
    let (a, b) = (time_of(x), time_of(y));
    if a.is_none() && b.is_none() {
        return Ok(None);
    }
    let int = |v: &Value| match v {
        Value::Int(n) => Some(*n),
        _ => None,
    };
    let ts = T::Timestamp(None);
    let v = match (op, a, b, x, y) {
        // date ± integer, integer + date, date − date.
        (Plus, Some(T::Date), None, Value::Scalar(_, d), n) if int(n).is_some() => scalar(
            T::Date,
            i64::from(times::date_pli(*d as i32, int(n).unwrap())?),
        ),
        (Plus, None, Some(T::Date), n, Value::Scalar(_, d)) if int(n).is_some() => scalar(
            T::Date,
            i64::from(times::date_pli(*d as i32, int(n).unwrap())?),
        ),
        (Minus, Some(T::Date), None, Value::Scalar(_, d), n) if int(n).is_some() => scalar(
            T::Date,
            i64::from(times::date_mii(*d as i32, int(n).unwrap())?),
        ),
        (Minus, Some(T::Date), Some(T::Date), Value::Scalar(_, p), Value::Scalar(_, q)) => {
            Value::Int(times::date_mi(*p as i32, *q as i32)?)
        }
        // date ± interval, interval + date: a timestamp.
        (
            Plus,
            Some(T::Date),
            Some(T::Interval { .. }),
            Value::Scalar(_, d),
            Value::Interval(v),
        )
        | (
            Plus,
            Some(T::Interval { .. }),
            Some(T::Date),
            Value::Interval(v),
            Value::Scalar(_, d),
        ) => scalar(ts, times::date_pl_interval(*d as i32, *v)?),
        (
            Minus,
            Some(T::Date),
            Some(T::Interval { .. }),
            Value::Scalar(_, d),
            Value::Interval(v),
        ) => scalar(ts, times::date_mi_interval(*d as i32, *v)?),
        // date + time and time + date: a timestamp; date + timetz and timetz + date: a timestamptz.
        (Plus, Some(T::Date), Some(T::Time(_)), Value::Scalar(_, d), Value::Scalar(_, t))
        | (Plus, Some(T::Time(_)), Some(T::Date), Value::Scalar(_, t), Value::Scalar(_, d)) => {
            scalar(ts, times::datetime_pl(*d as i32, *t)?)
        }
        (Plus, Some(T::Date), Some(T::Timetz(_)), Value::Scalar(_, d), Value::Scalar(_, k))
        | (Plus, Some(T::Timetz(_)), Some(T::Date), Value::Scalar(_, k), Value::Scalar(_, d)) => {
            let (t, z) = crate::scalars::timetz_parts(*k);
            scalar(T::Timestamptz, times::datetimetz_pl(*d as i32, t, z)?)
        }
        // An instant ± interval, interval + instant.
        (
            Plus,
            Some(T::Timestamp(_)),
            Some(T::Interval { .. }),
            Value::Scalar(_, t),
            Value::Interval(v),
        )
        | (
            Plus,
            Some(T::Interval { .. }),
            Some(T::Timestamp(_)),
            Value::Interval(v),
            Value::Scalar(_, t),
        ) => scalar(ts, times::timestamp_pl_interval(*t, *v)?),
        (
            Minus,
            Some(T::Timestamp(_)),
            Some(T::Interval { .. }),
            Value::Scalar(_, t),
            Value::Interval(v),
        ) => scalar(ts, times::timestamp_mi_interval(*t, *v)?),
        (
            Plus,
            Some(T::Timestamptz),
            Some(T::Interval { .. }),
            Value::Scalar(_, t),
            Value::Interval(v),
        )
        | (
            Plus,
            Some(T::Interval { .. }),
            Some(T::Timestamptz),
            Value::Interval(v),
            Value::Scalar(_, t),
        ) => scalar(T::Timestamptz, times::timestamptz_pl_interval(*t, *v)?),
        (
            Minus,
            Some(T::Timestamptz),
            Some(T::Interval { .. }),
            Value::Scalar(_, t),
            Value::Interval(v),
        ) => scalar(T::Timestamptz, times::timestamptz_mi_interval(*t, *v)?),
        // time ± interval, interval + time, time − time; timetz ± interval, interval + timetz.
        (
            Plus,
            Some(T::Time(_)),
            Some(T::Interval { .. }),
            Value::Scalar(_, t),
            Value::Interval(v),
        )
        | (
            Plus,
            Some(T::Interval { .. }),
            Some(T::Time(_)),
            Value::Interval(v),
            Value::Scalar(_, t),
        ) => scalar(T::Time(None), times::time_pl_interval(*t, *v)?),
        (
            Minus,
            Some(T::Time(_)),
            Some(T::Interval { .. }),
            Value::Scalar(_, t),
            Value::Interval(v),
        ) => scalar(T::Time(None), times::time_mi_interval(*t, *v)?),
        (Minus, Some(T::Time(_)), Some(T::Time(_)), Value::Scalar(_, p), Value::Scalar(_, q)) => {
            Value::Interval(times::time_mi_time(*p, *q))
        }
        (
            Plus,
            Some(T::Timetz(_)),
            Some(T::Interval { .. }),
            Value::Scalar(_, k),
            Value::Interval(v),
        )
        | (
            Plus,
            Some(T::Interval { .. }),
            Some(T::Timetz(_)),
            Value::Interval(v),
            Value::Scalar(_, k),
        ) => {
            let (t, z) = crate::scalars::timetz_parts(*k);
            timetz(times::timetz_pl_interval(t, z, *v)?)
        }
        (
            Minus,
            Some(T::Timetz(_)),
            Some(T::Interval { .. }),
            Value::Scalar(_, k),
            Value::Interval(v),
        ) => {
            let (t, z) = crate::scalars::timetz_parts(*k);
            timetz(times::timetz_mi_interval(t, z, *v)?)
        }
        // interval ± interval, and by a number.
        (Plus, _, _, Value::Interval(p), Value::Interval(q)) => {
            Value::Interval(times::interval_pl(*p, *q)?)
        }
        (Minus, _, _, Value::Interval(p), Value::Interval(q)) => {
            Value::Interval(times::interval_mi(*p, *q)?)
        }
        (Multiply, _, None, Value::Interval(v), f) | (Multiply, None, _, f, Value::Interval(v))
            if factor(f).is_some() =>
        {
            Value::Interval(times::interval_mul(*v, factor(f).unwrap())?)
        }
        (Divide, _, None, Value::Interval(v), f) if factor(f).is_some() => {
            Value::Interval(times::interval_div(*v, factor(f).unwrap())?)
        }
        // An instant less another, the date or the timestamp taken as the wider: an interval.
        (Minus, Some(_), Some(_), _, _) => match instants(x, y)? {
            Some((_, p, q)) => Value::Interval(times::timestamp_mi(p, q)?),
            None => return Err(no_operator(op, x, y)),
        },
        _ => return Err(no_operator(op, x, y)),
    };
    Ok(Some(v))
}

fn no_operator(op: &BinaryOperator, x: &Value, y: &Value) -> Error {
    Error::new(format!(
        "operator does not exist: {} {op} {}",
        super::pipeline::kind_name(x),
        super::pipeline::kind_name(y)
    ))
}

/// A value's unary minus, where it is an interval.
pub(super) fn negate(v: &Value) -> Result<Option<Value>> {
    match v {
        Value::Interval(x) => Ok(Some(Value::Interval(times::interval_um(*x)?))),
        _ => Ok(None),
    }
}

/// Whether a type is one of the time types.
pub(super) fn is_time(ty: ColumnType) -> bool {
    matches!(
        ty,
        ColumnType::Date
            | ColumnType::Time(_)
            | ColumnType::Timetz(_)
            | ColumnType::Timestamp(_)
            | ColumnType::Timestamptz
            | ColumnType::TimestamptzRounded(_)
            | ColumnType::Interval { .. }
    )
}

/// A value cast to a time type `to`, as PostgreSQL's casts take it: text by the type's input, and
/// between the time types by pg_cast's rows, rounded to the precision `to` names.
pub(super) fn cast(v: Value, to: ColumnType) -> Result<Value> {
    use ColumnType as T;
    let refused = |v: &Value| {
        Error::new(format!(
            "cannot cast type {} to {}",
            super::pipeline::kind_name(v),
            to.pg_name()
        ))
    };
    let p = |p: Option<u8>| p.map(u32::from);
    if let Value::Text(s) = &v {
        return Ok(match to {
            T::Interval { .. } => {
                // A cast from text reads the text whole, then cuts it to the fields named.
                let (range, precision) = match to {
                    T::Interval { range, precision } => (range, precision),
                    _ => unreachable!(),
                };
                let x = times::interval_in(s, times::INTERVAL_FULL_RANGE, None)?;
                Value::Interval(times::round_interval(x, range, p(precision))?)
            }
            to => scalar(to, crate::scalars::scalar_parse(to, s)?),
        });
    }
    let from = time_of(&v).ok_or_else(|| refused(&v))?;
    let null = |x: Option<Value>| x.unwrap_or(Value::Null);
    Ok(match (from, to, &v) {
        (T::Interval { .. }, T::Interval { range, precision }, Value::Interval(x)) => {
            Value::Interval(times::round_interval(*x, range, p(precision))?)
        }
        (T::Interval { .. }, T::Time(q), Value::Interval(x)) => {
            let t = times::interval_time(*x)?;
            scalar(T::Time(q), p(q).map_or(t, |q| times::round_time(t, q)))
        }
        (_, _, Value::Scalar(_, k)) => {
            let k = *k;
            let rounded = |t: i64, q: Option<u8>| -> Result<i64> {
                match p(q) {
                    Some(q) => times::round_instant(t, q),
                    None => Ok(t),
                }
            };
            match (from, to) {
                (T::Date, T::Date) => v,
                (T::Date, T::Timestamp(q)) => {
                    scalar(to, rounded(times::date_timestamp(k as i32)?, q)?)
                }
                (T::Date, T::Timestamptz) => scalar(to, times::date_timestamptz(k as i32)?),
                (T::Date, T::TimestamptzRounded(q)) => {
                    scalar(to, rounded(times::date_timestamptz(k as i32)?, Some(q))?)
                }
                (T::Timestamp(_), T::Date) => scalar(to, i64::from(times::timestamp_date(k)?)),
                (T::Timestamptz, T::Date) => scalar(to, i64::from(times::timestamptz_date(k)?)),
                (T::Timestamp(_), T::Timestamp(q)) => scalar(to, rounded(k, q)?),
                (T::Timestamp(_), T::Timestamptz) => scalar(to, times::timestamp_timestamptz(k)?),
                (T::Timestamp(_), T::TimestamptzRounded(q)) => {
                    scalar(to, rounded(times::timestamp_timestamptz(k)?, Some(q))?)
                }
                (T::Timestamptz, T::Timestamptz) => v,
                (T::Timestamptz, T::TimestamptzRounded(q)) => scalar(to, rounded(k, Some(q))?),
                (T::Timestamptz, T::Timestamp(q)) => {
                    scalar(to, rounded(times::timestamptz_timestamp(k)?, q)?)
                }
                (T::Timestamp(_), T::Time(q)) => null(
                    times::timestamp_time(k)?
                        .map(|t| scalar(to, p(q).map_or(t, |q| times::round_time(t, q)))),
                ),
                (T::Timestamptz, T::Time(q)) => null(
                    times::timestamptz_time(k)?
                        .map(|t| scalar(to, p(q).map_or(t, |q| times::round_time(t, q)))),
                ),
                (T::Timestamptz, T::Timetz(q)) => {
                    null(times::timestamptz_timetz(k)?.map(|(t, z)| {
                        timetz(p(q).map_or((t, z), |q| times::round_timetz(t, z, q)))
                    }))
                }
                (T::Time(_), T::Time(q)) => scalar(to, p(q).map_or(k, |q| times::round_time(k, q))),
                (T::Time(_), T::Timetz(q)) => {
                    let (t, z) = times::time_timetz(k)?;
                    timetz(p(q).map_or((t, z), |q| times::round_timetz(t, z, q)))
                }
                (T::Time(_), T::Interval { range, precision }) => Value::Interval(
                    times::round_interval(times::time_interval(k), range, p(precision))?,
                ),
                (T::Timetz(_), T::Timetz(q)) => {
                    let (t, z) = crate::scalars::timetz_parts(k);
                    timetz(p(q).map_or((t, z), |q| times::round_timetz(t, z, q)))
                }
                (T::Timetz(_), T::Time(q)) => {
                    let (t, z) = crate::scalars::timetz_parts(k);
                    let t = times::timetz_time(t, z);
                    scalar(to, p(q).map_or(t, |q| times::round_time(t, q)))
                }
                _ => return Err(refused(&v)),
            }
        }
        _ => return Err(refused(&v)),
    })
}

/// An instant past either end of what the snail holds, refused as PostgreSQL refuses one past its
/// range, and from the snail's last instant on by name.
fn held_instant(t: i128) -> Result<i64> {
    let first = (times::date2j(-4713, 11, 24) - times::UNIX_EPOCH_JDATE) as i128
        * times::MICROS_PER_DAY as i128;
    if t < first {
        return Err(Error::new("timestamp out of range"));
    }
    if t >= i128::from(i64::MAX) {
        return Err(Error::new(
            "timestamp out of range: past the snail's last instant, 294247-01-10 04:00:54.775806",
        ));
    }
    Ok(t as i64)
}

/// The record of a row of pg_timezone_names, the zone's name first, or of the abbreviations'
/// functions: an abbreviation, its offset east of UTC and whether it is daylight time.
pub(super) fn zones_record(named: bool) -> ColumnType {
    use crate::composed::{Composed, Field};
    let text = ColumnType::Text(None);
    let mut fields = Vec::with_capacity(4);
    if named {
        fields.push(Field::new("name", text));
    }
    fields.extend([
        Field::new("abbrev", text),
        Field::new("utc_offset", full_interval()),
        Field::new("is_dst", ColumnType::Boolean),
    ]);
    crate::composed::type_of(Composed::Record {
        name: "record".into(),
        fields,
    })
}

/// The rows of pg_timezone_names, of pg_timezone_abbrevs (pg_timezone_abbrevs_abbrevs from 18) and
/// of pg_timezone_abbrevs_zone, at the statement's clock `now`, microseconds after 1970-01-01 00:00
/// UTC: every zone compiled beside the server with its abbreviation, offset and daylight time then;
/// PostgreSQL's set of abbreviations in its order, a dynamic one as its zone means it then; the
/// statement zone's own. None where the name is none of them.
pub(super) fn zone_sets(name: &str, now: i64) -> Result<Option<Vec<Value>>> {
    let utc = now.div_euclid(1_000_000);
    let interval = |east: i32| {
        Value::Interval(times::Interval {
            months: 0,
            days: 0,
            micros: i64::from(east) * 1_000_000,
        })
    };
    let row = |abbrev: &str, east: i32, dst: bool| {
        vec![Value::Text(abbrev.to_string()), interval(east), Value::Bool(dst)]
    };
    let rows: Vec<Vec<Value>> = match name {
        "pg_timezone_names" => crate::zones::every_zone_at(utc)?
            .into_iter()
            .map(|(zone, abbrev, east, dst)| {
                let mut r = vec![Value::Text(zone)];
                r.extend(row(&abbrev, east, dst));
                r
            })
            .collect(),
        "pg_timezone_abbrevs" | "pg_timezone_abbrevs_abbrevs" => times::abbreviations()
            .map(|(abbrev, names)| {
                let (east, dst) = match names {
                    times::Abbreviated::Fixed(east, dst) => (*east, *dst),
                    times::Abbreviated::Zone(zone) => times::abbrev_utc_offset(zone, abbrev, utc)?,
                };
                Ok(row(&abbrev.to_ascii_uppercase(), east, dst))
            })
            .collect::<Result<_>>()?,
        "pg_timezone_abbrevs_zone" => crate::zones::session_abbrevs_at(utc)?
            .into_iter()
            .map(|(abbrev, east, dst)| row(&abbrev, east, dst))
            .collect(),
        _ => return Ok(None),
    };
    let ty = zones_record(name == "pg_timezone_names");
    let crate::catalog::ColumnType::Composed(r) = ty else {
        unreachable!("a record")
    };
    let crate::composed::Composed::Record { fields, .. } = r.def() else {
        unreachable!("a record")
    };
    rows.into_iter()
        .map(|r| {
            let items = r
                .into_iter()
                .zip(fields.iter())
                .map(|(v, f)| super::pipeline::part_of(f.ty, v, false))
                .collect::<Result<Vec<_>>>()?;
            Ok(super::pipeline::composed(ty, crate::composed::Item::Record(items)))
        })
        .collect::<Result<Vec<_>>>()
        .map(Some)
}

/// A value AT TIME ZONE `zone`, as PostgreSQL's timestamptz_zone and timestamp_zone take it: an
/// instant as the zone's clock shows it, and a time on the zone's clock as the instant it is. A zone
/// other than UTC is read only under the feature zones; an interval as `at_interval` reads it. A
/// date is the instant of its midnight, as PostgreSQL resolves it to timezone(…, timestamptz); a
/// time a timetz on the session's zone, as it resolves it to timezone(…, timetz), whose zone a
/// name gives as the zone's offset where the transaction began (timetz_zone).
pub(super) fn at_zone(v: Value, zone: Value) -> Result<Value> {
    let v = match time_of(&v) {
        Some(ColumnType::Date) => cast(v, ColumnType::Timestamptz)?,
        Some(ColumnType::Time(_)) => cast(v, ColumnType::Timetz(None))?,
        _ => v,
    };
    let name = match zone {
        Value::Null => return Ok(Value::Null),
        Value::Text(t) => t,
        Value::Interval(i) => return at_interval(v, i),
        other => {
            return Err(Error::new(format!(
                "function timezone({}, …) does not exist",
                super::pipeline::kind_name(&other)
            )))
        }
    };
    let second = 1_000_000i128;
    // An abbreviation first, as PostgreSQL reads the name: of a fixed offset, or of a zone whose
    // rules give the offset it means at the time.
    let abbreviated = times::zone_abbreviation(&name)?;
    Ok(match (time_of(&v), &v) {
        (_, Value::Null) => Value::Null,
        (Some(ColumnType::Timestamptz), Value::Scalar(_, t)) => {
            let local = if *t == i64::MIN || *t == i64::MAX {
                *t
            } else {
                let utc = t.div_euclid(1_000_000);
                let east = match &abbreviated {
                    Some(times::Abbreviated::Fixed(east, _)) => *east,
                    Some(times::Abbreviated::Zone(zone)) => {
                        times::abbrev_utc_offset(zone, &name, utc)?.0
                    }
                    None => crate::zones::utc_offset(&name, utc)?,
                };
                held_instant(i128::from(*t) + i128::from(east) * second)?
            };
            Value::Scalar(ColumnType::Timestamp(None), local)
        }
        (Some(ColumnType::Timestamp(_)), Value::Scalar(_, t)) => {
            let at = if *t == i64::MIN || *t == i64::MAX {
                *t
            } else {
                let local = t.div_euclid(1_000_000);
                let east = match &abbreviated {
                    Some(times::Abbreviated::Fixed(east, _)) => *east,
                    Some(times::Abbreviated::Zone(zone)) => {
                        times::abbrev_local_offset(zone, &name, local)?
                    }
                    None => crate::zones::local_offset(&name, local)?,
                };
                held_instant(i128::from(*t) - i128::from(east) * second)?
            };
            Value::Scalar(ColumnType::Timestamptz, at)
        }
        (Some(ColumnType::Timetz(_)), Value::Scalar(_, k)) => {
            let utc = crate::scalars::transaction_clock().div_euclid(1_000_000);
            let east = match &abbreviated {
                Some(times::Abbreviated::Fixed(east, _)) => *east,
                Some(times::Abbreviated::Zone(zone)) => times::abbrev_utc_offset(zone, &name, utc)?.0,
                None => crate::zones::utc_offset(&name, utc)?,
            };
            timetz_at(*k, i64::from(east))?
        }
        _ => {
            return Err(Error::new(format!(
                "{} AT TIME ZONE is not read",
                super::pipeline::kind_name(&v)
            )))
        }
    })
}

/// A timetz on the clock of a zone `east` seconds east of UTC, as timetz_zone and timetz_izone
/// move it: the same moment of the day, its time within the day.
fn timetz_at(k: i64, east: i64) -> Result<Value> {
    let (time, west) = crate::scalars::timetz_parts(k);
    // The zones a timetz's key holds.
    if !(-65_536..65_536).contains(&-east) {
        return Err(Error::new(
            "a time with time zone more than 18:12:16 from UTC is not held",
        ));
    }
    let moved = (time + (i64::from(west) + east) * 1_000_000).rem_euclid(times::MICROS_PER_DAY);
    Ok(Value::Scalar(
        ColumnType::Timetz(None),
        crate::scalars::timetz_key(moved, -east as i32),
    ))
}

/// A value AT TIME ZONE an interval, as PostgreSQL's timestamptz_izone, timestamp_izone and
/// timetz_izone take it: the zone the interval's whole seconds east of UTC, its fraction cut toward
/// zero; an infinite value as it is, before the interval is read; an infinite interval, or one with
/// months or days, refused in PostgreSQL's words.
fn at_interval(v: Value, zone: times::Interval) -> Result<Value> {
    let (ty, t) = match (time_of(&v), &v) {
        (_, Value::Null) => return Ok(Value::Null),
        (
            Some(ty @ (ColumnType::Timestamptz | ColumnType::Timestamp(_) | ColumnType::Timetz(_))),
            Value::Scalar(_, t),
        ) => (ty, *t),
        _ => {
            return Err(Error::new(format!(
                "{} AT TIME ZONE is not read",
                super::pipeline::kind_name(&v)
            )))
        }
    };
    let other = match ty {
        ColumnType::Timestamptz => ColumnType::Timestamp(None),
        _ => ColumnType::Timestamptz,
    };
    let timetz = matches!(ty, ColumnType::Timetz(_));
    if !timetz && (t == i64::MIN || t == i64::MAX) {
        return Ok(Value::Scalar(other, t));
    }
    let shown = times::interval_out(zone);
    if !zone.is_finite() {
        return Err(Error::new(format!(
            "interval time zone \"{shown}\" must be finite"
        )));
    }
    if zone.months != 0 || zone.days != 0 {
        return Err(Error::new(format!(
            "interval time zone \"{shown}\" must not include months or days"
        )));
    }
    if timetz {
        return timetz_at(t, zone.micros / 1_000_000);
    }
    let east = i128::from(zone.micros / 1_000_000) * 1_000_000;
    let moved = match ty {
        ColumnType::Timestamptz => i128::from(t) + east,
        _ => i128::from(t) - east,
    };
    Ok(Value::Scalar(other, held_instant(moved)?))
}
