//! A table met with rows made before it, kept to their keys: the values those rows hold of the
//! columns the table meets them on, as the table's levels key them, so its read is priced once they
//! are known and only the rows that meet them are held.

use super::{conjuncts, Merged, Node, Reading};
use crate::catalog::{Column, ColumnType, Level, Table};
use crate::places::{column_byte_key, KeyValue};
use crate::read::pipeline::{self, Key, Value};
use crate::read::spill::Rows;
use crate::read::walk::KeySet;
use crate::Result;
use sqlparser::ast::{BinaryOperator, Expr};
use crate::read::held::{Columns, KeySet as Keys};
use std::rc::Rc;

/// What rows made before a table hold of the columns it meets them on: the table's places in the
/// FROM, each row's values there, and whether a NULL meets a NULL, as a set operation's arms meet;
/// a join's NULL meets none.
#[derive(Clone)]
pub(in crate::read) struct Keep {
    pub(in crate::read) places: Vec<usize>,
    pub(in crate::read) values: Rc<Columns>,
    pub(in crate::read) nulls_meet: bool,
    /// What the keys hold in the round's room.
    _held: Rc<crate::room::Holding>,
}

impl Keep {
    /// These values of the columns at `places`, kept as they are given, their keys held twice as
    /// the walk finds rows by them.
    pub(in crate::read) fn of_values(places: Vec<usize>, values: Vec<Vec<Value>>) -> Keep {
        let mut keys = Keys::default();
        for v in &values {
            keys.insert(&v.iter().map(pipeline::key).collect::<Vec<_>>());
        }
        let values: Columns = values.into_iter().collect();
        let held = values.bytes() + 2 * keys.bytes();
        Keep {
            places,
            values: Rc::new(values),
            nulls_meet: true,
            _held: Rc::new(crate::room::holding(held)),
        }
    }

    /// What `made` holds, each row `of` read on it, each set of values once: none where they do not
    /// fit the round's room, its keys held twice as the walk finds rows by them.
    pub(in crate::read) fn of_rows(
        places: Vec<usize>,
        made: &Rows,
        nulls_meet: bool,
        of: &mut dyn FnMut(&[Value]) -> Result<Vec<Value>>,
    ) -> Result<Option<Keep>> {
        let mut seen = Keys::default();
        let mut values = Columns::default();
        let mut held = crate::room::holding(0);
        let mut over = false;
        made.each_chunk(&mut |c| {
            for r in &c {
                if over {
                    break;
                }
                let v = of(r)?;
                let k: Vec<Key> = v.iter().map(pipeline::key).collect();
                if seen.ask(&k).is_none() {
                    let w = values.width_of(&v) + 2 * seen.width();
                    seen.hold();
                    if !held.fits(w) {
                        over = true;
                        break;
                    }
                    values.push(v);
                }
            }
            Ok(())
        })?;
        Ok((!over).then(|| Keep {
            places,
            values: Rc::new(values),
            nulls_meet,
            _held: Rc::new(held),
        }))
    }
}

/// The value a level of `column` keys from a value, where a kept read finds rows by it: none where
/// no value of the level is it.
fn kept_value(v: &Value, c: &Column) -> Option<KeyValue> {
    let ty = c.ty;
    let of_bytes = |b: &[u8]| -> Option<KeyValue> {
        let key = crate::strings::column_key(ty, c.orderer().ok()?, b);
        Some(column_byte_key(c, &key))
    };
    match v {
        Value::Null | Value::Geometric(_) => None,
        Value::Int(_) | Value::Num(..) if ty.is_numeric() => numeric_key(v, ty),
        Value::Numeric(n) if ty.is_numeric() => match ty {
            ColumnType::NumericText(_) => of_bytes(n.to_string().as_bytes()),
            _ => numeric_key(&Value::of_numeric(n.clone()), ty),
        },
        Value::Int(x) if ty.is_integer() => Some(KeyValue::Int(*x)),
        Value::Text(_) | Value::Str(..) if ty.is_string() => of_bytes(v.string_key()?),
        Value::Bytes(b) if ty.keyed_by_bytes() => Some(crate::places::byte_key(ty, b)),
        Value::Bits(_, k) if ty.is_bits() => Some(crate::places::byte_key(ty, k)),
        Value::Typed(_, s) if ty.keyed_by_bytes() => Some(crate::places::byte_key(
            ty,
            &crate::strings::key_bytes(ty, s.as_bytes()),
        )),
        Value::Typed(_, s) if ty.keyed_by_text() => of_bytes(s.as_bytes()),
        Value::Bool(b) if ty == ColumnType::Boolean => Some(KeyValue::Int(i64::from(*b))),
        Value::Uuid(k) if ty == ColumnType::Uuid => Some(KeyValue::Wide(*k)),
        Value::Scalar(t, k) if *t == ty => Some(KeyValue::Int(*k)),
        Value::Real(f) if ty == ColumnType::Float4 => {
            Some(KeyValue::Int(crate::scalars::float4_key(f.to_bits())))
        }
        Value::Double(d) if ty == ColumnType::Float8 => {
            Some(KeyValue::Int(crate::scalars::float8_key(d.to_bits())))
        }
        Value::Interval(i) if matches!(ty, ColumnType::Interval { .. }) => {
            Some(KeyValue::Wide(i.total()))
        }
        Value::Composed(t, x) if *t == ty && !x.item.is_null() => match &x.item {
            crate::composed::Item::Scalar(crate::read::statement::Datum::Int(rank))
                if !ty.is_nested() =>
            {
                Some(KeyValue::Int(*rank))
            }
            _ if ty.is_nested() => Some(crate::places::byte_key(ty, x.hex.as_bytes())),
            _ => None,
        },
        _ => None,
    }
}

