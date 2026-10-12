//! A write's round. The rows a statement inserts, deletes or replaces are routed on the table's order
//! and on every index that holds their columns, each to the block its places name. A row deleted is
//! marked dead in its block's page map; a row inserted is written into each column's last page of its
//! block, the page written afresh; a block whose last page has no room left is rewritten in order,
//! its dead rows dropped, and cut again. Every page changed is written to a fresh page, the directory
//! up to its top, and the commit is the root.

use super::statement::{self, Datum, Fill, Row, Source, Statement};
use super::{Snail, Wasted};
use crate::catalog::{Catalog, ColumnType, Order, Table};
use crate::cut::ROW_GROUP_ROWS;
use crate::directory::{
    build_from, decode_page, decode_top, encode_page, encode_top, pack, Entry, Head, PageEntry,
    PageMap,
};
use crate::encode::{decode, descriptor, plain_pages};
use crate::file::{decode_runs, encode_runs, Extent, Place, Root, Run, Writer, STAGED};
use crate::lock;

/// The runs of reused pages the root names, at most.
const REUSED_KEPT: usize = 64;

/// The times a statement is run again on the newest root, at most, when it finds the blocks it
/// changes written since.
const RETRIES: u32 = 64;
use crate::places::{key, month_utc, KeyValue};
use crate::values::Values;
use crate::write::{empty_head, head_of, laid_out, write_block, Laid, Rows};
use crate::{Error, Result};
use arrow::array::{
    Array, BinaryArray, Decimal128Array, Int16Array, Int32Array, Int64Array, RecordBatch,
    StringArray, TimestampMicrosecondArray,
};
use parquet::data_type::ByteArray;
use std::collections::BTreeMap;

/// What a statement did.
#[derive(Clone, Debug, Default)]
pub struct Done {
    pub statement: &'static str,
    /// The rows it inserted, updated or deleted.
    pub rows: u64,
    /// The generation the file stood at when it ended: a statement outside a transaction commits.
    pub generation: u64,
    pub pages_read: u64,
    /// The pages written, the root among them where the statement committed.
    pub pages_written: u64,
    /// The blocks rewritten in order.
    pub rewritten: u64,
    /// The orders it wrote: those holding a column it changed.
    pub orders: u64,
    /// Of the pages written, those written into pages freed before.
    pub pages_reused: u64,
}

/// How a writer takes a block's lock: waiting while another writer has it; at once or not at all;
/// or, already holding others, trying again for ten seconds, never waiting on a writer that may be
/// waiting on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Waiting {
    Wait,
    Try,
    Retry,
}

/// One row's change on a table.
enum Change {
    Insert(Row),
    Delete(Row),
    Replace(Row, Row),
}

/// One row's change on one order: its key there, the values the order holds, and for a row
/// replaced, the key of the row that replaced it.
struct Effect {
    key: Vec<KeyValue>,
    values: Vec<Datum>,
    delete: bool,
    chain: Option<Vec<KeyValue>>,
}

/// A block a transaction began from, by its first key and its page map then, and the entries that
/// stand for it now, their pages staged.
pub(super) struct Rebased {
    first: Vec<KeyValue>,
    child: Extent,
    entries: Vec<Entry>,
    /// For an order that held no row, the head its first rows gave it.
    head: Option<Head>,
}

/// The catalog a statement or a transaction declared: the catalog it began from, its DDL now, and
/// the orders it made, each empty until rows come.
pub(super) struct Declared {
    from: Extent,
    ddl: String,
    made: Vec<(String, String)>,
}

/// A directory page read on the way down, and the pages under it read too.
struct Node {
    entries: Vec<Entry>,
    height: u32,
    kids: BTreeMap<usize, Node>,
    /// The page it was read from; none at the top.
    at: Extent,
}

/// The entry, among an ordered page's, whose rows a key belongs with: the last whose first key is at
/// most the key, or the first.
fn pick(entries: &[Entry], key: &[KeyValue]) -> usize {
    entries
        .partition_point(|e| e.first.as_slice() <= key)
        .saturating_sub(1)
}

fn datum_at(v: &Values, i: usize) -> Datum {
    match v {
        Values::Int(x) => x[i].map_or(Datum::Null, Datum::Int),
        _ => v
            .text(i)
            .map_or(Datum::Null, |s| Datum::Text(s.to_string())),
    }
}

/// A value of a column of type `ty`: bytea's bytes as they are, any other as `datum_at` reads it.
fn datum_typed(v: &Values, i: usize, ty: ColumnType) -> Datum {
    match ty {
        ColumnType::Bytes => v
            .text_value(i)
            .map_or(Datum::Null, |b| Datum::Bytes(b.data().to_vec())),
        _ => datum_at(v, i),
    }
}

fn push(v: &mut Values, d: &Datum) {
    match (v, d) {
        (Values::Int(x), Datum::Int(n)) => x.push(Some(*n)),
        (Values::Int(x), Datum::Null) => x.push(None),
        (Values::Text(x), Datum::Text(s)) => x.push(Some(s.clone())),
        (Values::Text(x), Datum::Null) => x.push(None),
        (Values::Utf8(x), Datum::Bytes(b)) => x.push(Some(ByteArray::from(b.clone()))),
        (Values::Utf8(x), Datum::Null) => x.push(None),
        _ => panic!("a value pushed onto a column of another kind"),
    }
}

/// A row's key on an order.
fn key_of(order: &Order, row: &Row) -> Vec<KeyValue> {
    order
        .levels
        .iter()
        .map(|&l| match &row[l.column()] {
            Datum::Int(v) => key(l, *v),
            Datum::Text(s) => KeyValue::Text(s.as_str().into()),
            Datum::Null => unreachable!("a level holds no NULL; checked before"),
            Datum::Bytes(_) => unreachable!("a level holds no bytea; the catalog refuses it"),
        })
        .collect()
}

