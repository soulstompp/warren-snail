//! A serializable transaction's round. Each entry a round looks at is marked, each block a write
//! changes tests the marks on it, each read tests the writers of what it marked, and each edge found
//! is tested for two in a row, as PostgreSQL's monitor tests them; the failure in its words. A running
//! transaction's marks are locks on bytes past the file's pages, and a committed one's are written
//! into the records the root names, kept while a round older than its commit runs.

use super::walk::Walk;
use super::Snail;
use crate::file::{Extent, In, Out, Place, Writer};
use crate::lock;
use crate::{Error, Result};
use std::collections::{BTreeMap, BTreeSet};

/// What PostgreSQL's monitor says as it refuses a transaction.
const FAILED: &str = "could not serialize access due to read/write dependencies among transactions";

/// The entries a transaction marks on one order before it marks the order whole, as PostgreSQL's
/// default `max_pred_locks_per_relation` promotes a relation's.
const MARKS_AN_ORDER: usize = 32;

fn failed(reason: &str) -> Error {
    Error::new(format!(
        "{FAILED}\nDETAIL:  Reason code: {reason}\nHINT:  The transaction might succeed if retried."
    ))
}

const READ_ONLY: u8 = 1;
const DID_WRITE: u8 = 2;
const ROLLED_BACK: u8 = 4;

/// A committed serializable transaction, as the records hold it; or, its mark 0, only the blocks a
/// commit cut; or, rolled back, only those it doomed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Record {
    id: u64,
    snapshot: u64,
    commit: u64,
    flags: u8,
    /// The first commit among the transactions it read what they wrote, those that committed before it.
    conflict_out: Option<u64>,
    marks: Vec<u64>,
    wrote: Vec<u64>,
    ins: Vec<u64>,
    outs: Vec<u64>,
    dooms: Vec<u64>,
    /// Each block cut, a block whose rows lay in another: it, and that other.
    cuts: Vec<(u64, u64)>,
}

fn encode(records: &[Record]) -> Vec<u8> {
    let mut o = Out::default();
    o.u32(records.len() as u32);
    for r in records {
        o.u64(r.id);
        o.u64(r.snapshot);
        o.u64(r.commit);
        o.u8(r.flags);
        o.u64(r.conflict_out.unwrap_or(u64::MAX));
        for v in [&r.marks, &r.wrote, &r.ins, &r.outs, &r.dooms] {
            o.u32(v.len() as u32);
            for x in v {
                o.u64(*x);
            }
        }
        o.u32(r.cuts.len() as u32);
        for (a, b) in &r.cuts {
            o.u64(*a);
            o.u64(*b);
        }
    }
    o.0
}

fn decode(buf: &[u8]) -> Result<Vec<Record>> {
    let mut i = In::new(buf);
    (0..i.u32()?)
        .map(|_| {
            let (id, snapshot, commit, flags) = (i.u64()?, i.u64()?, i.u64()?, i.u8()?);
            let conflict_out = Some(i.u64()?).filter(|&c| c != u64::MAX);
            let mut lists = Vec::with_capacity(5);
            for _ in 0..5 {
                let list: Vec<u64> = (0..i.u32()?).map(|_| i.u64()).collect::<Result<_>>()?;
                lists.push(list);
            }
            let cuts = (0..i.u32()?)
                .map(|_| Ok((i.u64()?, i.u64()?)))
                .collect::<Result<_>>()?;
            let dooms = lists.pop().unwrap();
            let outs = lists.pop().unwrap();
            let ins = lists.pop().unwrap();
            let wrote = lists.pop().unwrap();
            let marks = lists.pop().unwrap();
            Ok(Record {
                id,
                snapshot,
                commit,
                flags,
                conflict_out,
                marks,
                wrote,
                ins,
                outs,
                dooms,
                cuts,
            })
        })
        .collect()
}

/// A serializable transaction under way on this writer, from its first statement that reads.
pub(super) struct Serial {
    /// Whether it still marks what it reads: a READ ONLY transaction beside no writer that can hurt
    /// it, or found safe once those it ran beside ended, does not.
    active: bool,
    id: u64,
    read_only: bool,
    snapshot: u64,
    /// Those that were not READ ONLY as it took its snapshot, while it is READ ONLY: a slot and a
    /// mark each, until each has ended.
    possible: Vec<(u64, u64)>,
    /// Whether one of them committed with an edge out to a commit before its snapshot.
    not_safe: bool,
    /// What it marked, by table and order, each entry by its lock's byte; and the orders marked whole.
    marks: BTreeMap<(usize, usize), BTreeSet<u64>>,
    whole: BTreeSet<(usize, usize)>,
    /// The blocks and orders it wrote, by their bytes.
    wrote: BTreeSet<u64>,
    did_write: bool,
    /// The edges it found, out to others and in from others, and those it doomed.
    outs: BTreeSet<u64>,
    ins: BTreeSet<u64>,
    dooms: BTreeSet<u64>,
    /// Where it passed its commit's test, the generation its commit makes.
    prepared: Option<u64>,
    /// The first commit it read over, once it has passed.
    conflict_out: Option<u64>,
}

