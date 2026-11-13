//! A place's rows in the round's room: held while the room has room, and past it written to the
//! round's spill once, a page at a time, and read back where the place is read. A value's held width
//! is the bytes it takes as it is held; on the spill it takes a byte naming its kind, then the value.

use super::held::{Columns, RowsAt};
use super::pipeline::{Key, Value};
use crate::catalog::ColumnType;
use crate::cut::PAGE;
use crate::geo::GeoKind;
use crate::room::{current, Room};
use crate::{Error, Result};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

const NULL: u8 = 0;
const INT: u8 = 1;
const NUM: u8 = 2;
const TEXT: u8 = 3;
const BYTES: u8 = 4;
const BOOL: u8 = 5;
const UUID: u8 = 6;
const SCALAR: u8 = 7;
const REAL: u8 = 8;
const DOUBLE: u8 = 9;
const STR: u8 = 10;
const BITS: u8 = 11;
const NUMERIC: u8 = 12;
const COMPOSED: u8 = 13;
const COMPOSED_NULL: u8 = 14;
const INTERVAL: u8 = 15;
const TYPED: u8 = 16;
const GEOMETRIC: u8 = 17;

/// The types and the geometric kinds a run's values name, each written as its place here.
#[derive(Default, Clone)]
struct Named {
    types: Vec<ColumnType>,
    kinds: Vec<GeoKind>,
}

impl Named {
    fn ty(&mut self, ty: ColumnType) -> u16 {
        match self.types.iter().position(|t| *t == ty) {
            Some(i) => i as u16,
            None => {
                self.types.push(ty);
                (self.types.len() - 1) as u16
            }
        }
    }

    fn kind(&mut self, k: GeoKind) -> u16 {
        match self.kinds.iter().position(|x| *x == k) {
            Some(i) => i as u16,
            None => {
                self.kinds.push(k);
                (self.kinds.len() - 1) as u16
            }
        }
    }
}

fn bytes_of(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(&(b.len() as u32).to_le_bytes());
    out.extend_from_slice(b);
}

/// A value as the spill writes it.
fn encode(v: &Value, named: &mut Named, out: &mut Vec<u8>) {
    match v {
        Value::Null => out.push(NULL),
        Value::Int(x) => {
            out.push(INT);
            out.extend_from_slice(&x.to_le_bytes());
        }
        Value::Num(m, s) => {
            out.push(NUM);
            out.extend_from_slice(&m.to_le_bytes());
            out.extend_from_slice(&s.to_le_bytes());
        }
        Value::Text(t) => {
            out.push(TEXT);
            bytes_of(out, t.as_bytes());
        }
        Value::Bytes(b) => {
            out.push(BYTES);
            bytes_of(out, b);
        }
        Value::Bool(b) => {
            out.push(BOOL);
            out.push(*b as u8);
        }
        Value::Uuid(k) => {
            out.push(UUID);
            out.extend_from_slice(&k.to_le_bytes());
        }
        Value::Scalar(ty, k) => {
            out.push(SCALAR);
            out.extend_from_slice(&named.ty(*ty).to_le_bytes());
            out.extend_from_slice(&k.to_le_bytes());
        }
        Value::Real(f) => {
            out.push(REAL);
            out.extend_from_slice(&f.to_bits().to_le_bytes());
        }
        Value::Double(d) => {
            out.push(DOUBLE);
            out.extend_from_slice(&d.to_bits().to_le_bytes());
        }
        Value::Str(ty, s) => {
            out.push(STR);
            out.extend_from_slice(&named.ty(*ty).to_le_bytes());
            bytes_of(out, s.as_bytes());
        }
        Value::Bits(ty, k) => {
            out.push(BITS);
            out.extend_from_slice(&named.ty(*ty).to_le_bytes());
            bytes_of(out, k);
        }
        Value::Numeric(n) => {
            out.push(NUMERIC);
            bytes_of(out, n.to_string().as_bytes());
        }
        Value::Composed(ty, c) if c.item.is_null() => {
            out.push(COMPOSED_NULL);
            out.extend_from_slice(&named.ty(*ty).to_le_bytes());
        }
        Value::Composed(ty, c) => {
            out.push(COMPOSED);
            out.extend_from_slice(&named.ty(*ty).to_le_bytes());
            bytes_of(out, crate::composed::text_out(*ty, &c.item).as_bytes());
        }
        Value::Interval(i) => {
            out.push(INTERVAL);
            out.extend_from_slice(&i.to_bytes());
        }
        Value::Typed(ty, s) => {
            out.push(TYPED);
            out.extend_from_slice(&named.ty(*ty).to_le_bytes());
            bytes_of(out, s.as_bytes());
        }
        Value::Geometric(g) => {
            out.push(GEOMETRIC);
            out.extend_from_slice(&named.kind(g.kind()).to_le_bytes());
            bytes_of(out, &crate::geo::geo_bytes(g));
        }
    }
}

