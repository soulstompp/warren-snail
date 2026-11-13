//! One table's round: its orders priced from their directories' tops, walked down only where the
//! question may hold rows, its pages predicted from the page maps, then read.

use super::answer::{same, Answer, Cell, Dict, Text};
use super::question::{Cond, Countable, OpCond, Side, TextCond};
use super::{Snail, Wasted};
use crate::catalog::{ColumnType, Level, Order, Shape, Table};
use crate::cut::PAGE;
use crate::directory::{
    below_least, decode_page, decode_top, map_run_of, past_greatest, Entry, Head, PageMap, Summary,
    Tail,
};
use crate::encode::decode_leaf_with;
use crate::file::Extent;
use crate::geo::{GeoKind, Point};
use crate::places::{level_key, month_of, null_key, KeyValue};
use crate::plane::Near;
use crate::room::{HeldIn, Room};
use crate::values::Values;
use crate::{Error, Result};
use std::cell::{Cell as Counted, RefCell};
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::ops::Bound;
use std::rc::Rc;

/// A column's collator, where a library orders its text.
type Coll = Option<&'static crate::collations::Collator>;

/// Where a scan hands the rows it keeps.
pub trait Sink {
    /// The rows the counts say the round may give at most, told before its first.
    fn rows_most(&mut self, _n: u64) {}
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

