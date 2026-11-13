//! An order's blocks cut as its rows come in its order, each handed on as it closes: the cut `cut`
//! makes of every row at once, made holding only the rows no closed block holds yet.

use crate::cut::{PAGE, ROW_GROUP_ROWS};
use crate::places::{KeyValue, Sublevels};
use crate::values::Values;
use std::collections::VecDeque;

/// A row's plain bytes on one leaf, or its bits on a leaf of bits, its top bit set where the row is
/// NULL there.
pub(crate) const NULL_BIT: u64 = 1 << 63;

/// A block closed: its first row's place in the order, its depth, and its rows, their keys and
/// each row's width on each leaf.
pub(crate) struct Closed {
    pub start: usize,
    pub end: usize,
    pub depth: usize,
    pub columns: Vec<Values>,
    pub keys: Vec<KeyValue>,
    pub widths: Vec<u64>,
}

impl Closed {
    pub fn rows(&self) -> usize {
        self.end - self.start
    }
}

/// A block not yet closed: its rows, and its depth.
struct Open {
    id: u64,
    start: usize,
    end: usize,
    depth: usize,
}

impl Open {
    fn rows(&self) -> usize {
        self.end - self.start
    }
}

/// Rows before a part that fills, joined to the first block that part makes, or a block of their
/// own before it, once that block's rows are known.
struct After {
    from: usize,
    to: usize,
    block: Option<u64>,
}

/// A place the cut has entered: where it starts, the part of it the newest row lies on, and either
/// the parts before that one, none of which fills, while no part of it has filled; or, once one has,
/// where its rows not yet in a block begin and the blocks it has made.
struct Frame {
    depth: usize,
    start: usize,
    part: usize,
    whole: Option<Vec<(usize, usize)>>,
    first: u64,
    thin: Option<usize>,
    after: Option<After>,
}

/// The cut of an order's rows as they come.
pub(crate) struct Closing<'a> {
    sub: &'a Sublevels,
    group: usize,
    stride: usize,
    bits: Vec<bool>,
    /// Each level's sublevels, from its first.
    levels: Vec<(usize, usize)>,
    /// The rows from `held` on, column by column, their keys and widths, and the widths summed up to
    /// each of them and one past.
    held: usize,
    columns: Vec<Values>,
    keys: Vec<KeyValue>,
    widths: Vec<u64>,
    summed: Vec<u64>,
    /// Rows from `held` up to `kept` lie in blocks already handed on.
    kept: usize,
    rows: usize,
    frames: Vec<Frame>,
    open: VecDeque<Open>,
    ids: u64,
    pushed: u64,
    closed: VecDeque<Closed>,
}

