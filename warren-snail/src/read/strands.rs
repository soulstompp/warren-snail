//! The strand a question's rows read along where it gives the order a step would hold them to get:
//! one the DDL declares whose levels lead with its windows' PARTITION BY's columns, then their ORDER
//! BY's; with its ORDER BY's columns; or with its GROUP BY's columns, then the ORDER BY of an
//! aggregate a caller handed in; and that holds every column the step reads. Taken where its pages
//! from the file are fewer than another order's with the sort it saves.

use super::walk::Want;
use super::{Part, Snail};
use crate::catalog::{ColumnType, Level, Method, Order, Table};
use crate::cut::PAGE;
use crate::Result;
use sqlparser::ast::{
    Expr, FunctionArgumentClause, FunctionArguments, GroupByExpr, OrderByExpr, OrderByKind,
    Query, Select, SelectItem, SetExpr, WindowType,
};

/// The bytes a value of a column is held at, at least: a fixed type's as the spill writes it, any
/// other's its length's.
pub(super) fn held_width(ty: ColumnType) -> u64 {
    match ty {
        ColumnType::Int2 | ColumnType::Int4 | ColumnType::Int8 | ColumnType::Timestamptz => 9,
        ColumnType::Float8 => 9,
        ColumnType::Float4 => 5,
        ColumnType::Uuid => 17,
        _ => 5,
    }
}

/// What a step does with rows that come in a strand's order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Led {
    /// Its windows fold each partition once its rows have come.
    Partitions,
    /// Its ORDER BY holds nothing, its rows handed on as they come.
    Ordered,
    /// Its aggregates end each group as its key changes.
    Groups,
}

/// What a step's rows would come in the order of: the expressions they lead by in any order among
/// themselves, then those they lead by in turn.
pub(super) struct Wants<'q> {
    pub(super) led: Led,
    pub(super) by: Vec<&'q Expr>,
    pub(super) ordered: Vec<&'q Expr>,
}

/// Each ascending column of an ORDER BY, NULLs last; none where one is otherwise.
fn ascending(es: &[OrderByExpr]) -> Option<Vec<&Expr>> {
    es.iter()
        .map(|o| {
            super::ordering::ascending(&o.options).then_some(&o.expr)
        })
        .collect()
}

/// The windows' one PARTITION BY and ORDER BY, where every window shares them and none lies inside
/// an expression.
fn of_windows(s: &Select) -> Option<Wants<'_>> {
    let mut specs = Vec::new();
    for item in &s.projection {
        let e = match item {
            SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => e,
            _ => continue,
        };
        match e {
            Expr::Function(f) if matches!(f.over, Some(WindowType::WindowSpec(_))) => {
                let Some(WindowType::WindowSpec(spec)) = &f.over else {
                    unreachable!()
                };
                specs.push(spec);
            }
            e if e.to_string().to_uppercase().contains(" OVER ") => return None,
            _ => {}
        }
    }
    let first = *specs.first()?;
    let text = |es: &mut dyn Iterator<Item = String>| es.collect::<Vec<_>>();
    let parts =
        |s: &sqlparser::ast::WindowSpec| text(&mut s.partition_by.iter().map(|e| e.to_string()));
    let orders =
        |s: &sqlparser::ast::WindowSpec| text(&mut s.order_by.iter().map(|e| e.to_string()));
    if specs
        .iter()
        .any(|s| parts(s) != parts(first) || orders(s) != orders(first))
        || first.partition_by.is_empty()
    {
        return None;
    }
    Some(Wants {
        led: Led::Partitions,
        by: first.partition_by.iter().collect(),
        ordered: ascending(&first.order_by)?,
    })
}

