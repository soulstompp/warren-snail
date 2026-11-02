//! Locks on bytes of the file, each held by an open file: a round's on the generation it reads, a
//! writer's on each block it changes, and a commit's turn at the root. The bytes lie past any page
//! the file holds; a lock goes when the file that holds it is closed, or its program ends.

use crate::{Error, Result};
use std::fs::File;
use std::os::unix::io::AsRawFd;

/// The byte whose lock is a commit's turn at the root.
const ROOT_TURN: u64 = 1 << 60;
/// The bytes of the generations, one a generation, from the first; they end well short of the
/// blocks' bytes.
const GENERATIONS: u64 = (1 << 60) + 1;
const GENERATION_BYTES: u64 = 1 << 58;
/// The bytes of the blocks, one a block, as its order and first key fall.
const BLOCKS: u64 = 1 << 61;
const BLOCK_BYTES: u64 = 1 << 40;
/// The bytes of the writers, one a writer open on the file, by its slot.
const WRITERS: u64 = 1 << 59;
const WRITER_SLOTS: u64 = 1 << 16;
/// A byte for each block and slot: the writer in that slot holds the block.
const HOLDS: u64 = 1 << 62;
/// A byte for each slot and block: the writer in that slot waits for the block.
const WAITS: u64 = (1 << 62) + (1 << 61);
/// A byte for each slot and writer: the writer of snails opened together that holds the slot, the
/// same slot in each of their files.
const WRITER_OF: u64 = (1 << 62) + (1 << 58);
const WRITER_IDS: u64 = 1 << 40;
/// A run for each slot and each file beside: the writer of snails opened together that holds the
/// slot marks, in each of their files, the path of every other, a lock a byte, so that a writer that
/// never opened those files follows a ring through them.
const BESIDE: u64 = (1 << 62) + (1 << 59);
const BESIDE_FILES: u64 = 1 << 16;
const PATH_BYTES: u64 = 1 << 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Shared,
    Alone,
}

fn flock(kind: libc::c_short, start: u64, len: u64) -> libc::flock {
    // SAFETY: flock is plain data; every field the call reads is set below.
    let mut f: libc::flock = unsafe { std::mem::zeroed() };
    f.l_type = kind;
    f.l_whence = libc::SEEK_SET as libc::c_short;
    f.l_start = start as libc::off_t;
    f.l_len = len as libc::off_t;
    f
}

