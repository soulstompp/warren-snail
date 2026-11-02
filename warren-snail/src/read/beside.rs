//! A transaction of one snapshot, at REPEATABLE READ or SERIALIZABLE, on two versions of each block it
//! writes. Its statements write the version its snapshot reads, its rows after the block's last, and
//! keep what its rows came to; nothing else reads that version, and it locks no block for it. Each
//! statement holds its rows to the newest root as it stands: a row found that another changed since
//! is refused, as a key another holds now is, and the monitor is told of the newest blocks. Its commit
//! writes its rows into the newest root, as a statement at READ COMMITTED writes them, under the
//! blocks' locks, tables and orders in their order and each order's locks in theirs.

use super::change::{datum_typed, entry_at, key_of, level_key, settle, Change, Node, Writing};
use super::statement::{Datum, Row};
use super::Snail;
use crate::catalog::{Order, Table};
use crate::directory::{build_from, encode_top, read_map, Head};
use crate::file::{Extent, Place, Root};
use crate::lock;
use crate::places::{null_key, KeyValue};
use crate::values::Values;
use crate::write::empty_head;
use crate::{Error, Result};
use std::collections::{BTreeMap, HashMap};

/// PostgreSQL's words for a row another changed since the transaction's snapshot.
pub(super) fn concurrent_update() -> Error {
    Error::new("could not serialize access due to concurrent update")
}

/// PostgreSQL's words for a row another deleted since the transaction's snapshot.
pub(super) fn concurrent_delete() -> Error {
    Error::new("could not serialize access due to concurrent delete")
}

/// What a commit's key checks find on the newest root: a statement's own check passed on its snapshot,
/// so another's commit moved it.
fn met_since(e: Error) -> Error {
    let said = e.to_string();
    let moved = said.starts_with("insert or update on table")
        || said.starts_with("update or delete on table")
        || said.starts_with("conflicting key value");
    match moved {
        true => concurrent_update(),
        false => e,
    }
}

/// What a transaction's rows on one table have come to so far: each row of its snapshot it took, each
/// row it wrote, a row it wrote and took again gone from them.
#[derive(Clone, Default)]
pub(super) struct OwnRows {
    changes: Vec<Option<Change>>,
    /// Where each row it wrote lies among them.
    written: HashMap<Row, Vec<usize>>,
    /// How many of the rows it wrote hold each key of a unique order, by the order's place.
    keys: HashMap<(usize, Vec<KeyValue>), i64>,
    /// The bytes of its rows' values, for the room.
    bytes: u64,
}

impl OwnRows {
    /// Whether a row is one the transaction wrote.
    pub(super) fn holds(&self, r: &Row) -> bool {
        self.written.get(r).is_some_and(|at| !at.is_empty())
    }

    fn add(&mut self, c: Change) {
        let weigh = |r: &Row| r.iter().map(datum_bytes).sum::<u64>();
        self.bytes += match &c {
            Change::Insert(r) | Change::Delete(r) => weigh(r),
            Change::Replace(o, n) => weigh(o) + weigh(n),
        };
        let (from, to) = match c {
            Change::Insert(n) => (None, Some(n)),
            Change::Delete(o) => (Some(o), None),
            Change::Replace(o, n) => (Some(o), Some(n)),
        };
        let at = from
            .as_ref()
            .and_then(|o| self.written.get_mut(o))
            .and_then(Vec::pop);
        let (base, at) = match at {
            Some(i) => match self.changes[i].take() {
                Some(Change::Replace(f, _)) => (Some(f), i),
                _ => (None, i),
            },
            None => {
                self.changes.push(None);
                (from, self.changes.len() - 1)
            }
        };
        if let Some(n) = &to {
            self.written.entry(n.clone()).or_default().push(at);
        }
        self.changes[at] = match (base, to) {
            (None, None) => None,
            (None, Some(n)) => Some(Change::Insert(n)),
            (Some(o), None) => Some(Change::Delete(o)),
            (Some(o), Some(n)) => Some(Change::Replace(o, n)),
        };
    }

