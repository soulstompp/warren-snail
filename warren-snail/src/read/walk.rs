//! One table's round: its orders priced from their directories' tops, walked down only where the
//! question may hold rows, its pages predicted from the page maps, then read.

use super::answer::{same, Answer, Cell, Dict, Text};
use super::question::{Cond, Countable, Side, TextCond};
use super::{Snail, Wasted};
use crate::catalog::{Level, Order, Shape, Table};
use crate::directory::{decode_page, decode_top, Entry, Head, PageMap, Summary};
use crate::encode::decode_with;
use crate::file::Extent;
use crate::places::{key, month_of, null_key, wide_cmp, KeyValue};
use crate::values::Values;
use crate::{Error, Result};
use std::cmp::Ordering;
use std::ops::Bound;

/// Where a scan hands the rows it keeps.
pub trait Sink {
    /// The dictionaries the codes of the rows that follow index, one for each column scanned.
    fn dictionaries(&mut self, dicts: &[Dict]);
    /// How many of the columns scanned, from the first, `admits` looks at.
    fn lead(&self) -> usize {
        0
    }
    /// Whether a row whose leading cells these are is wanted; the rest of its cells are made only if it is.
    fn admits(&mut self, _lead: &[Cell]) -> bool {
        true
    }
    fn row(&mut self, cells: &[Cell]);
    /// A run of rows, every one wanted, given at once: each column read, as `(is bytes, is a month,
    /// the row its values begin at, its values)`, from row `from` to `to`. Whether the sink took them;
    /// where it did not, they come row by row.
    fn run(&mut self, _outs: &[(bool, bool, usize, &Values)], _from: usize, _to: usize) -> bool {
        false
    }
    /// The rows of one block have all come.
    fn block_end(&mut self) -> Result<()> {
        Ok(())
    }
}

/// A table read alone hands its rows to the answer, its columns the answer's inputs in order.
impl Sink for Answer {
    fn dictionaries(&mut self, dicts: &[Dict]) {
        Answer::dictionaries(self, dicts);
    }

    fn row(&mut self, cells: &[Cell]) {
        self.add(cells);
    }

    fn run(&mut self, outs: &[(bool, bool, usize, &Values)], from: usize, to: usize) -> bool {
        Answer::run(self, outs, from, to)
    }
}

/// What a round along one table reads.
pub struct Want<'a> {
    pub table: &'a Table,
    pub side: &'a Side,
    /// The columns it returns, to the answer or to a join.
    pub columns: Vec<usize>,
    /// What its counts can answer, where the table is read alone.
    pub countable: Option<Countable>,
    /// The keys another table holds, which this table is kept to.
    pub keys: Option<&'a KeySet>,
}

/// The values of a join's columns another table holds.
pub struct KeySet {
    /// This table's columns they meet, in the join's order.
    pub columns: Vec<usize>,
    /// Each key's values, the keys sorted, laid flat.
    keys: Vec<i128>,
}

impl KeySet {
    pub fn new<'a>(columns: Vec<usize>, keys: impl IntoIterator<Item = &'a Vec<i128>>) -> KeySet {
        let mut sorted: Vec<&[i128]> = keys.into_iter().map(|k| k.as_slice()).collect();
        sorted.sort_unstable();
        sorted.dedup();
        KeySet {
            keys: sorted.concat(),
            columns,
        }
    }

    fn len(&self) -> usize {
        self.keys.len().checked_div(self.columns.len()).unwrap_or(0)
    }

    fn key(&self, i: usize) -> &[i128] {
        let w = self.columns.len();
        &self.keys[i * w..(i + 1) * w]
    }

    /// Whether some key lies from `first` to `last`, both read on their first `j` values.
    fn meets(&self, first: &[KeyValue], last: &[KeyValue]) -> bool {
        let cmp = |key: &[i128], bound: &[KeyValue]| -> Ordering {
            key.iter()
                .zip(bound)
                .map(|(k, b)| wide_cmp(*k, b))
                .find(|o| *o != Ordering::Equal)
                .unwrap_or(Ordering::Equal)
        };
        let (mut lo, mut hi) = (0, self.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            if cmp(self.key(mid), first) == Ordering::Less {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo < self.len() && cmp(self.key(lo), last) != Ordering::Greater
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    None,
    May,
    Must,
}

/// An entry the round reads below, or at the blocks reads the pages of, and what of it is left open.
struct Open {
    entry: Entry,
    interval_may: bool,
    /// The conditions off the leading levels that the entry's counts leave open.
    rest_may: Vec<usize>,
    /// The conditions on text the entry may hold.
    text_may: Vec<usize>,
    /// The tests for NULL the entry's counts leave open.
    null_may: Vec<usize>,
    /// The columns the round reads under it.
    slots: Vec<usize>,
}

/// One order, as a round walks down its directory from the top.
pub struct Walk {
    pub order: usize,
    head: Head,
    /// The leading levels the question fixes, and their bounds.
    m: usize,
    lo: Vec<KeyValue>,
    hi: Vec<KeyValue>,
    rest: Vec<Cond>,
    /// The conditions on text no level fixes, by their place among the side's.
    pub(super) texts: Vec<usize>,
    /// The tests for NULL no level fixes, by their place among the side's.
    nulls: Vec<usize>,
    /// How many leading levels are the join's columns another table's keys restrict, or 0.
    keyed: usize,
    /// The entries left open, all on one level.
    open: Vec<Open>,
    /// The level they lie on, 1 at the blocks.
    pub level: u32,
    /// The rows the counts answered: in all, and on each month of a count by month.
    pub counted: u64,
    pub counted_months: [u64; 12],
    /// The rows the question holds, as the counts bound them: those of entries closed by the counts,
    /// and the fewest and most among the entries left open.
    pub rows_closed: u64,
    pub rows_low: u64,
    pub rows_high: u64,
    /// The rows the order holds, as its top counts them.
    pub table_rows: u64,
    pub directory_pages: u64,
    /// The fewest and the most column pages the round can read, over the entries left open, and as
    /// the top alone said them.
    pub low: u64,
    pub high: u64,
    pub top: (u64, u64),
    pub statuses: (u64, u64, u64),
}

impl Walk {
    pub fn is_open(&self) -> bool {
        !self.open.is_empty()
    }

    /// The directory pages the round reads next, under the entries left open: a directory page each
    /// above the blocks, and the pages that hold the blocks' page maps at them, each once.
    pub fn below(&self) -> impl Iterator<Item = u64> + '_ {
        let pages: std::collections::BTreeSet<u64> = self
            .open
            .iter()
            .flat_map(|o| {
                let run = if self.level == 1 {
                    o.entry.map_run().0
                } else {
                    o.entry.child
                };
                run.start..run.start + run.pages()
            })
            .collect();
        pages.into_iter()
    }
}

struct BlockRead {
    map: PageMap,
    interval_may: bool,
    rest_may: Vec<usize>,
    text_may: Vec<usize>,
    null_may: Vec<usize>,
    /// For every column read, its slot and the pages read of it, ascending.
    reads: Vec<(usize, Vec<usize>)>,
}

pub struct Plan {
    pub walk: Walk,
    reads: Vec<BlockRead>,
    pub column_pages: u64,
    /// The rows the question holds, estimated from the blocks' page maps: those the counts closed on,
    /// and of each block read in part, its rows by the share of it each open condition covers.
    pub rows_estimate: f64,
}

impl Plan {
    pub fn blocks_read(&self) -> u64 {
        self.reads.len() as u64
    }

    /// Every disk page the plan's column reads take, the dictionaries' among them.
    pub fn disk_pages(&self) -> impl Iterator<Item = u64> + '_ {
        let span = |e: Extent| e.start..e.start + e.pages();
        self.reads.iter().flat_map(move |br| {
            br.reads
                .iter()
                .filter(|(_, chosen)| !chosen.is_empty())
                .flat_map(move |(s, chosen)| {
                    let cp = &br.map.columns[*s];
                    cp.dictionary
                        .into_iter()
                        .flat_map(span)
                        .chain(chosen.iter().flat_map(move |&p| span(cp.pages[p].at)))
                })
        })
    }
}

