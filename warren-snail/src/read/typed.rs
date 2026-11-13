//! The types a step's expressions take, read before any row: a column's from the relation it comes
//! from, a literal's as PostgreSQL types it, an operator's and a function's as PostgreSQL resolves
//! them. An aggregate or a function PostgreSQL has no form of for its arguments is refused here,
//! over no rows too, as is a function of the server's own; an expression whose type the snail does
//! not resolve is read as before, a row at a time.

use crate::catalog::ColumnType;
use crate::error::{FUNCTION_NOT_UNIQUE, NO_OPERATOR};
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

thread_local! {
    /// The domain each column of the step being typed is of, by its place, where its table's DDL
    /// gives it one.
    static DOMAINS: std::cell::RefCell<Vec<Option<String>>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// The domains of a step's columns, held while it is typed.
pub(super) struct Domains(Vec<Option<String>>);

/// Holds the domains of the columns of the step about to be typed.
pub(super) fn domains_held(domains: Vec<Option<String>>) -> Domains {
    Domains(DOMAINS.with(|d| d.replace(domains)))
}

impl Drop for Domains {
    fn drop(&mut self) {
        let before = std::mem::take(&mut self.0);
        DOMAINS.with(|d| *d.borrow_mut() = before);
    }
}

/// The domain the value of an expression over a step's columns is of: a column's of a domain, or a
/// cast to one; none for anything else.
pub(super) fn domain_in(e: &Expr, columns: &[TypedColumn]) -> Option<String> {
    match e {
        Expr::Nested(x) => return domain_in(x, columns),
        Expr::Cast { data_type, .. } => {
            return super::handed::domain_named(data_type).map(|(d, _)| d.name)
        }
        _ => {}
    }
    let (q, n) = super::pipeline::column_of(e)?;
    let found = super::pipeline::naming(
        columns.iter().map(|(cq, cn, _)| (cq.as_deref(), cn.as_str())),
        q.as_deref(),
        &n,
    );
    let [i] = found.as_slice() else {
        return None;
    };
    DOMAINS.with(|d| {
        let d = d.borrow();
        match d.len() == columns.len() {
            true => d[*i].clone(),
            false => None,
        }
    })
}

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
pub(super) fn arg_name(t: Option<ColumnType>) -> String {
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

pub(super) use crate::error::NO_FUNCTION;

/// PostgreSQL's refusal of a call no form of the function takes, the function named as the call
/// names it.
pub(super) fn no_form(name: &str, args: &[Option<ColumnType>]) -> Error {
    Error::new(format!(
        "function {name}({}) does not exist{NO_FUNCTION}",
        args.iter()
            .map(|t| arg_name(*t))
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

/// PostgreSQL's refusal of an operator no form of it takes, each side by its type's name.
pub(super) fn no_operator(left: &str, op: &str, right: &str) -> Error {
    crate::error::no_operator(Some(left), op, right)
}

/// A refusal of a call no form of a function takes, the function named as the call writes it, in
/// its schema where it names one; any other refusal as it is.
fn named_as_written(e: Error, name: &str, written: &str) -> Error {
    let bare = format!("function {name}(");
    match e.0.strip_prefix(&bare) {
        Some(rest) if written != name && e.sqlstate() == Some("42883") => {
            Error::new(format!("function {written}({rest}"))
        }
        _ => e,
    }
}

/// The wider of two integers, as PostgreSQL's operators across integer widths return.
pub(super) fn wider(a: ColumnType, b: ColumnType) -> ColumnType {
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

/// The type PostgreSQL's select_common_type finds for expressions read together, `context` naming
/// them in its refusal: an untyped literal takes the others' type, text where every one is untyped;
/// of two numbers, or two instants, the one the other is implicitly cast to; of two strings, the
/// first's, but `name` over a varchar or a bpchar, which casts to it unasked where it casts back only
/// on assignment; any other two only where they are one. Types that differ only in their modifier
/// take none.
pub(super) fn common_type(ts: &[Option<ColumnType>], context: &str) -> Result<ColumnType> {
    use ColumnType as C;
    let place = |t: ColumnType| -> Option<(u8, u8)> {
        Some(match t {
            C::Int2 => (0, 0),
            C::Int4 => (0, 1),
            C::Int8 => (0, 2),
            t if t.is_numeric() => (0, 3),
            C::Float4 => (0, 4),
            C::Float8 => (0, 5),
            C::Text(_) | C::Bpchar(_) | C::Name => (1, 0),
            C::Date => (2, 0),
            C::Timestamp(_) => (2, 1),
            C::Timestamptz | C::TimestamptzRounded(_) => (2, 2),
            C::Time(_) => (3, 0),
            C::Timetz(_) => (3, 1),
            _ => return None,
        })
    };
    let unmodified = |t: ColumnType| match t {
        C::Text(Some(_)) => C::Text(Some(C::VARCHAR)),
        C::Bpchar(_) => C::Bpchar(None),
        t if t.is_numeric() => NUMERIC,
        t => super::clock::base(t),
    };
    let mut out: Option<ColumnType> = None;
    for &t in ts.iter().flatten() {
        let Some(p) = out else {
            out = Some(t);
            continue;
        };
        if p == t {
            continue;
        }
        if t == C::Name && matches!(p, C::Text(Some(_)) | C::Bpchar(_)) {
            out = Some(t);
            continue;
        }
        // An oid is the preferred type its integers are cast to.
        if p == C::Oid && t.is_integer() {
            continue;
        }
        if t == C::Oid && p.is_integer() {
            out = Some(t);
            continue;
        }
        // A vector meets its elements' array as that array.
        if crate::composed::array_type(p) == t {
            out = Some(t);
            continue;
        }
        if crate::composed::array_type(t) == p {
            continue;
        }
        match (place(p), place(t)) {
            (Some((a, x)), Some((b, y))) if a == b && y > x => out = Some(unmodified(t)),
            (Some((a, x)), Some((b, y))) if a == b && (y < x || a == 1) => {
                out = Some(unmodified(p))
            }
            _ if unmodified(p) == unmodified(t) => out = Some(unmodified(p)),
            _ => {
                return Err(Error::new(format!(
                    "{context} types {} and {} cannot be matched",
                    p.pg_name(),
                    t.pg_name()
                )))
            }
        }
    }
    Ok(out.unwrap_or(C::Text(None)))
}

/// An array of a type.
pub(crate) fn array_of(element: ColumnType) -> ColumnType {
    crate::composed::type_of(crate::composed::Composed::Array {
        element,
        domain: None,
    })
}

/// An array type's element type.
pub(crate) fn element_of(ty: ColumnType) -> Option<ColumnType> {
    match ty {
        ColumnType::Composed(r) => match r.def() {
            crate::composed::Composed::Array { element, .. }
            | crate::composed::Composed::Vector { element } => Some(*element),
            _ => None,
        },
        _ => None,
    }
}

/// The element a fixed-length type's subscript gives, as PostgreSQL's raw subscripting gives it:
/// a point's or a line's double precision, an lseg's or a box's point, a name's `"char"`.
pub(crate) fn raw_element_of(ty: ColumnType) -> Option<ColumnType> {
    use crate::geo::GeoKind;
    Some(match ty {
        ColumnType::Geometric(GeoKind::Point | GeoKind::Line) => ColumnType::Float8,
        ColumnType::Geometric(GeoKind::Lseg | GeoKind::Box) => {
            ColumnType::Geometric(GeoKind::Point)
        }
        ColumnType::Name => ColumnType::Char,
        _ => return None,
    })
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
        match super::pipeline::naming(
            self.columns
                .iter()
                .map(|(cq, cn, _)| (cq.as_deref(), cn.as_str())),
            q.as_deref(),
            &n,
        )
        .as_slice()
        {
            [i] => self.columns.get(*i)?.2,
            _ => None,
        }
    }

    /// The domain a column an expression names is of, where its table's DDL gives it one.
    fn domain(&self, e: &Expr) -> Option<String> {
        domain_in(e, self.columns)
    }

    /// An expression's type, where the snail resolves it, each sub-expression's recorded.
    pub fn type_of(&mut self, e: &Expr) -> Result<Option<ColumnType>> {
        let t = self.resolve(e)?;
        if let Some(t) = t {
            self.types.insert(e as *const Expr, t);
        }
        Ok(t)
    }

    /// An expression standing where a truth stands, typed as PostgreSQL's coerce_to_boolean types
    /// it, `clause` naming where: a quoted literal read by boolean's input before any row, refused
    /// where it reads as none; an expression of another type refused in PostgreSQL's words.
    pub fn condition(&mut self, e: &Expr, clause: &str) -> Result<()> {
        if let Some(Some(s)) = untyped(e) {
            return match super::question::truth(s) {
                Some(_) => Ok(()),
                None => Err(Error::new(format!(
                    "invalid input syntax for type boolean: \"{s}\""
                ))),
            };
        }
        match self.type_of(e)? {
            Some(t) if t != ColumnType::Boolean => Err(Error::new(format!(
                "argument of {clause} must be type boolean, not type {}",
                type_name(t)
            ))),
            _ => Ok(()),
        }
    }

    /// A subscripted column's assignment (`image::ASSIGNED`), its column's type: the value taken by
    /// assignment as an element of a type PostgreSQL subscripts from 0, or as the type itself for a
    /// slice; each subscript an integer.
    fn assigned(&mut self, parts: Vec<&Expr>) -> Result<Option<ColumnType>> {
        let [column, value, subs @ ..] = parts.as_slice() else {
            return Ok(None);
        };
        let ty = self.type_of(column)?;
        let given = self.type_of(value)?;
        let mut sliced = false;
        for s in subs {
            let bounds = match s {
                Expr::Function(f) if super::pipeline::function_name(f) == super::image::SLICED => {
                    sliced = true;
                    super::pipeline::parts_of(s)
                }
                s => vec![*s],
            };
            for b in bounds {
                match self.type_of(b)? {
                    Some(t) if !t.is_integer() && !crate::catalog::assignable(t, ColumnType::Int4) => {
                        return Err(Error::coded("42804", "array subscript must have type integer"))
                    }
                    _ => {}
                }
            }
        }
        let Some(ty) = ty else {
            return Ok(None);
        };
        let named = super::pipeline::column_of(column).map(|(_, n)| n).unwrap_or_default();
        let wanted = match (raw_element_of(ty), element_of(ty)) {
            (Some(el), _) => match sliced {
                true => ty,
                false => el,
            },
            (None, Some(_)) => {
                return Err(Error::new(format!(
                    "an element of the array \"{named}\" assigned is not read"
                )))
            }
            (None, None) => {
                return Err(Error::coded(
                    "42804",
                    format!(
                        "cannot subscript type {} because it does not support subscripting",
                        type_name(ty)
                    ),
                ))
            }
        };
        if let Some(g) = given.filter(|&g| g != wanted && !crate::catalog::assignable(g, wanted)) {
            return Err(Error::coded(
                "42804",
                format!(
                    "subscripted assignment to \"{named}\" requires type {} but expression is of \
                     type {}\nHINT:  You will need to rewrite or cast the expression.",
                    type_name(wanted),
                    type_name(g)
                ),
            ));
        }
        Ok(Some(ty))
    }

    fn resolve(&mut self, e: &Expr) -> Result<Option<ColumnType>> {
        Ok(match e {
            // A scalar subquery's type is its column's, given before the step is typed; EXISTS, and
            // IN, ANY or ALL over a subquery, are truths.
            Expr::Subquery(_) => self.types.get(&(e as *const Expr)).copied(),
            Expr::Exists { .. } => Some(ColumnType::Boolean),
            Expr::InSubquery { expr, .. } => {
                self.type_of(expr)?;
                Some(ColumnType::Boolean)
            }
            Expr::AnyOp { left, right, .. } | Expr::AllOp { left, right, .. }
                if super::image::marked(right).is_some() =>
            {
                let (name, array) = super::image::marked(right).unwrap();
                super::image::ordered(name)?;
                let (a, b) = (self.type_of(left)?, self.type_of(array)?);
                super::image::over_array_operands(name, a, b, left, array)?;
                Some(ColumnType::Boolean)
            }
            Expr::AnyOp { left, right, .. } | Expr::AllOp { left, right, .. }
                if matches!(right.as_ref(), Expr::Subquery(_)) =>
            {
                self.type_of(left)?;
                Some(ColumnType::Boolean)
            }
            Expr::AnyOp { left, right, .. } | Expr::AllOp { left, right, .. } => {
                self.type_of(left)?;
                self.type_of(right)?;
                Some(ColumnType::Boolean)
            }
            Expr::Identifier(_) | Expr::CompoundIdentifier(_) => self.column(e),
            Expr::Nested(x) => self.type_of(x)?,
            // A COLLATE over a type no collation orders is refused as it is typed, before the
            // collation's name is looked up; an untyped literal is let go.
            Expr::Collate { expr, .. } => match self.type_of(expr)? {
                Some(t) if !super::collate::collatable(t) => {
                    let named = self.domain(expr).unwrap_or_else(|| type_name(t));
                    return Err(Error::new(format!(
                        "collations are not supported by type {named}"
                    )));
                }
                t => t,
            },
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
                let from = self.type_of(expr)?;
                let to = crate::catalog::held_type(data_type)
                    .ok()
                    .or_else(|| crate::composed::resolve(data_type))
                    .or_else(|| super::handed::domain_named(data_type).and_then(|(d, _)| d.base_type))
                    .or_else(|| super::prng::is_void(data_type).then_some(super::prng::VOID));
                if to != from && to != Some(ColumnType::Bytes) {
                    written(from)?;
                }
                to
            }
            Expr::TypedString(t) => crate::catalog::held_type(&t.data_type).ok(),
            Expr::Interval(_) => Some(super::calls::full_interval()),
            // A field of a composite by its name.
            Expr::CompoundFieldAccess { root, access_chain }
                if matches!(
                    access_chain.as_slice(),
                    [sqlparser::ast::AccessExpr::Dot(Expr::Identifier(_))]
                ) =>
            {
                let t = self.type_of(root)?;
                let [sqlparser::ast::AccessExpr::Dot(Expr::Identifier(i))] = access_chain.as_slice()
                else {
                    unreachable!()
                };
                match t {
                    Some(ColumnType::Composed(r)) => match r.def() {
                        crate::composed::Composed::Record { name, fields } => {
                            let n = crate::catalog::ident(i);
                            match fields.iter().find(|f| f.name == n) {
                                Some(f) => Some(f.ty),
                                None => {
                                    return Err(Error::new(format!(
                                        "column \"{n}\" not found in data type {name}"
                                    )))
                                }
                            }
                        }
                        _ => None,
                    },
                    _ => None,
                }
            }
            // An element of an array by its subscripts; a slice among them, the array.
            Expr::CompoundFieldAccess { root, access_chain }
                if access_chain
                    .iter()
                    .all(|a| matches!(a, sqlparser::ast::AccessExpr::Subscript(_))) =>
            {
                let t = self.type_of(root)?;
                // Each subscript is coerced to integer by assignment: refused before any row where
                // its type has no such cast, as PostgreSQL's parser refuses it.
                for x in super::pipeline::subscripts(access_chain) {
                    if self.type_of(x)?.is_some_and(|s| {
                        !(s.is_integer() || s.is_numeric() || s.is_float() || s.is_oid())
                    }) {
                        return Err(Error::new("array subscript must have type integer"));
                    }
                }
                let sliced = access_chain.iter().any(|a| {
                    matches!(
                        a,
                        sqlparser::ast::AccessExpr::Subscript(
                            sqlparser::ast::Subscript::Slice { .. }
                        )
                    )
                });
                match (t.and_then(element_of), t.and_then(raw_element_of)) {
                    (Some(e), _) if sliced => Some(array_of(e)),
                    (Some(e), _) => Some(e),
                    (None, Some(_)) if sliced => t,
                    (None, raw) => raw,
                }
            }
            // An ARRAY's elements take one type; an array among them is a dimension of it.
            Expr::Array(a) => {
                let mut ts = Vec::with_capacity(a.elem.len());
                for x in &a.elem {
                    ts.push(self.type_of(x)?);
                }
                if a.elem.is_empty() {
                    None
                } else {
                    let t = common_type(&ts, "ARRAY")?;
                    // A vector among them is an element, as PostgreSQL's parser takes it.
                    Some(if element_of(t).is_some() && !crate::composed::is_vector(t) {
                        t
                    } else {
                        array_of(t)
                    })
                }
            }
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
                op:
                    op @ (UnaryOperator::Plus
                    | UnaryOperator::PGAbs
                    | UnaryOperator::PGSquareRoot
                    | UnaryOperator::PGCubeRoot
                    | UnaryOperator::BitwiseNot),
                expr,
            } => {
                let t = self.type_of(expr)?;
                super::ops::prefix_type(op, t, expr)?
            }
            Expr::UnaryOp {
                op: UnaryOperator::Minus,
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
                self.condition(expr, "NOT")?;
                Some(ColumnType::Boolean)
            }
            Expr::IsNull(x) | Expr::IsNotNull(x) => {
                self.type_of(x)?;
                Some(ColumnType::Boolean)
            }
            Expr::IsTrue(x)
            | Expr::IsNotTrue(x)
            | Expr::IsFalse(x)
            | Expr::IsNotFalse(x)
            | Expr::IsUnknown(x)
            | Expr::IsNotUnknown(x) => {
                let clause = match e {
                    Expr::IsTrue(_) => "IS TRUE",
                    Expr::IsNotTrue(_) => "IS NOT TRUE",
                    Expr::IsFalse(_) => "IS FALSE",
                    Expr::IsNotFalse(_) => "IS NOT FALSE",
                    Expr::IsUnknown(_) => "IS UNKNOWN",
                    _ => "IS NOT UNKNOWN",
                };
                self.condition(x, clause)?;
                Some(ColumnType::Boolean)
            }
            Expr::InList { expr, list, .. } => {
                let t = self.type_of(expr)?;
                // A row IN a list as PostgreSQL's parser refuses it, where it is not written as the
                // OR of its rows' comparisons (`spelled::rows_in`).
                if super::pipeline::row_parts(expr).is_some() {
                    super::spelled::rows_in_refused(e, &mut |x, with| {
                        let u = self.type_of(x)?;
                        match (with, u) {
                            (Some((op, field)), u) => {
                                let f = self.type_of(field)?;
                                compared(&op.to_string(), (f, field), (u, x))
                            }
                            (None, _) if super::image::unknown(x) => Err(Error::new(
                                "input of anonymous composite types is not implemented",
                            )),
                            (None, Some(u)) => Err(Error::new(format!(
                                "operator does not exist: record = {}{NO_OPERATOR}",
                                type_name(u)
                            ))),
                            (None, None) => Ok(()),
                        }
                    })?;
                }
                for x in list {
                    let u = self.type_of(x)?;
                    statistics_operator("=", t, u)?;
                }
                Some(ColumnType::Boolean)
            }
            Expr::SimilarTo {
                expr,
                pattern,
                escape_char,
                ..
            } => {
                let (t, u) = (self.type_of(expr)?, self.type_of(pattern)?);
                if let Some(c) = escape_char {
                    self.type_of(c)?;
                }
                statistics_operator("~", t, u)?;
                Some(ColumnType::Boolean)
            }
            // A pattern of LIKE, or of ILIKE, is `~~` or `~~*`.
            Expr::Like {
                negated,
                expr,
                pattern,
                escape_char,
                ..
            }
            | Expr::ILike {
                negated,
                expr,
                pattern,
                escape_char,
                ..
            } => {
                let (t, u) = (self.type_of(expr)?, self.type_of(pattern)?);
                if let Some(c) = escape_char {
                    self.type_of(c)?;
                }
                let op = match (matches!(e, Expr::ILike { .. }), *negated) {
                    (false, false) => "~~",
                    (false, true) => "!~~",
                    (true, false) => "~~*",
                    (true, true) => "!~~*",
                };
                statistics_operator(op, t, u)?;
                super::ops::like_form(op, t, u, expr, pattern)?;
                Some(ColumnType::Boolean)
            }
            Expr::IsDistinctFrom(a, b) | Expr::IsNotDistinctFrom(a, b) => {
                let (t, u) = (self.type_of(a)?, self.type_of(b)?);
                // Two rows of one length are compared column by column, as the statement is
                // written before it is typed (`spelled::between_written`); of two lengths, refused
                // as make_row_distinct_op refuses them.
                if let (Some(Ok(x)), Some(Ok(y))) =
                    (super::pipeline::row_parts(a), super::pipeline::row_parts(b))
                {
                    if x.len() != y.len() {
                        return Err(Error::new("unequal number of entries in row expressions"));
                    }
                }
                statistics_operator("=", t, u)?;
                compared("=", (t, a), (u, b))?;
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
                        "function pg_catalog.overlaps({}) is not unique{FUNCTION_NOT_UNIQUE}",
                        ends.iter()
                            .map(|t| arg_name(*t))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )));
                }
                Some(ColumnType::Boolean)
            }
            Expr::BinaryOp { left, op, right } if super::image::operator(op).is_some() => {
                let name = super::image::operator(op).unwrap_or_default();
                super::image::ordered(name)?;
                if let (Some(l), Some(r)) = (
                    super::pipeline::row_parts(left),
                    super::pipeline::row_parts(right),
                ) {
                    let (l, r) = (l?, r?);
                    super::image::rows_agree(l.len(), r.len())?;
                    for (x, y) in l.iter().zip(&r) {
                        let (a, b) = (self.type_of(x)?, self.type_of(y)?);
                        super::image::operands(name, a, b, x, y)?;
                    }
                } else {
                    let (a, b) = (self.type_of(left)?, self.type_of(right)?);
                    super::image::operands(name, a, b, left, right)?;
                }
                Some(ColumnType::Boolean)
            }
            // `@@@`, which sqlparser reads as `@@` and a prefix `@`: the match `@@` is, its prefix
            // `@` none of its own.
            Expr::BinaryOp {
                left,
                op: BinaryOperator::AtAt,
                right,
            } if super::spelled::right_of_three_ats(right).is_some() => {
                self.type_of(left)?;
                if let Some(inner) = super::spelled::right_of_three_ats(right) {
                    self.type_of(inner)?;
                }
                Some(ColumnType::Boolean)
            }
            Expr::BinaryOp {
                left,
                op: op @ (BinaryOperator::And | BinaryOperator::Or),
                right,
            } => {
                let clause = match op {
                    BinaryOperator::And => "AND",
                    _ => "OR",
                };
                self.condition(left, clause)?;
                self.condition(right, clause)?;
                Some(ColumnType::Boolean)
            }
            Expr::BinaryOp { left, op, right } => {
                let (a, b) = (self.type_of(left)?, self.type_of(right)?);
                if let Some(t) = statistics_operator(&op.to_string(), a, b)? {
                    return Ok(Some(t));
                }
                if let Some(t) = range_operator(&op.to_string(), a, b)? {
                    return Ok(Some(t));
                }
                if let Some(t) = super::ops::binary_type(op, a, b, left, right)? {
                    return Ok(Some(t));
                }
                if *op == BinaryOperator::StringConcat {
                    return concatenated(a, b, left, right);
                }
                match op {
                    BinaryOperator::Plus
                    | BinaryOperator::Minus
                    | BinaryOperator::Multiply
                    | BinaryOperator::Divide
                    | BinaryOperator::Modulo => arithmetic(a, b),
                    BinaryOperator::Eq
                    | BinaryOperator::NotEq
                    | BinaryOperator::Lt
                    | BinaryOperator::LtEq
                    | BinaryOperator::Gt
                    | BinaryOperator::GtEq => {
                        // Two rows compared, of two widths or of none, as make_row_comparison_op
                        // refuses them.
                        // Two rows of one width, each pair by the operator.
                        if let (Some(l), Some(r)) = (
                            super::pipeline::row_parts(left),
                            super::pipeline::row_parts(right),
                        ) {
                            let (l, r) = (l?, r?);
                            super::image::rows_agree(l.len(), r.len())?;
                            for (x, y) in l.iter().zip(&r) {
                                let (t, u) = (self.type_of(x)?, self.type_of(y)?);
                                compared(&op.to_string(), (t, *x), (u, *y))?;
                            }
                            return Ok(Some(ColumnType::Boolean));
                        }
                        compared(&op.to_string(), (a, left), (b, right))?;
                        Some(ColumnType::Boolean)
                    }
                    BinaryOperator::And
                    | BinaryOperator::Or
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
                let compared_to = match operand {
                    Some(o) => self.type_of(o)?,
                    None => None,
                };
                let mut arms = Vec::with_capacity(conditions.len() + 1);
                for c in conditions {
                    match operand {
                        None => self.condition(&c.condition, "CASE/WHEN")?,
                        // Each WHEN is the operand's `=`, which void has none of.
                        Some(_) => {
                            let t = self.type_of(&c.condition)?;
                            void_compared(compared_to, t)?;
                        }
                    }
                    arms.push((&c.result, self.type_of(&c.result)?));
                }
                if let Some(x) = else_result {
                    arms.push((x.as_ref(), self.type_of(x)?));
                }
                case_type(&arms, else_result.is_some())?
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
            Expr::Tuple(_) | Expr::Function(_) if super::pipeline::row_parts(e).is_some() => {
                let parts = super::pipeline::row_parts(e).unwrap()?;
                let mut types = Vec::with_capacity(parts.len());
                for x in &parts {
                    types.push(self.type_of(x)?);
                }
                // A relation's whole row, its type given before the step is typed.
                if let (Expr::Tuple(_), Some(&t)) = (e, self.types.get(&(e as *const Expr))) {
                    return Ok(Some(t));
                }
                let domains: Vec<Option<String>> = parts.iter().map(|x| self.domain(x)).collect();
                super::image::field_types(&parts, &types)
                    .map(|f| super::image::anonymous_of(&f, &domains))
            }
            Expr::Function(f) if super::pipeline::function_name(f) == super::image::ASSIGNED => {
                self.assigned(super::pipeline::parts_of(e))?
            }
            // A row's expansion no list of values holds: its row typed first.
            Expr::Function(f) if super::pipeline::function_name(f) == super::image::EXPANDED => {
                for x in super::pipeline::parts_of(e) {
                    self.type_of(x)?;
                }
                return Err(super::image::expansion_refused());
            }
            Expr::Function(f) => {
                let name = super::pipeline::function_name(f);
                // GROUPING (…): an integer, a bit for each argument.
                if super::pipeline::is_grouping_call(f) {
                    for a in super::pipeline::parts_of(e) {
                        self.type_of(a)?;
                    }
                    return Ok(Some(ColumnType::Int4));
                }
                if let Some(e) = type_keyword(f) {
                    return Err(e);
                }
                // A function named in a schema that stands nowhere, refused as PostgreSQL's lookup
                // refuses it, before its arguments.
                if let [sqlparser::ast::ObjectNamePart::Identifier(s), _] = f.name.0.as_slice() {
                    let s = crate::catalog::ident(s);
                    if s != "pg_catalog" {
                        crate::regs::schema_found(&s)?;
                    }
                }
                // An aggregate's FILTER stands where a truth stands.
                if let Some(w) = &f.filter {
                    self.condition(w, "FILTER")?;
                }
                // count(*) counts rows, a bigint, as an aggregate and as a window.
                if name == "count" && f.args_star() {
                    return Ok(Some(ColumnType::Int8));
                }
                // A function of the file's, its arguments by place or by name.
                if let Some(t) = super::own::typed(f, &mut |x| self.type_of(x))? {
                    return Ok(Some(t));
                }
                // A built-in called by its parameters' names: its arguments in their places, or
                // refused where no form takes them.
                let placed = match super::named::is_named(f) {
                    true => match super::named::positional(f, self.release)? {
                        Some(args) => Some(args),
                        None => {
                            let mut written = Vec::new();
                            for a in super::named::written_args(f) {
                                written.push(self.type_of(a)?);
                            }
                            return Err(super::named::no_form(f, &written));
                        }
                    },
                    false => None,
                };
                let Some(args) = placed.or_else(|| call_args(f)) else {
                    return Ok(None);
                };
                let mut types = Vec::with_capacity(args.len());
                for a in &args {
                    types.push(self.type_of(a)?);
                }
                // A call named in a schema calls a function of that schema: PostgreSQL's own only in
                // pg_catalog, and there only of a name it defines a function of.
                let as_written = crate::catalog::object_name(&f.name);
                if let [sqlparser::ast::ObjectNamePart::Identifier(s), _] = f.name.0.as_slice() {
                    let built_in = match crate::catalog::ident(s).as_str() {
                        "pg_catalog" => super::own::built_in_name(&name),
                        "information_schema" => true,
                        _ => false,
                    };
                    if !built_in && !super::handed::is_handed(f) {
                        return Err(no_form(&as_written, &types));
                    }
                }
                // A bare name of no function: none of PostgreSQL's, no word of its grammar, none
                // handed in.
                if let [sqlparser::ast::ObjectNamePart::Identifier(_)] = f.name.0.as_slice() {
                    let word = name.chars().all(|c| c.is_alphanumeric() || c == '_');
                    if word
                        && !super::own::built_in_name(&name)
                        && !super::calls::reserved().contains(name.as_str())
                        && !super::handed::is_handed(f)
                    {
                        return Err(no_form(&as_written, &types));
                    }
                }
                // Its refusals of no form name the function as the call writes it.
                let typed = (|| -> Result<Option<ColumnType>> {
                    if WRITING.contains(&name.as_str()) {
                        for t in &types {
                            written(*t)?;
                        }
                    }
                    if super::handed::is_handed(f) {
                        if let Some(t) = super::handed::typed(f, &types)? {
                            return Ok(Some(t));
                        }
                    }
                    if let Some(t) = super::populate::typed(&name, &types, self.release)? {
                        return Ok(t);
                    }
                    if f.name.0.len() == 1 && f.over.is_none() {
                        if let Some(t) = chosen(&name, &args, &types)? {
                            return Ok(Some(t));
                        }
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
                    if let Some(t) = super::rest::typed(&name, &types)? {
                        return Ok(t);
                    }
                    if let Some(t) = numbers(&name, &types)? {
                        return Ok(t);
                    }
                    // generate_series names its zone from 16 on; the abbreviations' function is
                    // pg_timezone_abbrevs before 18, and from 18 on the set's and the zone's apart.
                    if name == "generate_series" && types.len() == 4 && self.release < 16 {
                        return Err(no_form(&name, &types));
                    }
                    let abbreviations = match name.as_str() {
                        "pg_timezone_abbrevs" => Some(self.release < 18),
                        "pg_timezone_abbrevs_abbrevs" | "pg_timezone_abbrevs_zone" => {
                            Some(self.release >= 18)
                        }
                        _ => None,
                    };
                    if abbreviations == Some(false) {
                        return Err(no_form(&name, &types));
                    }
                    if let Some(t) = time(&name, &types)? {
                        return Ok(t);
                    }
                    if name == super::settings::ROWS && types.is_empty() {
                        return Ok(Some(super::settings::record()));
                    }
                    if let Some(t) = super::tsearch::typed(&name, &types)? {
                        return Ok(t);
                    }
                    if let Some(t) = super::ranges::typed(&name, &types)? {
                        return Ok(t);
                    }
                    if let Some(t) = strings(&name, &types)? {
                        return Ok(t);
                    }
                    if let Some(t) = regexps(&name, &types)? {
                        return Ok(t);
                    }
                    // unnest of a quoted literal or a NULL: an array's, a multirange's and a
                    // tsvector's forms all take it.
                    let literal = |e: &Expr| {
                        matches!(untyped(e), Some(Some(_)))
                            || matches!(e, Expr::Value(v) if v.value == SqlValue::Null)
                    };
                    if name == "unnest" && types == [None] && literal(args[0]) {
                        return Err(Error::new(format!(
                            "function {name}({}) is not unique{FUNCTION_NOT_UNIQUE}",
                            arg_name(None)
                        )));
                    }
                    if let Some(t) = arrays(&name, &types)? {
                        return Ok(t);
                    }
                    if let Some(t) = super::arrays::typed(&name, &types)? {
                        return Ok(t);
                    }
                    if let Some(t) = super::json::typed(&name, &types)? {
                        return Ok(t);
                    }
                    if let Some(t) = super::updatable::typed(&name) {
                        return Ok(Some(t));
                    }
                    if let Some(t) = super::sequences::typed(&name) {
                        return Ok(Some(t));
                    }
                    if let Some(t) = super::prepared::typed(&name, self.release) {
                        if !types.is_empty() {
                            return Err(no_form(&name, &types));
                        }
                        return Ok(Some(t));
                    }
                    if let Some(t) = crate::deparse::typed(&name) {
                        return Ok(Some(t));
                    }
                    if crate::regs::visibility_kind(&name).is_some() {
                        return Ok(Some(ColumnType::Boolean));
                    }
                    if name == "obj_description" || name == "col_description" {
                        return Ok(Some(ColumnType::Text(None)));
                    }
                    crate::functions::refuse_server(&name, self.release)?;
                    Ok(None)
                })();
                typed.map_err(|e| named_as_written(e, &name, &as_written))?
            }
            _ => None,
        })
    }
}

/// A side written as a quoted literal, NULL or a parameter, of no type yet: the literal's text, where
/// it has one. None where the side is written any other way.
pub(super) fn untyped(e: &Expr) -> Option<Option<&str>> {
    match e {
        Expr::Nested(e) => untyped(e),
        Expr::Value(v) => match &v.value {
            SqlValue::SingleQuotedString(s) | SqlValue::EscapedStringLiteral(s) => Some(Some(s)),
            SqlValue::DollarQuotedString(d) => Some(Some(&d.value)),
            SqlValue::Null | SqlValue::Placeholder(_) => Some(None),
            _ => None,
        },
        _ => None,
    }
}

/// The form of `||` a type has of its own, where it has one: what that form returns.
pub(super) fn concat_form(t: ColumnType) -> Option<ColumnType> {
    use crate::textual::Textual;
    match t {
        ColumnType::Bytes
        | ColumnType::Textual(Textual::Jsonb | Textual::Tsvector | Textual::Tsquery) => Some(t),
        ColumnType::Bit(_) | ColumnType::Varbit(_) => Some(ColumnType::Varbit(None)),
        t if element_of(t).is_some() => Some(crate::composed::array_type(t)),
        _ => None,
    }
}

/// The type `||` returns over its sides, as PostgreSQL resolves its forms: beside a literal, a
/// type's own form with the literal read in that type before any row, else text's; two arrays, or an
/// array and an element, in the array of their common type; two of one type's own form, that form;
/// a string beside anything else, text's. `"char"` casts to text unasked, so where text's form and a
/// string's both take it PostgreSQL cannot choose. Refused where no form takes them; none where a
/// side is not resolved.
fn concatenated(
    a: Option<ColumnType>,
    b: Option<ColumnType>,
    left: &Expr,
    right: &Expr,
) -> Result<Option<ColumnType>> {
    use ColumnType as C;
    let (l, r) = (untyped(left), untyped(right));
    if (a.is_none() && l.is_none()) || (b.is_none() && r.is_none()) {
        return Ok(None);
    }
    let refused = |why: &str| {
        Err(Error::new(format!(
            "operator {why}: {} || {}{}",
            arg_name(a),
            arg_name(b),
            super::ops::hint_of(why, crate::error::NO_OPERATOR)
        )))
    };
    Ok(Some(match (a, b) {
        (Some(t), None) | (None, Some(t)) => {
            if t == C::Char {
                return refused("is not unique");
            }
            match concat_form(t) {
                Some(form) => {
                    if let Some(Some(s)) = l.or(r) {
                        super::pipeline::part_of(
                            form,
                            super::pipeline::Value::Text(s.to_string()),
                            true,
                        )?;
                    }
                    form
                }
                None => C::Text(None),
            }
        }
        (Some(x), Some(y)) => {
            let (ex, ey) = (element_of(x), element_of(y));
            if ex.is_some() || ey.is_some() {
                return match common_type(&[ex.or(a), ey.or(b)], "||") {
                    Ok(t) => Ok(Some(array_of(t))),
                    Err(_) => refused("does not exist"),
                };
            }
            let string = |t: ColumnType| t.is_string() || t == C::Char;
            match (concat_form(x), concat_form(y)) {
                (Some(p), Some(q)) if p == q => p,
                _ if string(x) && string(y) && (x == C::Char || y == C::Char) => {
                    return refused("is not unique")
                }
                _ if string(x) || string(y) => C::Text(None),
                _ => return refused("does not exist"),
            }
        }
        (None, None) => C::Text(None),
    }))
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
    if super::json::AGGREGATES.contains(&name) {
        return Ok(super::json::typed(name, args)?.flatten());
    }
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
            ColumnType::Float4 | ColumnType::Float8 | ColumnType::Money(_) => ty,
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
            | ColumnType::Varbit(_)
            | ColumnType::Macaddr
            | ColumnType::Macaddr8
            | ColumnType::Geometric(_)
            | ColumnType::Textual(
                crate::textual::Textual::Json
                | crate::textual::Textual::Jsonb
                | crate::textual::Textual::Jsonpath
                | crate::textual::Textual::Xml
                | crate::textual::Textual::Tsvector
                | crate::textual::Textual::Tsquery
                | crate::textual::Textual::Aclitem,
            ) => return refused(),
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
        // random's forms of two integers, two bigints and two numerics, found as gcd's are.
        "random" if args.len() == 2 => match a.as_slice() {
            [Arg::Unknown, Arg::Unknown]
            | [Arg::Int(C::Int2), Arg::Int(C::Int2) | Arg::Unknown]
            | [Arg::Unknown, Arg::Int(C::Int2)] => {
                return Err(Error::new(format!(
                    "function {name}({}, {}) is not unique{FUNCTION_NOT_UNIQUE}",
                    arg_name(args[0]),
                    arg_name(args[1])
                )))
            }
            [Arg::Int(x), Arg::Int(y)] => Some(Some(wider(*x, *y).max_int4())),
            [Arg::Int(x), Arg::Unknown] | [Arg::Unknown, Arg::Int(x)] => Some(Some(x.max_int4())),
            [Arg::Numeric, Arg::Int(_) | Arg::Numeric | Arg::Unknown]
            | [Arg::Int(_) | Arg::Unknown, Arg::Numeric] => Some(Some(NUMERIC)),
            _ => return refused(),
        },
        "random_normal" if args.len() <= 2 => return doubles(args.len()),
        "setseed" if args.len() == 1 && a.iter().all(numberish) => {
            Some(Some(C::Internal(crate::textual::Internal::Void)))
        }
        "random" | "random_normal" | "setseed" => return refused(),
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
                return Err(Error::new(format!(
                    "function mod(unknown, unknown) is not unique{FUNCTION_NOT_UNIQUE}"
                )))
            }
            _ => return refused(),
        },
        "gcd" | "lcm" => match a.as_slice() {
            [Arg::Int(C::Int2), Arg::Int(C::Int2)] => {
                return Err(Error::new(format!(
                    "function {name}(smallint, smallint) is not unique{FUNCTION_NOT_UNIQUE}"
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
                    "function {name}({}) is not unique{FUNCTION_NOT_UNIQUE}",
                    arg_name(args[0])
                )))
            }
            [Arg::Int(_)] => Some(Some(C::Text(None))),
            _ => return refused(),
        },
        "num_nulls" | "num_nonnulls" => Some(Some(C::Int4)),
        // generate_series of numbers: numeric where an argument is one, else the widest integer;
        // its forms of time are time's.
        "generate_series" if matches!(args.len(), 2 | 3) && !args.iter().any(|t| of_time(*t)) => {
            if !a.iter().all(|x| matches!(x, Arg::Int(_) | Arg::Numeric | Arg::Unknown)) {
                return refused();
            }
            if a.contains(&Arg::Numeric) {
                return Ok(Some(Some(NUMERIC)));
            }
            let widest = a
                .iter()
                .filter_map(|x| match x {
                    Arg::Int(t) => Some(*t),
                    _ => None,
                })
                .reduce(wider);
            match widest {
                Some(t) if t != C::Int2 => Some(Some(t)),
                _ => {
                    return Err(Error::new(format!(
                        "function {name}({}) is not unique{FUNCTION_NOT_UNIQUE}",
                        args.iter().map(|t| arg_name(*t)).collect::<Vec<_>>().join(", ")
                    )))
                }
            }
        }
        _ => None,
    })
}

