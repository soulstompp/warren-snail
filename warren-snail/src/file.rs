//! The file as 4 KB pages. Pages 0 and 1 hold the root's two copies; every other page belongs to the
//! catalog, the directory, or a block. The pages lie in files of a span the root records: the
//! first file holds the root and the locks, and `<path>.1`, `.2` and on hold the pages after.

use crate::cut::PAGE;
#[cfg(feature = "standalone")]
use crate::held::Held;
use crate::{Error, Result};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;

const MAGIC: [u8; 8] = *b"wsnail\0\x01";
/// The format a snail is made in, and the one this build opens.
pub(crate) const FORMAT: u32 = 9;
/// The formats warren-snail 0.1.0 and 0.1.1 made: a file of either is refused by name, with the way
/// across.
const MADE_BY_0_1: [u32; 2] = [7, 8];

/// The pages a file holds before the next begins, unless the snail is made with another span: 1 GiB.
pub const FILE_PAGES: u64 = 1 << 18;

/// A run of whole pages from `start`, of which the first `bytes` are used.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Extent {
    pub start: u64,
    pub bytes: u64,
}

impl Extent {
    pub fn pages(self) -> u64 {
        self.bytes.div_ceil(PAGE as u64)
    }
}

/// Little-endian bytes, written in order.
#[derive(Default)]
pub struct Out(pub Vec<u8>);

impl Out {
    pub fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    pub fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub fn i64(&mut self, v: i64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub fn i128(&mut self, v: i128) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub fn str(&mut self, s: &str) {
        self.u32(s.len() as u32);
        self.0.extend_from_slice(s.as_bytes());
    }
    pub fn bytes(&mut self, b: &[u8]) {
        self.u32(b.len() as u32);
        self.0.extend_from_slice(b);
    }
    pub fn extent(&mut self, e: Extent) {
        self.u64(e.start);
        self.u64(e.bytes);
    }
    pub fn i64s(&mut self, v: &[i64]) {
        self.u32(v.len() as u32);
        for &x in v {
            self.i64(x);
        }
    }
}

/// Little-endian bytes, read in order.
pub struct In<'a> {
    buf: &'a [u8],
    at: usize,
}

impl<'a> In<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        In { buf, at: 0 }
    }
    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.at + n;
        if end > self.buf.len() {
            return Err(Error::new("the directory ends early"));
        }
        let s = &self.buf[self.at..end];
        self.at = end;
        Ok(s)
    }
    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    pub fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    pub fn i128(&mut self) -> Result<i128> {
        Ok(i128::from_le_bytes(self.take(16)?.try_into().unwrap()))
    }
    pub fn str(&mut self) -> Result<String> {
        let n = self.u32()? as usize;
        String::from_utf8(self.take(n)?.to_vec())
            .map_err(|_| Error::new("a name in the file is not UTF-8"))
    }
    pub fn bytes(&mut self) -> Result<Vec<u8>> {
        let n = self.u32()? as usize;
        Ok(self.take(n)?.to_vec())
    }
    pub fn extent(&mut self) -> Result<Extent> {
        Ok(Extent {
            start: self.u64()?,
            bytes: self.u64()?,
        })
    }
    pub fn i64s(&mut self) -> Result<Vec<i64>> {
        let n = self.u32()? as usize;
        (0..n).map(|_| self.i64()).collect()
    }
    pub fn at(&self) -> usize {
        self.at
    }
}

/// The root: where the catalog and each order's directory lie.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct Root {
    /// The generation: one more at each commit of a text, and none at a take.
    pub generation: u64,
    /// The root's turn: every root written takes the next, a commit's or a take's, and the newest
    /// whole copy is the root.
    pub turn: u64,
    /// The roots written since the generation's commit, each a take; none on a commit's root.
    pub takes: u64,
    /// The runs of pages the takes since the generation's commit wrote, which its next commit
    /// carries with its own.
    pub owed: Extent,
    pub pages: u64,
    /// The pages each file holds.
    pub span: u64,
    pub catalog: Extent,
    /// (table, order, the top page of the order's directory)
    pub orders: Vec<(String, String, Extent)>,
    /// The free map: the runs of pages free, and those freed once no round older than their tag
    /// runs.
    pub free: Extent,
    /// The runs of pages reused at the last generations: (generation, first page, pages). It names
    /// every run reused after generation `reused_from`.
    pub reused: Vec<(u64, u64, u64)>,
    pub reused_from: u64,
    /// Where each sequence stands that has handed out a value since the DDL made or restarted it.
    pub sequences: Extent,
    /// The PostgreSQL release the file's OIDs came from.
    pub release: u8,
    /// Each money column's `lc_monetary`: its table, its column and the locale.
    pub monetary: Vec<(String, String, String)>,
    /// The file's own default collation, as a database's, which jsonb's strings sort under; libc
    /// C.UTF-8 where it names none.
    pub collation: String,
    /// The file's `default_text_search_config`; `pg_catalog.simple` where it names none.
    pub text_search: String,
    /// The transactions across snails this root's writes decided, each until a round across them
    /// finds every other snail's root written.
    pub decided: Vec<Decision>,
    /// Where this copy of the root was written before another snail's root decided its
    /// transaction: it is taken only once a round across them finds that root written.
    pub pending: Option<Pending>,
    /// The tag of the host's transaction this root was written for, where its host tags them: it
    /// is taken only where the host says that transaction committed, or where a root of the next
    /// generation lies beside it; a round with no host to ask reads the copy beside it.
    pub host: Option<u64>,
    /// The source the snail is a replica of, where it is marked one: every writer but the applier
    /// presenting that source is refused while the mark stands.
    pub replica: Option<String>,
    /// The serializable transactions' records: each committed one's marks and edges, kept while a
    /// round older than its commit runs.
    pub serial: Extent,
    /// Where the last of them stands on the clock the records of snails opened together share, so that
    /// commits have one order: 0 where none has been.
    pub clock: u64,
    /// The next OID a writer of the file takes, where it is past where the catalog's text goes on:
    /// a transaction's taken and not yet committed, or one's rolled back; 0 where the text's own is
    /// next.
    pub next_oid: u32,
    /// The statements of the catalog's text whose OIDs depart from its order, each by its place and
    /// its first OID (`oids::Record`); none where every statement's follow the text.
    pub oids: Extent,
    /// Where what grows with the schema lies once the root would not hold it.
    pub(crate) cut: Option<Cut>,
}

/// What a root cut into pages once it would not hold it: the tables' orders and money
/// columns, a list whose top the root holds; the catalog's text, a list whose top page `catalog`
/// names; and the sequences, a list whose top page `sequences` names. A root once cut stays cut.
#[derive(Clone, Debug, Default)]
pub(crate) struct Cut {
    pub(crate) tables: crate::listing::Shape,
    /// The generation that wrote the catalog's top page; none where `catalog` holds the text itself,
    /// as a declaration stages it.
    pub(crate) catalog_at: Option<u64>,
    /// The generation that wrote the sequences' top page.
    pub(crate) sequences_at: u64,
}

/// The count of orders a root names that says it is cut.
const CUT: u32 = u32::MAX;

/// The runs of reused pages the root names, at most.
pub(crate) const REUSED_KEPT: usize = 64;

/// What a commit adds to its root once its pages are written: its host's tag, and a pending mark
/// naming the table that decides a transaction across snails, by PostgreSQL's two names of 63 bytes
/// at most.
const MARGIN: usize = 9 + 4 + 2 * 63 + 1 + 8;

/// The tables' list: an order's top under its table, and a money column's lc_monetary.
const ORDER: u8 = 1;
const MONEY: u8 = 2;

fn listed_key(table: &str, kind: u8, name: &str) -> Vec<u8> {
    let mut k = Vec::with_capacity(table.len() + 2 + name.len());
    k.extend_from_slice(table.as_bytes());
    k.push(0);
    k.push(kind);
    k.extend_from_slice(name.as_bytes());
    k
}

/// The default collation a file names where it names none: libc C.UTF-8, which orders text by its
/// bytes.
pub const COLLATION: &str = "C.UTF-8";

impl Root {
    /// The file's `default_text_search_config`: `pg_catalog.simple` where it names none.
    pub fn text_search(&self) -> &str {
        if self.text_search.is_empty() {
            TEXT_SEARCH
        } else {
            &self.text_search
        }
    }

    /// Whether what grows with the schema lies in pages the root names, the root no longer holding
    /// it.
    pub fn is_cut(&self) -> bool {
        self.cut.is_some()
    }

    /// The file's own default collation.
    pub fn collation(&self) -> &str {
        if self.collation.is_empty() {
            COLLATION
        } else {
            &self.collation
        }
    }
}

/// A transaction across snails, as the root that decided it holds it: the generation that root
/// made, the root it wrote into each other snail, and the pages that carry those roots' bytes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Decision {
    pub at: u64,
    pub roots: Vec<Written>,
    pub carried: Extent,
}

/// The bytes of each root a decision carries, in the order it names them.
pub(crate) fn encode_carried(roots: &[Vec<u8>]) -> Vec<u8> {
    let mut o = Out::default();
    o.u32(roots.len() as u32);
    for r in roots {
        o.bytes(r);
    }
    o.0
}

pub(crate) fn decode_carried(buf: &[u8]) -> Result<Vec<Vec<u8>>> {
    let mut i = In::new(buf);
    (0..i.u32()?).map(|_| i.bytes()).collect()
}

/// One snail's root a transaction across snails wrote: a table the snail holds, which names it
/// among the snails opened together, and the root's turn and checksum.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Written {
    pub holds: String,
    pub turn: u64,
    pub crc: u32,
}

/// The root that decides a pending copy's transaction: a table its snail holds, and the
/// generation it makes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Pending {
    pub decider: String,
    pub at: u64,
}

/// The `lc_monetary` money is written under where nothing names another.
pub const MONETARY: &str = "C";

/// The `default_text_search_config` a file reads under where nothing names another.
pub const TEXT_SEARCH: &str = "pg_catalog.simple";

/// Each column of a catalog holding money, with its `lc_monetary`: the one `had` records for it, or
/// for a column it does not, the file's: the one `had` records for a column the catalog holds, C
/// where it records none.
pub fn monetary_of(
    catalog: &crate::catalog::Catalog,
    had: &[(String, String, String)],
) -> Vec<(String, String, String)> {
    let held = |t: &str, c: &str| {
        catalog
            .table(t)
            .is_some_and(|t| t.columns.iter().any(|x| x.name == c && x.ty.holds_money()))
    };
    let file = had
        .iter()
        .find(|(t, c, _)| held(t, c))
        .map_or(MONETARY, |m| m.2.as_str());
    let mut out = Vec::new();
    for t in &catalog.tables {
        for c in t.columns.iter().filter(|c| c.ty.holds_money()) {
            let locale = had
                .iter()
                .find(|(a, b, _)| *a == t.name && *b == c.name)
                .map_or(file.to_string(), |m| m.2.clone());
            out.push((t.name.clone(), c.name.clone(), locale));
        }
    }
    out
}

/// A run of pages in the free map: free where its tag is 0, else freed once no round older than its
/// tag, the generation that stopped naming it, runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Run {
    pub start: u64,
    pub pages: u64,
    pub tag: u64,
}

pub fn encode_runs(runs: &[Run]) -> Vec<u8> {
    let mut o = Out::default();
    o.u32(runs.len() as u32);
    for r in runs {
        o.u64(r.start);
        o.u64(r.pages);
        o.u64(r.tag);
    }
    o.0
}

