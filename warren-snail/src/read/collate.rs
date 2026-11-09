//! The collation each of a step's expressions takes, as PostgreSQL's parser derives it
//! (`parse_collate.c`): a column's, a COLLATE's, a literal's default, and two different implicit
//! ones in conflict; and the collation each function or operator is called under. Each relation a
//! step makes carries the collation of each of its columns, by its place.

use super::typed::Types;
use crate::catalog::{object_name, ColumnType};
use crate::{Error, Result};
use sqlparser::ast::{
    BinaryOperator, Expr, FunctionArg, FunctionArgExpr, FunctionArgumentClause, FunctionArguments,
    Value as SqlValue,
};
use std::collections::HashMap;

/// The database's default collation, as a column that names none carries it.
pub(super) const DEFAULT: &str = "default";

/// The collation a relation's column carries: none, for a type no collation orders; one, by its
/// name; or one two implicit collations in the expression that made it left unsettled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Carried {
    None,
    Named(String),
    Unsettled,
}

/// How an expression's collation was derived, weakest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Strength {
    None,
    Implicit,
    Conflict,
    Explicit,
}

/// An expression's collation as it is derived: its strength, the collation, and the second of two
/// in conflict; unsettled where it reads a column that carries a collation no rule settled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Derived {
    strength: Strength,
    collation: Option<String>,
    other: Option<String>,
    unsettled: bool,
}

impl Derived {
    fn none() -> Derived {
        Derived {
            strength: Strength::None,
            collation: None,
            other: None,
            unsettled: false,
        }
    }

    fn of(strength: Strength, collation: &str) -> Derived {
        Derived {
            strength,
            collation: Some(collation.to_string()),
            ..Derived::none()
        }
    }

    /// Merges a sibling's collation into this one, as `merge_collation_state` does: the stronger
    /// wins; of two implicit ones, a collation other than the default wins over it, and two others
    /// conflict; two explicit ones that differ are refused.
    fn merge(&mut self, d: &Derived) -> Result<()> {
        self.unsettled |= d.unsettled;
        if d.strength > self.strength {
            let unsettled = self.unsettled;
            *self = d.clone();
            self.unsettled = unsettled;
            return Ok(());
        }
        if d.strength < self.strength || d.collation == self.collation {
            return Ok(());
        }
        match d.strength {
            Strength::Implicit if self.collation.as_deref() == Some(DEFAULT) => {
                self.collation = d.collation.clone();
            }
            Strength::Implicit if d.collation.as_deref() != Some(DEFAULT) => {
                self.strength = Strength::Conflict;
                self.other = d.collation.clone();
            }
            Strength::Explicit => {
                return Err(Error::new(format!(
                    "collation mismatch between explicit collations \"{}\" and \"{}\"",
                    self.collation.as_deref().unwrap_or_default(),
                    d.collation.as_deref().unwrap_or_default()
                )))
            }
            _ => {}
        }
        Ok(())
    }

    /// The collation a function or an operator that needs one is called under, refused in
    /// PostgreSQL's words for `what` where none is settled; none where its inputs are of no
    /// collatable type.
    pub(super) fn settled(&self, what: &str) -> Result<Option<&str>> {
        match self.strength {
            Strength::Conflict => Err(undetermined(what)),
            Strength::None if self.unsettled => Err(undetermined(what)),
            Strength::None => Ok(None),
            _ => Ok(self.collation.as_deref()),
        }
    }

    /// The collation a key a step sorts or groups by is compared under, refused as PostgreSQL's
    /// parser refuses two implicit ones in conflict there.
    pub(super) fn keyed(&self) -> Result<Option<&str>> {
        match self.strength {
            Strength::Conflict => Err(Error::new(format!(
                "collation mismatch between implicit collations \"{}\" and \"{}\"",
                self.collation.as_deref().unwrap_or_default(),
                self.other.as_deref().unwrap_or_default()
            ))),
            _ => self.settled("string comparison"),
        }
    }

    /// The collation a call over it is made under: its own, implicit or explicit; none where two
    /// implicit ones conflict or it carries none.
    pub(super) fn under(&self) -> Option<&str> {
        match self.strength {
            Strength::Implicit | Strength::Explicit => self.collation.as_deref(),
            Strength::None | Strength::Conflict => None,
        }
    }