/// The GROUP BY's columns, and the ORDER BY of the aggregate a caller handed in among the step's
/// items, where one is; every such aggregate sharing it.
fn of_groups<'q>(s: &'q Select, grouping: &'q [Expr]) -> Option<Wants<'q>> {
    let mut ordered: Option<Vec<&Expr>> = None;
    for item in &s.projection {
        let e = match item {
            SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => e,
            _ => continue,
        };
        let Expr::Function(f) = e else {
            continue;
        };
        if !super::handed::is_aggregate(&super::pipeline::function_name(f)) {
            continue;
        }
        let FunctionArguments::List(l) = &f.args else {
            continue;
        };
        let mut this = Vec::new();
        for c in &l.clauses {
            if let FunctionArgumentClause::OrderBy(os) = c {
                this = ascending(os)?;
            }
        }
        match &ordered {
            Some(o)
                if o.iter()
                    .map(|e| e.to_string())
                    .ne(this.iter().map(|e| e.to_string())) =>
            {
                return None
            }
            _ => ordered = Some(this),
        }
    }
    Some(Wants {
        led: Led::Groups,
        by: grouping.iter().collect(),
        ordered: ordered?,
    })
}

/// What a SELECT's rows would come in the order of, where a strand may give it.
pub(super) fn wants(q: &Query) -> Option<Wants<'_>> {
    let SetExpr::Select(s) = q.body.as_ref() else {
        return None;
    };
    if s.from.len() != 1 || super::pipeline::says_distinct(s) || s.having.is_some() {
        return None;
    }
    let grouping: &[Expr] = match &s.group_by {
        // Grouping sets come in the order of none of their sets.
        GroupByExpr::Expressions(v, _) if !super::grouping::written(v) => v,
        _ => return None,
    };
    if !grouping.is_empty() {
        return of_groups(s, grouping);
    }
    if super::pipeline::aggregates(s) {
        return None;
    }
    if let Some(w) = of_windows(s) {
        return Some(w);
    }
    let Some(OrderByKind::Expressions(os)) = q.order_by.as_ref().map(|o| &o.kind) else {
        return None;
    };
    // A name an item takes as its alias reads that item, not the column.
    let aliased = |n: &str| {
        s.projection.iter().any(|i| match i {
            SelectItem::ExprWithAlias { alias, .. } => crate::catalog::ident(alias) == n,
            _ => false,
        })
    };
    let ordered = ascending(os)?;
    if ordered
        .iter()
        .any(|e| super::pipeline::column_of(e).is_none_or(|(_, n)| aliased(&n)))
    {
        return None;
    }
    Some(Wants {
        led: Led::Ordered,
        by: Vec::new(),
        ordered,
    })
}

/// The strand a declared B-tree gives what a step wants: `by` the columns it leads with in any order
/// among themselves, then `ordered` in turn, each ascending, and holding each of `needs`.
pub(super) fn led_by(
    table: &Table,
    by: &[usize],
    ordered: &[usize],
    needs: &[usize],
) -> Option<usize> {
    let lead = by.len() + ordered.len();
    if lead == 0 {
        return None;
    }
    let fits = |o: &Order| -> bool {
        o.method == Method::Btree
            && o.predicate.is_none()
            && o.levels.len() >= lead
            && (0..lead).all(|i| {
                !o.desc[i] && !o.nulls_first[i] && !table.columns[o.levels[i].column()].collated()
            })
            && o.levels[..by.len()].iter().all(|l| match l {
                Level::Column(c) => by.contains(c),
                _ => false,
            })
            && (0..by.len()).all(|i| (0..i).all(|j| o.levels[i] != o.levels[j]))
            && o.levels[by.len()..lead]
                .iter()
                .zip(ordered)
                .all(|(l, &c)| *l == Level::Column(c))
            && needs.iter().all(|&c| o.holds(c))
    };
    (0..table.orders.len()).find(|&i| fits(&table.orders[i]))
}

