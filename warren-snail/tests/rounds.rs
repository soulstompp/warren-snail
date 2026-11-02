//! A round's places on a table that cuts: a binary tree of 2^14 - 1 rows, a node's children 2c and
//! 2c + 1, ordered by its parent, so a node's children lie together and the root last. Each read of
//! a statement is priced from its directory once the rows before it are known, and reads exactly
//! its pages less those the round holds; the statement's pages are its reads'. A table met with rows
//! made before it is kept to their keys; no page is read twice in one round; and a round past its
//! room writes the rows past it once and reads them back once, to a file of its own.

mod weather;

use std::path::PathBuf;
use warren_snail::catalog::Catalog;
use warren_snail::read::{Round, Snail};
use warren_snail::room::Sizes;
use warren_snail::values::{push_datum, Values};
use warren_snail::write::Rows;
use weather::{tmp, write};

/// The tree's nodes: ids 1 to 2^14 - 1.
const N: i64 = (1 << 14) - 1;

/// The root's parent, past every id, so the root lies last.
const ROOT_PARENT: i64 = 1 << 14;

const DDL: &str = "CREATE TABLE s.t (parent bigint NOT NULL, id bigint NOT NULL, PRIMARY KEY (parent, id));";

/// The tree, as a snail of its own.
fn tree(name: &str) -> PathBuf {
    let path = tmp(&format!("rounds-{name}.snail"));
    let catalog = Catalog::parse(DDL).unwrap();
    let table = catalog.table("s.t").unwrap();
    let parent = |id: i64| if id == 1 { ROOT_PARENT } else { id / 2 };
    let columns = table
        .columns
        .iter()
        .enumerate()
        .map(|(c, col)| {
            let mut v = Values::new(col.ty);
            for id in 1..=N {
                let x = if c == 0 { parent(id) } else { id };
                push_datum(
                    &mut v,
                    &warren_snail::composed::scalar_in(col.ty, &x.to_string()).unwrap(),
                );
            }
            v
        })
        .collect();
    write(&path, &catalog, &[("s.t".into(), Rows { columns })]).unwrap();
    path
}

/// A question's one integer.
fn one(b: &arrow::array::RecordBatch) -> i64 {
    let rows = warren_snail::read::pipeline_rows(b).unwrap();
    match &rows[..] {
        [r] => match &r[..] {
            [warren_snail::read::Value::Int(v)] => *v,
            [warren_snail::read::Value::Num(v, 0)] => *v as i64,
            other => panic!("one integer: {other:?}"),
        },
        other => panic!("one row: {other:?}"),
    }
}

/// Every read of a round priced exactly before it was read, and the round's pages their sum.
fn each_read_exact(q: &str, round: &Round) {
    assert!(!round.reads.is_empty(), "{q}: no read");
    let (mut predicted, mut held, mut read) = (0, 0, 0);
    for (i, r) in round.reads.iter().enumerate() {
        assert_eq!(
            r.column_pages_read,
            r.column_pages_predicted - r.column_pages_held,
            "{q}: read {i}: {r:?}"
        );
        assert!(
            r.column_pages_low <= r.column_pages_predicted
                && r.column_pages_predicted <= r.column_pages_high,
            "{q}: read {i}: {r:?}"
        );
        predicted += r.column_pages_predicted;
        held += r.column_pages_held;
        read += r.column_pages_read;
    }
    assert_eq!(
        (
            round.column_pages_predicted,
            round.column_pages_held,
            round.column_pages_read
        ),
        (predicted, held, read),
        "{q}"
    );
}

/// The column pages of the whole table, as a read of every row reads them.
fn whole(path: &PathBuf) -> u64 {
    let mut snail = Snail::open(path).unwrap();
    let (_, r) = snail
        .ask("SELECT t.parent, t.id FROM s.t t", None)
        .unwrap();
    r.column_pages_predicted
}

