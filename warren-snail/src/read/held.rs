//! A place's rows held column-wise: each column one vector of its values' kind, a NULL a bit, as a
//! block's values are held; a value of a kind no vector holds kept as it is. Each row's bytes are
//! known before it is held.

use super::answer::{Cell, Dict, Text};
use super::hash::WordHasher;
use super::pipeline::{Key, Value};
use crate::catalog::ColumnType;
use crate::times::Interval;
use parquet::data_type::ByteArray;
use std::cell::RefCell;
use std::sync::Arc;

/// Which rows are NULL: a bit a row, a word each 64 rows.
#[derive(Default)]
struct Nulls(Vec<u64>);

impl Nulls {
    fn set(&mut self, at: usize, null: bool) {
        if at % 64 == 0 {
            self.0.push(0);
        }
        if null {
            self.0[at / 64] |= 1 << (at % 64);
        }
    }

    fn get(&self, at: usize) -> bool {
        self.0[at / 64] & (1 << (at % 64)) != 0
    }
}

/// Values of one width, a NULL's place holding the kind's default.
struct Fixed<T> {
    values: Vec<T>,
    nulls: Nulls,
}

impl<T: Copy + Default> Fixed<T> {
    fn of_nulls(n: usize) -> Fixed<T> {
        let mut f = Fixed {
            values: Vec::with_capacity(n + 1),
            nulls: Nulls::default(),
        };
        for _ in 0..n {
            f.push(None);
        }
        f
    }

    fn push(&mut self, v: Option<T>) {
        self.nulls.set(self.values.len(), v.is_none());
        self.values.push(v.unwrap_or_default());
    }

    fn get(&self, at: usize) -> Option<T> {
        (!self.nulls.get(at)).then(|| self.values[at])
    }
}

/// What a column of text keeps of each value's type.
#[derive(Clone, Copy, PartialEq)]
enum Texts {
    Text,
    Str(ColumnType),
    Typed(ColumnType),
}

/// What a column of bytes keeps of each value's type.
#[derive(Clone, Copy, PartialEq)]
enum Bins {
    Bytes,
    Bits(ColumnType),
}

/// One column's values.
enum Column {
    /// As many NULLs as rows, and no value yet.
    Nulls(usize),
    Int(Fixed<i64>),
    Real(Fixed<f32>),
    Double(Fixed<f64>),
    Bool(Fixed<bool>),
    Uuid(Fixed<i128>),
    Num(Fixed<i128>, Vec<u32>),
    Scalar(ColumnType, Fixed<i64>),
    Interval(Fixed<Interval>),
    /// Each value's text one after another, and where each ends.
    Text(Texts, String, Vec<u32>, Nulls),
    Bin(Bins, Vec<u8>, Vec<u32>, Nulls),
    /// Values of kinds no vector of its own holds, or of more kinds than one.
    Values(Vec<Value>),
}

/// The bytes a value of each width holds.
fn fixed<T>() -> u64 {
    std::mem::size_of::<T>() as u64
}

/// The bytes a value held as itself holds.
fn as_value(v: &Value) -> u64 {
    super::spill::width(v)
}

/// The bytes the NULL bits of row `at` take: a word where it begins one.
fn bits(at: usize) -> u64 {
    if at % 64 == 0 {
        8
    } else {
        0
    }
}

/// The column a first value of this kind begins, after `n` NULLs.
fn begun(v: &Value, n: usize) -> Column {
    match v {
        Value::Int(_) => Column::Int(Fixed::of_nulls(n)),
        Value::Real(_) => Column::Real(Fixed::of_nulls(n)),
        Value::Double(_) => Column::Double(Fixed::of_nulls(n)),
        Value::Bool(_) => Column::Bool(Fixed::of_nulls(n)),
        Value::Uuid(_) => Column::Uuid(Fixed::of_nulls(n)),
        Value::Num(..) => Column::Num(Fixed::of_nulls(n), vec![0; n]),
        Value::Scalar(ty, _) => Column::Scalar(*ty, Fixed::of_nulls(n)),
        Value::Interval(_) => Column::Interval(Fixed::of_nulls(n)),
        Value::Text(_) | Value::Str(..) | Value::Typed(..) => {
            let kind = match v {
                Value::Str(ty, _) => Texts::Str(*ty),
                Value::Typed(ty, _) => Texts::Typed(*ty),
                _ => Texts::Text,
            };
            let mut nulls = Nulls::default();
            (0..n).for_each(|i| nulls.set(i, true));
            Column::Text(kind, String::new(), vec![0; n], nulls)
        }
        Value::Bytes(_) | Value::Bits(..) => {
            let kind = match v {
                Value::Bits(ty, _) => Bins::Bits(*ty),
                _ => Bins::Bytes,
            };
            let mut nulls = Nulls::default();
            (0..n).for_each(|i| nulls.set(i, true));
            Column::Bin(kind, Vec::new(), vec![0; n], nulls)
        }
        _ => Column::Values(vec![Value::Null; n]),
    }
}

impl Column {
    fn len(&self) -> usize {
        match self {
            Column::Nulls(n) => *n,
            Column::Int(f) | Column::Scalar(_, f) => f.values.len(),
            Column::Real(f) => f.values.len(),
            Column::Double(f) => f.values.len(),
            Column::Bool(f) => f.values.len(),
            Column::Uuid(f) | Column::Num(f, _) => f.values.len(),
            Column::Interval(f) => f.values.len(),
            Column::Text(_, _, ends, _) | Column::Bin(_, _, ends, _) => ends.len(),
            Column::Values(v) => v.len(),
        }
    }

