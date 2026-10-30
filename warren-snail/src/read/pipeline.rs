//! A question read as a pipeline over one table in its order: each step of a WITH, and the question
//! itself, a SELECT over the table or a step before it. A window is read only along the table's own
//! order, folded as the rows come; a step keeps the order while it carries the order's key and
//! groups nothing. The expressions it reads are CASE, mod, abs, casts, comparisons, IS [NOT]
//! DISTINCT FROM, and the aggregates count, sum, min, max, bool_and and bool_or.

use crate::catalog::{Casing, ColumnType};
use crate::{Error, Result};
use sqlparser::ast::{
    BinaryOperator, CastKind, DataType, Expr, FunctionArg, FunctionArgExpr, FunctionArguments,
    GroupByExpr, Join, JoinOperator, ObjectNamePart, OrderByKind, OrderBySort, Query, SelectItem,
    SetExpr, TableFactor, TableWithJoins, UnaryOperator, Value as SqlValue, WindowFrameBound,
    WindowFrameUnits, WindowType,
};
use std::cmp::Ordering;
use std::collections::HashMap;

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
}

impl Value {
    fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// A numeric's digits at a scale at least its own.
    fn at_scale(m: i128, from: u32, to: u32) -> i128 {
        m * 10i128.pow(to - from)
    }

    /// The value as a numeric: its digits and scale.
    fn numeric(&self) -> Option<(i128, u32)> {
        match self {
            Value::Int(v) => Some((*v as i128, 0)),
            Value::Num(m, s) => Some((*m, *s)),
            _ => None,
        }
    }

    /// The value as PostgreSQL writes it as text.
    pub fn text(&self) -> Option<String> {
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
        })
    }

    /// The bytes a string is compared by, beside another string: text's and a name's own, a
    /// bpchar's without its trailing blanks.
    fn string_key(&self) -> Option<&[u8]> {
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

/// Two values compared as PostgreSQL compares values of one kind; none where either is NULL.
fn compare(a: &Value, b: &Value) -> Result<Option<Ordering>> {
    if a.is_null() || b.is_null() {
        return Ok(None);
    }
    if let (Some((x, sx)), Some((y, sy))) = (a.numeric(), b.numeric()) {
        let s = sx.max(sy);
        return Ok(Some(
            Value::at_scale(x, sx, s).cmp(&Value::at_scale(y, sy, s)),
        ));
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
    Ok(Some(match (a, b) {
        (Value::Bits(_, x), Value::Bits(_, y)) => x.cmp(y),
        (Value::Bytes(x), Value::Bytes(y)) => x.cmp(y),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (Value::Uuid(x), Value::Uuid(y)) => x.cmp(y),
        (Value::Scalar(a, x), Value::Scalar(b, y)) if a == b || a.is_oid() && b.is_oid() => {
            x.cmp(y)
        }
        (Value::Scalar(ty, _), other) | (other, Value::Scalar(ty, _)) => {
            return Err(Error::new(format!(
                "operator does not exist: {} = {}",
                ty.pg_name(),
                kind_name(other)
            )))
        }
        _ => return Err(Error::new(format!("{a:?} and {b:?} are not compared"))),
    }))
}

/// A value as a key that groups and partitions: equal values, at any scale, are one key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Key {
    Null,
    Num(i128, u32),
    Text(String),
    Bytes(Vec<u8>),
    Bool(bool),
    Uuid(i128),
    Scalar(ColumnType, i64),
    Real(i64),
    Double(i64),
}

fn key(v: &Value) -> Key {
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
        Value::Text(t) => Key::Text(t.clone()),
        Value::Bytes(b) => Key::Bytes(b.clone()),
        Value::Bool(b) => Key::Bool(*b),
        Value::Uuid(k) => Key::Uuid(*k),
        Value::Scalar(ty, k) => Key::Scalar(*ty, *k),
        Value::Real(f) => Key::Real(crate::scalars::float4_key(f.to_bits())),
        Value::Double(d) => Key::Double(crate::scalars::float8_key(d.to_bits())),
        Value::Str(..) => Key::Text(String::from_utf8_lossy(v.string_key().unwrap()).into()),
        Value::Bits(_, k) => Key::Bytes(k.clone()),
    }
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
struct Frame<'a> {
    columns: Vec<(Option<String>, String)>,
    params: &'a [String],
    /// The release whose catalog names a reg* value.
    release: u8,
    /// How the columns map case, by their names.
    casings: &'a Casings,
}

/// How the columns a statement reads and makes map case, by their names, where not as the
/// database's default does: a string column under a collation its DDL or a COLLATE names, a name
/// under its own C; and the collations the DDL makes. A name two columns map differently by maps
/// as the default.
#[derive(Clone, Debug, Default)]
pub struct Casings {
    columns: HashMap<String, Casing>,
    declared: Vec<crate::catalog::Collation>,
}

impl Casings {
    /// The casings of a table's columns, and the collations the DDL makes.
    pub fn of_table(
        table: &crate::catalog::Table,
        declared: &[crate::catalog::Collation],
    ) -> Casings {
        let mut c = Casings {
            columns: HashMap::new(),
            declared: declared.to_vec(),
        };
        for col in &table.columns {
            let named = match (&col.collation, col.ty) {
                (Some(n), _) => crate::catalog::collation_held(n, declared),
                (None, ColumnType::Name) => Some(Casing::Ascii),
                _ => None,
            };
            if let Some(k) = named {
                c.columns.insert(col.name.clone(), k);
            }
        }
        c
    }

    /// How the collation named maps case, where the snail holds it.
    fn named(&self, name: &str) -> Result<Casing> {
        crate::catalog::collation_held(name, &self.declared)
            .ok_or_else(|| Error::new(format!("COLLATE {name}: {}", crate::catalog::NOT_HELD)))
    }

    /// The casing an expression's text takes where a COLLATE or a column names one, as PostgreSQL
    /// carries a collation from a column or a COLLATE to what a function makes of it.
    fn implicit(&self, e: &Expr) -> Result<Option<Casing>> {
        Ok(match e {
            Expr::Collate { collation, .. } => {
                Some(self.named(&crate::catalog::object_name(collation))?)
            }
            Expr::Nested(x) => self.implicit(x)?,
            Expr::Identifier(_) | Expr::CompoundIdentifier(_) => {
                column_of(e).and_then(|(_, n)| self.columns.get(&n).copied())
            }
            Expr::Function(f) => match args(f).ok().and_then(|a| a.first().copied()) {
                Some(x) => self.implicit(x)?,
                None => None,
            },
            Expr::BinaryOp { left, right, .. } => match self.implicit(left)? {
                Some(k) => Some(k),
                None => self.implicit(right)?,
            },
            _ => None,
        })
    }