/// A walk of the tree from node `from`: its nodes counted.
fn walk(from: i64) -> String {
    format!(
        "WITH RECURSIVE w (id) AS (SELECT CAST({from} AS bigint) UNION ALL \
         SELECT t.id FROM s.t t JOIN w ON t.parent = w.id) SELECT count(*) FROM w"
    )
}

/// The nodes under `from`, itself among them, and the levels they lie on.
fn under(from: i64) -> (i64, usize) {
    let (mut nodes, mut levels, mut width) = (0, 0, 1);
    let mut first = from;
    while first <= N {
        nodes += width.min(N - first + 1);
        levels += 1;
        first *= 2;
        width *= 2;
    }
    (nodes, levels)
}

#[test]
fn a_recursion_reads_its_table_kept_to_each_run_s_keys_and_no_page_twice() {
    let path = tree("walk");
    let table = whole(&path);
    for from in [1, 128] {
        let q = walk(from);
        let mut snail = Snail::open(&path).unwrap();
        let (b, round) = snail.ask(&q, None).unwrap();
        let (nodes, levels) = under(from);
        assert_eq!(one(&b), nodes, "{q}");
        // One read a run, the last adding nothing.
        assert_eq!(round.reads.len(), levels, "{q}");
        each_read_exact(&q, &round);
        // Each run reads only the pages its keys lie on, so the runs together ask for less than the
        // table each time; and no page comes from the file twice.
        assert!(
            round.column_pages_predicted < levels as u64 * table,
            "{q}: {round:?}"
        );
        assert!(round.column_pages_read <= table, "{q}: {round:?}");
        if from == 1 {
            assert_eq!(round.column_pages_read, table, "{q}: every page once");
        }
        eprintln!(
            "{from}: runs {} asked {} read {} of {table}",
            round.reads.len(),
            round.column_pages_predicted,
            round.column_pages_read
        );
    }
}

/// A question asked of the tree on a snail opened afresh: its answer's one integer, and its round,
/// every read exact.
fn asked(path: &PathBuf, q: &str) -> (i64, Round) {
    let mut snail = Snail::open(path).unwrap();
    let (b, round) = snail.ask(q, None).unwrap_or_else(|e| panic!("{q}: {e}"));
    each_read_exact(q, &round);
    (one(&b), round)
}

#[test]
fn a_table_met_with_rows_made_before_it_is_kept_to_their_keys_in_each_place() {
    let path = tree("places");
    let table = whole(&path);
    // Node 3000's two children are 6000 and 6001; their four, 12000 to 12003, sum to 48,006.
    let step = "WITH k AS (SELECT t.id FROM s.t t WHERE t.parent = 3000) \
                SELECT sum(u.id) FROM k JOIN s.t u ON u.parent = k.id";
    let comma = "SELECT sum(u.id) FROM (SELECT t.id FROM s.t t WHERE t.parent = 3000) k, s.t u \
                 WHERE u.parent = k.id";
    // The children of 4000 to 4003 are 8000 to 8007, each a parent: 8 of them.
    let subquery = "SELECT count(*) FROM s.t t WHERE t.parent BETWEEN 4000 AND 4003 \
                    AND EXISTS (SELECT 1 FROM s.t u WHERE u.parent = t.id)";
    // The children of 100 to 110, 200 to 221, are parents: an INTERSECT keeps the 22; an EXCEPT of
    // the children of 8000 to 8191, 16000 to 16383, keeps all 384, none of them a parent.
    let intersect = "SELECT count(*) FROM (SELECT t.id FROM s.t t WHERE t.parent BETWEEN 100 AND 110 \
                     INTERSECT SELECT u.parent FROM s.t u) x";
    let except = "SELECT count(*) FROM (SELECT t.id FROM s.t t WHERE t.parent BETWEEN 8000 AND 8191 \
                  EXCEPT SELECT u.parent FROM s.t u) x";
    for (q, want, reads) in [
        (step, 48_006, 2),
        (comma, 48_006, 2),
        (subquery, 8, 2),
        (intersect, 22, 2),
        (except, 384, 2),
    ] {
        let (got, round) = asked(&path, q);
        assert_eq!(got, want, "{q}");
        assert_eq!(round.reads.len(), reads, "{q}: {round:?}");
        // The table read second, kept to the first's keys, reads its blocks in part of the table.
        let kept = &round.reads[1];
        assert!(
            kept.blocks_read > 0 && kept.column_pages_predicted < table / 4,
            "{q}: {kept:?}"
        );
        eprintln!(
            "{q}\n  kept read: predicted {} held {} read {}, blocks {}",
            kept.column_pages_predicted,
            kept.column_pages_held,
            kept.column_pages_read,
            kept.blocks_read
        );
    }
}

