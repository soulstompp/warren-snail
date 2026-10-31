//! A row as a write fills it: a value assigned to a column of another type as PostgreSQL's
//! assignment casts take it, a stored generated column computed, and a table's CHECKs checked.

use super::pipeline::{eval_on, Casings, Value};
use super::statement::{Datum, Row};
use crate::catalog::{ColumnType, Table};
use crate::numeric::Numeric;
use crate::{Error, Result};

/// An instant as PostgreSQL writes a timestamptz under TimeZone UTC and DateStyle ISO.
pub fn instant_text(micros: i64) -> String {
    crate::times::timestamptz_out(micros)
}

/// A numeric column's value as the number it is.
pub(crate) fn numeric_of(d: &Datum, ty: ColumnType) -> Option<Numeric> {
    match (d, ty) {
        (Datum::Int(u), ColumnType::Numeric { precision, scale }) => Some(
            crate::numeric::column_numeric(i128::from(*u), precision, scale),
        ),
        (Datum::Wide(u), ColumnType::Numeric { precision, scale }) => {
            Some(crate::numeric::column_numeric(*u, precision, scale))
        }
        (Datum::Text(s), ColumnType::NumericText(_)) => Numeric::parse(s).ok(),
        _ => None,
    }
}

/// A number held to a numeric column's type, as the column's datum: rounded to its scale and
/// refused past its digits, as PostgreSQL holds a value to a typmod.
pub(crate) fn numeric_datum(n: Numeric, ty: ColumnType) -> Result<Datum> {
    match ty {
        ColumnType::Numeric { precision, scale } => {
            let u = crate::numeric::column_units(&n, precision, scale)?;
            Ok(if precision > 18 {
                Datum::Wide(u)
            } else {
                Datum::Int(u as i64)
            })
        }
        ColumnType::NumericText(t) => {
            let n = match t {
                Some((p, s)) => n.typmod(u32::from(p), i32::from(s))?,
                None => n,
            };
            Ok(Datum::Text(n.to_string()))
        }
        other => Err(Error::new(format!("{} is no numeric", other.pg_name()))),
    }
}

/// A value as text, as PostgreSQL's output function for its type writes it.
pub(crate) fn text_of(d: &Datum, from: ColumnType) -> Option<String> {
    Some(match (d, from) {
        (Datum::Null, _) => return None,
        (Datum::Int(v), ColumnType::Boolean) => if *v != 0 { "true" } else { "false" }.into(),
        (Datum::Int(v), ty) if ty.is_timestamptz() => instant_text(*v),
        (Datum::Int(_) | Datum::Wide(_), ColumnType::Numeric { .. }) => {
            numeric_of(d, from)?.to_string()
        }
        (Datum::Int(v), ty) if crate::scalars::is_scalar(ty) => crate::scalars::scalar_text(ty, *v),
        (Datum::Int(v), ty @ ColumnType::Composed(_)) => {
            crate::composed::scalar_out(ty, &Datum::Int(*v))
        }
        (Datum::Int(v), _) => v.to_string(),
        // As a value held as its text is cast to text: inet's and cidr's mask always written.
        (Datum::Text(s), ty @ (ColumnType::Textual(_) | ColumnType::Internal(_))) => {
            crate::textual::text_cast(ty, s)
        }
        (Datum::Text(s), _) => s.clone(),
        (Datum::Bytes(k), ty) if ty.is_bits() => crate::strings::bits_text(k),
        (Datum::Bytes(b), _) => Value::Bytes(b.clone()).text().unwrap_or_default(),
        (Datum::Wide(k), _) => crate::scalars::uuid_text(*k),
        (Datum::Float4(b), _) => crate::scalars::float_text(u64::from(*b), false),
        (Datum::Float8(b), _) => crate::scalars::float_text(*b, true),
        (Datum::Compound(c), ty) => crate::composed::text_out(ty, &c.item),
        (Datum::Interval(v), _) => crate::times::interval_out(*v),
    })
}