/// What a test reads of a transaction it is told of by its mark.
#[derive(Clone, Copy)]
enum Who<'a> {
    Me,
    Running(u64),
    Committed(&'a Record),
    Gone,
}

/// The monitor as a transaction reads it, at its turn: those running, and the records.
struct View {
    me: u64,
    running: Vec<(u64, u64)>,
    records: Vec<Record>,
    /// Each block's blocks it was cut from, by its byte.
    parents: BTreeMap<u64, Vec<u64>>,
}

impl View {
    fn who(&self, id: u64) -> Who<'_> {
        if id == self.me {
            return Who::Me;
        }
        if let Some(r) = self
            .records
            .iter()
            .find(|r| r.id == id && r.flags & ROLLED_BACK == 0)
        {
            return Who::Committed(r);
        }
        match self.running.iter().find(|(_, i)| *i == id) {
            Some((slot, _)) => Who::Running(*slot),
            None => Who::Gone,
        }
    }

    /// A block's byte and every byte of a block it was cut from.
    fn with_parents(&self, byte: u64) -> Vec<u64> {
        let mut out = vec![byte];
        let mut at = 0;
        while at < out.len() {
            if let Some(ps) = self.parents.get(&out[at]) {
                for p in ps {
                    if !out.contains(p) {
                        out.push(*p);
                    }
                }
            }
            at += 1;
        }
        out
    }
}

impl Snail {
    /// The records the root the snail reads names: for the tests of when they go.
    #[doc(hidden)]
    pub fn serial_records_kept(&self) -> Result<usize> {
        Ok(self.records_at(self.root.serial)?.len())
    }

    /// Whether the rounds of this snail mark what they read.
    pub(super) fn marking(&self) -> bool {
        self.monitor.is_some() || self.serial.borrow().as_ref().is_some_and(|s| s.active)
    }

    /// The monitor that is told of this snail's rounds, inside a host that has one: the file keeps no
    /// marks. Told of what its rounds read, what its writes change and what its commits cut.
    pub fn monitored_by(&mut self, monitor: std::rc::Rc<dyn crate::carry::Monitor>) {
        self.monitor = Some(monitor);
    }

    fn the_writer_file(&self) -> Result<std::fs::File> {
        let w = self
            .writer
            .as_ref()
            .ok_or_else(|| Error::new("this snail was opened to read; open it to write"))?;
        Ok(w.file().try_clone()?)
    }

    /// Refuses SERIALIZABLE named on a snail that cannot run it: of an older format, with the way
    /// across; or marked a replica, as PostgreSQL's hot standby refuses it.
    pub(super) fn serial_allowed(&self, named_here: bool) -> Result<()> {
        if self.reader.format() < crate::file::FORMAT {
            return Err(crate::file::serial_refused(self.reader.format()));
        }
        if named_here && self.root.replica.is_some() {
            return Err(Error::new(
                "cannot use serializable mode in a hot standby\nHINT:  You can use REPEATABLE READ \
                 instead.",
            ));
        }
        Ok(())
    }

