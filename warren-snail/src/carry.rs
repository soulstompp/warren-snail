//! A commit as it is carried to followers: every page it wrote, by its bytes, and its root. A writer
//! given a host hands it each commit once the commit's pages are on the disk, holding the root's turn
//! meanwhile, and writes the root once the host returns. A follower applies the commits one at a
//! time, each one's pages, then its root, and so stands at each of the leader's generations. A new
//! follower begins as a byte copy of the leader's file.

use crate::cut::PAGE;
use crate::file::{decode_free, newer, path_of, Files, In, Out, Reader, Root, Run, Stop};
use crate::{Error, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::os::unix::fs::FileExt;
use std::path::Path;

/// The bytes a commit's encoding begins with.
const MAGIC: [u8; 8] = *b"wscommit";

/// One page a commit wrote: the file it lies in, 0 for the first and `n` for `<path>.n`, its page in
/// that file, and its bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Page {
    pub file: u64,
    pub page: u64,
    pub bytes: Vec<u8>,
}

/// A commit: the generation it began from, the one it makes, every page it wrote but the root, and
/// the root's bytes, which lie on the root's copy its generation names.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Commit {
    pub parent: u64,
    pub generation: u64,
    pub pages: Vec<Page>,
    pub root: Vec<u8>,
}

/// What carries a writer's commits. It is handed each commit once the commit's pages are on the
/// disk, while the commit holds the root's turn, so that commits take turns meanwhile, and returns
/// once a majority holds it: the root is written then. An error leaves the file at the commit's
/// parent, the pages it wrote named by nothing, and the statement fails with it. It moves with its
/// writer between threads.
pub trait Host: Send {
    fn carry(&mut self, commit: &Commit) -> Result<()>;

    /// The tag of the host's transaction a transaction's root is written for, where the host
    /// decides whether it is taken: the root carries it, and is taken only where the host's judge
    /// says that transaction committed. A host's transaction commits a snail once. A sequence's
    /// advance lands at once, its root tagged `AT_ONCE`.
    fn tag(&self) -> Option<u64> {
        None
    }

    /// Whether the host keeps the root: it writes the commit's root itself, at its own word, before
    /// `carry` returns, and the writer writes none.
    fn keeps_root(&self) -> bool {
        false
    }
}

/// The tag of a root a host's writer wrote that lands at once, a sequence's advance: taken without
/// asking, and marking the snail as one written through its host.
pub const AT_ONCE: u64 = 0;

/// What tells a round whether a root its host tagged is taken: whether the host's transaction the
/// tag names committed, as the round asking sees it. An error refuses the round.
pub trait Judge {
    fn taken(&self, tag: u64) -> Result<bool>;

    /// Whether the round asking sees the host's transaction `tag` as ended, a transaction known
    /// to have committed, since a root built on its own lies in the file. Where it does not, the
    /// generation the round sees is no longer in the file (`Snail::unseen`). A round sees every
    /// transaction that committed unless its judge says otherwise.
    fn sees(&self, tag: u64) -> Result<bool> {
        let _ = tag;
        Ok(true)
    }

    /// The root the round takes, where the judge hands it rather than saying which copy is taken:
    /// its page as it lies in the file (`taken_root`). The round takes that root, refused by name
    /// where it lies in neither copy, and a writer takes its pages from its free map.
    fn root(&self) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }
}

/// What is told, inside a host with a monitor of its own, of each place a round reads and each place a
/// write changes, so that the host's monitor is the one monitor: the file keeps no marks. A place
/// comes by its lock's byte: a block's, its rows and its gaps at once; a block's gaps alone; or one
/// row's. An order comes by its table's and its own names; its top is every place of it. An error
/// refuses the round.
pub trait Monitor {
    /// A round read the place; or, with none, every place of the order.
    fn read(&self, table: &str, order: &str, block: Option<u64>) -> Result<()>;

    /// A write changes the place.
    fn wrote(&self, table: &str, order: &str, block: u64) -> Result<()>;

    /// A commit cut the block `from`, or laid it out afresh: what `to` holds lay in it; told of the
    /// blocks and of their gaps.
    fn cut(&self, table: &str, order: &str, from: u64, to: u64) -> Result<()>;

