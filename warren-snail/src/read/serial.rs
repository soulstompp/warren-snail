//! A serializable transaction's round. Each block a round looks at is marked, or, read by one whole
//! key of the table's order, that key's row and the block's gaps; each write tests the marks of the
//! blocks it changes, of the rows it changes, and of the gaps it lands keys in; each read tests the
//! writers of what it marked; and each edge found is tested for two in a row, as PostgreSQL's monitor
//! tests them; the failure in its words. A running
//! transaction's marks are locks on bytes past the file's pages, and a committed one's are written
//! into the records the root names, kept while a round older than its commit runs.
//!
//! A transaction across several snails is one transaction in each of their files: what is its own
//! whatever it reads, who it is and the edges it found, lies in every one of them, and what it marked
//! in the file of what it read. Its test walks every file those running beside it run on, so that two
//! in a row are found whichever file each lies in, and the commits it compares take one order from
//! the clock their records share.

use super::walk::Walk;
use super::Snail;
use crate::file::{Extent, In, Out, Place, Reader, Writer};
use crate::lock;
use crate::{Error, Result};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::rc::Rc;

/// What PostgreSQL's monitor says as it refuses a transaction.
const FAILED: &str = "could not serialize access due to read/write dependencies among transactions";

/// The entries a transaction marks on one order before it marks the order whole, as PostgreSQL's
/// default `max_pred_locks_per_relation` promotes a relation's.
const MARKS_AN_ORDER: usize = 32;

/// What a mark stands for, by its block's byte or its row's: a block's rows and the gaps between its
/// keys at once; those gaps alone; or one row. PostgreSQL's monitor marks an index's leaf page for its
/// gaps and a heap tuple for its row, and a block is both at once.
#[derive(Clone, Copy, Debug)]
pub(super) enum Mark {
    Block(u64),
    Gap(u64),
    Row(u64),
}

/// The marks a round takes on the block of byte `block`: one key of the table's unique order fixed
/// whole, the gap and that key's row; any other read, the block.
fn read_marks(
    table: u64,
    order: usize,
    block: u64,
    point: Option<&[crate::places::KeyValue]>,
) -> Vec<(u64, Mark)> {
    match point {
        Some(key) => {
            let row = lock::row_byte(table, order, key);
            vec![
                (lock::gap_byte(block), Mark::Gap(block)),
                (row, Mark::Row(row)),
            ]
        }
        None => vec![(block, Mark::Block(block))],
    }
}

/// What a write of one order tests: every block it changes, by its rows and its gaps; each row of the
/// table's order it changes; and the gaps of the blocks it lands a key in, an INSERT's and an UPDATE's
/// that changes a key or a column a strand holds.
#[derive(Clone, Debug, Default)]
pub(super) struct Written {
    pub(super) blocks: Vec<u64>,
    pub(super) rows: Vec<u64>,
    pub(super) landed: Vec<u64>,
}

impl Written {
    /// The blocks alone, a write whose rows the order cannot name.
    pub(super) fn blocks(blocks: Vec<u64>) -> Written {
        Written {
            blocks,
            ..Written::default()
        }
    }

    /// Every byte the write marks as its own: each block's, each row's, and each gap's it lands in.
    fn bytes(&self) -> impl Iterator<Item = u64> + '_ {
        self.blocks
            .iter()
            .chain(&self.rows)
            .copied()
            .chain(self.landed.iter().map(|&b| lock::gap_byte(b)))
    }
}

fn failed(reason: &str) -> Error {
    Error::new(format!(
        "{FAILED}\nDETAIL:  Reason code: {reason}\nHINT:  The transaction might succeed if retried."
    ))
}

const READ_ONLY: u8 = 1;
const DID_WRITE: u8 = 2;
const ROLLED_BACK: u8 = 4;

/// A committed serializable transaction, as the records of one file hold it; or, its mark 0, only the
/// blocks a commit cut; or, rolled back, only those it doomed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Record {
    id: u64,
    /// Where its snapshot stood on the clock the records share.
    snapshot: u64,
    /// The generation of this file its commit made, and where it stood on that clock.
    commit: u64,
    order: u64,
    flags: u8,
    /// The first commit among the transactions it read what they wrote, those that committed before it,
    /// on that clock.
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
        o.u64(r.order);
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
            let (id, snapshot, commit, order, flags) =
                (i.u64()?, i.u64()?, i.u64()?, i.u64()?, i.u8()?);
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
                order,
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

/// A file a serializable transaction runs on: the file as its writer holds it, its path and its
/// device and inode; and a reader of its records, where another of the transaction's snails holds it.
pub(super) struct TxnFile {
    lock: std::fs::File,
    path: PathBuf,
    at: crate::file::Identity,
    reader: Option<Reader>,
}

/// A serializable transaction under way, whatever file it reads: from its first statement that reads.
pub(super) struct Txn {
    /// Whether it still marks what it reads: a READ ONLY transaction beside no writer that can hurt
    /// it, or found safe once those it ran beside ended, does not.
    active: bool,
    id: u64,
    read_only: bool,
    /// Where its snapshot stands on the clock the records share.
    order: u64,
    /// Those that were not READ ONLY as it took its snapshot, while it is READ ONLY, until each has
    /// ended.
    possible: Vec<u64>,
    /// Whether one of them committed with an edge out to a commit before its snapshot.
    not_safe: bool,
    did_write: bool,
    /// The edges it found, out to others and in from others, and those it doomed.
    outs: BTreeSet<u64>,
    ins: BTreeSet<u64>,
    dooms: BTreeSet<u64>,
    /// Where it passed its commit's test, where its commit stands on the clock.
    prepared: Option<u64>,
    /// The first commit it read over, once it has passed.
    conflict_out: Option<u64>,
    /// The files it runs on, each snail's place among them; and files it reached through those it met,
    /// by their paths, each with the file its turn is taken on.
    files: Vec<TxnFile>,
    reached: Vec<TxnFile>,
}