impl<'a> Closing<'a> {
    /// The cut on `sub`, each leaf of bits where `bits` says.
    pub fn new(sub: &'a Sublevels, stride: usize, bits: Vec<bool>) -> Closing<'a> {
        Closing::within(sub, stride, bits, ROW_GROUP_ROWS)
    }

    /// The same, a row group `group` rows.
    pub fn within(sub: &'a Sublevels, stride: usize, bits: Vec<bool>, group: usize) -> Closing<'a> {
        let leaves = bits.len();
        Closing {
            sub,
            group,
            stride,
            bits,
            levels: sub.level_rungs(),
            held: 0,
            columns: Vec::new(),
            keys: Vec::new(),
            widths: Vec::new(),
            summed: vec![0; 2 * leaves],
            kept: 0,
            rows: 0,
            frames: Vec::new(),
            open: VecDeque::new(),
            ids: 0,
            pushed: 0,
            closed: VecDeque::new(),
        }
    }

    /// The blocks closed since last asked, in their order.
    pub fn take(&mut self) -> Option<Closed> {
        self.closed.pop_front()
    }

    /// Row `i` of `from` comes next, its key and its width on each leaf given.
    pub fn push(&mut self, from: &[Values], i: usize, key: &[KeyValue], widths: &[u64]) {
        if self.columns.is_empty() {
            self.columns = from.iter().map(Values::new_like).collect();
        }
        let n = self.rows;
        if n == 0 {
            self.frames.push(Frame::whole(0, 0));
        } else {
            let deepest = self.frames.last().expect("a place holds the rows").depth;
            let limit = (deepest + 1).min(self.sub.len());
            let r = self.parts_at(n - 1, key, limit);
            if r < limit {
                self.part_closes();
                while self.frames.last().expect("a place").depth > r {
                    self.frame_closes();
                }
                self.frames.last_mut().expect("a place").part = n;
            }
        }
        for (c, v) in self.columns.iter_mut().zip(from) {
            c.push_from(v, i);
        }
        self.keys.extend_from_slice(key);
        self.widths.extend_from_slice(widths);
        let at = self.summed.len() - 2 * self.bits.len();
        for (s, &w) in widths.iter().enumerate() {
            let (bytes, nulls) = (self.summed[at + 2 * s], self.summed[at + 2 * s + 1]);
            self.summed.push(bytes + (w & !NULL_BIT));
            self.summed.push(nulls + u64::from(w & NULL_BIT != 0));
        }
        self.rows += 1;
        self.descend();
        self.settle();
    }

    /// Every row has come: the blocks still open close.
    pub fn finish(&mut self) {
        if self.rows == 0 {
            return;
        }
        self.part_closes();
        while !self.frames.is_empty() {
            self.frame_closes();
        }
        self.settle();
        while let Some(b) = self.open.pop_front() {
            self.hand_on(b);
        }
    }

    /// The first sublevel below `limit` row `prev` and `key` part on, or `limit`.
    fn parts_at(&self, prev: usize, key: &[KeyValue], limit: usize) -> usize {
        let was = self.key(prev);
        for (li, &(from, to)) in self.levels.iter().enumerate() {
            if from >= limit {
                return limit;
            }
            if was[li] == key[li] {
                continue;
            }
            for r in from..to.min(limit) {
                if self.sub.value(was, r) != self.sub.value(key, r) {
                    return r;
                }
            }
        }
        limit
    }

    fn key(&self, row: usize) -> &[KeyValue] {
        let at = (row - self.held) * self.stride;
        &self.keys[at..at + self.stride]
    }

    fn summed_at(&self, row: usize, s: usize) -> (u64, u64) {
        let at = (row - self.held) * 2 * self.bits.len() + 2 * s;
        (self.summed[at], self.summed[at + 1])
    }

    /// Whether rows `a..b` fill a page of every leaf that decides them, and of one leaf at least, as
    /// `Widths::fill` reads it.
    fn fill(&self, a: usize, b: usize) -> bool {
        let mut some = false;
        for (s, &bits) in self.bits.iter().enumerate() {
            let (ab, an) = self.summed_at(a, s);
            let (bb, bn) = self.summed_at(b, s);
            let bytes = if bits { (bb - ab).div_ceil(8) } else { bb - ab };
            let full = bytes >= PAGE as u64;
            some |= full;
            if !bits && bn == an && !full {
                return false;
            }
        }
        some
    }

    /// The newest part of the deepest place closes at the newest row.
    fn part_closes(&mut self) {
        let n = self.rows;
        let leaves = self.sub.len();
        let f = self.frames.last_mut().expect("a place");
        if f.depth == leaves {
            return;
        }
        let part = f.part;
        if let Some(parts) = &mut f.whole {
            parts.push((part, n));
            return;
        }
        self.thin_part(part, n);
    }

    /// The deepest place closes at the newest row, its newest part closed.
    fn frame_closes(&mut self) {
        let n = self.rows;
        let f = self.frames.pop().expect("a place");
        if f.whole.is_some() {
            self.push_block(f.depth, f.start, n);
        } else if let Some(t) = f.thin {
            if !self.join_before(&f, t, n) {
                self.push_block(f.depth, t, n);
            }
        }
        if let Some(p) = self.frames.len().checked_sub(1) {
            if let Some(after) = self.frames[p].after.take() {
                self.resolve(p, after);
            }
        }
        self.settle();
    }

    /// The part of the deepest place from `a` to `b` fills no page: it joins the rows before it that
    /// no block holds yet, and those that fill a page together are a block.
    fn thin_part(&mut self, a: usize, b: usize) {
        let f = self.frames.last_mut().expect("a place");
        let t = *f.thin.get_or_insert(a);
        if self.fill(t, b) {
            let depth = self.frames.last().expect("a place").depth;
            self.push_block(depth, t, b);
            self.frames.last_mut().expect("a place").thin = None;
        }
    }

    /// Joins the rows `t..a` of the deepest place `f` to the block before them, where it lies in
    /// the place and stays within a row group.
    fn join_before(&mut self, f: &Frame, t: usize, a: usize) -> bool {
        if self.pushed <= f.first {
            return false;
        }
        let group = self.group;
        let last = self.open.back_mut().expect("a block was pushed");
        if last.rows() + (a - t) <= group {
            last.end = a;
            last.depth = f.depth;
            true
        } else {
            false
        }
    }

    /// The deepest place's parts so far, none of which fills, as it splits: those that fill a page
    /// together are blocks.
    fn split(&mut self) {
        let pushed = self.pushed;
        let f = self.frames.last_mut().expect("a place");
        let parts = f.whole.take().expect("a place not yet split");
        f.first = pushed;
        f.thin = None;
        f.after = None;
        for (a, b) in parts {
            self.thin_part(a, b);
        }
    }

    /// The newest part of the deepest place, split, fills: the place goes on down into it.
    fn enter_part(&mut self) {
        let f = self.frames.last_mut().expect("a place");
        let a = f.part;
        let depth = f.depth + 1;
        if let Some(t) = f.thin.take() {
            let f = self.frames.pop().expect("a place");
            let joined = self.join_before(&f, t, a);
            self.frames.push(f);
            if !joined {
                self.frames.last_mut().expect("a place").after = Some(After {
                    from: t,
                    to: a,
                    block: None,
                });
            }
        }
        let mut frame = Frame::whole(depth, a);
        if depth < self.sub.len() {
            let n = self.rows;
            let value = |i: usize| self.sub.value(self.key(i), depth);
            let last = value(n - 1);
            if value(a) != last {
                let parts = frame.whole.as_mut().expect("a place not yet split");
                let mut from = a;
                let mut v = value(a);
                for i in a + 1..n {
                    let w = value(i);
                    if w != v {
                        parts.push((from, i));
                        from = i;
                        v = w;
                    }
                }
                frame.part = from;
            }
        }
        self.frames.push(frame);
    }

    /// Goes on down while the deepest place's newest part fills, or splits a place past a row group.
    fn descend(&mut self) {
        let leaves = self.sub.len();
        loop {
            let f = self.frames.last().expect("a place");
            if f.depth == leaves {
                return;
            }
            let fills = self.fill(f.part, self.rows);
            let whole = f.whole.is_some();
            let past = self.rows - f.start > self.group;
            match (whole, fills) {
                (true, true) => {
                    self.split();
                    self.enter_part();
                }
                (true, false) if past => self.split(),
                (false, true) => self.enter_part(),
                _ => return,
            }
        }
    }

    fn push_block(&mut self, depth: usize, start: usize, end: usize) {
        let id = self.ids;
        self.ids += 1;
        self.pushed += 1;
        self.open.push_back(Open {
            id,
            start,
            end,
            depth,
        });
        for f in &mut self.frames {
            if let Some(a) = &mut f.after {
                a.block.get_or_insert(id);
            }
        }
    }

    /// The rows before a part that filled, joined to the first block it made, or a block of their
    /// own before it.
    fn resolve(&mut self, frame: usize, after: After) {
        let target = after.block.expect("a part that fills makes a block");
        let at = self
            .open
            .iter()
            .rposition(|b| b.id == target)
            .expect("the block is open");
        let depth = self.frames[frame].depth;
        if self.open[at].rows() + (after.to - after.from) <= self.group {
            self.open[at].start = after.from;
            self.open[at].depth = depth;
        } else {
            let id = self.ids;
            self.ids += 1;
            self.open.insert(
                at,
                Open {
                    id,
                    start: after.from,
                    end: after.to,
                    depth,
                },
            );
            for f in &mut self.frames {
                if let Some(a) = &mut f.after {
                    if a.block == Some(target) {
                        a.block = Some(id);
                    }
                }
            }
        }
    }

    /// Settles each part's rows whose first block is no longer the newest, the deepest first, and
    /// hands on every block but the newest.
    fn settle(&mut self) {
        let newest = self.open.back().map(|b| b.id);
        for f in (0..self.frames.len()).rev() {
            let ready = matches!(
                &self.frames[f].after,
                Some(After { block: Some(b), .. }) if Some(*b) != newest
            );
            if ready {
                let after = self.frames[f].after.take().expect("rows before a part");
                self.resolve(f, after);
            }
        }
        while self.open.len() > 1 {
            let b = self.open.pop_front().expect("a block");
            self.hand_on(b);
        }
    }

    /// A closed block's rows taken from those held.
    fn hand_on(&mut self, b: Open) {
        debug_assert_eq!(b.start, self.kept);
        let (from, to) = (b.start - self.held, b.end - self.held);
        let rows: Vec<usize> = (from..to).collect();
        let leaves = self.bits.len();
        self.closed.push_back(Closed {
            start: b.start,
            end: b.end,
            depth: b.depth,
            columns: self.columns.iter().map(|c| c.gather(&rows)).collect(),
            keys: self.keys[from * self.stride..to * self.stride].to_vec(),
            widths: self.widths[from * leaves..to * leaves].to_vec(),
        });
        self.kept = b.end;
        // The rows handed on are let go once they are half of those held.
        let gone = self.kept - self.held;
        if gone > 0 && 2 * gone >= self.rows - self.held {
            let rest: Vec<usize> = (gone..self.rows - self.held).collect();
            for c in &mut self.columns {
                *c = c.gather(&rest);
            }
            self.keys.drain(..gone * self.stride);
            self.widths.drain(..gone * leaves);
            self.summed.drain(..gone * 2 * leaves);
            self.held = self.kept;
        }
    }
}

impl Frame {
    fn whole(depth: usize, start: usize) -> Frame {
        Frame {
            depth,
            start,
            part: start,
            whole: Some(Vec::new()),
            first: 0,
            thin: None,
            after: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Catalog;
    use crate::cut::{cut_within, Width, Widths};
    use crate::places::{ints, Keys};

    /// A seeded stream of numbers.
    struct Seeded(u64);

    impl Seeded {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    /// The blocks `cut_within` makes of the rows, and those `Closing` closes as they come.
    fn both(
        ddl: &str,
        keys: Vec<KeyValue>,
        widths: &[Vec<u64>],
        bits: &[bool],
        group: usize,
    ) -> (Vec<(usize, usize, usize)>, Vec<(usize, usize, usize)>) {
        let c = Catalog::parse(ddl).unwrap();
        let t = &c.tables[0];
        let order = &t.orders[0];
        let stride = order.levels.len();
        let mut text = crate::write::level_kinds(t, order, crate::file::FORMAT);
        for r in 0..keys.len() / stride {
            for li in 0..stride {
                if let KeyValue::Text(s) = &keys[r * stride + li] {
                    let ty = t.columns[order.levels[li].column()].ty;
                    text[li] = crate::places::rungs_for(ty, s.as_bytes(), text[li]);
                }
            }
        }
        let sub = Sublevels::new(order, t, &text);
        let n = keys.len() / stride;
        let leaf = |s: usize| -> Width {
            let mut sum = vec![0u64];
            let mut nulls = vec![0u64];
            for w in &widths[s] {
                sum.push(sum.last().unwrap() + (w & !NULL_BIT));
                nulls.push(nulls.last().unwrap() + u64::from(w & NULL_BIT != 0));
            }
            if bits[s] {
                Width::Bits(sum)
            } else {
                Width::Nulls { bytes: sum, nulls }
            }
        };
        let all = Widths((0..bits.len()).map(leaf).collect());
        let k = Keys {
            stride,
            data: keys.clone(),
        };
        let whole: Vec<(usize, usize, usize)> = cut_within(&k, &sub, &all, group)
            .iter()
            .map(|b| (b.start, b.end, b.place.len()))
            .collect();
        let mut closing = Closing::within(&sub, stride, bits.to_vec(), group);
        let column = vec![Values::Int((0..n as i64).map(Some).collect())];
        let mut out = Vec::new();
        for r in 0..n {
            let w: Vec<u64> = (0..bits.len()).map(|s| widths[s][r]).collect();
            closing.push(&column, r, k.row(r), &w);
            while let Some(b) = closing.take() {
                out.push((b.start, b.start + b.rows(), b.depth));
            }
        }
        closing.finish();
        while let Some(b) = closing.take() {
            out.push((b.start, b.start + b.rows(), b.depth));
        }
        (whole, out)
    }

    const INTS: &str =
        "CREATE SCHEMA s; CREATE TABLE s.t (a integer NOT NULL, b integer NOT NULL, \
                        PRIMARY KEY (a, b));";

    #[test]
    fn the_blocks_closed_as_rows_come_are_the_cut_of_them_all() {
        let mut seed = Seeded(0x9e37_79b9_7f4a_7c15);
        for case in 0..300 {
            let n = 1 + (seed.next() % 6000) as usize;
            let spread = 1 + seed.next() % [4, 64, 1 << 12, 1 << 30][case % 4];
            let mut rows: Vec<(i64, i64)> = (0..n)
                .map(|_| {
                    (
                        (seed.next() % spread) as i64 - (spread / 2) as i64,
                        (seed.next() % 1000) as i64,
                    )
                })
                .collect();
            rows.sort();
            let keys: Vec<KeyValue> = rows
                .iter()
                .flat_map(|&(a, b)| [KeyValue::Int(a), KeyValue::Int(b)])
                .collect();
            // Three leaves: a fixed width, a varying one holding NULLs, and one of bits.
            let fixed = vec![4u64; n];
            let varying: Vec<u64> = (0..n)
                .map(|_| match seed.next() % 7 {
                    0 => NULL_BIT,
                    x => 4 + x * 9,
                })
                .collect();
            let thin: Vec<u64> = (0..n).map(|_| 1).collect();
            let group = [n + 1, 600, 1500, 97][case % 4];
            let (whole, closed) = both(
                INTS,
                keys,
                &[fixed, varying, thin],
                &[false, false, true],
                group,
            );
            assert_eq!(whole, closed, "case {case}: {n} rows, spread {spread}");
        }
    }

    #[test]
    fn text_keys_close_as_their_cut_has_them() {
        let ddl = "CREATE SCHEMA s; CREATE TABLE s.t (s text NOT NULL, PRIMARY KEY (s));";
        let mut seed = Seeded(0x2545_f491_4f6c_dd1d);
        for case in 0..60 {
            let n = 1 + (seed.next() % 5000) as usize;
            let mut text: Vec<String> = (0..n)
                .map(|_| {
                    let len = 1 + seed.next() % 12;
                    (0..len)
                        .map(|_| char::from(b'a' + (seed.next() % [2, 5, 26][case % 3]) as u8))
                        .collect()
                })
                .collect();
            text.sort();
            text.dedup();
            let n = text.len();
            let keys: Vec<KeyValue> = text
                .iter()
                .map(|s| KeyValue::Text(s.as_str().into()))
                .collect();
            let widths: Vec<u64> = text.iter().map(|s| s.len() as u64 + 4).collect();
            let group = [n + 1, 300, 1000][case % 3];
            let (whole, closed) = both(ddl, keys, &[widths], &[false], group);
            assert_eq!(whole, closed, "case {case}: {n} rows");
        }
    }

    #[test]
    fn one_wide_row_fills_alone_and_closes_at_its_full_depth() {
        let ddl = "CREATE SCHEMA s; CREATE TABLE s.t (a integer NOT NULL, PRIMARY KEY (a));";
        let n = 3000;
        let keys = ints(0..n as i64);
        let widths: Vec<u64> = (0..n as u64)
            .map(|i| if i % 500 == 7 { 5000 } else { 4 })
            .collect();
        let (whole, closed) = both(ddl, keys, &[widths], &[false], ROW_GROUP_ROWS);
        assert_eq!(whole, closed);
    }
}