#[test]
fn a_page_read_once_in_a_round_is_not_read_again() {
    let path = tree("once");
    let table = whole(&path);
    // Two arms on one table read it twice, the second from the pages the first kept: every one of
    // them held, none from the file.
    let q = "SELECT count(*) FROM (SELECT t.id FROM s.t t UNION ALL SELECT u.id FROM s.t u) x";
    let (got, round) = asked(&path, q);
    assert_eq!(got, 2 * N);
    assert_eq!(round.reads.len(), 2);
    assert_eq!(round.reads[0].column_pages_read, table / 2, "{q}: {round:?}");
    assert_eq!(
        (round.reads[1].column_pages_held, round.reads[1].column_pages_read),
        (table / 2, 0),
        "{q}: {round:?}"
    );
    // With no room to keep them in, and none held between rounds, the second reads them again.
    #[cfg(feature = "standalone")]
    let mut snail = Snail::open_holding(&path, 0).unwrap();
    #[cfg(not(feature = "standalone"))]
    let mut snail = Snail::open(&path).unwrap();
    snail.size_rounds(Sizes::of(0));
    let (_, round) = snail.ask(q, None).unwrap();
    each_read_exact(q, &round);
    assert_eq!(round.column_pages_read, table, "{q}: {round:?}");
}

/// A directory of the test's own, for a round's spill.
fn spill_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("rounds-spill-{name}"));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

#[test]
fn a_step_past_the_round_s_room_writes_the_rows_past_it_once_and_reads_them_back_once() {
    let path = tree("spill");
    let dir = spill_dir("step");
    // The step holds every node, two integers a row, 9 bytes each as held: 18 bytes a row. A room
    // of 65,536 bytes holds 3,640 of them; the other 12,743, 229,374 bytes, are 56 pages.
    let room: u64 = 65_536;
    let row = 18;
    let held = room / row;
    let past = (N as u64 - held) * row;
    let pages = past.div_ceil(4096);
    assert_eq!((held, pages), (3_640, 56));
    let q = "SELECT sum(a.id) FROM (SELECT t.parent, t.id FROM s.t t) a";
    let mut snail = Snail::open(&path).unwrap();
    snail.size_rounds(Sizes::of(room).in_dir(&dir));
    let (b, round) = snail.ask(q, None).unwrap();
    each_read_exact(q, &round);
    assert_eq!(one(&b), N * (N + 1) / 2, "{q}");
    assert_eq!(
        (round.spill_pages_written, round.spill_pages_read),
        (pages, pages),
        "{q}: {round:?}"
    );
    // The step reading it makes its FROM whole before it reads it: the most held is every row, as
    // it is read back, beside what the room keeps.
    assert!(
        round.bytes_held >= N as u64 * row + held * row
            && round.bytes_held <= N as u64 * row + room,
        "{q}: {round:?}"
    );
    assert_eq!(
        std::fs::read_dir(&dir).unwrap().count(),
        0,
        "the spill is removed as soon as it is opened"
    );
    // Given room for every row, it writes nothing.
    snail.size_rounds(Sizes::of(1 << 20).in_dir(&dir));
    let (_, round) = snail.ask(q, None).unwrap();
    assert_eq!((round.spill_pages_written, round.spill_pages_read), (0, 0));
    // Past the most its opener lets it write, it is refused by name.
    snail.size_rounds(Sizes::of(room).in_dir(&dir).at_most(10 * 4096));
    let refused = snail.ask(q, None).unwrap_err().to_string();
    assert!(refused.contains("lets it spill"), "{refused}");
}