fn fcntl(file: &File, cmd: libc::c_int, f: &mut libc::flock) -> std::io::Result<()> {
    // SAFETY: the descriptor is open for the life of `file`, and `f` is a valid flock.
    let r = unsafe { libc::fcntl(file.as_raw_fd(), cmd, f as *mut libc::flock) };
    if r == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Takes a lock on bytes `start..start + len`, waiting for it where `wait`. Returns false where it
/// does not wait and another file holds a lock in its way.
fn take(file: &File, kind: Kind, start: u64, len: u64, wait: bool) -> Result<bool> {
    let t = match kind {
        Kind::Shared => libc::F_RDLCK,
        Kind::Alone => libc::F_WRLCK,
    };
    let mut f = flock(t as libc::c_short, start, len);
    let cmd = if wait {
        libc::F_OFD_SETLKW
    } else {
        libc::F_OFD_SETLK
    };
    match fcntl(file, cmd, &mut f) {
        Ok(()) => Ok(true),
        Err(e) if !wait && matches!(e.raw_os_error(), Some(libc::EAGAIN) | Some(libc::EACCES)) => {
            Ok(false)
        }
        Err(e) => Err(Error::new(format!("a lock on the file: {e}"))),
    }
}

fn give(file: &File, start: u64, len: u64) -> Result<()> {
    let mut f = flock(libc::F_UNLCK as libc::c_short, start, len);
    fcntl(file, libc::F_OFD_SETLK, &mut f)
        .map_err(|e| Error::new(format!("a lock on the file: {e}")))
}

/// Holds the generation a round reads, shared: its pages are not freed while it is held.
pub fn hold_generation(file: &File, generation: u64) -> Result<()> {
    take(file, Kind::Shared, GENERATIONS + generation, 1, true).map(|_| ())
}

pub fn let_go_generation(file: &File, generation: u64) -> Result<()> {
    give(file, GENERATIONS + generation, 1)
}

/// Holds every generation from `from` on, shared, while a round reads which generation is newest:
/// no page a later generation might name is freed meanwhile.
pub fn hold_generations_from(file: &File, from: u64) -> Result<()> {
    take(
        file,
        Kind::Shared,
        GENERATIONS + from,
        GENERATION_BYTES - from,
        true,
    )
    .map(|_| ())
}

/// Of the generations held from `from` on, keeps only `generation`.
pub fn keep_generation(file: &File, from: u64, generation: u64) -> Result<()> {
    if generation > from {
        give(file, GENERATIONS + from, generation - from)?;
    }
    give(
        file,
        GENERATIONS + generation + 1,
        GENERATION_BYTES - generation - 1,
    )
}

/// Whether a round of another file holds a generation older than `generation`. It never waits.
pub fn older_held(file: &File, generation: u64) -> Result<bool> {
    if generation == 0 {
        return Ok(false);
    }
    let mut f = flock(libc::F_WRLCK as libc::c_short, GENERATIONS, generation);
    fcntl(file, libc::F_OFD_GETLK, &mut f)
        .map_err(|e| Error::new(format!("a test of the file's locks: {e}")))?;
    Ok(f.l_type != libc::F_UNLCK as libc::c_short)
}

/// Waits until no round of another file holds a generation older than `generation`, as long as it
/// takes, holding no lock a round would wait for.
pub fn wait_older_let_go(file: &File, generation: u64) -> Result<()> {
    let mut nap = std::time::Duration::from_millis(1);
    while older_held(file, generation)? {
        std::thread::sleep(nap);
        nap = (nap * 2).min(std::time::Duration::from_millis(50));
    }
    Ok(())
}

/// A hash of bytes that is the same in every build: FNV-1a, 64 bits.
#[derive(Clone, Copy)]
struct Fnv(u64);

impl Default for Fnv {
    fn default() -> Fnv {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
}

impl std::hash::Hasher for Fnv {
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

/// The byte of a block's lock: its table, order and first key, as they fall among the block bytes.
pub fn block_byte(key: &impl std::hash::Hash) -> u64 {
    use std::hash::{BuildHasher, BuildHasherDefault};
    BLOCKS + BuildHasherDefault::<Fnv>::default().hash_one(key) % BLOCK_BYTES
}

/// The byte of a block's lock from its table's and order's places in the catalog and its first
/// key's bytes, the same in every build of this version.
pub(crate) fn block_byte_of(table: usize, order: usize, key: &[crate::places::KeyValue]) -> u64 {
    use std::hash::Hasher;
    let mut h = Fnv::default();
    h.write(&(table as u64).to_le_bytes());
    h.write(&(order as u64).to_le_bytes());
    for v in key {
        match v {
            crate::places::KeyValue::Int(x) => {
                h.write(&[0]);
                h.write(&x.to_le_bytes());
            }
            crate::places::KeyValue::Text(s) => {
                h.write(&[1]);
                h.write(&(s.len() as u64).to_le_bytes());
                h.write(s.as_bytes());
            }
            crate::places::KeyValue::Null { first } => h.write(&[2 + *first as u8]),
            crate::places::KeyValue::Wide(x) => {
                h.write(&[4]);
                h.write(&x.to_le_bytes());
            }
            crate::places::KeyValue::Bytes(b) => {
                h.write(&[5]);
                h.write(&(b.len() as u64).to_le_bytes());
                h.write(b);
            }
        }
    }
    BLOCKS + h.finish() % BLOCK_BYTES
}

/// The two bytes of a row's lock, its key's and the rest's, from its table's place in the catalog
/// and its key's bytes, the same in every build of this version; they fall among the block bytes.
pub(crate) fn row_bytes(table: usize, key: &[u8]) -> (u64, u64) {
    use std::hash::Hasher;
    let mut h = Fnv::default();
    h.write(b"row");
    h.write(&(table as u64).to_le_bytes());
    h.write(&(key.len() as u64).to_le_bytes());
    h.write(key);
    let k = BLOCKS + (h.finish() % (BLOCK_BYTES / 2)) * 2;
    (k, k + 1)
}

/// The byte a table's lockers hold, shared: rows of it are locked by a question, so its writers
/// name the rows they change.
pub(crate) fn lockers_byte(table: usize) -> u64 {
    block_byte(&(table as u64, "lockers"))
}

/// The byte a table's writers hold, shared, while rows they changed are not named by their own
/// bytes.
pub(crate) fn unnamed_byte(table: usize) -> u64 {
    block_byte(&(table as u64, "unnamed"))
}

/// Takes a lock on one byte, shared or alone, waiting for it where `wait`. Returns false where it
/// does not wait and another file holds a lock in its way.
pub(crate) fn lock_as(file: &File, byte: u64, shared: bool, wait: bool) -> Result<bool> {
    let kind = if shared { Kind::Shared } else { Kind::Alone };
    take(file, kind, byte, 1, wait)
}

/// Whether another file holds a lock on `byte`, shared or alone. It never waits.
pub(crate) fn held_by_other(file: &File, byte: u64) -> Result<bool> {
    Ok(held_at(file, byte, 1)?.is_some())
}
/// Takes a slot for a writer open on the file, held until the file is closed: the first slot no
/// other writer holds.
pub(crate) fn take_slot(file: &File) -> Result<u64> {
    for s in 0..WRITER_SLOTS {
        if take(file, Kind::Alone, WRITERS + s, 1, false)? {
            return Ok(s);
        }
    }
    Err(Error::new("every writer's slot on the file is taken"))
}

/// Takes one slot in every file of snails opened together to write, the same in each, held until
/// they are closed, and marks it in each as the writer `id`'s: the last slot no other writer holds
/// in any of them.
#[cfg(feature = "standalone")]
pub(crate) fn take_slot_across(files: &[&File], id: u64) -> Result<u64> {
    for s in (0..WRITER_SLOTS).rev() {
        let mut taken = Vec::new();
        for f in files {
            if !take(f, Kind::Alone, WRITERS + s, 1, false)? {
                break;
            }
            taken.push(*f);
        }
        if taken.len() == files.len() {
            for f in files {
                take(f, Kind::Alone, WRITER_OF + (s << 40) + id, 1, false)?;
            }
            return Ok(s);
        }
        for f in taken {
            give(f, WRITERS + s, 1)?;
        }
    }
    Err(Error::new(
        "no writer's slot is free in every file of the snails opened together",
    ))
}

/// A writer's mark among those of every process now running: its process and its count there.
#[cfg(feature = "standalone")]
pub(crate) fn writer_id() -> u64 {
    static OPENED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = OPENED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    ((u64::from(std::process::id()) << 18) | (n & 0x3_ffff)) % WRITER_IDS
}

/// The mark of the writer of snails opened together that holds `slot` in another file, if one does.
pub(crate) fn writer_of(file: &File, slot: u64) -> Result<Option<u64>> {
    first_held(file, WRITER_OF + (slot << 40), WRITER_IDS)
}

/// The byte of the `i`th byte of the `k`th path the writer in `slot` marks beside the file.
fn beside_at(slot: u64, k: u64, i: u64) -> u64 {
    BESIDE + (slot << 37) + (k << 21) + (i << 9)
}

/// Marks, in a file of snails opened together to write, that the writer in `slot` writes the files
/// of `paths` too: each byte of each path a lock whose length is the byte and one more.
#[cfg(feature = "standalone")]
pub(crate) fn mark_beside(file: &File, slot: u64, paths: &[&[u8]]) -> Result<()> {
    if paths.len() as u64 > BESIDE_FILES {
        return Err(Error::new(format!(
            "more than {} snails opened together to write",
            BESIDE_FILES + 1
        )));
    }
    for (k, path) in paths.iter().enumerate() {
        if path.len() as u64 > PATH_BYTES {
            return Err(Error::new(format!(
                "a snail opened together with others to write lies at a path longer than \
                 {PATH_BYTES} bytes"
            )));
        }
        for (i, &b) in path.iter().enumerate() {
            take(
                file,
                Kind::Alone,
                beside_at(slot, k as u64, i as u64),
                1 + u64::from(b),
                false,
            )?;
        }
    }
    Ok(())
}

/// The paths of the files the writer in another file's `slot` writes beside it, as it marked them.
pub(crate) fn beside(file: &File, slot: u64) -> Result<Vec<Vec<u8>>> {
    let mut out = Vec::new();
    for k in 0..BESIDE_FILES {
        let mut path = Vec::new();
        for i in 0..PATH_BYTES {
            match held_at(file, beside_at(slot, k, i), 1)? {
                Some((_, len)) => path.push((len - 1) as u8),
                None => break,
            }
        }
        if path.is_empty() {
            break;
        }
        out.push(path);
    }
    Ok(out)
}

/// The writers of other files that wait for a block of this one: for the tests of writers waiting
/// in a ring.
#[doc(hidden)]
pub fn waiting(file: &File) -> Result<u64> {
    fn among(file: &File, start: u64, len: u64) -> Result<u64> {
        if len == 0 {
            return Ok(0);
        }
        Ok(match held_at(file, start, len)? {
            None => 0,
            Some((at, n)) => {
                1 + among(file, start, at - start)? + among(file, at + n, start + len - at - n)?
            }
        })
    }
    among(file, WAITS, WRITER_SLOTS << 40)
}

/// Marks that the writer in `slot` holds the block of lock byte `byte`, or no longer does.
pub(crate) fn mark_held(file: &File, slot: u64, byte: u64, held: bool) -> Result<()> {
    let at = HOLDS + ((byte - BLOCKS) << 16) + slot;
    if held {
        take(file, Kind::Alone, at, 1, false).map(|_| ())
    } else {
        give(file, at, 1)
    }
}

/// Marks that the writer in `slot` waits for the block of lock byte `byte`, or no longer does.
pub(crate) fn mark_waiting(file: &File, slot: u64, byte: u64, waits: bool) -> Result<()> {
    let at = WAITS + (slot << 40) + (byte - BLOCKS);
    if waits {
        take(file, Kind::Alone, at, 1, false).map(|_| ())
    } else {
        give(file, at, 1)
    }
}

/// The first of another file's locks on `start..start + len`, as its first byte's place in the run.
fn first_held(file: &File, start: u64, len: u64) -> Result<Option<u64>> {
    Ok(held_at(file, start, len)?.map(|(at, _)| at - start))
}

/// The first of another file's locks on `start..start + len`, by its first byte and its length.
fn held_at(file: &File, start: u64, len: u64) -> Result<Option<(u64, u64)>> {
    let mut f = flock(libc::F_WRLCK as libc::c_short, start, len);
    fcntl(file, libc::F_OFD_GETLK, &mut f)
        .map_err(|e| Error::new(format!("a test of the file's locks: {e}")))?;
    Ok((f.l_type != libc::F_UNLCK as libc::c_short).then(|| (f.l_start as u64, f.l_len as u64)))
}

#[cfg(test)]
/// The slot of the other writer that holds the block of lock byte `byte`, if one does.
pub(crate) fn holder(file: &File, byte: u64) -> Result<Option<u64>> {
    first_held(file, HOLDS + ((byte - BLOCKS) << 16), WRITER_SLOTS)
}

/// The slots of the other writers that hold the block or row of lock byte `byte`: one where it is
/// held alone, any number where it is shared.
pub(crate) fn holders(file: &File, byte: u64) -> Result<Vec<u64>> {
    let start = HOLDS + ((byte - BLOCKS) << 16);
    let mut out = Vec::new();
    all_held(file, start, WRITER_SLOTS, &mut |at| out.push(at - start))?;
    Ok(out)
}

/// Every lock another file holds on `start..start + len`, by its first byte: the kernel answers with
/// any one of them, so the run is asked again on each side of it.
fn all_held(file: &File, start: u64, len: u64, each: &mut dyn FnMut(u64)) -> Result<()> {
    if len == 0 {
        return Ok(());
    }
    let Some((at, n)) = held_at(file, start, len)? else {
        return Ok(());
    };
    let at = at.max(start);
    let end = at.saturating_add(n.max(1)).min(start + len);
    all_held(file, start, at - start, each)?;
    for byte in at..end {
        each(byte);
    }
    all_held(file, end, start + len - end, each)
}

/// The lock byte of the block the writer in another file's `slot` waits for, if it waits.
pub(crate) fn awaited(file: &File, slot: u64) -> Result<Option<u64>> {
    Ok(first_held(file, WAITS + (slot << 40), BLOCK_BYTES)?.map(|b| BLOCKS + b))
}

/// Locks a block to change it, waiting while another writer has it, or, where `wait` is false,
/// returning false at once.
pub fn lock_block(file: &File, byte: u64, wait: bool) -> Result<bool> {
    take(file, Kind::Alone, byte, 1, wait)
}

pub fn unlock_block(file: &File, byte: u64) -> Result<()> {
    give(file, byte, 1)
}

/// Waits for the root's turn, the one commit at a time.
pub fn take_root_turn(file: &File) -> Result<()> {
    take(file, Kind::Alone, ROOT_TURN, 1, true).map(|_| ())
}

pub fn give_root_turn(file: &File) -> Result<()> {
    give(file, ROOT_TURN, 1)
}

/// The bytes of a serializable transaction's marks, a run each: what it has read and who it is, the
/// edges it found and the transactions it doomed. Each goes with the file that holds it.
const SERIAL: u64 = (1 << 62) + (1 << 60);
/// A byte for each block and slot: the transaction in that slot read the block, gaps and all.
const READ_MARKS: u64 = SERIAL;
/// A byte for each slot and transaction: the serializable transaction in that slot.
const SERIAL_OF: u64 = SERIAL + (1 << 56);
/// A byte for each slot and generation: the snapshot of the transaction in that slot.
const SERIAL_SNAPSHOT: u64 = SERIAL + (2 << 56);
/// A byte for each slot: the transaction in that slot was not declared READ ONLY.
const SERIAL_WRITES: u64 = SERIAL + (3 << 56);
/// A byte for each slot and transaction: the transaction in that slot read what the other wrote.
const EDGE_OUT: u64 = SERIAL + (4 << 56);
/// A byte for each slot and transaction: the other read what the transaction in that slot wrote.
const EDGE_IN: u64 = SERIAL + (5 << 56);
/// A byte for each slot and transaction: the transaction in that slot doomed the other.
const DOOMS: u64 = SERIAL + (6 << 56);
/// A byte for each slot and generation: the transaction in that slot passed its commit's test and
/// its commit is that generation.
const SERIAL_PREPARED: u64 = SERIAL + (7 << 56);
/// The byte whose lock is the turn at the marks: an edge and its test, a commit's test, and a
/// transaction's end, one at a time.
const SERIAL_TURN: u64 = SERIAL + (8 << 56);
const SERIAL_NAMES: u64 = 1 << 40;

/// The byte of an order's own mark, which a read holds for all of it and a writer of any of it.
pub(crate) fn order_byte(table: usize, order: usize) -> u64 {
    block_byte(&(table as u64, order as u64, "order"))
}

/// A serializable transaction's mark among those of every process now running: its process and its
/// count there.
pub(crate) fn serial_id() -> u64 {
    static BEGUN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let n = BEGUN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    ((u64::from(std::process::id()) << 18) | (n & 0x3_ffff)) % SERIAL_NAMES
}

/// Takes or lets go of a mark on one byte.
fn mark(file: &File, byte: u64, on: bool) -> Result<()> {
    match on {
        true => take(file, Kind::Alone, byte, 1, false).map(|_| ()),
        false => give(file, byte, 1),
    }
}

/// Marks that the transaction in `slot` read the block of lock byte `byte`, or no longer does.
pub(crate) fn mark_read(file: &File, slot: u64, byte: u64, on: bool) -> Result<()> {
    mark(file, READ_MARKS + ((byte - BLOCKS) << 16) + slot, on)
}

/// The slots of the other transactions that read the block of lock byte `byte`.
pub(crate) fn readers(file: &File, byte: u64) -> Result<Vec<u64>> {
    let start = READ_MARKS + ((byte - BLOCKS) << 16);
    let mut out = Vec::new();
    all_held(file, start, WRITER_SLOTS, &mut |at| out.push(at - start))?;
    Ok(out)
}

/// Marks the serializable transaction `id` in `slot`, or lets it go.
pub(crate) fn mark_serial(file: &File, slot: u64, id: u64, on: bool) -> Result<()> {
    mark(file, SERIAL_OF + (slot << 40) + id, on)
}

/// Every other serializable transaction running on the file: its slot and its mark.
pub(crate) fn serials(file: &File) -> Result<Vec<(u64, u64)>> {
    let mut out = Vec::new();
    all_held(file, SERIAL_OF, WRITER_SLOTS << 40, &mut |at| {
        out.push(((at - SERIAL_OF) >> 40, (at - SERIAL_OF) % SERIAL_NAMES))
    })?;
    Ok(out)
}

/// Waits until the serializable transaction `id` in another file's `slot` has ended, as long as it
/// takes, holding no lock.
pub(crate) fn wait_serial_end(file: &File, slot: u64, id: u64) -> Result<()> {
    let mut nap = std::time::Duration::from_millis(1);
    while held_at(file, SERIAL_OF + (slot << 40) + id, 1)?.is_some() {
        std::thread::sleep(nap);
        nap = (nap * 2).min(std::time::Duration::from_millis(50));
    }
    Ok(())
}

/// Marks the snapshot of the transaction in `slot`, or lets it go.
pub(crate) fn mark_snapshot(file: &File, slot: u64, generation: u64, on: bool) -> Result<()> {
    mark(file, SERIAL_SNAPSHOT + (slot << 40) + generation, on)
}

/// The snapshot of the transaction in another file's `slot`.
pub(crate) fn snapshot_of(file: &File, slot: u64) -> Result<Option<u64>> {
    first_held(file, SERIAL_SNAPSHOT + (slot << 40), SERIAL_NAMES)
}

/// Marks that the transaction in `slot` was not declared READ ONLY, or lets it go.
pub(crate) fn mark_writes(file: &File, slot: u64, on: bool) -> Result<()> {
    mark(file, SERIAL_WRITES + slot, on)
}

/// Whether the transaction in another file's `slot` was not declared READ ONLY.
pub(crate) fn writes_in(file: &File, slot: u64) -> Result<bool> {
    Ok(first_held(file, SERIAL_WRITES + slot, 1)?.is_some())
}

/// Marks an edge the transaction in `slot` found, out to or in from the transaction `other`, or lets
/// it go.
pub(crate) fn mark_edge(file: &File, slot: u64, out: bool, other: u64, on: bool) -> Result<()> {
    let run = if out { EDGE_OUT } else { EDGE_IN };
    mark(file, run + (slot << 40) + other, on)
}

/// The transactions another file's `slot` found edges with, out to them or in from them.
pub(crate) fn edges_in_slot(file: &File, slot: u64, out: bool) -> Result<Vec<u64>> {
    let start = (if out { EDGE_OUT } else { EDGE_IN }) + (slot << 40);
    let mut found = Vec::new();
    all_held(file, start, SERIAL_NAMES, &mut |at| found.push(at - start))?;
    Ok(found)
}

/// Whether another file's `slot` found an edge with the transaction `other`, out to it or in from it.
pub(crate) fn edge_with(file: &File, slot: u64, out: bool, other: u64) -> Result<bool> {
    let run = if out { EDGE_OUT } else { EDGE_IN };
    Ok(first_held(file, run + (slot << 40) + other, 1)?.is_some())
}

/// Marks that the transaction in `slot` doomed the transaction `other`, or lets it go.
pub(crate) fn mark_doom(file: &File, slot: u64, other: u64, on: bool) -> Result<()> {
    mark(file, DOOMS + (slot << 40) + other, on)
}

/// Whether another file's `slot` doomed the transaction `other`.
pub(crate) fn doomed_by(file: &File, slot: u64, other: u64) -> Result<bool> {
    Ok(first_held(file, DOOMS + (slot << 40) + other, 1)?.is_some())
}

/// Marks that the transaction in `slot` passed its commit's test, its commit being `generation`, or
/// lets it go.
pub(crate) fn mark_prepared(file: &File, slot: u64, generation: u64, on: bool) -> Result<()> {
    mark(file, SERIAL_PREPARED + (slot << 40) + generation, on)
}

/// The commit of the transaction in another file's `slot`, where it passed its test.
pub(crate) fn prepared_of(file: &File, slot: u64) -> Result<Option<u64>> {
    first_held(file, SERIAL_PREPARED + (slot << 40), SERIAL_NAMES)
}

/// Waits for the turn at the marks, as short as an edge's test or a commit's.
pub(crate) fn take_serial_turn(file: &File) -> Result<()> {
    take(file, Kind::Alone, SERIAL_TURN, 1, true).map(|_| ())
}

pub(crate) fn give_serial_turn(file: &File) -> Result<()> {
    give(file, SERIAL_TURN, 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str) -> (std::path::PathBuf, File, File) {
        let path =
            std::env::temp_dir().join(format!("warren-snail-lock-{name}-{}", std::process::id()));
        std::fs::write(&path, b"x").unwrap();
        let open = || {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap()
        };
        let (a, b) = (open(), open());
        (path, a, b)
    }

    #[test]
    fn a_generation_held_by_another_file_is_seen_held_and_one_s_own_is_not() {
        let (path, a, b) = file("generations");
        hold_generation(&a, 5).unwrap();
        assert!(older_held(&b, 6).unwrap());
        assert!(!older_held(&b, 5).unwrap());
        assert!(!older_held(&a, 6).unwrap(), "a file's own lock");
        hold_generation(&b, 5).unwrap();
        let_go_generation(&a, 5).unwrap();
        assert!(!older_held(&b, 6).unwrap());
        assert!(older_held(&a, 6).unwrap());
        drop(b);
        assert!(!older_held(&a, 6).unwrap(), "a closed file's locks go");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_round_finding_its_generation_never_waits_on_a_writer_s_block() {
        let (path, a, b) = file("apart");
        assert!(lock_block(&a, block_byte(&(0usize, 1usize, "x")), false).unwrap());
        take_root_turn(&a).unwrap();
        assert!(take(&b, Kind::Shared, GENERATIONS, GENERATION_BYTES, false).unwrap());
        keep_generation(&b, 0, 7).unwrap();
        assert!(older_held(&a, 8).unwrap());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_round_that_moves_on_keeps_only_the_generation_it_read() {
        let (path, a, b) = file("moves");
        hold_generations_from(&b, 3).unwrap();
        keep_generation(&b, 3, 7).unwrap();
        assert!(older_held(&a, 8).unwrap());
        assert!(
            !older_held(&a, 7).unwrap(),
            "the generations before it are let go"
        );
        hold_generations_from(&b, 7).unwrap();
        keep_generation(&b, 7, 9).unwrap();
        assert!(!older_held(&a, 9).unwrap());
        assert!(older_held(&a, 10).unwrap());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_block_locked_by_one_writer_is_refused_to_another_until_let_go() {
        let (path, a, b) = file("blocks");
        let x = block_byte(&(0usize, 1usize, "x"));
        assert!(lock_block(&a, x, false).unwrap());
        assert!(!lock_block(&b, x, false).unwrap());
        assert!(lock_block(&b, block_byte(&(0usize, 1usize, "y")), false).unwrap());
        unlock_block(&a, x).unwrap();
        assert!(lock_block(&b, x, false).unwrap());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn every_lock_on_a_run_is_found_whichever_file_holds_it_and_however_it_lies() {
        let (path, a, b) = file("all");
        let seen = File::open(&path).unwrap();
        let (x, y) = (
            block_byte(&(0usize, 1usize, "x")),
            block_byte(&(0usize, 1usize, "y")),
        );
        mark_held(&a, 9, x, true).unwrap();
        mark_held(&b, 3, x, true).unwrap();
        mark_held(&a, 2, x, true).unwrap();
        mark_held(&a, 10, x, true).unwrap();
        mark_held(&a, 4, y, true).unwrap();
        let mut slots = holders(&seen, x).unwrap();
        slots.sort_unstable();
        assert_eq!(slots, [2, 3, 9, 10]);
        mark_read(&b, 9, x, true).unwrap();
        mark_read(&a, 3, x, true).unwrap();
        let mut slots = readers(&seen, x).unwrap();
        slots.sort_unstable();
        assert_eq!(slots, [3, 9]);
        mark_serial(&a, 7, 77, true).unwrap();
        mark_serial(&b, 2, 22, true).unwrap();
        let mut running = serials(&seen).unwrap();
        running.sort_unstable();
        assert_eq!(running, [(2, 22), (7, 77)]);
        assert_eq!(serials(&a).unwrap(), [(2, 22)], "a file's own marks");
        drop(b);
        assert_eq!(
            serials(&seen).unwrap(),
            [(7, 77)],
            "a closed file's marks go"
        );
        std::fs::remove_file(path).unwrap();
    }

    /// The lock byte of block (5, 'x') of the third order of the second table.
    const GOLDEN: u64 = 2_305_843_784_697_650_849;

    #[test]
    #[cfg(feature = "standalone")]
    fn a_writers_marks_tell_another_who_holds_a_block_and_what_it_waits_for() {
        use crate::places::KeyValue;
        let (path, a, b) = file("marks");
        // A writer before them takes the first slot.
        let (_, first, _) = file("marks");
        take_slot(&first).unwrap();
        let (sa, sb) = (take_slot(&a).unwrap(), take_slot(&b).unwrap());
        assert!(sa > 0 && sb > 0);
        assert_ne!(sa, sb);
        let x = block_byte_of(0, 0, &[KeyValue::Int(5)]);
        let y = block_byte_of(0, 0, &[KeyValue::Int(6)]);
        mark_held(&a, sa, x, true).unwrap();
        assert_eq!(
            (holder(&b, x).unwrap(), holder(&b, y).unwrap()),
            (Some(sa), None)
        );
        mark_waiting(&a, sa, y, true).unwrap();
        assert_eq!(
            (awaited(&b, sa).unwrap(), awaited(&b, sb).unwrap()),
            (Some(y), None)
        );
        mark_waiting(&a, sa, y, false).unwrap();
        mark_held(&a, sa, x, false).unwrap();
        assert_eq!(
            (holder(&b, x).unwrap(), awaited(&b, sa).unwrap()),
            (None, None)
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    #[cfg(feature = "standalone")]
    fn a_writers_marks_of_the_files_beside_it_are_read_back_by_another_opened_only_to_read() {
        let (path, a, b) = file("beside");
        let seen = File::open(&path).unwrap();
        let far: &[u8] = b"/a/snail/at/the/end/of/a/long/path/\x01\xff.snail";
        mark_beside(&a, 7, &[b"/one.snail", far]).unwrap();
        assert_eq!(
            beside(&seen, 7).unwrap(),
            vec![b"/one.snail".to_vec(), far.to_vec()]
        );
        assert!(
            beside(&seen, 6).unwrap().is_empty(),
            "another slot marks none"
        );
        assert!(beside(&a, 7).unwrap().is_empty(), "a file's own marks");
        let (x, y) = (
            block_byte(&(0usize, 0usize, 1)),
            block_byte(&(0usize, 0usize, 2)),
        );
        mark_waiting(&a, 7, x, true).unwrap();
        mark_waiting(&b, 8, y, true).unwrap();
        assert_eq!(waiting(&seen).unwrap(), 2);
        drop(a);
        assert!(
            beside(&seen, 7).unwrap().is_empty(),
            "a closed file's marks go"
        );
        assert_eq!(waiting(&seen).unwrap(), 1);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_blocks_lock_byte_is_the_same_in_every_build() {
        use crate::places::KeyValue;
        assert_eq!(
            block_byte_of(1, 2, &[KeyValue::Int(5), KeyValue::Text("x".into())]),
            GOLDEN
        );
    }
}