    fn came_to(&self) -> Vec<Change> {
        self.changes.iter().flatten().cloned().collect()
    }

    /// The rows of its snapshot the transaction took.
    pub(super) fn taken(&self) -> impl Iterator<Item = &Row> {
        self.changes.iter().flatten().filter_map(|c| match c {
            Change::Delete(o) | Change::Replace(o, _) => Some(o),
            Change::Insert(_) => None,
        })
    }

    /// The rows the transaction wrote.
    pub(super) fn wrote(&self) -> impl Iterator<Item = &Row> {
        self.changes.iter().flatten().filter_map(|c| match c {
            Change::Insert(n) | Change::Replace(_, n) => Some(n),
            Change::Delete(_) => None,
        })
    }
}

/// The bytes a value of a row takes.
fn datum_bytes(d: &Datum) -> u64 {
    let held = match d {
        Datum::Text(s) => s.len(),
        Datum::Bytes(b) => b.len(),
        _ => 0,
    };
    std::mem::size_of::<Datum>() as u64 + held as u64
}

/// What became of a row since the transaction's snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Since {
    Same,
    Updated,
    Deleted,
}

/// A key of a block's row at a position, on the order the block lies on.
fn key_at(table: &Table, order: &Order, columns: &[Values], p: usize) -> Result<Vec<KeyValue>> {
    order
        .levels
        .iter()
        .enumerate()
        .map(|(li, &l)| {
            let column = &table.columns[l.column()];
            let d = datum_typed(&columns[order.slot(l.column()).unwrap()], p, column.ty);
            Ok(level_key(
                column,
                column.orderer()?,
                l,
                &d,
                null_key(order, li),
            ))
        })
        .collect()
}

impl Snail {
    /// A round's order as a root names it: none where the root names none.
    fn top_on(&self, root: &Root, table: &Table, order: &Order) -> Result<Option<(Head, Node)>> {
        let Some((_, _, top)) = root
            .orders
            .iter()
            .find(|(t, o, _)| *t == table.name && *o == order.name)
        else {
            return Ok(None);
        };
        let (head, entries) =
            crate::write::top_read(table, order, &self.reader.read_extent(*top)?)?;
        let node = Node {
            entries,
            height: head.height,
            kids: BTreeMap::new(),
            at: Extent::default(),
        };
        Ok(Some((head, node)))
    }

    /// Enters the round's room, what the transaction of one snapshot keeps of its own rows counted in
    /// it: their values, and the pages of the version it reads that hold them.
    pub(super) fn entered_round(
        &self,
        room: &std::rc::Rc<crate::room::Room>,
    ) -> crate::room::Entered {
        let entered = crate::room::entered(room);
        if entered.outermost() {
            room.hold_anyway(self.own_bytes());
        }
        entered
    }

    /// What a transaction of one snapshot keeps of its own rows.
    pub(super) fn own_bytes(&self) -> u64 {
        match self.own.is_empty() {
            true => 0,
            false => {
                self.own.values().map(|o| o.bytes).sum::<u64>()
                    + self.reader.staged_mark() * crate::cut::PAGE as u64
            }
        }
    }

    /// Runs `f` reading the newest root as it stands, nothing of the transaction's own in it and
    /// nothing marked.
    pub(super) fn on_newest<T>(&mut self, f: impl FnOnce(&mut Snail) -> Result<T>) -> Result<T> {
        let newest = self.reader.peek_root()?;
        let seen = std::mem::replace(&mut self.root, newest);
        let was = self.comparing.replace(true);
        let done = f(self);
        self.comparing.set(was);
        self.root = seen;
        done
    }