    /// The first statement of a serializable transaction that reads: its marks taken and its snapshot,
    /// the newest root, at the root's turn, so that no commit is under way that did not see it;
    /// a READ ONLY one beside no transaction that writes marks nothing, and a DEFERRABLE one waits for
    /// a snapshot no such transaction can hurt.
    pub(super) fn serial_snapshot(&mut self, begun: bool) -> Result<()> {
        if self.serial.borrow().is_some() {
            return Ok(());
        }
        if self.root.replica.is_some() {
            return Err(Error::new(
                "cannot use serializable mode in a hot standby\nDETAIL:  \
                 \"default_transaction_isolation\" is set to \"serializable\".\nHINT:  You can use \
                 \"SET default_transaction_isolation = 'repeatable read'\" to change the default.",
            ));
        }
        let read_only = self.levels.read_only(begun);
        let deferrable = read_only && self.levels.deferrable(begun);
        let file = self.the_writer_file()?;
        loop {
            let id = lock::serial_id();
            lock::take_root_turn(&file)?;
            let taken = (|| {
                lock::mark_serial(&file, self.slot, id, true)?;
                if !read_only {
                    lock::mark_writes(&file, self.slot, true)?;
                }
                if begun {
                    self.renew_in_transaction()?;
                } else {
                    self.renew()?;
                }
                let snapshot = self.root.generation;
                lock::mark_snapshot(&file, self.slot, snapshot, true)?;
                let mut possible = Vec::new();
                if read_only {
                    for (slot, other) in lock::serials(&file)? {
                        if lock::writes_in(&file, slot)? {
                            possible.push((slot, other));
                        }
                    }
                }
                Ok::<_, Error>((snapshot, possible))
            })();
            let given = lock::give_root_turn(&file);
            let (snapshot, possible) = match taken {
                Ok(took) => took,
                Err(e) => {
                    lock::mark_writes(&file, self.slot, false)?;
                    lock::mark_serial(&file, self.slot, id, false)?;
                    return Err(e);
                }
            };
            given?;
            let mut serial = Serial {
                active: !(read_only && possible.is_empty()),
                id,
                read_only,
                snapshot,
                possible,
                not_safe: false,
                marks: BTreeMap::new(),
                whole: BTreeSet::new(),
                wrote: BTreeSet::new(),
                did_write: false,
                outs: BTreeSet::new(),
                ins: BTreeSet::new(),
                dooms: BTreeSet::new(),
                prepared: None,
                conflict_out: None,
            };
            if !serial.active {
                self.release_marks(&file, &mut serial)?;
                *self.serial.borrow_mut() = Some(serial);
                return Ok(());
            }
            // It waits as long as it takes, holding no lock, until each beside it has ended or one has
            // shown the snapshot unsafe.
            if deferrable {
                let mut nap = std::time::Duration::from_millis(1);
                loop {
                    self.settle_read_only(&file, &mut serial)?;
                    if serial.not_safe || !serial.active {
                        break;
                    }
                    std::thread::sleep(nap);
                    nap = (nap * 2).min(std::time::Duration::from_millis(50));
                }
                if serial.not_safe {
                    self.release_marks(&file, &mut serial)?;
                    continue;
                }
            }
            *self.serial.borrow_mut() = Some(serial);
            return Ok(());
        }
    }

    /// A READ ONLY transaction's account of those it ran beside: each that ended goes from it, and it
    /// is no longer safe where one committed having read over a commit before its snapshot. Once none
    /// is left and it is safe, it marks nothing more.
    fn settle_read_only(&self, file: &std::fs::File, s: &mut Serial) -> Result<()> {
        if !s.read_only || s.not_safe || !s.active {
            return Ok(());
        }
        let running = lock::serials(file)?;
        let records = self.records_now()?;
        let snapshot = s.snapshot;
        let mut not_safe = false;
        s.possible.retain(|p| {
            if running.contains(p) {
                return true;
            }
            if let Some(r) = records.iter().find(|r| r.id == p.1) {
                if r.flags & DID_WRITE != 0 && r.conflict_out.is_some_and(|c| c <= snapshot) {
                    not_safe = true;
                }
            }
            false
        });
        s.not_safe = not_safe;
        if s.possible.is_empty() && !s.not_safe {
            lock::take_serial_turn(file)?;
            let released = self.release_marks(file, s);
            lock::give_serial_turn(file)?;
            released?;
        }
        Ok(())
    }

    /// Lets every mark of the transaction go; its mark as a transaction among them, so that it is
    /// seen as ended, and it marks nothing more.
    fn release_marks(&self, file: &std::fs::File, s: &mut Serial) -> Result<()> {
        for bytes in s.marks.values() {
            for &b in bytes {
                lock::mark_read(file, self.slot, b, false)?;
            }
        }
        for &(ti, oi) in &s.whole {
            lock::mark_read(file, self.slot, lock::order_byte(ti, oi), false)?;
        }
        for &b in &s.wrote {
            lock::mark_held(file, self.slot, b, false)?;
        }
        for &o in &s.outs {
            lock::mark_edge(file, self.slot, true, o, false)?;
        }
        for &o in &s.ins {
            lock::mark_edge(file, self.slot, false, o, false)?;
        }
        for &o in &s.dooms {
            lock::mark_doom(file, self.slot, o, false)?;
        }
        if let Some(g) = s.prepared {
            lock::mark_prepared(file, self.slot, g, false)?;
        }
        lock::mark_snapshot(file, self.slot, s.snapshot, false)?;
        lock::mark_writes(file, self.slot, false)?;
        lock::mark_serial(file, self.slot, s.id, false)?;
        s.marks.clear();
        s.whole.clear();
        s.wrote.clear();
        s.outs.clear();
        s.ins.clear();
        s.dooms.clear();
        s.prepared = None;
        s.active = false;
        Ok(())
    }

