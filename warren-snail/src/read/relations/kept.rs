//! A table met with rows made before it, kept to their keys: the values those rows hold of the
//! columns the table meets them on, as the table's levels key them, so its read is priced once they
//! are known and only the rows that meet them are held.

use super::{conjuncts, stored, Merged, Node, Reading};
use crate::catalog::{Column, ColumnType, Level, Table};
use crate::places::{column_byte_key, KeyValue};
use crate::read::pipeline::{self, Key, Value};
use crate::read::walk::KeySet;
use sqlparser::ast::{BinaryOperator, Expr};
use std::collections::HashSet;

/// What rows made before a table hold of the columns it meets them on: the table's places in the
/// FROM, each row's values there, and whether a NULL meets a NULL, as a set operation's arms meet;
/// a join's NULL meets none.
#[derive(Clone)]
pub(in crate::read) struct Keep {
    pub(in crate::read) places: Vec<usize>,
    pub(in crate::read) values: Vec<Vec<Value>>,
    pub(in crate::read) nulls_meet: bool,
}

/// The value a level of `column` keys from a value, where a kept read finds rows by it: an integer,
/// or text compared by its bytes.
fn kept_value(v: &Value, c: &Column) -> Option<KeyValue> {
    match (v, c.ty) {
        (Value::Int(x), ColumnType::Int2 | ColumnType::Int4 | ColumnType::Int8) => {
            Some(KeyValue::Int(*x))
        }
        (Value::Text(s), ColumnType::Text(_)) if !c.collated() => Some(column_byte_key(
            c,
            &crate::strings::key_bytes(c.ty, s.as_bytes()),
        )),
        _ => None,
    }
}

/// The keys a table's read is kept to: the key set its walk finds them by, where an order of the
/// table leads with columns they all hold; the keys every row it keeps holds, of its places from
/// the node's first.
pub(super) struct Kept {
    pub(super) set: Option<KeySet>,
    keys: HashSet<Vec<Key>>,
    at: Vec<usize>,
    nulls_meet: bool,
}

impl Kept {
    pub(super) fn of(keep: &Keep, node_at: usize, table: &Table) -> Kept {
        let columns: Vec<usize> = stored(table).collect();
        let cols: Vec<usize> = keep
            .places
            .iter()
            .map(|p| columns[p - node_at])
            .collect();
        let keyable: Vec<bool> = (0..cols.len())
            .map(|i| {
                keep.values.iter().all(|r| match r[i].is_null() {
                    true => !keep.nulls_meet,
                    false => kept_value(&r[i], &table.columns[cols[i]]).is_some(),
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
                keep.values.iter().filter_map(|r| {
                    best.iter()
                        .map(|&i| match r[i].is_null() {
                            true => None,
                            false => kept_value(&r[i], &table.columns[cols[i]]),
                        })
                        .collect::<Option<Vec<KeyValue>>>()
                }),
            )
        });
        let keys = keep
            .values
            .iter()
            .filter(|r| keep.nulls_meet || !r.iter().any(|v| v.is_null()))
            .map(|r| r.iter().map(pipeline::key).collect())
            .collect();
        Kept {
            set,
            keys,
            at: keep.places.iter().map(|p| p - node_at).collect(),
            nulls_meet: keep.nulls_meet,
        }
    }

    /// Whether a row the table's read gives, its places from the node's first, meets a row made
    /// before it.
    pub(super) fn admits(&self, row: &[Value]) -> bool {
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
        made: &[Vec<Value>],
        made_at: usize,
    ) -> Option<Keep> {
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
    pub(super) fn keep_for_item(&self, table: &Node, made: &[Vec<Value>]) -> Option<Keep> {
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
        made: &[Vec<Value>],
        made_at: usize,
        nulls_meet: bool,
    ) -> Option<Keep> {
        if pairs.is_empty() {
            return None;
        }
        Some(Keep {
            places: pairs.iter().map(|p| p.0).collect(),
            values: made
                .iter()
                .map(|r| pairs.iter().map(|p| r[p.1 - made_at].clone()).collect())
                .collect(),
            nulls_meet,
        })
    }
}