    /// One statement's rows at REPEATABLE READ or SERIALIZABLE: each row it found held to the newest
    /// root, the monitor told of the blocks they fall in there, each key it writes held to what the
    /// newest root and the transaction hold, then written into the version the transaction reads.
    pub(super) fn apply_beside(
        &mut self,
        ti: usize,
        table: &Table,
        changes: Vec<Change>,
    ) -> Result<(u64, u64)> {
        if self.own_from.is_none() {
            self.own_from = Some(self.root.clone());
        }
        self.format_holds(table, &changes)?;
        let found: Vec<&Row> = {
            let own = self.own.get(&table.name);
            changes
                .iter()
                .filter_map(|c| match c {
                    Change::Delete(o) | Change::Replace(o, _) => Some(o),
                    Change::Insert(_) => None,
                })
                .filter(|o| !own.is_some_and(|x| x.holds(o)))
                .collect()
        };
        match self.changed_since(ti, &found)? {
            Since::Same => {}
            Since::Updated => return Err(concurrent_update()),
            Since::Deleted => return Err(concurrent_delete()),
        }
        self.told_of_writes(ti, table, &changes)?;
        self.keys_named(ti, table, &changes)?;
        self.rows_kept(table, &changes)?;
        self.keys_unique(table, &changes)?;
        let mut stage = self.reader.stage();
        let mut orders = 0;
        for oi in 0..table.orders.len() {
            orders += u64::from(self.view_order(&mut stage, ti, table, oi, &changes)?);
        }
        self.touch(ti, &changes);
        self.touch_exclusions(ti, &changes);
        Ok((0, orders))
    }

    /// A snail of an older format holds a transaction's rows on one version of a block: a row written
    /// into a block another wrote since the snapshot is refused by name, with the way across.
    fn format_holds(&self, table: &Table, changes: &[Change]) -> Result<()> {
        let format = self.reader.format();
        if format >= crate::file::FORMAT {
            return Ok(());
        }
        let newest = self.reader.peek_root()?;
        let from = self.own_from.as_ref().expect("the transaction's snapshot");
        if newest.generation == from.generation {
            return Ok(());
        }
        for order in &table.orders {
            let effects = self.effects_on(table, order, changes)?;
            let (Some((nh, mut nt)), Some((sh, mut st))) = (
                self.top_on(&newest, table, order)?,
                self.top_on(from, table, order)?,
            ) else {
                continue;
            };
            for e in &effects {
                let same = match (nt.entries.is_empty(), st.entries.is_empty()) {
                    (true, true) => true,
                    (false, false) => {
                        let (mut np, mut sp) = (Vec::new(), Vec::new());
                        self.locate(&mut nt, &nh, &e.key, &mut np)?;
                        self.locate(&mut st, &sh, &e.key, &mut sp)?;
                        entry_at(&nt, &np).child == entry_at(&st, &sp).child
                    }
                    _ => false,
                };
                if !same {
                    return Err(Error::new(format!(
                        "a snail of format {format} writes no row into a block another wrote since \
                         the transaction's snapshot, each block lying in one version: put the snail \
                         `out` and `write` it afresh, in format {}",
                        crate::file::FORMAT
                    )));
                }
            }
        }
        Ok(())
    }