/// The rows of an answer whose columns are the table's, in its order.
fn rows_of(batch: &RecordBatch) -> Result<Vec<Row>> {
    let mut rows = vec![Vec::with_capacity(batch.num_columns()); batch.num_rows()];
    for a in batch.columns() {
        let any = a.as_any();
        for (r, row) in rows.iter_mut().enumerate() {
            row.push(if a.is_null(r) {
                Datum::Null
            } else if let Some(x) = any.downcast_ref::<Int16Array>() {
                Datum::Int(x.value(r) as i64)
            } else if let Some(x) = any.downcast_ref::<Int32Array>() {
                Datum::Int(x.value(r) as i64)
            } else if let Some(x) = any.downcast_ref::<Int64Array>() {
                Datum::Int(x.value(r))
            } else if let Some(x) = any.downcast_ref::<TimestampMicrosecondArray>() {
                Datum::Int(x.value(r))
            } else if let Some(x) = any.downcast_ref::<StringArray>() {
                Datum::Text(x.value(r).to_string())
            } else if let Some(x) = any.downcast_ref::<Decimal128Array>() {
                // A numeric column's answer is its smallest units, at the column's own scale.
                Datum::Int(x.value(r) as i64)
            } else if let Some(x) = any.downcast_ref::<BinaryArray>() {
                Datum::Bytes(x.value(r).to_vec())
            } else {
                return Err(Error::new(format!(
                    "an answer of {} is not read back as a row",
                    a.data_type()
                )));
            });
        }
    }
    Ok(rows)
}

/// Refuses a row a table cannot hold.
fn check(table: &Table, row: &Row) -> Result<()> {
    if row.len() != table.columns.len() {
        return Err(Error::new(format!(
            "{}: a row of {} values for {} columns",
            table.name,
            row.len(),
            table.columns.len()
        )));
    }
    let months = table.month_columns();
    for (c, (col, d)) in table.columns.iter().zip(row).enumerate() {
        let what = |why: String| Error::new(format!("{}.{}: {why}", table.name, col.name));
        match (col.ty, d) {
            (_, Datum::Null) if col.not_null => {
                return Err(what("is NOT NULL and the row holds NULL".into()))
            }
            (_, Datum::Null) => {}
            (ColumnType::Int2, Datum::Int(v)) if i16::try_from(*v).is_err() => {
                return Err(what(format!("{v} is out of range")))
            }
            (ColumnType::Int4, Datum::Int(v)) if i32::try_from(*v).is_err() => {
                return Err(what(format!("{v} is out of range")))
            }
            (ColumnType::Timestamptz, Datum::Int(v))
                if (*v == i64::MAX || *v == i64::MIN) && months.contains(&c) =>
            {
                return Err(what(
                    "holds an infinite instant, whose month cannot be counted".into(),
                ))
            }
            (ColumnType::Text(Some(n)), Datum::Text(s)) if s.chars().count() > n as usize => {
                return Err(what(format!(
                    "is varchar({n}) and the row holds {} characters",
                    s.chars().count()
                )))
            }
            (ColumnType::Numeric { precision, .. }, Datum::Int(v))
                if v.unsigned_abs() >= 10u64.pow(precision as u32) =>
            {
                return Err(what(format!("{v} has more digits than it holds")))
            }
            (ColumnType::Text(_), Datum::Text(_)) | (ColumnType::Bytes, Datum::Bytes(_)) => {}
            (ColumnType::Text(_) | ColumnType::Bytes, Datum::Int(_))
            | (_, Datum::Text(_))
            | (_, Datum::Bytes(_)) => {
                return Err(what(format!("{d:?} is not a value of {}", col.ty.name())))
            }
            (_, Datum::Int(_)) => {}
        }
    }
    Ok(())
}

impl Snail {
    /// Runs one statement that writes, or begins or ends a transaction. Outside a transaction a
    /// statement commits; inside one, its pages wait for the COMMIT, and its rows are what the
    /// transaction's later statements read.
    pub fn execute(&mut self, sql: &str) -> Result<Done> {
        if self.writer.is_none() {
            return Err(Error::new(
                "this snail was opened to read; open it to write",
            ));
        }
        let st = statement::parse(&self.catalog, sql)?;
        if self.begun.is_none() && self.rounds > 0 {
            self.renew()?;
        }
        self.rounds += 1;
        let (read0, written0, reused0) = (self.reader.pages_read(), self.written(), self.reused);
        let mut done = Done {
            statement: st.kind(),
            ..Default::default()
        };
        let mut tries = 0;
        let changed = match st {
            Statement::Begin => {
                if self.begun.is_some() {
                    return Err(Error::new("a transaction is begun already"));
                }
                self.begun = Some((self.root.clone(), false));
                false
            }
            Statement::Commit => {
                let (_, changed) = self
                    .begun
                    .take()
                    .ok_or_else(|| Error::new("no transaction is begun"))?;
                changed
            }
            Statement::Rollback => {
                let (root, _) = self
                    .begun
                    .take()
                    .ok_or_else(|| Error::new("no transaction is begun"))?;
                self.root = root;
                self.abandon()?;
                false
            }
            st => loop {
                tries += 1;
                if tries > RETRIES {
                    self.abandon()?;
                    return Err(Error::new(
                        "the blocks the statement changes were written since by other writers, again \
                         and again",
                    ));
                }
                self.stale = false;
                match self.write_rows(st.clone(), &mut done) {
                    Ok(_) if self.stale => {
                        // A block the statement locked was written since the root it read: on
                        // its own, it is run again on the newest root; in a transaction, refused.
                        self.abandon()?;
                        if self.begun.is_some() {
                            self.begun = None;
                            return Err(Error::new(
                                "a block the transaction changes was written since by another \
                                 writer: the transaction is rolled back",
                            ));
                        }
                        self.renew()?;
                    }
                    Ok(changed) => break changed,
                    Err(e) => {
                        self.abandon()?;
                        self.begun = None;
                        return Err(e);
                    }
                }
            },
        };
        if let Some((_, any)) = self.begun.as_mut() {
            *any |= changed;
        } else if changed {
            self.commit()?;
        }
        done.generation = self.root.generation;
        done.pages_read = self.reader.pages_read() - read0;
        done.pages_written = self.written() - written0;
        done.pages_reused = self.reused - reused0;
        Ok(done)
    }

