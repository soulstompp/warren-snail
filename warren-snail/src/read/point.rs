//! A question on one table priced as one rescan of a scan whose columns each take one value, given
//! only as the rescan begins: a scan inside another engine's nested loop, run once for each of its
//! outer rows.

use super::question::{Cond, Interval, Side, TextCond};
use super::walk::{condition_columns, Want};
use super::Snail;
use crate::catalog::{Level, Order, Shape, Table};
use crate::directory::{decode_page, read_map, Entry};
use crate::places::{column_byte_key, KeyValue};
use crate::{Error, Result};
use std::ops::Bound;

/// One rescan of a question, where each of a set of columns takes one value the rescan is given:
/// the round for a key the order those values fix holds, priced from the directory as every round
/// is, every order that serves the question priced from its top, the one read walked down to its
/// blocks and predicted from their page maps. The order read, its directory and column pages, and the
/// rows the key holds. Nothing is held between rescans, so every rescan reads its pages again.
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
    /// The rows the key holds, as the counts bound them, and the estimate from its page maps.
    pub rows_low: u64,
    pub rows_high: u64,
    pub rows_estimate: u64,
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
            Level::Scaled(c, k) => Shape::Scaled(c, k),
        };
        side.conds
            .iter()
            .any(|x| x.shape == shape && x.is_range() && x.kept.lo == x.kept.hi)
    };
    order.levels.iter().take_while(|&&l| equal(l)).count()
}

/// The entry among `held` the middle of their rows lies under; where they count none, the first.
fn middle_entry<'a>(held: impl Iterator<Item = &'a Entry>) -> Option<&'a Entry> {
    let held: Vec<&Entry> = held.collect();
    let rows: u64 = held.iter().map(|e| e.rows as u64).sum();
    let mut seen = 0;
    held.iter()
        .copied()
        .find(|e| {
            seen += e.rows as u64;
            seen * 2 > rows
        })
        .or(held.first().copied())
}

/// The keys the question's own equalities give the first `own` levels of `order`, as the order keys
/// them.
fn own_keys(order: &Order, table: &Table, side: &Side, own: usize) -> Vec<KeyValue> {
    order.levels[..own]
        .iter()
        .map(|&l| {
            let c = l.column();
            let column = &table.columns[c];
            if matches!(l, Level::Column(_))
                && (column.ty.keyed_by_text() || column.ty.keyed_by_bytes())
            {
                let t = side.texts.iter().find(|t| t.column == c).unwrap();
                let Bound::Included(v) = &t.lo else {
                    unreachable!()
                };
                return column_byte_key(column, v);
            }
            let shape = match l {
                Level::Column(c) => Shape::Column(c),
                Level::MonthUtc(c) => Shape::MonthUtc(c),
                Level::Scaled(c, k) => Shape::Scaled(c, k),
            };
            let v = side
                .conds
                .iter()
                .find(|x| x.shape == shape && x.is_range() && x.kept.lo == x.kept.hi)
                .unwrap()
                .kept
                .lo;
            if column.ty.is_wide() {
                KeyValue::Wide(v)
            } else {
                KeyValue::Int(v as i64)
            }
        })
        .collect()
}

/// The question's own conditions with a column fixed to one key, as a literal there fixes it.
fn fixing(side: &mut Side, column: usize, key: &KeyValue) -> bool {
    side.conds.retain(|x| x.shape != Shape::Column(column));
    side.texts.retain(|t| t.column != column);
    match key {
        KeyValue::Int(v) => side.conds.push(Cond::new(
            Shape::Column(column),
            Interval::at(i128::from(*v)),
        )),
        KeyValue::Wide(v) => side
            .conds
            .push(Cond::new(Shape::Column(column), Interval::at(*v))),
        KeyValue::Text(_) | KeyValue::Bytes(_) => {
            let bytes = match key {
                KeyValue::Text(s) => s.as_bytes().to_vec(),
                KeyValue::Bytes(b) => b.to_vec(),
                _ => unreachable!(),
            };
            side.texts.push(TextCond::new(
                column,
                Bound::Included(bytes.clone()),
                Bound::Included(bytes),
            ))
        }
        KeyValue::Null { .. } => return false,
    }
    true
}