pub fn decode_runs(buf: &[u8]) -> Result<Vec<Run>> {
    let mut i = In::new(buf);
    (0..i.u32()?)
        .map(|_| {
            Ok(Run {
                start: i.u64()?,
                pages: i.u64()?,
                tag: i.u64()?,
            })
        })
        .collect()
}

/// Where a sequence stands: its name, its last value, and whether it was handed out.
pub type SequenceState = (String, i64, bool);

pub fn encode_sequences(states: &[SequenceState]) -> Vec<u8> {
    let mut o = Out::default();
    o.u32(states.len() as u32);
    for (name, last, called) in states {
        o.str(name);
        o.i64(*last);
        o.u8(*called as u8);
    }
    o.0
}

pub fn decode_sequences(buf: &[u8]) -> Result<Vec<SequenceState>> {
    let mut i = In::new(buf);
    (0..i.u32()?)
        .map(|_| Ok((i.str()?, i.i64()?, i.u8()? != 0)))
        .collect()
}

/// The root's record of the statements whose OIDs depart from the text's order: each one's place
/// and first OID.
pub(crate) fn encode_record(record: &crate::oids::Record) -> Vec<u8> {
    let mut o = Out::default();
    o.u32(record.len() as u32);
    for (place, first) in record {
        o.u32(*place);
        o.u32(*first);
    }
    o.0
}

pub(crate) fn decode_record(buf: &[u8]) -> Result<Vec<(u32, u32)>> {
    let mut i = In::new(buf);
    (0..i.u32()?).map(|_| Ok((i.u32()?, i.u32()?))).collect()
}

/// The free map: its runs, then each page of packed page maps some of whose maps a commit has
/// stopped naming, with how many.
pub(crate) fn encode_free(runs: &[Run], spent: &[(u64, u32)]) -> Vec<u8> {
    let mut bytes = encode_runs(runs);
    let mut o = Out::default();
    o.u32(spent.len() as u32);
    for &(page, n) in spent {
        o.u64(page);
        o.u32(n);
    }
    bytes.extend_from_slice(&o.0);
    bytes
}

/// Each page of packed page maps some of whose maps a commit has stopped naming, with how many.
pub(crate) type Spent = Vec<(u64, u32)>;

pub(crate) fn decode_free(buf: &[u8]) -> Result<(Vec<Run>, Spent)> {
    let runs = decode_runs(buf)?;
    let mut i = In::new(&buf[4 + 24 * runs.len()..]);
    let spent = (0..i.u32()?)
        .map(|_| Ok((i.u64()?, i.u32()?)))
        .collect::<Result<Vec<_>>>()?;
    Ok((runs, spent))
}

impl Root {
    pub(crate) fn encode(&self) -> Result<Vec<u8>> {
        if let Some(cut) = &self.cut {
            if cut.tables.read() && cut.tables.entries() != self.listed()? {
                return Err(Error::new(
                    "the root's tables changed since their list was written: settle it first",
                ));
            }
        }
        let bytes = self.encoded(false, None)?;
        if bytes.len() > PAGE {
            return Err(Error::new("the root does not fit one page"));
        }
        Ok(bytes)
    }

    /// Whether a root not cut holds what grows with the schema, with its reused runs at their most
    /// and room for what its commit adds after its pages.
    pub(crate) fn fits_uncut(&self) -> Result<bool> {
        Ok(self.encoded(true, None)?.len() + MARGIN <= PAGE)
    }

    /// The bytes a cut root has for the top of its tables' list, its reused runs at their most and
    /// room left for what its commit adds after its pages.
    pub(crate) fn top_room(&self) -> Result<usize> {
        let empty = crate::listing::Node::default();
        let rest = self.encoded(true, Some(&empty))?.len() - empty.bytes();
        Ok(PAGE.saturating_sub(rest + MARGIN))
    }

    /// The entries of the tables' list: each order's top and each money column's lc_monetary, under
    /// its table, in their keys' order.
    pub(crate) fn listed(&self) -> Result<Vec<crate::listing::Item>> {
        let mut out = Vec::with_capacity(self.orders.len() + self.monetary.len());
        for (t, o, e) in &self.orders {
            let mut v = Out::default();
            v.extent(*e);
            out.push((listed_key(t, ORDER, o), v.0));
        }
        for (t, c, l) in &self.monetary {
            out.push((listed_key(t, MONEY, c), l.as_bytes().to_vec()));
        }
        out.sort();
        if let Some(w) = out.windows(2).find(|w| w[0].0 == w[1].0) {
            return Err(Error::new(format!(
                "the root names {} twice",
                String::from_utf8_lossy(&w[0].0).replace(['\0', '\u{1}', '\u{2}'], " ")
            )));
        }
        Ok(out)
    }

    /// The orders and money columns the tables' list holds.
    fn unlist(&mut self, entries: &[crate::listing::Item]) -> Result<()> {
        let bad = || Error::new("the root's list of tables holds an entry of no table");
        let text = |b: &[u8]| String::from_utf8(b.to_vec()).map_err(|_| bad());
        self.orders.clear();
        self.monetary.clear();
        for (k, v) in entries {
            let at = k.iter().position(|&b| b == 0).ok_or_else(bad)?;
            let (table, name) = (text(&k[..at])?, text(k.get(at + 2..).ok_or_else(bad)?)?);
            match k.get(at + 1) {
                Some(&ORDER) => {
                    let mut i = In::new(v);
                    self.orders.push((table, name, i.extent()?));
                }
                Some(&MONEY) => self.monetary.push((table, name, text(v)?)),
                _ => return Err(bad()),
            }
        }
        Ok(())
    }

    /// The catalog as the root names it: its extent, and the generation that wrote it where it is
    /// a list.
    pub(crate) fn catalog_named(&self) -> (Extent, u64) {
        (
            self.catalog,
            self.cut.as_ref().and_then(|c| c.catalog_at).unwrap_or(0),
        )
    }

    /// The root's bytes: `full` with its reused runs at their most, and with `top` in place of its
    /// tables' list's top where it is cut.
    fn encoded(&self, full: bool, top: Option<&crate::listing::Node>) -> Result<Vec<u8>> {
        let mut o = Out::default();
        o.0.extend_from_slice(&MAGIC);
        o.u32(FORMAT);
        o.u64(self.generation);
        o.u64(self.turn);
        o.u64(self.pages);
        o.u64(self.span);
        o.extent(self.catalog);
        match &self.cut {
            None => {
                o.u32(self.orders.len() as u32);
                for (t, n, e) in &self.orders {
                    o.str(t);
                    o.str(n);
                    o.extent(*e);
                }
            }
            Some(cut) => {
                o.u32(CUT);
                o.u64(cut.catalog_at.unwrap_or(0));
                o.u64(cut.sequences_at);
                top.unwrap_or(&cut.tables.top).encode(&mut o);
            }
        }
        o.extent(self.free);
        o.u64(self.reused_from);
        let reused = if full {
            self.reused.len().max(REUSED_KEPT)
        } else {
            self.reused.len()
        };
        o.u32(reused as u32);
        for i in 0..reused {
            let (g, s, p) = self.reused.get(i).copied().unwrap_or_default();
            o.u64(g);
            o.u64(s);
            o.u64(p);
        }
        o.extent(self.sequences);
        o.u8(self.release);
        let monetary: &[(String, String, String)] = match self.cut {
            None => &self.monetary,
            Some(_) => &[],
        };
        o.u32(monetary.len() as u32);
        for (t, c, l) in monetary {
            o.str(t);
            o.str(c);
            o.str(l);
        }
        o.str(self.collation());
        o.u32(self.decided.len() as u32);
        for d in &self.decided {
            o.u64(d.at);
            o.extent(d.carried);
            o.u32(d.roots.len() as u32);
            for w in &d.roots {
                o.str(&w.holds);
                o.u64(w.turn);
                o.u32(w.crc);
            }
        }
        // Whether a transaction across snails is pending, and whether the serializable
        // transactions' records follow.
        let searched = self.text_search() != TEXT_SEARCH;
        let records = u8::from(self.serial.bytes > 0) * 2
            + u8::from(self.clock > 0) * 4
            + u8::from(searched) * 8
            + u8::from(self.next_oid > 0) * 16
            + u8::from(self.oids.bytes > 0) * 32
            + u8::from(self.takes > 0) * 64;
        match &self.pending {
            None => o.u8(records),
            Some(p) => {
                o.u8(1 + records);
                o.str(&p.decider);
                o.u64(p.at);
            }
        }
        if self.serial.bytes > 0 {
            o.extent(self.serial);
        }
        if self.clock > 0 {
            o.u64(self.clock);
        }
        if searched {
            o.str(self.text_search());
        }
        if self.next_oid > 0 {
            o.u32(self.next_oid);
        }
        if self.oids.bytes > 0 {
            o.extent(self.oids);
        }
        if self.takes > 0 {
            o.u64(self.takes);
            o.extent(self.owed);
        }
        // A root marked a replica carries its source, then whether its host tagged it.
        match &self.replica {
            None => {
                if let Some(tag) = self.host {
                    o.u64(tag);
                }
            }
            Some(source) => {
                o.str(source);
                match self.host {
                    None => o.u8(0),
                    Some(tag) => {
                        o.u8(1);
                        o.u64(tag);
                    }
                }
            }
        }
        let crc = crc32fast::hash(&o.0);
        o.u32(crc);
        Ok(o.0)
    }