/// The bytes a value takes on the spill.
pub(super) fn file_width(v: &Value) -> u64 {
    let var = |n: usize| 1 + 4 + n as u64;
    match v {
        Value::Null => 1,
        Value::Int(_) | Value::Double(_) => 9,
        Value::Num(..) => 21,
        Value::Text(t) => var(t.len()),
        Value::Bytes(b) => var(b.len()),
        Value::Bool(_) => 2,
        Value::Uuid(_) => 17,
        Value::Scalar(..) => 11,
        Value::Real(_) => 5,
        Value::Str(_, s) | Value::Typed(_, s) => 2 + var(s.len()),
        Value::Bits(_, k) => 2 + var(k.len()),
        Value::Interval(_) => 17,
        _ => {
            let mut out = Vec::new();
            encode(v, &mut Named::default(), &mut out);
            out.len() as u64
        }
    }
}

/// The bytes a row takes on the spill: as a block's rows are counted.
pub(super) fn row_file_width(row: &[Value]) -> u64 {
    row.iter().map(file_width).sum()
}

/// The bytes a value holds beyond its own: its text's, its bytes', what it keeps whole.
fn heap(v: &Value) -> u64 {
    match v {
        Value::Text(t) | Value::Str(_, t) | Value::Typed(_, t) => t.capacity() as u64,
        Value::Bytes(b) | Value::Bits(_, b) => b.capacity() as u64,
        Value::Numeric(_) | Value::Composed(..) | Value::Geometric(_) => file_width(v),
        _ => 0,
    }
}

/// The bytes a value is held at: its own and those it holds.
pub(super) fn width(v: &Value) -> u64 {
    std::mem::size_of::<Value>() as u64 + heap(v)
}

/// The bytes a row is held at: its vector, its values and what they hold.
pub(super) fn row_width(row: &Vec<Value>) -> u64 {
    (std::mem::size_of::<Vec<Value>>() + row.capacity() * std::mem::size_of::<Value>()) as u64
        + row.iter().map(heap).sum::<u64>()
}

/// The bytes rows are held at.
pub(super) fn rows_width(rows: &[Vec<Value>]) -> u64 {
    rows.iter().map(row_width).sum()
}

/// The bytes a key is held at: its own and those it holds.
pub(super) fn key_width(k: &Key) -> u64 {
    let heap = match k {
        Key::Text(t) | Key::Class(t) => t.capacity(),
        Key::Bytes(b) => b.capacity(),
        Key::Composed(b) => b.len(),
        _ => 0,
    };
    (std::mem::size_of::<Key>() + heap) as u64
}

/// The bytes a row's keys are held at.
pub(super) fn keys_width(keys: &Vec<Key>) -> u64 {
    (std::mem::size_of::<Vec<Key>>() + (keys.capacity() - keys.len()) * std::mem::size_of::<Key>())
        as u64
        + keys.iter().map(key_width).sum::<u64>()
}

/// The bytes a hash table holds for each entry of `T`: its slot and its control byte, at the most
/// its load lets it fill.
pub(super) fn slot<T>() -> u64 {
    ((std::mem::size_of::<T>() as u64 + 1) * 8).div_ceil(7)
}

struct Bytes<'a> {
    b: &'a [u8],
    at: usize,
}

impl Bytes<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8]> {
        let s = self
            .b
            .get(self.at..self.at + n)
            .ok_or_else(|| Error::new("a row of the round's spill is cut short"))?;
        self.at += n;
        Ok(s)
    }

    fn arr<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.take(N)?.try_into().unwrap())
    }

    fn var(&mut self) -> Result<Vec<u8>> {
        let n = u32::from_le_bytes(self.arr()?) as usize;
        Ok(self.take(n)?.to_vec())
    }

    fn text(&mut self) -> Result<String> {
        String::from_utf8(self.var()?)
            .map_err(|_| Error::new("a text of the round's spill is not UTF-8"))
    }
}

