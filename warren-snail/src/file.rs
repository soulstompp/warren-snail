//! The file as 4 KB pages. Pages 0 and 1 hold the root's two copies; every other page belongs to the
//! catalog, the directory, or a block. The pages lie in files of a span the root records: the
//! first file holds the root and the locks, and `<path>.1`, `.2` and on hold the pages after.

use crate::cut::PAGE;
#[cfg(feature = "standalone")]
use crate::held::{Held, UNTAGGED};
use crate::{Error, Result};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;

const MAGIC: [u8; 8] = *b"wsnail\0\x01";
/// The format a snail is made in, and the two before it, each read and written as it is: format 8,
/// 0.1.1's, and format 7, 0.1.0's, whose page maps each lie in pages of their own.
const FORMAT: u32 = 9;
pub(crate) const FORMAT_EIGHT: u32 = 8;
pub(crate) const FORMAT_SEVEN: u32 = 7;

/// Whether this build reads and writes a snail of `format`.
fn reads(format: u32) -> bool {
    format == FORMAT || format == FORMAT_EIGHT || format == FORMAT_SEVEN
}

/// Refuses a table whose columns a snail of `format` cannot hold: formats 7 and 8 hold the types
/// 0.1.x held, and no other.
pub(crate) fn holds(format: u32, table: &crate::catalog::Table) -> crate::Result<()> {
    if format >= FORMAT {
        return Ok(());
    }
    let way = format!(
        "which a snail of format {format} cannot hold: put the snail `out` and `write` it afresh, in \
         format {FORMAT}"
    );
    if let Some(c) = table
        .columns
        .iter()
        .find(|c| c.default.is_some() || c.generated.is_some() || c.identity.is_some())
    {
        return Err(crate::Error::new(format!(
            "{}.{} has a DEFAULT, a generation or an identity, {way}",
            table.name, c.name
        )));
    }
    if let Some(k) = table.checks.first() {
        return Err(crate::Error::new(format!(
            "{}: the CHECK {}, {way}",
            table.name, k.name
        )));
    }
    if let Some(o) = table.orders.iter().find(|o| o.without_overlaps()) {
        return Err(crate::Error::new(format!(
            "{}: the key WITHOUT OVERLAPS {}, {way}",
            table.name, o.name
        )));
    }
    if let Some(o) = table
        .orders
        .iter()
        .find(|o| o.constraint.as_ref().is_some_and(|k| !k.primary))
    {
        return Err(crate::Error::new(format!(
            "{}: the UNIQUE {}, {way}",
            table.name, o.name
        )));
    }
    // 0.1.x refuses any index but a B-tree in the DDL it reads.
    if let Some(o) = table
        .orders
        .iter()
        .find(|o| o.method != crate::catalog::Method::Btree)
    {
        return Err(crate::Error::new(format!(
            "{}: the {} index {}, {way}",
            table.name,
            o.method.name(),
            o.name
        )));
    }
    if let Some(o) = table.orders.iter().find(|o| o.predicate.is_some()) {
        return Err(crate::Error::new(format!(
            "{}: the partial index {}, {way}",
            table.name, o.name
        )));
    }
    if let Some(o) = table.orders.iter().find(|o| o.exclusion.is_some()) {
        return Err(crate::Error::new(format!(
            "{}: the EXCLUDE {}, {way}",
            table.name, o.name
        )));
    }
    // 0.1.x refuses a FOREIGN KEY in the DDL it reads.
    if let Some(k) = table.foreign_keys.first() {
        return Err(crate::Error::new(format!(
            "{}: the FOREIGN KEY {}, {way}",
            table.name, k.name
        )));
    }
    // Formats 7 and 8 key a level by a number or text alone.
    if let Some((o, c)) = table.orders.iter().find_map(|o| {
        o.levels
            .iter()
            .map(|l| &table.columns[l.column()])
            .find(|c| c.ty == crate::catalog::ColumnType::Bytes)
            .map(|c| (o, c))
    }) {
        return Err(crate::Error::new(format!(
            "{}: {} is a bytea level of {}, {way}",
            table.name, c.name, o.name
        )));
    }
    // Formats 7 and 8 compare text by its bytes alone.
    if let Some(c) = table.columns.iter().find(|c| c.library.is_some()) {
        return Err(crate::Error::new(format!(
            "{}.{} is under the collation {}, {way}",
            table.name,
            c.name,
            c.collation.as_deref().unwrap_or_default()
        )));
    }
    // Formats 7 and 8 hold a column as one leaf.
    if let Some(c) = table.columns.iter().find(|c| c.ty.leaf_count() > 1) {
        let leaves: Vec<String> = c.ty.leaves().iter().map(|l| l.name(&c.name)).collect();
        return Err(crate::Error::new(format!(
            "{}.{} is {}, a group of {} leaves, {}, {way}",
            table.name,
            c.name,
            c.ty.name(),
            leaves.len(),
            leaves.join(" and ")
        )));
    }
    match table.columns.iter().find(|c| !c.ty.held_by_0_1()) {
        Some(c) => Err(crate::Error::new(format!(
            "{}.{} is {}, which a snail of format {format} cannot hold: put the snail `out` and \
             `write` it afresh, in format {FORMAT}",
            table.name,
            c.name,
            c.ty.name()
        ))),
        None => Ok(()),
    }
}

