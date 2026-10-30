//! A round's foreign keys. A FOREIGN KEY is the null side of its child's left outer join with its
//! parent: a child's row with nothing on the parent's side either holds a NULL in its reference,
//! which is never checked, or is an orphan, and there must be none. Only what a round changes can
//! make one: a child's row written, or a key taken from the parent. So the round records those as
//! it writes them, writes the actions a key taken calls for, and checks the rest where PostgreSQL
//! checks them, at the statement's end or, deferred, at the commit.
//!
//! A row written is looked up in its parent's key, and where the key is there, the parent's block
//! that holds it is locked, so no writer takes it away until the round ends. A key taken is looked
//! up among the child's rows, the blocks where they would lie read as the newest root names them,
//! and those that hold one locked. A block written since the root the round reads runs the
//! statement again, as a block it writes does.

use super::change::{datum_typed, key_of, pick, rows_of, widen, Change, Waiting};
use super::fill;
use super::statement::{Datum, Row};
use super::Snail;
use crate::catalog::{Action, Catalog, ColumnType, ForeignKey, Table};
use crate::directory::{decode_page, decode_top, Entry, Head};
use crate::places::KeyValue;
use crate::write::Rows;
use crate::{Error, Result};
use arrow::array::{Array, Int64Array};
use std::collections::BTreeMap;

/// What a round changed that its foreign keys read, by key: its child table and its place among
/// the child's foreign keys.
#[derive(Clone, Debug, Default)]
pub(super) struct Touched {
    /// The rows written into a child, each still as the round leaves it.
    written: BTreeMap<(usize, usize), Vec<Row>>,
    /// The keys taken from a parent: its row as it was, and as it is where its key changed.
    taken: BTreeMap<(usize, usize), Vec<(Row, Option<Row>)>>,
}

impl Touched {
    pub(super) fn is_empty(&self) -> bool {
        self.written.values().all(Vec::is_empty) && self.taken.values().all(Vec::is_empty)
    }
}

/// A value as `=` sees it: a float's class, any other value itself.
pub(crate) fn class(d: &Datum) -> Datum {
    match d {
        Datum::Float4(b) => Datum::Int(crate::scalars::float4_key(*b)),
        Datum::Float8(b) => Datum::Int(crate::scalars::float8_key(*b)),
        other => other.clone(),
    }
}

/// A row's reference: the values of the key's columns.
fn reference(k: &ForeignKey, row: &Row) -> Vec<Datum> {
    k.columns.iter().map(|&c| row[c].clone()).collect()
}

/// Whether two rows reference the same key: neither holding a NULL, and each value equal.
fn same_reference(k: &ForeignKey, a: &Row, b: &Row) -> bool {
    k.columns
        .iter()
        .all(|&c| a[c] != Datum::Null && b[c] != Datum::Null && class(&a[c]) == class(&b[c]))
}

/// Drops one row equal to `row` from the rows written for a key; whether there was one.
fn forget(t: &mut Touched, at: (usize, usize), row: &Row) -> bool {
    let Some(rows) = t.written.get_mut(&at) else {
        return false;
    };
    match rows.iter().position(|r| r == row) {
        Some(i) => {
            rows.swap_remove(i);
            true
        }
        None => false,
    }
}