    /// A commit let the block go, holding no row, told of it and of its gaps: what lands in its gaps
    /// lands in another of the order.
    fn let_go(&self, table: &str, order: &str, block: u64) -> Result<()>;
}

/// The root a round takes from the snail at `path` as `judge` decides, its page as it lies in the
/// file: what a host that keeps the root hands a writer outside it, which takes that root.
pub fn taken_root(path: &Path, judge: &dyn Judge) -> Result<Vec<u8>> {
    let files = Files::open(path, false, false)?;
    let mut both = vec![0u8; 2 * PAGE];
    files.first.read_exact_at(&mut both, 0)?;
    let c = crate::file::copies(&both, Some(judge))?.copy as usize;
    let mut page = both[c * PAGE..(c + 1) * PAGE].to_vec();
    while page.last() == Some(&0) {
        page.pop();
    }
    Ok(page)
}

/// The newest generation whose root lies whole in the snail at `path`, whether a round takes it or
/// not.
pub fn newest(path: &Path) -> Result<u64> {
    let files = Files::open(path, false, false)?;
    let mut both = vec![0u8; 2 * PAGE];
    files.first.read_exact_at(&mut both, 0)?;
    [Root::decode(&both[..PAGE]), Root::decode(&both[PAGE..])]
        .into_iter()
        .flatten()
        .map(|(r, _)| r.generation)
        .max()
        .ok_or_else(|| Error::new("neither copy of the root is whole"))
}

impl Commit {
    /// The commit making `generation`, from the pages it wrote, each by its number in a snail whose
    /// files hold `span` pages, and its root's bytes.
    pub(crate) fn of(
        generation: u64,
        pages: BTreeMap<u64, Vec<u8>>,
        root: &[u8],
        span: u64,
    ) -> Commit {
        Commit {
            parent: generation - 1,
            generation,
            pages: pages
                .into_iter()
                .map(|(n, bytes)| {
                    let (file, page) = match n.checked_div(span) {
                        Some(file) => (file, n % span),
                        None => (0, n),
                    };
                    Page { file, page, bytes }
                })
                .collect(),
            root: root.to_vec(),
        }
    }

    /// The commit as bytes, as a host carries it.
    pub fn encode(&self) -> Vec<u8> {
        let mut o = Out::default();
        o.0.extend_from_slice(&MAGIC);
        o.u64(self.parent);
        o.u64(self.generation);
        o.u32(self.pages.len() as u32);
        for p in &self.pages {
            o.u64(p.file);
            o.u64(p.page);
            o.u32(p.bytes.len() as u32);
            o.0.extend_from_slice(&p.bytes);
        }
        o.u32(self.root.len() as u32);
        o.0.extend_from_slice(&self.root);
        let crc = crc32fast::hash(&o.0);
        o.u32(crc);
        o.0
    }

    /// The tag its host gave the commit's root, where it tagged it: a sequence's advance, and a
    /// commit whose host tags nothing, carry none.
    pub fn host(&self) -> Option<u64> {
        let mut page = self.root.clone();
        page.resize(PAGE, 0);
        Root::decode(&page).and_then(|(r, _)| r.host)
    }

    /// The source the commit's root marks the snail a replica of, where it marks it one.
    pub fn replica(&self) -> Option<String> {
        let mut page = self.root.clone();
        page.resize(PAGE, 0);
        Root::decode(&page).and_then(|(r, _)| r.replica)
    }

    /// The commit `encode` wrote, refused where its bytes are not whole.
    pub fn decode(bytes: &[u8]) -> Result<Commit> {
        let n = bytes.len();
        if n < MAGIC.len() + 4
            || bytes[..MAGIC.len()] != MAGIC
            || crc32fast::hash(&bytes[..n - 4])
                != u32::from_le_bytes(bytes[n - 4..].try_into().unwrap())
        {
            return Err(Error::new("these bytes are not a snail's commit, whole"));
        }
        let mut i = In::new(&bytes[MAGIC.len()..n - 4]);
        let parent = i.u64()?;
        let generation = i.u64()?;
        let mut pages = Vec::new();
        for _ in 0..i.u32()? {
            let file = i.u64()?;
            let page = i.u64()?;
            let len = i.u32()? as usize;
            pages.push(Page {
                file,
                page,
                bytes: i.take(len)?.to_vec(),
            });
        }
        let len = i.u32()? as usize;
        let root = i.take(len)?.to_vec();
        Ok(Commit {
            parent,
            generation,
            pages,
            root,
        })
    }
}

