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

const DDL: &str =
    "CREATE SCHEMA s; CREATE TABLE s.t (parent bigint NOT NULL, id bigint NOT NULL, PRIMARY KEY (parent, id));";

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
            r.directory_pages, r.directory_pages_predicted,
            "{q}: read {i}: {r:?}"
        );
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
    let (_, r) = snail.ask("SELECT t.parent, t.id FROM s.t t", None).unwrap();
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
    let intersect =
        "SELECT count(*) FROM (SELECT t.id FROM s.t t WHERE t.parent BETWEEN 100 AND 110 \
                     INTERSECT SELECT u.parent FROM s.t u) x";
    let except =
        "SELECT count(*) FROM (SELECT t.id FROM s.t t WHERE t.parent BETWEEN 8000 AND 8191 \
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
    }
    // A table a RIGHT JOIN keeps whole is never kept to the other side's keys: every node, four of
    // them met, read of the one column of its the question names.
    let right =
        "SELECT count(*), count(k.id) FROM (SELECT t.id FROM s.t t WHERE t.parent = 3000) k \
                 RIGHT JOIN s.t u ON u.parent = k.id";
    let mut snail = Snail::open(&path).unwrap();
    let (b, round) = snail.ask(right, None).unwrap();
    each_read_exact(right, &round);
    let rows = warren_snail::read::pipeline_rows(&b).unwrap();
    assert_eq!(
        format!("{rows:?}"),
        format!(
            "{:?}",
            [[
                warren_snail::read::Value::Int(N),
                warren_snail::read::Value::Int(4)
            ]]
        ),
        "{right}"
    );
    assert_eq!(
        round.reads[1].column_pages_predicted,
        table / 2,
        "{right}: {round:?}"
    );
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
    assert_eq!(
        round.reads[0].column_pages_read,
        table / 2,
        "{q}: {round:?}"
    );
    assert_eq!(
        (
            round.reads[1].column_pages_held,
            round.reads[1].column_pages_read
        ),
        (table / 2, 0),
        "{q}: {round:?}"
    );
    // With nothing held between rounds, the round's room keeps them.
    #[cfg(feature = "standalone")]
    {
        let mut snail = Snail::open_holding(&path, 0).unwrap();
        let (_, round) = snail.ask(q, None).unwrap();
        each_read_exact(q, &round);
        assert_eq!(
            (round.reads[1].column_pages_held, round.column_pages_read),
            (table / 2, table / 2),
            "{q}: {round:?}"
        );
    }
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

/// What one chunk of rows of `row` bytes on the spill holds in hand, at most: a block's rows as the
/// table hands them on, or the rows that first pass four pages, handed back from the spill; a row of
/// integers, `row / 9` of them, in hand as `in_hand` holds it.
fn one_chunk(path: &PathBuf, row: u64) -> u64 {
    let block = Snail::open(path)
        .unwrap()
        .blocks()
        .unwrap()
        .iter()
        .map(|b| b.rows)
        .max()
        .unwrap();
    block.max((4 * 4096u64).div_ceil(row)) * in_hand(row / 9) + FOLD
}

/// The bytes a row of `values` values holds in hand, handed from place to place: its vector and its
/// values.
fn in_hand(values: u64) -> u64 {
    (std::mem::size_of::<Vec<warren_snail::read::Value>>()
        + values as usize * std::mem::size_of::<warren_snail::read::Value>()) as u64
}

/// The bytes the first `rows` rows of `values` integers take held column-wise: each integer 8 bytes,
/// and a word of NULL bits a column for every 64 rows begun.
fn held_ints(rows: u64, values: u64) -> u64 {
    values * (8 * rows + 8 * rows.div_ceil(64))
}

/// How many rows of `values` integers `bytes` hold column-wise.
fn rows_in(bytes: u64, values: u64) -> u64 {
    let mut rows = bytes / (9 * values);
    while held_ints(rows + 1, values) <= bytes {
        rows += 1;
    }
    while held_ints(rows, values) > bytes {
        rows -= 1;
    }
    rows
}

/// What a fold of no GROUP BY holds beside: its one group's first row and folds, at most.
const FOLD: u64 = 64;

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
    // The step holds every node, two integers a row held column-wise, 8 bytes each and a word of
    // NULL bits a column every 64 rows: a room of 65,536 bytes holds 4,032 of them, 65,520 bytes; the
    // other 12,351, 18 bytes each on the spill, are 222,318 bytes, 55 pages. The read's plan takes
    // part of the room: while it takes under 2,656 bytes, 3,868 rows or more are held and no page
    // more is written.
    let room: u64 = 65_536;
    let row = 18;
    let held = rows_in(room, 2);
    let past = (N as u64 - held) * row;
    let pages = past.div_ceil(4096);
    assert_eq!((held, pages), (4_032, 55));
    let least_held = rows_in(room - 2_656, 2);
    assert_eq!(((N as u64 - least_held) * row).div_ceil(4096), pages);
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
    // The step reading it is handed its FROM a chunk at a time: the most held is the room, and what
    // one chunk holds in hand, the rows of a block of the table or of four pages of the spill.
    assert!(
        round.bytes_held >= held_ints(least_held, 2) + 3 * 4096
            && round.bytes_held <= room + one_chunk(&path, row),
        "{q}: {round:?}"
    );
    eprintln!("room {room}: most held {}", round.bytes_held);
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
    // The step holds every row as the table's read gives it, keyed by its partition's hash: each row
    // one string of bytes as the spill writes it, its two integers 18 bytes, beside the 40 bytes that
    // find it (its hash, its place, the string's box, its bytes): 58 bytes a row. The 16,383 rows to
    // come are 950,214 bytes, within two rooms of 512 KiB, so the rows past the room are written as
    // one part, once. The read's plan holds a block read's 40 bytes for each of the table's 31
    // blocks, 1,240: the room holds 9,018 rows, and the other 7,365 at 18 bytes are 132,570 bytes, 33
    // pages, 33 for any share of the plan's from none to 9,596 bytes.
    let room: u64 = 512 << 10;
    let row = 18 + 40;
    assert!(N as u64 * row <= 2 * room);
    let rows_held = |plan: u64| (room - plan) / row;
    let pages = ((N as u64 - rows_held(31 * 40)) * 18).div_ceil(4096);
    assert_eq!((rows_held(31 * 40), pages), (9_018, 33));
    for plan in [0, 9_596] {
        assert_eq!(((N as u64 - rows_held(plan)) * 18).div_ceil(4096), pages);
    }
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
        // Beside the room, one chunk in hand: the part written is read back a chunk at a time.
        assert!(
            round.bytes_held <= size + one_chunk(&path, 18),
            "{q}: {round:?}"
        );
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    }
}

/// Purchases: 16,384, each of one of 512 owners spread over them, at its own moment.
const P_DDL: &str =
    "CREATE SCHEMA s; CREATE TABLE s.p (id bigint NOT NULL, owner bigint NOT NULL, at bigint NOT NULL, \
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
    // A scan along the strand, ordered as it leads, hands out each block's rows in order: block by
    // block, every row in its place.
    let q = "SELECT p.owner, p.at, p.id FROM s.p p ORDER BY p.owner, p.at";
    let mut scan = snail.scan(q, Some("p_owner")).unwrap();
    let mut blocks = 0;
    let mut all = Vec::new();
    while let Some(b) = snail.next_block(&mut scan).unwrap() {
        blocks += 1;
        all.extend(ints(&b));
    }
    let mut want: Vec<Vec<i64>> = rows.iter().map(|&(id, o, at)| vec![o, at, id]).collect();
    want.sort();
    assert!(blocks > 1, "{blocks}");
    assert_eq!(all, want);
}

