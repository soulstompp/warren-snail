//! A step's subqueries: a scalar subquery, EXISTS, and IN, ANY or ALL over a subquery, in its select
//! list, WHERE, HAVING, GROUP BY or ORDER BY. One that reads nothing of the query around it is a step
//! run once, the first time a row asks for it. One that reads that query is answered for each set of
//! values a row gives what it names, its tables read from the file once, its FROM's rows kept where
//! they read nothing of the query around it, and met with each row by the =s of its WHERE that tie
//! a column of its own to that query; where none ties them, every kept row is read for each.

use super::pipeline::{self, Casings, Computed, Frame, Key, Value};
use super::question::Question;
use super::relations::{self, Accrued, Asked, Col, Outer, Reads, Steps};
use super::typed::Types;
use super::Round;
use crate::catalog::{Catalog, ColumnType};
use crate::{Error, Result};
use arrow::array::RecordBatch;
use sqlparser::ast::{
    BinaryOperator, Expr, GroupByExpr, OrderByKind, Query, SelectItem, SetExpr, TableFactor,
};
use std::cell::{OnceCell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

/// What a subquery's reads keep between the rows it is answered for: each question the question
/// path read, and each of its steps' FROM rows where they read nothing of the query around it.
#[derive(Default)]
pub(super) struct Kept {
    read: RefCell<HashMap<String, RecordBatch>>,
    pub(super) rows: RefCell<HashMap<String, Rc<relations::Parted>>>,
}

/// The snail's tables read through what a subquery keeps: each question from the file once, then
/// as it was read.
pub(super) struct Cached<'a> {
    pub(super) reads: &'a dyn Reads,
    pub(super) kept: &'a Kept,
}

impl Reads for Cached<'_> {
    fn catalog(&self) -> &Catalog {
        self.reads.catalog()
    }

    fn read(&self, q: &Question, order: Option<&str>) -> Result<(RecordBatch, Round)> {
        let key = format!("{order:?} {q:?}");
        if let Some(b) = self.kept.read.borrow().get(&key) {
            return Ok((b.clone(), Round::default()));
        }
        let (b, r) = self.reads.read(q, order)?;
        self.kept.read.borrow_mut().insert(key, b.clone());
        Ok((b, r))
    }

    fn snails(&self) -> usize {
        self.reads.snails()
    }
}

/// What a subquery's place asks of its rows.
enum Asks<'q> {
    /// A scalar subquery: its one row's one value, NULL where it returns none.
    Scalar,
    Exists {
        negated: bool,
    },
    /// `x op ANY (…)`, IN as `= ANY (…)`, and `x op ALL (…)`; NOT IN is `NOT (x = ANY (…))`.
    Compared {
        left: &'q Expr,
        op: BinaryOperator,
        any: bool,
        negated: bool,
    },
}

/// The rows a subquery returned, and the keys of its one column, where an = compares by them.
struct Returned {
    rows: Vec<Vec<Value>>,
    keys: OnceCell<(HashSet<Key>, bool)>,
}

impl Returned {
    fn new(rows: Vec<Vec<Value>>) -> Rc<Returned> {
        Rc::new(Returned {
            rows,
            keys: OnceCell::new(),
        })
    }
}

/// A subquery of a step, as its place asks it.
struct Plan<'q> {
    query: &'q Query,
    asks: Asks<'q>,
    /// The type of its first column.
    first: Option<ColumnType>,
    /// Where it reads the query around it: the places of that query's columns its words name.
    around: Option<Vec<usize>>,
    /// Of those, the ones it cannot be read without, where it is read on each group.
    reads_around: Vec<usize>,
    /// What it returned: once, where it reads nothing of the query around it; else for each set of
    /// values of what it names.
    once: RefCell<Option<Rc<Returned>>>,
    by_values: RefCell<HashMap<String, Rc<Returned>>>,
    kept: Kept,
}

/// A step's subqueries, by their places in its query.
#[derive(Default)]
pub(super) struct Plans<'q> {
    plans: HashMap<*const Expr, Plan<'q>>,
    around: Vec<usize>,
}

impl<'q> Plans<'q> {
    pub(super) fn is_empty(&self) -> bool {
        self.plans.is_empty()
    }

