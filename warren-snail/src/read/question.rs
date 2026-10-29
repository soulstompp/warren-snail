//! A question in PostgreSQL's SQL, as the snail reads it: the tables it reads and the conditions on
//! each, the keys two tables meet on, and what it returns.

use crate::catalog::{ident, object_name, shape, Catalog, ColumnType, Shape, Table};
use crate::{Error, Result};
use sqlparser::ast::{
    BinaryOperator, Expr, FunctionArg, FunctionArgExpr, FunctionArguments, GroupByExpr,
    JoinConstraint, JoinOperator, OrderByKind, OrderBySort, SelectItem, SetExpr, Statement,
    TableFactor, UnaryOperator, Value,
};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use std::ops::Bound::{self, Excluded, Included, Unbounded};

/// Values from `lo` to `hi`, both included, as their keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Interval {
    pub lo: i128,
    pub hi: i128,
}

impl Interval {
    /// Every value.
    pub const ALL: Interval = Interval {
        lo: i128::MIN,
        hi: i128::MAX,
    };

    pub fn at(v: i128) -> Interval {
        Interval { lo: v, hi: v }
    }

    pub fn holds(&self, v: i128) -> bool {
        self.lo <= v && v <= self.hi
    }
}

/// A condition on one column, or on its month: the values `kept` holds less those `except` names,
/// and the column's NULLs too where `nulls`. `<>` keeps every value but one; IS NOT TRUE keeps
/// false and NULL.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Cond {
    pub shape: Shape,
    pub kept: Interval,
    pub except: Vec<i128>,
    pub nulls: bool,
}

impl Cond {
    pub fn new(shape: Shape, kept: Interval) -> Cond {
        Cond {
            shape,
            kept,
            except: Vec::new(),
            nulls: false,
        }
    }

    /// Whether it keeps a run of values and nothing else, as a range on a level does.
    pub fn is_range(&self) -> bool {
        self.except.is_empty() && !self.nulls
    }

    /// Whether it keeps a row whose value is `v`, none for a NULL.
    pub fn holds(&self, v: Option<i128>) -> bool {
        match v {
            None => self.nulls,
            Some(v) => self.kept.holds(v) && !self.except.contains(&v),
        }
    }

    /// Whether it keeps no value, and no NULL.
    fn is_empty(&self) -> bool {
        !self.nulls
            && (self.kept.lo > self.kept.hi
                || (self.kept.lo == self.kept.hi && self.except.contains(&self.kept.lo)))
    }
}

/// A column of one of the tables a question reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Col {
    pub side: usize,
    pub column: usize,
}

/// A value a question returns or groups by: a column as it is, or the month of an instant on the UTC
/// clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Key {
    Column(Col),
    Month(Col),
}

impl Key {
    pub fn col(self) -> Col {
        match self {
            Key::Column(c) | Key::Month(c) => c,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Item {
    Key(Key),
    Count,
    Sum(Col),
}

#[derive(Debug)]
#[non_exhaustive]
pub enum Select {
    /// Rows, as they are.
    Rows(Vec<(String, Key)>),
    /// One row a group of the keys, or one row in all where there are none.
    Groups {
        keys: Vec<Key>,
        items: Vec<(String, Item)>,
    },
}

/// A question the counts can answer: a count in all, or a count by the month of a column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Countable {
    Total,
    ByMonth(usize),
}

impl Select {
    /// What the counts of a table read alone can answer.
    pub fn countable(&self) -> Option<Countable> {
        let (keys, items) = match self {
            Select::Groups { keys, items } => (keys, items),
            Select::Rows(_) => return None,
        };
        if !items
            .iter()
            .all(|(_, i)| matches!(i, Item::Count | Item::Key(_)))
        {
            return None;
        }
        match keys.as_slice() {
            [] => Some(Countable::Total),
            [Key::Month(c)] => Some(Countable::ByMonth(c.column)),
            _ => None,
        }
    }

    /// The columns the answer reads, the keys first, then the columns summed.
    pub fn inputs(&self) -> Vec<Key> {
        match self {
            Select::Rows(keys) => keys.iter().map(|(_, k)| *k).collect(),
            Select::Groups { keys, items } => {
                let mut out = keys.clone();
                for (_, i) in items {
                    if let Item::Sum(c) = i {
                        if !out[keys.len()..].contains(&Key::Column(*c)) {
                            out.push(Key::Column(*c));
                        }
                    }
                }
                out
            }
        }
    }
}

/// A condition on a text column: its values between two bounds, compared by their bytes, less those
/// `except` names.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct TextCond {
    pub column: usize,
    pub lo: Bound<String>,
    pub hi: Bound<String>,
    pub except: Vec<String>,
}

impl TextCond {
    pub fn holds(&self, v: &str) -> bool {
        if self.except.iter().any(|x| x == v) {
            return false;
        }
        let v = v.as_bytes();
        let above = match &self.lo {
            Included(l) => v >= l.as_bytes(),
            Excluded(l) => v > l.as_bytes(),
            Unbounded => true,
        };
        let below = match &self.hi {
            Included(h) => v <= h.as_bytes(),
            Excluded(h) => v < h.as_bytes(),
            Unbounded => true,
        };
        above && below
    }