#[test]
fn a_table_kept_to_a_few_keys_reads_their_block_in_part() {
    // Beside the tree, 16,384 rows keyed by a 4-byte integer, a page of it 1,024 rows, so a block is
    // 1,024 rows; its two 8-byte columns take 2 pages each in every block.
    let path = tmp("rounds-part.snail");
    let ddl = format!(
        "{DDL} CREATE TABLE s.w (k integer NOT NULL, v bigint NOT NULL, x bigint NOT NULL, \
         PRIMARY KEY (k));"
    );
    let catalog = Catalog::parse(&ddl).unwrap();
    let column =
        |table: &str, c: usize, f: &dyn Fn(i64) -> i64, n: std::ops::RangeInclusive<i64>| {
            let col = &catalog.table(table).unwrap().columns[c];
            let mut v = Values::new(col.ty);
            for i in n {
                push_datum(
                    &mut v,
                    &warren_snail::composed::scalar_in(col.ty, &f(i).to_string()).unwrap(),
                );
            }
            v
        };
    let parent = |id: i64| if id == 1 { ROOT_PARENT } else { id / 2 };
    let tree = Rows {
        columns: vec![
            column("s.t", 0, &parent, 1..=N),
            column("s.t", 1, &|i| i, 1..=N),
        ],
    };
    let wide = Rows {
        columns: vec![
            column("s.w", 0, &|i| i, 0..=(1 << 14) - 1),
            column("s.w", 1, &|i| 3 * i, 0..=(1 << 14) - 1),
            column("s.w", 2, &|i| -i, 0..=(1 << 14) - 1),
        ],
    };
    write(
        &path,
        &catalog,
        &[("s.t".into(), tree), ("s.w".into(), wide)],
    )
    .unwrap();
    // Node 3000's children, 6000 and 6001, lie in the block of 5,120 to 6,143, at its 880th and 881st
    // rows: the key's one page and the second of v's two.
    let q = "SELECT sum(w.v) FROM (SELECT t.id FROM s.t t WHERE t.parent = 3000) k \
             JOIN s.w w ON w.k = k.id";
    let (got, round) = asked(&path, q);
    assert_eq!(got, 3 * (6000 + 6001));
    let kept = &round.reads[1];
    assert_eq!(
        (kept.blocks_read, kept.column_pages_predicted),
        (1, 2),
        "{q}: {kept:?}"
    );
    assert!(
        kept.column_pages_high > 2,
        "{q}: more than it reads: {kept:?}"
    );
}

#[test]
fn a_lateral_subquery_s_reads_are_the_round_s_and_kept_to_the_rows_before_it() {
    let path = tree("lateral");
    // Node 3000's two children, 6000 and 6001, each with two children of its own.
    let q = "SELECT sum(d.n) FROM (SELECT t.id FROM s.t t WHERE t.parent = 3000) k, \
             LATERAL (SELECT count(*) AS n FROM s.t u WHERE u.parent = k.id) d";
    let (got, round) = asked(&path, q);
    assert_eq!(got, 4, "{q}");
    // Its table is read once, in its reads, kept to the two keys: the one block they lie in, one page
    // of the one column of its its words name.
    assert_eq!(round.reads.len(), 2, "{q}: {round:?}");
    assert_eq!(round.reads[1].blocks_read, 1, "{q}: {round:?}");
    assert_eq!(round.reads[1].column_pages_predicted, 1, "{q}: {round:?}");
    // The same under a JOIN.
    let q = "SELECT sum(d.n) FROM (SELECT t.id FROM s.t t WHERE t.parent = 3000) k JOIN \
             LATERAL (SELECT count(*) AS n FROM s.t u WHERE u.parent = k.id) d ON true";
    let (got, round) = asked(&path, q);
    assert_eq!(got, 4, "{q}");
    assert_eq!(round.reads[1].column_pages_predicted, 1, "{q}: {round:?}");
}

/// Every node, twice over, each a step of its own, 18 bytes a row.
const BOTH: &str = "WITH a AS (SELECT t.parent, t.id FROM s.t t), \
                    b AS (SELECT u.parent, u.id FROM s.t u) ";

#[test]
fn a_join_of_two_sides_past_the_room_is_met_in_batches_each_row_read_back_once() {
    let path = tree("batches");
    let dir = spill_dir("batches");
    let room: u64 = 65_536;
    let row = 18;
    // Each node with a parent among the nodes meets it: every node but the root.
    let inner = format!("{BOTH}SELECT count(*), sum(b.id) FROM a JOIN b ON b.parent = a.id");
    let every = N * (N + 1) / 2;
    // A FULL JOIN adds the root, its parent none, and the 8,192 nodes that are no node's parent.
    let leaves = N - N / 2;
    let full = format!(
        "{BOTH}SELECT count(*), count(a.id), count(b.id) FROM a FULL JOIN b ON b.parent = a.id"
    );
    // A LEFT JOIN from the children: every child with its parent, the root with none.
    let left = format!("{BOTH}SELECT count(*), count(a.id) FROM b LEFT JOIN a ON b.parent = a.id");
    for (q, want) in [
        (inner, vec![N - 1, every - 1]),
        (full, vec![N + leaves, N - 1 + leaves, N]),
        (left, vec![N, N - 1]),
    ] {
        let mut snail = Snail::open(&path).unwrap();
        snail.size_rounds(Sizes::of(room).in_dir(&dir));
        let (b, round) = snail.ask(&q, None).unwrap_or_else(|e| panic!("{q}: {e}"));
        each_read_exact(&q, &round);
        assert_eq!(ints_or_nums(&b), want, "{q}");
        // The first step holds its rows column-wise in the room less the read's plan, a block read's
        // 40 bytes for each of the table's 31 blocks: 3,956, and writes the other 12,427, 55 pages.
        // The second, the room taken, writes all 16,383, 72 pages; the first's held rows are written
        // once the room is wanted, 18 more. Each side then goes into 9 batches, its 72 pages' bytes
        // and a part page each at most. Every page is read back once.
        let held = rows_in(room - 31 * 40, 2);
        let first = ((N as u64 - held) * row).div_ceil(4096);
        let least = first + 72 + (held * row).div_ceil(4096) + 2 * 72;
        assert_eq!((held, first, least), (3_956, 55, 289));
        assert!(
            round.spill_pages_written >= least && round.spill_pages_written <= least + 18,
            "{q}: {round:?}"
        );
        assert_eq!(
            round.spill_pages_read, round.spill_pages_written,
            "{q}: {round:?}"
        );
        // Each batch's rows within the room, 4,032 at most, and beside it the batch's map, counted
        // where it lives, 40 bytes a row at most. In hand, a chunk of the other side's batch, 911 rows
        // of two values, and of the rows met, 456 of four.
        let map = 40 * rows_in(room, 2);
        assert!(
            round.bytes_held <= room + map + 911 * in_hand(2) + 456 * in_hand(4),
            "{q}: {round:?}"
        );
        eprintln!(
            "{q}: {} pages written, most held {}",
            round.spill_pages_written, round.bytes_held
        );
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        // With room for it all, nothing is written.
        let mut snail = Snail::open(&path).unwrap();
        snail.size_rounds(Sizes::default().in_dir(&dir));
        let (b, round) = snail.ask(&q, None).unwrap();
        assert_eq!(ints_or_nums(&b), want, "{q}");
        assert_eq!(
            (round.spill_pages_written, round.spill_pages_read),
            (0, 0),
            "{q}"
        );
    }
}