    /// What became of rows of a table since the transaction's snapshot, by the newest root: the first
    /// row that is no longer as it is given. A row lies where its key on the table's order puts it;
    /// where the block there is the block the snapshot names, it is as it was.
    pub(super) fn changed_since(&self, ti: usize, rows: &[&Row]) -> Result<Since> {
        if rows.is_empty() {
            return Ok(Since::Same);
        }
        let from = self.own_from.as_ref().unwrap_or(&self.root);
        let newest = self.reader.peek_root()?;
        if newest.generation == from.generation {
            return Ok(Since::Same);
        }
        let table = &self.catalog.tables[ti];
        let order = table
            .orders
            .iter()
            .find(|o| o.table_order)
            .unwrap_or(&table.orders[0]);
        let Some((head, mut top)) = self.top_on(&newest, table, order)? else {
            return Ok(Since::Deleted);
        };
        if top.entries.is_empty() {
            return Ok(Since::Deleted);
        }
        let mut seen = self.top_on(from, table, order)?;
        let mut by_block: BTreeMap<Vec<usize>, Vec<&Row>> = BTreeMap::new();
        for r in rows {
            let key = key_of(table, order, r)?;
            let mut path = Vec::new();
            self.locate(&mut top, &head, &key, &mut path)?;
            if let Some((sh, st)) = seen.as_mut().filter(|(_, st)| !st.entries.is_empty()) {
                let mut sp = Vec::new();
                self.locate(st, sh, &key, &mut sp)?;
                if entry_at(st, &sp).child == entry_at(&top, &path).child {
                    continue;
                }
            }
            by_block.entry(path).or_default().push(r);
        }
        let held =
            |r: &Row| -> Vec<Datum> { order.columns.iter().map(|&c| r[c].clone()).collect() };
        let identity: Vec<usize> = super::locking::key_columns(table)
            .into_iter()
            .filter_map(|c| order.slot(c))
            .collect();
        let types: Vec<_> = order.columns.iter().map(|&c| table.columns[c].ty).collect();
        for (path, rows) in by_block {
            let map = read_map(&self.reader, entry_at(&top, &path), &head.level_text)?;
            let columns = self.block_columns(table, order, &map)?;
            let positions = map.rows() as usize;
            let at = |p: usize| -> Vec<Datum> {
                columns
                    .iter()
                    .zip(&types)
                    .map(|(v, &ty)| datum_typed(v, p, ty))
                    .collect()
            };
            let dead = |p: usize| map.dead.contains(&(p as u32));
            for r in rows {
                let values = held(r);
                if (0..positions).any(|p| !dead(p) && at(p) == values) {
                    continue;
                }
                let same_key = |p: usize| {
                    let a = at(p);
                    identity.iter().all(|&s| a[s] == values[s])
                };
                let chained = |p: usize| map.chains.iter().any(|(c, _)| *c as usize == p);
                let updated = (0..positions).any(|p| !dead(p) && same_key(p))
                    || (0..positions).any(|p| dead(p) && chained(p) && at(p) == values);
                return Ok(if updated {
                    Since::Updated
                } else {
                    Since::Deleted
                });
            }
        }
        Ok(Since::Same)
    }

    /// Tells the monitor of the blocks a statement's rows fall in on the newest root, on each order
    /// holding a column they change.
    fn told_of_writes(&self, ti: usize, table: &Table, changes: &[Change]) -> Result<()> {
        if !self.marking() {
            return Ok(());
        }
        let newest = self.reader.peek_root()?;
        for (oi, order) in table.orders.iter().enumerate() {
            let effects = self.effects_on(table, order, changes)?;
            if effects.is_empty() {
                continue;
            }
            let mut bytes = Vec::new();
            match self.top_on(&newest, table, order)? {
                Some((head, mut top)) if !top.entries.is_empty() => {
                    for e in &effects {
                        let mut path = Vec::new();
                        self.locate(&mut top, &head, &e.key, &mut path)?;
                        bytes.push(lock::block_byte_of(ti, oi, &entry_at(&top, &path).first));
                    }
                    bytes.sort_unstable();
                    bytes.dedup();
                }
                _ => bytes.push(lock::block_byte_of(ti, oi, &[])),
            }
            self.mark_wrote(ti, oi, &bytes)?;
        }
        Ok(())
    }

