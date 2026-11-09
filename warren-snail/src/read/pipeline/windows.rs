//! Windows over a relation held whole, on any order: each call's rows cut into its partitions, each
//! partition sorted on the window's own ORDER BY, its peers the rows equal on every item, and its
//! frame carried along it, as PostgreSQL's WindowAgg reads them (`nodeWindowAgg.c`,
//! `windowfuncs.c`); and before a step is read, each named window written into the calls that name
//! it, as PostgreSQL's parser merges them (`transformWindowDefinitions`).

use super::{
    args, column_of, columns_read, compare, computed_none, eval, function_name, grouped_key, ident,
    is_aggregate, is_count_star, key_under, kind_name, naming, Acc, Frame, Key, Relation,
    Value,
};
use crate::catalog::{Casing, ColumnType};
use crate::{Error, Result};
use sqlparser::ast::{
    BinaryOperator, Expr, FunctionArg, FunctionArgExpr, FunctionArgumentClause, FunctionArguments,
    Ident, NamedWindowExpr, NullTreatment, OrderByExpr, OrderByKind, Query, Select,
    SelectItem, SetExpr, TableFactor, WindowFrameBound, WindowFrameUnits, WindowSpec, WindowType,
};
use std::cmp::Ordering;
use std::collections::HashMap;

use super::super::spelled::EXCLUDED;

/// The rows a frame leaves out around the current row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Exclusion {
    NoOthers,
    CurrentRow,
    Group,
    Ties,
}

/// The window a spec names, and the exclusion its frame carries in its name (`spelled::EXCLUDED`).
fn named(spec: &WindowSpec) -> (Option<String>, Exclusion) {
    let Some(i) = &spec.window_name else {
        return (None, Exclusion::NoOthers);
    };
    let Some(rest) = i.value.strip_prefix(EXCLUDED) else {
        return (Some(ident(i)), Exclusion::NoOthers);
    };
    let (kind, name) = rest.split_once(EXCLUDED).unwrap_or((rest, ""));
    let exclusion = match kind {
        "current row" => Exclusion::CurrentRow,
        "group" => Exclusion::Group,
        "ties" => Exclusion::Ties,
        _ => Exclusion::NoOthers,
    };
    ((!name.is_empty()).then(|| name.to_string()), exclusion)
}

/// The name a spec carries for its exclusion alone, where it has one.
fn marked(exclusion: Exclusion) -> Option<Ident> {
    let kind = match exclusion {
        Exclusion::NoOthers => return None,
        Exclusion::CurrentRow => "current row",
        Exclusion::Group => "group",
        Exclusion::Ties => "ties",
    };
    Some(Ident::with_quote(
        '"',
        format!("{EXCLUDED}{kind}{EXCLUDED}"),
    ))
}

/// A question with each window its calls name written into them: `OVER w` the definition itself,
/// `OVER (w ORDER BY …)` the definition's partitions with the call's order and frame; each SELECT's
/// WINDOW clause then dropped.
pub(in crate::read) fn written(q: &Query) -> Result<Query> {
    let mut q = q.clone();
    query(&mut q)?;
    Ok(q)
}

fn query(q: &mut Query) -> Result<()> {
    if let Some(w) = &mut q.with {
        for c in &mut w.cte_tables {
            query(&mut c.query)?;
        }
    }
    let defined = match q.body.as_mut() {
        SetExpr::Select(s) => select(s)?,
        other => {
            set_expr(other)?;
            HashMap::new()
        }
    };
    if let Some(OrderByKind::Expressions(os)) = q.order_by.as_mut().map(|o| &mut o.kind) {
        for o in os {
            expr(&mut o.expr, &defined)?;
        }
    }
    Ok(())
}

fn set_expr(s: &mut SetExpr) -> Result<()> {
    match s {
        SetExpr::Select(s) => {
            select(s)?;
        }
        SetExpr::Query(q) => query(q)?,
        SetExpr::SetOperation { left, right, .. } => {
            set_expr(left)?;
            set_expr(right)?;
        }
        _ => {}
    }
    Ok(())
}

/// A SELECT's windows written into its calls, and the windows it defines.
fn select(s: &mut Select) -> Result<HashMap<String, WindowSpec>> {
    let mut defined: HashMap<String, WindowSpec> = HashMap::new();
    for d in &s.named_window {
        let name = ident(&d.0);
        if defined.contains_key(&name) {
            return Err(Error::new(format!("window \"{name}\" is already defined")));
        }
        let spec = match &d.1 {
            NamedWindowExpr::WindowSpec(w) => merged(w, &defined)?,
            NamedWindowExpr::NamedWindow(r) => {
                return Err(Error::new(format!("syntax error at or near \"{r}\"")))
            }
        };
        defined.insert(name, spec);
    }
    for item in &mut s.projection {
        if let SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } = item {
            expr(e, &defined)?;
        }
    }
    for e in s.selection.iter_mut().chain(s.having.iter_mut()) {
        expr(e, &defined)?;
    }
    for t in &mut s.from {
        for f in std::iter::once(&mut t.relation).chain(t.joins.iter_mut().map(|j| &mut j.relation))
        {
            if let TableFactor::Derived { subquery, .. } = f {
                query(subquery)?;
            }
        }
    }
    s.named_window.clear();
    Ok(defined)
}

/// A spec with the window it names merged in, as PostgreSQL merges them.
fn merged(w: &WindowSpec, defined: &HashMap<String, WindowSpec>) -> Result<WindowSpec> {
    let (name, exclusion) = named(w);
    let Some(name) = name else {
        return Ok(w.clone());
    };
    let Some(r) = defined.get(&name) else {
        return Err(Error::new(format!("window \"{name}\" does not exist")));
    };
    if !w.partition_by.is_empty() {
        return Err(Error::new(format!(
            "cannot override PARTITION BY clause of window \"{name}\""
        )));
    }
    if !w.order_by.is_empty() && !r.order_by.is_empty() {
        return Err(Error::new(format!(
            "cannot override ORDER BY clause of window \"{name}\""
        )));
    }
    if r.window_frame.is_some() {
        return Err(Error::new(format!(
            "cannot copy window \"{name}\" because it has a frame clause"
        )));
    }
    Ok(WindowSpec {
        window_name: marked(exclusion),
        partition_by: r.partition_by.clone(),
        order_by: match w.order_by.is_empty() {
            true => r.order_by.clone(),
            false => w.order_by.clone(),
        },
        window_frame: w.window_frame.clone(),
    })
}

