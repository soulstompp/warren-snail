//! A LIMIT over a FROM subquery, read as PostgreSQL 18.6's plan reads it: where the query around
//! the subquery only names its columns, its rows are the subquery's own, so its LIMIT, OFFSET or
//! FETCH, its ORDER BY by the subquery's columns and a WHERE on them are the subquery's, which then
//! makes no row past the last its LIMIT keeps. The query around keeps its select list and its ORDER
//! BY, and reads the rows the subquery returns. A subquery of one SELECT that reads no condition
//! and computes no column, or of a set operation, keeps its rows as they are.

use super::{outputs, pipeline};
use crate::catalog::ident;
use sqlparser::ast::{
    BinaryOperator, Distinct, Expr, GroupByExpr, LimitClause, OrderBy, OrderByExpr, OrderByKind,
    Query, Select, SelectItem, SetExpr, TableFactor, Value, VisitMut, VisitorMut,
};
use std::ops::ControlFlow;

/// Each query level of a statement whose cut its FROM subquery can take, so written.
pub(super) fn pushed(q: &mut Query, release: u8) {
    let _ = VisitMut::visit(q, &mut Pushing { release });
}

struct Pushing {
    release: u8,
}

impl VisitorMut for Pushing {
    type Break = ();

    fn pre_visit_query(&mut self, q: &mut Query) -> ControlFlow<()> {
        // A star over pg_settings alone is its columns, each of which is computed.
        if let SetExpr::Select(s) = q.body.as_mut() {
            for t in &mut s.from {
                if let TableFactor::Derived { subquery, .. } = &mut t.relation {
                    if let SetExpr::Select(inner) = subquery.body.as_mut() {
                        outputs::settings_starred(inner);
                    }
                }
            }
        }
        if let Some(m) = what_moves(q, self.release) {
            moved(q, m);
        } else if let Some(mut inlined) = inlined(q, self.release) {
            if let Some(m) = what_moves(&inlined, self.release) {
                moved(&mut inlined, m);
                *q = inlined;
            }
        }
        ControlFlow::Continue(())
    }
}

/// What of a query moves into the subquery it reads alone: its cut, where it has one, its ORDER BY
/// by the subquery's columns' places, where it moves with the cut, and its WHERE's condition.
struct Moves {
    cut: bool,
    sorted: Vec<OrderByExpr>,
    condition: Option<Expr>,
}

/// A query reading a WITH query alone, the WITH query read in its place as a subquery, as
/// PostgreSQL's planner reads one read once and not MATERIALIZED, writing nothing and calling
/// nothing that may be volatile; none for any other.
fn inlined(q: &Query, release: u8) -> Option<Query> {
    let with = q.with.as_ref().filter(|w| !w.recursive)?;
    let SetExpr::Select(s) = q.body.as_ref() else {
        return None;
    };
    if s.from.len() != 1 || !s.from[0].joins.is_empty() {
        return None;
    }
    let TableFactor::Table {
        name,
        alias,
        args: None,
        ..
    } = &s.from[0].relation
    else {
        return None;
    };
    let [sqlparser::ast::ObjectNamePart::Identifier(n)] = name.0.as_slice() else {
        return None;
    };
    let named = ident(n);
    let at = with
        .cte_tables
        .iter()
        .position(|c| ident(&c.alias.name) == named)?;
    let cte = &with.cte_tables[at];
    if cte.materialized.is_some() || !matches!(cte.query.body.as_ref(), SetExpr::Select(_)) {
        return None;
    }
    // Read once in the whole statement.
    let mut times = 0;
    let _ = sqlparser::ast::visit_relations(q, |r| {
        if matches!(r.0.as_slice(), [sqlparser::ast::ObjectNamePart::Identifier(i)] if ident(i) == named)
        {
            times += 1;
        }
        ControlFlow::<()>::Continue(())
    });
    let volatile = sqlparser::ast::visit_expressions(&cte.query, |x| match x {
        Expr::Function(f) if super::own::may_be_volatile(&pipeline::function_name(f), release) || pipeline::draws(x) => {
            ControlFlow::Break(())
        }
        _ => ControlFlow::Continue(()),
    })
    .is_break();
    if times != 1 || volatile {
        return None;
    }
    let mut out = q.clone();
    let columns = match alias.as_ref().filter(|a| !a.columns.is_empty()) {
        Some(a) => a.columns.clone(),
        None => cte.alias.columns.clone(),
    };
    let alias = sqlparser::ast::TableAlias {
        explicit: true,
        name: alias.as_ref().map_or_else(|| n.clone(), |a| a.name.clone()),
        columns,
        at: None,
    };
    let subquery = cte.query.clone();
    if let Some(w) = out.with.as_mut() {
        w.cte_tables.remove(at);
        if w.cte_tables.is_empty() {
            out.with = None;
        }
    }
    let SetExpr::Select(s) = out.body.as_mut() else {
        return None;
    };
    s.from[0].relation = TableFactor::Derived {
        lateral: false,
        subquery,
        alias: Some(alias),
        sample: None,
    };
    Some(out)
}