    /// Each key of a unique order that a statement's rows write or take, named until the transaction
    /// ends, waiting for any other that holds it, as PostgreSQL's unique check waits for a writer of
    /// that key; while the transaction names at most `NAMED` bytes: past them, its commit meets the
    /// key.
    fn keys_named(&mut self, ti: usize, table: &Table, changes: &[Change]) -> Result<()> {
        for (oi, order) in table.orders.iter().enumerate().filter(|(_, o)| o.unique) {
            let predicate = super::partial::Predicate::of(&self.catalog, table, order)?;
            let admits = |r: &Row| predicate.as_ref().is_none_or(|p| p.admits(table, r));
            for c in changes {
                let (old, new) = match c {
                    Change::Insert(n) => (None, Some(n)),
                    Change::Delete(o) => (Some(o), None),
                    Change::Replace(o, n) => (Some(o), Some(n)),
                };
                let of = |r: Option<&Row>| -> Result<Option<Vec<KeyValue>>> {
                    r.filter(|x| admits(x))
                        .map(|x| key_of(table, order, x))
                        .transpose()
                };
                let (from, to) = (of(old)?, of(new)?);
                if from == to {
                    continue;
                }
                for k in [from, to].into_iter().flatten() {
                    if order.nulls_distinct && k.iter().any(KeyValue::is_null) {
                        continue;
                    }
                    if self.row_locks.len() >= super::locking::NAMED {
                        return Ok(());
                    }
                    let byte = lock::key_byte(ti, oi, &k);
                    self.take_bytes(&[(byte, false)], super::change::Waiting::Wait)?;
                }
            }
        }
        Ok(())
    }

    /// A statement's rows added to what the transaction's rows have come to.
    fn rows_kept(&mut self, table: &Table, changes: &[Change]) -> Result<()> {
        let unique: Vec<(usize, &Order, Option<super::partial::Predicate>)> = table
            .orders
            .iter()
            .enumerate()
            .filter(|(_, o)| o.unique)
            .map(|(oi, o)| {
                Ok((
                    oi,
                    o,
                    super::partial::Predicate::of(&self.catalog, table, o)?,
                ))
            })
            .collect::<Result<_>>()?;
        let own = self.own.entry(table.name.clone()).or_default();
        for c in changes {
            let (old, new) = match c {
                Change::Insert(n) => (None, Some(n)),
                Change::Delete(o) => (Some(o), None),
                Change::Replace(o, n) => (Some(o), Some(n)),
            };
            for (oi, order, predicate) in &unique {
                let admits = |r: &Row| predicate.as_ref().is_none_or(|p| p.admits(table, r));
                if let Some(o) = old.filter(|o| own.holds(o) && admits(o)) {
                    *own.keys.entry((*oi, key_of(table, order, o)?)).or_default() -= 1;
                }
                if let Some(n) = new.filter(|n| admits(n)) {
                    *own.keys.entry((*oi, key_of(table, order, n)?)).or_default() += 1;
                }
            }
            own.add(c.clone());
        }
        Ok(())
    }

    /// Refuses a key a statement writes on a unique order where the transaction's own rows hold it
    /// twice, or the newest root holds it in a row the transaction did not take, as PostgreSQL's unique
    /// check sees what is committed whatever the snapshot.
    fn keys_unique(&self, table: &Table, changes: &[Change]) -> Result<()> {
        let own = &self.own[&table.name];
        for (oi, order) in table.orders.iter().enumerate().filter(|(_, o)| o.unique) {
            let predicate = super::partial::Predicate::of(&self.catalog, table, order)?;
            let admits = |r: &Row| predicate.as_ref().is_none_or(|p| p.admits(table, r));
            let mut asked: Vec<Vec<KeyValue>> = Vec::new();
            for c in changes {
                let (old, new) = match c {
                    Change::Insert(n) => (None, n),
                    Change::Replace(o, n) => (Some(o), n),
                    Change::Delete(_) => continue,
                };
                if !admits(new) {
                    continue;
                }
                let k = key_of(table, order, new)?;
                if order.nulls_distinct && k.iter().any(KeyValue::is_null) {
                    continue;
                }
                if let Some(o) = old.filter(|o| admits(o)) {
                    if key_of(table, order, o)? == k {
                        continue;
                    }
                }
                if own.keys.get(&(oi, k.clone())).is_some_and(|n| *n > 1) {
                    return Err(crate::write::key_shared(table, order, &k));
                }
                asked.push(k);
            }
            if asked.is_empty() {
                continue;
            }
            let newest = self.reader.peek_root()?;
            let Some((head, mut top)) = self.top_on(&newest, table, order)? else {
                continue;
            };
            if top.entries.is_empty() {
                continue;
            }
            let mut by_block: BTreeMap<Vec<usize>, Vec<Vec<KeyValue>>> = BTreeMap::new();
            for k in asked {
                let mut path = Vec::new();
                self.locate(&mut top, &head, &k, &mut path)?;
                by_block.entry(path).or_default().push(k);
            }
            for (path, keys) in by_block {
                let map = read_map(&self.reader, entry_at(&top, &path), &head.level_text)?;
                let columns = self.block_columns(table, order, &map)?;
                let mut live: HashMap<Vec<KeyValue>, i64> = HashMap::new();
                for p in 0..map.rows() as usize {
                    if !map.dead.contains(&(p as u32)) {
                        let k = key_at(table, order, &columns, p)?;
                        if keys.contains(&k) {
                            *live.entry(k).or_default() += 1;
                        }
                    }
                }
                for (k, n) in live {
                    let mut took = 0;
                    for o in own.taken().filter(|o| admits(o)) {
                        took += i64::from(key_of(table, order, o)? == k);
                    }
                    if n > took {
                        return Err(crate::write::key_shared(table, order, &k));
                    }
                }
            }
        }
        Ok(())
    }

