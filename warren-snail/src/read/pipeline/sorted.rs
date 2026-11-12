//! A step's rows sorted as its ORDER BY says, in the round's room. Past it, within two rooms, the rows
//! that sort last are set aside, a row at a time, written where the room has no room for them, so
//! that every row set aside sorts after every row held: those held are handed on sorted, then those
//! set aside. Past two rooms, runs of a room each, merged. And what a LIMIT keeps of rows as they
//! come in their order.

use super::{compare, computed_none, tie, Casing, Casings, Frame, Limit, Prepared, Value};
use crate::cut::PAGE;
use crate::read::spill::{row_width, Back, Run, CHUNK};
use crate::room::{current, Room};
use crate::{Error, Result};
use sqlparser::ast::OrderByKind;
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::rc::Rc;

/// How an ORDER BY compares the values rows are sorted by, and the first comparison it could not
/// make.
struct Spec {
    /// Each value's direction, and whether its NULLs come first.
    dirs: Vec<(bool, bool)>,
    failed: RefCell<Option<Error>>,
}

impl Spec {
    fn compare(&self, a: &[Value], b: &[Value]) -> Ordering {
        for (i, &(desc, nulls_first)) in self.dirs.iter().enumerate() {
            let ord = match (a[i].is_null(), b[i].is_null()) {
                (true, true) => Ordering::Equal,
                (true, false) => {
                    if nulls_first {
                        Ordering::Less
                    } else {
                        Ordering::Greater
                    }
                }
                (false, true) => {
                    if nulls_first {
                        Ordering::Greater
                    } else {
                        Ordering::Less
                    }
                }
                (false, false) => match compare(&a[i], &b[i]) {
                    Ok(Some(o)) => {
                        if desc {
                            o.reverse()
                        } else {
                            o
                        }
                    }
                    Ok(None) => Ordering::Equal,
                    Err(e) => {
                        self.failed.borrow_mut().get_or_insert(e);
                        Ordering::Equal
                    }
                },
            };
            if ord != Ordering::Equal {
                return ord;
            }
        }
        Ordering::Equal
    }
}

/// A row held: the values it sorts by, the place it came, the row, and the bytes it holds.
struct Held {
    key: Vec<Value>,
    seq: u64,
    row: Vec<Value>,
    bytes: u64,
    spec: Rc<Spec>,
}

impl PartialEq for Held {
    fn eq(&self, other: &Held) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Held {}
impl PartialOrd for Held {
    fn partial_cmp(&self, other: &Held) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Held {
    fn cmp(&self, other: &Held) -> Ordering {
        self.spec
            .compare(&self.key, &other.key)
            .then(self.seq.cmp(&other.seq))
    }
}

/// The values a row is sorted by, refused where its type has no ordering.
pub(super) fn sort_key(
    row: &[Value],
    sorted: &[usize],
    under: &[Option<Casing>],
) -> Result<Vec<Value>> {
    sorted
        .iter()
        .enumerate()
        .map(|(i, &at)| {
            let v = row[at].clone();
            match v {
                Value::Scalar(ty, _) | Value::Composed(ty, _) | Value::Typed(ty, _)
                    if !ty.ordered() =>
                {
                    Err(Error::new(format!(
                        "could not identify an ordering operator for type {}",
                        ty.pg_name()
                    )))
                }
                Value::Geometric(g) => Err(Error::new(format!(
                    "could not identify an ordering operator for type {}",
                    g.kind().name()
                ))),
                v => Ok(match (under[i], v.string_key()) {
                    (Some(Casing::Library(id)), Some(s)) => {
                        Value::Bytes(crate::collations::get(id).key(s))
                    }
                    _ => v,
                }),
            }
        })
        .collect()
}

/// What a step's ORDER BY reads a row by: its places and the collations they are sorted under.
pub(super) fn sort_under(p: &Prepared, casings: &Casings) -> Result<Vec<Option<Casing>>> {
    let bytes = |i: usize| match p.order_by.map(|ob| &ob.kind) {
        Some(OrderByKind::Expressions(e)) => e
            .get(i)
            .is_some_and(|o| crate::read::ordering::by_bytes(&o.options)),
        _ => false,
    };
    p.sorted
        .iter()
        .enumerate()
        .map(|(i, &at)| match p.carried.get(at) {
            _ if bytes(i) => Ok(None),
            Some(c) => casings.carried(c),
            None => casings.keyed(p.hidden[at - p.items.len()]),
        })
        .collect()
}

/// A step's rows, the least of them held. Rows within two rooms: those that sort last written, once,
/// each pass handed on in order. Past that, known before the first write, or once a room of them is
/// written: runs of a room each, the last held, merged, each row written once and read back once; the
/// rows written before then standing as one part.
pub(super) struct Sorter {
    room: Option<Rc<Room>>,
    spec: Rc<Spec>,
    sorted: Vec<usize>,
    under: Vec<Option<Casing>>,
    held: Vec<Held>,
    /// Those held as a heap, the greatest first, once one was written.
    heap: Option<BinaryHeap<Held>>,
    bytes: u64,
    past: Option<Past>,
    /// Whether the rows are cut into runs, and those written.
    in_runs: bool,
    runs: Vec<Run>,
    /// How many rows are to come, where known.
    pub(super) rows_to_come: Option<u64>,
    seq: u64,
}

impl Sorter {
    pub(super) fn new(p: &Prepared, casings: &Casings) -> Result<Sorter> {
        let mut dirs: Vec<(bool, bool)> = match p.order_by.map(|ob| &ob.kind) {
            Some(OrderByKind::Expressions(e)) => e
                .iter()
                .map(|o| {
                    let desc = crate::read::ordering::descends(&o.options);
                    (desc, o.options.nulls_first.unwrap_or(desc))
                })
                .collect(),
            Some(_) => return Err(Error::new("ORDER BY ALL is not read")),
            None => Vec::new(),
        };
        // The DISTINCT ON keys sorted after the ORDER BY's, ascending, NULLs last.
        dirs.resize(p.sorted.len(), (false, false));
        Ok(Sorter {
            room: current(),
            spec: Rc::new(Spec {
                dirs,
                failed: RefCell::new(None),
            }),
            sorted: p.sorted.clone(),
            under: sort_under(p, casings)?,
            held: Vec::new(),
            heap: None,
            bytes: 0,
            past: None,
            in_runs: false,
            runs: Vec::new(),
            rows_to_come: None,
            seq: 0,
        })
    }

