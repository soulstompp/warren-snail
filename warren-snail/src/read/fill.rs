//! A row as a write fills it: a value assigned to a column of another type as PostgreSQL's
//! assignment casts take it, a stored generated column computed, and a table's CHECKs checked.

use super::pipeline::{eval_on, numeric_text, Value};
use super::statement::{Datum, Row};
use crate::catalog::{ColumnType, Table};
use crate::{Error, Result};
use chrono::{Datelike, Timelike};

/// An instant as PostgreSQL writes a timestamptz under TimeZone UTC and DateStyle ISO: its
/// microseconds where it has any, its year past 9999 as it is, and BC after a year before 1.
pub fn instant_text(micros: i64) -> String {
    match micros {
        i64::MAX => return "infinity".into(),
        i64::MIN => return "-infinity".into(),
        _ => {}
    }
    let t = crate::places::civil_utc(micros);
    let (bc, year) = if t.year() <= 0 {
        (true, 1 - t.year())
    } else {
        (false, t.year())
    };
    let mut out = format!(
        "{year:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        t.month(),
        t.day(),
        t.hour(),
        t.minute(),
        t.second()
    );
    let us = micros.rem_euclid(1_000_000);
    if us > 0 {
        let f = format!("{us:06}");
        out.push('.');
        out.push_str(f.trim_end_matches('0'));
    }
    out.push_str("+00");
    if bc {
        out.push_str(" BC");
    }
    out
}

/// A value as text, as PostgreSQL's output function for its type writes it.
fn text_of(d: &Datum, from: ColumnType) -> Option<String> {
    Some(match (d, from) {
        (Datum::Null, _) => return None,
        (Datum::Int(v), ColumnType::Boolean) => if *v != 0 { "true" } else { "false" }.into(),
        (Datum::Int(v), ColumnType::Timestamptz) => instant_text(*v),
        (Datum::Int(v), ColumnType::Numeric { scale, .. }) => {
            numeric_text(*v as i128, scale as u32)
        }
        (Datum::Int(v), ty) if crate::scalars::is_scalar(ty) => crate::scalars::scalar_text(ty, *v),
        (Datum::Int(v), _) => v.to_string(),
        (Datum::Text(s), _) => s.clone(),
        (Datum::Bytes(b), _) => Value::Bytes(b.clone()).text().unwrap_or_default(),
        (Datum::Wide(k), _) => crate::scalars::uuid_text(*k),
    })
}