    /// The rows an INSERT, UPDATE or DELETE writes, staged. Returns whether it changed any.
    fn write_rows(&mut self, st: Statement, done: &mut Done) -> Result<bool> {
        Ok(match st {
            Statement::Insert { table, source } => {
                let rows = match source {
                    Source::Values(rows) => rows,
                    Source::Select { question, fill } => {
                        let (batch, _) = self.answer(&question, None)?;
                        rows_of(&batch)?
                            .into_iter()
                            .map(|a| {
                                fill.iter()
                                    .map(|f| match f {
                                        Fill::Literal(d) => d.clone(),
                                        Fill::Answer(i) => a[*i].clone(),
                                    })
                                    .collect()
                            })
                            .collect()
                    }
                };
                done.rows = rows.len() as u64;
                (done.rewritten, done.orders) =
                    self.apply(table, rows.into_iter().map(Change::Insert).collect())?;
                done.rows > 0
            }
            Statement::Update { table, find, sets } => {
                let (batch, _) = self.answer(&find, None)?;
                let old = rows_of(&batch)?;
                done.rows = old.len() as u64;
                let changes = old
                    .into_iter()
                    .map(|o| {
                        let mut n = o.clone();
                        for (c, d) in &sets {
                            n[*c] = d.clone();
                        }
                        Change::Replace(o, n)
                    })
                    .collect();
                (done.rewritten, done.orders) = self.apply(table, changes)?;
                done.rows > 0
            }
            Statement::Delete { table, find } => {
                let (batch, _) = self.answer(&find, None)?;
                let old = rows_of(&batch)?;
                done.rows = old.len() as u64;
                (done.rewritten, done.orders) =
                    self.apply(table, old.into_iter().map(Change::Delete).collect())?;
                done.rows > 0
            }
            Statement::Declare { ddl, .. } => {
                self.declare(&ddl)?;
                true
            }
            Statement::Begin | Statement::Commit | Statement::Rollback => {
                unreachable!("a transaction's own statements write no rows")
            }
        })
    }

    /// Drops what the statement or the transaction staged, the catalog it declared among it, and
    /// lets go of the blocks it locked.
    fn abandon(&mut self) -> Result<()> {
        self.rebased.clear();
        self.reader.clear_staged();
        if self.declared.take().is_some() {
            self.catalog = Snail::catalog(&self.reader, &self.root)?;
        }
        self.let_go_blocks()
    }

    fn let_go_blocks(&mut self) -> Result<()> {
        if let Some(w) = self.writer.as_ref() {
            for &b in &self.locks {
                lock::unlock_block(w.file(), b)?;
            }
        }
        self.locks.clear();
        Ok(())
    }

    fn written(&self) -> u64 {
        self.writer.as_ref().map_or(0, Writer::pages_written)
    }

    /// Commits what the statement, or the transaction, staged: each block it wrote spliced into the
    /// newest directory in place of the block it began from, its staged pages written into the file,
    /// the directory pages above them afresh, then the root.
    fn commit(&mut self) -> Result<()> {
        let mut w = self.writer.take().expect("a snail open to write");
        let turn = lock::take_root_turn(w.file());
        let committed = turn.and_then(|_| self.commit_with(&mut w));
        let given = lock::give_root_turn(w.file());
        self.writer = Some(w);
        self.root = committed?;
        given?;
        self.abandon()
    }

    /// The commit, at the root's turn: it begins from the newest root.
    fn commit_with(&mut self, w: &mut Writer) -> Result<Root> {
        let mut root = self.reader.peek_root()?;
        w.begin_commit(&root);
        let generation = root.generation + 1;
        // The free map: its runs whose tag no older round still runs under are free now, and the
        // commit takes pages from the free runs first.
        let mut runs = if root.free.bytes > 0 {
            decode_runs(&self.reader.read_extent(root.free)?)?
        } else {
            Vec::new()
        };
        let mut gone: Vec<Extent> = Vec::new();
        if root.free.bytes > 0 {
            gone.push(root.free);
        }
        let mut tags: Vec<u64> = runs.iter().map(|r| r.tag).filter(|&t| t > 0).collect();
        tags.sort_unstable();
        tags.dedup();
        let mut freed = 0;
        for t in tags {
            if lock::older_held(self.reader.file(), t)? {
                break;
            }
            freed = t;
        }
        for r in &mut runs {
            if r.tag > 0 && r.tag <= freed {
                r.tag = 0;
            }
        }
        let free: Vec<(u64, u64)> = runs
            .iter()
            .filter(|r| r.tag == 0)
            .map(|r| (r.start, r.pages))
            .collect();
        for &(start, pages) in &free {
            self.reader.forget(start, pages);
        }
        w.use_free(free);
        let waiting: Vec<Run> = runs.into_iter().filter(|r| r.tag > 0).collect();
        // The catalog declared: written afresh, each table it touches given the orders it declares,
        // those it made empty.
        if let Some(d) = self.declared.take() {
            if root.catalog != d.from {
                return Err(Error::new(
                    "the catalog was declared since by another writer",
                ));
            }
            gone.push(root.catalog);
            root.catalog = w.place(d.ddl.as_bytes())?;
            let touched: Vec<&String> = d.made.iter().map(|(t, _)| t).collect();
            let mut orders = Vec::new();
            for (t, o, e) in std::mem::take(&mut root.orders) {
                let made = d.made.contains(&(t.clone(), o.clone()));
                let declared = self
                    .catalog
                    .table(&t)
                    .is_some_and(|x| x.orders.iter().any(|y| y.name == o));
                if touched.contains(&&t) && (made || !declared) {
                    gone.push(e);
                } else {
                    orders.push((t, o, e));
                }
            }
            for (tn, on) in &d.made {
                if let Some(t) = self.catalog.table(tn) {
                    if let Some(o) = t.orders.iter().find(|x| x.name == *on) {
                        let top = w.place(&encode_top(&empty_head(t, o), &[]))?;
                        orders.push((tn.clone(), on.clone(), top));
                    }
                }
            }
            root.orders = orders;
        }
        let rebased = std::mem::take(&mut self.rebased);
        for ((ti, oi), blocks) in rebased {
            let table = &self.catalog.tables[ti];
            let order = &table.orders[oi];
            let at = root
                .orders
                .iter()
                .position(|(t, o, _)| *t == table.name && *o == order.name)
                .ok_or_else(|| {
                    Error::new(format!("{}: the root names no {}", table.name, order.name))
                })?;
            let (mut head, entries) = decode_top(&self.reader.read_extent(root.orders[at].2)?)?;
            let mut top = Node {
                entries,
                height: head.height,
                kids: BTreeMap::new(),
                at: Extent::default(),
            };
            let mut replaced: BTreeMap<Vec<usize>, Vec<Entry>> = BTreeMap::new();
            for b in blocks {
                if let Some(first) = &b.head {
                    head = first.clone();
                }
                let entries = b
                    .entries
                    .into_iter()
                    .map(|e| self.relocate(w, &head, e))
                    .collect::<Result<Vec<_>>>()?;
                if b.child == Extent::default() {
                    if !top.entries.is_empty() {
                        return Err(Error::new(format!(
                            "{}: {} was written since by another writer",
                            table.name, order.name
                        )));
                    }
                    top.entries = entries;
                    top.height = 1;
                    continue;
                }
                let mut path = Vec::new();
                self.locate(&mut top, &head, &b.first, &mut path)?;
                let was = entry_at(&top, &path).clone();
                if was.child != b.child {
                    return Err(Error::new(format!(
                        "{}: a block of {} the statement wrote was written since by another writer",
                        table.name, order.name
                    )));
                }
                // The block's pages the entries standing for it now no longer name.
                let kept: std::collections::HashSet<u64> = entries
                    .iter()
                    .map(|e| self.extents(&head, e))
                    .collect::<Result<Vec<_>>>()?
                    .into_iter()
                    .flatten()
                    .map(|e| e.start)
                    .collect();
                gone.extend(
                    self.extents(&head, &was)?
                        .into_iter()
                        .filter(|e| !kept.contains(&e.start)),
                );
                replaced.insert(path, entries);
            }
            settle(&mut top, &mut Vec::new(), &replaced, w, &mut gone)?;
            gone.push(root.orders[at].2);
            let (bytes, _) = build_from(head, top.entries, top.height, &mut |x| w.place(x))?;
            root.orders[at].2 = w.place(&bytes)?;
        }
        // The free map as it stands now: the runs left free, those still waiting, and the pages
        // this generation stops naming, tagged with it.
        let mut map: Vec<Run> = w
            .left_free()
            .into_iter()
            .map(|(start, pages)| Run {
                start,
                pages,
                tag: 0,
            })
            .chain(waiting)
            .chain(gone.iter().map(|e| Run {
                start: e.start,
                pages: e.pages().max(1),
                tag: generation,
            }))
            .collect();
        map.sort_by_key(|r| r.start);
        let mut runs: Vec<Run> = Vec::with_capacity(map.len());
        for r in map {
            match runs.last_mut() {
                Some(l) if l.tag == r.tag && l.start + l.pages == r.start => l.pages += r.pages,
                _ => runs.push(r),
            }
        }
        root.free = if runs.is_empty() {
            Extent::default()
        } else {
            w.place_end(&encode_runs(&runs))?
        };
        for (start, pages) in w.taken() {
            self.reused += pages;
            root.reused.push((generation, start, pages));
        }
        while root.reused.len() > REUSED_KEPT {
            let (g, _, _) = root.reused.remove(0);
            root.reused_from = root.reused_from.max(g);
        }
        root.generation = generation;
        w.commit(&mut root)?;
        Ok(root)
    }

