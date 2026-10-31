//! A partial index's predicate: read as a question's WHERE on its table alone, it says which rows the
//! strand holds, routed there at the write, and whether a question's conditions imply it, so that the
//! strand serves the question.

use super::question::{self, Cond, Side, TextCond};
use super::statement::{Datum, Row};
use crate::catalog::{Catalog, ColumnType, Order, Shape, Table};
use crate::places::month_of;
use crate::values::Values;
use crate::write::Rows;
use crate::{Error, Result};
use std::ops::Bound::{self, Excluded, Included, Unbounded};

/// The rows a partial strand holds, as conditions on its table's columns.
#[derive(Clone, Debug)]
pub(crate) struct Predicate {
    side: Side,
}

impl Predicate {
    /// A predicate as written, read as the question path reads a WHERE on `table` alone: comparisons
    /// of a column with a value, ranges, IS NULL and IS NOT NULL, the truth tests.
    pub(crate) fn read(catalog: &Catalog, table: &Table, predicate: &str) -> Result<Predicate> {
        let q = question::parse(
            catalog,
            &format!("SELECT count(*) FROM {} WHERE {predicate}", table.name),
        )?;
        match q.sides.as_slice() {
            [side] if q.meets.is_empty() && q.after.is_empty() => {
                Ok(Predicate { side: side.clone() })
            }
            _ => Err(Error::new("a predicate reads its own table alone")),
        }
    }

    /// The predicate of an order, where its index has one.
    pub(crate) fn of(catalog: &Catalog, table: &Table, order: &Order) -> Result<Option<Predicate>> {
        order
            .predicate
            .as_deref()
            .map(|p| Predicate::read(catalog, table, p))
            .transpose()
    }

    /// The columns it reads, each once, in the table's order.
    pub(crate) fn columns(&self) -> Vec<usize> {
        let mut out: Vec<usize> = self
            .side
            .conds
            .iter()
            .map(|c| super::walk::s_column(c.shape))
            .chain(self.side.texts.iter().map(|t| t.column))
            .chain(self.side.nulls.iter().map(|n| n.0))
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Whether a row whose column `c` is `value(c)`, row `i` of the values given, passes it.
    fn admits_by<'v>(&self, table: &Table, value: &dyn Fn(usize) -> (&'v Values, usize)) -> bool {
        if self.side.empty {
            return false;
        }
        let conds = self.side.conds.iter().all(|c| {
            let (v, i) = value(super::walk::s_column(c.shape));
            let x = match c.shape {
                Shape::MonthUtc(_) => v.int(i).and_then(month_of).map(i128::from),
                _ => v.wide(i),
            };
            c.holds(x)
        });
        let texts = self.side.texts.iter().all(|t| {
            let (v, i) = value(t.column);
            let column = &table.columns[t.column];
            let Ok(coll) = column.orderer() else {
                return false;
            };
            v.text_value(i)
                .is_some_and(|x| t.holds(&crate::strings::column_key(column.ty, coll, x.data())))
        });
        let nulls = self.side.nulls.iter().all(|&(c, null)| {
            let (v, i) = value(c);
            v.is_null(i) == null
        });
        conds && texts && nulls
    }

    /// Whether row `r` of rows given column by column passes it.
    pub(crate) fn admits_at(&self, table: &Table, rows: &Rows, r: usize) -> bool {
        self.admits_by(table, &|c| (&rows.columns[c], r))
    }

    /// Whether a row a statement writes passes it.
    pub(crate) fn admits(&self, table: &Table, row: &Row) -> bool {
        let one: Vec<Values> = table
            .columns
            .iter()
            .zip(row)
            .map(|(col, d)| one_value(col.ty, d))
            .collect();
        self.admits_by(table, &|c| (&one[c], 0))
    }

    /// Whether every row a question's conditions keep passes it: each of its conditions is implied
    /// by one of the question's on the same column, a range within its range, a NULL kept only where
    /// it keeps one.
    pub(crate) fn implied_by(&self, q: &Side) -> bool {
        if q.empty {
            return true;
        }
        if self.side.empty {
            return false;
        }
        let conds = self.side.conds.iter().all(|p| {
            q.conds
                .iter()
                .any(|c| c.shape == p.shape && cond_within(c, p))
        });
        let texts = self.side.texts.iter().all(|p| {
            q.texts
                .iter()
                .any(|t| t.column == p.column && text_within(t, p))
        });
        // IS NOT NULL is implied by any comparison of the column, which no NULL passes.
        let nulls = self.side.nulls.iter().all(|&(c, null)| {
            q.nulls.contains(&(c, null))
                || (!null
                    && (q
                        .conds
                        .iter()
                        .any(|x| super::walk::s_column(x.shape) == c && !x.nulls)
                        || q.texts.iter().any(|t| t.column == c)))
        });
        conds && texts && nulls
    }
}

/// A row's value of a column, as one value of that column's kind.
fn one_value(ty: ColumnType, d: &Datum) -> Values {
    let mut v = Values::new(ty);
    match d {
        Datum::Null => v.push_null(),
        d => super::change::push(&mut v, d),
    }
    v
}

/// Whether every value `c` keeps, `p` keeps.
fn cond_within(c: &Cond, p: &Cond) -> bool {
    if c.nulls && !p.nulls {
        return false;
    }
    if c.kept.lo > c.kept.hi {
        return true;
    }
    c.kept.lo >= p.kept.lo
        && c.kept.hi <= p.kept.hi
        && p.except
            .iter()
            .all(|x| !c.kept.holds(*x) || c.except.contains(x))
}

/// Whether a lower bound `a` keeps no value `b` leaves out: `a` at or above `b`.
fn lower_within(a: &Bound<Vec<u8>>, b: &Bound<Vec<u8>>) -> bool {
    match (a, b) {
        (_, Unbounded) => true,
        (Unbounded, _) => false,
        (Included(x), Included(y)) | (Excluded(x), Excluded(y)) | (Excluded(x), Included(y)) => {
            x >= y
        }
        (Included(x), Excluded(y)) => x > y,
    }
}

/// Whether an upper bound `a` keeps no value `b` leaves out: `a` at or below `b`.
fn upper_within(a: &Bound<Vec<u8>>, b: &Bound<Vec<u8>>) -> bool {
    match (a, b) {
        (_, Unbounded) => true,
        (Unbounded, _) => false,
        (Included(x), Included(y)) | (Excluded(x), Excluded(y)) | (Excluded(x), Included(y)) => {
            x <= y
        }
        (Included(x), Excluded(y)) => x < y,
    }
}

/// Whether every value `t` keeps, `p` keeps.
fn text_within(t: &TextCond, p: &TextCond) -> bool {
    lower_within(&t.lo, &p.lo)
        && upper_within(&t.hi, &p.hi)
        && p.except.iter().all(|x| !t.holds(x) || t.except.contains(x))
}
