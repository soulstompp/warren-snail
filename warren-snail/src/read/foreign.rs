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
    /// The rows written into an EXCLUDE's strand, by its table and order, each as the round leaves it.
    excluded: BTreeMap<(usize, usize), Vec<Row>>,
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

/// Whether a row `b` that replaced `a` references what `a` did: neither holding a NULL, each value
/// equal, and a PERIOD's within `a`'s, as PostgreSQL's `ri_KeysEqual` reads a child's key.
fn same_reference(t: &Table, k: &ForeignKey, a: &Row, b: &Row) -> bool {
    let equal = k
        .equal_columns()
        .iter()
        .all(|&c| a[c] != Datum::Null && b[c] != Datum::Null && class(&a[c]) == class(&b[c]));
    if !equal || !k.period {
        return equal;
    }
    let p = k.columns[k.columns.len() - 1];
    match (period_of(&a[p]), period_of(&b[p])) {
        (Some(old), Some(new)) => {
            crate::composed::periods_hold(t.columns[p].ty, &[old], &new).unwrap_or(false)
        }
        _ => false,
    }
}

/// A period's value: none where it is NULL.
fn period_of(d: &Datum) -> Option<crate::composed::Item> {
    match d {
        Datum::Compound(c) => Some(c.item.clone()),
        _ => None,
    }
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

/// The schemas a name no schema qualifies is looked for in, as PostgreSQL's default search path
/// reads: the file's objects that name no schema lie in public.
const SEARCH_PATH: [&str; 1] = ["public"];

/// The schema an object of the file lies in: the one its name gives, else public.
fn schema_of(name: &str) -> &str {
    name.rsplit_once('.').map_or("public", |(s, _)| s)
}

/// A constraint SET CONSTRAINTS names, each with whether it is DEFERRABLE.
pub(super) enum Named<'c> {
    /// A foreign key: its table's place and its own.
    Key((usize, usize), bool),
    /// A PRIMARY KEY, a UNIQUE or an EXCLUDE: its table and its order.
    Order(&'c Table, &'c crate::catalog::Order, bool),
    /// A table's CHECK or a domain's, which is never DEFERRABLE.
    Check,
}