/// Whether a SELECT groups, returns each row once or makes a window or a set: any of which its
/// rows are not its FROM's, one for one.
fn folds(s: &Select) -> bool {
    s.distinct.is_some()
        || s.top.is_some()
        || s.having.is_some()
        || !s.named_window.is_empty()
        || s.qualify.is_some()
        || s.prewhere.is_some()
        || !s.connect_by.is_empty()
        || s.into.is_some()
        || !s.lateral_views.is_empty()
        || !matches!(&s.group_by, GroupByExpr::Expressions(v, m) if v.is_empty() && m.is_empty())
        || pipeline::aggregates(s)
        || s.projection.iter().any(|i| match i {
            SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
                pipeline::folds_or_makes_sets(e)
            }
            _ => false,
        })
}

/// The subquery a SELECT reads alone, with the alias it goes by and the names that alias gives its
/// columns.
fn alone(s: &Select) -> Option<(&Query, Option<String>, Vec<String>)> {
    if s.from.len() != 1 || !s.from[0].joins.is_empty() {
        return None;
    }
    match &s.from[0].relation {
        TableFactor::Derived {
            lateral: false,
            subquery,
            alias,
            ..
        } => Some((
            subquery,
            alias.as_ref().map(|a| ident(&a.name)),
            alias
                .iter()
                .flat_map(|a| &a.columns)
                .map(|c| ident(&c.name))
                .collect(),
        )),
        _ => None,
    }
}