/// One snail's part of a serializable transaction.
pub(super) struct Serial {
    t: Rc<RefCell<Txn>>,
    /// Its place among the transaction's files; and the generation of this file its snapshot reads.
    place: usize,
    snapshot: u64,
    /// What it marked, by its table's lock key (`Snail::lock_key`) and order, each entry by its lock's
    /// byte; and the orders marked whole.
    marks: BTreeMap<(u64, usize), BTreeSet<u64>>,
    whole: BTreeSet<(u64, usize)>,
    /// The blocks and orders it wrote, by their bytes.
    wrote: BTreeSet<u64>,
    /// Where it passed its commit's test, the generation of this file its commit makes.
    commit_at: Option<u64>,
}

/// What a test reads of a transaction it is told of by its mark.
#[derive(Clone, Copy)]
enum Who<'a> {
    Me,
    Running(usize, u64),
    Committed(&'a Record),
    Gone,
}

/// One file as a test reads it: those running on it, and its records.
struct FileView {
    file: std::fs::File,
    running: Vec<(u64, u64)>,
    records: Vec<Record>,
    /// Each block's blocks it was cut from, by its byte.
    parents: BTreeMap<u64, Vec<u64>>,
}

/// The monitor as a transaction reads it, at its turn: every file it reaches.
struct View {
    me: u64,
    files: Vec<FileView>,
}

impl View {
    fn who(&self, id: u64) -> Who<'_> {
        if id == self.me {
            return Who::Me;
        }
        for f in &self.files {
            if let Some(r) = f
                .records
                .iter()
                .find(|r| r.id == id && r.flags & ROLLED_BACK == 0)
            {
                return Who::Committed(r);
            }
        }
        for (i, f) in self.files.iter().enumerate() {
            if let Some((slot, _)) = f.running.iter().find(|(_, x)| *x == id) {
                return Who::Running(i, *slot);
            }
        }
        Who::Gone
    }
}

impl FileView {
    /// The bytes a mark meets: a block's, those of every block it was cut from or that was cut from
    /// it; a gap's, those blocks' gaps; a row's, its own, which no cut moves.
    fn met_by(&self, m: Mark) -> Vec<u64> {
        match m {
            Mark::Block(b) => self.related(b),
            Mark::Gap(b) => self.related(b).into_iter().map(lock::gap_byte).collect(),
            Mark::Row(r) => vec![r],
        }
    }

