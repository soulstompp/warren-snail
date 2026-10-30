//! One column's values in row order: integers and instants as numbers, uuids as their keys, text as
//! text.

use crate::catalog::ColumnType;
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
}

/// The bytes Parquet writes before each plain text value: its length.
pub const LENGTH_PREFIX: u64 = 4;

impl Values {
    /// No values yet, of the kind a column of type `ty` holds.
    pub fn new(ty: ColumnType) -> Values {
        match ty {
            ColumnType::Text(_) => Values::Text(Vec::new()),
            ColumnType::Bytes => Values::Utf8(Vec::new()),
            ColumnType::Uuid => Values::Wide(Vec::new()),
            ColumnType::Float4 => Values::Float4(Vec::new()),
            ColumnType::Float8 => Values::Float8(Vec::new()),
            _ => Values::Int(Vec::new()),
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
            Values::Text(_) | Values::Utf8(_) | Values::Dict { .. } | Values::Wide(_) => None,
        }
    }

    /// Row `i`'s key in 128 bits: a number's own value, or a wide value's key.
    pub fn wide(&self, i: usize) -> Option<i128> {
        match self {
            Values::Int(v) => v[i].map(i128::from),
            Values::Float4(_) | Values::Float8(_) => self.int(i).map(i128::from),
            Values::Wide(v) => v[i],
            Values::Text(_) | Values::Utf8(_) | Values::Dict { .. } => None,
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

    pub fn text(&self, i: usize) -> Option<&str> {
        match self {
            Values::Text(v) => v[i].as_deref(),
            Values::Utf8(v) => v[i]
                .as_ref()
                .map(|b| std::str::from_utf8(b.data()).unwrap_or_default()),
            Values::Dict { dict, codes } => {
                codes[i].map(|c| std::str::from_utf8(dict[c as usize].data()).unwrap_or_default())
            }
            Values::Int(_) | Values::Wide(_) | Values::Float4(_) | Values::Float8(_) => None,
        }
    }

    /// Text at row `i` as shared bytes: without a copy where it was decoded from its pages.
    pub fn text_value(&self, i: usize) -> Option<ByteArray> {
        match self {
            Values::Utf8(v) => v[i].clone(),
            Values::Dict { dict, codes } => codes[i].map(|c| dict[c as usize].clone()),
            Values::Text(v) => v[i].as_deref().map(ByteArray::from),
            Values::Int(_) | Values::Wide(_) | Values::Float4(_) | Values::Float8(_) => None,
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
            | Values::Float8(_) => None,
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
        }
    }

    /// Appends row `i` of `other`, a column of the same kind.
    pub fn push_from(&mut self, other: &Values, i: usize) {
        match (self, other) {
            (Values::Int(a), Values::Int(b)) => a.push(b[i]),
            (Values::Text(a), Values::Text(b)) => a.push(b[i].clone()),
            (Values::Utf8(a), Values::Utf8(b)) => a.push(b[i].clone()),
            (Values::Utf8(a), b @ Values::Dict { .. }) => a.push(b.text_value(i)),
            (Values::Wide(a), Values::Wide(b)) => a.push(b[i]),
            (Values::Float4(a), Values::Float4(b)) => a.push(b[i]),
            (Values::Float8(a), Values::Float8(b)) => a.push(b[i]),
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
            Values::Int(_) | Values::Wide(_) | Values::Float4(_) | Values::Float8(_) => 0,
        }
    }

    /// The bytes row `i` takes written plain: a number its width, text its bytes and its length, and
    /// a NULL none.
    pub fn plain(&self, i: usize, ty: ColumnType) -> u64 {
        if self.is_null(i) {
            return 0;
        }
        match (self, ty.width()) {
            (_, Some(w)) => w as u64,
            (Values::Text(v), None) => v[i].as_ref().map_or(0, |s| s.len() as u64 + LENGTH_PREFIX),
            (Values::Utf8(v), None) => v[i].as_ref().map_or(0, |b| b.len() as u64 + LENGTH_PREFIX),
            (Values::Dict { dict, codes }, None) => {
                codes[i].map_or(0, |c| dict[c as usize].len() as u64 + LENGTH_PREFIX)
            }
            (Values::Int(_) | Values::Wide(_) | Values::Float4(_) | Values::Float8(_), None) => 0,
        }
    }

    /// A column's values as the leaves of its type hold them, one `Values` a leaf: its own where it
    /// is one leaf.
    pub fn into_leaves(self, _ty: ColumnType) -> Vec<Values> {
        vec![self]
    }

    /// A column's values from its leaves', each of the same rows: its one leaf's own where it is
    /// one.
    pub fn from_leaves(ty: ColumnType, leaves: Vec<Values>) -> Values {
        leaves
            .into_iter()
            .next()
            .unwrap_or_else(|| Values::new(ty))
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
            Values::Int(_) | Values::Wide(_) | Values::Float4(_) | Values::Float8(_) => 0,
        }
    }
}