    /// Each subquery a step's query holds outside any other, read over tables holding no rows before
    /// any row, as PostgreSQL's parser reads it: refused where it returns columns its place does not
    /// take, and read as reading the query around it where it names a column only that query has.
    /// With the type each scalar subquery takes.
    pub(super) fn of(
        asked: Asked,
        q: &'q Query,
        columns: &[Col],
        made: &Steps,
        casings: &Casings,
    ) -> Result<(Plans<'q>, Types)> {
        // Those read on each group where the step groups, and those read before it groups.
        let mut after = Vec::new();
        let mut before = Vec::new();
        if let SetExpr::Select(s) = q.body.as_ref() {
            for item in &s.projection {
                if let SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } = item
                {
                    on_groups(e, &mut after, &mut before);
                }
            }
            if let Some(e) = &s.having {
                on_groups(e, &mut after, &mut before);
            }
            if let Some(e) = &s.selection {
                pipeline::subqueries_in(e, &mut before);
            }
            if let GroupByExpr::Expressions(g, _) = &s.group_by {
                for e in g {
                    pipeline::subqueries_in(e, &mut before);
                }
            }
        }
        if let Some(OrderByKind::Expressions(os)) = q.order_by.as_ref().map(|o| &o.kind) {
            for o in os {
                on_groups(&o.expr, &mut after, &mut before);
            }
        }
        let mut plans = Plans::default();
        let mut seeded = Types::new();
        let found = after
            .into_iter()
            .map(|e| (e, true))
            .chain(before.into_iter().map(|e| (e, false)));
        for (e, grouped) in found {
            let plan = Plan::of(asked, e, columns, made, casings, grouped)?;
            if let (Asks::Scalar, Some(t)) = (&plan.asks, plan.first) {
                seeded.insert(e as *const Expr, t);
            }
            plans.around.extend(plan.reads_around.iter().copied());
            plans.plans.insert(e as *const Expr, plan);
        }
        Ok((plans, seeded))
    }

    /// The columns of the step its subqueries read on each group, where it groups.
    pub(super) fn around(&self) -> &[usize] {
        &self.around
    }
}

impl<'q> Plan<'q> {
    fn of(
        asked: Asked,
        e: &'q Expr,
        columns: &[Col],
        made: &Steps,
        casings: &Casings,
        grouped: bool,
    ) -> Result<Plan<'q>> {
        let arity = |x: &Expr| match x {
            Expr::Tuple(xs) => xs.len(),
            _ => 1,
        };
        let (query, asks, wants): (&Query, Asks, Option<usize>) = match e {
            Expr::Subquery(q) => (q, Asks::Scalar, None),
            Expr::Exists { subquery, negated } => {
                (subquery, Asks::Exists { negated: *negated }, None)
            }
            Expr::InSubquery {
                expr,
                subquery,
                negated,
            } => (
                subquery,
                Asks::Compared {
                    left: expr,
                    op: BinaryOperator::Eq,
                    any: true,
                    negated: *negated,
                },
                Some(arity(expr)),
            ),
            Expr::AnyOp {
                left,
                compare_op,
                right,
                ..
            }
            | Expr::AllOp {
                left,
                compare_op,
                right,
            } => {
                let Expr::Subquery(q) = right.as_ref() else {
                    unreachable!("a subquery")
                };
                (
                    q,
                    Asks::Compared {
                        left,
                        op: compare_op.clone(),
                        any: matches!(e, Expr::AnyOp { .. }),
                        negated: false,
                    },
                    Some(arity(left)),
                )
            }
            _ => unreachable!("a subquery"),
        };
        super::question::unlocked(query)?;
        let empty = relations::Empty(asked.reads.catalog());
        let probe = Asked {
            reads: &empty,
            ..asked
        };
        // Read with the step's columns as the query around it, all of them, or all but one.
        let asked_around = |hidden: Option<usize>| {
            let kept = Kept::default();
            let mut columns: Vec<Col> = columns.iter().map(relations::marked).collect();
            if let Some(i) = hidden {
                columns[i].name.insert(0, '\u{3}');
            }
            let outer = Outer {
                values: vec![Value::Null; columns.len()],
                columns,
                kept: &kept,
            };
            relations::made_by(
                probe,
                query,
                made,
                casings,
                Some(&outer),
                &mut Accrued::default(),
            )
        };
        let none = relations::made_by(probe, query, made, casings, None, &mut Accrued::default());
        let (m, around, reads_around) = match none {
            Ok((m, _)) => (m, None, Vec::new()),
            Err(_) => {
                let (m, _) = asked_around(None)?;
                let ws = relations::words(&query.to_string());
                let named: Vec<usize> = (0..columns.len())
                    .filter(|&i| ws.contains(&columns[i].name))
                    .collect();
                // Where it is read on each group, the columns it cannot be read without.
                let reads: Vec<usize> = match grouped {
                    true => named
                        .iter()
                        .copied()
                        .filter(|&i| asked_around(Some(i)).is_err())
                        .collect(),
                    false => Vec::new(),
                };
                (m, Some(named), reads)
            }
        };
        let width = m.rel.names.len();
        match (&asks, wants) {
            (Asks::Scalar, _) if width != 1 => {
                return Err(Error::new("subquery must return only one column"))
            }
            (_, Some(n)) if width > n => return Err(Error::new("subquery has too many columns")),
            (_, Some(n)) if width < n => return Err(Error::new("subquery has too few columns")),
            _ => {}
        }
        Ok(Plan {
            query,
            asks,
            first: m.types.first().copied().flatten(),
            around,
            reads_around,
            once: RefCell::new(None),
            by_values: RefCell::new(HashMap::new()),
            kept: Kept::default(),
        })
    }
}