    /// The rows it holds.
    pub(super) fn held(&self) -> usize {
        self.held.len() + self.heap.as_ref().map_or(0, BinaryHeap::len)
    }

    /// The rows held, sorted, their room let go with `bytes`.
    fn sorted_held(&mut self) -> Result<Vec<Held>> {
        let mut rows = match self.heap.take() {
            Some(heap) => heap.into_vec(),
            None => std::mem::take(&mut self.held),
        };
        rows.sort_unstable();
        match self.spec.failed.borrow_mut().take() {
            Some(e) => Err(e),
            None => Ok(rows),
        }
    }

    /// The rows held written as a run, sorted, those that tie in the order they came, and their room
    /// let go.
    fn write_run(&mut self) -> Result<()> {
        let rows = self.sorted_held()?;
        let run = Run::past();
        for h in rows {
            run.push(h.row)?;
        }
        run.seal()?;
        self.runs.push(run);
        if let Some(r) = &self.room {
            r.let_go(std::mem::take(&mut self.bytes));
        }
        Ok(())
    }

    /// A row more, after those pushed before it.
    pub(super) fn push(&mut self, row: Vec<Value>) -> Result<()> {
        let seq = self.seq;
        self.seq += 1;
        self.place(row, seq, true)
    }

    /// A row at place `seq`: held where the room has room for it, or while it holds less than a
    /// chunk. Else, where `cut`, cut into runs, the rows held written as one; or within two rooms
    /// set aside as it came, or the row of the greatest key held let go to make room for it where its
    /// own is less. Within two rooms, a row that sorts after one set aside is set aside too.
    fn place(&mut self, row: Vec<Value>, seq: u64, cut: bool) -> Result<()> {
        let key = sort_key(&row, &self.sorted, &self.under)?;
        let bytes = row_width(&row);
        let mut item = Held {
            key,
            seq,
            row,
            bytes,
            spec: self.spec.clone(),
        };
        let room = self.room.clone();
        if let Some(room) = room {
            loop {
                // Where a row let go freed more room than the row that took it needs, or another
                // place let room go, a row that sorts after one set aside is set aside with those
                // that came, to be handed on after it.
                let after = !self.in_runs
                    && self
                        .past
                        .as_ref()
                        .and_then(|p| p.least.as_ref())
                        .is_some_and(|l| item > *l);
                if room.hold(bytes) {
                    if !after {
                        break;
                    }
                    room.let_go(bytes);
                    let past = self.past.as_mut().expect("rows were set aside");
                    return past.came.push(item.row);
                }
                if !after && self.bytes + bytes <= CHUNK {
                    room.hold_anyway(bytes);
                    break;
                }
                let size = room.sizes().bytes;
                // Rows known past two rooms, or a room of them written, are cut into runs, where they
                // may be.
                if cut {
                    self.in_runs = self.in_runs
                        || self
                            .rows_to_come
                            .is_some_and(|n| n.saturating_mul(bytes) > size.saturating_mul(2))
                        || self.past.as_ref().is_some_and(|p| p.total() > size);
                }
                if self.in_runs {
                    self.write_run()?;
                    continue;
                }
                let heap = self
                    .heap
                    .get_or_insert_with(|| std::mem::take(&mut self.held).into());
                match heap.peek() {
                    Some(top) if item < *top => {
                        let mut out = heap.pop().expect("a row is held");
                        self.bytes -= out.bytes;
                        room.let_go(out.bytes);
                        let past = self.past.get_or_insert_with(Past::new);
                        past.let_go.push(std::mem::take(&mut out.row))?;
                        past.least = Some(out);
                    }
                    _ => {
                        let past = self.past.get_or_insert_with(Past::new);
                        past.came.push(std::mem::take(&mut item.row))?;
                        if !after {
                            past.least = Some(item);
                        }
                        return Ok(());
                    }
                }
            }
        }
        self.bytes += bytes;
        match &mut self.heap {
            Some(heap) => heap.push(item),
            None => self.held.push(item),
        }
        Ok(())
    }