    fn block_end(&mut self) -> Result<()> {
        self.within_room()
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
    /// Each key's values as this table's levels key them, the keys sorted, laid flat.
    keys: Vec<KeyValue>,
}

impl KeySet {
    pub fn new(columns: Vec<usize>, keys: impl IntoIterator<Item = Vec<KeyValue>>) -> KeySet {
        let mut sorted: Vec<Vec<KeyValue>> = keys.into_iter().collect();
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

    fn key(&self, i: usize) -> &[KeyValue] {
        let w = self.columns.len();
        &self.keys[i * w..(i + 1) * w]
    }

    /// Whether some key lies from `first` to `last`, both read on their first `j` values.
    fn meets(&self, first: &[KeyValue], last: &[KeyValue], open: bool) -> bool {
        let (mut lo, mut hi) = (0, self.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.key(mid) < first {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo < self.len()
            && if open {
                self.key(lo) < last
            } else {
                self.key(lo) <= last
            }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    None,
    May,
    Must,
}

/// What of an entry the round reads is left open, the same for most entries in a row, which share one.
#[derive(PartialEq)]
struct Reading {
    interval_may: bool,
    /// The conditions off the leading levels that the entry's counts leave open.
    rest_may: Vec<usize>,
    /// The conditions on text the entry may hold.
    text_may: Vec<usize>,
    /// The tests for NULL the entry's counts leave open.
    null_may: Vec<usize>,
    /// The conditions by an operator a GiST or SP-GiST strand answers that the entry may hold.
    op_may: Vec<usize>,
    /// The columns the round reads under it.
    slots: Vec<usize>,
}

/// An entry's keys, where the round goes on needing them: above the blocks, and at a block whose pages
/// are chosen by them or which a writer may prune.
struct Ends {
    first: Vec<KeyValue>,
    last: Vec<KeyValue>,
    open: bool,
}

/// The bytes a key holds.
fn key_bytes(key: &[KeyValue]) -> u64 {
    let held: usize = key
        .iter()
        .map(|v| match v {
            KeyValue::Text(s) => s.len(),
            KeyValue::Bytes(b) => b.len(),
            _ => 0,
        })
        .sum();
    (std::mem::size_of_val(key) + held) as u64
}

/// An entry the round reads below, or at the blocks reads the pages of.
struct Open {
    reading: Rc<Reading>,
    /// The directory page below it, or at the blocks what it names of the block's page map.
    child: Extent,
    rows: u32,
    /// At the blocks, the disk pages of the columns it reads.
    pages: u32,
    /// At the blocks, the share of its rows the conditions off the leading levels cover.
    share: f64,
    ends: Option<Box<Ends>>,
}

impl Open {
    /// The bytes it holds past its place among the entries left open.
    fn held_past(&self) -> u64 {
        self.ends.as_ref().map_or(0, |e| {
            (std::mem::size_of::<Ends>()) as u64 + key_bytes(&e.first) + key_bytes(&e.last)
        })
    }
}

/// One order, as a round walks down its directory from the top.
pub struct Walk {
    pub order: usize,
    head: Head,
    /// The leading levels the question fixes, and their bounds.
    m: usize,
    lo: Vec<KeyValue>,
    hi: Vec<KeyValue>,
    /// Whether the last level fixed leaves out its bound's own key, at its low end and at its high.
    ends_open: (bool, bool),
    rest: Vec<Cond>,
    /// The conditions on text no level fixes, by their place among the side's.
    pub(super) texts: Vec<usize>,
    /// The tests for NULL no level fixes, by their place among the side's.
    nulls: Vec<usize>,
    /// The key of each BRIN summary its entries keep.
    summed: Vec<Level>,
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
    /// Whether the round marks what it looks at; each block it looked at, by its first key, that a
    /// key the question holds belongs with, whether or not it reads a row of it; whether it stopped
    /// above the blocks anywhere, so that it marks the order whole; and whether its conditions hold
    /// no row whatever the table holds.
    marking: bool,
    pub(super) marks: Vec<Vec<KeyValue>>,
    pub(super) upper: bool,
    pub(super) empty: bool,
    /// Whether the round may prune what it reads, so its blocks' first keys go on being needed.
    writing: bool,
    /// What it reads of the entry last left open.
    reading: Option<Rc<Reading>>,
    /// The bytes of each leaf under the blocks left open that it reads whole, as their entries give them,
    /// and each leaf's width where it has one.
    whole_bytes: Vec<u64>,
    widths: Vec<Option<u64>>,
    /// What the entries left open hold, and then the plan, counted in the round's room.
    held: HeldIn,
    open_bytes: u64,
    /// Where it walks a strand nearest first: the point and its box's place in an entry; and how near
    /// each entry left open may lie.
    nearest: Option<Nearest>,
    near: Vec<Near>,
}

impl Walk {
    pub fn is_open(&self) -> bool {
        !self.open.is_empty()
    }

    /// Whether a key ordered so against the low end of the run the leading levels fix lies before it.
    fn short_of(&self, o: Ordering) -> bool {
        o == Ordering::Less || (o == Ordering::Equal && self.ends_open.0)
    }

    /// Whether a key ordered so against the run's high end lies past it.
    fn over(&self, o: Ordering) -> bool {
        o == Ordering::Greater || (o == Ordering::Equal && self.ends_open.1)
    }

    /// Whether keys of the leading levels lie before the run they fix.
    fn before(&self, k: &[KeyValue]) -> bool {
        self.short_of(k.cmp(self.lo.as_slice()))
    }

    /// Whether keys of the leading levels lie past the run they fix.
    fn past(&self, k: &[KeyValue]) -> bool {
        self.over(k.cmp(self.hi.as_slice()))
    }

    /// The one key of the table's own order, a unique one, the walk fixes whole, each of its levels a
    /// column by `=`: at most one row, which a serializable round marks by itself.
    pub(super) fn point(&self, order: &Order) -> Option<&[KeyValue]> {
        Walk::one_key(order, self.m, &self.lo, &self.hi, self.ends_open)
            .then_some(self.lo.as_slice())
    }

    /// Whether a run of keys of the leading levels is one key of the table's own order, a unique
    /// one, fixed whole, each of its levels a column by `=`, none NULL: at most one row.
    fn one_key(
        order: &Order,
        m: usize,
        lo: &[KeyValue],
        hi: &[KeyValue],
        ends_open: (bool, bool),
    ) -> bool {
        let whole = order.unique
            && order.table_order
            && m == order.levels.len()
            && order.levels.iter().all(|l| matches!(l, Level::Column(_)))
            && lo == hi
            && ends_open == (false, false);
        whole && !lo.iter().any(KeyValue::is_null)
    }

    /// Counts what the entries left open hold now in the round's room.
    fn count_open(&mut self) {
        let now = (self.open.capacity() * std::mem::size_of::<Open>()) as u64
            + self.open.iter().map(Open::held_past).sum::<u64>();
        if now > self.open_bytes {
            self.held.more(now - self.open_bytes);
        } else {
            self.held.less(self.open_bytes - now);
        }
        self.open_bytes = now;
    }

    /// The directory pages the round reads next, under the entries left open: a directory page each
    /// above the blocks, and the pages that hold the blocks' page maps at them, each once.
    pub fn below(&self) -> impl Iterator<Item = u64> + '_ {
        let pages: std::collections::BTreeSet<u64> = self
            .open
            .iter()
            .flat_map(|o| {
                let run = if self.level == 1 {
                    map_run_of(o.child).0
                } else {
                    o.child
                };
                run.start..run.start + run.pages()
            })
            .collect();
        pages.into_iter()
    }
}

/// The run of keys a side's conditions fix the leading levels of an order to, each by IS NULL on its
/// column, by an equality on it, or by a range, the last level fixed by a range ending the run; and
/// which conditions fixed them, each read by the walk no further.
struct Fixed {
    lo: Vec<KeyValue>,
    hi: Vec<KeyValue>,
    ends_open: (bool, bool),
    used: Vec<bool>,
    used_texts: Vec<bool>,
    used_nulls: Vec<bool>,
}

/// The one key of the table's own order a side's conditions fix whole, as a walk of it reads it by
/// itself (`Walk::point`), and the side's conditions that fix it.
pub(super) fn point_of(table: &Table, order: &Order, side: &Side) -> Option<(Vec<KeyValue>, Side)> {
    let f = fixed(table, order, side);
    if !Walk::one_key(order, f.lo.len(), &f.lo, &f.hi, f.ends_open) {
        return None;
    }
    let fixing = Side {
        conds: (side.conds.iter().zip(&f.used))
            .filter(|(_, u)| **u)
            .map(|(c, _)| c.clone())
            .collect(),
        texts: (side.texts.iter().zip(&f.used_texts))
            .filter(|(_, u)| **u)
            .map(|(t, _)| t.clone())
            .collect(),
        nulls: Vec::new(),
        empty: false,
        ops: Vec::new(),
        ..side.clone()
    };
    Some((f.lo, fixing))
}

/// What a side's conditions fix of an order's leading levels.
fn fixed(table: &Table, order: &Order, side: &Side) -> Fixed {
    let (conds, texts, nulls) = (&side.conds, &side.texts, &side.nulls);
    let mut used = vec![false; conds.len()];
    let mut used_texts = vec![false; texts.len()];
    let mut used_nulls = vec![false; nulls.len()];
    let (mut lo, mut hi) = (Vec::new(), Vec::new());
    let mut ends_open = (false, false);
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
        // A level of a string type, bytea or a bit string is fixed by an equality on it, or by a
        // range, each end its bound's key; an end left open by the NULLs on its side, which no
        // value is.
        if let Level::Column(c) = *l {
            let ty = table.columns[c].ty;
            if ty.keyed_by_text() || ty.keyed_by_bytes() {
                let column = &table.columns[c];
                let fixed = texts.iter().position(|t| {
                    t.column == c && t.gaps.is_empty() && matches!((&t.lo, &t.hi), (Bound::Included(a), Bound::Included(b)) if a == b)
                });
                if let Some(i) = fixed {
                    used_texts[i] = true;
                    let Bound::Included(v) = &texts[i].lo else {
                        unreachable!()
                    };
                    lo.push(crate::places::column_byte_key(column, v));
                    hi.push(crate::places::column_byte_key(column, v));
                    continue;
                }
                let ranged = texts.iter().position(|t| {
                    t.column == c
                        && t.is_range()
                        && !matches!((&t.lo, &t.hi), (Bound::Unbounded, Bound::Unbounded))
                });
                if let Some(i) = ranged {
                    used_texts[i] = true;
                    let end = |b: &Bound<Vec<u8>>, first: bool| match b {
                        Bound::Included(v) => (crate::places::column_byte_key(column, v), false),
                        Bound::Excluded(v) => (crate::places::column_byte_key(column, v), true),
                        Bound::Unbounded => (KeyValue::Null { first }, true),
                    };
                    let (l, lo_open) = end(&texts[i].lo, true);
                    let (h, hi_open) = end(&texts[i].hi, false);
                    lo.push(l);
                    hi.push(h);
                    ends_open = (lo_open, hi_open);
                }
                break;
            }
        }
        let wanted = match *l {
            Level::Column(c) => Shape::Column(c),
            Level::MonthUtc(c) => Shape::MonthUtc(c),
            Level::Scaled(c, k) => Shape::Scaled(c, k),
        };
        // A level is fixed by a range on it; `<>`, and a test that keeps NULLs, stay
        // conditions to read.
        match conds.iter().position(|c| c.shape == wanted && c.is_range()) {
            Some(i) => {
                used[i] = true;
                let iv = conds[i].kept;
                let bound = |v: i128| {
                    if table.columns[l.column()].ty.is_wide() {
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
    Fixed {
        lo,
        hi,
        ends_open,
        used,
        used_texts,
        used_nulls,
    }
}

/// A strand's walk nearest first: the point, its column's kind, and where an entry keeps its box.
struct Nearest {
    point: Point,
    kind: GeoKind,
    span_at: usize,
}

/// A strand's entries as a walk nearest first has them left: each by how near it may lie, then the
/// lower level, then as they came.
pub(super) struct Best {
    walk: Option<Walk>,
    queue: BTreeMap<(Near, u32, u64), Open>,
    seq: u64,
    /// What the walk held in the round's room as it made the plan under way.
    before_plan: u64,
}

/// The bytes an entry takes in the queue.
fn queued_bytes(o: &Open) -> u64 {
    (std::mem::size_of::<(Near, u32, u64)>() + std::mem::size_of::<Open>()) as u64 + o.held_past()
}

impl Best {
    /// How near the nearest entry left may lie.
    pub(super) fn next(&self) -> Option<Near> {
        self.queue.keys().next().map(|k| k.0)
    }

    /// The walk under way.
    pub(super) fn walk(&self) -> &Walk {
        self.walk.as_ref().expect("a walk under way")
    }

    /// The walk under way, which marks the order whole: a block it leaves unread may take a row nearer
    /// than the rows it finds.
    pub(super) fn mark_whole(&mut self) -> &Walk {
        let w = self.walk.as_mut().expect("a walk under way");
        w.upper = true;
        w
    }

    /// The walk back from a plan read, what the plan held let go.
    pub(super) fn read(&mut self, plan: Plan) {
        let w = plan.walk;
        w.held.less(w.held.holds().saturating_sub(self.before_plan));
        self.walk = Some(w);
    }

    /// The entries the walk has just left open, queued.
    fn queue_open(&mut self) {
        let w = self.walk.as_mut().expect("a walk under way");
        let open = std::mem::take(&mut w.open);
        let near = std::mem::take(&mut w.near);
        w.count_open();
        for (o, n) in open.into_iter().zip(near) {
            self.seq += 1;
            w.held.more(queued_bytes(&o));
            self.queue.insert((n, w.level, self.seq), o);
        }
    }
}

/// What a block read in part goes on needing: its last key, where its pages are chosen by keys, and
/// the rows a sample keeps.
struct Partly {
    last: Vec<KeyValue>,
    only: Option<Vec<u32>>,
}

/// A block a plan reads: what of it is left open, where its page map lies, its live rows and the disk
/// pages its reads take. Its map is read when it is reached, and the pages to read chosen again from it.
struct BlockRead {
    reading: Rc<Reading>,
    child: Extent,
    rows: u32,
    pages: u32,
    partly: Option<Box<Partly>>,
}

impl BlockRead {
    fn last(&self) -> &[KeyValue] {
        self.partly.as_ref().map_or(&[][..], |p| &p.last)
    }

    fn only(&self) -> Option<&[u32]> {
        self.partly.as_ref().and_then(|p| p.only.as_deref())
    }
}

/// The directory pages of its page maps a read touched as its blocks were reached: all of them, those
/// found held, and those read from the file.
#[derive(Clone, Copy, Default)]
pub(super) struct MapsRead {
    pub(super) touched: u64,
    pub(super) held: u64,
    pub(super) file: u64,
}

pub struct Plan {
    pub walk: Walk,
    reads: Vec<BlockRead>,
    pub column_pages: u64,
    /// The rows the question holds, estimated from the blocks' page maps: those the counts closed on,
    /// and of each block read in part, its rows by the share of it each open condition covers.
    pub rows_estimate: f64,
    /// The pages of page maps the plan holds, as long as its round's room lets it, and those read last
    /// as a block was reached.
    maps: RefCell<MapPagesHeld>,
    /// The directory pages the read touches of page maps as its blocks are reached, known before it
    /// reads a column page, and what it has touched so far.
    pub map_pages: u64,
    maps_read: Counted<MapsRead>,
    /// Of the column pages, those held when the plan was made, and those found held as the read reached
    /// them.
    pub held: u64,
    held_read: Counted<u64>,
    /// The bytes of the pages it reads of each leaf.
    written: Vec<u64>,
}

impl Plan {
    /// The bytes a row of the blocks the plan reads holds of the held leaves `slots`, as it writes them.
    pub(super) fn bytes_a_row(&self, slots: std::ops::Range<usize>) -> u64 {
        let bytes: u64 = self.written[slots].iter().sum();
        bytes / self.live_rows().max(1)
    }

    /// The live rows of the blocks the plan reads.
    pub(super) fn live_rows(&self) -> u64 {
        self.reads.iter().map(|br| u64::from(br.rows)).sum()
    }

    /// Of the blocks it reads, the most rows one holds, and the most and the fewest of their pages it
    /// reads.
    pub(super) fn block_bounds(&self) -> (u32, u32, u32) {
        let rows = self.reads.iter().map(|br| br.rows).max().unwrap_or(0);
        let pages = self.reads.iter().map(|br| br.pages);
        (
            rows,
            pages.clone().max().unwrap_or(0),
            pages.min().unwrap_or(0),
        )
    }

    pub fn blocks_read(&self) -> u64 {
        self.reads.len() as u64
    }

    /// The directory pages its read has touched of page maps so far.
    pub(super) fn maps_read(&self) -> MapsRead {
        self.maps_read.get()
    }

    /// The column pages its read has found held so far.
    pub(super) fn held_read(&self) -> u64 {
        self.held_read.get()
    }

    /// The directory pages reaching its blocks touches, each block's map found among those the plan
    /// holds or those last read.
    fn count_map_pages(&mut self) {
        let mut last: Option<(u64, u64)> = None;
        let mut n = 0;
        let maps = &self.maps.get_mut().pages;
        for br in &self.reads {
            let run = map_run_of(br.child).0;
            let pages = run.pages();
            let enough = pages * PAGE as u64;
            let held = maps
                .get(&run.start)
                .is_some_and(|b| b.len() as u64 >= enough);
            if held || matches!(last, Some((s, p)) if s == run.start && p >= pages) {
                continue;
            }
            n += pages;
            last = Some((run.start, pages));
        }
        self.map_pages = n;
    }
}

/// The pages of a block's map a read reads of a leaf, ascending, and the disk pages among them read
/// only for rows written after the sorted ones: a page holding such rows is read whenever the block
/// is, and a page's next key bounds it only while that key is a sorted row's. A page of the tail holds
/// no sorted row, so no key range brings a read to it.
fn leaf_pages(
    map: &PageMap,
    slot: usize,
    interval_may: bool,
    walk: &Walk,
    keys: Option<&KeySet>,
    last: &[KeyValue],
) -> (Vec<usize>, u64) {
    let (m, j) = (walk.m, walk.keyed);
    let pages = &map.columns[slot].pages;
    let unsorted = |p: usize| pages[p].first_row + pages[p].rows > map.sorted;
    let next = |p: usize| {
        if p + 1 < pages.len() && !unsorted(p) && pages[p + 1].first_row < map.sorted {
            &pages[p + 1].first[..]
        } else {
            last
        }
    };
    let fits = |p: usize| {
        pages[p].first_row < map.sorted
            && (!interval_may || (!walk.past(&pages[p].first[..m]) && !walk.before(&next(p)[..m])))
            && (j == 0
                || keys
                    .unwrap()
                    .meets(&pages[p].first[..j], &next(p)[..j], false))
    };
    let chosen: Vec<usize> = (0..pages.len())
        .filter(|&p| unsorted(p) || fits(p))
        .collect();
    let extra = chosen
        .iter()
        .filter(|&&p| !fits(p))
        .map(|&p| pages[p].at.pages())
        .sum();
    (chosen, extra)
}

/// Whether a page holds one of these rows.
fn holds_any(page: &crate::directory::PageEntry, rows: &[u32]) -> bool {
    let (a, z) = (page.first_row, page.first_row + page.rows);
    rows.iter().any(|&r| a <= r && r < z)
}

/// For every column a block's read reads, its slot and the pages read of it, ascending.
fn pages_read_of(
    map: &PageMap,
    br: &BlockRead,
    walk: &Walk,
    keys: Option<&KeySet>,
) -> Vec<(usize, Vec<usize>)> {
    br.reading
        .slots
        .iter()
        .map(|&s| {
            let (mut chosen, _) =
                leaf_pages(map, s, br.reading.interval_may, walk, keys, br.last());
            if let Some(rows) = br.only() {
                chosen.retain(|&p| holds_any(&map.columns[s].pages[p], rows));
            }
            (s, chosen)
        })
        .collect()
}

/// Every disk page these reads of a block's columns take, the dictionaries' among them.
fn disk_pages<'a>(
    map: &'a PageMap,
    reads: &'a [(usize, Vec<usize>)],
) -> impl Iterator<Item = u64> + 'a {
    let span = |e: Extent| e.start..e.start + e.pages();
    reads
        .iter()
        .filter(|(_, chosen)| !chosen.is_empty())
        .flat_map(move |(s, chosen)| {
            let cp = &map.columns[*s];
            cp.dictionary
                .into_iter()
                .flat_map(span)
                .chain(chosen.iter().flat_map(move |&p| span(cp.pages[p].at)))
        })
}

/// The share of a plan's room the page maps it holds may take, the rest kept for the rows it reads.
const MAPS_SHARE: u64 = 4;

/// What the plan's read counts of a page map's pages: those it holds, by their first, and those last
/// read.
#[derive(Default)]
struct MapPagesHeld {
    pages: BTreeMap<u64, Vec<u8>>,
    bytes: u64,
    last: Option<(u64, Vec<u8>)>,
}

impl MapPagesHeld {
    /// The bytes of the run a map lies in, where they are held.
    fn found(&self, start: u64, pages: u64) -> Option<&[u8]> {
        let enough = pages as usize * PAGE;
        self.pages
            .get(&start)
            .filter(|b| b.len() >= enough)
            .map(|b| &b[..])
            .or_else(|| {
                self.last
                    .as_ref()
                    .filter(|(s, b)| *s == start && b.len() >= enough)
                    .map(|(_, b)| &b[..])
            })
    }

    /// Holds the bytes of a run just read, as long as the room lets it; or as the last read.
    fn take(&mut self, start: u64, bytes: Vec<u8>, held: &HeldIn) {
        let cost = bytes.len() as u64;
        if self.bytes + cost <= held.size() / MAPS_SHARE && held.fits(cost) {
            self.bytes += cost;
            self.pages.insert(start, bytes);
        } else {
            self.last = Some((start, bytes));
        }
    }
}

/// The bytes a block read holds in a plan.
fn block_held(br: &BlockRead) -> u64 {
    std::mem::size_of::<BlockRead>() as u64
        + br.partly.as_ref().map_or(0, |p| {
            std::mem::size_of::<Partly>() as u64
                + key_bytes(&p.last)
                + p.only.as_ref().map_or(0, |o| 4 * o.len() as u64)
        })
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
        Shape::Column(c) | Shape::Clock(c) | Shape::MonthUtc(c) | Shape::Scaled(c, _) => c,
    }
}

/// What an OR or a NOT reads of a block: its columns' names, and those past its first, each with its
/// type and where its values start.
type OpBeside<'a> = (Vec<String>, Vec<(ColumnType, usize, &'a Values)>);

/// The columns a side's own conditions read.
pub fn condition_columns(side: &Side) -> Vec<usize> {
    side.conds
        .iter()
        .map(|c| s_column(c.shape))
        .chain(side.texts.iter().map(|t| t.column))
        .chain(side.nulls.iter().map(|&(c, _)| c))
        .chain(side.ops.iter().map(|o| o.column))
        .chain(side.ops.iter().flat_map(|o| o.beside.iter().map(|b| b.0)))
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
        self.start_near(oi, want, None)
    }

    /// A round along one order begun at its top, each entry left open weighed by how near a point it
    /// may lie, where a point of a column of the plane is given.
    fn start_near(
        &self,
        oi: usize,
        want: &Want,
        point: Option<(usize, Point)>,
    ) -> Result<Walk> {
        let order = &want.table.orders[oi];
        let (head, entries, pages) = self.top(want.table, oi)?;
        crate::write::cut_checked(want.table, order, &head)?;
        let (conds, texts, nulls) = (&want.side.conds, &want.side.texts, &want.side.nulls);
        let Fixed {
            lo,
            hi,
            ends_open,
            used,
            used_texts,
            used_nulls,
        } = fixed(want.table, order, want.side);
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
        let whole_bytes = vec![0; head.units.len()];
        let nearest = point.and_then(|(c, point)| {
            let slot = order.leaf_slot(want.table, c)? as u32;
            let span_at = head.span_slots.iter().position(|&s| s == slot)?;
            match want.table.columns[c].ty {
                ColumnType::Geometric(kind) => Some(Nearest {
                    point,
                    kind,
                    span_at,
                }),
                _ => None,
            }
        });
        let widths = order
            .leaves(want.table)
            .iter()
            .map(|(_, l)| l.ty.width().map(|w| w as u64))
            .collect();
        let mut walk = Walk {
            order: oi,
            head,
            m,
            lo,
            hi,
            ends_open,
            rest,
            texts,
            nulls,
            summed: crate::brin::kept(want.table, order)
                .into_iter()
                .map(|(l, _)| l)
                .collect(),
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
            marking: self.marking(),
            marks: Vec::new(),
            upper: false,
            empty: want.side.empty,
            writing: self.writer.is_some(),
            reading: None,
            whole_bytes,
            widths,
            held: HeldIn::new(
                crate::room::current().unwrap_or_else(|| Room::new(self.reader.room().sizes())),
            ),
            open_bytes: 0,
            nearest,
            near: Vec::new(),
        };
        for e in entries {
            assess(&mut walk, e, order, want);
        }
        walk.count_open();
        walk.top = (walk.low, walk.high);
        Ok(walk)
    }

    /// A strand of a column of the plane walked nearest first from `point`, begun at its top.
    pub(super) fn best_first(
        &self,
        oi: usize,
        want: &Want,
        column: usize,
        point: Point,
    ) -> Result<Best> {
        let walk = self.start_near(oi, want, Some((column, point)))?;
        let mut best = Best {
            walk: Some(walk),
            queue: BTreeMap::new(),
            seq: 0,
            before_plan: 0,
        };
        best.queue_open();
        Ok(best)
    }

    /// The entries under one above the blocks, read from the file and queued.
    fn best_down(&self, best: &mut Best, level: u32, o: Open, want: &Want) -> Result<()> {
        let w = best.walk.as_mut().expect("a walk under way");
        w.held.less(queued_bytes(&o));
        let ends = o
            .ends
            .as_ref()
            .expect("an entry above the blocks keeps its last key");
        let entries = decode_page(
            &self.reader.read_extent(o.child)?,
            &w.head,
            (&ends.last, ends.open),
        )?;
        w.directory_pages += o.child.pages();
        w.level = level - 1;
        let order = &want.table.orders[w.order];
        for e in entries {
            assess(w, e, order, want);
        }
        best.queue_open();
        Ok(())
    }

    /// The pages of the nearest blocks whose live rows come to `rows`, nothing read but the directory
    /// above them: the fewest a walk that stops at that many rows reads.
    pub(super) fn best_fewest(&self, best: &mut Best, want: &Want, rows: u64) -> Result<u64> {
        let (mut low, mut found) = (0u64, 0u64);
        let mut aside = Vec::new();
        while found < rows {
            let Some((k, o)) = best.queue.pop_first() else {
                break;
            };
            if k.1 > 1 {
                self.best_down(best, k.1, o, want)?;
                continue;
            }
            low += u64::from(o.pages);
            found += u64::from(o.rows);
            aside.push((k, o));
        }
        best.queue.extend(aside);
        Ok(low)
    }

    /// The walk's next step: the directory page under the nearest entry, read, which gives no plan; or
    /// the nearest block's plan, its pages known now.
    pub(super) fn best_step(&self, best: &mut Best, want: &Want) -> Result<Option<Plan>> {
        let Some((k, o)) = best.queue.pop_first() else {
            return Ok(None);
        };
        if k.1 > 1 {
            self.best_down(best, k.1, o, want)?;
            return Ok(None);
        }
        let mut w = best.walk.take().expect("a walk under way");
        w.held.less(queued_bytes(&o));
        best.before_plan = w.held.holds();
        w.level = 1;
        w.open.push(o);
        w.near.clear();
        self.predict(w, want).map(Some)
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
            let ends = o
                .ends
                .as_ref()
                .expect("an entry above the blocks keeps its last key");
            let entries = decode_page(
                &self.reader.read_extent(o.child)?,
                &walk.head,
                (&ends.last, ends.open),
            )?;
            walk.directory_pages += o.child.pages();
            for e in entries {
                assess(walk, e, order, want);
            }
        }
        walk.count_open();
        Ok(())
    }

