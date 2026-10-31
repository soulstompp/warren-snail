//! The types a step's expressions take, read before any row: a column's from the relation it comes
//! from, a literal's as PostgreSQL types it, an operator's and a function's as PostgreSQL resolves
//! them. An aggregate or a function PostgreSQL has no form of for its arguments is refused here,
//! over no rows too, as is a function of the server's own; an expression whose type the snail does
//! not resolve is read as before, a row at a time.

use crate::catalog::ColumnType;
use crate::{Error, Result};
use sqlparser::ast::{
    BinaryOperator, CastKind, CeilFloorKind, DateTimeField, Expr, FunctionArg, FunctionArgExpr,
    FunctionArguments, UnaryOperator, Value as SqlValue,
};
use std::collections::HashMap;

/// Each expression's type, by place, where it is resolved.
pub(super) type Types = HashMap<*const Expr, ColumnType>;

/// A column a step reads: its qualifier, its name, and its type where it is known.
pub(super) type TypedColumn = (Option<String>, String, Option<ColumnType>);

/// An unconstrained numeric, the type of what numeric's operators and functions return.
pub(super) const NUMERIC: ColumnType = ColumnType::NumericText(None);

/// What the typing of a step reads: its columns, and the release whose functions it resolves.
pub(super) struct Typing<'a> {
    pub columns: &'a [TypedColumn],
    pub release: u8,
    pub types: Types,
}

/// How an argument stands for overload resolution: an integer of its width, a numeric, a float of
/// its width, a quoted literal or NULL of no type yet, or another type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Arg {
    Int(ColumnType),
    Numeric,
    Float(ColumnType),
    Unknown,
    Other(ColumnType),
}

fn arg_of(t: Option<ColumnType>) -> Arg {
    match t {
        None => Arg::Unknown,
        Some(t) if t.is_integer() => Arg::Int(t),
        Some(t) if t.is_numeric() => Arg::Numeric,
        Some(t) if t.is_float() => Arg::Float(t),
        Some(t) => Arg::Other(t),
    }
}

/// The name an argument's type is written under in PostgreSQL's errors.
fn arg_name(t: Option<ColumnType>) -> String {
    match t {
        None => "unknown".into(),
        Some(ColumnType::NumericText(_) | ColumnType::Numeric { .. }) => "numeric".into(),
        Some(t) => t
            .pg_name()
            .split('(')
            .next()
            .unwrap_or_default()
            .trim()
            .to_string(),
    }
}

