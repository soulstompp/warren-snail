//! An order's rows sorted within the write's room: held while the room has room for them; past it,
//! the rows held sorted and written as a run, and the next held in their place; the last rows held,
//! and every run read back once as the runs are merged. Past as many runs as the room has two pieces
//! for, the first runs merged and written again as one.

use crate::catalog::Table;
use crate::cut::PAGE;
use crate::places::KeyValue;
use crate::room::Room;
use crate::values::Values;
use crate::{Error, Result};
use bytes::Bytes;
use parquet::data_type::ByteArray;
use std::ops::Range;
use std::rc::Rc;

/// Rows of an order, column by column, with each row's key and its width on each of the order's
/// leaves, and the bytes they are held at.
pub(crate) struct Chunk {
    pub columns: Vec<Values>,
    pub keys: Vec<KeyValue>,
    pub widths: Vec<u64>,
    pub bytes: u64,
}

impl Chunk {
    pub fn rows(&self) -> usize {
        self.columns.first().map_or(0, Values::len)
    }

    fn none() -> Chunk {
        Chunk {
            columns: Vec::new(),
            keys: Vec::new(),
            widths: Vec::new(),
            bytes: 0,
        }
    }
}

/// The plain bytes of rows a run writes as one piece, and reads back as one.
const PIECE: u64 = 16 * PAGE as u64;

/// A run's pieces, each its first page on the room's spill, its pages and its bytes.
#[derive(Default)]
struct Run {
    pieces: Vec<(u64, u64, usize)>,
}

/// Hands on one row: its columns and its place there, its key and its widths.
pub(crate) type Each<'e> = dyn FnMut(&[Values], usize, &[KeyValue], &[u64]) -> Result<()> + 'e;

/// Rows sorted on an order within a room.
pub(crate) struct Sorter<'a> {
    room: Rc<Room>,
    table: &'a Table,
    stride: usize,
    leaves: usize,
    /// Reads back a piece's rows as the rows of an order: their keys and widths.
    keyed: &'a dyn Fn(Vec<Values>) -> Result<Chunk>,
    held: Vec<Chunk>,
    held_bytes: u64,
    runs: Vec<Run>,
}

