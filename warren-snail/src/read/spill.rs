//! A place's rows in the round's room: held while the room has room, and past it written to the
//! round's spill once, a page at a time, and read back where the place is read. A value's held width
//! is its bytes as the spill writes it: a byte naming its kind, then the value.

use super::pipeline::Value;
use crate::catalog::ColumnType;
use crate::cut::PAGE;
use crate::geo::GeoKind;
use crate::room::{current, Room};
use crate::{Error, Result};
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

/// A place's rows: those the room holds, in order, then those past it, on the round's spill.
pub(super) struct Run {
    room: Option<Rc<Room>>,
    width: usize,
    rows: Vec<Vec<Value>>,
    bytes: u64,
    named: Named,
    pages: Vec<u64>,
    tail: Vec<u8>,
    spilled: usize,
}

impl Run {
    /// An empty place of rows `width` values wide, in the room of the round under way.
    pub(super) fn new(width: usize) -> Run {
        Run {
            room: current(),
            width,
            rows: Vec::new(),
            bytes: 0,
            named: Named::default(),
            pages: Vec::new(),
            tail: Vec::new(),
            spilled: 0,
        }
    }

    /// The rows `rows` held as a place: as many as the room has room for, the rest written.
    pub(super) fn of(width: usize, rows: Vec<Vec<Value>>) -> Result<Run> {
        let mut run = Run::new(width);
        for r in rows {
            run.push(r)?;
        }
        run.seal()?;
        Ok(run)
    }

    /// One row more: held where the room has room for it and no row before it was written, so the
    /// rows keep their order; written otherwise.
    pub(super) fn push(&mut self, row: Vec<Value>) -> Result<()> {
        let w = row_width(&row);
        let room = match &self.room {
            Some(r) => r,
            None => {
                self.rows.push(row);
                return Ok(());
            }
        };
        if self.spilled == 0 && room.hold(w) {
            self.bytes += w;
            self.rows.push(row);
            return Ok(());
        }
        for v in &row {
            encode(v, &mut self.named, &mut self.tail);
        }
        self.spilled += 1;
        while self.tail.len() >= PAGE {
            let at = room.spill_page(&self.tail[..PAGE])?;
            self.pages.push(at);
            self.tail.drain(..PAGE);
        }
        Ok(())
    }

    /// Writes the rows past the room not yet written, the last page padded.
    pub(super) fn seal(&mut self) -> Result<()> {
        if self.tail.is_empty() {
            return Ok(());
        }
        let room = self.room.as_ref().expect("a run that writes has a room");
        self.tail.resize(PAGE, 0);
        let at = room.spill_page(&self.tail)?;
        self.pages.push(at);
        self.tail.clear();
        Ok(())
    }

    pub(super) fn len(&self) -> usize {
        self.rows.len() + self.spilled
    }

    /// The rows written past the room.
    pub(super) fn spilled(&self) -> usize {
        self.spilled
    }

    /// The rows held in the room.
    pub(super) fn held(&self) -> &[Vec<Value>] {
        &self.rows
    }

    /// The rows written past the room, read back.
    pub(super) fn read_back(&self) -> Result<Vec<Vec<Value>>> {
        if self.spilled == 0 {
            return Ok(Vec::new());
        }
        let room = self.room.as_ref().expect("a run that writes has a room");
        let mut all = Vec::with_capacity(self.pages.len() * PAGE + self.tail.len());
        let mut page = vec![0u8; PAGE];
        for &at in &self.pages {
            room.spilled_page(at, &mut page)?;
            all.extend_from_slice(&page);
        }
        all.extend_from_slice(&self.tail);
        let mut b = Bytes { b: &all, at: 0 };
        let mut out = Vec::with_capacity(self.spilled);
        for _ in 0..self.spilled {
            let mut row = Vec::with_capacity(self.width);
            for _ in 0..self.width {
                row.push(decode(&mut b, &self.named)?);
            }
            out.push(row);
        }
        Ok(out)
    }

    /// Every row, in order: those held, then those written, read back.
    pub(super) fn rows(&self) -> Result<Vec<Vec<Value>>> {
        let mut out = self.rows.clone();
        out.extend(self.read_back()?);
        Ok(out)
    }

    /// Every row, in order, the room they held let go.
    pub(super) fn into_rows(mut self) -> Result<Vec<Vec<Value>>> {
        let back = self.read_back()?;
        let mut out = std::mem::take(&mut self.rows);
        out.extend(back);
        Ok(out)
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        if let Some(r) = &self.room {
            r.let_go(self.bytes);
        }
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
        let run = Run::of(2, many.clone()).unwrap();
        assert_eq!((run.held().len(), run.spilled()), (52, 48));
        assert_eq!(room.spill_pages(), (1, 0));
        let shown = |r: &[Vec<Value>]| format!("{r:?}");
        assert_eq!(shown(&run.rows().unwrap()), shown(&many));
        assert_eq!(room.spill_pages(), (1, 1));
        assert_eq!(room.most(), 52 * 19);
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
        let run = Run::of(row.len(), vec![row.clone(), row.clone()]).unwrap();
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