    /// A copy of the root, where it is whole and of the format this build reads.
    pub(crate) fn decode(page: &[u8]) -> Option<Root> {
        let mut i = In::new(page);
        if i.take(8).ok()? != MAGIC || i.u32().ok()? != FORMAT {
            return None;
        }
        let generation = i.u64().ok()?;
        let turn = i.u64().ok()?;
        let pages = i.u64().ok()?;
        let span = i.u64().ok()?;
        let catalog = i.extent().ok()?;
        let n = i.u32().ok()?;
        let mut orders = Vec::new();
        let mut cut = None;
        if n == CUT {
            let catalog_at = i.u64().ok()?;
            let sequences_at = i.u64().ok()?;
            let top = crate::listing::Node::decode(&mut i).ok()?;
            cut = Some(Cut {
                tables: crate::listing::Shape::of(top),
                catalog_at: (catalog_at > 0).then_some(catalog_at),
                sequences_at,
            });
        } else {
            orders.reserve((n as usize).min(256));
            for _ in 0..n {
                orders.push((i.str().ok()?, i.str().ok()?, i.extent().ok()?));
            }
        }
        let free = i.extent().ok()?;
        let reused_from = i.u64().ok()?;
        let m = i.u32().ok()? as usize;
        let mut reused = Vec::with_capacity(m.min(256));
        for _ in 0..m {
            reused.push((i.u64().ok()?, i.u64().ok()?, i.u64().ok()?));
        }
        let sequences = i.extent().ok()?;
        let release = i.u8().ok()?;
        let n = i.u32().ok()? as usize;
        let mut monetary = Vec::with_capacity(n.min(256));
        for _ in 0..n {
            monetary.push((i.str().ok()?, i.str().ok()?, i.str().ok()?));
        }
        // A root written before roots named a default collation ends here, its CRC next; one
        // written before they held decisions across snails ends after its collation.
        let ended_at = |at: usize| {
            page.get(at..at + 4).is_some_and(|c| {
                u32::from_le_bytes(c.try_into().unwrap()) == crc32fast::hash(&page[..at])
            })
        };
        let ended = |i: &In| ended_at(i.at());
        let collation = if !ended(&i) {
            i.str().ok()?
        } else {
            String::new()
        };
        let ((decided, pending, serial, clock, text_search), (next_oid, oids, takes, owed)) = if !ended(&i) {
            let n = i.u32().ok()? as usize;
            let mut decided = Vec::with_capacity(n.min(256));
            for _ in 0..n {
                let at = i.u64().ok()?;
                let carried = i.extent().ok()?;
                let m = i.u32().ok()? as usize;
                let mut roots = Vec::with_capacity(m.min(256));
                for _ in 0..m {
                    roots.push(Written {
                        holds: i.str().ok()?,
                        turn: i.u64().ok()?,
                        crc: i.u32().ok()?,
                    });
                }
                decided.push(Decision { at, roots, carried });
            }
            let marks = i.u8().ok()?;
            let pending = match marks & 1 {
                0 => None,
                _ => Some(Pending {
                    decider: i.str().ok()?,
                    at: i.u64().ok()?,
                }),
            };
            let serial = match marks & 2 {
                0 => Extent::default(),
                _ => i.extent().ok()?,
            };
            let clock = match marks & 4 {
                0 => 0,
                _ => i.u64().ok()?,
            };
            let text_search = match marks & 8 {
                0 => String::new(),
                _ => i.str().ok()?,
            };
            let next_oid = match marks & 16 {
                0 => 0,
                _ => i.u32().ok()?,
            };
            let oids = match marks & 32 {
                0 => Extent::default(),
                _ => i.extent().ok()?,
            };
            let (takes, owed) = match marks & 64 {
                0 => (0, Extent::default()),
                _ => (i.u64().ok()?, i.extent().ok()?),
            };
            (
                (decided, pending, serial, clock, text_search),
                (next_oid, oids, takes, owed),
            )
        } else {
            (
                (Vec::new(), None, Extent::default(), 0, String::new()),
                (0, Extent::default(), 0, Extent::default()),
            )
        };
        // A root its host tagged carries the tag last; one marked a replica carries its source,
        // then whether its host tagged it.
        let (replica, host) = if ended(&i) {
            (None, None)
        } else if ended_at(i.at() + 8) {
            (None, Some(i.u64().ok()?))
        } else {
            let source = i.str().ok()?;
            let host = match i.u8().ok()? {
                0 => None,
                _ => Some(i.u64().ok()?),
            };
            (Some(source), host)
        };
        let end = i.at();
        let crc = i.u32().ok()?;
        if crc32fast::hash(&page[..end]) != crc {
            return None;
        }
        let mut root = Root {
            generation,
            turn,
            takes,
            owed,
            pages,
            span,
            catalog,
            orders,
            free,
            reused,
            reused_from,
            sequences,
            release,
            monetary,
            collation,
            text_search,
            decided,
            pending,
            host,
            replica,
            serial,
            clock,
            next_oid,
            oids,
            cut,
        };
        // A list of tables its top holds whole is read with the root.
        if let Some(entries) = root
            .cut
            .as_ref()
            .filter(|c| c.tables.read())
            .map(|c| c.tables.entries())
        {
            root.unlist(&entries).ok()?;
        }
        Some(root)
    }

    /// The checksum a root's encoding ends with, as the root it stands for once taken: its pending
    /// mark left out.
    pub(crate) fn crc(&self) -> Result<u32> {
        let mut taken = self.clone();
        taken.pending = None;
        let bytes = taken.encode()?;
        Ok(u32::from_le_bytes(
            bytes[bytes.len() - 4..].try_into().unwrap(),
        ))
    }
}

/// The pages that hold the root's two copies, read afresh by every round and never held.
const ROOT_PAGES: u64 = 2;

/// The first number of a page staged in memory, far past any page a file holds.
pub const STAGED: u64 = 1 << 48;

/// Where pages are written: the file, or the memory a statement stages its pages in until it commits.
pub trait Place {
    /// Reserves `pages` pages in a run, and says where the run begins.
    fn reserve(&mut self, pages: u64) -> u64;
    /// Writes bytes at a page already reserved, padded to whole pages.
    fn write_at(&mut self, start: u64, bytes: &[u8]) -> Result<()>;

    /// Whether a run of `pages` reserved next lies in the file that holds `page`.
    fn lies_with(&self, page: u64, pages: u64) -> bool {
        let _ = (page, pages);
        true
    }

    /// Writes bytes at the next free page, padded to whole pages.
    fn place(&mut self, bytes: &[u8]) -> Result<Extent> {
        let start = self.reserve((bytes.len().div_ceil(PAGE) as u64).max(1));
        self.write_at(start, bytes)?;
        Ok(Extent {
            start,
            bytes: bytes.len() as u64,
        })
    }
}

/// The pages a statement, or a transaction, has made and not yet committed.
#[derive(Default)]
pub struct Staged {
    pages: BTreeMap<u64, Vec<u8>>,
    next: u64,
}

/// A place that stages pages in a reader's memory.
pub struct Stage(Rc<RefCell<Staged>>);

impl Place for Stage {
    fn reserve(&mut self, pages: u64) -> u64 {
        let mut s = self.0.borrow_mut();
        let at = STAGED + s.next;
        s.next += pages;
        at
    }

    fn write_at(&mut self, start: u64, bytes: &[u8]) -> Result<()> {
        let mut s = self.0.borrow_mut();
        for (i, chunk) in bytes.chunks(PAGE).enumerate() {
            let mut page = chunk.to_vec();
            page.resize(PAGE, 0);
            s.pages.insert(start + i as u64, page);
        }
        if bytes.is_empty() {
            s.pages.insert(start, vec![0u8; PAGE]);
        }
        Ok(())
    }
}

/// The files a snail's pages lie in: the first, then `<path>.1`, `.2` and on, each holding the span's
/// pages. Until the span is known from the root, every page is the first file's.
pub(crate) struct Files {
    path: PathBuf,
    pub(crate) first: File,
    rest: RefCell<Vec<Option<File>>>,
    pub(crate) span: Cell<u64>,
    write: bool,
    /// The files written since they last reached the disk.
    dirty: RefCell<BTreeSet<u64>>,
}

impl Files {
    pub(crate) fn open(path: &Path, write: bool, create: bool) -> Result<Files> {
        let first = OpenOptions::new()
            .read(true)
            .write(write)
            .create_new(create)
            .open(path)
            .map_err(|e| Error::new(format!("{}: {e}", path.display())))?;
        Ok(Files {
            path: path.to_path_buf(),
            first,
            rest: RefCell::new(Vec::new()),
            span: Cell::new(0),
            write,
            dirty: RefCell::new(BTreeSet::new()),
        })
    }

    /// The path of the `n`th file.
    fn path_of(&self, n: u64) -> PathBuf {
        path_of(&self.path, n)
    }

    fn with<T>(&self, n: u64, f: impl FnOnce(&File) -> std::io::Result<T>) -> Result<T> {
        if n == 0 {
            return Ok(f(&self.first)?);
        }
        let mut rest = self.rest.borrow_mut();
        let i = (n - 1) as usize;
        if rest.len() <= i {
            rest.resize_with(i + 1, || None);
        }
        if rest[i].is_none() {
            let p = self.path_of(n);
            rest[i] = Some(
                OpenOptions::new()
                    .read(true)
                    .write(self.write)
                    .create(self.write)
                    .truncate(false)
                    .open(&p)
                    .map_err(|e| Error::new(format!("{}: {e}", p.display())))?,
            );
        }
        Ok(f(rest[i].as_ref().unwrap())?)
    }

    /// The runs of `pages` pages from `page` that each lie in one file: (file, its first page in the
    /// file, pages, the run's first page counted from `page`).
    fn runs(&self, page: u64, pages: u64) -> Vec<(u64, u64, u64, u64)> {
        let span = self.span.get();
        let mut out = Vec::new();
        let mut done = 0;
        while done < pages {
            let p = page + done;
            let (n, at, room) = match p.checked_div(span) {
                None => (0, p, pages - done),
                Some(n) => (n, p % span, span - p % span),
            };
            let k = room.min(pages - done);
            out.push((n, at, k, done));
            done += k;
        }
        out
    }

    pub(crate) fn read_at(&self, buf: &mut [u8], page: u64) -> Result<()> {
        for (n, at, k, from) in self.runs(page, (buf.len() / PAGE) as u64) {
            let b = &mut buf[from as usize * PAGE..(from + k) as usize * PAGE];
            self.with(n, |f| f.read_exact_at(b, at * PAGE as u64))?;
        }
        Ok(())
    }

    pub(crate) fn write_at(&self, buf: &[u8], page: u64) -> Result<()> {
        for (n, at, k, from) in self.runs(page, (buf.len() / PAGE) as u64) {
            let b = &buf[from as usize * PAGE..(from + k) as usize * PAGE];
            self.with(n, |f| f.write_all_at(b, at * PAGE as u64))?;
            self.dirty.borrow_mut().insert(n);
        }
        Ok(())
    }

    /// Every file written reaches the disk.
    pub(crate) fn sync(&self) -> Result<()> {
        for n in std::mem::take(&mut *self.dirty.borrow_mut()) {
            self.with(n, |f| f.sync_data())?;
        }
        Ok(())
    }
}

/// A snail's file by what it is, whatever path names it: its device and inode.
pub type Identity = (u64, u64);

/// The file `file` is, by its device and inode.
pub fn identity(file: &File) -> std::io::Result<Identity> {
    file.metadata().map(|m| identity_of(&m))
}

/// The file at `path`, by its device and inode.
pub fn identity_at(path: &Path) -> std::io::Result<Identity> {
    std::fs::metadata(path).map(|m| identity_of(&m))
}

fn identity_of(m: &std::fs::Metadata) -> Identity {
    use std::os::unix::fs::MetadataExt;
    (m.dev(), m.ino())
}

/// The path of a snail's `n`th file, the first being the path itself.
pub(crate) fn path_of(path: &Path, n: u64) -> PathBuf {
    let mut p = path.to_path_buf().into_os_string();
    p.push(format!(".{n}"));
    PathBuf::from(p)
}

/// Reads whole pages, one `pread` a run, and counts every page it reads from the file. Built
/// `standalone`, it also holds the pages it reads between rounds, and counts the ones it finds held.
pub struct Reader {
    files: Files,
    pages: Cell<u64>,
    /// The turn of the root last read, which the reader's round holds.
    turn: Cell<u64>,
    round: Cell<Option<u64>>,
    staged: Rc<RefCell<Staged>>,
    /// The pending copy of the root the reader last found beside it, written before another
    /// snail's root decided its transaction.
    spare: RefCell<Option<Root>>,
    /// Which of the root's two copies the root last read lies in.
    copy: Cell<u64>,
    /// What decides the roots a host tagged, where the reader has one to ask; and the tag of the
    /// copy beside the root last read that it did not take, a root of the generation after it.
    judge: RefCell<Option<Rc<dyn crate::carry::Judge>>>,
    waiting: Cell<Option<u64>>,
    /// The tag of a transaction whose commit the root last read holds and which the judge's round
    /// does not see.
    unseen: Cell<Option<u64>>,
    #[cfg(feature = "standalone")]
    held: RefCell<Held>,
    hits: Cell<u64>,
    tag: Cell<u32>,
    /// The room of the reader's rounds, which keeps the pages a round has read for its later reads,
    /// and the reader's name there.
    room: RefCell<Rc<crate::room::Room>>,
    id: u64,
    /// The pages of the lists a cut root names that the reader read last: the tables', the
    /// catalog's and the sequences'.
    listed: RefCell<[crate::listing::Read; 3]>,
}

