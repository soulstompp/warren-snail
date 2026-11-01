//! The answer as rows arrive: the rows themselves, or the groups they fall in, then ordered and
//! returned as Arrow columns. Text read from a dictionary arrives as its code: rows are grouped by
//! their codes while the dictionaries stand, and each group's codes become text when they change.

use super::hash::WordMap;
use super::question::{Countable, Item, Key, Select, Sort};
use crate::catalog::{ColumnType, Table};
use crate::Result;
use arrow::array::{
    ArrayRef, BooleanArray, Decimal128Array, FixedSizeBinaryArray, Float32Array, Float64Array,
    Int16Array, Int32Array, Int64Array, RecordBatch, RecordBatchOptions, StringArray,
    TimestampMicrosecondArray,
};
use arrow::datatypes::{Field, Schema};
use parquet::data_type::ByteArray;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::sync::Arc;

/// Text a round keeps, sharing its bytes with the page it was read from, compared by its bytes.
#[derive(Clone, Debug)]
pub struct Text(pub ByteArray);

impl Text {
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(self.0.data()).unwrap_or_default()
    }
}

impl PartialEq for Text {
    fn eq(&self, other: &Self) -> bool {
        self.0.data() == other.0.data()
    }
}

impl Eq for Text {}

impl std::hash::Hash for Text {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.data().hash(state)
    }
}

impl PartialOrd for Text {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Text {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.data().cmp(other.0.data())
    }
}

/// A value compared by a key other than its own bytes, a bpchar's without its trailing blanks:
/// compared, grouped and hashed by its key, and written as its own bytes.
#[derive(Clone, Debug)]
pub struct Keyed {
    pub key: Box<[u8]>,
    pub image: Text,
}

impl Keyed {
    pub fn new(key: Box<[u8]>, image: ByteArray) -> Keyed {
        Keyed {
            key,
            image: Text(image),
        }
    }
}

impl PartialEq for Keyed {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl Eq for Keyed {}

impl std::hash::Hash for Keyed {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.key.hash(state)
    }
}

impl PartialOrd for Keyed {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Keyed {
    fn cmp(&self, other: &Self) -> Ordering {
        self.key.cmp(&other.key)
    }
}

/// A running sum of money with `step` added: past the range money holds, it stays past it.
fn held_sum(total: i128, step: i128) -> i128 {
    if total == PAST_RANGE {
        return PAST_RANGE;
    }
    let t = total + step;
    if i64::try_from(t).is_ok() {
        t
    } else {
        PAST_RANGE
    }
}

/// An interval a round keeps: compared, grouped and hashed by its total, as PostgreSQL's `=` sees
/// it, and written out as its own fields.
#[derive(Clone, Copy, Debug)]
pub struct Lapse(pub crate::times::Interval);

impl PartialEq for Lapse {
    fn eq(&self, other: &Self) -> bool {
        self.0.total() == other.0.total()
    }
}

impl Eq for Lapse {}

impl std::hash::Hash for Lapse {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.total().hash(state)
    }
}

impl PartialOrd for Lapse {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Lapse {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total().cmp(&other.0.total())
    }
}

/// A float a round keeps: its class's key, by which it is compared, grouped and hashed, and its own
/// bits, which it is written out as.
#[derive(Clone, Copy, Debug)]
pub struct Float {
    pub key: i64,
    pub bits: u64,
}

impl PartialEq for Float {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl Eq for Float {}

impl std::hash::Hash for Float {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.key.hash(state)
    }
}

impl PartialOrd for Float {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Float {
    fn cmp(&self, other: &Self) -> Ordering {
        self.key.cmp(&other.key)
    }
}

/// Whether a column's values are numerics held as their text.
fn is_classed(ty: Option<ColumnType>) -> bool {
    matches!(ty, Some(ColumnType::NumericText(_)))
}

/// Where an item of the answer is a numeric: the type it is written from, and whether it takes
/// NULL. A numeric column itself, its min, max and sum; a bigint's sum, at scale 0; an integer's or
/// a numeric's mean.
fn numeric_output(item: &Item, tables: &[&Table]) -> Option<(ColumnType, bool)> {
    let col = |c: &super::question::Col| &tables[c.side].columns[c.column];
    match item {
        Item::Key(Key::Column(c)) if col(c).ty.is_numeric() => Some((col(c).ty, !col(c).not_null)),
        Item::Least(c) | Item::Greatest(c) | Item::Sum(c) if col(c).ty.is_numeric() => {
            Some((col(c).ty, true))
        }
        Item::Sum(c) if col(c).ty == ColumnType::Int8 => Some((
            ColumnType::Numeric {
                precision: 38,
                scale: 0,
            },
            true,
        )),
        Item::Avg(c) if col(c).ty.is_numeric() || col(c).ty.is_integer() => {
            Some((ColumnType::NumericText(None), true))
        }
        _ => None,
    }
}

/// A numeric held as its text, as a round keeps it: compared, grouped and hashed by its class's
/// key, and written out as its own text.
#[derive(Clone, Debug)]
pub struct Classed {
    pub class: Text,
    pub image: Text,
}

impl PartialEq for Classed {
    fn eq(&self, other: &Self) -> bool {
        self.class == other.class
    }
}

impl Eq for Classed {}

impl std::hash::Hash for Classed {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.class.hash(state)
    }
}

