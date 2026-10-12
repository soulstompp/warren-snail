//! The answer as rows arrive: the rows themselves, or the groups they fall in, then ordered and
//! returned as Arrow columns. Text read from a dictionary arrives as its code: rows are grouped by
//! their codes while the dictionaries stand, and each group's codes become text when they change.

use super::hash::WordMap;
use super::question::{Countable, Item, Key, Select, Sort};
use crate::catalog::{ColumnType, Table};
use crate::Result;
use arrow::array::{
    ArrayRef, BinaryArray, Decimal128Array, Int16Array, Int32Array, Int64Array, RecordBatch,
    RecordBatchOptions, StringArray, TimestampMicrosecondArray,
};
use arrow::datatypes::{DataType, Field, Schema};
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

/// Adds one group's aggregates into another's.
fn merge(into: &mut [Acc], from: &[Acc]) {
    for (a, b) in into.iter_mut().zip(from) {
        match (a, b) {
            (Acc::Count(x), Acc::Count(y)) => *x += y,
            (Acc::Sum(x), Acc::Sum(Some(y))) => *x = Some(x.unwrap_or(0) + y),
            _ => {}
        }
    }
}

/// One value of a row a round keeps.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Cell {
    Null,
    Int(i64),
    Text(Text),
    /// Text as its place in the dictionary of the rows now arriving.
    Code(u32),
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
#[derive(Clone, Copy, Debug)]
pub enum Acc {
    Count(u64),
    Sum(Option<i128>),
}

/// The answer as rows arrive, each as the cells of the question's inputs.
pub struct Answer {
    /// For each input, the dictionary its codes index in the rows now arriving.
    dicts: Vec<Dict>,
    /// Whether any input has one.
    coded: bool,
    body: Body,
}

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
        }
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

    fn fresh(items: &[Item]) -> Vec<Acc> {
        items
            .iter()
            .map(|i| match i {
                Item::Sum(_) => Acc::Sum(None),
                _ => Acc::Count(0),
            })
            .collect()
    }

    pub fn add(&mut self, cells: &[Cell]) {
        self.add_times(cells, 1);
    }

    /// Adds `n` rows of these cells.
    pub fn add_times(&mut self, cells: &[Cell], n: u64) {
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
                        by_code[c].get_or_insert_with(|| Answer::fresh(items))
                    }
                    _ => match local.get_mut(&cells[..*keys]) {
                        Some(a) => a,
                        None => local
                            .entry(cells[..*keys].to_vec())
                            .or_insert_with(|| Answer::fresh(items)),
                    },
                };
                for (acc, item) in accs.iter_mut().zip(items.iter()) {
                    match (acc, item) {
                        (Acc::Count(c), Item::Count) => *c += n,
                        (Acc::Sum(total), Item::Sum(c)) => {
                            let at = inputs.iter().position(|k| *k == Key::Column(*c)).unwrap();
                            if let Cell::Int(v) = cells[at] {
                                *total = Some(total.unwrap_or(0) + v as i128 * n as i128);
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
        for (i, c) in codes.iter().enumerate() {
            let c = c.unwrap() as usize;
            count[c] += 1;
            for (s, at) in sums.iter_mut().zip(&summed) {
                if let Some(at) = at {
                    let (_, _, st, v) = outs[*at];
                    if let Some(x) = v.int(from + i - st) {
                        s[c] = Some(s[c].unwrap_or(0) + x as i128);
                    }
                }
            }
        }
        if by_code.len() < n {
            by_code.resize(n, None);
        }
        for c in (0..n).filter(|&c| count[c] > 0) {
            let accs = by_code[c].get_or_insert_with(|| Answer::fresh(items));
            for ((acc, item), s) in accs.iter_mut().zip(items.iter()).zip(&sums) {
                match (acc, item) {
                    (Acc::Count(k), Item::Count) => *k += count[c],
                    (Acc::Sum(total), Item::Sum(_)) => {
                        if let Some(x) = s[c] {
                            *total = Some(total.unwrap_or(0) + x);
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
        let (items, groups) = match &mut self.body {
            Body::Groups { items, groups, .. } => (items.clone(), groups),
            Body::Rows(_) => return,
        };
        let mut add = |key: Vec<Cell>, n: u64| {
            let accs = groups.entry(key).or_insert_with(|| Answer::fresh(&items));
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
                    groups.insert(Vec::new(), Answer::fresh(&items));
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
                                (_, Acc::Sum(total)) => {
                                    total.map_or(Cell::Null, |t| Cell::Int(t as i64))
                                }
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
            let (nullable, a): (bool, ArrayRef) = match *item {
                Item::Key(Key::Column(c)) => {
                    let col = &tables[c.side].columns[c.column];
                    let a: ArrayRef = match col.ty {
                        ColumnType::Int2 => {
                            Arc::new(ints().map(|v| v.map(|x| x as i16)).collect::<Int16Array>())
                        }
                        ColumnType::Int4 => {
                            Arc::new(ints().map(|v| v.map(|x| x as i32)).collect::<Int32Array>())
                        }
                        ColumnType::Int8 => Arc::new(ints().collect::<Int64Array>()),
                        ColumnType::Timestamptz => Arc::new(
                            ints()
                                .collect::<TimestampMicrosecondArray>()
                                .with_timezone("UTC"),
                        ),
                        ColumnType::Text(_) => Arc::new(
                            rows.iter()
                                .map(|r| match &r[i] {
                                    Cell::Text(t) => Some(t.as_str()),
                                    _ => None,
                                })
                                .collect::<StringArray>(),
                        ),
                        ColumnType::Bytes => Arc::new(
                            rows.iter()
                                .map(|r| match &r[i] {
                                    Cell::Text(t) => Some(t.0.data()),
                                    _ => None,
                                })
                                .collect::<BinaryArray>(),
                        ),
                        ColumnType::Numeric { precision, scale } => Arc::new(
                            ints()
                                .map(|v| v.map(i128::from))
                                .collect::<Decimal128Array>()
                                .with_precision_and_scale(precision, scale as i8)?,
                        ),
                    };
                    (!col.not_null, a)
                }
                Item::Key(Key::Month(c)) => (
                    !tables[c.side].columns[c.column].not_null,
                    Arc::new(ints().map(|v| v.map(|x| x as i16)).collect::<Int16Array>()),
                ),
                Item::Count => (false, Arc::new(ints().collect::<Int64Array>())),
                Item::Sum(c) => match tables[c.side].columns[c.column].ty {
                    ColumnType::Numeric { scale, .. } => (
                        true,
                        Arc::new(
                            ints()
                                .map(|v| v.map(i128::from))
                                .collect::<Decimal128Array>()
                                .with_precision_and_scale(38, scale as i8)?,
                        ),
                    ),
                    _ => (true, Arc::new(ints().collect::<Int64Array>())),
                },
            };
            let dt: DataType = a.data_type().clone();
            fields.push(Field::new(name, dt, nullable));
            arrays.push(a);
        }
        Ok(RecordBatch::try_new_with_options(
            Arc::new(Schema::new(fields)),
            arrays,
            &RecordBatchOptions::new().with_row_count(Some(rows.len())),
        )?)
    }
}