/// Each window call inside `e` with the window it names written in.
fn expr(e: &mut Expr, defined: &HashMap<String, WindowSpec>) -> Result<()> {
    let each = |x: &mut Expr| expr(x, defined);
    match e {
        Expr::Function(f) => {
            match &f.over {
                Some(WindowType::NamedWindow(n)) => {
                    let name = ident(n);
                    let spec = defined
                        .get(&name)
                        .cloned()
                        .ok_or_else(|| Error::new(format!("window \"{name}\" does not exist")))?;
                    f.over = Some(WindowType::WindowSpec(spec));
                }
                Some(WindowType::WindowSpec(w)) => {
                    f.over = Some(WindowType::WindowSpec(merged(w, defined)?));
                }
                None => {}
            }
            if let FunctionArguments::List(l) = &mut f.args {
                for a in &mut l.args {
                    if let FunctionArg::Unnamed(FunctionArgExpr::Expr(x))
                    | FunctionArg::Named {
                        arg: FunctionArgExpr::Expr(x),
                        ..
                    } = a
                    {
                        each(x)?;
                    }
                }
            }
        }
        Expr::BinaryOp { left, right, .. }
        | Expr::IsDistinctFrom(left, right)
        | Expr::IsNotDistinctFrom(left, right) => {
            each(left)?;
            each(right)?;
        }
        Expr::UnaryOp { expr: x, .. }
        | Expr::Nested(x)
        | Expr::Cast { expr: x, .. }
        | Expr::Collate { expr: x, .. }
        | Expr::IsNull(x)
        | Expr::IsNotNull(x)
        | Expr::IsTrue(x)
        | Expr::IsFalse(x)
        | Expr::IsNotTrue(x)
        | Expr::IsNotFalse(x)
        | Expr::IsUnknown(x)
        | Expr::IsNotUnknown(x)
        | Expr::Extract { expr: x, .. } => each(x)?,
        Expr::InList { expr: x, list, .. } => {
            each(x)?;
            for y in list {
                each(y)?;
            }
        }
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            if let Some(x) = operand {
                each(x)?;
            }
            for c in conditions {
                each(&mut c.condition)?;
                each(&mut c.result)?;
            }
            if let Some(x) = else_result {
                each(x)?;
            }
        }
        Expr::Tuple(xs) => {
            for x in xs {
                each(x)?;
            }
        }
        Expr::Subquery(q) | Expr::Exists { subquery: q, .. } => query(q)?,
        Expr::InSubquery {
            expr: x, subquery, ..
        } => {
            each(x)?;
            query(subquery)?;
        }
        _ => {}
    }
    Ok(())
}

/// Each window call refused where PostgreSQL's parser refuses it, before any row: its frame's
/// bounds, a FILTER, DISTINCT or ORDER BY it cannot take, RESPECT or IGNORE NULLS where its release
/// has none or the function takes none, GROUPS with no ORDER BY, and a RANGE offset its ORDER BY
/// cannot be stepped by.
pub(super) fn checked(
    calls: &[&Expr],
    items: &[&Expr],
    typing: &mut super::super::typed::Typing,
    release: u8,
) -> Result<()> {
    for &call in calls {
        let Expr::Function(f) = call else {
            continue;
        };
        let spec = match &f.over {
            Some(WindowType::WindowSpec(s)) => s,
            _ => {
                return Err(Error::new(format!(
                    "{call}: a named window is not read here"
                )))
            }
        };
        if let (Some(name), _) = named(spec) {
            return Err(Error::new(format!("window \"{name}\" does not exist")));
        }
        if let Some(fr) = &spec.window_frame {
            use WindowFrameBound as B;
            match (&fr.start_bound, &fr.end_bound) {
                (B::Following(None), _) => {
                    return Err(Error::new("frame start cannot be UNBOUNDED FOLLOWING"))
                }
                (B::Following(Some(_)), None) => {
                    return Err(Error::new(
                        "frame starting from following row cannot end with current row",
                    ))
                }
                (_, Some(B::Preceding(None))) => {
                    return Err(Error::new("frame end cannot be UNBOUNDED PRECEDING"))
                }
                (B::CurrentRow, Some(B::Preceding(Some(_)))) => {
                    return Err(Error::new(
                        "frame starting from current row cannot have preceding rows",
                    ))
                }
                (B::Following(Some(_)), Some(B::Preceding(Some(_)) | B::CurrentRow)) => {
                    return Err(Error::new(
                        "frame starting from following row cannot have preceding rows",
                    ))
                }
                _ => {}
            }
        }
        let name = function_name(f);
        let aggregate = is_aggregate(&name) || is_count_star(f);
        if f.null_treatment.is_some() {
            if release < 19 {
                let word = match (items.iter().any(|i| bare(i, call)), f.null_treatment) {
                    (true, _) => "NULLS",
                    (false, Some(NullTreatment::RespectNulls)) => "RESPECT",
                    (false, _) => "IGNORE",
                };
                return Err(Error::new(format!("syntax error at or near \"{word}\"")));
            }
            if aggregate {
                return Err(Error::new(
                    "aggregate functions do not accept RESPECT/IGNORE NULLS",
                ));
            }
        }
        if !aggregate && f.filter.is_some() {
            return Err(Error::new(
                "FILTER is not implemented for non-aggregate window functions",
            ));
        }
        if let FunctionArguments::List(l) = &f.args {
            if aggregate && l.duplicate_treatment.is_some() {
                return Err(Error::new(
                    "DISTINCT is not implemented for window functions",
                ));
            }
            if aggregate
                && l.clauses
                    .iter()
                    .any(|c| matches!(c, FunctionArgumentClause::OrderBy(_)))
            {
                return Err(Error::new(
                    "aggregate ORDER BY is not implemented for window functions",
                ));
            }
        }
        let Some(fr) = &spec.window_frame else {
            continue;
        };
        let units = match fr.units {
            WindowFrameUnits::Rows => "ROWS",
            WindowFrameUnits::Range => "RANGE",
            WindowFrameUnits::Groups => "GROUPS",
        };
        let offsets: Vec<&Expr> = [Some(&fr.start_bound), fr.end_bound.as_ref()]
            .into_iter()
            .flatten()
            .filter_map(|b| match b {
                WindowFrameBound::Preceding(Some(x)) | WindowFrameBound::Following(Some(x)) => {
                    Some(&**x)
                }
                _ => None,
            })
            .collect();
        if fr.units == WindowFrameUnits::Range && !offsets.is_empty() && spec.order_by.len() != 1 {
            return Err(Error::new(
                "RANGE with offset PRECEDING/FOLLOWING requires exactly one ORDER BY column",
            ));
        }
        if fr.units == WindowFrameUnits::Groups && spec.order_by.is_empty() {
            return Err(Error::new("GROUPS mode requires an ORDER BY clause"));
        }
        for x in &offsets {
            let mut read = Vec::new();
            columns_read(x, &mut read);
            if !read.is_empty() {
                return Err(Error::new(format!(
                    "argument of {units} must not contain variables"
                )));
            }
        }
        if fr.units != WindowFrameUnits::Range || offsets.is_empty() {
            continue;
        }
        let Some(column) = typing.type_of(&spec.order_by[0].expr)? else {
            continue;
        };
        for x in offsets {
            let offset = typing.type_of(x)?;
            if let Err(e) = stepped_by(column, offset) {
                return Err(e);
            }
        }
    }
    Ok(())
}