impl Snail {
    /// A question on one table priced as one rescan in which each column `given` names takes one
    /// value, on the root the snail last read: along the order the values fix furthest, the round
    /// for a key that order holds, from the directory alone. The key is the first of a data page in
    /// the order's middle block among those holding the question's own values. None where the values
    /// fix no level of any order beyond the question's own conditions, or no block holds the
    /// question's own values: each rescan then reads what the question alone reads, or nothing.
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
        self.point_of(&want, &given)
    }

    /// The round of a key along the order the columns `given` fix furthest, as `price_point` prices
    /// it, for a table's part of a round as `want` reads it.
    pub(super) fn point_of(&self, want: &Want, given: &[usize]) -> Result<Option<Point>> {
        let table = want.table;
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
        // The order the values fix furthest, the fewest levels its own conditions fix first.
        let fixed = served
            .iter()
            .map(|&o| {
                let order = &table.orders[o];
                let own = fixed_levels(order, table, want.side, &[]);
                (o, own, fixed_levels(order, table, want.side, given))
            })
            .filter(|&(_, own, levels)| levels > own)
            .max_by_key(|&(o, own, levels)| (levels - own, std::cmp::Reverse(o)));
        let Some((o, own, levels)) = fixed else {
            return Ok(None);
        };
        let order = &table.orders[o];
        let keys = own_keys(order, table, want.side, own);
        let holds = |e: &Entry| {
            let (first, last, open) = super::walk::narrowed(e, order, table);
            first[..own] <= keys[..]
                && if open && crate::directory::kept_len(&last) <= own {
                    keys[..] < last[..own]
                } else {
                    keys[..] <= last[..own]
                }
        };
        // Down the order to its middle block among those holding the question's own values: the one
        // their middle row lies in, as each entry counts the rows under it.
        let (head, mut entries, _) = self.top(table, o)?;
        for _ in 1..head.height {
            let Some(middle) = middle_entry(entries.iter().filter(|e| holds(e))) else {
                return Ok(None);
            };
            entries = decode_page(
                &self.reader.read_extent(middle.child)?,
                &head,
                middle.bound(),
            )?;
        }
        let Some(block) = middle_entry(entries.iter().filter(|e| holds(e))) else {
            return Ok(None);
        };
        let map = read_map(&self.reader, block, &head.level_text)?;
        // The key: the first of a data page beginning on the question's own values, the middle of
        // them; where none begins on them, their values with the rest of the page they begin in,
        // which lies in that page's range as every key on them does.
        let firsts: Vec<&Vec<KeyValue>> = std::iter::once(&block.first)
            .chain(
                map.columns
                    .first()
                    .into_iter()
                    .flat_map(|c| c.pages.iter().map(|p| &p.first)),
            )
            .filter(|k| k.len() >= levels)
            .collect();
        let on: Vec<&&Vec<KeyValue>> = firsts.iter().filter(|k| k[..own] == keys[..]).collect();
        let rest = match on.get(on.len() / 2) {
            Some(k) => &k[own..levels],
            None => match firsts.iter().rev().find(|k| k[..own] < keys[..]) {
                Some(k) => &k[own..levels],
                None => return Ok(None),
            },
        };
        let mut side = want.side.clone();
        for (l, k) in order.levels[own..levels].iter().zip(rest) {
            if !fixing(&mut side, l.column(), k) {
                return Ok(None);
            }
        }
        let point = Want {
            table,
            side: &side,
            columns: want.columns.clone(),
            countable: want.countable,
            keys: None,
        };
        let before = self.touched();
        let part = self.part(&point, None)?;
        Ok(Some(Point {
            order: table.orders[part.plan.walk.order].name.clone(),
            levels,
            unique: order.unique && levels == order.levels.len(),
            directory_pages: self.touched() - before,
            column_pages: part.plan.column_pages,
            rows_low: part.rows.0,
            rows_high: part.rows.1,
            rows_estimate: (part.plan.rows_estimate.round() as u64).clamp(part.rows.0, part.rows.1),
        }))
    }
}