/// What of a query moves into the subquery it reads alone (`Moves`); none where nothing moves.
/// With no cut, its WHERE alone moves, and only into a subquery that computes what it reads.
fn what_moves(q: &Query, release: u8) -> Option<Moves> {
    let cuts = match &q.limit_clause {
        Some(LimitClause::LimitOffset {
            limit: Some(l),
            limit_by,
            ..
        }) => limit_by.is_empty() && !matches!(l, Expr::Value(v) if matches!(v.value, Value::Null)),
        Some(LimitClause::OffsetCommaLimit { .. }) => false,
        _ => q.fetch.is_some(),
    };
    let fetch_ties = q.fetch.as_ref().is_some_and(|f| f.with_ties || f.percent);
    if q.limit_clause.is_some() && !cuts
        || fetch_ties
        || !q.locks.is_empty()
        || q.for_clause.is_some()
        || q.settings.is_some()
        || q.format_clause.is_some()
        || !q.pipe_operators.is_empty()
    {
        return None;
    }
    let SetExpr::Select(s) = q.body.as_ref() else {
        return None;
    };
    if folds(s) || !cuts && s.selection.is_none() {
        return None;
    }
    let (sub, alias, renames) = alone(s)?;
    if sub.limit_clause.is_some()
        || sub.fetch.is_some()
        || !sub.locks.is_empty()
        || !sub.pipe_operators.is_empty()
    {
        return None;
    }
    // Only a subquery that reads a condition or computes a column makes anything its cut saves.
    let computes = match sub.body.as_ref() {
        SetExpr::Select(inner) => {
            inner.selection.is_some()
                || inner.projection.iter().any(|i| match i {
                    SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
                        !outputs::read_as_written(e)
                    }
                    _ => false,
                })
        }
        _ => false,
    } || outputs::settings_alone(sub);
    if !computes && (s.selection.is_none() || !cuts) {
        return None;
    }
    // The subquery's columns, by the names they go by, and what makes each where it is one SELECT.
    let made: Option<Vec<(String, Expr)>> = match sub.body.as_ref() {
        SetExpr::Select(inner) => inner
            .projection
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let (own, e) = match item {
                    SelectItem::UnnamedExpr(e) => (pipeline::default_name(e), e),
                    SelectItem::ExprWithAlias { expr, alias } => (ident(alias), expr),
                    _ => return None,
                };
                Some((renames.get(i).cloned().unwrap_or(own), e.clone()))
            })
            .collect(),
        _ => None,
    };
    let names: Vec<String> = match &made {
        Some(m) => m.iter().map(|(n, _)| n.clone()).collect(),
        None => outputs::names_of(sub, &renames)?,
    };
    // The place among the subquery's columns of one an expression of the query around names.
    let place = |e: &Expr| column_place(e, &alias, &names);
    let by_item = |item: &SelectItem| match item {
        SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => place(e),
        _ => None,
    };
    // Its ORDER BY, each key one of the subquery's columns by its name or by its place in the
    // select list, made the subquery's by that column's place.
    let sorted: Vec<OrderByExpr> = match &q.order_by {
        _ if !cuts => Vec::new(),
        None => Vec::new(),
        Some(OrderBy {
            kind: OrderByKind::Expressions(os),
            interpolate: None,
        }) => os
            .iter()
            .map(|o| {
                if o.with_fill.is_some() {
                    return None;
                }
                let at = match &o.expr {
                    Expr::Value(v) => match &v.value {
                        Value::Number(n, _) => {
                            let i: usize = n.parse().ok()?;
                            by_item(s.projection.get(i.checked_sub(1)?)?)?
                        }
                        _ => return None,
                    },
                    Expr::Identifier(i) => {
                        let n = ident(i);
                        // A name an item takes as its alias reads that item.
                        match s.projection.iter().find(|it| {
                            matches!(it, SelectItem::ExprWithAlias { alias, .. } if ident(alias) == n)
                        }) {
                            Some(it) => by_item(it)?,
                            None => place(&o.expr)?,
                        }
                    }
                    e => place(e)?,
                };
                // A column the subquery returns as it is is sorted by by its name, where no other
                // column the subquery returns takes that name; any other by its place.
                let column = made
                    .as_ref()
                    .map(|m| &m[at].1)
                    .and_then(|e| pipeline::column_of(e).map(|(_, n)| (e, n)))
                    .filter(|(_, n)| names.iter().enumerate().all(|(i, x)| x != n || i == at));
                Some(OrderByExpr {
                    expr: match column {
                        Some((e, _)) => e.clone(),
                        None => Expr::Value(Value::Number((at + 1).to_string(), false).into()),
                    },
                    options: o.options.clone(),
                    with_fill: None,
                })
            })
            .collect::<Option<_>>()?,
        Some(_) => return None,
    };
    let distinct_on = matches!(sub.body.as_ref(),
        SetExpr::Select(i) if matches!(i.distinct, Some(Distinct::On(_))));
    if !sorted.is_empty() && distinct_on {
        return None;
    }
    // Its WHERE, each name of the subquery's columns read as what makes it: only where the subquery
    // is one plain SELECT and nothing it reads may be volatile.
    let condition = match &s.selection {
        None => None,
        Some(w) => {
            let made = made.as_ref()?;
            let SetExpr::Select(inner) = sub.body.as_ref() else {
                return None;
            };
            if folds(inner) || outputs::calls_volatile(w, release) {
                return None;
            }
            let mut w = w.clone();
            let mut by = Substitute {
                alias: &alias,
                made,
                release,
                failed: false,
                written: None,
            };
            let _ = VisitMut::visit(&mut w, &mut by);
            if by.failed {
                return None;
            }
            Some(w)
        }
    };
    Some(Moves {
        cut: cuts,
        sorted,
        condition,
    })
}

