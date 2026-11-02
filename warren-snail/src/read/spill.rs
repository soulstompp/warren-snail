//! A place's rows in the round's room: held while the room has room, and past it written to the
//! round's spill once, a page at a time, and read back where the place is read. A value's held width
//! is its bytes as the spill writes it: a byte naming its kind, then the value.

use super::pipeline::Value;
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

/// The bytes a value is held at: as the spill writes it.
pub(super) fn width(v: &Value) -> u64 {
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

/// The bytes a row is held at.
pub(super) fn row_width(row: &[Value]) -> u64 {
    row.iter().map(width).sum()
}

/// The bytes rows are held at.
pub(super) fn rows_width(rows: &[Vec<Value>]) -> u64 {
    rows.iter().map(|r| row_width(r)).sum()
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

/// A place's rows as they lie: those the room holds, then those written.
#[derive(Default)]
struct Stored {
    rows: Vec<Vec<Value>>,
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
        let w = row_width(&row);
        let mut s = self.stored.borrow_mut();
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

    /// The rows written past the room.
    pub(super) fn spilled(&self) -> usize {
        self.stored.borrow().spilled
    }

    /// The rows held in the room.
    #[cfg(test)]
    pub(super) fn held(&self) -> std::cell::Ref<'_, Vec<Vec<Value>>> {
        std::cell::Ref::map(self.stored.borrow(), |s| &s.rows)
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
            for row in &rows {
                for v in row {
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
            let row = self.stored.borrow().rows[i].clone();
            chunk.push(row)?;
        }
        let (pages, tail, named, spilled) = {
            let s = self.stored.borrow();
            (s.pages.clone(), s.tail.clone(), s.named.clone(), s.spilled)
        };
        if spilled == 0 {
            return chunk.finish();
        }
        let room = self.room.as_ref().expect("a run that writes has a room");
        let width = self.width.get();
        let mut bytes: Vec<u8> = Vec::new();
        let mut at = 0;
        let mut next = 0;
        let mut page = vec![0u8; PAGE];
        for _ in 0..spilled {
            let row = loop {
                let mut b = Bytes { b: &bytes, at };
                match decode_row(&mut b, width, &named) {
                    Ok(row) => {
                        at = b.at;
                        break row;
                    }
                    Err(e) => {
                        bytes.drain(..at);
                        at = 0;
                        if next < pages.len() {
                            let (p, valid) = pages[next];
                            room.spilled_page(p, &mut page)?;
                            bytes.extend_from_slice(&page[..valid as usize]);
                            next += 1;
                        } else if next == pages.len() && !tail.is_empty() {
                            bytes.extend_from_slice(&tail);
                            next += 1;
                        } else {
                            return Err(e);
                        }
                    }
                }
            };
            chunk.push(row)?;
        }
        chunk.finish()
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
                let mut b = Bytes { b: &bytes, at: from };
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

/// Rows gathered into chunks and handed on as each fills, what one holds counted while it is handed.
pub(super) struct Chunk<'e> {
    rows: Vec<Vec<Value>>,
    bytes: u64,
    each: &'e mut dyn FnMut(Vec<Vec<Value>>) -> Result<()>,
}

impl<'e> Chunk<'e> {
    pub(super) fn new(each: &'e mut dyn FnMut(Vec<Vec<Value>>) -> Result<()>) -> Chunk<'e> {
        Chunk {
            rows: Vec::new(),
            bytes: 0,
            each,
        }
    }

    pub(super) fn push(&mut self, row: Vec<Value>) -> Result<()> {
        self.bytes += row_width(&row);
        self.rows.push(row);
        if self.bytes >= CHUNK {
            self.hand()?;
        }
        Ok(())
    }

    fn hand(&mut self) -> Result<()> {
        let _flying = crate::room::flying(self.bytes);
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

impl Rows {
    pub(super) fn one(run: Run) -> Rows {
        Rows(vec![Rc::new(run)])
    }

    pub(super) fn of(rows: Vec<Vec<Value>>) -> Result<Rows> {
        Ok(Rows::one(Run::of(rows)?))
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
    pub(super) fn with_held<T>(&self, f: impl FnOnce(&[&Vec<Value>]) -> T) -> T {
        let guards: Vec<_> = self.0.iter().map(|r| r.stored.borrow()).collect();
        let rows: Vec<&Vec<Value>> = guards.iter().flat_map(|g| g.rows.iter()).collect();
        f(&rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::room::{entered, Sizes};

    fn rows(n: i64) -> Vec<Vec<Value>> {
        (0..n)
            .map(|i| vec![Value::Int(i), Value::Text(format!("row {i}"))])
            .collect()
    }

    #[test]
    fn rows_past_the_room_are_written_once_and_read_back_in_order() {
        // A row of an integer and the text "row i", i under 10, is 9 + 10 = 19 bytes: of 100 rows,
        // 1,000 bytes of room hold 52, the other 48 are 912 bytes, one page.
        let room = Room::new(Sizes::of(1000));
        let _in = entered(&room);
        let all = rows(10);
        let w: u64 = all.iter().map(|r| row_width(r)).sum();
        assert_eq!(w, 190);
        let mut many = Vec::new();
        for i in 0..100 {
            many.push(vec![Value::Int(i % 10), Value::Text(format!("row {}", i % 10))]);
        }
        let run = Run::of(many.clone()).unwrap();
        assert_eq!((run.held().len(), run.spilled()), (52, 48));
        assert_eq!(room.spill_pages(), (1, 0));
        assert_eq!(room.most(), 52 * 19);
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
