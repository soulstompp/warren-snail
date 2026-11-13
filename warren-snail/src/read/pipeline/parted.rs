//! A place's rows held by a hash of their keys, in the round's room. Past it, the rows of the keys
//! that come last, by that hash, are written, a row at a time, so that every key's rows are whole
//! among the rows of one pass: the rows held of every key but the last held, then, read back with the
//! rows of that one, the next. Rows within two rooms are written as one part; past that as many as
//! each fits the room, each row written once and read back once. Held for windows that read their
//! partitions whole, and for a subquery's table met by its keys.

use super::{eval, grouped_key, key_under, Frame, Key, Relation, Value};
use crate::read::held::Columns;
use crate::read::spill::{Packed, Packer, Parts, Queue, CHUNK};
use crate::room::{current, Room};
use crate::Result;
use sqlparser::ast::Expr;
use std::collections::BinaryHeap;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

/// A row held: its key's hash, the place it came in, and the row as one string of bytes.
struct Held {
    hash: u64,
    seq: u64,
    row: Packed,
}

impl Held {
    fn of(hash: u64, seq: u64, row: Packed) -> Held {
        Held { hash, seq, row }
    }

    /// The bytes it holds.
    fn bytes(&self) -> u64 {
        (std::mem::size_of::<Held>() + self.row.len()) as u64
    }
}

/// The rows of one pass, held column-wise, in the order they came: every row of a key whose hash
/// lies below `below`, or of every key left where it is none. They hold their room until it is
/// dropped.
pub(in crate::read) struct Pass {
    pub(in crate::read) rows: Columns,
    pub(in crate::read) below: Option<u64>,
    bytes: u64,
    room: Option<Rc<Room>>,
}

impl Drop for Pass {
    fn drop(&mut self) {
        if let Some(r) = &self.room {
            r.let_go(self.bytes);
        }
    }
}

pub(in crate::read) struct Parted {
    room: Option<Rc<Room>>,
    held: Vec<Option<Held>>,
    held_bytes: u64,
    free: Vec<usize>,
    /// The rows held by their keys' hashes, the greatest first, and their places among them.
    heap: BinaryHeap<(u64, usize)>,
    /// The rows being written of the hashes being read, those to come known as bytes where they are.
    writing: Option<Parts>,
    range: (u64, u64),
    expected: Option<u64>,
    /// How many rows are to come, where known, its bytes read from the first row written.
    pub(in crate::read) rows_to_come: Option<u64>,
    /// The parts written and still to read; the rows of the last key held, kept for the next pass.
    queue: Queue,
    carried: Vec<Held>,
    seq: u64,
    packer: Packer,
    pub(super) order: Option<usize>,
}

/// The hash of a row's partition, as the windows' PARTITION BY keys it.
pub(super) fn partition_hash(by: &[Expr], row: &[Value], frame: &Frame) -> Result<u64> {
    let none = super::computed_none();
    let mut h = std::hash::DefaultHasher::new();
    for p in by {
        let v = eval(p, row, frame, &none)?;
        grouped_key(&v)?;
        let k: Key = key_under(&v, frame.casings.keyed(p)?);
        k.hash(&mut h);
    }
    Ok(h.finish())
}

/// The hash of a key, salted by how many times its rows were cut into batches.
pub(in crate::read) fn key_hash(k: &[Key], salt: u32) -> u64 {
    let mut h = std::hash::DefaultHasher::new();
    salt.hash(&mut h);
    k.hash(&mut h);
    h.finish()
}

fn bytes_of(rows: &[Held]) -> u64 {
    rows.iter().map(|h| h.bytes()).sum()
}

/// The bytes a part's rows take held here: each as the spill wrote it, and its place.
fn held_as_packed(run: &crate::read::spill::Run) -> u64 {
    run.written() + run.len() as u64 * std::mem::size_of::<Held>() as u64
}

impl Parted {
    pub(in crate::read) fn new() -> Parted {
        Parted {
            room: current(),
            held: Vec::new(),
            held_bytes: 0,
            free: Vec::new(),
            heap: BinaryHeap::new(),
            writing: None,
            range: (0, u64::MAX),
            expected: None,
            rows_to_come: None,
            queue: Queue::default(),
            carried: Vec::new(),
            seq: 0,
            packer: Packer::default(),
            order: None,
        }
    }