    /// The collations it names: its own, and the second of two in conflict.
    pub(super) fn names(&self) -> impl Iterator<Item = &str> {
        self.collation
            .as_deref()
            .into_iter()
            .chain(self.other.as_deref())
    }

    /// The collation the column a step makes of it carries, where its type is collatable.
    pub(super) fn carried(&self, collatable: bool) -> Carried {
        match (self.strength, &self.collation) {
            (Strength::Implicit | Strength::Explicit, Some(c)) => Carried::Named(c.clone()),
            (Strength::Conflict, _) => Carried::Unsettled,
            _ if self.unsettled && collatable => Carried::Unsettled,
            _ => Carried::None,
        }
    }
}

/// PostgreSQL's refusal of a function or operator called under no collation.
fn undetermined(what: &str) -> Error {
    Error::new(format!(
        "could not determine which collation to use for {what}"
    ))
}

/// A collation's name as a column or a COLLATE writes it, the schema PostgreSQL's own live in left
/// off.
pub(super) fn named(name: &str) -> String {
    name.strip_prefix("pg_catalog.").unwrap_or(name).to_string()
}

/// Whether a type's values are ordered under a collation: a string's, and an array's of strings.
pub(super) fn collatable(ty: ColumnType) -> bool {
    ty.is_string() || super::typed::element_of(ty).is_some_and(collatable)
}

/// The collation a type gives a value of it made from no collated input: name's C, any other
/// string's the default.
fn type_collation(ty: ColumnType) -> &'static str {
    match ty {
        ColumnType::Name => "C",
        _ => DEFAULT,
    }
}

/// Each of a step's expressions with the collation it takes and the one it is called under, by its
/// place in the text, derived over the columns the step reads.
pub(super) struct Deriving<'a> {
    /// The columns the step reads, each under its qualifier, with its collation.
    pub columns: &'a [(Option<String>, String, Carried)],
    pub types: &'a Types,
    pub nodes: HashMap<*const Expr, (Derived, Derived)>,
}

