//! generate_series: from its start, each value the one before plus its step, while it lies within its
//! stop; of integers, of numerics, of timestamps and of timestamptzs, the last on the session zone's
//! clock or on the zone it names.

use super::pipeline::Value;
use crate::catalog::ColumnType;
use crate::numeric::Numeric;
use crate::times::Interval;
use crate::{Error, Result};

/// A generate_series call's values, made one at a time.
pub(super) enum Series {
    /// None: a NULL argument.
    Empty,
    /// The next value, none once a sum overflows the integers.
    Integers {
        next: Option<i64>,
        stop: i64,
        step: i64,
    },
    Numerics {
        next: Option<Numeric>,
        stop: Numeric,
        step: Numeric,
    },
    /// Instants: the next, made before the one before it is given.
    Instants {
        next: Option<i64>,
        stop: i64,
        step: Interval,
        clock: Clock,
    },
}

/// The clock a series of instants adds its step on.
pub(super) enum Clock {
    /// A timestamp's own.
    Plain,
    /// A timestamptz's, on the statement's zone.
    Session,
    /// A timestamptz's, on the zone the call names: UTC where none.
    Named(Option<crate::zones::Session>),
}

/// The step of a call written without one.
const ONE: i64 = 1;

impl Iterator for Series {
    type Item = Result<Value>;

    fn next(&mut self) -> Option<Result<Value>> {
        match self {
            Series::Empty => None,
            Series::Integers { next, stop, step } => {
                let at = (*next)?;
                if (*step > 0 && at > *stop) || (*step < 0 && at < *stop) {
                    *next = None;
                    return None;
                }
                *next = at.checked_add(*step);
                Some(Ok(Value::Int(at)))
            }
            Series::Numerics { next, stop, step } => {
                let at = next.take()?;
                let within = match step.sign() > 0 {
                    true => at <= *stop,
                    false => at >= *stop,
                };
                if !within {
                    return None;
                }
                match at.add(step) {
                    Ok(n) => *next = Some(n),
                    Err(e) => return Some(Err(e)),
                }
                Some(Ok(Value::of_numeric(at)))
            }
            Series::Instants {
                next,
                stop,
                step,
                clock,
            } => {
                let at = (*next)?;
                let within = match step.total() > 0 {
                    true => at <= *stop,
                    false => at >= *stop,
                };
                if !within {
                    *next = None;
                    return None;
                }
                let after = match clock {
                    Clock::Plain => crate::times::timestamp_pl_interval(at, *step),
                    Clock::Session => crate::times::timestamptz_pl_interval(at, *step),
                    Clock::Named(zone) => {
                        let _held = crate::zones::handed(zone.clone());
                        crate::times::timestamptz_pl_interval(at, *step)
                    }
                };
                match after {
                    Ok(t) => *next = Some(t),
                    Err(e) => {
                        *next = None;
                        return Some(Err(e));
                    }
                }
                let ty = match clock {
                    Clock::Plain => ColumnType::Timestamp(None),
                    _ => ColumnType::Timestamptz,
                };
                Some(Ok(Value::Scalar(ty, at)))
            }
        }
    }
}