    /// A row more, its key's hash `hash`: held where the room has room for it, or while a pass holds
    /// less than a chunk; else written, or the row of the greatest hash held written to make room
    /// for it where its own is less.
    pub(in crate::read) fn push(&mut self, hash: u64, row: Vec<Value>) -> Result<()> {
        let packed = self.packer.pack(&row);
        self.place(hash, packed, Some(row))
    }

    /// A row more as one string of bytes, `whole` the row where it is at hand.
    fn place(&mut self, hash: u64, packed: Packed, whole: Option<Vec<Value>>) -> Result<()> {
        let h = Held::of(hash, self.seq, packed);
        let bytes = h.bytes();
        self.seq += 1;
        if let Some(room) = self.room.clone() {
            loop {
                if room.hold(bytes) {
                    break;
                }
                if self.held_bytes + bytes <= CHUNK {
                    room.hold_anyway(bytes);
                    break;
                }
                if self.expected.is_none() {
                    self.expected = self.rows_to_come.map(|n| n * bytes);
                }
                match self.heap.peek() {
                    Some(&(top, _)) if hash < top => {
                        let (_, at) = self.heap.pop().expect("a row is held");
                        let out = self.held[at].take().expect("a row is held");
                        self.free.push(at);
                        self.held_bytes -= out.bytes();
                        room.let_go(out.bytes());
                        let row = self.packer.unpack(&out.row)?;
                        self.write(out.hash, row)?;
                    }
                    _ => {
                        let row = match whole {
                            Some(row) => row,
                            None => self.packer.unpack(&h.row)?,
                        };
                        return self.write(hash, row);
                    }
                }
            }
        }
        self.held_bytes += bytes;
        let at = match self.free.pop() {
            Some(at) => {
                self.held[at] = Some(h);
                at
            }
            None => {
                self.held.push(Some(h));
                self.held.len() - 1
            }
        };
        self.heap.push((hash, at));
        Ok(())
    }

    fn write(&mut self, hash: u64, row: Vec<Value>) -> Result<()> {
        let (lo, hi) = self.range;
        let expected = self.expected;
        self.writing
            .get_or_insert_with(|| Parts::new(lo, hi, expected))
            .push(hash, row)
    }

    /// The next pass: every row where none was written; else the rows held of every key but the
    /// last held, that one's kept for the next pass with the parts read back next. Where the last key
    /// held is all a pass holds, it is past the room by itself, and is held whole.
    pub(in crate::read) fn pass(
        &mut self,
        hash_of: &mut dyn FnMut(&[Value]) -> Result<u64>,
    ) -> Result<Option<Pass>> {
        // The rows of the key the pass before kept, and the next parts as many as the room holds.
        let carried = std::mem::take(&mut self.carried);
        let carrying = !carried.is_empty() || self.writing.is_none();
        if carrying && self.heap.is_empty() {
            let bytes = bytes_of(&carried);
            if let Some(r) = &self.room {
                r.let_go(bytes);
            }
            let free = self.room.as_ref().map_or(u64::MAX, |r| r.free_bytes());
            let n = carried.len() as u64;
            let next = self
                .queue
                .next(free.saturating_sub(bytes).max(CHUNK), &held_as_packed);
            for h in carried {
                self.place(h.hash, h.row, None)?;
            }
            if let Some((lo, hi, rows, runs)) = next {
                self.range = (lo, hi);
                self.expected = None;
                self.rows_to_come = Some(rows + n);
                for run in runs {
                    run.each_chunk(&mut |c| {
                        for r in c {
                            let h = hash_of(&r)?;
                            self.push(h, r)?;
                        }
                        Ok(())
                    })?;
                }
            }
        }
        let mut rows: Vec<Held> = self.held.drain(..).flatten().collect();
        self.heap.clear();
        self.free.clear();
        self.held_bytes = 0;
        rows.sort_by_key(|h| h.seq);
        // A pass's rows held column-wise, each row's string of bytes let go as it is read.
        let pass = |rows: Vec<Held>, below: Option<u64>, this: &Parted| -> Result<Pass> {
            let packed = bytes_of(&rows);
            let mut held = Columns::default();
            let mut bytes = 0;
            for h in rows {
                bytes += held.hold(this.packer.unpack(&h.row)?);
            }
            if let Some(r) = &this.room {
                r.let_go(packed);
                r.hold_anyway(bytes);
            }
            Ok(Pass {
                rows: held,
                below,
                bytes,
                room: this.room.clone(),
            })
        };
        let Some(written) = self.writing.take() else {
            if rows.is_empty() && self.queue.is_empty() {
                return Ok(None);
            }
            // Nothing written with them: the next part's first hash bounds them.
            let below = self.queue.front().map(|(lo, _)| lo);
            return Ok(Some(pass(rows, below, self)?));
        };
        written.into_queue(self.range.1, &mut self.queue)?;
        let last = rows.iter().map(|h| h.hash).max();
        let (done, carried): (Vec<Held>, Vec<Held>) =
            rows.into_iter().partition(|h| Some(h.hash) != last);
        if !done.is_empty() {
            self.carried = carried;
            return Ok(Some(pass(done, last, self)?));
        }
        // One key past the room by itself: its rows held whole, the rest of its part read again.
        let mut whole = carried;
        let mut last = last;
        let Some((lo, hi, rows, runs)) = self.queue.next(0, &held_as_packed) else {
            return Ok(Some(pass(whole, None, self)?));
        };
        self.range = (lo, hi);
        self.expected = None;
        self.rows_to_come = Some(rows);
        for run in runs {
            run.each_chunk(&mut |c| {
                for r in c {
                    let hash = hash_of(&r)?;
                    if *last.get_or_insert(hash) == hash {
                        let h = Held::of(hash, 0, self.packer.pack(&r));
                        if let Some(room) = &self.room {
                            room.hold_anyway(h.bytes());
                        }
                        whole.push(h);
                    } else {
                        self.push(hash, r)?;
                    }
                }
                Ok(())
            })?;
        }
        let below = last.map(|h| h.saturating_add(1));
        Ok(Some(pass(whole, below, self)?))
    }