    /// Every page an entry's block holds: its page map, and each column's dictionary and pages.
    fn extents(&self, head: &Head, e: &Entry) -> Result<Vec<Extent>> {
        let map = PageMap::decode(&self.reader.read_extent(e.child)?, &head.level_text)?;
        let mut out = vec![e.child];
        for c in &map.columns {
            out.extend(c.dictionary);
            out.extend(c.pages.iter().map(|p| p.at));
        }
        Ok(out)
    }

    /// An entry whose pages are staged, its pages written into the file and its page map with them.
    fn relocate(&self, w: &mut Writer, head: &Head, mut e: Entry) -> Result<Entry> {
        if e.child.start < STAGED {
            return Ok(e);
        }
        let mut map = PageMap::decode(&self.reader.read_extent(e.child)?, &head.level_text)?;
        let mut copy = |x: Extent| -> Result<Extent> {
            if x.start < STAGED {
                Ok(x)
            } else {
                w.place(&self.reader.read_extent(x)?)
            }
        };
        for c in &mut map.columns {
            if let Some(d) = c.dictionary {
                c.dictionary = Some(copy(d)?);
            }
            for p in &mut c.pages {
                p.at = copy(p.at)?;
            }
        }
        e.child = w.place(&map.encode())?;
        Ok(e)
    }

    /// Notes that a block of an order is replaced by `new`: either a block the transaction began from,
    /// or one it staged itself, found among those that stand for a block it began from.
    fn rebase(&mut self, key: (usize, usize), old: &Entry, new: &[Entry]) -> Result<()> {
        let log = self.rebased.entry(key).or_default();
        if old.child.start < STAGED {
            log.push(Rebased {
                first: old.first.clone(),
                child: old.child,
                entries: new.to_vec(),
                head: None,
            });
            return Ok(());
        }
        for r in log.iter_mut() {
            if let Some(i) = r.entries.iter().position(|e| e.child == old.child) {
                r.entries.splice(i..i + 1, new.iter().cloned());
                return Ok(());
            }
        }
        Err(Error::new(
            "a block staged is none of the blocks the transaction wrote",
        ))
    }

    #[doc(hidden)]
    pub fn stop_at(&mut self, stop: Option<crate::file::Stop>) {
        if let Some(w) = self.writer.as_mut() {
            w.stop_at(stop);
        }
    }

    /// Applies one statement's changes to every order of a table. Returns the blocks rewritten in
    /// order, and the orders written.
    fn apply(&mut self, ti: usize, changes: Vec<Change>) -> Result<(u64, u64)> {
        let table = self.catalog.tables[ti].clone();
        for c in &changes {
            match c {
                Change::Insert(r) | Change::Replace(_, r) => check(&table, r)?,
                Change::Delete(_) => {}
            }
        }
        let mut stage = self.reader.stage();
        let applied: Vec<u64> = (0..table.orders.len())
            .map(|oi| self.apply_order(&mut stage, ti, &table, oi, &changes))
            .collect::<Result<Vec<Option<u64>>>>()?
            .into_iter()
            .flatten()
            .collect();
        Ok((applied.iter().sum(), applied.len() as u64))
    }