/// Each row's integers.
fn ints(b: &arrow::array::RecordBatch) -> Vec<Vec<i64>> {
    warren_snail::read::pipeline_rows(b)
        .unwrap()
        .into_iter()
        .map(|r| {
            r.into_iter()
                .map(|v| match v {
                    warren_snail::read::Value::Int(x) => x,
                    other => panic!("an integer: {other:?}"),
                })
                .collect()
        })
        .collect()
}

#[test]
fn a_window_past_the_round_s_room_holds_every_partition_whole_and_writes_the_rows_past_it_once() {
    let path = tree("window");
    let dir = spill_dir("window");
    // Every node, partitioned by its id modulo 64 and numbered by id: node i is the
    // ((i - i % 64) / 64)-th of its partition, one more where i % 64 is not 0.
    let q = "SELECT t.id, row_number() OVER (PARTITION BY t.id % 64 ORDER BY t.id) FROM s.t t";
    let rank = |i: i64| (i - i % 64) / 64 + i64::from(i % 64 != 0);
    // The step held whole is every row as the table's read gives it, its two integers 18 bytes:
    // 196,608 bytes of room hold 10,922, the other 5,461 are 98,298 bytes, 24 pages, and fewer than
    // the room holds, so they are written once.
    let room: u64 = 196_608;
    let held = room / 18;
    let pages = ((N as u64 - held) * 18).div_ceil(4096);
    assert_eq!((held, pages), (10_922, 24));
    for (sizes, written) in [
        (Sizes::of(room).in_dir(&dir), pages),
        (Sizes::default().in_dir(&dir), 0),
    ] {
        let size = sizes.bytes;
        let mut snail = Snail::open(&path).unwrap();
        snail.size_rounds(sizes);
        let (b, round) = snail.ask(q, None).unwrap();
        weather::exact(q, &round);
        let mut got = ints(&b);
        got.sort();
        assert_eq!(got.len() as i64, N);
        for (k, r) in got.iter().enumerate() {
            assert_eq!(r, &vec![k as i64 + 1, rank(k as i64 + 1)], "{q}");
        }
        assert_eq!(
            (round.spill_pages_written, round.spill_pages_read),
            (written, written),
            "{q}: {round:?}"
        );
        assert!(round.bytes_held <= size, "{q}: {round:?}");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    }
}

/// Purchases: 16,384, each of one of 512 owners spread over them, at its own moment.
const P_DDL: &str = "CREATE TABLE s.p (id bigint NOT NULL, owner bigint NOT NULL, at bigint NOT NULL, \
                     PRIMARY KEY (id)); CREATE INDEX p_owner ON s.p (owner, at) INCLUDE (id);";
const PURCHASES: i64 = 1 << 14;

fn owner(i: i64) -> i64 {
    i * 7919 % 512
}

fn purchases(name: &str) -> PathBuf {
    let path = tmp(&format!("rounds-{name}.snail"));
    let catalog = Catalog::parse(P_DDL).unwrap();
    let table = catalog.table("s.p").unwrap();
    let columns = table
        .columns
        .iter()
        .enumerate()
        .map(|(c, col)| {
            let mut v = Values::new(col.ty);
            for i in 0..PURCHASES {
                let x = match c {
                    0 => i,
                    1 => owner(i),
                    _ => PURCHASES - i,
                };
                push_datum(
                    &mut v,
                    &warren_snail::composed::scalar_in(col.ty, &x.to_string()).unwrap(),
                );
            }
            v
        })
        .collect();
    write(&path, &catalog, &[("s.p".into(), Rows { columns })]).unwrap();
    path
}