impl PartialOrd for Classed {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Classed {
    fn cmp(&self, other: &Self) -> Ordering {
        self.class.cmp(&other.class)
    }
}

/// Adds one group's aggregates into another's.
fn merge(into: &mut [Acc], from: &[Acc]) {
    for (a, b) in into.iter_mut().zip(from) {
        match (a, b) {
            (Acc::Count(x), Acc::Count(y)) => *x += y,
            (Acc::Sum(x), Acc::Sum(Some(y))) => {
                *x = Some(match (x.unwrap_or(0), *y) {
                    (PAST_RANGE, _) | (_, PAST_RANGE) => PAST_RANGE,
                    (a, b) => a.saturating_add(b),
                })
            }
            (Acc::Least(x), Acc::Least(Some(y))) => *x = Some(x.map_or(*y, |x| x.min(*y))),
            (Acc::Greatest(x), Acc::Greatest(Some(y))) => *x = Some(x.map_or(*y, |x| x.max(*y))),
            (Acc::Extreme(x, least), Acc::Extreme(Some(y), _)) => {
                let better = x
                    .as_ref()
                    .is_none_or(|x| if *least { y < x } else { y > x });
                if better {
                    *x = Some(y.clone());
                }
            }
            (Acc::IntervalExtreme(x, least), Acc::IntervalExtreme(Some(y), _)) => {
                *x = Some(later_extreme(*x, *y, *least))
            }
            (Acc::IntervalSum(x, _, past), Acc::IntervalSum(y, _, o)) => {
                *past |= *o || x.combine(y).is_err();
            }
            (Acc::FloatSum(x, over), Acc::FloatSum(Some(y), o)) => {
                *x = Some(x.map_or(*y, |x| x + y));
                *over |= *o;
            }
            (Acc::FloatSum(_, over), Acc::FloatSum(None, o)) => *over |= *o,
            (Acc::Cell(x, greatest), Acc::Cell(Some(y), _)) => {
                let take = x
                    .as_ref()
                    .is_none_or(|x| if *greatest { y > x } else { y < x });
                if take {
                    *x = Some(y.clone());
                }
            }
            (Acc::Avg(n, sx, sxx, over), Acc::Avg(m, sy, syy, o)) => {
                // Two runs of rows combined as PostgreSQL's float8_combine combines them.
                if *m > 0.0 {
                    if *n == 0.0 {
                        (*n, *sx, *sxx) = (*m, *sy, *syy);
                    } else {
                        let total = *n + m;
                        let t = *sx / *n - sy / m;
                        *sxx = *sxx + syy + *n * m * t * t / total;
                        *sx += sy;
                        *n = total;
                    }
                }
                *over |= *o;
            }
            (Acc::Numeric(x), Acc::Numeric(y)) | (Acc::NumAvg(x), Acc::NumAvg(y)) => x.merge(y),
            (Acc::LeastCell(x), Acc::LeastCell(Some(y))) if x.as_ref().is_none_or(|x| y <= x) => {
                *x = Some(y.clone());
            }
            (Acc::GreatestCell(x), Acc::GreatestCell(Some(y)))
                if x.as_ref().is_none_or(|x| y >= x) =>
            {
                *x = Some(y.clone());
            }
            _ => {}
        }
    }
}

/// Of an interval's extreme so far and the next, the one an aggregate keeps: the next where they
/// are equal.
fn later_extreme(
    x: Option<crate::times::Interval>,
    y: crate::times::Interval,
    least: bool,
) -> crate::times::Interval {
    match x {
        Some(x) if least && x.total() < y.total() => x,
        Some(x) if !least && x.total() > y.total() => x,
        _ => y,
    }
}

/// A float's value, a real's widened, from its bits.
fn float_value(f: &Float, wide: bool) -> f64 {
    if wide {
        f64::from_bits(f.bits)
    } else {
        f64::from(f32::from_bits(f.bits as u32))
    }
}

/// A float as a round keeps it, a real where `wide` is not.
fn float_cell(v: f64, wide: bool) -> Cell {
    if wide {
        Cell::Float(Float {
            key: crate::scalars::float8_key(v.to_bits()),
            bits: v.to_bits(),
        })
    } else {
        let b = (v as f32).to_bits();
        Cell::Float(Float {
            key: crate::scalars::float4_key(b),
            bits: u64::from(b),
        })
    }
}

/// One value of a row a round keeps.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum Cell {
    Null,
    Int(i64),
    Text(Text),
    /// Text as its place in the dictionary of the rows now arriving.
    Code(u32),
    /// A value keyed in 128 bits, as its key.
    Wide(i128),
    /// A float, compared by its class and written as its own bits.
    Float(Float),
    /// A value compared by a key other than its own bytes, and written as its own bytes.
    Keyed(Keyed),
    /// A numeric held as its text, compared by its class and written as its own text.
    Classed(Classed),
    /// A composed value, compared by its key and written as its parts.
    Composed(Arc<crate::composed::Compound>),
    /// An interval, compared by its total and written as its own fields.
    Interval(Lapse),
}

impl Cell {
    /// A numeric's own text, kept beside its class.
    pub fn classed(image: ByteArray) -> Cell {
        let key = crate::numeric::class_of_text(std::str::from_utf8(image.data()).unwrap_or(""));
        Cell::Classed(Classed {
            class: Text(ByteArray::from(key.as_str())),
            image: Text(image),
        })
    }

    /// A numeric, kept beside its class.
    pub fn of_numeric(n: &crate::numeric::Numeric) -> Cell {
        Cell::Classed(Classed {
            class: Text(ByteArray::from(n.class_key().as_str())),
            image: Text(ByteArray::from(n.to_string().as_str())),
        })
    }

    /// The numeric a cell of a column of type `ty` holds: a numeric's, or an integer's.
    pub fn numeric(&self, ty: ColumnType) -> Option<crate::numeric::Numeric> {
        match (self, ty) {
            (Cell::Int(u), ColumnType::Numeric { precision, scale }) => Some(
                crate::numeric::column_numeric(i128::from(*u), precision, scale),
            ),
            (Cell::Wide(u), ColumnType::Numeric { precision, scale }) => {
                Some(crate::numeric::column_numeric(*u, precision, scale))
            }
            (Cell::Int(v), _) => Some(crate::numeric::Numeric::of_int(*v)),
            (Cell::Classed(c), _) => crate::numeric::Numeric::parse(c.image.as_str()).ok(),
            _ => None,
        }
    }
    /// A number's or a wide value's key, in 128 bits.
    pub fn key(&self) -> Option<i128> {
        match self {
            Cell::Int(v) => Some(i128::from(*v)),
            Cell::Wide(k) => Some(*k),
            // A float's key, its bits beside it, so the least and the greatest keep their own.
            Cell::Float(f) => Some((i128::from(f.key) << 64) | i128::from(f.bits)),
            Cell::Interval(v) => Some(v.0.total()),
            _ => None,
        }
    }
}

/// The dictionary a column's codes index in the rows now arriving: none for a column read otherwise.
pub type Dict = Option<Arc<[ByteArray]>>;

/// A cell as text, where it is a code into `dict`.
pub fn as_text(c: &Cell, dict: &Dict) -> Cell {
    match (c, dict) {
        (Cell::Code(k), Some(d)) => Cell::Text(Text(d[*k as usize].clone())),
        _ => c.clone(),
    }
}

/// Whether two sets of dictionaries are the same ones.
pub fn same(a: &[Dict], b: &[Dict]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| match (x, y) {
            (Some(x), Some(y)) => Arc::ptr_eq(x, y),
            (None, None) => true,
            _ => false,
        })
}

