//! A question on one table priced as one rescan of a scan whose columns each take one value, given
//! only as the rescan begins: a scan inside another engine's nested loop, run once for each of its
//! outer rows.

use super::question::Side;
use super::walk::condition_columns;
use super::Snail;
use crate::catalog::{Level, Order, Shape, Table};
use crate::directory::{decode_page, read_map};
use crate::{Error, Result};
use std::ops::Bound;

/// One rescan of a question, priced from the directory where each of a set of columns takes one value
/// the rescan is given: along the order whose leading levels those values fix further than the
/// question's own conditions do, a directory page on each level from its top down to the order's
/// middle block, that block's page map, and of each leaf the round reads there its dictionary and
/// the data page holding the block's middle row, as the value of that row is read where its rows lie
/// in one data page; and the top of every other order, each of which a round prices. Nothing is
/// held between rescans, so every rescan reads them all again.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Point {
    pub order: String,
    /// The leading levels of the order the values and the question's own equalities fix.
    pub levels: usize,
    /// Whether they fix every level of an order that holds each key once: a rescan finds a row at
    /// most.
    pub unique: bool,
    pub directory_pages: u64,
    pub column_pages: u64,
}

/// The leading levels of `order` that `side`'s equalities and a value for each of `given`'s
/// columns fix, one after another.
fn fixed_levels(order: &Order, table: &Table, side: &Side, given: &[usize]) -> usize {
    let equal = |l: Level| -> bool {
        let c = l.column();
        if matches!(l, Level::Column(_)) && given.contains(&c) {
            return true;
        }
        let ty = table.columns[c].ty;
        if matches!(l, Level::Column(_)) && (ty.keyed_by_text() || ty.keyed_by_bytes()) {
            return side.texts.iter().any(|t| {
                t.column == c
                    && matches!((&t.lo, &t.hi), (Bound::Included(a), Bound::Included(b)) if a == b)
            });
        }
        let shape = match l {
            Level::Column(c) => Shape::Column(c),
            Level::MonthUtc(c) => Shape::MonthUtc(c),
        };
        side.conds
            .iter()
            .any(|x| x.shape == shape && x.is_range() && x.kept.lo == x.kept.hi)
    };
    order.levels.iter().take_while(|&&l| equal(l)).count()
}

impl Snail {
    /// A question on one table priced as one rescan in which each column `given` names takes one
    /// value, on the root the snail last read: along the order the values fix, a point read, priced
    /// from the directory and the page map of the order's middle block. None where the values fix
    /// no level of any order beyond the question's own conditions, or the table holds no row: each
    /// rescan then reads what the question alone reads.
    pub fn price_point(&self, sql: &str, given: &[&str]) -> Result<Option<Point>> {
        let q = self.one_table(sql)?;
        let (want, _) = self.want_of(&q);
        let table = want.table;
        let given: Vec<usize> = given
            .iter()
            .map(|n| {
                table
                    .columns
                    .iter()
                    .position(|c| c.name == *n)
                    .ok_or_else(|| Error::new(format!("{} has no column {n}", table.name)))
            })
            .collect::<Result<_>>()?;
        let mut read = condition_columns(want.side);
        read.extend(want.columns.iter().copied());
        read.extend(given.iter().copied());
        let served: Vec<usize> = (0..table.orders.len())
            .filter(|&o| read.iter().all(|&c| table.orders[o].slot(c).is_some()))
            .collect();
        if served.is_empty() {
            return Err(Error::new(format!(
                "no order of {} holds every column the question reads",
                table.name
            )));
        }
        let tops: Vec<u64> = served
            .iter()
            .map(|&o| self.top_pages(table, o))
            .collect::<Result<_>>()?;
        let mut best: Option<Point> = None;
        for (i, &o) in served.iter().enumerate() {
            let order = &table.orders[o];
            let levels = fixed_levels(order, table, want.side, &given);
            if levels <= fixed_levels(order, table, want.side, &[]) {
                continue;
            }
            let (head, mut entries, _) = self.top(table, o)?;
            let mut directory_pages = tops[i];
            for _ in 1..head.height {
                let Some(middle) = entries.get(entries.len() / 2) else {
                    break;
                };
                directory_pages += middle.child.pages();
                entries = decode_page(&self.reader.read_extent(middle.child)?, &head)?;
            }
            let Some(block) = entries.get(entries.len() / 2) else {
                continue;
            };
            let map = read_map(&self.reader, block, &head.level_text)?;
            directory_pages += block.map_run().0.pages();
            // A column read is each of its leaves read: those the answer takes, those the conditions
            // test, and the levels the values fix, which a block read in part is searched on. Each
            // is read in the data page holding the block's middle row, its dictionary beside it.
            let mut columns = read.clone();
            columns.extend(order.levels[..levels].iter().map(|l| l.column()));
            let middle = map.sorted / 2;
            let column_pages = order
                .leaves(table)
                .iter()
                .enumerate()
                .filter(|(_, (c, _))| columns.contains(c))
                .map(|(s, _)| {
                    let cp = &map.columns[s];
                    cp.dictionary.map_or(0, |d| d.pages())
                        + cp.pages
                            .iter()
                            .find(|p| p.first_row <= middle && middle < p.first_row + p.rows)
                            .map_or(0, |p| p.at.pages())
                })
                .sum();
            directory_pages += tops
                .iter()
                .enumerate()
                .filter(|&(j, _)| j != i)
                .map(|(_, p)| p)
                .sum::<u64>();
            let point = Point {
                order: order.name.clone(),
                levels,
                unique: order.unique && levels == order.levels.len(),
                directory_pages,
                column_pages,
            };
            let pages = |p: &Point| p.directory_pages + p.column_pages;
            if best.as_ref().is_none_or(|b| pages(&point) < pages(b)) {
                best = Some(point);
            }
        }
        Ok(best)
    }

    /// The pages an order's top lies in, as the root names it.
    fn top_pages(&self, table: &Table, order: usize) -> Result<u64> {
        let name = &table.orders[order].name;
        self.root
            .orders
            .iter()
            .find(|(t, o, _)| *t == table.name && o == name)
            .map(|(_, _, e)| e.pages())
            .ok_or_else(|| {
                Error::new(format!(
                    "{}: the root names no directory for {name}",
                    table.name
                ))
            })
    }
}