    /// The casings a step's columns take, by their names, added: each where its expression names
    /// one, and none where two columns of one name map differently.
    fn add_step(&mut self, q: &Query) -> Result<()> {
        let SetExpr::Select(sel) = q.body.as_ref() else {
            return Ok(());
        };
        for item in &sel.projection {
            let (e, name) = match item {
                SelectItem::UnnamedExpr(e) => (e, default_name(e)),
                SelectItem::ExprWithAlias { expr, alias } => (expr, ident(alias)),
                _ => continue,
            };
            let k = self.implicit(e)?;
            match (self.columns.get(&name).copied(), k) {
                (None, Some(k)) => {
                    self.columns.insert(name, k);
                }
                (Some(had), k) if Some(had) != k => {
                    self.columns.remove(&name);
                }
                _ => {}
            }
        }
        Ok(())
    }
}

impl Frame<'_> {
    fn find(&self, qualifier: Option<&str>, name: &str) -> Result<usize> {
        let found: Vec<usize> = self
            .columns
            .iter()
            .enumerate()
            .filter(|(_, (q, n))| n == name && (qualifier.is_none() || q.as_deref() == qualifier))
            .map(|(i, _)| i)
            .collect();
        match found.as_slice() {
            [i] => Ok(*i),
            [] => Err(Error::new(format!("{name} is no column the step reads"))),
            _ => Err(Error::new(format!(
                "{name} could name more than one column: qualify it"
            ))),
        }
    }
}

fn ident(i: &sqlparser::ast::Ident) -> String {
    crate::catalog::ident(i)
}

/// The column an expression names, where it names one.
fn column_of(e: &Expr) -> Option<(Option<String>, String)> {
    match e {
        Expr::Identifier(i) => Some((None, ident(i))),
        Expr::CompoundIdentifier(p) if p.len() == 2 => Some((Some(ident(&p[0])), ident(&p[1]))),
        Expr::Nested(e) => column_of(e),
        _ => None,
    }
}

/// The aggregates and windows an expression holds, found once each, by their place in the text.
#[derive(Default)]
struct Calls<'q> {
    aggregates: Vec<&'q Expr>,
    windows: Vec<&'q Expr>,
}

const AGGREGATES: [&str; 7] = ["count", "sum", "min", "max", "avg", "bool_and", "bool_or"];

fn function_name(f: &sqlparser::ast::Function) -> String {
    match f.name.0.last() {
        Some(ObjectNamePart::Identifier(i)) => ident(i),
        _ => f.name.to_string().to_lowercase(),
    }
}

fn find_calls<'q>(e: &'q Expr, out: &mut Calls<'q>) {
    match e {
        Expr::Function(f) => {
            if f.over.is_some() {
                out.windows.push(e);
                return;
            }
            if AGGREGATES.contains(&function_name(f).as_str()) {
                out.aggregates.push(e);
                return;
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
        _ => {}
    }
}

/// The values a row's expressions see beyond its columns: each aggregate's and window's, by place.
struct Computed<'q> {
    calls: HashMap<*const Expr, Value>,
    _q: std::marker::PhantomData<&'q ()>,
}

