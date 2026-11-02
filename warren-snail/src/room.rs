//! The room a round holds its rows in, beside the pages it keeps for its later reads, and the file
//! it writes the rows past its room to: a file of the round's own, in a directory its opener may
//! name, removed as soon as it is opened, never the snail's. Past the most its opener lets it write
//! there, the round is refused by name.

use crate::cut::PAGE;
use crate::{Error, Result};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::rc::Rc;

/// What a round holds unless its opener says otherwise: as much as an open snail holds of its
/// pages between rounds.
pub const ROUND_BYTES: u64 = 128 << 20;

/// What an opener gives each round: the bytes it holds its rows and kept pages in, the directory
/// its spill lies in (the system's temporary directory where none is named), and the most it may
/// write there (none: as much as the disk takes).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Sizes {
    pub bytes: u64,
    pub dir: Option<PathBuf>,
    pub disk: Option<u64>,
}

impl Default for Sizes {
    fn default() -> Sizes {
        Sizes {
            bytes: ROUND_BYTES,
            dir: None,
            disk: None,
        }
    }
}

impl Sizes {
    /// A round of `bytes`, its spill in the system's temporary directory, as much as the disk takes.
    pub fn of(bytes: u64) -> Sizes {
        Sizes {
            bytes,
            ..Sizes::default()
        }
    }

    /// The same, its spill in `dir`.
    pub fn in_dir(mut self, dir: impl Into<PathBuf>) -> Sizes {
        self.dir = Some(dir.into());
        self
    }

    /// The same, its spill refused past `bytes`.
    pub fn at_most(mut self, bytes: u64) -> Sizes {
        self.disk = Some(bytes);
        self
    }
}

/// A page kept for the round's later reads: its file, its number there.
type At = (u64, u64);

#[derive(Default)]
struct Pages {
    kept: HashMap<At, (u64, u32, Box<[u8]>)>,
    by_use: BTreeMap<u64, At>,
    tick: u64,
    pinned: HashSet<At>,
    tagged: HashMap<(u64, u32), u64>,
}

impl Pages {
    fn drop_at(&mut self, at: At) -> bool {
        match self.kept.remove(&at) {
            Some((tick, tag, _)) => {
                self.by_use.remove(&tick);
                self.pinned.remove(&at);
                if let Some(c) = self.tagged.get_mut(&(at.0, tag)) {
                    *c -= 1;
                }
                true
            }
            None => false,
        }
    }
}

/// One round's room: what its places hold, the pages it keeps, and its spill.
pub struct Room {
    sizes: RefCell<Sizes>,
    held: Cell<u64>,
    flying: Cell<u64>,
    most: Cell<u64>,
    pages: RefCell<Pages>,
    spill: RefCell<Option<File>>,
    spill_pages: Cell<u64>,
    written: Cell<u64>,
    read: Cell<u64>,
}

static SPILLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl Room {
    pub fn new(sizes: Sizes) -> Rc<Room> {
        Rc::new(Room {
            sizes: RefCell::new(sizes),
            held: Cell::new(0),
            flying: Cell::new(0),
            most: Cell::new(0),
            pages: Default::default(),
            spill: RefCell::new(None),
            spill_pages: Cell::new(0),
            written: Cell::new(0),
            read: Cell::new(0),
        })
    }

    pub fn sizes(&self) -> Sizes {
        self.sizes.borrow().clone()
    }

    pub fn set_sizes(&self, sizes: Sizes) {
        *self.sizes.borrow_mut() = sizes;
    }

    /// Begins a round: nothing held, no page kept, and no spill.
    pub fn begin(&self) {
        self.held.set(0);
        self.flying.set(0);
        self.most.set(0);
        *self.pages.borrow_mut() = Pages::default();
        *self.spill.borrow_mut() = None;
        self.spill_pages.set(0);
        self.written.set(0);
        self.read.set(0);
    }

    fn page_bytes(&self) -> u64 {
        self.pages.borrow().kept.len() as u64 * PAGE as u64
    }

    fn noted(&self) {
        let now = self.held.get() + self.page_bytes() + self.flying.get();
        if now > self.most.get() {
            self.most.set(now);
        }
    }

    /// Rows handed from place to place, a chunk at a time: counted in the most the round holds, and
    /// taking no room from the places that hold rows.
    fn fly(&self, bytes: u64) {
        self.flying.set(self.flying.get() + bytes);
        self.noted();
    }

    fn land(&self, bytes: u64) {
        self.flying.set(self.flying.get().saturating_sub(bytes));
    }

    /// Whether `bytes` more rows fit, the pages kept for later reads let go, the longest unused
    /// first, to make room for them: held where they fit.
    pub fn hold(&self, bytes: u64) -> bool {
        let size = self.sizes.borrow().bytes;
        if self.held.get() + bytes > size {
            return false;
        }
        let mut p = self.pages.borrow_mut();
        while self.held.get() + bytes + p.kept.len() as u64 * PAGE as u64 > size {
            let Some(at) = p.by_use.values().find(|at| !p.pinned.contains(at)).copied() else {
                return false;
            };
            p.drop_at(at);
        }
        drop(p);
        self.held.set(self.held.get() + bytes);
        self.noted();
        true
    }

    /// The bytes more rows may hold, the pages kept for later reads let go for them.
    pub fn free_bytes(&self) -> u64 {
        self.sizes.borrow().bytes.saturating_sub(self.held.get())
    }

    /// Whether the rows held are past the room.
    pub fn over(&self) -> bool {
        self.held.get() > self.sizes.borrow().bytes
    }

    /// Rows held past the room, where a place holding them cannot write them: counted, so the most
    /// the round holds says so.
    pub fn hold_anyway(&self, bytes: u64) {
        if !self.hold(bytes) {
            self.held.set(self.held.get() + bytes);
            self.noted();
        }
    }

    /// Rows a place lets go.
    pub fn let_go(&self, bytes: u64) {
        self.held.set(self.held.get().saturating_sub(bytes));
    }

    /// The most bytes the round held at once: its places' rows and the pages it kept.
    pub fn most(&self) -> u64 {
        self.most.get()
    }

    /// The page, where the round keeps it, copied into `buf`.
    pub fn page(&self, file: u64, n: u64, buf: &mut [u8]) -> bool {
        let mut p = self.pages.borrow_mut();
        let p = &mut *p;
        match p.kept.get_mut(&(file, n)) {
            Some((tick, _, page)) => {
                buf.copy_from_slice(page);
                p.by_use.remove(tick);
                p.tick += 1;
                *tick = p.tick;
                p.by_use.insert(p.tick, (file, n));
                true
            }
            None => false,
        }
    }

    /// Whether the round keeps the page.
    pub fn keeps(&self, file: u64, n: u64) -> bool {
        self.pages.borrow().kept.contains_key(&(file, n))
    }

    /// Keeps a page just read for the round's later reads, under `tag`, where the room has room for
    /// it; it is let go first when rows need the room.
    pub fn keep_page(&self, file: u64, n: u64, page: &[u8], tag: u32) {
        let size = self.sizes.borrow().bytes;
        let mut p = self.pages.borrow_mut();
        if p.kept.contains_key(&(file, n))
            || self.held.get() + (p.kept.len() as u64 + 1) * PAGE as u64 > size
        {
            return;
        }
        p.tick += 1;
        let tick = p.tick;
        p.kept.insert((file, n), (tick, tag, page.into()));
        p.by_use.insert(tick, (file, n));
        *p.tagged.entry((file, tag)).or_default() += 1;
        drop(p);
        self.noted();
    }

    /// Of the pages a read will read, keeps those the round keeps until the read ends: how many.
    pub fn pin(&self, file: u64, pages: impl Iterator<Item = u64>) -> u64 {
        let mut p = self.pages.borrow_mut();
        let mut n = 0;
        for page in pages {
            if p.kept.contains_key(&(file, page)) {
                p.pinned.insert((file, page));
                n += 1;
            }
        }
        n
    }

    /// Lets the pages a read kept go with the room again.
    pub fn unpin(&self) {
        self.pages.borrow_mut().pinned.clear();
    }

    /// The pages the round keeps that were read under `tag`.
    pub fn tagged(&self, file: u64, tag: u32) -> u64 {
        self.pages
            .borrow()
            .tagged
            .get(&(file, tag))
            .copied()
            .unwrap_or(0)
    }

    /// Drops the pages kept in `start..start + pages`, which are about to be written again.
    pub fn forget(&self, file: u64, start: u64, pages: u64) {
        let mut p = self.pages.borrow_mut();
        for n in start..start + pages {
            p.drop_at((file, n));
        }
    }

    /// Drops every page kept of a file.
    pub fn forget_file(&self, file: u64) {
        let mut p = self.pages.borrow_mut();
        let ats: Vec<At> = p.kept.keys().filter(|a| a.0 == file).copied().collect();
        for at in ats {
            p.drop_at(at);
        }
    }

    /// Writes one page of rows past the room to the round's spill, made at its first page and
    /// removed as soon as it is opened: the page's place in it. Refused past the most the opener lets
    /// the round write.
    pub fn spill_page(&self, page: &[u8]) -> Result<u64> {
        debug_assert_eq!(page.len(), PAGE);
        let sizes = self.sizes.borrow().clone();
        let at = self.spill_pages.get();
        if let Some(most) = sizes.disk {
            if (at + 1) * PAGE as u64 > most {
                return Err(Error::new(format!(
                    "the round writes past the {most} bytes its opener lets it spill"
                )));
            }
        }
        let mut spill = self.spill.borrow_mut();
        if spill.is_none() {
            let dir = sizes.dir.unwrap_or_else(std::env::temp_dir);
            let path = dir.join(format!(
                "warren-snail-round-{}-{}",
                std::process::id(),
                SPILLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)
                .map_err(|e| {
                    Error::new(format!(
                        "the round's spill in {} is not made: {e}",
                        dir.display()
                    ))
                })?;
            std::fs::remove_file(&path)?;
            *spill = Some(file);
        }
        spill
            .as_ref()
            .expect("the spill is made")
            .write_all_at(page, at * PAGE as u64)?;
        self.spill_pages.set(at + 1);
        self.written.set(self.written.get() + 1);
        Ok(at)
    }

    /// Reads back one page of the round's spill.
    pub fn spilled_page(&self, at: u64, buf: &mut [u8]) -> Result<()> {
        let spill = self.spill.borrow();
        let file = spill
            .as_ref()
            .ok_or_else(|| Error::new("the round has no spill"))?;
        file.read_exact_at(buf, at * PAGE as u64)?;
        self.read.set(self.read.get() + 1);
        Ok(())
    }

    /// The pages the round wrote to its spill, and those it read back.
    pub fn spill_pages(&self) -> (u64, u64) {
        (self.written.get(), self.read.get())
    }
}

thread_local! {
    static CURRENT: RefCell<Option<Rc<Room>>> = const { RefCell::new(None) };
}

/// The room of the round under way, where one is.
pub fn current() -> Option<Rc<Room>> {
    CURRENT.with(|c| c.borrow().clone())
}

/// While it lives, the round under way holds its rows in `room`: a round within a round holds them
/// in the outer one's.
pub struct Entered(bool);

pub fn entered(room: &Rc<Room>) -> Entered {
    CURRENT.with(|c| {
        let mut c = c.borrow_mut();
        match c.is_some() {
            true => Entered(false),
            false => {
                room.begin();
                *c = Some(room.clone());
                Entered(true)
            }
        }
    })
}

impl Entered {
    /// Whether this is the round's outermost entry, the one that begins and ends it.
    pub fn outermost(&self) -> bool {
        self.0
    }
}

impl Drop for Entered {
    fn drop(&mut self) {
        if self.0 {
            CURRENT.with(|c| *c.borrow_mut() = None);
        }
    }
}

/// Counts `bytes` of rows handed from place to place, as long as the guard lives.
pub struct Flying(u64);

pub fn flying(bytes: u64) -> Flying {
    if let Some(r) = current() {
        r.fly(bytes);
    }
    Flying(bytes)
}

impl Drop for Flying {
    fn drop(&mut self) {
        if let Some(r) = current() {
            r.land(self.0);
        }
    }
}

/// Counts `bytes` held at a place that cannot write them, as long as the guard lives.
pub struct Holding(u64);

pub fn holding(bytes: u64) -> Holding {
    match current() {
        Some(r) => {
            r.hold_anyway(bytes);
            Holding(bytes)
        }
        None => Holding(0),
    }
}

impl Holding {
    /// Adds `bytes` to what the place holds where the room has room for them: whether it has.
    pub fn fits(&mut self, bytes: u64) -> bool {
        match current() {
            Some(r) => {
                let fits = r.hold(bytes);
                if fits {
                    self.0 += bytes;
                }
                fits
            }
            None => true,
        }
    }

    /// Adds `bytes` to what the place holds.
    pub fn more(&mut self, bytes: u64) {
        if let Some(r) = current() {
            r.hold_anyway(bytes);
            self.0 += bytes;
        }
    }
}

impl Drop for Holding {
    fn drop(&mut self) {
        if self.0 > 0 {
            if let Some(r) = current() {
                r.let_go(self.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_take_the_room_from_kept_pages_and_never_from_pinned_ones() {
        let room = Room::new(Sizes::of(4 * PAGE as u64));
        room.begin();
        let page = vec![7u8; PAGE];
        for n in 0..4 {
            room.keep_page(1, n, &page, 0);
        }
        assert_eq!(room.tagged(1, 0), 4);
        room.keep_page(1, 9, &page, 0);
        assert!(!room.keeps(1, 9), "a page past the room is not kept");
        assert_eq!(room.pin(1, [2, 3].into_iter()), 2);
        assert!(room.hold(2 * PAGE as u64));
        assert!(!room.keeps(1, 0) && !room.keeps(1, 1) && room.keeps(1, 2) && room.keeps(1, 3));
        assert!(!room.hold(1), "the pinned pages stay");
        room.unpin();
        assert!(room.hold(PAGE as u64));
        assert_eq!(room.most(), 4 * PAGE as u64);
    }

    #[test]
    fn a_spill_is_written_and_read_back_and_refused_past_its_most() {
        let room = Room::new(Sizes::of(0).at_most(2 * PAGE as u64));
        room.begin();
        let a = vec![1u8; PAGE];
        let b = vec![2u8; PAGE];
        assert_eq!(room.spill_page(&a).unwrap(), 0);
        assert_eq!(room.spill_page(&b).unwrap(), 1);
        let mut buf = vec![0u8; PAGE];
        room.spilled_page(1, &mut buf).unwrap();
        assert_eq!(buf, b);
        assert_eq!(room.spill_pages(), (2, 1));
        let refused = room.spill_page(&a).unwrap_err().to_string();
        assert!(refused.contains("lets it spill"), "{refused}");
    }
}