/// Refuses a catalog's types, domains, sequences and collations in a snail of `format`: 0.1.x reads
/// none, and a sequence's state lies in format 9's root.
pub(crate) fn holds_catalog(format: u32, catalog: &crate::catalog::Catalog) -> crate::Result<()> {
    if format >= FORMAT {
        return Ok(());
    }
    let what = if let Some(t) = catalog.types.first() {
        format!("the type {}", t.name)
    } else if let Some(d) = catalog.domains.first() {
        format!("the domain {}", d.name)
    } else if let Some(s) = catalog.sequences.first() {
        format!("the sequence {}", s.name)
    } else if let Some(k) = catalog.collations.first() {
        format!("the collation {}", k.name)
    } else {
        return Ok(());
    };
    Err(crate::Error::new(format!(
        "{what}, which a snail of format {format} cannot hold: put the snail `out` and `write` it \
         afresh, in format {FORMAT}"
    )))
}

/// Refuses a numeric column's NaN in a snail of `format`: 0.1.x would read the integer that stands
/// for it as a number.
pub(crate) fn holds_nan(
    format: u32,
    table: &crate::catalog::Table,
    column: usize,
) -> crate::Result<()> {
    if format >= FORMAT {
        return Ok(());
    }
    Err(crate::Error::new(format!(
        "{}.{} holds NaN, which a snail of format {format} cannot hold: put the snail `out` and \
         `write` it afresh, in format {FORMAT}",
        table.name, table.columns[column].name
    )))
}

/// Refuses a NULL on a level of `order` in a snail of `format`: no key of formats 7 and 8 holds one.
pub(crate) fn keys_null(
    format: u32,
    table: &crate::catalog::Table,
    order: &crate::catalog::Order,
) -> crate::Result<()> {
    if format >= FORMAT {
        return Ok(());
    }
    Err(crate::Error::new(format!(
        "{}: a level of {} holds NULL, which a snail of format {format} cannot hold: put the snail \
         `out` and `write` it afresh, in format {FORMAT}",
        table.name, order.name
    )))
}