impl<'a> Sorter<'a> {
    pub fn new(
        room: Rc<Room>,
        table: &'a Table,
        stride: usize,
        leaves: usize,
        keyed: &'a dyn Fn(Vec<Values>) -> Result<Chunk>,
    ) -> Sorter<'a> {
        Sorter {
            room,
            table,
            stride,
            leaves,
            keyed,
            held: Vec::new(),
            held_bytes: 0,
            runs: Vec::new(),
        }
    }

    /// Rows more, after those before them: held where the room has room for them; else the rows
    /// held are written as a run first.
    pub fn push(&mut self, chunk: Chunk) -> Result<()> {
        if chunk.rows() == 0 {
            return Ok(());
        }
        if !self.room.hold(chunk.bytes) {
            if !self.held.is_empty() {
                self.write_held()?;
            }
            self.room.hold_anyway(chunk.bytes);
        }
        self.held_bytes += chunk.bytes;
        self.held.push(chunk);
        Ok(())
    }

    /// The rows held, in their order: each by its chunk and its place there, those that tie in the
    /// order they came.
    fn sorted_held(&self) -> Vec<(u32, u32)> {
        let mut at: Vec<(u32, u32)> = self
            .held
            .iter()
            .enumerate()
            .flat_map(|(c, k)| (0..k.rows() as u32).map(move |r| (c as u32, r)))
            .collect();
        let s = self.stride;
        let key = |&(c, r): &(u32, u32)| {
            let r = r as usize;
            &self.held[c as usize].keys[r * s..(r + 1) * s]
        };
        at.sort_by(|a, b| key(a).cmp(key(b)));
        at
    }

    /// The rows held written as a run, sorted, and their room let go.
    fn write_held(&mut self) -> Result<()> {
        let at = self.sorted_held();
        let mut run = Run::default();
        let mut piece = Piece::new(&self.held[0].columns);
        let leaves = self.leaves;
        for &(c, r) in &at {
            let (k, r) = (&self.held[c as usize], r as usize);
            piece.push(&k.columns, r, &k.widths[r * leaves..(r + 1) * leaves]);
            if piece.plain >= PIECE {
                run.pieces.push(self.write_piece(&mut piece)?);
            }
        }
        if piece.rows > 0 {
            run.pieces.push(self.write_piece(&mut piece)?);
        }
        self.runs.push(run);
        self.held.clear();
        self.room.let_go(std::mem::take(&mut self.held_bytes));
        Ok(())
    }

    fn write_piece(&self, piece: &mut Piece) -> Result<(u64, u64, usize)> {
        let bytes = written(self.table, &piece.columns, piece.rows)?;
        piece.clear();
        let mut first = None;
        let mut page = vec![0u8; PAGE];
        for p in bytes.chunks(PAGE) {
            page[..p.len()].copy_from_slice(p);
            page[p.len()..].fill(0);
            let at = self.room.spill_page(&page)?;
            first.get_or_insert(at);
        }
        Ok((
            first.unwrap_or(0),
            bytes.len().div_ceil(PAGE) as u64,
            bytes.len(),
        ))
    }

    /// Hands `each` every row in the order's order, those that tie in the order they came: each run
    /// read back a piece at a time, then the rows held.
    pub fn merge(mut self, each: &mut Each) -> Result<()> {
        // As many runs as the room has two pieces each for; past that, the first runs merged into
        // one, written again, so the runs stand in the order they were written.
        let most = (self.room.sizes().bytes / (2 * PIECE)).max(8) as usize;
        while self.runs.len() + 1 > most {
            let take = (self.runs.len() + 2 - most).clamp(2, most);
            let group: Vec<Run> = self.runs.drain(..take).collect();
            let mut pieces = Vec::new();
            {
                let me = &self;
                let mut cursors: Vec<Cursor> = group
                    .into_iter()
                    .map(|r| Cursor::of(r, me))
                    .collect::<Result<_>>()?;
                let mut piece: Option<Piece> = None;
                merged(
                    &mut cursors,
                    None,
                    me.stride,
                    me.leaves,
                    &mut |cols, r, _, widths| {
                        let p = piece.get_or_insert_with(|| Piece::new(cols));
                        p.push(cols, r, widths);
                        if p.plain >= PIECE {
                            pieces.push(me.write_piece(p)?);
                        }
                        Ok(())
                    },
                )?;
                if let Some(p) = piece.as_mut().filter(|p| p.rows > 0) {
                    pieces.push(me.write_piece(p)?);
                }
            }
            self.runs.insert(0, Run { pieces });
        }
        let runs = std::mem::take(&mut self.runs);
        let order = self.sorted_held();
        let mut cursors: Vec<Cursor> = runs
            .into_iter()
            .map(|r| Cursor::of(r, &self))
            .collect::<Result<_>>()?;
        let held = Held {
            chunks: &self.held,
            order: &order,
        };
        merged(&mut cursors, Some(&held), self.stride, self.leaves, each)?;
        self.room.let_go(self.held_bytes);
        Ok(())
    }
}

/// The rows held, in their order.
struct Held<'h> {
    chunks: &'h [Chunk],
    order: &'h [(u32, u32)],
}

/// A run read back a piece at a time.
struct Cursor<'s, 'a> {
    sorter: &'s Sorter<'a>,
    run: Run,
    next: usize,
    chunk: Chunk,
    at: usize,
}

impl<'s, 'a> Cursor<'s, 'a> {
    fn of(run: Run, sorter: &'s Sorter<'a>) -> Result<Cursor<'s, 'a>> {
        let mut c = Cursor {
            sorter,
            run,
            next: 0,
            chunk: Chunk::none(),
            at: 0,
        };
        c.load()?;
        Ok(c)
    }