impl Deriving<'_> {
    /// An expression's collation, each sub-expression's recorded with the collation its call takes.
    pub(super) fn derive(&mut self, e: &Expr) -> Result<Derived> {
        let (out, input) = self.node(e)?;
        self.nodes.insert(e as *const Expr, (out.clone(), input));
        Ok(out)
    }

    /// The merged collation of expressions read together.
    fn merged(&mut self, es: &[&Expr]) -> Result<Derived> {
        let mut d = Derived::none();
        for e in es {
            let x = self.derive(e)?;
            d.merge(&x)?;
        }
        Ok(d)
    }

    /// What an expression makes of the collation of its inputs: theirs where its type is
    /// collatable and they carry one, else its type's own; none for a type no collation orders. An
    /// expression of a type the step does not resolve takes its inputs'.
    fn made(&self, e: &Expr, input: &Derived) -> Derived {
        match self.types.get(&(e as *const Expr)) {
            Some(&ty) if !collatable(ty) => Derived::none(),
            Some(&ty) if input.strength == Strength::None => {
                Derived::of(Strength::Implicit, type_collation(ty))
            }
            _ => input.clone(),
        }
    }

    fn column(&self, e: &Expr) -> Derived {
        let Some((q, n)) = super::pipeline::column_of(e) else {
            return Derived::none();
        };
        let found = super::pipeline::naming(
            self.columns
                .iter()
                .map(|(cq, cn, _)| (cq.as_deref(), cn.as_str())),
            q.as_deref(),
            &n,
        );
        let carried = match found.as_slice() {
            [i] => self.columns.get(*i).map(|c| &c.2),
            _ => None,
        };
        match carried {
            Some(Carried::Named(c)) => Derived::of(Strength::Implicit, c),
            Some(Carried::Unsettled) => Derived {
                unsettled: true,
                ..Derived::none()
            },
            _ => Derived::none(),
        }
    }

    /// The collation an expression takes, and the one its call or operator is called under.
    fn node(&mut self, e: &Expr) -> Result<(Derived, Derived)> {
        let quoted = |x: &Expr| {
            matches!(x, Expr::Value(v) if matches!(v.value,
                SqlValue::SingleQuotedString(_)
                    | SqlValue::EscapedStringLiteral(_)
                    | SqlValue::DollarQuotedString(_)
                    | SqlValue::NationalStringLiteral(_)))
        };
        Ok(match e {
            Expr::Identifier(_) | Expr::CompoundIdentifier(_) => {
                let d = self.column(e);
                (d.clone(), d)
            }
            Expr::Nested(x) => {
                let d = self.derive(x)?;
                (d.clone(), d)
            }
            Expr::Collate { expr, collation } => {
                let inner = self.derive(expr)?;
                (
                    Derived::of(Strength::Explicit, &named(&object_name(collation))),
                    inner,
                )
            }
            Expr::Value(v) => match v.value {
                SqlValue::Placeholder(_) => {
                    (Derived::of(Strength::Implicit, DEFAULT), Derived::none())
                }
                _ if quoted(e) => (Derived::of(Strength::Implicit, DEFAULT), Derived::none()),
                _ => (Derived::none(), Derived::none()),
            },
            Expr::TypedString(t) => {
                let d = match crate::catalog::held_type(&t.data_type) {
                    Ok(ty) if collatable(ty) => Derived::of(Strength::Implicit, type_collation(ty)),
                    _ => Derived::none(),
                };
                (d, Derived::none())
            }
            // A quoted literal cast is a constant of its type, its type's collation its own.
            Expr::Cast {
                expr, data_type, ..
            } => {
                let inner = self.derive(expr)?;
                let ty = crate::catalog::held_type(data_type).ok();
                let out = match ty {
                    Some(ty) if collatable(ty) && quoted(expr) => {
                        Derived::of(Strength::Implicit, type_collation(ty))
                    }
                    Some(ty) if collatable(ty) => self.made(e, &inner),
                    Some(_) => Derived::none(),
                    None => self.made(e, &inner),
                };
                (out, inner)
            }
            Expr::BinaryOp { left, op, right } => {
                let input = self.merged(&[left, right])?;
                let out = match op {
                    BinaryOperator::StringConcat => self.made(e, &input),
                    BinaryOperator::Eq
                    | BinaryOperator::NotEq
                    | BinaryOperator::Lt
                    | BinaryOperator::LtEq
                    | BinaryOperator::Gt
                    | BinaryOperator::GtEq
                    | BinaryOperator::And
                    | BinaryOperator::Or
                    | BinaryOperator::PGRegexMatch
                    | BinaryOperator::PGRegexIMatch
                    | BinaryOperator::PGRegexNotMatch
                    | BinaryOperator::PGRegexNotIMatch => Derived::none(),
                    _ => match self.types.get(&(e as *const Expr)) {
                        Some(&ty) if collatable(ty) => self.made(e, &input),
                        _ => Derived::none(),
                    },
                };
                (out, input)
            }
            Expr::Like {
                expr,
                pattern,
                escape_char,
                ..
            }
            | Expr::ILike {
                expr,
                pattern,
                escape_char,
                ..
            } => {
                let mut input = self.merged(&[expr, pattern])?;
                if let Some(c) = escape_char {
                    let d = self.derive(c)?;
                    input.merge(&d)?;
                }
                (Derived::none(), input)
            }
            Expr::SimilarTo {
                expr,
                pattern,
                escape_char,
                ..
            } => {
                let mut input = self.merged(&[expr, pattern])?;
                if let Some(c) = escape_char {
                    let d = self.derive(c)?;
                    input.merge(&d)?;
                }
                (Derived::none(), input)
            }
            Expr::InList { expr, list, .. } => {
                let mut input = self.derive(expr)?;
                for x in list {
                    let d = self.derive(x)?;
                    input.merge(&d)?;
                }
                (Derived::none(), input)
            }
            // Its two comparisons' sides read together.
            Expr::Between {
                expr, low, high, ..
            } => (Derived::none(), self.merged(&[expr, low, high])?),
            Expr::IsDistinctFrom(a, b) | Expr::IsNotDistinctFrom(a, b) => {
                (Derived::none(), self.merged(&[a, b])?)
            }
            Expr::UnaryOp { expr, .. } => {
                let input = self.derive(expr)?;
                (self.made(e, &input), input)
            }
            Expr::IsNull(x)
            | Expr::IsNotNull(x)
            | Expr::IsTrue(x)
            | Expr::IsNotTrue(x)
            | Expr::IsFalse(x)
            | Expr::IsNotFalse(x)
            | Expr::IsUnknown(x)
            | Expr::IsNotUnknown(x) => {
                self.derive(x)?;
                (Derived::none(), Derived::none())
            }
            // A CASE's result is its results' collation; its conditions are truths, and its operand
            // is read on its own.
            Expr::Case {
                operand,
                conditions,
                else_result,
                ..
            } => {
                if let Some(o) = operand {
                    self.derive(o)?;
                }
                let mut out = Derived::none();
                for c in conditions {
                    self.derive(&c.condition)?;
                    let d = self.derive(&c.result)?;
                    out.merge(&d)?;
                }
                if let Some(x) = else_result {
                    let d = self.derive(x)?;
                    out.merge(&d)?;
                }
                (self.made(e, &out), out)
            }
            Expr::Function(f) => self.call(e, f)?,
            // Any other expression's inputs read together, as a call of them.
            other => {
                let parts = children(other);
                let input = self.merged(&parts)?;
                (self.made(e, &input), input)
            }
        })
    }

    /// A call: an aggregate's arguments read together, its ORDER BY and FILTER each on its own; a
    /// window's arguments together, its partitions and order each on its own; any other function's
    /// arguments together.
    fn call(&mut self, e: &Expr, f: &sqlparser::ast::Function) -> Result<(Derived, Derived)> {
        let mut args = Vec::new();
        if let FunctionArguments::List(l) = &f.args {
            for a in &l.args {
                if let FunctionArg::Unnamed(FunctionArgExpr::Expr(x))
                | FunctionArg::Named {
                    arg: FunctionArgExpr::Expr(x),
                    ..
                } = a
                {
                    args.push(x);
                }
            }
            for c in &l.clauses {
                if let FunctionArgumentClause::OrderBy(o) = c {
                    for x in o {
                        self.derive(&x.expr)?.keyed()?;
                    }
                }
            }
        }
        let input = self.merged(&args)?;
        if let Some(w) = &f.filter {
            self.derive(w)?;
        }
        for o in &f.within_group {
            self.derive(&o.expr)?.keyed()?;
        }
        if let Some(sqlparser::ast::WindowType::WindowSpec(spec)) = &f.over {
            for p in &spec.partition_by {
                self.derive(p)?.keyed()?;
            }
            for o in &spec.order_by {
                self.derive(&o.expr)?.keyed()?;
            }
        }
        Ok((self.made(e, &input), input))
    }
}

/// The expressions an expression reads, where it is none `Deriving::node` reads by name.
pub(super) fn children(e: &Expr) -> Vec<&Expr> {
    match e {
        Expr::Extract { expr, .. }
        | Expr::Ceil { expr, .. }
        | Expr::Floor { expr, .. }
        | Expr::AtTimeZone {
            timestamp: expr, ..
        } => vec![expr],
        Expr::Position { expr, r#in } => vec![expr, r#in],
        Expr::Substring {
            expr,
            substring_from,
            substring_for,
            ..
        } => std::iter::once(&**expr)
            .chain(substring_from.as_deref())
            .chain(substring_for.as_deref())
            .collect(),
        Expr::Trim {
            expr, trim_what, ..
        } => std::iter::once(&**expr)
            .chain(trim_what.as_deref())
            .collect(),
        Expr::Overlay {
            expr,
            overlay_what,
            overlay_from,
            overlay_for,
        } => [&**expr, &**overlay_what, &**overlay_from]
            .into_iter()
            .chain(overlay_for.as_deref())
            .collect(),
        Expr::Array(a) => a.elem.iter().collect(),
        Expr::Tuple(xs) => xs.iter().collect(),
        _ => Vec::new(),
    }
}