/// Whether a RANGE offset of type `offset` steps an ORDER BY column of type `column`, as
/// PostgreSQL's in_range support functions do; refused in its words where none does.
fn stepped_by(column: ColumnType, offset: Option<ColumnType>) -> Result<()> {
    use ColumnType as C;
    let name = |t: ColumnType| match t {
        C::Numeric { .. } | C::NumericText(_) => "numeric".to_string(),
        C::Timestamp(_) => "timestamp without time zone".to_string(),
        C::Timestamptz | C::TimestamptzRounded(_) => "timestamp with time zone".to_string(),
        C::Time(_) => "time without time zone".to_string(),
        C::Timetz(_) => "time with time zone".to_string(),
        C::Interval { .. } => "interval".to_string(),
        C::Text(_) => "text".to_string(),
        t => t.pg_name(),
    };
    let takes: fn(ColumnType) -> bool = match column {
        C::Int2 | C::Int4 | C::Int8 => |t| matches!(t, C::Int2 | C::Int4 | C::Int8),
        C::Numeric { .. } | C::NumericText(_) => |t| {
            matches!(
                t,
                C::Int2 | C::Int4 | C::Int8 | C::Numeric { .. } | C::NumericText(_)
            )
        },
        C::Float4 | C::Float8 => |t| {
            matches!(
                t,
                C::Int2
                    | C::Int4
                    | C::Int8
                    | C::Numeric { .. }
                    | C::NumericText(_)
                    | C::Float4
                    | C::Float8
            )
        },
        C::Date
        | C::Timestamp(_)
        | C::Timestamptz
        | C::TimestamptzRounded(_)
        | C::Time(_)
        | C::Timetz(_)
        | C::Interval { .. } => |t| matches!(t, C::Interval { .. }),
        _ => {
            return Err(Error::new(format!(
                "RANGE with offset PRECEDING/FOLLOWING is not supported for column type {}",
                name(column)
            )))
        }
    };
    match offset {
        Some(t) if !takes(t) => Err(Error::new(format!(
            "RANGE with offset PRECEDING/FOLLOWING is not supported for column type {} and \
             offset type {}",
            name(column),
            name(t)
        ))),
        _ => Ok(()),
    }
}

/// The item of a window's ORDER BY that is a column, ascending, its NULLs last.
fn plain_column(o: &OrderByExpr) -> Option<(Option<String>, String)> {
    if !crate::read::ordering::ascending(&o.options) {
        return None;
    }
    column_of(&o.expr)
}

/// Whether every window a step calls folds along the relation's order as the rows come: each one
/// ordered by the column holding the order alone, ascending, its NULLs last. Its columns are
/// found by `find`.
fn along_by(
    calls: &[&Expr],
    order: Option<usize>,
    find: impl Fn(Option<&str>, &str) -> Option<usize>,
) -> bool {
    let Some(order) = order else {
        return calls.is_empty();
    };
    calls.iter().all(|call| {
        let Expr::Function(f) = call else {
            return false;
        };
        let Some(WindowType::WindowSpec(spec)) = &f.over else {
            return false;
        };
        match spec.order_by.as_slice() {
            [o] => plain_column(o).is_some_and(|(q, n)| find(q.as_deref(), &n) == Some(order)),
            _ => false,
        }
    })
}

/// The PARTITION BY every window a step calls shares, none where they part their rows otherwise.
pub(super) fn shared_partition<'q>(calls: &[&'q Expr]) -> &'q [Expr] {
    let by = |call: &&'q Expr| -> Option<&'q [Expr]> {
        let Expr::Function(f) = call else {
            return None;
        };
        match &f.over {
            Some(WindowType::WindowSpec(spec)) => Some(&spec.partition_by),
            _ => None,
        }
    };
    let Some(first) = calls.first().and_then(by) else {
        return &[];
    };
    let text = |p: &[Expr]| p.iter().map(|e| e.to_string()).collect::<Vec<_>>();
    match calls.iter().all(|c| by(c).is_some_and(|p| text(p) == text(first))) {
        true => first,
        false => &[],
    }
}

/// Whether the windows fold along the order a relation's rows come in.
pub(super) fn along(calls: &[&Expr], rel: &Relation, frame: &Frame) -> bool {
    along_by(calls, rel.order, |q, n| frame.find(q, n).ok())
}