/// A number's key on a numeric(p, s) level: the integer of its smallest unit; a numeric of no fixed
/// scale by its class's text. None where no value of the level is it.
fn numeric_key(v: &Value, ty: ColumnType) -> Option<KeyValue> {
    let (m, s) = v.numeric()?;
    match ty {
        ColumnType::Numeric { scale, .. } => {
            let mut m = m;
            let to = u32::try_from(scale).ok()?;
            if s > to {
                let d = 10i128.checked_pow(s - to)?;
                if m % d != 0 {
                    return None;
                }
                m /= d;
            } else {
                m = m.checked_mul(10i128.checked_pow(to - s)?)?;
            }
            match ty.is_wide() {
                true => Some(KeyValue::Wide(m)),
                false => Some(KeyValue::Int(i64::try_from(m).ok()?)),
            }
        }
        _ => {
            let n = v.to_numeric()?;
            Some(crate::places::byte_key(
                ty,
                &crate::strings::key_bytes(ty, n.to_string().as_bytes()),
            ))
        }
    }
}

/// The keys a table's read is kept to: the key set its walk finds them by, where an order of the
/// table leads with columns they all hold; the keys every row it keeps holds, of its places from
/// the node's first.
pub(in crate::read) struct Kept {
    pub(in crate::read) set: Option<KeySet>,
    keys: Keys,
    at: Vec<usize>,
    nulls_meet: bool,
}

impl Kept {
    pub(in crate::read) fn of(keep: &Keep, node_at: usize, table: &Table) -> Kept {
        let columns: Vec<usize> = super::node_columns(table, usize::MAX);
        let cols: Vec<usize> = keep.places.iter().map(|p| columns[p - node_at]).collect();
        // A system column keys no walk: no order holds it.
        let keyable: Vec<bool> = (0..cols.len())
            .map(|i| {
                if crate::catalog::is_system(cols[i]) {
                    return false;
                }
                (0..keep.values.len()).all(|at| match keep.values.value(at, i) {
                    Value::Null => !keep.nulls_meet,
                    v => kept_value(&v, &table.columns[cols[i]]).is_some(),
                })
            })
            .collect();
        // The order that leads with the most of its columns, in its own order.
        let mut best: Vec<usize> = Vec::new();
        for o in &table.orders {
            let mut prefix: Vec<usize> = Vec::new();
            for l in &o.levels {
                let Level::Column(c) = l else { break };
                match (0..cols.len()).find(|&i| cols[i] == *c && keyable[i] && !prefix.contains(&i))
                {
                    Some(i) => prefix.push(i),
                    None => break,
                }
            }
            if prefix.len() > best.len() {
                best = prefix;
            }
        }
        let set = (!best.is_empty()).then(|| {
            KeySet::new(
                best.iter().map(|&i| cols[i]).collect(),
                keep.values.rows().filter_map(|r| {
                    best.iter()
                        .map(|&i| match r[i].is_null() {
                            true => None,
                            false => kept_value(&r[i], &table.columns[cols[i]]),
                        })
                        .collect::<Option<Vec<KeyValue>>>()
                }),
            )
        });
        let mut keys = Keys::default();
        for r in keep
            .values
            .rows()
            .filter(|r| keep.nulls_meet || !r.iter().any(|v| v.is_null()))
        {
            keys.insert(&r.iter().map(pipeline::key).collect::<Vec<_>>());
        }
        Kept {
            set,
            keys,
            at: keep.places.iter().map(|p| p - node_at).collect(),
            nulls_meet: keep.nulls_meet,
        }
    }