/// Refuses an infinite instant in column `column` of `table` in a snail of `format` where a level
/// of an order holds the column or its month: no key of formats 7 and 8 holds one, nor their twelve
/// counts of a month.
pub(crate) fn instant_ends(
    format: u32,
    table: &crate::catalog::Table,
    column: usize,
) -> crate::Result<()> {
    if format >= FORMAT
        || !table
            .orders
            .iter()
            .any(|o| o.levels.iter().any(|l| l.column() == column))
    {
        return Ok(());
    }
    Err(crate::Error::new(format!(
        "{}.{} holds an infinite instant on a level, which a snail of format {format} cannot hold: \
         put the snail `out` and `write` it afresh, in format {FORMAT}",
        table.name, table.columns[column].name
    )))
}

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
    pub generation: u64,
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
    /// Where each sequence stands that has handed out a value since the DDL made or restarted it:
    /// format 9 only.
    pub sequences: Extent,
    /// The PostgreSQL release the file's OIDs came from: format 9 only, 19 in formats 7 and 8,
    /// which hold no reg* value.
    pub release: u8,
    /// Each money column's `lc_monetary`: its table, its column and the locale; format 9 only, as
    /// formats 7 and 8 hold no money.
    pub monetary: Vec<(String, String, String)>,
    /// The file's own default collation, as a database's, which jsonb's strings sort under: format 9
    /// only; libc C.UTF-8 where it names none.
    pub collation: String,
    /// The transactions across snails this root's writes decided, each until a round across them
    /// finds every other snail's root written: format 9 only.
    pub decided: Vec<Decision>,
    /// Where this copy of the root was written before another snail's root decided its
    /// transaction: it is taken only once a round across them finds that root written. Format 9
    /// only.
    pub pending: Option<Pending>,
    /// The tag of the host's transaction this root was written for, where its host tags them: it
    /// is taken only where the host says that transaction committed, or where a root of the next
    /// generation lies beside it; a round with no host to ask reads the copy beside it. Format 9
    /// only.
    pub host: Option<u64>,
}

/// The default collation a file names where it names none: libc C.UTF-8, which orders text by its
/// bytes.
pub const COLLATION: &str = "C.UTF-8";

impl Root {
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
/// among the snails opened together, and the root's generation and checksum.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Written {
    pub holds: String,
    pub generation: u64,
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

/// The free map as a snail of the current format holds it: its runs, then each page of packed page
/// maps some of whose maps a commit has stopped naming, with how many. A snail of format 7 holds the
/// runs alone.
pub(crate) fn encode_free(runs: &[Run], spent: &[(u64, u32)], format: u32) -> Vec<u8> {
    let mut bytes = encode_runs(runs);
    if format != FORMAT_SEVEN {
        let mut o = Out::default();
        o.u32(spent.len() as u32);
        for &(page, n) in spent {
            o.u64(page);
            o.u32(n);
        }
        bytes.extend_from_slice(&o.0);
    }
    bytes
}

/// Each page of packed page maps some of whose maps a commit has stopped naming, with how many.
pub(crate) type Spent = Vec<(u64, u32)>;

pub(crate) fn decode_free(buf: &[u8], format: u32) -> Result<(Vec<Run>, Spent)> {
    let runs = decode_runs(buf)?;
    if format == FORMAT_SEVEN {
        return Ok((runs, Vec::new()));
    }
    let mut i = In::new(&buf[4 + 24 * runs.len()..]);
    let spent = (0..i.u32()?)
        .map(|_| Ok((i.u64()?, i.u32()?)))
        .collect::<Result<Vec<_>>>()?;
    Ok((runs, spent))
}

impl Root {
    pub(crate) fn encode(&self, format: u32) -> Result<Vec<u8>> {
        let mut o = Out::default();
        o.0.extend_from_slice(&MAGIC);
        o.u32(format);
        o.u64(self.generation);
        o.u64(self.pages);
        o.u64(self.span);
        o.extent(self.catalog);
        o.u32(self.orders.len() as u32);
        for (t, n, e) in &self.orders {
            o.str(t);
            o.str(n);
            o.extent(*e);
        }
        o.extent(self.free);
        o.u64(self.reused_from);
        o.u32(self.reused.len() as u32);
        for &(g, s, p) in &self.reused {
            o.u64(g);
            o.u64(s);
            o.u64(p);
        }
        if format >= FORMAT {
            o.extent(self.sequences);
            o.u8(self.release);
            o.u32(self.monetary.len() as u32);
            for (t, c, l) in &self.monetary {
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
                    o.u64(w.generation);
                    o.u32(w.crc);
                }
            }
            match &self.pending {
                None => o.u8(0),
                Some(p) => {
                    o.u8(1);
                    o.str(&p.decider);
                    o.u64(p.at);
                }
            }
            if let Some(tag) = self.host {
                o.u64(tag);
            }
        } else if self.host.is_some() {
            return Err(Error::new(format!(
                "a snail of format {format} is written through no host that tags its roots: `out`, \
                 then `write`"
            )));
        } else if !self.decided.is_empty() || self.pending.is_some() {
            return Err(Error::new(format!(
                "a snail of format {format} is written by no transaction across snails: `out`, \
                 then `write`"
            )));
        }
        let crc = crc32fast::hash(&o.0);
        o.u32(crc);
        if o.0.len() > PAGE {
            return Err(Error::new("the root does not fit one page"));
        }
        Ok(o.0)
    }