/// Whether the windows fold along the order of a relation whose columns are `columns`, its order
/// held by the column `order`, before any row comes.
pub(super) fn along_columns(
    calls: &[&Expr],
    columns: &[super::super::typed::TypedColumn],
    order: Option<usize>,
) -> bool {
    along_by(calls, order, |q, n| {
        match naming(
            columns.iter().map(|(q, n, _)| (q.as_deref(), n.as_str())),
            q,
            n,
        )
        .as_slice()
        {
            [i] => Some(*i),
            _ => None,
        }
    })
}

/// A frame's bound, its offset read once for the call.
#[derive(Clone, Debug)]
enum Bound {
    Unbounded,
    Current,
    Preceding(Value),
    Following(Value),
}

/// A window's frame: its units, its two bounds and what it leaves out.
struct Shape {
    units: WindowFrameUnits,
    start: Bound,
    end: Bound,
    exclusion: Exclusion,
}

impl Shape {
    /// The frame a spec gives, its offsets read once, as PostgreSQL reads them before the first
    /// partition: by default from the partition's first row to the current row's last peer.
    fn of(spec: &WindowSpec, frame: &Frame) -> Result<Shape> {
        let (_, exclusion) = named(spec);
        let Some(fr) = &spec.window_frame else {
            return Ok(Shape {
                units: WindowFrameUnits::Range,
                start: Bound::Unbounded,
                end: Bound::Current,
                exclusion,
            });
        };
        let bound = |b: &WindowFrameBound, what: &str| -> Result<Bound> {
            Ok(match b {
                WindowFrameBound::CurrentRow => Bound::Current,
                WindowFrameBound::Preceding(None) | WindowFrameBound::Following(None) => {
                    Bound::Unbounded
                }
                WindowFrameBound::Preceding(Some(x)) => {
                    Bound::Preceding(offset(x, fr.units, what, frame)?)
                }
                WindowFrameBound::Following(Some(x)) => {
                    Bound::Following(offset(x, fr.units, what, frame)?)
                }
            })
        };
        let start = bound(&fr.start_bound, "starting")?;
        let end = match &fr.end_bound {
            None => Bound::Current,
            Some(b) => bound(b, "ending")?,
        };
        Ok(Shape {
            units: fr.units,
            start,
            end,
            exclusion,
        })
    }
}

/// A frame's offset: a RANGE's as it is, a count of rows or peer groups as a bigint not below zero.
fn offset(x: &Expr, units: WindowFrameUnits, what: &str, frame: &Frame) -> Result<Value> {
    let v = eval(x, &[], frame, &computed_none())?;
    if v.is_null() {
        return Err(Error::new(format!("frame {what} offset must not be null")));
    }
    if units == WindowFrameUnits::Range {
        return Ok(v);
    }
    let k = match &v {
        Value::Int(k) => *k,
        Value::Num(..) | Value::Numeric(_) => {
            v.to_numeric()
                .unwrap()
                .to_integer(i64::MIN, i64::MAX, "bigint")?
        }
        Value::Double(d) => rounded(*d)?,
        Value::Real(r) => rounded(f64::from(*r))?,
        Value::Text(s) => s
            .trim()
            .parse::<i64>()
            .map_err(|_| Error::new(format!("invalid input syntax for type bigint: \"{s}\"")))?,
        other => {
            return Err(Error::new(format!(
                "argument of {} must be type bigint, not type {}",
                match units {
                    WindowFrameUnits::Rows => "ROWS",
                    _ => "GROUPS",
                },
                kind_name(other)
            )))
        }
    };
    if k < 0 {
        return Err(Error::new(format!(
            "frame {what} offset must not be negative"
        )));
    }
    Ok(Value::Int(k))
}

/// A float as a bigint, rounded half to even as PostgreSQL's dtoi8 rounds it.
fn rounded(d: f64) -> Result<i64> {
    let r = d.round_ties_even();
    if r.is_nan() || r < -9.223_372_036_854_776e18 || r >= 9.223_372_036_854_776e18 {
        return Err(Error::new("bigint out of range"));
    }
    Ok(r as i64)
}

/// Whether `val` lies on the frame's side of `base` moved by `offset`, as PostgreSQL's in_range
/// support functions decide: `sub` moves back, `less` asks `val <= base ± offset`.
fn in_range(val: &Value, base: &Value, offset: &Value, sub: bool, less: bool) -> Result<bool> {
    let refused = || Error::new("invalid preceding or following size in window function");
    match (val, base, offset) {
        (Value::Int(v), Value::Int(b), Value::Int(o)) => {
            if *o < 0 {
                return Err(refused());
            }
            let sum = match sub {
                true => i128::from(*b) - i128::from(*o),
                false => i128::from(*b) + i128::from(*o),
            };
            let v = i128::from(*v);
            Ok(if less { v <= sum } else { v >= sum })
        }
        (Value::Real(_) | Value::Double(_), _, _) | (_, Value::Real(_) | Value::Double(_), _) => {
            let f = |x: &Value| super::as_double(x).ok_or_else(refused);
            let (v, b, o) = (f(val)?, f(base)?, f(offset)?);
            if o.is_nan() || o < 0.0 {
                return Err(refused());
            }
            if v.is_nan() {
                return Ok(if b.is_nan() { true } else { !less });
            }
            if b.is_nan() {
                return Ok(less);
            }
            if o.is_infinite() && b.is_infinite() && (if sub { b > 0.0 } else { b < 0.0 }) {
                return Ok(true);
            }
            let sum = if sub { b - o } else { b + o };
            Ok(if less { v <= sum } else { v >= sum })
        }
        (Value::Interval(_) | Value::Scalar(..), _, Value::Interval(o)) => {
            if o.total() < 0 {
                return Err(refused());
            }
            if *o == crate::times::Interval::INFINITY {
                let end = match base {
                    Value::Interval(b) => {
                        if sub {
                            *b == crate::times::Interval::INFINITY
                        } else {
                            *b == crate::times::Interval::NEG_INFINITY
                        }
                    }
                    Value::Scalar(_, b) => {
                        if sub {
                            *b == i64::MAX || *b == i64::from(i32::MAX)
                        } else {
                            *b == i64::MIN || *b == i64::from(i32::MIN)
                        }
                    }
                    _ => false,
                };
                if end {
                    return Ok(true);
                }
            }
            let op = if sub {
                BinaryOperator::Minus
            } else {
                BinaryOperator::Plus
            };
            let sum = super::super::clock::arith(&op, base, offset)?
                .ok_or_else(|| Error::new("in_range: no operator moves the bound"))?;
            let ord = compare(val, &sum)?.unwrap_or(Ordering::Equal);
            Ok(if less {
                ord != Ordering::Greater
            } else {
                ord != Ordering::Less
            })
        }
        _ => {
            let (Some(v), Some(b), Some(o)) =
                (val.to_numeric(), base.to_numeric(), offset.to_numeric())
            else {
                return Err(Error::new(format!(
                    "RANGE with offset PRECEDING/FOLLOWING is not supported for column type {} \
                     and offset type {}",
                    kind_name(val),
                    kind_name(offset)
                )));
            };
            if o.is_nan() || o.sign() < 0 {
                return Err(refused());
            }
            if v.is_nan() {
                return Ok(if b.is_nan() { true } else { !less });
            }
            if b.is_nan() {
                return Ok(less);
            }
            let pinf = |n: &crate::numeric::Numeric| n.is_infinite() && n.sign() > 0;
            let ninf = |n: &crate::numeric::Numeric| n.is_infinite() && n.sign() < 0;
            if o.is_infinite() {
                return Ok(if if sub { pinf(&b) } else { ninf(&b) } {
                    true
                } else if sub {
                    if less {
                        ninf(&v)
                    } else {
                        true
                    }
                } else if less {
                    true
                } else {
                    pinf(&v)
                });
            }
            if v.is_infinite() {
                return Ok(if pinf(&v) {
                    if pinf(&b) {
                        true
                    } else {
                        !less
                    }
                } else if ninf(&b) {
                    true
                } else {
                    less
                });
            }
            if b.is_infinite() {
                return Ok(if ninf(&b) { !less } else { less });
            }
            let sum = if sub { b.sub(&o)? } else { b.add(&o)? };
            Ok(if less { v <= sum } else { v >= sum })
        }
    }
}