/// Whether a type is a date, a timestamp, a timestamptz or an interval.
fn of_time(t: Option<ColumnType>) -> bool {
    use ColumnType as C;
    matches!(
        t.map(super::clock::base),
        Some(C::Date | C::Timestamp(_) | C::Timestamptz | C::Interval { .. })
    )
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
            "function {name}({}) is not unique{FUNCTION_NOT_UNIQUE}",
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
        ("pg_timezone_names", []) => super::clock::zones_record(true),
        ("pg_timezone_abbrevs" | "pg_timezone_abbrevs_abbrevs" | "pg_timezone_abbrevs_zone", []) => {
            super::clock::zones_record(false)
        }
        ("current_date", []) => C::Date,
        ("localtimestamp", []) => C::Timestamp(None),
        ("current_time", []) => C::Timetz(None),
        ("localtime", []) => C::Time(None),
        // With a precision.
        ("current_timestamp", [_]) => C::Timestamptz,
        ("localtimestamp", [_]) => C::Timestamp(None),
        ("current_time", [_]) => C::Timetz(None),
        ("localtime", [_]) => C::Time(None),
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
        // generate_series of instants: a timestamp where its bounds are more timestamps than
        // timestamptzs and it names no zone, else a timestamptz, a date read as one.
        ("generate_series", [s, e, step, zone @ ..]) if zone.len() <= 1 => {
            let named = zone.iter().all(|z| {
                matches!(
                    z.map(super::clock::base),
                    None | Some(C::Text(_) | C::Bpchar(_) | C::Name)
                )
            });
            if !instant(*s)
                || !instant(*e)
                || !matches!(base(*step), Some(C::Interval { .. }) | None)
                || !named
            {
                return refused();
            }
            let count = |want: fn(&ColumnType) -> bool| {
                [*s, *e]
                    .iter()
                    .filter(|t| base(**t).as_ref().is_some_and(want))
                    .count()
            };
            let plain = count(|t| matches!(t, C::Timestamp(_)));
            let zoned = count(|t| matches!(t, C::Timestamptz));
            match zone.is_empty() && plain > zoned {
                true => C::Timestamp(None),
                false => C::Timestamptz,
            }
        }
        ("generate_series", _) => return refused(),
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
        ("range_agg", [t]) => match t {
            Some(t) if multirange_range(*t).is_some() => Some(*t),
            Some(t) if crate::composed::range_of(*t).is_some() => {
                crate::composed::multirange_of(*t)
            }
            None => None,
            Some(_) => return refused(),
        },
        ("range_intersect_agg", [t]) => match t {
            Some(t) if crate::composed::range_of(*t).is_some() => Some(*t),
            None => None,
            Some(_) => return refused(),
        },
        // An array of its argument's type, one more dimension of an array's.
        ("array_agg", [t]) => match t {
            Some(t) if element_of(*t).is_some() => Some(*t),
            Some(t) => Some(array_of(*t)),
            None => None,
        },
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
        // Of an array of fractions, an array.
        ("percentile_disc", [Some(f)], [t]) if element_of(*f).is_some() => t.map(array_of),
        ("percentile_disc", [_], [t]) => *t,
        ("percentile_cont", [f], [t]) => {
            let one = match t.map(super::clock::base) {
                Some(C::Interval { .. }) => Some(super::calls::full_interval()),
                Some(t) if t.is_integer() || t.is_numeric() || t.is_float() => Some(C::Float8),
                None => None,
                Some(_) => return refused(),
            };
            match f {
                Some(f) if element_of(*f).is_some() => one.map(array_of),
                _ => one,
            }
        }
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
    if matches!(name, "upper" | "lower") {
        return match args {
            [None] => Ok(Some(Some(C::Text(None)))),
            [Some(t)] if t.is_string() || *t == C::Char => Ok(Some(Some(C::Text(None)))),
            _ => Err(no_form(name, args)),
        };
    }
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
    // Each form, a letter an argument: t a string, i an integer.
    let forms: &[&str] = match name {
        "regexp_count" => &["tt", "tti", "ttit"],
        "regexp_instr" => &["tt", "tti", "ttii", "ttiii", "ttiiit", "ttiiiti"],
        "regexp_like"
        | "regexp_match"
        | "regexp_split_to_array"
        | "regexp_matches"
        | "regexp_split_to_table" => &["tt", "ttt"],
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
        "regexp_match" | "regexp_split_to_array" | "regexp_matches" => super::calls::text_array(),
        _ => C::Text(None),
    })))
}