/// Applies a commit to a follower's snail at `path`, at the follower's root's turn, as a commit
/// there would take it: the commit's pages written where they lie and brought to the disk, then its
/// root. The follower must stand at the commit's parent, and is refused by name otherwise; one that
/// stands at the commit already, its root the commit's, is left as it is. Where the commit frees
/// pages a round of the follower's may still read, holding an older generation, it waits for that
/// round to end first, as long as it takes.
pub fn apply(path: &Path, commit: &Commit) -> Result<()> {
    land(path, commit, None)
}

/// `apply`, stopping where `stop` says: for the tests of a follower stopped partway.
#[doc(hidden)]
pub fn apply_stopping(path: &Path, commit: &Commit, stop: Option<Stop>) -> Result<()> {
    land(path, commit, stop)
}

fn land(path: &Path, commit: &Commit, stop: Option<Stop>) -> Result<()> {
    let files = Files::open(path, true, false)?;
    crate::lock::take_root_turn(&files.first)?;
    let landed = land_at_turn(&files, commit, stop);
    let given = crate::lock::give_root_turn(&files.first);
    landed?;
    given
}

fn land_at_turn(files: &Files, commit: &Commit, stop: Option<Stop>) -> Result<()> {
    let mut both = vec![0u8; 2 * PAGE];
    files.first.read_exact_at(&mut both, 0)?;
    let (root, format) = newer(&both)?;
    files.span.set(root.span);
    files.format.set(format);
    let mut page = commit.root.clone();
    page.resize(PAGE, 0);
    let (theirs, their_format) = Root::decode(&page)
        .filter(|(r, _)| {
            commit.root.len() <= PAGE
                && r.generation == commit.generation
                && commit.parent + 1 == commit.generation
        })
        .ok_or_else(|| {
            Error::new(format!(
                "the commit of generation {} holds no whole root of its generation",
                commit.generation
            ))
        })?;
    let copy = (commit.generation % 2) as usize * PAGE;
    if root.generation == commit.generation && both[copy..copy + PAGE] == page[..] {
        return Ok(());
    }
    if root.generation != commit.parent {
        return Err(Error::new(format!(
            "the commit begins from generation {}, and the follower stands at generation {}",
            commit.parent, root.generation
        )));
    }
    if their_format != format || theirs.span != root.span {
        return Err(Error::new(format!(
            "the commit is of format {their_format} in files of {} pages, and the follower of \
             format {format} in files of {}",
            theirs.span, root.span
        )));
    }
    let span = root.span;
    let pages = pages_of(commit, span)?;
    crate::lock::wait_older_let_go(&files.first, freed(files, &root, &theirs, &pages, format)?)?;
    write_pages(files, &pages)?;
    match stop {
        Some(Stop::BeforeRoot) => return Err(Error::new("the apply stopped before the root")),
        Some(Stop::HalfRoot) => {
            files
                .first
                .write_all_at(&commit.root[..commit.root.len() / 2], copy as u64)?;
            return Err(Error::new("the apply stopped halfway through the root"));
        }
        None => {}
    }
    files.first.write_all_at(&page, copy as u64)?;
    files.first.sync_data()?;
    Ok(())
}

/// A commit's pages by their numbers in a snail whose files hold `span` pages, refused where one is
/// no page a commit writes.
fn pages_of(commit: &Commit, span: u64) -> Result<BTreeMap<u64, &[u8]>> {
    let mut pages: BTreeMap<u64, &[u8]> = BTreeMap::new();
    for p in &commit.pages {
        let n = p.file * span + p.page;
        if p.bytes.len() != PAGE
            || (span > 0 && p.page >= span)
            || (span == 0 && p.file > 0)
            || n < 2
        {
            return Err(Error::new(format!(
                "page {} of file {} is no page a commit writes",
                p.page, p.file
            )));
        }
        pages.insert(n, &p.bytes);
    }
    Ok(pages)
}