#[test]
fn a_join_of_a_side_the_room_holds_hashes_it_and_reads_the_other_a_chunk_at_a_time() {
    let path = tree("one-side");
    let dir = spill_dir("one-side");
    // A hundred nodes held, against every node written: 16,383 rows past the room, the step's own
    // 72 pages, read back once; the hash is the hundred's. Nodes 5000 to 5099 have 200 children.
    let q = "WITH k AS (SELECT t.id FROM s.t t WHERE t.id BETWEEN 5000 AND 5099), \
             b AS (SELECT u.parent, u.id FROM s.t u) \
             SELECT count(*) FROM k JOIN b ON b.parent = k.id";
    let room = 1024 + 2 * (N as u64) * 18 / (N as u64) * 100;
    let mut snail = Snail::open(&path).unwrap();
    snail.size_rounds(Sizes::of(room).in_dir(&dir));
    let (b, round) = snail.ask(q, None).unwrap();
    each_read_exact(q, &round);
    assert_eq!(one(&b), 200);
    let held = room - 100 * 9;
    let past = (N as u64 * 18 - (held / 18) * 18).div_ceil(4096);
    assert_eq!(
        (round.spill_pages_written, round.spill_pages_read),
        (past, past),
        "{q}: {round:?}"
    );
    assert!(
        round.bytes_held <= room + one_chunk(&path, 18),
        "{q}: {round:?}"
    );
}

/// The integers of an answer's one row.
fn ints_or_nums(b: &arrow::array::RecordBatch) -> Vec<i64> {
    let rows = warren_snail::read::pipeline_rows(b).unwrap();
    assert_eq!(rows.len(), 1);
    rows[0]
        .iter()
        .map(|v| match v {
            warren_snail::read::Value::Int(x) => *x,
            warren_snail::read::Value::Num(x, 0) => *x as i64,
            other => panic!("an integer: {other:?}"),
        })
        .collect()
}

#[test]
fn a_set_operation_s_image_past_the_room_is_met_in_passes_each_key_whole_in_one() {
    let path = tree("image");
    let dir = spill_dir("image");
    let room: u64 = 65_536;
    // An arm of one integer a row held column-wise, 8 bytes and a word of NULL bits every 64 rows:
    // the room holds 8,065 of its 16,383, the other 8,318 at 9 bytes are 19 pages, 19 still while
    // the read's plan takes under 2,680 bytes of the room.
    let first = (N as u64 - rows_in(room, 1)) * 9;
    assert_eq!((rows_in(room, 1), first.div_ceil(4096)), (8_065, 19));
    assert_eq!(((N as u64 - rows_in(room - 2_680, 1)) * 9).div_ceil(4096), 19);
    // The parents are 1 to 8,191 and the root's, 16,384; the nodes 1 to 16,383. Their union is 16,384
    // values, at 9 bytes each past the room; 8,191 lie in both, and 8,192 nodes are no parent.
    for (q, want) in [
        ("SELECT count(*) FROM (SELECT t.parent FROM s.t t UNION SELECT u.id FROM s.t u) x", 16_384),
        ("SELECT count(*) FROM (SELECT t.parent FROM s.t t INTERSECT SELECT u.id FROM s.t u) x", 8_191),
        ("SELECT count(*) FROM (SELECT t.id FROM s.t t EXCEPT SELECT u.parent FROM s.t u) x", 8_192),
        // Each node's id once, and a parent's twice over: as many as the less of each.
        ("SELECT count(*) FROM (SELECT t.parent FROM s.t t INTERSECT ALL SELECT u.id FROM s.t u) x", 8_191),
        // Every parent's second row is left, and the root's.
        ("SELECT count(*) FROM (SELECT t.parent FROM s.t t EXCEPT ALL SELECT u.id FROM s.t u) x", 8_192),
    ] {
        let mut snail = Snail::open(&path).unwrap();
        snail.size_rounds(Sizes::of(room).in_dir(&dir));
        let (b, round) = snail.ask(q, None).unwrap_or_else(|e| panic!("{q}: {e}"));
        each_read_exact(q, &round);
        assert_eq!(one(&b), want, "{q}");
        assert!(round.spill_pages_written > 0, "{q}: {round:?}");
        // Each pass's pages are read back once; a second arm kept to the first's keys reads the
        // first's pages once more for them.
        assert!(
            round.spill_pages_read >= round.spill_pages_written
                && round.spill_pages_read <= round.spill_pages_written + 19,
            "{q}: {round:?}"
        );
        // The image's rows are one integer each: beside the room, one chunk of them in hand.
        assert!(round.bytes_held <= room + one_chunk(&path, 9), "{q}: {round:?}");
        let mut snail = Snail::open(&path).unwrap();
        snail.size_rounds(Sizes::default().in_dir(&dir));
        let (b, round) = snail.ask(q, None).unwrap();
        assert_eq!(one(&b), want, "{q}");
        assert_eq!(round.spill_pages_written, 0, "{q}");
    }
    // A UNION ALL's arms lie one after the other, its rows written once, read back once: the
    // first arm's 8,065 held, the rest of its 16,383 and all the second's at 9 bytes a row, 19 + 36.
    let q = "SELECT count(*) FROM (SELECT t.id FROM s.t t UNION ALL SELECT u.id FROM s.t u) x";
    let mut snail = Snail::open(&path).unwrap();
    snail.size_rounds(Sizes::of(room).in_dir(&dir));
    let (b, round) = snail.ask(q, None).unwrap();
    each_read_exact(q, &round);
    assert_eq!(one(&b), 2 * N, "{q}");
    let pages = first.div_ceil(4096) + (N as u64 * 9).div_ceil(4096);
    assert_eq!(pages, 55);
    assert_eq!(
        (round.spill_pages_written, round.spill_pages_read),
        (pages, pages),
        "{q}: {round:?}"
    );
    assert!(
        round.bytes_held <= room + one_chunk(&path, 9),
        "{q}: {round:?}"
    );
}

#[test]
fn a_sort_past_the_room_writes_the_rows_that_sort_last_and_hands_on_each_pass_in_order() {
    let path = tree("sort");
    let dir = spill_dir("sort");
    let room: u64 = 32 << 10;
    let ints_of = |q: &str| {
        let mut snail = Snail::open(&path).unwrap();
        snail.size_rounds(Sizes::of(room).in_dir(&dir));
        let (b, round) = snail.ask(q, None).unwrap_or_else(|e| panic!("{q}: {e}"));
        weather::exact(q, &round);
        assert_eq!(
            round.spill_pages_read, round.spill_pages_written,
            "{q}: {round:?}"
        );
        // Beside the room, a page for each of the 8 runs a merge takes at most, and the chunk it hands
        // on: rows of one integer until their 9 bytes each pass four pages, 1,821, each in hand as the
        // sort holds it, 72 bytes beside it, 64 its key's value and the row's 9.
        assert!(
            round.bytes_held <= room + 1_821 * (72 + 64 + 9) + 8 * 4096,
            "{q}: {round:?}"
        );
        (ints(&b), round)
    };
    // Every node, last first: a row held to be sorted is 72 bytes beside it, 64 its key's value and
    // the row's 9, so a run is 217 rows in the room less the read's plan, a block read's 40 bytes for
    // each of the table's 31 blocks.
    let (got, round) = ints_of("SELECT x.id FROM (SELECT t.id FROM s.t t ORDER BY t.id DESC) x");
    let want: Vec<Vec<i64>> = (1..=N).rev().map(|i| vec![i]).collect();
    assert_eq!(got, want);
    assert!(round.spill_pages_written > 0, "{round:?}");
    // Its OFFSET and LIMIT across the passes; and its ties: two rows a parent.
    let (got, _) = ints_of("SELECT t.id FROM s.t t ORDER BY t.id DESC OFFSET 8000 LIMIT 3");
    assert_eq!(got, [[N - 8000], [N - 8001], [N - 8002]]);
    let (got, _) =
        ints_of("SELECT t.parent FROM s.t t ORDER BY t.parent FETCH FIRST 3 ROWS WITH TIES");
    assert_eq!(got, [[1], [1], [2], [2]]);
    // Sorted in a step, read back in that order by the one after it.
    let q =
        "SELECT x.id FROM (SELECT t.parent, t.id FROM s.t t ORDER BY t.parent DESC, t.id DESC) x \
         LIMIT 2";
    let (got, round) = ints_of(q);
    // The root's parent sorts first, then node 8,191's two children.
    assert_eq!(got, [[1], [16_383]]);
    // A row held to be sorted by two values is 72 bytes beside it, 128 its key's and the row's 18: 218
    // bytes, 144 rows a run in the room less the plan's 1,240 bytes (144 for any plan from 1,159 bytes
    // to 1,376), each run one page. 113 runs, the last 111 rows held. A merge takes 8 runs at most in
    // this room: 15 merges bring the 113 to 8, 14 of them of 8 runs of one page, 1,152 rows of 18 bytes,
    // 6 pages each, and the last of the run left and 7 merged, 8,208 rows, 37 pages. The step after it
    // holds its rows column-wise beside the 111 held, 526 of them, and writes the other 15,857, 70
    // pages: 113 + 84 + 37 + 70.
    let per_run = |plan: u64| (room - plan) / 218;
    assert_eq!((per_run(1_159), per_run(1_240), per_run(1_376)), (144, 144, 144));
    let per_run = per_run(1_240);
    let (runs, left) = (N as u64 / per_run, N as u64 % per_run);
    assert_eq!((runs, left, (runs - 8) / 7), (113, 111, 15));
    let merged = (8 * per_run * 18).div_ceil(4096);
    let last = ((7 * 8 + 1) * per_run * 18).div_ceil(4096);
    let place = ((N as u64 - rows_in(room - left * 218, 2)) * 18).div_ceil(4096);
    assert_eq!((merged, last, rows_in(room - left * 218, 2), place), (6, 37, 526, 70));
    assert_eq!(
        round.spill_pages_written,
        runs + 14 * merged + last + place,
        "{q}: {round:?}"
    );
    // Rows that tie across 9 runs, in the order they came: the root, then the children of nodes
    // 4,096 to 8,191, then the rest.
    let (got, _) =
        ints_of("SELECT x.id FROM (SELECT t.id FROM s.t t ORDER BY t.parent / 4096 DESC) x");
    let want: Vec<Vec<i64>> = std::iter::once(1)
        .chain(8_192..=N)
        .chain(2..8_192)
        .map(|i| vec![i])
        .collect();
    assert_eq!(got, want);
}