/// The type a function of arrays returns, or its refusal where it has no form for its arguments;
/// none where the name is no function of arrays the snail answers.
pub(super) fn arrays(
    name: &str,
    args: &[Option<ColumnType>],
) -> Result<Option<Option<ColumnType>>> {
    let array = |t: &Option<ColumnType>| t.and_then(element_of);
    Ok(Some(match (name, args) {
        ("unnest", [None]) => None,
        ("unnest", [t]) => match (array(t), t.and_then(multirange_range)) {
            (Some(e), _) | (_, Some(e)) => Some(e),
            _ => return Err(no_form(name, args)),
        },
        ("range_minus_multi" | "multirange_minus_multi", [a, _]) => *a,
        _ => return Ok(None),
    }))
}

/// A multirange type's range type.
pub(super) fn multirange_range(ty: ColumnType) -> Option<ColumnType> {
    match ty {
        ColumnType::Composed(r) => match r.def() {
            crate::composed::Composed::Multirange { range, .. } => Some(*range),
            _ => None,
        },
        _ => None,
    }
}

/// An `=` of two values, refused as PostgreSQL's parser refuses it where either is void.
fn void_compared(a: Option<ColumnType>, b: Option<ColumnType>) -> Result<()> {
    if [a, b].contains(&Some(super::prng::VOID)) {
        statistics_operator("=", a, b)?;
    }
    Ok(())
}