fn decode(b: &mut Bytes, named: &Named) -> Result<Value> {
    let ty = |b: &mut Bytes| -> Result<ColumnType> {
        Ok(named.types[u16::from_le_bytes(b.arr()?) as usize])
    };
    Ok(match b.take(1)?[0] {
        NULL => Value::Null,
        INT => Value::Int(i64::from_le_bytes(b.arr()?)),
        NUM => {
            let m = i128::from_le_bytes(b.arr()?);
            Value::Num(m, u32::from_le_bytes(b.arr()?))
        }
        TEXT => Value::Text(b.text()?),
        BYTES => Value::Bytes(b.var()?),
        BOOL => Value::Bool(b.take(1)?[0] != 0),
        UUID => Value::Uuid(i128::from_le_bytes(b.arr()?)),
        SCALAR => {
            let t = ty(b)?;
            Value::Scalar(t, i64::from_le_bytes(b.arr()?))
        }
        REAL => Value::Real(f32::from_bits(u32::from_le_bytes(b.arr()?))),
        DOUBLE => Value::Double(f64::from_bits(u64::from_le_bytes(b.arr()?))),
        STR => {
            let t = ty(b)?;
            Value::Str(t, b.text()?)
        }
        BITS => {
            let t = ty(b)?;
            Value::Bits(t, b.var()?)
        }
        NUMERIC => Value::Numeric(crate::numeric::Numeric::parse(&b.text()?)?),
        COMPOSED_NULL => super::pipeline::composed(ty(b)?, crate::composed::Item::Null),
        COMPOSED => {
            let t = ty(b)?;
            super::pipeline::composed_value(t, &b.text()?)?
        }
        INTERVAL => Value::Interval(
            crate::times::Interval::from_bytes(b.take(16)?)
                .ok_or_else(|| Error::new("an interval of the round's spill is not one"))?,
        ),
        TYPED => {
            let t = ty(b)?;
            Value::Typed(t, b.text()?)
        }
        GEOMETRIC => {
            let k = named.kinds[u16::from_le_bytes(b.arr()?) as usize];
            Value::Geometric(crate::geo::geo_of_bytes(k, &b.var()?)?)
        }
        x => return Err(Error::new(format!("the round's spill holds no kind {x}"))),
    })
}

/// One row, its `width` values, as the spill wrote it.
fn decode_row(b: &mut Bytes, width: usize, named: &Named) -> Result<Vec<Value>> {
    let mut row = Vec::with_capacity(width);
    for _ in 0..width {
        row.push(decode(b, named)?);
    }
    Ok(row)
}

/// The bytes of rows a place hands on at a time, past its first row: as a block's rows are.
pub(super) const CHUNK: u64 = 4 * PAGE as u64;

/// A place's rows as they lie: those the room holds, column-wise, then those written.
#[derive(Default)]
struct Stored {
    rows: Columns,
    bytes: u64,
    named: Named,
    /// Each page written, and the bytes of rows it holds.
    pages: Vec<(u64, u16)>,
    tail: Vec<u8>,
    spilled: usize,
    /// The bytes all its rows are held at.
    total: u64,
}

/// A place's rows: those the room holds, in order, then those past it, on the round's spill.
pub(super) struct Run {
    room: Option<Rc<Room>>,
    width: Cell<usize>,
    /// Whether every row is written, a place's rows past the room alone.
    past: bool,
    stored: RefCell<Stored>,
    /// Whether the step reading it now is its last, so the rows it holds may be written to make
    /// room, and how many readers are handing on its rows.
    last: Cell<bool>,
    reading: Cell<u32>,
}

impl Run {
    /// An empty place of rows, in the room of the round under way.
    pub(super) fn new() -> Run {
        Run {
            room: current(),
            width: Cell::new(0),
            past: false,
            stored: RefCell::default(),
            last: Cell::new(false),
            reading: Cell::new(0),
        }
    }

    /// An empty place of rows past the room, every row written.
    pub(super) fn past() -> Run {
        let mut run = Run::new();
        run.past = true;
        run
    }

    /// The rows `rows` held as a place: as many as the room has room for, the rest written.
    pub(super) fn of(rows: Vec<Vec<Value>>) -> Result<Run> {
        let run = Run::new();
        for r in rows {
            run.push(r)?;
        }
        run.seal()?;
        Ok(run)
    }

