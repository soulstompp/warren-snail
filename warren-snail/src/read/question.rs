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

/// Values from `lo` to `hi`, both included.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Interval {
    pub lo: i64,
    pub hi: i64,
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
pub enum Item {
    Key(Key),
    Count,
    Sum(Col),
}

#[derive(Debug)]
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

/// A condition on a text column: its values between two bounds, compared by their bytes.
#[derive(Clone, Debug)]
pub struct TextCond {
    pub column: usize,
    pub lo: Bound<String>,
    pub hi: Bound<String>,
}

impl TextCond {
    pub fn holds(&self, v: &str) -> bool {
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
            (Included(l), Included(h)) => l.as_bytes() > h.as_bytes(),
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
pub struct Sort {
    pub at: usize,
    pub desc: bool,
    pub nulls_first: bool,
}

/// One table a question reads, and the conditions on it alone.
#[derive(Debug)]
pub struct Side {
    pub table: usize,
    pub conds: Vec<(Shape, Interval)>,
    pub texts: Vec<TextCond>,
    /// Some condition can hold for no value.
    pub empty: bool,
}

impl Side {
    /// Whether a condition of its own narrows what the side reads.
    pub fn conditioned(&self) -> bool {
        !self.conds.is_empty() || !self.texts.is_empty()
    }
}

#[derive(Debug)]
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

fn literal(e: &Expr, s: Shape, table: &Table) -> Result<i64> {
    match s {
        Shape::MonthUtc(_) => number(e).ok_or_else(|| Error::new(format!("{e} is not a month"))),
        Shape::Column(c) => match table.columns[c].ty {
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
            empty: false,
        })
        .collect();
    let mut join: Vec<(usize, usize)> = Vec::new();
    for p in parts {
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
        if let Shape::Column(c) = s {
            if table.columns[c].ty == ColumnType::Bytes {
                return Err(Error::new(format!(
                    "{p}: a condition on bytea is read only in a pipeline"
                )));
            }
            if table.columns[c].ty.is_text() {
                let v = text_literal(lit)?;
                let (lo, hi) = match op {
                    BinaryOperator::Eq => (Included(v.clone()), Included(v)),
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
                    }
                    None => here.texts.push(TextCond { column: c, lo, hi }),
                }
                continue;
            }
        }
        let (s, v) = match s {
            Shape::Clock(c) => (Shape::Column(c), clock_literal(lit)?),
            _ => (s, literal(lit, s, table)?),
        };
        let iv = match op {
            BinaryOperator::Eq => Interval { lo: v, hi: v },
            BinaryOperator::Lt => Interval {
                lo: i64::MIN,
                hi: v.saturating_sub(1),
            },
            BinaryOperator::LtEq => Interval {
                lo: i64::MIN,
                hi: v,
            },
            BinaryOperator::Gt => Interval {
                lo: v.saturating_add(1),
                hi: i64::MAX,
            },
            BinaryOperator::GtEq => Interval {
                lo: v,
                hi: i64::MAX,
            },
            other => return Err(Error::new(format!("the comparison {other} is not read"))),
        };
        match here.conds.iter_mut().find(|(x, _)| *x == s) {
            Some((_, have)) => {
                *have = Interval {
                    lo: have.lo.max(iv.lo),
                    hi: have.hi.min(iv.hi),
                }
            }
            None => here.conds.push((s, iv)),
        }
    }
    if sides.len() == 2 && join.is_empty() {
        return Err(Error::new("two tables are joined on no key"));
    }
    for side in &mut sides {
        side.empty = side.conds.iter().any(|(_, iv)| iv.lo > iv.hi)
            || side.texts.iter().any(TextCond::is_empty);
    }
    Ok(Question {
        sides,
        join,
        select,
        order,
    })
}
