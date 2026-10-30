//! One column's values in row order: integers and instants as numbers, uuids as their keys, text as
//! text.

use crate::catalog::ColumnType;
use crate::composed::{Compound, Nested};
use crate::read::statement::Datum;
use parquet::data_type::ByteArray;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Values {
    /// Integers as they are, instants as microseconds since 1970-01-01 UTC.
    Int(Vec<Option<i64>>),
    Text(Vec<Option<String>>),
    /// Text as decoded from its pages, each value sharing its bytes with the page or dictionary it
    /// came from. Every value is valid UTF-8.
    Utf8(Vec<Option<ByteArray>>),
    /// Text read from a dictionary: each value's code, its place in the dictionary. Every entry of
    /// the dictionary is valid UTF-8.
    Dict {
        dict: Arc<[ByteArray]>,
        codes: Vec<Option<u32>>,
    },
    /// Values keyed in 128 bits, as their keys: a uuid's.
    Wide(Vec<Option<i128>>),
    /// Reals, as their own bits; each one's key is its class's.
    Float4(Vec<Option<u32>>),
    /// Double precisions, as their own bits.
    Float8(Vec<Option<u64>>),
    /// Values of a composed type held as a tree of leaves, each held whole.
    Composed(Vec<Option<Arc<Compound>>>),
    /// One leaf of a composed column's tree: its levels, and a value an entry of them.
    Nested(Box<Nested>),
    /// Intervals, as their own fields; each one's key is its total.
    Interval(Vec<Option<crate::times::Interval>>),
}

/// The bytes Parquet writes before each plain text value: its length.
pub const LENGTH_PREFIX: u64 = 4;

impl Values {
    /// No values yet, of the kind a column of type `ty` holds.
    pub fn new(ty: ColumnType) -> Values {
        match ty {
            ColumnType::Text(_) | ColumnType::Bpchar(_) | ColumnType::Name => {
                Values::Text(Vec::new())
            }
            ColumnType::Bytes | ColumnType::Bit(_) | ColumnType::Varbit(_) => {
                Values::Utf8(Vec::new())
            }
            ColumnType::Uuid => Values::Wide(Vec::new()),
            ColumnType::Numeric { precision, .. } if precision > 18 => Values::Wide(Vec::new()),
            ColumnType::NumericText(_) => Values::Text(Vec::new()),
            ColumnType::Float4 => Values::Float4(Vec::new()),
            ColumnType::Float8 => Values::Float8(Vec::new()),
            ColumnType::Interval { .. } => Values::Interval(Vec::new()),
            ty if ty.is_nested() => Values::Composed(Vec::new()),
            _ => Values::Int(Vec::new()),
        }
    }

    /// No values yet, of the kind `other` holds.
    pub fn new_like(other: &Values) -> Values {
        match other {
            Values::Int(_) => Values::Int(Vec::new()),
            Values::Text(_) => Values::Text(Vec::new()),
            Values::Utf8(_) | Values::Dict { .. } => Values::Utf8(Vec::new()),
            Values::Wide(_) => Values::Wide(Vec::new()),
            Values::Float4(_) => Values::Float4(Vec::new()),
            Values::Float8(_) => Values::Float8(Vec::new()),
            Values::Interval(_) => Values::Interval(Vec::new()),
            Values::Composed(_) => Values::Composed(Vec::new()),
            Values::Nested(n) => Values::Nested(Box::new(Nested {
                values: Values::new_like(&n.values),
                defs: Vec::new(),
                reps: Vec::new(),
                starts: Vec::new(),
            })),
        }
    }