    /// One order's part of a statement on the version the transaction reads. Returns whether the
    /// statement changed a column it holds.
    fn view_order(
        &mut self,
        w: &mut dyn Place,
        ti: usize,
        table: &Table,
        oi: usize,
        changes: &[Change],
    ) -> Result<bool> {
        let order = &table.orders[oi];
        let effects = self.effects_on(table, order, changes)?;
        if effects.is_empty() {
            return Ok(false);
        }
        if effects.iter().any(|e| e.key.iter().any(KeyValue::is_null)) {
            crate::file::keys_null(self.reader.format(), table, order)?;
        }
        let at = self
            .root
            .orders
            .iter()
            .position(|(t, o, _)| *t == table.name && *o == order.name)
            .ok_or_else(|| {
                Error::new(format!("{}: the root names no {}", table.name, order.name))
            })?;
        let (mut head, entries) = crate::write::top_read(
            table,
            order,
            &self.reader.read_extent(self.root.orders[at].2)?,
        )?;
        let mut top = Node {
            entries,
            height: head.height,
            kids: BTreeMap::new(),
            at: Extent::default(),
        };
        let mut replaced = BTreeMap::new();
        if top.entries.is_empty() {
            if effects.iter().any(|e| e.delete) {
                return Err(Error::new(format!("{} holds no row to delete", order.name)));
            }
            let rows: Vec<Vec<Datum>> = effects.into_iter().map(|e| e.values).collect();
            let (entries, first) = self.written_in_order(w, table, order, rows)?;
            top.entries = entries;
            top.height = 1;
            head = first;
        } else {
            let mut by_block: BTreeMap<Vec<usize>, Vec<super::change::Effect>> = BTreeMap::new();
            for e in effects {
                let mut path = Vec::new();
                self.locate(&mut top, &head, &e.key, &mut path)?;
                by_block.entry(path).or_default().push(e);
            }
            for (path, effects) in by_block {
                let entry = entry_at(&top, &path).clone();
                let (new, _) = self.write_block_afresh(
                    w,
                    table,
                    order,
                    &head,
                    &entry,
                    effects,
                    Writing::Beside,
                )?;
                if let Some(n) = new.first().filter(|n| n.first != entry.first) {
                    let was = self.block_read_byte(ti, oi, &entry.first);
                    self.first_bytes
                        .insert(lock::block_byte_of(ti, oi, &n.first), was);
                }
                replaced.insert(path, new);
            }
        }
        settle(
            &mut top,
            &mut Vec::new(),
            &replaced,
            w,
            &mut Vec::new(),
            head.separated,
        )?;
        let (bytes, _) = build_from(head, top.entries, top.height, &mut |b| w.place(b))?;
        self.root.orders[at].2 = w.place(&bytes)?;
        Ok(true)
    }