    /// One order's part of a statement: its effects routed to their blocks, each block written
    /// afresh, then the directory above them up to a new top. Returns the blocks rewritten in order,
    /// or none where the order holds no column the statement changed.
    fn apply_order(
        &mut self,
        w: &mut dyn Place,
        ti: usize,
        table: &Table,
        oi: usize,
        changes: &[Change],
    ) -> Result<Option<u64>> {
        if self.stale {
            return Ok(None);
        }
        let order = &table.orders[oi];
        let held =
            |r: &Row| -> Vec<Datum> { order.columns.iter().map(|&c| r[c].clone()).collect() };
        let mut effects = Vec::new();
        for c in changes {
            match c {
                Change::Insert(r) => effects.push(Effect {
                    key: key_of(order, r),
                    values: held(r),
                    delete: false,
                    chain: None,
                }),
                Change::Delete(r) => effects.push(Effect {
                    key: key_of(order, r),
                    values: held(r),
                    delete: true,
                    chain: None,
                }),
                Change::Replace(o, n) => {
                    if held(o) == held(n) {
                        continue;
                    }
                    let to = key_of(order, n);
                    effects.push(Effect {
                        key: key_of(order, o),
                        values: held(o),
                        delete: true,
                        chain: Some(to.clone()),
                    });
                    effects.push(Effect {
                        key: to,
                        values: held(n),
                        delete: false,
                        chain: None,
                    });
                }
            }
        }
        if effects.is_empty() {
            return Ok(None);
        }
        let at = self
            .root
            .orders
            .iter()
            .position(|(t, o, _)| *t == table.name && *o == order.name)
            .ok_or_else(|| {
                Error::new(format!("{}: the root names no {}", table.name, order.name))
            })?;
        let (mut head, entries) = decode_top(&self.reader.read_extent(self.root.orders[at].2)?)?;
        let mut top = Node {
            entries,
            height: head.height,
            kids: BTreeMap::new(),
            at: Extent::default(),
        };
        let mut pruned = 0;
        let mut replaced: BTreeMap<Vec<usize>, Vec<Entry>> = BTreeMap::new();
        if top.entries.is_empty() {
            if effects.iter().any(|e| e.delete) {
                return Err(Error::new(format!("{} holds no row to delete", order.name)));
            }
            let rows: Vec<Vec<Datum>> = effects.into_iter().map(|e| e.values).collect();
            let (entries, first) = self.written_in_order(w, table, order, rows)?;
            top.entries = entries;
            top.height = 1;
            head = first;
            self.rebased.entry((ti, oi)).or_default().push(Rebased {
                first: Vec::new(),
                child: Extent::default(),
                entries: top.entries.clone(),
                head: Some(head.clone()),
            });
        } else {
            let mut by_block: BTreeMap<Vec<usize>, Vec<Effect>> = BTreeMap::new();
            for e in effects {
                let mut path = Vec::new();
                self.locate(&mut top, &head, &e.key, &mut path)?;
                by_block.entry(path).or_default().push(e);
            }
            let blocks: Vec<Entry> = by_block.keys().map(|p| entry_at(&top, p).clone()).collect();
            let waiting = if self.locks.is_empty() {
                Waiting::Wait
            } else {
                Waiting::Retry
            };
            if !self.lock_blocks(ti, oi, blocks, waiting)? {
                self.stale = true;
                return Ok(None);
            }
            for (path, effects) in by_block {
                let entry = entry_at(&top, &path).clone();
                let (new, rewrote) =
                    self.write_block_afresh(w, table, order, &head, &entry, effects, false)?;
                pruned += rewrote as u64;
                self.rebase((ti, oi), &entry, &new)?;
                replaced.insert(path, new);
            }
        }
        let mut path = Vec::new();
        settle(&mut top, &mut path, &replaced, w, &mut Vec::new())?;
        let (bytes, _) = build_from(head, top.entries, top.height, &mut |b| w.place(b))?;
        let extent = w.place(&bytes)?;
        self.root.orders[at].2 = extent;
        Ok(Some(pruned))
    }