    /// Hands `each` the rows sorted, a pass or a chunk at a time, with the values each was sorted by.
    pub(super) fn finish(
        mut self,
        each: &mut dyn FnMut(Vec<Vec<Value>>, Vec<Vec<Value>>) -> Result<()>,
    ) -> Result<()> {
        loop {
            if self.in_runs {
                return self.merge(each);
            }
            let rows = self.sorted_held()?;
            let bytes = std::mem::take(&mut self.bytes);
            let mut keys = Vec::with_capacity(rows.len());
            let mut out = Vec::with_capacity(rows.len());
            for h in rows {
                keys.push(h.key);
                out.push(h.row);
            }
            each(out, keys)?;
            if let Some(r) = &self.room {
                r.let_go(bytes);
            }
            let Some(past) = self.past.take() else {
                return Ok(());
            };
            self.rows_to_come = None;
            // The rows let go come greatest first: none of them is cut into runs, whose rows tie in
            // the order they come.
            past.each(&mut |row, seq, let_go| self.place(row, seq, !let_go))?;
        }
    }

    /// The runs merged: the rows held, sorted; where a part was written before the runs began, it is
    /// held in their place, sorted, and they are written as a run.
    fn merge(
        mut self,
        each: &mut dyn FnMut(Vec<Vec<Value>>, Vec<Vec<Value>>) -> Result<()>,
    ) -> Result<()> {
        let mut bytes = 0;
        let parted = match self.past.take() {
            Some(part) => {
                self.write_run()?;
                bytes = part.total();
                // The rows that came it holds let their room go as they are handed on, and are held
                // again here.
                let came = part.came.held_bytes();
                if let Some(r) = &self.room {
                    r.hold_anyway(bytes - came);
                }
                part.each(&mut |row, seq, _| {
                    self.held.push(Held {
                        key: sort_key(&row, &self.sorted, &self.under)?,
                        seq,
                        bytes: row_width(&row),
                        row,
                        spec: self.spec.clone(),
                    });
                    Ok(())
                })?;
                if let Some(r) = self.room.as_ref().filter(|_| came > 0) {
                    r.hold_anyway(came);
                }
                true
            }
            None => false,
        };
        let held = self.sorted_held()?;
        bytes += std::mem::take(&mut self.bytes);
        // As many runs as the room has a page each for. Past it, runs side by side merged, those of
        // the fewest bytes first, written again in their place, so the runs stand in the order they
        // were written; never the first with those after the part. The first merge takes only as many
        // as leave merges of that many to bring them to that many, each after it that many.
        let most = self
            .room
            .as_ref()
            .map_or(usize::MAX, |r| {
                (r.sizes().bytes / (2 * PAGE as u64)) as usize
            })
            .max(8);
        let after = usize::from(parted);
        let mut take = self.runs.len().saturating_sub(2) % (most - 1) + 2;
        while self.runs.len() > most {
            let from = (after..=self.runs.len() - take)
                .min_by_key(|&i| self.runs[i..i + take].iter().map(Run::total).sum::<u64>())
                .expect("runs past the most");
            let group: Vec<Run> = self.runs.drain(from..from + take).collect();
            let run = Run::past();
            self.merged(Vec::new(), take, &group, &mut |rows, _| {
                rows.into_iter().try_for_each(|r| run.push(r))
            })?;
            run.seal()?;
            self.runs.insert(from, run);
            take = most;
        }
        let runs = std::mem::take(&mut self.runs);
        // The rows held came last; a part, after the first run, the rows held as the runs began.
        let held_at = if parted { 1 } else { runs.len() };
        self.merged(held, held_at, &runs, each)?;
        if let Some(r) = &self.room {
            r.let_go(bytes);
        }
        Ok(())
    }

