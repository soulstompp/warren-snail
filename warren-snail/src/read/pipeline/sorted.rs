//! A step's rows sorted as its ORDER BY says, in the round's room. Past it, the rows that sort last
//! are written, a row at a time, so that every row written sorts after every row held: those held
//! are handed on sorted, then those written read back, the same again. And what a LIMIT keeps of
//! rows as they come in their order.

use super::{compare, computed_none, tie, Casing, Casings, Frame, Limit, Prepared, Value};
use crate::read::spill::{row_width, Run, CHUNK};
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

/// A step's rows, the least of them held.
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
            seq: 0,
        })
    }

    /// The rows it holds.
    pub(super) fn held(&self) -> usize {
        self.held.len() + self.heap.as_ref().map_or(0, BinaryHeap::len)
    }

    /// A row more: held where the room has room for it, or while it holds less than a chunk; else
    /// written, or the row of the greatest key held written to make room for it where its own is
    /// less.
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

    /// Hands `each` the rows sorted, a pass at a time, with the values each was sorted by.
    pub(super) fn finish(
        mut self,
        each: &mut dyn FnMut(Vec<Vec<Value>>, Vec<Vec<Value>>) -> Result<()>,
    ) -> Result<()> {
        loop {
            let mut rows = match self.heap.take() {
                Some(heap) => heap.into_vec(),
                None => std::mem::take(&mut self.held),
            };
            rows.sort_unstable();
            if let Some(e) = self.spec.failed.borrow_mut().take() {
                return Err(e);
            }
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
            past.each_chunk(&mut |c| c.into_iter().try_for_each(|r| self.push(r)))?;
        }
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