    /// Hands `fold` the rows a partition at a time or more, each partition whole: every row, in the
    /// order it came, where none was written.
    pub(super) fn finish(
        mut self,
        hash_of: &dyn Fn(&[Value]) -> Result<u64>,
        fold: &mut dyn FnMut(Relation) -> Result<()>,
    ) -> Result<bool> {
        let order = self.order;
        let mut spilled = false;
        while let Some(mut pass) = self.pass(&mut |r| hash_of(r))? {
            spilled |= pass.below.is_some();
            let held = std::mem::take(&mut pass.rows);
            // The windows read a pass's rows whole, flying while they are folded, where the room
            // has room for them so; past that, as many whole partitions at a time as it has room
            // for, each in the order its first row came.
            let free = self.room.as_ref().map_or(u64::MAX, |r| r.free_bytes());
            let most = held.len() as u64
                * (std::mem::size_of::<Vec<Value>>() + held.width() * std::mem::size_of::<Value>())
                    as u64
                + held.bytes();
            if most <= free {
                let rows: Vec<Vec<Value>> = held.rows().collect();
                let _flying = crate::room::flying(crate::read::spill::rows_width(&rows));
                fold(Relation {
                    names: Vec::new(),
                    rows,
                    order: order.filter(|_| !spilled),
                })?;
                continue;
            }
            let _index = crate::room::holding(held.len() as u64 * 4);
            let free = self.room.as_ref().map_or(u64::MAX, |r| r.free_bytes());
            let mut parts: Vec<(Vec<u32>, u64)> = Vec::new();
            let mut of_hash: std::collections::HashMap<u64, usize> = Default::default();
            for i in 0..held.len() {
                let row = held.row(i);
                let p = *of_hash.entry(hash_of(&row)?).or_insert_with(|| {
                    parts.push((Vec::new(), 0));
                    parts.len() - 1
                });
                parts[p].0.push(i as u32);
                parts[p].1 += crate::read::spill::row_width(&row);
            }
            let whole = parts.len() == 1;
            let mut hand = |at: Vec<u32>, bytes: u64| -> Result<()> {
                let rows: Vec<Vec<Value>> = at.into_iter().map(|i| held.row(i as usize)).collect();
                let _flying = crate::room::flying(bytes);
                fold(Relation {
                    names: Vec::new(),
                    rows,
                    order: order.filter(|_| !spilled && whole),
                })
            };
            let (mut group, mut bytes) = (Vec::new(), 0);
            for (at, w) in parts {
                if !group.is_empty() && bytes + w > free {
                    hand(std::mem::take(&mut group), std::mem::take(&mut bytes))?;
                }
                group.extend(at);
                bytes += w;
            }
            hand(group, bytes)?;
        }
        Ok(spilled)
    }
}