    /// Appends a NULL.
    pub fn push_null(&mut self) {
        match self {
            Values::Int(v) => v.push(None),
            Values::Text(v) => v.push(None),
            Values::Utf8(v) => v.push(None),
            Values::Dict { codes, .. } => codes.push(None),
            Values::Wide(v) => v.push(None),
            Values::Float4(v) => v.push(None),
            Values::Float8(v) => v.push(None),
            Values::Interval(v) => v.push(None),
            Values::Composed(v) => v.push(None),
            Values::Nested(_) => panic!("a NULL pushed onto a leaf without its levels"),
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Values::Int(v) => v.len(),
            Values::Text(v) => v.len(),
            Values::Utf8(v) => v.len(),
            Values::Dict { codes, .. } => codes.len(),
            Values::Wide(v) => v.len(),
            Values::Float4(v) => v.len(),
            Values::Float8(v) => v.len(),
            Values::Composed(v) => v.len(),
            Values::Nested(n) => n.rows(),
            Values::Interval(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn is_null(&self, i: usize) -> bool {
        match self {
            Values::Int(v) => v[i].is_none(),
            Values::Text(v) => v[i].is_none(),
            Values::Utf8(v) => v[i].is_none(),
            Values::Dict { codes, .. } => codes[i].is_none(),
            Values::Wide(v) => v[i].is_none(),
            Values::Float4(v) => v[i].is_none(),
            Values::Float8(v) => v[i].is_none(),
            Values::Composed(v) => v[i].is_none(),
            Values::Nested(n) => n.defs[n.starts[i] as usize] == 0,
            Values::Interval(v) => v[i].is_none(),
        }
    }

    pub fn nulls(&self) -> usize {
        (0..self.len()).filter(|&i| self.is_null(i)).count()
    }

    /// Row `i`'s key in 64 bits: a number's own value, a float's class's.
    pub fn int(&self, i: usize) -> Option<i64> {
        match self {
            Values::Int(v) => v[i],
            Values::Float4(v) => v[i].map(crate::scalars::float4_key),
            Values::Float8(v) => v[i].map(crate::scalars::float8_key),
            Values::Text(_)
            | Values::Utf8(_)
            | Values::Dict { .. }
            | Values::Wide(_)
            | Values::Composed(_)
            | Values::Interval(_)
            | Values::Nested(_) => None,
        }
    }

    /// Row `i`'s key in 128 bits: a number's own value, or a wide value's key.
    pub fn wide(&self, i: usize) -> Option<i128> {
        match self {
            Values::Int(v) => v[i].map(i128::from),
            Values::Float4(_) | Values::Float8(_) => self.int(i).map(i128::from),
            Values::Wide(v) => v[i],
            Values::Interval(v) => v[i].map(crate::times::Interval::total),
            Values::Text(_)
            | Values::Utf8(_)
            | Values::Dict { .. }
            | Values::Composed(_)
            | Values::Nested(_) => None,
        }
    }

    /// Row `i`'s own fields, for an interval.
    pub fn interval(&self, i: usize) -> Option<crate::times::Interval> {
        match self {
            Values::Interval(v) => v[i],
            _ => None,
        }
    }

    /// Row `i`'s own bits, for a float: a real's in the low 32.
    pub fn bits(&self, i: usize) -> Option<u64> {
        match self {
            Values::Float4(v) => v[i].map(u64::from),
            Values::Float8(v) => v[i],
            _ => None,
        }
    }

    /// The dictionary the codes index, for text read from one.
    pub fn dict(&self) -> Option<&Arc<[ByteArray]>> {
        match self {
            Values::Dict { dict, .. } => Some(dict),
            _ => None,
        }
    }

    /// The code of row `i`, for text read from a dictionary.
    pub fn code(&self, i: usize) -> Option<u32> {
        match self {
            Values::Dict { codes, .. } => codes[i],
            _ => None,
        }
    }

    /// Row `i`'s text; a composed value's key, written as text that sorts as the key does.
    pub fn text(&self, i: usize) -> Option<&str> {
        match self {
            Values::Text(v) => v[i].as_deref(),
            Values::Utf8(v) => v[i]
                .as_ref()
                .map(|b| std::str::from_utf8(b.data()).unwrap_or_default()),
            Values::Dict { dict, codes } => {
                codes[i].map(|c| std::str::from_utf8(dict[c as usize].data()).unwrap_or_default())
            }
            Values::Composed(v) => v[i].as_ref().map(|c| &*c.hex),
            Values::Int(_)
            | Values::Wide(_)
            | Values::Float4(_)
            | Values::Float8(_)
            | Values::Interval(_)
            | Values::Nested(_) => None,
        }
    }

    /// Text at row `i` as shared bytes: without a copy where it was decoded from its pages.
    pub fn text_value(&self, i: usize) -> Option<ByteArray> {
        match self {
            Values::Utf8(v) => v[i].clone(),
            Values::Dict { dict, codes } => codes[i].map(|c| dict[c as usize].clone()),
            Values::Text(v) => v[i].as_deref().map(ByteArray::from),
            // A composed value's key, written as text that sorts as the key does.
            Values::Composed(v) => v[i]
                .as_ref()
                .map(|c| ByteArray::from(c.hex.as_bytes().to_vec())),
            Values::Int(_)
            | Values::Wide(_)
            | Values::Float4(_)
            | Values::Float8(_)
            | Values::Interval(_)
            | Values::Nested(_) => None,
        }
    }

    /// Row `i`'s composed value.
    pub fn compound(&self, i: usize) -> Option<&Arc<Compound>> {
        match self {
            Values::Composed(v) => v[i].as_ref(),
            _ => None,
        }
    }

    pub fn ints(&self) -> Option<&[Option<i64>]> {
        match self {
            Values::Int(v) => Some(v),
            Values::Text(_)
            | Values::Utf8(_)
            | Values::Dict { .. }
            | Values::Wide(_)
            | Values::Float4(_)
            | Values::Float8(_)
            | Values::Composed(_)
            | Values::Interval(_)
            | Values::Nested(_) => None,
        }
    }

    /// The values of the rows given, in that order.
    pub fn gather(&self, rows: &[usize]) -> Values {
        match self {
            Values::Int(v) => Values::Int(rows.iter().map(|&r| v[r]).collect()),
            Values::Text(v) => Values::Text(rows.iter().map(|&r| v[r].clone()).collect()),
            Values::Utf8(v) => Values::Utf8(rows.iter().map(|&r| v[r].clone()).collect()),
            Values::Dict { dict, codes } => Values::Dict {
                dict: dict.clone(),
                codes: rows.iter().map(|&r| codes[r]).collect(),
            },
            Values::Wide(v) => Values::Wide(rows.iter().map(|&r| v[r]).collect()),
            Values::Float4(v) => Values::Float4(rows.iter().map(|&r| v[r]).collect()),
            Values::Float8(v) => Values::Float8(rows.iter().map(|&r| v[r]).collect()),
            Values::Composed(v) => Values::Composed(rows.iter().map(|&r| v[r].clone()).collect()),
            Values::Nested(n) => Values::Nested(Box::new(n.gather(rows))),
            Values::Interval(v) => Values::Interval(rows.iter().map(|&r| v[r]).collect()),
        }
    }

    /// Appends row `i` of `other`, a column of the same kind.
    pub fn push_from(&mut self, other: &Values, i: usize) {
        match (self, other) {
            (Values::Int(a), Values::Int(b)) => a.push(b[i]),
            (Values::Text(a), Values::Text(b)) => a.push(b[i].clone()),
            (Values::Utf8(a), Values::Utf8(b)) => a.push(b[i].clone()),
            (Values::Utf8(a), b @ Values::Dict { .. }) => a.push(b.text_value(i)),
            (Values::Utf8(a), Values::Text(b)) => a.push(b[i].as_deref().map(ByteArray::from)),
            (Values::Text(a), b @ (Values::Utf8(_) | Values::Dict { .. })) => {
                a.push(b.text(i).map(str::to_string))
            }
            (Values::Wide(a), Values::Wide(b)) => a.push(b[i]),
            (Values::Float4(a), Values::Float4(b)) => a.push(b[i]),
            (Values::Float8(a), Values::Float8(b)) => a.push(b[i]),
            (Values::Composed(a), Values::Composed(b)) => a.push(b[i].clone()),
            (Values::Nested(a), Values::Nested(b)) => a.push_from(b, i),
            (Values::Interval(a), Values::Interval(b)) => a.push(b[i]),
            _ => panic!("a column's values pushed onto another kind"),
        }
    }

    /// The bytes of the text present, as PostgreSQL's `octet_length` counts them; 0 for numbers.
    pub fn bytes(&self) -> u64 {
        match self {
            Values::Text(v) => v.iter().flatten().map(|s| s.len() as u64).sum(),
            Values::Utf8(v) => v.iter().flatten().map(|b| b.len() as u64).sum(),
            Values::Dict { dict, codes } => codes
                .iter()
                .flatten()
                .map(|&c| dict[c as usize].len() as u64)
                .sum(),
            Values::Nested(n) => n.values.bytes(),
            Values::Int(_)
            | Values::Wide(_)
            | Values::Float4(_)
            | Values::Float8(_)
            | Values::Interval(_)
            | Values::Composed(_) => 0,
        }
    }

    /// The bytes row `i` takes written plain: a number its width, text its bytes and its length, an
    /// enum its label's, a leaf of a composed column the values of its entries, and a NULL none.
    pub fn plain(&self, i: usize, ty: ColumnType) -> u64 {
        if self.is_null(i) {
            return 0;
        }
        if let Values::Nested(n) = self {
            return n
                .entries(i)
                .filter(|&e| !n.values.is_null(e))
                .map(|e| n.values.plain(e, ty))
                .sum();
        }
        if let (Values::Int(v), Some(labels)) = (self, crate::composed::labels(ty)) {
            return v[i].map_or(0, |k| labels[k as usize].len() as u64 + LENGTH_PREFIX);
        }
        match (self, ty.width()) {
            (_, Some(w)) => w as u64,
            (Values::Text(v), None) => v[i].as_ref().map_or(0, |s| s.len() as u64 + LENGTH_PREFIX),
            (Values::Utf8(v), None) => v[i].as_ref().map_or(0, |b| b.len() as u64 + LENGTH_PREFIX),
            (Values::Dict { dict, codes }, None) => {
                codes[i].map_or(0, |c| dict[c as usize].len() as u64 + LENGTH_PREFIX)
            }
            (
                Values::Int(_)
                | Values::Wide(_)
                | Values::Float4(_)
                | Values::Float8(_)
                | Values::Composed(_)
                | Values::Interval(_)
                | Values::Nested(_),
                None,
            ) => 0,
        }
    }

    /// A column's values as the leaves of its type hold them, one `Values` a leaf: its own where it
    /// is one leaf.
    pub fn into_leaves(self, ty: ColumnType) -> Vec<Values> {
        use crate::scalars::{tid_parts, unsigned_key};
        match (ty, self) {
            // A tid's block number and offset, each keyed in the signed range of its own bits.
            (ColumnType::Tid, Values::Int(v)) => {
                let parts: Vec<Option<(u32, u16)>> = v.iter().map(|k| k.map(tid_parts)).collect();
                vec![
                    Values::Int(
                        parts
                            .iter()
                            .map(|p| p.map(|(b, _)| unsigned_key(u64::from(b), 32)))
                            .collect(),
                    ),
                    Values::Int(
                        parts
                            .iter()
                            .map(|p| p.map(|(_, o)| unsigned_key(u64::from(o), 16)))
                            .collect(),
                    ),
                ]
            }
            // A bit string, held as its key: its length, and its bits as bytes.
            (ColumnType::Varbit(_), values) => {
                let parts: Vec<Option<(u32, Vec<u8>)>> = (0..values.len())
                    .map(|i| {
                        values
                            .text_value(i)
                            .map(|k| crate::strings::bits_of_key(k.data()))
                    })
                    .collect();
                vec![
                    Values::Int(
                        parts
                            .iter()
                            .map(|p| p.as_ref().map(|p| i64::from(p.0)))
                            .collect(),
                    ),
                    Values::Utf8(
                        parts
                            .into_iter()
                            .map(|p| p.map(|p| ByteArray::from(p.1)))
                            .collect(),
                    ),
                ]
            }
            (ColumnType::Composed(r), Values::Composed(v)) if crate::composed::is_nested(r) => {
                crate::composed::stripe(r, &v)
                    .into_iter()
                    .map(|n| Values::Nested(Box::new(n)))
                    .collect()
            }
            // A timetz's time and zone, each as it is.
            (ColumnType::Timetz(_), Values::Int(v)) => {
                let parts: Vec<Option<(i64, i32)>> = v
                    .iter()
                    .map(|k| k.map(crate::scalars::timetz_parts))
                    .collect();
                vec![
                    Values::Int(parts.iter().map(|p| p.map(|(t, _)| t)).collect()),
                    Values::Int(parts.iter().map(|p| p.map(|(_, z)| i64::from(z))).collect()),
                ]
            }
            (ColumnType::Interval { .. }, Values::Interval(v)) => vec![
                Values::Int(v.iter().map(|x| x.map(|x| i64::from(x.months))).collect()),
                Values::Int(v.iter().map(|x| x.map(|x| i64::from(x.days))).collect()),
                Values::Int(v.iter().map(|x| x.map(|x| x.micros)).collect()),
            ],
            (_, values) => vec![values],
        }
    }

    /// A column's values from its leaves', each of the same rows: its one leaf's own where it is
    /// one.
    pub fn from_leaves(ty: ColumnType, leaves: Vec<Values>) -> Values {
        use crate::scalars::{tid_key, unsigned_of};
        match (ty, leaves.as_slice()) {
            (ColumnType::Tid, [Values::Int(blocks), Values::Int(offsets)]) => Values::Int(
                blocks
                    .iter()
                    .zip(offsets)
                    .map(|(b, o)| {
                        Some(tid_key(
                            unsigned_of((*b)?, 32) as u32,
                            unsigned_of((*o)?, 16) as u16,
                        ))
                    })
                    .collect(),
            ),
            (ColumnType::Varbit(_), [lengths, bits]) => Values::Utf8(
                (0..lengths.len())
                    .map(|i| {
                        let len = lengths.int(i)?;
                        let b = bits.text_value(i)?;
                        Some(ByteArray::from(crate::strings::bits_key(
                            len as u32,
                            b.data(),
                        )))
                    })
                    .collect(),
            ),
            (ColumnType::Composed(r), parts) if crate::composed::is_nested(r) => {
                let nested: Vec<&Nested> = parts
                    .iter()
                    .filter_map(|p| match p {
                        Values::Nested(n) => Some(&**n),
                        _ => None,
                    })
                    .collect();
                Values::Composed(crate::composed::assemble_rows(r, &nested))
            }
            (ColumnType::Timetz(_), [Values::Int(times), Values::Int(zones)]) => Values::Int(
                times
                    .iter()
                    .zip(zones)
                    .map(|(t, z)| Some(crate::scalars::timetz_key((*t)?, (*z)? as i32)))
                    .collect(),
            ),
            (
                ColumnType::Interval { .. },
                [Values::Int(months), Values::Int(days), Values::Int(micros)],
            ) => Values::Interval(
                months
                    .iter()
                    .zip(days)
                    .zip(micros)
                    .map(|((m, d), u)| {
                        Some(crate::times::Interval {
                            months: (*m)? as i32,
                            days: (*d)? as i32,
                            micros: (*u)?,
                        })
                    })
                    .collect(),
            ),
            _ => leaves.into_iter().next().unwrap_or_else(|| Values::new(ty)),
        }
    }

    /// The most characters any value holds.
    pub fn longest(&self) -> usize {
        match self {
            Values::Text(v) => v
                .iter()
                .flatten()
                .map(|s| s.chars().count())
                .max()
                .unwrap_or(0),
            Values::Utf8(v) => v
                .iter()
                .flatten()
                .map(|b| std::str::from_utf8(b.data()).map_or(0, |s| s.chars().count()))
                .max()
                .unwrap_or(0),
            Values::Dict { dict, codes } => codes
                .iter()
                .flatten()
                .map(|&c| {
                    std::str::from_utf8(dict[c as usize].data()).map_or(0, |s| s.chars().count())
                })
                .max()
                .unwrap_or(0),
            Values::Int(_)
            | Values::Wide(_)
            | Values::Float4(_)
            | Values::Float8(_)
            | Values::Composed(_)
            | Values::Interval(_)
            | Values::Nested(_) => 0,
        }
    }
}

/// Appends a value to a column's values of its kind.
pub fn push_datum(v: &mut Values, d: &Datum) {
    match (v, d) {
        (v, Datum::Null) => v.push_null(),
        (Values::Int(x), Datum::Int(n)) => x.push(Some(*n)),
        (Values::Text(x), Datum::Text(s)) => x.push(Some(s.clone())),
        (Values::Utf8(x), Datum::Bytes(b)) => x.push(Some(ByteArray::from(b.clone()))),
        (Values::Utf8(x), Datum::Text(s)) => x.push(Some(ByteArray::from(s.as_str()))),
        (Values::Wide(x), Datum::Wide(k)) => x.push(Some(*k)),
        (Values::Float4(x), Datum::Float4(b)) => x.push(Some(*b)),
        (Values::Float8(x), Datum::Float8(b)) => x.push(Some(*b)),
        (Values::Composed(x), Datum::Compound(c)) => x.push(Some(c.clone())),
        _ => panic!("a value pushed onto a column of another kind"),
    }
}

/// Row `i`'s value as a column of type `ty` holds it: a number, a float's bits, a uuid's key, text,
/// bytea's bytes, or a composed value.
pub fn datum_of(v: &Values, i: usize, ty: ColumnType) -> Datum {
    match v {
        Values::Int(x) => x[i].map_or(Datum::Null, Datum::Int),
        Values::Wide(x) => x[i].map_or(Datum::Null, Datum::Wide),
        Values::Float4(x) => x[i].map_or(Datum::Null, Datum::Float4),
        Values::Float8(x) => x[i].map_or(Datum::Null, Datum::Float8),
        Values::Composed(x) => x[i].clone().map_or(Datum::Null, Datum::Compound),
        Values::Nested(_) => panic!("a leaf's value read without its levels"),
        _ if ty == ColumnType::Bytes => v
            .text_value(i)
            .map_or(Datum::Null, |b| Datum::Bytes(b.data().to_vec())),
        _ => v
            .text(i)
            .map_or(Datum::Null, |s| Datum::Text(s.to_string())),
    }
}