    fn is_empty(&self) -> bool {
        match (&self.lo, &self.hi) {
            (Unbounded, _) | (_, Unbounded) => false,
            (Included(l), Included(h)) => {
                l.as_bytes() > h.as_bytes() || (l == h && self.except.contains(l))
            }
            (Included(l) | Excluded(l), Included(h) | Excluded(h)) => l.as_bytes() >= h.as_bytes(),
        }
    }
}

/// The tighter of two bounds on one side: the greater of two lower bounds, the lesser of two upper
/// ones, and at one value the one that excludes it.
fn tighter(a: Bound<String>, b: Bound<String>, lower: bool) -> Bound<String> {
    let take_a = match (&a, &b) {
        (Unbounded, _) => false,
        (_, Unbounded) => true,
        (Included(x) | Excluded(x), Included(y) | Excluded(y)) => {
            if x == y {
                matches!(a, Excluded(_))
            } else {
                (x.as_bytes() > y.as_bytes()) == lower
            }
        }
    };
    if take_a {
        a
    } else {
        b
    }
}

/// One column the answer is ordered by: its place, whether descending, and whether NULLs come first.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct Sort {
    pub at: usize,
    pub desc: bool,
    pub nulls_first: bool,
}

/// One table a question reads, and the conditions on it alone.
#[derive(Debug)]
#[non_exhaustive]
pub struct Side {
    pub table: usize,
    pub conds: Vec<Cond>,
    pub texts: Vec<TextCond>,
    /// Each column a condition keeps to its NULLs, IS NULL (true), or to its values, IS NOT NULL
    /// (false).
    pub nulls: Vec<(usize, bool)>,
    /// Some condition can hold for no value.
    pub empty: bool,
}

impl Side {
    /// Whether a condition of its own narrows what the side reads.
    pub fn conditioned(&self) -> bool {
        !self.conds.is_empty() || !self.texts.is_empty() || !self.nulls.is_empty()
    }
}

#[derive(Debug)]
#[non_exhaustive]
pub struct Question {
    pub sides: Vec<Side>,
    /// The columns two tables meet on: a column of the first, and the column of the second it equals.
    pub join: Vec<(usize, usize)>,
    pub select: Select,
    /// How the answer is ordered: by which of its columns, outermost first.
    pub order: Vec<Sort>,
}

pub fn number(e: &Expr) -> Option<i64> {
    match e {
        Expr::Value(v) => match &v.value {
            Value::Number(s, _) => s.parse().ok(),
            _ => None,
        },
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr,
        } => number(expr).map(|v| -v),
        Expr::Nested(e) => number(e),
        _ => None,
    }
}

/// An instant written with its offset from UTC, as microseconds since 1970-01-01 UTC.
pub fn instant(s: &str) -> Result<i64> {
    for f in [
        "%Y-%m-%d %H:%M:%S%.f%#z",
        "%Y-%m-%d %H:%M:%S%#z",
        "%Y-%m-%dT%H:%M:%S%.f%#z",
        "%Y-%m-%dT%H:%M:%S%#z",
    ] {
        if let Ok(t) = chrono::DateTime::parse_from_str(s, f) {
            return Ok(t.timestamp_micros());
        }
    }
    Err(Error::new(format!(
        "'{s}' is not an instant with its offset from UTC"
    )))
}

