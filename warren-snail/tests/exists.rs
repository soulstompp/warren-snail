//! An EXISTS or NOT EXISTS tied to the query around it by =s, read as the join it is: its
//! subquery's rows read once a round, each row asking met with those of its keys, a NULL key on
//! either side meeting none, a conjunct naming both sides met on each pair. Where the keys pass the
//! room, both sides are cut on their keys' hash, each part's rows written once and read back once,
//! and every row asking comes out in the order it came. Every answer is PostgreSQL 18.6's over the
//! same rows; a subquery not so tied is answered as before.

mod weather;

use arrow::array::{Array, RecordBatch};
use arrow::util::display::array_value_to_string;
use std::path::{Path, PathBuf};
use warren_snail::catalog::Catalog;
use warren_snail::read::Round;
use warren_snail::room::Sizes;
use warren_snail::values::Values;
use warren_snail::write::Rows;
use weather::{fresh, tmp, write};

const DDL: &str = "CREATE SCHEMA j;
CREATE TABLE j.f (id integer PRIMARY KEY, k integer, s text, n integer);
CREATE TABLE j.e (id integer PRIMARY KEY, k integer, s text, m integer);";

/// The rows asking, 4,000: `k` NULL on every 97th, `s` on every 89th. The rows asked, 6,000: `k`
/// NULL on every 101st and each other key twice, those of the keys a multiple of 13 moved out of
/// reach; `m` NULL on every 71st. As PostgreSQL's tables were made:
///
/// ```sql
/// INSERT INTO j.f SELECT i, CASE WHEN i % 97 = 0 THEN NULL ELSE i % 1500 END,
///     CASE WHEN i % 89 = 0 THEN NULL ELSE 'a' || (i % 7) END, i % 11
///     FROM generate_series(1, 4000) i;
/// INSERT INTO j.e SELECT i, CASE WHEN i % 101 = 0 THEN NULL
///     WHEN (i * 7) % 3000 % 13 = 0 THEN (i * 7) % 3000 + 5000 ELSE (i * 7) % 3000 END,
///     'a' || (i % 5), CASE WHEN i % 71 = 0 THEN NULL ELSE i % 13 END
///     FROM generate_series(1, 6000) i;
/// ```
fn snail(name: &str) -> PathBuf {
    let path = tmp(&format!("exists-{name}.snail"));
    let catalog = Catalog::parse(DDL).unwrap();
    let ints = |v: Vec<Option<i64>>| Values::Int(v);
    let texts = |v: Vec<Option<String>>| Values::Text(v);
    let f = Rows {
        columns: vec![
            ints((1..=4000).map(Some).collect()),
            ints((1..=4000).map(|i| (i % 97 != 0).then_some(i % 1500)).collect()),
            texts(
                (1..=4000)
                    .map(|i| (i % 89 != 0).then(|| format!("a{}", i % 7)))
                    .collect(),
            ),
            ints((1..=4000).map(|i| Some(i % 11)).collect()),
        ],
    };
    let key = |i: i64| {
        let k = (i * 7) % 3000;
        match (i % 101, k % 13) {
            (0, _) => None,
            (_, 0) => Some(k + 5000),
            _ => Some(k),
        }
    };
    let e = Rows {
        columns: vec![
            ints((1..=6000).map(Some).collect()),
            ints((1..=6000).map(key).collect()),
            texts((1..=6000).map(|i| Some(format!("a{}", i % 5))).collect()),
            ints((1..=6000).map(|i| (i % 71 != 0).then_some(i % 13)).collect()),
        ],
    };
    write(&path, &catalog, &[("j.f".into(), f), ("j.e".into(), e)]).unwrap();
    path
}

type Row = Vec<Option<String>>;

fn rows(b: &RecordBatch) -> Vec<Row> {
    (0..b.num_rows())
        .map(|r| {
            b.columns()
                .iter()
                .map(|c| (!c.is_null(r)).then(|| array_value_to_string(c, r).unwrap()))
                .collect()
        })
        .collect()
}