/// The place of the subquery's column an expression names, alone or after the subquery's alias;
/// none where it names another or more than one.
fn column_place(e: &Expr, alias: &Option<String>, names: &[String]) -> Option<usize> {
    let n = match e {
        Expr::Nested(x) => return column_place(x, alias, names),
        Expr::Identifier(i) => ident(i),
        Expr::CompoundIdentifier(parts) if parts.len() == 2 => {
            if alias.as_deref() != Some(ident(&parts[0]).as_str()) {
                return None;
            }
            ident(&parts[1])
        }
        _ => return None,
    };
    let mut at = names.iter().enumerate().filter(|(_, x)| **x == n).map(|(i, _)| i);
    match (at.next(), at.next()) {
        (Some(i), None) => Some(i),
        _ => None,
    }
}

/// Each name of the subquery's columns in a condition, written as what makes it; failed where the
/// condition names anything else, holds a subquery, or names a column made by what may be volatile.
struct Substitute<'s> {
    alias: &'s Option<String>,
    made: &'s [(String, Expr)],
    release: u8,
    failed: bool,
    /// The expression written as what makes a column, whose words are the subquery's own.
    written: Option<*const Expr>,
}

impl VisitorMut for Substitute<'_> {
    type Break = ();

    fn pre_visit_query(&mut self, _q: &mut Query) -> ControlFlow<()> {
        if self.written.is_some() {
            return ControlFlow::Continue(());
        }
        self.failed = true;
        ControlFlow::Break(())
    }

    fn pre_visit_expr(&mut self, e: &mut Expr) -> ControlFlow<()> {
        if self.written.is_some() {
            return ControlFlow::Continue(());
        }
        if matches!(e, Expr::Identifier(_) | Expr::CompoundIdentifier(_)) {
            let names: Vec<String> = self.made.iter().map(|(n, _)| n.clone()).collect();
            match column_place(e, self.alias, &names) {
                Some(i)
                    if !outputs::calls_volatile(&self.made[i].1, self.release)
                        && !pipeline::folds_or_makes_sets(&self.made[i].1) =>
                {
                    *e = Expr::Nested(Box::new(self.made[i].1.clone()));
                    self.written = Some(e as *const Expr);
                }
                _ => {
                    self.failed = true;
                    return ControlFlow::Break(());
                }
            }
        }
        ControlFlow::Continue(())
    }

    fn post_visit_expr(&mut self, e: &mut Expr) -> ControlFlow<()> {
        if self.written == Some(e as *const Expr) {
            self.written = None;
        }
        ControlFlow::Continue(())
    }
}

/// What moves of a query, moved into the subquery it reads alone.
fn moved(q: &mut Query, m: Moves) {
    let Moves {
        cut,
        sorted,
        condition,
    } = m;
    let (limit, fetch) = match cut {
        true => (q.limit_clause.take(), q.fetch.take()),
        false => (None, None),
    };
    let SetExpr::Select(s) = q.body.as_mut() else {
        return;
    };
    if condition.is_some() {
        s.selection = None;
    }
    let TableFactor::Derived { subquery, .. } = &mut s.from[0].relation else {
        return;
    };
    if cut {
        subquery.limit_clause = limit;
        subquery.fetch = fetch;
    }
    if !sorted.is_empty() {
        subquery.order_by = Some(OrderBy {
            kind: OrderByKind::Expressions(sorted),
            interpolate: None,
        });
    }
    if let (Some(w), SetExpr::Select(inner)) = (condition, subquery.body.as_mut()) {
        inner.selection = Some(match inner.selection.take() {
            Some(own) => Expr::BinaryOp {
                left: Box::new(Expr::Nested(Box::new(own))),
                op: BinaryOperator::And,
                right: Box::new(Expr::Nested(Box::new(w))),
            },
            None => w,
        });
    }
}