    /// A copy of the root and its format, where it is whole and of a format this build reads.
    pub(crate) fn decode(page: &[u8]) -> Option<(Root, u32)> {
        let mut i = In::new(page);
        if i.take(8).ok()? != MAGIC {
            return None;
        }
        let format = i.u32().ok()?;
        if !reads(format) {
            return None;
        }
        let generation = i.u64().ok()?;
        let pages = i.u64().ok()?;
        let span = i.u64().ok()?;
        let catalog = i.extent().ok()?;
        let n = i.u32().ok()? as usize;
        let mut orders = Vec::with_capacity(n);
        for _ in 0..n {
            orders.push((i.str().ok()?, i.str().ok()?, i.extent().ok()?));
        }
        let free = i.extent().ok()?;
        let reused_from = i.u64().ok()?;
        let m = i.u32().ok()? as usize;
        let mut reused = Vec::with_capacity(m.min(256));
        for _ in 0..m {
            reused.push((i.u64().ok()?, i.u64().ok()?, i.u64().ok()?));
        }
        let (sequences, release, monetary) = if format >= FORMAT {
            let sequences = i.extent().ok()?;
            let release = i.u8().ok()?;
            let n = i.u32().ok()? as usize;
            let mut monetary = Vec::with_capacity(n.min(256));
            for _ in 0..n {
                monetary.push((i.str().ok()?, i.str().ok()?, i.str().ok()?));
            }
            (sequences, release, monetary)
        } else {
            (Extent::default(), crate::regs::LATEST, Vec::new())
        };
        // A root written before format 9 named its default collation ends here, its CRC next; one
        // written before it held decisions across snails ends after its collation.
        let ended = |i: &In| {
            let at = i.at();
            page.get(at..at + 4).is_some_and(|c| {
                u32::from_le_bytes(c.try_into().unwrap()) == crc32fast::hash(&page[..at])
            })
        };
        let collation = if format >= FORMAT && !ended(&i) {
            i.str().ok()?
        } else {
            String::new()
        };
        let (decided, pending) = if format >= FORMAT && !ended(&i) {
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
                        generation: i.u64().ok()?,
                        crc: i.u32().ok()?,
                    });
                }
                decided.push(Decision { at, roots, carried });
            }
            let pending = match i.u8().ok()? {
                0 => None,
                _ => Some(Pending {
                    decider: i.str().ok()?,
                    at: i.u64().ok()?,
                }),
            };
            (decided, pending)
        } else {
            (Vec::new(), None)
        };
        // A root its host tagged carries the tag last.
        let host = if format >= FORMAT && !ended(&i) {
            Some(i.u64().ok()?)
        } else {
            None
        };
        let end = i.at();
        let crc = i.u32().ok()?;
        (crc32fast::hash(&page[..end]) == crc).then_some((
            Root {
                generation,
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
                decided,
                pending,
                host,
            },
            format,
        ))
    }

    /// The checksum a root's encoding ends with, as the root it stands for once taken: its pending
    /// mark left out.
    pub(crate) fn crc(&self, format: u32) -> Result<u32> {
        let mut taken = self.clone();
        taken.pending = None;
        let bytes = taken.encode(format)?;
        Ok(u32::from_le_bytes(
            bytes[bytes.len() - 4..].try_into().unwrap(),
        ))
    }
}