#[test]
fn a_grouping_and_a_distinct_past_the_room_write_the_rows_of_groups_none_has_come_for() {
    let path = tree("groups");
    let dir = spill_dir("groups");
    let room: u64 = 32 << 10;
    // Every node its own group; two rows a parent; the parents, 8,191 and the root's.
    for (q, want) in [
        (
            "SELECT count(*), sum(x.n) FROM (SELECT t.id, count(*) AS n FROM s.t t GROUP BY t.id) x",
            vec![N, N],
        ),
        (
            "SELECT count(*), sum(x.n) FROM (SELECT t.parent, count(*) AS n FROM s.t t GROUP BY t.parent) x",
            vec![8_192, N],
        ),
        (
            "SELECT count(*), sum(x.parent) FROM (SELECT DISTINCT t.parent FROM s.t t) x",
            vec![8_192, 8_191 * 8_192 / 2 + ROOT_PARENT],
        ),
    ] {
        let mut snail = Snail::open(&path).unwrap();
        snail.size_rounds(Sizes::of(room).in_dir(&dir));
        let (b, round) = snail.ask(q, None).unwrap_or_else(|e| panic!("{q}: {e}"));
        each_read_exact(q, &round);
        assert_eq!(ints_or_nums(&b), want, "{q}");
        assert!(round.spill_pages_written > 0, "{q}: {round:?}");
        assert_eq!(round.spill_pages_read, round.spill_pages_written, "{q}: {round:?}");
        // Beside the room, a page for each part being filled, 8 at most, and one chunk of a part
        // read back in hand: rows of the table's two columns, the one the step groups by 9 bytes on
        // the spill and the other, unread, a NULL's 1, so 1,639 rows pass four pages, each in hand
        // its vector and two values.
        assert!(
            round.bytes_held <= room + 16_384u64.div_ceil(10) * in_hand(2) + 8 * 4096,
            "{q}: {round:?}"
        );
        let mut snail = Snail::open(&path).unwrap();
        snail.size_rounds(Sizes::default().in_dir(&dir));
        let (b, round) = snail.ask(q, None).unwrap();
        assert_eq!(ints_or_nums(&b), want, "{q}");
        assert_eq!(round.spill_pages_written, 0, "{q}");
    }
}

#[test]
fn a_join_of_tables_whose_counts_may_not_fit_the_room_is_met_as_rows_in_it() {
    let path = owned("c1");
    let dir = spill_dir("c1");
    // The first side read is held: all 16,384 purchases, at 9 bytes the id.
    let q = "SELECT sum(a.id) FROM s.p a JOIN s.p b ON b.id = a.id";
    let want = PURCHASES * (PURCHASES - 1) / 2;
    // With room for it, one round of the question path, no read of its own.
    let mut snail = Snail::open(&path).unwrap();
    let (b, round) = snail.ask(q, None).unwrap();
    assert_eq!(one(&b), want);
    assert!(round.reads.is_empty(), "{round:?}");
    assert_eq!(round.spill_pages_written, 0);
    // In 32 KiB they are met by the pipeline, each read exact.
    let room = 32 << 10;
    let mut snail = Snail::open(&path).unwrap();
    snail.size_rounds(Sizes::of(room).in_dir(&dir));
    let (b, round) = snail.ask(q, None).unwrap();
    each_read_exact(q, &round);
    assert_eq!(one(&b), want);
    // Each batch's rows within the room, the table's three columns of which the join reads one: 8
    // bytes and a bit a row, so 4,032 rows at most; and beside it the batch's map, counted where it
    // lives, a key and its rows' chain 40 bytes a row at most. In hand, a chunk of the other side read
    // back, rows of 9 bytes and two unread NULLs of 1 on the spill, 1,490 in four pages, three values
    // each; and a chunk of the rows met, 745 of 22 bytes, six values each.
    let map = 40 * rows_in(room, 1);
    assert_eq!(rows_in(room, 1), 4_032);
    assert!(
        round.bytes_held <= room + map + 1_490 * in_hand(3) + 745 * in_hand(6),
        "{q}: {round:?}"
    );
}

/// Purchases, beside a table of their 512 owners.
fn owned(name: &str) -> PathBuf {
    let path = tmp(&format!("rounds-{name}.snail"));
    let ddl = format!("{P_DDL} CREATE TABLE s.o (k bigint NOT NULL, PRIMARY KEY (k));");
    let catalog = Catalog::parse(&ddl).unwrap();
    let column = |table: &str, c: usize, values: &mut dyn Iterator<Item = i64>| {
        let col = &catalog.table(table).unwrap().columns[c];
        let mut v = Values::new(col.ty);
        for x in values {
            push_datum(
                &mut v,
                &warren_snail::composed::scalar_in(col.ty, &x.to_string()).unwrap(),
            );
        }
        v
    };
    let p = Rows {
        columns: vec![
            column("s.p", 0, &mut (0..PURCHASES)),
            column("s.p", 1, &mut (0..PURCHASES).map(owner)),
            column("s.p", 2, &mut (0..PURCHASES).map(|i| PURCHASES - i)),
        ],
    };
    let o = Rows {
        columns: vec![column("s.o", 0, &mut (0..512))],
    };
    write(&path, &catalog, &[("s.p".into(), p), ("s.o".into(), o)]).unwrap();
    path
}

const OWNERS: &str = "s.o o";