    /// `held` and each of `runs` merged, handed on a chunk at a time: rows that tie in the order of
    /// the runs, `held` as though it were the run written `held_at`th.
    fn merged(
        &self,
        held: Vec<Held>,
        held_at: usize,
        runs: &[Run],
        each: &mut dyn FnMut(Vec<Vec<Value>>, Vec<Vec<Value>>) -> Result<()>,
    ) -> Result<()> {
        let _pages = crate::room::flying(runs.len() as u64 * PAGE as u64);
        let mut backs: Vec<Back> = runs.iter().map(Run::back).collect();
        let mut first = held.into_iter();
        // Where the rows of each sort among rows that tie.
        let rank = |i: usize| -> u64 {
            match i {
                _ if i == runs.len() => held_at as u64,
                _ if i < held_at => i as u64,
                _ => i as u64 + 1,
            }
        };
        let next_of = |i: usize,
                       backs: &mut Vec<Back>,
                       first: &mut std::vec::IntoIter<Held>|
         -> Result<Option<Held>> {
            if i == backs.len() {
                return Ok(first.next().map(|mut h| {
                    h.seq = rank(i);
                    h
                }));
            }
            Ok(match backs[i].next_row()? {
                Some(row) => Some(Held {
                    key: sort_key(&row, &self.sorted, &self.under)?,
                    seq: rank(i),
                    bytes: 0,
                    row,
                    spec: self.spec.clone(),
                }),
                None => None,
            })
        };
        // Each row beside the run it came from.
        let mut heap = BinaryHeap::with_capacity(backs.len() + 1);
        for i in 0..=backs.len() {
            if let Some(h) = next_of(i, &mut backs, &mut first)? {
                heap.push(std::cmp::Reverse((h, i)));
            }
        }
        let (mut rows, mut keys, mut bytes) = (Vec::new(), Vec::new(), 0u64);
        while let Some(std::cmp::Reverse((h, i))) = heap.pop() {
            bytes += row_width(&h.row);
            keys.push(h.key);
            rows.push(h.row);
            if bytes >= CHUNK {
                let _flying = crate::room::flying(std::mem::take(&mut bytes));
                each(std::mem::take(&mut rows), std::mem::take(&mut keys))?;
            }
            if let Some(n) = next_of(i, &mut backs, &mut first)? {
                heap.push(std::cmp::Reverse((n, i)));
            }
        }
        if let Some(e) = self.spec.failed.borrow_mut().take() {
            return Err(e);
        }
        if !rows.is_empty() {
            let _flying = crate::room::flying(bytes);
            each(rows, keys)?;
        }
        Ok(())
    }
}

/// The rows set aside within two rooms: those the room let go to make room for a row that sorts
/// before them, each sorting before the one let go before it, written; and those set aside as they
/// came, each sorting after every row held then, held while the room has room for them and none of
/// them is written. Of rows that tie, those let go came before those set aside as they came.
struct Past {
    let_go: Run,
    came: Run,
    /// The least of its rows, its values and place.
    least: Option<Held>,
}

impl Past {
    fn new() -> Past {
        Past {
            let_go: Run::past(),
            came: Run::new(),
            least: None,
        }
    }

    /// The bytes all its rows are held at.
    fn total(&self) -> u64 {
        self.let_go.total() + self.came.total()
    }

    /// Its rows read back once, each with a place that keeps rows that tie in the order they came,
    /// and whether it was let go: those let go first, the last let go at the first place, then those
    /// set aside as they came, those held each letting its room go as it is handed on. These keep
    /// their last page in memory, so the two write no more pages than one.
    fn each(&self, each: &mut dyn FnMut(Vec<Value>, u64, bool) -> Result<()>) -> Result<()> {
        self.let_go.seal()?;
        let n = self.let_go.len() as u64;
        let mut at = n;
        self.let_go.each_chunk(&mut |c| {
            c.into_iter().try_for_each(|row| {
                at -= 1;
                each(row, at, true)
            })
        })?;
        let mut at = n;
        self.came.hand_held(&mut |row| {
            at += 1;
            each(row, at - 1, false)
        })?;
        self.came.each_chunk(&mut |c| {
            c.into_iter().try_for_each(|row| {
                at += 1;
                each(row, at - 1, false)
            })
        })
    }
}

/// What DISTINCT ON keeps of rows as they come in their order: each row whose first `lead` values
/// it is sorted by tie with none it kept before, the first of each run, held from chunk to chunk.
pub(super) struct First {
    lead: usize,
    last: Option<Vec<Value>>,
}

impl First {
    pub(super) fn of(lead: usize) -> First {
        First { lead, last: None }
    }