    /// The bytes every value it holds takes.
    fn bytes(&self) -> u64 {
        let n = self.len();
        let words = n.div_ceil(64) as u64 * 8;
        match self {
            Column::Nulls(_) => 0,
            Column::Int(_) | Column::Scalar(..) => n as u64 * fixed::<i64>() + words,
            Column::Real(_) => n as u64 * fixed::<f32>() + words,
            Column::Double(_) => n as u64 * fixed::<f64>() + words,
            Column::Bool(_) => n as u64 * fixed::<bool>() + words,
            Column::Uuid(_) => n as u64 * fixed::<i128>() + words,
            Column::Num(..) => n as u64 * (fixed::<i128>() + fixed::<u32>()) + words,
            Column::Interval(_) => n as u64 * fixed::<Interval>() + words,
            Column::Text(_, t, ..) => t.len() as u64 + n as u64 * fixed::<u32>() + words,
            Column::Bin(_, b, ..) => b.len() as u64 + n as u64 * fixed::<u32>() + words,
            Column::Values(v) => v.iter().map(as_value).sum(),
        }
    }

    /// The bytes `v` adds held here, a column it turns into another counted whole.
    fn width_of(&self, v: &Value) -> u64 {
        let at = self.len();
        let fits = match (self, v) {
            (Column::Values(_), _) => return as_value(v),
            (Column::Nulls(_), Value::Null) => return 0,
            (Column::Nulls(n), _) => {
                let begun = begun(v, *n);
                return begun.bytes() + begun.width_of(v);
            }
            (_, Value::Null) => true,
            (Column::Int(_), Value::Int(_))
            | (Column::Real(_), Value::Real(_))
            | (Column::Double(_), Value::Double(_))
            | (Column::Bool(_), Value::Bool(_))
            | (Column::Uuid(_), Value::Uuid(_))
            | (Column::Num(..), Value::Num(..))
            | (Column::Interval(_), Value::Interval(_)) => true,
            (Column::Scalar(ty, _), Value::Scalar(t, _)) => ty == t,
            (Column::Text(k, t, ..), _) => {
                text_of(*k, v).is_some_and(|s| t.len() + s.len() <= u32::MAX as usize)
            }
            (Column::Bin(k, b, ..), _) => {
                bin_of(*k, v).is_some_and(|s| b.len() + s.len() <= u32::MAX as usize)
            }
            _ => false,
        };
        if !fits {
            let held: u64 = (0..at).map(|i| as_value(&self.get(i))).sum();
            return held - self.bytes() + as_value(v);
        }
        bits(at)
            + match self {
                Column::Int(_) | Column::Scalar(..) => fixed::<i64>(),
                Column::Real(_) => fixed::<f32>(),
                Column::Double(_) => fixed::<f64>(),
                Column::Bool(_) => fixed::<bool>(),
                Column::Uuid(_) => fixed::<i128>(),
                Column::Num(..) => fixed::<i128>() + fixed::<u32>(),
                Column::Interval(_) => fixed::<Interval>(),
                Column::Text(k, ..) => {
                    fixed::<u32>() + text_of(*k, v).map_or(0, |s| s.len() as u64)
                }
                Column::Bin(k, ..) => fixed::<u32>() + bin_of(*k, v).map_or(0, |s| s.len() as u64),
                Column::Nulls(_) | Column::Values(_) => unreachable!("answered above"),
            }
    }

    fn push(&mut self, v: Value) {
        let at = self.len();
        let fits = match (&mut *self, &v) {
            (Column::Values(vs), _) => {
                vs.push(v);
                return;
            }
            (Column::Nulls(n), Value::Null) => {
                *n += 1;
                return;
            }
            (Column::Nulls(n), _) => {
                *self = begun(&v, *n);
                return self.push(v);
            }
            (Column::Int(f), Value::Int(x)) => {
                f.push(Some(*x));
                return;
            }
            (Column::Int(f), Value::Null) | (Column::Scalar(_, f), Value::Null) => {
                f.push(None);
                return;
            }
            (Column::Scalar(ty, f), Value::Scalar(t, x)) if ty == t => {
                f.push(Some(*x));
                return;
            }
            (Column::Real(f), Value::Real(x)) => {
                f.push(Some(*x));
                return;
            }
            (Column::Real(f), Value::Null) => {
                f.push(None);
                return;
            }
            (Column::Double(f), Value::Double(x)) => {
                f.push(Some(*x));
                return;
            }
            (Column::Double(f), Value::Null) => {
                f.push(None);
                return;
            }
            (Column::Bool(f), Value::Bool(x)) => {
                f.push(Some(*x));
                return;
            }
            (Column::Bool(f), Value::Null) => {
                f.push(None);
                return;
            }
            (Column::Uuid(f), Value::Uuid(x)) => {
                f.push(Some(*x));
                return;
            }
            (Column::Uuid(f), Value::Null) => {
                f.push(None);
                return;
            }
            (Column::Num(f, s), Value::Num(m, x)) => {
                f.push(Some(*m));
                s.push(*x);
                return;
            }
            (Column::Num(f, s), Value::Null) => {
                f.push(None);
                s.push(0);
                return;
            }
            (Column::Interval(f), Value::Interval(x)) => {
                f.push(Some(*x));
                return;
            }
            (Column::Interval(f), Value::Null) => {
                f.push(None);
                return;
            }
            (Column::Text(k, t, ends, nulls), _) => match (&v, text_of(*k, &v)) {
                (Value::Null, _) => {
                    nulls.set(at, true);
                    ends.push(t.len() as u32);
                    return;
                }
                (_, Some(s)) if t.len() + s.len() <= u32::MAX as usize => {
                    t.push_str(s);
                    nulls.set(at, false);
                    ends.push(t.len() as u32);
                    return;
                }
                _ => false,
            },
            (Column::Bin(k, b, ends, nulls), _) => match (&v, bin_of(*k, &v)) {
                (Value::Null, _) => {
                    nulls.set(at, true);
                    ends.push(b.len() as u32);
                    return;
                }
                (_, Some(s)) if b.len() + s.len() <= u32::MAX as usize => {
                    b.extend_from_slice(s);
                    nulls.set(at, false);
                    ends.push(b.len() as u32);
                    return;
                }
                _ => false,
            },
            _ => false,
        };
        debug_assert!(!fits);
        let mut values: Vec<Value> = (0..at).map(|i| self.get(i)).collect();
        values.push(v);
        *self = Column::Values(values);
    }

