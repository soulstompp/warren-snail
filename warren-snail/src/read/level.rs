//! The rows on each place of the level an order leads with, read from its directory: each entry
//! whose rows all lie on one place counted whole, and below the entries that hold several, each
//! page by the place it begins on, down to the page. An entry's places are bounded by its keys and
//! by the least and greatest its leaves keep.

use super::statement::Datum;
use super::walk::narrowed;
use super::Snail;
use crate::catalog::{Column, ColumnType, Level as Leading, Order, Table};
use crate::directory::{decode_page, read_map, Entry, Head, PageMap};
use crate::encode::decode_leaf_with;
use crate::places::KeyValue;
use crate::values::Values;
use crate::{Error, Result};

/// The rows on each place of the level an order leads with, as its directory counts them.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct LevelCounts {
    pub order: String,
    /// Whether the column alone is a key the order holds once.
    pub unique: bool,
    /// The table's live rows, and those of them whose column is NULL.
    pub rows: u64,
    pub nulls: u64,
    /// Each place an entry or a page's first row names, in the order's order, NULL left out, with
    /// the fewest and the most live rows it holds; none where reading them would pass the pages
    /// given, or where the column is a key the order holds once.
    pub places: Option<Vec<PlaceRows>>,
    /// Whether `places` names every place the level holds, each with its rows exactly.
    pub whole: bool,
    /// The directory pages read: the top's, the entries' below it and the page maps; and a block's
    /// last column page, where its greatest key is read there.
    pub directory_pages: u64,
}

/// A place of a level and the live rows it holds: exactly where the two are equal.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct PlaceRows {
    pub key: KeyValue,
    pub low: u64,
    pub high: u64,
    /// The live rows from its first row to the next place's, in the order's order: those of the
    /// entries and the pages that begin on it.
    pub run: u64,
}

/// The places counted so far.
struct Counted {
    places: Vec<PlaceRows>,
    whole: bool,
}

impl Counted {
    /// Adds rows to `key`'s place, the place after every place counted so far or the last of them;
    /// a place holding no live row is left out.
    fn add(&mut self, key: &KeyValue, low: u64, high: u64, run: u64) {
        if matches!(key, KeyValue::Null { .. }) || high == 0 {
            return;
        }
        match self.places.last_mut() {
            Some(p) if p.key == *key => {
                p.low += low;
                p.high += high;
                p.run += run;
            }
            _ => self.places.push(PlaceRows {
                key: key.clone(),
                low,
                high,
                run,
            }),
        }
    }
}

impl Snail {
    /// The rows on each place of the level `column` leads an order of `table` with, on the root the
    /// snail last read, reading at most `pages` directory pages below the order's top. None where no
    /// order of the table leads with the column itself.
    pub fn level_counts(
        &self,
        table: &str,
        column: &str,
        pages: u64,
    ) -> Result<Option<LevelCounts>> {
        let t = self
            .catalog
            .tables
            .iter()
            .find(|t| t.name == table)
            .ok_or_else(|| Error::new(format!("the snail holds no table {table}")))?;
        let c = t
            .columns
            .iter()
            .position(|x| x.name == column)
            .ok_or_else(|| Error::new(format!("{table} has no column {column}")))?;
        let leading: Vec<usize> = (0..t.orders.len())
            .filter(|&o| t.orders[o].levels.first() == Some(&Leading::Column(c)))
            .collect();
        let alone = |o: &usize| t.orders[*o].unique && t.orders[*o].levels.len() == 1;
        let Some(o) = leading
            .iter()
            .copied()
            .find(alone)
            .or(leading.first().copied())
        else {
            return Ok(None);
        };
        let order = &t.orders[o];
        let before = self.touched();
        let (head, entries, _) = self.top(t, o)?;
        let slot = order.leaf_slot(t, c).unwrap();
        let rows = entries.iter().map(|e| e.rows as u64).sum();
        let nulls = entries.iter().map(|e| e.columns[slot].nulls as u64).sum();
        let unique = alone(&o);
        let mut counted = Counted {
            places: Vec::new(),
            whole: true,
        };
        let within = !unique
            && self.count(
                (t, order),
                &head,
                &entries,
                head.height,
                &mut counted,
                before.saturating_add(pages),
            )?;
        Ok(Some(LevelCounts {
            order: order.name.clone(),
            unique,
            rows,
            nulls,
            places: within.then_some(counted.places),
            whole: within && counted.whole,
            directory_pages: self.touched() - before,
        }))
    }