    /// Locks the blocks a statement changes before it reads them: each block the transaction began
    /// from, by its table, order and first key, in the order of their locks; a block the transaction
    /// staged lies under the lock of the block it began from. Returns false where a lock was not
    /// taken, under `Waiting::Try`, or where a block was written since the root the round reads.
    fn lock_blocks(
        &mut self,
        ti: usize,
        oi: usize,
        blocks: Vec<Entry>,
        waiting: Waiting,
    ) -> Result<bool> {
        let mut want: Vec<(u64, Entry)> = blocks
            .into_iter()
            .filter(|e| e.child.start < STAGED)
            .map(|e| (lock::block_byte(&(ti, oi, &e.first)), e))
            .collect();
        want.sort_by_key(|x| x.0);
        want.dedup_by_key(|x| x.0);
        let file = self.writer.as_ref().expect("a snail open to write").file();
        let mut got = Vec::new();
        for (b, _) in &want {
            if self.locks.contains(b) || got.contains(b) {
                continue;
            }
            let taken = match waiting {
                Waiting::Wait => lock::lock_block(file, *b, true)?,
                Waiting::Try => lock::lock_block(file, *b, false)?,
                Waiting::Retry => {
                    let mut tries = 0;
                    loop {
                        if lock::lock_block(file, *b, false)? {
                            break true;
                        }
                        tries += 1;
                        if tries == 1000 {
                            break false;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                }
            };
            if !taken {
                for g in &got {
                    lock::unlock_block(file, *g)?;
                }
                if waiting == Waiting::Retry {
                    return Err(Error::new(
                        "waited ten seconds for a block another writer holds",
                    ));
                }
                return Ok(false);
            }
            got.push(*b);
        }
        self.locks.extend(got);
        // Each block, under its lock, is the block the newest root names at its first key.
        let root = self.reader.peek_root()?;
        let table = &self.catalog.tables[ti];
        let order = &table.orders[oi];
        let at = root
            .orders
            .iter()
            .position(|(t, o, _)| *t == table.name && *o == order.name)
            .ok_or_else(|| {
                Error::new(format!("{}: the root names no {}", table.name, order.name))
            })?;
        let (head, entries) = decode_top(&self.reader.read_extent(root.orders[at].2)?)?;
        let mut top = Node {
            entries,
            height: head.height,
            kids: BTreeMap::new(),
            at: Extent::default(),
        };
        for (_, e) in &want {
            let mut path = Vec::new();
            self.locate(&mut top, &head, &e.first, &mut path)?;
            if entry_at(&top, &path).child != e.child {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Reads down the directory to the block a key belongs with, keeping the pages read.
    fn locate(
        &self,
        node: &mut Node,
        head: &Head,
        key: &[KeyValue],
        path: &mut Vec<usize>,
    ) -> Result<()> {
        let i = pick(&node.entries, key);
        path.push(i);
        if node.height > 1 {
            if !node.kids.contains_key(&i) {
                let entries = decode_page(&self.reader.read_extent(node.entries[i].child)?, head)?;
                node.kids.insert(
                    i,
                    Node {
                        entries,
                        height: node.height - 1,
                        kids: BTreeMap::new(),
                        at: node.entries[i].child,
                    },
                );
            }
            self.locate(node.kids.get_mut(&i).unwrap(), head, key, path)?;
        }
        Ok(())
    }

    /// Prunes the blocks a round found a page or more of waste in: each rewritten in order, its dead
    /// rows dropped, and cut again; then committed as a write is, or within the transaction begun.
    /// Returns the blocks pruned and the pages written.
    pub(super) fn prune(&mut self, wasted: Vec<Wasted>) -> Result<(u64, u64)> {
        let written = self.written();
        let mut stage = self.reader.stage();
        let pruned = self.prune_with(&mut stage, wasted)?;
        match self.begun.as_mut() {
            Some((_, any)) => *any |= pruned > 0,
            None if pruned > 0 => self.commit()?,
            None => self.abandon()?,
        }
        Ok((pruned, self.written() - written))
    }

    fn prune_with(&mut self, w: &mut dyn Place, wasted: Vec<Wasted>) -> Result<u64> {
        let mut by_order: BTreeMap<(usize, usize), Vec<Wasted>> = BTreeMap::new();
        for b in wasted {
            by_order.entry((b.table, b.order)).or_default().push(b);
        }
        let mut pruned = 0;
        for ((ti, oi), blocks) in by_order {
            let table = self.catalog.tables[ti].clone();
            let order = &table.orders[oi];
            let at = self
                .root
                .orders
                .iter()
                .position(|(t, o, _)| *t == table.name && *o == order.name)
                .expect("a block read lies in an order the root names");
            let (head, entries) = decode_top(&self.reader.read_extent(self.root.orders[at].2)?)?;
            let mut top = Node {
                entries,
                height: head.height,
                kids: BTreeMap::new(),
                at: Extent::default(),
            };
            let mut replaced: BTreeMap<Vec<usize>, Vec<Entry>> = BTreeMap::new();
            for b in blocks {
                let mut path = Vec::new();
                self.locate(&mut top, &head, &b.first, &mut path)?;
                let entry = entry_at(&top, &path).clone();
                if entry.child != b.child || replaced.contains_key(&path) {
                    continue;
                }
                // A block another writer has locked, or has written since, is read as it is.
                if !self.lock_blocks(ti, oi, vec![entry.clone()], Waiting::Try)? {
                    continue;
                }
                let (new, _) =
                    self.write_block_afresh(w, &table, order, &head, &entry, Vec::new(), true)?;
                self.rebase((ti, oi), &entry, &new)?;
                replaced.insert(path, new);
                pruned += 1;
            }
            if replaced.is_empty() {
                continue;
            }
            settle(&mut top, &mut Vec::new(), &replaced, w, &mut Vec::new())?;
            let (bytes, _) = build_from(head, top.entries, top.height, &mut |x| w.place(x))?;
            self.root.orders[at].2 = w.place(&bytes)?;
        }
        Ok(pruned)
    }

    /// One block's effects written: the dead marked and the new rows written after each column's
    /// last rows; or the block rewritten in order, where `rewrite` asks it, where it would hold more
    /// rows than a row group, or where rows come and the cut of its rows would make it more than one
    /// block. Returns the entries that stand for it now, and whether it was rewritten.
    #[allow(clippy::too_many_arguments)]
    fn write_block_afresh(
        &self,
        w: &mut dyn Place,
        table: &Table,
        order: &Order,
        head: &Head,
        entry: &Entry,
        effects: Vec<Effect>,
        rewrite: bool,
    ) -> Result<(Vec<Entry>, bool)> {
        let mut map = PageMap::decode(&self.reader.read_extent(entry.child)?, &head.level_text)?;
        let positions = map.rows() as usize;
        let descs = order
            .columns
            .iter()
            .map(|&c| {
                let col = &table.columns[c];
                descriptor(&col.name, col.ty, !col.not_null)
            })
            .collect::<Result<Vec<_>>>()?;
        // Every column of the block, whole.
        let mut columns = Vec::with_capacity(order.columns.len());
        for (s, &c) in order.columns.iter().enumerate() {
            let cp = &map.columns[s];
            let dictionary = cp
                .dictionary
                .map(|d| self.reader.read_extent(d))
                .transpose()?;
            let pages = cp
                .pages
                .iter()
                .map(|p| self.reader.read_extent(p.at))
                .collect::<Result<Vec<_>>>()?;
            let slices: Vec<&[u8]> = pages.iter().map(Vec::as_slice).collect();
            columns.push(decode(
                &descs[s],
                table.columns[c].ty,
                dictionary.as_deref(),
                &slices,
                positions,
            )?);
        }
        let mut dead: std::collections::BTreeSet<u32> = map.dead.iter().copied().collect();
        let types: Vec<ColumnType> = order.columns.iter().map(|&c| table.columns[c].ty).collect();
        let holds = |p: usize, values: &[Datum]| -> bool {
            columns
                .iter()
                .zip(values)
                .zip(&types)
                .all(|((v, d), &ty)| datum_typed(v, p, ty) == *d)
        };
        let level_slots: Vec<usize> = order
            .levels
            .iter()
            .map(|l| order.slot(l.column()).unwrap())
            .collect();
        let mut gone: Vec<(usize, Vec<Datum>)> = Vec::new();
        let mut added: Vec<(Vec<KeyValue>, Vec<Datum>)> = Vec::new();
        for e in effects {
            if e.delete {
                let p = (0..positions)
                    .find(|&p| !dead.contains(&(p as u32)) && holds(p, &e.values))
                    .ok_or_else(|| {
                        Error::new(format!(
                            "{}: a row the statement found is not in {}",
                            table.name, order.name
                        ))
                    })?;
                dead.insert(p as u32);
                if let Some(to) = e.chain {
                    map.chains.push((p as u32, to));
                }
                gone.push((p, e.values));
            } else {
                added.push((e.key, e.values));
            }
        }
        // The block's live rows, and the new ones. Where rows come, they are laid out as a write lays
        // them out, which refuses two that share a unique order's key.
        let rows_now = || {
            let mut rows = Rows {
                columns: table.columns.iter().map(|c| Values::new(c.ty)).collect(),
            };
            let live: Vec<usize> = (0..positions)
                .filter(|p| !dead.contains(&(*p as u32)))
                .collect();
            let n = live.len() + added.len();
            for c in 0..table.columns.len() {
                match order.slot(c) {
                    Some(s) => {
                        for &p in &live {
                            push(&mut rows.columns[c], &datum_typed(&columns[s], p, types[s]));
                        }
                        for (_, row) in &added {
                            push(&mut rows.columns[c], &row[s]);
                        }
                    }
                    None => {
                        for _ in 0..n {
                            push(&mut rows.columns[c], &Datum::Null);
                        }
                    }
                }
            }
            rows
        };
        let mut rows = None;
        let split = !added.is_empty() && {
            let r = rows_now();
            let cut = laid_out(table, order, &r, Some(&head.level_text))?
                .blocks
                .len()
                > 1;
            rows = Some(r);
            cut
        };
        if rewrite || split || positions + added.len() > ROW_GROUP_ROWS {
            // Written in order, and cut by the counts as a block written afresh is.
            let rows = rows.unwrap_or_else(rows_now);
            return Ok((self.blocks_written(w, table, order, head, &rows)?, true));
        }
        // Each column's last page written afresh with the new rows after it, plain, in as many pages
        // as they take.
        let key_at = |p: usize| -> Vec<KeyValue> {
            order
                .levels
                .iter()
                .zip(&level_slots)
                .map(|(&l, &s)| match datum_at(&columns[s], p) {
                    Datum::Int(v) => key(l, v),
                    Datum::Text(t) => KeyValue::Text(t.into_boxed_str()),
                    Datum::Null => unreachable!("a level holds no NULL"),
                    Datum::Bytes(_) => unreachable!("a level holds no bytea"),
                })
                .collect()
        };
        if !added.is_empty() {
            for (s, &c) in order.columns.iter().enumerate() {
                let ty = table.columns[c].ty;
                let cp = &map.columns[s];
                let last = cp
                    .pages
                    .last()
                    .expect("a block's column holds a page")
                    .clone();
                let dictionary = cp
                    .dictionary
                    .map(|d| self.reader.read_extent(d))
                    .transpose()?;
                let bytes = self.reader.read_extent(last.at)?;
                let held = decode(
                    &descs[s],
                    ty,
                    dictionary.as_deref(),
                    &[bytes.as_slice()],
                    last.rows as usize,
                )?;
                let mut values = Values::new(ty);
                for i in 0..held.len() {
                    push(&mut values, &datum_typed(&held, i, ty));
                }
                for (_, row) in &added {
                    push(&mut values, &row[s]);
                }
                let base = last.first_row as usize;
                let mut fresh = Vec::new();
                for p in plain_pages(&descs[s], ty, &values, head.units[s] as usize)?.pages {
                    let row = base + p.first_row;
                    fresh.push(PageEntry {
                        at: w.place(&p.bytes)?,
                        first_row: row as u32,
                        rows: p.rows as u32,
                        first: if row < positions {
                            key_at(row)
                        } else {
                            added[row - positions].0.clone()
                        },
                    });
                }
                let pages = &mut map.columns[s].pages;
                pages.pop();
                pages.extend(fresh);
            }
        }
        // The page map and the entry as they stand now.
        let mut entry = entry.clone();
        map.dead = dead.iter().copied().collect();
        // Where no row of the block held a value of a column, live or dead, its least and greatest
        // mean nothing yet.
        let valueless: Vec<bool> = entry
            .columns
            .iter()
            .map(|sum| sum.nulls == entry.rows && entry.dead == 0)
            .collect();
        entry.rows = entry.rows + added.len() as u32 - gone.len() as u32;
        entry.dead += gone.len() as u32;
        for (key, _) in &added {
            if *key < entry.first {
                entry.first = key.clone();
            }
            if *key > entry.last {
                entry.last = key.clone();
            }
        }
        for s in 0..order.columns.len() {
            let sum = &mut entry.columns[s];
            let mut first_value = valueless[s];
            for (_, row) in &added {
                match &row[s] {
                    Datum::Null => sum.nulls += 1,
                    Datum::Int(v) => {
                        if first_value {
                            sum.min = *v;
                            sum.max = *v;
                            first_value = false;
                        } else {
                            sum.min = sum.min.min(*v);
                            sum.max = sum.max.max(*v);
                        }
                    }
                    Datum::Text(_) | Datum::Bytes(_) => {}
                }
            }
            for (_, values) in &gone {
                if values[s] == Datum::Null {
                    sum.nulls -= 1;
                }
            }
            let cp = &map.columns[s];
            sum.pages = (cp.dictionary.map_or(0, Extent::pages)
                + cp.pages.iter().map(|p| p.at.pages()).sum::<u64>())
                as u32;
            if let Some(mi) = head.month_slots.iter().position(|&x| x as usize == s) {
                let month = |d: &Datum| match d {
                    Datum::Int(v) => Some(month_utc(*v) as usize - 1),
                    _ => None,
                };
                for (_, row) in &added {
                    if let Some(m) = month(&row[s]) {
                        entry.months[mi][m] += 1;
                    }
                }
                for (_, values) in &gone {
                    if let Some(m) = month(&values[s]) {
                        entry.months[mi][m] -= 1;
                    }
                }
            }
            if let Some(t) = head.text_slots.iter().position(|&x| x as usize == s) {
                let bytes = |d: &Datum| match d {
                    Datum::Text(x) => x.len() as u32,
                    Datum::Bytes(x) => x.len() as u32,
                    _ => 0,
                };
                for (_, row) in &added {
                    entry.bytes[t] += bytes(&row[s]);
                }
                for (_, values) in &gone {
                    entry.bytes[t] -= bytes(&values[s]);
                }
            }
        }
        entry.child = w.place(&map.encode())?;
        Ok((vec![entry], false))
    }

    /// Rows of an order written as new blocks, cut by the counts at each sublevel of the order's key.
    fn blocks_written(
        &self,
        w: &mut dyn Place,
        table: &Table,
        order: &Order,
        head: &Head,
        rows: &Rows,
    ) -> Result<Vec<Entry>> {
        let Laid {
            perm, keys, blocks, ..
        } = laid_out(table, order, rows, Some(&head.level_text))?;
        let descs = order
            .columns
            .iter()
            .map(|&c| {
                let col = &table.columns[c];
                descriptor(&col.name, col.ty, !col.not_null)
            })
            .collect::<Result<Vec<_>>>()?;
        let units: Vec<usize> = head.units.iter().map(|&k| k as usize).collect();
        let month_slots: Vec<usize> = head.month_slots.iter().map(|&s| s as usize).collect();
        let text_slots: Vec<usize> = head.text_slots.iter().map(|&s| s as usize).collect();
        let mut entries = Vec::with_capacity(blocks.len());
        for b in &blocks {
            let (entry, _) = write_block(
                w,
                table,
                order,
                rows,
                &perm,
                &keys,
                b,
                &descs,
                &units,
                &month_slots,
                &text_slots,
                &[],
            )?;
            entries.push(entry);
        }
        Ok(entries)
    }

    /// The rows an empty order is given, as its first blocks, and the head they give the order: its
    /// units and its text levels' sublevels, as a snail written from them would hold.
    fn written_in_order(
        &self,
        w: &mut dyn Place,
        table: &Table,
        order: &Order,
        held: Vec<Vec<Datum>>,
    ) -> Result<(Vec<Entry>, Head)> {
        let mut rows = Rows {
            columns: table.columns.iter().map(|c| Values::new(c.ty)).collect(),
        };
        for (c, values) in rows.columns.iter_mut().enumerate() {
            for r in &held {
                push(values, order.slot(c).map_or(&Datum::Null, |s| &r[s]));
            }
        }
        let laid = laid_out(table, order, &rows, None)?;
        let head = head_of(table, order, &laid, held.len());
        Ok((self.blocks_written(w, table, order, &head, &rows)?, head))
    }

    /// A table, an index or a table's order declared: the catalog read again with it, and each order
    /// it adds or changes empty, until rows come. A table that holds rows keeps its columns and
    /// orders as written.
    fn declare(&mut self, ddl: &str) -> Result<()> {
        let text = format!("{}\n{ddl};\n", self.catalog.ddl.trim_end());
        let catalog = Catalog::parse(&text)?;
        let mut made: Vec<(String, String)> = Vec::new();
        let mut kept: Vec<(String, String)> = Vec::new();
        for t in &catalog.tables {
            let old = self.catalog.tables.iter().find(|x| x.name == t.name);
            let columns_same = old.is_some_and(|o| {
                o.columns.len() == t.columns.len()
                    && o.columns
                        .iter()
                        .zip(&t.columns)
                        .all(|(a, b)| a.name == b.name && a.ty == b.ty && a.not_null == b.not_null)
            });
            let mut changed = false;
            for o in &t.orders {
                let was = old.and_then(|x| x.orders.iter().find(|y| y.name == o.name));
                let same = columns_same
                    && was.is_some_and(|w| {
                        w.levels == o.levels
                            && w.columns == o.columns
                            && w.unique == o.unique
                            && w.table_order == o.table_order
                    });
                if same {
                    kept.push((t.name.clone(), o.name.clone()));
                } else {
                    made.push((t.name.clone(), o.name.clone()));
                    changed = true;
                }
            }
            if let (true, Some(o)) = (changed, old) {
                if self.holds_rows(o)? {
                    return Err(Error::new(format!(
                        "{}: it holds rows, so its columns and orders stay as written",
                        t.name
                    )));
                }
            }
        }
        let mut stage = self.reader.stage();
        let mut orders: Vec<(String, String, Extent)> = self
            .root
            .orders
            .iter()
            .filter(|(t, o, _)| kept.contains(&(t.clone(), o.clone())))
            .cloned()
            .collect();
        for (tn, on) in &made {
            let t = catalog.table(tn).expect("a table the catalog names");
            let o = t.orders.iter().find(|x| x.name == *on).unwrap();
            let top = stage.place(&encode_top(&empty_head(t, o), &[]))?;
            orders.push((tn.clone(), on.clone(), top));
        }
        let from = self.declared.as_ref().map_or(self.root.catalog, |d| d.from);
        self.root.catalog = stage.place(text.as_bytes())?;
        self.root.orders = orders;
        self.catalog = catalog;
        let d = self.declared.get_or_insert(Declared {
            from,
            ddl: String::new(),
            made: Vec::new(),
        });
        d.ddl = text;
        for m in made {
            if !d.made.contains(&m) {
                d.made.push(m);
            }
        }
        Ok(())
    }

    /// Whether any order of a table holds a live row, as the round reads the file.
    fn holds_rows(&self, table: &Table) -> Result<bool> {
        for (t, _, top) in &self.root.orders {
            if *t == table.name {
                let (_, entries) = decode_top(&self.reader.read_extent(*top)?)?;
                if entries.iter().any(|e| e.rows > 0) {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
}

fn entry_at<'a>(node: &'a Node, path: &[usize]) -> &'a Entry {
    match path {
        [i] => &node.entries[*i],
        [i, rest @ ..] => entry_at(&node.kids[i], rest),
        [] => unreachable!("a path names a block"),
    }
}

/// Writes the directory pages under `node` that changed, from the blocks up: each block replaced by
/// the entries that stand for it now, and each page read on the way down written afresh, split where
/// it no longer fits a page. Leaves `node`'s own entries as they stand now; the pages written over
/// are added to `gone`.
fn settle(
    node: &mut Node,
    path: &mut Vec<usize>,
    replaced: &BTreeMap<Vec<usize>, Vec<Entry>>,
    w: &mut dyn Place,
    gone: &mut Vec<Extent>,
) -> Result<()> {
    let kids: Vec<usize> = node.kids.keys().rev().copied().collect();
    for i in kids {
        let mut kid = node.kids.remove(&i).unwrap();
        path.push(i);
        settle(&mut kid, path, replaced, w, gone)?;
        path.pop();
        gone.push(kid.at);
        let mut parents = Vec::new();
        for page in pack(kid.entries)? {
            let at = w.place(&encode_page(&page))?;
            parents.push(Entry::over(&page, at)?);
        }
        node.entries.splice(i..i + 1, parents);
    }
    if node.height == 1 {
        let here: Vec<(usize, &Vec<Entry>)> = replaced
            .iter()
            .filter(|(p, _)| p.len() == path.len() + 1 && p[..path.len()] == path[..])
            .map(|(p, e)| (p[path.len()], e))
            .collect();
        for (i, entries) in here.into_iter().rev() {
            node.entries.splice(i..i + 1, entries.iter().cloned());
        }
    }
    Ok(())
}