#[test]
fn a_subquery_s_kept_table_past_the_room_is_read_back_in_the_order_of_its_keys_hashes() {
    let path = owned("kept");
    let dir = spill_dir("kept");
    // Each owner's 32 purchases, kept to the 512 owners: all 16,384, each held by its key's hash as
    // the spill writes it, the column named 9 bytes and two unread NULLs 1 each, beside the 40 bytes
    // that find it: 835,584 bytes, past a room of 64 KiB.
    let room: u64 = 64 << 10;
    // A subquery is read while the chunk of rows that asks is handed on: the owners' 512, in hand one
    // value each.
    let asking = 512 * in_hand(1);
    // The kept table's rows read back a chunk at a time: the column named 9 bytes on the spill and
    // the two unread NULLs 1 each, 1,490 rows in four pages, in hand three values each. Its pass holds
    // a chunk at least, past the room where the room is full; and the page each of its parts is
    // filling, 8 at least.
    let most = room + 4 * 4096 + asking + 1_490 * in_hand(3) + 8 * 4096;
    for (q, want) in [
        (
            format!("SELECT sum((SELECT count(*) FROM s.p p WHERE p.owner = o.k)) FROM {OWNERS}"),
            PURCHASES,
        ),
        (
            format!(
                "SELECT count(*) FROM {OWNERS} WHERE EXISTS (SELECT 1 FROM s.p p WHERE p.owner = o.k + 256)"
            ),
            256,
        ),
        // Each owner's answer is its 32 rows: past the room they are written, and read back for the
        // row that asks.
        (
            format!(
                "SELECT count(*) FROM {OWNERS} WHERE o.k IN (SELECT p.owner FROM s.p p WHERE p.owner = o.k)"
            ),
            512,
        ),
    ] {
        let mut snail = Snail::open(&path).unwrap();
        snail.size_rounds(Sizes::of(room).in_dir(&dir));
        let (b, round) = snail.ask(&q, None).unwrap_or_else(|e| panic!("{q}: {e}"));
        each_read_exact(&q, &round);
        assert_eq!(one(&b), want, "{q}");
        assert!(round.spill_pages_written > 0, "{q}: {round:?}");
        assert!(round.bytes_held <= most, "{q}: {round:?}");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        // With room for it all, nothing is written.
        let mut snail = Snail::open(&path).unwrap();
        snail.size_rounds(Sizes::default().in_dir(&dir));
        let (b, round) = snail.ask(&q, None).unwrap();
        assert_eq!(one(&b), want, "{q}");
        assert_eq!(round.spill_pages_written, 0, "{q}");
    }
    // A LATERAL's table past the room is read a chunk at a time by each row before it.
    // The first 16 owners' 512 purchases, at 9 bytes the column named, past a room of 4 KiB.
    let q = "SELECT sum(d.n) FROM (SELECT o.k FROM s.o o WHERE o.k < 16) o, \
             LATERAL (SELECT count(*) AS n FROM s.p p WHERE p.owner = o.k) d";
    let mut snail = Snail::open(&path).unwrap();
    snail.size_rounds(Sizes::of(4096).in_dir(&dir));
    let (b, round) = snail.ask(q, None).unwrap();
    each_read_exact(q, &round);
    assert_eq!(one(&b), 512, "{q}");
    assert!(
        round.spill_pages_read > round.spill_pages_written,
        "{q}: {round:?}"
    );
    // Beside the room, a chunk of the table in hand and the 16 owners asking, in hand one value each.
    assert!(
        round.bytes_held <= 4096 + one_chunk(&path, 27) + 16 * in_hand(1),
        "{q}: {round:?}"
    );
    // An uncorrelated IN's rows past the room, their keys fitting no room, are read by each row.
    let q = "SELECT count(*) FROM s.p p WHERE p.id < 4 AND p.id IN (SELECT q.owner FROM s.p q)";
    let mut snail = Snail::open(&path).unwrap();
    snail.size_rounds(Sizes::of(4096).in_dir(&dir));
    let (b, round) = snail.ask(q, None).unwrap();
    each_read_exact(q, &round);
    // Ids 0 to 3 are each some purchase's owner: 7,919 is prime to 512.
    assert_eq!(one(&b), 4, "{q}");
    // Beside the room, a chunk of the IN's rows read back, its one column 9 bytes on the spill, 1,821
    // rows in four pages, in hand one value each; and the 4 rows asking, in hand three values each.
    assert!(
        round.bytes_held <= 4096 + 1_821 * in_hand(1) + 4 * in_hand(3),
        "{q}: {round:?}"
    );
}

/// Each purchase, as (id, owner, moment), by the owners and moments it reads in.
fn all_purchases() -> Vec<(i64, i64, i64)> {
    (0..PURCHASES)
        .map(|i| (i, owner(i), PURCHASES - i))
        .collect()
}

#[test]
fn an_order_by_a_strand_gives_is_read_along_it_where_its_price_is_fewest() {
    let path = purchases("order-by");
    let dir = spill_dir("order-by");
    // Sorted as the strand leads, its first four rows.
    let mut rows = all_purchases();
    rows.sort_by_key(|&(_, o, at)| (o, at));
    let want: Vec<Vec<i64>> = rows[..4].iter().map(|&(id, _, _)| vec![id]).collect();
    let q = "SELECT p.id FROM s.p p ORDER BY p.owner, p.at LIMIT 4";
    // Past a room of 64 KiB the sort would spill 184 pages: the strand's 192 against the table's
    // own 192 with them. Nothing is written; at 128 MB the table's own order is read and sorted.
    for (room, order) in [(64u64 << 10, "p_owner"), (128 << 20, "p_pkey")] {
        let mut snail = Snail::open(&path).unwrap();
        snail.size_rounds(Sizes::of(room).in_dir(&dir));
        let (b, round) = snail.ask(q, None).unwrap();
        weather::exact(q, &round);
        assert_eq!(ints(&b), want, "{room}");
        assert_eq!(round.order, order, "{room}");
        assert_eq!(
            (round.spill_pages_written, round.spill_pages_read),
            (0, 0),
            "{round:?}"
        );
    }
    // A step's ORDER BY, read along the strand: its rows past the room are its own, written once and
    // read back once in that order. Three integers a row held column-wise, 8 bytes each and a word of
    // NULL bits a column every 64 rows: the room holds 2,688; the other 13,696 at 27 bytes on the
    // spill are 91 pages, 91 still while the read's plan takes under 2,650 bytes of the room.
    let q =
        "SELECT y.id FROM (SELECT p.owner, p.at, p.id FROM s.p p ORDER BY p.owner, p.at) y LIMIT 4";
    let room: u64 = 64 << 10;
    let mut snail = Snail::open(&path).unwrap();
    snail.size_rounds(Sizes::of(room).in_dir(&dir));
    let (b, round) = snail.ask(q, None).unwrap();
    each_read_exact(q, &round);
    assert_eq!(ints(&b), want, "{q}");
    assert_eq!(round.reads[0].order, "p_owner", "{q}: {round:?}");
    let pages = ((PURCHASES as u64 - rows_in(room, 3)) * 27).div_ceil(4096);
    assert_eq!((rows_in(room, 3), pages), (2_688, 91));
    assert_eq!(
        ((PURCHASES as u64 - rows_in(room - 2_650, 3)) * 27).div_ceil(4096),
        pages
    );
    assert_eq!(
        (round.spill_pages_written, round.spill_pages_read),
        (pages, pages),
        "{q}: {round:?}"
    );
    // A DESC is no strand's, its rows read along the table's own order and sorted.
    let q = "SELECT p.id FROM s.p p ORDER BY p.owner DESC, p.at DESC LIMIT 2";
    let mut last = all_purchases();
    last.sort_by_key(|&(_, o, at)| std::cmp::Reverse((o, at)));
    let mut snail = Snail::open(&path).unwrap();
    snail.size_rounds(Sizes::of(room).in_dir(&dir));
    let (b, round) = snail.ask(q, None).unwrap();
    assert_eq!(ints(&b), [[last[0].0], [last[1].0]], "{q}");
    assert_eq!(round.order, "p_pkey", "{q}");
    // Nor a name an item takes as its alias, read as that item.
    let q = "SELECT p.id AS at, p.at AS id FROM s.p p ORDER BY id LIMIT 1";
    let mut snail = Snail::open(&path).unwrap();
    snail.size_rounds(Sizes::of(room).in_dir(&dir));
    let (b, round) = snail.ask(q, None).unwrap();
    assert_eq!(ints(&b), [[PURCHASES - 1, 1]], "{q}");
    assert_eq!(round.order, "p_pkey", "{q}");
}