/// One partition of a window, sorted: its rows, the values of its ORDER BY on each, and the peer
/// group each lies in, with each group's first row and the row after its last.
struct Sorted {
    rows: Vec<usize>,
    keys: Vec<Vec<Value>>,
    group: Vec<usize>,
    starts: Vec<usize>,
    ends: Vec<usize>,
}

/// Two rows' ORDER BY values compared as the window's ORDER BY sorts them.
fn ordered(a: &[Value], b: &[Value], items: &[OrderByExpr]) -> Result<Ordering> {
    for (i, o) in items.iter().enumerate() {
        let desc = crate::read::ordering::descends(&o.options);
        let nulls_first = o.options.nulls_first.unwrap_or(desc);
        let ord = match (a[i].is_null(), b[i].is_null()) {
            (true, true) => Ordering::Equal,
            (true, false) if nulls_first => Ordering::Less,
            (true, false) => Ordering::Greater,
            (false, true) if nulls_first => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => {
                let o = compare(&a[i], &b[i])?.unwrap_or(Ordering::Equal);
                if desc {
                    o.reverse()
                } else {
                    o
                }
            }
        };
        if ord != Ordering::Equal {
            return Ok(ord);
        }
    }
    Ok(Ordering::Equal)
}

/// A value a window sorts by: refused where its type has no ordering, a string under a library
/// collation by the collation's key.
fn sort_key(v: Value, under: Option<Casing>) -> Result<Value> {
    match v {
        Value::Scalar(ty, _) | Value::Composed(ty, _) | Value::Typed(ty, _) if !ty.ordered() => {
            Err(Error::new(format!(
                "could not identify an ordering operator for type {}",
                ty.pg_name()
            )))
        }
        Value::Geometric(g) => Err(Error::new(format!(
            "could not identify an ordering operator for type {}",
            g.kind().name()
        ))),
        v => Ok(match (under, v.string_key()) {
            (Some(Casing::Library(id)), Some(s)) => Value::Bytes(crate::collations::get(id).key(s)),
            _ => v,
        }),
    }
}