/// Rows written as text, fields apart by `|`, an empty field NULL.
fn want(rows: &[&str]) -> Vec<Row> {
    rows.iter()
        .map(|r| {
            r.split('|')
                .map(|v| (!v.is_empty()).then(|| v.to_string()))
                .collect()
        })
        .collect()
}

/// A question's answer, in a room of `room` bytes where one is given, its rows those written, each
/// table read its predicted pages.
fn same(path: &Path, room: Option<u64>, q: &str, expected: &[&str]) -> Round {
    let mut snail = fresh(path);
    if let Some(room) = room {
        snail.size_rounds(Sizes::of(room).in_dir(spill_dir(path)));
    }
    let (b, round) = snail.ask(q, None).unwrap_or_else(|e| panic!("{q}: {e}"));
    assert_eq!(rows(&b), want(expected), "{q}");
    assert_eq!(
        round.column_pages_read,
        round.column_pages_predicted - round.column_pages_held,
        "{q}: {round:?}"
    );
    round
}

/// Where a round on a snail writes past its room.
fn spill_dir(path: &Path) -> PathBuf {
    let dir = path.with_extension("spill");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// The questions read as joins, each with PostgreSQL 18.6's answer over the same rows.
const JOINED: [(&str, &[&str]); 13] = [
    (
        "SELECT count(*) FROM j.f f WHERE EXISTS (SELECT 1 FROM j.e e WHERE e.k = f.k)",
        &["3655"],
    ),
    (
        "SELECT count(*) FROM j.f f WHERE NOT EXISTS (SELECT 1 FROM j.e e WHERE e.k = f.k)",
        &["345"],
    ),
    // A conjunct naming both sides, met on each pair; `m` NULL meets none.
    (
        "SELECT count(*) FROM j.f f \
         WHERE EXISTS (SELECT 1 FROM j.e e WHERE e.k = f.k AND e.m < f.n)",
        &["2032"],
    ),
    (
        "SELECT count(*) FROM j.f f \
         WHERE NOT EXISTS (SELECT 1 FROM j.e e WHERE e.k = f.k AND e.m < f.n)",
        &["1968"],
    ),
    // A conjunct of its own columns alone, on its side.
    (
        "SELECT count(*) FROM j.f f \
         WHERE EXISTS (SELECT 1 FROM j.e e WHERE e.k = f.k AND e.s = 'a3')",
        &["732"],
    ),
    // Its side empty.
    (
        "SELECT count(*) FROM j.f f \
         WHERE EXISTS (SELECT 1 FROM j.e e WHERE e.k = f.k AND e.m > 100)",
        &["0"],
    ),
    (
        "SELECT count(*) FROM j.f f \
         WHERE NOT EXISTS (SELECT 1 FROM j.e e WHERE e.k = f.k AND e.m > 100)",
        &["4000"],
    ),
    // Under OR, two ties, `s` NULL on either side meeting none.
    (
        "SELECT count(*) FROM j.f f \
         WHERE f.n > 5 OR NOT EXISTS (SELECT 1 FROM j.e e WHERE e.k = f.k AND e.s = f.s)",
        &["3714"],
    ),
    // In a select list, its tie an expression of the query around it.
    (
        "SELECT count(*) FILTER (WHERE x), count(*) FILTER (WHERE NOT x), \
         count(*) FILTER (WHERE x IS NULL) FROM \
         (SELECT f.id, EXISTS (SELECT 1 FROM j.e e WHERE e.k = f.k + 1) AS x FROM j.f f) q",
        &["3656|344|0"],
    ),
    // Under CASE, in an aggregate's argument.
    (
        "SELECT sum(CASE WHEN NOT EXISTS (SELECT 1 FROM j.e e WHERE e.k = f.k AND e.s = f.s) \
         THEN f.id ELSE 0 END) FROM j.f f",
        &["6970950"],
    ),
    // Its FROM a step of its own.
    (
        "SELECT count(*) FROM j.f f WHERE NOT EXISTS \
         (SELECT 1 FROM (SELECT DISTINCT e.k, e.s FROM j.e e) d WHERE d.k = f.k AND d.s = f.s)",
        &["3483"],
    ),
    // Two in one WHERE, the second's conjunct naming both sides by <>.
    (
        "SELECT count(*), sum(f.id) FROM j.f f WHERE EXISTS (SELECT 1 FROM j.e e WHERE e.k = f.k) \
         AND NOT EXISTS (SELECT 1 FROM j.e e WHERE e.k = f.k AND e.s = f.s AND e.m <> f.n)",
        &["3141|6288124"],
    ),
    // A conjunct naming the query around it alone, met on each pair.
    (
        "SELECT count(*) FROM j.f f \
         WHERE NOT EXISTS (SELECT 1 FROM j.e e WHERE e.k = f.k AND f.n > 5)",
        &["2342"],
    ),
];

/// A subquery not read as a join, its step made once a round: a scalar subquery, with PostgreSQL
/// 18.6's answer.
const SCALAR: (&str, &[&str]) = (
    "SELECT sum((SELECT max(e.m) FROM j.e e WHERE e.k = f.k)) FROM j.f f",
    &["29883"],
);

/// EXISTS not read as joins, each with PostgreSQL 18.6's answer: one over an aggregate, and one
/// read on each group.
const NOT_JOINED: [(&str, &[&str]); 2] = [
    (
        "SELECT count(*) FROM j.f f \
         WHERE EXISTS (SELECT count(*) FROM j.e e WHERE e.k = f.k AND e.m > 100)",
        &["4000"],
    ),
    (
        "SELECT f.k, EXISTS (SELECT 1 FROM j.e e WHERE e.k = f.k) FROM j.f f \
         WHERE f.k < 27 OR f.k IS NULL GROUP BY f.k HAVING f.k IS NULL OR f.k % 13 < 2 \
         ORDER BY f.k NULLS FIRST",
        &["|false", "0|false", "1|true", "13|false", "14|true", "26|false"],
    ),
];

/// The first 300 rows asking that no row meets: those of the keys a multiple of 13 and those
/// whose key is NULL, in the order they came.
const UNMET: &str = "13,26,39,52,65,78,91,97,104,117,130,143,156,169,182,194,195,208,221,234,\
                     247,260,273,286,291,299";

#[test]
fn an_exists_tied_by_equalities_answers_as_postgresql_does() {
    let path = snail("answers");
    for (q, expected) in JOINED.iter().chain(&NOT_JOINED).chain([&SCALAR]) {
        let round = same(&path, None, q, expected);
        assert_eq!(round.spill_pages_written, 0, "{q}");
    }
}

#[test]
fn past_the_room_an_exists_s_keys_are_cut_on_their_hash_and_each_read_back_once() {
    let path = snail("past");
    // The rows asked give about 2,800 keys, 9 bytes each, past a room of 8 KiB.
    let room = 8 << 10;
    for (q, expected) in &JOINED {
        let round = same(&path, Some(room), q, expected);
        // The rows asking are written once and read back three times: to foresee their keys, to
        // meet the parts, and as the step reads them; each part's rows once.
        assert!(round.spill_pages_written > 0, "{q}: {round:?}");
        assert!(
            round.spill_pages_read <= 3 * round.spill_pages_written,
            "{q}: {round:?}"
        );
        assert_eq!(std::fs::read_dir(spill_dir(&path)).unwrap().count(), 0);
    }
    for (q, expected) in &NOT_JOINED {
        same(&path, Some(room), q, expected);
    }
}

#[test]
fn rows_an_exists_keeps_come_in_the_order_they_came() {
    let path = snail("order");
    let q = "SELECT f.id FROM j.f f WHERE f.id <= 300 \
             AND NOT EXISTS (SELECT 1 FROM j.e e WHERE e.k = f.k)";
    let expected: Vec<&str> = UNMET.split(',').collect();
    for room in [None, Some(4 << 10)] {
        same(&path, room, q, &expected);
    }
    // As PostgreSQL 18.6 aggregates them in that order.
    let q = "SELECT string_agg(f.id::text, ',' ORDER BY f.id) FROM j.f f \
             WHERE f.id <= 300 AND NOT EXISTS (SELECT 1 FROM j.e e WHERE e.k = f.k)";
    same(&path, Some(4 << 10), q, &[UNMET]);
}
