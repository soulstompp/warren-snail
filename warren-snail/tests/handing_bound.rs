//! What a round handing out its answer holds, as the allocator counts it: however many rows it
//! answers past its room, the most it holds stays near what the room allows; with room for them all,
//! it grows with them.

mod weather;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use warren_snail::catalog::Catalog;
use warren_snail::read::{begin_together, Snail};
use warren_snail::room::Sizes;
use warren_snail::values::{push_datum, Values};
use warren_snail::write::Rows;
use weather::{fresh, tmp, write};

struct Counting;

static NOW: AtomicUsize = AtomicUsize::new(0);
static MOST: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let n = NOW.fetch_add(l.size(), Ordering::Relaxed) + l.size();
        MOST.fetch_max(n, Ordering::Relaxed);
        System.alloc(l)
    }

    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        NOW.fetch_sub(l.size(), Ordering::Relaxed);
        System.dealloc(p, l)
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Held by each test while it counts, so that no other allocates beside it.
static COUNTING: std::sync::Mutex<()> = std::sync::Mutex::new(());

const DDL: &str = "CREATE SCHEMA s; CREATE TABLE s.a (id bigint NOT NULL, PRIMARY KEY (id));\n\
     CREATE TABLE s.b (id bigint NOT NULL, a_id bigint NOT NULL, v bigint NOT NULL, \
     PRIMARY KEY (id)); CREATE INDEX b_a ON s.b (a_id, id) INCLUDE (v);";

const JOIN: &str =
    "SELECT a.id AS c1, b.id AS c2, b.v AS c3 FROM s.a a JOIN s.b b ON b.a_id = a.id";

fn int_rows(catalog: &Catalog, table: &str, row: impl Fn(i64, usize) -> i64, n: i64) -> Rows {
    let t = catalog.table(table).unwrap();
    Rows {
        columns: t
            .columns
            .iter()
            .enumerate()
            .map(|(c, col)| {
                let mut v = Values::new(col.ty);
                for i in 0..n {
                    push_datum(
                        &mut v,
                        &warren_snail::composed::scalar_in(col.ty, &row(i, c).to_string()).unwrap(),
                    );
                }
                v
            })
            .collect(),
    }
}

/// A snail of `n` rows met on a side of few, its answer all of them.
fn snail(n: i64) -> Snail {
    let path = tmp(&format!("handing-bound-{n}.snail"));
    let catalog = Catalog::parse(DDL).unwrap();
    let a = int_rows(&catalog, "s.a", |i, _| i, 64);
    let b = int_rows(
        &catalog,
        "s.b",
        |i, c| match c {
            0 => i,
            1 => i % 64,
            _ => n - i,
        },
        n,
    );
    write(&path, &catalog, &[("s.a".into(), a), ("s.b".into(), b)]).unwrap();
    fresh(&path)
}

/// The most the round allocated past what stood before it, all its answer handed out; the blocks it
/// read; the directory pages it touched and those it was to touch; the rows it answered; and the
/// most its room counted.
fn most_held(snail: &Snail, room: u64) -> (u64, u64, (u64, u64), u64, u64) {
    let before = NOW.load(Ordering::Relaxed);
    MOST.store(before, Ordering::Relaxed);
    let mut a = begin_together(&[snail], JOIN, Sizes::of(room)).unwrap();
    let mut rows = 0;
    while let Some(b) = a.next_batch().unwrap() {
        rows += b.num_rows() as u64;
    }
    let most = (MOST.load(Ordering::Relaxed) - before) as u64;
    let r = a.round();
    assert!(r.spill_pages_written > 0 || room > 16 << 20);
    drop(a);
    (
        most,
        r.blocks_read,
        (r.directory_pages, r.directory_pages_predicted),
        rows,
        r.bytes_held,
    )
}

/// What a plan goes on holding of a block it reads in part, when the room has let its map go: where
/// the map lies, its rows and pages, and its last key.
const BLOCK: u64 = 100;

#[test]
fn the_room_bounds_what_a_round_holds_however_many_rows_it_answers() {
    let _alone = COUNTING.lock().unwrap_or_else(|e| e.into_inner());
    let room = 256u64 << 10;
    // The round's own part, beside its rows, as this build holds it: the question read and planned,
    // its plan's bookkeeping of a read, over a few rows held whole, less those rows.
    let (few, .., counted) = most_held(&snail(64), 1 << 30);
    let own = few.saturating_sub(2 * counted);
    for n in [1i64 << 16, 1 << 18, 1 << 20] {
        let large = snail(n);
        let (most, blocks, (touched, predicted), rows, _) = most_held(&large, room);
        assert_eq!(rows, n as u64);
        assert_eq!(touched, predicted, "{n}");
        // The round's own part as well as the rows: the rows take about twice the room, their
        // cells' own bytes beside their held widths, and the plan a hundred bytes a block.
        assert!(
            most <= own + 2 * room + blocks * BLOCK,
            "{n} rows, {blocks} blocks: {most} bytes at most, {own} the round's own"
        );
    }
    // Held whole, they grow with the rows.
    let (whole, ..) = most_held(&snail(1 << 18), 1 << 30);
    assert!(whole > 4 * room, "{whole} bytes held whole");
}

/// The most a round making a set's rows allocated past what stood before it, all its answer handed
/// out, and the rows it answered.
fn most_made(snail: &Snail, sql: &str, room: u64) -> (u64, u64) {
    let before = NOW.load(Ordering::Relaxed);
    MOST.store(before, Ordering::Relaxed);
    let mut a = begin_together(&[snail], sql, Sizes::of(room)).unwrap();
    let mut rows = 0;
    while let Some(b) = a.next_batch().unwrap() {
        rows += b.num_rows() as u64;
    }
    let most = (MOST.load(Ordering::Relaxed) - before) as u64;
    drop(a);
    (most, rows)
}

// The rows one row's set makes are handed on as they are made, a chunk at a time: however many it
// makes past the room, the most the round holds stays near what the room allows.
#[test]
fn a_set_s_rows_are_handed_on_as_they_are_made() {
    let _alone = COUNTING.lock().unwrap_or_else(|e| e.into_inner());
    let room = 256u64 << 10;
    let snail = snail(32);
    let set = |n: i64| format!("SELECT generate_series(1, {n}) AS g");
    let (few, _) = most_made(&snail, &set(64), 1 << 30);
    for n in [1i64 << 16, 1 << 18, 1 << 20] {
        let (most, rows) = most_made(&snail, &set(n), room);
        assert_eq!(rows, n as u64);
        assert!(
            most <= few + 2 * room,
            "{n} rows: {most} bytes at most, {few} for 64 rows"
        );
    }
}