/// Each purchase's place among its owner's by moment, as (id, place), in id order.
fn places(rows: &[(i64, i64, i64)]) -> Vec<Vec<i64>> {
    let mut by: std::collections::BTreeMap<i64, Vec<(i64, i64)>> = Default::default();
    for &(id, o, at) in rows {
        by.entry(o).or_default().push((at, id));
    }
    let mut out = Vec::new();
    for (_, mut v) in by {
        v.sort();
        for (k, (_, id)) in v.into_iter().enumerate() {
            out.push(vec![id, k as i64 + 1]);
        }
    }
    out.sort();
    out
}

const BY_OWNER: &str =
    "SELECT p.id, row_number() OVER (PARTITION BY p.owner ORDER BY p.at) FROM s.p p";

#[test]
fn a_window_a_strand_gives_is_read_along_it_a_partition_at_a_time_where_its_price_is_fewest() {
    let path = purchases("strand");
    let dir = spill_dir("strand");
    let rows: Vec<(i64, i64, i64)> = (0..PURCHASES)
        .map(|i| (i, owner(i), PURCHASES - i))
        .collect();
    let want = places(&rows);
    // Each order's pages for every column, from its directory.
    let base = "SELECT p.id, p.owner, p.at FROM s.p p";
    let pages = |order: &str| {
        Snail::open(&path)
            .unwrap()
            .price(base, Some(order))
            .unwrap()
            .column_pages
    };
    let (own, strand) = (pages("p_pkey"), pages("p_owner"));
    // Held whole along the table's own order, every purchase is three integers, 27 bytes.
    let held = PURCHASES as u64 * 27;
    for room in [64 << 10, 128 << 20] {
        let sort = 2 * held.saturating_sub(room).div_ceil(4096);
        let wins = strand < own + sort;
        let mut snail = Snail::open(&path).unwrap();
        snail.size_rounds(Sizes::of(room).in_dir(&dir));
        let (b, round) = snail.ask(BY_OWNER, None).unwrap();
        weather::exact(BY_OWNER, &round);
        let mut got = ints(&b);
        got.sort();
        assert_eq!(got, want, "{room}");
        assert_eq!(
            round.order,
            if wins { "p_owner" } else { "p_pkey" },
            "{room}: {own} against {strand} with {sort}"
        );
        if wins {
            // One owner's purchases held at a time, nothing written.
            assert_eq!((round.spill_pages_written, round.spill_pages_read), (0, 0));
            assert!(round.bytes_held <= room, "{round:?}");
        }
        eprintln!(
            "{room}: own {own}, strand {strand}, sort {sort}: read along {}, {} pages, held {}",
            round.order, round.column_pages_predicted, round.bytes_held
        );
    }
}

#[cfg(feature = "standalone")]
#[test]
fn a_strand_s_rows_written_after_its_blocks_were_cut_are_put_in_their_place_first() {
    let path = purchases("tails");
    let mut rows: Vec<(i64, i64, i64)> = (0..PURCHASES)
        .map(|i| (i, owner(i), PURCHASES - i))
        .collect();
    // Purchases made after the cut, each at a moment before every one its owner had: they go after
    // each block's sorted rows, and come first in their partitions.
    let mut w = Snail::open_writing(&path).unwrap();
    for k in 0..64 {
        let (id, o, at) = (PURCHASES + k, (k * 37) % 512, -k);
        w.execute(&format!("INSERT INTO s.p VALUES ({id}, {o}, {at})"))
            .unwrap();
        rows.push((id, o, at));
    }
    drop(w);
    let mut snail = Snail::open(&path).unwrap();
    snail.size_rounds(Sizes::of(64 << 10));
    let (b, round) = snail.ask(BY_OWNER, None).unwrap();
    weather::exact(BY_OWNER, &round);
    assert_eq!(round.order, "p_owner");
    let mut got = ints(&b);
    got.sort();
    assert_eq!(got, places(&rows));
}