    fn get(&self, at: usize) -> Value {
        let or_null = |v: Option<Value>| v.unwrap_or(Value::Null);
        match self {
            Column::Nulls(_) => Value::Null,
            Column::Int(f) => or_null(f.get(at).map(Value::Int)),
            Column::Real(f) => or_null(f.get(at).map(Value::Real)),
            Column::Double(f) => or_null(f.get(at).map(Value::Double)),
            Column::Bool(f) => or_null(f.get(at).map(Value::Bool)),
            Column::Uuid(f) => or_null(f.get(at).map(Value::Uuid)),
            Column::Num(f, s) => or_null(f.get(at).map(|m| Value::Num(m, s[at]))),
            Column::Scalar(ty, f) => or_null(f.get(at).map(|x| Value::Scalar(*ty, x))),
            Column::Interval(f) => or_null(f.get(at).map(Value::Interval)),
            Column::Text(k, t, ends, nulls) => match nulls.get(at) {
                true => Value::Null,
                false => {
                    let from = if at == 0 { 0 } else { ends[at - 1] as usize };
                    let s = t[from..ends[at] as usize].to_owned();
                    match k {
                        Texts::Text => Value::Text(s),
                        Texts::Str(ty) => Value::Str(*ty, s),
                        Texts::Typed(ty) => Value::Typed(*ty, s),
                    }
                }
            },
            Column::Bin(k, b, ends, nulls) => match nulls.get(at) {
                true => Value::Null,
                false => {
                    let from = if at == 0 { 0 } else { ends[at - 1] as usize };
                    let s = b[from..ends[at] as usize].to_vec();
                    match k {
                        Bins::Bytes => Value::Bytes(s),
                        Bins::Bits(ty) => Value::Bits(*ty, s),
                    }
                }
            },
            Column::Values(v) => v[at].clone(),
        }
    }
}

/// A value's text where a column of text of this kind holds it.
fn text_of(k: Texts, v: &Value) -> Option<&str> {
    match (k, v) {
        (Texts::Text, Value::Text(s)) => Some(s),
        (Texts::Str(ty), Value::Str(t, s)) | (Texts::Typed(ty), Value::Typed(t, s)) if ty == *t => {
            Some(s)
        }
        _ => None,
    }
}

/// A value's bytes where a column of bytes of this kind holds them.
fn bin_of(k: Bins, v: &Value) -> Option<&[u8]> {
    match (k, v) {
        (Bins::Bytes, Value::Bytes(b)) => Some(b),
        (Bins::Bits(ty), Value::Bits(t, b)) if ty == *t => Some(b),
        _ => None,
    }
}

/// Rows held, found by their place among them.
pub(super) trait RowsAt {
    fn count(&self) -> usize;
    /// Row `at`, made whole.
    fn row(&self, at: usize) -> Vec<Value>;
}

impl RowsAt for Columns {
    fn count(&self) -> usize {
        self.len
    }

    fn row(&self, at: usize) -> Vec<Value> {
        Columns::row(self, at)
    }
}

/// Rows held column-wise, in the order they came.
#[derive(Default)]
pub(super) struct Columns {
    columns: Vec<Column>,
    len: usize,
}

impl Columns {
    pub(super) fn len(&self) -> usize {
        self.len
    }

    /// The bytes `row` adds held here.
    pub(super) fn width_of(&self, row: &[Value]) -> u64 {
        match self.columns.is_empty() {
            true => row.iter().map(|v| Column::Nulls(0).width_of(v)).sum(),
            false => self.columns.iter().zip(row).map(|(c, v)| c.width_of(v)).sum(),
        }
    }

    /// The bytes every row held takes.
    #[cfg(test)]
    fn bytes(&self) -> u64 {
        self.columns.iter().map(Column::bytes).sum()
    }

    pub(super) fn push(&mut self, row: Vec<Value>) {
        if self.columns.is_empty() {
            self.columns = (0..row.len()).map(|_| Column::Nulls(self.len)).collect();
        }
        debug_assert_eq!(row.len(), self.columns.len());
        for (c, v) in self.columns.iter_mut().zip(row) {
            c.push(v);
        }
        self.len += 1;
    }

    /// Row `at`, made whole.
    pub(super) fn row(&self, at: usize) -> Vec<Value> {
        self.columns.iter().map(|c| c.get(at)).collect()
    }
}