fn args(f: &sqlparser::ast::Function) -> Result<Vec<&Expr>> {
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

/// A numeric's text, in plain or exponent form, as its digits and their scale.
pub(crate) fn numeric_parts(s: &str) -> Result<(i128, u32)> {
    let (mantissa, exp) = match s.split_once(['e', 'E']) {
        Some((m, e)) => (
            m,
            e.parse::<i32>()
                .map_err(|_| Error::new(format!("'{s}' is not a number")))?,
        ),
        None => (s, 0),
    };
    let v = numeric_literal(mantissa)?;
    let (m, sc) = v.numeric().unwrap();
    let sc = sc as i32 - exp;
    Ok(if sc >= 0 {
        (m, sc as u32)
    } else {
        (
            10i128
                .checked_pow((-sc) as u32)
                .and_then(|p| m.checked_mul(p))
                .ok_or_else(|| Error::new(format!("'{s}' is out of range")))?,
            0,
        )
    })
}

fn numeric_literal(s: &str) -> Result<Value> {
    let t = s.trim();
    let (neg, t) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let (whole, frac) = t.split_once('.').unwrap_or((t, ""));
    if whole.is_empty() && frac.is_empty()
        || !whole.chars().all(|c| c.is_ascii_digit())
        || !frac.chars().all(|c| c.is_ascii_digit())
    {
        return Err(Error::new(format!("'{s}' is not a number")));
    }
    let digits = format!("{whole}{frac}");
    let m: i128 = digits
        .parse()
        .map_err(|_| Error::new(format!("'{s}' is out of range")))?;
    let m = if neg { -m } else { m };
    Ok(if frac.is_empty() {
        i64::try_from(m).map(Value::Int).unwrap_or(Value::Num(m, 0))
    } else {
        Value::Num(m, frac.len() as u32)
    })
}

/// Whether an expression is a quoted literal: of unknown type, which a cast reads by the target's
/// input.
fn quoted(e: &Expr) -> bool {
    match e {
        Expr::Value(v) => matches!(v.value, SqlValue::SingleQuotedString(_)),
        Expr::Nested(e) => quoted(e),
        _ => false,
    }
}

fn cast(v: Value, to: &DataType, release: u8, literal: bool) -> Result<Value> {
    if v.is_null() {
        return Ok(Value::Null);
    }
    let name = to.to_string().to_lowercase();
    let base = name.split('(').next().unwrap_or("").trim().to_string();
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
            Value::Int(x) => Value::Int(x),
            Value::Num(m, s) => {
                let d = 10i128.pow(s);
                let (q, r) = (m / d, (m % d).abs());
                let q = if r * 2 >= d { q + m.signum() } else { q };
                Value::Int(i64::try_from(q).map_err(|_| Error::new("out of range for an integer"))?)
            }
            Value::Text(t) => match numeric_literal(&t)? {
                Value::Int(x) => Value::Int(x),
                _ => return Err(Error::new(format!("'{t}' is not an integer"))),
            },
            other => return Err(Error::new(format!("{other:?} cast to an integer"))),
        },
        "numeric" | "decimal" => {
            let v = match v {
                Value::Text(t) => numeric_literal(&t)?,
                Value::Int(x) => Value::Num(x as i128, 0),
                Value::Num(m, s) => Value::Num(m, s),
                Value::Scalar(ColumnType::Money, k) => Value::Num(k as i128, 2),
                Value::Real(_) | Value::Double(_) => {
                    let wide = matches!(v, Value::Double(_));
                    let f = as_double(&v).unwrap();
                    if !f.is_finite() {
                        return Err(Error::new(format!(
                            "cannot convert {} to numeric: the snail holds no NaN or infinity in one",
                            if f.is_nan() { "NaN" } else { "infinity" }
                        )));
                    }
                    let (m, s) = numeric_parts(&crate::scalars::float_numeric_text(f, wide))?;
                    Value::Num(m, s)
                }
                other => return Err(Error::new(format!("{other:?} cast to numeric"))),
            };
            let (m, s) = v.numeric().unwrap();
            match to {
                DataType::Numeric(info) | DataType::Decimal(info) => match info {
                    sqlparser::ast::ExactNumberInfo::PrecisionAndScale(_, scale) => {
                        let scale = *scale as u32;
                        if scale >= s {
                            Value::Num(Value::at_scale(m, s, scale), scale)
                        } else {
                            let d = 10i128.pow(s - scale);
                            let (q, r) = (m / d, (m % d).abs());
                            Value::Num(if r * 2 >= d { q + m.signum() } else { q }, scale)
                        }
                    }
                    _ => Value::Num(m, s),
                },
                _ => Value::Num(m, s),
            }
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
            other => return Err(Error::new(format!("{other:?} cast to boolean"))),
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
            Ok(ty) if crate::scalars::is_scalar(ty) => scalar_cast(v, ty)?,
            Ok(ty @ (ColumnType::Bpchar(_) | ColumnType::Name)) => string_cast(v, ty, true)?,
            Ok(ty) if ty.is_bits() => bits_cast(v, ty, true)?,
            Ok(ty @ (ColumnType::Float4 | ColumnType::Float8)) => {
                float_cast(v, ty == ColumnType::Float8)?
            }
            _ => return Err(Error::new(format!("a cast to {to} is not read"))),
        },
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
fn kind_name(v: &Value) -> String {
    match v {
        Value::Null => "unknown".into(),
        Value::Int(x) if i32::try_from(*x).is_ok() => "integer".into(),
        Value::Int(_) => "bigint".into(),
        Value::Num(..) => "numeric".into(),
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
        (Value::Int(x), ColumnType::Money) => x
            .checked_mul(100)
            .ok_or_else(|| Error::new("bigint out of range"))?,
        (Value::Num(m, s), ColumnType::Money) => {
            i64::try_from(rounded(*m, *s, 2)).map_err(|_| Error::new("bigint out of range"))?
        }
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
            "operator does not exist: {} {op} {}",
            kind_name(x),
            kind_name(y)
        )))
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
    let money = |r: Option<i64>| {
        r.map(|v| Value::Scalar(Money, v))
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
        (B::Plus, Value::Scalar(Money, a), Value::Scalar(Money, b)) => money(a.checked_add(*b))?,
        (B::Minus, Value::Scalar(Money, a), Value::Scalar(Money, b)) => money(a.checked_sub(*b))?,
        (B::Multiply, Value::Scalar(Money, a), Value::Int(b))
        | (B::Multiply, Value::Int(b), Value::Scalar(Money, a)) => money(a.checked_mul(*b))?,
        (B::Divide, Value::Scalar(Money, a), Value::Int(b)) => {
            if *b == 0 {
                return Err(Error::new("division by zero"));
            }
            money(a.checked_div(*b))?
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
                "operator does not exist: {} {op} {}",
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
        (B::StringConcat, a, b)
            if matches!(a, Value::Text(_) | Value::Str(..))
                || matches!(b, Value::Text(_) | Value::Str(..)) =>
        {
            Value::Text(text_part(a) + &text_part(b))
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
                "operator does not exist: {} {op} {}",
                kind_name(x),
                kind_name(y)
            )))
        }
    }))
}

/// A LIKE or an ILIKE, as PostgreSQL reads one: text, a name, a bpchar's own text with its blanks,
/// or bytea, matched by a pattern of its kind, an ILIKE's both in lower case under `casing`.
fn like_of(v: &Value, p: &Value, escape: Option<char>, ilike: Option<Casing>) -> Result<bool> {
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
                t = crate::strings::case(&t, false, casing)?;
                q = crate::strings::case(&q, false, casing)?;
            }
            let (t, q): (Vec<char>, Vec<char>) = (t.chars().collect(), q.chars().collect());
            crate::strings::like(&t, &q, '%', '_', escape)
        }
        _ => Err(Error::new(format!(
            "operator does not exist: {} {} {}",
            kind_name(v),
            if ilike.is_some() { "~~*" } else { "~~" },
            kind_name(p)
        ))),
    }
}