/// The lists a cut root names, as a reader keeps the pages it read of each.
const TABLES: usize = 0;
const CATALOG: usize = 1;
const SEQUENCES: usize = 2;

static READERS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The tag of a page read under none.
const NO_TAG: u32 = u32::MAX;

impl Reader {
    /// The file the reader was opened on.
    pub(crate) fn path(&self) -> &Path {
        &self.files.path
    }

    /// A reader that holds no page between rounds.
    pub fn open(path: &Path) -> Result<Reader> {
        Ok(Reader {
            files: Files::open(path, false, false)?,
            pages: Cell::new(0),
            turn: Cell::new(0),
            round: Cell::new(None),
            staged: Default::default(),
            spare: RefCell::new(None),
            copy: Cell::new(0),
            judge: RefCell::new(None),
            waiting: Cell::new(None),
            unseen: Cell::new(None),
            #[cfg(feature = "standalone")]
            held: RefCell::new(Held::new(0)),
            hits: Cell::new(0),
            tag: Cell::new(NO_TAG),
            room: RefCell::new(crate::room::Room::new(Default::default())),
            id: READERS.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            listed: Default::default(),
        })
    }

    /// A reader that holds up to `pages` pages between rounds.
    #[cfg(feature = "standalone")]
    pub fn holding(path: &Path, pages: usize) -> Result<Reader> {
        let r = Reader::open(path)?;
        *r.held.borrow_mut() = Held::new(pages);
        r.held.borrow_mut().begin_round();
        Ok(r)
    }

    /// The pages read from the file.
    pub fn pages_read(&self) -> u64 {
        self.pages.get()
    }

    /// The pages found held, rather than read from the file: held between rounds, or kept by the
    /// round for its later reads.
    pub fn pages_held(&self) -> u64 {
        self.hits.get()
    }

    /// The room of the reader's rounds.
    pub fn room(&self) -> Rc<crate::room::Room> {
        self.room.borrow().clone()
    }

    /// Holds the reader's rounds in `room`, which several readers may share.
    pub fn hold_rounds_in(&self, room: Rc<crate::room::Room>) {
        *self.room.borrow_mut() = room;
    }

    /// The room of the round under way, where it is the reader's.
    fn round_room(&self) -> Option<Rc<crate::room::Room>> {
        crate::room::current().filter(|r| Rc::ptr_eq(r, &self.room.borrow()))
    }

    /// A place that stages pages in this reader's memory, where its reads find them.
    pub fn stage(&self) -> Stage {
        Stage(self.staged.clone())
    }

    /// Drops every page staged.
    pub fn clear_staged(&self) {
        *self.staged.borrow_mut() = Staged::default();
    }

    /// Where the pages staged so far end, for `staged_back_to`.
    pub(crate) fn staged_mark(&self) -> u64 {
        self.staged.borrow().next
    }

    /// Drops the pages staged since `mark`, numbering the pages staged after them afresh, as a
    /// statement run again stages them.
    pub(crate) fn staged_back_to(&self, mark: u64) {
        let mut s = self.staged.borrow_mut();
        s.pages.split_off(&(STAGED + mark));
        s.next = mark;
    }

    /// Drops the pages staged since `mark`, as a savepoint rolled back drops them: the pages staged
    /// after them never take their numbers.
    pub(crate) fn staged_drop_since(&self, mark: u64) {
        self.staged.borrow_mut().pages.split_off(&(STAGED + mark));
    }

    pub fn read(&self, start: u64, pages: u64) -> Result<Vec<u8>> {
        crate::read::settings::in_time()?;
        let mut buf = vec![0u8; pages as usize * PAGE];
        if start >= STAGED {
            // Pages staged in memory: read from no file, so counted as found held.
            let staged = self.staged.borrow();
            for i in 0..pages {
                let page = staged
                    .pages
                    .get(&(start + i))
                    .ok_or_else(|| Error::new("a page staged is missing"))?;
                buf[i as usize * PAGE..(i as usize + 1) * PAGE].copy_from_slice(page);
            }
            self.hits.set(self.hits.get() + pages);
            return Ok(buf);
        }
        // A page is found held between rounds, or kept by the round under way; the rest are read,
        // and kept so where they fit.
        let room = self.round_room();
        #[cfg(feature = "standalone")]
        let mut held = self.held.borrow_mut();
        let mut missing = Vec::new();
        for i in 0..pages as usize {
            let n = start + i as u64;
            let page = &mut buf[i * PAGE..(i + 1) * PAGE];
            #[cfg(feature = "standalone")]
            if let Some(p) = held.touch(n) {
                page.copy_from_slice(p);
                self.hits.set(self.hits.get() + 1);
                continue;
            }
            match room.as_ref().is_some_and(|r| r.page(self.id, n, page)) {
                true => self.hits.set(self.hits.get() + 1),
                false => missing.push(i),
            }
        }
        let mut k = 0;
        while k < missing.len() {
            let mut z = k + 1;
            while z < missing.len() && missing[z] == missing[z - 1] + 1 {
                z += 1;
            }
            let (a, b) = (missing[k], missing[z - 1] + 1);
            self.files
                .read_at(&mut buf[a * PAGE..b * PAGE], start + a as u64)?;
            self.pages.set(self.pages.get() + (b - a) as u64);
            for i in a..b {
                let n = start + i as u64;
                if n < ROOT_PAGES {
                    continue;
                }
                let page = &buf[i * PAGE..(i + 1) * PAGE];
                #[cfg(feature = "standalone")]
                {
                    held.keep(n, page, self.tag.get());
                    if held.holds(n) {
                        continue;
                    }
                }
                if let Some(r) = &room {
                    r.keep_page(self.id, n, page, self.tag.get());
                }
            }
            k = z;
        }
        Ok(buf)
    }

    /// Tags the pages read from now on, until tagged otherwise; `None` leaves them untagged.
    pub fn tag_reads(&self, tag: Option<u32>) {
        self.tag.set(tag.unwrap_or(NO_TAG));
    }

    /// The pages held that were read under `tag`: between rounds, and kept by the round.
    pub fn held_under(&self, tag: u32) -> u64 {
        let kept = self.round_room().map_or(0, |r| r.tagged(self.id, tag));
        #[cfg(feature = "standalone")]
        return self.held.borrow().tagged(tag) + kept;
        #[cfg(not(feature = "standalone"))]
        kept
    }

    /// Of these pages, those held, their use left as it was.
    pub fn held_among(&self, pages: impl Iterator<Item = u64>) -> u64 {
        let room = self.round_room();
        let kept = |n: u64| room.as_ref().is_some_and(|r| r.keeps(self.id, n));
        #[cfg(feature = "standalone")]
        let held = self.held.borrow();
        #[cfg(feature = "standalone")]
        let between = |n: u64| held.holds(n);
        #[cfg(not(feature = "standalone"))]
        let between = |_: u64| false;
        pages
            .filter(|&n| n >= STAGED || between(n) || kept(n))
            .count() as u64
    }

    /// Of the pages a read will read, those held, which the round now keeps until the read ends.
    pub fn keep_for_round(&self, pages: impl Iterator<Item = u64>) -> u64 {
        let room = self.round_room();
        let kept = |n: u64| {
            room.as_ref()
                .is_some_and(|r| r.pin(self.id, std::iter::once(n)) == 1)
        };
        #[cfg(feature = "standalone")]
        let mut held = self.held.borrow_mut();
        #[cfg(feature = "standalone")]
        let mut between = |n: u64| held.touch(n).is_some();
        #[cfg(not(feature = "standalone"))]
        let between = |_: u64| false;
        pages
            .filter(|&n| n >= STAGED || (n >= ROOT_PAGES && (between(n) || kept(n))))
            .count() as u64
    }

    /// Lets the pages a read kept go with the round's room again, the read done.
    pub fn read_done(&self) {
        if let Some(r) = self.round_room() {
            r.unpin();
        }
    }

    /// Begins a round after the first: the root read afresh. A page is never written twice, so the
    /// pages held stay as they were read.
    pub fn begin_round(&self) -> Result<Root> {
        #[cfg(feature = "standalone")]
        self.held.borrow_mut().begin_round();
        self.root()
    }

    pub fn read_extent(&self, e: Extent) -> Result<Vec<u8>> {
        let mut buf = self.read(e.start, e.pages())?;
        buf.truncate(e.bytes as usize);
        Ok(buf)
    }

    /// The newer of the root's two copies that is whole. The reader's round holds its generation,
    /// so no page it names is freed while the reader reads it; and the pages held that were reused
    /// since the root last read are dropped.
    pub fn root(&self) -> Result<Root> {
        let from = self.round.get().unwrap_or(0);
        crate::lock::hold_generations_from(&self.files.first, from)?;
        let Copies {
            root,
            pending,
            copy,
            waiting,
            unseen,
        } = self.copies()?;
        self.files.span.set(root.span);
        crate::lock::keep_generation(&self.files.first, from, root.turn)?;
        self.unseen.set(unseen);
        self.round.set(Some(root.turn));
        self.forget_reused(&root);
        self.turn.set(root.turn);
        let root = self.expanded(root)?;
        *self.spare.borrow_mut() = pending.map(|p| self.expanded(p)).transpose()?;
        self.copy.set(copy);
        self.waiting.set(waiting);
        Ok(root)
    }

    /// The newest root, read now, the pages held that were reused since the reader's root dropped.
    /// The round holds the generation it held, older, so no page the newest root names is freed while
    /// the reader reads it.
    pub fn peek_root(&self) -> Result<Root> {
        let Copies {
            root,
            pending,
            copy,
            waiting,
            ..
        } = self.copies()?;
        self.files.span.set(root.span);
        self.forget_reused(&root);
        let root = self.expanded(root)?;
        *self.spare.borrow_mut() = pending.map(|p| self.expanded(p)).transpose()?;
        self.copy.set(copy);
        self.waiting.set(waiting);
        Ok(root)
    }

    /// The root with the tables' list it cut into pages read: a page the reader read before, by its
    /// page and the generation that wrote it, taken as it read it.
    pub(crate) fn expanded(&self, mut root: Root) -> Result<Root> {
        let Some(top) = root
            .cut
            .as_ref()
            .filter(|c| !c.tables.read())
            .map(|c| c.tables.top.clone())
        else {
            return Ok(root);
        };
        self.listed.borrow_mut()[TABLES].begin();
        let shape = crate::listing::walk(top, &mut |n, at| self.list_page(TABLES, n, at))?;
        let entries = shape.entries();
        root.cut.as_mut().unwrap().tables = shape;
        root.unlist(&entries)?;
        Ok(root)
    }

    /// A page of one of the lists a cut root names, read where the reader has not read it.
    fn list_page(&self, list: usize, page: u64, at: u64) -> Result<Rc<crate::listing::Node>> {
        if let Some(n) = self.listed.borrow_mut()[list].found(page, at) {
            return Ok(n);
        }
        let buf = self.read(page, 1)?;
        let node = Rc::new(crate::listing::Node::decode(&mut In::new(&buf))?);
        self.listed.borrow_mut()[list].keep(page, at, node.clone());
        Ok(node)
    }

    /// A list whose top page lies at `top`, written at generation `at`.
    fn paged_list(&self, list: usize, top: Extent, at: u64) -> Result<crate::listing::Shape> {
        self.listed.borrow_mut()[list].begin();
        let node = self.list_page(list, top.start, at)?;
        crate::listing::walk(node, &mut |n, a| self.list_page(list, n, a))
    }