/// Of the orders that can read one level further down, given with their fewest pages, the one whose
/// fewest are fewest, the first of them on a tie.
pub fn next_down(lows: impl IntoIterator<Item = (usize, u64)>) -> Option<usize> {
    lows.into_iter().min_by_key(|&(_, low)| low).map(|(i, _)| i)
}

/// The orders priced at these ranges of column pages that are left unread: each one whose fewest
/// pages are more than some order's most.
pub fn ruled_out(ranges: &[(u64, u64)]) -> Vec<bool> {
    let least = ranges.iter().map(|r| r.1).min().unwrap_or(0);
    ranges.iter().map(|r| r.0 > least).collect()
}

pub fn s_column(s: Shape) -> usize {
    match s {
        Shape::Column(c) | Shape::Clock(c) | Shape::MonthUtc(c) => c,
    }
}

/// The columns a side's own conditions read.
pub fn condition_columns(side: &Side) -> Vec<usize> {
    side.conds
        .iter()
        .map(|c| s_column(c.shape))
        .chain(side.texts.iter().map(|t| t.column))
        .chain(side.nulls.iter().map(|&(c, _)| c))
        .collect()
}

impl Snail {
    /// An order's top page: its head, its entries, and the disk pages it took.
    pub(super) fn top(&self, table: &Table, order: usize) -> Result<(Head, Vec<Entry>, u64)> {
        let name = &table.orders[order].name;
        let (_, _, e) = self
            .root
            .orders
            .iter()
            .find(|(t, o, _)| *t == table.name && o == name)
            .ok_or_else(|| {
                Error::new(format!(
                    "{}: the root names no directory for {name}",
                    table.name
                ))
            })?;
        let (head, entries) = decode_top(&self.reader.read_extent(*e)?)?;
        Ok((head, entries, e.pages()))
    }

    /// A round along one order, begun at its directory's top.
    pub(super) fn start(&self, oi: usize, want: &Want) -> Result<Walk> {
        let order = &want.table.orders[oi];
        let (head, entries, pages) = self.top(want.table, oi)?;
        let conds = &want.side.conds;
        let texts = &want.side.texts;
        let nulls = &want.side.nulls;
        let mut used = vec![false; conds.len()];
        let mut used_texts = vec![false; texts.len()];
        let mut used_nulls = vec![false; nulls.len()];
        let (mut lo, mut hi) = (Vec::new(), Vec::new());
        // For each level fixed, the test for NULL that fixed it, if one did.
        let mut by_null: Vec<Option<usize>> = Vec::new();
        for (li, l) in order.levels.iter().enumerate() {
            // A level is fixed on its NULL rows' own run by IS NULL on its column.
            if let Some(i) = nulls.iter().position(|&n| n == (l.column(), true)) {
                used_nulls[i] = true;
                lo.push(null_key(order, li));
                hi.push(null_key(order, li));
                by_null.push(Some(i));
                continue;
            }
            by_null.push(None);
            // A text level is fixed by an equality on it; a range on text stays a condition to read.
            if let Level::Column(c) = *l {
                if want.table.columns[c].ty.is_text() {
                    let fixed = texts.iter().position(|t| {
                        t.column == c && matches!((&t.lo, &t.hi), (Bound::Included(a), Bound::Included(b)) if a == b)
                    });
                    match fixed {
                        Some(i) => {
                            used_texts[i] = true;
                            let Bound::Included(v) = &texts[i].lo else {
                                unreachable!()
                            };
                            lo.push(KeyValue::Text(v.as_str().into()));
                            hi.push(KeyValue::Text(v.as_str().into()));
                            continue;
                        }
                        None => break,
                    }
                }
            }
            let wanted = match *l {
                Level::Column(c) => Shape::Column(c),
                Level::MonthUtc(c) => Shape::MonthUtc(c),
            };
            // A level is fixed by a range on it; `<>`, and a test that keeps NULLs, stay
            // conditions to read.
            match conds.iter().position(|c| c.shape == wanted && c.is_range()) {
                Some(i) => {
                    used[i] = true;
                    let iv = conds[i].kept;
                    let bound = |v: i128| {
                        if want.table.columns[l.column()].ty.is_wide() {
                            KeyValue::Wide(v)
                        } else {
                            KeyValue::Int(v.clamp(i64::MIN as i128, i64::MAX as i128) as i64)
                        }
                    };
                    lo.push(bound(iv.lo));
                    hi.push(bound(iv.hi));
                    if iv.lo != iv.hi {
                        break;
                    }
                }
                None => break,
            }
        }
        // A NULL run fixed last narrows nothing the NULLs counted do not, and the counts close a
        // block holding values beside it, so IS NULL fixes a level only ahead of another fixed.
        by_null.truncate(lo.len());
        while let Some(Some(i)) = by_null.last().copied() {
            used_nulls[i] = false;
            by_null.pop();
            lo.pop();
            hi.pop();
        }
        let m = lo.len();
        let rest: Vec<Cond> = conds
            .iter()
            .zip(&used)
            .filter(|(_, u)| !**u)
            .map(|(c, _)| c.clone())
            .collect();
        let texts: Vec<usize> = (0..texts.len()).filter(|&i| !used_texts[i]).collect();
        let nulls: Vec<usize> = (0..nulls.len()).filter(|&i| !used_nulls[i]).collect();
        let keyed = match want.keys {
            Some(k)
                if order.levels.len() >= k.columns.len()
                    && order
                        .levels
                        .iter()
                        .zip(&k.columns)
                        .all(|(l, &c)| *l == Level::Column(c)) =>
            {
                k.columns.len()
            }
            _ => 0,
        };
        let level = head.height;
        let mut walk = Walk {
            order: oi,
            head,
            m,
            lo,
            hi,
            rest,
            texts,
            nulls,
            keyed,
            open: Vec::new(),
            level,
            counted: 0,
            counted_months: [0; 12],
            rows_closed: 0,
            rows_low: 0,
            rows_high: 0,
            table_rows: entries.iter().map(|e| e.rows as u64).sum(),
            directory_pages: pages,
            low: 0,
            high: 0,
            top: (0, 0),
            statuses: (0, 0, 0),
        };
        for e in entries {
            assess(&mut walk, e, order, want);
        }
        walk.top = (walk.low, walk.high);
        Ok(walk)
    }

