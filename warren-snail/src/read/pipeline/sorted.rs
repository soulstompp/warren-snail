//! A step's rows sorted as its ORDER BY says, in the round's room. Past it, within two rooms, the rows
//! that sort last are set aside, a row at a time, written where the room has no room for them, so
//! that every row set aside sorts after every row held: those held are handed on sorted, then those
//! set aside. Past two rooms, runs of a room each, merged. And what a LIMIT keeps of rows as they
//! come in their order.

use super::{compare, computed_none, tie, Casing, Casings, Frame, Limit, Prepared, Value};
use crate::catalog::ColumnType;
use crate::cut::PAGE;
use crate::read::spill::{row_file_width, Back, Packer, Run, CHUNK};
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
    /// Two values of one place compared in its direction, its NULLs first or last.
    fn one(&self, a: &Value, b: &Value, desc: bool, nulls_first: bool) -> Ordering {
        match (a.is_null(), b.is_null()) {
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
            (false, false) => match sort_compare(a, b) {
                Ok(Some(o)) => directed(o, desc),
                Ok(None) => Ordering::Equal,
                Err(e) => {
                    self.failed.borrow_mut().get_or_insert(e);
                    Ordering::Equal
                }
            },
        }
    }
}

/// What a sort's held rows are compared and made whole by: its ORDER BY, the packer of their
/// values, and the types of the scalars and strings among their keys, each named by its place.
struct Ctx {
    spec: Spec,
    packer: RefCell<Packer>,
    types: RefCell<Vec<ColumnType>>,
}

/// A row held: one string of bytes, the values it sorts by, each a tag and its bytes, then the row
/// packed (none where only its values are compared); the place it came; what it is read by.
struct Held {
    data: Box<[u8]>,
    seq: u64,
    ctx: Rc<Ctx>,
}

// The tags of a key's values: those of one tag (a scalar of one type, a numeric of one scale)
// compared by their bytes, any others made whole and compared as values.
const NULL: u8 = 0;
const INT: u8 = 1;
const REAL: u8 = 2;
const DOUBLE: u8 = 3;
const BOOL: u8 = 4;
const UUID: u8 = 5;
const TEXT: u8 = 6;
const BYTES: u8 = 7;
const SCALAR: u8 = 8;
const NUM: u8 = 9;
const STR: u8 = 10;
const PACKED: u8 = 11;