/// A value of a row of cells held, as it lies.
#[derive(Clone, Copy)]
pub(super) enum At<'a> {
    Null,
    Int(i64),
    Wide(i128),
    Text(&'a [u8]),
    Cell(&'a Cell),
}

impl At<'_> {
    /// Whether it is this cell, as cells compare.
    fn is(&self, c: &Cell) -> bool {
        match (self, c) {
            (At::Null, Cell::Null) => true,
            (At::Int(x), Cell::Int(y)) => x == y,
            (At::Wide(x), Cell::Wide(y)) => x == y,
            (At::Text(x), Cell::Text(y)) => *x == y.0.data(),
            (At::Cell(x), y) => *x == y,
            _ => false,
        }
    }
}

/// One column of cells.
enum CellColumn {
    Nulls(usize),
    Int(Fixed<i64>),
    Wide(Fixed<i128>),
    /// Each text's bytes one after another and where each ends; once shut, the bytes shared.
    Text {
        open: Vec<u8>,
        shut: Option<ByteArray>,
        ends: Vec<u32>,
        nulls: Nulls,
    },
    /// Text as codes into the dictionaries of the blocks it came from, laid end to end.
    Coded {
        words: Vec<ByteArray>,
        last: Option<Arc<[ByteArray]>>,
        codes: Vec<u32>,
        nulls: Nulls,
    },
    /// Cells of kinds no vector of its own holds, or of more kinds than one.
    Cells(Vec<Cell>),
}

impl CellColumn {
    fn len(&self) -> usize {
        match self {
            CellColumn::Nulls(n) => *n,
            CellColumn::Int(f) => f.values.len(),
            CellColumn::Wide(f) => f.values.len(),
            CellColumn::Text { ends, .. } => ends.len(),
            CellColumn::Coded { codes, .. } => codes.len(),
            CellColumn::Cells(c) => c.len(),
        }
    }

    /// The bytes every cell it holds takes.
    fn bytes(&self) -> u64 {
        let n = self.len() as u64;
        let bits = (self.len().div_ceil(64) * 8) as u64;
        match self {
            CellColumn::Nulls(_) => 0,
            CellColumn::Int(_) => n * fixed::<i64>() + bits,
            CellColumn::Wide(_) => n * fixed::<i128>() + bits,
            CellColumn::Text { open, shut, .. } => {
                shut.as_ref().map_or(open.len(), |s| s.len()) as u64 + n * fixed::<u32>() + bits
            }
            CellColumn::Coded { words, .. } => {
                words
                    .iter()
                    .map(|w| fixed::<ByteArray>() + w.len() as u64)
                    .sum::<u64>()
                    + n * fixed::<u32>()
                    + bits
            }
            CellColumn::Cells(c) => c.iter().map(cell_bytes).sum(),
        }
    }

    /// The column a first cell of this kind begins, after `n` NULLs.
    fn begun(c: &Cell, dict: &Dict, n: usize) -> CellColumn {
        let mut nulls = Nulls::default();
        (0..n).for_each(|i| nulls.set(i, true));
        match (c, dict) {
            (Cell::Int(_), _) => CellColumn::Int(Fixed::of_nulls(n)),
            (Cell::Wide(_), _) => CellColumn::Wide(Fixed::of_nulls(n)),
            (Cell::Code(_), Some(_)) => CellColumn::Coded {
                words: Vec::new(),
                last: None,
                codes: vec![0; n],
                nulls,
            },
            (Cell::Text(_), _) => CellColumn::Text {
                open: Vec::new(),
                shut: None,
                ends: vec![0; n],
                nulls,
            },
            _ => CellColumn::Cells(vec![Cell::Null; n]),
        }
    }

    fn push(&mut self, c: &Cell, dict: &Dict) {
        let at = self.len();
        match (&mut *self, c) {
            (CellColumn::Cells(v), _) => v.push(super::answer::as_text(c, dict)),
            (CellColumn::Nulls(n), Cell::Null) => *n += 1,
            (CellColumn::Nulls(n), _) => {
                *self = CellColumn::begun(c, dict, *n);
                self.push(c, dict);
            }
            (CellColumn::Int(f), Cell::Int(x)) => f.push(Some(*x)),
            (CellColumn::Int(f), Cell::Null) => f.push(None),
            (CellColumn::Wide(f), Cell::Wide(x)) => f.push(Some(*x)),
            (CellColumn::Wide(f), Cell::Null) => f.push(None),
            (CellColumn::Text { open, ends, nulls, .. }, _)
                if matches!(c, Cell::Null | Cell::Text(_))
                    || matches!((c, dict), (Cell::Code(_), Some(_))) =>
            {
                match super::answer::as_text(c, dict) {
                    Cell::Text(t) if open.len() + t.0.len() <= u32::MAX as usize => {
                        open.extend_from_slice(t.0.data());
                        nulls.set(at, false);
                    }
                    Cell::Text(_) => return self.into_cells(c, dict),
                    _ => nulls.set(at, true),
                }
                ends.push(open.len() as u32);
            }
            (
                CellColumn::Coded {
                    words,
                    last,
                    codes,
                    nulls,
                },
                Cell::Code(k),
            ) if dict.is_some() => {
                let d = dict.as_ref().unwrap();
                let from = match last {
                    Some(l) if Arc::ptr_eq(l, d) => words.len() - l.len(),
                    _ => {
                        let from = words.len();
                        if from + d.len() > u32::MAX as usize {
                            return self.into_cells(c, dict);
                        }
                        words.extend(d.iter().cloned());
                        *last = Some(d.clone());
                        from
                    }
                };
                codes.push((from + *k as usize) as u32);
                nulls.set(at, false);
            }
            (CellColumn::Coded { codes, nulls, .. }, Cell::Null) => {
                codes.push(0);
                nulls.set(at, true);
            }
            (CellColumn::Coded { .. }, Cell::Text(_)) => {
                let mut text = CellColumn::Text {
                    open: Vec::new(),
                    shut: None,
                    ends: Vec::with_capacity(at + 1),
                    nulls: Nulls::default(),
                };
                for i in 0..at {
                    text.push(&self.cell(i), &None);
                }
                *self = text;
                self.push(c, dict);
            }
            _ => self.into_cells(c, dict),
        }
    }