/// Whether a type is one of the time types.
fn is_time(ty: ColumnType) -> bool {
    use ColumnType::*;
    matches!(
        ty,
        Date | Time(_)
            | Timetz(_)
            | Timestamp(_)
            | Timestamptz
            | TimestamptzRounded(_)
            | Interval { .. }
    )
}

/// A time type's value assigned to a column of a time type, as PostgreSQL's implicit and assignment
/// casts take it, rounded to the column's precision; an empty error where no cast takes it.
fn time_assign(d: Datum, from: ColumnType, to: ColumnType) -> Result<Datum> {
    if !crate::catalog::assignable(from, to) {
        return Err(Error::new(""));
    }
    Ok(match super::clock::cast(value_of(&d, from), to)? {
        Value::Scalar(_, k) => Datum::Int(k),
        Value::Interval(x) => Datum::Interval(x),
        _ => Datum::Null,
    })
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
    // A value is assigned to a string as its type's output writes it, where the snail writes it.
    if let (Internal(t), Text(_) | Bpchar(_) | Name) = (from, to) {
        if let Some(e) = t.unwritten() {
            return Err(e);
        }
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
    // A string as text: a bpchar's without its trailing blanks, as its cast to text takes it.
    let as_text = |d: &Datum| -> String {
        let s = text_of(d, from).unwrap_or_default();
        crate::strings::text_of(from, &s).to_string()
    };
    Ok(match (from, to) {
        // A "char" is its one byte as a bpchar, which UTF-8 holds only below 0x80.
        (Char, Bpchar(n)) => {
            let Datum::Int(k) = d else {
                return Err(refused());
            };
            let b = crate::scalars::unsigned_of(k, 8) as u8;
            if b >= 0x80 {
                return Err(Error::new(format!(
                    "the \"char\" byte {b:#04x} makes a {} that is no UTF-8",
                    to.pg_name()
                )));
            }
            let s = if b == 0 {
                String::new()
            } else {
                char::from(b).to_string()
            };
            Datum::Text(crate::strings::bpchar_in(&s, n, false)?)
        }
        (_, Bpchar(n)) => Datum::Text(crate::strings::bpchar_in(
            &text_of(&d, from).unwrap_or_default(),
            n,
            false,
        )?),
        // A boolean meets a name through its output, which no cast to text takes: `t` or `f`.
        (Boolean, Name) => Datum::Text(if d == Datum::Int(0) { "f" } else { "t" }.to_string()),
        (_, Name) => Datum::Text(crate::strings::name_in(&as_text(&d))),
        (Bit(_) | Varbit(_), Bit(_) | Varbit(_)) => {
            let Datum::Bytes(k) = &d else {
                return Err(refused());
            };
            let (n, varying) = match to {
                Bit(n) => (Some(n), false),
                Varbit(n) => (n, true),
                _ => unreachable!("a bit string's type"),
            };
            Datum::Bytes(crate::strings::bits_cast(k, n, varying, false)?)
        }
        (_, Text(n)) => {
            let s = as_text(&d);
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
        // A numeric on either side is taken as the number it is.
        (f, t) if f.is_numeric() || t.is_numeric() => {
            let n = match (&d, from) {
                (_, f) if f.is_numeric() => numeric_of(&d, from).ok_or_else(refused)?,
                (Datum::Int(v), Int2 | Int4 | Int8) => crate::numeric::Numeric::of_int(*v),
                (Datum::Int(count), Money(m)) if to.is_numeric() => {
                    crate::monetary::to_numeric(*count, m.conv())
                }
                (Datum::Float4(b), Float4) => {
                    crate::numeric::Numeric::of_float(f64::from(f32::from_bits(*b)), false)
                }
                (Datum::Float8(b), Float8) => {
                    crate::numeric::Numeric::of_float(f64::from_bits(*b), true)
                }
                _ => return Err(refused()),
            };
            match to {
                t if t.is_numeric() => numeric_datum(n, to)?,
                Int2 | Int4 | Int8 => {
                    let (lo, hi) = to.integer_range().unwrap();
                    Datum::Int(n.to_integer(lo, hi, &to.pg_name())?)
                }
                // numeric_cash: its count, as numeric_int8 rounds it.
                Money(m) => Datum::Int(crate::monetary::of_numeric(&n, m.conv())?),
                Float4 | Float8 => {
                    let f = n.to_float(to == Float8)?;
                    if to == Float8 {
                        Datum::Float8(f.to_bits())
                    } else {
                        Datum::Float4((f as f32).to_bits())
                    }
                }
                _ => return Err(refused()),
            }
        }
        (Int2 | Int4 | Int8, Int2 | Int4 | Int8) => {
            let Datum::Int(v) = d else {
                return Err(refused());
            };
            let (lo, hi) = to.integer_range().unwrap();
            if v < lo || v > hi {
                return Err(out_of_range());
            }
            Datum::Int(v)
        }
        (from, to) if is_time(from) && is_time(to) => {
            time_assign(d, from, to).map_err(|e| if e.0.is_empty() { refused() } else { e })?
        }
        (Bytes, Bytes) | (Boolean, Boolean) | (Uuid, Uuid) => d,
        (Oid, Oid)
        | (Xid, Xid)
        | (Cid, Cid)
        | (Xid8, Xid8)
        | (PgLsn, PgLsn)
        | (Char, Char)
        | (Money(_), Money(_))
        | (Tid, Tid) => d,
        (Composed(a), Composed(b)) if a == b => d,
        // A reg* type is an oid to PostgreSQL's casts: from and to one, binary-coercible, as are a
        // function's two and an operator's two to each other.
        (Oid | Reg(..), Oid | Reg(..))
            if from.is_oid()
                && (from == Oid
                    || to == Oid
                    || matches!((from, to), (Reg(a, _), Reg(b, _)) if a == b || crate::regs::casts_between(a, b))) =>
        {
            d
        }
        (Text(_), Reg(crate::regs::RegKind::Class, release)) => {
            let s = text_of(&d, from).unwrap_or_default();
            Datum::Int(crate::scalars::unsigned_key(
                u64::from(crate::regs::class_of_text(&s, release)?),
                32,
            ))
        }
        (Oid8, Oid8) => d,
        (Int2 | Int4 | Int8, Oid8) | (Oid8, Int8) => {
            let Datum::Int(v) = d else {
                return Err(refused());
            };
            // int8 and oid8 are binary-coercible; a smaller integer comes through int8.
            Datum::Int(if to == Oid8 {
                crate::scalars::unsigned_key(v as u64, 64)
            } else {
                crate::scalars::unsigned_of(v, 64) as i64
            })
        }
        (Oid, Oid8) => {
            let Datum::Int(v) = d else {
                return Err(refused());
            };
            Datum::Int(crate::scalars::unsigned_key(
                crate::scalars::unsigned_of(v, 32),
                64,
            ))
        }
        (Int2 | Int4 | Int8, Oid | Reg(..)) => {
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
        (Oid | Reg(..), Int4 | Int8) => {
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
        (Int4 | Int8, Money(m)) => {
            let Datum::Int(v) = d else {
                return Err(refused());
            };
            Datum::Int(crate::monetary::of_integer(v, m.conv())?)
        }
        (Macaddr | Macaddr8, Macaddr | Macaddr8) => {
            let Datum::Int(v) = d else {
                return Err(refused());
            };
            Datum::Int(crate::scalars::mac_cast(from, to, v)?)
        }
        (Float4, Float4) | (Float8, Float8) => d,
        (Int2 | Int4 | Int8 | Float4 | Float8, Float4 | Float8) => {
            let v = value_of(&d, from);
            let wide = to == Float8;
            match super::pipeline::float_cast(v, wide)? {
                Value::Real(f) => Datum::Float4(f.to_bits()),
                Value::Double(f) => Datum::Float8(f.to_bits()),
                _ => return Err(refused()),
            }
        }
        (Float4 | Float8, Int2 | Int4 | Int8) => {
            let f = match d {
                Datum::Float4(b) => f64::from(f32::from_bits(b)),
                Datum::Float8(b) => f64::from_bits(b),
                _ => return Err(refused()),
            };
            let base = match to {
                Int2 => "smallint",
                Int4 => "integer",
                _ => "bigint",
            };
            match super::pipeline::float_integer(f, base)? {
                Value::Int(v) => Datum::Int(v),
                _ => return Err(refused()),
            }
        }
        (Text(_) | Bpchar(_), Char) => {
            Datum::Int(crate::scalars::scalar_parse(Char, &as_text(&d))?)
        }
        // A value held as its text into its own type, or by the assignment casts between json and
        // jsonb, inet and cidr.
        (Textual(a), Textual(b)) if crate::textual::assignable(a, b) => {
            let Datum::Text(s) = &d else {
                return Err(refused());
            };
            Datum::Text(crate::textual::cast(a, b, s)?)
        }
        (Internal(a), Internal(b)) if a == b => d,
        (Geometric(a), Geometric(b)) if a == b => d,
        _ => return Err(refused()),
    })
}

/// A row's value as the pipeline's expressions read it.
pub fn value_of(d: &Datum, ty: ColumnType) -> Value {
    match (d, ty) {
        (Datum::Null, _) => Value::Null,
        (Datum::Int(v), ColumnType::Boolean) => Value::Bool(*v != 0),
        (_, ty) if ty.is_numeric() => numeric_of(d, ty).map_or(Value::Null, Value::of_numeric),
        (Datum::Int(v), ty) if crate::scalars::is_scalar(ty) => Value::Scalar(ty, *v),
        // An enum: its rank, held whole.
        (Datum::Int(v), ty @ ColumnType::Composed(_)) => Value::Composed(
            ty,
            std::sync::Arc::new(crate::composed::Compound::new(
                ty,
                crate::composed::Item::Scalar(Datum::Int(*v)),
            )),
        ),
        (Datum::Int(v), ty) if ty.is_timestamptz() => Value::Scalar(ColumnType::Timestamptz, *v),
        (Datum::Int(v), _) => Value::Int(*v),
        (Datum::Text(s), ColumnType::Bpchar(_) | ColumnType::Name) => Value::Str(ty, s.clone()),
        (Datum::Text(s), ColumnType::Textual(_) | ColumnType::Internal(_)) => {
            Value::Typed(ty, s.clone())
        }
        (Datum::Text(s), _) => Value::Text(s.clone()),
        (Datum::Bytes(k), ty) if ty.is_bits() => Value::Bits(ty, k.clone()),
        (Datum::Bytes(b), ColumnType::Geometric(k)) => {
            crate::geo::geo_of_bytes(k, b).map_or(Value::Null, Value::Geometric)
        }
        (Datum::Bytes(b), _) => Value::Bytes(b.clone()),
        (Datum::Wide(k), _) => Value::Uuid(*k),
        (Datum::Float4(b), _) => Value::Real(f32::from_bits(*b)),
        (Datum::Float8(b), _) => Value::Double(f64::from_bits(*b)),
        (Datum::Compound(c), ty) => Value::Composed(ty, c.clone()),
        (Datum::Interval(v), _) => Value::Interval(*v),
    }
}

/// A value an expression computed, assigned to a column `what` of type `to`.
pub fn assign_value(v: Value, to: ColumnType, what: &str) -> Result<Datum> {
    let (d, from) = match v {
        Value::Null => return Ok(Datum::Null),
        // An instant's column reads its microseconds as an integer; an integer is one where the
        // column is a timestamptz.
        Value::Int(x) if to.is_timestamptz() => (Datum::Int(x), ColumnType::Timestamptz),
        Value::Int(x) => (Datum::Int(x), ColumnType::Int8),
        // A numeric an expression computes is unconstrained.
        v @ (Value::Num(..) | Value::Numeric(_)) => (
            Datum::Text(v.text().unwrap_or_default()),
            ColumnType::NumericText(None),
        ),
        Value::Text(s) => (Datum::Text(s), ColumnType::Text(None)),
        Value::Bytes(b) => (Datum::Bytes(b), ColumnType::Bytes),
        Value::Bool(b) => (Datum::Int(b as i64), ColumnType::Boolean),
        Value::Uuid(k) => (Datum::Wide(k), ColumnType::Uuid),
        Value::Scalar(ty, k) => (Datum::Int(k), ty),
        Value::Real(f) => (Datum::Float4(f.to_bits()), ColumnType::Float4),
        Value::Double(d) => (Datum::Float8(d.to_bits()), ColumnType::Float8),
        Value::Str(ty, s) => (Datum::Text(s), ty),
        Value::Bits(ty, k) => (Datum::Bytes(k), ty),
        Value::Composed(ty, c) => match &c.item {
            crate::composed::Item::Null => return Ok(Datum::Null),
            crate::composed::Item::Scalar(d) => (d.clone(), ty),
            _ => (Datum::Compound(c), ty),
        },
        Value::Interval(v) => (
            Datum::Interval(v),
            ColumnType::Interval {
                range: crate::times::INTERVAL_FULL_RANGE,
                precision: None,
            },
        ),
        Value::Typed(ty, s) => (Datum::Text(s), ty),
        Value::Geometric(g) => (
            Datum::Bytes(crate::geo::geo_bytes(&g)),
            ColumnType::Geometric(g.kind()),
        ),
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
                assign_value(
                    eval_on(
                        &g.expr,
                        &names,
                        &vals,
                        table.release,
                        &Casings::of_table(table, &[]),
                    )?,
                    col.ty,
                    &col.name,
                )?
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
            out[c] = assign_value(
                eval_on(
                    &g.expr,
                    &names,
                    &vals,
                    table.release,
                    &Casings::of_table(table, &[]),
                )?,
                col.ty,
                &col.name,
            )?;
        }
    }
    Ok(out)
}

/// Refuses a row that holds NULL in a NOT NULL column, then one that fails one of its table's
/// enforced CHECKs, as PostgreSQL's ExecConstraints checks them. A CHECK whose condition is NULL
/// passes, as in PostgreSQL. Its virtual columns are computed for the CHECKs that read them.
pub fn check(table: &Table, row: &Row) -> Result<()> {
    not_null(table, row)?;
    if table.checks.iter().all(|k| !k.enforced) {
        return Ok(());
    }
    let full = with_virtual(table, row)?;
    let names = names(table);
    let vals = values(table, &full);
    for k in table.checks.iter().filter(|k| k.enforced) {
        match eval_on(
            &k.expr,
            &names,
            &vals,
            table.release,
            &Casings::of_table(table, &[]),
        )? {
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

/// Refuses a row whose period of a key WITHOUT OVERLAPS is empty, as PostgreSQL refuses it where
/// the row enters the key's index.
pub fn without_overlaps(table: &Table, row: &Row) -> Result<()> {
    for o in table.orders.iter().filter(|o| o.without_overlaps()) {
        let c = o.levels[o.levels.len() - 1].column();
        if matches!(
            crate::spans::bounds(table.columns[c].ty, &row[c]),
            Some(None)
        ) {
            return Err(Error::new(format!(
                "empty WITHOUT OVERLAPS value found in column \"{}\" in relation \"{}\"",
                table.columns[c].name,
                table.name.rsplit('.').next().unwrap_or(&table.name)
            )));
        }
    }
    Ok(())
}

/// Refuses a row that holds NULL in a NOT NULL column, in PostgreSQL's words with its detail: each
/// stored column in turn, then each virtual one, computed.
fn not_null(table: &Table, row: &Row) -> Result<()> {
    let refused = |c: usize| {
        Error::new(format!(
            "null value in column \"{}\" of relation \"{}\" violates not-null constraint: Failing \
             row contains {}.",
            table.columns[c].name,
            table.name.rsplit('.').next().unwrap_or(&table.name),
            failing_row(table, row)
        ))
    };
    let stored = table
        .columns
        .iter()
        .zip(row)
        .position(|(col, d)| col.not_null && !col.is_virtual() && *d == Datum::Null);
    if let Some(c) = stored {
        return Err(refused(c));
    }
    if !table.columns.iter().any(|c| c.not_null && c.is_virtual()) {
        return Ok(());
    }
    let full = with_virtual(table, row)?;
    match table
        .columns
        .iter()
        .zip(&full)
        .position(|(col, d)| col.not_null && col.is_virtual() && *d == Datum::Null)
    {
        Some(c) => Err(refused(c)),
        None => Ok(()),
    }
}

/// A row as PostgreSQL writes it in a refusal's detail: each value as its type's output function
/// writes it, cut to 64 bytes, a virtual column as `virtual`.
pub(crate) fn failing_row(table: &Table, row: &Row) -> String {
    const LONGEST: usize = 64;
    let values: Vec<String> = table
        .columns
        .iter()
        .zip(row)
        .map(|(col, d)| {
            if col.is_virtual() {
                return "virtual".to_string();
            }
            if *d == Datum::Null {
                return "null".to_string();
            }
            let mut text = crate::composed::scalar_out(col.ty, d);
            if text.len() > LONGEST {
                let cut = (0..=LONGEST)
                    .rev()
                    .find(|&i| text.is_char_boundary(i))
                    .unwrap_or(0);
                text.truncate(cut);
                text.push_str("...");
            }
            text
        })
        .collect();
    format!("({})", values.join(", "))
}

/// The DEFAULT a domain gives a column that names it and gives none of its own: its own, or its
/// base domain's.
pub fn domain_default<'a>(
    domains: &'a [crate::catalog::Domain],
    name: &str,
) -> Option<&'a sqlparser::ast::Expr> {
    let d = domains.iter().find(|d| d.name == name)?;
    d.default
        .as_ref()
        .or_else(|| domain_default(domains, &d.base))
}

/// Whether a value of a column of `table` meets a domain's CHECK: where its condition is true or
/// NULL, its money read under the file's lc_monetary.
pub(super) fn check_holds(
    check: &sqlparser::ast::Expr,
    ty: ColumnType,
    d: &Datum,
    table: &Table,
) -> Result<bool> {
    let v = eval_on(
        check,
        &["value".to_string()],
        &[value_of(d, ty)],
        table.release,
        &Casings::under(table.monetary),
    )?;
    Ok(!matches!(v, Value::Bool(false)))
}

/// Refuses a value a domain does not allow: a NULL where it is NOT NULL, or one a CHECK of its own
/// or of its base domain finds false.
fn meets_domain(
    domains: &[crate::catalog::Domain],
    name: &str,
    ty: ColumnType,
    d: &Datum,
    release: u8,
    casings: &Casings,
) -> Result<()> {
    let Some(dom) = domains.iter().find(|x| x.name == name) else {
        return Ok(());
    };
    meets_domain(domains, &dom.base, ty, d, release, casings)?;
    if dom.not_null && *d == Datum::Null {
        return Err(Error::new(format!(
            "domain {} does not allow null values",
            dom.name
        )));
    }
    let names = ["value".to_string()];
    let value = [value_of(d, ty)];
    for k in &dom.checks {
        match eval_on(&k.check, &names, &value, release, casings)? {
            Value::Bool(false) => {
                return Err(Error::new(format!(
                    "value for domain {} violates check constraint \"{}\"",
                    dom.name, k.name
                )))
            }
            Value::Null | Value::Bool(true) => {}
            other => {
                return Err(Error::new(format!(
                    "{}: the CHECK {} gives {other:?}, not a truth",
                    dom.name, k.name
                )))
            }
        }
    }
    Ok(())
}

/// Refuses a part of a value of type `ty` that a domain its type names does not allow: an array's
/// elements, a composite's fields, each in turn.
fn meets_parts(
    domains: &[crate::catalog::Domain],
    ty: ColumnType,
    item: &crate::composed::Item,
    release: u8,
    casings: &Casings,
) -> Result<()> {
    use crate::composed::{datum_of, Composed, Item};
    let ColumnType::Composed(r) = ty else {
        return Ok(());
    };
    match (r.def(), item) {
        (Composed::Array { element, domain }, Item::Array { elements, .. }) => {
            for e in elements {
                if let Some(d) = domain {
                    meets_domain(
                        domains,
                        d,
                        *element,
                        &datum_of(*element, e),
                        release,
                        casings,
                    )?;
                }
                meets_parts(domains, *element, e, release, casings)?;
            }
        }
        (Composed::Record { fields, .. }, Item::Record(values)) => {
            for (f, v) in fields.iter().zip(values) {
                if let Some(d) = &f.domain {
                    meets_domain(domains, d, f.ty, &datum_of(f.ty, v), release, casings)?;
                }
                meets_parts(domains, f.ty, v, release, casings)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Refuses a row whose values a domain their columns name does not allow, or whose parts a domain
/// an array's element or a composite's field names does not. A domain's CHECK reads its money
/// under the file's lc_monetary.
pub fn check_domains(domains: &[crate::catalog::Domain], table: &Table, row: &Row) -> Result<()> {
    let casings = Casings::under(table.monetary);
    for (col, d) in table.columns.iter().zip(row) {
        if let Some(name) = &col.domain {
            meets_domain(domains, name, col.ty, d, table.release, &casings)?;
        }
        if let Datum::Compound(c) = d {
            meets_parts(domains, col.ty, &c.item, table.release, &casings)?;
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
    fn a_string_is_assigned_as_postgresql_s_casts_to_one_take_it() {
        use ColumnType::*;
        let a = |d: Datum, f, t| assign(d, f, t, "c");
        let text = |s: &str| Datum::Text(s.to_string());
        // A bpchar's trailing blanks go where it becomes text or a name, and stay where it stays
        // a bpchar, held to the length.
        assert_eq!(
            a(text("ab "), Bpchar(Some(3)), Text(None)).unwrap(),
            text("ab")
        );
        assert_eq!(a(text("ab "), Bpchar(Some(3)), Name).unwrap(), text("ab"));
        assert_eq!(
            a(text("ab "), Bpchar(Some(3)), Bpchar(Some(2))).unwrap(),
            text("ab")
        );
        assert!(a(text("abc"), Text(None), Bpchar(Some(2))).is_err());
        assert_eq!(a(Datum::Int(1), Boolean, Name).unwrap(), text("t"));
        assert_eq!(a(Datum::Int(1), Boolean, Text(None)).unwrap(), text("true"));
        let c = |b: u64| Datum::Int(crate::scalars::unsigned_key(b, 8));
        assert_eq!(a(c(0x61), Char, Bpchar(Some(2))).unwrap(), text("a "));
        for b in [0x80, 0xc3, 0xff] {
            assert!(a(c(b), Char, Bpchar(None)).is_err(), "{b:#x}");
        }
        let bits = |s: &str| Datum::Bytes(crate::strings::bits_parse(s, None, true).unwrap());
        assert_eq!(a(bits("101"), Varbit(None), Bit(3)).unwrap(), bits("101"));
        assert!(a(bits("1010"), Varbit(None), Bit(3)).is_err());
        assert!(a(bits("1010"), Bit(4), Varbit(Some(3))).is_err());
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