/// The type a comparison, a pattern or `||` returns beside a value of the planner's statistics
/// types, as PostgreSQL resolves it: they have no operator of their own, and each casts implicitly
/// to text, through its output, and to bytea, as its bytes. Beside bytea, bytea's form; beside a
/// string or an untyped literal, text's for a comparison or a pattern, `||` no unique one; beside
/// another of them, no unique form; beside any other type, none. Their kin that hold only NULL
/// cast to neither, and have no comparison, nor has void, what a function of no value answers.
/// None where neither side is one, or the operator is none of these.
pub(super) fn statistics_operator(
    op: &str,
    a: Option<ColumnType>,
    b: Option<ColumnType>,
) -> Result<Option<ColumnType>> {
    let statistics = |t: Option<ColumnType>| match t {
        Some(ColumnType::Internal(i)) if i.is_bytes() || i == crate::textual::Internal::Void => {
            Some(i)
        }
        _ => None,
    };
    let (x, y) = (statistics(a), statistics(b));
    let Some(s) = x.or(y) else {
        return Ok(None);
    };
    let compare = ["=", "<>", "<", "<=", ">", ">="].contains(&op);
    let like = ["~~", "!~~"].contains(&op);
    let pattern = like || ["~~*", "!~~*", "~", "~*", "!~", "!~*"].contains(&op);
    let concat = op == "||";
    let refused = |why: &str| {
        Err(Error::new(format!(
            "operator {why}: {} {op} {}{}",
            arg_name(a),
            arg_name(b),
            super::ops::hint_of(why, crate::error::NO_OPERATOR)
        )))
    };
    if !s.casts() {
        return match compare || pattern {
            true => refused("does not exist"),
            false => Ok(None),
        };
    }
    if !compare && !pattern && !concat {
        return Ok(None);
    }
    if x.is_some() && y.is_some() {
        return refused("is not unique");
    }
    match if x.is_some() { b } else { a } {
        Some(ColumnType::Bytes) if compare || like => Ok(Some(ColumnType::Boolean)),
        Some(ColumnType::Bytes) if concat => Ok(Some(ColumnType::Bytes)),
        o if o.is_none_or(|t| t.is_string()) => {
            if concat {
                return refused("is not unique");
            }
            written(Some(ColumnType::Internal(s)))?;
            Ok(Some(ColumnType::Boolean))
        }
        _ => refused("does not exist"),
    }
}