    /// Turns into a column of cells, `c` the last.
    fn into_cells(&mut self, c: &Cell, dict: &Dict) {
        let mut cells: Vec<Cell> = (0..self.len()).map(|i| self.cell(i)).collect();
        cells.push(super::answer::as_text(c, dict));
        *self = CellColumn::Cells(cells);
    }

    fn shut(&mut self) {
        match self {
            CellColumn::Text { open, shut, .. } if shut.is_none() => {
                *shut = Some(ByteArray::from(std::mem::take(open)));
            }
            CellColumn::Coded { last, .. } => *last = None,
            _ => {}
        }
    }

    fn at(&self, at: usize) -> At<'_> {
        match self {
            CellColumn::Nulls(_) => At::Null,
            CellColumn::Int(f) => f.get(at).map_or(At::Null, At::Int),
            CellColumn::Wide(f) => f.get(at).map_or(At::Null, At::Wide),
            CellColumn::Text {
                open,
                shut,
                ends,
                nulls,
            } => match nulls.get(at) {
                true => At::Null,
                false => {
                    let from = if at == 0 { 0 } else { ends[at - 1] as usize };
                    let bytes = shut.as_ref().map_or(&open[..], |s| s.data());
                    At::Text(&bytes[from..ends[at] as usize])
                }
            },
            CellColumn::Coded {
                words,
                codes,
                nulls,
                ..
            } => match nulls.get(at) {
                true => At::Null,
                false => At::Text(words[codes[at] as usize].data()),
            },
            CellColumn::Cells(c) => match &c[at] {
                Cell::Null => At::Null,
                c => At::Cell(c),
            },
        }
    }

    fn cell(&self, at: usize) -> Cell {
        match self {
            CellColumn::Text {
                shut: Some(s),
                ends,
                nulls,
                ..
            } if !nulls.get(at) => {
                let from = if at == 0 { 0 } else { ends[at - 1] as usize };
                Cell::Text(Text(s.slice(from, ends[at] as usize - from)))
            }
            CellColumn::Coded {
                words,
                codes,
                nulls,
                ..
            } if !nulls.get(at) => Cell::Text(Text(words[codes[at] as usize].clone())),
            _ => match self.at(at) {
                At::Null => Cell::Null,
                At::Int(v) => Cell::Int(v),
                At::Wide(v) => Cell::Wide(v),
                At::Text(t) => Cell::Text(Text(ByteArray::from(t.to_vec()))),
                At::Cell(c) => c.clone(),
            },
        }
    }
}

/// The bytes a cell held as itself takes: the cell, and what it holds apart from it.
fn cell_bytes(c: &Cell) -> u64 {
    fixed::<Cell>()
        + match c {
            Cell::Text(t) => t.0.len() as u64,
            Cell::Keyed(k) => (k.key.len() + k.image.0.len()) as u64,
            Cell::Classed(c) => (c.class.0.len() + c.image.0.len()) as u64,
            _ => 0,
        }
}

/// Rows of cells held column-wise, in the order they came.
#[derive(Default)]
pub(super) struct Cells {
    columns: Vec<CellColumn>,
    len: usize,
}

impl Cells {
    #[cfg(test)]
    fn len(&self) -> usize {
        self.len
    }

    /// The bytes every row held takes.
    pub(super) fn bytes(&self) -> u64 {
        self.columns.iter().map(CellColumn::bytes).sum()
    }

    /// A row more, a code among its cells read in its column's dictionary.
    pub(super) fn push(&mut self, row: &[Cell], dicts: &[Dict]) {
        if self.columns.is_empty() {
            self.columns = (0..row.len()).map(|_| CellColumn::Nulls(self.len)).collect();
        }
        for (i, (col, c)) in self.columns.iter_mut().zip(row).enumerate() {
            col.push(c, dicts.get(i).unwrap_or(&None));
        }
        self.len += 1;
    }

    /// Done taking rows: the text held shared from here on.
    pub(super) fn shut(&mut self) {
        self.columns.iter_mut().for_each(CellColumn::shut);
    }

    /// Column `column` of row `at`, as it lies.
    pub(super) fn at(&self, at: usize, column: usize) -> At<'_> {
        self.columns[column].at(at)
    }

    /// Column `column` of row `at`, as a cell.
    pub(super) fn cell(&self, at: usize, column: usize) -> Cell {
        self.columns[column].cell(at)
    }

    /// Whether row `at` is `row`, cell for cell.
    pub(super) fn same(&self, at: usize, row: &[Cell]) -> bool {
        self.columns.iter().zip(row).all(|(col, c)| col.at(at).is(c))
    }
}