fn put_len(out: &mut Vec<u8>, mut n: usize) {
    while n >= 0x80 {
        out.push((n as u8) | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
}

fn get_len(b: &[u8], at: &mut usize) -> usize {
    let (mut n, mut shift) = (0usize, 0);
    loop {
        let byte = b[*at];
        *at += 1;
        n |= ((byte & 0x7f) as usize) << shift;
        if byte < 0x80 {
            return n;
        }
        shift += 7;
    }
}

impl Ctx {
    /// The place of a type among those its keys name.
    fn type_at(&self, ty: ColumnType) -> usize {
        let mut types = self.types.borrow_mut();
        match types.iter().position(|t| *t == ty) {
            Some(i) => i,
            None => {
                types.push(ty);
                types.len() - 1
            }
        }
    }

    /// A key's values as held, after `out`'s bytes.
    fn encode(&self, key: &[Value], out: &mut Vec<u8>) {
        let flip64 = |x: i64| ((x as u64) ^ (1 << 63)).to_be_bytes();
        for v in key {
            match v {
                Value::Null => out.push(NULL),
                Value::Int(x) => {
                    out.push(INT);
                    out.extend(flip64(*x));
                }
                Value::Real(f) => {
                    out.push(REAL);
                    out.extend(flip64(crate::scalars::float4_key(f.to_bits())));
                    out.extend(f.to_bits().to_be_bytes());
                }
                Value::Double(f) => {
                    out.push(DOUBLE);
                    out.extend(flip64(crate::scalars::float8_key(f.to_bits())));
                    out.extend(f.to_bits().to_be_bytes());
                }
                Value::Bool(b) => {
                    out.push(BOOL);
                    out.push(u8::from(*b));
                }
                Value::Uuid(u) => {
                    out.push(UUID);
                    out.extend(((*u as u128) ^ (1 << 127)).to_be_bytes());
                }
                Value::Text(s) => {
                    out.push(TEXT);
                    put_len(out, s.len());
                    out.extend(s.as_bytes());
                }
                Value::Bytes(b) => {
                    out.push(BYTES);
                    put_len(out, b.len());
                    out.extend(b);
                }
                Value::Scalar(ty, x) => {
                    out.push(SCALAR);
                    put_len(out, self.type_at(*ty));
                    out.extend(flip64(*x));
                }
                Value::Num(m, s) => {
                    out.push(NUM);
                    put_len(out, *s as usize);
                    out.extend(((*m as u128) ^ (1 << 127)).to_be_bytes());
                }
                Value::Str(ty, s) => {
                    out.push(STR);
                    put_len(out, self.type_at(*ty));
                    put_len(out, s.len());
                    out.extend(s.as_bytes());
                }
                v => {
                    out.push(PACKED);
                    let p = self.packer.borrow_mut().pack(std::slice::from_ref(v));
                    put_len(out, p.len());
                    out.extend(p.iter());
                }
            }
        }
    }

    /// The next value of a key as held, from `at`: its tag, the type or scale it names, and its
    /// bytes.
    fn part<'a>(b: &'a [u8], at: &mut usize) -> (u8, usize, &'a [u8]) {
        let tag = b[*at];
        *at += 1;
        let aux = match tag {
            SCALAR | NUM | STR => get_len(b, at),
            _ => 0,
        };
        let n = match tag {
            NULL => 0,
            INT | SCALAR => 8,
            REAL => 12,
            DOUBLE => 16,
            BOOL => 1,
            UUID | NUM => 16,
            _ => get_len(b, at),
        };
        let s = &b[*at..*at + n];
        *at += n;
        (tag, aux, s)
    }

    /// A key's value made whole from its tag and bytes.
    fn value(&self, tag: u8, aux: usize, s: &[u8]) -> Result<Value> {
        let int = |s: &[u8]| (u64::from_be_bytes(s[..8].try_into().unwrap()) ^ (1 << 63)) as i64;
        let wide = |s: &[u8]| (u128::from_be_bytes(s[..16].try_into().unwrap()) ^ (1 << 127)) as i128;
        let text = |s: &[u8]| String::from_utf8(s.to_vec()).map_err(|e| Error::new(e.to_string()));
        Ok(match tag {
            NULL => Value::Null,
            INT => Value::Int(int(s)),
            REAL => Value::Real(f32::from_bits(u32::from_be_bytes(s[8..12].try_into().unwrap()))),
            DOUBLE => Value::Double(f64::from_bits(u64::from_be_bytes(s[8..16].try_into().unwrap()))),
            BOOL => Value::Bool(s[0] != 0),
            UUID => Value::Uuid(wide(s)),
            TEXT => Value::Text(text(s)?),
            BYTES => Value::Bytes(s.to_vec()),
            SCALAR => Value::Scalar(self.types.borrow()[aux], int(s)),
            NUM => Value::Num(wide(s), aux as u32),
            STR => Value::Str(self.types.borrow()[aux], text(s)?),
            _ => {
                let mut row = self.packer.borrow().unpack(s)?;
                row.pop().ok_or_else(|| Error::new("a key's value packed as none"))?
            }
        })
    }

    /// A key's values made whole.
    fn values(&self, key: &[u8]) -> Result<Vec<Value>> {
        let (mut at, mut out) = (0, Vec::with_capacity(self.spec.dirs.len()));
        while at < key.len() {
            let (tag, aux, s) = Ctx::part(key, &mut at);
            out.push(self.value(tag, aux, s)?);
        }
        Ok(out)
    }

    /// Two keys as held compared as the ORDER BY says: values of one tag by their bytes, any
    /// others made whole and compared as values.
    fn compare(&self, a: &[u8], b: &[u8]) -> Ordering {
        let (mut i, mut j) = (0, 0);
        for &(desc, nulls_first) in &self.spec.dirs {
            let (ta, xa, sa) = Ctx::part(a, &mut i);
            let (tb, xb, sb) = Ctx::part(b, &mut j);
            let ord = match (ta, tb) {
                // A composed type's NULL is packed: either side packed is made whole first.
                (PACKED, _) | (_, PACKED) => match (self.value(ta, xa, sa), self.value(tb, xb, sb)) {
                    (Ok(x), Ok(y)) => self.spec.one(&x, &y, desc, nulls_first),
                    (Err(e), _) | (_, Err(e)) => {
                        self.spec.failed.borrow_mut().get_or_insert(e);
                        Ordering::Equal
                    }
                },
                (NULL, NULL) => Ordering::Equal,
                (NULL, _) => {
                    if nulls_first {
                        Ordering::Less
                    } else {
                        Ordering::Greater
                    }
                }
                (_, NULL) => {
                    if nulls_first {
                        Ordering::Greater
                    } else {
                        Ordering::Less
                    }
                }
                (REAL, REAL) | (DOUBLE, DOUBLE) => directed(sa[..8].cmp(&sb[..8]), desc),
                (STR, STR) if xa == xb => {
                    fn text(ty: ColumnType, s: &[u8]) -> &[u8] {
                        crate::strings::text_of(ty, std::str::from_utf8(s).unwrap_or("")).as_bytes()
                    }
                    let ty = self.types.borrow()[xa];
                    directed(text(ty, sa).cmp(text(ty, sb)), desc)
                }
                (SCALAR, SCALAR) | (NUM, NUM) if xa == xb => directed(sa.cmp(sb), desc),
                (INT, INT) | (BOOL, BOOL) | (UUID, UUID) | (TEXT, TEXT) | (BYTES, BYTES) => {
                    directed(sa.cmp(sb), desc)
                }
                _ => match (self.value(ta, xa, sa), self.value(tb, xb, sb)) {
                    (Ok(x), Ok(y)) => self.spec.one(&x, &y, desc, nulls_first),
                    (Err(e), _) | (_, Err(e)) => {
                        self.spec.failed.borrow_mut().get_or_insert(e);
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

fn directed(o: Ordering, desc: bool) -> Ordering {
    if desc {
        o.reverse()
    } else {
        o
    }
}

/// The bytes a sort holds a row at whose values to sort by take `key` bytes and the row `row` as the
/// spill writes them.
pub(in crate::read) fn held_bytes(key: u64, row: u64) -> u64 {
    std::mem::size_of::<Held>() as u64 + 1 + key + row
}

impl Held {
    /// A row held by its values `key`, with `row` packed after them where it is given.
    fn of(ctx: &Rc<Ctx>, key: &[Value], row: Option<&[Value]>, seq: u64) -> Held {
        let mut k = Vec::new();
        ctx.encode(key, &mut k);
        let mut data = Vec::with_capacity(
            k.len() + 4 + row.map_or(0, |r| row_file_width(r) as usize),
        );
        put_len(&mut data, k.len());
        data.extend(k);
        if let Some(r) = row {
            data.extend(ctx.packer.borrow_mut().pack(r).iter());
        }
        Held {
            data: data.into_boxed_slice(),
            seq,
            ctx: ctx.clone(),
        }
    }

    /// Its values with `row` packed after them.
    fn with_row(self, row: &[Value]) -> Held {
        let packed = self.ctx.packer.borrow_mut().pack(row);
        let mut data = Vec::with_capacity(self.data.len() + packed.len());
        data.extend(self.data.iter());
        data.extend(packed.iter());
        Held {
            data: data.into_boxed_slice(),
            ..self
        }
    }

    /// Its values alone, its row let go.
    fn key_only(self) -> Held {
        let (_, to) = self.key_at();
        Held {
            data: self.data[..to].into(),
            ..self
        }
    }

    /// Where its key's bytes lie in `data`.
    fn key_at(&self) -> (usize, usize) {
        let mut at = 0;
        let n = get_len(&self.data, &mut at);
        (at, at + n)
    }

    fn key(&self) -> &[u8] {
        let (from, to) = self.key_at();
        &self.data[from..to]
    }

    /// The values it is sorted by.
    fn key_values(&self) -> Result<Vec<Value>> {
        self.ctx.values(self.key())
    }

    /// The row made whole.
    fn row(&self) -> Result<Vec<Value>> {
        let (_, to) = self.key_at();
        self.ctx.packer.borrow().unpack(&self.data[to..])
    }

    /// The bytes it holds.
    fn bytes(&self) -> u64 {
        (std::mem::size_of::<Held>() + self.data.len()) as u64
    }
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
        self.ctx
            .compare(self.key(), other.key())
            .then(self.seq.cmp(&other.seq))
    }
}

/// Two values as a sort compares them: two row constructors' records field by field as far as
/// their fields are alike, refused where that reaches a field of a type PostgreSQL compares by no
/// function.
fn sort_compare(a: &Value, b: &Value) -> Result<Option<Ordering>> {
    if let (Value::Composed(ty, x), Value::Composed(_, y)) = (a, b) {
        if super::anonymous_record(*ty) {
            return match crate::composed::compare_reaches(*ty, &x.item, &y.item) {
                Some(part) => Err(super::no_comparison(part)),
                None => Ok(Some(x.key.cmp(&y.key))),
            };
        }
    }
    compare(a, b)
}

/// The values a row is sorted by, refused where its type has no ordering; a row constructor's
/// record is compared as two rows meet.
pub(super) fn sort_key(
    row: &[Value],
    sorted: &[usize],
    under: &[Option<Casing>],
) -> Result<Vec<Value>> {
    // As many values as it sorts by, and no room for more: each row held keeps its key.
    let mut key = Vec::with_capacity(sorted.len());
    for (i, &at) in sorted.iter().enumerate() {
        let v = row[at].clone();
        key.push(match v {
            Value::Scalar(ty, _) | Value::Composed(ty, _) | Value::Typed(ty, _)
                if !ty.ordered() && !super::anonymous_record(ty) =>
            {
                return Err(super::unordered(&ty.pg_name()))
            }
            Value::Geometric(g) => return Err(super::unordered(g.kind().name())),
            v => match (under[i], v.string_key()) {
                (Some(Casing::Library(id)), Some(s)) => {
                    Value::Bytes(crate::collations::get(id).key(s))
                }
                _ => v,
            },
        });
    }
    Ok(key)
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
    ctx: Rc<Ctx>,
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
            ctx: Rc::new(Ctx {
                spec: Spec {
                    dirs,
                    failed: RefCell::new(None),
                },
                packer: RefCell::new(Packer::default()),
                types: RefCell::new(Vec::new()),
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
        match self.ctx.spec.failed.borrow_mut().take() {
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
            run.push(h.row()?)?;
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
        // Its values alone while it is compared; held, the row packed after them.
        let item = Held::of(&self.ctx, &key, None, seq);
        let bytes = item.bytes() + row_file_width(&row);
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
                    return past.came.push(row);
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
                        let out = heap.pop().expect("a row is held");
                        self.bytes -= out.bytes();
                        room.let_go(out.bytes());
                        let past = self.past.get_or_insert_with(Past::new);
                        past.let_go.push(out.row()?)?;
                        past.least = Some(out.key_only());
                    }
                    _ => {
                        let past = self.past.get_or_insert_with(Past::new);
                        past.came.push(row)?;
                        if !after {
                            past.least = Some(item);
                        }
                        return Ok(());
                    }
                }
            }
        }
        let item = item.with_row(&row);
        debug_assert_eq!(item.bytes(), bytes, "a row held at its counted bytes");
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
            let mut bytes = std::mem::take(&mut self.bytes);
            // Handed on a chunk at a time, each chunk's room let go once it is handed.
            let (mut keys, mut out, mut file, mut handed) = (Vec::new(), Vec::new(), 0, 0);
            let none = rows.is_empty();
            for h in rows {
                let row = h.row()?;
                file += row_file_width(&row);
                handed += h.bytes();
                keys.push(h.key_values()?);
                out.push(row);
                if file >= CHUNK {
                    each(std::mem::take(&mut out), std::mem::take(&mut keys))?;
                    if let Some(r) = &self.room {
                        r.let_go(handed);
                    }
                    bytes -= handed;
                    (file, handed) = (0, 0);
                }
            }
            if !out.is_empty() || none {
                each(out, keys)?;
            }
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
                    let key = sort_key(&row, &self.sorted, &self.under)?;
                    self.held.push(Held::of(&self.ctx, &key, Some(&row), seq));
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
        // A run's next row: its values held to be compared, the row beside them whole.
        let next_of = |i: usize,
                       backs: &mut Vec<Back>,
                       first: &mut std::vec::IntoIter<Held>|
         -> Result<Option<(Held, Option<Vec<Value>>)>> {
            if i == backs.len() {
                return Ok(first.next().map(|mut h| {
                    h.seq = rank(i);
                    (h, None)
                }));
            }
            Ok(match backs[i].next_row()? {
                Some(row) => {
                    let key = sort_key(&row, &self.sorted, &self.under)?;
                    Some((Held::of(&self.ctx, &key, None, rank(i)), Some(row)))
                }
                None => None,
            })
        };
        // Each row beside the run it came from.
        let mut heap = BinaryHeap::with_capacity(backs.len() + 1);
        let mut beside: Vec<Option<Vec<Value>>> = (0..=backs.len()).map(|_| None).collect();
        for i in 0..=backs.len() {
            if let Some((h, row)) = next_of(i, &mut backs, &mut first)? {
                beside[i] = row;
                heap.push(std::cmp::Reverse((h, i)));
            }
        }
        let (mut rows, mut keys, mut bytes, mut held) = (Vec::new(), Vec::new(), 0u64, 0u64);
        while let Some(std::cmp::Reverse((h, i))) = heap.pop() {
            let row = match beside[i].take() {
                Some(row) => {
                    held += row_file_width(&row);
                    row
                }
                None => h.row()?,
            };
            bytes += row_file_width(&row);
            held += h.bytes();
            keys.push(h.key_values()?);
            rows.push(row);
            if bytes >= CHUNK {
                bytes = 0;
                let _flying = crate::room::flying(std::mem::take(&mut held));
                each(std::mem::take(&mut rows), std::mem::take(&mut keys))?;
            }
            if let Some((n, row)) = next_of(i, &mut backs, &mut first)? {
                beside[i] = row;
                heap.push(std::cmp::Reverse((n, i)));
            }
        }
        if let Some(e) = self.ctx.spec.failed.borrow_mut().take() {
            return Err(e);
        }
        if !rows.is_empty() {
            let _flying = crate::room::flying(held);
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

    /// Whether it reads one row more, as PostgreSQL's Limit asks the node beneath it for one: until
    /// its count is reached, and past it where it keeps ties, until a row does not tie.
    pub(super) fn wants(&self) -> bool {
        !self.ended && (self.ties || self.left != Some(0))
    }

    /// A row it read, made where `wants` said, sorting by `key`: passed over within its OFFSET,
    /// kept within its count or where it ties with the last kept, else the last it reads.
    pub(super) fn offer(&mut self, row: Vec<Value>, key: &[Value], out: &mut Vec<Vec<Value>>) {
        if self.skip > 0 {
            self.skip -= 1;
            return;
        }
        match self.left {
            Some(0) => {
                if !self.last.as_deref().is_some_and(|l| tie(l, key)) {
                    self.ended = true;
                    return;
                }
            }
            Some(n) => self.left = Some(n - 1),
            None => {}
        }
        if self.ties {
            self.last = Some(key.to_vec());
        }
        out.push(row);
    }

    /// The rows as they come it reaches: its OFFSET's and its count; none where it has no count.
    pub(super) fn reached(&self) -> Option<u64> {
        self.left.map(|n| n.saturating_add(self.skip))
    }

    /// Whether it reads no row past those it has read.
    pub(super) fn ended(&self) -> bool {
        self.ended || (self.skip == 0 && self.left == Some(0) && !self.ties)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(desc: bool, nulls_first: bool) -> Rc<Ctx> {
        Rc::new(Ctx {
            spec: Spec {
                dirs: vec![(desc, nulls_first)],
                failed: RefCell::new(None),
            },
            packer: RefCell::new(Packer::default()),
            types: RefCell::new(Vec::new()),
        })
    }

    /// Every pair of values held compares as the values do, each way, and each comes back whole.
    #[test]
    fn a_key_held_as_bytes_compares_and_comes_back_as_its_values() {
        let groups: Vec<Vec<Value>> = vec![
            [i64::MIN, -300, -1, 0, 1, 255, 256, i64::MAX].map(Value::Int).to_vec(),
            [f32::NEG_INFINITY, -2.5, -0.0, 0.0, 1e-30, 3.0, f32::INFINITY, f32::NAN]
                .map(Value::Real)
                .to_vec(),
            [f64::NEG_INFINITY, -1e300, -0.0, 0.0, 2.0, f64::INFINITY, f64::NAN]
                .map(Value::Double)
                .to_vec(),
            [i128::MIN, -5, 0, 7, i128::MAX].map(Value::Uuid).to_vec(),
            vec![Value::Bool(false), Value::Bool(true)],
            ["", "a", "a\u{0}", "ab", "b", "é", "\u{10ffff}"]
                .map(|s| Value::Text(s.into()))
                .to_vec(),
            vec![
                Value::Bytes(vec![]),
                Value::Bytes(vec![0]),
                Value::Bytes(vec![0, 255]),
                Value::Bytes(vec![1]),
            ],
            [-5, 0, 3]
                .map(|x| Value::Scalar(ColumnType::Timestamptz, x))
                .to_vec(),
            // One scale and two: 1.0 and 1.00 tie, 0.5 sorts before both.
            vec![
                Value::Num(10, 1),
                Value::Num(100, 2),
                Value::Num(5, 1),
                Value::Num(-120, 2),
                Value::Int(1),
            ],
            // A bpchar's trailing blanks take no part.
            ["a", "a  ", "ab", "b "]
                .map(|s| Value::Str(ColumnType::Bpchar(Some(3)), s.into()))
                .to_vec(),
            // Held packed, compared made whole: 30 days and a month tie.
            [(0, 0, 0), (0, 30, 0), (1, 0, 0), (0, 0, -5)]
                .map(|(months, days, micros)| {
                    Value::Interval(crate::times::Interval {
                        months,
                        days,
                        micros,
                    })
                })
                .to_vec(),
        ];
        for (desc, nulls_first) in [(false, false), (true, true), (false, true), (true, false)] {
            let c = ctx(desc, nulls_first);
            for group in &groups {
                let mut values = group.clone();
                values.push(Value::Null);
                let held: Vec<Held> = values
                    .iter()
                    .enumerate()
                    .map(|(i, v)| Held::of(&c, std::slice::from_ref(v), Some(&[v.clone()]), i as u64))
                    .collect();
                for (a, x) in held.iter().zip(&values) {
                    for (b, y) in held.iter().zip(&values) {
                        let want = c.spec.one(x, y, desc, nulls_first);
                        assert_eq!(c.compare(a.key(), b.key()), want, "{x:?} {y:?} {desc} {nulls_first}");
                    }
                    let back = a.key_values().unwrap();
                    assert_eq!(format!("{back:?}"), format!("{:?}", [x]), "{x:?}");
                    assert_eq!(format!("{:?}", a.row().unwrap()), format!("{:?}", [x]));
                }
                assert!(c.spec.failed.borrow().is_none());
            }
        }
    }
}