    /// The column pages a round along an order walked down to its blocks reads, exactly: of a block
    /// read whole, the pages its entry lists; of one read in part, those its page map names. A snail
    /// open to write prunes by what each map says, and where pages of the order are held, which ones
    /// is known from the maps, so there every block's is read now.
    pub(super) fn predict(&self, mut walk: Walk, want: &Want) -> Result<Plan> {
        let table = want.side.table;
        let maps_now = self.writer.is_some()
            || self.reader.held_under(super::order_tag(table, walk.order)) > 0;
        let mut written: Vec<u64> = match maps_now {
            true => vec![0; walk.whole_bytes.len()],
            false => walk.whole_bytes.clone(),
        };
        let mut column_pages = 0u64;
        let mut held = 0u64;
        let mut rows_estimate = walk.rows_closed as f64;
        let open = std::mem::take(&mut walk.open);
        let mut reads = Vec::with_capacity(open.len());
        let mut maps = MapPagesHeld::default();
        walk.held
            .more((open.len() * std::mem::size_of::<BlockRead>()) as u64);
        for o in open {
            let by_keys = o.reading.interval_may || walk.keyed > 0;
            let rows = o.rows as f64 * o.share;
            if !by_keys && !maps_now {
                column_pages += u64::from(o.pages);
                rows_estimate += rows;
                reads.push(BlockRead {
                    reading: o.reading,
                    child: o.child,
                    rows: o.rows,
                    pages: o.pages,
                    partly: None,
                });
                continue;
            }
            let (run, at) = map_run_of(o.child);
            if maps.found(run.start, run.pages()).is_none() {
                let bytes = self.reader.read(run.start, run.pages())?;
                walk.directory_pages += run.pages();
                maps.take(run.start, bytes, &walk.held);
            }
            let map = PageMap::decode(
                &maps.found(run.start, run.pages()).unwrap()[at..],
                &walk.head.level_text,
            )?;
            let last = o.ends.as_ref().map_or(&[][..], |e| &e.last[..]);
            rows_estimate += rows * interval_share(&o.reading, &map, &walk, last);
            let mut block_reads = Vec::with_capacity(o.reading.slots.len());
            let (mut pages_of_block, mut waste) = (0u64, 0u64);
            for &s in &o.reading.slots {
                let (chosen, extra) =
                    leaf_pages(&map, s, o.reading.interval_may, &walk, want.keys, last);
                let pages = &map.columns[s].pages;
                // The column's waste: the pages read only for rows written after the sorted ones, and
                // the pages the dead rows among those read fill.
                let (mut rows, mut dead, mut disk) = (0u64, 0u64, 0u64);
                for &p in &chosen {
                    let (a, z) = (pages[p].first_row, pages[p].first_row + pages[p].rows);
                    rows += pages[p].rows as u64;
                    dead += (map.dead.partition_point(|&x| x < z)
                        - map.dead.partition_point(|&x| x < a)) as u64;
                    disk += pages[p].at.pages();
                    written[s] += pages[p].at.bytes;
                }
                let live = if rows == 0 {
                    0
                } else {
                    (disk * (rows - dead)).div_ceil(rows)
                };
                waste = waste.max(extra + disk - live);
                if !chosen.is_empty() {
                    pages_of_block += map.columns[s].dictionary.map_or(0, Extent::pages);
                    pages_of_block += disk;
                }
                block_reads.push((s, chosen));
            }
            if waste >= 1 && self.writer.is_some() {
                let ends = o.ends.as_ref().expect("a writer keeps a block's first key");
                self.wasted.borrow_mut().push(Wasted {
                    table,
                    order: walk.order,
                    first: ends.first.clone(),
                    child: o.child,
                });
            }
            if maps_now {
                held += self.reader.held_among(disk_pages(&map, &block_reads));
            }
            column_pages += pages_of_block;
            let partly = by_keys.then(|| {
                let last = o.ends.map_or_else(Vec::new, |e| e.last);
                Box::new(Partly { last, only: None })
            });
            let read = BlockRead {
                reading: o.reading,
                child: o.child,
                rows: o.rows,
                pages: pages_of_block as u32,
                partly,
            };
            walk.held
                .more(block_held(&read) - std::mem::size_of::<BlockRead>() as u64);
            reads.push(read);
        }
        walk.count_open();
        maps.last = None;
        let mut plan = Plan {
            walk,
            reads,
            column_pages,
            rows_estimate,
            maps: RefCell::new(maps),
            map_pages: 0,
            maps_read: Default::default(),
            held,
            held_read: Default::default(),
            written,
        };
        plan.count_map_pages();
        Ok(plan)
    }