/// Row numbers found by their hash: open addressing, each slot a row's number and its hash's high
/// half, at most three slots of four taken.
#[derive(Default)]
pub(super) struct Slots {
    slots: Vec<(u32, u32)>,
    len: usize,
}

const EMPTY: u32 = u32::MAX;

impl Slots {
    /// The row of this hash that `same` says is the one sought.
    pub(super) fn find(&self, hash: u64, mut same: impl FnMut(u32) -> bool) -> Option<u32> {
        if self.slots.is_empty() {
            return None;
        }
        let tag = (hash >> 32) as u32;
        let mask = self.slots.len() - 1;
        let mut i = tag as usize & mask;
        loop {
            let (row, t) = self.slots[i];
            if row == EMPTY {
                return None;
            }
            if t == tag && same(row) {
                return Some(row);
            }
            i = (i + 1) & mask;
        }
    }

    /// Row `row` of this hash, not yet among them.
    pub(super) fn insert(&mut self, hash: u64, row: u32) {
        if (self.len + 1) * 4 > self.slots.len() * 3 {
            let n = (self.slots.len() * 2).max(16);
            let old = std::mem::replace(&mut self.slots, vec![(EMPTY, 0); n]);
            for (r, t) in old {
                if r != EMPTY {
                    self.place(t, r);
                }
            }
        }
        self.place((hash >> 32) as u32, row);
        self.len += 1;
    }

    fn place(&mut self, tag: u32, row: u32) {
        let mask = self.slots.len() - 1;
        let mut i = tag as usize & mask;
        while self.slots[i].0 != EMPTY {
            i = (i + 1) & mask;
        }
        self.slots[i] = (row, tag);
    }

    /// The bytes its slots take.
    pub(super) fn bytes(&self) -> u64 {
        (self.slots.capacity() * std::mem::size_of::<(u32, u32)>()) as u64
    }

    /// The bytes a row more adds to its slots: none until they grow.
    fn width_of_one(&self) -> u64 {
        match (self.len + 1) * 4 > self.slots.len() * 3 {
            true => (((self.slots.len() * 2).max(16) - self.slots.len()) * 8) as u64,
            false => 0,
        }
    }
}

/// A value of a key as a key set holds it: its kind, then its payload, its length first where it
/// varies, so that equal values are equal bytes.
pub(super) trait Encoded {
    /// Appends its bytes, a type it names by its place among `types`.
    fn encode(&self, types: &mut Vec<ColumnType>, out: &mut Vec<u8>);
}