/// Each window call's value on each row of a relation read whole, on any order: each partition
/// sorted on the window's ORDER BY, ties in the order the rows came, and its frame carried along.
pub(super) fn values(
    calls: &[&Expr],
    rel: &Relation,
    frame: &Frame,
) -> Result<Vec<HashMap<*const Expr, Value>>> {
    let mut out: Vec<HashMap<*const Expr, Value>> = vec![HashMap::new(); rel.rows.len()];
    let none = computed_none();
    let at = |e: &Expr, r: usize| eval(e, &rel.rows[r], frame, &none);
    for &call in calls {
        let Expr::Function(f) = call else {
            unreachable!()
        };
        let Some(WindowType::WindowSpec(spec)) = &f.over else {
            return Err(Error::new(format!(
                "{call}: a named window is not read here"
            )));
        };
        let name = function_name(f);
        let a = if is_count_star(f) {
            Vec::new()
        } else {
            args(f)?
        };
        let aggregate = is_aggregate(&name) || is_count_star(f);
        let ignore = matches!(f.null_treatment, Some(NullTreatment::IgnoreNulls));
        let shape = Shape::of(spec, frame)?;
        // The partitions, each its rows in the order they came.
        let parted_under = spec
            .partition_by
            .iter()
            .map(|p| frame.casings.keyed(p))
            .collect::<Result<Vec<_>>>()?;
        let mut parts: Vec<Vec<usize>> = Vec::new();
        let mut index: HashMap<Vec<Key>, usize> = HashMap::new();
        for r in 0..rel.rows.len() {
            let mut k = Vec::with_capacity(spec.partition_by.len());
            for (p, &u) in spec.partition_by.iter().zip(&parted_under) {
                let v = at(p, r)?;
                grouped_key(&v)?;
                k.push(key_under(&v, u));
            }
            let i = *index.entry(k).or_insert_with(|| {
                parts.push(Vec::new());
                parts.len() - 1
            });
            parts[i].push(r);
        }
        let ordered_under = spec
            .order_by
            .iter()
            .map(|o| match crate::read::ordering::by_bytes(&o.options) {
                true => Ok(None),
                false => frame.casings.keyed(&o.expr),
            })
            .collect::<Result<Vec<_>>>()?;
        let folding = if aggregate {
            let (acc, folds) = Acc::of(call, frame.types)?;
            let (under, refused) = frame.casings.folded(call, &folds.args)?;
            Some((acc, folds, under, refused))
        } else {
            None
        };
        for ix in parts {
            if f.null_treatment.is_some()
                && !aggregate
                && !matches!(
                    name.as_str(),
                    "lag" | "lead" | "first_value" | "last_value" | "nth_value"
                )
            {
                return Err(Error::new(format!(
                    "function {name} does not allow RESPECT/IGNORE NULLS"
                )));
            }
            let s = sorted(spec, &ix, &ordered_under, &at)?;
            let n = s.rows.len();
            let bounds = frames(&shape, &s, spec)?;
            let in_frame = |pos: usize, i: usize| -> bool {
                let (head, tail) = bounds[i];
                if pos < head || pos >= tail {
                    return false;
                }
                let peer = spec.order_by.is_empty() || s.group[pos] == s.group[i];
                match shape.exclusion {
                    Exclusion::NoOthers => true,
                    Exclusion::CurrentRow => pos != i,
                    Exclusion::Group => !peer,
                    Exclusion::Ties => pos == i || !peer,
                }
            };
            if let Some((empty, folds, under, refused)) = &folding {
                // Folded along the frame: a frame whose head holds and whose tail moves on takes the
                // rows it gains; any other is folded again from its head, as PostgreSQL restarts it.
                let mut held: Option<(usize, usize, Acc)> = None;
                for i in 0..n {
                    let (head, tail) = bounds[i];
                    let fold = |acc: &mut Acc, pos: usize| -> Result<()> {
                        let row = s.rows[pos];
                        if let Some(w) = folds.filter {
                            if super::truth_of(w, at(w, row)?)? != Some(true) {
                                return Ok(());
                            }
                        }
                        let values: Vec<Value> = folds
                            .args
                            .iter()
                            .map(|x| at(x, row))
                            .collect::<Result<_>>()?;
                        if let Some(e) = refused.as_ref().filter(|_| acc.compares(&values)) {
                            return Err(Error::new(e.clone()));
                        }
                        acc.add_row(values, under)
                    };
                    let value = if shape.exclusion == Exclusion::NoOthers {
                        match &mut held {
                            Some((h, t, acc)) if *h == head && *t <= tail => {
                                for pos in *t..tail {
                                    fold(acc, pos)?;
                                }
                                *t = tail;
                                acc.value()?
                            }
                            _ => {
                                let mut acc = empty.clone();
                                for pos in head..tail.max(head) {
                                    fold(&mut acc, pos)?;
                                }
                                let v = acc.value()?;
                                held = Some((head, tail.max(head), acc));
                                v
                            }
                        }
                    } else {
                        let mut acc = empty.clone();
                        for pos in head..tail.max(head) {
                            if in_frame(pos, i) {
                                fold(&mut acc, pos)?;
                            }
                        }
                        acc.value()?
                    };
                    out[s.rows[i]].insert(call as *const Expr, value);
                }
                continue;
            }
            let mut ntile: Option<(i64, i64, i64, i64)> = None;
            for i in 0..n {
                let r = s.rows[i];
                let g = s.group[i];
                let value = match (name.as_str(), a.as_slice()) {
                    ("row_number", []) => Value::Int(i as i64 + 1),
                    ("rank", []) => Value::Int(s.starts[g] as i64 + 1),
                    ("dense_rank", []) => Value::Int(g as i64 + 1),
                    ("percent_rank", []) => Value::Double(if n > 1 {
                        s.starts[g] as f64 / (n - 1) as f64
                    } else {
                        0.0
                    }),
                    ("cume_dist", []) => Value::Double(s.ends[g] as f64 / n as f64),
                    ("ntile", [k]) => {
                        if ntile.is_none() {
                            match at(k, r)? {
                                Value::Null => {
                                    out[r].insert(call as *const Expr, Value::Null);
                                    continue;
                                }
                                Value::Int(b) if b <= 0 => {
                                    return Err(Error::new(
                                        "argument of ntile must be greater than zero",
                                    ))
                                }
                                Value::Int(b) => {
                                    let total = n as i64;
                                    let mut boundary = total / b;
                                    let mut remainder = 0;
                                    if boundary <= 0 {
                                        boundary = 1;
                                    } else {
                                        remainder = total % b;
                                        if remainder != 0 {
                                            boundary += 1;
                                        }
                                    }
                                    ntile = Some((1, 0, boundary, remainder));
                                }
                                other => {
                                    return Err(Error::new(format!(
                                        "function ntile({}) does not exist",
                                        kind_name(&other)
                                    )))
                                }
                            }
                        }
                        let (tile, per, boundary, remainder) = ntile.as_mut().unwrap();
                        *per += 1;
                        if *boundary < *per {
                            if *remainder != 0 && *tile == *remainder {
                                *remainder = 0;
                                *boundary -= 1;
                            }
                            *tile += 1;
                            *per = 1;
                        }
                        Value::Int(*tile)
                    }
                    ("lag" | "lead", [x, rest @ ..]) if rest.len() <= 2 => {
                        let k = match rest.first() {
                            None => 1,
                            Some(k) => match at(k, r)? {
                                Value::Null => {
                                    out[r].insert(call as *const Expr, Value::Null);
                                    continue;
                                }
                                Value::Int(k) => k,
                                other => {
                                    return Err(Error::new(format!(
                                        "function {name}(…, {}) does not exist",
                                        kind_name(&other)
                                    )))
                                }
                            },
                        };
                        let relpos = if name == "lag" { -k } else { k };
                        let found = if !ignore || relpos == 0 {
                            let target = i as i64 + relpos;
                            match (0..n as i64).contains(&target) {
                                true => Some(at(x, s.rows[target as usize])?),
                                false => None,
                            }
                        } else {
                            // The relpos-th row past the current whose value is not NULL.
                            let step = relpos.signum();
                            let mut pos = i as i64;
                            let mut seen = 0;
                            loop {
                                pos += step;
                                if !(0..n as i64).contains(&pos) {
                                    break None;
                                }
                                let v = at(x, s.rows[pos as usize])?;
                                if !v.is_null() {
                                    seen += 1;
                                    if seen == relpos.abs() {
                                        break Some(v);
                                    }
                                }
                            }
                        };
                        match (found, rest.get(1)) {
                            (Some(v), _) => v,
                            (None, Some(d)) => at(d, r)?,
                            (None, None) => Value::Null,
                        }
                    }
                    ("first_value", [x]) => {
                        seek(&s, &bounds, &shape, i, 0, true, ignore, x, &at, &in_frame)?
                    }
                    ("last_value", [x]) => {
                        seek(&s, &bounds, &shape, i, 0, false, ignore, x, &at, &in_frame)?
                    }
                    ("nth_value", [x, k]) => match at(k, r)? {
                        Value::Null => Value::Null,
                        Value::Int(k) if k <= 0 => {
                            return Err(Error::new(
                                "argument of nth_value must be greater than zero",
                            ))
                        }
                        Value::Int(k) => seek(
                            &s,
                            &bounds,
                            &shape,
                            i,
                            k - 1,
                            true,
                            ignore,
                            x,
                            &at,
                            &in_frame,
                        )?,
                        other => {
                            return Err(Error::new(format!(
                                "function nth_value(…, {}) does not exist",
                                kind_name(&other)
                            )))
                        }
                    },
                    _ => return Err(Error::new(format!("the window {call} is not read"))),
                };
                out[r].insert(call as *const Expr, value);
            }
        }
    }
    Ok(out)
}