    /// Reads one level further down: the page under every entry left open.
    pub(super) fn descend(&self, walk: &mut Walk, want: &Want) -> Result<()> {
        let order = &want.table.orders[walk.order];
        let open = std::mem::take(&mut walk.open);
        walk.level -= 1;
        walk.low = 0;
        walk.high = 0;
        walk.rows_low = 0;
        walk.rows_high = 0;
        for o in open {
            let entries = decode_page(&self.reader.read_extent(o.entry.child)?, &walk.head)?;
            walk.directory_pages += o.entry.child.pages();
            for e in entries {
                assess(walk, e, order, want);
            }
        }
        Ok(())
    }

    /// The column pages a round along an order walked down to its blocks reads, exactly, from the
    /// page maps of the blocks it reads.
    pub(super) fn predict(&self, mut walk: Walk, want: &Want) -> Result<Plan> {
        let mut column_pages = 0u64;
        let mut rows_estimate = walk.rows_closed as f64;
        let mut reads = Vec::with_capacity(walk.open.len());
        // The pages that hold the page maps last read: blocks side by side share them.
        let mut held: Option<(u64, Vec<u8>)> = None;
        for o in std::mem::take(&mut walk.open) {
            let (m, lo, hi, j) = (walk.m, &walk.lo, &walk.hi, walk.keyed);
            let b = &o.entry;
            let (run, at) = b.map_run();
            let pages = run.pages();
            if !matches!(&held, Some((start, buf)) if *start == run.start && buf.len() as u64 >= pages * crate::cut::PAGE as u64)
            {
                held = Some((run.start, self.reader.read(run.start, pages)?));
                walk.directory_pages += pages;
            }
            let map = PageMap::decode(&held.as_ref().unwrap().1[at..], &walk.head.level_text)?;
            rows_estimate += estimate(&o, &map, &walk, want.table, &want.side.nulls);
            let mut block_reads = Vec::new();
            let mut waste = 0u64;
            for &s in &o.slots {
                let pages = &map.columns[s].pages;
                // A page holding rows written after the sorted ones is read whenever the block is,
                // and a page's next key bounds it only while that key is a sorted row's. A page of
                // the tail holds no sorted row, so no key range brings a read to it.
                let unsorted = |p: usize| pages[p].first_row + pages[p].rows > map.sorted;
                let next = |p: usize| {
                    if p + 1 < pages.len() && !unsorted(p) && pages[p + 1].first_row < map.sorted {
                        &pages[p + 1].first
                    } else {
                        &b.last
                    }
                };
                let fits = |p: usize| {
                    pages[p].first_row < map.sorted
                        && (!o.interval_may
                            || (pages[p].first[..m] <= *hi.as_slice()
                                && next(p)[..m] >= *lo.as_slice()))
                        && (j == 0
                            || want
                                .keys
                                .unwrap()
                                .meets(&pages[p].first[..j], &next(p)[..j]))
                };
                let chosen: Vec<usize> = (0..pages.len())
                    .filter(|&p| unsorted(p) || fits(p))
                    .collect();
                // The column's waste: the pages read only for rows written after the sorted ones,
                // and the pages the dead rows among those read fill.
                let extra: u64 = chosen
                    .iter()
                    .filter(|&&p| !fits(p))
                    .map(|&p| pages[p].at.pages())
                    .sum();
                let (mut rows, mut dead, mut disk) = (0u64, 0u64, 0u64);
                for &p in &chosen {
                    let (a, z) = (pages[p].first_row, pages[p].first_row + pages[p].rows);
                    rows += pages[p].rows as u64;
                    dead += (map.dead.partition_point(|&x| x < z)
                        - map.dead.partition_point(|&x| x < a)) as u64;
                    disk += pages[p].at.pages();
                }
                let live = if rows == 0 {
                    0
                } else {
                    (disk * (rows - dead)).div_ceil(rows)
                };
                waste = waste.max(extra + disk - live);
                if !chosen.is_empty() {
                    column_pages += map.columns[s].dictionary.map_or(0, Extent::pages);
                    column_pages += chosen.iter().map(|&p| pages[p].at.pages()).sum::<u64>();
                }
                block_reads.push((s, chosen));
            }
            if waste >= 1 && self.writer.is_some() {
                self.wasted.borrow_mut().push(Wasted {
                    table: want.side.table,
                    order: walk.order,
                    first: b.first.clone(),
                    child: b.child,
                });
            }
            reads.push(BlockRead {
                map,
                interval_may: o.interval_may,
                rest_may: o.rest_may,
                text_may: o.text_may,
                null_may: o.null_may,
                reads: block_reads,
            });
        }
        Ok(Plan {
            walk,
            reads,
            column_pages,
            rows_estimate,
        })
    }

    /// Hands each row the plan keeps to `each`, as the values of `columns`, in the order's order. The
    /// pages it reads are tagged with the order they were read for.
    pub(super) fn read_plan(
        &self,
        plan: &Plan,
        want: &Want,
        columns: &[(usize, bool)],
        sink: &mut dyn Sink,
    ) -> Result<()> {
        self.scan_blocks(plan, want, columns, sink, 0..plan.reads.len())
    }

    /// As `scan`, for the blocks of the plan in `blocks` alone.
    pub(super) fn scan_blocks(
        &self,
        plan: &Plan,
        want: &Want,
        columns: &[(usize, bool)],
        sink: &mut dyn Sink,
        blocks: std::ops::Range<usize>,
    ) -> Result<()> {
        self.reader
            .tag_reads(Some(super::order_tag(want.side.table, plan.walk.order)));
        let read = self.scan_tagged(plan, want, columns, sink, blocks);
        self.reader.tag_reads(None);
        read
    }