    /// The plan kept to the live rows `picks` names, ascending, each counted from the first live row
    /// of the first block it reads: each block holding none of them left unread, and of every other,
    /// each column's pages that hold one.
    pub(super) fn keep_rows(&self, plan: &mut Plan, picks: &[u64], want: &Want) -> Result<()> {
        let mut before = 0u64;
        let mut at = 0;
        let mut kept = Vec::new();
        let mut column_pages = 0u64;
        let mut maps = std::mem::take(plan.maps.get_mut());
        for mut br in std::mem::take(&mut plan.reads) {
            let live = u64::from(br.rows);
            let mut mine = Vec::new();
            while at < picks.len() && picks[at] < before + live {
                mine.push(picks[at] - before);
                at += 1;
            }
            before += live;
            if mine.is_empty() {
                continue;
            }
            let (run, start) = map_run_of(br.child);
            if maps.found(run.start, run.pages()).is_none() {
                let bytes = self.reader.read(run.start, run.pages())?;
                plan.walk.directory_pages += run.pages();
                maps.take(run.start, bytes, &plan.walk.held);
            }
            let map = PageMap::decode(
                &maps.found(run.start, run.pages()).unwrap()[start..],
                &plan.walk.head.level_text,
            )?;
            // Each live ordinal as the row it is, past the dead rows before it.
            let mut rows = Vec::with_capacity(mine.len());
            let (mut d, mut seen) = (0, 0u64);
            for r in 0..map.rows() {
                if map.dead.get(d) == Some(&r) {
                    d += 1;
                    continue;
                }
                if rows.len() < mine.len() && mine[rows.len()] == seen {
                    rows.push(r);
                }
                seen += 1;
            }
            br.partly = Some(Box::new(Partly {
                last: br.partly.take().map_or_else(Vec::new, |p| p.last),
                only: Some(rows),
            }));
            let reads = pages_read_of(&map, &br, &plan.walk, want.keys);
            let pages = disk_pages(&map, &reads).count() as u64;
            br.pages = pages as u32;
            column_pages += pages;
            plan.walk
                .held
                .more(block_held(&br) - std::mem::size_of::<BlockRead>() as u64);
            kept.push(br);
        }
        maps.last = None;
        *plan.maps.get_mut() = maps;
        plan.reads = kept;
        plan.column_pages = column_pages;
        plan.count_map_pages();
        Ok(())
    }

    /// The plan of a whole table kept to the rows a sample draws from its blocks' live rows.
    pub(super) fn keep_drawn(
        &self,
        plan: &mut Plan,
        sample: &super::tablesample::Sample,
        want: &Want,
    ) -> Result<()> {
        let live: Vec<u64> = plan.reads.iter().map(|br| u64::from(br.rows)).collect();
        self.keep_rows(plan, &sample.picks(&live), want)
    }

