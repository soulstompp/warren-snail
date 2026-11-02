//! The rows on each place of the level an order leads with, read from its directory: each entry
//! whose rows all lie on one place counted whole, and below the entries that hold several, each
//! page by the place it begins on, down to the page.

use super::Snail;
use crate::catalog::Level as Leading;
use crate::directory::{decode_page, read_map, Entry, Head};
use crate::places::KeyValue;
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
    /// The directory pages read: the top's, the entries' below it and the page maps.
    pub directory_pages: u64,
}

/// A place of a level and the live rows it holds: exactly where the two are equal.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct PlaceRows {
    pub key: KeyValue,
    pub low: u64,
    pub high: u64,
}

/// The places counted so far.
struct Counted {
    places: Vec<PlaceRows>,
    whole: bool,
}

impl Counted {
    /// Adds rows to `key`'s place, the place after every place counted so far or the last of them;
    /// a place holding no live row is left out.
    fn add(&mut self, key: &KeyValue, low: u64, high: u64) {
        if matches!(key, KeyValue::Null { .. }) || high == 0 {
            return;
        }
        match self.places.last_mut() {
            Some(p) if p.key == *key => {
                p.low += low;
                p.high += high;
            }
            _ => self.places.push(PlaceRows {
                key: key.clone(),
                low,
                high,
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
        head: &Head,
        entries: &[Entry],
        height: u32,
        counted: &mut Counted,
        until: u64,
    ) -> Result<bool> {
        for e in entries {
            let (Some(first), Some(last)) = (e.first.first(), e.last.first()) else {
                counted.whole = false;
                continue;
            };
            if first == last {
                counted.add(first, e.rows as u64, e.rows as u64);
                continue;
            }
            if self.touched() >= until {
                return Ok(false);
            }
            if height > 1 {
                let below = decode_page(&self.reader.read_extent(e.child)?, head, e.bound())?;
                if !self.count(head, &below, height - 1, counted, until)? {
                    return Ok(false);
                }
            } else {
                self.count_block(head, e, counted)?;
            }
        }
        Ok(true)
    }

    /// Counts a block's places by its page map: a page whose first row and the next page's lie on
    /// one place holds that place alone; any other holds rows of both and of whatever lies between,
    /// as do the rows written after the block's sorted rows, wherever their keys lie in it.
    fn count_block(&self, head: &Head, e: &Entry, counted: &mut Counted) -> Result<()> {
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
        let mut add = |key: &KeyValue, low: u64, high: u64| match own.last_mut() {
            Some(p) if p.key == *key => {
                p.low += low;
                p.high += high;
            }
            _ => own.push(PlaceRows {
                key: key.clone(),
                low,
                high,
            }),
        };
        for (i, p) in pages.iter().enumerate() {
            let end = p.first_row + p.rows;
            let in_order = end.min(sorted).max(p.first_row);
            tail += (end - in_order) as u64 - dead(in_order, end);
            if in_order == p.first_row {
                continue;
            }
            let live = (in_order - p.first_row) as u64 - dead(p.first_row, in_order);
            let next = pages
                .get(i + 1)
                .filter(|n| n.first_row < sorted)
                .and_then(|n| n.first.first())
                .or(e.last.first());
            match (p.first.first(), next) {
                (Some(a), Some(b)) if a == b => add(a, live, live),
                (Some(a), Some(b)) => {
                    add(a, 0, live);
                    add(b, 0, live);
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
            counted.add(&p.key, p.low, p.high);
        }
        Ok(())
    }
}