/// A table's name as the question reads it, each part quoted.
fn quoted(name: &str) -> String {
    name.split('.')
        .map(|p| format!("\"{}\"", p.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(".")
}

/// A value written as a literal the question reads as a value of a column of type `ty`.
fn literal(d: &Datum, ty: ColumnType) -> String {
    match (d, ty) {
        (Datum::Int(v), ColumnType::Int2 | ColumnType::Int4 | ColumnType::Int8) => v.to_string(),
        (Datum::Int(v), ColumnType::Reg(..)) => {
            format!("'{}'", crate::scalars::scalar_text(ColumnType::Oid, *v))
        }
        _ => format!(
            "'{}'",
            fill::text_of(d, ty).unwrap_or_default().replace('\'', "''")
        ),
    }
}

/// A key's columns and values as PostgreSQL's DETAIL writes them: `(a, b)=(1, 2)`.
pub(crate) fn shown(t: &Table, columns: &[usize], key: &[Datum]) -> String {
    let names: Vec<&str> = columns
        .iter()
        .map(|&c| t.columns[c].name.as_str())
        .collect();
    let values: Vec<String> = columns
        .iter()
        .zip(key)
        .map(|(&c, d)| fill::text_of(d, t.columns[c].ty).unwrap_or_else(|| "null".into()))
        .collect();
    format!("({})=({})", names.join(", "), values.join(", "))
}

fn bare(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

impl Snail {
    /// The foreign keys a table's changes touch, each change recorded where the key reads it: a row
    /// written into a child, and a key taken from a parent, the parent's row holding no NULL in it.
    pub(super) fn touch(&mut self, ti: usize, changes: &[Change]) {
        let name = &self.catalog.tables[ti].name;
        for (ci, child) in self.catalog.tables.iter().enumerate() {
            for (fi, k) in child.foreign_keys.iter().enumerate() {
                if !k.enforced {
                    continue;
                }
                let at = (ci, fi);
                if ci == ti {
                    for c in changes {
                        match c {
                            Change::Insert(r) => {
                                self.touched.written.entry(at).or_default().push(r.clone())
                            }
                            Change::Delete(o) => {
                                if !forget(&mut self.touched, at, o) {
                                    forget(&mut self.pending, at, o);
                                }
                            }
                            Change::Replace(o, n) => {
                                // A row the round wrote is checked as it stands now; another row
                                // whose reference did not change is not checked again.
                                let ours = forget(&mut self.touched, at, o)
                                    || forget(&mut self.pending, at, o);
                                if ours || !same_reference(k, o, n) {
                                    self.touched.written.entry(at).or_default().push(n.clone());
                                }
                            }
                        }
                    }
                }
                if k.parent == *name {
                    let keyed = |r: &Row| k.parent_columns.iter().all(|&p| r[p] != Datum::Null);
                    for c in changes {
                        match c {
                            Change::Delete(o) if keyed(o) => self
                                .touched
                                .taken
                                .entry(at)
                                .or_default()
                                .push((o.clone(), None)),
                            Change::Replace(o, n)
                                if keyed(o) && k.parent_columns.iter().any(|&p| o[p] != n[p]) =>
                            {
                                self.touched
                                    .taken
                                    .entry(at)
                                    .or_default()
                                    .push((o.clone(), Some(n.clone())))
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    /// Whether a key is checked at the commit: deferrable and deferred, by its DDL or by SET
    /// CONSTRAINTS, inside a transaction.
    fn deferred_now(&self, at: (usize, usize)) -> bool {
        let k = &self.catalog.tables[at.0].foreign_keys[at.1];
        self.begun.is_some() && k.deferrable && self.modes.get(&at).copied().unwrap_or(k.deferred)
    }

    /// The foreign keys where a statement ends: the actions its keys taken call for written, then
    /// what is checked now checked, and what is deferred kept for the commit. Returns false where
    /// the statement must run again on the newest root.
    pub(super) fn keys_at_statement_end(&mut self) -> Result<bool> {
        if !self.acted()? {
            return Ok(false);
        }
        let touched = std::mem::take(&mut self.touched);
        let mut now = Touched::default();
        for (at, rows) in touched.written {
            let to = if self.deferred_now(at) {
                &mut self.pending
            } else {
                &mut now
            };
            to.written.entry(at).or_default().extend(rows);
        }
        for (at, taken) in touched.taken {
            let k = &self.catalog.tables[at.0].foreign_keys[at.1];
            for (o, n) in taken {
                let action = if n.is_some() {
                    &k.on_update
                } else {
                    &k.on_delete
                };
                let to = match action {
                    Action::Cascade | Action::SetNull(_) => continue,
                    Action::NoAction if self.deferred_now(at) => &mut self.pending,
                    _ => &mut now,
                };
                to.taken.entry(at).or_default().push((o, n));
            }
        }
        self.keys_checked(&now)
    }

    /// The foreign keys deferred to the commit, checked before it on the newest root, with the
    /// transaction's own changes in place.
    pub(super) fn keys_at_commit(&mut self) -> Result<()> {
        let all: Vec<(usize, usize)> = self
            .pending
            .written
            .keys()
            .chain(self.pending.taken.keys())
            .copied()
            .collect();
        self.keys_now(&all)
    }

    /// The deferred changes of the keys given checked now, and dropped from those the commit
    /// checks: at the commit, or where SET CONSTRAINTS makes them immediate.
    fn keys_now(&mut self, keys: &[(usize, usize)]) -> Result<()> {
        let mut due = Touched::default();
        for at in keys {
            if let Some(w) = self.pending.written.remove(at) {
                due.written.insert(*at, w);
            }
            if let Some(t) = self.pending.taken.remove(at) {
                due.taken.insert(*at, t);
            }
        }
        if due.is_empty() {
            return Ok(());
        }
        loop {
            let generation = self.root.generation;
            if self.keys_checked(&due)? {
                return Ok(());
            }
            self.stale = false;
            self.renew_in_transaction()?;
            if self.root.generation == generation {
                return Err(Error::new(
                    "a block a foreign key reads was written by another writer, and the \
                     transaction, which declared, reads the file as it began",
                ));
            }
        }
    }

    /// SET CONSTRAINTS: each foreign key named, or ALL, deferred or immediate until the transaction
    /// ends; made immediate, its deferred changes checked now. Outside a transaction it does
    /// nothing, as PostgreSQL warns.
    pub(super) fn constrained(&mut self, names: Option<Vec<String>>, deferred: bool) -> Result<()> {
        let mut keys: Vec<(usize, usize)> = Vec::new();
        let named = |n: &str, table: &str, k: &str| match n.rsplit_once('.') {
            Some((schema, bare_name)) => {
                bare_name == k && table.rsplit_once('.').map(|x| x.0) == Some(schema)
            }
            None => n == k,
        };
        match &names {
            None => {
                for (ci, t) in self.catalog.tables.iter().enumerate() {
                    for (fi, k) in t.foreign_keys.iter().enumerate() {
                        if k.deferrable {
                            keys.push((ci, fi));
                        }
                    }
                    if deferred {
                        if let Some(o) = t
                            .orders
                            .iter()
                            .find(|o| o.constraint.as_ref().is_some_and(|k| k.deferrable))
                        {
                            return Err(Error::new(format!(
                                "SET CONSTRAINTS ALL DEFERRED: {}.{} is a DEFERRABLE key, and the \
                                 snail checks a key at each statement's end",
                                t.name, o.name
                            )));
                        }
                    }
                }
            }
            Some(names) => {
                for n in names {
                    let mut found = false;
                    for (ci, t) in self.catalog.tables.iter().enumerate() {
                        for (fi, k) in t.foreign_keys.iter().enumerate() {
                            if named(n, &t.name, &k.name) {
                                if !k.deferrable {
                                    return Err(Error::new(format!(
                                        "constraint \"{}\" is not deferrable",
                                        k.name
                                    )));
                                }
                                keys.push((ci, fi));
                                found = true;
                            }
                        }
                        for o in &t.orders {
                            let Some(c) = &o.constraint else { continue };
                            if !named(n, &t.name, &o.name) {
                                continue;
                            }
                            if !c.deferrable {
                                return Err(Error::new(format!(
                                    "constraint \"{}\" is not deferrable",
                                    o.name
                                )));
                            }
                            if deferred {
                                return Err(Error::new(format!(
                                    "SET CONSTRAINTS {n} DEFERRED: {}.{} is a DEFERRABLE key, and \
                                     the snail checks a key at each statement's end",
                                    t.name, o.name
                                )));
                            }
                            found = true;
                        }
                        if t.checks.iter().any(|k| named(n, &t.name, &k.name)) {
                            return Err(Error::new(format!(
                                "constraint \"{n}\" is not deferrable"
                            )));
                        }
                    }
                    if !found {
                        return Err(Error::new(format!("constraint \"{n}\" does not exist")));
                    }
                }
            }
        }
        if self.begun.is_none() {
            return Ok(());
        }
        for &at in &keys {
            self.modes.insert(at, deferred);
        }
        if deferred {
            return Ok(());
        }
        self.keys_now(&keys)
    }

    /// Writes the actions the keys taken call for, CASCADE, SET NULL and SET DEFAULT, in turn, each
    /// rows' changes touching the keys that read them, until none is left. Returns false where the
    /// statement must run again.
    fn acted(&mut self) -> Result<bool> {
        let mut done: BTreeMap<(usize, usize), usize> = BTreeMap::new();
        loop {
            let next = self.touched.taken.iter().find_map(|(&at, list)| {
                let d = done.get(&at).copied().unwrap_or(0);
                (d < list.len()).then(|| (at, d, list[d].clone()))
            });
            let Some((at, d, (old, new))) = next else {
                return Ok(true);
            };
            done.insert(at, d + 1);
            let (ci, fi) = at;
            let k = self.catalog.tables[ci].foreign_keys[fi].clone();
            let action = if new.is_some() {
                k.on_update.clone()
            } else {
                k.on_delete.clone()
            };
            if matches!(action, Action::NoAction | Action::Restrict) {
                continue;
            }
            let key: Vec<Datum> = k.parent_columns.iter().map(|&p| old[p].clone()).collect();
            let Some(rows) = self.children(ci, &k, &key)? else {
                return Ok(false);
            };
            if rows.is_empty() {
                continue;
            }
            let t = self.catalog.tables[ci].clone();
            let changes = match (&action, &new) {
                (Action::Cascade, None) => rows.into_iter().map(Change::Delete).collect(),
                _ => {
                    let p = self.parent_of(&k).clone();
                    let set = k.set_by(&action);
                    let mut drawn = match action {
                        Action::SetDefault(_) => {
                            self.draws(&t, set.iter().map(|&c| (c, rows.len())))?
                        }
                        _ => Default::default(),
                    };
                    let mut out = Vec::with_capacity(rows.len());
                    for r in rows {
                        let mut m = r.clone();
                        match &action {
                            Action::Cascade => {
                                let n = new.as_ref().expect("an update gives the new key");
                                for (&c, &pc) in k.columns.iter().zip(&k.parent_columns) {
                                    m[c] = fill::assign(
                                        n[pc].clone(),
                                        p.columns[pc].ty,
                                        t.columns[c].ty,
                                        &t.columns[c].name,
                                    )?;
                                }
                            }
                            Action::SetNull(_) => {
                                for &c in &set {
                                    m[c] = Datum::Null;
                                }
                            }
                            _ => {
                                for &c in &set {
                                    m[c] = self.default_of(&t, c, &mut drawn)?;
                                }
                            }
                        }
                        fill::generate(&t, &mut m)?;
                        fill::check(&t, &m)?;
                        out.push(Change::Replace(r, m));
                    }
                    out
                }
            };
            self.apply(ci, changes)?;
            if self.stale {
                return Ok(false);
            }
        }
    }

    fn parent_of(&self, k: &ForeignKey) -> &Table {
        self.catalog
            .table(&k.parent)
            .expect("a foreign key's parent is in the file")
    }

    /// Checks the changes given: each row written has its parent, unless its reference holds a
    /// NULL; each key taken is held again, under NO ACTION and SET DEFAULT, or no row references
    /// it. Returns false where the round must run again.
    fn keys_checked(&mut self, t: &Touched) -> Result<bool> {
        for (&(ci, fi), rows) in &t.written {
            let k = self.catalog.tables[ci].foreign_keys[fi].clone();
            let child = &self.catalog.tables[ci];
            // Each reference once, as `=` sees it, in the order of its classes.
            let mut keys: Vec<(Vec<Datum>, Vec<Datum>)> = Vec::new();
            for r in rows {
                let key = reference(&k, r);
                let nulls = key.iter().filter(|d| **d == Datum::Null).count();
                if nulls == key.len() {
                    continue;
                }
                if nulls > 0 {
                    if k.full {
                        return Err(Error::new(format!(
                            "insert or update on table \"{}\" violates foreign key constraint \
                             \"{}\": MATCH FULL does not allow mixing of null and nonnull key \
                             values.",
                            bare(&child.name),
                            k.name
                        )));
                    }
                    continue;
                }
                keys.push((key.iter().map(class).collect(), key));
            }
            keys.sort();
            keys.dedup_by(|a, b| a.0 == b.0);
            for (_, key) in keys {
                match self.holds_key(&k, &key)? {
                    None => return Ok(false),
                    Some(true) => {}
                    Some(false) => {
                        let child = &self.catalog.tables[ci];
                        return Err(Error::new(format!(
                            "insert or update on table \"{}\" violates foreign key constraint \
                             \"{}\": Key {} is not present in table \"{}\".",
                            bare(&child.name),
                            k.name,
                            shown(child, &k.columns, &key),
                            bare(&k.parent)
                        )));
                    }
                }
            }
        }
        for (&(ci, fi), taken) in &t.taken {
            let k = self.catalog.tables[ci].foreign_keys[fi].clone();
            for (old, new) in taken {
                let action = if new.is_some() {
                    &k.on_update
                } else {
                    &k.on_delete
                };
                let key: Vec<Datum> = k.parent_columns.iter().map(|&p| old[p].clone()).collect();
                if !matches!(action, Action::Restrict) {
                    match self.holds_key(&k, &key)? {
                        None => return Ok(false),
                        Some(true) => continue,
                        Some(false) => {}
                    }
                }
                match self.referenced(ci, &k, &key)? {
                    None => return Ok(false),
                    Some(false) => {}
                    Some(true) => {
                        let p = self.parent_of(&k);
                        let (setting, still) = match action {
                            Action::Restrict => ("RESTRICT setting of foreign key", ""),
                            _ => ("foreign key", "still "),
                        };
                        return Err(Error::new(format!(
                            "update or delete on table \"{}\" violates {setting} constraint \"{}\" \
                             on table \"{}\": Key {} is {still}referenced from table \"{}\".",
                            bare(&p.name),
                            k.name,
                            bare(&self.catalog.tables[ci].name),
                            shown(p, &k.parent_columns, &key),
                            bare(&self.catalog.tables[ci].name)
                        )));
                    }
                }
            }
        }
        Ok(true)
    }

    /// Whether the parent holds a key, given in its columns' order, as the round reads it once no
    /// writer can take it away or bring it: where it holds it, the block it lies in locked; where it
    /// does not, that block still the one the newest root names. None where the round must run
    /// again.
    fn holds_key(&mut self, k: &ForeignKey, key: &[Datum]) -> Result<Option<bool>> {
        let pi = self
            .catalog
            .tables
            .iter()
            .position(|t| t.name == k.parent)
            .expect("a foreign key's parent is in the file");
        let p = &self.catalog.tables[pi];
        let oi = p
            .orders
            .iter()
            .position(|o| o.name == k.key)
            .expect("the order a foreign key references");
        let mut row = vec![Datum::Null; p.columns.len()];
        for (&c, d) in k.parent_columns.iter().zip(key) {
            row[c] = d.clone();
        }
        let at = key_of(p, &p.orders[oi], &row);
        let blocks = self.covering(pi, oi, &at, &at)?;
        let n = self.count_where(pi, &k.parent_columns, key)?;
        if n == 0 {
            if !self.fresh(pi, oi, &blocks)? {
                self.stale = true;
                return Ok(None);
            }
            return Ok(Some(false));
        }
        if !self.lock_blocks(pi, oi, blocks, Waiting::Wait)? {
            self.stale = true;
            return Ok(None);
        }
        Ok(Some(true))
    }

    /// Whether the blocks of an order the round reads are the newest root's: where the round reads
    /// none, whether the order holds none in the newest root, or is one the round itself emptied.
    fn fresh(&self, ti: usize, oi: usize, blocks: &[Entry]) -> Result<bool> {
        if !blocks.is_empty() {
            return self.newest_names(ti, oi, blocks);
        }
        if self.rebased.contains_key(&(ti, oi)) {
            return Ok(true);
        }
        let t = &self.catalog.tables[ti];
        self.empty_in_newest(t, &t.orders[oi])
    }

    /// Whether any child's row references a key, given in the parent's columns' order, as the round
    /// reads it once no writer can add or take one it does not see. None where the round must run
    /// again.
    fn referenced(&mut self, ci: usize, k: &ForeignKey, key: &[Datum]) -> Result<Option<bool>> {
        let t = &self.catalog.tables[ci];
        if let Some(oi) = t.order_led_by(&k.columns) {
            let (lo, hi) = self.run_of(ci, oi, k, key);
            let blocks = self.covering(ci, oi, &lo, &hi)?;
            if !self.fresh(ci, oi, &blocks)? {
                self.stale = true;
                return Ok(None);
            }
            let n = self.count_where(ci, &k.columns, key)?;
            if n > 0 && !self.lock_blocks(ci, oi, blocks, Waiting::Wait)? {
                self.stale = true;
                return Ok(None);
            }
            return Ok(Some(n > 0));
        }
        Ok(self.children(ci, k, key)?.map(|rows| !rows.is_empty()))
    }

    /// The child's rows that reference a key, given in the parent's columns' order, as the round
    /// reads them once no writer can add or take one it does not see: the blocks where they would
    /// lie the newest along an order the reference leads, or else the whole child unchanged since
    /// the round's root; the blocks that hold one locked. None where the round must run again.
    fn children(&mut self, ci: usize, k: &ForeignKey, key: &[Datum]) -> Result<Option<Vec<Row>>> {
        let t = &self.catalog.tables[ci];
        let (oi, blocks) = match t.order_led_by(&k.columns) {
            Some(oi) => {
                let (lo, hi) = self.run_of(ci, oi, k, key);
                let blocks = self.covering(ci, oi, &lo, &hi)?;
                if !self.fresh(ci, oi, &blocks)? {
                    self.stale = true;
                    return Ok(None);
                }
                (oi, Some(blocks))
            }
            None => {
                if self.reader.peek_root()?.generation != self.root.generation {
                    self.stale = true;
                    return Ok(None);
                }
                (
                    t.orders.iter().position(|o| o.table_order).unwrap_or(0),
                    None,
                )
            }
        };
        let rows = self.rows_where(ci, &k.columns, key)?;
        if rows.is_empty() {
            return Ok(Some(rows));
        }
        let blocks = match blocks {
            Some(b) => b,
            None => {
                let table = &self.catalog.tables[ci];
                let order = &table.orders[oi];
                let mut out: Vec<Entry> = Vec::new();
                for r in &rows {
                    let at = key_of(table, order, r);
                    for e in self.covering(ci, oi, &at, &at)? {
                        if !out.iter().any(|x| x.first == e.first) {
                            out.push(e);
                        }
                    }
                }
                out
            }
        };
        if !self.lock_blocks(ci, oi, blocks, Waiting::Wait)? {
            self.stale = true;
            return Ok(None);
        }
        Ok(Some(rows))
    }

    /// The least and the greatest key of an order the reference leads where a key's references
    /// lie: the reference's values on its first levels, the rest open at both ends.
    fn run_of(
        &self,
        ci: usize,
        oi: usize,
        k: &ForeignKey,
        key: &[Datum],
    ) -> (Vec<KeyValue>, Vec<KeyValue>) {
        let t = &self.catalog.tables[ci];
        let order = &t.orders[oi];
        let mut row = vec![Datum::Null; t.columns.len()];
        for (&c, d) in k.columns.iter().zip(key) {
            row[c] = d.clone();
        }
        let full = key_of(t, order, &row);
        let m = k.columns.len();
        let mut lo = full[..m].to_vec();
        let mut hi = lo.clone();
        lo.extend((m..full.len()).map(|_| KeyValue::Null { first: true }));
        hi.extend((m..full.len()).map(|_| KeyValue::Null { first: false }));
        (lo, hi)
    }

    /// The blocks of an order, as the round reads it, where its keys from `lo` to `hi` lie.
    fn covering(
        &self,
        ti: usize,
        oi: usize,
        lo: &[KeyValue],
        hi: &[KeyValue],
    ) -> Result<Vec<Entry>> {
        let table = &self.catalog.tables[ti];
        let order = &table.orders[oi];
        let Some((_, _, top)) = self
            .root
            .orders
            .iter()
            .find(|(t, o, _)| *t == table.name && *o == order.name)
        else {
            return Err(Error::new(format!(
                "{}: the root names no {}",
                table.name, order.name
            )));
        };
        let (head, entries) = decode_top(&self.reader.read_extent(*top)?)?;
        let mut out = Vec::new();
        self.covering_below(&head, entries, head.height, lo, hi, &mut out)?;
        Ok(out)
    }

    fn covering_below(
        &self,
        head: &Head,
        entries: Vec<Entry>,
        height: u32,
        lo: &[KeyValue],
        hi: &[KeyValue],
        out: &mut Vec<Entry>,
    ) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let (a, b) = (pick(&entries, lo), pick(&entries, hi));
        for e in &entries[a..=b] {
            if height > 1 {
                let below = decode_page(&self.reader.read_extent(e.child)?, head)?;
                self.covering_below(head, below, height - 1, lo, hi, out)?;
            } else {
                out.push(e.clone());
            }
        }
        Ok(())
    }

    /// The conditions on a table's columns that keep it to one key, as a question writes them.
    fn conditions(t: &Table, columns: &[usize], key: &[Datum]) -> String {
        columns
            .iter()
            .zip(key)
            .map(|(&c, d)| {
                format!(
                    "r.\"{}\" = {}",
                    t.columns[c].name.replace('"', "\"\""),
                    literal(d, t.columns[c].ty)
                )
            })
            .collect::<Vec<_>>()
            .join(" AND ")
    }

    /// How many of a table's rows hold a key, as the round reads the file.
    fn count_where(&self, ti: usize, columns: &[usize], key: &[Datum]) -> Result<i64> {
        let t = &self.catalog.tables[ti];
        let sql = format!(
            "SELECT count(*) FROM {} r WHERE {}",
            quoted(&t.name),
            Snail::conditions(t, columns, key)
        );
        let (b, _) = self.answer(&sql, None)?;
        Ok(b.column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .filter(|a| !a.is_empty())
            .map_or(0, |a| a.value(0)))
    }

    /// A table's rows that hold a key, every one where no columns are given, as the round reads
    /// the file.
    pub(super) fn rows_where(
        &self,
        ti: usize,
        columns: &[usize],
        key: &[Datum],
    ) -> Result<Vec<Row>> {
        let t = &self.catalog.tables[ti];
        let stored = t
            .columns
            .iter()
            .filter(|c| !c.is_virtual())
            .map(|c| format!("r.\"{}\"", c.name.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = if columns.is_empty() {
            format!("SELECT {stored} FROM {} r", quoted(&t.name))
        } else {
            format!(
                "SELECT {stored} FROM {} r WHERE {}",
                quoted(&t.name),
                Snail::conditions(t, columns, key)
            )
        };
        let (b, _) = self.answer(&sql, None)?;
        Ok(widen(t, rows_of(&b)?))
    }

    /// Checks every row of a child against a foreign key made valid on a table that holds rows:
    /// added, or validated.
    pub(super) fn key_validated(&mut self, ci: usize, fi: usize) -> Result<bool> {
        let rows = self.rows_where(ci, &[], &[])?;
        let mut t = Touched::default();
        t.written.insert((ci, fi), rows);
        self.keys_checked(&t)
    }
}

/// Refuses the rows a new snail is written from where a foreign key finds an orphan among them, as
/// PostgreSQL refuses a key added to tables that hold rows: each child's reference that holds no
/// NULL among its parent's keys, the parent's keys and the child's references walked in one order.
pub(crate) fn rows_held(catalog: &Catalog, tables: &[(String, Rows)]) -> Result<()> {
    let given = |name: &str| tables.iter().find(|(n, _)| n == name).map(|(_, r)| r);
    for child in &catalog.tables {
        let Some(rows) = given(&child.name) else {
            continue;
        };
        for k in child.foreign_keys.iter().filter(|k| k.enforced) {
            let parent = catalog
                .table(&k.parent)
                .expect("a foreign key's parent is in the file");
            let mut held: Vec<Vec<Datum>> = match given(&parent.name) {
                Some(p) => (0..p.len())
                    .map(|i| {
                        k.parent_columns
                            .iter()
                            .map(|&c| class(&datum_typed(&p.columns[c], i, parent.columns[c].ty)))
                            .collect()
                    })
                    .collect(),
                None => Vec::new(),
            };
            held.sort();
            let mut references: Vec<(Vec<Datum>, Vec<Datum>)> = Vec::new();
            for i in 0..rows.len() {
                let key: Vec<Datum> = k
                    .columns
                    .iter()
                    .map(|&c| datum_typed(&rows.columns[c], i, child.columns[c].ty))
                    .collect();
                let nulls = key.iter().filter(|d| **d == Datum::Null).count();
                if nulls == key.len() || (nulls > 0 && !k.full) {
                    continue;
                }
                if nulls > 0 {
                    return Err(Error::new(format!(
                        "insert or update on table \"{}\" violates foreign key constraint \"{}\": \
                         MATCH FULL does not allow mixing of null and nonnull key values.",
                        bare(&child.name),
                        k.name
                    )));
                }
                references.push((key.iter().map(class).collect(), key));
            }
            references.sort();
            let mut at = 0;
            for (c, key) in &references {
                while at < held.len() && held[at] < *c {
                    at += 1;
                }
                if at == held.len() || held[at] != *c {
                    return Err(Error::new(format!(
                        "insert or update on table \"{}\" violates foreign key constraint \"{}\": \
                         Key {} is not present in table \"{}\".",
                        bare(&child.name),
                        k.name,
                        shown(child, &k.columns, key),
                        bare(&parent.name)
                    )));
                }
            }
        }
    }
    Ok(())
}
