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
/// The format a snail is made in, and the one before it, which is read and written as it is: its
/// page maps each lie in pages of their own.
const FORMAT: u32 = 8;
pub(crate) const FORMAT_SEVEN: u32 = 7;

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
    pub fn str(&mut self, s: &str) {
        self.u32(s.len() as u32);
        self.0.extend_from_slice(s.as_bytes());
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
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
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
    pub fn str(&mut self) -> Result<String> {
        let n = self.u32()? as usize;
        String::from_utf8(self.take(n)?.to_vec())
            .map_err(|_| Error::new("a name in the file is not UTF-8"))
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
    fn encode(&self, format: u32) -> Result<Vec<u8>> {
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
        let crc = crc32fast::hash(&o.0);
        o.u32(crc);
        if o.0.len() > PAGE {
            return Err(Error::new("the root does not fit one page"));
        }
        Ok(o.0)
    }

    /// A copy of the root and its format, where it is whole and of a format this build reads.
    fn decode(page: &[u8]) -> Option<(Root, u32)> {
        let mut i = In::new(page);
        if i.take(8).ok()? != MAGIC {
            return None;
        }
        let format = i.u32().ok()?;
        if format != FORMAT && format != FORMAT_SEVEN {
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
            },
            format,
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
struct Files {
    path: PathBuf,
    first: File,
    rest: RefCell<Vec<Option<File>>>,
    span: Cell<u64>,
    /// The format the root names, once read.
    format: Cell<u32>,
    write: bool,
    /// The files written since they last reached the disk.
    dirty: RefCell<BTreeSet<u64>>,
}

impl Files {
    fn open(path: &Path, write: bool, create: bool) -> Result<Files> {
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
        let mut p = self.path.clone().into_os_string();
        p.push(format!(".{n}"));
        PathBuf::from(p)
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

    fn read_at(&self, buf: &mut [u8], page: u64) -> Result<()> {
        for (n, at, k, from) in self.runs(page, (buf.len() / PAGE) as u64) {
            let b = &mut buf[from as usize * PAGE..(from + k) as usize * PAGE];
            self.with(n, |f| f.read_exact_at(b, at * PAGE as u64))?;
        }
        Ok(())
    }

    fn write_at(&self, buf: &[u8], page: u64) -> Result<()> {
        for (n, at, k, from) in self.runs(page, (buf.len() / PAGE) as u64) {
            let b = &buf[from as usize * PAGE..(from + k) as usize * PAGE];
            self.with(n, |f| f.write_all_at(b, at * PAGE as u64))?;
            self.dirty.borrow_mut().insert(n);
        }
        Ok(())
    }

    /// Every file written reaches the disk.
    fn sync(&self) -> Result<()> {
        for n in std::mem::take(&mut *self.dirty.borrow_mut()) {
            self.with(n, |f| f.sync_data())?;
        }
        Ok(())
    }
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
    #[cfg(feature = "standalone")]
    held: RefCell<Held>,
    #[cfg(feature = "standalone")]
    hits: Cell<u64>,
    #[cfg(feature = "standalone")]
    tag: Cell<u32>,
}

impl Reader {
    /// A reader that holds no page between rounds.
    pub fn open(path: &Path) -> Result<Reader> {
        Ok(Reader {
            files: Files::open(path, false, false)?,
            pages: Cell::new(0),
            generation: Cell::new(0),
            round: Cell::new(None),
            staged: Default::default(),
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
        let (root, format) = newer(&self.read(0, 2)?)?;
        self.files.span.set(root.span);
        self.files.format.set(format);
        crate::lock::keep_generation(&self.files.first, from, root.generation)?;
        self.round.set(Some(root.generation));
        self.forget_reused(&root);
        self.generation.set(root.generation);
        Ok(root)
    }

    /// The newest root, read now, the pages held that were reused since the reader's root dropped.
    /// The round holds the generation it held, older, so no page the newest root names is freed while
    /// the reader reads it.
    pub fn peek_root(&self) -> Result<Root> {
        let (root, format) = newer(&self.read(0, 2)?)?;
        self.files.span.set(root.span);
        self.files.format.set(format);
        self.forget_reused(&root);
        Ok(root)
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
/// format. A file whose root names a format this build does not read is refused by that format.
fn newer(both: &[u8]) -> Result<(Root, u32)> {
    match (Root::decode(&both[..PAGE]), Root::decode(&both[PAGE..])) {
        (Some(a), Some(b)) => Ok(if a.0.generation >= b.0.generation {
            a
        } else {
            b
        }),
        (Some(r), None) | (None, Some(r)) => Ok(r),
        (None, None) => {
            for copy in [&both[..PAGE], &both[PAGE..]] {
                if copy[..8] == MAGIC {
                    let format = u32::from_le_bytes(copy[8..12].try_into().unwrap());
                    if format != FORMAT && format != FORMAT_SEVEN {
                        return Err(Error::new(format!(
                            "the snail is of format {format}, and this warren-snail reads formats {FORMAT_SEVEN} and {FORMAT}"
                        )));
                    }
                }
            }
            Err(Error::new("neither copy of the root is whole"))
        }
    }
}

/// Where a commit stops short, for the tests of a crash: before the root, or halfway through it.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

    /// The page a run of `pages` reserved now would begin on.
    fn lands(&self, pages: u64) -> u64 {
        for &(s, n) in &self.free {
            let at = if self.in_one_file(s, pages) {
                s
            } else {
                self.next_file(s)
            };
            if at + pages <= s + n {
                return at;
            }
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
            let at = if self.in_one_file(s, pages) {
                s
            } else {
                self.next_file(s)
            };
            if at + pages <= s + n {
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
        Ok(())
    }
}

impl Writer {
    /// The commit: every page reaches the disk, then the root, written to the copy its generation
    /// names.
    pub fn commit(&mut self, root: &mut Root) -> Result<()> {
        root.pages = self.next;
        root.span = self.files.span.get();
        self.files.sync()?;
        let bytes = root.encode(self.files.format.get())?;
        match self.stop {
            Some(Stop::BeforeRoot) => return Err(Error::new("the commit stopped before the root")),
            Some(Stop::HalfRoot) => {
                self.files.first.write_all_at(
                    &bytes[..bytes.len() / 2],
                    (root.generation % 2) * PAGE as u64,
                )?;
                self.written += 1;
                return Err(Error::new("the commit stopped halfway through the root"));
            }
            None => {}
        }
        self.write_at(root.generation % 2, &bytes)?;
        self.files.sync()?;
        Ok(())
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
        };
        let mut a = r.encode(FORMAT).unwrap();
        a.resize(PAGE, 0);
        let (back, format) = Root::decode(&a).unwrap();
        assert_eq!(format, FORMAT);
        assert_eq!((back.generation, back.span), (7, 64));
        assert_eq!(back.orders[0].2.pages(), 2);
        assert_eq!(
            (back.free, back.reused.clone(), back.reused_from),
            (r.free, r.reused.clone(), 5)
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
        // A root of format 7 is read as one; of another format, the file is refused by its format.
        let mut seven = r.encode(FORMAT_SEVEN).unwrap();
        seven.resize(PAGE, 0);
        assert_eq!(Root::decode(&seven).unwrap().1, FORMAT_SEVEN);
        let mut nine = r.encode(9).unwrap();
        nine.resize(PAGE, 0);
        let mut both = nine.clone();
        both.extend_from_slice(&nine);
        let e = newer(&both).unwrap_err().to_string();
        assert!(e.contains("format 9"), "{e}");
        a[20] ^= 1;
        assert!(Root::decode(&a).is_none());
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