/// A group's aggregate as rows arrive.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Acc {
    Count(u64),
    Sum(Option<i128>),
    /// The least key so far.
    Least(Option<i128>),
    /// The greatest key so far.
    Greatest(Option<i128>),
    /// An interval's least, or its greatest where not `least`, so far: the later of two equal ones,
    /// as PostgreSQL's interval_smaller and interval_larger take it.
    IntervalExtreme(Option<crate::times::Interval>, bool),
    /// An interval's sum, or its average where `avg`, as PostgreSQL's interval_avg_accum folds it: a
    /// time's through its cast to an interval; and whether a step left the interval's range, which
    /// refuses the answer.
    IntervalSum(crate::times::IntervalAccum, bool, bool),
    /// A float's sum so far, and whether finite numbers made it infinite on the way.
    FloatSum(Option<f64>, bool),
    /// A float's rows, their sum and their sum of squared deviations, as PostgreSQL's
    /// float8_accum folds them, and whether finite numbers made either infinite on the way.
    Avg(f64, f64, f64, bool),
    /// The least value so far, or the greatest where `greatest`, of a column whose values are no
    /// number: a string's, bytea's, a bit string's.
    Cell(Option<Cell>, bool),
    /// A numeric's sum, or an integer's or a numeric's mean, as numeric_accum folds them.
    Numeric(Box<crate::numeric::Sum>),
    NumAvg(Box<crate::numeric::Sum>),
    /// The least and the greatest numeric held as its text, by its class.
    LeastCell(Option<Cell>),
    GreatestCell(Option<Cell>),
    /// A composed value's least (true) or greatest so far, by its key.
    Extreme(Option<Arc<crate::composed::Compound>>, bool),
}

/// The answer as rows arrive, each as the cells of the question's inputs.
pub struct Answer {
    /// For each input, the dictionary its codes index in the rows now arriving.
    dicts: Vec<Dict>,
    /// Whether any input has one.
    coded: bool,
    body: Body,
    /// For each item, the type of the column it aggregates, where it is known.
    typed: Vec<Option<ColumnType>>,
}

/// A running sum held past money's range: refused at the end, as PostgreSQL refuses the step that
/// leaves it.
const PAST_RANGE: i128 = i128::MAX;

enum Body {
    Rows(Vec<Vec<Cell>>),
    Groups {
        keys: usize,
        items: Vec<Item>,
        inputs: Vec<Key>,
        groups: BTreeMap<Vec<Cell>, Vec<Acc>>,
        /// The groups of the rows since the dictionaries last changed, by their codes.
        local: WordMap<Vec<Cell>, Vec<Acc>>,
        /// The same, where the one key is a code: by the code itself.
        by_code: Vec<Option<Vec<Acc>>>,
    },
}

impl Answer {
    pub fn new(select: &Select) -> Answer {
        let body = match select {
            Select::Rows(_) => Body::Rows(Vec::new()),
            Select::Groups { keys, items } => Body::Groups {
                keys: keys.len(),
                items: items.iter().map(|(_, i)| *i).collect(),
                inputs: select.inputs(),
                groups: BTreeMap::new(),
                local: WordMap::default(),
                by_code: Vec::new(),
            },
        };
        Answer {
            dicts: vec![None; select.inputs().len()],
            coded: false,
            body,
            typed: Vec::new(),
        }
    }

    /// The answer told the types of the columns its aggregates read, `tables` side by side: a sum
    /// of money held to its range row by row, and a `"char"`'s min and max taken by its text.
    pub(crate) fn typed(mut self, tables: &[&Table]) -> Answer {
        if let Body::Groups { items, .. } = &self.body {
            self.typed = items
                .iter()
                .map(|i| match i {
                    Item::Sum(c) | Item::Least(c) | Item::Greatest(c) | Item::Avg(c) => {
                        Some(tables[c.side].columns[c.column].ty)
                    }
                    _ => None,
                })
                .collect();
        }
        self
    }

    /// The dictionaries the codes of the rows that follow index, one for each input.
    pub fn dictionaries(&mut self, dicts: &[Dict]) {
        if same(&self.dicts, dicts) {
            return;
        }
        self.flush();
        self.dicts = dicts.to_vec();
        self.coded = dicts.iter().any(Option::is_some);
    }

    /// Moves the groups by codes into the groups by text.
    fn flush(&mut self) {
        let dicts = &self.dicts;
        if let Body::Groups {
            groups,
            local,
            by_code,
            ..
        } = &mut self.body
        {
            let coded = by_code
                .drain(..)
                .enumerate()
                .filter_map(|(c, a)| a.map(|a| (vec![Cell::Code(c as u32)], a)));
            for (key, accs) in local.drain().chain(coded) {
                let key: Vec<Cell> = key.iter().zip(dicts).map(|(c, d)| as_text(c, d)).collect();
                match groups.get_mut(&key) {
                    Some(into) => merge(into, &accs),
                    None => {
                        groups.insert(key, accs);
                    }
                }
            }
        }
    }

    fn fresh(items: &[Item], typed: &[Option<ColumnType>]) -> Vec<Acc> {
        items
            .iter()
            .enumerate()
            .map(|(at, i)| match i {
                Item::Sum(_)
                    if typed
                        .get(at)
                        .copied()
                        .flatten()
                        .is_some_and(ColumnType::is_float) =>
                {
                    Acc::FloatSum(None, false)
                }
                Item::Sum(_)
                    if typed
                        .get(at)
                        .copied()
                        .flatten()
                        .is_some_and(ColumnType::is_numeric) =>
                {
                    Acc::Numeric(Box::default())
                }
                Item::Avg(_)
                    if typed
                        .get(at)
                        .copied()
                        .flatten()
                        .is_some_and(|t| t.is_numeric() || t.is_integer()) =>
                {
                    Acc::NumAvg(Box::default())
                }
                Item::Sum(_) | Item::Avg(_)
                    if matches!(
                        typed.get(at).copied().flatten(),
                        Some(ColumnType::Interval { .. } | ColumnType::Time(_))
                    ) =>
                {
                    Acc::IntervalSum(Default::default(), matches!(i, Item::Avg(_)), false)
                }
                Item::Avg(_) => Acc::Avg(0.0, 0.0, 0.0, false),
                Item::Least(_) | Item::Greatest(_)
                    if is_classed(typed.get(at).copied().flatten()) =>
                {
                    if matches!(i, Item::Least(_)) {
                        Acc::LeastCell(None)
                    } else {
                        Acc::GreatestCell(None)
                    }
                }
                Item::Least(_) | Item::Greatest(_)
                    if typed
                        .get(at)
                        .copied()
                        .flatten()
                        .is_some_and(|t| t.is_byte_array() || t.is_bits()) =>
                {
                    Acc::Cell(None, matches!(i, Item::Greatest(_)))
                }
                Item::Sum(_) => Acc::Sum(None),
                Item::Least(_) | Item::Greatest(_)
                    if typed
                        .get(at)
                        .copied()
                        .flatten()
                        .is_some_and(ColumnType::is_nested) =>
                {
                    Acc::Extreme(None, matches!(i, Item::Least(_)))
                }
                Item::Least(_) | Item::Greatest(_)
                    if matches!(
                        typed.get(at).copied().flatten(),
                        Some(ColumnType::Interval { .. })
                    ) =>
                {
                    Acc::IntervalExtreme(None, matches!(i, Item::Least(_)))
                }
                Item::Least(_) => Acc::Least(None),
                Item::Greatest(_) => Acc::Greatest(None),
                _ => Acc::Count(0),
            })
            .collect()
    }

