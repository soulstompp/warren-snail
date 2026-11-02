//! A place's rows held by a hash of their keys, in the round's room. Past it, the rows of the keys
//! that come last, by that hash, are written, a row at a time, so that every key's rows are whole
//! among the rows of one pass: the rows held of every key but the last held, then, read back with the
//! rows of that one, the next. Held for windows that read their partitions whole, and for a hash a
//! join or a subquery meets rows by.

use super::{eval, grouped_key, key_under, Frame, Key, Relation, Value};
use crate::read::spill::{row_width, Run, CHUNK};
use crate::room::{current, Room};
use crate::Result;
use sqlparser::ast::Expr;
use std::collections::BinaryHeap;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

/// A row held: its key's hash, the place it came in, the row, and the bytes it holds.
struct Held {
    hash: u64,
    seq: u64,
    row: Vec<Value>,
    bytes: u64,
}

/// The rows of one pass, each with its key's hash, in the order they came: every row of a key whose
/// hash lies below `below`, or of every key left where it is none. They hold their room until it
/// is dropped.
pub(in crate::read) struct Pass {
    pub(in crate::read) rows: Vec<(u64, Vec<Value>)>,
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
    past: Option<Run>,
    /// What the pass given last left: the rows of the last key held, and the rows written.
    after: Option<(Vec<Held>, Run)>,
    seq: u64,
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
    rows.iter().map(|h| h.bytes).sum()
}

impl Parted {
    pub(in crate::read) fn new() -> Parted {
        Parted {
            room: current(),
            held: Vec::new(),
            held_bytes: 0,
            free: Vec::new(),
            heap: BinaryHeap::new(),
            past: None,
            after: None,
            seq: 0,
            order: None,
        }
    }

    /// A row more, its key's hash `hash`: held where the room has room for it, or while a pass holds
    /// less than a chunk; else written, or the row of the greatest hash held written to make room
    /// for it where its own is less.
    pub(in crate::read) fn push(&mut self, hash: u64, row: Vec<Value>) -> Result<()> {
        let bytes = row_width(&row);
        let seq = self.seq;
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
                match self.heap.peek() {
                    Some(&(top, _)) if hash < top => {
                        let (_, at) = self.heap.pop().expect("a row is held");
                        let h = self.held[at].take().expect("a row is held");
                        self.free.push(at);
                        self.held_bytes -= h.bytes;
                        room.let_go(h.bytes);
                        self.write(h.row)?;
                    }
                    _ => return self.write(row),
                }
            }
        }
        self.held_bytes += bytes;
        let h = Held {
            hash,
            seq,
            row,
            bytes,
        };
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

    fn write(&mut self, row: Vec<Value>) -> Result<()> {
        self.past.get_or_insert_with(Run::past).push(row)
    }

    /// The next pass: every row where none was written; else the rows held of every key but the
    /// last held, that one's kept for the next pass with the rows read back. Where the last key
    /// held is all a pass holds, it is past the room by itself, and is held whole.
    pub(in crate::read) fn pass(
        &mut self,
        hash_of: &mut dyn FnMut(&[Value]) -> Result<u64>,
    ) -> Result<Option<Pass>> {
        if let Some((carried, past)) = self.after.take() {
            if let Some(r) = &self.room {
                r.let_go(bytes_of(&carried));
            }
            for h in carried {
                self.push(h.hash, h.row)?;
            }
            past.each_chunk(&mut |c| {
                for r in c {
                    let h = hash_of(&r)?;
                    self.push(h, r)?;
                }
                Ok(())
            })?;
        }
        let mut rows: Vec<Held> = self.held.drain(..).flatten().collect();
        self.heap.clear();
        self.free.clear();
        self.held_bytes = 0;
        rows.sort_by_key(|h| h.seq);
        let pass = |rows: Vec<Held>, below: Option<u64>, room: &Option<Rc<Room>>| Pass {
            bytes: bytes_of(&rows),
            rows: rows.into_iter().map(|h| (h.hash, h.row)).collect(),
            below,
            room: room.clone(),
        };
        let Some(past) = self.past.take() else {
            if rows.is_empty() {
                return Ok(None);
            }
            return Ok(Some(pass(rows, None, &self.room)));
        };
        past.seal()?;
        let last = rows.iter().map(|h| h.hash).max();
        let (done, carried): (Vec<Held>, Vec<Held>) =
            rows.into_iter().partition(|h| Some(h.hash) != last);
        if !done.is_empty() {
            self.after = Some((carried, past));
            return Ok(Some(pass(done, last, &self.room)));
        }
        let mut whole = carried;
        let mut last = last;
        past.each_chunk(&mut |c| {
            for r in c {
                let hash = hash_of(&r)?;
                if *last.get_or_insert(hash) == hash {
                    let bytes = row_width(&r);
                    if let Some(room) = &self.room {
                        room.hold_anyway(bytes);
                    }
                    whole.push(Held {
                        hash,
                        seq: 0,
                        row: r,
                        bytes,
                    });
                } else {
                    self.push(hash, r)?;
                }
            }
            Ok(())
        })?;
        let below = last.map(|h| h.saturating_add(1));
        Ok(Some(pass(whole, below, &self.room)))
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
            let rows = std::mem::take(&mut pass.rows);
            fold(Relation {
                names: Vec::new(),
                rows: rows.into_iter().map(|(_, r)| r).collect(),
                order: order.filter(|_| !spilled),
            })?;
        }
        Ok(spilled)
    }
}