    /// Reads back the next piece, where one is left.
    fn load(&mut self) -> Result<()> {
        self.at = 0;
        let Some(&(first, pages, len)) = self.run.pieces.get(self.next) else {
            self.chunk = Chunk::none();
            return Ok(());
        };
        self.next += 1;
        let mut bytes = Vec::with_capacity(pages as usize * PAGE);
        let mut page = vec![0u8; PAGE];
        for p in first..first + pages {
            self.sorter.room.spilled_page(p, &mut page)?;
            bytes.extend_from_slice(&page);
        }
        bytes.truncate(len);
        let columns = read(self.sorter.table, Bytes::from(bytes))?;
        self.chunk = (self.sorter.keyed)(columns)?;
        Ok(())
    }

    fn done(&self) -> bool {
        self.at >= self.chunk.rows()
    }

    fn advance(&mut self) -> Result<()> {
        self.at += 1;
        if self.done() {
            self.load()?;
        }
        Ok(())
    }
}

/// The row source `i` hands on next: its chunk and its place there.
fn next_of<'c>(
    i: usize,
    cursors: &'c [Cursor],
    held: Option<&'c Held>,
    held_at: usize,
) -> Option<(&'c Chunk, usize)> {
    if i < cursors.len() {
        let c = &cursors[i];
        (!c.done()).then_some((&c.chunk, c.at))
    } else {
        let h = held?;
        let &(c, r) = h.order.get(held_at)?;
        Some((&h.chunks[c as usize], r as usize))
    }
}

/// `cursors` and `held` merged, handed on a row at a time: rows that tie in the order of the
/// cursors, then `held`.
fn merged(
    cursors: &mut [Cursor],
    held: Option<&Held>,
    stride: usize,
    leaves: usize,
    each: &mut Each,
) -> Result<()> {
    let mut held_at = 0usize;
    let sources = cursors.len() + usize::from(held.is_some());
    let less = |a: usize, b: usize, cursors: &[Cursor], held_at: usize| -> bool {
        let key = |i: usize| {
            next_of(i, cursors, held, held_at).map(|(k, r)| &k.keys[r * stride..(r + 1) * stride])
        };
        match (key(a), key(b)) {
            (Some(x), Some(y)) => (x, a) < (y, b),
            (Some(_), None) => true,
            _ => false,
        }
    };
    let sift = |heap: &mut [usize], mut i: usize, cursors: &[Cursor], held_at: usize| loop {
        let (l, r) = (2 * i + 1, 2 * i + 2);
        let mut m = i;
        if l < heap.len() && less(heap[l], heap[m], cursors, held_at) {
            m = l;
        }
        if r < heap.len() && less(heap[r], heap[m], cursors, held_at) {
            m = r;
        }
        if m == i {
            break;
        }
        heap.swap(i, m);
        i = m;
    };
    // The sources by their next rows, the least on top.
    let mut heap: Vec<usize> = (0..sources)
        .filter(|&i| next_of(i, cursors, held, held_at).is_some())
        .collect();
    for i in (0..heap.len() / 2).rev() {
        sift(&mut heap, i, cursors, held_at);
    }
    while let Some(&top) = heap.first() {
        {
            let (k, r) = next_of(top, cursors, held, held_at).expect("a source on the heap");
            each(
                &k.columns,
                r,
                &k.keys[r * stride..(r + 1) * stride],
                &k.widths[r * leaves..(r + 1) * leaves],
            )?;
        }
        if top < cursors.len() {
            cursors[top].advance()?;
        } else {
            held_at += 1;
        }
        if next_of(top, cursors, held, held_at).is_none() {
            let last = heap.pop().expect("a source");
            if !heap.is_empty() {
                heap[0] = last;
            }
        }
        if !heap.is_empty() {
            sift(&mut heap, 0, cursors, held_at);
        }
    }
    Ok(())
}