    /// The lock byte a read of a block marks: the first key the transaction's snapshot gives it,
    /// where its rows lowered it.
    pub(super) fn block_read_byte(&self, ti: usize, oi: usize, first: &[KeyValue]) -> u64 {
        let b = lock::block_byte_of(ti, oi, first);
        self.first_bytes.get(&b).copied().unwrap_or(b)
    }

    /// Where a transaction commits: the foreign keys deferred to its commit checked, and a transaction
    /// of one snapshot's rows written into the newest root.
    pub(super) fn commit_checked(&mut self) -> Result<()> {
        if !self.held {
            if self.pending.is_empty() {
                return Ok(());
            }
            self.renew_in_transaction()?;
            return self.keys_at_commit();
        }
        if !self.pending.is_empty() {
            self.keys_at_commit()?;
        }
        let own = std::mem::take(&mut self.own);
        self.held = false;
        if own.values().all(|o| o.changes.iter().all(Option::is_none)) {
            return Ok(());
        }
        self.committing = true;
        let written = self.rows_into_newest(&own);
        self.committing = false;
        written
    }

    /// A transaction's rows written into the newest root, as a statement at READ COMMITTED writes them,
    /// and its keys checked there: run again from the newest root where a block it locks was written
    /// meanwhile.
    fn rows_into_newest(&mut self, own: &BTreeMap<String, OwnRows>) -> Result<()> {
        let mark = self.mark();
        let seen_catalog = self.root.catalog;
        loop {
            self.stale = false;
            self.touched = Default::default();
            let root = self.root_for_commit(seen_catalog, own)?;
            self.root = root;
            for (name, rows) in own {
                let ti = self
                    .catalog
                    .tables
                    .iter()
                    .position(|t| t.name == *name)
                    .ok_or_else(concurrent_update)?;
                self.apply(ti, rows.came_to())?;
                if self.stale {
                    break;
                }
            }
            if !self.stale && self.exclusions_checked().map_err(met_since)? && !self.stale {
                let touched = std::mem::take(&mut self.touched);
                self.keys_checked(&touched).map_err(met_since)?;
            }
            if !self.stale {
                self.pending = Default::default();
                return Ok(());
            }
            self.back_to(&mark);
        }
    }

    /// The root a transaction's commit writes its rows on: the newest, with each order it declared as
    /// it declared it; refused where another's DDL since changed a table it wrote.
    fn root_for_commit(
        &mut self,
        seen_catalog: Extent,
        own: &BTreeMap<String, OwnRows>,
    ) -> Result<Root> {
        let mut root = self.reader.peek_root()?;
        match &self.declared {
            Some(d) => {
                let mut stage = self.reader.stage();
                for (tn, on) in d.made() {
                    let t = self.catalog.table(tn).expect("a table it declared");
                    let o = t
                        .orders
                        .iter()
                        .find(|x| x.name == *on)
                        .expect("an order it declared");
                    let top =
                        stage.place(&encode_top(&empty_head(t, o, self.reader.format()), &[]))?;
                    match root.orders.iter_mut().find(|(a, b, _)| a == tn && b == on) {
                        Some(named) => named.2 = top,
                        None => root.orders.push((tn.clone(), on.clone(), top)),
                    }
                }
            }
            None if root.catalog != seen_catalog => {
                let catalog = Snail::catalog(&self.reader, &root)?;
                for name in own.keys() {
                    let (Some(a), Some(b)) = (self.catalog.table(name), catalog.table(name)) else {
                        return Err(concurrent_update());
                    };
                    if !a.same_shape(b) {
                        return Err(concurrent_update());
                    }
                }
                self.catalog = catalog;
            }
            None => {}
        }
        Ok(root)
    }
}