/// A number rounded to `to` places from `from`, half away from zero, as PostgreSQL rounds a numeric.
fn rescale(m: i128, from: u32, to: u32) -> i128 {
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

/// A value of type `from` assigned to a column `what` of type `to`, as PostgreSQL's implicit and
/// assignment casts take it: integers and numerics rounded and held to the column's range, any
/// value written as text into a string column, held to its length; any other pair refused, as
/// PostgreSQL refuses it.
pub fn assign(d: Datum, from: ColumnType, to: ColumnType, what: &str) -> Result<Datum> {
    use ColumnType::*;
    if d == Datum::Null {
        return Ok(Datum::Null);
    }
    let refused = || {
        Error::new(format!(
            "column \"{what}\" is of type {} but expression is of type {}",
            to.pg_name(),
            // PostgreSQL names the expression's type without its modifier.
            from.pg_name().split('(').next().unwrap_or_default()
        ))
    };
    let out_of_range = || Error::new(format!("{} out of range", to.pg_name()));
    Ok(match (from, to) {
        (_, Text(n)) => {
            let s = text_of(&d, from).unwrap_or_default();
            match n {
                Some(n) if s.chars().count() > n as usize => {
                    // PostgreSQL takes off a value's excess where it is only spaces.
                    let kept: String = s.chars().take(n as usize).collect();
                    if s.chars().skip(n as usize).all(|c| c == ' ') {
                        Datum::Text(kept)
                    } else {
                        return Err(Error::new(format!(
                            "value too long for type character varying({n})"
                        )));
                    }
                }
                _ => Datum::Text(s),
            }
        }
        (Int2 | Int4 | Int8 | Numeric { .. }, Int2 | Int4 | Int8 | Numeric { .. }) => {
            let Datum::Int(v) = d else {
                return Err(refused());
            };
            let (m, s) = match from {
                Numeric { scale, .. } => (v as i128, scale as u32),
                _ => (v as i128, 0),
            };
            match to {
                Numeric { precision, scale } => {
                    let units = rescale(m, s, scale as u32);
                    if units.unsigned_abs() >= 10u128.pow(precision as u32) {
                        return Err(Error::new("numeric field overflow"));
                    }
                    Datum::Int(units as i64)
                }
                _ => {
                    let whole = rescale(m, s, 0);
                    let (lo, hi) = to.integer_range().unwrap();
                    if whole < lo as i128 || whole > hi as i128 {
                        return Err(out_of_range());
                    }
                    Datum::Int(whole as i64)
                }
            }
        }
        (Timestamptz, Timestamptz) | (Bytes, Bytes) | (Boolean, Boolean) | (Uuid, Uuid) => d,
        (Oid, Oid)
        | (Xid, Xid)
        | (Cid, Cid)
        | (Xid8, Xid8)
        | (PgLsn, PgLsn)
        | (Char, Char)
        | (Money, Money) => d,
        (Int2 | Int4 | Int8, Oid) => {
            let Datum::Int(v) = d else {
                return Err(refused());
            };
            // A bigint is held to an oid's range; a smaller integer's bits are taken as they are.
            let u = if from == Int8 {
                u32::try_from(v).map_err(|_| Error::new("OID out of range"))?
            } else {
                v as i32 as u32
            };
            Datum::Int(crate::scalars::unsigned_key(u64::from(u), 32))
        }
        (Oid, Int4 | Int8) => {
            let Datum::Int(v) = d else {
                return Err(refused());
            };
            let u = crate::scalars::unsigned_of(v, 32);
            Datum::Int(if to == Int4 {
                i64::from(u as u32 as i32)
            } else {
                u as i64
            })
        }
        (Int4 | Int8 | Numeric { .. }, Money) => {
            let Datum::Int(v) = d else {
                return Err(refused());
            };
            let s = match from {
                Numeric { scale, .. } => scale as u32,
                _ => 0,
            };
            let cents = rescale(v as i128, s, 2);
            Datum::Int(i64::try_from(cents).map_err(|_| Error::new("bigint out of range"))?)
        }
        (Money, Numeric { precision, scale }) => {
            let Datum::Int(v) = d else {
                return Err(refused());
            };
            let units = rescale(v as i128, 2, scale as u32);
            if units.unsigned_abs() >= 10u128.pow(precision as u32) {
                return Err(Error::new("numeric field overflow"));
            }
            Datum::Int(units as i64)
        }
        (Macaddr | Macaddr8, Macaddr | Macaddr8) => {
            let Datum::Int(v) = d else {
                return Err(refused());
            };
            Datum::Int(crate::scalars::mac_cast(from, to, v)?)
        }
        (Text(_), Char) => {
            let s = text_of(&d, from).unwrap_or_default();
            Datum::Int(crate::scalars::scalar_parse(Char, &s)?)
        }
        _ => return Err(refused()),
    })
}

/// A row's value as the pipeline's expressions read it.
pub fn value_of(d: &Datum, ty: ColumnType) -> Value {
    match (d, ty) {
        (Datum::Null, _) => Value::Null,
        (Datum::Int(v), ColumnType::Boolean) => Value::Bool(*v != 0),
        (Datum::Int(v), ColumnType::Numeric { scale, .. }) => Value::Num(*v as i128, scale as u32),
        (Datum::Int(v), ty) if crate::scalars::is_scalar(ty) => Value::Scalar(ty, *v),
        (Datum::Int(v), _) => Value::Int(*v),
        (Datum::Text(s), _) => Value::Text(s.clone()),
        (Datum::Bytes(b), _) => Value::Bytes(b.clone()),
        (Datum::Wide(k), _) => Value::Uuid(*k),
    }
}

/// A value an expression computed, assigned to a column `what` of type `to`.
pub fn assign_value(v: Value, to: ColumnType, what: &str) -> Result<Datum> {
    let (d, from) = match v {
        Value::Null => return Ok(Datum::Null),
        // An instant's column reads its microseconds as an integer; an integer is one where the
        // column is a timestamptz.
        Value::Int(x) if to == ColumnType::Timestamptz => (Datum::Int(x), ColumnType::Timestamptz),
        Value::Int(x) => (Datum::Int(x), ColumnType::Int8),
        Value::Num(m, s) => (
            Datum::Int(i64::try_from(m).map_err(|_| Error::new("numeric field overflow"))?),
            ColumnType::Numeric {
                precision: 18,
                scale: s.min(18) as u8,
            },
        ),
        Value::Text(s) => (Datum::Text(s), ColumnType::Text(None)),
        Value::Bytes(b) => (Datum::Bytes(b), ColumnType::Bytes),
        Value::Bool(b) => (Datum::Int(b as i64), ColumnType::Boolean),
        Value::Uuid(k) => (Datum::Wide(k), ColumnType::Uuid),
        Value::Scalar(ty, k) => (Datum::Int(k), ty),
    };
    assign(d, from, to, what)
}

fn names(table: &Table) -> Vec<String> {
    table.columns.iter().map(|c| c.name.clone()).collect()
}

fn values(table: &Table, row: &Row) -> Vec<Value> {
    row.iter()
        .zip(&table.columns)
        .map(|(d, c)| value_of(d, c.ty))
        .collect()
}

/// Computes a row's generated columns from its other columns: each stored one kept, each virtual
/// one left NULL, as no order holds it.
pub fn generate(table: &Table, row: &mut Row) -> Result<()> {
    if table.columns.iter().all(|c| c.generated.is_none()) {
        return Ok(());
    }
    let names = names(table);
    let vals = values(table, row);
    for (c, col) in table.columns.iter().enumerate() {
        if let Some(g) = &col.generated {
            row[c] = if g.stored {
                assign_value(eval_on(&g.expr, &names, &vals)?, col.ty, &col.name)?
            } else {
                Datum::Null
            };
        }
    }
    Ok(())
}

/// The row with its virtual columns computed, as a read gives it.
pub fn with_virtual(table: &Table, row: &Row) -> Result<Row> {
    let mut out = row.clone();
    if !table.columns.iter().any(|c| c.is_virtual()) {
        return Ok(out);
    }
    let names = names(table);
    let vals = values(table, row);
    for (c, col) in table.columns.iter().enumerate() {
        if let Some(g) = col.generated.as_ref().filter(|g| !g.stored) {
            out[c] = assign_value(eval_on(&g.expr, &names, &vals)?, col.ty, &col.name)?;
        }
    }
    Ok(out)
}

/// Refuses a row that fails one of its table's enforced CHECKs. A CHECK whose condition is NULL
/// passes, as in PostgreSQL. Its virtual columns are computed for the CHECKs that read them.
pub fn check(table: &Table, row: &Row) -> Result<()> {
    if table.checks.iter().all(|k| !k.enforced) {
        return Ok(());
    }
    let full = with_virtual(table, row)?;
    let names = names(table);
    let vals = values(table, &full);
    for k in table.checks.iter().filter(|k| k.enforced) {
        match eval_on(&k.expr, &names, &vals)? {
            Value::Bool(false) => {
                return Err(Error::new(format!(
                    "new row for relation \"{}\" violates check constraint \"{}\"",
                    table.name.rsplit('.').next().unwrap_or(&table.name),
                    k.name
                )))
            }
            Value::Bool(true) | Value::Null => {}
            other => {
                return Err(Error::new(format!(
                    "{}: the CHECK {} gives {other:?}, not a truth",
                    table.name, k.name
                )))
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_instant_is_written_as_postgresql_writes_it_under_utc() {
        assert_eq!(instant_text(0), "1970-01-01 00:00:00+00");
        assert_eq!(instant_text(1_500_000), "1970-01-01 00:00:01.5+00");
        assert_eq!(instant_text(-1), "1969-12-31 23:59:59.999999+00");
        // 0001-01-01 BC is year 0 on the proleptic calendar.
        assert_eq!(
            instant_text(-62_167_219_200_000_000),
            "0001-01-01 00:00:00+00 BC"
        );
        assert_eq!(instant_text(i64::MAX), "infinity");
    }

    #[test]
    fn a_value_is_assigned_as_postgresql_s_assignment_casts_take_it() {
        let n92 = ColumnType::Numeric {
            precision: 9,
            scale: 2,
        };
        let a = |d: Datum, f, t| assign(d, f, t, "c");
        assert_eq!(
            a(Datum::Int(70000), ColumnType::Int4, ColumnType::Int8).unwrap(),
            Datum::Int(70000)
        );
        assert!(a(Datum::Int(70000), ColumnType::Int4, ColumnType::Int2)
            .unwrap_err()
            .to_string()
            .contains("smallint out of range"));
        // 12.345 at scale 3 rounds half away from zero to 12.35, and -2.5 to -3.
        let n3 = ColumnType::Numeric {
            precision: 9,
            scale: 3,
        };
        assert_eq!(a(Datum::Int(12345), n3, n92).unwrap(), Datum::Int(1235));
        assert_eq!(
            a(
                Datum::Int(-25),
                ColumnType::Numeric {
                    precision: 2,
                    scale: 1
                },
                ColumnType::Int4
            )
            .unwrap(),
            Datum::Int(-3)
        );
        assert!(a(Datum::Int(10_000_000), ColumnType::Int8, n92)
            .unwrap_err()
            .to_string()
            .contains("numeric field overflow"));
        assert_eq!(
            a(Datum::Int(150), n92, ColumnType::Text(None)).unwrap(),
            Datum::Text("1.50".into())
        );
        assert_eq!(
            a(Datum::Int(1), ColumnType::Boolean, ColumnType::Text(None)).unwrap(),
            Datum::Text("true".into())
        );
        assert_eq!(
            a(
                Datum::Text("ab  ".into()),
                ColumnType::Text(None),
                ColumnType::Text(Some(2))
            )
            .unwrap(),
            Datum::Text("ab".into())
        );
        assert!(a(
            Datum::Text("abc".into()),
            ColumnType::Text(None),
            ColumnType::Text(Some(2))
        )
        .is_err());
        for (f, t) in [
            (ColumnType::Text(None), ColumnType::Int4),
            (ColumnType::Boolean, ColumnType::Int4),
            (ColumnType::Int4, ColumnType::Boolean),
            (ColumnType::Int8, ColumnType::Timestamptz),
            (ColumnType::Text(None), ColumnType::Bytes),
        ] {
            let d = if f.is_text() {
                Datum::Text("1".into())
            } else {
                Datum::Int(1)
            };
            assert!(a(d, f, t)
                .unwrap_err()
                .to_string()
                .contains("but expression is of type"));
        }
    }
}
