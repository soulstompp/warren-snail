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

const DDL: &str = "CREATE TABLE s.a (id bigint NOT NULL, PRIMARY KEY (id));\n\
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

/// What the plan of a read holds of a block: its entry and its page map, whichever way the rows lie.
const PLAN: u64 = 4 << 10;

/// The most the round allocated past what stood before it and what its plan holds, all its answer
/// handed out; and the rows it answered.
fn most_held(snail: &Snail, room: u64) -> (u64, u64) {
    let before = NOW.load(Ordering::Relaxed);
    MOST.store(before, Ordering::Relaxed);
    let mut a = begin_together(&[snail], JOIN, Sizes::of(room)).unwrap();
    let mut rows = 0;
    while let Some(b) = a.next_batch().unwrap() {
        rows += b.num_rows() as u64;
    }
    let most = (MOST.load(Ordering::Relaxed) - before) as u64;
    assert!(a.round().spill_pages_written > 0 || room > 16 << 20);
    let blocks = a.round().blocks_read;
    drop(a);
    (most.saturating_sub(blocks * PLAN), rows)
}

#[test]
fn the_room_bounds_what_a_round_holds_however_many_rows_it_answers() {
    let room = 256u64 << 10;
    let small = snail(1 << 16);
    let large = snail(1 << 18);
    let (a, rows_a) = most_held(&small, room);
    let (b, rows_b) = most_held(&large, room);
    assert_eq!((rows_a, rows_b), (1 << 16, 1 << 18));
    assert!(a <= 2 * room && b <= 2 * room, "{a} and {b} bytes at most");
    // Held whole, they grow with the rows.
    let (whole, _) = most_held(&large, 1 << 30);
    assert!(whole > 4 * room, "{whole} bytes held whole");
}