    fn scan_tagged(
        &self,
        plan: &Plan,
        want: &Want,
        columns: &[(usize, bool)],
        sink: &mut dyn Sink,
        blocks: std::ops::Range<usize>,
    ) -> Result<()> {
        let table = want.table;
        let order = &table.orders[plan.walk.order];
        let walk = &plan.walk;
        let mut cells: Vec<Cell> = vec![Cell::Null; columns.len()];
        let mut sent: Vec<Dict> = vec![None; columns.len()];
        let lead = sink.lead();
        let leaves = order.leaves(table);
        let descs = crate::write::leaf_descriptors(table, order)?;
        for br in &plan.reads[blocks] {
            // For every leaf read, its runs of pages decoded: (slot, [(first row, values)]).
            let mut held: Vec<(usize, Vec<(usize, Values)>)> = Vec::new();
            for (s, chosen) in &br.reads {
                let leaf = &leaves[*s].1;
                let cp = &br.map.columns[*s];
                let mut runs = Vec::new();
                if !chosen.is_empty() {
                    let dictionary = match cp.dictionary {
                        Some(d) => Some(self.reader.read_extent(d)?),
                        None => None,
                    };
                    let desc = &descs[*s];
                    let mut dict = None;
                    let mut i = 0;
                    while i < chosen.len() {
                        let mut z = i + 1;
                        // A run is pages next to each other in the block and in the file: a page
                        // written afresh lies apart from the pages before it.
                        while z < chosen.len()
                            && chosen[z] == chosen[z - 1] + 1
                            && cp.pages[chosen[z]].at.start
                                == cp.pages[chosen[z - 1]].at.start
                                    + cp.pages[chosen[z - 1]].at.pages()
                        {
                            z += 1;
                        }
                        let pages = &cp.pages[chosen[i]..=chosen[z - 1]];
                        let first = pages[0].at.start;
                        let last = &pages[pages.len() - 1].at;
                        let run = self.reader.read(first, last.start + last.pages() - first)?;
                        let slices: Vec<&[u8]> = pages
                            .iter()
                            .map(|p| {
                                let at = (p.at.start - first) as usize * crate::cut::PAGE;
                                &run[at..at + p.at.bytes as usize]
                            })
                            .collect();
                        let rows: usize = pages.iter().map(|p| p.rows as usize).sum();
                        runs.push((
                            pages[0].first_row as usize,
                            decode_with(
                                desc,
                                leaf.ty,
                                dictionary.as_deref(),
                                &slices,
                                rows,
                                &mut dict,
                            )?,
                        ));
                        i = z;
                    }
                }
                held.push((*s, runs));
            }
            // The rows every leaf read holds. Within one of these ranges each leaf is one run.
            let mut rows: Vec<(usize, usize)> = vec![(0, br.map.rows() as usize)];
            let dead = &br.map.dead;
            for (_, runs) in &held {
                let mut kept = Vec::new();
                for &(a, z) in &rows {
                    for (start, v) in runs {
                        let (x, y) = (a.max(*start), z.min(start + v.len()));
                        if x < y {
                            kept.push((x, y));
                        }
                    }
                }
                rows = kept;
            }
            // The columns read whose values are a group of leaves.
            let mut groups: Vec<usize> = Vec::new();
            for (s, _) in &br.reads {
                let c = leaves[*s].0;
                if table.columns[c].ty.leaf_count() > 1 && !groups.contains(&c) {
                    groups.push(c);
                }
            }
            for &(from, to) in &rows {
                // Each leaf read: the run that holds this range, and its first row.
                let run = |s: usize| -> (usize, &Values) {
                    let (_, runs) = held.iter().find(|h| h.0 == s).unwrap();
                    let (start, v) = runs
                        .iter()
                        .find(|(start, v)| *start <= from && from < start + v.len())
                        .unwrap();
                    (*start, v)
                };
                // A group's values over this range, made from its leaves'.
                let joined: Vec<(usize, Values)> = groups
                    .iter()
                    .map(|&c| {
                        let parts = order
                            .slots(table, c)
                            .unwrap()
                            .map(|s| {
                                let (start, v) = run(s);
                                v.gather(&(from - start..to - start).collect::<Vec<_>>())
                            })
                            .collect();
                        (c, Values::from_leaves(table.columns[c].ty, parts))
                    })
                    .collect();
                // Each column read: the values that hold this range, and their first row.
                let column = |c: usize| -> (usize, &Values) {
                    match joined.iter().find(|j| j.0 == c) {
                        Some((_, v)) => (from, v),
                        None => run(order.leaf_slot(table, c).unwrap()),
                    }
                };
                let fixed = if br.interval_may {
                    &order.levels[..walk.m]
                } else {
                    &[][..]
                };
                let levels: Vec<(Level, bool, usize, &Values)> = fixed
                    .iter()
                    .map(|&l| {
                        let (start, v) = column(l.column());
                        (l, table.columns[l.column()].ty.is_text(), start, v)
                    })
                    .collect();
                let rests: Vec<(&Cond, usize, &Values)> = br
                    .rest_may
                    .iter()
                    .map(|&ri| {
                        let c = &walk.rest[ri];
                        let (start, v) = column(s_column(c.shape));
                        (c, start, v)
                    })
                    .collect();
                let texts: Vec<(&TextCond, usize, &Values)> = br
                    .text_may
                    .iter()
                    .map(|&ti| {
                        let t = &want.side.texts[ti];
                        let (start, v) = column(t.column);
                        (t, start, v)
                    })
                    .collect();
                let nulls: Vec<(bool, usize, &Values)> = br
                    .null_may
                    .iter()
                    .map(|&ni| {
                        let (c, null) = want.side.nulls[ni];
                        let (start, v) = column(c);
                        (null, start, v)
                    })
                    .collect();
                let outs: Vec<(bool, bool, usize, &Values)> = columns
                    .iter()
                    .map(|&(c, month)| {
                        let (start, v) = column(c);
                        (table.columns[c].ty.is_byte_array(), month, start, v)
                    })
                    .collect();
                let dicts: Vec<Dict> = outs.iter().map(|o| o.3.dict().cloned()).collect();
                if !same(&sent, &dicts) {
                    sink.dictionaries(&dicts);
                    sent = dicts;
                }
                // How a row's fixed levels compare with a bound, level by level, a NULL lying on its
                // level's side.
                let against = |r: usize, bound: &[KeyValue]| -> Ordering {
                    for (li, ((l, is_text, start, v), b)) in levels.iter().zip(bound).enumerate() {
                        let null = KeyValue::Null {
                            first: order.nulls_first[li],
                        };
                        let ord = match (v.is_null(r - start), is_text, b) {
                            (true, _, _) => null.cmp(b),
                            (false, _, KeyValue::Null { first }) => {
                                if *first {
                                    Ordering::Greater
                                } else {
                                    Ordering::Less
                                }
                            }
                            (false, true, KeyValue::Text(t)) => {
                                v.text(r - start).unwrap_or("").cmp(&**t)
                            }
                            (false, _, KeyValue::Int(x)) => v
                                .int(r - start)
                                .map_or(i64::MIN, |y| key(*l, y).int())
                                .cmp(x),
                            (false, _, KeyValue::Wide(x)) => {
                                v.wide(r - start).unwrap_or(i128::MIN).cmp(x)
                            }
                            _ => Ordering::Equal,
                        };
                        if ord != Ordering::Equal {
                            return ord;
                        }
                    }
                    Ordering::Equal
                };
                // Where the other side's keys lead the order, the rows of the block's sorted part that
                // hold them are found by their keys, and no other row of that part is visited.
                let spans = match want.keys {
                    Some(k) if lead > 0 && walk.keyed == lead => keyed_spans(
                        k,
                        &outs[..lead],
                        &order.nulls_first[..lead],
                        from,
                        to,
                        br.map.sorted as usize,
                    ),
                    _ => vec![(from, to)],
                };
                for (a, b) in spans {
                    let mut d = dead.partition_point(|&x| (x as usize) < a);
                    // A run every row of which is wanted goes to the sink at once, where it takes it.
                    if lead == 0
                        && !br.interval_may
                        && rests.is_empty()
                        && texts.is_empty()
                        && nulls.is_empty()
                        && dead.get(d).is_none_or(|&x| x as usize >= b)
                        && sink.run(&outs, a, b)
                    {
                        self.taken.set(self.taken.get() + (b - a) as u64);
                        continue;
                    }
                    self.walked.set(self.walked.get() + (b - a) as u64);
                    for r in a..b {
                        if dead.get(d).is_some_and(|&x| x as usize == r) {
                            d += 1;
                            continue;
                        }
                        if br.interval_may
                            && (against(r, &walk.lo) == Ordering::Less
                                || against(r, &walk.hi) == Ordering::Greater)
                        {
                            continue;
                        }
                        let keep = rests.iter().all(|&(c, start, v)| {
                            let x = match c.shape {
                                // An instant with no month is NULL to a condition on its month.
                                Shape::MonthUtc(_) => {
                                    v.int(r - start).and_then(month_of).map(i128::from)
                                }
                                _ => v.wide(r - start),
                            };
                            c.holds(x)
                        });
                        let keep =
                            keep && texts.iter().all(|&(t, start, v)| {
                                v.text(r - start).is_some_and(|x| t.holds(x))
                            }) && nulls
                                .iter()
                                .all(|&(null, start, v)| v.is_null(r - start) == null);
                        if !keep {
                            continue;
                        }
                        let make = |&(is_text, month, start, v): &(bool, bool, usize, &Values)| {
                            let i = r - start;
                            if let Values::Dict { codes, .. } = v {
                                codes[i].map_or(Cell::Null, Cell::Code)
                            } else if is_text {
                                v.text_value(i).map_or(Cell::Null, |t| Cell::Text(Text(t)))
                            } else if month {
                                v.int(i).and_then(month_of).map_or(Cell::Null, Cell::Int)
                            } else if let Values::Wide(w) = v {
                                w[i].map_or(Cell::Null, Cell::Wide)
                            } else if let (Some(key), Some(bits)) = (v.int(i), v.bits(i)) {
                                Cell::Float(crate::read::answer::Float { key, bits })
                            } else {
                                v.int(i).map_or(Cell::Null, Cell::Int)
                            }
                        };
                        for (cell, o) in cells[..lead].iter_mut().zip(&outs) {
                            *cell = make(o);
                        }
                        if lead > 0 && !sink.admits(&cells[..lead]) {
                            continue;
                        }
                        for (cell, o) in cells[lead..].iter_mut().zip(&outs[lead..]) {
                            *cell = make(o);
                        }
                        sink.row(&cells);
                    }
                }
            }
            sink.block_end()?;
        }
        Ok(())
    }
}