/// What answers a step's subqueries, for each row it reads them on.
pub(super) struct Subs<'a, 'q> {
    plans: &'a Plans<'q>,
    asked: Asked<'a>,
    made: &'a Steps,
    casings: &'a Casings,
    /// The step's columns, those of the query around it among them.
    columns: &'a [Col],
    accrued: RefCell<&'a mut Accrued>,
}

impl<'a, 'q> Subs<'a, 'q> {
    pub(super) fn new(
        plans: &'a Plans<'q>,
        asked: Asked<'a>,
        made: &'a Steps,
        casings: &'a Casings,
        columns: &'a [Col],
        accrued: &'a mut Accrued,
    ) -> Subs<'a, 'q> {
        Subs {
            plans,
            asked,
            made,
            casings,
            columns,
            accrued: RefCell::new(accrued),
        }
    }

    /// What a subquery returns for a row: once, where it reads nothing of the query around it; else
    /// for the values the row gives what it names, the query around it read from those alone.
    fn returned(&self, plan: &Plan, row: &[Value]) -> Result<Rc<Returned>> {
        let Some(places) = &plan.around else {
            if let Some(r) = plan.once.borrow().as_ref() {
                return Ok(r.clone());
            }
            let mut accrued = self.accrued.borrow_mut();
            let (m, _) = relations::made_by(
                self.asked,
                plan.query,
                self.made,
                self.casings,
                None,
                &mut accrued,
            )?;
            let r = Returned::new(m.rel.rows);
            *plan.once.borrow_mut() = Some(r.clone());
            return Ok(r);
        };
        let key = places
            .iter()
            .map(|&i| format!("{:?}", row[i]))
            .collect::<Vec<_>>()
            .join("\0");
        if let Some(r) = plan.by_values.borrow().get(&key) {
            return Ok(r.clone());
        }
        let outer = Outer {
            columns: places
                .iter()
                .map(|&i| relations::marked(&self.columns[i]))
                .collect(),
            values: places.iter().map(|&i| row[i].clone()).collect(),
            kept: &plan.kept,
        };
        let cached = Cached {
            reads: self.asked.reads,
            kept: &plan.kept,
        };
        let asked = Asked {
            reads: &cached,
            ..self.asked
        };
        let mut accrued = self.accrued.borrow_mut();
        let (m, _) = relations::made_by(
            asked,
            plan.query,
            self.made,
            self.casings,
            Some(&outer),
            &mut accrued,
        )?;
        let r = Returned::new(m.rel.rows);
        plan.by_values.borrow_mut().insert(key, r.clone());
        Ok(r)
    }
}

/// A truth's negation, NULL staying NULL.
fn not(v: Value) -> Value {
    match v {
        Value::Bool(b) => Value::Bool(!b),
        other => other,
    }
}

/// Whether an = between values of two types holds exactly where their keys are one.
fn keyed_alike(a: Option<ColumnType>, b: Option<ColumnType>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => {
            (a.is_integer() && b.is_integer())
                || matches!((a, b), (ColumnType::Text(_), ColumnType::Text(_)))
                || (a == b && a == ColumnType::Uuid)
        }
        _ => false,
    }
}

/// A row compared with each row a subquery returned, by = or <>, field by field: true where one (or
/// each) holds, NULL where none (or not every one) holds and one compared NULL.
fn rows_compared(
    op: &BinaryOperator,
    xs: &[Value],
    lefts: &[Expr],
    rows: &[Vec<Value>],
    any: bool,
) -> Result<Value> {
    if !matches!(op, BinaryOperator::Eq | BinaryOperator::NotEq) {
        return Err(Error::new(format!(
            "a row compared by {op} with a subquery is not read"
        )));
    }
    let mut unknown = false;
    for r in rows {
        let mut row = Some(true);
        for ((x, left), v) in xs.iter().zip(lefts).zip(r) {
            match pipeline::compared_each(
                &BinaryOperator::Eq,
                x.clone(),
                std::iter::once(v.clone()),
                left,
                true,
            )? {
                Value::Bool(false) => {
                    row = Some(false);
                    break;
                }
                Value::Bool(true) => {}
                _ => row = None,
            }
        }
        if *op == BinaryOperator::NotEq {
            row = row.map(|b| !b);
        }
        match row {
            Some(true) if any => return Ok(Value::Bool(true)),
            Some(false) if !any => return Ok(Value::Bool(false)),
            None => unknown = true,
            _ => {}
        }
    }
    Ok(match unknown {
        true => Value::Null,
        false => Value::Bool(!any),
    })
}