/// A commit's pages written where they lie, a run at a time, and brought to the disk.
fn write_pages(files: &Files, pages: &BTreeMap<u64, &[u8]>) -> Result<()> {
    let mut run: Vec<u8> = Vec::new();
    let mut start = 0;
    for (&n, bytes) in pages {
        if !run.is_empty() && start + (run.len() / PAGE) as u64 != n {
            files.write_at(&run, start)?;
            run.clear();
        }
        if run.is_empty() {
            start = n;
        }
        run.extend_from_slice(bytes);
    }
    if !run.is_empty() {
        files.write_at(&run, start)?;
    }
    files.sync()
}

/// What a replay of a commit did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Replayed {
    /// Its pages and its root written.
    Landed,
    /// Its root found where it lies already: nothing written.
    Already,
    /// The file found past it, at a later generation: nothing written.
    Passed,
}

/// Replays a commit on the snail at `path`, at its root's turn, as a host's recovery reads its log
/// again from a point the file may be past already. A commit whose root lies in the copy its
/// generation names is left as it is. One whose parent lies in the other copy has its pages written
/// and brought to the disk, then its root, over whatever that copy held: a root of the same
/// generation its host never took among them, since the log holds the commits in the order they
/// were written. One the file is past, holding a later generation, is left; any other is refused by
/// name. Where the commit frees pages a round of the snail's may still read, holding an older
/// generation, `waiting` is called with the newest tag it frees each time the replay finds such a
/// round, until none does; an error from it ends the replay, nothing written.
pub fn replay(
    path: &Path,
    commit: &Commit,
    waiting: &mut dyn FnMut(u64) -> Result<()>,
) -> Result<Replayed> {
    let files = Files::open(path, true, false)?;
    crate::lock::take_root_turn(&files.first)?;
    let replayed = replay_at_turn(&files, commit, waiting);
    let given = crate::lock::give_root_turn(&files.first);
    let replayed = replayed?;
    given?;
    Ok(replayed)
}

/// `replay`, by a host that keeps the root, for a commit whose writer holds the root's turn while
/// its host writes the root: the turn is not taken again.
pub fn replay_held(
    path: &Path,
    commit: &Commit,
    waiting: &mut dyn FnMut(u64) -> Result<()>,
) -> Result<Replayed> {
    replay_at_turn(&Files::open(path, true, false)?, commit, waiting)
}

fn replay_at_turn(
    files: &Files,
    commit: &Commit,
    waiting: &mut dyn FnMut(u64) -> Result<()>,
) -> Result<Replayed> {
    let mut both = vec![0u8; 2 * PAGE];
    files.first.read_exact_at(&mut both, 0)?;
    let mut page = commit.root.clone();
    page.resize(PAGE, 0);
    let (theirs, their_format) = Root::decode(&page)
        .filter(|(r, _)| {
            commit.root.len() <= PAGE
                && r.generation == commit.generation
                && commit.parent + 1 == commit.generation
        })
        .ok_or_else(|| {
            Error::new(format!(
                "the commit of generation {} holds no whole root of its generation",
                commit.generation
            ))
        })?;
    let c = (commit.generation % 2) as usize;
    if both[c * PAGE..(c + 1) * PAGE] == page[..] {
        return Ok(Replayed::Already);
    }
    let decoded = [Root::decode(&both[..PAGE]), Root::decode(&both[PAGE..])];
    let newest = decoded.iter().flatten().map(|(r, _)| r.generation).max();
    let (root, format) = match (&decoded[1 - c], newest) {
        (_, Some(n)) if n > commit.generation => return Ok(Replayed::Passed),
        (Some((r, f)), _) if r.generation == commit.parent => (r.clone(), *f),
        (None, Some(n)) if n == commit.generation => return Ok(Replayed::Passed),
        _ => {
            return Err(Error::new(format!(
                "the commit begins from generation {}, and the snail stands at generation {}",
                commit.parent,
                newest.map_or("none whole".to_string(), |n| n.to_string())
            )))
        }
    };
    files.span.set(root.span);
    files.format.set(format);
    if their_format != format || theirs.span != root.span {
        return Err(Error::new(format!(
            "the commit is of format {their_format} in files of {} pages, and the snail of \
             format {format} in files of {}",
            theirs.span, root.span
        )));
    }
    let pages = pages_of(commit, root.span)?;
    let f = freed(files, &root, &theirs, &pages, format)?;
    let mut nap = std::time::Duration::from_millis(1);
    while crate::lock::older_held(&files.first, f)? {
        waiting(f)?;
        std::thread::sleep(nap);
        nap = (nap * 2).min(std::time::Duration::from_millis(50));
    }
    write_pages(files, &pages)?;
    files.first.write_all_at(&page, (c * PAGE) as u64)?;
    files.first.sync_data()?;
    Ok(Replayed::Landed)
}