/// Whether a SELECT over one table sorts as one of the table's declared B-trees read backward: each
/// key a column of the table, in a level's turn, each direction and NULLs' place every one the
/// opposite of the level's, as 18.6's plan reads such an index backward for it.
pub(super) fn backward(q: &Query, table: &Table) -> bool {
    let SetExpr::Select(s) = q.body.as_ref() else {
        return false;
    };
    if q.with.is_some() || s.from.len() != 1 || !s.from[0].joins.is_empty() {
        return false;
    }
    let Some(OrderByKind::Expressions(os)) = q.order_by.as_ref().map(|o| &o.kind) else {
        return false;
    };
    // A name an item takes as its alias reads that item, not the column.
    let aliased = |n: &str| {
        s.projection.iter().any(|i| match i {
            SelectItem::ExprWithAlias { alias, .. } => crate::catalog::ident(alias) == n,
            _ => false,
        })
    };
    let keys: Option<Vec<(usize, bool, bool)>> = os
        .iter()
        .map(|o| {
            let (_, n) = super::pipeline::column_of(&o.expr)?;
            if aliased(&n) {
                return None;
            }
            let c = table.columns.iter().position(|c| c.name == n)?;
            let desc = super::ordering::descends(&o.options);
            Some((c, desc, o.options.nulls_first.unwrap_or(desc)))
        })
        .collect();
    let Some(keys) = keys.filter(|k| !k.is_empty()) else {
        return false;
    };
    table.orders.iter().any(|o| {
        o.method == Method::Btree
            && o.predicate.is_none()
            && o.levels.len() >= keys.len()
            && keys.iter().enumerate().all(|(i, &(c, desc, first))| {
                o.levels[i] == Level::Column(c)
                    && desc != o.desc[i]
                    && first != o.nulls_first[i]
                    && !table.columns[c].collated()
            })
    })
}

/// Whether a SELECT over one table has its windows' rows come in the order the windows read them
/// with no sort, as 18.6's plan reads the table beneath its WindowAgg: every window sharing one
/// PARTITION BY and ORDER BY, its keys (each PARTITION BY key ascending) the columns of one of the
/// table's B-trees in its levels' turn, read forward or backward; windows that order nothing take
/// the query's ORDER BY, and with none the rows come as the FROM gives them. An ORDER BY of the
/// query beside windows that order their rows is theirs alone, partitioned by nothing.
pub(super) fn gives_windows(q: &Query, table: &Table) -> bool {
    let SetExpr::Select(s) = q.body.as_ref() else {
        return false;
    };
    if q.with.is_some() || s.from.len() != 1 || !s.from[0].joins.is_empty() {
        return false;
    }
    let mut specs = Vec::new();
    for item in &s.projection {
        let e = match item {
            SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => e,
            _ => continue,
        };
        match e {
            Expr::Function(f) if f.over.is_some() => match &f.over {
                Some(WindowType::WindowSpec(spec)) if spec.window_name.is_none() => specs.push(spec),
                _ => return false,
            },
            e if e.to_string().to_uppercase().contains(" OVER ") => return false,
            _ => {}
        }
    }
    let Some(&first) = specs.first() else {
        return false;
    };
    let parts = |s: &sqlparser::ast::WindowSpec| -> Vec<String> {
        s.partition_by.iter().map(|e| e.to_string()).collect()
    };
    let orders = |os: &[OrderByExpr]| -> Vec<String> { os.iter().map(|o| o.to_string()).collect() };
    if specs
        .iter()
        .any(|s| parts(s) != parts(first) || orders(&s.order_by) != orders(&first.order_by))
    {
        return false;
    }
    let query: &[OrderByExpr] = match q.order_by.as_ref().map(|o| &o.kind) {
        None => &[],
        Some(OrderByKind::Expressions(os)) => os,
        Some(_) => return false,
    };
    let ordered = !first.partition_by.is_empty() || !first.order_by.is_empty();
    if ordered
        && !query.is_empty()
        && (!first.partition_by.is_empty() || orders(query) != orders(&first.order_by))
    {
        return false;
    }
    // A name an item takes as its alias reads that item, not the column.
    let aliased = |n: &str| {
        s.projection.iter().any(|i| match i {
            SelectItem::ExprWithAlias { alias, .. } => crate::catalog::ident(alias) == n,
            _ => false,
        })
    };
    let column = |e: &Expr| -> Option<usize> {
        let (_, n) = super::pipeline::column_of(e)?;
        if aliased(&n) {
            return None;
        }
        table.columns.iter().position(|c| c.name == n)
    };
    let key = |o: &OrderByExpr| -> Option<(usize, bool, bool)> {
        let desc = super::ordering::descends(&o.options);
        Some((column(&o.expr)?, desc, o.options.nulls_first.unwrap_or(desc)))
    };
    let keys: Option<Vec<(usize, bool, bool)>> = match ordered {
        true => first
            .partition_by
            .iter()
            .map(|e| Some((column(e)?, false, false)))
            .chain(first.order_by.iter().map(key))
            .collect(),
        false => query.iter().map(key).collect(),
    };
    let Some(keys) = keys else {
        return false;
    };
    keys.is_empty()
        || table.orders.iter().any(|o| {
            o.method == Method::Btree
                && o.predicate.is_none()
                && o.levels.len() >= keys.len()
                && [false, true].into_iter().any(|back| {
                    keys.iter().enumerate().all(|(i, &(c, desc, first))| {
                        o.levels[i] == Level::Column(c)
                            && (desc != o.desc[i]) == back
                            && (first != o.nulls_first[i]) == back
                            && !table.columns[c].collated()
                    })
                })
        })
}