    pub fn add(&mut self, cells: &[Cell]) {
        self.add_times(cells, 1);
    }

    /// Adds `n` rows of these cells.
    pub fn add_times(&mut self, cells: &[Cell], n: u64) {
        let typed = &self.typed;
        let dicts = &self.dicts;
        match &mut self.body {
            Body::Rows(rows) => {
                let row: Vec<Cell> = if self.coded {
                    cells
                        .iter()
                        .zip(&self.dicts)
                        .map(|(c, d)| as_text(c, d))
                        .collect()
                } else {
                    cells.to_vec()
                };
                for _ in 1..n {
                    rows.push(row.clone());
                }
                if n > 0 {
                    rows.push(row);
                }
            }
            Body::Groups {
                keys,
                items,
                inputs,
                local,
                by_code,
                ..
            } => {
                let accs = match (*keys, cells.first()) {
                    (1, Some(&Cell::Code(c))) => {
                        let c = c as usize;
                        if by_code.len() <= c {
                            by_code.resize(c + 1, None);
                        }
                        by_code[c].get_or_insert_with(|| Answer::fresh(items, typed))
                    }
                    _ => match local.get_mut(&cells[..*keys]) {
                        Some(a) => a,
                        None => local
                            .entry(cells[..*keys].to_vec())
                            .or_insert_with(|| Answer::fresh(items, typed)),
                    },
                };
                for (i, (acc, item)) in accs.iter_mut().zip(items.iter()).enumerate() {
                    let ty = typed.get(i).copied().flatten();
                    match (acc, item) {
                        (Acc::Count(c), Item::Count) => *c += n,
                        (Acc::Sum(total), Item::Sum(c)) => {
                            let at = inputs.iter().position(|k| *k == Key::Column(*c)).unwrap();
                            if let Cell::Int(v) = cells[at] {
                                let t = total.unwrap_or(0);
                                *total = Some(if matches!(ty, Some(ColumnType::Money(_))) {
                                    held_sum(t, v as i128 * n as i128)
                                } else {
                                    t + v as i128 * n as i128
                                });
                            }
                        }
                        (Acc::FloatSum(total, over), Item::Sum(c)) => {
                            let at = inputs.iter().position(|k| *k == Key::Column(*c)).unwrap();
                            if let Cell::Float(f) = &cells[at] {
                                let wide = ty == Some(ColumnType::Float8);
                                let x = float_value(f, wide);
                                for _ in 0..n {
                                    let t = total.unwrap_or(0.0);
                                    let s = if total.is_none() {
                                        x
                                    } else if wide {
                                        t + x
                                    } else {
                                        f64::from(t as f32 + x as f32)
                                    };
                                    *over |=
                                        s.is_infinite() && !t.is_infinite() && !x.is_infinite();
                                    *total = Some(s);
                                }
                            }
                        }
                        (Acc::Avg(count, sx, sxx, over), Item::Avg(c)) => {
                            let at = inputs.iter().position(|k| *k == Key::Column(*c)).unwrap();
                            if let Cell::Float(f) = &cells[at] {
                                let x = float_value(f, ty == Some(ColumnType::Float8));
                                for _ in 0..n {
                                    let (n0, sx0) = (*count, *sx);
                                    *count = n0 + 1.0;
                                    *sx = sx0 + x;
                                    if n0 > 0.0 {
                                        let t = x * *count - *sx;
                                        *sxx += t * t / (*count * n0);
                                        if sx.is_infinite() || sxx.is_infinite() {
                                            *over |= !sx0.is_infinite() && !x.is_infinite();
                                            *sxx = f64::NAN;
                                        }
                                    } else if x.is_nan() || x.is_infinite() {
                                        *sxx = f64::NAN;
                                    }
                                }
                            }
                        }
                        (Acc::Cell(m, greatest), Item::Least(c) | Item::Greatest(c)) => {
                            let at = inputs.iter().position(|k| *k == Key::Column(*c)).unwrap();
                            if cells[at] != Cell::Null {
                                let y = as_text(&cells[at], &dicts[at]);
                                let take =
                                    m.as_ref()
                                        .is_none_or(|x| if *greatest { &y > x } else { &y < x });
                                if take {
                                    *m = Some(y);
                                }
                            }
                        }
                        (Acc::Extreme(m, least), Item::Least(c) | Item::Greatest(c)) => {
                            let at = inputs.iter().position(|k| *k == Key::Column(*c)).unwrap();
                            if let Cell::Composed(v) = &cells[at] {
                                let better =
                                    m.as_ref()
                                        .is_none_or(|x| if *least { v < x } else { v > x });
                                if better {
                                    *m = Some(v.clone());
                                }
                            }
                        }
                        (Acc::IntervalSum(s, _, past), Item::Sum(c) | Item::Avg(c)) => {
                            let at = inputs.iter().position(|k| *k == Key::Column(*c)).unwrap();
                            let v = match &cells[at] {
                                Cell::Interval(v) => Some(v.0),
                                Cell::Int(t) => Some(crate::times::time_interval(*t)),
                                _ => None,
                            };
                            if let Some(v) = v {
                                for _ in 0..n {
                                    *past |= s.add(v).is_err();
                                }
                            }
                        }
                        (Acc::IntervalExtreme(m, least), Item::Least(c) | Item::Greatest(c)) => {
                            let at = inputs.iter().position(|k| *k == Key::Column(*c)).unwrap();
                            if let Cell::Interval(v) = cells[at] {
                                *m = Some(later_extreme(*m, v.0, *least));
                            }
                        }
                        (Acc::Least(m), Item::Least(c)) | (Acc::Greatest(m), Item::Greatest(c)) => {
                            let at = inputs.iter().position(|k| *k == Key::Column(*c)).unwrap();
                            let least = matches!(item, Item::Least(_));
                            let v = match (ty, &cells[at]) {
                                (Some(ColumnType::Char), Cell::Int(k)) => {
                                    Some(i128::from(crate::scalars::char_text_rank(
                                        crate::scalars::unsigned_of(*k, 8) as u8,
                                    )))
                                }
                                (_, c) => c.key(),
                            };
                            if let Some(v) = v {
                                *m = Some(match *m {
                                    Some(x) if least => x.min(v),
                                    Some(x) => x.max(v),
                                    None => v,
                                });
                            }
                        }
                        (Acc::Numeric(s), Item::Sum(c)) | (Acc::NumAvg(s), Item::Avg(c)) => {
                            let at = inputs.iter().position(|k| *k == Key::Column(*c)).unwrap();
                            if let Some(x) = ty.and_then(|t| cells[at].numeric(t)) {
                                s.add_times(&x, n);
                            }
                        }
                        // Of two numerics equal, numeric_smaller and numeric_larger keep the later.
                        (Acc::LeastCell(m), Item::Least(c)) => {
                            let at = inputs.iter().position(|k| *k == Key::Column(*c)).unwrap();
                            let v = &cells[at];
                            if *v != Cell::Null && m.as_ref().is_none_or(|x| v <= x) {
                                *m = Some(v.clone());
                            }
                        }
                        (Acc::GreatestCell(m), Item::Greatest(c)) => {
                            let at = inputs.iter().position(|k| *k == Key::Column(*c)).unwrap();
                            let v = &cells[at];
                            if *v != Cell::Null && m.as_ref().is_none_or(|x| v >= x) {
                                *m = Some(v.clone());
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    /// Takes a run of rows at once, where the answer groups on one column read by its codes and only
    /// counts rows or sums whole numbers: each code's rows counted, and its sums added, in one pass.
    /// Whether it took them.
    pub fn run(
        &mut self,
        outs: &[(bool, bool, usize, &crate::values::Values)],
        from: usize,
        to: usize,
    ) -> bool {
        // A money sum is held to its range row by row, and a "char"'s min and max go by its text.
        if self.typed.iter().flatten().any(|t| {
            matches!(
                t,
                ColumnType::Money(_) | ColumnType::Char | ColumnType::Float4 | ColumnType::Float8
            )
        }) {
            return false;
        }
        let typed = &self.typed;
        let Body::Groups {
            keys,
            items,
            inputs,
            by_code,
            ..
        } = &mut self.body
        else {
            return false;
        };
        let (_, month, start, crate::values::Values::Dict { dict, codes }) = outs[0] else {
            return false;
        };
        if *keys != 1 || month {
            return false;
        }
        // Each item: a count, a sum of the input at this place, or the key.
        let mut summed = Vec::with_capacity(items.len());
        for i in items.iter() {
            summed.push(match i {
                Item::Count | Item::Key(_) => None,
                Item::Least(_) | Item::Greatest(_) | Item::Avg(_) => return false,
                Item::Sum(c) => match inputs.iter().position(|k| *k == Key::Column(*c)) {
                    Some(at) if !outs[at].1 && outs[at].3.ints().is_some() => Some(at),
                    _ => return false,
                },
            });
        }
        let codes = &codes[from - start..to - start];
        if codes.iter().any(Option::is_none) {
            return false;
        }
        let n = dict.len();
        let mut count = vec![0u64; n];
        let mut sums: Vec<Vec<Option<i128>>> = summed
            .iter()
            .map(|s| {
                if s.is_some() {
                    vec![None; n]
                } else {
                    Vec::new()
                }
            })
            .collect();
        // A numeric's NaN is left to the rows one at a time.
        let nan: Vec<Option<i64>> = typed
            .iter()
            .map(|t| match t {
                Some(ColumnType::Numeric { precision, .. }) => {
                    Some(crate::numeric::nan_units(*precision) as i64)
                }
                _ => None,
            })
            .collect();
        for (i, c) in codes.iter().enumerate() {
            let c = c.unwrap() as usize;
            count[c] += 1;
            for (item, (s, at)) in sums.iter_mut().zip(&summed).enumerate() {
                if let Some(at) = at {
                    let (_, _, st, v) = outs[*at];
                    if let Some(x) = v.int(from + i - st) {
                        if nan.get(item).copied().flatten() == Some(x) {
                            return false;
                        }
                        s[c] = Some(s[c].unwrap_or(0) + x as i128);
                    }
                }
            }
        }
        if by_code.len() < n {
            by_code.resize(n, None);
        }
        for c in (0..n).filter(|&c| count[c] > 0) {
            let accs = by_code[c].get_or_insert_with(|| Answer::fresh(items, typed));
            for (at, ((acc, item), s)) in accs.iter_mut().zip(items.iter()).zip(&sums).enumerate() {
                match (acc, item) {
                    (Acc::Count(k), Item::Count) => *k += count[c],
                    (Acc::Sum(total), Item::Sum(_)) => {
                        if let Some(x) = s[c] {
                            *total = Some(total.unwrap_or(0) + x);
                        }
                    }
                    (Acc::Numeric(sum), Item::Sum(_)) => {
                        if let (Some(x), Some(ColumnType::Numeric { scale, .. })) =
                            (s[c], typed.get(at).copied().flatten())
                        {
                            sum.add_units(x, u32::from(scale));
                        }
                    }
                    _ => {}
                }
            }
        }
        true
    }

    /// Adds the rows the counts answered, in all or on each month.
    pub fn counted(&mut self, total: u64, months: &[u64; 12], countable: Option<Countable>) {
        let typed = &self.typed;
        let (items, groups) = match &mut self.body {
            Body::Groups { items, groups, .. } => (items.clone(), groups),
            Body::Rows(_) => return,
        };
        let mut add = |key: Vec<Cell>, n: u64| {
            let accs = groups
                .entry(key)
                .or_insert_with(|| Answer::fresh(&items, typed));
            for (acc, item) in accs.iter_mut().zip(&items) {
                if let (Acc::Count(c), Item::Count) = (acc, item) {
                    *c += n;
                }
            }
        };
        match countable {
            Some(Countable::Total) if total > 0 => add(Vec::new(), total),
            Some(Countable::ByMonth(_)) => {
                for (m, &n) in months.iter().enumerate() {
                    if n > 0 {
                        add(vec![Cell::Int(m as i64 + 1)], n);
                    }
                }
            }
            _ => {}
        }
    }

    /// The answer as Arrow columns, one for each item of the select list, in the order asked.
    /// `tables` are the question's tables, side by side.
    pub fn finish(
        mut self,
        tables: &[&Table],
        select: &Select,
        order: &[Sort],
    ) -> Result<RecordBatch> {
        self.flush();
        let typed = self.typed.clone();
        let typed = &typed;
        let columns: Vec<(String, Item)> = match select {
            Select::Rows(keys) => keys
                .iter()
                .map(|(n, k)| (n.clone(), Item::Key(*k)))
                .collect(),
            Select::Groups { items, .. } => items.clone(),
        };
        let mut rows: Vec<Vec<Cell>> = match self.body {
            Body::Rows(rows) => rows,
            Body::Groups {
                keys,
                items,
                mut groups,
                ..
            } => {
                let key_list = match select {
                    Select::Groups { keys, .. } => keys.clone(),
                    Select::Rows(_) => unreachable!(),
                };
                if keys == 0 && groups.is_empty() {
                    groups.insert(Vec::new(), Answer::fresh(&items, typed));
                }
                // A sum of money past its range is refused, as PostgreSQL refuses it.
                for accs in groups.values() {
                    for (item, acc) in items.iter().zip(accs) {
                        if let (Item::Sum(c), Acc::Sum(Some(t))) = (item, acc) {
                            if matches!(tables[c.side].columns[c.column].ty, ColumnType::Money(_))
                                && i64::try_from(*t).is_err()
                            {
                                return Err(crate::Error::new("money out of range"));
                            }
                        }
                        // So is a float's that finite numbers made infinite.
                        if let Acc::FloatSum(_, true) | Acc::Avg(_, _, _, true) = acc {
                            return Err(crate::Error::new("value out of range: overflow"));
                        }
                        // And a numeric's past what a numeric holds.
                        if let Acc::Numeric(s) = acc {
                            s.total().transpose()?;
                        }
                        if let Acc::NumAvg(s) = acc {
                            s.avg().transpose()?;
                        }
                        // And an interval's whose steps left its range, or that holds both
                        // infinities.
                        if let Acc::IntervalSum(s, avg, past) = acc {
                            if *past {
                                return Err(crate::Error::new("interval out of range"));
                            }
                            if *avg {
                                s.avg()?;
                            } else {
                                s.sum()?;
                            }
                        }
                    }
                }
                groups
                    .into_iter()
                    .map(|(key, accs)| {
                        items
                            .iter()
                            .zip(accs)
                            .map(|(item, acc)| match (item, acc) {
                                (Item::Key(k), _) => {
                                    key[key_list.iter().position(|x| x == k).unwrap()].clone()
                                }
                                (_, Acc::Count(n)) => Cell::Int(n as i64),
                                // A bigint's sum is a numeric; a smallint's and an integer's a
                                // bigint, wrapping as PostgreSQL's int2_sum and int4_sum add.
                                (Item::Sum(c), Acc::Sum(total))
                                    if tables[c.side].columns[c.column].ty == ColumnType::Int8 =>
                                {
                                    total.map_or(Cell::Null, |t| {
                                        if t.unsigned_abs() < 10u128.pow(38) {
                                            Cell::Wide(t)
                                        } else {
                                            Cell::of_numeric(&crate::numeric::Numeric::of_units(
                                                t, 0,
                                            ))
                                        }
                                    })
                                }
                                (_, Acc::Sum(total)) => {
                                    total.map_or(Cell::Null, |t| Cell::Int(t as i64))
                                }
                                (
                                    Item::Least(c) | Item::Greatest(c),
                                    Acc::Least(m) | Acc::Greatest(m),
                                ) if tables[c.side].columns[c.column].ty == ColumnType::Char => m
                                    .map_or(Cell::Null, |r| {
                                        let b = crate::scalars::char_of_text_rank(r as i64);
                                        let t = crate::scalars::scalar_text(
                                            ColumnType::Char,
                                            crate::scalars::unsigned_key(u64::from(b), 8),
                                        );
                                        Cell::Text(Text(ByteArray::from(t.as_str())))
                                    }),
                                (
                                    Item::Least(c) | Item::Greatest(c),
                                    Acc::Least(m) | Acc::Greatest(m),
                                ) if tables[c.side].columns[c.column].ty.is_float() => {
                                    m.map_or(Cell::Null, |v| {
                                        Cell::Float(Float {
                                            key: (v >> 64) as i64,
                                            bits: v as u64,
                                        })
                                    })
                                }
                                (
                                    Item::Least(c) | Item::Greatest(c),
                                    Acc::Least(m) | Acc::Greatest(m),
                                ) if tables[c.side].columns[c.column].ty.is_wide() => {
                                    m.map_or(Cell::Null, Cell::Wide)
                                }
                                (_, Acc::Extreme(m, _)) => m.map_or(Cell::Null, Cell::Composed),
                                (_, Acc::Least(m) | Acc::Greatest(m)) => {
                                    m.map_or(Cell::Null, |v| Cell::Int(v as i64))
                                }
                                (_, Acc::Cell(m, _)) => m.unwrap_or(Cell::Null),
                                (_, Acc::LeastCell(m) | Acc::GreatestCell(m)) => {
                                    m.unwrap_or(Cell::Null)
                                }
                                // A numeric(p, s)'s sum as its units at s, where 38 digits hold
                                // them; else, and any other numeric's, beside its class.
                                (Item::Sum(c), Acc::Numeric(s)) => match s.total() {
                                    None => Cell::Null,
                                    Some(t) => {
                                        let t = t.expect("a sum held");
                                        let units = match tables[c.side].columns[c.column].ty {
                                            ColumnType::Numeric { scale, .. } => t
                                                .units_at(u32::from(scale))
                                                .filter(|u| u.unsigned_abs() < 10u128.pow(38)),
                                            _ => None,
                                        };
                                        units.map_or_else(|| Cell::of_numeric(&t), Cell::Wide)
                                    }
                                },
                                (_, Acc::NumAvg(s)) => s.avg().map_or(Cell::Null, |a| {
                                    Cell::of_numeric(&a.expect("a mean held"))
                                }),
                                (_, Acc::Numeric(_)) => Cell::Null,
                                (_, Acc::IntervalExtreme(m, _)) => {
                                    m.map_or(Cell::Null, |v| Cell::Interval(Lapse(v)))
                                }
                                (_, Acc::IntervalSum(s, avg, _)) => {
                                    let v = if avg { s.avg() } else { s.sum() };
                                    v.ok()
                                        .flatten()
                                        .map_or(Cell::Null, |v| Cell::Interval(Lapse(v)))
                                }
                                (Item::Sum(c), Acc::FloatSum(total, _)) => {
                                    total.map_or(Cell::Null, |t| {
                                        float_cell(
                                            t,
                                            tables[c.side].columns[c.column].ty
                                                == ColumnType::Float8,
                                        )
                                    })
                                }
                                (_, Acc::Avg(n, sx, _, _)) if n > 0.0 => float_cell(sx / n, true),
                                (_, Acc::Avg(..) | Acc::FloatSum(..)) => Cell::Null,
                            })
                            .collect()
                    })
                    .collect()
            }
        };
        rows.sort_by(|a, b| {
            for s in order {
                let ord = match (&a[s.at], &b[s.at]) {
                    (Cell::Null, Cell::Null) => Ordering::Equal,
                    (Cell::Null, _) => {
                        if s.nulls_first {
                            Ordering::Less
                        } else {
                            Ordering::Greater
                        }
                    }
                    (_, Cell::Null) => {
                        if s.nulls_first {
                            Ordering::Greater
                        } else {
                            Ordering::Less
                        }
                    }
                    (x, y) => {
                        if s.desc {
                            y.cmp(x)
                        } else {
                            x.cmp(y)
                        }
                    }
                };
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            Ordering::Equal
        });
        let mut fields = Vec::new();
        let mut arrays: Vec<ArrayRef> = Vec::new();
        for (i, (name, item)) in columns.iter().enumerate() {
            let ints = || {
                rows.iter().map(|r| match &r[i] {
                    Cell::Int(v) => Some(*v),
                    _ => None,
                })
            };
            let floats = || {
                rows.iter().map(|r| match &r[i] {
                    Cell::Float(f) => Some(f.bits),
                    _ => None,
                })
            };
            let char_text = matches!(*item, Item::Least(c) | Item::Greatest(c)
                if tables[c.side].columns[c.column].ty == ColumnType::Char);
            // A numeric: as its page form gives it, its sum at its scale up to 38 digits, a mean
            // and anything past them as its text; each named a numeric.
            if let Some((ty, nullable)) = numeric_output(item, tables) {
                let units = || {
                    rows.iter().map(|r| match &r[i] {
                        Cell::Int(v) => Some(i128::from(*v)),
                        Cell::Wide(v) => Some(*v),
                        _ => None,
                    })
                };
                let texted = rows.iter().any(|r| matches!(r[i], Cell::Classed(_)));
                let (a, field): (ArrayRef, Field) = match (ty, item) {
                    (ColumnType::Numeric { scale, .. }, Item::Sum(_)) if !texted => (
                        Arc::new(
                            units()
                                .collect::<Decimal128Array>()
                                .with_precision_and_scale(38, scale as i8)?,
                        ),
                        Field::new(
                            name,
                            arrow::datatypes::DataType::Decimal128(38, scale as i8),
                            nullable,
                        )
                        .with_metadata(crate::numeric::text_field(name, true).metadata().clone()),
                    ),
                    (
                        ColumnType::Numeric { precision, scale },
                        Item::Key(_) | Item::Least(_) | Item::Greatest(_),
                    ) => (
                        crate::numeric::decimal_array(precision, scale, units())?,
                        crate::numeric::numeric_field(name, ty, nullable),
                    ),
                    _ => (
                        Arc::new(
                            rows.iter()
                                .map(|r| match (&r[i], ty) {
                                    (Cell::Classed(c), _) => Some(c.image.as_str().to_string()),
                                    // A sum's units at its column's scale.
                                    (Cell::Wide(u), ColumnType::Numeric { scale, .. })
                                        if matches!(item, Item::Sum(_)) =>
                                    {
                                        Some(
                                            crate::numeric::Numeric::of_units(*u, u32::from(scale))
                                                .to_string(),
                                        )
                                    }
                                    (Cell::Null, _) => None,
                                    (other, _) => other.numeric(ty).map(|n| n.to_string()),
                                })
                                .collect::<StringArray>(),
                        ),
                        crate::numeric::text_field(name, nullable),
                    ),
                };
                fields.push(field);
                arrays.push(a);
                continue;
            }
            let (nullable, a): (bool, ArrayRef) = match *item {
                // PostgreSQL takes a "char"'s min and max through its text.
                Item::Least(_) | Item::Greatest(_) if char_text => (
                    true,
                    Arc::new(
                        rows.iter()
                            .map(|r| match &r[i] {
                                Cell::Text(t) => Some(t.as_str()),
                                _ => None,
                            })
                            .collect::<StringArray>(),
                    ),
                ),
                Item::Key(Key::Column(c)) | Item::Least(c) | Item::Greatest(c) => {
                    let col = &tables[c.side].columns[c.column];
                    let a: ArrayRef = match col.ty {
                        ColumnType::Boolean => {
                            Arc::new(ints().map(|v| v.map(|x| x != 0)).collect::<BooleanArray>())
                        }
                        ColumnType::Int2 => {
                            Arc::new(ints().map(|v| v.map(|x| x as i16)).collect::<Int16Array>())
                        }
                        ColumnType::Int4 => {
                            Arc::new(ints().map(|v| v.map(|x| x as i32)).collect::<Int32Array>())
                        }
                        ColumnType::Int8 => Arc::new(ints().collect::<Int64Array>()),
                        ColumnType::Timestamptz | ColumnType::TimestamptzRounded(_) => Arc::new(
                            ints()
                                .collect::<TimestampMicrosecondArray>()
                                .with_timezone("UTC"),
                        ),
                        // A cidr's min and max are inet's, an inet.
                        ColumnType::Textual(t)
                            if t.is_network() && !matches!(*item, Item::Key(_)) =>
                        {
                            let inet: Vec<Option<String>> = rows
                                .iter()
                                .map(|r| match &r[i] {
                                    Cell::Keyed(k) => crate::textual::cast(
                                        t,
                                        crate::textual::Textual::Inet,
                                        k.image.as_str(),
                                    )
                                    .ok(),
                                    _ => None,
                                })
                                .collect();
                            Arc::new(inet.iter().map(Option::as_deref).collect::<StringArray>())
                        }
                        ty if ty.is_byte_array() || ty.is_bits() => crate::strings::string_array(
                            ty,
                            rows.iter().map(|r| match &r[i] {
                                Cell::Text(t) => Some(t.0.data()),
                                Cell::Keyed(k) => Some(k.image.0.data()),
                                _ => None,
                            }),
                        ),
                        ColumnType::Uuid => {
                            Arc::new(FixedSizeBinaryArray::try_from_sparse_iter_with_size(
                                rows.iter().map(|r| match &r[i] {
                                    Cell::Wide(k) => Some(crate::scalars::uuid_bytes(*k)),
                                    _ => None,
                                }),
                                16,
                            )?)
                        }
                        ColumnType::Float4 => Arc::new(
                            floats()
                                .map(|b| b.map(|b| f32::from_bits(b as u32)))
                                .collect::<Float32Array>(),
                        ),
                        ColumnType::Float8 => Arc::new(
                            floats()
                                .map(|b| b.map(f64::from_bits))
                                .collect::<Float64Array>(),
                        ),
                        ty @ ColumnType::Composed(_) if ty.is_nested() => {
                            crate::composed::arrow_array(
                                ty,
                                rows.iter().map(|r| match &r[i] {
                                    Cell::Composed(c) => Some(&**c),
                                    _ => None,
                                }),
                            )?
                        }
                        ty @ ColumnType::Composed(_) => crate::composed::enum_array(ty, ints())?,
                        ColumnType::Interval { .. } => {
                            crate::scalars::interval_array(rows.iter().map(|r| match &r[i] {
                                Cell::Interval(v) => Some(v.0),
                                _ => None,
                            }))
                        }
                        ty => crate::scalars::scalar_array(ty, ints()),
                    };
                    (!col.not_null || !matches!(*item, Item::Key(_)), a)
                }
                // An infinite instant has no month, so a month may be NULL whatever its column holds.
                Item::Key(Key::Month(_)) => (
                    true,
                    Arc::new(ints().map(|v| v.map(|x| x as i16)).collect::<Int16Array>()),
                ),
                Item::Count => (false, Arc::new(ints().collect::<Int64Array>())),
                // An interval's sum and average, and a time's, are intervals.
                Item::Avg(c) | Item::Sum(c)
                    if matches!(
                        tables[c.side].columns[c.column].ty,
                        ColumnType::Interval { .. } | ColumnType::Time(_)
                    ) =>
                {
                    (
                        true,
                        crate::scalars::interval_array(rows.iter().map(|r| match &r[i] {
                            Cell::Interval(v) => Some(v.0),
                            _ => None,
                        })),
                    )
                }
                Item::Avg(_) => (
                    true,
                    Arc::new(
                        floats()
                            .map(|b| b.map(f64::from_bits))
                            .collect::<Float64Array>(),
                    ),
                ),
                Item::Sum(c) => match tables[c.side].columns[c.column].ty {
                    ColumnType::Float4 => (
                        true,
                        Arc::new(
                            floats()
                                .map(|b| b.map(|b| f32::from_bits(b as u32)))
                                .collect::<Float32Array>(),
                        ),
                    ),
                    ColumnType::Float8 => (
                        true,
                        Arc::new(
                            floats()
                                .map(|b| b.map(f64::from_bits))
                                .collect::<Float64Array>(),
                        ),
                    ),
                    _ => (true, Arc::new(ints().collect::<Int64Array>())),
                },
            };
            let scalar = match *item {
                _ if char_text => None,
                // A reg* type's min and max are oid's, an oid.
                Item::Least(c) | Item::Greatest(c)
                    if tables[c.side].columns[c.column].ty.is_oid() =>
                {
                    Some(ColumnType::Oid)
                }
                Item::Avg(c) | Item::Sum(c)
                    if matches!(
                        tables[c.side].columns[c.column].ty,
                        ColumnType::Interval { .. } | ColumnType::Time(_)
                    ) =>
                {
                    Some(ColumnType::Interval {
                        range: crate::times::INTERVAL_FULL_RANGE,
                        precision: None,
                    })
                }
                Item::Key(Key::Column(c)) | Item::Least(c) | Item::Greatest(c) | Item::Sum(c) => {
                    Some(tables[c.side].columns[c.column].ty).filter(|&t| {
                        crate::scalars::is_scalar(t) || matches!(t, ColumnType::Interval { .. })
                    })
                }
                _ => None,
            };
            // A name's and a node tree's min and max are text's, as PostgreSQL reads them through
            // text; a cidr's are inet's.
            let named = match *item {
                Item::Key(Key::Column(c)) | Item::Least(c) | Item::Greatest(c) => {
                    let extreme = matches!(*item, Item::Least(_) | Item::Greatest(_));
                    match tables[c.side].columns[c.column].ty {
                        ColumnType::Textual(crate::textual::Textual::Cidr) if extreme => {
                            Some(ColumnType::Textual(crate::textual::Textual::Inet))
                        }
                        t => Some(t).filter(|&t| {
                            crate::strings::is_named(t)
                                && !(extreme
                                    && matches!(
                                        t,
                                        ColumnType::Name
                                            | ColumnType::Internal(
                                                crate::textual::Internal::NodeTree
                                            )
                                    ))
                        }),
                    }
                }
                _ => None,
            };
            let composed = match *item {
                Item::Key(Key::Column(c)) | Item::Least(c) | Item::Greatest(c) => {
                    Some(tables[c.side].columns[c.column].ty)
                        .filter(|t| matches!(t, ColumnType::Composed(_)))
                }
                _ => None,
            };
            fields.push(match (scalar, named, composed) {
                (_, _, Some(ty)) => crate::composed::arrow_field(name, ty, nullable),
                (Some(ty), _, _) => crate::scalars::scalar_field(name, ty, nullable),
                (None, Some(ty), _) => crate::strings::string_field(name, ty, nullable),
                (None, None, None) => Field::new(name, a.data_type().clone(), nullable),
            });
            arrays.push(a);
        }
        Ok(RecordBatch::try_new_with_options(
            Arc::new(Schema::new(fields)),
            arrays,
            &RecordBatchOptions::new().with_row_count(Some(rows.len())),
        )?)
    }
}
