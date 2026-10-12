//! A question read as a pipeline over one table in its order: each step of a WITH, and the question
//! itself, a SELECT over the table or a step before it. A window is read only along the table's own
//! order, folded as the rows come; a step keeps the order while it carries the order's key and
//! groups nothing. The expressions it reads are CASE, mod, abs, casts, comparisons, IS [NOT]
//! DISTINCT FROM, and the aggregates count, sum, min, max, bool_and and bool_or.

use crate::{Error, Result};
use sqlparser::ast::{
    BinaryOperator, CastKind, DataType, Expr, FunctionArg, FunctionArgExpr, FunctionArguments,
    GroupByExpr, Join, JoinOperator, ObjectNamePart, OrderByKind, OrderBySort, Query, SelectItem,
    SetExpr, TableFactor, TableWithJoins, UnaryOperator, Value as SqlValue, WindowFrameBound,
    WindowFrameUnits, WindowType,
};
use std::cmp::Ordering;
use std::collections::HashMap;

/// One value of a row: a number whole or with its scale, text, bytes, or a truth.
#[derive(Clone, Debug)]
pub enum Value {
    Null,
    Int(i64),
    /// A numeric: its digits as one integer, and how many of them lie after the point.
    Num(i128, u32),
    Text(String),
    Bytes(Vec<u8>),
    Bool(bool),
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
        })
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
    Ok(Some(match (a, b) {
        (Value::Text(x), Value::Text(y)) => x.as_bytes().cmp(y.as_bytes()),
        (Value::Bytes(x), Value::Bytes(y)) => x.cmp(y),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
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

const AGGREGATES: [&str; 6] = ["count", "sum", "min", "max", "bool_and", "bool_or"];

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
        | Expr::IsFalse(expr) => find_calls(expr, out),
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

fn cast(v: Value, to: &DataType) -> Result<Value> {
    if v.is_null() {
        return Ok(Value::Null);
    }
    let name = to.to_string().to_lowercase();
    let base = name.split('(').next().unwrap_or("").trim().to_string();
    Ok(match base.as_str() {
        "text" | "varchar" | "character varying" => Value::Text(v.text().unwrap()),
        "bigint" | "int8" | "integer" | "int" | "int4" | "smallint" | "int2" => match v {
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
            Value::Text(t) => Value::Bytes(hex_bytes(&t)?),
            other => return Err(Error::new(format!("{other:?} cast to bytea"))),
        },
        "boolean" | "bool" => match v {
            Value::Bool(b) => Value::Bool(b),
            Value::Text(t) if t == "true" || t == "t" => Value::Bool(true),
            Value::Text(t) if t == "false" || t == "f" => Value::Bool(false),
            other => return Err(Error::new(format!("{other:?} cast to boolean"))),
        },
        _ => return Err(Error::new(format!("a cast to {to} is not read"))),
    })
}

/// Bytes written as PostgreSQL's hex form, `\x` and two hex digits a byte.
pub fn hex_bytes(t: &str) -> Result<Vec<u8>> {
    let h = t
        .strip_prefix("\\x")
        .ok_or_else(|| Error::new(format!("'{t}' is not bytea's hex form")))?;
    if h.len() % 2 != 0 {
        return Err(Error::new(format!("'{t}' has an odd count of hex digits")));
    }
    (0..h.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&h[i..i + 2], 16)
                .map_err(|_| Error::new(format!("'{t}' is not bytea's hex form")))
        })
        .collect()
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

fn truth(v: &Value) -> Result<Option<bool>> {
    match v {
        Value::Null => Ok(None),
        Value::Bool(b) => Ok(Some(*b)),
        other => Err(Error::new(format!("{other:?} is not a truth"))),
    }
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
        Expr::Value(v) => match &v.value {
            SqlValue::Number(s, _) => numeric_literal(s)?,
            SqlValue::SingleQuotedString(s) => Value::Text(s.clone()),
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
        } => cast(ev(expr)?, data_type)?,
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
            other => return Err(Error::new(format!("-{other:?} is not a number"))),
        },
        Expr::UnaryOp {
            op: UnaryOperator::Not,
            expr,
        } => match truth(&ev(expr)?)? {
            None => Value::Null,
            Some(b) => Value::Bool(!b),
        },
        Expr::IsNull(x) => Value::Bool(ev(x)?.is_null()),
        Expr::IsNotNull(x) => Value::Bool(!ev(x)?.is_null()),
        Expr::IsDistinctFrom(a, b) | Expr::IsNotDistinctFrom(a, b) => {
            let (x, y) = (ev(a)?, ev(b)?);
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
            let (x, y) = (ev(left)?, ev(right)?);
            match op {
                BinaryOperator::Plus => numeric_op(&x, &y, i128::checked_add)?,
                BinaryOperator::Minus => numeric_op(&x, &y, i128::checked_sub)?,
                BinaryOperator::Modulo => numeric_op(&x, &y, i128::checked_rem)?,
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
                ("abs", [x]) => match ev(x)? {
                    Value::Null => Value::Null,
                    Value::Int(v) => Value::Int(
                        v.checked_abs()
                            .ok_or_else(|| Error::new("bigint out of range"))?,
                    ),
                    Value::Num(m, s) => Value::Num(m.abs(), s),
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
                && column_of(&o.expr)
                    .is_some_and(|(q, n)| frame.find(q.as_deref(), &n).ok() == Some(order)) => {}
            _ => {
                return Err(Error::new(format!(
                    "{call}: a window is read only along the table's order, ascending"
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
        match self {
            Acc::Count(n) => *n += 1,
            // The sum of whole numbers is a numeric of scale 0, as PostgreSQL writes it.
            Acc::Sum(acc) => {
                *acc = if acc.is_null() {
                    match v {
                        Value::Int(x) => Value::Num(x as i128, 0),
                        other => other,
                    }
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
        }
        Ok(())
    }

    fn value(&self) -> Value {
        match self {
            Acc::Count(n) => Value::Int(*n),
            Acc::Sum(v) => v.clone(),
            Acc::Least(b) | Acc::Greatest(b) => b.clone().unwrap_or(Value::Null),
            Acc::All(a) | Acc::Any(a) => a.map_or(Value::Null, Value::Bool),
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
    ) -> Result<Option<Relation>> {
        let frame = Frame { columns, params };
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
    fn finish(mut self, params: &[String]) -> Result<Relation> {
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
            sort(&mut out, ob, params)?;
        }
        Ok(out)
    }
}

/// A step's rows sorted as its ORDER BY says, on the step's own columns.
fn sort(out: &mut Relation, ob: &sqlparser::ast::OrderBy, params: &[String]) -> Result<()> {
    let exprs = match &ob.kind {
        OrderByKind::Expressions(e) => e,
        _ => return Err(Error::new("ORDER BY ALL is not read")),
    };
    let named = Frame {
        columns: out.names.iter().map(|n| (None, n.clone())).collect(),
        params,
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
                    eval(&e, &row, &named, &none)
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
    match run.feed(rel, columns, params)? {
        Some(out) => Ok(out),
        None => run.finish(params),
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
                let r = select(query, &s.view, &s.table, params)?;
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
            if let Some(out) = run.feed(rel, columns, &self.params)? {
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
                Kind::Folded => self.runs[i].take().unwrap().finish(&self.params)?,
                Kind::After => select(query, &whole, &table, &self.params)?,
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
        Array, BinaryArray, BooleanArray, Decimal128Array, Int16Array, Int32Array, Int64Array,
        StringArray, TimestampMicrosecondArray,
    };
    use arrow::datatypes::DataType as A;
    let mut rows = vec![Vec::with_capacity(batch.num_columns()); batch.num_rows()];
    for a in batch.columns() {
        let any = a.as_any();
        for (r, row) in rows.iter_mut().enumerate() {
            row.push(if a.is_null(r) {
                Value::Null
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
        ArrayRef, BinaryArray, BooleanArray, Decimal128Array, Int64Array, RecordBatch,
        RecordBatchOptions, StringArray,
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

/// Sorts a relation's rows on one of its columns, ascending, so they lie in the table's order.
pub fn in_order(rel: &mut Relation, column: usize) -> Result<()> {
    let mut failed = None;
    rel.rows
        .sort_by(|a, b| match compare(&a[column], &b[column]) {
            Ok(Some(o)) => o,
            Ok(None) => Ordering::Equal,
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
        assert_eq!(hex_bytes("\\xc101").unwrap(), [0xc1, 0x01]);
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