/// A truth as PostgreSQL's boolean input reads it: `true`, `yes`, `on` or `1`, `false`, `no`, `off`
/// or `0`, or the start of one that names no other, in either case, the white space around it taken
/// off.
pub fn truth(s: &str) -> Option<bool> {
    let s = s
        .trim_matches(|c| matches!(c, ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r'))
        .to_ascii_lowercase();
    let starts = |word: &str, least: usize| s.len() >= least && word.starts_with(s.as_str());
    match s.as_bytes().first()? {
        b't' if starts("true", 1) => Some(true),
        b'f' if starts("false", 1) => Some(false),
        b'y' if starts("yes", 1) => Some(true),
        b'n' if starts("no", 1) => Some(false),
        b'o' if starts("on", 2) => Some(true),
        b'o' if starts("off", 2) => Some(false),
        b'1' if s.len() == 1 => Some(true),
        b'0' if s.len() == 1 => Some(false),
        _ => None,
    }
}

/// A boolean written out, TRUE or FALSE or a quoted truth, as 1 or 0.
pub(super) fn truth_literal(e: &Expr) -> Result<i64> {
    let wrong = || Error::new(format!("{e} is not a boolean"));
    match e {
        Expr::Value(v) => match &v.value {
            Value::Boolean(b) => Ok(*b as i64),
            Value::SingleQuotedString(s) => truth(s).map(i64::from).ok_or_else(wrong),
            _ => Err(wrong()),
        },
        Expr::Nested(e) => truth_literal(e),
        _ => Err(wrong()),
    }
}

/// A literal compared with a column, or its month, as the key its values are compared by.
fn literal(e: &Expr, s: Shape, table: &Table) -> Result<i128> {
    literal_i64(e, s, table)
        .map(i128::from)
        .or_else(|err| match s {
            Shape::Column(c) if table.columns[c].ty == ColumnType::Uuid => {
                crate::scalars::uuid_parse(&text_literal(e)?)
            }
            _ => Err(err),
        })
}

fn literal_i64(e: &Expr, s: Shape, table: &Table) -> Result<i64> {
    match s {
        Shape::MonthUtc(_) => number(e).ok_or_else(|| Error::new(format!("{e} is not a month"))),
        Shape::Column(c) => match table.columns[c].ty {
            ColumnType::Boolean => truth_literal(e),
            ColumnType::Uuid => Err(Error::new(format!("{e} is not a uuid"))),
            ColumnType::Timestamptz => {
                instant(&text_literal(e).map_err(|_| Error::new(format!("{e} is not an instant")))?)
            }
            ColumnType::Numeric { precision, scale } => {
                let text = match e {
                    Expr::Value(v) => match &v.value {
                        Value::Number(s, _) | Value::SingleQuotedString(s) => s.clone(),
                        _ => return Err(Error::new(format!("{e} is not a number"))),
                    },
                    Expr::UnaryOp {
                        op: UnaryOperator::Minus,
                        expr,
                    } => format!("-{expr}"),
                    _ => return Err(Error::new(format!("{e} is not a number"))),
                };
                if text
                    .split_once('.')
                    .is_some_and(|(_, f)| f.len() > scale as usize)
                {
                    return Err(Error::new(format!(
                        "{e}: compare a numeric({precision},{scale}) with at most {scale} digits after the point"
                    )));
                }
                crate::catalog::numeric_units(&text, 18, scale)
            }
            _ => number(e).ok_or_else(|| Error::new(format!("{e} is not an integer"))),
        },
        Shape::Clock(_) => Err(Error::new("compare the column itself, or its month")),
    }
}

/// A time on the UTC clock, as microseconds since 1970-01-01 UTC: what a column at time zone 'UTC' is
/// compared with.
fn clock_literal(e: &Expr) -> Result<i64> {
    let s = text_literal(e)?;
    for f in [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
    ] {
        if let Ok(t) = chrono::NaiveDateTime::parse_from_str(&s, f) {
            return Ok(t.and_utc().timestamp_micros());
        }
    }
    match chrono::NaiveDate::parse_from_str(&s, "%Y-%m-%d") {
        Ok(d) => Ok(d.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp_micros()),
        Err(_) => Err(Error::new(format!("'{s}' is not a time on the clock"))),
    }
}

pub(super) fn text_literal(e: &Expr) -> Result<String> {
    match e {
        Expr::Value(v) => match &v.value {
            Value::SingleQuotedString(s) => Ok(s.clone()),
            _ => Err(Error::new(format!("{e} is not text"))),
        },
        Expr::Nested(e) => text_literal(e),
        _ => Err(Error::new(format!("{e} is not text"))),
    }
}

/// A condition kept to another on the same column as well: the values both keep, and the NULLs
/// both keep.
fn merge(conds: &mut Vec<Cond>, c: Cond) {
    match conds.iter_mut().find(|x| x.shape == c.shape) {
        Some(have) => {
            have.kept = Interval {
                lo: have.kept.lo.max(c.kept.lo),
                hi: have.kept.hi.min(c.kept.hi),
            };
            for v in c.except {
                if !have.except.contains(&v) {
                    have.except.push(v);
                }
            }
            have.nulls &= c.nulls;
        }
        None => conds.push(c),
    }
}

/// A truth test on a boolean column: the column standing as a condition, alone or under NOT, or
/// under IS [NOT] TRUE or FALSE. Its side and the condition it is: each keeps what `= true` or
/// `= false` keeps, and IS NOT TRUE and IS NOT FALSE keep the column's NULLs too. Refused on a
/// column of any other type, as PostgreSQL refuses it.
fn truth_test(from: &From, e: &Expr) -> Result<Option<(usize, Cond)>> {
    let (x, v, nulls, test) = match e {
        Expr::UnaryOp {
            op: UnaryOperator::Not,
            expr,
        } => (expr.as_ref(), 0, false, None),
        Expr::IsTrue(x) => (x.as_ref(), 1, false, Some("IS TRUE")),
        Expr::IsFalse(x) => (x.as_ref(), 0, false, Some("IS FALSE")),
        Expr::IsNotTrue(x) => (x.as_ref(), 0, true, Some("IS NOT TRUE")),
        Expr::IsNotFalse(x) => (x.as_ref(), 1, true, Some("IS NOT FALSE")),
        x => (x, 1, false, None),
    };
    match from.resolve(x) {
        Ok((side, Shape::Column(c))) if from.table(side).columns[c].ty == ColumnType::Boolean => {
            let mut cond = Cond::new(Shape::Column(c), Interval::at(v));
            cond.nulls = nulls;
            Ok(Some((side, cond)))
        }
        Ok((side, _)) if test.is_some() => Err(Error::new(format!(
            "argument of {} must be type boolean, not type {}",
            test.unwrap(),
            match from.resolve(x)? {
                (_, Shape::Column(c)) | (_, Shape::Clock(c)) =>
                    from.table(side).columns[c].ty.pg_name(),
                (_, Shape::MonthUtc(_)) => "smallint".into(),
            }
        ))),
        _ => Ok(None),
    }
}

fn conjuncts<'a>(e: &'a Expr, out: &mut Vec<&'a Expr>) {
    match e {
        Expr::BinaryOp {
            left,
            op: BinaryOperator::And,
            right,
        } => {
            conjuncts(left, out);
            conjuncts(right, out);
        }
        Expr::Nested(inner) => conjuncts(inner, out),
        other => out.push(other),
    }
}

/// The tables a question reads, as it names them.
struct From<'a> {
    catalog: &'a Catalog,
    tables: Vec<(usize, Option<String>)>,
}