    /// Whether a row the table's read gives, its places from the node's first, meets a row made
    /// before it.
    pub(in crate::read) fn admits(&self, row: &[Value]) -> bool {
        if !self.nulls_meet && self.at.iter().any(|&i| row[i].is_null()) {
            return false;
        }
        let k: Vec<Key> = self.at.iter().map(|&i| pipeline::key(&row[i])).collect();
        self.keys.contains(&k)
    }
}

impl Reading<'_> {
    /// The keep that applies to a table's node: one whose places all lie in it.
    pub(super) fn keep_of(&self, node: &Node) -> Option<&Keep> {
        let range = node.at..node.at + node.width;
        self.kept
            .iter()
            .rev()
            .find(|k| !k.places.is_empty() && k.places.iter().all(|p| range.contains(p)))
    }

    /// Whether two expressions compared by = meet by their keys: their types one, or both integers,
    /// and no library's collation between them.
    fn keyed_alike(&self, c: &Expr, a: &Expr, b: &Expr) -> bool {
        let (ta, tb) = (
            self.types.get(&(a as *const Expr)),
            self.types.get(&(b as *const Expr)),
        );
        let same = match (ta, tb) {
            (Some(x), Some(y)) => x == y || (x.is_integer() && y.is_integer()),
            _ => false,
        };
        same && !self.casings.compared_under_library(c)
    }

    /// The places, in the FROM from `at` for `width`, of the columns each conjunct of `conds`
    /// comparing two columns by = names, where they meet by their keys.
    fn equal_places(&self, conds: &[&Expr], at: usize, width: usize) -> Vec<(usize, usize)> {
        let cols = self.from.frame_columns(at, width);
        let place = |e: &Expr| -> Option<usize> {
            let (q, n) = pipeline::column_of(e)?;
            match pipeline::naming(
                cols.iter().map(|(q, n)| (q.as_deref(), n.as_str())),
                q.as_deref(),
                &n,
            )
            .as_slice()
            {
                [i] => Some(at + *i),
                _ => None,
            }
        };
        let mut out = Vec::new();
        for &c in conds {
            let Expr::BinaryOp {
                left: a,
                op: BinaryOperator::Eq,
                right: b,
            } = c
            else {
                continue;
            };
            if !self.keyed_alike(c, a, b) {
                continue;
            }
            if let (Some(x), Some(y)) = (place(a), place(b)) {
                out.push((x, y));
            }
        }
        out
    }

    /// What the rows made of one side of a join hold of the columns of a table on its other side
    /// that the join's USING and ON meet by =: `made` the rows of the side from `made_at`, `table`
    /// the table's node.
    pub(super) fn keep_for_join(
        &self,
        node: &Node,
        on: Option<&Expr>,
        merged: &[Merged],
        table: &Node,
        made: &Rows,
        made_at: usize,
    ) -> Result<Option<Keep>> {
        let mut conds = Vec::new();
        if let Some(e) = on {
            conjuncts(e, &mut conds);
        }
        let in_table = |p: usize| (table.at..table.at + table.width).contains(&p);
        let mut pairs: Vec<(usize, usize)> = merged.iter().map(|m| (m.left, m.right)).collect();
        pairs.extend(self.equal_places(&conds, node.at, node.width));
        let pairs: Vec<(usize, usize)> = pairs
            .into_iter()
            .filter_map(|(x, y)| match (in_table(x), in_table(y)) {
                (true, false) => Some((x, y)),
                (false, true) => Some((y, x)),
                _ => None,
            })
            .collect();
        self.keep_of_pairs(&pairs, made, made_at, false)
    }

    /// What the FROM's rows made so far hold of the columns of a table among its items that a
    /// conjunct of WHERE meets them on by =: `made` the rows of the items before it.
    pub(super) fn keep_for_item(&self, table: &Node, made: &Rows) -> Result<Option<Keep>> {
        let conds = self.wheres.clone();
        let pairs: Vec<(usize, usize)> = self
            .equal_places(&conds, 0, self.from.outer_at)
            .into_iter()
            .filter_map(|(x, y)| {
                let in_table = |p: usize| (table.at..table.at + table.width).contains(&p);
                match (in_table(x), in_table(y)) {
                    (true, false) if y < table.at => Some((x, y)),
                    (false, true) if x < table.at => Some((y, x)),
                    _ => None,
                }
            })
            .collect();
        self.keep_of_pairs(&pairs, made, 0, false)
    }

    fn keep_of_pairs(
        &self,
        pairs: &[(usize, usize)],
        made: &Rows,
        made_at: usize,
        nulls_meet: bool,
    ) -> Result<Option<Keep>> {
        if pairs.is_empty() {
            return Ok(None);
        }
        Keep::of_rows(
            pairs.iter().map(|p| p.0).collect(),
            made,
            nulls_meet,
            &mut |r| Ok(pairs.iter().map(|p| r[p.1 - made_at].clone()).collect()),
        )
    }
}