    /// The records as the newest root names them.
    fn records_now(&self) -> Result<Vec<Record>> {
        let root = self.reader.peek_root()?;
        self.records_at(root.serial)
    }

    fn records_at(&self, e: Extent) -> Result<Vec<Record>> {
        if e.bytes == 0 {
            return Ok(Vec::new());
        }
        decode(&self.reader.read_extent(e)?)
    }

    fn view(&self, file: &std::fs::File, me: u64) -> Result<View> {
        let running = lock::serials(file)?;
        let records = self.records_now()?;
        let mut parents: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
        for r in &records {
            for (child, parent) in &r.cuts {
                parents.entry(*child).or_default().push(*parent);
            }
        }
        Ok(View {
            me,
            running,
            records,
            parents,
        })
    }

    fn doomed(&self, file: &std::fs::File, v: &View, s: &Serial, id: u64) -> Result<bool> {
        if s.dooms.contains(&id) {
            return Ok(true);
        }
        for (slot, _) in &v.running {
            if lock::doomed_by(file, *slot, id)? {
                return Ok(true);
            }
        }
        Ok(v.records.iter().any(|r| r.dooms.contains(&id)))
    }

    /// Those a transaction has an edge with: out to them, or in from them.
    fn edges(
        &self,
        file: &std::fs::File,
        v: &View,
        s: &Serial,
        id: u64,
        out: bool,
    ) -> Result<BTreeSet<u64>> {
        let mut edges = BTreeSet::new();
        match v.who(id) {
            Who::Me => edges.extend(if out { &s.outs } else { &s.ins }),
            Who::Running(slot) => edges.extend(lock::edges_in_slot(file, slot, out)?),
            Who::Committed(r) => edges.extend(if out { &r.outs } else { &r.ins }),
            Who::Gone => {}
        }
        // Found from the other end: by this transaction, by one running, or by one committed since.
        if id != s.id && (if out { &s.ins } else { &s.outs }).contains(&id) {
            edges.insert(s.id);
        }
        for &(slot, other) in &v.running {
            if other != id && lock::edge_with(file, slot, !out, id)? {
                edges.insert(other);
            }
        }
        for r in &v.records {
            if r.id != id
                && r.flags & ROLLED_BACK == 0
                && (if out { &r.ins } else { &r.outs }).contains(&id)
            {
                edges.insert(r.id);
            }
        }
        Ok(edges)
    }

    /// The generation a transaction's commit makes, where it has passed its commit's test.
    fn prepared(&self, file: &std::fs::File, v: &View, s: &Serial, id: u64) -> Result<Option<u64>> {
        Ok(match v.who(id) {
            Who::Me => s.prepared,
            Who::Running(slot) => lock::prepared_of(file, slot)?,
            Who::Committed(r) => Some(r.commit),
            Who::Gone => None,
        })
    }

    fn is_read_only(&self, file: &std::fs::File, v: &View, s: &Serial, id: u64) -> Result<bool> {
        Ok(match v.who(id) {
            Who::Me => s.read_only,
            Who::Running(slot) => !lock::writes_in(file, slot)?,
            Who::Committed(r) => r.flags & READ_ONLY != 0,
            Who::Gone => false,
        })
    }

    fn snapshot_of(&self, file: &std::fs::File, v: &View, s: &Serial, id: u64) -> Result<u64> {
        Ok(match v.who(id) {
            Who::Me => s.snapshot,
            Who::Running(slot) => lock::snapshot_of(file, slot)?.unwrap_or(0),
            Who::Committed(r) => r.snapshot,
            Who::Gone => 0,
        })
    }