/// PostgreSQL's refusal of a call no form of the function takes.
pub(super) fn no_form(name: &str, args: &[Option<ColumnType>]) -> Error {
    Error::new(format!(
        "function {name}({}) does not exist",
        args.iter()
            .map(|t| arg_name(*t))
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

/// The wider of two integers, as PostgreSQL's operators across integer widths return.
fn wider(a: ColumnType, b: ColumnType) -> ColumnType {
    let rank = |t: ColumnType| match t {
        ColumnType::Int2 => 0,
        ColumnType::Int4 => 1,
        _ => 2,
    };
    if rank(a) >= rank(b) {
        a
    } else {
        b
    }
}

/// A number literal's type: an integer of the narrowest of integer and bigint that holds it, else
/// a numeric.
fn number_type(s: &str) -> ColumnType {
    let t = s.trim();
    let digits = t.strip_prefix(['-', '+']).unwrap_or(t);
    if !digits.is_empty() && digits.bytes().all(|c| c.is_ascii_digit()) {
        if t.parse::<i32>().is_ok() {
            return ColumnType::Int4;
        }
        if t.parse::<i64>().is_ok() {
            return ColumnType::Int8;
        }
    }
    NUMERIC
}

/// The type an arithmetic operator returns over two numbers, where both are resolved.
fn arithmetic(a: Option<ColumnType>, b: Option<ColumnType>) -> Option<ColumnType> {
    match (arg_of(a), arg_of(b)) {
        (Arg::Int(x), Arg::Int(y)) => Some(wider(x, y)),
        (Arg::Int(x), Arg::Unknown) | (Arg::Unknown, Arg::Int(x)) => Some(x),
        (Arg::Numeric, Arg::Int(_) | Arg::Numeric | Arg::Unknown)
        | (Arg::Int(_) | Arg::Unknown, Arg::Numeric) => Some(NUMERIC),
        (Arg::Float(ColumnType::Float4), Arg::Float(ColumnType::Float4))
        | (Arg::Float(ColumnType::Float4), Arg::Unknown)
        | (Arg::Unknown, Arg::Float(ColumnType::Float4)) => Some(ColumnType::Float4),
        (Arg::Float(_), Arg::Int(_) | Arg::Numeric | Arg::Float(_) | Arg::Unknown)
        | (Arg::Int(_) | Arg::Numeric | Arg::Unknown, Arg::Float(_)) => Some(ColumnType::Float8),
        _ => None,
    }
}

/// The arguments of a call as expressions; none where they are not plain.
fn call_args(f: &sqlparser::ast::Function) -> Option<Vec<&Expr>> {
    match &f.args {
        FunctionArguments::None => Some(Vec::new()),
        FunctionArguments::List(l) => l
            .args
            .iter()
            .map(|a| match a {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(x)) => Some(x),
                _ => None,
            })
            .collect(),
        _ => None,
    }
}

impl Typing<'_> {
    fn column(&self, e: &Expr) -> Option<ColumnType> {
        let (q, n) = super::pipeline::column_of(e)?;
        let mut found = self
            .columns
            .iter()
            .filter(|(cq, cn, _)| *cn == n && (q.is_none() || cq.as_deref() == q.as_deref()));
        let first = found.next()?;
        if found.next().is_some() {
            return None;
        }
        first.2
    }

    /// An expression's type, where the snail resolves it, each sub-expression's recorded.
    pub fn type_of(&mut self, e: &Expr) -> Result<Option<ColumnType>> {
        let t = self.resolve(e)?;
        if let Some(t) = t {
            self.types.insert(e as *const Expr, t);
        }
        Ok(t)
    }

    fn resolve(&mut self, e: &Expr) -> Result<Option<ColumnType>> {
        Ok(match e {
            Expr::Identifier(_) | Expr::CompoundIdentifier(_) => self.column(e),
            Expr::Nested(x) => self.type_of(x)?,
            Expr::Collate { expr, .. } => self.type_of(expr)?,
            Expr::Value(v) => match &v.value {
                SqlValue::Number(s, _) => Some(number_type(s)),
                SqlValue::Boolean(_) => Some(ColumnType::Boolean),
                _ => None,
            },
            Expr::Cast {
                kind: CastKind::Cast | CastKind::DoubleColon,
                expr,
                data_type,
                ..
            } => {
                self.type_of(expr)?;
                crate::catalog::held_type(data_type).ok()
            }
            Expr::TypedString(t) => crate::catalog::held_type(&t.data_type).ok(),
            Expr::Interval(_) => Some(super::calls::full_interval()),
            // A number written with its minus is one constant, as PostgreSQL's grammar folds it.
            Expr::UnaryOp {
                op: UnaryOperator::Minus,
                expr,
            } if matches!(expr.as_ref(), Expr::Value(v) if matches!(v.value, SqlValue::Number(..))) =>
            {
                let Expr::Value(v) = expr.as_ref() else {
                    unreachable!()
                };
                let SqlValue::Number(s, _) = &v.value else {
                    unreachable!()
                };
                Some(number_type(&format!("-{s}")))
            }
            Expr::UnaryOp {
                op: UnaryOperator::Minus | UnaryOperator::Plus,
                expr,
            } => {
                let t = self.type_of(expr)?;
                t.filter(|t| t.is_integer() || t.is_numeric() || t.is_float())
                    .map(|t| if t.is_numeric() { NUMERIC } else { t })
            }
            Expr::UnaryOp {
                op: UnaryOperator::Not,
                expr,
            } => {
                self.type_of(expr)?;
                Some(ColumnType::Boolean)
            }
            Expr::IsNull(x)
            | Expr::IsNotNull(x)
            | Expr::IsTrue(x)
            | Expr::IsNotTrue(x)
            | Expr::IsFalse(x)
            | Expr::IsNotFalse(x)
            | Expr::IsUnknown(x)
            | Expr::IsNotUnknown(x) => {
                self.type_of(x)?;
                Some(ColumnType::Boolean)
            }
            Expr::InList { expr, list, .. } => {
                self.type_of(expr)?;
                for x in list {
                    self.type_of(x)?;
                }
                Some(ColumnType::Boolean)
            }
            Expr::SimilarTo {
                expr,
                pattern,
                escape_char,
                ..
            } => {
                self.type_of(expr)?;
                self.type_of(pattern)?;
                if let Some(c) = escape_char {
                    self.type_of(c)?;
                }
                Some(ColumnType::Boolean)
            }
            Expr::IsDistinctFrom(a, b) | Expr::IsNotDistinctFrom(a, b) => {
                self.type_of(a)?;
                self.type_of(b)?;
                Some(ColumnType::Boolean)
            }
            // Untyped NULL beside ends of more than one type leaves PostgreSQL more than one form.
            Expr::BinaryOp {
                left,
                op: BinaryOperator::Overlaps,
                right,
            } => {
                let mut ends = Vec::new();
                for side in [left, right] {
                    if let Expr::Tuple(xs) = side.as_ref() {
                        for x in xs {
                            ends.push(self.type_of(x)?);
                        }
                    }
                }
                let known: Vec<ColumnType> = ends
                    .iter()
                    .flatten()
                    .map(|t| super::clock::base(*t))
                    .collect();
                if ends.iter().any(Option::is_none) && known.windows(2).any(|w| w[0] != w[1]) {
                    return Err(Error::new(format!(
                        "function pg_catalog.overlaps({}) is not unique",
                        ends.iter()
                            .map(|t| arg_name(*t))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )));
                }
                Some(ColumnType::Boolean)
            }
            Expr::BinaryOp { left, op, right } => {
                let (a, b) = (self.type_of(left)?, self.type_of(right)?);
                match op {
                    BinaryOperator::Plus
                    | BinaryOperator::Minus
                    | BinaryOperator::Multiply
                    | BinaryOperator::Divide
                    | BinaryOperator::Modulo => arithmetic(a, b),
                    BinaryOperator::And
                    | BinaryOperator::Or
                    | BinaryOperator::Eq
                    | BinaryOperator::NotEq
                    | BinaryOperator::Lt
                    | BinaryOperator::LtEq
                    | BinaryOperator::Gt
                    | BinaryOperator::GtEq
                    | BinaryOperator::Overlaps
                    | BinaryOperator::PGRegexMatch
                    | BinaryOperator::PGRegexIMatch
                    | BinaryOperator::PGRegexNotMatch
                    | BinaryOperator::PGRegexNotIMatch => Some(ColumnType::Boolean),
                    _ => None,
                }
            }
            Expr::Case {
                operand,
                conditions,
                else_result,
                ..
            } => {
                if let Some(o) = operand {
                    self.type_of(o)?;
                }
                let mut t = None;
                for c in conditions {
                    self.type_of(&c.condition)?;
                    t = t.or(self.type_of(&c.result)?);
                }
                if let Some(x) = else_result {
                    t = t.or(self.type_of(x)?);
                }
                t
            }
            Expr::Extract { expr, .. } => {
                self.type_of(expr)?;
                Some(NUMERIC)
            }
            Expr::Ceil {
                expr,
                field: CeilFloorKind::DateTimeField(DateTimeField::NoDateTime),
            }
            | Expr::Floor {
                expr,
                field: CeilFloorKind::DateTimeField(DateTimeField::NoDateTime),
            } => {
                let t = self.type_of(expr)?;
                let name = if matches!(e, Expr::Ceil { .. }) {
                    "ceil"
                } else {
                    "floor"
                };
                numbers(name, &[t])?.flatten()
            }
            Expr::Function(f) => {
                let name = super::pipeline::function_name(f);
                let Some(args) = call_args(f) else {
                    return Ok(None);
                };
                let mut types = Vec::with_capacity(args.len());
                for a in &args {
                    types.push(self.type_of(a)?);
                }
                if f.over.is_none() && !f.within_group.is_empty() {
                    let mut within = Vec::with_capacity(f.within_group.len());
                    for o in &f.within_group {
                        within.push(self.type_of(&o.expr)?);
                    }
                    return ordered_set(&name, &types, &within);
                }
                if f.over.is_none() && super::pipeline::is_aggregate(&name) {
                    return aggregate(&name, &types, f.args_star());
                }
                if f.over.is_some() {
                    return window(&name, &types, f.args_star());
                }
                if let Some(t) = numbers(&name, &types)? {
                    return Ok(t);
                }
                if let Some(t) = time(&name, &types)? {
                    return Ok(t);
                }
                if let Some(t) = strings(&name, &types)? {
                    return Ok(t);
                }
                if let Some(t) = regexps(&name, &types)? {
                    return Ok(t);
                }
                crate::functions::refuse_server(&name, self.release)?;
                None
            }
            _ => None,
        })
    }
}

/// Whether a call is `f(*)`.
trait Star {
    fn args_star(&self) -> bool;
}

impl Star for sqlparser::ast::Function {
    fn args_star(&self) -> bool {
        matches!(&self.args, FunctionArguments::List(l)
            if matches!(l.args.as_slice(), [FunctionArg::Unnamed(FunctionArgExpr::Wildcard)]))
    }
}

/// The type an aggregate the types carry returns over its argument's, or PostgreSQL's refusal where
/// it has no form for it.
fn aggregate(name: &str, args: &[Option<ColumnType>], star: bool) -> Result<Option<ColumnType>> {
    if name == "count" {
        return Ok(Some(ColumnType::Int8));
    }
    if star {
        return Ok(None);
    }
    if let Some(t) = further(name, args)? {
        return Ok(t);
    }
    let [t] = args else {
        return Ok(None);
    };
    let Some(ty) = *t else {
        return Ok(None);
    };
    let refused = || Err(no_form(name, args));
    Ok(Some(match name {
        "sum" => match ty {
            ColumnType::Int2 | ColumnType::Int4 => ColumnType::Int8,
            ColumnType::Int8 => NUMERIC,
            t if t.is_numeric() => NUMERIC,
            ColumnType::Float4 | ColumnType::Float8 | ColumnType::Money => ty,
            ColumnType::Interval { .. } | ColumnType::Time(_) => ColumnType::Interval {
                range: crate::times::INTERVAL_FULL_RANGE,
                precision: None,
            },
            _ => return refused(),
        },
        "avg" => match ty {
            t if t.is_integer() || t.is_numeric() => NUMERIC,
            ColumnType::Float4 | ColumnType::Float8 => ColumnType::Float8,
            ColumnType::Interval { .. } | ColumnType::Time(_) => ColumnType::Interval {
                range: crate::times::INTERVAL_FULL_RANGE,
                precision: None,
            },
            _ => return refused(),
        },
        "min" | "max" => match ty {
            ColumnType::Boolean
            | ColumnType::Uuid
            | ColumnType::Xid
            | ColumnType::Cid
            | ColumnType::Bit(_)
            | ColumnType::Varbit(_) => return refused(),
            // An array or a row of a type with no order is refused only where two are compared.
            ColumnType::Composed(r)
                if matches!(
                    r.def(),
                    crate::composed::Composed::Range { .. }
                        | crate::composed::Composed::Multirange { .. }
                ) =>
            {
                return refused()
            }
            t if t.is_numeric() => NUMERIC,
            t => t,
        },
        "bool_and" | "bool_or" | "every" => match ty {
            ColumnType::Boolean => ColumnType::Boolean,
            _ => return refused(),
        },
        _ => return Ok(None),
    }))
}

/// The type a window function returns, an aggregate's as over groups, where the snail resolves
/// it; PostgreSQL's refusal where an aggregate has no form for its arguments.
fn window(name: &str, args: &[Option<ColumnType>], star: bool) -> Result<Option<ColumnType>> {
    Ok(match (name, args) {
        ("row_number" | "rank" | "dense_rank", []) => Some(ColumnType::Int8),
        ("ntile", [_]) => Some(ColumnType::Int4),
        ("percent_rank" | "cume_dist", []) => Some(ColumnType::Float8),
        ("lag" | "lead" | "first_value" | "last_value" | "nth_value", [t, ..]) => *t,
        (n, _) if super::pipeline::is_aggregate(n) => aggregate(n, args, star)?,
        _ => None,
    })
}

/// The type a function of the numbers returns, as PostgreSQL resolves its forms, or its refusal
/// where none takes the arguments; none where the name is no function of the numbers the snail
/// answers.
pub(super) fn numbers(
    name: &str,
    args: &[Option<ColumnType>],
) -> Result<Option<Option<ColumnType>>> {
    use ColumnType as C;
    let a: Vec<Arg> = args.iter().map(|t| arg_of(*t)).collect();
    let refused = || Err(no_form(name, args));
    let numberish =
        |x: &Arg| matches!(x, Arg::Int(_) | Arg::Numeric | Arg::Float(_) | Arg::Unknown);
    // A function of double precision alone: every number goes to it.
    let doubles = |n: usize| -> Result<Option<Option<ColumnType>>> {
        if a.len() == n && a.iter().all(numberish) {
            Ok(Some(Some(C::Float8)))
        } else {
            refused()
        }
    };
    // A function of double precision and of numeric: numeric where an argument is one and none is a
    // float, else double precision.
    let either = |n: usize| -> Result<Option<Option<ColumnType>>> {
        if a.len() != n || !a.iter().all(numberish) {
            return refused();
        }
        let numeric = a.contains(&Arg::Numeric) && !a.iter().any(|x| matches!(x, Arg::Float(_)));
        Ok(Some(Some(if numeric { NUMERIC } else { C::Float8 })))
    };
    // A function of numeric alone: integers and numerics go to it.
    let numerics = |n: usize, ret: ColumnType| -> Result<Option<Option<ColumnType>>> {
        if a.len() == n
            && a.iter()
                .all(|x| matches!(x, Arg::Int(_) | Arg::Numeric | Arg::Unknown))
        {
            Ok(Some(Some(ret)))
        } else {
            refused()
        }
    };
    Ok(match name {
        "cbrt" | "degrees" | "radians" | "sin" | "cos" | "tan" | "cot" | "asin" | "acos"
        | "atan" | "sind" | "cosd" | "tand" | "cotd" | "asind" | "acosd" | "atand" | "sinh"
        | "cosh" | "tanh" | "asinh" | "acosh" | "atanh" | "erf" | "erfc" | "gamma" | "lgamma"
        | "dround" | "dtrunc" | "dexp" | "dlog1" | "dlog10" => return doubles(1),
        "atan2" | "atan2d" => return doubles(2),
        "pi" | "random" if args.is_empty() => Some(Some(C::Float8)),
        "ceil" | "ceiling" | "floor" | "sign" | "sqrt" | "exp" | "ln" | "log10" => {
            return either(1)
        }
        "round" | "trunc" if args.len() == 1 => return either(1),
        "round" | "trunc" => match a.as_slice() {
            [Arg::Int(_) | Arg::Numeric | Arg::Unknown, Arg::Int(C::Int2 | C::Int4) | Arg::Unknown] => {
                Some(Some(NUMERIC))
            }
            _ => return refused(),
        },
        "log" if args.len() == 1 => return either(1),
        "log" => return numerics(2, NUMERIC),
        "pow" | "power" => return either(2),
        "div" | "numeric_div_trunc" | "numeric_log" => return numerics(2, NUMERIC),
        "numeric_sqrt" | "numeric_exp" | "numeric_ln" | "trim_scale" => {
            return numerics(1, NUMERIC)
        }
        "scale" | "min_scale" => return numerics(1, C::Int4),
        "abs" => match a.as_slice() {
            [Arg::Int(t)] => Some(Some(*t)),
            [Arg::Numeric] => Some(Some(NUMERIC)),
            [Arg::Float(t)] => Some(Some(*t)),
            [Arg::Unknown] => Some(Some(C::Float8)),
            _ => return refused(),
        },
        "mod" => match a.as_slice() {
            [Arg::Int(x), Arg::Int(y)] => Some(Some(wider(*x, *y))),
            [Arg::Int(x), Arg::Unknown] | [Arg::Unknown, Arg::Int(x)] => Some(Some(*x)),
            [Arg::Numeric, Arg::Int(_) | Arg::Numeric | Arg::Unknown]
            | [Arg::Int(_) | Arg::Unknown, Arg::Numeric] => Some(Some(NUMERIC)),
            [Arg::Unknown, Arg::Unknown] => {
                return Err(Error::new("function mod(unknown, unknown) is not unique"))
            }
            _ => return refused(),
        },
        "gcd" | "lcm" => match a.as_slice() {
            [Arg::Int(C::Int2), Arg::Int(C::Int2)] => {
                return Err(Error::new(format!(
                    "function {name}(smallint, smallint) is not unique"
                )))
            }
            [Arg::Int(x), Arg::Int(y)] => Some(Some(wider(*x, *y).max_int4())),
            [Arg::Int(x), Arg::Unknown] | [Arg::Unknown, Arg::Int(x)] => Some(Some(x.max_int4())),
            [Arg::Numeric, Arg::Int(_) | Arg::Numeric | Arg::Unknown]
            | [Arg::Int(_) | Arg::Unknown, Arg::Numeric] => Some(Some(NUMERIC)),
            _ => return refused(),
        },
        "factorial" => match a.as_slice() {
            [Arg::Int(_) | Arg::Unknown] => Some(Some(NUMERIC)),
            _ => return refused(),
        },
        "width_bucket" if args.len() == 4 => {
            if !a[..3].iter().all(numberish)
                || !matches!(a[3], Arg::Int(C::Int2 | C::Int4) | Arg::Unknown)
            {
                return refused();
            }
            Some(Some(C::Int4))
        }
        "to_hex" | "to_oct" | "to_bin" => match a.as_slice() {
            [Arg::Int(C::Int2) | Arg::Unknown] => {
                return Err(Error::new(format!(
                    "function {name}({}) is not unique",
                    arg_name(args[0])
                )))
            }
            [Arg::Int(_)] => Some(Some(C::Text(None))),
            _ => return refused(),
        },
        "num_nulls" | "num_nonnulls" => Some(Some(C::Int4)),
        _ => None,
    })
}

trait AtLeastInt4 {
    fn max_int4(self) -> ColumnType;
}

impl AtLeastInt4 for ColumnType {
    /// An integer of at least integer's width: gcd and lcm have no smallint form.
    fn max_int4(self) -> ColumnType {
        if self == ColumnType::Int2 {
            ColumnType::Int4
        } else {
            self
        }
    }
}

/// The type a function of time returns, as PostgreSQL resolves its forms, or its refusal where
/// none takes the arguments or more than one does; none where the name is no function of time the
/// snail answers.
pub(super) fn time(name: &str, args: &[Option<ColumnType>]) -> Result<Option<Option<ColumnType>>> {
    use ColumnType as C;
    let base = |t: Option<ColumnType>| t.map(super::clock::base);
    let interval = super::calls::full_interval();
    let not_unique = || {
        Err(Error::new(format!(
            "function {name}({}) is not unique",
            args.iter()
                .map(|t| arg_name(*t))
                .collect::<Vec<_>>()
                .join(", ")
        )))
    };
    let refused = || Err(no_form(name, args));
    let instant = |t: Option<ColumnType>| {
        matches!(
            base(t),
            Some(C::Date | C::Timestamp(_) | C::Timestamptz) | None
        )
    };
    Ok(Some(Some(match (name, args) {
        (
            "now"
            | "transaction_timestamp"
            | "statement_timestamp"
            | "clock_timestamp"
            | "current_timestamp",
            [],
        ) => C::Timestamptz,
        ("timeofday", []) => C::Text(None),
        ("current_date", []) => C::Date,
        ("localtimestamp", []) => C::Timestamp(None),
        ("current_time", []) => C::Timetz(None),
        ("localtime", []) => C::Time(None),
        ("date_part", [_, t]) => match base(*t) {
            None => return not_unique(),
            Some(
                C::Date
                | C::Timestamp(_)
                | C::Timestamptz
                | C::Time(_)
                | C::Timetz(_)
                | C::Interval { .. },
            ) => C::Float8,
            _ => return refused(),
        },
        ("date_trunc", [_, t]) => match base(*t) {
            None => return not_unique(),
            Some(C::Timestamp(_)) => C::Timestamp(None),
            Some(C::Timestamptz | C::Date) => C::Timestamptz,
            Some(C::Interval { .. }) => interval,
            _ => return refused(),
        },
        ("date_trunc", [_, t, _]) if instant(*t) => C::Timestamptz,
        ("age", [t]) => match base(*t) {
            None => return not_unique(),
            Some(C::Timestamp(_) | C::Timestamptz | C::Date) => interval,
            _ => return Ok(None),
        },
        ("age", [a, b]) if instant(*a) && instant(*b) => interval,
        ("make_date", [_, _, _]) => C::Date,
        ("make_time", [_, _, _]) => C::Time(None),
        ("make_timestamp", [_, _, _, _, _, _]) => C::Timestamp(None),
        ("make_timestamptz", [_, _, _, _, _, _] | [_, _, _, _, _, _, _]) => C::Timestamptz,
        ("make_interval", a) if a.len() <= 7 => interval,
        ("date_bin", [_, s, o]) if instant(*s) && instant(*o) => {
            if matches!(
                (base(*s), base(*o)),
                (Some(C::Timestamp(_)), Some(C::Timestamp(_)) | None)
                    | (None, Some(C::Timestamp(_)))
            ) {
                C::Timestamp(None)
            } else {
                C::Timestamptz
            }
        }
        ("to_timestamp", [_] | [_, _]) => C::Timestamptz,
        ("to_char", [_, _]) => C::Text(None),
        ("to_number", [_, _]) => NUMERIC,
        ("to_date", [_, _]) => C::Date,
        ("isfinite", [t]) => match base(*t) {
            None => return not_unique(),
            Some(C::Date | C::Timestamp(_) | C::Timestamptz | C::Interval { .. }) => C::Boolean,
            _ => return refused(),
        },
        ("justify_days" | "justify_hours" | "justify_interval", [_]) => interval,
        ("date_add" | "date_subtract", [_, _]) => C::Timestamptz,
        ("timestamp", [_, _]) => C::Timestamp(None),
        ("timestamptz", [_, _]) => C::Timestamptz,
        _ => return Ok(None),
    })))
}

/// The type an aggregate past those the types carry returns, or its refusal where it has no form
/// for its arguments; none where the name is none of them.
fn further(name: &str, args: &[Option<ColumnType>]) -> Result<Option<Option<ColumnType>>> {
    use ColumnType as C;
    let refused = || Err(no_form(name, args));
    let numberish = |t: &Option<ColumnType>| {
        matches!(
            arg_of(*t),
            Arg::Int(_) | Arg::Numeric | Arg::Float(_) | Arg::Unknown
        )
    };
    Ok(Some(match (name, args) {
        ("var_pop" | "var_samp" | "variance" | "stddev" | "stddev_pop" | "stddev_samp", [t]) => {
            match arg_of(*t) {
                Arg::Int(_) | Arg::Numeric => Some(NUMERIC),
                Arg::Float(_) => Some(C::Float8),
                Arg::Unknown => None,
                Arg::Other(_) => return refused(),
            }
        }
        ("regr_count", [_, _]) if args.iter().all(numberish) => Some(C::Int8),
        (
            "covar_pop" | "covar_samp" | "corr" | "regr_sxx" | "regr_syy" | "regr_sxy"
            | "regr_avgx" | "regr_avgy" | "regr_r2" | "regr_slope" | "regr_intercept"
            | "regr_count",
            [_, _],
        ) => {
            if !args.iter().all(numberish) {
                return refused();
            }
            Some(C::Float8)
        }
        ("bit_and" | "bit_or" | "bit_xor", [t]) => match t {
            Some(t) if t.is_integer() => Some(*t),
            Some(C::Bit(_) | C::Varbit(_)) => Some(C::Bit(0)),
            None => None,
            Some(_) => return refused(),
        },
        ("any_value", [t]) => *t,
        ("string_agg", [t, _]) => match t {
            Some(C::Bytes) => Some(C::Bytes),
            None | Some(C::Text(_) | C::Bpchar(_) | C::Name) => Some(C::Text(None)),
            Some(_) => return refused(),
        },
        _ => return Ok(None),
    }))
}

/// The type an ordered-set or hypothetical-set aggregate returns over its direct arguments and the
/// columns its WITHIN GROUP orders by.
fn ordered_set(
    name: &str,
    direct: &[Option<ColumnType>],
    within: &[Option<ColumnType>],
) -> Result<Option<ColumnType>> {
    use ColumnType as C;
    let refused = || {
        Err(Error::new(format!(
            "function {name}({}) does not exist",
            direct
                .iter()
                .chain(within)
                .map(|t| arg_name(*t))
                .collect::<Vec<_>>()
                .join(", ")
        )))
    };
    Ok(match (name, direct, within) {
        ("mode", [], [t]) => *t,
        ("percentile_disc", [_], [t]) => *t,
        ("percentile_cont", [_], [t]) => match t.map(super::clock::base) {
            Some(C::Interval { .. }) => Some(super::calls::full_interval()),
            Some(t) if t.is_integer() || t.is_numeric() || t.is_float() => Some(C::Float8),
            None => None,
            Some(_) => return refused(),
        },
        ("rank" | "dense_rank", d, w) if d.len() == w.len() => Some(C::Int8),
        ("percent_rank" | "cume_dist", d, w) if d.len() == w.len() => Some(C::Float8),
        _ => return refused(),
    })
}

/// The type a function of strings returns, or its refusal where its first argument is no string;
/// none where the name is no function of strings the snail answers.
pub(super) fn strings(
    name: &str,
    args: &[Option<ColumnType>],
) -> Result<Option<Option<ColumnType>>> {
    use ColumnType as C;
    if !super::calls::is_string_function(name) {
        return Ok(None);
    }
    let stringy = |t: &Option<ColumnType>| {
        matches!(
            t,
            None | Some(C::Text(_) | C::Bpchar(_) | C::Name | C::Bytes)
        )
    };
    let bytes = matches!(args.first(), Some(Some(C::Bytes)));
    Ok(Some(match name {
        "concat" | "concat_ws" | "format" if args.is_empty() => return Err(no_form(name, args)),
        "concat" | "concat_ws" | "format" | "quote_literal" | "quote_nullable" => {
            Some(C::Text(None))
        }
        "pg_encoding_to_char" => Some(C::Name),
        "pg_encoding_max_length" | "pg_char_to_encoding" | "get_byte" | "get_bit" => Some(C::Int4),
        "crc32" | "crc32c" | "bit_count" => Some(C::Int8),
        "md5" | "encode" | "convert_from" | "to_ascii" => Some(C::Text(None)),
        "sha224" | "sha256" | "sha384" | "sha512" | "decode" | "set_byte" | "set_bit"
        | "convert" | "convert_to" => Some(C::Bytes),
        "chr" => match args {
            [Some(t)] if t.is_integer() => Some(C::Text(None)),
            [None] => Some(C::Text(None)),
            _ => return Err(no_form(name, args)),
        },
        _ if !args.first().is_none_or(stringy) => return Err(no_form(name, args)),
        "length" | "char_length" | "character_length" | "textlen" | "octet_length"
        | "bit_length" | "ascii" | "strpos" => Some(C::Int4),
        "like" | "notlike" | "starts_with" => Some(C::Boolean),
        "substr" | "reverse" | "btrim" | "ltrim" | "rtrim" if bytes => Some(C::Bytes),
        _ => Some(C::Text(None)),
    }))
}

/// The type a regular expression's function returns, in the form PostgreSQL resolves for its
/// arguments, or its refusal where it has no such form or returns a set; none where the name is
/// none of them.
pub(super) fn regexps(
    name: &str,
    args: &[Option<ColumnType>],
) -> Result<Option<Option<ColumnType>>> {
    use ColumnType as C;
    if !super::calls::is_regexp_function(name) {
        return Ok(None);
    }
    if super::calls::returns_a_set(name) {
        return Err(super::calls::set_refused(name));
    }
    // Each form, a letter an argument: t a string, i an integer.
    let forms: &[&str] = match name {
        "regexp_count" => &["tt", "tti", "ttit"],
        "regexp_instr" => &["tt", "tti", "ttii", "ttiii", "ttiiit", "ttiiiti"],
        "regexp_like" | "regexp_match" | "regexp_split_to_array" => &["tt", "ttt"],
        "regexp_replace" => &["ttt", "tttt", "ttti", "tttii", "tttiit"],
        "regexp_substr" => &["tt", "tti", "ttii", "ttiit", "ttiiti"],
        "similar_escape" => &["tt"],
        _ => &["t", "tt"],
    };
    let fits = |form: &str| {
        form.len() == args.len()
            && form.chars().zip(args).all(|(k, t)| match (k, t) {
                (_, None) => true,
                ('t', Some(t)) => matches!(t, C::Text(_) | C::Bpchar(_) | C::Name),
                (_, Some(t)) => matches!(t, C::Int2 | C::Int4),
            })
    };
    if !forms.iter().any(|f| fits(f)) {
        return Err(no_form(name, args));
    }
    Ok(Some(Some(match name {
        "regexp_count" | "regexp_instr" => C::Int4,
        "regexp_like" => C::Boolean,
        "regexp_match" | "regexp_split_to_array" => super::calls::text_array(),
        _ => C::Text(None),
    })))
}
