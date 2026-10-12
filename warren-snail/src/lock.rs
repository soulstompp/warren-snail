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

/// The byte of a block's lock: its table, order and first key, as they fall among the block bytes.
pub fn block_byte(key: &impl std::hash::Hash) -> u64 {
    use std::hash::{BuildHasher, BuildHasherDefault, DefaultHasher};
    BLOCKS + BuildHasherDefault::<DefaultHasher>::default().hash_one(key) % BLOCK_BYTES
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
}