    /// Counts the places of `entries`, a level `height` above the blocks counting from 1; false
    /// where reading below them would pass `until` pages touched.
    fn count(
        &self,
        on: (&Table, &Order),
        head: &Head,
        entries: &[Entry],
        height: u32,
        counted: &mut Counted,
        until: u64,
    ) -> Result<bool> {
        for e in entries {
            let (lo, hi, _) = narrowed(e, on.1, on.0);
            let (Some(first), Some(last)) = (lo.first(), hi.first()) else {
                counted.whole = false;
                continue;
            };
            if first == last {
                let rows = e.rows as u64;
                counted.add(first, rows, rows, rows);
                continue;
            }
            if self.touched() >= until {
                return Ok(false);
            }
            if height > 1 {
                let below = decode_page(&self.reader.read_extent(e.child)?, head, e.bound())?;
                if !self.count(on, head, &below, height - 1, counted, until)? {
                    return Ok(false);
                }
            } else {
                let greatest = greatest(on, head, e, last);
                self.count_block(on, head, e, greatest, counted, until)?;
            }
        }
        Ok(true)
    }

    /// Counts a block's places by its page map: a page whose first row and the next page's lie on
    /// one place holds that place alone, the last page's next its block's `greatest`, or where that
    /// is not known, its own last row, read where `until` leaves a page for it; any other holds rows
    /// of both and of whatever lies between, as do the rows written after the block's sorted rows,
    /// wherever their keys lie in it.
    fn count_block(
        &self,
        on: (&Table, &Order),
        head: &Head,
        e: &Entry,
        greatest: Option<KeyValue>,
        counted: &mut Counted,
        until: u64,
    ) -> Result<()> {
        let map = read_map(&self.reader, e, &head.level_text)?;
        let Some(pages) = map.columns.iter().map(|c| &c.pages).max_by_key(|p| p.len()) else {
            counted.whole = false;
            return Ok(());
        };
        let dead = |from: u32, to: u32| {
            (map.dead.partition_point(|&x| x < to) - map.dead.partition_point(|&x| x < from)) as u64
        };
        let sorted = map.sorted;
        let mut tail = 0u64;
        let mut own: Vec<PlaceRows> = Vec::new();
        let mut add = |key: &KeyValue, low: u64, high: u64, run: u64| match own.last_mut() {
            Some(p) if p.key == *key => {
                p.low += low;
                p.high += high;
                p.run += run;
            }
            _ => own.push(PlaceRows {
                key: key.clone(),
                low,
                high,
                run,
            }),
        };
        let mut greatest = greatest;
        for (i, p) in pages.iter().enumerate() {
            let end = p.first_row + p.rows;
            let in_order = end.min(sorted).max(p.first_row);
            tail += (end - in_order) as u64 - dead(in_order, end);
            if in_order == p.first_row {
                continue;
            }
            let live = (in_order - p.first_row) as u64 - dead(p.first_row, in_order);
            let following = pages
                .get(i + 1)
                .filter(|n| n.first_row < sorted)
                .and_then(|n| n.first.first());
            if following.is_none() && greatest.is_none() && self.touched() < until {
                greatest = self.last_sorted_key(on, &map)?;
            }
            match (p.first.first(), following.or(greatest.as_ref())) {
                (Some(a), Some(b)) if a == b => add(a, live, live, live),
                (Some(a), Some(b)) => {
                    add(a, 0, live, live);
                    add(b, 0, live, 0);
                    counted.whole = false;
                }
                (Some(a), None) => {
                    add(a, 0, live, live);
                    counted.whole = false;
                }
                _ => counted.whole = false,
            }
        }
        if tail > 0 {
            counted.whole = false;
            for p in &mut own {
                p.high += tail;
            }
        }
        for p in own {
            counted.add(&p.key, p.low, p.high, p.run);
        }
        Ok(())
    }

    /// The key a block's last sorted row holds on the order's first level, where that level is
    /// keyed by its bytes, read from the page of each of its leaves that holds the row.
    fn last_sorted_key(&self, on: (&Table, &Order), map: &PageMap) -> Result<Option<KeyValue>> {
        let (table, order) = on;
        let c = order.levels[0].column();
        let column = &table.columns[c];
        let (Some(slots), Some(r)) = (order.slots(table, c), map.sorted.checked_sub(1)) else {
            return Ok(None);
        };
        if !crate::write::bounded(table, c) {
            return Ok(None);
        }
        let descs = crate::write::leaf_descriptors(table, order)?;
        let leaves = order.leaves(table);
        let mut parts = Vec::with_capacity(slots.len());
        for s in slots {
            let cp = &map.columns[s];
            let Some(p) = cp
                .pages
                .iter()
                .find(|p| p.first_row <= r && r < p.first_row + p.rows)
            else {
                return Ok(None);
            };
            let dictionary = cp
                .dictionary
                .map(|d| self.reader.read_extent(d))
                .transpose()?;
            let page = self.reader.read_extent(p.at)?;
            let values = decode_leaf_with(
                &descs[s],
                &leaves[s].1,
                dictionary.as_deref(),
                &[&page],
                p.rows as usize,
                &mut None,
            )?;
            parts.push(values.gather(&[(r - p.first_row) as usize]));
        }
        let values = match parts.len() {
            1 => parts.pop().unwrap(),
            _ => Values::from_leaves(column.ty, parts),
        };
        Ok(match &values {
            Values::Composed(x) => x[0].as_ref().map(|v| KeyValue::Text(v.hex.clone())),
            v => v
                .text_value(0)
                .map(|h| {
                    let key = crate::strings::column_key(column.ty, column.orderer()?, h.data());
                    Ok::<_, Error>(crate::places::column_byte_key(column, &key))
                })
                .transpose()?,
        })
    }
}