/// What a name SET CONSTRAINTS gives finds, as PostgreSQL's AfterTriggerSetState finds it: its
/// bare name, and every constraint of that name in the schema it names, or else in the first schema
/// of the search path holding one. Refused where it names another database or too many parts, a
/// schema the file names nothing in, or no constraint.
pub(super) fn named<'c, 'n>(catalog: &'c Catalog, n: &'n str) -> Result<(&'n str, Vec<Named<'c>>)> {
    let parts: Vec<&str> = n.split('.').collect();
    let (schema, bare) = match parts.as_slice() {
        [bare] => (None, *bare),
        [schema, bare] => (Some(*schema), *bare),
        [_, _, _] => {
            return Err(Error::new(format!(
                "cross-database references are not implemented: \"{n}\""
            )))
        }
        _ => {
            return Err(Error::new(format!(
                "improper qualified name (too many dotted names): {n}"
            )))
        }
    };
    let schemas: Vec<&str> = match schema {
        Some(s) if catalog.holds_schema(s) => vec![s],
        Some(s) => return Err(Error::new(format!("schema \"{s}\" does not exist"))),
        None => SEARCH_PATH.to_vec(),
    };
    let mut found = Vec::new();
    for s in schemas {
        for (ci, t) in catalog.tables.iter().enumerate() {
            if schema_of(&t.name) != s {
                continue;
            }
            for (fi, k) in t.foreign_keys.iter().enumerate() {
                if k.name == bare {
                    found.push(Named::Key((ci, fi), k.deferrable));
                }
            }
            for o in t.orders.iter().filter(|o| o.name == bare) {
                let deferrable = (o.constraint.as_ref().map(|k| k.deferrable))
                    .or(o.exclusion.as_ref().map(|e| e.deferrable));
                if let Some(d) = deferrable {
                    found.push(Named::Order(t, o, d));
                }
            }
            if t.checks.iter().any(|k| k.name == bare) {
                found.push(Named::Check);
            }
        }
        for d in &catalog.domains {
            if schema_of(&d.name) == s && d.checks.iter().any(|k| k.name == bare) {
                found.push(Named::Check);
            }
        }
        // The first schema holding a constraint of that name is the only one read.
        if !found.is_empty() {
            break;
        }
    }
    if found.is_empty() {
        return Err(Error::new(format!("constraint \"{bare}\" does not exist")));
    }
    Ok((bare, found))
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
                                if ours || !same_reference(child, k, o, n) {
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

    /// SET CONSTRAINTS: each constraint named, or ALL, deferred or immediate until the transaction
    /// ends; a foreign key made immediate has its deferred changes checked now. A name is found as
    /// PostgreSQL's AfterTriggerSetState finds it: in the schema it names, or else in the first
    /// schema of the search path holding a constraint of that name, every constraint of that name
    /// there; one not DEFERRABLE refused only where deferred. Outside a transaction it does
    /// nothing, as PostgreSQL warns.
    pub(super) fn constrained(&mut self, names: Option<Vec<String>>, deferred: bool) -> Result<()> {
        let mut keys: Vec<(usize, usize)> = Vec::new();
        let key_deferrable = |o: &crate::catalog::Order| {
            (o.constraint.as_ref().map(|k| k.deferrable))
                .or(o.exclusion.as_ref().map(|e| e.deferrable))
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
                        if let Some(o) = t.orders.iter().find(|o| key_deferrable(o) == Some(true)) {
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
                    let (bare, found) = named(&self.catalog, n)?;
                    let not_deferrable =
                        || Error::new(format!("constraint \"{bare}\" is not deferrable"));
                    for c in found {
                        match c {
                            Named::Key(at, true) => keys.push(at),
                            Named::Order(t, o, true) if deferred => {
                                return Err(Error::new(format!(
                                    "SET CONSTRAINTS {n} DEFERRED: {}.{} is a DEFERRABLE key, and \
                                     the snail checks a key at each statement's end",
                                    t.name, o.name
                                )))
                            }
                            Named::Key(_, false) | Named::Order(..) | Named::Check if deferred => {
                                return Err(not_deferrable())
                            }
                            _ => {}
                        }
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
                        fill::check_domains(&self.catalog.domains, &t, &m)?;
                        fill::check(&t, &m)?;
                        fill::without_overlaps(&t, &m)?;
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
                let held = if k.period {
                    self.holds_period(&k, &key)?
                } else {
                    self.holds_key(&k, &key)?
                };
                match held {
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
                if !k.period && !matches!(action, Action::Restrict) {
                    match self.holds_key(&k, &key)? {
                        None => return Ok(false),
                        Some(true) => continue,
                        Some(false) => {}
                    }
                }
                let referenced = if k.period {
                    self.period_referenced(ci, &k, &key)?
                } else {
                    self.referenced(ci, &k, &key)?
                };
                match referenced {
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
        let at = key_of(p, &p.orders[oi], &row)?;
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
            let (lo, hi) = self.run_of(ci, oi, k, key)?;
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
    /// the round's root; the blocks that hold one locked. Under a PERIOD, the rows whose PERIOD
    /// meets the key's, along an order its other columns lead. None where the round must run again.
    fn children(&mut self, ci: usize, k: &ForeignKey, key: &[Datum]) -> Result<Option<Vec<Row>>> {
        let t = &self.catalog.tables[ci];
        let (oi, blocks) = match t.order_led_by(k.equal_columns()) {
            Some(oi) => {
                let (lo, hi) = self.run_of(ci, oi, k, key)?;
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
        let rows = self.rows_where(ci, &k.columns, key, k.period)?;
        if rows.is_empty() {
            return Ok(Some(rows));
        }
        let blocks = match blocks {
            Some(b) => b,
            None => self.blocks_holding(ci, oi, &rows)?,
        };
        if !self.lock_blocks(ci, oi, blocks, Waiting::Wait)? {
            self.stale = true;
            return Ok(None);
        }
        Ok(Some(rows))
    }

    /// The blocks of an order, as the round reads it, that hold the rows given.
    fn blocks_holding(&self, ti: usize, oi: usize, rows: &[Row]) -> Result<Vec<Entry>> {
        let table = &self.catalog.tables[ti];
        let order = &table.orders[oi];
        let mut out: Vec<Entry> = Vec::new();
        for r in rows {
            let at = key_of(table, order, r)?;
            for e in self.covering(ti, oi, &at, &at)? {
                if !out.iter().any(|x| x.first == e.first) {
                    out.push(e);
                }
            }
        }
        Ok(out)
    }

    /// The least and the greatest key of an order the reference leads where a key's references
    /// lie: the reference's values on its first levels, the rest open at both ends; under a
    /// PERIOD, its other columns' values.
    fn run_of(
        &self,
        ci: usize,
        oi: usize,
        k: &ForeignKey,
        key: &[Datum],
    ) -> Result<(Vec<KeyValue>, Vec<KeyValue>)> {
        self.run(ci, oi, k.equal_columns(), key)
    }

    /// The least and the greatest key of an order whose first levels are the columns given, where
    /// they hold the values given: those values, the rest open at both ends.
    fn run(
        &self,
        ti: usize,
        oi: usize,
        columns: &[usize],
        values: &[Datum],
    ) -> Result<(Vec<KeyValue>, Vec<KeyValue>)> {
        let t = &self.catalog.tables[ti];
        let order = &t.orders[oi];
        let mut row = vec![Datum::Null; t.columns.len()];
        for (&c, d) in columns.iter().zip(values) {
            row[c] = d.clone();
        }
        let full = key_of(t, order, &row)?;
        let m = columns.len();
        let mut lo = full[..m].to_vec();
        let mut hi = lo.clone();
        lo.extend((m..full.len()).map(|_| KeyValue::Null { first: true }));
        hi.extend((m..full.len()).map(|_| KeyValue::Null { first: false }));
        Ok((lo, hi))
    }

    /// The parent of a foreign key, and its order that stands for the key referenced.
    fn referenced_order(&self, k: &ForeignKey) -> (usize, usize) {
        let pi = self
            .catalog
            .tables
            .iter()
            .position(|t| t.name == k.parent)
            .expect("a foreign key's parent is in the file");
        let oi = self.catalog.tables[pi]
            .orders
            .iter()
            .position(|o| o.name == k.key)
            .expect("the order a foreign key references");
        (pi, oi)
    }

    /// The parent's rows whose columns of a PERIOD key match a reference's other columns and whose
    /// PERIOD meets the reference's, with their PERIODs, as the round reads them; and whether the
    /// blocks where the rows of its other columns lie are the newest root's.
    fn periods_meeting(
        &self,
        k: &ForeignKey,
        key: &[Datum],
    ) -> Result<(Vec<Row>, Vec<crate::composed::Item>, bool)> {
        let (pi, oi) = self.referenced_order(k);
        let rows = self.rows_where(pi, &k.parent_columns, key, true)?;
        let pc = k.parent_columns[k.parent_columns.len() - 1];
        let periods = rows.iter().filter_map(|r| period_of(&r[pc])).collect();
        let n = key.len() - 1;
        let (lo, hi) = self.run(pi, oi, &k.parent_columns[..n], &key[..n])?;
        let blocks = self.covering(pi, oi, &lo, &hi)?;
        let fresh = self.fresh(pi, oi, &blocks)?;
        Ok((rows, periods, fresh))
    }

    /// Whether the periods of the parent's rows matching a reference's other columns together hold
    /// its PERIOD, as the round reads them once no writer can take one away or bring one: where
    /// they hold it, the blocks those rows lie in locked; where they do not, the blocks where the
    /// rows of its other columns lie still the newest root's. None where the round must run again.
    fn holds_period(&mut self, k: &ForeignKey, key: &[Datum]) -> Result<Option<bool>> {
        let (pi, oi) = self.referenced_order(k);
        let (rows, periods, fresh) = self.periods_meeting(k, key)?;
        let ty = self.catalog.tables[pi].columns[k.parent_columns[key.len() - 1]].ty;
        let Some(period) = period_of(&key[key.len() - 1]) else {
            return Ok(Some(true));
        };
        if !crate::composed::periods_hold(ty, &periods, &period)? {
            if !fresh {
                self.stale = true;
                return Ok(None);
            }
            return Ok(Some(false));
        }
        let blocks = self.blocks_holding(pi, oi, &rows)?;
        if !self.lock_blocks(pi, oi, blocks, Waiting::Wait)? {
            self.stale = true;
            return Ok(None);
        }
        Ok(Some(true))
    }

    /// Whether a child's row still references a key taken from the parent under a PERIOD, as
    /// PostgreSQL's NO ACTION asks it: a row whose other columns match the key's and whose PERIOD
    /// meets the key's, the part of its PERIOD within the key's not held by the periods of the
    /// parent's rows that match the key and meet its PERIOD now. The child's rows found and the
    /// parent's rows that hold them locked. None where the round must run again.
    fn period_referenced(
        &mut self,
        ci: usize,
        k: &ForeignKey,
        key: &[Datum],
    ) -> Result<Option<bool>> {
        let Some(rows) = self.children(ci, k, key)? else {
            return Ok(None);
        };
        if rows.is_empty() {
            return Ok(Some(false));
        }
        let (pi, oi) = self.referenced_order(k);
        let (parents, periods, fresh) = self.periods_meeting(k, key)?;
        let n = key.len() - 1;
        let ty = self.catalog.tables[ci].columns[k.columns[n]].ty;
        let taken = period_of(&key[n]).expect("a key taken holds no NULL");
        for r in &rows {
            let Some(p) = period_of(&r[k.columns[n]]) else {
                continue;
            };
            let within = crate::composed::period_intersection(ty, &p, &taken)?;
            if !crate::composed::periods_hold(ty, &periods, &within)? {
                if !fresh {
                    self.stale = true;
                    return Ok(None);
                }
                return Ok(Some(true));
            }
        }
        let blocks = self.blocks_holding(pi, oi, &parents)?;
        if !self.lock_blocks(pi, oi, blocks, Waiting::Wait)? {
            self.stale = true;
            return Ok(None);
        }
        Ok(Some(false))
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

    /// The conditions on a table's columns that keep it to one key, as a question writes them;
    /// under a PERIOD, its last column by `&&`.
    fn conditions(t: &Table, columns: &[usize], key: &[Datum], period: bool) -> String {
        columns
            .iter()
            .zip(key)
            .enumerate()
            .map(|(i, (&c, d))| {
                let op = if period && i + 1 == columns.len() {
                    "&&"
                } else {
                    "="
                };
                format!(
                    "r.\"{}\" {op} {}",
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
            Snail::conditions(t, columns, key, false)
        );
        let (b, _) = self.answer(&sql, None)?;
        Ok(b.column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .filter(|a| !a.is_empty())
            .map_or(0, |a| a.value(0)))
    }

    /// A table's rows that hold a key, every one where no columns are given, as the round reads
    /// the file; under a PERIOD, those whose last column meets its last value.
    pub(super) fn rows_where(
        &self,
        ti: usize,
        columns: &[usize],
        key: &[Datum],
        period: bool,
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
                Snail::conditions(t, columns, key, period)
            )
        };
        let (b, _) = self.answer(&sql, None)?;
        Ok(widen(t, rows_of(&b)?))
    }

    /// Checks every row of a child against a foreign key made valid on a table that holds rows:
    /// added, or validated.
    pub(super) fn key_validated(&mut self, ci: usize, fi: usize) -> Result<bool> {
        let rows = self.rows_where(ci, &[], &[], false)?;
        let mut t = Touched::default();
        t.written.insert((ci, fi), rows);
        self.keys_checked(&t)
    }

    /// The rows a table's changes write into each of its EXCLUDE's strands, as they stand now.
    pub(super) fn touch_exclusions(&mut self, ti: usize, changes: &[Change]) {
        let table = &self.catalog.tables[ti];
        for (oi, o) in table.orders.iter().enumerate() {
            if o.exclusion.is_none() {
                continue;
            }
            let rows = self.touched.excluded.entry((ti, oi)).or_default();
            for c in changes {
                match c {
                    Change::Insert(r) => rows.push(r.clone()),
                    Change::Delete(o) => rows.retain(|x| x != o),
                    Change::Replace(o, n) => {
                        rows.retain(|x| x != o);
                        rows.push(n.clone());
                    }
                }
            }
        }
    }

    /// Each row a statement wrote into an EXCLUDE's strand held against the rows the strand holds:
    /// writers of one strand taking turns, the blocks whose span the row's value meets locked, and
    /// a row conflicting with another refused. Returns false where the statement must run again.
    pub(super) fn exclusions_checked(&mut self) -> Result<bool> {
        let due = std::mem::take(&mut self.touched.excluded);
        for (&(ti, oi), rows) in &due {
            if rows.is_empty() {
                continue;
            }
            let b = crate::lock::block_byte_of(ti, oi, &[]);
            if !self.locks.contains(&b) {
                self.lock_byte(b, Waiting::Wait)?;
                self.locks.insert(b);
                // Another writer of the strand committed while it waited: run again on the newest root.
                if self.root.generation != self.reader.peek_root()?.generation {
                    self.stale = true;
                    return Ok(false);
                }
            }
            for r in rows {
                if !self.excluded_row(ti, oi, r)? {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    /// One row held against an EXCLUDE's strand: none conflicting, a row holding a NULL in one of
    /// its levels, or one its predicate does not admit, conflicting with none. Under a UNIQUE NULLS
    /// NOT DISTINCT WITHOUT OVERLAPS, a NULL conflicts with a NULL in its level.
    fn excluded_row(&mut self, ti: usize, oi: usize, r: &Row) -> Result<bool> {
        let t = &self.catalog.tables[ti];
        let o = &t.orders[oi];
        let operators = o.exclusion.as_ref().unwrap().operators.clone();
        let columns: Vec<usize> = o.levels.iter().map(|l| l.column()).collect();
        if o.nulls_distinct && columns.iter().any(|&c| r[c] == Datum::Null) {
            return Ok(true);
        }
        if let Some(p) = super::partial::Predicate::of(&self.catalog, t, o)? {
            if !p.admits(t, r) {
                return Ok(true);
            }
        }
        let blocks = self.spanning(ti, oi, r, &operators)?;
        // Locked as a statement locks the blocks it changes: those the round staged lie under the
        // locks of the blocks they began from.
        if !self.lock_blocks(ti, oi, blocks, Waiting::Wait)? {
            self.stale = true;
            return Ok(false);
        }
        let t = &self.catalog.tables[ti];
        let o = &t.orders[oi];
        // A row is told from another by the table's key, or by every column the strand holds where
        // the table's order is no unique key.
        let own = t.orders.iter().find(|x| x.table_order).unwrap();
        let key: Vec<usize> = if own.unique {
            own.levels.iter().map(|l| l.column()).collect()
        } else {
            o.columns.clone()
        };
        let column = |c: usize| format!("r.\"{}\"", t.columns[c].name.replace('"', "\"\""));
        let conditions = columns
            .iter()
            .zip(&operators)
            .map(|(&c, op)| match &r[c] {
                Datum::Null => format!("{} IS NULL", column(c)),
                d => format!("{} {op} {}", column(c), literal(d, t.columns[c].ty)),
            })
            .collect::<Vec<_>>()
            .join(" AND ");
        let admitted = o
            .predicate
            .as_ref()
            .map(|p| format!(" AND ({p})"))
            .unwrap_or_default();
        let sql = format!(
            "SELECT {} FROM {} r WHERE {conditions}{admitted}",
            key.iter()
                .map(|&c| column(c))
                .collect::<Vec<_>>()
                .join(", "),
            quoted(&t.name)
        );
        let (b, _) = self.answer(&sql, Some(&o.name))?;
        let mine: Vec<Datum> = key.iter().map(|&c| r[c].clone()).collect();
        let mut held = rows_of(&b)?;
        if let Some(at) = held.iter().position(|other| *other == mine) {
            held.swap_remove(at);
        }
        if !held.is_empty() {
            return Err(Error::new(format!(
                "conflicting key value violates exclusion constraint \"{}\"",
                o.name
            )));
        }
        Ok(true)
    }

    /// The blocks of an EXCLUDE's strand whose rows may conflict with a row: each spanned level's
    /// span meeting its value by its operator, and where its first level is compared by `=`, its key
    /// there between the block's first and last.
    fn spanning(&self, ti: usize, oi: usize, r: &Row, operators: &[String]) -> Result<Vec<Entry>> {
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
        let at = key_of(table, order, r)?;
        let probes: Vec<Option<(usize, crate::spans::Probe, bool)>> = order
            .levels
            .iter()
            .map(|l| {
                let c = l.column();
                let slot = order.leaf_slot(table, c)?;
                let k = head.span_slots.iter().position(|&s| s as usize == slot)?;
                let probe = match crate::spans::bounds(table.columns[c].ty, &r[c])? {
                    None => crate::spans::Probe::Empty,
                    Some((lo, hi)) => crate::spans::Probe::Span(lo, hi),
                };
                Some((k, probe, crate::spans::is_inet(table.columns[c].ty)))
            })
            .collect();
        let meets = |e: &Entry| {
            let first = operators[0] != "=" || (e.first[..1] <= at[..1] && at[..1] <= e.last[..1]);
            first
                && probes.iter().zip(operators).all(|(p, op)| match p {
                    Some((k, probe, inet)) => {
                        crate::spans::may_hold(op, &e.spans[*k], probe, *inet)
                    }
                    None => true,
                })
        };
        let mut out = Vec::new();
        let mut level = vec![(entries, head.height)];
        while let Some((entries, height)) = level.pop() {
            for e in entries.into_iter().filter(|e| meets(e)) {
                if height > 1 {
                    level.push((
                        decode_page(&self.reader.read_extent(e.child)?, &head)?,
                        height - 1,
                    ));
                } else {
                    out.push(e);
                }
            }
        }
        Ok(out)
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
            // Under a PERIOD, the parent's periods by the classes of its other columns.
            let n = k.equal_columns().len();
            let mut periods: BTreeMap<Vec<Datum>, Vec<crate::composed::Item>> = BTreeMap::new();
            if k.period {
                for h in &held {
                    if let Some(p) = period_of(&h[n]) {
                        periods.entry(h[..n].to_vec()).or_default().push(p);
                    }
                }
            }
            let mut at = 0;
            for (c, key) in &references {
                let found = if k.period {
                    let ty = child.columns[k.columns[n]].ty;
                    let p = period_of(&key[n]).expect("a reference holding no NULL");
                    let some = periods.get(&c[..n]).map_or(&[][..], |v| v.as_slice());
                    !matches!(crate::spans::bounds(ty, &key[n]), Some(None))
                        && crate::composed::periods_hold(ty, some, &p)?
                } else {
                    while at < held.len() && held[at] < *c {
                        at += 1;
                    }
                    at < held.len() && held[at] == *c
                };
                if !found {
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