/// The rows from `from` to `to` a join's keys can lie in: in the block's sorted part, the runs whose
/// leading values are a key, found by the keys, unless the keys there are about as many as the rows;
/// past the sorted part, every row.
fn keyed_spans(
    keys: &KeySet,
    lead: &[(bool, bool, usize, &Values)],
    nulls_first: &[bool],
    from: usize,
    to: usize,
    sorted: usize,
) -> Vec<(usize, usize)> {
    // A row's leading values against a key, compared in place, a NULL lying on its level's side.
    let cmp = |r: usize, key: &[i128]| -> Ordering {
        lead.iter()
            .zip(nulls_first)
            .zip(key)
            .map(|((&(_, _, start, v), &first), k)| match v.wide(r - start) {
                Some(x) => x.cmp(k),
                None if first => Ordering::Less,
                None => Ordering::Greater,
            })
            .find(|o| *o != Ordering::Equal)
            .unwrap_or(Ordering::Equal)
    };
    let mut out = Vec::new();
    let end = to.min(sorted);
    if from < end {
        // The keys from the first the sorted rows reach to the last they reach.
        let below = |r: usize| -> usize {
            let (mut lo, mut hi) = (0, keys.len());
            while lo < hi {
                let mid = (lo + hi) / 2;
                if cmp(r, keys.key(mid)) == Ordering::Greater {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            lo
        };
        let (i, z) = (below(from), {
            let mut z = below(end - 1);
            while z < keys.len() && cmp(end - 1, keys.key(z)) != Ordering::Less {
                z += 1;
            }
            z
        });
        if (z - i) * 4 >= end - from {
            out.push((from, end));
        } else {
            // The first row from `lo` whose leading values pass `key`, or reach it.
            let find = |key: &[i128], mut lo: usize, past: bool| -> usize {
                let mut hi = end;
                while lo < hi {
                    let mid = (lo + hi) / 2;
                    let o = cmp(mid, key);
                    let before = o == Ordering::Less || (past && o == Ordering::Equal);
                    if before {
                        lo = mid + 1;
                    } else {
                        hi = mid;
                    }
                }
                lo
            };
            let mut at = from;
            for k in i..z {
                let key = keys.key(k);
                let a = find(key, at, false);
                let b = find(key, a, true);
                if a < b {
                    out.push((a, b));
                }
                at = b;
            }
        }
    }
    if from.max(end) < to {
        out.push((from.max(end), to));
    }
    out
}

/// What a question makes of one entry on the walk's level: none of its rows, so it is skipped; all of
/// a count, taken from its counts; or rows the round reads under it, left open with the columns it
/// reads. An open entry adds its columns' pages to the walk's bounds. Above the blocks the fewest are
/// those pages only where every row under the entry is read whole, since a block under it may hold none.
fn assess(walk: &mut Walk, e: Entry, order: &Order, want: &Want) {
    let m = walk.m;
    let side = want.side;
    let mut interval = if side.empty {
        Status::None
    } else if m == 0 {
        Status::Must
    } else {
        let (pf, pl) = (&e.first[..m], &e.last[..m]);
        if pl < walk.lo.as_slice() || pf > walk.hi.as_slice() {
            Status::None
        } else if walk.lo.as_slice() <= pf && pl <= walk.hi.as_slice() {
            Status::Must
        } else {
            Status::May
        }
    };
    let j = walk.keyed;
    let keys_may = match want.keys {
        Some(k) if j > 0 => {
            if !k.meets(&e.first[..j], &e.last[..j]) {
                interval = Status::None;
            }
            true
        }
        Some(_) => true,
        None => false,
    };
    let mut status = interval;
    let mut rest_may = Vec::new();
    let mut exact: Option<u64> = None;
    let table = want.table;
    for (ri, c) in walk.rest.iter().enumerate() {
        let (sum, slot) = summary_of(table, order, c, &e);
        let (st, n) = rest_status(sum, slot, c, &e, &walk.head);
        match st {
            Status::None => status = Status::None,
            Status::May => {
                rest_may.push(ri);
                exact = if rest_may.len() == 1 { n } else { None };
            }
            Status::Must => {}
        }
    }
    // A test for NULL is answered by the column's NULLs the entry counts, which each of its leaves
    // holds.
    let mut null_may = Vec::new();
    for &ni in &walk.nulls {
        let (c, null) = side.nulls[ni];
        let nulls = e.columns[order.leaf_slot(table, c).unwrap()].nulls;
        let n = if null { nulls } else { e.rows - nulls } as u64;
        if n == 0 {
            status = Status::None;
        } else if n < e.rows as u64 {
            null_may.push(ni);
            exact = if rest_may.len() + null_may.len() == 1 {
                Some(n)
            } else {
                None
            };
        }
    }
    let mut text_may = Vec::new();
    for &ti in &walk.texts {
        let t = &side.texts[ti];
        if e.columns[order.leaf_slot(table, t.column).unwrap()].nulls == e.rows {
            status = Status::None;
        } else {
            text_may.push(ti);
        }
    }
    let may = rest_may.len() + null_may.len();
    if status != Status::None && (may > 0 || !text_may.is_empty() || keys_may) {
        status = Status::May;
    }
    match status {
        Status::None => {
            walk.statuses.0 += 1;
            return;
        }
        Status::May => walk.statuses.1 += 1,
        Status::Must => walk.statuses.2 += 1,
    }
    if interval == Status::Must && text_may.is_empty() && !keys_may {
        match want.countable {
            Some(Countable::Total) => {
                if may == 0 {
                    walk.counted += e.rows as u64;
                    walk.rows_closed += e.rows as u64;
                    return;
                }
                if let (1, Some(n)) = (may, exact) {
                    walk.counted += n;
                    walk.rows_closed += n;
                    return;
                }
            }
            Some(Countable::ByMonth(c)) => {
                let slot = order.leaf_slot(table, c).unwrap();
                let mi = walk
                    .head
                    .month_slots
                    .iter()
                    .position(|&x| x as usize == slot);
                let months = match rest_may.as_slice() {
                    _ if !null_may.is_empty() => None,
                    [] => Some((1, 12)),
                    [ri] => match &walk.rest[*ri] {
                        Cond {
                            shape: Shape::MonthUtc(x),
                            kept,
                            ..
                        } if *x == c && exact.is_some() && walk.rest[*ri].is_range() => {
                            Some((kept.lo.max(1) as i64, kept.hi.min(12) as i64))
                        }
                        _ => None,
                    },
                    _ => None,
                };
                if let (Some(mi), Some((a, z)), 0) = (mi, months, e.columns[slot].nulls) {
                    for m in a..=z {
                        let n = e.months[mi][m as usize - 1] as u64;
                        walk.counted_months[m as usize - 1] += n;
                        walk.rows_closed += n;
                    }
                    return;
                }
            }
            None => {}
        }
    }
    // A column read is each of its leaves read.
    let mut slots: Vec<usize> = Vec::new();
    let mut need = |c: usize| {
        for s in order.slots(table, c).unwrap() {
            if !slots.contains(&s) {
                slots.push(s);
            }
        }
    };
    for &c in &want.columns {
        need(c);
    }
    if interval == Status::May {
        for l in &order.levels[..m] {
            need(l.column());
        }
    }
    for &ri in &rest_may {
        need(s_column(walk.rest[ri].shape));
    }
    for &ti in &text_may {
        need(side.texts[ti].column);
    }
    for &ni in &null_may {
        need(side.nulls[ni].0);
    }
    if let Some(k) = want.keys {
        for &c in &k.columns {
            need(c);
        }
    }
    let whole: u64 = slots.iter().map(|&s| e.columns[s].pages as u64).sum();
    let read_whole = interval == Status::Must && may == 0 && text_may.is_empty() && !keys_may;
    walk.low += if walk.level > 1 {
        if read_whole {
            whole
        } else {
            0
        }
    } else if interval == Status::Must && j == 0 {
        whole
    } else {
        slots.len() as u64
    };
    walk.high += whole;
    // The rows: every row of an entry read whole, the count where one condition's is exact, else
    // none to all of them.
    let rows = e.rows as u64;
    if read_whole {
        walk.rows_low += rows;
        walk.rows_high += rows;
    } else {
        match (interval, text_may.is_empty(), keys_may, may, exact) {
            (Status::Must, true, false, 1, Some(n)) => {
                walk.rows_low += n;
                walk.rows_high += n;
            }
            _ => walk.rows_high += rows,
        }
    }
    walk.open.push(Open {
        entry: e,
        interval_may: interval == Status::May,
        rest_may,
        text_may,
        null_may,
        slots,
    });
}

/// What an entry keeps of the column a condition reads: its one leaf's summary, or the one its
/// leaves' summaries make; and the slot of its first leaf.
fn summary_of(table: &Table, order: &Order, c: &Cond, e: &Entry) -> (Summary, usize) {
    let column = s_column(c.shape);
    let slots = order
        .slots(table, column)
        .expect("an order holds the columns read along it");
    let leaves = &e.columns[slots.clone()];
    if let [one] = leaves {
        return (*one, slots.start);
    }
    // A group's least and greatest: its leaves' least, and its leaves' greatest, each made into
    // the column's key as its leaves' values are.
    let ty = table.columns[column].ty;
    let key = |pick: fn(&Summary) -> i128| {
        let parts = leaves
            .iter()
            .map(|s| Values::Int(vec![Some(pick(s) as i64)]))
            .collect();
        Values::from_leaves(ty, parts).wide(0).unwrap_or(0)
    };
    let sum = Summary {
        nulls: leaves[0].nulls,
        min: key(|s| s.min),
        max: key(|s| s.max),
        pages: leaves.iter().map(|s| s.pages).sum(),
        wide: false,
    };
    (sum, slots.start)
}

/// A condition off the leading levels, against one entry's counts, `sum` what it keeps of the
/// column and `slot` the column's first leaf: whether no row, some or every row holds it, and how
/// many hold it where the counts say exactly.
fn rest_status(
    sum: Summary,
    slot: usize,
    c: &Cond,
    b: &Entry,
    head: &Head,
) -> (Status, Option<u64>) {
    let present = (b.rows - sum.nulls) as u64;
    let rows = b.rows as u64;
    let status = |n: u64| {
        if n == 0 {
            (Status::None, Some(0))
        } else if n == rows {
            (Status::Must, Some(n))
        } else {
            (Status::May, Some(n))
        }
    };
    let iv = c.kept;
    if !c.is_range() {
        // A test that keeps the column's NULLs takes them from the counts; `<>` passes over a value
        // the column's least and greatest leave out, and keeps none where both are that value.
        let nulls = if c.nulls { sum.nulls as u64 } else { 0 };
        let values = match c.shape {
            Shape::MonthUtc(_) => head
                .month_slots
                .iter()
                .position(|&x| x as usize == slot)
                .map(|mi| {
                    (1..=12i128)
                        .filter(|&m| c.holds(Some(m)))
                        .map(|m| b.months[mi][m as usize - 1] as u64)
                        .sum::<u64>()
                }),
            _ if present == 0 => Some(0),
            _ if sum.max < iv.lo || sum.min > iv.hi => Some(0),
            _ if sum.min == sum.max => Some(if c.holds(Some(sum.min)) { present } else { 0 }),
            _ if iv.lo <= sum.min
                && sum.max <= iv.hi
                && !c.except.iter().any(|&x| sum.min <= x && x <= sum.max) =>
            {
                Some(present)
            }
            _ => None,
        };
        return match values {
            Some(v) => status(v + nulls),
            None if nulls == rows => (Status::Must, Some(rows)),
            None => (Status::May, None),
        };
    }
    if present == 0 {
        return (Status::None, Some(0));
    }
    match c.shape {
        Shape::MonthUtc(_) => match head.month_slots.iter().position(|&x| x as usize == slot) {
            Some(mi) => status(
                (iv.lo.max(1)..=iv.hi.min(12))
                    .map(|m| b.months[mi][m as usize - 1] as u64)
                    .sum(),
            ),
            None => (Status::May, None),
        },
        _ => {
            if sum.max < iv.lo || sum.min > iv.hi {
                (Status::None, Some(0))
            } else if iv.lo <= sum.min && sum.max <= iv.hi {
                // Every value passes, and the column's NULLs the counts hold are left out.
                status(present)
            } else {
                (Status::May, None)
            }
        }
    }
}

/// The rows of the question an open block holds, estimated. The leading levels' range covers the
/// block page by page, each page's rows taken as lying evenly across its keys; each condition off
/// the levels covers the block by the share of its least to greatest it takes, or by its count
/// where the counts give one, and a test for NULL by the NULLs counted; a condition on text off the
/// levels leaves the block whole.
fn estimate(o: &Open, map: &PageMap, walk: &Walk, table: &Table, nulls: &[(usize, bool)]) -> f64 {
    let order = &table.orders[walk.order];
    let e = &o.entry;
    let mut share = 1.0;
    if o.interval_may {
        let m = walk.m;
        let finest = o
            .slots
            .iter()
            .copied()
            .max_by_key(|&s| map.columns[s].pages.len());
        if let Some(s) = finest {
            let pages = &map.columns[s].pages;
            let keyed = |p: usize| map.sorted > pages[p].first_row;
            let (mut covered, mut all) = (0.0, 0.0);
            for p in (0..pages.len()).filter(|&p| keyed(p)) {
                let rows = pages[p].rows as f64;
                let (next, last) = if p + 1 < pages.len() && keyed(p + 1) {
                    (&pages[p + 1].first[..m], false)
                } else {
                    (&e.last[..m], true)
                };
                all += rows;
                covered += rows * page_share(&pages[p].first[..m], next, last, &walk.lo, &walk.hi);
            }
            if all > 0.0 {
                share *= covered / all;
            }
        }
    }
    for &ri in &o.rest_may {
        let c = &walk.rest[ri];
        let (sum, slot) = summary_of(table, order, c, e);
        share *= rest_share(sum, slot, c, e, &walk.head);
    }
    for &ni in &o.null_may {
        let (c, null) = nulls[ni];
        let n = e.columns[order.leaf_slot(table, c).unwrap()].nulls as f64;
        share *= if null { n } else { e.rows as f64 - n } / (e.rows as f64).max(1.0);
    }
    e.rows as f64 * share
}

/// The share of a page's rows the range `lo` to `hi` covers, the page holding the keys from `first`
/// to before `next`, or to `next` itself where it is the block's last key: its rows taken as lying
/// evenly across the integers of the first level its two keys part on; half of them where that
/// level holds text.
fn page_share(
    first: &[KeyValue],
    next: &[KeyValue],
    last: bool,
    lo: &[KeyValue],
    hi: &[KeyValue],
) -> f64 {
    if next < lo || first > hi {
        return 0.0;
    }
    if lo <= first && next <= hi {
        return 1.0;
    }
    let Some(d) = (0..first.len()).find(|&d| first[d] != next[d]) else {
        return 1.0;
    };
    let (KeyValue::Int(a), KeyValue::Int(z), KeyValue::Int(l), KeyValue::Int(h)) =
        (&first[d], &next[d], &lo[d], &hi[d])
    else {
        return 0.5;
    };
    let (a, z) = (*a as f64, *z as f64 + if last { 1.0 } else { 0.0 });
    let from = match lo[..d].cmp(&first[..d]) {
        std::cmp::Ordering::Less => f64::NEG_INFINITY,
        std::cmp::Ordering::Greater => f64::INFINITY,
        std::cmp::Ordering::Equal => *l as f64,
    };
    let to = match hi[..d].cmp(&first[..d]) {
        std::cmp::Ordering::Greater => f64::INFINITY,
        std::cmp::Ordering::Less => f64::NEG_INFINITY,
        std::cmp::Ordering::Equal => *h as f64 + 1.0,
    };
    ((to.min(z) - from.max(a)).max(0.0) / (z - a)).clamp(0.0, 1.0)
}

/// The share of an entry's rows a condition off the leading levels covers: its count where the
/// counts give one, else the share of the column's least to greatest the condition's range takes,
/// of the rows that hold a value.
fn rest_share(sum: Summary, slot: usize, cond: &Cond, e: &Entry, head: &Head) -> f64 {
    if e.rows == 0 {
        return 0.0;
    }
    match rest_status(sum, slot, cond, e, head) {
        (Status::None, _) => 0.0,
        (_, Some(n)) => n as f64 / e.rows as f64,
        _ => {
            let c = sum;
            let held = (e.rows - c.nulls) as f64 / e.rows as f64;
            let nulls = if cond.nulls {
                c.nulls as f64 / e.rows as f64
            } else {
                0.0
            };
            let (a, z) = (c.min as f64, c.max as f64 + 1.0);
            let iv = cond.kept;
            let (from, to) = (iv.lo as f64, iv.hi as f64 + 1.0);
            nulls + held * ((to.min(z) - from.max(a)).max(0.0) / (z - a)).clamp(0.0, 1.0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directory::Summary;
    use crate::read::question::Interval;

    #[test]
    fn a_pages_rows_lie_evenly_across_the_integers_its_keys_part_on() {
        let k = |v: &[i64]| v.iter().map(|&x| KeyValue::Int(x)).collect::<Vec<_>>();
        // The keys 10 to before 20, the range 15 to 100: half of them.
        assert_eq!(
            page_share(&k(&[10]), &k(&[20]), false, &k(&[15]), &k(&[100])),
            0.5
        );
        // The block's last page holds its last key itself: 10 to 19.
        assert_eq!(
            page_share(&k(&[10]), &k(&[19]), true, &k(&[15]), &k(&[100])),
            0.5
        );
        assert_eq!(
            page_share(&k(&[10]), &k(&[20]), false, &k(&[0]), &k(&[100])),
            1.0
        );
        assert_eq!(
            page_share(&k(&[10]), &k(&[20]), false, &k(&[30]), &k(&[100])),
            0.0
        );
        // A first level the question fixes: the second parts the keys, 10 to 19 of 0 to before 40.
        assert_eq!(
            page_share(&k(&[7, 0]), &k(&[7, 40]), false, &k(&[7, 10]), &k(&[7, 19])),
            0.25
        );
        // Text holds no integers to spread the rows across: half of them.
        let t = |s: &str| vec![KeyValue::Text(s.into())];
        assert_eq!(page_share(&t("a"), &t("m"), false, &t("c"), &t("z")), 0.5);
    }

    #[test]
    fn a_condition_off_the_levels_covers_a_block_by_its_month_count_or_its_least_to_greatest() {
        // 120 rows: ids from 1 to 100 on all but 20 of them; instants on every month, December 21.
        let mut months = [9u32; 12];
        months[11] = 21;
        let block = Entry {
            depth: 1,
            rows: 120,
            dead: 0,
            first: vec![KeyValue::Int(0)],
            last: vec![KeyValue::Int(0)],
            columns: vec![
                Summary {
                    nulls: 20,
                    min: 1,
                    max: 100,
                    pages: 1,
                    ..Default::default()
                },
                Summary::default(),
            ],
            months: vec![months],
            bytes: vec![],
            child: Extent::default(),
        };
        let head = Head {
            month_slots: vec![1],
            ..Default::default()
        };
        let december = Cond::new(Shape::MonthUtc(1), Interval { lo: 12, hi: 12 });
        assert_eq!(
            rest_share(block.columns[1], 1, &december, &block, &head),
            21.0 / 120.0
        );
        let ids = Cond::new(Shape::Column(0), Interval { lo: 1, hi: 25 });
        assert_eq!(
            rest_share(block.columns[0], 0, &ids, &block, &head),
            100.0 / 120.0 * 0.25
        );
        let none = Cond::new(Shape::Column(0), Interval { lo: 200, hi: 300 });
        assert_eq!(rest_share(block.columns[0], 0, &none, &block, &head), 0.0);
    }

    #[test]
    fn a_comparison_every_value_passes_is_counted_less_the_nulls() {
        // 100 rows, `nulls` of them NULL, the rest from 1 to 50.
        let block = |nulls: u32| Entry {
            depth: 1,
            rows: 100,
            dead: 0,
            first: vec![KeyValue::Int(0)],
            last: vec![KeyValue::Int(0)],
            columns: vec![Summary {
                nulls,
                min: 1,
                max: 50,
                pages: 1,
                ..Default::default()
            }],
            months: vec![],
            bytes: vec![],
            child: Extent::default(),
        };
        let head = Head::default();
        let status = |lo: i128, hi: i128, b: &Entry| {
            let c = Cond::new(Shape::Column(0), Interval { lo, hi });
            rest_status(b.columns[0], 0, &c, b, &head)
        };
        assert_eq!(status(1, 60, &block(20)), (Status::May, Some(80)));
        assert_eq!(status(1, 60, &block(0)), (Status::Must, Some(100)));
        assert_eq!(status(60, 70, &block(20)), (Status::None, Some(0)));
        // A bound among the values: the counts cannot say.
        assert_eq!(status(10, 60, &block(20)), (Status::May, None));
        // Every row NULL: none.
        assert_eq!(status(1, 60, &block(100)), (Status::None, Some(0)));
    }

    #[test]
    fn unequal_and_a_test_keeping_nulls_are_counted_where_the_counts_close() {
        // 100 rows, 20 of them NULL, the rest from 1 to 50.
        let block = |min: i128, max: i128, nulls: u32| Entry {
            depth: 1,
            rows: 100,
            dead: 0,
            first: vec![KeyValue::Int(0)],
            last: vec![KeyValue::Int(0)],
            columns: vec![Summary {
                nulls,
                min,
                max,
                pages: 1,
                ..Default::default()
            }],
            months: vec![],
            bytes: vec![],
            child: Extent::default(),
        };
        let head = Head::default();
        let status = |c: &Cond, b: &Entry| rest_status(b.columns[0], 0, c, b, &head);
        let unequal = |v: i128| {
            let mut c = Cond::new(Shape::Column(0), Interval::ALL);
            c.except.push(v);
            c
        };
        // A value the least and greatest leave out: every value kept, no NULL.
        assert_eq!(
            status(&unequal(60), &block(1, 50, 20)),
            (Status::May, Some(80))
        );
        assert_eq!(
            status(&unequal(60), &block(1, 50, 0)),
            (Status::Must, Some(100))
        );
        // Every value is the one left out: none kept.
        assert_eq!(
            status(&unequal(7), &block(7, 7, 20)),
            (Status::None, Some(0))
        );
        // A value among them: the counts cannot say.
        assert_eq!(status(&unequal(7), &block(1, 50, 0)), (Status::May, None));
        // IS NOT TRUE: false and the NULLs.
        let mut not_true = Cond::new(Shape::Column(0), Interval::at(0));
        not_true.nulls = true;
        assert_eq!(
            status(&not_true, &block(0, 0, 20)),
            (Status::Must, Some(100))
        );
        assert_eq!(status(&not_true, &block(1, 1, 20)), (Status::May, Some(20)));
        assert_eq!(status(&not_true, &block(1, 1, 0)), (Status::None, Some(0)));
        assert_eq!(
            status(&not_true, &block(0, 0, 100)),
            (Status::Must, Some(100))
        );
        assert_eq!(status(&not_true, &block(0, 1, 20)), (Status::May, None));
    }

    #[test]
    fn an_order_is_left_unread_only_when_its_fewest_pages_pass_another_orders_most() {
        // One order priced exactly, the other between 3 and 50 pages: both must be read to know.
        assert_eq!(ruled_out(&[(3, 50), (10, 10)]), [false, false]);
        assert_eq!(ruled_out(&[(742, 742), (24, 24)]), [true, false]);
        assert_eq!(ruled_out(&[(24, 24), (24, 24)]), [false, false]);
        assert_eq!(ruled_out(&[(30, 40), (12, 29)]), [true, false]);
    }

    #[test]
    fn the_order_whose_fewest_pages_are_fewest_reads_further_down_first() {
        assert_eq!(next_down([(0, 166), (2, 3), (3, 9)]), Some(2));
        assert_eq!(next_down([(4, 3), (1, 3)]), Some(4));
        assert_eq!(next_down([]), None);
    }

    #[test]
    fn a_key_set_meets_a_range_that_holds_one_of_its_keys() {
        use crate::places::ints;
        let keys = KeySet::new(
            vec![0, 1],
            &[vec![3, 1], vec![7, 2], vec![3, 9], vec![3, 1]],
        );
        assert!(keys.meets(&ints([3, 0]), &ints([3, 1])));
        assert!(keys.meets(&ints([3, 1]), &ints([3, 1])));
        assert!(keys.meets(&ints([3, 2]), &ints([5, 0])));
        assert!(!keys.meets(&ints([3, 2]), &ints([3, 8])));
        assert!(!keys.meets(&ints([7, 3]), &ints([9, 9])));
    }
}