/// The functions that write an argument of any type as its type's output writes it.
const WRITING: [&str; 22] = [
    "concat",
    "concat_ws",
    "format",
    "string_agg",
    "to_json",
    "to_jsonb",
    "json_build_array",
    "json_build_object",
    "jsonb_build_array",
    "jsonb_build_object",
    "json_agg",
    "jsonb_agg",
    "json_agg_strict",
    "jsonb_agg_strict",
    "json_object_agg",
    "jsonb_object_agg",
    "json_object_agg_strict",
    "jsonb_object_agg_strict",
    "json_object_agg_unique",
    "jsonb_object_agg_unique",
    "json_object_agg_unique_strict",
    "jsonb_object_agg_unique_strict",
];

/// Refused where an expression writes a value of a type as its output does, and the snail does not
/// write that type's output as PostgreSQL's.
fn written(t: Option<ColumnType>) -> Result<()> {
    match t {
        Some(ColumnType::Internal(i)) => i.unwritten().map_or(Ok(()), Err),
        _ => Ok(()),
    }
}

/// A type without its modifier, as PostgreSQL's polymorphic forms match it.
pub(super) fn unmodified(ty: ColumnType) -> ColumnType {
    match super::clock::base(ty) {
        t if t.is_numeric() => NUMERIC,
        ColumnType::Text(Some(_)) => ColumnType::Text(Some(ColumnType::VARCHAR)),
        ColumnType::Bpchar(_) => ColumnType::Bpchar(None),
        ColumnType::Varbit(_) => ColumnType::Varbit(None),
        t => t,
    }
}