impl pipeline::Subqueries for Subs<'_, '_> {
    fn value(&self, e: &Expr, row: &[Value], frame: &Frame, computed: &Computed) -> Result<Value> {
        let plan = self
            .plans
            .plans
            .get(&(e as *const Expr))
            .ok_or_else(|| Error::new(format!("{e} is not read here")))?;
        let returned = self.returned(plan, row)?;
        let rows = &returned.rows;
        match &plan.asks {
            Asks::Scalar => match rows.as_slice() {
                [] => Ok(Value::Null),
                [r] => Ok(r[0].clone()),
                _ => Err(Error::new(
                    "more than one row returned by a subquery used as an expression",
                )),
            },
            Asks::Exists { negated } => Ok(Value::Bool(rows.is_empty() == *negated)),
            Asks::Compared {
                left,
                op,
                any,
                negated,
            } => {
                let v = match left {
                    Expr::Tuple(lefts) => {
                        let xs = lefts
                            .iter()
                            .map(|x| pipeline::eval(x, row, frame, computed))
                            .collect::<Result<Vec<_>>>()?;
                        rows_compared(op, &xs, lefts, rows, *any)?
                    }
                    _ => {
                        let x = pipeline::eval(left, row, frame, computed)?;
                        let left_type = frame.types.get(&(*left as *const Expr)).copied();
                        if *op == BinaryOperator::Eq && *any && keyed_alike(left_type, plan.first) {
                            let (keys, nulls) = returned.keys.get_or_init(|| {
                                let mut keys = HashSet::new();
                                let mut nulls = false;
                                for r in rows {
                                    match r[0].is_null() {
                                        true => nulls = true,
                                        false => {
                                            keys.insert(pipeline::key(&r[0]));
                                        }
                                    }
                                }
                                (keys, nulls)
                            });
                            match () {
                                _ if rows.is_empty() => Value::Bool(false),
                                _ if x.is_null() => Value::Null,
                                _ if keys.contains(&pipeline::key(&x)) => Value::Bool(true),
                                _ if *nulls => Value::Null,
                                _ => Value::Bool(false),
                            }
                        } else {
                            pipeline::compared_each(
                                op,
                                x,
                                rows.iter().map(|r| r[0].clone()),
                                left,
                                *any,
                            )?
                        }
                    }
                };
                Ok(match negated {
                    true => not(v),
                    false => v,
                })
            }
        }
    }
}

/// Whether a question holds a subquery: in an expression of any of its steps, or as a LATERAL
/// subquery in a FROM.
pub(super) fn held(q: &Query) -> bool {
    fn expr(e: &Expr) -> bool {
        let mut found = Vec::new();
        pipeline::subqueries_in(e, &mut found);
        !found.is_empty()
    }
    fn factor(t: &TableFactor) -> bool {
        match t {
            TableFactor::Derived { lateral: true, .. } => true,
            TableFactor::Derived { subquery, .. } => held(subquery),
            TableFactor::NestedJoin {
                table_with_joins, ..
            } => {
                factor(&table_with_joins.relation)
                    || table_with_joins.joins.iter().any(|j| factor(&j.relation))
            }
            _ => false,
        }
    }
    fn body(b: &SetExpr) -> bool {
        match b {
            SetExpr::Select(s) => {
                s.projection.iter().any(|i| match i {
                    SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
                        expr(e)
                    }
                    _ => false,
                }) || s.selection.as_ref().is_some_and(expr)
                    || s.having.as_ref().is_some_and(expr)
                    || matches!(&s.group_by, GroupByExpr::Expressions(g, _) if g.iter().any(expr))
                    || s.from.iter().any(|t| {
                        factor(&t.relation)
                            || t.joins.iter().any(|j| {
                                factor(&j.relation)
                                    || relations::join_on(&j.join_operator).is_some_and(expr)
                            })
                    })
            }
            SetExpr::Query(q) => held(q),
            SetExpr::SetOperation { left, right, .. } => body(left) || body(right),
            _ => false,
        }
    }
    q.with
        .as_ref()
        .is_some_and(|w| w.cte_tables.iter().any(|c| held(&c.query)))
        || body(&q.body)
        || matches!(
            q.order_by.as_ref().map(|o| &o.kind),
            Some(OrderByKind::Expressions(os)) if os.iter().any(|o| expr(&o.expr))
        )
}