    /// A block's byte, and every byte of a block it was cut from or that was cut from it: what a mark on
    /// one means on the others, whichever generation each was named under.
    fn related(&self, byte: u64) -> Vec<u64> {
        let mut out = self.with_parents(byte);
        let mut at = 0;
        while at < out.len() {
            for (child, parents) in &self.parents {
                if parents.contains(&out[at]) && !out.contains(child) {
                    out.push(*child);
                }
            }
            at += 1;
        }
        out
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

/// The clock the records share, where a commit that has seen `clocks` stands: past each of them, and
/// no earlier than the machine's clock, so that commits on files that share no writer take one order.
fn next_on_clock(clocks: impl Iterator<Item = u64>) -> u64 {
    let now = super::change::wall_micros().max(0) as u64;
    clocks.fold(now, |at, c| at.max(c + 1))
}

/// Takes a serializable transaction's snapshot on `snails`, its writers' root turns held together, so
/// that no commit on any of them is under way that did not see it: its marks taken and every newest
/// root read. A READ ONLY one beside no transaction that writes marks nothing, and a DEFERRABLE one
/// waits for a snapshot no such transaction can hurt.
pub(super) fn snapshot_taken(snails: &mut [&mut Snail], begun: bool) -> Result<()> {
    if snails[0].serial.borrow().is_some() {
        return Ok(());
    }
    for s in snails.iter() {
        if s.root.replica.is_some() {
            return Err(Error::new(
                "cannot use serializable mode in a hot standby\nDETAIL:  \
                 \"default_transaction_isolation\" is set to \"serializable\".\nHINT:  You can use \
                 \"SET default_transaction_isolation = 'repeatable read'\" to change the default.",
            ));
        }
    }
    let read_only = snails.iter().any(|s| s.levels.read_only(begun));
    let deferrable = read_only && snails.iter().any(|s| s.levels.deferrable(begun));
    let several = snails.len() > 1;
    let mut files = Vec::with_capacity(snails.len());
    for s in snails.iter() {
        let path = s.reader.path().to_path_buf();
        let lock = s.the_writer_file()?;
        files.push(TxnFile {
            at: crate::file::identity(&lock)?,
            lock,
            reader: if several {
                Some(Reader::open(&path)?)
            } else {
                None
            },
            path,
        });
    }
    let mut turns: Vec<usize> = (0..files.len()).collect();
    turns.sort_by_key(|&i| files[i].at);
    let t = Rc::new(RefCell::new(Txn {
        active: true,
        id: 0,
        read_only,
        order: 0,
        possible: Vec::new(),
        not_safe: false,
        did_write: false,
        outs: BTreeSet::new(),
        ins: BTreeSet::new(),
        dooms: BTreeSet::new(),
        prepared: None,
        conflict_out: None,
        files,
        reached: Vec::new(),
    }));
    loop {
        let id = lock::serial_id();
        let mut parts = Vec::with_capacity(snails.len());
        {
            let txn = t.borrow();
            let mut taken = Vec::new();
            let mut took = Ok(());
            for &i in &turns {
                match lock::take_root_turn(&txn.files[i].lock) {
                    Ok(()) => taken.push(i),
                    Err(e) => {
                        took = Err(e);
                        break;
                    }
                }
            }
            let snapped = took.and_then(|_| {
                let mut order = 0;
                let mut possible = Vec::new();
                for (i, s) in snails.iter_mut().enumerate() {
                    let file = &txn.files[i].lock;
                    lock::mark_serial(file, s.slot, id, true)?;
                    if !read_only {
                        lock::mark_writes(file, s.slot, true)?;
                    }
                    if begun {
                        s.renew_in_transaction()?;
                    } else {
                        s.renew()?;
                    }
                    order = order.max(s.root.clock);
                    parts.push(s.root.generation);
                    if read_only {
                        for (slot, other) in lock::serials(file)? {
                            if other != id
                                && lock::writes_in(file, slot)?
                                && !possible.contains(&other)
                            {
                                possible.push(other);
                            }
                        }
                    }
                }
                for (i, s) in snails.iter().enumerate() {
                    lock::mark_order(&txn.files[i].lock, s.slot, false, order, true)?;
                }
                Ok::<_, Error>((order, possible))
            });
            for &i in taken.iter().rev() {
                lock::give_root_turn(&txn.files[i].lock)?;
            }
            let (order, possible) = match snapped {
                Ok(took) => took,
                Err(e) => {
                    for (i, s) in snails.iter().enumerate() {
                        lock::mark_writes(&txn.files[i].lock, s.slot, false)?;
                        lock::mark_serial(&txn.files[i].lock, s.slot, id, false)?;
                    }
                    return Err(e);
                }
            };
            drop(txn);
            let mut txn = t.borrow_mut();
            txn.id = id;
            txn.order = order;
            txn.active = !(read_only && possible.is_empty());
            txn.not_safe = false;
            txn.possible = possible;
        }
        let snapshots: Vec<u64> = parts;
        for (i, s) in snails.iter().enumerate() {
            *s.serial.borrow_mut() = Some(Serial {
                t: t.clone(),
                place: i,
                snapshot: snapshots[i],
                marks: BTreeMap::new(),
                whole: BTreeSet::new(),
                wrote: BTreeSet::new(),
                commit_at: None,
            });
        }
        if !t.borrow().active {
            for s in snails.iter() {
                s.let_go_own(&t)?;
            }
            return Ok(());
        }
        // It waits as long as it takes, holding no lock, until each beside it has ended or one has
        // shown the snapshot unsafe.
        if deferrable {
            let mut nap = std::time::Duration::from_millis(1);
            loop {
                snails[0].settle_read_only(&t)?;
                let (not_safe, active) = {
                    let txn = t.borrow();
                    (txn.not_safe, txn.active)
                };
                if not_safe || !active {
                    break;
                }
                std::thread::sleep(nap);
                nap = (nap * 2).min(std::time::Duration::from_millis(50));
            }
            if t.borrow().not_safe {
                for s in snails.iter() {
                    s.let_go_own(&t)?;
                    *s.serial.borrow_mut() = None;
                }
                t.borrow_mut().active = true;
                continue;
            }
            if !t.borrow().active {
                for s in snails.iter() {
                    s.let_go_own(&t)?;
                }
            }
        }
        return Ok(());
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
        self.monitor.is_some()
            || self
                .serial
                .borrow()
                .as_ref()
                .is_some_and(|s| s.t.borrow().active)
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

    /// Refuses SERIALIZABLE named on a snail that cannot run it: marked a replica, as PostgreSQL's
    /// hot standby refuses it.
    pub(super) fn serial_allowed(&self, named_here: bool) -> Result<()> {
        if named_here && self.root.replica.is_some() {
            return Err(Error::new(
                "cannot use serializable mode in a hot standby\nHINT:  You can use REPEATABLE READ \
                 instead.",
            ));
        }
        Ok(())
    }

    /// The first statement of a serializable transaction that reads, on this snail alone.
    pub(super) fn serial_snapshot(&mut self, begun: bool) -> Result<()> {
        snapshot_taken(&mut [self], begun)
    }

    /// A READ ONLY transaction's account of those it ran beside: each that ended goes from it, and it
    /// is no longer safe where one committed having read over a commit before its snapshot. Once none
    /// is left and it is safe, it marks nothing more.
    fn settle_read_only(&self, t: &Rc<RefCell<Txn>>) -> Result<()> {
        {
            let txn = t.borrow();
            if !txn.read_only || txn.not_safe || !txn.active {
                return Ok(());
            }
        }
        let v = self.view_of_own(&t.borrow())?;
        let mut txn = t.borrow_mut();
        let order = txn.order;
        let mut not_safe = false;
        txn.possible.retain(|p| {
            if v.files
                .iter()
                .any(|f| f.running.iter().any(|(_, x)| x == p))
            {
                return true;
            }
            for f in &v.files {
                if let Some(r) = f.records.iter().find(|r| r.id == *p) {
                    if r.flags & DID_WRITE != 0 && r.conflict_out.is_some_and(|c| c <= order) {
                        not_safe = true;
                    }
                }
            }
            false
        });
        txn.not_safe = not_safe;
        if txn.possible.is_empty() && !txn.not_safe {
            txn.active = false;
            drop(txn);
            let txn = t.borrow();
            for f in &txn.files {
                lock::take_serial_turn(&f.lock)?;
                let released = lock::mark_order(&f.lock, self.slot, false, txn.order, false)
                    .and_then(|_| lock::mark_writes(&f.lock, self.slot, false))
                    .and_then(|_| lock::mark_serial(&f.lock, self.slot, txn.id, false));
                lock::give_serial_turn(&f.lock)?;
                released?;
            }
        }
        Ok(())
    }

    /// Lets every mark of the transaction in this snail's file go, its mark as a transaction among them,
    /// so that it is seen as ended here, and it marks nothing more here.
    fn let_go_own(&self, t: &Rc<RefCell<Txn>>) -> Result<()> {
        let mut guard = self.serial.borrow_mut();
        let Some(s) = guard.as_mut() else {
            return Ok(());
        };
        let txn = t.borrow();
        let file = &txn.files[s.place].lock;
        lock::take_serial_turn(file)?;
        let released = (|| {
            for bytes in s.marks.values() {
                for &b in bytes {
                    lock::mark_read(file, self.slot, b, false)?;
                }
            }
            for &(key, oi) in &s.whole {
                lock::mark_read(file, self.slot, lock::order_byte(key, oi), false)?;
            }
            for &b in &s.wrote {
                lock::mark_wrote(file, self.slot, b, false)?;
            }
            for &o in &txn.outs {
                lock::mark_edge(file, self.slot, true, o, false)?;
            }
            for &o in &txn.ins {
                lock::mark_edge(file, self.slot, false, o, false)?;
            }
            for &o in &txn.dooms {
                lock::mark_doom(file, self.slot, o, false)?;
            }
            if let Some(g) = txn.prepared {
                lock::mark_order(file, self.slot, true, g, false)?;
            }
            lock::mark_order(file, self.slot, false, txn.order, false)?;
            lock::mark_writes(file, self.slot, false)?;
            lock::mark_serial(file, self.slot, txn.id, false)
        })();
        lock::give_serial_turn(file)?;
        released?;
        s.marks.clear();
        s.whole.clear();
        s.wrote.clear();
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

    /// One file as a test reads it, its records by `read`.
    fn file_view(
        &self,
        file: &std::fs::File,
        read: impl FnOnce() -> Result<Vec<Record>>,
    ) -> Result<FileView> {
        let running = lock::serials(file)?;
        let records = read()?;
        let mut parents: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
        for r in &records {
            for (child, parent) in &r.cuts {
                parents.entry(*child).or_default().push(*parent);
            }
        }
        Ok(FileView {
            file: file.try_clone()?,
            running,
            records,
            parents,
        })
    }

    /// The files of the transaction's own snails as a test reads them.
    fn view_of_own(&self, txn: &Txn) -> Result<View> {
        let mut files = Vec::with_capacity(txn.files.len());
        for f in &txn.files {
            files.push(self.file_view(&f.lock, || match &f.reader {
                Some(r) => {
                    let root = r.peek_root()?;
                    if root.serial.bytes == 0 {
                        return Ok(Vec::new());
                    }
                    decode(&r.read_extent(root.serial)?)
                }
                None => self.records_now(),
            })?);
        }
        Ok(View { me: txn.id, files })
    }

    /// The monitor as the transaction reads it: its own files, and each file another running beside it
    /// runs on. Returns what it found, and the paths of the files it has yet to reach.
    fn view(&self, txn: &Txn) -> Result<(View, Vec<Vec<u8>>)> {
        let mut v = self.view_of_own(txn)?;
        for f in &txn.reached {
            let path = f.path.clone();
            v.files.push(self.file_view(&f.lock, || {
                let r = Reader::open(&path)?;
                let root = r.root()?;
                if root.serial.bytes == 0 {
                    return Ok(Vec::new());
                }
                decode(&r.read_extent(root.serial)?)
            })?);
        }
        let mut known: Vec<crate::file::Identity> =
            txn.files.iter().chain(&txn.reached).map(|f| f.at).collect();
        let mut more: Vec<Vec<u8>> = Vec::new();
        let mut note = |path: &[u8]| {
            use std::os::unix::ffi::OsStrExt;
            let p = std::path::Path::new(std::ffi::OsStr::from_bytes(path));
            if let Ok(at) = crate::file::identity_at(p) {
                if !known.contains(&at) {
                    known.push(at);
                    more.push(path.to_vec());
                }
            }
        };
        for f in &v.files {
            for (slot, _) in &f.running {
                for path in lock::beside(&f.file, *slot)? {
                    note(&path);
                }
            }
        }
        Ok((v, more))
    }

    /// Runs `f` at the turn at the marks of every file the transaction reaches, taken in their order on
    /// the machine, each file a test may read held still until it ends.
    fn at_turn<T>(&self, t: &Rc<RefCell<Txn>>, f: impl FnOnce(&View) -> Result<T>) -> Result<T> {
        loop {
            let mut turns: Vec<std::fs::File> = Vec::new();
            {
                let txn = t.borrow();
                let mut all: Vec<&TxnFile> = txn.files.iter().chain(&txn.reached).collect();
                all.sort_by_key(|x| x.at);
                for x in all {
                    lock::take_serial_turn(&x.lock)?;
                    turns.push(x.lock.try_clone()?);
                }
            }
            let give = |turns: &[std::fs::File]| -> Result<()> {
                for x in turns.iter().rev() {
                    lock::give_serial_turn(x)?;
                }
                Ok(())
            };
            let seen = self.view(&t.borrow());
            let (v, more) = match seen {
                Ok(x) => x,
                Err(e) => {
                    give(&turns)?;
                    return Err(e);
                }
            };
            if more.is_empty() {
                let done = f(&v);
                give(&turns)?;
                return done;
            }
            give(&turns)?;
            let mut txn = t.borrow_mut();
            for path in more {
                use std::os::unix::ffi::OsStringExt;
                let path = PathBuf::from(std::ffi::OsString::from_vec(path));
                let lock = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&path)
                    .map_err(|e| {
                        Error::new(format!(
                            "{}, on which a serializable transaction beside this one runs, is not \
                             opened: {e}",
                            path.display()
                        ))
                    })?;
                let at = crate::file::identity(&lock)?;
                txn.reached.push(TxnFile {
                    lock,
                    path,
                    at,
                    reader: None,
                });
            }
        }
    }

    fn doomed(&self, v: &View, txn: &Txn, id: u64) -> Result<bool> {
        if txn.dooms.contains(&id) {
            return Ok(true);
        }
        for f in &v.files {
            for (slot, _) in &f.running {
                if lock::doomed_by(&f.file, *slot, id)? {
                    return Ok(true);
                }
            }
            if f.records.iter().any(|r| r.dooms.contains(&id)) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Those a transaction has an edge with: out to them, or in from them.
    fn edges(&self, v: &View, txn: &Txn, id: u64, out: bool) -> Result<BTreeSet<u64>> {
        let mut edges = BTreeSet::new();
        match v.who(id) {
            Who::Me => edges.extend(if out { &txn.outs } else { &txn.ins }),
            Who::Running(f, slot) => {
                edges.extend(lock::edges_in_slot(&v.files[f].file, slot, out)?)
            }
            Who::Committed(r) => edges.extend(if out { &r.outs } else { &r.ins }),
            Who::Gone => {}
        }
        // Found from the other end: by this transaction, by one running, or by one committed since.
        if id != txn.id && (if out { &txn.ins } else { &txn.outs }).contains(&id) {
            edges.insert(txn.id);
        }
        for f in &v.files {
            for &(slot, other) in &f.running {
                if other != id && lock::edge_with(&f.file, slot, !out, id)? {
                    edges.insert(other);
                }
            }
            for r in &f.records {
                if r.id != id
                    && r.flags & ROLLED_BACK == 0
                    && (if out { &r.ins } else { &r.outs }).contains(&id)
                {
                    edges.insert(r.id);
                }
            }
        }
        Ok(edges)
    }

    /// Where a transaction's commit stands on the clock, where it has passed its commit's test.
    fn prepared(&self, v: &View, txn: &Txn, id: u64) -> Result<Option<u64>> {
        Ok(match v.who(id) {
            Who::Me => txn.prepared,
            Who::Running(f, slot) => lock::order_of(&v.files[f].file, slot, true)?,
            Who::Committed(r) => Some(r.order),
            Who::Gone => None,
        })
    }

    fn is_read_only(&self, v: &View, txn: &Txn, id: u64) -> Result<bool> {
        Ok(match v.who(id) {
            Who::Me => txn.read_only,
            Who::Running(f, slot) => !lock::writes_in(&v.files[f].file, slot)?,
            Who::Committed(r) => r.flags & READ_ONLY != 0,
            Who::Gone => false,
        })
    }

    fn snapshot_of(&self, v: &View, txn: &Txn, id: u64) -> Result<u64> {
        Ok(match v.who(id) {
            Who::Me => txn.order,
            Who::Running(f, slot) => lock::order_of(&v.files[f].file, slot, false)?.unwrap_or(0),
            Who::Committed(r) => r.snapshot,
            Who::Gone => 0,
        })
    }

    /// An edge found from `reader` to `writer`, one of them this transaction, tested as it is added:
    /// two in a row refuse this transaction, or doom the other.
    fn flag(&self, v: &View, t: &Rc<RefCell<Txn>>, reader: u64, writer: u64) -> Result<()> {
        let mut txn = t.borrow_mut();
        let committed = |id: u64| match v.who(id) {
            Who::Committed(r) => Some(r.order),
            _ => None,
        };
        let mut failure = false;
        if let Who::Committed(w) = v.who(writer) {
            failure = w.conflict_out.is_some();
        }
        if !failure {
            for t2 in self.edges(v, &txn, writer, true)? {
                let Some(p) = self.prepared(v, &txn, t2)? else {
                    continue;
                };
                if committed(reader).is_none_or(|c| p <= c)
                    && committed(writer).is_none_or(|c| p <= c)
                    && (!self.is_read_only(v, &txn, reader)?
                        || p <= self.snapshot_of(v, &txn, reader)?)
                {
                    failure = true;
                    break;
                }
            }
        }
        let writer_prepared = self.prepared(v, &txn, writer)?;
        if let (false, Some(w), false) = (
            failure,
            writer_prepared,
            self.is_read_only(v, &txn, reader)?,
        ) {
            for t0 in self.edges(v, &txn, reader, false)? {
                if matches!(v.who(t0), Who::Gone) || self.doomed(v, &txn, t0)? {
                    continue;
                }
                if committed(t0).is_none_or(|c| c >= w)
                    && (!self.is_read_only(v, &txn, t0)? || self.snapshot_of(v, &txn, t0)? >= w)
                {
                    failure = true;
                    break;
                }
            }
        }
        if failure {
            if writer == txn.id {
                return Err(failed(
                    "Canceled on identification as a pivot, during write.",
                ));
            }
            if writer_prepared.is_some() {
                return Err(failed(&format!(
                    "Canceled on conflict out to pivot {writer}, during read."
                )));
            }
            for f in &txn.files {
                lock::mark_doom(&f.lock, self.slot, writer, true)?;
            }
            txn.dooms.insert(writer);
        }
        if reader == txn.id {
            for f in &txn.files {
                lock::mark_edge(&f.lock, self.slot, true, writer, true)?;
            }
            txn.outs.insert(writer);
        } else {
            for f in &txn.files {
                lock::mark_edge(&f.lock, self.slot, false, reader, true)?;
            }
            txn.ins.insert(reader);
        }
        Ok(())
    }

    /// Whether an edge from `reader` to `writer` stands already.
    fn edge_stands(&self, v: &View, txn: &Txn, reader: u64, writer: u64) -> Result<bool> {
        Ok(self.edges(v, txn, reader, true)?.contains(&writer))
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
            let key = self.lock_key(table);
            if !walk.empty && (walk.upper || walk.marks.is_empty()) {
                m.read(&t.name, &order.name, None)?;
            }
            for first in &walk.marks {
                let b = self.block_read_byte(table, walk.order, first);
                for (byte, _) in read_marks(key, walk.order, b, walk.point(order)) {
                    m.read(&t.name, &order.name, Some(byte))?;
                }
            }
        }
        let mut guard = self.serial.borrow_mut();
        let Some(s) = guard.as_mut().filter(|s| s.t.borrow().active) else {
            return Ok(());
        };
        let t = s.t.clone();
        self.settle_read_only(&t)?;
        if !t.borrow().active {
            return Ok(());
        }
        self.at_turn(&t, |v| self.marked_read(v, &t, s, table, walk))
    }

    fn marked_read(
        &self,
        v: &View,
        t: &Rc<RefCell<Txn>>,
        s: &mut Serial,
        table: usize,
        walk: &Walk,
    ) -> Result<()> {
        if walk.empty {
            return Ok(());
        }
        let here = &v.files[s.place];
        let file = &here.file;
        let (me, read_only, order) = {
            let txn = t.borrow();
            if self.doomed(v, &txn, txn.id)? {
                return Err(failed(
                    "Canceled on identification as a pivot, during conflict out checking.",
                ));
            }
            (txn.id, txn.read_only, txn.order)
        };
        let key = (self.lock_key(table), walk.order);
        let point = walk.point(&self.catalog.tables[table].orders[walk.order]);
        let mut fresh: Vec<(u64, Mark)> = Vec::new();
        let order_byte = lock::order_byte(key.0, walk.order);
        if !s.whole.contains(&key) {
            let mut whole = !walk.empty && (walk.upper || walk.marks.is_empty());
            if !whole {
                let held = s.marks.entry(key).or_default();
                for first in &walk.marks {
                    let b = self.block_read_byte(table, walk.order, first);
                    for (byte, mark) in read_marks(key.0, walk.order, b, point) {
                        if held.insert(byte) {
                            lock::mark_read(file, self.slot, byte, true)?;
                            fresh.push((byte, mark));
                        }
                    }
                }
                whole = held.len() > MARKS_AN_ORDER;
            }
            if whole {
                lock::mark_read(file, self.slot, order_byte, true)?;
                fresh.push((order_byte, Mark::Block(order_byte)));
                for b in s.marks.remove(&key).unwrap_or_default() {
                    lock::mark_read(file, self.slot, b, false)?;
                }
                s.whole.insert(key);
            }
        }
        // Each writer of what it newly marked: one running holds the block, its gap or its row, and a
        // commit since its snapshot named it, or a block cut from it.
        let mut met: BTreeSet<u64> = BTreeSet::new();
        let meets: BTreeSet<u64> = fresh.iter().flat_map(|&(_, m)| here.met_by(m)).collect();
        for &b in &meets {
            for slot in lock::writers(file, b)? {
                if let Some((_, id)) = here.running.iter().find(|(x, _)| *x == slot) {
                    met.insert(*id);
                }
            }
        }
        for r in &here.records {
            if r.id == 0
                || r.id == me
                || r.flags & ROLLED_BACK != 0
                || r.commit <= s.snapshot
                || r.flags & DID_WRITE == 0
            {
                continue;
            }
            if r.wrote.iter().any(|w| meets.contains(w)) {
                met.insert(r.id);
            }
        }
        for w in met {
            {
                let txn = t.borrow();
                if self.doomed(v, &txn, w)? {
                    continue;
                }
            }
            // A READ ONLY transaction comes first where its writer read over no commit before its
            // snapshot.
            if read_only {
                if let Who::Committed(r) = v.who(w) {
                    if r.conflict_out.is_none_or(|c| order < c) {
                        continue;
                    }
                }
            }
            let stands = self.edge_stands(v, &t.borrow(), me, w)?;
            if !stands {
                self.flag(v, t, me, w)?;
            }
        }
        Ok(())
    }

    /// A write of the transaction changes these blocks of an order, these rows, and lands keys in these
    /// blocks: each block's marks tested, each row's, the gaps' of those it lands in, and the marks on
    /// what they were cut from and on the order whole. A transaction someone doomed fails here.
    pub(super) fn mark_wrote(&self, table: usize, order: usize, w: &Written) -> Result<()> {
        if self.committing {
            return self.wrote_at_commit(w);
        }
        if let Some(m) = &self.monitor {
            let t = &self.catalog.tables[table];
            for b in w.bytes() {
                m.wrote(&t.name, &t.orders[order].name, b)?;
            }
        }
        let mut guard = self.serial.borrow_mut();
        let Some(s) = guard.as_mut().filter(|s| s.t.borrow().active) else {
            return Ok(());
        };
        let t = s.t.clone();
        self.at_turn(&t, |v| self.tested_write(v, &t, s, table, order, w))
    }

    fn tested_write(
        &self,
        v: &View,
        t: &Rc<RefCell<Txn>>,
        s: &mut Serial,
        table: usize,
        order: usize,
        w: &Written,
    ) -> Result<()> {
        let here = &v.files[s.place];
        let file = &here.file;
        let me = {
            let mut txn = t.borrow_mut();
            if self.doomed(v, &txn, txn.id)? {
                return Err(failed(
                    "Canceled on identification as a pivot, during conflict in checking.",
                ));
            }
            txn.did_write = true;
            txn.id
        };
        let order_byte = lock::order_byte(self.lock_key(table), order);
        if s.wrote.insert(order_byte) {
            lock::mark_wrote(file, self.slot, order_byte, true)?;
        }
        // What lands anywhere in the order lands in the gaps of a block a commit let go.
        let mut tests: BTreeSet<u64> = here.with_parents(order_byte).into_iter().collect();
        for b in w.bytes() {
            if s.wrote.insert(b) {
                lock::mark_wrote(file, self.slot, b, true)?;
            }
        }
        for &b in &w.blocks {
            tests.extend(here.met_by(Mark::Block(b)));
        }
        for &r in &w.rows {
            tests.extend(here.met_by(Mark::Row(r)));
        }
        for &b in &w.landed {
            tests.extend(here.met_by(Mark::Gap(b)));
        }
        let mut readers: BTreeSet<u64> = BTreeSet::new();
        for &b in &tests {
            for slot in lock::readers(file, b)? {
                if let Some((_, id)) = here.running.iter().find(|(x, _)| *x == slot) {
                    readers.insert(*id);
                }
            }
        }
        for r in &here.records {
            if r.id != 0
                && r.id != me
                && r.flags & ROLLED_BACK == 0
                && r.commit > s.snapshot
                && r.marks.iter().any(|m| tests.contains(m))
            {
                readers.insert(r.id);
            }
        }
        for x in readers {
            let stands = {
                let txn = t.borrow();
                self.doomed(v, &txn, x)? || self.edge_stands(v, &txn, x, me)?
            };
            if !stands {
                self.flag(v, t, x, me)?;
            }
        }
        Ok(())
    }

    /// The blocks a transaction's commit writes its rows into, the rows and the gaps, which its
    /// statements tested: named among those it wrote, for a read after its commit.
    fn wrote_at_commit(&self, w: &Written) -> Result<()> {
        let mut guard = self.serial.borrow_mut();
        let Some(s) = guard.as_mut().filter(|s| s.t.borrow().active) else {
            return Ok(());
        };
        let t = s.t.clone();
        let txn = t.borrow();
        let file = &txn.files[s.place].lock;
        lock::take_serial_turn(file)?;
        let mut marked = Ok(());
        for b in w.bytes() {
            if marked.is_ok() && s.wrote.insert(b) {
                marked = lock::mark_wrote(file, self.slot, b, true);
            }
        }
        lock::give_serial_turn(file)?;
        marked
    }

    /// Whether the transaction's commit writes its records into this file though it wrote nothing here:
    /// another serializable transaction that is not READ ONLY is open on it.
    pub(super) fn serial_records(&self) -> Result<bool> {
        let guard = self.serial.borrow();
        let Some(s) = guard.as_ref().filter(|s| s.t.borrow().active) else {
            return Ok(false);
        };
        let txn = s.t.borrow();
        let file = &txn.files[s.place].lock;
        for (slot, id) in lock::serials(file)? {
            if id != txn.id && lock::writes_in(file, slot)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The transaction's commit tested, as its commit's place on the clock is known: refused where
    /// someone doomed it; a pivot whose other edge comes from here, or from one neither READ ONLY nor
    /// ended, doomed. `generation`, where it commits by a root in this file, is the generation it makes.
    pub(super) fn serial_commit_tested(&self, generation: u64) -> Result<()> {
        let mut guard = self.serial.borrow_mut();
        let Some(s) = guard.as_mut().filter(|s| s.t.borrow().active) else {
            return Ok(());
        };
        let t = s.t.clone();
        let tested = self.at_turn(&t, |v| self.commit_test(v, &t));
        if tested.is_ok() {
            s.commit_at = Some(generation);
        }
        tested
    }

    fn commit_test(&self, v: &View, t: &Rc<RefCell<Txn>>) -> Result<()> {
        if t.borrow().prepared.is_some() {
            return Ok(());
        }
        {
            let txn = t.borrow();
            if self.doomed(v, &txn, txn.id)? {
                return Err(failed(
                    "Canceled on identification as a pivot, during commit attempt.",
                ));
            }
        }
        let me = t.borrow().id;
        let near_of = self.edges(v, &t.borrow(), me, false)?;
        for near in near_of {
            let doomed = {
                let txn = t.borrow();
                !matches!(v.who(near), Who::Running(..)) || self.doomed(v, &txn, near)?
            };
            if doomed {
                continue;
            }
            let far_of = self.edges(v, &t.borrow(), near, false)?;
            for far in far_of {
                let counts = far == me || {
                    let txn = t.borrow();
                    matches!(v.who(far), Who::Running(..))
                        && !self.is_read_only(v, &txn, far)?
                        && !self.doomed(v, &txn, far)?
                };
                if counts {
                    let mut txn = t.borrow_mut();
                    for f in &txn.files {
                        lock::mark_doom(&f.lock, self.slot, near, true)?;
                    }
                    txn.dooms.insert(near);
                    break;
                }
            }
        }
        let mut txn = t.borrow_mut();
        // The first commit it read over, those that committed before it.
        if txn.did_write && !txn.read_only {
            for x in self.edges(v, &txn, txn.id, true)? {
                if let Who::Committed(r) = v.who(x) {
                    txn.conflict_out = Some(txn.conflict_out.map_or(r.order, |c| c.min(r.order)));
                }
            }
        }
        // Past every clock its files and those it reached stand at: their commits are behind it.
        let mut clocks = vec![txn.order];
        for f in &txn.files {
            clocks.push(match &f.reader {
                Some(r) => r.peek_root()?.clock,
                None => self.reader.peek_root()?.clock,
            });
        }
        clocks.extend(
            v.files
                .iter()
                .flat_map(|f| f.records.iter().map(|r| r.order)),
        );
        let order = next_on_clock(clocks.into_iter());
        for f in &txn.files {
            lock::mark_order(&f.lock, self.slot, true, order, true)?;
        }
        txn.prepared = Some(order);
        Ok(())
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
            let txn = s.t.borrow();
            s.commit_at.filter(|p| *p == generation).map(|_| Record {
                id: txn.id,
                snapshot: txn.order,
                commit: generation,
                order: txn.prepared.unwrap_or(0),
                flags: if txn.read_only || !txn.did_write {
                    READ_ONLY
                } else {
                    0
                } | if txn.did_write { DID_WRITE } else { 0 },
                conflict_out: txn.conflict_out,
                marks: s
                    .marks
                    .values()
                    .flatten()
                    .copied()
                    .chain(s.whole.iter().map(|(t, o)| lock::order_byte(*t, *o)))
                    .collect(),
                wrote: s.wrote.iter().copied().collect(),
                ins: txn.ins.iter().copied().collect(),
                outs: txn.outs.iter().copied().collect(),
                dooms: txn.dooms.iter().copied().collect(),
                cuts: Vec::new(),
            })
        });
        let open = {
            let file = w.file().try_clone()?;
            !lock::serials(&file)?.is_empty()
        };
        if let Some(mut r) = mine {
            r.cuts = cuts;
            root.clock = root.clock.max(r.order);
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
                let key = self.lock_key(*ti);
                blocks.iter().flat_map(move |b| {
                    let from = lock::block_byte_of(key, *oi, b.first_key());
                    let pairs: Vec<(u64, u64)> = match b.staged_entries().is_empty() {
                        // A block no longer named: what lands in its gap lands in any of its order.
                        true => vec![(lock::order_byte(key, *oi), from)],
                        false => b
                            .staged_entries()
                            .iter()
                            .map(|e| lock::block_byte_of(key, *oi, &e.first))
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
            let key = self.lock_key(*ti);
            for b in blocks {
                let from = lock::block_byte_of(key, *oi, b.first_key());
                if b.staged_entries().is_empty() {
                    m.let_go(&t.name, &o.name, from)?;
                    m.let_go(&t.name, &o.name, lock::gap_byte(from))?;
                }
                for e in b.staged_entries() {
                    let to = lock::block_byte_of(key, *oi, &e.first);
                    if to != from {
                        m.cut(&t.name, &o.name, from, to)?;
                        m.cut(&t.name, &o.name, lock::gap_byte(from), lock::gap_byte(to))?;
                    }
                }
            }
        }
        Ok(())
    }

    /// The transaction's end, its marks in this file let go at the turn: once its root is written,
    /// committed; or rolled back, where those it doomed go into the records by a commit of its own, as
    /// PostgreSQL's doom outlives its doomer.
    pub(super) fn serial_end(&mut self) -> Result<()> {
        let Some(s) = self.serial.borrow_mut().take() else {
            return Ok(());
        };
        if self.writer.is_none() {
            return Ok(());
        }
        let t = s.t.clone();
        let doomed: Option<Record> = {
            let txn = t.borrow();
            (txn.prepared.is_none() && !txn.dooms.is_empty()).then(|| Record {
                id: txn.id,
                flags: ROLLED_BACK,
                dooms: txn.dooms.iter().copied().collect(),
                ..Default::default()
            })
        };
        if doomed.is_some() {
            self.ending = doomed;
            let mut w = self.writer.take().expect("a snail open to write");
            let turn = lock::take_root_turn(w.file());
            let written = turn.and_then(|_| self.commit_with(&mut w));
            let given = lock::give_root_turn(w.file());
            self.writer = Some(w);
            self.ending = None;
            written?;
            given?;
        }
        *self.serial.borrow_mut() = Some(s);
        let released = self.let_go_own(&t);
        *self.serial.borrow_mut() = None;
        released
    }

    /// At the root's turn, a commit's test: it stands at the generation the commit makes in this file.
    pub(super) fn serial_tested(&self) -> Result<()> {
        let active = self
            .serial
            .borrow()
            .as_ref()
            .is_some_and(|s| s.t.borrow().active);
        if !active {
            return Ok(());
        }
        let generation = self.reader.peek_root()?.generation + 1;
        self.serial_commit_tested(generation)
    }

    /// A commit's test where the transaction writes no root: at the root's turn too, so that it
    /// stands among the commits.
    pub(super) fn serial_tested_alone(&self) -> Result<()> {
        let active = self
            .serial
            .borrow()
            .as_ref()
            .is_some_and(|s| s.t.borrow().active);
        if !active {
            return Ok(());
        }
        let file = self.the_writer_file()?;
        lock::take_root_turn(&file)?;
        let tested = self.serial_tested();
        lock::give_root_turn(&file)?;
        // It writes no record: it commits by no generation of this file.
        if let Some(s) = self.serial.borrow_mut().as_mut() {
            s.commit_at = None;
        }
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

#[cfg(test)]
mod tests {
    use super::next_on_clock;

    #[test]
    fn a_commit_stands_past_every_clock_it_saw_and_no_earlier_than_the_machine_s() {
        let far = u64::MAX / 2;
        assert_eq!(next_on_clock([1, far, 3].into_iter()), far + 1);
        let now = super::super::change::wall_micros() as u64;
        let at = next_on_clock([1, 2].into_iter());
        assert!(at >= now, "{at} {now}");
    }
}