/// A partition's rows sorted on the window's ORDER BY, ties in the order they came, and its peer
/// groups.
fn sorted(
    spec: &WindowSpec,
    ix: &[usize],
    under: &[Option<Casing>],
    at: &dyn Fn(&Expr, usize) -> Result<Value>,
) -> Result<Sorted> {
    let mut keyed: Vec<(Vec<Value>, Vec<Value>, usize)> = Vec::with_capacity(ix.len());
    for &r in ix {
        let mut raw = Vec::with_capacity(spec.order_by.len());
        let mut sort = Vec::with_capacity(spec.order_by.len());
        for (o, &u) in spec.order_by.iter().zip(under) {
            let v = at(&o.expr, r)?;
            sort.push(sort_key(v.clone(), u)?);
            raw.push(v);
        }
        keyed.push((sort, raw, r));
    }
    let mut failed = None;
    keyed.sort_by(|a, b| match ordered(&a.0, &b.0, &spec.order_by) {
        Ok(o) => o,
        Err(e) => {
            failed.get_or_insert(e);
            Ordering::Equal
        }
    });
    if let Some(e) = failed {
        return Err(e);
    }
    let n = keyed.len();
    let mut group = Vec::with_capacity(n);
    let mut starts = Vec::new();
    let mut ends = Vec::new();
    for i in 0..n {
        let peer =
            i > 0 && ordered(&keyed[i - 1].0, &keyed[i].0, &spec.order_by)? == Ordering::Equal;
        if !peer {
            if i > 0 {
                ends.push(i);
            }
            starts.push(i);
        }
        group.push(starts.len() - 1);
    }
    if n > 0 {
        ends.push(n);
    }
    let (keys, rows) = keyed.into_iter().map(|(_, raw, r)| (raw, r)).unzip();
    Ok(Sorted {
        rows,
        keys,
        group,
        starts,
        ends,
    })
}

/// Each row's frame in a sorted partition: its head and the row after its tail, before any row is
/// left out, as PostgreSQL's update_frameheadpos and update_frametailpos find them. A RANGE bound
/// an offset away moves only forward along the partition.
fn frames(shape: &Shape, s: &Sorted, spec: &WindowSpec) -> Result<Vec<(usize, usize)>> {
    let n = s.rows.len();
    let groups = s.starts.len();
    let desc = spec
        .order_by
        .first()
        .is_some_and(|o| crate::read::ordering::descends(&o.options));
    let nulls_first = spec
        .order_by
        .first()
        .map(|o| o.options.nulls_first.unwrap_or(desc))
        .unwrap_or(false);
    let rows = |i: usize, k: i64, forward: bool| -> usize {
        let at = if forward {
            (i as i128) + i128::from(k)
        } else {
            (i as i128) - i128::from(k)
        };
        at.clamp(0, n as i128) as usize
    };
    let first_of_group = |g: i128| -> usize {
        if g <= 0 {
            0
        } else if g >= groups as i128 {
            n
        } else {
            s.starts[g as usize]
        }
    };
    let end_of_group = |g: i128| -> usize {
        if g < 0 {
            0
        } else if g >= groups as i128 - 1 {
            n
        } else {
            s.ends[g as usize]
        }
    };
    let k_of = |v: &Value| match v {
        Value::Int(k) => *k,
        _ => 0,
    };
    let mut out = Vec::with_capacity(n);
    let mut head_at = 0;
    let mut tail_at = 0;
    for i in 0..n {
        let g = s.group[i] as i128;
        let head = match (&shape.start, shape.units) {
            (Bound::Unbounded, _) => 0,
            (Bound::Current, WindowFrameUnits::Rows) => i,
            (Bound::Current, _) => s.starts[s.group[i]],
            (Bound::Preceding(k), WindowFrameUnits::Rows) => rows(i, k_of(k), false),
            (Bound::Following(k), WindowFrameUnits::Rows) => rows(i, k_of(k), true),
            (Bound::Preceding(k), WindowFrameUnits::Groups) => {
                first_of_group(g - i128::from(k_of(k)))
            }
            (Bound::Following(k), WindowFrameUnits::Groups) => {
                first_of_group(g + i128::from(k_of(k)))
            }
            (Bound::Preceding(o) | Bound::Following(o), WindowFrameUnits::Range) => {
                let (mut sub, mut less) = (matches!(shape.start, Bound::Preceding(_)), false);
                if desc {
                    sub = !sub;
                    less = true;
                }
                let curr = &s.keys[i][0];
                while head_at < n {
                    let h = &s.keys[head_at][0];
                    let stop = match (h.is_null(), curr.is_null()) {
                        (false, false) => in_range(h, curr, o, sub, less)?,
                        (hn, cn) if nulls_first => !hn || cn,
                        (hn, cn) => hn || !cn,
                    };
                    if stop {
                        break;
                    }
                    head_at += 1;
                }
                head_at
            }
        };
        let tail = match (&shape.end, shape.units) {
            (Bound::Unbounded, _) => n,
            (Bound::Current, WindowFrameUnits::Rows) => i + 1,
            (Bound::Current, _) => s.ends[s.group[i]],
            (Bound::Preceding(k), WindowFrameUnits::Rows) => rows(i + 1, k_of(k), false),
            (Bound::Following(k), WindowFrameUnits::Rows) => rows(i + 1, k_of(k), true),
            (Bound::Preceding(k), WindowFrameUnits::Groups) => {
                end_of_group(g - i128::from(k_of(k)))
            }
            (Bound::Following(k), WindowFrameUnits::Groups) => {
                end_of_group(g + i128::from(k_of(k)))
            }
            (Bound::Preceding(o) | Bound::Following(o), WindowFrameUnits::Range) => {
                let (mut sub, mut less) = (matches!(shape.end, Bound::Preceding(_)), true);
                if desc {
                    sub = !sub;
                    less = false;
                }
                let curr = &s.keys[i][0];
                while tail_at < n {
                    let t = &s.keys[tail_at][0];
                    let stop = match (t.is_null(), curr.is_null()) {
                        (false, false) => !in_range(t, curr, o, sub, less)?,
                        (tn, _) if nulls_first => !tn,
                        (_, cn) => !cn,
                    };
                    if stop {
                        break;
                    }
                    tail_at += 1;
                }
                tail_at
            }
        };
        out.push((head, tail));
    }
    Ok(out)
}