/// The id of the last row a group gives it, in the order it gives them.
struct LastOf(i64);

impl warren_snail::read::handed::Fold for LastOf {
    fn add(&mut self, args: &[warren_snail::read::Value]) -> warren_snail::Result<()> {
        if let warren_snail::read::Value::Int(x) = args[0] {
            self.0 = x;
        }
        Ok(())
    }

    fn finish(&mut self) -> warren_snail::Result<warren_snail::read::Value> {
        Ok(warren_snail::read::Value::Int(self.0))
    }
}

#[test]
fn a_caller_s_ordered_aggregate_along_a_strand_lets_each_group_go_as_its_key_changes() {
    use warren_snail::read::handed::{self, Function};
    let path = purchases("groups-along");
    let dir = spill_dir("groups-along");
    let ty = handed::type_named("bigint").unwrap();
    let q = "SELECT p.owner, last_of(p.id ORDER BY p.at) FROM s.p p GROUP BY p.owner";
    // Each owner's id at its greatest moment.
    let mut best: std::collections::BTreeMap<i64, (i64, i64)> = Default::default();
    for (id, o, at) in all_purchases() {
        let e = best.entry(o).or_insert((at, id));
        if at > e.0 {
            *e = (at, id);
        }
    }
    let want: Vec<Vec<i64>> = best.iter().map(|(&o, &(_, id))| vec![o, id]).collect();
    for (room, order) in [(64u64 << 10, "p_owner"), (128 << 20, "p_pkey")] {
        let mut snail = Snail::open(&path).unwrap();
        snail
            .hand(Function::aggregate("last_of", &[ty], ty, || {
                Box::new(LastOf(0))
            }))
            .unwrap();
        snail.size_rounds(Sizes::of(room).in_dir(&dir));
        let (b, round) = snail.ask(q, None).unwrap();
        weather::exact(q, &round);
        let mut got = ints(&b);
        got.sort();
        assert_eq!(got, want, "{room}");
        assert_eq!(round.order, order, "{room}");
        assert_eq!(round.spill_pages_written, 0, "{room}: {round:?}");
        if order == "p_owner" {
            // A block's rows handed on, and one owner's 32 rows' 18 bytes kept at a time.
            assert!(round.bytes_held <= room, "{round:?}");
        }
    }
    // Over a join the same, its first table read along the strand.
    let q = "SELECT p.owner, last_of(p.id ORDER BY p.at) FROM s.p p JOIN s.o o ON o.k = p.owner \
             GROUP BY p.owner";
    let path = owned("groups-join");
    let room = 64u64 << 10;
    let mut snail = Snail::open(&path).unwrap();
    snail
        .hand(Function::aggregate("last_of", &[ty], ty, || {
            Box::new(LastOf(0))
        }))
        .unwrap();
    snail.size_rounds(Sizes::of(room).in_dir(&dir));
    let (b, round) = snail.ask(q, None).unwrap();
    each_read_exact(q, &round);
    let mut got = ints(&b);
    got.sort();
    assert_eq!(got, want, "{q}");
    assert!(
        round.reads.iter().any(|r| r.order == "p_owner"),
        "{round:?}"
    );
    assert_eq!(round.spill_pages_written, 0, "{round:?}");
}

#[test]
fn a_window_over_a_join_along_its_first_table_s_strand_holds_one_partition_at_a_time() {
    let path = owned("window-join");
    let dir = spill_dir("window-join");
    let want = places(&all_purchases());
    let q = "SELECT p.id, row_number() OVER (PARTITION BY p.owner ORDER BY p.at) \
             FROM s.p p JOIN s.o o ON o.k = p.owner";
    let room: u64 = 64 << 10;
    let mut snail = Snail::open(&path).unwrap();
    snail.size_rounds(Sizes::of(room).in_dir(&dir));
    let (b, round) = snail.ask(q, None).unwrap();
    each_read_exact(q, &round);
    let mut got = ints(&b);
    got.sort();
    assert_eq!(got, want, "{q}");
    assert!(
        round.reads.iter().any(|r| r.order == "p_owner"),
        "{round:?}"
    );
    assert_eq!(
        (round.spill_pages_written, round.spill_pages_read),
        (0, 0),
        "{q}: {round:?}"
    );
    // Beside the room, a block of the table in hand and a chunk of the rows it makes.
    assert!(
        round.bytes_held <= room + 2 * one_chunk(&path, 27),
        "{q}: {round:?}"
    );
    // A LEFT JOIN keeps each row of its first table, those that meet none among them: the owners
    // from 256 on meet none.
    let left = "SELECT p.id, row_number() OVER (PARTITION BY p.owner ORDER BY p.at) \
                FROM s.p p LEFT JOIN (SELECT o.k FROM s.o o WHERE o.k < 256) d ON d.k = p.owner";
    let mut snail = Snail::open(&path).unwrap();
    snail.size_rounds(Sizes::of(room).in_dir(&dir));
    let (b, round) = snail.ask(left, None).unwrap();
    each_read_exact(left, &round);
    let mut got = ints(&b);
    got.sort();
    assert_eq!(got, want, "{left}");
    assert!(
        round.reads.iter().any(|r| r.order == "p_owner"),
        "{round:?}"
    );
    // With the other side past the room, 512 owners at 9 bytes in 4 KiB, its order is lost: the
    // partitions held whole, and the answer the same over the first 32 owners' purchases.
    let q = "SELECT p.id, row_number() OVER (PARTITION BY p.owner ORDER BY p.at) \
             FROM s.p p JOIN (SELECT o.k FROM s.o o ORDER BY o.k) d ON d.k = p.owner \
             WHERE p.owner < 32";
    let few: Vec<(i64, i64, i64)> = all_purchases().into_iter().filter(|r| r.1 < 32).collect();
    let mut snail = Snail::open(&path).unwrap();
    snail.size_rounds(Sizes::of(4096).in_dir(&dir));
    let (b, round) = snail.ask(q, None).unwrap();
    each_read_exact(q, &round);
    let mut got = ints(&b);
    got.sort();
    assert_eq!(got, places(&few), "{q}");
    // At 128 MB the sort fits: read as the join's round reads it, nothing written.
    let q = "SELECT p.id, row_number() OVER (PARTITION BY p.owner ORDER BY p.at) \
             FROM s.p p JOIN s.o o ON o.k = p.owner";
    let mut snail = Snail::open(&path).unwrap();
    snail.size_rounds(Sizes::default().in_dir(&dir));
    let (b, round) = snail.ask(q, None).unwrap();
    let mut got = ints(&b);
    got.sort();
    assert_eq!(got, want, "{q}");
    assert_eq!(round.spill_pages_written, 0, "{q}");
}

/// A question asked past `room`, its rows' ids and their other integers in order.
fn past_room(path: &PathBuf, dir: &PathBuf, room: u64, q: &str) -> (Vec<Vec<i64>>, Round) {
    let mut snail = Snail::open(path).unwrap();
    snail.size_rounds(Sizes::of(room).in_dir(dir));
    let (b, round) = snail.ask(q, None).unwrap_or_else(|e| panic!("{q}: {e}"));
    weather::exact(q, &round);
    assert_eq!(
        round.spill_pages_read, round.spill_pages_written,
        "{q}: every page written is read back once: {round:?}"
    );
    (ints(&b), round)
}