    /// The catalog's text, as the root names it.
    pub(crate) fn catalog_text(&self, root: &Root) -> Result<Vec<u8>> {
        match root.cut.as_ref().and_then(|c| c.catalog_at) {
            None => self.read_extent(root.catalog),
            Some(at) => Ok(self
                .paged_list(CATALOG, root.catalog, at)?
                .entries()
                .into_iter()
                .flat_map(|(_, v)| v)
                .collect()),
        }
    }

    /// The statements of the catalog's text whose OIDs depart from its order, as the root records
    /// them.
    pub(crate) fn oid_record(&self, root: &Root) -> Result<Vec<(u32, u32)>> {
        if root.oids.bytes == 0 {
            return Ok(Vec::new());
        }
        decode_record(&self.read_extent(root.oids)?)
    }

    /// The record of the statements whose OIDs depart from the text's order, written over the one
    /// `root` names: in pages of its own, none where it is empty.
    pub(crate) fn put_oid_record(
        &self,
        root: &mut Root,
        record: &crate::oids::Record,
        w: &mut dyn Place,
        gone: &mut Vec<Extent>,
    ) -> Result<()> {
        if root.oids.bytes > 0 {
            gone.push(root.oids);
        }
        root.oids = match record.is_empty() {
            true => Extent::default(),
            false => w.place(&encode_record(record))?,
        };
        Ok(())
    }

    /// Where each sequence stands that has handed out a value, as the root names them.
    pub(crate) fn sequence_states(&self, root: &Root) -> Result<Vec<SequenceState>> {
        if root.sequences.bytes == 0 {
            return Ok(Vec::new());
        }
        let Some(cut) = &root.cut else {
            return decode_sequences(&self.read_extent(root.sequences)?);
        };
        let bad = || Error::new("a sequence's place in the file is not whole");
        self.paged_list(SEQUENCES, root.sequences, cut.sequences_at)?
            .entries()
            .into_iter()
            .map(|(k, v)| {
                let mut i = In::new(&v);
                let state = (
                    String::from_utf8(k).map_err(|_| bad())?,
                    i.i64()?,
                    i.u8()? != 0,
                );
                Ok(state)
            })
            .collect()
    }

    /// The pages of the lists a cut root names: none where it is not cut.
    pub(crate) fn listed_pages(&self, root: &Root) -> Result<Vec<Extent>> {
        let Some(cut) = &root.cut else {
            return Ok(Vec::new());
        };
        let mut out: Vec<Extent> = self
            .expanded(root.clone())?
            .cut
            .unwrap()
            .tables
            .pages()
            .map(crate::listing::Paged::extent)
            .collect();
        let paged = [
            (CATALOG, root.catalog, cut.catalog_at.unwrap_or(0)),
            (SEQUENCES, root.sequences, cut.sequences_at),
        ];
        for (list, top, at) in paged {
            if top.bytes > 0 && at > 0 {
                out.push(top);
                let shape = self.paged_list(list, top, at)?;
                out.extend(shape.pages().map(crate::listing::Paged::extent));
            }
        }
        Ok(out)
    }

    /// The catalog's text written over the one `root` names, at generation `at`: in its own pages
    /// where the root is not cut, else as a list.
    pub(crate) fn put_catalog(
        &self,
        root: &mut Root,
        text: &[u8],
        at: u64,
        w: &mut dyn Place,
        gone: &mut Vec<Extent>,
    ) -> Result<()> {
        let Some(cut) = &root.cut else {
            gone.push(root.catalog);
            root.catalog = w.place(text)?;
            return Ok(());
        };
        let old = match cut.catalog_at {
            Some(a) => Some((root.catalog, a, self.paged_list(CATALOG, root.catalog, a)?)),
            None => {
                if root.catalog.bytes > 0 && root.catalog.start < STAGED {
                    gone.push(root.catalog);
                }
                None
            }
        };
        let (top, a) = crate::listing::settle_paged(
            old.as_ref().map(|(e, a, s)| (*e, *a, s)),
            crate::listing::chunks(text),
            at,
            &mut |b| w.place(b),
            gone,
        )?;
        root.catalog = top;
        root.cut.as_mut().unwrap().catalog_at = Some(a);
        Ok(())
    }

    /// Where each sequence stands, written over the states `root` names, at generation `at`: in
    /// their own pages where the root is not cut, else as a list, none where no sequence has handed
    /// out a value.
    pub(crate) fn put_sequences(
        &self,
        root: &mut Root,
        states: &[SequenceState],
        at: u64,
        w: &mut dyn Place,
        gone: &mut Vec<Extent>,
    ) -> Result<()> {
        let Some(cut) = &root.cut else {
            if root.sequences.bytes > 0 {
                gone.push(root.sequences);
            }
            root.sequences = w.place(&encode_sequences(states))?;
            return Ok(());
        };
        let old = match root.sequences.bytes > 0 && cut.sequences_at > 0 {
            true => Some((
                root.sequences,
                cut.sequences_at,
                self.paged_list(SEQUENCES, root.sequences, cut.sequences_at)?,
            )),
            false => None,
        };
        let (top, a) = crate::listing::settle_paged(
            old.as_ref().map(|(e, a, s)| (*e, *a, s)),
            sequence_entries(states),
            at,
            &mut |b| w.place(b),
            gone,
        )?;
        root.sequences = top;
        root.cut.as_mut().unwrap().sequences_at = a;
        Ok(())
    }

    /// The commit's last step before its free map, at generation `at`: what grows with the schema
    /// cut into pages where the root would hold it no longer, its catalog and sequences moved out
    /// of their own pages; and each page of a cut root's tables' list whose entries changed written
    /// afresh. The pages replaced are added to `gone`.
    pub(crate) fn settle(
        &self,
        root: &mut Root,
        at: u64,
        w: &mut dyn Place,
        gone: &mut Vec<Extent>,
    ) -> Result<()> {
        if root.cut.is_none() {
            if root.fits_uncut()? {
                return Ok(());
            }
            let text = self.read_extent(root.catalog)?;
            let states = self.sequence_states(root)?;
            cut_afresh(root, &text, &states, at, w, gone)?;
        }
        settle_tables(root, at, w, gone)
    }
}

/// A root not cut, cut at generation `at`: its catalog's text and its sequences' states written as
/// lists, the pages that held them added to `gone`.
pub(crate) fn cut_afresh(
    root: &mut Root,
    text: &[u8],
    states: &[SequenceState],
    at: u64,
    w: &mut dyn Place,
    gone: &mut Vec<Extent>,
) -> Result<()> {
    if root.catalog.start > 0 {
        gone.push(root.catalog);
    }
    if root.sequences.bytes > 0 {
        gone.push(root.sequences);
    }
    root.catalog = Extent::default();
    root.sequences = Extent::default();
    root.cut = Some(Cut::default());
    let (top, a) = crate::listing::settle_paged(
        None,
        crate::listing::chunks(text),
        at,
        &mut |b| w.place(b),
        gone,
    )?;
    root.catalog = top;
    root.cut.as_mut().unwrap().catalog_at = Some(a);
    if !states.is_empty() {
        let (top, a) = crate::listing::settle_paged(
            None,
            sequence_entries(states),
            at,
            &mut |b| w.place(b),
            gone,
        )?;
        root.sequences = top;
        root.cut.as_mut().unwrap().sequences_at = a;
    }
    Ok(())
}

/// The sequences' states as the entries of their list, by name.
fn sequence_entries(states: &[SequenceState]) -> Vec<crate::listing::Item> {
    let mut entries: Vec<crate::listing::Item> = states
        .iter()
        .map(|(name, last, called)| {
            let mut v = Out::default();
            v.i64(*last);
            v.u8(*called as u8);
            (name.as_bytes().to_vec(), v.0)
        })
        .collect();
    entries.sort();
    entries.dedup_by(|a, b| a.0 == b.0);
    entries
}

/// Each page of a cut root's tables' list whose entries changed written afresh at generation
/// `at`, levels standing above them until the root holds the top.
pub(crate) fn settle_tables(
    root: &mut Root,
    at: u64,
    w: &mut dyn Place,
    gone: &mut Vec<Extent>,
) -> Result<()> {
    if root.cut.is_none() {
        return Ok(());
    }
    let entries = root.listed()?;
    let room = root.top_room()?;
    let cut = root.cut.as_mut().unwrap();
    cut.tables = crate::listing::settle(
        &cut.tables,
        entries,
        at,
        &|b| b <= room,
        &mut |b| w.place(b),
        gone,
    )?;
    Ok(())
}

impl Reader {
    /// The root's two copies read, and the root taken from them as the reader's judge, where it has
    /// one, decides the roots a host tagged, or the root it hands.
    fn copies(&self) -> Result<Copies> {
        let both = self.read(0, 2)?;
        let judge = self.judge.borrow().clone();
        match judge.as_deref().map(|j| j.root()).transpose()?.flatten() {
            Some(page) => handed(&both, &page),
            None => copies(&both, judge.as_deref()),
        }
    }

    /// Takes the roots a host tagged from now on only where `judge` says the host's transaction
    /// committed, as the round asking sees it.
    pub fn judged_by(&self, judge: Rc<dyn crate::carry::Judge>) {
        *self.judge.borrow_mut() = Some(judge);
    }

    /// Whether the reader has a judge of the roots a host tagged.
    pub(crate) fn judged(&self) -> bool {
        self.judge.borrow().is_some()
    }

    /// The tag of the copy beside the root last read, a root of the generation after it its host
    /// tagged, where the round did not take it.
    pub fn waiting_on(&self) -> Option<u64> {
        self.waiting.get()
    }

    /// The tag of a transaction whose commit the root last read holds and which the judge's round
    /// does not see, where there is one: the generation that round sees is no longer in the file.
    pub fn unseen(&self) -> Option<u64> {
        self.unseen.get()
    }

    /// The copy, 0 or 1, of the root's two that the root last read lies in.
    pub(crate) fn root_copy(&self) -> u64 {
        self.copy.get()
    }

    /// The pending copy of the root found beside the root last read, if there was one.
    pub(crate) fn pending_copy(&self) -> Option<Root> {
        self.spare.borrow().clone()
    }

    /// Reads from now on as of a pending copy of the root, written for the generation after the
    /// one the round holds: the pages held that it reused are dropped.
    pub(crate) fn adopt(&self, root: &Root) {
        self.forget_reused(root);
        self.turn.set(root.turn);
    }

    fn forget_reused(&self, root: &Root) {
        #[cfg(feature = "standalone")]
        {
            let last = self.turn.get();
            let mut held = self.held.borrow_mut();
            if last < root.reused_from {
                held.clear();
            } else {
                for &(g, start, pages) in &root.reused {
                    if g > last {
                        held.forget(start, pages);
                    }
                }
            }
        }
        let last = self.turn.get();
        let room = self.room.borrow();
        if last < root.reused_from {
            room.forget_file(self.id);
        } else {
            for &(g, start, pages) in &root.reused {
                if g > last {
                    room.forget(self.id, start, pages);
                }
            }
        }
    }

    /// Drops the held copies of the pages in `start..start + pages`, which are about to be written
    /// again.
    pub fn forget(&self, start: u64, pages: u64) {
        #[cfg(feature = "standalone")]
        self.held.borrow_mut().forget(start, pages);
        self.room.borrow().forget(self.id, start, pages);
    }

    /// The first file the reader reads, whose locks its round holds.
    pub fn file(&self) -> &File {
        &self.files.first
    }

    /// The format the root names: this build's, the one format it opens.
    pub fn format(&self) -> u32 {
        FORMAT
    }
}