/// The type a range's or a multirange's operator returns over its two sides, as PostgreSQL resolves
/// its polymorphic forms: `@>`, `<@`, `&&`, `<<`, `>>`, `&<`, `&>` and `-|-` beside a range or a
/// multirange over the same range type; `@>` with an element of the subtype on its right, `<@` on
/// its left; `+`, `*`, `-` and the comparisons beside the same type; an untyped literal or NULL as
/// the other side's type. Refused where PostgreSQL finds no operator; none where neither side is a
/// range or a multirange, or the operator is none of theirs.
pub(super) fn range_operator(
    op: &str,
    a: Option<ColumnType>,
    b: Option<ColumnType>,
) -> Result<Option<ColumnType>> {
    use crate::composed::Composed;
    // The range type a side stands on, and whether it is a multirange of it.
    let ranged = |t: ColumnType| match t {
        ColumnType::Composed(r) => match r.def() {
            Composed::Range { .. } => Some((t, false)),
            Composed::Multirange { range, .. } => Some((*range, true)),
            _ => None,
        },
        _ => None,
    };
    let (x, y) = (a.and_then(ranged), b.and_then(ranged));
    if x.is_none() && y.is_none() {
        return Ok(None);
    }
    let truth = ["@>", "<@", "&&", "<<", ">>", "&<", "&>", "-|-"].contains(&op);
    let same = ["+", "*", "-", "=", "<>", "<", "<=", ">", ">="].contains(&op);
    if !truth && !same {
        return Ok(None);
    }
    let subtype = |r: ColumnType| crate::composed::range_of(r).map(|(s, _)| unmodified(s));
    let fits = match (a, b, x, y) {
        (None, _, _, _) | (_, None, _, _) => true,
        (_, _, Some(rx), Some(ry)) if truth => rx.0 == ry.0,
        (Some(s), Some(t), Some(_), Some(_)) => s == t,
        (_, Some(e), Some(rx), None) => op == "@>" && subtype(rx.0) == Some(unmodified(e)),
        (Some(e), _, None, Some(ry)) => op == "<@" && subtype(ry.0) == Some(unmodified(e)),
        _ => false,
    };
    if !fits {
        return Err(no_operator(&arg_name(a), op, &arg_name(b)));
    }
    let comparison = ["=", "<>", "<", "<=", ">", ">="].contains(&op);
    Ok(Some(if truth || comparison {
        ColumnType::Boolean
    } else {
        a.or(b).unwrap()
    }))
}

