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
#[cfg(feature = "standalone")]
const WRITERS: u64 = 1 << 59;
const WRITER_SLOTS: u64 = 1 << 16;
/// A byte for each block and slot: the writer in that slot holds the block.
const HOLDS: u64 = 1 << 62;
/// A byte for each slot and block: the writer in that slot waits for the block.
const WAITS: u64 = (1 << 62) + (1 << 61);

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
        }
    }
    BLOCKS + h.finish() % BLOCK_BYTES
}

/// Takes a slot for a writer open on the file, held until the file is closed: the first slot no
/// other writer holds.
#[cfg(feature = "standalone")]
pub(crate) fn take_slot(file: &File) -> Result<u64> {
    for s in 0..WRITER_SLOTS {
        if take(file, Kind::Alone, WRITERS + s, 1, false)? {
            return Ok(s);
        }
    }
    Err(Error::new("every writer's slot on the file is taken"))
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
    let mut f = flock(libc::F_WRLCK as libc::c_short, start, len);
    fcntl(file, libc::F_OFD_GETLK, &mut f)
        .map_err(|e| Error::new(format!("a test of the file's locks: {e}")))?;
    Ok((f.l_type != libc::F_UNLCK as libc::c_short).then(|| f.l_start as u64 - start))
}

/// The slot of the other writer that holds the block of lock byte `byte`, if one does.
pub(crate) fn holder(file: &File, byte: u64) -> Result<Option<u64>> {
    first_held(file, HOLDS + ((byte - BLOCKS) << 16), WRITER_SLOTS)
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
    fn a_blocks_lock_byte_is_the_same_in_every_build() {
        use crate::places::KeyValue;
        assert_eq!(
            block_byte_of(1, 2, &[KeyValue::Int(5), KeyValue::Text("x".into())]),
            GOLDEN
        );
    }
}