/// The value of `x` on the row `relpos` rows into the frame from its head (`from_head`) or back
/// from its tail, as PostgreSQL's WinGetFuncArgInFrame finds it: past the rows the frame leaves
/// out, by its arithmetic; or under IGNORE NULLS the `relpos + 1`-th row whose value is not NULL.
/// NULL where no such row lies in the frame.
#[allow(clippy::too_many_arguments)]
fn seek(
    s: &Sorted,
    bounds: &[(usize, usize)],
    shape: &Shape,
    i: usize,
    relpos: i64,
    from_head: bool,
    ignore: bool,
    x: &Expr,
    at: &dyn Fn(&Expr, usize) -> Result<Value>,
    in_frame: &dyn Fn(usize, usize) -> bool,
) -> Result<Value> {
    let n = s.rows.len() as i64;
    let (head, tail) = (bounds[i].0 as i64, bounds[i].1 as i64);
    let cur = i as i64;
    if ignore {
        let (mut pos, step) = if from_head { (head, 1) } else { (tail - 1, -1) };
        let mut seen = 0;
        loop {
            if pos < 0 || pos >= n || (from_head && pos >= tail) {
                return Ok(Value::Null);
            }
            if pos < head && !from_head {
                return Ok(Value::Null);
            }
            if in_frame(pos as usize, i) {
                let v = at(x, s.rows[pos as usize])?;
                if !v.is_null() {
                    if seen == relpos {
                        return Ok(v);
                    }
                    seen += 1;
                }
            }
            pos += step;
        }
    }
    let (gh, gt) = (s.starts[s.group[i]] as i64, s.ends[s.group[i]] as i64);
    let pos = if from_head {
        let mut pos = head + relpos;
        match shape.exclusion {
            Exclusion::NoOthers => {}
            Exclusion::CurrentRow => {
                if pos >= cur && cur >= head {
                    pos += 1;
                }
            }
            Exclusion::Group => {
                if pos >= gh && gt > head {
                    pos += gt - gh.max(head);
                }
            }
            Exclusion::Ties => {
                if pos >= gh && gt > head {
                    let overlap = gh.max(head);
                    if pos == overlap {
                        pos = cur;
                    } else {
                        pos += gt - overlap - 1;
                    }
                }
            }
        }
        pos
    } else {
        let mut pos = tail - 1 + relpos;
        match shape.exclusion {
            Exclusion::NoOthers => {}
            Exclusion::CurrentRow => {
                if pos <= cur && cur < tail {
                    pos -= 1;
                }
            }
            Exclusion::Group => {
                if pos < gt && gh < tail {
                    pos -= gt.min(tail) - gh;
                }
            }
            Exclusion::Ties => {
                if pos < gt && gh < tail {
                    let overlap = gt.min(tail);
                    if pos == overlap - 1 {
                        pos = cur;
                    } else {
                        pos -= overlap - 1 - gh;
                    }
                }
            }
        }
        if shape.exclusion != Exclusion::NoOthers && pos < head {
            return Ok(Value::Null);
        }
        pos
    };
    if pos < 0 || pos >= n || !in_frame(pos as usize, i) {
        return Ok(Value::Null);
    }
    at(x, s.rows[pos as usize])
}

/// Whether a call stands in a select list's item outside any parentheses, a CASE or a call's
/// arguments, where PostgreSQL 18's grammar reads a word after it as the item's alias.
fn bare(e: &Expr, call: &Expr) -> bool {
    if std::ptr::eq(e, call) {
        return true;
    }
    match e {
        Expr::BinaryOp { left, right, .. }
        | Expr::IsDistinctFrom(left, right)
        | Expr::IsNotDistinctFrom(left, right) => bare(left, call) || bare(right, call),
        Expr::Cast {
            kind: sqlparser::ast::CastKind::DoubleColon,
            expr,
            ..
        }
        | Expr::UnaryOp { expr, .. }
        | Expr::Collate { expr, .. }
        | Expr::InList { expr, .. }
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr)
        | Expr::IsTrue(expr)
        | Expr::IsFalse(expr)
        | Expr::IsNotTrue(expr)
        | Expr::IsNotFalse(expr)
        | Expr::IsUnknown(expr)
        | Expr::IsNotUnknown(expr) => bare(expr, call),
        Expr::Between {
            expr, low, high, ..
        } => bare(expr, call) || bare(low, call) || bare(high, call),
        _ => false,
    }
}