    /// One row more: held where the room has room for it and no row before it was written, so the
    /// rows keep their order; written otherwise.
    pub(super) fn push(&self, row: Vec<Value>) -> Result<()> {
        self.width.set(row.len());
        let mut s = self.stored.borrow_mut();
        let w = s.rows.width_of(&row);
        s.total += w;
        let room = match &self.room {
            Some(r) => r,
            None => {
                s.rows.push(row);
                return Ok(());
            }
        };
        if !self.past && s.spilled == 0 && room.hold(w) {
            s.bytes += w;
            s.rows.push(row);
            return Ok(());
        }
        let s = &mut *s;
        for v in &row {
            encode(v, &mut s.named, &mut s.tail);
        }
        s.spilled += 1;
        while s.tail.len() >= PAGE {
            let at = room.spill_page(&s.tail[..PAGE])?;
            s.pages.push((at, PAGE as u16));
            s.tail.drain(..PAGE);
        }
        Ok(())
    }

    /// Writes the rows past the room not yet written, the last page filled with nothing.
    pub(super) fn seal(&self) -> Result<()> {
        let mut s = self.stored.borrow_mut();
        if s.tail.is_empty() {
            return Ok(());
        }
        let room = self.room.as_ref().expect("a run that writes has a room");
        let valid = s.tail.len() as u16;
        s.tail.resize(PAGE, 0);
        let at = room.spill_page(&s.tail)?;
        s.pages.push((at, valid));
        s.tail.clear();
        Ok(())
    }

    pub(super) fn len(&self) -> usize {
        let s = self.stored.borrow();
        s.rows.len() + s.spilled
    }

    /// The bytes all its rows are held at.
    pub(super) fn total(&self) -> u64 {
        self.stored.borrow().total
    }

    /// The rows written past the room.
    pub(super) fn spilled(&self) -> usize {
        self.stored.borrow().spilled
    }

    /// The bytes of the rows it holds in the room.
    pub(super) fn held_bytes(&self) -> u64 {
        self.stored.borrow().bytes
    }

    /// Hands `each` the rows it holds in the room, in order, each letting its room go as it is
    /// handed on; those written stay, to be read back.
    pub(super) fn hand_held(&self, each: &mut dyn FnMut(Vec<Value>) -> Result<()>) -> Result<()> {
        let (rows, bytes) = {
            let mut s = self.stored.borrow_mut();
            (std::mem::take(&mut s.rows), std::mem::take(&mut s.bytes))
        };
        let handed = (0..rows.len()).try_for_each(|i| each(rows.row(i)));
        drop(rows);
        if let Some(r) = &self.room {
            r.let_go(bytes);
        }
        handed
    }

    /// How many rows it holds in the room.
    #[cfg(test)]
    pub(super) fn held(&self) -> usize {
        self.stored.borrow().rows.len()
    }

    /// Reads as the last reader of the rows reads them, so the rows it holds may be written.
    pub(super) fn last_reader(&self) {
        self.last.set(true);
    }

    /// Writes the rows it holds and lets their room go, where the step reading them is the last and
    /// no reader is handing them on.
    pub(super) fn write_held(&self) -> Result<()> {
        if !self.last.get() || self.reading.get() > 0 || self.room.is_none() {
            return Ok(());
        }
        let (rows, bytes) = {
            let mut s = self.stored.borrow_mut();
            (std::mem::take(&mut s.rows), std::mem::take(&mut s.bytes))
        };
        let room = self.room.as_ref().expect("a run that writes has a room");
        room.let_go(bytes);
        {
            let mut s = self.stored.borrow_mut();
            let s = &mut *s;
            for i in 0..rows.len() {
                for v in &rows.row(i) {
                    encode(v, &mut s.named, &mut s.tail);
                }
                s.spilled += 1;
                while s.tail.len() >= PAGE {
                    let at = room.spill_page(&s.tail[..PAGE])?;
                    s.pages.push((at, PAGE as u16));
                    s.tail.drain(..PAGE);
                }
            }
        }
        self.seal()
    }

    /// Hands `each` its rows, in order, a chunk at a time: those held as copies, those written read
    /// back, one chunk in memory at a time.
    pub(super) fn each_chunk(
        &self,
        each: &mut dyn FnMut(Vec<Vec<Value>>) -> Result<()>,
    ) -> Result<()> {
        self.reading.set(self.reading.get() + 1);
        let done = self.chunks(each);
        self.reading.set(self.reading.get() - 1);
        done
    }

    fn chunks(&self, each: &mut dyn FnMut(Vec<Vec<Value>>) -> Result<()>) -> Result<()> {
        let held = self.stored.borrow().rows.len();
        let mut chunk = Chunk::new(each);
        for i in 0..held {
            let row = self.stored.borrow().rows.row(i);
            chunk.push(row)?;
        }
        let mut back = self.back();
        while let Some(row) = back.next_row()? {
            chunk.push(row)?;
        }
        chunk.finish()
    }