    /// Of `rows` as they come, with the values each is sorted by, those it keeps and their values.
    pub(super) fn take(
        &mut self,
        rows: Vec<Vec<Value>>,
        keys: Vec<Vec<Value>>,
    ) -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
        let mut kept = (Vec::new(), Vec::new());
        for (row, key) in rows.into_iter().zip(keys) {
            let lead = &key[..self.lead.min(key.len())];
            if self.last.as_deref().is_some_and(|l| tie(l, lead)) {
                continue;
            }
            self.last = Some(lead.to_vec());
            kept.0.push(row);
            kept.1.push(key);
        }
        kept
    }
}

/// What a LIMIT keeps of rows as they come in their order: its OFFSET's first passed over, then as
/// many as its count, and where it keeps ties, each after them that sorts with the last.
pub(super) struct Cut {
    skip: u64,
    left: Option<u64>,
    ties: bool,
    last: Option<Vec<Value>>,
    /// Whether it reads no more rows: a count of none, which reads none, its OFFSET's neither; or
    /// a row past its count found not to tie.
    ended: bool,
}

impl Cut {
    pub(super) fn of(limit: &Limit, frame: &Frame) -> Result<Cut> {
        let read = |e: &sqlparser::ast::Expr| -> Result<Option<i64>> {
            let v = super::eval(e, &[], frame, &computed_none())?;
            match super::cast(
                v,
                &sqlparser::ast::DataType::BigInt(None),
                frame.release,
                frame.casings.monetary(),
                super::quoted(e),
            )? {
                Value::Int(n) => Ok(Some(n)),
                _ => Ok(None),
            }
        };
        let skip = match limit.offset.map(read).transpose()?.flatten() {
            Some(n) if n < 0 => return Err(Error::new("OFFSET must not be negative")),
            n => n.unwrap_or(0) as u64,
        };
        let left = match limit.count {
            super::Count::All => None,
            super::Count::One => Some(1),
            super::Count::Of(e) => match read(e)? {
                Some(n) if n < 0 => return Err(Error::new("LIMIT must not be negative")),
                n => n.map(|n| n as u64),
            },
        };
        Ok(Cut {
            skip,
            left,
            ties: limit.ties,
            last: None,
            ended: left == Some(0),
        })
    }

    /// The rows as they come it reaches: its OFFSET's and its count; none where it has no count.
    pub(super) fn reached(&self) -> Option<u64> {
        self.left.map(|n| n.saturating_add(self.skip))
    }

    /// Whether it reads no row past those it has read.
    pub(super) fn ended(&self) -> bool {
        self.ended || (self.skip == 0 && self.left == Some(0) && !self.ties)
    }

    /// Of `rows` as they come, with the values each sorts by where it keeps ties, those it keeps;
    /// `each` made of every row it reaches in their order, those its OFFSET passes over among them
    /// and, where it keeps ties, the first past its count, made before it is found to tie or not.
    pub(super) fn take(
        &mut self,
        rows: Vec<Vec<Value>>,
        keys: &[Vec<Value>],
        each: &mut dyn FnMut(&mut Vec<Value>) -> Result<()>,
    ) -> Result<Vec<Vec<Value>>> {
        let mut out = Vec::new();
        for (i, mut row) in rows.into_iter().enumerate() {
            if self.ended {
                break;
            }
            if self.skip > 0 {
                each(&mut row)?;
                self.skip -= 1;
                continue;
            }
            match self.left {
                Some(0) if !self.ties => {
                    self.ended = true;
                    break;
                }
                Some(0) => {
                    each(&mut row)?;
                    let with = self
                        .last
                        .as_ref()
                        .zip(keys.get(i))
                        .is_some_and(|(a, b)| tie(a, b));
                    if !with {
                        self.ended = true;
                        break;
                    }
                    out.push(row);
                    continue;
                }
                Some(n) => self.left = Some(n - 1),
                None => {}
            }
            if self.ties {
                self.last = keys.get(i).cloned();
            }
            each(&mut row)?;
            out.push(row);
        }
        Ok(out)
    }
}