    /// An edge found from `reader` to `writer`, one of them this transaction, tested as it is added:
    /// two in a row refuse this transaction, or doom the other.
    fn flag(
        &self,
        file: &std::fs::File,
        v: &View,
        s: &mut Serial,
        reader: u64,
        writer: u64,
    ) -> Result<()> {
        let committed = |id: u64| match v.who(id) {
            Who::Committed(r) => Some(r.commit),
            _ => None,
        };
        let mut failure = false;
        if let Who::Committed(w) = v.who(writer) {
            failure = w.conflict_out.is_some();
        }
        if !failure {
            for t2 in self.edges(file, v, s, writer, true)? {
                let Some(p) = self.prepared(file, v, s, t2)? else {
                    continue;
                };
                if committed(reader).is_none_or(|c| p <= c)
                    && committed(writer).is_none_or(|c| p <= c)
                    && (!self.is_read_only(file, v, s, reader)?
                        || p <= self.snapshot_of(file, v, s, reader)?)
                {
                    failure = true;
                    break;
                }
            }
        }
        let writer_prepared = self.prepared(file, v, s, writer)?;
        if let (false, Some(w), false) = (
            failure,
            writer_prepared,
            self.is_read_only(file, v, s, reader)?,
        ) {
            for t0 in self.edges(file, v, s, reader, false)? {
                if matches!(v.who(t0), Who::Gone) || self.doomed(file, v, s, t0)? {
                    continue;
                }
                if committed(t0).is_none_or(|c| c >= w)
                    && (!self.is_read_only(file, v, s, t0)?
                        || self.snapshot_of(file, v, s, t0)? >= w)
                {
                    failure = true;
                    break;
                }
            }
        }
        if failure {
            if writer == s.id {
                return Err(failed(
                    "Canceled on identification as a pivot, during write.",
                ));
            }
            if writer_prepared.is_some() {
                return Err(failed(&format!(
                    "Canceled on conflict out to pivot {writer}, during read."
                )));
            }
            lock::mark_doom(file, self.slot, writer, true)?;
            s.dooms.insert(writer);
        }
        if reader == s.id {
            lock::mark_edge(file, self.slot, true, writer, true)?;
            s.outs.insert(writer);
        } else {
            lock::mark_edge(file, self.slot, false, reader, true)?;
            s.ins.insert(reader);
        }
        Ok(())
    }

    /// Whether an edge from `reader` to `writer` stands already.
    fn edge_stands(
        &self,
        file: &std::fs::File,
        v: &View,
        s: &Serial,
        reader: u64,
        writer: u64,
    ) -> Result<bool> {
        Ok(self.edges(file, v, s, reader, true)?.contains(&writer))
    }

    /// A round of the transaction read along an order: each entry its walk looked at marked, or the
    /// order whole, and an edge found to each writer of what it marked. A transaction someone doomed
    /// fails here, as PostgreSQL's does at its next read.
    pub(super) fn mark_read(&self, table: usize, walk: &Walk) -> Result<()> {
        if self.comparing.get() {
            return Ok(());
        }
        if let Some(m) = &self.monitor {
            let t = &self.catalog.tables[table];
            let order = &t.orders[walk.order];
            if !walk.empty && (walk.upper || walk.marks.is_empty()) {
                m.read(&t.name, &order.name, None)?;
            }
            for first in &walk.marks {
                m.read(
                    &t.name,
                    &order.name,
                    Some(lock::block_byte_of(table, walk.order, first)),
                )?;
            }
        }
        let mut guard = self.serial.borrow_mut();
        let Some(s) = guard.as_mut().filter(|s| s.active) else {
            return Ok(());
        };
        let file = self.the_writer_file()?;
        self.settle_read_only(&file, s)?;
        if !s.active {
            return Ok(());
        }
        lock::take_serial_turn(&file)?;
        let marked = self.marked_read(&file, s, table, walk);
        lock::give_serial_turn(&file)?;
        marked
    }