/// The pages that hold the root's two copies, read afresh by every round and never held.
#[cfg(feature = "standalone")]
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
    /// The format the root names, once read.
    pub(crate) format: Cell<u32>,
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
            format: Cell::new(FORMAT),
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
    /// The generation of the root last read, which the reader's round holds.
    generation: Cell<u64>,
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
    #[cfg(feature = "standalone")]
    held: RefCell<Held>,
    #[cfg(feature = "standalone")]
    hits: Cell<u64>,
    #[cfg(feature = "standalone")]
    tag: Cell<u32>,
}

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
            generation: Cell::new(0),
            round: Cell::new(None),
            staged: Default::default(),
            spare: RefCell::new(None),
            copy: Cell::new(0),
            judge: RefCell::new(None),
            waiting: Cell::new(None),
            #[cfg(feature = "standalone")]
            held: RefCell::new(Held::new(0)),
            #[cfg(feature = "standalone")]
            hits: Cell::new(0),
            #[cfg(feature = "standalone")]
            tag: Cell::new(UNTAGGED),
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

    /// The pages found held, rather than read from the file.
    pub fn pages_held(&self) -> u64 {
        #[cfg(feature = "standalone")]
        return self.hits.get();
        #[cfg(not(feature = "standalone"))]
        0
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

    /// Drops the pages staged since `mark`.
    pub(crate) fn staged_back_to(&self, mark: u64) {
        let mut s = self.staged.borrow_mut();
        s.pages.split_off(&(STAGED + mark));
        s.next = mark;
    }

    pub fn read(&self, start: u64, pages: u64) -> Result<Vec<u8>> {
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
            #[cfg(feature = "standalone")]
            self.hits.set(self.hits.get() + pages);
            return Ok(buf);
        }
        #[cfg(feature = "standalone")]
        {
            let mut held = self.held.borrow_mut();
            let mut missing = Vec::new();
            for i in 0..pages as usize {
                let n = start + i as u64;
                match held.touch(n) {
                    Some(page) => {
                        buf[i * PAGE..(i + 1) * PAGE].copy_from_slice(page);
                        self.hits.set(self.hits.get() + 1);
                    }
                    None => missing.push(i),
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
                    if n >= ROOT_PAGES {
                        held.keep(n, &buf[i * PAGE..(i + 1) * PAGE], self.tag.get());
                    }
                }
                k = z;
            }
            Ok(buf)
        }
        #[cfg(not(feature = "standalone"))]
        {
            self.files.read_at(&mut buf, start)?;
            self.pages.set(self.pages.get() + pages);
            Ok(buf)
        }
    }

    /// Tags the pages read from now on, until tagged otherwise; `None` leaves them untagged.
    pub fn tag_reads(&self, tag: Option<u32>) {
        #[cfg(feature = "standalone")]
        self.tag.set(tag.unwrap_or(UNTAGGED));
        #[cfg(not(feature = "standalone"))]
        let _ = tag;
    }

    /// The pages held that were read under `tag`.
    pub fn held_under(&self, tag: u32) -> u64 {
        #[cfg(feature = "standalone")]
        return self.held.borrow().tagged(tag);
        #[cfg(not(feature = "standalone"))]
        {
            let _ = tag;
            0
        }
    }

    /// Of these pages, those held, their use left as it was.
    pub fn held_among(&self, pages: impl Iterator<Item = u64>) -> u64 {
        #[cfg(feature = "standalone")]
        {
            let held = self.held.borrow();
            pages.filter(|&n| n >= STAGED || held.holds(n)).count() as u64
        }
        #[cfg(not(feature = "standalone"))]
        {
            let _ = pages;
            0
        }
    }

    /// Of the pages a round will read, those held, which the round now keeps until it ends.
    pub fn keep_for_round(&self, pages: impl Iterator<Item = u64>) -> u64 {
        #[cfg(feature = "standalone")]
        {
            let mut held = self.held.borrow_mut();
            pages
                .filter(|&n| n >= STAGED || (n >= ROOT_PAGES && held.touch(n).is_some()))
                .count() as u64
        }
        #[cfg(not(feature = "standalone"))]
        {
            let _ = pages;
            0
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
            format,
            pending,
            copy,
            waiting,
        } = self.copies()?;
        self.files.span.set(root.span);
        self.files.format.set(format);
        crate::lock::keep_generation(&self.files.first, from, root.generation)?;
        self.round.set(Some(root.generation));
        self.forget_reused(&root);
        self.generation.set(root.generation);
        *self.spare.borrow_mut() = pending;
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
            format,
            pending,
            copy,
            waiting,
        } = self.copies()?;
        self.files.span.set(root.span);
        self.files.format.set(format);
        self.forget_reused(&root);
        *self.spare.borrow_mut() = pending;
        self.copy.set(copy);
        self.waiting.set(waiting);
        Ok(root)
    }

    /// The root's two copies read, and the root taken from them as the reader's judge, where it has
    /// one, decides the roots a host tagged.
    fn copies(&self) -> Result<Copies> {
        let both = self.read(0, 2)?;
        let judge = self.judge.borrow().clone();
        copies(&both, judge.as_deref())
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
        self.generation.set(root.generation);
    }

    fn forget_reused(&self, root: &Root) {
        #[cfg(feature = "standalone")]
        {
            let last = self.generation.get();
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
        #[cfg(not(feature = "standalone"))]
        let _ = root;
    }

    /// Drops the held copies of the pages in `start..start + pages`, which are about to be written
    /// again.
    pub fn forget(&self, start: u64, pages: u64) {
        #[cfg(feature = "standalone")]
        self.held.borrow_mut().forget(start, pages);
        #[cfg(not(feature = "standalone"))]
        let _ = (start, pages);
    }

    /// The first file the reader reads, whose locks its round holds.
    pub fn file(&self) -> &File {
        &self.files.first
    }

    /// The format the root last read names.
    pub fn format(&self) -> u32 {
        self.files.format.get()
    }
}