    /// Its written rows, read back a row at a time, a page in memory.
    pub(super) fn back(&self) -> Back {
        let s = self.stored.borrow();
        Back {
            room: self.room.clone(),
            width: self.width.get(),
            pages: s.pages.clone(),
            tail: s.tail.clone(),
            named: s.named.clone(),
            left: s.spilled,
            next: 0,
            bytes: Vec::new(),
            at: 0,
        }
    }

    /// Every row, in order: those held, then those written, read back.
    #[cfg(test)]
    pub(super) fn rows(&self) -> Result<Vec<Vec<Value>>> {
        let mut out = Vec::with_capacity(self.len());
        self.each_chunk(&mut |c| {
            out.extend(c);
            Ok(())
        })?;
        Ok(out)
    }

    /// Writes `rows`, a place every row of which is written, and where they begin: read back whole
    /// from there by `read_at`.
    pub(super) fn write_at(&self, rows: &[Vec<Value>]) -> Result<u64> {
        let at = {
            let s = self.stored.borrow();
            s.pages.len() as u64 * PAGE as u64 + s.tail.len() as u64
        };
        for r in rows {
            self.push(r.clone())?;
        }
        Ok(at)
    }

    /// The `n` rows `write_at` wrote from `at`.
    pub(super) fn read_at(&self, at: u64, n: u32) -> Result<Vec<Vec<Value>>> {
        let s = self.stored.borrow();
        let room = self.room.as_ref().expect("a run that writes has a room");
        let width = self.width.get();
        let mut bytes: Vec<u8> = Vec::new();
        let mut next = (at / PAGE as u64) as usize;
        let mut from = (at % PAGE as u64) as usize;
        let mut page = vec![0u8; PAGE];
        let mut out = Vec::with_capacity(n as usize);
        for _ in 0..n {
            let row = loop {
                let mut b = Bytes {
                    b: &bytes,
                    at: from,
                };
                match decode_row(&mut b, width, &s.named) {
                    Ok(row) => {
                        from = b.at;
                        break row;
                    }
                    Err(e) => {
                        if next < s.pages.len() {
                            room.spilled_page(s.pages[next].0, &mut page)?;
                            bytes.extend_from_slice(&page);
                        } else if next == s.pages.len() {
                            bytes.extend_from_slice(&s.tail);
                        } else {
                            return Err(e);
                        }
                        next += 1;
                    }
                }
            };
            out.push(row);
        }
        Ok(out)
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        if let Some(r) = &self.room {
            r.let_go(self.stored.borrow().bytes);
        }
    }
}

/// A run's written rows read back a row at a time, a page in memory.
pub(super) struct Back {
    room: Option<Rc<Room>>,
    width: usize,
    pages: Vec<(u64, u16)>,
    tail: Vec<u8>,
    named: Named,
    left: usize,
    next: usize,
    bytes: Vec<u8>,
    at: usize,
}

impl Back {
    /// The next row; none once every row has come.
    pub(super) fn next_row(&mut self) -> Result<Option<Vec<Value>>> {
        if self.left == 0 {
            return Ok(None);
        }
        self.left -= 1;
        loop {
            let mut b = Bytes {
                b: &self.bytes,
                at: self.at,
            };
            match decode_row(&mut b, self.width, &self.named) {
                Ok(row) => {
                    self.at = b.at;
                    return Ok(Some(row));
                }
                Err(e) => {
                    self.bytes.drain(..self.at);
                    self.at = 0;
                    if self.next < self.pages.len() {
                        let (p, valid) = self.pages[self.next];
                        let mut page = vec![0u8; PAGE];
                        self.room
                            .as_ref()
                            .expect("a run that writes has a room")
                            .spilled_page(p, &mut page)?;
                        self.bytes.extend_from_slice(&page[..valid as usize]);
                        self.next += 1;
                    } else if self.next == self.pages.len() && !self.tail.is_empty() {
                        self.bytes.extend_from_slice(&self.tail);
                        self.next += 1;
                    } else {
                        return Err(e);
                    }
                }
            }
        }
    }
}

/// How many parts rows past the room are cut into where how many there are is not known.
const PARTS_UNKNOWN: u64 = 16;

/// Rows past the room by a hash of their keys, every row of a key in one part. Where the rows to come
/// are within two rooms, one part: those past the room are written once and read back once. Past
/// that, or where they are not known, as many parts as each fits the room, so each row is written
/// once and read back once wherever its part fits.
pub(in crate::read) struct Parts {
    lo: u64,
    step: u64,
    runs: Vec<Run>,
    /// The page each part is filling.
    _filling: crate::room::Flying,
}