/// A generate_series call over its arguments' values, each with whether it is a quoted literal,
/// `ty` the type the statement resolved it to; read from the values where it resolved none.
pub(super) fn series(given: Vec<(Value, bool)>, ty: Option<ColumnType>) -> Result<Series> {
    use ColumnType as C;
    let ty = match ty {
        Some(t) => t,
        None => resolved(&given)?,
    };
    let at = |t: ColumnType, (v, q): &(Value, bool)| -> Result<Value> {
        if v.is_null() {
            return Ok(Value::Null);
        }
        super::pipeline::part_of(t, v.clone(), *q).map(|p| super::pipeline::part_value(t, &p))
    };
    let interval = super::calls::full_interval();
    let types: Vec<ColumnType> = match (super::clock::base(ty), given.len()) {
        (C::Timestamp(_) | C::Timestamptz, n) => {
            let mut ts = vec![ty, ty, interval];
            ts.extend((n == 4).then_some(C::Text(None)));
            ts
        }
        (_, n) => vec![ty; n],
    };
    let values = given
        .iter()
        .zip(&types)
        .map(|(g, t)| at(*t, g))
        .collect::<Result<Vec<_>>>()?;
    // A strict function: a NULL argument gives no rows.
    if values.iter().any(Value::is_null) {
        return Ok(Series::Empty);
    }
    match (super::clock::base(ty), values.as_slice()) {
        (C::Int2 | C::Int4 | C::Int8, [Value::Int(start), Value::Int(stop), rest @ ..]) => {
            let step = match rest {
                [Value::Int(s)] => *s,
                _ => ONE,
            };
            if step == 0 {
                return Err(Error::new("step size cannot equal zero"));
            }
            Ok(Series::Integers {
                next: Some(*start),
                stop: *stop,
                step,
            })
        }
        (C::Timestamp(_) | C::Timestamptz, [start, stop, Value::Interval(step), zone @ ..]) => {
            let instant = |v: &Value| match v {
                Value::Scalar(_, t) => Ok(*t),
                v => Err(Error::new(format!(
                    "{} is no instant",
                    super::pipeline::kind_name(v)
                ))),
            };
            let clock = match zone {
                [Value::Text(name) | Value::Str(_, name)] => Clock::Named(zone_named(name)?),
                _ if super::clock::base(ty) == C::Timestamptz => Clock::Session,
                _ => Clock::Plain,
            };
            if step.total() == 0 {
                return Err(Error::new("step size cannot equal zero"));
            }
            if !step.is_finite() {
                return Err(Error::new("step size cannot be infinite"));
            }
            Ok(Series::Instants {
                next: Some(instant(start)?),
                stop: instant(stop)?,
                step: *step,
                clock,
            })
        }
        (_, [start, stop, rest @ ..]) if ty.is_numeric() => {
            let number = |v: &Value, what: &str| -> Result<Numeric> {
                let n = v.to_numeric().ok_or_else(|| {
                    Error::new(format!("{} is no numeric", super::pipeline::kind_name(v)))
                })?;
                if n.is_nan() {
                    return Err(Error::new(format!("{what} cannot be NaN")));
                }
                if n.is_infinite() {
                    return Err(Error::new(format!("{what} cannot be infinity")));
                }
                Ok(n)
            };
            let (start, stop) = (number(start, "start value")?, number(stop, "stop value")?);
            let step = match rest {
                [s] => {
                    let s = number(s, "step size")?;
                    if s.sign() == 0 {
                        return Err(Error::new("step size cannot equal zero"));
                    }
                    s
                }
                _ => Numeric::of_int(ONE),
            };
            Ok(Series::Numerics {
                next: Some(start),
                stop,
                step,
            })
        }
        _ => Err(super::typed::no_form(
            "generate_series",
            &given.iter().map(|_| Some(ty)).collect::<Vec<_>>(),
        )),
    }
}

/// Its values whole.
pub(super) fn values(given: Vec<(Value, bool)>, ty: Option<ColumnType>) -> Result<Vec<Value>> {
    series(given, ty)?.collect()
}

/// The form a call's values take where the statement resolved none: each argument's type read
/// from its value, as the forms of numbers and of time resolve them.
fn resolved(given: &[(Value, bool)]) -> Result<ColumnType> {
    use ColumnType as C;
    let types: Vec<Option<ColumnType>> = given
        .iter()
        .map(|(v, q)| match v {
            _ if *q || v.is_null() => None,
            Value::Int(i) if i32::try_from(*i).is_ok() => Some(C::Int4),
            Value::Int(_) => Some(C::Int8),
            Value::Num(..) | Value::Numeric(_) => Some(super::typed::NUMERIC),
            Value::Scalar(t, _) => Some(*t),
            Value::Interval(_) => Some(super::calls::full_interval()),
            Value::Text(_) | Value::Str(..) => Some(C::Text(None)),
            _ => Some(C::Boolean),
        })
        .collect();
    let form = match super::typed::numbers("generate_series", &types)? {
        Some(t) => t,
        None => super::typed::time("generate_series", &types)?.flatten(),
    };
    form.ok_or_else(|| super::typed::no_form("generate_series", &types))
}

/// The zone a call names, as PostgreSQL's lookup_timezone reads it: an abbreviation first, of a
/// fixed offset or of the zone whose rules it names, then a zone's name; refused where it names
/// none. None for UTC.
fn zone_named(name: &str) -> Result<Option<crate::zones::Session>> {
    let name = match crate::times::zone_abbreviation(name)? {
        Some(crate::times::Abbreviated::Fixed(east, _)) => {
            let fixed = crate::zones::Session::Fixed(east, name.to_uppercase());
            return Ok((east != 0).then_some(fixed));
        }
        Some(crate::times::Abbreviated::Zone(zone)) => zone,
        None => name.to_string(),
    };
    let name = name.as_str();
    let Some(zone) = crate::zones::set_name(name)? else {
        return Err(Error::new(format!("time zone \"{name}\" not recognized")));
    };
    let _held = crate::zones::in_zone(Some(&zone));
    Ok(crate::zones::session())
}