/// The keywords PostgreSQL's grammar reads as a type's, or as a clause's, never as a function's
/// name where they are written unquoted: the column-name keywords that have no call syntax of their
/// own.
const TYPE_KEYWORDS: [&str; 26] = [
    "between",
    "bigint",
    "bit",
    "boolean",
    "char",
    "character",
    "dec",
    "decimal",
    "float",
    "inout",
    "int",
    "integer",
    "interval",
    "national",
    "nchar",
    "none",
    "numeric",
    "out",
    "precision",
    "real",
    "setof",
    "smallint",
    "time",
    "timestamp",
    "values",
    "varchar",
];

/// The refusal of a call named by a type's keyword written unquoted (`timestamp(d, t)`), which
/// PostgreSQL's grammar takes for no function; quoted (`"timestamp"(d, t)`) it is one.
fn type_keyword(f: &sqlparser::ast::Function) -> Option<Error> {
    let [sqlparser::ast::ObjectNamePart::Identifier(i)] = f.name.0.as_slice() else {
        return None;
    };
    let name = i.value.to_lowercase();
    (i.quote_style.is_none() && TYPE_KEYWORDS.contains(&name.as_str())).then(|| {
        Error::new(format!(
            "syntax error at or near \"(\": {name}, written unquoted, is a type's keyword and names \
             no function"
        ))
    })
}