/// Rows gathered from chunks into one piece of a run.
struct Piece {
    columns: Vec<Values>,
    rows: usize,
    plain: u64,
}

impl Piece {
    fn new(like: &[Values]) -> Piece {
        Piece {
            columns: like.iter().map(Values::new_like).collect(),
            rows: 0,
            plain: 0,
        }
    }

    fn push(&mut self, from: &[Values], r: usize, widths: &[u64]) {
        for (c, v) in self.columns.iter_mut().zip(from) {
            c.push_from(v, r);
        }
        self.rows += 1;
        self.plain += widths
            .iter()
            .map(|w| w & !super::closing::NULL_BIT)
            .sum::<u64>()
            .max(1);
    }

    fn clear(&mut self) {
        for c in &mut self.columns {
            *c = Values::new_like(c);
        }
        self.rows = 0;
        self.plain = 0;
    }
}

const INT: u8 = 0;
const BYTES: u8 = 1;
const WIDE: u8 = 2;
const FLOAT4: u8 = 3;
const FLOAT8: u8 = 4;
const INTERVAL: u8 = 5;
const LEAVES: u8 = 6;

/// The disk pages a leaf's data page may take as a run writes it: as many as a value needs.
const RUN_UNIT: usize = 1 << 16;

/// A piece's rows written column by column: each column's kind, which of its rows are NULL, and its
/// values; a composed column's leaves as the snail's pages hold them.
fn written(table: &Table, columns: &[Values], rows: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    out.extend_from_slice(&(rows as u32).to_le_bytes());
    for (c, v) in columns.iter().enumerate() {
        let kind = match v {
            Values::Int(_) => INT,
            Values::Text(_) | Values::Utf8(_) | Values::Dict { .. } => BYTES,
            Values::Wide(_) => WIDE,
            Values::Float4(_) => FLOAT4,
            Values::Float8(_) => FLOAT8,
            Values::Interval(_) => INTERVAL,
            Values::Composed(_) | Values::Nested(_) => LEAVES,
        };
        out.push(kind);
        if kind == LEAVES {
            let col = &table.columns[c];
            let leaves = table.column_leaves(c);
            let values = table.split_leaves(c, v.clone());
            out.extend_from_slice(&(leaves.len() as u32).to_le_bytes());
            for (leaf, values) in leaves.iter().zip(&values) {
                let desc = crate::encode::leaf_descriptor(&col.name, leaf, true)?;
                let e = crate::encode::plain_pages(&desc, leaf.ty, values, RUN_UNIT)?;
                out.extend_from_slice(&(e.pages.len() as u32).to_le_bytes());
                for p in &e.pages {
                    out.extend_from_slice(&(p.bytes.len() as u32).to_le_bytes());
                    out.extend_from_slice(&p.bytes);
                }
            }
            continue;
        }
        let mut bits = vec![0u8; rows.div_ceil(8)];
        for (i, b) in (0..rows)
            .filter(|&i| !v.is_null(i))
            .map(|i| (i, 1u8 << (i % 8)))
        {
            bits[i / 8] |= b;
        }
        out.extend_from_slice(&bits);
        match v {
            Values::Int(x) => x.iter().flatten().for_each(|n| out.extend(n.to_le_bytes())),
            Values::Wide(x) => x.iter().flatten().for_each(|n| out.extend(n.to_le_bytes())),
            Values::Float4(x) => x.iter().flatten().for_each(|n| out.extend(n.to_le_bytes())),
            Values::Float8(x) => x.iter().flatten().for_each(|n| out.extend(n.to_le_bytes())),
            Values::Interval(x) => x.iter().flatten().for_each(|n| out.extend(n.to_bytes())),
            _ => {
                for b in (0..rows).filter_map(|i| v.text_value(i)) {
                    out.extend_from_slice(&(b.len() as u32).to_le_bytes());
                    out.extend_from_slice(b.data());
                }
            }
        }
    }
    Ok(out)
}