/// The newer of the root's two copies, given as the file's first two pages, that is whole, and its
/// format. A file whose root names a format this build does not read is refused by that format. A
/// pending copy is never taken.
pub(crate) fn newer(both: &[u8]) -> Result<(Root, u32)> {
    copies(both, None).map(|c| (c.root, c.format))
}

/// The root a round takes from the root's two copies, and what lies beside it.
pub(crate) struct Copies {
    pub(crate) root: Root,
    pub(crate) format: u32,
    /// The other copy, where it is a pending one written for the generation after the root.
    pub(crate) pending: Option<Root>,
    /// The copy, 0 or 1, the root lies in.
    pub(crate) copy: u64,
    /// The tag of the other copy, where it is a root of the generation after the root that its
    /// host tagged and the round did not take.
    pub(crate) waiting: Option<u64>,
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
        let Some((r, _)) = &decoded[c] else {
            continue;
        };
        settled[c] = match r.host {
            None => true,
            Some(tag) => {
                decoded[1 - c]
                    .as_ref()
                    .is_some_and(|(o, _)| o.host.is_some() && o.generation == r.generation + 1)
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
                .filter(|(r, _)| r.pending.is_none() && settled[c as usize])
                .map(|d| (c, d))
        })
        .max_by_key(|(c, (r, _))| (r.generation, std::cmp::Reverse(*c)));
    if let Some((copy, (root, format))) = taken {
        let pending = decoded
            .iter()
            .flatten()
            .map(|(r, _)| r)
            .find(|r| r.pending.is_some() && r.generation == root.generation + 1)
            .cloned();
        let waiting = decoded[1 - copy as usize]
            .as_ref()
            .filter(|(r, _)| r.generation == root.generation + 1)
            .and_then(|(r, _)| r.host);
        return Ok(Copies {
            root: root.clone(),
            format: *format,
            pending,
            copy,
            waiting,
        });
    }
    if let Some((copy, (root, format))) =
        (0..2u64).find_map(|c| decoded[c as usize].as_ref().map(|d| (c, d)))
    {
        let mut root = root.clone();
        root.pending = None;
        return Ok(Copies {
            root,
            format: *format,
            pending: None,
            copy,
            waiting: None,
        });
    }
    for copy in [&both[..PAGE], &both[PAGE..]] {
        if copy[..8] == MAGIC {
            let format = u32::from_le_bytes(copy[8..12].try_into().unwrap());
            if !reads(format) {
                return Err(Error::new(format!(
                    "the snail is of format {format}, and this warren-snail reads formats {FORMAT_SEVEN}, {FORMAT_EIGHT} and {FORMAT}"
                )));
            }
        }
    }
    Err(Error::new("neither copy of the root is whole"))
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
        })
    }

    /// A writer on a snail already written, from the first page its root names none of.
    pub fn open(path: &Path) -> Result<(Writer, Root)> {
        let files = Files::open(path, true, false)?;
        let mut both = vec![0u8; 2 * PAGE];
        files.first.read_exact_at(&mut both, 0)?;
        let (root, format) = newer(&both)?;
        files.span.set(root.span);
        files.format.set(format);
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

    /// The format the snail is written in: a new snail's, or the one its root names.
    pub fn format(&self) -> u32 {
        self.files.format.get()
    }

    /// Makes a new snail in another format this build writes.
    pub(crate) fn set_format(&mut self, format: u32) {
        self.files.format.set(format);
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
        self.commit_into(root, root.generation % 2)
    }

    /// The commit, its root written to `copy`, 0 or 1, of the root's two: the one that does not
    /// hold the root it begins from.
    pub(crate) fn commit_into(&mut self, root: &mut Root, copy: u64) -> Result<()> {
        let bytes = self.prepare(root)?;
        if let Some(host) = self.host.as_mut() {
            let pages = std::mem::take(&mut self.carried);
            host.carry(&crate::carry::Commit::of(
                root.generation,
                pages,
                &bytes,
                root.span,
            ))?;
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
        root.encode(self.files.format.get())
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
    fn the_newer_whole_root_is_read_and_a_torn_one_is_not() {
        let r = Root {
            generation: 7,
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
        let mut a = r.encode(FORMAT).unwrap();
        a.resize(PAGE, 0);
        let (back, format) = Root::decode(&a).unwrap();
        assert_eq!(format, FORMAT);
        assert_eq!(back.collation(), "und-x-icu");
        // A root that names no default collation names C.UTF-8, and so does one written before
        // format 9 named one, its CRC right after its money columns.
        let plain = Root {
            collation: String::new(),
            ..r.clone()
        };
        let full = plain.encode(FORMAT).unwrap();
        let mut p = full.clone();
        p.resize(PAGE, 0);
        assert_eq!(Root::decode(&p).unwrap().0.collation(), COLLATION);
        // The decisions across snails come after the collation: no decision is a count and a flag.
        let none = 4 + 1;
        let mut old = full[..full.len() - 4 - none - (4 + COLLATION.len())].to_vec();
        let crc = crc32fast::hash(&old);
        old.extend_from_slice(&crc.to_le_bytes());
        old.resize(PAGE, 0);
        let (was, _) = Root::decode(&old).unwrap();
        assert_eq!((was.collation.as_str(), was.collation()), ("", COLLATION));
        assert_eq!(was.monetary, r.monetary);
        // A root written before it held decisions across snails ends after its collation.
        let mut old = full[..full.len() - 4 - none].to_vec();
        let crc = crc32fast::hash(&old);
        old.extend_from_slice(&crc.to_le_bytes());
        old.resize(PAGE, 0);
        let (was, _) = Root::decode(&old).unwrap();
        assert_eq!(
            (was.collation(), was.decided.len(), was.pending.is_none()),
            (COLLATION, 0, true)
        );
        assert_eq!((back.generation, back.span), (7, 64));
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
            decode_free(&encode_free(&runs, &spent, FORMAT), FORMAT).unwrap(),
            (runs.to_vec(), spent.to_vec())
        );
        assert_eq!(
            decode_free(&encode_free(&runs, &[], FORMAT_SEVEN), FORMAT_SEVEN).unwrap(),
            (runs.to_vec(), Vec::new())
        );
        assert_eq!(
            decode_free(&encode_free(&runs, &spent, FORMAT_EIGHT), FORMAT_EIGHT).unwrap(),
            (runs.to_vec(), spent.to_vec())
        );
        // A root of format 7 or 8 is read as one; of another format, the file is refused by its
        // format.
        for old in [FORMAT_SEVEN, FORMAT_EIGHT] {
            let mut page = r.encode(old).unwrap();
            page.resize(PAGE, 0);
            assert_eq!(Root::decode(&page).unwrap().1, old);
        }
        let mut ten = r.encode(10).unwrap();
        ten.resize(PAGE, 0);
        let mut both = ten.clone();
        both.extend_from_slice(&ten);
        let e = newer(&both).unwrap_err().to_string();
        assert!(e.contains("format 10"), "{e}");
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
            let mut p = r.encode(FORMAT).unwrap();
            p.resize(PAGE, 0);
            p
        };
        let root = Root {
            generation: 4,
            decided: vec![Decision {
                at: 4,
                roots: vec![Written {
                    holds: "b.t".into(),
                    generation: 9,
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
            generation: 5,
            pending: Some(Pending {
                decider: "a.t".into(),
                at: 12,
            }),
            ..Default::default()
        };
        let (back, _) = Root::decode(&page(&pending)).unwrap();
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
        assert_eq!((taken.generation, &taken.decided), (4, &root.decided));
        assert_eq!((spare.map(|s| s.generation), copy), (Some(5), 0));
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
            (taken.generation, taken.pending, spare.is_none(), copy),
            (5, None, true, 1)
        );
        // Taken, it is the root it was written for: its checksum leaves the mark out.
        let mut landed = pending.clone();
        landed.pending = None;
        let bytes = landed.encode(FORMAT).unwrap();
        assert_eq!(
            pending.crc(FORMAT).unwrap().to_le_bytes(),
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
        assert_eq!((taken.generation, spare.is_none(), copy), (5, true, 0));
        // A copy pending beside an older root than its own parent is no pending copy of it.
        let mut both = page(&Root {
            generation: 3,
            ..Default::default()
        });
        both.extend_from_slice(&page(&pending));
        assert!(copies(&both, None).unwrap().pending.is_none());
        // Formats 7 and 8 hold no decision.
        assert!(root.encode(FORMAT_EIGHT).is_err());
    }

    /// A judge that takes the tags it is given.
    struct Committed(Vec<u64>);

    impl crate::carry::Judge for Committed {
        fn taken(&self, tag: u64) -> Result<bool> {
            Ok(self.0.contains(&tag))
        }
    }

    fn tagged(generation: u64, host: Option<u64>) -> Vec<u8> {
        let mut p = Root {
            generation,
            host,
            ..Default::default()
        }
        .encode(FORMAT)
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
        (c.root.generation, c.waiting)
    }

    #[test]
    fn a_root_its_host_tagged_is_taken_only_where_its_host_says_it_committed() {
        let (back, _) = Root::decode(&tagged(5, Some(1 << 40 | 7))).unwrap();
        assert_eq!(back.host, Some(1 << 40 | 7));
        assert_eq!(Root::decode(&tagged(5, None)).unwrap().0.host, None);
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
        // Formats 7 and 8 hold no tag.
        assert!(Root {
            host: Some(9),
            ..Default::default()
        }
        .encode(FORMAT_EIGHT)
        .is_err());
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
            generation: 1,
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
            generation: 1,
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
        root.generation += 1;
        w.commit(&mut root).unwrap();
        assert_eq!(w.pages_written(), 2);
        r.begin_round().unwrap();
        assert_eq!(r.root().unwrap().generation, 2);
        assert_eq!(r.read_extent(e).unwrap(), [1u8; 10]);
        assert_eq!(r.read_extent(f).unwrap(), [2u8; 10]);
        assert_eq!(r.pages_held(), 2);
        std::fs::remove_file(&path).unwrap();
    }
}