    fn marked_read(
        &self,
        file: &std::fs::File,
        s: &mut Serial,
        table: usize,
        walk: &Walk,
    ) -> Result<()> {
        if walk.empty {
            return Ok(());
        }
        let v = self.view(file, s.id)?;
        if self.doomed(file, &v, s, s.id)? {
            return Err(failed(
                "Canceled on identification as a pivot, during conflict out checking.",
            ));
        }
        let key = (table, walk.order);
        let mut fresh = Vec::new();
        let order_byte = lock::order_byte(table, walk.order);
        if !s.whole.contains(&key) {
            let mut whole = !walk.empty && (walk.upper || walk.marks.is_empty());
            if !whole {
                let held = s.marks.entry(key).or_default();
                for first in &walk.marks {
                    let b = lock::block_byte_of(table, walk.order, first);
                    if held.insert(b) {
                        lock::mark_read(file, self.slot, b, true)?;
                        fresh.push(b);
                    }
                }
                whole = held.len() > MARKS_AN_ORDER;
            }
            if whole {
                lock::mark_read(file, self.slot, order_byte, true)?;
                fresh.push(order_byte);
                for b in s.marks.remove(&key).unwrap_or_default() {
                    lock::mark_read(file, self.slot, b, false)?;
                }
                s.whole.insert(key);
            }
        }
        // Each writer of what it newly marked: one running holds the block, and a commit since its
        // snapshot named it, or a block cut from it.
        let mut met: BTreeSet<u64> = BTreeSet::new();
        for &b in &fresh {
            for slot in lock::holders(file, b)? {
                if let Some((_, id)) = v.running.iter().find(|(x, _)| *x == slot) {
                    met.insert(*id);
                }
            }
        }
        for r in &v.records {
            if r.id == 0
                || r.id == s.id
                || r.flags & ROLLED_BACK != 0
                || r.commit <= s.snapshot
                || r.flags & DID_WRITE == 0
            {
                continue;
            }
            let lies_under = r
                .wrote
                .iter()
                .any(|w| v.with_parents(*w).iter().any(|b| fresh.contains(b)));
            if lies_under {
                met.insert(r.id);
            }
        }
        for w in met {
            if self.doomed(file, &v, s, w)? {
                continue;
            }
            // A READ ONLY transaction comes first where its writer read over no commit before its
            // snapshot.
            if s.read_only {
                if let Who::Committed(r) = v.who(w) {
                    if r.conflict_out.is_none_or(|c| s.snapshot < c) {
                        continue;
                    }
                }
            }
            if !self.edge_stands(file, &v, s, s.id, w)? {
                self.flag(file, &v, s, s.id, w)?;
            }
        }
        Ok(())
    }

    /// A write of the transaction changes these blocks of an order: each block's marks tested, and the
    /// marks on what it was cut from and on the order whole. A transaction someone doomed fails here.
    pub(super) fn mark_wrote(&self, table: usize, order: usize, blocks: &[u64]) -> Result<()> {
        if let Some(m) = &self.monitor {
            let t = &self.catalog.tables[table];
            for &b in blocks {
                m.wrote(&t.name, &t.orders[order].name, b)?;
            }
        }
        let mut guard = self.serial.borrow_mut();
        let Some(s) = guard.as_mut().filter(|s| s.active) else {
            return Ok(());
        };
        let file = self.the_writer_file()?;
        lock::take_serial_turn(&file)?;
        let tested = self.tested_write(&file, s, table, order, blocks);
        lock::give_serial_turn(&file)?;
        tested
    }

    fn tested_write(
        &self,
        file: &std::fs::File,
        s: &mut Serial,
        table: usize,
        order: usize,
        blocks: &[u64],
    ) -> Result<()> {
        let v = self.view(file, s.id)?;
        if self.doomed(file, &v, s, s.id)? {
            return Err(failed(
                "Canceled on identification as a pivot, during conflict in checking.",
            ));
        }
        s.did_write = true;
        let order_byte = lock::order_byte(table, order);
        if s.wrote.insert(order_byte) {
            lock::mark_held(file, self.slot, order_byte, true)?;
        }
        // What lands anywhere in the order lands in the gaps of a block a commit let go.
        let mut tests: BTreeSet<u64> = v.with_parents(order_byte).into_iter().collect();
        for &b in blocks {
            s.wrote.insert(b);
            tests.extend(v.with_parents(b));
        }
        let mut readers: BTreeSet<u64> = BTreeSet::new();
        for &b in &tests {
            for slot in lock::readers(file, b)? {
                if let Some((_, id)) = v.running.iter().find(|(x, _)| *x == slot) {
                    readers.insert(*id);
                }
            }
        }
        for r in &v.records {
            if r.id != 0
                && r.id != s.id
                && r.flags & ROLLED_BACK == 0
                && r.commit > s.snapshot
                && r.marks.iter().any(|m| tests.contains(m))
            {
                readers.insert(r.id);
            }
        }
        for x in readers {
            if !self.doomed(file, &v, s, x)? && !self.edge_stands(file, &v, s, x, s.id)? {
                self.flag(file, &v, s, x, s.id)?;
            }
        }
        Ok(())
    }

    /// A round reads the newest root from here on only to compare a statement's rows with it, or no
    /// longer does.
    pub(super) fn serial_comparing(&self, comparing: bool) {
        self.comparing.set(comparing);
    }