/// The newer of the root's two copies, given as the file's first two pages, that is whole. A file
/// whose root names another format than this build's is refused by that format, one 0.1.x made
/// with the way across. A pending copy is never taken.
pub(crate) fn newer(both: &[u8]) -> Result<Root> {
    copies(both, None).map(|c| c.root)
}

/// The root a round takes from the root's two copies, and what lies beside it.
pub(crate) struct Copies {
    pub(crate) root: Root,
    /// The other copy, where it is a pending one written for the generation after the root.
    pub(crate) pending: Option<Root>,
    /// The copy, 0 or 1, the root lies in.
    pub(crate) copy: u64,
    /// The tag of the other copy, where it is a root of the generation after the root that its
    /// host tagged and the round did not take.
    pub(crate) waiting: Option<u64>,
    /// The tag of a transaction whose commit the root holds and which the judge's round does not
    /// see: the root's own, or its parent's beside it.
    pub(crate) unseen: Option<u64>,
}

/// `newer`, the copy of the two it lies in, and the other copy where it is a pending one, written
/// for the generation after it. Where the only whole copy is a pending one, it is the root: the
/// copy beside a pending copy is written only once the pending copy's transaction is decided, so
/// one torn there was its root's last write. A copy its host tagged is taken where `judge` says
/// its host's transaction committed, or where the other copy is a tagged root of the generation
/// after it, which its host wrote only once that transaction had: else the copy beside it.
pub(crate) fn copies(both: &[u8], judge: Option<&dyn crate::carry::Judge>) -> Result<Copies> {
    let decoded = [Root::decode(&both[..PAGE]), Root::decode(&both[PAGE..])];
    let mut settled = [false; 2];
    for c in 0..2 {
        let Some(r) = &decoded[c] else {
            continue;
        };
        settled[c] = match r.host {
            None | Some(crate::carry::AT_ONCE) => true,
            Some(tag) => {
                decoded[1 - c]
                    .as_ref()
                    .is_some_and(|o| o.host.is_some() && o.turn == r.turn + 1)
                    || match judge {
                        Some(j) => j.taken(tag)?,
                        None => false,
                    }
            }
        };
    }
    let taken = (0..2u64)
        .filter_map(|c| {
            decoded[c as usize]
                .as_ref()
                .filter(|r| r.pending.is_none() && settled[c as usize])
                .map(|d| (c, d))
        })
        .max_by_key(|(c, r)| (r.turn, std::cmp::Reverse(*c)));
    if let Some((copy, root)) = taken {
        let pending = decoded
            .iter()
            .flatten()
            .find(|r| r.pending.is_some() && r.turn == root.turn + 1)
            .cloned();
        let waiting = decoded[1 - copy as usize]
            .as_ref()
            .filter(|r| r.turn == root.turn + 1)
            .and_then(|r| r.host);
        // The root holds its own commit and its parent's: the tag of either the judge's round does
        // not see.
        let mut unseen = None;
        if let Some(j) = judge {
            let parent = decoded[1 - copy as usize]
                .as_ref()
                .filter(|r| r.turn + 1 == root.turn);
            let tags = [Some(root), parent]
                .into_iter()
                .flatten()
                .filter_map(|r| r.host);
            for tag in tags {
                if tag != crate::carry::AT_ONCE && !j.sees(tag)? {
                    unseen = Some(tag);
                    break;
                }
            }
        }
        return Ok(Copies {
            root: root.clone(),
            pending,
            copy,
            waiting,
            unseen,
        });
    }
    if let Some((copy, root)) = (0..2u64).find_map(|c| decoded[c as usize].as_ref().map(|d| (c, d)))
    {
        let mut root = root.clone();
        root.pending = None;
        return Ok(Copies {
            root,
            pending: None,
            copy,
            waiting: None,
            unseen: None,
        });
    }
    for copy in [&both[..PAGE], &both[PAGE..]] {
        if copy[..8] == MAGIC {
            let format = u32::from_le_bytes(copy[8..12].try_into().unwrap());
            if MADE_BY_0_1.contains(&format) {
                return Err(Error::new(format!(
                    "the snail is of format {format}, made by 0.1.x: `out` it with 0.1.x and \
                     `write` it afresh with 0.2.0"
                )));
            }
            if format != FORMAT {
                return Err(Error::new(format!(
                    "the snail is of format {format}, and this warren-snail reads format {FORMAT}"
                )));
            }
        }
    }
    Err(Error::new("neither copy of the root is whole"))
}

/// The root a judge hands a round, in place of its word on each copy: taken where it lies in
/// either copy, and refused by name where it lies in neither.
pub(crate) fn handed(both: &[u8], page: &[u8]) -> Result<Copies> {
    let whole = || Error::new("the root the snail's host hands is no root, whole");
    if page.len() > PAGE {
        return Err(whole());
    }
    let mut page = page.to_vec();
    page.resize(PAGE, 0);
    let root = Root::decode(&page).ok_or_else(whole)?;
    let copy = (0..2usize)
        .find(|&c| both[c * PAGE..(c + 1) * PAGE] == page[..])
        .ok_or_else(|| {
            Error::new(format!(
                "the root of generation {} the snail's host hands lies in neither copy of its root",
                root.turn
            ))
        })?;
    let other = Root::decode(&both[(1 - copy) * PAGE..(2 - copy) * PAGE])
        .filter(|r| r.turn == root.turn + 1);
    Ok(Copies {
        pending: other.clone().filter(|r| r.pending.is_some()),
        waiting: other.and_then(|r| r.host),
        root,
        copy: copy as u64,
        unseen: None,
    })
}

/// Where a commit stops short, for the tests of a crash: before the root, or halfway through it.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Stop {
    BeforeRoot,
    HalfRoot,
}

/// Writes pages into free runs and past the file's last page, and the root last.
pub struct Writer {
    files: Files,
    next: u64,
    written: u64,
    stop: Option<Stop>,
    /// The free runs a commit takes pages from before the file's last, and the runs it took.
    free: Vec<(u64, u64)>,
    taken: Vec<(u64, u64)>,
    /// What carries each commit, where one does, and the pages the commit under way has written. A
    /// host that panics leaves the file at the commit's parent, its root never written.
    host: Option<std::panic::AssertUnwindSafe<Box<dyn crate::carry::Host>>>,
    carried: BTreeMap<u64, Vec<u8>>,
    /// The runs of pages the commit under way has written, its root's copy aside.
    runs: Vec<(u64, u64)>,
}

impl Writer {
    pub fn create(path: &Path) -> Result<Writer> {
        Writer::create_spanned(path, FILE_PAGES)
    }

    /// A new snail whose files each hold `span` pages.
    pub fn create_spanned(path: &Path, span: u64) -> Result<Writer> {
        if span < 2 {
            return Err(Error::new("a file holds at least the root's two pages"));
        }
        // A snail marked a replica is written by its source's applier alone, and never afresh.
        if let Some(source) = Reader::open(path)
            .and_then(|r| r.root())
            .ok()
            .and_then(|r| r.replica)
        {
            return Err(Error::new(format!(
                "{}: the snail there is marked a replica of '{source}', which only its applier \
                 writes; a new snail is written where no file is yet",
                path.display()
            )));
        }
        let files = Files::open(path, true, true)?;
        files.span.set(span);
        files.first.write_all_at(&vec![0u8; 2 * PAGE], 0)?;
        Ok(Writer {
            files,
            next: 2,
            written: 0,
            stop: None,
            free: Vec::new(),
            taken: Vec::new(),
            host: None,
            carried: BTreeMap::new(),
            runs: Vec::new(),
        })
    }

    /// A writer on a snail already written, from the first page its root names none of.
    pub fn open(path: &Path) -> Result<(Writer, Root)> {
        let files = Files::open(path, true, false)?;
        let mut both = vec![0u8; 2 * PAGE];
        files.first.read_exact_at(&mut both, 0)?;
        let root = newer(&both)?;
        files.span.set(root.span);
        Ok((
            Writer {
                files,
                next: root.pages,
                written: 0,
                stop: None,
                free: Vec::new(),
                taken: Vec::new(),
                host: None,
                carried: BTreeMap::new(),
                runs: Vec::new(),
            },
            root,
        ))
    }

    /// The pages written, the roots among them.
    pub fn pages_written(&self) -> u64 {
        self.written
    }

    #[doc(hidden)]
    pub fn stop_at(&mut self, stop: Option<Stop>) {
        self.stop = stop;
    }

    /// The next free page.
    pub fn next(&self) -> u64 {
        self.next
    }

    /// Begins a commit on the newest root: the writer's next page is the first page it names none
    /// of.
    pub fn begin_commit(&mut self, root: &Root) {
        self.next = root.pages;
        self.free.clear();
        self.taken.clear();
        self.carried.clear();
        self.runs.clear();
    }

    /// The runs of pages the commit under way has written, its root's copy aside.
    pub(crate) fn runs_written(&self) -> Vec<(u64, u64)> {
        self.runs.clone()
    }

    /// Pages a take wrote, carried with the commit under way where a host carries it: those it
    /// writes itself stand.
    pub(crate) fn carries_also(&mut self, pages: impl IntoIterator<Item = (u64, Vec<u8>)>) {
        if self.host.is_some() {
            for (n, bytes) in pages {
                self.carried.entry(n).or_insert(bytes);
            }
        }
    }

    /// Whether a host carries the commits.
    pub(crate) fn is_carried(&self) -> bool {
        self.host.is_some()
    }

    /// Carries each commit from now on by the host.
    pub(crate) fn carried_by(&mut self, host: Box<dyn crate::carry::Host>) {
        self.host = Some(std::panic::AssertUnwindSafe(host));
    }

    /// The tag the host gives a transaction's root, where it tags them.
    pub(crate) fn host_tag(&self) -> Option<u64> {
        self.host.as_ref().and_then(|h| h.tag())
    }

    /// The first file, whose locks a writer holds on the blocks it changes and on the root's turn.
    pub fn file(&self) -> &File {
        &self.files.first
    }

    /// The format the snail is written in: this build's, the one format it writes.
    pub fn format(&self) -> u32 {
        FORMAT
    }

    /// Whether `pages` pages from `at` lie in one file. A run longer than a file lies across files.
    fn in_one_file(&self, at: u64, pages: u64) -> bool {
        let span = self.files.span.get();
        span == 0 || pages > span || at / span == (at + pages - 1) / span
    }

    /// The first page of the file after the one `at` lies in.
    fn next_file(&self, at: u64) -> u64 {
        let span = self.files.span.get();
        (at / span + 1) * span
    }

    /// Gives the writer free runs to take pages from, first, before the file's last.
    pub fn use_free(&mut self, runs: Vec<(u64, u64)>) {
        self.free = runs;
    }

    /// The free runs left, and the runs taken from them.
    pub fn left_free(&mut self) -> Vec<(u64, u64)> {
        std::mem::take(&mut self.free)
    }

    pub fn taken(&mut self) -> Vec<(u64, u64)> {
        std::mem::take(&mut self.taken)
    }

    /// Where a run of `pages` lies in the free run `(s, n)`, where it has room: at its start, or at
    /// the first page of the next file where a run from its start would cross into it.
    fn fits(&self, (s, n): (u64, u64), pages: u64) -> Option<u64> {
        let at = if self.in_one_file(s, pages) {
            s
        } else {
            self.next_file(s)
        };
        (at + pages <= s + n).then_some(at)
    }

    /// The page a run of `pages` reserved now would begin on.
    fn lands(&self, pages: u64) -> u64 {
        if let Some(at) = self.free.iter().find_map(|&r| self.fits(r, pages)) {
            return at;
        }
        if self.in_one_file(self.next, pages) {
            self.next
        } else {
            self.next_file(self.next)
        }
    }