#[test]
fn rows_past_several_rooms_are_written_once_and_read_back_once_at_each_place() {
    let path = owned("rooms");
    let dir = spill_dir("rooms");
    let n = PURCHASES as u64;
    let room: u64 = 64 << 10;
    // A window over every purchase on an order no strand gives, its three integers 27 bytes: 442,368
    // bytes, 6.75 rooms, cut at once into 8 parts of whole partitions. The 2,427 rows the room holds are
    // never written; the other 13,957, 376,839 bytes, are 93 pages, a part page more in each part at
    // most. Written again each pass they would be 312. Its moments fall as its ids rise.
    let q = "SELECT p.id, row_number() OVER (PARTITION BY p.id % 64 ORDER BY p.at) FROM s.p p";
    let rank = |i: i64| 256 - i / 64;
    let (mut got, round) = past_room(&path, &dir, room, q);
    got.sort();
    let want: Vec<Vec<i64>> = (0..PURCHASES).map(|i| vec![i, rank(i)]).collect();
    assert_eq!(got, want, "{q}");
    let least = ((n - room / 27) * 27).div_ceil(4096);
    assert!(
        round.spill_pages_written >= least && round.spill_pages_written <= least + 8,
        "{q}: {least}: {round:?}"
    );
    assert!(
        round.bytes_held <= room + one_chunk(&path, 27) + 8 * 4096,
        "{q}: {round:?}"
    );
    // The same over a join, how many rows come not known before the first write, each row its four
    // columns, 36 bytes: 589,824 bytes, 9 rooms, in the 8 parts a room this size lets it fill. A part
    // past the room goes round again: at most half as many pages again as the 128 past it; written
    // again each pass, 576.
    let q = "SELECT p.id, row_number() OVER (PARTITION BY p.id % 64 ORDER BY p.at) \
             FROM s.o o JOIN s.p p ON p.owner = o.k";
    let (mut got, round) = past_room(&path, &dir, room, q);
    got.sort();
    assert_eq!(got, want, "{q}");
    assert!(round.spill_pages_written > 0, "{q}: {round:?}");
    let past = (n * 36 - room).div_ceil(4096);
    assert!(
        round.spill_pages_written >= past && round.spill_pages_written <= past * 3 / 2,
        "{q}: {round:?}"
    );
    // A grouping, in 128 KiB: each group its first row, 27 bytes, and 32 of its fold: 966,656 bytes,
    // 7.4 rooms, cut at once into 9 parts. The 2,221 groups the room holds write nothing; the other
    // 14,163 rows are 382,401 bytes, 94 pages, a part page more in each at most.
    let groups: u64 = 128 << 10;
    let q = "SELECT p.id, count(*) FROM s.p p GROUP BY p.id LIMIT 100000";
    let (mut got, round) = past_room(&path, &dir, groups, q);
    got.sort();
    let once: Vec<Vec<i64>> = (0..PURCHASES).map(|i| vec![i, 1]).collect();
    assert_eq!(got, once, "{q}");
    let least = ((n - groups / 59) * 27).div_ceil(4096);
    assert!(
        round.spill_pages_written >= least && round.spill_pages_written <= least + 9,
        "{q}: {least}: {round:?}"
    );
    // A DISTINCT, in 32 KiB: every purchase's id at 9 bytes, 147,456 bytes, 4.5 rooms, cut at once into
    // 5 parts. The 3,640 the room holds write nothing; the other 12,744 are 28 pages, a part page more
    // in each at most.
    let distinct: u64 = 32 << 10;
    let q = "SELECT DISTINCT p.id FROM s.p p LIMIT 100000";
    let (mut got, round) = past_room(&path, &dir, distinct, q);
    got.sort();
    let ids: Vec<Vec<i64>> = (0..PURCHASES).map(|i| vec![i]).collect();
    assert_eq!(got, ids, "{q}");
    let least = ((n - distinct / 9) * 9).div_ceil(4096);
    assert!(
        round.spill_pages_written >= least && round.spill_pages_written <= least + 5,
        "{q}: {least}: {round:?}"
    );
    // A sort, in 32 KiB: the same 147,456 bytes known past two rooms, cut into runs of 3,640 rows,
    // 8 pages each: 4 of them, the last 1,824 rows held and merged with them. Each page read back once.
    let q = "SELECT p.id FROM s.p p ORDER BY p.id DESC LIMIT 100000";
    let (got, round) = past_room(&path, &dir, distinct, q);
    let down: Vec<Vec<i64>> = (0..PURCHASES).rev().map(|i| vec![i]).collect();
    assert_eq!(got, down, "{q}");
    assert_eq!(round.spill_pages_written, 4 * 8, "{q}: {round:?}");
    assert!(
        round.bytes_held <= distinct + one_chunk(&path, 9) + 4 * 4096,
        "{q}: {round:?}"
    );
    // Over a join, not known: once a room of them is written they are cut into runs, the rows written
    // standing as one part, held for the merge; every row written once, 147,456 bytes, 37 pages,
    // a part page more in each run at most.
    let q = "SELECT p.id FROM s.o o JOIN s.p p ON p.owner = o.k ORDER BY p.id DESC LIMIT 100000";
    let (got, round) = past_room(&path, &dir, distinct, q);
    assert_eq!(got, down, "{q}");
    assert!(
        round.spill_pages_written >= 28 && round.spill_pages_written <= 37 + 5,
        "{q}: {round:?}"
    );
}

/// Rows of very different widths: 20 of 3,000 bytes of text, then 280 of one byte that sort before
/// them, then 200 of one byte that sort after them.
fn widths(name: &str) -> PathBuf {
    let path = tmp(&format!("rounds-{name}.snail"));
    let ddl = "CREATE SCHEMA s; CREATE TABLE s.w (id bigint NOT NULL, k bigint NOT NULL, \
               pad text NOT NULL, PRIMARY KEY (id));";
    let catalog = Catalog::parse(ddl).unwrap();
    let k = |i: i64| match i {
        0..=19 => 100_000 + i,
        20..=299 => i,
        _ => 200_000 + i,
    };
    let pad = |i: i64| if i < 20 { "x".repeat(3000) } else { "x".into() };
    let w = Rows {
        columns: vec![
            weather::ints(&(0..=499).collect::<Vec<_>>()),
            weather::ints(&(0..=499).map(k).collect::<Vec<_>>()),
            Values::Text((0..=499).map(|i| Some(pad(i))).collect()),
        ],
    };
    write(&path, &catalog, &[("s.w".into(), w)]).unwrap();
    path
}

#[test]
fn a_row_past_the_room_that_sorts_after_one_written_is_never_handed_on_before_it() {
    let path = widths("widths");
    let dir = spill_dir("widths");
    // A row held to be sorted is 72 bytes beside it, its key's two values 64 each and the text's
    // bytes, and the row as the spill writes it: each wide row 6,223 bytes, each narrow one 225. The
    // 20 wide come first, 124,460 bytes; the 280 narrow that sort before them 63,000 more, 6,223 past
    // 181,237 bytes, so in a room of 180 KiB less the read's plan one narrow row lets the last wide one
    // go, freeing room for the rest; the 200 narrow rows that sort after it, 45,000 bytes, past the
    // room and within two, are set aside with it, held in the room it freed until one finds none,
    // then written. 18.6's order: the narrow rows that sort first, the wide, the narrow that sort
    // last. The wide row let go, 3,023 bytes on the spill, is the one page written; the narrow rows
    // written, 24 bytes each, lie in the part page kept in memory.
    let (wide, narrow) = (72 + 128 + 3_000 + 3_023, 72 + 128 + 1 + 24);
    assert_eq!(20 * wide + 280 * narrow - wide, 181_237);
    let q = "SELECT w.id FROM s.w w ORDER BY w.k, w.pad";
    let want: Vec<Vec<i64>> = (20..=299)
        .chain(0..=19)
        .chain(300..=499)
        .map(|i| vec![i])
        .collect();
    let (got, round) = past_room(&path, &dir, 180 << 10, q);
    assert_eq!(got, want, "{q}");
    assert_eq!(round.spill_pages_written, 1, "{q}: {round:?}");
    // Given room for every row, the same rows in the same order, nothing written.
    let (got, round) = past_room(&path, &dir, 1 << 20, q);
    assert_eq!(got, want, "{q}");
    assert_eq!(round.spill_pages_written, 0, "{q}: {round:?}");
}

