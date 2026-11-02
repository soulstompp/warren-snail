//! A step's rows sorted as its ORDER BY says, in the round's room. Past it, within two rooms, the rows
//! that sort last are written, a row at a time, so that every row written sorts after every row held:
//! those held are handed on sorted, then those written read back. Past two rooms, runs of a room
//! each, merged. And what a LIMIT keeps of rows as they come in their order.

use super::{compare, computed_none, tie, Casing, Casings, Frame, Limit, Prepared, Value};
use crate::cut::PAGE;
use crate::read::spill::{row_width, Back, Run, CHUNK};
use crate::room::{current, Room};
use crate::{Error, Result};
use sqlparser::ast::{OrderByKind, OrderBySort};
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
    p.sorted
        .iter()
        .map(|&at| match p.carried.get(at) {
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
    past: Option<Run>,
    /// Whether the rows are cut into runs, and those written.
    in_runs: bool,
    runs: Vec<Run>,
    /// How many rows are to come, where known.
    pub(super) rows_to_come: Option<u64>,
    seq: u64,
}

impl Sorter {
    pub(super) fn new(p: &Prepared, casings: &Casings) -> Result<Sorter> {
        let dirs = match p.order_by.map(|ob| &ob.kind) {
            Some(OrderByKind::Expressions(e)) => e
                .iter()
                .map(|o| {
                    let desc = matches!(o.options.sort, Some(OrderBySort::Desc));
                    (desc, o.options.nulls_first.unwrap_or(desc))
                })
                .collect(),
            Some(_) => return Err(Error::new("ORDER BY ALL is not read")),
            None => Vec::new(),
        };
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

    /// The rows held written as a run, sorted, and their room let go.
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

    /// A row more: held where the room has room for it, or while it holds less than a chunk. Else
    /// cut into runs, the rows held written as one; or within two rooms written, or the row of the
    /// greatest key held written to make room for it where its own is less.
    pub(super) fn push(&mut self, row: Vec<Value>) -> Result<()> {
        let key = sort_key(&row, &self.sorted, &self.under)?;
        let bytes = row_width(&row);
        let seq = self.seq;
        self.seq += 1;
        let item = Held {
            key,
            seq,
            row,
            bytes,
            spec: self.spec.clone(),
        };
        let room = self.room.clone();
        if let Some(room) = room {
            loop {
                if room.hold(bytes) {
                    break;
                }
                if self.bytes + bytes <= CHUNK {
                    room.hold_anyway(bytes);
                    break;
                }
                let size = room.sizes().bytes;
                // Rows known past two rooms, or a room of them written, are cut into runs.
                self.in_runs = self.in_runs
                    || self
                        .rows_to_come
                        .is_some_and(|n| n.saturating_mul(bytes) > size.saturating_mul(2))
                    || self.past.as_ref().is_some_and(|p| p.total() > size);
                if self.in_runs {
                    self.write_run()?;
                    continue;
                }
                let heap = self
                    .heap
                    .get_or_insert_with(|| std::mem::take(&mut self.held).into());
                match heap.peek() {
                    Some(top) if item < *top => {
                        let out = heap.pop().expect("a row is held");
                        self.bytes -= out.bytes;
                        room.let_go(out.bytes);
                        self.past.get_or_insert_with(Run::past).push(out.row)?;
                    }
                    _ => return self.past.get_or_insert_with(Run::past).push(item.row),
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
            past.seal()?;
            self.rows_to_come = None;
            past.each_chunk(&mut |c| c.into_iter().try_for_each(|r| self.push(r)))?;
        }
    }

    /// The runs merged: the rows held, sorted; where a part was written before the runs began, it is
    /// held in their place, sorted, and they are written as a run.
    fn merge(
        mut self,
        each: &mut dyn FnMut(Vec<Vec<Value>>, Vec<Vec<Value>>) -> Result<()>,
    ) -> Result<()> {
        let mut bytes = 0;
        if let Some(part) = self.past.take() {
            self.write_run()?;
            part.seal()?;
            bytes = part.total();
            if let Some(r) = &self.room {
                r.hold_anyway(bytes);
            }
            part.each_chunk(&mut |c| {
                for row in c {
                    self.held.push(Held {
                        key: sort_key(&row, &self.sorted, &self.under)?,
                        seq: 0,
                        bytes: row_width(&row),
                        row,
                        spec: self.spec.clone(),
                    });
                }
                Ok(())
            })?;
        }
        let held = self.sorted_held()?;
        bytes += std::mem::take(&mut self.bytes);
        // As many runs as the room has a page each for: those past it merged first, written again.
        let most = self
            .room
            .as_ref()
            .map_or(usize::MAX, |r| {
                (r.sizes().bytes / (2 * PAGE as u64)) as usize
            })
            .max(8);
        while self.runs.len() > most {
            let rest = self.runs.split_off(most);
            let first = std::mem::replace(&mut self.runs, rest);
            let run = Run::past();
            self.merged(Vec::new(), &first, &mut |rows, _| {
                rows.into_iter().try_for_each(|r| run.push(r))
            })?;
            run.seal()?;
            self.runs.push(run);
        }
        let runs = std::mem::take(&mut self.runs);
        self.merged(held, &runs, each)?;
        if let Some(r) = &self.room {
            r.let_go(bytes);
        }
        Ok(())
    }

    /// `held` and each of `runs` merged, handed on a chunk at a time.
    fn merged(
        &self,
        held: Vec<Held>,
        runs: &[Run],
        each: &mut dyn FnMut(Vec<Vec<Value>>, Vec<Vec<Value>>) -> Result<()>,
    ) -> Result<()> {
        let _pages = crate::room::flying(runs.len() as u64 * PAGE as u64);
        let mut backs: Vec<Back> = runs.iter().map(Run::back).collect();
        let mut first = held.into_iter();
        let next_of = |i: usize,
                       backs: &mut Vec<Back>,
                       first: &mut std::vec::IntoIter<Held>|
         -> Result<Option<Held>> {
            if i == backs.len() {
                return Ok(first.next().map(|mut h| {
                    h.seq = i as u64;
                    h
                }));
            }
            Ok(match backs[i].next_row()? {
                Some(row) => Some(Held {
                    key: sort_key(&row, &self.sorted, &self.under)?,
                    seq: i as u64,
                    bytes: 0,
                    row,
                    spec: self.spec.clone(),
                }),
                None => None,
            })
        };
        let mut heap = BinaryHeap::with_capacity(backs.len() + 1);
        for i in 0..=backs.len() {
            if let Some(h) = next_of(i, &mut backs, &mut first)? {
                heap.push(std::cmp::Reverse(h));
            }
        }
        let (mut rows, mut keys, mut bytes) = (Vec::new(), Vec::new(), 0u64);
        while let Some(std::cmp::Reverse(h)) = heap.pop() {
            let i = h.seq as usize;
            bytes += row_width(&h.row);
            keys.push(h.key);
            rows.push(h.row);
            if bytes >= CHUNK {
                let _flying = crate::room::flying(std::mem::take(&mut bytes));
                each(std::mem::take(&mut rows), std::mem::take(&mut keys))?;
            }
            if let Some(n) = next_of(i, &mut backs, &mut first)? {
                heap.push(std::cmp::Reverse(n));
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

/// What a LIMIT keeps of rows as they come in their order: its OFFSET's first passed over, then as
/// many as its count, and where it keeps ties, each after them that sorts with the last.
pub(super) struct Cut {
    skip: u64,
    left: Option<u64>,
    ties: bool,
    last: Option<Vec<Value>>,
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
        })
    }

    /// The rows as they come it reaches: its OFFSET's and its count; none where it has no count.
    pub(super) fn reached(&self) -> Option<u64> {
        self.left.map(|n| n.saturating_add(self.skip))
    }

    /// Of `rows` as they come, with the values each sorts by where it keeps ties, those it keeps.
    pub(super) fn take(&mut self, rows: Vec<Vec<Value>>, keys: &[Vec<Value>]) -> Vec<Vec<Value>> {
        let mut out = Vec::new();
        for (i, row) in rows.into_iter().enumerate() {
            if self.skip > 0 {
                self.skip -= 1;
                continue;
            }
            match self.left {
                Some(0) => {
                    let with = self
                        .last
                        .as_ref()
                        .zip(keys.get(i))
                        .is_some_and(|(a, b)| self.ties && tie(a, b));
                    if !with {
                        break;
                    }
                }
                Some(n) => self.left = Some(n - 1),
                None => {}
            }
            if self.ties {
                self.last = keys.get(i).cloned();
            }
            out.push(row);
        }
        out
    }
}