/// The strand a question on one table may read along, the places among the columns the table stores
/// of the columns its levels lead with, and what its rows coming so do for it.
pub(super) fn of_step(
    q: &Query,
    table: &Table,
    stored: &[usize],
) -> Option<(usize, Vec<usize>, Led)> {
    if q.with.is_some() {
        return None;
    }
    let w = wants(q)?;
    let SetExpr::Select(s) = q.body.as_ref() else {
        return None;
    };
    if !s.from[0].joins.is_empty() {
        return None;
    }
    let column = |e: &&Expr| -> Option<usize> {
        let (_, n) = super::pipeline::column_of(e)?;
        table.columns.iter().position(|c| c.name == n)
    };
    let by: Vec<usize> = w.by.iter().map(column).collect::<Option<_>>()?;
    let ordered: Vec<usize> = w.ordered.iter().map(column).collect::<Option<_>>()?;
    let oi = led_by(table, &by, &ordered, stored)?;
    let lead = by.len() + ordered.len();
    let at = table.orders[oi].levels[..lead]
        .iter()
        .map(|l| stored.iter().position(|&c| *l == Level::Column(c)))
        .collect::<Option<Vec<usize>>>()?;
    Some((oi, at, w.led))
}

impl Snail {
    /// Whether a strand's pages from the file, with what its walk writes, are fewer than another
    /// order's together with the sort the strand saves: `own` where the round reads along it else, or
    /// the order of fewest pages. What fits the round's room costs no page; past it, the rows past
    /// what it holds are written and read back, priced from the rows the counts give the question:
    /// a sort's at the bytes it holds a row at, a nearest walk's at a whole row each.
    pub(super) fn strand_wins(
        &self,
        q: &super::question::Question,
        table: &Table,
        strand: usize,
        own: Option<&str>,
    ) -> Result<bool> {
        if table.orders[strand].table_order {
            return Ok(true);
        }
        let (want, _): (Want, _) = self.want_of(q);
        let along = self.part(&want, own)?;
        if table.orders[along.plan.walk.order].name == table.orders[strand].name {
            return Ok(true);
        }
        let by = self.part(&want, Some(&table.orders[strand].name))?;
        let from_file = |p: &Part| p.plan.column_pages - p.plan.held;
        let width: u64 = want
            .columns
            .iter()
            .map(|&c| held_width(table.columns[c].ty))
            .sum();
        let room = self.reader.room().sizes().bytes;
        let rows = along.rows.1;
        let past = |held: u64| {
            2 * (rows.saturating_sub(room / held.max(1)) * width).div_ceil(PAGE as u64)
        };
        let sort = past(super::pipeline::sort_held_bytes(width, width));
        let walk = match table.orders[strand].method {
            Method::Gist | Method::Spgist => past(
                super::nearest::ring_row_bytes(want.columns.len()) + width,
            ),
            _ => 0,
        };
        Ok(from_file(&by) + walk < from_file(&along) + sort)
    }
}