impl From<'_> {
    fn table(&self, side: usize) -> &Table {
        &self.catalog.tables[self.tables[side].0]
    }

    /// What an expression reads, and of which table: the one table whose columns it names.
    fn resolve(&self, e: &Expr) -> Result<(usize, Shape)> {
        let mut found = Vec::new();
        let mut first_error = None;
        for (side, (ti, alias)) in self.tables.iter().enumerate() {
            let t = &self.catalog.tables[*ti];
            match shape(
                e,
                &t.name,
                alias.as_deref(),
                &t.columns,
                &self.catalog.functions,
                &[],
            ) {
                Ok(s) => found.push((side, s)),
                Err(err) => {
                    first_error.get_or_insert(err);
                }
            }
        }
        match found.as_slice() {
            [(side, Shape::Column(c))] if self.table(*side).columns[*c].is_virtual() => {
                Err(Error::new(format!(
                    "{e} is a virtual generated column, computed at the read from the columns the \
                     table stores: a scan reads only those"
                )))
            }
            [one] => Ok(*one),
            [] => Err(first_error.unwrap_or_else(|| Error::new(format!("{e} reads no table")))),
            _ => Err(Error::new(format!(
                "{e} could name a column of either table: qualify it"
            ))),
        }
    }

    fn key_of(&self, e: &Expr) -> Result<Key> {
        match self.resolve(e)? {
            (side, Shape::Column(column)) => Ok(Key::Column(Col { side, column })),
            (side, Shape::MonthUtc(column)) => Ok(Key::Month(Col { side, column })),
            (_, Shape::Clock(_)) => Err(Error::new(format!(
                "{e}: return the column itself, or its month"
            ))),
        }
    }

    /// An aggregate the snail reads, `count(*)` or `sum` of an integer column; `None` for anything that
    /// is no aggregate.
    fn aggregate(&self, e: &Expr) -> Result<Option<Item>> {
        let f = match e {
            Expr::Function(f) => f,
            _ => return Ok(None),
        };
        let name = object_name(&f.name);
        if !matches!(name.as_str(), "count" | "sum" | "min" | "max" | "avg") {
            return Ok(None);
        }
        let list = match &f.args {
            FunctionArguments::List(l)
                if l.duplicate_treatment.is_none() && l.clauses.is_empty() =>
            {
                &l.args
            }
            _ => return Err(Error::new(format!("{e} is not read"))),
        };
        if f.filter.is_some() || f.over.is_some() || !f.within_group.is_empty() {
            return Err(Error::new(format!("{e} is not read")));
        }
        match (name.as_str(), list.as_slice()) {
            ("count", [FunctionArg::Unnamed(FunctionArgExpr::Wildcard)]) => Ok(Some(Item::Count)),
            ("sum", [FunctionArg::Unnamed(FunctionArgExpr::Expr(x))]) => match self.resolve(x)? {
                (side, Shape::Column(column))
                    if matches!(
                        self.table(side).columns[column].ty,
                        ColumnType::Int2
                            | ColumnType::Int4
                            | ColumnType::Int8
                            | ColumnType::Numeric { .. }
                    ) =>
                {
                    Ok(Some(Item::Sum(Col { side, column })))
                }
                _ => Err(Error::new(format!(
                    "{e}: sum is read over an integer or numeric column"
                ))),
            },
            _ => Err(Error::new(format!("{e} is not read"))),
        }
    }

    /// The name PostgreSQL gives a column the question leaves unnamed.
    fn default_name(&self, item: Item) -> String {
        match item {
            Item::Key(Key::Column(c)) => self.table(c.side).columns[c.column].name.clone(),
            Item::Key(Key::Month(_)) => "extract".into(),
            Item::Count => "count".into(),
            Item::Sum(_) => "sum".into(),
        }
    }
}