/// A number as a double precision, as PostgreSQL widens it beside a float: an integer as itself
/// rounded, a numeric read from its text, a real exactly.
fn as_double(v: &Value) -> Option<f64> {
    Some(match v {
        Value::Int(x) => *x as f64,
        Value::Num(m, s) => numeric_text(*m, *s).parse().ok()?,
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
/// it divides by zero. Money times or divided by a float, rounded to the cent, and money divided
/// by money. None where neither side is a float.
fn float_arith(op: &BinaryOperator, x: &Value, y: &Value) -> Result<Option<Value>> {
    use BinaryOperator as B;
    let float = |v: &Value| matches!(v, Value::Real(_) | Value::Double(_));
    let money = |v: &Value| matches!(v, Value::Scalar(ColumnType::Money, _));
    if !(float(x) || float(y) || (money(x) && money(y) && matches!(op, B::Divide))) {
        return Ok(None);
    }
    let with_money = match (x, y) {
        (Value::Scalar(ColumnType::Money, c), f) if float(f) => Some((*c, as_double(f), true)),
        (f, Value::Scalar(ColumnType::Money, c)) if float(f) => Some((*c, as_double(f), false)),
        _ => None,
    };
    if let Some((c, f, money_left)) = with_money {
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
                    "operator does not exist: {} {op} {}",
                    kind_name(x),
                    kind_name(y)
                )))
            }
        };
        let r = r.round_ties_even();
        if !(-9.223_372_036_854_776e18..9.223_372_036_854_776e18).contains(&r) {
            return Err(Error::new("money out of range"));
        }
        return Ok(Some(Value::Scalar(ColumnType::Money, r as i64)));
    }
    if let (Value::Scalar(ColumnType::Money, a), Value::Scalar(ColumnType::Money, b)) = (x, y) {
        if *b == 0 {
            return Err(Error::new("division by zero"));
        }
        return Ok(Some(Value::Double(*a as f64 / *b as f64)));
    }
    let (Some(a), Some(b)) = (as_double(x), as_double(y)) else {
        return Err(Error::new(format!(
            "operator does not exist: {} {op} {}",
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
                "operator does not exist: {} {op} {}",
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

fn numeric_op(a: &Value, b: &Value, op: fn(i128, i128) -> Option<i128>) -> Result<Value> {
    if a.is_null() || b.is_null() {
        return Ok(Value::Null);
    }
    let (Some((x, sx)), Some((y, sy))) = (a.numeric(), b.numeric()) else {
        return Err(Error::new(format!("{a:?} and {b:?} are not numbers")));
    };
    let s = sx.max(sy);
    let r = op(Value::at_scale(x, sx, s), Value::at_scale(y, sy, s))
        .ok_or_else(|| Error::new("a number out of range, or divided by zero"))?;
    Ok(match (a, b) {
        (Value::Int(_), Value::Int(_)) => {
            Value::Int(i64::try_from(r).map_err(|_| Error::new("bigint out of range"))?)
        }
        _ => Value::Num(r, s),
    })
}

/// A product, as PostgreSQL multiplies: integers to an integer, any numeric to a numeric whose
/// scale is its factors' scales added.
fn multiply(a: &Value, b: &Value) -> Result<Value> {
    if a.is_null() || b.is_null() {
        return Ok(Value::Null);
    }
    if let (Value::Int(x), Value::Int(y)) = (a, b) {
        return x
            .checked_mul(*y)
            .map(Value::Int)
            .ok_or_else(|| Error::new("bigint out of range"));
    }
    let (Some((x, sx)), Some((y, sy))) = (a.numeric(), b.numeric()) else {
        return Err(Error::new(format!("{a:?} and {b:?} are not numbers")));
    };
    x.checked_mul(y)
        .map(|m| Value::Num(m, sx + sy))
        .ok_or_else(|| Error::new("a number out of range"))
}

fn truth(v: &Value) -> Result<Option<bool>> {
    match v {
        Value::Null => Ok(None),
        Value::Bool(b) => Ok(Some(*b)),
        other => Err(Error::new(format!("{other:?} is not a truth"))),
    }
}

/// The type a quoted literal beside a scalar of type `ty` is read as: an oid beside a reg* type,
/// whose comparisons are oid's; else `ty`.
fn literal_type(ty: ColumnType) -> ColumnType {
    if ty.is_oid() {
        ColumnType::Oid
    } else {
        ty
    }
}

/// A string type without its length: the type a literal beside one is read as.
fn unbounded(ty: ColumnType) -> ColumnType {
    match ty {
        ColumnType::Bpchar(_) => ColumnType::Bpchar(None),
        other => other,
    }
}

/// Two values compared, a quoted literal beside a boolean or a uuid read as one, as PostgreSQL gives
/// its unknown type the other side's.
fn beside(x: Value, y: Value, left: &Expr, right: &Expr) -> Result<(Value, Value)> {
    let quoted = |e: &Expr| matches!(e, Expr::Value(v) if matches!(v.value, SqlValue::SingleQuotedString(_)));
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
        (Value::Real(_) | Value::Double(_), Value::Text(t)) if quoted(right) => {
            let t = float_cast(Value::Text(t.clone()), matches!(x, Value::Double(_)))?;
            (x, t)
        }
        (Value::Text(t), Value::Real(_) | Value::Double(_)) if quoted(left) => (
            float_cast(Value::Text(t.clone()), matches!(y, Value::Double(_)))?,
            y,
        ),
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
        (Value::Bits(..), Value::Text(t)) if quoted(right) => {
            let t = bits_cast(Value::Text(t.clone()), ColumnType::Varbit(None), false)?;
            (x, t)
        }
        (Value::Text(t), Value::Bits(..)) if quoted(left) => (
            bits_cast(Value::Text(t.clone()), ColumnType::Varbit(None), false)?,
            y,
        ),
        (Value::Scalar(..), Value::Int(_)) => {
            let y = integer_beside(&x, y)?;
            (x, y)
        }
        // xid's comparisons with an integer take it on their right only.
        (Value::Int(_), Value::Scalar(ColumnType::Xid, _)) => {
            return Err(Error::new("operator does not exist: integer = xid"))
        }
        (Value::Int(_), Value::Scalar(..)) => {
            let x = integer_beside(&y, x)?;
            (x, y)
        }
        _ => (x, y),
    })
}

/// Evaluates an expression on one row of a frame, its aggregates' and windows' values computed.
fn eval(e: &Expr, row: &[Value], frame: &Frame, computed: &Computed) -> Result<Value> {
    if let Some(v) = computed.calls.get(&(e as *const Expr)) {
        return Ok(v.clone());
    }
    let ev = |x: &Expr| eval(x, row, frame, computed);
    Ok(match e {
        Expr::Identifier(_) | Expr::CompoundIdentifier(_) => {
            let (q, n) = column_of(e).ok_or_else(|| Error::new(format!("{e} is not read")))?;
            row[frame.find(q.as_deref(), &n)?].clone()
        }
        Expr::Nested(x) => ev(x)?,
        // Every collation the snail holds orders text by its bytes: a COLLATE changes no value.
        Expr::Collate { expr, collation } => {
            frame
                .casings
                .named(&crate::catalog::object_name(collation))?;
            ev(expr)?
        }
        Expr::Value(v) => match &v.value {
            SqlValue::Number(s, _) if s.contains(['e', 'E']) => {
                let (m, sc) = numeric_parts(s)?;
                Value::Num(m, sc)
            }
            SqlValue::Number(s, _) => numeric_literal(s)?,
            SqlValue::SingleQuotedString(s) => Value::Text(s.clone()),
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
        Expr::Cast {
            kind: CastKind::Cast | CastKind::DoubleColon,
            expr,
            data_type,
            ..
        } => cast(ev(expr)?, data_type, frame.release, quoted(expr))?,
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr,
        } => match ev(expr)? {
            Value::Null => Value::Null,
            Value::Int(x) => Value::Int(
                x.checked_neg()
                    .ok_or_else(|| Error::new("bigint out of range"))?,
            ),
            Value::Num(m, s) => Value::Num(-m, s),
            Value::Real(f) => Value::Real(-f),
            Value::Double(d) => Value::Double(-d),
            other => return Err(Error::new(format!("-{other:?} is not a number"))),
        },
        Expr::UnaryOp {
            op: UnaryOperator::BitwiseNot,
            expr,
        } => match ev(expr)? {
            Value::Null => Value::Null,
            Value::Bits(_, k) => {
                let k = crate::strings::bits_not(&k);
                Value::Bits(ColumnType::Bit(crate::strings::bits_of_key(&k).0), k)
            }
            Value::Scalar(t, k) if t.fixed_bytes().is_some() => {
                let bits = t.unsigned_bits();
                let v = !crate::scalars::unsigned_of(k, bits) & (u64::MAX >> (64 - bits));
                Value::Scalar(t, crate::scalars::unsigned_key(v, bits))
            }
            other => return Err(Error::new(format!("~{other:?} is not read"))),
        },
        Expr::UnaryOp {
            op: UnaryOperator::Not,
            expr,
        } => match truth(&ev(expr)?)? {
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
            let (v, p) = (ev(expr)?, ev(pattern)?);
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
            let ilike = match e {
                Expr::ILike { .. } => Some(match frame.casings.implicit(expr)? {
                    Some(k) => k,
                    None => frame.casings.implicit(pattern)?.unwrap_or(Casing::Libc),
                }),
                _ => None,
            };
            Value::Bool(like_of(&v, &p, escape, ilike)? != *negated)
        }
        Expr::IsNull(x) => Value::Bool(ev(x)?.is_null()),
        Expr::IsNotNull(x) => Value::Bool(!ev(x)?.is_null()),
        // A truth tested is never NULL: an unknown truth is neither true nor false.
        Expr::IsTrue(x) => Value::Bool(truth(&ev(x)?)? == Some(true)),
        Expr::IsNotTrue(x) => Value::Bool(truth(&ev(x)?)? != Some(true)),
        Expr::IsFalse(x) => Value::Bool(truth(&ev(x)?)? == Some(false)),
        Expr::IsNotFalse(x) => Value::Bool(truth(&ev(x)?)? != Some(false)),
        Expr::IsUnknown(x) => Value::Bool(truth(&ev(x)?)?.is_none()),
        Expr::IsNotUnknown(x) => Value::Bool(truth(&ev(x)?)?.is_some()),
        Expr::IsDistinctFrom(a, b) | Expr::IsNotDistinctFrom(a, b) => {
            let (x, y) = beside(ev(a)?, ev(b)?, a, b)?;
            let same = match (x.is_null(), y.is_null()) {
                (true, true) => true,
                (false, false) => compare(&x, &y)? == Some(Ordering::Equal),
                _ => false,
            };
            Value::Bool(same == matches!(e, Expr::IsNotDistinctFrom(..)))
        }
        Expr::BinaryOp { left, op, right } => {
            match op {
                BinaryOperator::And | BinaryOperator::Or => {
                    let (x, y) = (truth(&ev(left)?)?, truth(&ev(right)?)?);
                    let and = matches!(op, BinaryOperator::And);
                    return Ok(match (x, y) {
                        (Some(a), Some(b)) => Value::Bool(if and { a && b } else { a || b }),
                        (Some(false), None) | (None, Some(false)) if and => Value::Bool(false),
                        (Some(true), None) | (None, Some(true)) if !and => Value::Bool(true),
                        _ => Value::Null,
                    });
                }
                _ => {}
            }
            let (x, y) = beside(ev(left)?, ev(right)?, left, right)?;
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
                if let Some(v) = float_arith(op, &x, &y)? {
                    return Ok(v);
                }
                if let Some(v) = scalar_arith(op, &x, &y)? {
                    return Ok(v);
                }
            } else {
                compared(op, &x, &y)?;
            }
            match op {
                BinaryOperator::Plus => numeric_op(&x, &y, i128::checked_add)?,
                BinaryOperator::Minus => numeric_op(&x, &y, i128::checked_sub)?,
                BinaryOperator::Modulo => numeric_op(&x, &y, i128::checked_rem)?,
                BinaryOperator::Multiply => multiply(&x, &y)?,
                BinaryOperator::Eq
                | BinaryOperator::NotEq
                | BinaryOperator::Lt
                | BinaryOperator::LtEq
                | BinaryOperator::Gt
                | BinaryOperator::GtEq => match compare(&x, &y)? {
                    None => Value::Null,
                    Some(o) => Value::Bool(match op {
                        BinaryOperator::Eq => o == Ordering::Equal,
                        BinaryOperator::NotEq => o != Ordering::Equal,
                        BinaryOperator::Lt => o == Ordering::Less,
                        BinaryOperator::LtEq => o != Ordering::Greater,
                        BinaryOperator::Gt => o == Ordering::Greater,
                        _ => o != Ordering::Less,
                    }),
                },
                other => return Err(Error::new(format!("the operator {other} is not read"))),
            }
        }
        Expr::Case {
            operand: None,
            conditions,
            else_result,
            ..
        } => {
            for c in conditions {
                if truth(&ev(&c.condition)?)? == Some(true) {
                    return ev(&c.result);
                }
            }
            match else_result {
                Some(x) => ev(x)?,
                None => Value::Null,
            }
        }
        Expr::Function(f) => {
            let name = function_name(f);
            let a = args(f)?;
            match (name.as_str(), a.as_slice()) {
                ("mod", [x, y]) => numeric_op(&ev(x)?, &ev(y)?, i128::checked_rem)?,
                // Under the collation its argument carries, the database's default where none.
                ("upper" | "lower", [x]) => {
                    let casing = frame.casings.implicit(x)?.unwrap_or(Casing::Libc);
                    match ev(x)? {
                        Value::Null => Value::Null,
                        v @ (Value::Text(_) | Value::Str(..)) => Value::Text(crate::strings::case(
                            std::str::from_utf8(v.string_key().unwrap()).unwrap_or_default(),
                            name == "upper",
                            casing,
                        )?),
                        other => {
                            return Err(Error::new(format!(
                                "function {name}({}) does not exist",
                                kind_name(&other)
                            )))
                        }
                    }
                }
                ("abs", [x]) => match ev(x)? {
                    Value::Null => Value::Null,
                    Value::Int(v) => Value::Int(
                        v.checked_abs()
                            .ok_or_else(|| Error::new("bigint out of range"))?,
                    ),
                    Value::Num(m, s) => Value::Num(m.abs(), s),
                    Value::Real(f) => Value::Real(f.abs()),
                    Value::Double(d) => Value::Double(d.abs()),
                    other => return Err(Error::new(format!("abs of {other:?}"))),
                },
                _ => return Err(Error::new(format!("{e} is not read"))),
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

/// Columns by their qualifier and name.
type Columns = Vec<(Option<String>, String)>;

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
                let q = alias
                    .as_ref()
                    .map(|a| ident(&a.name))
                    .unwrap_or_else(|| n.rsplit('.').next().unwrap_or(&n).to_string());
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
            [o] if !matches!(o.options.sort, Some(OrderBySort::Desc))
                && o.options.nulls_first != Some(true)
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
            _q: std::marker::PhantomData,
        };
        let running = state.entry(call as *const Expr).or_default();
        for (r, row) in rel.rows.iter().enumerate() {
            let part: Vec<Key> = spec
                .partition_by
                .iter()
                .map(|p| eval(p, row, frame, &none).map(|v| key(&v)))
                .collect::<Result<_>>()?;
            let state = running.entry(part).or_insert(Value::Null);
            let value = match (name.as_str(), a.as_slice()) {
                ("lag", [x]) => {
                    let now = eval(x, row, frame, &none)?;
                    std::mem::replace(state, now)
                }
                ("sum", [x]) => {
                    let v = eval(x, row, frame, &none)?;
                    if !v.is_null() {
                        *state = if state.is_null() {
                            v
                        } else {
                            numeric_op(state, &v, i128::checked_add)?
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

/// The output name PostgreSQL gives an item it is not told.
fn default_name(e: &Expr) -> String {
    match e {
        Expr::Identifier(i) => ident(i),
        Expr::CompoundIdentifier(p) => ident(p.last().unwrap()),
        Expr::Function(f) => function_name(f),
        Expr::Cast { expr, .. } => default_name(expr),
        Expr::Nested(x) => default_name(x),
        Expr::Case { .. } => "case".into(),
        _ => "?column?".into(),
    }
}

/// One aggregate, folded a row at a time.
enum Acc {
    Count(i64),
    Sum(Value),
    Least(Option<Value>),
    Greatest(Option<Value>),
    All(Option<bool>),
    Any(Option<bool>),
    /// avg of a float: its rows, their sum and their sum of squared deviations, as
    /// PostgreSQL's float8_accum folds them.
    Avg(f64, f64, f64),
}

impl Acc {
    /// The aggregate a call folds, and the expression it folds: none for count(*).
    fn of(call: &Expr) -> Result<(Acc, Option<&Expr>)> {
        let Expr::Function(f) = call else {
            unreachable!()
        };
        let name = function_name(f);
        if name == "count" && is_count_star(f) {
            return Ok((Acc::Count(0), None));
        }
        let a = args(f)?;
        let [x] = a.as_slice() else {
            return Err(Error::new(format!("{call} is not read")));
        };
        let acc = match name.as_str() {
            "count" => Acc::Count(0),
            "sum" => Acc::Sum(Value::Null),
            "min" => Acc::Least(None),
            "max" => Acc::Greatest(None),
            "bool_and" => Acc::All(None),
            "bool_or" => Acc::Any(None),
            "avg" => Acc::Avg(0.0, 0.0, 0.0),
            _ => return Err(Error::new(format!("{call} is not read"))),
        };
        Ok((acc, Some(*x)))
    }

    /// Folds in one row: its value, or none for count(*). A NULL is passed over.
    fn add(&mut self, v: Option<Value>) -> Result<()> {
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
        // Of the scalars, sum is money's alone; min and max are an oid's, an xid8's, a pg_lsn's,
        // money's and a tid's, and a `"char"`'s through its text.
        let v = match (&v, &self) {
            (Value::Scalar(ColumnType::Char, _), Acc::Least(_) | Acc::Greatest(_)) => {
                Value::Text(v.text().unwrap_or_default())
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
                if !(matches!(self, Acc::Sum(_)) && *ty == ColumnType::Money
                    || !matches!(self, Acc::Sum(_))
                        && matches!(
                            ty,
                            ColumnType::Oid
                                | ColumnType::Oid8
                                | ColumnType::Xid8
                                | ColumnType::PgLsn
                                | ColumnType::Money
                                | ColumnType::Tid
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
        match self {
            Acc::Count(n) => *n += 1,
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
                    numeric_op(acc, &v, i128::checked_add)?
                };
            }
            Acc::Least(best) => {
                let better = match best {
                    None => true,
                    Some(b) => compare(&v, b)? == Some(Ordering::Less),
                };
                if better {
                    *best = Some(v);
                }
            }
            Acc::Greatest(best) => {
                let better = match best {
                    None => true,
                    Some(b) => compare(&v, b)? == Some(Ordering::Greater),
                };
                if better {
                    *best = Some(v);
                }
            }
            Acc::All(a) => {
                let t = truth(&v)?.unwrap_or(true);
                *a = Some(a.map_or(t, |x| x && t));
            }
            Acc::Any(a) => {
                let t = truth(&v)?.unwrap_or(false);
                *a = Some(a.map_or(t, |x| x || t));
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

    fn value(&self) -> Value {
        match self {
            Acc::Count(n) => Value::Int(*n),
            Acc::Sum(v) => v.clone(),
            Acc::Least(b) | Acc::Greatest(b) => b.clone().unwrap_or(Value::Null),
            Acc::All(a) | Acc::Any(a) => a.map_or(Value::Null, Value::Bool),
            Acc::Avg(n, sx, _) if *n > 0.0 => Value::Double(sx / n),
            Acc::Avg(..) => Value::Null,
        }
    }
}

/// Each window call's running fold, for each of its partitions.
type Running = HashMap<*const Expr, HashMap<Vec<Key>, Value>>;

/// A SELECT of the pipeline as it is read: what it returns, its windows and aggregates, and how it
/// groups.
struct Prepared<'q> {
    from: &'q [TableWithJoins],
    selection: Option<&'q Expr>,
    order_by: Option<&'q sqlparser::ast::OrderBy>,
    items: Vec<(&'q Expr, String)>,
    calls: Calls<'q>,
    grouping: &'q [Expr],
    grouped: bool,
}

fn prepare(q: &Query) -> Result<Prepared<'_>> {
    let s = match q.body.as_ref() {
        SetExpr::Select(s) => s,
        _ => return Err(Error::new("a step is a SELECT")),
    };
    if s.distinct.is_some() || s.having.is_some() || q.limit_clause.is_some() {
        return Err(Error::new(
            "DISTINCT, HAVING and LIMIT are not read in a step",
        ));
    }
    let items: Vec<(&Expr, String)> = s
        .projection
        .iter()
        .map(|i| match i {
            SelectItem::UnnamedExpr(e) => Ok((e, default_name(e))),
            SelectItem::ExprWithAlias { expr, alias } => Ok((expr, ident(alias))),
            _ => Err(Error::new("name each column a step returns")),
        })
        .collect::<Result<_>>()?;
    let mut calls = Calls::default();
    for (e, _) in &items {
        find_calls(e, &mut calls);
    }
    let grouping = match &s.group_by {
        GroupByExpr::Expressions(g, m) if m.is_empty() => g.as_slice(),
        _ => return Err(Error::new("GROUP BY ALL is not read")),
    };
    let grouped = !grouping.is_empty() || !calls.aggregates.is_empty();
    if grouped && !calls.windows.is_empty() {
        return Err(Error::new("a window over groups is not read"));
    }
    Ok(Prepared {
        from: &s.from,
        selection: s.selection.as_ref(),
        order_by: q.order_by.as_ref(),
        items,
        calls,
        grouping,
        grouped,
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
    let frame = Frame {
        columns: names.iter().map(|n| (None, n.clone())).collect(),
        params: &[],
        release,
        casings,
    };
    eval(e, row, &frame, &computed_none())
}

/// The values of a row's aggregates and windows: none.
fn computed_none<'q>() -> Computed<'q> {
    Computed {
        calls: HashMap::new(),
        _q: std::marker::PhantomData,
    }
}

/// One SELECT read over rows given a chunk at a time, in the order: its windows' running folds and
/// its groups kept from chunk to chunk. A step that neither groups nor sorts hands each chunk on.
struct StepRun<'q> {
    p: Prepared<'q>,
    running: Running,
    at: HashMap<Vec<Key>, usize>,
    /// Each group's first row and its aggregates' folds.
    groups: Vec<(Vec<Value>, Vec<Acc>)>,
    /// The rows held for an ORDER BY.
    rows: Vec<Vec<Value>>,
    /// The columns the step's FROM reads, as the last chunk gave them.
    columns: Columns,
}

impl<'q> StepRun<'q> {
    fn new(p: Prepared<'q>) -> StepRun<'q> {
        StepRun {
            p,
            running: HashMap::new(),
            at: HashMap::new(),
            groups: Vec::new(),
            rows: Vec::new(),
            columns: Vec::new(),
        }
    }

    fn names(&self) -> Vec<String> {
        self.p.items.iter().map(|(_, n)| n.clone()).collect()
    }

    fn fresh(&self) -> Result<Vec<Acc>> {
        self.p
            .calls
            .aggregates
            .iter()
            .map(|&c| Acc::of(c).map(|(a, _)| a))
            .collect()
    }

    /// Reads one chunk of the rows its FROM gives: the rows the step returns for it, unless the step
    /// groups or sorts, which hold them until `finish`.
    fn feed(
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
        };
        let none = computed_none();
        if let Some(w) = self.p.selection {
            let mut kept = Vec::with_capacity(rel.rows.len());
            for row in std::mem::take(&mut rel.rows) {
                if truth(&eval(w, &row, &frame, &none)?)? == Some(true) {
                    kept.push(row);
                }
            }
            rel.rows = kept;
        }
        if self.p.grouped {
            let folded: Vec<Option<&Expr>> = self
                .p
                .calls
                .aggregates
                .iter()
                .map(|&c| Acc::of(c).map(|(_, x)| x))
                .collect::<Result<_>>()?;
            for row in rel.rows {
                let k: Vec<Key> = self
                    .p
                    .grouping
                    .iter()
                    .map(|g| eval(g, &row, &frame, &none).map(|v| key(&v)))
                    .collect::<Result<_>>()?;
                let i = match self.at.get(&k) {
                    Some(&i) => i,
                    None => {
                        let accs = self.fresh()?;
                        self.groups.push((row.clone(), accs));
                        self.at.insert(k, self.groups.len() - 1);
                        self.groups.len() - 1
                    }
                };
                for (acc, x) in self.groups[i].1.iter_mut().zip(&folded) {
                    acc.add(x.map(|x| eval(x, &row, &frame, &none)).transpose()?)?;
                }
            }
            self.columns = frame.columns;
            return Ok(None);
        }
        let per_row = windows(&self.p.calls.windows, &rel, &frame, &mut self.running)?;
        let mut out = Relation {
            names: self.names(),
            rows: Vec::with_capacity(rel.rows.len()),
            order: None,
        };
        for (row, w) in rel.rows.iter().zip(per_row) {
            let computed = Computed {
                calls: w,
                _q: std::marker::PhantomData,
            };
            out.rows.push(
                self.p
                    .items
                    .iter()
                    .map(|(e, _)| eval(e, row, &frame, &computed))
                    .collect::<Result<_>>()?,
            );
        }
        // The order holds where an item is the column that held it.
        out.order = rel.order.and_then(|o| {
            self.p.items.iter().position(|(e, _)| {
                column_of(e).is_some_and(|(q, n)| frame.find(q.as_deref(), &n).ok() == Some(o))
            })
        });
        self.columns = frame.columns;
        if self.p.order_by.is_some() {
            self.rows.extend(out.rows);
            return Ok(None);
        }
        Ok(Some(out))
    }

    /// The rows a step that groups or sorts returns, once every chunk is read.
    fn finish(mut self, params: &[String], release: u8, casings: &Casings) -> Result<Relation> {
        let mut out = Relation {
            names: self.names(),
            rows: std::mem::take(&mut self.rows),
            order: None,
        };
        if self.p.grouped {
            if self.groups.is_empty() && self.p.grouping.is_empty() {
                let accs = self.fresh()?;
                self.groups
                    .push((vec![Value::Null; self.columns.len()], accs));
            }
            let frame = Frame {
                columns: std::mem::take(&mut self.columns),
                params,
                release,
                casings,
            };
            for (first, accs) in &self.groups {
                let computed = Computed {
                    calls: self
                        .p
                        .calls
                        .aggregates
                        .iter()
                        .zip(accs)
                        .map(|(&c, a)| (c as *const Expr, a.value()))
                        .collect(),
                    _q: std::marker::PhantomData,
                };
                out.rows.push(
                    self.p
                        .items
                        .iter()
                        .map(|(e, _)| eval(e, first, &frame, &computed))
                        .collect::<Result<_>>()?,
                );
            }
        }
        if let Some(ob) = self.p.order_by {
            sort(&mut out, ob, params, release, casings)?;
        }
        Ok(out)
    }
}

/// A step's rows sorted as its ORDER BY says, on the step's own columns.
fn sort(
    out: &mut Relation,
    ob: &sqlparser::ast::OrderBy,
    params: &[String],
    release: u8,
    casings: &Casings,
) -> Result<()> {
    let exprs = match &ob.kind {
        OrderByKind::Expressions(e) => e,
        _ => return Err(Error::new("ORDER BY ALL is not read")),
    };
    let named = Frame {
        columns: out.names.iter().map(|n| (None, n.clone())).collect(),
        params,
        release,
        casings,
    };
    let none = computed_none();
    let mut keyed: Vec<(Vec<Value>, Vec<Value>)> = std::mem::take(&mut out.rows)
        .into_iter()
        .map(|row| {
            let k = exprs
                .iter()
                .map(|o| {
                    let e = match column_of(&o.expr) {
                        Some((_, n)) => Expr::Identifier(sqlparser::ast::Ident::new(n)),
                        None => o.expr.clone(),
                    };
                    let v = eval(&e, &row, &named, &none)?;
                    match v {
                        Value::Scalar(ty, _) if !ty.ordered() => Err(Error::new(format!(
                            "could not identify an ordering operator for type {}",
                            ty.pg_name()
                        ))),
                        v => Ok(v),
                    }
                })
                .collect::<Result<Vec<_>>>()?;
            Ok((k, row))
        })
        .collect::<Result<_>>()?;
    let mut failed = None;
    keyed.sort_by(|a, b| {
        for (i, o) in exprs.iter().enumerate() {
            let desc = matches!(o.options.sort, Some(OrderBySort::Desc));
            let nulls_first = o.options.nulls_first.unwrap_or(desc);
            let ord = match (a.0[i].is_null(), b.0[i].is_null()) {
                (true, true) => Ordering::Equal,
                (true, false) => {
                    if nulls_first {
                        Ordering::Less
                    } else {
                        Ordering::Greater
                    }
                }
                (false, true) => {
                    if nulls_first {
                        Ordering::Greater
                    } else {
                        Ordering::Less
                    }
                }
                (false, false) => match compare(&a.0[i], &b.0[i]) {
                    Ok(Some(o)) => {
                        if desc {
                            o.reverse()
                        } else {
                            o
                        }
                    }
                    Ok(None) => Ordering::Equal,
                    Err(e) => {
                        failed.get_or_insert(e);
                        Ordering::Equal
                    }
                },
            };
            if ord != Ordering::Equal {
                return ord;
            }
        }
        Ordering::Equal
    });
    if let Some(e) = failed {
        return Err(e);
    }
    out.rows = keyed.into_iter().map(|(_, r)| r).collect();
    out.order = None;
    Ok(())
}

/// One SELECT read whole, over the steps before it and the table.
fn select(
    q: &Query,
    steps: &HashMap<String, Relation>,
    table: &Table,
    params: &[String],
    release: u8,
    casings: &Casings,
) -> Result<Relation> {
    let p = prepare(q)?;
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
    /// The steps read whole, then each streamed step's chunk as it is read.
    view: HashMap<String, Relation>,
    /// The streamed relations a step read after the rows have come reads, gathered.
    kept: HashMap<String, Relation>,
    fed: bool,
    /// The most rows held at once.
    most: usize,
}

impl<'q> Stream<'q> {
    /// `names` are the table's columns, and `order` the one holding its order's key, where the
    /// windows can read along it.
    pub fn new(
        q: &'q Query,
        table: &str,
        names: Vec<String>,
        order: Option<usize>,
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
            let p = prepare(query)?;
            s.casings.add_step(query)?;
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
                if p.grouped || p.order_by.is_some() {
                    Kind::Folded
                } else {
                    Kind::Streamed
                }
            } else {
                Kind::Fixed
            };
            if kind == Kind::Fixed {
                let r = select(query, &s.view, &s.table, params, release, &s.casings)?;
                s.view.insert(name.clone(), r);
            }
            s.runs.push(match kind {
                Kind::Streamed | Kind::Folded => Some(StepRun::new(p)),
                _ => None,
            });
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
                    r.groups.len()
                        + r.rows.len()
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
        for (i, (name, query, kind)) in steps.into_iter().enumerate() {
            let r = match kind {
                Kind::Fixed => continue,
                Kind::Streamed => match self.kept.remove(&name) {
                    Some(k) => k,
                    None => continue,
                },
                Kind::Folded => self.runs[i].take().unwrap().finish(
                    &self.params,
                    self.release,
                    &self.casings,
                )?,
                Kind::After => select(
                    query,
                    &whole,
                    &table,
                    &self.params,
                    self.release,
                    &self.casings,
                )?,
            };
            whole.insert(name, r);
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
        table.relation.order,
        params,
        crate::regs::LATEST,
        Casings::default(),
    )?;
    s.feed(table.relation.clone())?;
    s.finish()
}

/// Whether a question is read as a pipeline: a WITH, or a window in what it returns.
pub fn is_pipeline(q: &Query) -> bool {
    if q.with.is_some() {
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
        for (r, row) in rows.iter_mut().enumerate() {
            row.push(if a.is_null(r) {
                Value::Null
            } else if let Some(ty) = string.filter(|t| t.is_bits()) {
                Value::Bits(
                    ty,
                    crate::strings::bits_key_at(ty, a.as_ref(), r).ok_or_else(|| {
                        Error::new(format!("{} is not of its type", field.name()))
                    })?,
                )
            } else if let Some(ty) = string {
                Value::Str(
                    ty,
                    any.downcast_ref::<StringArray>()
                        .ok_or_else(|| Error::new(format!("{} is not of its type", field.name())))?
                        .value(r)
                        .to_string(),
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
                    A::Timestamp(..) => Value::Int(
                        any.downcast_ref::<TimestampMicrosecondArray>()
                            .unwrap()
                            .value(r),
                    ),
                    A::Decimal128(_, s) => Value::Num(
                        any.downcast_ref::<Decimal128Array>().unwrap().value(r),
                        *s as u32,
                    ),
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

/// The one table a pipeline reads, by name: every table its steps name that no step of the WITH is.
pub fn table_named(q: &Query) -> Result<String> {
    let mut steps: Vec<String> = Vec::new();
    let mut tables: Vec<String> = Vec::new();
    let mut note = |s: &sqlparser::ast::Select, steps: &[String]| {
        for item in &s.from {
            let mut names = vec![&item.relation];
            names.extend(item.joins.iter().map(|j| &j.relation));
            for t in names {
                if let TableFactor::Table { name, .. } = t {
                    let n = crate::catalog::object_name(name);
                    if !steps.contains(&n) && !tables.contains(&n) {
                        tables.push(n);
                    }
                }
            }
        }
    };
    if let Some(with) = &q.with {
        for cte in &with.cte_tables {
            if let SetExpr::Select(s) = cte.query.body.as_ref() {
                note(s, &steps);
            }
            steps.push(ident(&cte.alias.name));
        }
    }
    if let SetExpr::Select(s) = q.body.as_ref() {
        note(s, &steps);
    }
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
                t.relation.order,
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
    fn a_window_off_the_order_or_descending_is_refused() {
        for (sql, why) in [
            ("SELECT lag(seq) OVER (ORDER BY g) FROM t", "along the table's order"),
            ("SELECT lag(seq) OVER (ORDER BY seq DESC) FROM t", "ascending"),
            (
                "SELECT sum(amount) OVER (ORDER BY seq ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t",
                "partition's first",
            ),
        ] {
            let e = run(&query(sql), &table(), &[]).unwrap_err().to_string();
            assert!(e.contains(why), "{sql}: {e}");
        }
    }
}
