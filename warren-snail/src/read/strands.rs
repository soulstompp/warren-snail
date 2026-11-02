//! The strand a question's windows read along, a partition at a time: one the DDL declares whose
//! levels lead with their PARTITION BY's columns, then their ORDER BY's, and that holds every
//! column the table stores. Taken where its pages from the file are fewer than the table's own
//! order's with the sort it saves.

use super::walk::Want;
use super::{Part, Snail};
use crate::catalog::{ColumnType, Level, Method, Order, Table};
use crate::cut::PAGE;
use crate::Result;
use sqlparser::ast::{Expr, GroupByExpr, OrderBySort, Query, SelectItem, SetExpr, WindowType};

/// The bytes a value of a column is held at, at least: a fixed type's as the spill writes it, any
/// other's its length's.
fn held_width(ty: ColumnType) -> u64 {
    match ty {
        ColumnType::Int2 | ColumnType::Int4 | ColumnType::Int8 | ColumnType::Timestamptz => 9,
        ColumnType::Float8 => 9,
        ColumnType::Float4 => 5,
        ColumnType::Uuid => 17,
        _ => 5,
    }
}

/// The strand a question's own windows may read along, and the places, among the columns the table
/// stores, of the columns its levels lead with: none where the question is no one SELECT of the
/// table whose windows share one PARTITION BY of columns and one ORDER BY of columns, ascending.
pub(super) fn of_windows(q: &Query, table: &Table, stored: &[usize]) -> Option<(usize, Vec<usize>)> {
    if q.with.is_some() {
        return None;
    }
    let SetExpr::Select(s) = q.body.as_ref() else {
        return None;
    };
    let grouped = !matches!(&s.group_by, GroupByExpr::Expressions(v, _) if v.is_empty());
    if s.from.len() != 1
        || !s.from[0].joins.is_empty()
        || grouped
        || s.having.is_some()
        || super::pipeline::aggregates(s)
    {
        return None;
    }
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
    let first = specs.first()?;
    let text = |es: &mut dyn Iterator<Item = String>| es.collect::<Vec<_>>();
    let parts = |s: &sqlparser::ast::WindowSpec| text(&mut s.partition_by.iter().map(|e| e.to_string()));
    let orders = |s: &sqlparser::ast::WindowSpec| text(&mut s.order_by.iter().map(|e| e.to_string()));
    if specs
        .iter()
        .any(|s| parts(s) != parts(first) || orders(s) != orders(first))
        || first.partition_by.is_empty()
    {
        return None;
    }
    let column = |e: &Expr| -> Option<usize> {
        let (_, n) = super::pipeline::column_of(e)?;
        table.columns.iter().position(|c| c.name == n)
    };
    let by: Vec<usize> = first
        .partition_by
        .iter()
        .map(column)
        .collect::<Option<_>>()?;
    let mut ordered = Vec::new();
    for o in &first.order_by {
        if matches!(o.options.sort, Some(OrderBySort::Desc)) || o.options.nulls_first == Some(true)
        {
            return None;
        }
        ordered.push(column(&o.expr)?);
    }
    let lead = by.len() + ordered.len();
    let fits = |o: &Order| -> bool {
        o.method == Method::Btree
            && o.predicate.is_none()
            && o.levels.len() >= lead
            && (0..lead).all(|i| !o.desc[i] && !o.nulls_first[i])
            && o.levels[..by.len()].iter().all(|l| match l {
                Level::Column(c) => by.contains(c),
                _ => false,
            })
            && (0..by.len()).all(|i| {
                (0..i).all(|j| o.levels[i] != o.levels[j])
            })
            && o.levels[by.len()..lead]
                .iter()
                .zip(&ordered)
                .all(|(l, &c)| *l == Level::Column(c))
            && stored.iter().all(|&c| o.slot(c).is_some())
    };
    let oi = (0..table.orders.len()).find(|&i| fits(&table.orders[i]))?;
    let at = table.orders[oi].levels[..lead]
        .iter()
        .map(|l| stored.iter().position(|&c| *l == Level::Column(c)))
        .collect::<Option<Vec<usize>>>()?;
    Some((oi, at))
}

impl Snail {
    /// Whether a strand's pages from the file are fewer than the table's own order's together with
    /// the sort the strand saves: a sort that fits the round's room costs no page, one past it its
    /// spill, written and read back, priced from the rows the counts give the table at their held
    /// width.
    pub(super) fn strand_wins(
        &self,
        base: &super::question::Question,
        table: &Table,
        strand: usize,
        own: &Order,
        stored: &[usize],
    ) -> Result<bool> {
        if table.orders[strand].table_order {
            return Ok(true);
        }
        let (want, _): (Want, _) = self.want_of(base);
        let along = self.part(&want, Some(&own.name))?;
        let by = self.part(&want, Some(&table.orders[strand].name))?;
        let from_file =
            |p: &Part| p.plan.column_pages - self.reader.held_among(p.plan.disk_pages());
        let width: u64 = stored
            .iter()
            .map(|&c| held_width(table.columns[c].ty))
            .sum();
        let room = self.reader.room().sizes().bytes;
        let sort = 2 * (along.table_rows * width)
            .saturating_sub(room)
            .div_ceil(PAGE as u64);
        Ok(from_file(&by) < from_file(&along) + sort)
    }
}