fn relation(catalog: &Catalog, t: &TableFactor) -> Result<(usize, Option<String>)> {
    match t {
        TableFactor::Table { name, alias, .. } => {
            let name = object_name(name);
            let ti = catalog
                .tables
                .iter()
                .position(|t| t.name == name)
                .ok_or_else(|| Error::new(format!("{name} is not in the snail")))?;
            Ok((ti, alias.as_ref().map(|a| ident(&a.name))))
        }
        _ => Err(Error::new("a question reads tables")),
    }
}

pub fn parse(catalog: &Catalog, sql: &str) -> Result<Question> {
    let statements = Parser::parse_sql(&PostgreSqlDialect {}, sql)?;
    let q = match statements.as_slice() {
        [Statement::Query(q)] => q,
        _ => return Err(Error::new("a question is one SELECT")),
    };
    if q.with.is_some() || q.limit_clause.is_some() || q.fetch.is_some() {
        return Err(Error::new("WITH, LIMIT and FETCH are not read"));
    }
    let s = match q.body.as_ref() {
        SetExpr::Select(s) => s,
        _ => return Err(Error::new("set operations are not read")),
    };
    if s.distinct.is_some() || s.having.is_some() {
        return Err(Error::new("DISTINCT and HAVING are not read"));
    }
    let mut tables = Vec::new();
    let mut parts: Vec<&Expr> = Vec::new();
    match s.from.as_slice() {
        [t] => {
            tables.push(relation(catalog, &t.relation)?);
            for j in &t.joins {
                match &j.join_operator {
                    JoinOperator::Join(JoinConstraint::On(e))
                    | JoinOperator::Inner(JoinConstraint::On(e)) => {
                        tables.push(relation(catalog, &j.relation)?);
                        conjuncts(e, &mut parts);
                    }
                    _ => return Err(Error::new("only an inner JOIN … ON is read")),
                }
            }
        }
        _ => {
            return Err(Error::new(
                "a question reads one table, or tables joined with JOIN … ON",
            ))
        }
    }
    if tables.len() > 2 {
        return Err(Error::new("a join of more than two tables is not read"));
    }
    let from = From { catalog, tables };

    let mut listed: Vec<(String, Item)> = Vec::new();
    for item in &s.projection {
        let (e, label) = match item {
            SelectItem::UnnamedExpr(e) => (e, None),
            SelectItem::ExprWithAlias { expr, alias } => (expr, Some(ident(alias))),
            _ => return Err(Error::new("name each column a question returns")),
        };
        let it = match from.aggregate(e)? {
            Some(a) => a,
            None => Item::Key(from.key_of(e)?),
        };
        listed.push((label.unwrap_or_else(|| from.default_name(it)), it));
    }
    let grouping = match &s.group_by {
        GroupByExpr::Expressions(e, modifiers) if modifiers.is_empty() => e,
        _ => return Err(Error::new("GROUP BY ALL and its modifiers are not read")),
    };
    let aggregated = listed.iter().any(|(_, i)| !matches!(i, Item::Key(_)));
    let select = if grouping.is_empty() && !aggregated {
        Select::Rows(
            listed
                .iter()
                .map(|(n, i)| match i {
                    Item::Key(k) => (n.clone(), *k),
                    _ => unreachable!("an aggregate makes the question grouped"),
                })
                .collect(),
        )
    } else {
        let mut keys = Vec::new();
        for g in grouping {
            let k = match number(g) {
                Some(n) => match listed.get((n as usize).wrapping_sub(1)) {
                    Some((_, Item::Key(k))) => *k,
                    _ => {
                        return Err(Error::new(format!(
                            "GROUP BY {n} names no key of the select list"
                        )))
                    }
                },
                None => from.key_of(g)?,
            };
            if !keys.contains(&k) {
                keys.push(k);
            }
        }
        for (n, i) in &listed {
            if let Item::Key(k) = i {
                if !keys.contains(k) {
                    return Err(Error::new(format!(
                        "{n} must appear in GROUP BY or be an aggregate"
                    )));
                }
            }
        }
        Select::Groups {
            keys,
            items: listed.clone(),
        }
    };
    let mut order = Vec::new();
    if let Some(ob) = &q.order_by {
        let exprs = match &ob.kind {
            OrderByKind::Expressions(e) => e,
            _ => return Err(Error::new("ORDER BY ALL is not read")),
        };
        for o in exprs {
            let at = match number(&o.expr) {
                Some(n) if n >= 1 && (n as usize) <= listed.len() => n as usize - 1,
                Some(n) => {
                    return Err(Error::new(format!(
                        "ORDER BY {n} names no column the answer returns"
                    )))
                }
                None => {
                    let named = match &o.expr {
                        Expr::Identifier(i) => {
                            let name = ident(i);
                            listed.iter().position(|(n, _)| *n == name)
                        }
                        _ => None,
                    };
                    match named {
                        Some(at) => at,
                        None => {
                            let k = from.key_of(&o.expr)?;
                            listed
                                .iter()
                                .position(|(_, i)| *i == Item::Key(k))
                                .ok_or_else(|| {
                                    Error::new(format!(
                                        "ORDER BY {}: order by a column the answer returns",
                                        o.expr
                                    ))
                                })?
                        }
                    }
                }
            };
            let desc = match &o.options.sort {
                None | Some(OrderBySort::Asc) => false,
                Some(OrderBySort::Desc) => true,
                Some(_) => {
                    return Err(Error::new(format!(
                        "ORDER BY {}: USING is not read",
                        o.expr
                    )))
                }
            };
            order.push(Sort {
                at,
                desc,
                nulls_first: o.options.nulls_first.unwrap_or(desc),
            });
        }
    }

    if let Some(w) = &s.selection {
        conjuncts(w, &mut parts);
    }
    let mut sides: Vec<Side> = from
        .tables
        .iter()
        .map(|(ti, _)| Side {
            table: *ti,
            conds: Vec::new(),
            texts: Vec::new(),
            nulls: Vec::new(),
            empty: false,
        })
        .collect();
    let mut join: Vec<(usize, usize)> = Vec::new();
    for p in parts {
        if let Some((side, cond)) = truth_test(&from, p)? {
            merge(&mut sides[side].conds, cond);
            continue;
        }
        if let Expr::IsUnknown(x) | Expr::IsNotUnknown(x) = p {
            if let (side, Shape::Column(c)) = from.resolve(x)? {
                let ty = from.table(side).columns[c].ty;
                if ty != ColumnType::Boolean {
                    return Err(Error::new(format!(
                        "argument of {} must be type boolean, not type {}",
                        if matches!(p, Expr::IsUnknown(_)) {
                            "IS UNKNOWN"
                        } else {
                            "IS NOT UNKNOWN"
                        },
                        ty.pg_name()
                    )));
                }
            }
        }
        if let Expr::IsNull(x) | Expr::IsNotNull(x) | Expr::IsUnknown(x) | Expr::IsNotUnknown(x) = p
        {
            let null = matches!(p, Expr::IsNull(_) | Expr::IsUnknown(_));
            let (side, c) = match from.resolve(x)? {
                (side, Shape::Column(c)) => (side, c),
                _ => return Err(Error::new(format!("{p}: test a column itself for NULL"))),
            };
            let here = &mut sides[side];
            if from.table(side).columns[c].not_null {
                here.empty |= null;
            } else if !here.nulls.contains(&(c, null)) {
                here.nulls.push((c, null));
            }
            continue;
        }
        let (left, op, right) = match p {
            Expr::BinaryOp { left, op, right } => (left.as_ref(), op.clone(), right.as_ref()),
            other => {
                return Err(Error::new(format!(
                    "{other} is not a comparison the snail reads"
                )))
            }
        };
        if let (Ok((a, Shape::Column(x))), Ok((b, Shape::Column(y)))) =
            (from.resolve(left), from.resolve(right))
        {
            if a == b || op != BinaryOperator::Eq {
                return Err(Error::new(format!(
                    "{p}: two tables meet only on columns equal to each other"
                )));
            }
            let (x, y) = if a == 0 { (x, y) } else { (y, x) };
            for (side, c) in [(0, x), (1, y)] {
                let ty = from.table(side).columns[c].ty;
                if ty.is_byte_array() || ty == ColumnType::Timestamptz {
                    return Err(Error::new(format!(
                        "{p}: a join on {} is not read",
                        ty.name()
                    )));
                }
            }
            if !join.contains(&(x, y)) {
                join.push((x, y));
            }
            continue;
        }
        let (side, s, lit, op) = match from.resolve(left) {
            Ok((side, s)) => (side, s, right, op),
            Err(_) => {
                let (side, s) = from.resolve(right)?;
                let flipped = match op {
                    BinaryOperator::Lt => BinaryOperator::Gt,
                    BinaryOperator::LtEq => BinaryOperator::GtEq,
                    BinaryOperator::Gt => BinaryOperator::Lt,
                    BinaryOperator::GtEq => BinaryOperator::LtEq,
                    other => other,
                };
                (side, s, left, flipped)
            }
        };
        let table = from.table(side);
        let here = &mut sides[side];
        // No comparison matches a NULL, so one with NULL keeps no row.
        if matches!(lit, Expr::Value(v) if v.value == Value::Null)
            && matches!(
                op,
                BinaryOperator::Eq
                    | BinaryOperator::NotEq
                    | BinaryOperator::Lt
                    | BinaryOperator::LtEq
                    | BinaryOperator::Gt
                    | BinaryOperator::GtEq
            )
        {
            here.empty = true;
            continue;
        }
        if let Shape::Column(c) = s {
            if table.columns[c].ty == ColumnType::Bytes {
                return Err(Error::new(format!(
                    "{p}: a condition on bytea is read only in a pipeline"
                )));
            }
            if table.columns[c].ty.is_text() {
                let v = text_literal(lit)?;
                let mut except = Vec::new();
                let (lo, hi) = match op {
                    BinaryOperator::Eq => (Included(v.clone()), Included(v)),
                    BinaryOperator::NotEq => {
                        except.push(v);
                        (Unbounded, Unbounded)
                    }
                    BinaryOperator::Lt => (Unbounded, Excluded(v)),
                    BinaryOperator::LtEq => (Unbounded, Included(v)),
                    BinaryOperator::Gt => (Excluded(v), Unbounded),
                    BinaryOperator::GtEq => (Included(v), Unbounded),
                    other => {
                        return Err(Error::new(format!(
                            "the comparison {other} on text is not read"
                        )))
                    }
                };
                match here.texts.iter_mut().find(|t| t.column == c) {
                    Some(t) => {
                        t.lo = tighter(std::mem::replace(&mut t.lo, Unbounded), lo, true);
                        t.hi = tighter(std::mem::replace(&mut t.hi, Unbounded), hi, false);
                        for x in except {
                            if !t.except.contains(&x) {
                                t.except.push(x);
                            }
                        }
                    }
                    None => here.texts.push(TextCond {
                        column: c,
                        lo,
                        hi,
                        except,
                    }),
                }
                continue;
            }
        }
        let (s, v) = match s {
            Shape::Clock(c) => (Shape::Column(c), clock_literal(lit)? as i128),
            _ => (s, literal(lit, s, table)?),
        };
        let cond = match op {
            BinaryOperator::Eq => Cond::new(s, Interval::at(v)),
            BinaryOperator::NotEq => {
                let mut c = Cond::new(s, Interval::ALL);
                c.except.push(v);
                c
            }
            BinaryOperator::Lt => Cond::new(
                s,
                Interval {
                    lo: i128::MIN,
                    hi: v.saturating_sub(1),
                },
            ),
            BinaryOperator::LtEq => Cond::new(
                s,
                Interval {
                    lo: i128::MIN,
                    hi: v,
                },
            ),
            BinaryOperator::Gt => Cond::new(
                s,
                Interval {
                    lo: v.saturating_add(1),
                    hi: i128::MAX,
                },
            ),
            BinaryOperator::GtEq => Cond::new(
                s,
                Interval {
                    lo: v,
                    hi: i128::MAX,
                },
            ),
            other => return Err(Error::new(format!("the comparison {other} is not read"))),
        };
        merge(&mut here.conds, cond);
    }
    if sides.len() == 2 && join.is_empty() {
        return Err(Error::new("two tables are joined on no key"));
    }
    for side in &mut sides {
        // No comparison matches a NULL, so a column kept to its NULLs and compared holds no row.
        let compared = |c: usize| {
            side.conds
                .iter()
                .any(|x| !x.nulls && crate::read::walk::s_column(x.shape) == c)
                || side.texts.iter().any(|t| t.column == c)
                || side.nulls.contains(&(c, false))
        };
        side.empty |= side.conds.iter().any(Cond::is_empty)
            || side.texts.iter().any(TextCond::is_empty)
            || side.nulls.iter().any(|&(c, null)| null && compared(c));
    }
    Ok(Question {
        sides,
        join,
        select,
        order,
    })
}

#[cfg(test)]
mod tests {
    use super::truth;

    #[test]
    fn a_truth_is_read_as_postgresql_reads_a_boolean() {
        for (s, want) in [
            ("true", true),
            ("t", true),
            ("TrU", true),
            (" TRUE ", true),
            ("yes", true),
            ("y", true),
            ("on", true),
            ("1", true),
            ("\ttrue\n", true),
            ("false", false),
            ("F", false),
            ("no", false),
            ("n", false),
            ("off", false),
            ("of", false),
            ("0", false),
        ] {
            assert_eq!(truth(s), Some(want), "{s:?}");
        }
        for s in [
            "", " ", "o", "truex", "yess", "onn", "2", "01", "tr ue", "nope", "ja",
        ] {
            assert_eq!(truth(s), None, "{s:?}");
        }
    }
}