/// The newest tag of the runs a commit frees: those its parent's free map holds waiting on a tag the
/// commit's free map no longer holds; 0 where it frees none.
fn freed(
    files: &Files,
    parent: &Root,
    root: &Root,
    pages: &BTreeMap<u64, &[u8]>,
    format: u32,
) -> Result<u64> {
    let tags = |runs: Vec<Run>| -> BTreeSet<u64> {
        runs.into_iter().map(|r| r.tag).filter(|&t| t > 0).collect()
    };
    let mut before = BTreeSet::new();
    if parent.free.bytes > 0 {
        let mut buf = vec![0u8; parent.free.pages() as usize * PAGE];
        files.read_at(&mut buf, parent.free.start)?;
        buf.truncate(parent.free.bytes as usize);
        before = tags(decode_free(&buf, format)?.0);
    }
    let mut after = BTreeSet::new();
    if root.free.bytes > 0 {
        let mut buf = Vec::new();
        for n in root.free.start..root.free.start + root.free.pages() {
            buf.extend_from_slice(pages.get(&n).ok_or_else(|| {
                Error::new("the commit's free map lies outside the pages it wrote")
            })?);
        }
        buf.truncate(root.free.bytes as usize);
        after = tags(decode_free(&buf, format)?.0);
    }
    Ok(before.difference(&after).max().copied().unwrap_or(0))
}

/// Copies the snail at `leader` to `to`, where no file lies yet: every one of its files, byte for
/// byte, while a round holds the generation the copy then stands at, which it returns. The copy's
/// root is that generation's, its other copy the leader's where that one is older, and it takes the
/// leader's commits from that generation on.
pub fn copy(leader: &Path, to: &Path) -> Result<u64> {
    if to.exists() {
        return Err(Error::new(format!(
            "{}: a follower's first copy is made where no file lies",
            to.display()
        )));
    }
    let reader = Reader::open(leader)?;
    let root = reader.root()?;
    let format = reader.format();
    let mut copied = Vec::new();
    for n in 0.. {
        let (from, into) = match n {
            0 => (leader.to_path_buf(), to.to_path_buf()),
            n => (path_of(leader, n), path_of(to, n)),
        };
        if n > 0 && !from.exists() {
            break;
        }
        std::fs::copy(&from, &into).map_err(|e| Error::new(format!("{}: {e}", from.display())))?;
        copied.push(into);
    }
    let first = OpenOptions::new().read(true).write(true).open(to)?;
    let mut both = vec![0u8; 2 * PAGE];
    first.read_exact_at(&mut both, 0)?;
    let at = (root.generation % 2) as usize * PAGE;
    let other = PAGE - at;
    let mut ours = root.encode(format)?;
    ours.resize(PAGE, 0);
    if !Root::decode(&both[other..other + PAGE])
        .is_some_and(|(r, _)| r.generation < root.generation)
    {
        both[other..other + PAGE].fill(0);
    }
    both[at..at + PAGE].copy_from_slice(&ours);
    first.write_all_at(&both, 0)?;
    for path in &copied {
        OpenOptions::new().write(true).open(path)?.sync_all()?;
    }
    drop(reader);
    Ok(root.generation)
}