    /// Writes bytes past the file's last page, never into a free run.
    pub fn place_end(&mut self, bytes: &[u8]) -> Result<Extent> {
        let start = self.next;
        self.next += (bytes.len().div_ceil(PAGE) as u64).max(1);
        self.write_at(start, bytes)?;
        Ok(Extent {
            start,
            bytes: bytes.len() as u64,
        })
    }
}

impl Place for Writer {
    fn lies_with(&self, page: u64, pages: u64) -> bool {
        let span = self.files.span.get();
        span == 0 || self.lands(pages) / span == page / span
    }

    fn reserve(&mut self, pages: u64) -> u64 {
        // The first free run with room for the pages in one file.
        for i in 0..self.free.len() {
            let (s, n) = self.free[i];
            if let Some(at) = self.fits((s, n), pages) {
                let (before, after) = ((s, at - s), (at + pages, s + n - at - pages));
                self.free.remove(i);
                for r in [after, before] {
                    if r.1 > 0 {
                        self.free.insert(i, r);
                    }
                }
                self.taken.push((at, pages));
                return at;
            }
        }
        // Past the last page: a run that would cross into the next file starts there, and the pages
        // it passes over are free.
        let mut at = self.next;
        if !self.in_one_file(at, pages) {
            let next = self.next_file(at);
            self.free.push((at, next - at));
            at = next;
        }
        self.next = at + pages;
        at
    }

    fn write_at(&mut self, start: u64, bytes: &[u8]) -> Result<()> {
        let mut buf = bytes.to_vec();
        buf.resize(bytes.len().div_ceil(PAGE).max(1) * PAGE, 0);
        self.files.write_at(&buf, start)?;
        self.written += (buf.len() / PAGE) as u64;
        if start >= ROOT_PAGES {
            self.runs.push((start, (buf.len() / PAGE) as u64));
        }
        if self.host.is_some() {
            for (i, page) in buf.chunks(PAGE).enumerate() {
                self.carried.insert(start + i as u64, page.to_vec());
            }
        }
        Ok(())
    }
}

impl Writer {
    /// The commit: every page reaches the disk, then the root, written to the copy its generation
    /// names. Where a host carries the commits, the commit is handed to it once its pages are on
    /// the disk, and its root is written only once the host returns.
    pub fn commit(&mut self, root: &mut Root) -> Result<()> {
        self.commit_into(root, root.turn % 2)
    }

    /// The commit, its root written to `copy`, 0 or 1, of the root's two: the one that does not
    /// hold the root it begins from.
    pub(crate) fn commit_into(&mut self, root: &mut Root, copy: u64) -> Result<()> {
        let bytes = self.prepare(root)?;
        // A take's root is no commit: its pages go to the host with the next commit's.
        if let Some(host) = self.host.as_mut().filter(|_| root.takes == 0) {
            let pages = std::mem::take(&mut self.carried);
            host.carry(&crate::carry::Commit::of(
                root.generation,
                pages,
                &bytes,
                root.span,
            ))?;
            if host.keeps_root() {
                return Ok(());
            }
        }
        match self.stop {
            Some(Stop::BeforeRoot) => return Err(Error::new("the commit stopped before the root")),
            Some(Stop::HalfRoot) => {
                self.files
                    .first
                    .write_all_at(&bytes[..bytes.len() / 2], copy * PAGE as u64)?;
                self.written += 1;
                return Err(Error::new("the commit stopped halfway through the root"));
            }
            None => {}
        }
        self.write_copy(copy, &bytes)
    }

    /// A commit's pages on the disk, and its root's bytes: the commit but its root.
    pub(crate) fn prepare(&mut self, root: &mut Root) -> Result<Vec<u8>> {
        root.pages = self.next;
        root.span = self.files.span.get();
        self.files.sync()?;
        root.encode()
    }