    /// Of the pages a plan's read will read that are known now, those held the round keeps until the
    /// read ends: those of the blocks whose maps the plan holds, where any of the order's are held.
    pub(super) fn keep_held(&self, plan: &Plan, want: &Want) -> Result<()> {
        let maps = plan.maps.borrow();
        if maps.pages.is_empty()
            || self
                .reader
                .held_under(super::order_tag(want.side.table, plan.walk.order))
                == 0
                && self.writer.is_none()
        {
            return Ok(());
        }
        for br in &plan.reads {
            let (run, at) = map_run_of(br.child);
            if let Some(bytes) = maps.pages.get(&run.start) {
                if bytes.len() as u64 >= run.pages() * PAGE as u64 {
                    let map = PageMap::decode(&bytes[at..], &plan.walk.head.level_text)?;
                    let reads = pages_read_of(&map, br, &plan.walk, want.keys);
                    self.reader.keep_for_round(disk_pages(&map, &reads));
                }
            }
        }
        Ok(())
    }

    /// A block's page map as a read reaches it: from the pages the plan holds, or those last read, or
    /// read now, counted as directory pages and never under the read's tag.
    fn reached_map(&self, plan: &Plan, br: &BlockRead, tag: u32) -> Result<PageMap> {
        let (run, at) = map_run_of(br.child);
        let mut maps = plan.maps.borrow_mut();
        if maps.found(run.start, run.pages()).is_none() {
            let (file, held) = (self.reader.pages_read(), self.reader.pages_held());
            self.reader.tag_reads(None);
            let bytes = self.reader.read(run.start, run.pages());
            self.reader.tag_reads(Some(tag));
            let mut read = plan.maps_read.get();
            read.touched += run.pages();
            read.held += self.reader.pages_held() - held;
            read.file += self.reader.pages_read() - file;
            plan.maps_read.set(read);
            maps.last = Some((run.start, bytes?));
        }
        PageMap::decode(
            &maps.found(run.start, run.pages()).unwrap()[at..],
            &plan.walk.head.level_text,
        )
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

    /// A system column's values over the rows `from` to `to` of a block: the OID the file gave the
    /// table; or each row's ctid, the first page its block's page map names, a page the block alone
    /// owns as PostgreSQL's tid names a page of the heap, and the row's place in the block from 1.
    /// A page the transaction staged is numbered past every page the root names, in the order it
    /// was staged.
    fn system_values(
        &self,
        table: &Table,
        map: &crate::directory::PageMap,
        c: usize,
        from: usize,
        to: usize,
    ) -> Result<Values> {
        if c == crate::catalog::TABLEOID {
            let class = crate::oids::Object::Class(crate::oids::full(&table.name));
            let oid = self.catalog.oids.of(&class).unwrap_or(0);
            let key = crate::scalars::unsigned_key(u64::from(oid), 32);
            return Ok(Values::Int(vec![Some(key); to - from]));
        }
        let first = map
            .columns
            .iter()
            .flat_map(|cp| cp.dictionary.iter().chain(cp.pages.iter().map(|p| &p.at)))
            .map(|e| e.start)
            .min()
            .ok_or_else(|| Error::new(format!("ctid of {}: a block of no pages", table.name)))?;
        let first = match first.checked_sub(crate::file::STAGED) {
            Some(staged) => self.root.pages + staged,
            None => first,
        };
        let block = u32::try_from(first).map_err(|_| {
            Error::new(format!(
                "ctid of {}: its block's first page {first} lies past a tid's 2^32 blocks",
                table.name
            ))
        })?;
        if to > usize::from(u16::MAX) {
            return Err(Error::new(format!(
                "ctid of {}: a block of {to} rows, where a tid's place counts to 65,535",
                table.name
            )));
        }
        Ok(Values::Int(
            (from..to)
                .map(|r| Some(crate::scalars::tid_key(block, (r + 1) as u16)))
                .collect(),
        ))
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
        // The casings a condition by a pattern is held under.
        let casings = super::pipeline::Casings::of_table(table, &self.catalog.collations);
        // Each column's collator the round reads, where a library orders its text.
        let mut orderers: Vec<Coll> = vec![None; table.columns.len()];
        for c in columns
            .iter()
            .map(|&(c, _)| c)
            .chain(want.side.texts.iter().map(|t| t.column))
            .chain(order.levels.iter().map(|l| l.column()))
        {
            if crate::catalog::is_system(c) {
                continue;
            }
            orderers[c] = table.columns[c].orderer()?;
        }
        let descs = crate::write::leaf_descriptors(table, order)?;
        let tag = super::order_tag(want.side.table, plan.walk.order);
        for br in &plan.reads[blocks] {
            let map = self.reached_map(plan, br, tag)?;
            let reads = pages_read_of(&map, br, walk, want.keys);
            debug_assert_eq!(disk_pages(&map, &reads).count() as u32, br.pages);
            plan.held_read
                .set(plan.held_read.get() + self.reader.keep_for_round(disk_pages(&map, &reads)));
            // For every leaf read, its runs of pages decoded: (slot, [(first row, values)]).
            let mut held: Vec<(usize, Vec<(usize, Values)>)> = Vec::new();
            for (s, chosen) in &reads {
                let leaf = &leaves[*s].1;
                let cp = &map.columns[*s];
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
                            decode_leaf_with(
                                desc,
                                leaf,
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
            let mut rows: Vec<(usize, usize)> = vec![(0, map.rows() as usize)];
            let dead = &map.dead;
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
            // A sample keeps its own rows alone.
            if let Some(only) = br.only() {
                rows = only
                    .iter()
                    .map(|&r| r as usize)
                    .filter(|r| rows.iter().any(|&(a, z)| a <= *r && *r < z))
                    .map(|r| (r, r + 1))
                    .collect();
            }
            // The columns read whose values are a group of leaves.
            let mut groups: Vec<usize> = Vec::new();
            for (s, _) in &reads {
                let c = leaves[*s].0;
                if table.is_group(c) && !groups.contains(&c) {
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
                // The system columns asked over this range, made rather than read.
                let systems: Vec<(usize, Values)> = columns
                    .iter()
                    .filter(|&&(c, _)| crate::catalog::is_system(c))
                    .map(|&(c, _)| Ok((c, self.system_values(table, &map, c, from, to)?)))
                    .collect::<Result<_>>()?;
                // Each column read: the values that hold this range, and their first row.
                let column = |c: usize| -> (usize, &Values) {
                    if let Some((_, v)) = systems.iter().find(|s| s.0 == c) {
                        return (from, v);
                    }
                    match joined.iter().find(|j| j.0 == c) {
                        Some((_, v)) => (from, v),
                        None => run(order.leaf_slot(table, c).unwrap()),
                    }
                };
                let fixed = if br.reading.interval_may {
                    &order.levels[..walk.m]
                } else {
                    &[][..]
                };
                let levels: Vec<(Level, ColumnType, usize, &Values, Coll)> = fixed
                    .iter()
                    .map(|&l| {
                        let (start, v) = column(l.column());
                        (
                            l,
                            table.columns[l.column()].ty,
                            start,
                            v,
                            orderers[l.column()],
                        )
                    })
                    .collect();
                let rests: Vec<(&Cond, usize, &Values)> = br
                    .reading
                    .rest_may
                    .iter()
                    .map(|&ri| {
                        let c = &walk.rest[ri];
                        let (start, v) = column(s_column(c.shape));
                        (c, start, v)
                    })
                    .collect();
                let texts: Vec<(&TextCond, ColumnType, usize, &Values, Coll)> = br
                    .reading
                    .text_may
                    .iter()
                    .map(|&ti| {
                        let t = &want.side.texts[ti];
                        let (start, v) = column(t.column);
                        (t, table.columns[t.column].ty, start, v, orderers[t.column])
                    })
                    .collect();
                let nulls: Vec<(bool, usize, &Values)> = br
                    .reading
                    .null_may
                    .iter()
                    .map(|&ni| {
                        let (c, null) = want.side.nulls[ni];
                        let (start, v) = column(c);
                        (null, start, v)
                    })
                    .collect();
                let ops: Vec<(&OpCond, ColumnType, usize, &Values)> = br
                    .reading
                    .op_may
                    .iter()
                    .map(|&oi| {
                        let o = &want.side.ops[oi];
                        let (start, v) = column(o.column);
                        (o, table.columns[o.column].ty, start, v)
                    })
                    .collect();
                let beside: Vec<OpBeside> = ops
                    .iter()
                    .map(|(o, ..)| {
                        let mut names = vec![o.name.clone()];
                        names.extend(o.beside.iter().map(|b| b.1.clone()));
                        let others = o
                            .beside
                            .iter()
                            .map(|b| {
                                let (start, v) = column(b.0);
                                (table.columns[b.0].ty, start, v)
                            })
                            .collect();
                        (names, others)
                    })
                    .collect();
                let outs: Vec<(bool, bool, usize, &Values)> = columns
                    .iter()
                    .map(|&(c, month)| {
                        let (start, v) = column(c);
                        let ty = table.column_at(c).ty;
                        (ty.is_byte_array() || ty.is_bits(), month, start, v)
                    })
                    .collect();
                // The columns read whose values are compared by a key other than their bytes, each
                // its type: a bpchar's, without its trailing blanks; a jsonb's, an inet's, a
                // tsvector's or a tsquery's, its class's; a string's under a library collation, by
                // the collation's key.
                let keyed: Vec<Option<ColumnType>> = columns
                    .iter()
                    .map(|&(c, _)| {
                        let ty = table.column_at(c).ty;
                        matches!(ty, ColumnType::Bpchar(_) | ColumnType::Textual(_))
                            .then_some(ty)
                            .filter(|t| t.keyed_by_bytes() || matches!(t, ColumnType::Bpchar(_)))
                    })
                    .collect();
                let collated: Vec<(ColumnType, Coll)> = columns
                    .iter()
                    .map(|&(c, _)| match crate::catalog::is_system(c) {
                        true => (table.column_at(c).ty, None),
                        false => (table.columns[c].ty, orderers[c]),
                    })
                    .collect();
                // Which of them are numerics held as their text, each kept beside its class.
                let classed: Vec<bool> = columns
                    .iter()
                    .map(|&(c, _)| matches!(table.column_at(c).ty, ColumnType::NumericText(_)))
                    .collect();
                let dicts: Vec<Dict> = outs.iter().map(|o| o.3.dict().cloned()).collect();
                if !same(&sent, &dicts) {
                    sink.dictionaries(&dicts);
                    sent = dicts;
                }
                // How a row's fixed levels compare with a bound, level by level, a NULL lying on its
                // level's side.
                let against = |r: usize, bound: &[KeyValue]| -> Ordering {
                    for (li, ((_, ty, start, v, coll), b)) in levels.iter().zip(bound).enumerate() {
                        let null = KeyValue::Null {
                            first: order.nulls_first[li],
                        };
                        let ord = match (v.is_null(r - start), b) {
                            (true, _) => null.cmp(b),
                            (false, &KeyValue::Null { first }) => {
                                if first {
                                    Ordering::Greater
                                } else {
                                    Ordering::Less
                                }
                            }
                            (false, KeyValue::Text(t)) => {
                                v.text_value(r - start).map_or(Ordering::Less, |x| {
                                    crate::strings::column_key(*ty, *coll, x.data())
                                        .as_ref()
                                        .cmp(t.as_bytes())
                                })
                            }
                            (false, KeyValue::Bytes(t)) => {
                                v.text_value(r - start).map_or(Ordering::Less, |x| {
                                    crate::strings::column_key(*ty, *coll, x.data())
                                        .as_ref()
                                        .cmp(&**t)
                                })
                            }
                            (false, KeyValue::Int(x)) => v
                                .int(r - start)
                                .map_or(KeyValue::Int(i64::MIN), |y| level_key(order, li, y))
                                .cmp(&KeyValue::Int(*x)),
                            (false, KeyValue::Wide(x)) => {
                                v.wide(r - start).unwrap_or(i128::MIN).cmp(x)
                            }
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
                    Some(k) if walk.keyed > 0 && lead >= walk.keyed => keyed_spans(
                        k,
                        &outs[..walk.keyed],
                        &collated[..walk.keyed],
                        &order.nulls_first[..walk.keyed],
                        from,
                        to,
                        map.sorted as usize,
                    ),
                    _ => vec![(from, to)],
                };
                for (a, b) in spans {
                    let mut d = dead.partition_point(|&x| (x as usize) < a);
                    // A run every row of which is wanted goes to the sink at once, where it takes it.
                    if lead == 0
                        && !br.reading.interval_may
                        && rests.is_empty()
                        && texts.is_empty()
                        && nulls.is_empty()
                        && ops.is_empty()
                        && dead.get(d).is_none_or(|&x| x as usize >= b)
                        && collated.iter().all(|c| c.1.is_none())
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
                        if br.reading.interval_may
                            && (walk.short_of(against(r, &walk.lo))
                                || walk.over(against(r, &walk.hi)))
                        {
                            continue;
                        }
                        let keep = rests.iter().all(|&(c, start, v)| {
                            let x = match c.shape {
                                // An instant with no month is NULL to a condition on its month.
                                Shape::MonthUtc(_) => {
                                    v.int(r - start).and_then(month_of).map(i128::from)
                                }
                                Shape::Scaled(_, k) => v
                                    .int(r - start)
                                    .map(|x| i128::from(crate::places::scaled_key(x, k))),
                                _ => v.wide(r - start),
                            };
                            c.holds(x)
                        });
                        let keep =
                            keep && texts.iter().all(|&(t, ty, start, v, coll)| {
                                v.text_value(r - start).is_some_and(|x| {
                                    t.holds(&crate::strings::column_key(ty, coll, x.data()))
                                })
                            }) && nulls
                                .iter()
                                .all(|&(null, start, v)| v.is_null(r - start) == null);
                        if !keep {
                            continue;
                        }
                        // A condition by an operator or a pattern, held as a pipeline holds it; one
                        // it cannot hold fails the question. Every such condition but an OR's or a
                        // NOT's is strict: a NULL fails it.
                        let mut kept = true;
                        for (&(o, ty, start, v), (names, others)) in ops.iter().zip(&beside) {
                            if o.arms.is_none() && v.is_null(r - start) {
                                kept = false;
                                break;
                            }
                            let d = crate::values::datum_of(v, r - start, ty);
                            let mut values = vec![super::fill::value_of(&d, ty)];
                            for &(ty, start, v) in others {
                                let d = crate::values::datum_of(v, r - start, ty);
                                values.push(super::fill::value_of(&d, ty));
                            }
                            let held = super::pipeline::eval_on(
                                &o.expr,
                                names,
                                &values,
                                table.release,
                                &casings,
                            )?;
                            if !matches!(held, super::pipeline::Value::Bool(true)) {
                                kept = false;
                                break;
                            }
                        }
                        if !kept {
                            continue;
                        }
                        let make = |o: usize| {
                            let (is_text, month, start, v) = outs[o];
                            let i = r - start;
                            if let (ty, Some(coll)) = collated[o] {
                                v.text_value(i).map_or(Cell::Null, |t| {
                                    let key: Box<[u8]> =
                                        crate::strings::column_key(ty, Some(coll), t.data()).into();
                                    Cell::Keyed(crate::read::answer::Keyed::new(key, t))
                                })
                            } else if let Values::Composed(x) = v {
                                x[i].clone().map_or(Cell::Null, Cell::Composed)
                            } else if let Values::Dict { codes, .. } = v {
                                codes[i].map_or(Cell::Null, Cell::Code)
                            } else if let Some(ty) = keyed[o] {
                                v.text_value(i).map_or(Cell::Null, |t| {
                                    let key: Box<[u8]> =
                                        crate::strings::key_bytes(ty, t.data()).into();
                                    Cell::Keyed(crate::read::answer::Keyed::new(key, t))
                                })
                            } else if classed[o] {
                                v.text_value(i).map_or(Cell::Null, Cell::classed)
                            } else if is_text {
                                v.text_value(i).map_or(Cell::Null, |t| Cell::Text(Text(t)))
                            } else if month {
                                v.int(i).and_then(month_of).map_or(Cell::Null, Cell::Int)
                            } else if let Values::Interval(x) = v {
                                x[i].map_or(Cell::Null, |x| {
                                    Cell::Interval(crate::read::answer::Lapse(x))
                                })
                            } else if let Values::Wide(w) = v {
                                w[i].map_or(Cell::Null, Cell::Wide)
                            } else if let (Some(key), Some(bits)) = (v.int(i), v.bits(i)) {
                                Cell::Float(crate::read::answer::Float { key, bits })
                            } else {
                                v.int(i).map_or(Cell::Null, Cell::Int)
                            }
                        };
                        for (o, cell) in cells[..lead].iter_mut().enumerate() {
                            *cell = make(o);
                        }
                        if lead > 0 && !sink.admits(&cells[..lead]) {
                            continue;
                        }
                        for (o, cell) in cells.iter_mut().enumerate().skip(lead) {
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
    types: &[(ColumnType, Coll)],
    nulls_first: &[bool],
    from: usize,
    to: usize,
    sorted: usize,
) -> Vec<(usize, usize)> {
    // A row's leading values against a key, compared in place, a NULL lying on its level's side.
    let cmp = |r: usize, key: &[KeyValue]| -> Ordering {
        lead.iter()
            .zip(types)
            .zip(nulls_first)
            .zip(key)
            .map(|(((&(_, _, start, v), &(ty, coll)), &first), k)| {
                against_key(ty, coll, v, r - start, first, k)
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
            let find = |key: &[KeyValue], mut lo: usize, past: bool| -> usize {
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

/// Row `i`'s value of a level of type `ty` against a level's key: a number by its key, a value of a
/// string type, bytea, a bit string or a composed type by the bytes it is keyed by, under its
/// collation where a library orders it, a NULL lying on its level's side.
pub(super) fn against_key(
    ty: ColumnType,
    coll: Coll,
    v: &Values,
    i: usize,
    first: bool,
    key: &KeyValue,
) -> Ordering {
    if v.is_null(i) {
        return KeyValue::Null { first }.cmp(key);
    }
    match key {
        KeyValue::Null { first: true } => Ordering::Greater,
        KeyValue::Null { first: false } => Ordering::Less,
        KeyValue::Text(k) => v.text_value(i).map_or(Ordering::Less, |x| {
            crate::strings::column_key(ty, coll, x.data())
                .as_ref()
                .cmp(k.as_bytes())
        }),
        KeyValue::Bytes(k) => v.text_value(i).map_or(Ordering::Less, |x| {
            crate::strings::column_key(ty, coll, x.data())
                .as_ref()
                .cmp(&**k)
        }),
        KeyValue::Int(k) => v.wide(i).map_or(Ordering::Less, |x| x.cmp(&i128::from(*k))),
        KeyValue::Wide(k) => v.wide(i).map_or(Ordering::Less, |x| x.cmp(k)),
    }
}

/// An entry's bounds: its first and last keys, each narrowed by the least and greatest its leaves keep
/// of each leading level a number of one leaf keys, a month by its instants' within a year, a level's
/// NULLs on their side; and whether every key under it sorts before its last.
pub(crate) fn narrowed(
    e: &Entry,
    order: &Order,
    table: &Table,
) -> (Vec<KeyValue>, Vec<KeyValue>, bool) {
    let (mut lo, mut hi) = (Vec::new(), Vec::new());
    for (li, l) in order.levels.iter().enumerate() {
        let c = l.column();
        let ty = table.columns[c].ty;
        let Some(slot) = order.leaf_slot(table, c) else {
            break;
        };
        if table.leaf_count(c) != 1 || crate::write::bounded(table, c) {
            break;
        }
        let s = &e.columns[slot];
        let null = KeyValue::Null {
            first: order.nulls_first[li],
        };
        let (a, b) = if s.nulls == e.rows {
            (null.clone(), null.clone())
        } else {
            match l {
                Level::Column(_) if ty.is_wide() => (KeyValue::Wide(s.min), KeyValue::Wide(s.max)),
                Level::Column(_) => (KeyValue::Int(s.min as i64), KeyValue::Int(s.max as i64)),
                Level::Scaled(_, k) => (
                    KeyValue::Int(crate::places::scaled_key(s.min as i64, *k)),
                    KeyValue::Int(crate::places::scaled_key(s.max as i64, *k)),
                ),
                Level::MonthUtc(_) => match (month_of(s.min as i64), month_of(s.max as i64)) {
                    (Some(a), Some(b))
                        if crate::times::civil(s.min as i64).0
                            == crate::times::civil(s.max as i64).0 =>
                    {
                        (KeyValue::Int(a), KeyValue::Int(b))
                    }
                    _ => break,
                },
            }
        };
        let (a, b) = match (s.nulls > 0 && s.nulls < e.rows, order.nulls_first[li]) {
            (true, true) => (null, b),
            (true, false) => (a, null),
            _ => (a, b),
        };
        lo.push(a);
        hi.push(b);
    }
    let levels = order.levels.len();
    lo.resize(levels, KeyValue::Null { first: true });
    hi.resize(levels, KeyValue::Null { first: false });
    let first = e.first.clone().max(lo);
    if hi < e.last {
        (first, hi, false)
    } else {
        (first, e.last.clone(), e.open)
    }
}

/// Whether an entry's bounds on the first `levels` of its order are keys it holds, so that a block
/// they meet holds a page they reach: one level at most, a number of one leaf, a month by its
/// instants' within a year.
fn leads_by_its_own(e: &Entry, order: &Order, table: &Table, levels: usize) -> bool {
    if levels > 1 {
        return false;
    }
    let Some(&l) = order.levels.first() else {
        return true;
    };
    let c = l.column();
    let Some(slot) = order.leaf_slot(table, c) else {
        return false;
    };
    if table.leaf_count(c) != 1 || crate::write::bounded(table, c) {
        return false;
    }
    let s = &e.columns[slot];
    match l {
        Level::MonthUtc(_) => {
            crate::times::civil(s.min as i64).0 == crate::times::civil(s.max as i64).0
        }
        _ => true,
    }
}

/// Whether a key the question holds belongs with an entry, its gaps counted: within its first and the
/// last the next entry's separator leaves it, whatever its leaves' least and greatest rule out.
fn belongs(walk: &Walk, e: &Entry, want: &Want) -> bool {
    if want.side.empty {
        return false;
    }
    let m = walk.m;
    if m > 0 {
        let (first, last) = (&e.first[..m], &e.last[..m]);
        let short = if e.open && crate::directory::kept_len(&e.last) <= m {
            last <= walk.lo.as_slice()
        } else {
            walk.before(last)
        };
        if short || walk.past(first) {
            return false;
        }
    }
    let j = walk.keyed;
    match want.keys {
        Some(k) if j > 0 => {
            let open = e.open && crate::directory::kept_len(&e.last) <= j;
            k.meets(&e.first[..j], &e.last[..j], open)
        }
        _ => true,
    }
}

/// One entry on the walk's level, marked where the walk marks: a block a key the question holds
/// belongs with; above the blocks, an entry it does not go down under marks the order whole.
fn assess(walk: &mut Walk, e: Entry, order: &Order, want: &Want) {
    if !walk.marking {
        return assess_entry(walk, e, order, want);
    }
    let first = belongs(walk, &e, want).then(|| e.first.clone());
    let open = walk.open.len();
    assess_entry(walk, e, order, want);
    if let Some(first) = first {
        if walk.level == 1 {
            walk.marks.push(first);
        } else if walk.open.len() == open {
            walk.upper = true;
        }
    }
}

/// What a question makes of one entry on the walk's level: none of its rows, so it is skipped; all of
/// a count, taken from its counts; or rows the round reads under it, left open with the columns it
/// reads. An open entry adds its columns' pages to the walk's bounds. Above the blocks the fewest are
/// those pages only where every row under the entry is read whole, since a block under it may hold none.
fn assess_entry(walk: &mut Walk, e: Entry, order: &Order, want: &Want) {
    let m = walk.m;
    let side = want.side;
    let bounds = narrowed(&e, order, want.table);
    let mut interval = if side.empty {
        Status::None
    } else if m == 0 {
        Status::Must
    } else {
        let (pf, pl) = (&bounds.0[..m], &bounds.1[..m]);
        // Every key under an entry whose last is the separator above it sorts before that.
        let short = if bounds.2 && crate::directory::kept_len(&bounds.1) <= m {
            pl <= walk.lo.as_slice()
        } else {
            walk.before(pl)
        };
        if short || walk.past(pf) || outside_bounds(walk, &e, order, want.table) {
            Status::None
        } else if !walk.before(pf) && !walk.past(pl) {
            Status::Must
        } else {
            Status::May
        }
    };
    let j = walk.keyed;
    let keys_may = match want.keys {
        Some(k) if j > 0 => {
            let open = bounds.2 && crate::directory::kept_len(&bounds.1) <= j;
            if !k.meets(&bounds.0[..j], &bounds.1[..j], open) {
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
        let slot = order.leaf_slot(table, t.column).unwrap();
        let sum = &e.columns[slot];
        // Where the entry keeps the column's least and greatest, a value outside what it keeps of them
        // is none of the entry's.
        let outside = walk
            .head
            .bound_slots
            .iter()
            .position(|&b| b as usize == slot)
            .is_some_and(|k| !within_bounds(t, table.columns[t.column].ty, sum, e.tail(k)));
        if sum.nulls == e.rows || outside {
            status = Status::None;
        } else {
            text_may.push(ti);
        }
    }
    // A condition by an operator rules the entry out where the column holds no value there, or where a
    // summary the entry keeps of it says no value there can hold it: its span, kept on a GiST or SP-GiST
    // strand or for a BRIN inclusion, or a box's leaves.
    let mut op_may = Vec::new();
    for (oi, o) in side.ops.iter().enumerate() {
        let slot = order.leaf_slot(table, o.column).unwrap();
        let span = walk
            .head
            .span_slots
            .iter()
            .position(|&s| s as usize == slot)
            .and_then(|k| e.spans.get(k));
        let inet = crate::spans::is_inet(table.columns[o.column].ty);
        let ruled_out = o.arms.is_none()
            && (e.columns[slot].nulls == e.rows
                || o.probe.as_ref().is_some_and(|probe| {
                    span.is_some_and(|span| !crate::spans::may_hold(&o.op, span, probe, inet))
                        || !crate::spans::box_may_hold(&o.op, probe, &e.columns[slot..])
                }));
        if ruled_out {
            status = Status::None;
        } else {
            op_may.push(oi);
        }
    }
    // A BRIN summary rules the entry out where no value it sums can hold a condition on its key.
    if super::summed::rules_out(&walk.summed, &e.summed, side) {
        status = Status::None;
    }
    let may = rest_may.len() + null_may.len();
    if status != Status::None && (may > 0 || !text_may.is_empty() || !op_may.is_empty() || keys_may)
    {
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
    if interval == Status::Must && text_may.is_empty() && op_may.is_empty() && !keys_may {
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
                // Every row by its month only where no instant is infinite, whose month is NULL.
                let ends = |mi: usize| e.ends[mi].iter().map(|&n| n as u64).sum::<u64>();
                let months = months
                    .filter(|&(a, z)| (a, z) != (1, 12) || mi.is_some_and(|mi| ends(mi) == 0));
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
    for &c in want.columns.iter().filter(|&&c| !crate::catalog::is_system(c)) {
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
    for &oi in &op_may {
        need(side.ops[oi].column);
        for b in &side.ops[oi].beside {
            need(b.0);
        }
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
    let read_whole = interval == Status::Must
        && may == 0
        && text_may.is_empty()
        && op_may.is_empty()
        && !keys_may;
    walk.low += if walk.level > 1 {
        if read_whole {
            whole
        } else {
            0
        }
    } else if interval == Status::Must && j == 0 {
        whole
    } else if !leads_by_its_own(&e, order, want.table, m.max(j)) {
        0
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
        match (
            interval,
            text_may.is_empty() && op_may.is_empty(),
            keys_may,
            may,
            exact,
        ) {
            (Status::Must, true, false, 1, Some(n)) => {
                walk.rows_low += n;
                walk.rows_high += n;
            }
            _ => walk.rows_high += rows,
        }
    }
    let reading = Reading {
        interval_may: interval == Status::May,
        rest_may,
        text_may,
        null_may,
        op_may,
        slots,
    };
    let reading = match &walk.reading {
        Some(r) if **r == reading => r.clone(),
        _ => {
            let r = Rc::new(reading);
            walk.reading = Some(r.clone());
            r
        }
    };
    let at_blocks = walk.level == 1;
    let by_keys = reading.interval_may || j > 0;
    let (pages, share) = if at_blocks {
        if !by_keys {
            for &s in &reading.slots {
                let (sum, text) = (
                    &e.columns[s],
                    walk.head.text_slots.iter().position(|&t| t as usize == s),
                );
                let values = u64::from(e.rows.saturating_sub(sum.nulls));
                walk.whole_bytes[s] += match (text, walk.widths[s]) {
                    (Some(t), _) => u64::from(e.bytes[t]) + 4 * values,
                    (None, Some(w)) => w * values,
                    (None, None) => u64::from(sum.pages) * PAGE as u64,
                };
            }
        }
        (whole as u32, off_level_share(&reading, &e, walk, want))
    } else {
        (0, 1.0)
    };
    let ends = (!at_blocks || by_keys || walk.writing).then(|| {
        Box::new(Ends {
            first: if at_blocks && walk.writing {
                e.first.clone()
            } else {
                Vec::new()
            },
            last: if !at_blocks || by_keys {
                e.last.clone()
            } else {
                Vec::new()
            },
            open: e.open,
        })
    });
    if let Some(n) = &walk.nearest {
        let near = e.spans.get(n.span_at).map_or(Near::of(0.0), |span| {
            crate::plane::least_near(n.kind, span, &n.point)
        });
        walk.near.push(near);
    }
    walk.open.push(Open {
        reading,
        child: e.child,
        rows: e.rows,
        pages,
        share,
        ends,
    });
}

/// What an entry keeps of the column a condition reads: its one leaf's summary, or the one its
/// leaves' summaries make; and the slot of its first leaf.
/// A composed value's key, its bytes written as hex, as far as an entry's bounds keep it; any other
/// value's bytes as they are.
fn bound_bytes(v: &[u8], ty: ColumnType) -> std::borrow::Cow<'_, [u8]> {
    if ty.is_nested() {
        let d = |c: u8| (c as char).to_digit(16).unwrap_or(0) as u8;
        v.as_chunks::<2>()
            .0
            .iter()
            .take(64)
            .map(|h| d(h[0]) << 4 | d(h[1]))
            .collect()
    } else {
        v.into()
    }
}

/// Whether the run the walk's leading levels fix lies outside what an entry keeps of its least and
/// greatest of a level keyed by its bytes: at the first level, or one whose levels before it the run
/// fixes at one value.
fn outside_bounds(walk: &Walk, e: &Entry, order: &Order, table: &Table) -> bool {
    let bytes = |v: &KeyValue| match v {
        KeyValue::Text(s) => Some(s.as_bytes().to_vec()),
        KeyValue::Bytes(b) => Some(b.to_vec()),
        _ => None,
    };
    for li in 0..walk.m.min(walk.lo.len()).min(walk.hi.len()) {
        if walk.lo[..li] != walk.hi[..li] {
            break;
        }
        let c = order.levels[li].column();
        let Some(slot) = order.leaf_slot(table, c) else {
            break;
        };
        let Some(k) = walk
            .head
            .bound_slots
            .iter()
            .position(|&b| b as usize == slot)
        else {
            continue;
        };
        if e.columns[slot].nulls > 0 {
            continue;
        }
        let (sum, ty, tail) = (&e.columns[slot], table.columns[c].ty, e.tail(k));
        if bytes(&walk.lo[li]).is_some_and(|v| past_greatest(sum, tail, &bound_bytes(&v, ty)))
            || bytes(&walk.hi[li]).is_some_and(|v| below_least(sum, tail, &bound_bytes(&v, ty)))
        {
            return true;
        }
    }
    false
}

/// Whether a value `t` keeps may lie within what an entry keeps of its column's least and greatest; a
/// composed value's bounds in `t` written as hex.
fn within_bounds(t: &TextCond, ty: ColumnType, sum: &Summary, tail: &Tail) -> bool {
    let above = match &t.lo {
        Bound::Included(l) | Bound::Excluded(l) => !past_greatest(sum, tail, &bound_bytes(l, ty)),
        Bound::Unbounded => true,
    };
    let below = match &t.hi {
        Bound::Included(h) | Bound::Excluded(h) => !below_least(sum, tail, &bound_bytes(h, ty)),
        Bound::Unbounded => true,
    };
    above && below
}

fn summary_of(table: &Table, order: &Order, c: &Cond, e: &Entry) -> (Summary, usize) {
    let column = s_column(c.shape);
    let slots = order
        .slots(table, column)
        .expect("an order holds the columns read along it");
    let leaves = &e.columns[slots.clone()];
    if let [one] = leaves {
        // A count of a double precision's places, from its least and greatest.
        if let Shape::Scaled(_, k) = c.shape {
            let scaled = |x: i128| i128::from(crate::places::scaled_key(x as i64, k));
            let sum = Summary {
                min: scaled(one.min),
                max: scaled(one.max),
                ..*one
            };
            return (sum, slots.start);
        }
        return (*one, slots.start);
    }
    // A group's least and greatest: its leaves' least, and its leaves' greatest, each made into
    // the column's key as its leaves' values are.
    let ty = table.columns[column].ty;
    let key = |pick: fn(&Summary) -> i128| {
        let parts = leaves
            .iter()
            .map(|s| {
                if s.wide {
                    Values::Wide(vec![Some(pick(s))])
                } else {
                    Values::Int(vec![Some(pick(s) as i64)])
                }
            })
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
        // An infinite instant's month is NULL too.
        let nulls = nulls
            + match (c.nulls, c.shape) {
                (true, Shape::MonthUtc(_)) => head
                    .month_slots
                    .iter()
                    .position(|&x| x as usize == slot)
                    .map_or(0, |mi| b.ends[mi].iter().map(|&n| n as u64).sum()),
                _ => 0,
            };
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
            _ if c.gap_holds(sum.min, sum.max) => Some(0),
            _ if iv.lo <= sum.min
                && sum.max <= iv.hi
                && !c.except.iter().any(|&x| sum.min <= x && x <= sum.max)
                && c.clear_of_gaps(sum.min, sum.max) =>
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

/// The share of an open block's rows the leading levels' range covers, page by page, each page's
/// rows taken as lying evenly across its keys.
fn interval_share(reading: &Reading, map: &PageMap, walk: &Walk, block_last: &[KeyValue]) -> f64 {
    if !reading.interval_may {
        return 1.0;
    }
    let m = walk.m;
    let finest = reading
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
                (&block_last[..m], true)
            };
            all += rows;
            covered += rows * page_share(&pages[p].first[..m], next, last, &walk.lo, &walk.hi);
        }
        if all > 0.0 {
            return covered / all;
        }
    }
    1.0
}

/// The share of an open block's rows the conditions off the levels cover: each by the share of its
/// least to greatest it takes, or by its count where the counts give one, and a test for NULL by the
/// NULLs counted; a condition on text off the levels leaves the block whole.
fn off_level_share(reading: &Reading, e: &Entry, walk: &Walk, want: &Want) -> f64 {
    let table = want.table;
    let order = &table.orders[walk.order];
    let mut share = 1.0;
    for &ri in &reading.rest_may {
        let c = &walk.rest[ri];
        let (sum, slot) = summary_of(table, order, c, e);
        share *= rest_share(sum, slot, c, e, &walk.head);
    }
    for &ni in &reading.null_may {
        let (c, null) = want.side.nulls[ni];
        let n = e.columns[order.leaf_slot(table, c).unwrap()].nulls as f64;
        share *= if null { n } else { e.rows as f64 - n } / (e.rows as f64).max(1.0);
    }
    share
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
            open: false,
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
            ends: vec![[0, 0]],
            bytes: vec![],
            spans: Vec::new(),
            summed: Vec::new(),
            tails: Vec::new(),
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
            open: false,
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
            ends: vec![],
            bytes: vec![],
            spans: Vec::new(),
            summed: Vec::new(),
            tails: Vec::new(),
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
            open: false,
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
            ends: vec![],
            bytes: vec![],
            spans: Vec::new(),
            summed: Vec::new(),
            tails: Vec::new(),
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
            [ints([3, 1]), ints([7, 2]), ints([3, 9]), ints([3, 1])],
        );
        assert!(keys.meets(&ints([3, 0]), &ints([3, 1]), false));
        assert!(keys.meets(&ints([3, 1]), &ints([3, 1]), false));
        assert!(keys.meets(&ints([3, 2]), &ints([5, 0]), false));
        assert!(!keys.meets(&ints([3, 2]), &ints([3, 8]), false));
        assert!(!keys.meets(&ints([7, 3]), &ints([9, 9]), false));
    }
}