impl Parts {
    /// Parts of the hashes from `lo` to `hi`, the rows to come `expected` bytes in all where known.
    pub(in crate::read) fn new(lo: u64, hi: u64, expected: Option<u64>) -> Parts {
        let room = current().map_or(u64::MAX, |r| r.sizes().bytes).max(CHUNK);
        let most = (room / (2 * PAGE as u64)).max(8);
        let n = match expected {
            Some(b) if b <= room.saturating_mul(2) => 1,
            Some(b) => (b - room).div_ceil(room / 4 * 3).clamp(1, most),
            None => PARTS_UNKNOWN.min(most),
        };
        let step = ((hi - lo) / n).max(1);
        Parts {
            lo,
            step,
            runs: (0..n).map(|_| Run::past()).collect(),
            _filling: crate::room::flying(if n > 1 { n * PAGE as u64 } else { 0 }),
        }
    }

    pub(in crate::read) fn push(&self, hash: u64, row: Vec<Value>) -> Result<()> {
        let i = ((hash.saturating_sub(self.lo)) / self.step).min(self.runs.len() as u64 - 1);
        self.runs[i as usize].push(row)
    }

    /// Parts of the hashes from `lo` to `hi`, `n` of them.
    pub(in crate::read) fn of_count(lo: u64, hi: u64, n: u64) -> Parts {
        let n = n.max(1);
        Parts {
            lo,
            step: ((hi - lo) / n).max(1),
            runs: (0..n).map(|_| Run::past()).collect(),
            _filling: crate::room::flying(if n > 1 { n * PAGE as u64 } else { 0 }),
        }
    }

    /// Parts of none of its rows, cut on the hashes as these are.
    pub(in crate::read) fn like(&self) -> Parts {
        let n = self.runs.len() as u64;
        Parts {
            lo: self.lo,
            step: self.step,
            runs: (0..n).map(|_| Run::past()).collect(),
            _filling: crate::room::flying(if n > 1 { n * PAGE as u64 } else { 0 }),
        }
    }

    /// Each part written, with the hashes it begins and ends at, in the order of their hashes, the
    /// empty among them: the last goes up to `hi`.
    pub(in crate::read) fn into_parts(self, hi: u64) -> Result<Vec<(u64, u64, Run)>> {
        let n = self.runs.len() as u64;
        let mut parts = Vec::with_capacity(self.runs.len());
        for (i, run) in self.runs.into_iter().enumerate() {
            run.seal()?;
            let lo = self.lo + i as u64 * self.step;
            let top = match i as u64 + 1 == n {
                true => hi,
                false => lo + self.step - 1,
            };
            parts.push((lo, top, run));
        }
        Ok(parts)
    }

    /// Its parts written, on the front of `queue` in the order of their hashes: the last goes up to
    /// `hi`.
    pub(in crate::read) fn into_queue(self, hi: u64, queue: &mut Queue) -> Result<()> {
        let n = self.runs.len() as u64;
        let mut parts = Vec::with_capacity(self.runs.len());
        for (i, run) in self.runs.into_iter().enumerate() {
            run.seal()?;
            let lo = self.lo + i as u64 * self.step;
            let top = match i as u64 + 1 == n {
                true => hi,
                false => lo + self.step - 1,
            };
            if run.len() > 0 {
                parts.push((lo, top, run));
            }
        }
        for p in parts.into_iter().rev() {
            queue.0.push_front(p);
        }
        Ok(())
    }
}

/// Parts of rows past the room still to come, in the order of their hashes.
#[derive(Default)]
pub(in crate::read) struct Queue(std::collections::VecDeque<(u64, u64, Run)>);

impl Queue {
    pub(in crate::read) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The hashes the first part begins and ends at.
    pub(in crate::read) fn front(&self) -> Option<(u64, u64)> {
        self.0.front().map(|p| (p.0, p.1))
    }

    /// The next parts to read together: the first, and those after it while together they fit
    /// `bytes`: their hashes' first and last, the bytes they hold, and each part.
    pub(in crate::read) fn next(&mut self, bytes: u64) -> Option<(u64, u64, u64, Vec<Run>)> {
        let (lo, mut hi, first) = self.0.pop_front()?;
        let mut total = first.total();
        let mut runs = vec![first];
        while self.0.front().is_some_and(|p| total + p.2.total() <= bytes) {
            let (_, top, run) = self.0.pop_front().expect("a part is there");
            total += run.total();
            hi = top;
            runs.push(run);
        }
        Some((lo, hi, total, runs))
    }
}