    /// A root's bytes written to `copy`, 0 or 1, of the root's two, and on the disk.
    pub(crate) fn write_copy(&mut self, copy: u64, bytes: &[u8]) -> Result<()> {
        self.write_at(copy, bytes)?;
        self.carried.clear();
        self.files.sync()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_root_names_its_configuration_only_where_it_is_another() {
        let r = Root {
            text_search: "pg_catalog.english".into(),
            ..Default::default()
        };
        let mut bytes = r.encode().unwrap();
        bytes.resize(PAGE, 0);
        let back = Root::decode(&bytes).unwrap();
        assert_eq!(back.text_search(), "pg_catalog.english");
        let plain = Root::default().encode().unwrap();
        let simple = Root {
            text_search: TEXT_SEARCH.into(),
            ..Default::default()
        };
        assert_eq!(simple.encode().unwrap(), plain);
    }

    #[test]
    fn the_newer_whole_root_is_read_and_a_torn_one_is_not() {
        let r = Root {
            turn: 7,
            pages: 9,
            span: 64,
            catalog: Extent {
                start: 2,
                bytes: 10,
            },
            orders: vec![(
                "s.t".into(),
                "o".into(),
                Extent {
                    start: 3,
                    bytes: 5000,
                },
            )],
            free: Extent {
                start: 8,
                bytes: 28,
            },
            reused: vec![(6, 4, 1), (7, 5, 2)],
            reused_from: 5,
            sequences: Extent {
                start: 11,
                bytes: 40,
            },
            release: 16,
            monetary: vec![("s.t".into(), "m".into(), "C".into())],
            collation: "und-x-icu".into(),
            ..Default::default()
        };
        let mut a = r.encode().unwrap();
        a.resize(PAGE, 0);
        let back = Root::decode(&a).unwrap();
        assert_eq!(back.collation(), "und-x-icu");
        // A root that names no default collation names C.UTF-8, and so does one written before
        // roots named one, its CRC right after its money columns.
        let plain = Root {
            collation: String::new(),
            ..r.clone()
        };
        let full = plain.encode().unwrap();
        let mut p = full.clone();
        p.resize(PAGE, 0);
        assert_eq!(Root::decode(&p).unwrap().collation(), COLLATION);
        // The decisions across snails come after the collation: no decision is a count and a flag.
        let none = 4 + 1;
        let mut old = full[..full.len() - 4 - none - (4 + COLLATION.len())].to_vec();
        let crc = crc32fast::hash(&old);
        old.extend_from_slice(&crc.to_le_bytes());
        old.resize(PAGE, 0);
        let was = Root::decode(&old).unwrap();
        assert_eq!((was.collation.as_str(), was.collation()), ("", COLLATION));
        assert_eq!(was.monetary, r.monetary);
        // A root written before it held decisions across snails ends after its collation.
        let mut old = full[..full.len() - 4 - none].to_vec();
        let crc = crc32fast::hash(&old);
        old.extend_from_slice(&crc.to_le_bytes());
        old.resize(PAGE, 0);
        let was = Root::decode(&old).unwrap();
        assert_eq!(
            (was.collation(), was.decided.len(), was.pending.is_none()),
            (COLLATION, 0, true)
        );
        assert_eq!((back.turn, back.span), (7, 64));
        assert_eq!((back.release, &back.monetary), (16, &r.monetary));
        assert_eq!(back.orders[0].2.pages(), 2);
        assert_eq!(
            (
                back.free,
                back.reused.clone(),
                back.reused_from,
                back.sequences
            ),
            (r.free, r.reused.clone(), 5, r.sequences)
        );
        let runs = [
            Run {
                start: 4,
                pages: 1,
                tag: 0,
            },
            Run {
                start: 9,
                pages: 3,
                tag: 7,
            },
        ];
        assert_eq!(decode_runs(&encode_runs(&runs)).unwrap(), runs);
        let spent = [(12, 2), (40, 1)];
        assert_eq!(
            decode_free(&encode_free(&runs, &spent)).unwrap(),
            (runs.to_vec(), spent.to_vec())
        );
        // A root of another format is no root this build reads: the file is refused by its format,
        // one of 0.1.x's with the way across.
        let way = "made by 0.1.x: `out` it with 0.1.x and `write` it afresh with 0.2.0";
        for (other, says) in [
            (7, way),
            (8, way),
            (10, "and this warren-snail reads format 9"),
        ] {
            let mut page = a.clone();
            page[8..12].copy_from_slice(&u32::to_le_bytes(other));
            assert!(Root::decode(&page).is_none());
            let mut both = page.clone();
            both.extend_from_slice(&page);
            let e = newer(&both).unwrap_err().to_string();
            assert!(
                e.starts_with(&format!("the snail is of format {other}, ")) && e.ends_with(says),
                "{e}"
            );
        }
        a[20] ^= 1;
        assert!(Root::decode(&a).is_none());
    }

    fn landed_of(pending: &Root) -> Root {
        let mut r = pending.clone();
        r.pending = None;
        r
    }

    #[test]
    fn a_pending_copy_of_the_root_is_never_taken_and_is_found_beside_the_root() {
        let page = |r: &Root| {
            let mut p = r.encode().unwrap();
            p.resize(PAGE, 0);
            p
        };
        let root = Root {
            turn: 4,
            decided: vec![Decision {
                at: 4,
                roots: vec![Written {
                    holds: "b.t".into(),
                    turn: 9,
                    crc: 77,
                }],
                carried: Extent {
                    start: 3,
                    bytes: 900,
                },
            }],
            ..Default::default()
        };
        let pending = Root {
            turn: 5,
            pending: Some(Pending {
                decider: "a.t".into(),
                at: 12,
            }),
            ..Default::default()
        };
        let back = Root::decode(&page(&pending)).unwrap();
        assert_eq!(back.pending, pending.pending);
        // The copies lie at their generations' places: the pending one, newer, is not taken.
        let mut both = page(&root);
        both.extend_from_slice(&page(&pending));
        let Copies {
            root: taken,
            pending: spare,
            copy,
            ..
        } = copies(&both, None).unwrap();
        assert_eq!((taken.turn, &taken.decided), (4, &root.decided));
        assert_eq!((spare.map(|s| s.turn), copy), (Some(5), 0));
        // Its root, written over its parent after its transaction is decided, torn: the pending
        // copy is the root, taken.
        let mut torn = page(&landed_of(&pending));
        torn[20] ^= 1;
        torn.extend_from_slice(&page(&pending));
        let Copies {
            root: taken,
            pending: spare,
            copy,
            ..
        } = copies(&torn, None).unwrap();
        assert_eq!(
            (taken.turn, taken.pending, spare.is_none(), copy),
            (5, None, true, 1)
        );
        // Taken, it is the root it was written for: its checksum leaves the mark out.
        let mut landed = pending.clone();
        landed.pending = None;
        let bytes = landed.encode().unwrap();
        assert_eq!(
            pending.crc().unwrap().to_le_bytes(),
            bytes[bytes.len() - 4..]
        );
        // Written over its parent, it is taken, and the pending copy beside it is no longer one.
        let mut both = page(&landed);
        both.extend_from_slice(&page(&pending));
        let Copies {
            root: taken,
            pending: spare,
            copy,
            ..
        } = copies(&both, None).unwrap();
        assert_eq!((taken.turn, spare.is_none(), copy), (5, true, 0));
        // A copy pending beside an older root than its own parent is no pending copy of it.
        let mut both = page(&Root {
            turn: 3,
            ..Default::default()
        });
        both.extend_from_slice(&page(&pending));
        assert!(copies(&both, None).unwrap().pending.is_none());
    }

    /// A judge that takes the tags it is given.
    struct Committed(Vec<u64>);

    impl crate::carry::Judge for Committed {
        fn taken(&self, tag: u64) -> Result<bool> {
            Ok(self.0.contains(&tag))
        }
    }

    fn tagged(turn: u64, host: Option<u64>) -> Vec<u8> {
        let mut p = Root {
            turn,
            host,
            ..Default::default()
        }
        .encode()
        .unwrap();
        p.resize(PAGE, 0);
        p
    }

    /// Of the copies at their generations' places: `(the generation taken, the tag waiting beside
    /// it)` as `judge` decides.
    fn taken_of(
        a: Vec<u8>,
        b: Vec<u8>,
        judge: Option<&dyn crate::carry::Judge>,
    ) -> (u64, Option<u64>) {
        let mut both = a;
        both.extend_from_slice(&b);
        let c = copies(&both, judge).unwrap();
        (c.root.turn, c.waiting)
    }

    #[test]
    fn a_file_is_one_identity_whatever_path_names_it() {
        let dir = std::env::temp_dir().join(format!("snail-identity-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (one, link, other) = (dir.join("one"), dir.join("link"), dir.join("other"));
        for p in [&one, &link, &other] {
            let _ = std::fs::remove_file(p);
        }
        std::fs::write(&one, b"a").unwrap();
        std::fs::write(&other, b"b").unwrap();
        std::fs::hard_link(&one, &link).unwrap();
        let id = identity_at(&one).unwrap();
        assert_eq!(identity_at(&link).unwrap(), id);
        assert_eq!(identity(&File::open(&link).unwrap()).unwrap(), id);
        assert_ne!(identity_at(&other).unwrap(), id);
        assert!(identity_at(&dir.join("none")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_root_its_host_tagged_is_taken_only_where_its_host_says_it_committed() {
        let back = Root::decode(&tagged(5, Some(1 << 40 | 7))).unwrap();
        assert_eq!(back.host, Some(1 << 40 | 7));
        assert_eq!(Root::decode(&tagged(5, None)).unwrap().host, None);
        let yes = Committed(vec![9]);
        let no = Committed(vec![]);
        // Generation 5 tagged 9 beside its parent: taken where the judge says 9 committed; a round
        // with no judge, or a judge that says it did not, reads the parent.
        assert_eq!(
            taken_of(tagged(4, None), tagged(5, Some(9)), Some(&yes)),
            (5, None)
        );
        assert_eq!(
            taken_of(tagged(4, None), tagged(5, Some(9)), Some(&no)),
            (4, Some(9))
        );
        assert_eq!(
            taken_of(tagged(4, None), tagged(5, Some(9)), None),
            (4, Some(9))
        );
        // A tagged root of the generation after it lies beside a tagged root only where its host
        // took it: taken without asking.
        assert_eq!(
            taken_of(tagged(6, Some(10)), tagged(5, Some(9)), None),
            (5, Some(10))
        );
        assert_eq!(
            taken_of(tagged(6, Some(10)), tagged(5, Some(9)), Some(&no)),
            (5, Some(10))
        );
        assert_eq!(
            taken_of(
                tagged(6, Some(10)),
                tagged(5, Some(9)),
                Some(&Committed(vec![10]))
            ),
            (6, None)
        );
        // The parent torn by the commit after the tagged root: the tagged root is the root.
        let mut torn = tagged(6, None);
        torn[20] ^= 1;
        assert_eq!(taken_of(torn, tagged(5, Some(9)), None), (5, None));
    }

    #[test]
    fn a_root_marked_a_replica_reads_back_its_source_with_its_tag_or_none() {
        let marked = |replica: Option<&str>, host: Option<u64>| {
            let mut p = Root {
                turn: 5,
                host,
                replica: replica.map(str::to_string),
                ..Default::default()
            }
            .encode()
            .unwrap();
            p.resize(PAGE, 0);
            Root::decode(&p).map(|r| (r.replica, r.host))
        };
        for host in [None, Some(0), Some(9), Some(1 << 40 | 7)] {
            for replica in [None, Some("mysql-1"), Some("a"), Some("primário 3306")] {
                assert_eq!(
                    marked(replica, host),
                    Some((replica.map(str::to_string), host)),
                    "{replica:?} {host:?}"
                );
            }
        }
        // A root marked no replica is written as before the mark was: its tag, where it has one,
        // last before its checksum.
        let bare = |host: Option<u64>| {
            Root {
                turn: 5,
                host,
                ..Default::default()
            }
            .encode()
            .unwrap()
        };
        let (untagged, tagged_9) = (bare(None), bare(Some(9)));
        assert_eq!(tagged_9.len(), untagged.len() + 8);
        assert_eq!(
            tagged_9[..untagged.len() - 4],
            untagged[..untagged.len() - 4]
        );
        assert_eq!(
            tagged_9[untagged.len() - 4..untagged.len() + 4],
            9u64.to_le_bytes()
        );
    }

    #[test]
    fn a_root_reads_back_its_turn_apart_from_its_generation_and_a_take_s_owed_pages() {
        let owed = Extent {
            start: 60,
            bytes: 52,
        };
        for (takes, owed) in [(0, Extent::default()), (3, owed)] {
            for (replica, host) in [(None, None), (Some("mysql-1"), Some(9)), (None, Some(7))] {
                let mut p = Root {
                    generation: 4,
                    turn: 9,
                    takes,
                    owed,
                    next_oid: 16400,
                    host,
                    replica: replica.map(str::to_string),
                    ..Default::default()
                }
                .encode()
                .unwrap();
                p.resize(PAGE, 0);
                let r = Root::decode(&p).unwrap();
                assert_eq!(
                    (r.generation, r.turn, r.takes, r.owed, r.next_oid, r.replica.as_deref(), r.host),
                    (4, 9, takes, owed, 16400, replica, host)
                );
            }
        }
        // Of two copies, the newer turn is the root, whatever their generations.
        let take = Root {
            generation: 4,
            turn: 9,
            takes: 1,
            ..Default::default()
        };
        let commit = Root {
            generation: 4,
            turn: 8,
            ..Default::default()
        };
        let mut both = vec![0u8; 2 * PAGE];
        for (c, r) in [(1, &take), (0, &commit)] {
            let b = r.encode().unwrap();
            both[c * PAGE..c * PAGE + b.len()].copy_from_slice(&b);
        }
        let c = copies(&both, None).unwrap();
        assert_eq!((c.copy, c.root.turn, c.root.takes), (1, 9, 1));
    }

    #[test]
    fn a_root_reads_back_its_oid_counter_and_record() {
        let record = Extent {
            start: 40,
            bytes: 24,
        };
        for (next_oid, oids) in [(0, Extent::default()), (16400, Extent::default()), (0, record), (16400, record)] {
            for (replica, host) in [(None, None), (Some("mysql-1"), Some(9)), (None, Some(7))] {
                let mut p = Root {
                    turn: 5,
                    text_search: "pg_catalog.english".into(),
                    next_oid,
                    oids,
                    host,
                    replica: replica.map(str::to_string),
                    ..Default::default()
                }
                .encode()
                .unwrap();
                p.resize(PAGE, 0);
                let r = Root::decode(&p).unwrap();
                assert_eq!(
                    (r.next_oid, r.oids, r.text_search.as_str(), r.replica.as_deref(), r.host),
                    (next_oid, oids, "pg_catalog.english", replica, host)
                );
            }
        }
        // A root whose counter and record are the text's own is written as before them.
        let plain = Root::default().encode().unwrap();
        let counted = Root {
            next_oid: 16400,
            ..Default::default()
        };
        assert_eq!(counted.encode().unwrap().len(), plain.len() + 4);
        let back = decode_record(&encode_record(&[(3, 16391), (7, 16384)])).unwrap();
        assert_eq!(back, [(3, 16391), (7, 16384)]);
    }

    #[test]
    fn pages_lie_in_files_of_the_span_and_no_run_crosses_one() {
        let path =
            std::env::temp_dir().join(format!("warren-snail-span-{}.snail", std::process::id()));
        let more = |n: u64| {
            let mut p = path.clone().into_os_string();
            p.push(format!(".{n}"));
            PathBuf::from(p)
        };
        for p in [path.clone(), more(1), more(2), more(3)] {
            let _ = std::fs::remove_file(p);
        }
        let mut w = Writer::create_spanned(&path, 4).unwrap();
        let a = w.place(&[1u8; 2 * PAGE]).unwrap();
        let b = w.place(&[2u8; 3 * PAGE]).unwrap();
        let c = w.place(&[3u8; 2 * PAGE]).unwrap();
        // The third run would cross from the second file into the third, so it starts the third, and
        // the page it passes over is free.
        assert_eq!((a.start, b.start, c.start), (2, 4, 8));
        assert_eq!(w.left_free(), [(7, 1)]);
        // A free run is taken where the pages fit one file.
        w.use_free(vec![(3, 3)]);
        assert_eq!(w.reserve(2), 4);
        assert_eq!(w.left_free(), [(3, 1)]);
        w.commit(&mut Root {
            turn: 1,
            ..Default::default()
        })
        .unwrap();
        drop(w);
        let len = |p: &PathBuf| std::fs::metadata(p).unwrap().len() / PAGE as u64;
        assert_eq!((len(&path), len(&more(1)), len(&more(2))), (4, 3, 2));
        assert!(!more(3).exists());
        let r = Reader::open(&path).unwrap();
        assert_eq!(r.root().unwrap().span, 4);
        assert_eq!(r.read_extent(a).unwrap(), [1u8; 2 * PAGE]);
        assert_eq!(r.read_extent(b).unwrap(), [2u8; 3 * PAGE]);
        assert_eq!(r.read_extent(c).unwrap(), [3u8; 2 * PAGE]);
        // A read of a run across a file's end reads each file's part.
        let across = r.read(3, 2).unwrap();
        assert_eq!(&across[..PAGE], &[1u8; PAGE][..]);
        assert_eq!(&across[PAGE..], &[2u8; PAGE][..]);
        for p in [path.clone(), more(1), more(2)] {
            std::fs::remove_file(p).unwrap();
        }
    }

    #[cfg(feature = "standalone")]
    #[test]
    fn a_held_page_stays_held_when_the_file_moves_to_a_new_generation() {
        let path =
            std::env::temp_dir().join(format!("warren-snail-held-{}.snail", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut w = Writer::create(&path).unwrap();
        let e = w.place(&[1u8; 10]).unwrap();
        w.commit(&mut Root {
            turn: 1,
            ..Default::default()
        })
        .unwrap();
        drop(w);

        let r = Reader::holding(&path, 8).unwrap();
        r.root().unwrap();
        assert_eq!(r.read_extent(e).unwrap(), [1u8; 10]);
        r.begin_round().unwrap();
        assert_eq!(r.read_extent(e).unwrap(), [1u8; 10]);
        assert_eq!((r.pages_read(), r.pages_held()), (2 + 1 + 2, 1));

        // A later generation writes a page of its own, and the page held stays held.
        let (mut w, mut root) = Writer::open(&path).unwrap();
        let f = w.place(&[2u8; 10]).unwrap();
        root.turn += 1;
        w.commit(&mut root).unwrap();
        assert_eq!(w.pages_written(), 2);
        r.begin_round().unwrap();
        assert_eq!(r.root().unwrap().turn, 2);
        assert_eq!(r.read_extent(e).unwrap(), [1u8; 10]);
        assert_eq!(r.read_extent(f).unwrap(), [2u8; 10]);
        assert_eq!(r.pages_held(), 2);
        std::fs::remove_file(&path).unwrap();
    }
}
