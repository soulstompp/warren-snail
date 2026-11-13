//! A place's rows held column-wise: each column one vector of its values' kind, a NULL a bit, as a
//! block's values are held; a value of a kind no vector holds kept as it is. Each row's bytes are
//! known before it is held.

use super::pipeline::Value;
use crate::catalog::ColumnType;
use crate::times::Interval;

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
}