/// Rows gathered into chunks and handed on as each fills, what one holds counted while it is handed.
pub(super) struct Chunk<'e> {
    rows: Vec<Vec<Value>>,
    /// The bytes its rows take on the spill, and as they are held.
    bytes: u64,
    held: u64,
    each: &'e mut dyn FnMut(Vec<Vec<Value>>) -> Result<()>,
}

impl<'e> Chunk<'e> {
    pub(super) fn new(each: &'e mut dyn FnMut(Vec<Vec<Value>>) -> Result<()>) -> Chunk<'e> {
        Chunk {
            rows: Vec::new(),
            bytes: 0,
            held: 0,
            each,
        }
    }

    pub(super) fn push(&mut self, row: Vec<Value>) -> Result<()> {
        self.bytes += row_file_width(&row);
        self.held += row_width(&row);
        self.rows.push(row);
        if self.bytes >= CHUNK {
            self.hand()?;
        }
        Ok(())
    }

    fn hand(&mut self) -> Result<()> {
        let _flying = crate::room::flying(std::mem::take(&mut self.held));
        self.bytes = 0;
        (self.each)(std::mem::take(&mut self.rows))
    }

    /// Hands on the rows gathered.
    pub(super) fn finish(mut self) -> Result<()> {
        if self.rows.is_empty() {
            return Ok(());
        }
        self.hand()
    }
}

/// A place's rows as a chain of runs one after another: arms a UNION ALL appends, or a recursion's
/// runs, none copied.
#[derive(Clone, Default)]
pub(super) struct Rows(Vec<Rc<Run>>);

/// A place's rows handed out a chunk at a time to a caller asking for them, once they are all made.
pub(super) struct Handed {
    runs: std::collections::VecDeque<Rc<Run>>,
    at: usize,
    back: Option<Back>,
}

impl Handed {
    /// The next chunk of its rows, in order; none once every row has come.
    pub(super) fn next_chunk(&mut self) -> Result<Option<Vec<Vec<Value>>>> {
        let (mut out, mut bytes) = (Vec::new(), 0);
        while bytes < CHUNK {
            let Some(run) = self.runs.front() else {
                break;
            };
            let held = {
                let s = run.stored.borrow();
                (self.at < s.rows.len()).then(|| s.rows.row(self.at))
            };
            let row = match held {
                Some(r) => {
                    self.at += 1;
                    Some(r)
                }
                None => self.back.get_or_insert_with(|| run.back()).next_row()?,
            };
            match row {
                Some(r) => {
                    bytes += row_file_width(&r);
                    out.push(r);
                }
                None => {
                    self.runs.pop_front();
                    self.at = 0;
                    self.back = None;
                }
            }
        }
        Ok((!out.is_empty()).then_some(out))
    }
}

impl Rows {
    pub(super) fn one(run: Run) -> Rows {
        Rows(vec![Rc::new(run)])
    }

    pub(super) fn of(rows: Vec<Vec<Value>>) -> Result<Rows> {
        Ok(Rows::one(Run::of(rows)?))
    }

    /// Its rows, to hand out a chunk at a time.
    pub(super) fn handed(self) -> Handed {
        Handed {
            runs: self.0.into(),
            at: 0,
            back: None,
        }
    }

    /// Those of `other` after these.
    pub(super) fn then(mut self, other: Rows) -> Rows {
        self.0.extend(other.0);
        self
    }

    pub(super) fn len(&self) -> usize {
        self.0.iter().map(|r| r.len()).sum()
    }

    /// The bytes its rows are held at, those written among them.
    pub(super) fn bytes(&self) -> u64 {
        self.0.iter().map(|r| r.stored.borrow().total).sum()
    }

    /// Whether every row is held in the room.
    pub(super) fn held_whole(&self) -> bool {
        self.0.iter().all(|r| r.spilled() == 0)
    }

    /// Reads as the last reader of the rows reads them.
    pub(super) fn last_reader(&self) {
        self.0.iter().for_each(|r| r.last_reader());
    }

    /// Writes the rows held and lets their room go, where the step reading them is the last.
    pub(super) fn write_held(&self) -> Result<()> {
        for r in &self.0 {
            r.write_held()?;
        }
        Ok(())
    }