/// The greatest key a block holds on its order's first level, where its entry says it: its last
/// key, kept whole where the order keeps both keys or at the order's end; a number's greatest its
/// leaves keep, `last`; a numeric's that may hold NaN, its decimal's greatest where the block holds
/// no NaN, else NaN's; or text whose greatest its leaves keep cut to 8 bytes is shorter.
fn greatest(on: (&Table, &Order), head: &Head, e: &Entry, last: &KeyValue) -> Option<KeyValue> {
    let (table, order) = on;
    let c = order.levels[0].column();
    let slot = order.leaf_slot(table, c)?;
    let sum = &e.columns[slot];
    let ends_null = matches!(last, KeyValue::Null { first: false }) && sum.nulls > 0;
    if !head.separated || ends_null {
        return Some(last.clone());
    }
    let bounded = crate::write::bounded(table, c);
    if table.leaf_count(c) == 1 && !bounded {
        return Some(last.clone());
    }
    if let (ColumnType::Numeric { precision, .. }, Some(slots)) =
        (table.columns[c].ty, order.slots(table, c))
    {
        let (units, nan) = (&e.columns[slots.start], &e.columns[slots.start + 1]);
        let greatest = if nan.max == 0 {
            units.max
        } else {
            crate::numeric::nan_units(precision)
        };
        return (units.nulls < e.rows).then(|| match table.columns[c].ty.is_wide() {
            true => KeyValue::Wide(greatest),
            false => KeyValue::Int(greatest as i64),
        });
    }
    // The order's greatest end, which a level keyed by its bytes keeps whole up to 8 bytes.
    if bounded && !e.open && !matches!(last, KeyValue::Null { .. }) {
        return Some(last.clone());
    }
    if !head.bound_slots.contains(&(slot as u32)) || !table.columns[c].ty.is_string() {
        return None;
    }
    if table.columns[c].collated() || sum.nulls == e.rows {
        return None;
    }
    let bytes = ((sum.max as i64 as u64) ^ (1 << 63)).to_be_bytes();
    let n = bytes.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    (n < 8)
        .then(|| {
            std::str::from_utf8(&bytes[..n])
                .ok()
                .map(|s| KeyValue::Text(s.into()))
        })
        .flatten()
}

/// The value a place of a level of `column` names, in the column's own form, from its key: a
/// float's or an interval's the least its class holds; none where the key names no one value (a
/// string under a library collation, a composed value but an array of values so keyed, a type held
/// as its text but jsonb, inet and cidr).
pub fn held_of_key(column: &Column, key: &KeyValue) -> Option<Datum> {
    let ty = column.ty;
    if column.collated() {
        return None;
    }
    match key {
        KeyValue::Int(v) => Some(match Values::new(ty) {
            Values::Float4(_) => Datum::Float4(crate::scalars::float4_of_key(*v)),
            Values::Float8(_) => Datum::Float8(crate::places::float8_of_key(*v).to_bits()),
            Values::Int(_) => Datum::Int(*v),
            _ => return None,
        }),
        KeyValue::Wide(k) => Some(match Values::new(ty) {
            Values::Interval(_) => Datum::Interval(crate::times::Interval::of_total(*k)),
            Values::Wide(_) => Datum::Wide(*k),
            _ => return None,
        }),
        KeyValue::Text(s) if ty.is_nested() => {
            let hex = s.as_bytes();
            let digit = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
            let bytes = hex
                .chunks(2)
                .map(|h| Some(digit(h[0])? << 4 | digit(*h.get(1)?)?))
                .collect::<Option<Vec<u8>>>()?;
            let item = crate::composed::item_of_key(ty, &bytes)?;
            Some(Datum::Compound(std::sync::Arc::new(
                crate::composed::Compound::new(ty, item),
            )))
        }
        KeyValue::Text(s) => crate::composed::scalar_of_key_bytes(ty, s.as_bytes()),
        KeyValue::Bytes(b) => crate::composed::scalar_of_key_bytes(ty, b),
        KeyValue::Null { .. } => None,
    }
}
