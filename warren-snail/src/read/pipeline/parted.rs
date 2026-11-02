//! A step held whole for windows that read their partitions whole, in the round's room. Past it, the
//! rows of the partitions that come last, by a hash of their keys, are written, a row at a time, so
//! that every partition is whole either among the rows held or among those read back: the rows
//! held are folded first, those of the last partition held kept with the rows read back.

use super::{eval, grouped_key, key_under, Frame, Key, Relation, Value};
use crate::read::spill::{row_width, Run};
use crate::room::{current, Room};
use crate::Result;
use sqlparser::ast::Expr;
use std::collections::BinaryHeap;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

/// A row held: its partition's hash, the place it came in, the row, and the bytes it holds.
struct Held {
    hash: u64,
    seq: u64,
    row: Vec<Value>,
    bytes: u64,
}

pub(super) struct Parted {
    room: Option<Rc<Room>>,
    width: usize,
    held: Vec<Option<Held>>,
    free: Vec<usize>,
    /// The rows held by their partitions' hashes, the greatest first, and their places among them.
    heap: BinaryHeap<(u64, usize)>,
    past: Option<Run>,
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

impl Parted {
    pub(super) fn new() -> Parted {
        Parted {
            room: current(),
            width: 0,
            held: Vec::new(),
            free: Vec::new(),
            heap: BinaryHeap::new(),
            past: None,
            seq: 0,
            order: None,
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.heap.is_empty() && self.past.as_ref().is_none_or(|p| p.len() == 0)
    }

    /// A row more, its partition's hash `hash`: held where the room has room for it, else written,
    /// or the row of the greatest hash held written to make room for it where its own is less.
    pub(super) fn push(&mut self, hash: u64, row: Vec<Value>) -> Result<()> {
        self.width = row.len();
        let bytes = row_width(&row);
        let seq = self.seq;
        self.seq += 1;
        if let Some(room) = self.room.clone() {
            loop {
                if room.hold(bytes) {
                    break;
                }
                match self.heap.peek() {
                    Some(&(top, _)) if hash < top => {
                        let (_, at) = self.heap.pop().expect("a row is held");
                        let h = self.held[at].take().expect("a row is held");
                        self.free.push(at);
                        room.let_go(h.bytes);
                        self.write(h.row)?;
                    }
                    _ => return self.write(row),
                }
            }
        }
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
        let width = row.len();
        self.past
            .get_or_insert_with(|| Run::past(width))
            .push(row)
    }

    /// Hands `fold` the rows a partition at a time or more, each partition whole: every row, in the
    /// order it came, where none was written; else the rows held of every partition but the last
    /// held, then that one's with the rows read back, held again in the room. Where the last
    /// partition held is all a pass holds, it is past the room by itself, and is held whole.
    pub(super) fn finish(
        mut self,
        hash_of: &dyn Fn(&[Value]) -> Result<u64>,
        fold: &mut dyn FnMut(Relation) -> Result<()>,
    ) -> Result<bool> {
        let mut spilled = false;
        loop {
            let mut rows: Vec<Held> = self.held.drain(..).flatten().collect();
            self.heap.clear();
            self.free.clear();
            rows.sort_by_key(|h| h.seq);
            let room = self.room.clone();
            let let_go = |bytes: u64| {
                if let Some(r) = &room {
                    r.let_go(bytes);
                }
            };
            let bytes_of = |rows: &[Held]| rows.iter().map(|h| h.bytes).sum::<u64>();
            let Some(mut past) = self.past.take() else {
                let_go(bytes_of(&rows));
                fold(Relation {
                    names: Vec::new(),
                    rows: rows.into_iter().map(|h| h.row).collect(),
                    order: self.order.filter(|_| !spilled),
                })?;
                return Ok(spilled);
            };
            spilled = true;
            past.seal()?;
            let last = rows.iter().map(|h| h.hash).max();
            let (done, carried): (Vec<Held>, Vec<Held>) =
                rows.into_iter().partition(|h| Some(h.hash) != last);
            let_go(bytes_of(&done));
            let progressed = !done.is_empty();
            if progressed {
                fold(Relation {
                    names: Vec::new(),
                    rows: done.into_iter().map(|h| h.row).collect(),
                    order: None,
                })?;
            }
            let back = past.into_rows()?;
            let hashes = back
                .iter()
                .map(|r| hash_of(r))
                .collect::<Result<Vec<u64>>>()?;
            let carried_bytes = bytes_of(&carried);
            let mut again = Parted::new();
            again.room = room.clone();
            if progressed {
                let_go(carried_bytes);
                for h in carried {
                    again.push(h.hash, h.row)?;
                }
                for (r, h) in back.into_iter().zip(hashes) {
                    again.push(h, r)?;
                }
            } else {
                let mut whole: Vec<Vec<Value>> = carried.into_iter().map(|h| h.row).collect();
                let mut rest = Vec::new();
                let mut more = 0;
                for (r, h) in back.into_iter().zip(hashes) {
                    match Some(h) == last {
                        true => {
                            more += row_width(&r);
                            whole.push(r);
                        }
                        false => rest.push((h, r)),
                    }
                }
                if let Some(r) = &room {
                    r.hold_anyway(more);
                }
                fold(Relation {
                    names: Vec::new(),
                    rows: whole,
                    order: None,
                })?;
                let_go(carried_bytes + more);
                for (h, r) in rest {
                    again.push(h, r)?;
                }
            }
            if again.is_empty() {
                return Ok(spilled);
            }
            self = again;
        }
    }
}