/// A piece's bytes read in turn.
struct Reading<'b> {
    bytes: &'b Bytes,
    at: usize,
}

impl Reading<'_> {
    fn take(&mut self, n: usize) -> Result<Range<usize>> {
        let r = self.at..self.at + n;
        if r.end > self.bytes.len() {
            return Err(Error::new("a run's piece ends short"));
        }
        self.at = r.end;
        Ok(r)
    }

    fn u32(&mut self) -> Result<u32> {
        let r = self.take(4)?;
        Ok(u32::from_le_bytes(self.bytes[r].try_into().unwrap()))
    }

    /// The bytes of each present row's value of `width` bytes.
    fn each<const N: usize>(&mut self, present: &[bool]) -> Result<Vec<Option<[u8; N]>>> {
        present
            .iter()
            .map(|&p| {
                p.then(|| self.take(N).map(|r| self.bytes[r].try_into().unwrap()))
                    .transpose()
            })
            .collect()
    }
}

/// A piece's rows read back as `written` wrote them, text sharing the piece's bytes.
fn read(table: &Table, bytes: Bytes) -> Result<Vec<Values>> {
    let mut b = Reading {
        bytes: &bytes,
        at: 0,
    };
    let rows = b.u32()? as usize;
    let mut out = Vec::with_capacity(table.columns.len());
    for c in 0..table.columns.len() {
        let kind = bytes[b.take(1)?][0];
        if kind == LEAVES {
            let col = &table.columns[c];
            let leaves = table.column_leaves(c);
            if b.u32()? as usize != leaves.len() {
                return Err(Error::new("a run's piece holds another column's leaves"));
            }
            let mut held = Vec::with_capacity(leaves.len());
            for leaf in &leaves {
                let pages = b.u32()? as usize;
                let mut at = Vec::with_capacity(pages);
                for _ in 0..pages {
                    let len = b.u32()? as usize;
                    at.push(b.take(len)?);
                }
                let parts: Vec<&[u8]> = at.iter().map(|r| &bytes[r.clone()]).collect();
                let desc = crate::encode::leaf_descriptor(&col.name, leaf, true)?;
                held.push(crate::encode::decode_leaf(&desc, leaf, None, &parts, rows)?);
            }
            out.push(Values::from_leaves(col.ty, held));
            continue;
        }
        let bits = b.take(rows.div_ceil(8))?;
        let present: Vec<bool> = (0..rows)
            .map(|i| bytes[bits.start + i / 8] & (1 << (i % 8)) != 0)
            .collect();
        out.push(match kind {
            INT => Values::Int(
                b.each::<8>(&present)?
                    .into_iter()
                    .map(|x| x.map(i64::from_le_bytes))
                    .collect(),
            ),
            WIDE => Values::Wide(
                b.each::<16>(&present)?
                    .into_iter()
                    .map(|x| x.map(i128::from_le_bytes))
                    .collect(),
            ),
            FLOAT4 => Values::Float4(
                b.each::<4>(&present)?
                    .into_iter()
                    .map(|x| x.map(u32::from_le_bytes))
                    .collect(),
            ),
            FLOAT8 => Values::Float8(
                b.each::<8>(&present)?
                    .into_iter()
                    .map(|x| x.map(u64::from_le_bytes))
                    .collect(),
            ),
            INTERVAL => Values::Interval(
                b.each::<16>(&present)?
                    .into_iter()
                    .map(|x| x.and_then(|x| crate::times::Interval::from_bytes(&x)))
                    .collect(),
            ),
            BYTES => {
                let mut v = Vec::with_capacity(rows);
                for &p in &present {
                    v.push(match p {
                        true => {
                            let len = b.u32()? as usize;
                            Some(ByteArray::from(bytes.slice(b.take(len)?)))
                        }
                        false => None,
                    });
                }
                Values::Utf8(v)
            }
            _ => return Err(Error::new("a run's piece names no kind of values")),
        });
    }
    Ok(out)
}