/// Appends a value of a kind whose length varies: the kind, the length, the bytes.
pub(super) fn varied(out: &mut Vec<u8>, kind: u8, b: &[u8]) {
    out.push(kind);
    let mut n = b.len();
    while n >= 0x80 {
        out.push(n as u8 | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
    out.extend_from_slice(b);
}

impl Encoded for Key {
    fn encode(&self, types: &mut Vec<ColumnType>, out: &mut Vec<u8>) {
        encode(self, types, out)
    }
}

fn encode(k: &Key, types: &mut Vec<ColumnType>, out: &mut Vec<u8>) {
    match k {
        Key::Null => out.push(0),
        Key::Num(m, 0) if i64::try_from(*m).is_ok() => {
            out.push(1);
            out.extend_from_slice(&(*m as i64).to_le_bytes());
        }
        Key::Num(m, s) => {
            out.push(2);
            out.extend_from_slice(&m.to_le_bytes());
            out.extend_from_slice(&s.to_le_bytes());
        }
        Key::Text(t) => varied(out, 3, t.as_bytes()),
        Key::Bytes(b) => varied(out, 4, b),
        Key::Bool(b) => out.extend_from_slice(&[5, *b as u8]),
        Key::Uuid(u) => {
            out.push(6);
            out.extend_from_slice(&u.to_le_bytes());
        }
        Key::Scalar(ty, v) => {
            let at = match types.iter().position(|t| t == ty) {
                Some(at) => at,
                None => {
                    types.push(*ty);
                    types.len() - 1
                }
            };
            out.push(7);
            out.extend_from_slice(&(at as u32).to_le_bytes());
            out.extend_from_slice(&v.to_le_bytes());
        }
        Key::Real(v) => {
            out.push(8);
            out.extend_from_slice(&v.to_le_bytes());
        }
        Key::Double(v) => {
            out.push(9);
            out.extend_from_slice(&v.to_le_bytes());
        }
        Key::Class(s) => varied(out, 10, s.as_bytes()),
        Key::Composed(b) => varied(out, 11, b),
        Key::Interval(v) => {
            out.push(12);
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
}

/// Keys held, each as one string of bytes, its values' one after another, so that equal keys are
/// equal bytes; found by their hash.
#[derive(Default)]
pub(super) struct KeySet {
    bytes: Vec<u8>,
    ends: Vec<u64>,
    slots: Slots,
    types: RefCell<Vec<ColumnType>>,
    asked: RefCell<Vec<u8>>,
    hash: std::cell::Cell<u64>,
}

impl KeySet {
    #[cfg(test)]
    fn len(&self) -> usize {
        self.ends.len()
    }

    fn key(&self, at: usize) -> &[u8] {
        let from = if at == 0 { 0 } else { self.ends[at - 1] as usize };
        &self.bytes[from..self.ends[at] as usize]
    }

    /// Takes `key` as the one asked about next: where it lies among the keys held, none where it is
    /// not one.
    pub(super) fn ask<K: Encoded>(&self, key: &[K]) -> Option<usize> {
        let mut asked = self.asked.borrow_mut();
        asked.clear();
        let mut types = self.types.borrow_mut();
        for k in key {
            k.encode(&mut types, &mut asked);
        }
        let hash = std::hash::BuildHasher::hash_one(
            &std::hash::BuildHasherDefault::<WordHasher>::default(),
            &asked[..],
        );
        self.hash.set(hash);
        self.slots
            .find(hash, |r| self.key(r as usize) == &asked[..])
            .map(|r| r as usize)
    }

    /// The bytes the key last asked about adds held.
    pub(super) fn width(&self) -> u64 {
        self.asked.borrow().len() as u64 + 8 + self.slots.width_of_one()
    }

    /// Holds the key last asked about, not held: where it lies.
    pub(super) fn hold(&mut self) -> usize {
        let at = self.ends.len();
        self.bytes.extend_from_slice(&self.asked.borrow());
        self.ends.push(self.bytes.len() as u64);
        self.slots.insert(self.hash.get(), at as u32);
        at
    }

    /// Holds `key` where it is not held: whether it was not.
    pub(super) fn insert<K: Encoded>(&mut self, key: &[K]) -> bool {
        match self.ask(key) {
            Some(_) => false,
            None => {
                self.hold();
                true
            }
        }
    }

    /// Whether `key` is held.
    pub(super) fn contains<K: Encoded>(&self, key: &[K]) -> bool {
        self.ask(key).is_some()
    }

    /// The bytes `key` adds held: none where it is held.
    pub(super) fn width_of<K: Encoded>(&self, key: &[K]) -> u64 {
        match self.ask(key) {
            Some(_) => 0,
            None => self.width(),
        }
    }

    /// The bytes every key held takes.
    pub(super) fn bytes(&self) -> u64 {
        (self.bytes.len() + self.ends.len() * 8) as u64 + self.slots.bytes()
    }
}

/// Row numbers by their keys: each key held once, its rows chained in the order they came.
#[derive(Default)]
pub(super) struct KeyRows {
    keys: KeySet,
    /// Each key's first and last row.
    ends: Vec<(u32, u32)>,
    /// Each row's next of its key.
    next: Vec<u32>,
}

impl KeyRows {
    /// Row `row` of `key`, rows coming in order; the bytes it adds held.
    pub(super) fn push<K: Encoded>(&mut self, key: &[K], row: u32) -> u64 {
        let mut w = 0;
        if self.next.len() <= row as usize {
            w += (row as usize + 1 - self.next.len()) as u64 * 4;
            self.next.resize(row as usize + 1, EMPTY);
        }
        match self.keys.ask(key) {
            Some(k) => {
                let last = self.ends[k].1;
                self.next[last as usize] = row;
                self.ends[k].1 = row;
            }
            None => {
                w += self.keys.width() + 8;
                self.keys.hold();
                self.ends.push((row, row));
            }
        }
        w
    }

    /// The bytes a row more of `key` adds held.
    pub(super) fn width_of<K: Encoded>(&self, key: &[K]) -> u64 {
        4 + match self.keys.ask(key) {
            Some(_) => 0,
            None => self.keys.width() + 8,
        }
    }

    /// The rows of `key`, in the order they came.
    pub(super) fn rows<K: Encoded>(&self, key: &[K]) -> impl Iterator<Item = u32> + '_ {
        let first = self.keys.ask(key).map(|k| self.ends[k].0);
        std::iter::successors(first, move |&r| {
            Some(self.next[r as usize]).filter(|&n| n != EMPTY)
        })
    }

    /// The bytes it holds.
    #[cfg(test)]
    fn bytes(&self) -> u64 {
        self.keys.bytes() + (self.ends.len() * 8 + self.next.len() * 4) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_row_comes_back_as_it_went_and_takes_the_bytes_it_was_counted_at() {
        let rows = vec![
            vec![Value::Null, Value::Int(1), Value::Text("ab".into()), Value::Null],
            vec![Value::Null, Value::Null, Value::Null, Value::Bool(true)],
            vec![
                Value::Double(-0.0),
                Value::Int(-3),
                Value::Text("héllo".into()),
                Value::Bool(false),
            ],
            vec![
                Value::Double(2.5),
                Value::Text("not an integer".into()),
                Value::Text(String::new()),
                Value::Null,
            ],
        ];
        let mut cols = Columns::default();
        let mut counted = 0;
        for r in &rows {
            counted += cols.width_of(r);
            cols.push(r.clone());
            assert_eq!(cols.bytes(), counted, "{r:?}");
        }
        for _ in 0..70 {
            let r = vec![Value::Double(1.0), Value::Int(7), Value::Null, Value::Bool(true)];
            counted += cols.width_of(&r);
            cols.push(r);
            assert_eq!(cols.bytes(), counted);
        }
        assert_eq!(cols.len(), 74);
        for (i, r) in rows.iter().enumerate() {
            assert_eq!(format!("{:?}", cols.row(i)), format!("{r:?}"));
        }
        assert_eq!(
            format!("{:?}", cols.row(73)),
            format!("{:?}", vec![Value::Double(1.0), Value::Int(7), Value::Null, Value::Bool(true)])
        );
    }

    #[test]
    fn every_row_of_cells_comes_back_as_it_went_its_codes_read_in_their_dictionaries() {
        let text = |s: &str| Cell::Text(Text(ByteArray::from(s.as_bytes().to_vec())));
        let one: Dict = Some(vec![ByteArray::from("pt".as_bytes().to_vec())].into());
        let two: Dict = Some(
            vec![
                ByteArray::from("es".as_bytes().to_vec()),
                ByteArray::from("gl".as_bytes().to_vec()),
            ]
            .into(),
        );
        let rows: Vec<(Vec<Cell>, Vec<Dict>)> = vec![
            (vec![Cell::Null, Cell::Code(0), Cell::Int(1)], vec![None, one.clone(), None]),
            (vec![text("ab"), Cell::Code(0), Cell::Null], vec![None, one.clone(), None]),
            (vec![Cell::Null, Cell::Code(1), Cell::Int(-2)], vec![None, two.clone(), None]),
            (vec![text(""), Cell::Null, Cell::Wide(3)], vec![None, two.clone(), None]),
            (vec![text("héllo"), Cell::Code(0), Cell::Int(4)], vec![None, two.clone(), None]),
        ];
        let whole: Vec<Vec<Cell>> = rows
            .iter()
            .map(|(r, d)| {
                r.iter()
                    .zip(d)
                    .map(|(c, d)| super::super::answer::as_text(c, d))
                    .collect()
            })
            .collect();
        let mut cells = Cells::default();
        for (r, d) in &rows {
            cells.push(r, d);
        }
        assert_eq!(cells.len(), 5);
        let mut shut = Cells::default();
        for (r, d) in &rows {
            shut.push(r, d);
        }
        shut.shut();
        for held in [&cells, &shut] {
            for (i, r) in whole.iter().enumerate() {
                let back: Vec<Cell> = (0..3).map(|c| held.cell(i, c)).collect();
                assert_eq!(&back, r, "row {i}");
                assert!(held.same(i, r), "row {i}");
            }
            assert!(!held.same(0, &whole[1]));
            assert!(!held.same(4, &whole[2]));
        }
        assert!(matches!(cells.columns[0], CellColumn::Text { .. }));
        assert!(matches!(cells.columns[1], CellColumn::Coded { .. }));
        assert!(matches!(cells.columns[2], CellColumn::Cells(_)));
        assert_eq!(cells.columns[0].bytes(), 8 + 5 * 4 + 8);
        assert_eq!(cells.columns[1].bytes(), (3 * 32 + 6) + 5 * 4 + 8);
        assert_eq!(cells.bytes(), shut.bytes());
    }

    #[test]
    fn a_slot_finds_each_row_put_by_its_hash_and_no_other() {
        let mut slots = Slots::default();
        let hash = |r: u32| u64::from(r % 7) << 32 | u64::from(r);
        for r in 0..1000 {
            assert_eq!(slots.find(hash(r), |x| x == r), None);
            slots.insert(hash(r), r);
        }
        for r in 0..1000 {
            assert_eq!(slots.find(hash(r), |x| x == r), Some(r));
        }
        assert_eq!(slots.find(hash(1000), |x| x == 1000), None);
        assert_eq!(slots.bytes(), 2048 * 8);
    }

    #[test]
    fn equal_keys_are_one_key_and_each_takes_the_bytes_it_was_counted_at() {
        let keys = vec![
            vec![Key::Null, Key::Num(1, 0)],
            vec![Key::Text("ab".into()), Key::Num(i128::MAX, 3)],
            vec![Key::Text("a".into()), Key::Text("b".into())],
            vec![Key::Text("ab".into()), Key::Null],
            vec![Key::Scalar(ColumnType::Oid, 7), Key::Bool(true)],
            vec![Key::Scalar(ColumnType::Xid, 7), Key::Bool(true)],
            vec![Key::Bytes(vec![0; 200]), Key::Interval(-1)],
        ];
        let mut set = KeySet::default();
        let mut counted = 0;
        for (i, k) in keys.iter().enumerate() {
            assert_eq!(set.ask(k), None, "{k:?}");
            counted += set.width();
            assert_eq!(set.hold(), i);
            assert_eq!(set.bytes(), counted, "{k:?}");
        }
        for (i, k) in keys.iter().enumerate() {
            assert_eq!(set.ask(&k.clone()), Some(i), "{k:?}");
            assert!(!set.insert(k));
        }
        assert_eq!(set.width_of(&[Key::Text("ab".into()), Key::Num(i128::MAX, 3)]), 0);
        assert_eq!(set.len(), keys.len());
        // A NULL 1 byte, an integer 1 + 8, "ab" 1 + 1 + 2, 200 bytes 1 + 2 + 200, an interval 1 + 16.
        assert_eq!(set.key(0).len(), 1 + 9);
        assert_eq!(set.key(3).len(), 4 + 1);
        assert_eq!(set.key(6).len(), 203 + 17);

        let mut rows = KeyRows::default();
        let mut counted = 0;
        for (r, k) in [0, 1, 0, 2, 1, 0].into_iter().enumerate() {
            let w = rows.width_of(&keys[k]);
            assert_eq!(rows.push(&keys[k], r as u32), w);
            counted += w;
        }
        assert_eq!(rows.rows(&keys[0]).collect::<Vec<_>>(), vec![0, 2, 5]);
        assert_eq!(rows.rows(&keys[1]).collect::<Vec<_>>(), vec![1, 4]);
        assert_eq!(rows.rows(&keys[2]).collect::<Vec<_>>(), vec![3]);
        assert_eq!(rows.rows(&keys[3]).count(), 0);
        assert_eq!(rows.bytes(), counted);
        assert_eq!(rows.bytes(), rows.keys.bytes() + 3 * 8 + 6 * 4);
    }
}