/// The subqueries an expression read after its step groups holds, those inside an aggregate's
/// arguments read on each row before it.
fn on_groups<'q>(e: &'q Expr, after: &mut Vec<&'q Expr>, before: &mut Vec<&'q Expr>) {
    match e {
        Expr::Function(f)
            if f.over.is_none()
                && (pipeline::is_aggregate(&pipeline::function_name(f))
                    || !f.within_group.is_empty()) =>
        {
            pipeline::subqueries_in(e, before)
        }
        e if pipeline::is_subquery(e) => {
            if let Expr::InSubquery { expr, .. }
            | Expr::AnyOp { left: expr, .. }
            | Expr::AllOp { left: expr, .. } = e
            {
                on_groups(expr, after, before);
            }
            after.push(e);
        }
        e => {
            let mut parts = pipeline::parts_of(e);
            if parts.is_empty() {
                parts = super::collate::children(e);
            }
            for p in parts {
                on_groups(p, after, before);
            }
        }
    }
}

/// Calls `f` on each subquery an expression holds outside any other.
pub(super) fn each_query_mut(e: &mut Expr, f: &mut dyn FnMut(&mut Query)) {
    use sqlparser::ast::{FunctionArg, FunctionArgExpr, FunctionArguments};
    match e {
        Expr::Subquery(q) | Expr::Exists { subquery: q, .. } => f(q),
        Expr::InSubquery { expr, subquery, .. } => {
            each_query_mut(expr, f);
            f(subquery);
        }
        Expr::BinaryOp { left, right, .. }
        | Expr::AnyOp { left, right, .. }
        | Expr::AllOp { left, right, .. }
        | Expr::IsDistinctFrom(left, right)
        | Expr::IsNotDistinctFrom(left, right)
        | Expr::Like {
            expr: left,
            pattern: right,
            ..
        }
        | Expr::ILike {
            expr: left,
            pattern: right,
            ..
        } => {
            each_query_mut(left, f);
            each_query_mut(right, f);
        }
        Expr::UnaryOp { expr, .. }
        | Expr::Nested(expr)
        | Expr::Cast { expr, .. }
        | Expr::Collate { expr, .. }
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr)
        | Expr::IsTrue(expr)
        | Expr::IsNotTrue(expr)
        | Expr::IsFalse(expr)
        | Expr::IsNotFalse(expr)
        | Expr::IsUnknown(expr)
        | Expr::IsNotUnknown(expr) => each_query_mut(expr, f),
        Expr::InList { expr, list, .. } => {
            each_query_mut(expr, f);
            list.iter_mut().for_each(|x| each_query_mut(x, f));
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            each_query_mut(expr, f);
            each_query_mut(low, f);
            each_query_mut(high, f);
        }
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            if let Some(o) = operand {
                each_query_mut(o, f);
            }
            for c in conditions {
                each_query_mut(&mut c.condition, f);
                each_query_mut(&mut c.result, f);
            }
            if let Some(x) = else_result {
                each_query_mut(x, f);
            }
        }
        Expr::Function(fun) => {
            if let FunctionArguments::List(l) = &mut fun.args {
                for a in &mut l.args {
                    if let FunctionArg::Unnamed(FunctionArgExpr::Expr(x))
                    | FunctionArg::Named {
                        arg: FunctionArgExpr::Expr(x),
                        ..
                    } = a
                    {
                        each_query_mut(x, f);
                    }
                }
            }
            if let Some(x) = &mut fun.filter {
                each_query_mut(x, f);
            }
        }
        Expr::Tuple(xs) => xs.iter_mut().for_each(|x| each_query_mut(x, f)),
        Expr::Array(a) => a.elem.iter_mut().for_each(|x| each_query_mut(x, f)),
        _ => {}
    }
}

impl Plans<'_> {
    /// Whether an expression holds a subquery, each reading nothing of the query around it.
    pub(super) fn only_once(&self, e: &Expr) -> bool {
        let mut found = Vec::new();
        pipeline::subqueries_in(e, &mut found);
        !found.is_empty()
            && found.iter().all(|s| {
                self.plans
                    .get(&(*s as *const Expr))
                    .is_some_and(|p| p.around.is_none())
            })
    }
}