/// A type's name as PostgreSQL's format_type_be writes it in an error: without its modifier.
pub(crate) fn type_name(t: ColumnType) -> String {
    let element = match t {
        ColumnType::Composed(r) => match r.def() {
            crate::composed::Composed::Array {
                element,
                domain: None,
            } => Some(*element),
            _ => None,
        },
        _ => None,
    };
    match (t, element) {
        (ColumnType::Bit(_), _) => "bit".into(),
        (_, Some(e)) => format!("{}[]", type_name(e)),
        _ => unmodified(t).pg_name(),
    }
}

/// A comparison typed as PostgreSQL's parser chooses its operator: a quoted literal beside a number
/// or a truth read by that type's input, and a string compared with a number, which PostgreSQL has
/// no operator for and casts neither to the other of unasked, refused in its words; each before any
/// row. Any other two types are left to the comparison.
fn compared(
    op: &str,
    (a, left): (Option<ColumnType>, &Expr),
    (b, right): (Option<ColumnType>, &Expr),
) -> Result<()> {
    let number = |t: ColumnType| t.is_integer() || t.is_numeric() || t.is_float();
    // A quoted literal beside a number or a truth is read by that type's input as the operator is
    // chosen, each side in turn.
    for (t, other) in [(a, right), (b, left)] {
        if let (Some(t), Some(Some(s))) = (t, untyped(other)) {
            if number(t) || t == ColumnType::Boolean {
                super::pipeline::part_of(
                    unmodified(t),
                    super::pipeline::Value::Text(s.to_string()),
                    true,
                )?;
            }
        }
    }
    match (a, b) {
        (Some(x), Some(y)) if x.is_string() && number(y) || number(x) && y.is_string() => {
            Err(Error::new(format!(
                "operator does not exist: {} {op} {}{NO_OPERATOR}",
                type_name(x),
                type_name(y)
            )))
        }
        _ => Ok(()),
    }
}

/// A CASE's type, as transformCaseExpr gives it: its arms' as select_common_type finds it, each
/// quoted literal among them read in it before any row and refused here where it reads as none; its
/// modifier kept only where it has an ELSE and every arm is of that type and modifier, as exprTypmod
/// reads a CASE. None where no arm is typed; the first typed arm's where the arms' types are not
/// matched here.
fn case_type(arms: &[(&Expr, Option<ColumnType>)], has_else: bool) -> Result<Option<ColumnType>> {
    let types: Vec<Option<ColumnType>> = arms.iter().map(|(_, t)| *t).collect();
    let Some(first) = types.iter().flatten().next().copied() else {
        return Ok(None);
    };
    let Ok(t) = common_type(&types, "CASE") else {
        return Ok(Some(first));
    };
    let t = match has_else && types.iter().all(|a| *a == Some(t)) {
        true => t,
        false => unmodified(t),
    };
    if !t.is_string() && !matches!(t, ColumnType::Composed(_)) {
        for (x, ty) in arms {
            if let (None, Some(Some(s))) = (ty, untyped(x)) {
                super::pipeline::part_of(t, super::pipeline::Value::Text(s.to_string()), true)?;
            }
        }
    }
    Ok(Some(t))
}

/// The type COALESCE, LEAST, GREATEST and NULLIF take, as PostgreSQL's parser types them: the
/// first three their arguments' as select_common_type finds it, its modifier kept only where every
/// argument is of that type and modifier, as exprTypmod reads them; NULLIF the type of the left side
/// of the = it compares by, its first argument's, a number promoted to the other's where it is wider
/// in kind. A quoted literal among them is read in that type, refused here where it reads as none.
/// None for any other call.
fn chosen(name: &str, args: &[&Expr], types: &[Option<ColumnType>]) -> Result<Option<ColumnType>> {
    // NULLIF compares by `=`, which void has none of.
    if let ("nullif", [a, b]) = (name, types) {
        void_compared(*a, *b)?;
    }
    let ty = match (name, types) {
        ("coalesce" | "least" | "greatest", _) => {
            let t = common_type(types, &name.to_uppercase())?;
            match types.iter().all(|a| *a == Some(t)) {
                true => t,
                false => unmodified(t),
            }
        }
        ("nullif", [a, b]) => match (*a, *b) {
            (Some(x), Some(y))
                if x == y
                    || (x.is_integer() && y.is_integer())
                    || (x.is_float() && y.is_float()) =>
            {
                x
            }
            (Some(x), None) => x,
            (None, Some(y)) => y,
            (None, None) => ColumnType::Text(None),
            (Some(x), Some(y)) => common_type(&[Some(x), Some(y)], "NULLIF").map_err(|_| {
                Error::new(format!(
                    "operator does not exist: {} = {}{NO_OPERATOR}",
                    arg_name(Some(x)),
                    arg_name(Some(y))
                ))
            })?,
        },
        _ => return Ok(None),
    };
    for (a, t) in args.iter().zip(types) {
        if let (None, Expr::Value(v)) = (t, a) {
            if let SqlValue::SingleQuotedString(s) = &v.value {
                super::pipeline::part_of(ty, super::pipeline::Value::Text(s.clone()), true)?;
            }
        }
    }
    Ok(Some(ty))
}