#[test]
fn a_step_read_after_a_fold_over_one_table_holds_its_rows_in_the_room() {
    let path = owned("after");
    let dir = spill_dir("after");
    // Each purchase its own group, its id and count 18 bytes as a step: 294,912 bytes past 32 KiB,
    // read by the step after it a chunk at a time.
    let room: u64 = 32 << 10;
    let q = "WITH g AS (SELECT p.id, count(*) AS n FROM s.p p GROUP BY p.id) \
             SELECT sum(g.n), max(g.id) FROM g";
    let mut snail = Snail::open(&path).unwrap();
    snail.size_rounds(Sizes::of(room).in_dir(&dir));
    let (b, round) = snail.ask(q, None).unwrap();
    weather::exact(q, &round);
    assert_eq!(ints_or_nums(&b), [PURCHASES, PURCHASES - 1], "{q}");
    assert!(round.spill_pages_written > 0, "{q}: {round:?}");
    // Beside the room, one chunk in hand and a page for each part being filled, 8 at least.
    assert!(
        round.bytes_held <= room + one_chunk(&path, 27) + 8 * 4096,
        "{q}: {round:?}"
    );
    // With room for it, none written, the same answer.
    let mut snail = Snail::open(&path).unwrap();
    let (b, round) = snail.ask(q, None).unwrap();
    assert_eq!(ints_or_nums(&b), [PURCHASES, PURCHASES - 1], "{q}");
    assert_eq!(round.spill_pages_written, 0, "{q}");
    // A sort read by the step after it: a row held to be sorted is 72 bytes beside it, 64 its key's
    // value and the row's 9, so in 64 KiB less the read's plan, a block read's 40 bytes for each of
    // the table's 32 blocks, a run is 443 rows, one page: 36 runs and 436 rows held. A merge takes 8
    // runs at most, so 4 merges of 8 runs bring them to 8, 3,544 rows, 8 pages each: 68 pages. Its
    // rows as a step lie past the room as well: more than those 68 pages are written.
    let room: u64 = 64 << 10;
    let per_run = (room - 32 * 40) / 145;
    assert_eq!((per_run, PURCHASES as u64 / per_run), (443, 36));
    let sorted = 36 + 4 * (8 * per_run * 9).div_ceil(4096);
    assert_eq!(sorted, 68);
    let q = "WITH g AS (SELECT p.id FROM s.p p ORDER BY p.id LIMIT 100000) SELECT sum(g.id) FROM g";
    let mut snail = Snail::open(&path).unwrap();
    snail.size_rounds(Sizes::of(room).in_dir(&dir));
    let (b, round) = snail.ask(q, None).unwrap();
    weather::exact(q, &round);
    assert_eq!(ints_or_nums(&b), [PURCHASES * (PURCHASES - 1) / 2], "{q}");
    assert!(round.spill_pages_written > sorted, "{q}: {round:?}");
    assert_eq!(
        round.spill_pages_read, round.spill_pages_written,
        "{q}: {round:?}"
    );
    // Beside the room, a page for each of the 8 runs a merge takes, and the chunk it hands on: rows
    // of one integer until their 9 bytes each pass four pages, 1,821, each in hand as the sort holds
    // it, 145 bytes.
    assert!(
        round.bytes_held <= room + 1_821 * 145 + 8 * 4096,
        "{q}: {round:?}"
    );
}

#[test]
fn a_join_s_rows_read_as_a_place_go_on_a_block_s_at_a_time() {
    let path = owned("c1-rows");
    let dir = spill_dir("c1-rows");
    // A join of tables the question path reads, kept as a step: its 16,384 rows past the room lie in
    // the step's place, written, none held whole in between; the side it holds fits.
    let room: u64 = 64 << 10;
    let q = "SELECT sum(x.id) FROM (SELECT p.id, o.k FROM s.o o JOIN s.p p ON p.owner = o.k) x";
    let want = PURCHASES * (PURCHASES - 1) / 2;
    let mut snail = Snail::open(&path).unwrap();
    snail.size_rounds(Sizes::of(room).in_dir(&dir));
    let (b, round) = snail.ask(q, None).unwrap();
    each_read_exact(q, &round);
    assert_eq!(one(&b), want, "{q}");
    assert!(
        round.reads[0].order.contains('+'),
        "{q}: one round: {round:?}"
    );
    // Before the step's first row the room holds the owners' side: its 512 keys, 8 bytes each and a
    // word of NULL bits every 64, and a weight of 8 bytes a row, 8,256 bytes; the rows joined so far,
    // a row number of each table 4 bytes, 4,096; and each owner's key with its rows, to meet the
    // purchases as they come: the key 17 bytes (a kind byte and a number's 16), its end 8 and its share
    // of the slots 11, its first and last row 8 and its row's next 4, 48 bytes, 24,576. Beside them
    // the purchases' read's plan, as its walks count it, 17,664 bytes. The step's rows, two integers
    // held column-wise, fill the rest: 673 of them; the other 15,711, 18 bytes each on the spill, are
    // written once and read back once, 70 pages, 70 for any plan from 17,505 bytes to 21,184.
    let side = held_ints(512, 1) + 512 * 8 + 512 * 2 * 4 + 512 * 48;
    assert_eq!(side, 36_928);
    let pages = |plan: u64| {
        ((PURCHASES as u64 - rows_in(room - side - plan, 2)) * 18).div_ceil(4096)
    };
    assert_eq!((rows_in(room - side - 17_664, 2), pages(17_664)), (673, 70));
    for plan in [17_505, 21_184] {
        assert_eq!(pages(plan), 70);
    }
    let pages = pages(17_664);
    assert_eq!(
        (round.spill_pages_written, round.spill_pages_read),
        (pages, pages),
        "{q}: {round:?}"
    );
    assert!(
        round.bytes_held <= room + 2 * one_chunk(&path, 27),
        "{q}: {round:?}"
    );
}

#[test]
fn an_alias_hides_its_table_s_own_name() {
    let path = owned("alias");
    let want = PURCHASES * (PURCHASES - 1) / 2;
    // As PostgreSQL 18.6 reads them: `p` is the alias of the first, `q` hides the second's name, and
    // where the first takes none its own name names it.
    for q in [
        "SELECT sum(p.id) FROM s.p p JOIN s.p q ON q.id = p.id",
        "SELECT sum(q.id) FROM s.p p JOIN s.p q ON q.id = p.id",
        "SELECT sum(p.id) FROM s.p JOIN s.p q ON q.id = p.id",
        "SELECT sum(p.id) FROM s.p, s.p q WHERE q.id = p.id",
    ] {
        let (b, round) = Snail::open(&path).unwrap().ask(q, None).unwrap();
        weather::exact(q, &round);
        assert_eq!(one(&b), want, "{q}");
    }
    // A name an alias hides names nothing.
    let e = Snail::open(&path)
        .unwrap()
        .ask(
            "SELECT sum(p.id) FROM s.p a JOIN s.p b ON b.id = a.id",
            None,
        )
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("missing FROM-clause entry for table \"p\""),
        "{e}"
    );
}