    /// Whether the transaction's commit writes its records though it wrote nothing: another
    /// serializable transaction that is not READ ONLY is open.
    pub(super) fn serial_records(&self) -> Result<bool> {
        let guard = self.serial.borrow();
        let Some(s) = guard.as_ref().filter(|s| s.active) else {
            return Ok(false);
        };
        let file = self.the_writer_file()?;
        for (slot, id) in lock::serials(&file)? {
            if id != s.id && lock::writes_in(&file, slot)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The transaction's commit tested, as its commit's generation is known: refused where someone
    /// doomed it; a pivot whose other edge comes from here, or from one neither READ ONLY nor ended,
    /// doomed. `generation`, where it commits by a root, is the generation it makes.
    pub(super) fn serial_commit_tested(&self, file: &std::fs::File, generation: u64) -> Result<()> {
        let mut guard = self.serial.borrow_mut();
        let Some(s) = guard.as_mut().filter(|s| s.active) else {
            return Ok(());
        };
        lock::take_serial_turn(file)?;
        let tested = self.commit_test(file, s, generation);
        lock::give_serial_turn(file)?;
        tested
    }

    fn commit_test(&self, file: &std::fs::File, s: &mut Serial, generation: u64) -> Result<()> {
        let v = self.view(file, s.id)?;
        if self.doomed(file, &v, s, s.id)? {
            return Err(failed(
                "Canceled on identification as a pivot, during commit attempt.",
            ));
        }
        for near in self.edges(file, &v, s, s.id, false)? {
            if !matches!(v.who(near), Who::Running(_)) || self.doomed(file, &v, s, near)? {
                continue;
            }
            for far in self.edges(file, &v, s, near, false)? {
                let counts = far == s.id
                    || (matches!(v.who(far), Who::Running(_))
                        && !self.is_read_only(file, &v, s, far)?
                        && !self.doomed(file, &v, s, far)?);
                if counts {
                    lock::mark_doom(file, self.slot, near, true)?;
                    s.dooms.insert(near);
                    break;
                }
            }
        }
        // The first commit it read over, those that committed before it.
        if s.did_write && !s.read_only {
            for t in self.edges(file, &v, s, s.id, true)? {
                if let Who::Committed(r) = v.who(t) {
                    s.conflict_out = Some(s.conflict_out.map_or(r.commit, |c| c.min(r.commit)));
                }
            }
        }
        s.prepared = Some(generation);
        lock::mark_prepared(file, self.slot, generation, true)
    }

    /// What goes into the records at a commit at generation `generation` of the root `root`: the
    /// transaction's own record where one commits, the blocks it cut where a serializable transaction
    /// is open, and the records no round older than their commit still runs under let go.
    pub(super) fn records_at_commit(
        &mut self,
        w: &mut Writer,
        root: &mut crate::file::Root,
        generation: u64,
        gone: &mut Vec<Extent>,
    ) -> Result<()> {
        if root.serial.bytes == 0
            && self.serial.borrow().is_none()
            && self.ending.is_none()
            && self.cuts_named().is_empty()
        {
            return Ok(());
        }
        let mut records = self.records_at(root.serial)?;
        let before = records.len();
        let reader = self.reader.file().try_clone()?;
        let mut kept = Vec::with_capacity(records.len());
        for r in records.drain(..) {
            if lock::older_held(&reader, r.commit)? {
                kept.push(r);
            }
        }
        let mut records = kept;
        let mut changed = records.len() != before;
        let cuts = self.cuts_named();
        let mine = self.serial.borrow().as_ref().and_then(|s| {
            s.prepared.filter(|p| *p == generation).map(|_| Record {
                id: s.id,
                snapshot: s.snapshot,
                commit: generation,
                flags: if s.read_only || !s.did_write {
                    READ_ONLY
                } else {
                    0
                } | if s.did_write { DID_WRITE } else { 0 },
                conflict_out: s.conflict_out,
                marks: s
                    .marks
                    .values()
                    .flatten()
                    .copied()
                    .chain(s.whole.iter().map(|(t, o)| lock::order_byte(*t, *o)))
                    .collect(),
                wrote: s.wrote.iter().copied().collect(),
                ins: s.ins.iter().copied().collect(),
                outs: s.outs.iter().copied().collect(),
                dooms: s.dooms.iter().copied().collect(),
                cuts: Vec::new(),
            })
        });
        let open = {
            let file = w.file().try_clone()?;
            !lock::serials(&file)?.is_empty()
        };
        if let Some(mut r) = mine {
            r.cuts = cuts;
            records.push(r);
            changed = true;
        } else if let Some(mut r) = self.ending.take() {
            r.commit = generation;
            r.cuts = cuts;
            records.push(r);
            changed = true;
        } else if open && !cuts.is_empty() {
            records.push(Record {
                commit: generation,
                cuts,
                ..Default::default()
            });
            changed = true;
        }
        if changed {
            if root.serial.bytes > 0 {
                gone.push(root.serial);
            }
            root.serial = if records.is_empty() {
                Extent::default()
            } else {
                w.place(&encode(&records))?
            };
        }
        Ok(())
    }

    /// The blocks this commit cut, each block a block's rows lie in now and the block they lay in,
    /// by their bytes; told to the monitor where there is one.
    fn cuts_named(&self) -> Vec<(u64, u64)> {
        self.rebased
            .iter()
            .flat_map(|((ti, oi), blocks)| {
                blocks.iter().flat_map(move |b| {
                    let from = lock::block_byte_of(*ti, *oi, b.first_key());
                    let pairs: Vec<(u64, u64)> = match b.staged_entries().is_empty() {
                        // A block no longer named: what lands in its gap lands in any of its order.
                        true => vec![(lock::order_byte(*ti, *oi), from)],
                        false => b
                            .staged_entries()
                            .iter()
                            .map(|e| lock::block_byte_of(*ti, *oi, &e.first))
                            .filter(|to| *to != from)
                            .map(|to| (to, from))
                            .collect(),
                    };
                    pairs
                })
            })
            .collect()
    }

    /// Tells the monitor, where there is one, what this commit cut.
    pub(super) fn tell_cuts(&self) -> Result<()> {
        let Some(m) = &self.monitor else {
            return Ok(());
        };
        for ((ti, oi), blocks) in &self.rebased {
            let t = &self.catalog.tables[*ti];
            let o = &t.orders[*oi];
            for b in blocks {
                let from = lock::block_byte_of(*ti, *oi, b.first_key());
                if b.staged_entries().is_empty() {
                    m.let_go(&t.name, &o.name, from)?;
                }
                for e in b.staged_entries() {
                    let to = lock::block_byte_of(*ti, *oi, &e.first);
                    if to != from {
                        m.cut(&t.name, &o.name, from, to)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// The transaction's end, its marks let go at the turn: once its root is written, committed; or
    /// rolled back, where those it doomed go into the records by a commit of its own, as PostgreSQL's
    /// doom outlives its doomer.
    pub(super) fn serial_end(&mut self) -> Result<()> {
        let Some(mut s) = self.serial.borrow_mut().take() else {
            return Ok(());
        };
        if self.writer.is_none() {
            return Ok(());
        }
        let file = self.the_writer_file()?;
        if s.prepared.is_none() && !s.dooms.is_empty() {
            self.ending = Some(Record {
                id: s.id,
                flags: ROLLED_BACK,
                dooms: s.dooms.iter().copied().collect(),
                ..Default::default()
            });
            let mut w = self.writer.take().expect("a snail open to write");
            let turn = lock::take_root_turn(w.file());
            let written = turn.and_then(|_| self.commit_with(&mut w));
            let given = lock::give_root_turn(w.file());
            self.writer = Some(w);
            self.ending = None;
            written?;
            given?;
        }
        if s.active {
            lock::take_serial_turn(&file)?;
            let released = self.release_marks(&file, &mut s);
            lock::give_serial_turn(&file)?;
            released?;
        }
        Ok(())
    }

    /// At the root's turn, a commit's test: it stands at the generation the commit makes.
    pub(super) fn serial_tested(&self, file: &std::fs::File) -> Result<()> {
        if self.serial.borrow().as_ref().is_none_or(|s| !s.active) {
            return Ok(());
        }
        let generation = self.reader.peek_root()?.generation + 1;
        self.serial_commit_tested(file, generation)
    }

    /// A commit's test where the transaction writes no root: at the root's turn too, so that it
    /// stands among the commits.
    pub(super) fn serial_tested_alone(&self) -> Result<()> {
        if self.serial.borrow().as_ref().is_none_or(|s| !s.active) {
            return Ok(());
        }
        let file = self.the_writer_file()?;
        lock::take_root_turn(&file)?;
        let tested = self.serial_tested(&file);
        lock::give_root_turn(&file)?;
        tested
    }

    /// A question alone, at a session's SERIALIZABLE, is a transaction that ends with it.
    pub(super) fn serial_alone_ends(&mut self) -> Result<()> {
        if self.begun.is_some() || self.serial.borrow().is_none() {
            return Ok(());
        }
        if self.serial_records()? {
            return self.commit();
        }
        let tested = self.serial_tested_alone();
        self.serial_end()?;
        tested
    }
}