    pub(super) fn each_chunk(
        &self,
        each: &mut dyn FnMut(Vec<Vec<Value>>) -> Result<()>,
    ) -> Result<()> {
        for r in &self.0 {
            r.each_chunk(each)?;
        }
        Ok(())
    }

    /// Its first `n` rows.
    pub(super) fn first(&self, n: usize) -> Result<Vec<Vec<Value>>> {
        let mut out = Vec::with_capacity(n);
        self.each_chunk(&mut |c| {
            let left = n - out.len();
            out.extend(c.into_iter().take(left));
            Ok(())
        })?;
        Ok(out)
    }

    /// Every row, in order.
    pub(super) fn all(&self) -> Result<Vec<Vec<Value>>> {
        let mut out = Vec::with_capacity(self.len());
        self.each_chunk(&mut |c| {
            out.extend(c);
            Ok(())
        })?;
        Ok(out)
    }

    /// Calls `f` with the rows held, each run's in turn, where every row is held.
    pub(super) fn with_held<T>(&self, f: impl FnOnce(&HeldRows) -> T) -> T {
        let runs: Vec<_> = self.0.iter().map(|r| r.stored.borrow()).collect();
        let mut starts = Vec::with_capacity(runs.len());
        let mut len = 0;
        for r in &runs {
            starts.push(len);
            len += r.rows.len();
        }
        f(&HeldRows { runs, starts, len })
    }
}

/// The rows a place holds in the room, found by their place among them.
pub(super) struct HeldRows<'a> {
    runs: Vec<std::cell::Ref<'a, Stored>>,
    starts: Vec<usize>,
    len: usize,
}

impl RowsAt for HeldRows<'_> {
    fn count(&self) -> usize {
        self.len
    }

    fn row(&self, at: usize) -> Vec<Value> {
        let r = self.starts.partition_point(|&s| s <= at) - 1;
        self.runs[r].rows.row(at - self.starts[r])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::room::{entered, Sizes};

    /// Row `i`: an integer and the text "row i", its bytes held exactly.
    fn row(i: i64) -> Vec<Value> {
        vec![Value::Int(i), Value::Text(["row ", &i.to_string()].concat())]
    }

    #[test]
    fn rows_past_the_room_are_written_once_and_read_back_in_order() {
        // A row of an integer and the text "row i", i under 10, takes 9 + 10 = 19 bytes on the
        // spill; held column-wise, the integer's 8, the text's 5 and its end's 4, 17, the first row
        // 16 more for each column's word of NULL bits. Of 100 rows, 1,000 bytes of room hold
        // 33 + 56 * 17 = 985 bytes, 57 rows; the other 43 are written, 817 bytes, one page.
        let room = Room::new(Sizes::of(1000));
        let _in = entered(&room);
        let all: Vec<Vec<Value>> = (0..10).map(row).collect();
        assert_eq!(all.iter().map(|r| row_file_width(r)).sum::<u64>(), 190);
        let many: Vec<Vec<Value>> = (0..100).map(|i| row(i % 10)).collect();
        let run = Run::of(many.clone()).unwrap();
        assert_eq!((run.held(), run.spilled()), (57, 43));
        assert_eq!(room.spill_pages(), (1, 0));
        assert_eq!(room.most(), 985);
        let shown = |r: &[Vec<Value>]| format!("{r:?}");
        assert_eq!(shown(&run.rows().unwrap()), shown(&many));
        assert_eq!(room.spill_pages(), (1, 1));
        drop(run);
        assert!(room.hold(1000), "the room is let go with the place");
    }

    #[test]
    fn every_kind_of_value_comes_back_as_it_went() {
        let room = Room::new(Sizes::of(0));
        let _in = entered(&room);
        let row = vec![
            Value::Null,
            Value::Int(-5),
            Value::Num(-12345, 2),
            Value::Text("héllo".into()),
            Value::Bytes(vec![0, 255]),
            Value::Bool(true),
            Value::Uuid(-7),
            Value::Real(1.5),
            Value::Double(-0.0),
            Value::Interval(crate::times::Interval {
                months: 1,
                days: -2,
                micros: 3,
            }),
            Value::Numeric(crate::numeric::Numeric::parse("NaN").unwrap()),
        ];
        let run = Run::of(vec![row.clone(), row.clone()]).unwrap();
        assert_eq!(run.spilled(), 2);
        let back = run.rows().unwrap();
        assert_eq!(back.len(), 2);
        for r in back {
            for (a, b) in r.iter().zip(&row) {
                assert_eq!(format!("{a:?}"), format!("{b:?}"));
            }
        }
    }
}
