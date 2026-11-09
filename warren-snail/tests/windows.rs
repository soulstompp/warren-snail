//! Windows on any order: a partition sorted on its window's own ORDER BY, another column, DESC,
//! NULLS FIRST or LAST, its peers those equal on every item; frames of ROWS, RANGE and GROUPS offset
//! from the current row either side, whole partitions and EXCLUDE; named windows; RESPECT and IGNORE
//! NULLS on 19, refused in 18's words on 18; over one table, a step and a join. Every answer is
//! PostgreSQL 19's and 18's over the same rows.

mod weather;

use arrow::array::{Array, RecordBatch};
use arrow::util::display::array_value_to_string;
use warren_snail::catalog::Catalog;
use warren_snail::values::Values;
use warren_snail::write::Rows;
use weather::{fresh, tmp};

const DDL: &str = "CREATE SCHEMA a; CREATE TABLE a.w (id integer NOT NULL, g integer, v integer, d double precision, s text, PRIMARY KEY (id));
     CREATE TABLE a.k (g integer NOT NULL, label text, PRIMARY KEY (g));";

/// The tables' snail, its catalog of `release`.
fn snail(name: &str, release: u8) -> std::path::PathBuf {
    let path = tmp(&format!("windows-{name}.snail"));
    let catalog = Catalog::parse(DDL).unwrap();
    let int = |v: &[Option<i64>]| Values::Int(v.to_vec());
    let text = |v: &[Option<&str>]| Values::Text(v.iter().map(|s| s.map(str::to_string)).collect());
    let double =
        |v: &[Option<f64>]| Values::Float8(v.iter().map(|d| d.map(f64::to_bits)).collect());
    let w = Rows {
        columns: vec![
            int(&[
                Some(1),
                Some(2),
                Some(3),
                Some(4),
                Some(5),
                Some(6),
                Some(7),
                Some(8),
            ]),
            int(&[
                Some(1),
                Some(1),
                Some(1),
                Some(2),
                Some(2),
                Some(2),
                Some(3),
                Some(1),
            ]),
            int(&[
                Some(5),
                None,
                Some(5),
                Some(1),
                Some(3),
                None,
                Some(2),
                Some(7),
            ]),
            double(&[
                Some(1.5),
                Some(2.5),
                None,
                Some(0.5),
                Some(1e300),
                Some(f64::NAN),
                Some(-1.0),
                Some(3.0),
            ]),
            text(&[
                Some("b"),
                Some("a"),
                Some("c"),
                None,
                Some("a"),
                Some("b"),
                Some("c"),
                Some("a"),
            ]),
        ],
    };
    let k = Rows {
        columns: vec![
            int(&[Some(1), Some(2), Some(3)]),
            text(&[Some("one"), Some("two"), Some("one")]),
        ],
    };
    let tables = [("a.w".into(), w), ("a.k".into(), k)];
    warren_snail::write::write_released(&path, &catalog, &tables, &[], weather::span(), release)
        .unwrap();
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

/// Rows written as text, a field of `-` NULL.
fn want(rows: &[&str]) -> Vec<Row> {
    rows.iter()
        .map(|r| {
            r.split(',')
                .map(|v| (v != "-").then(|| v.to_string()))
                .collect()
        })
        .collect()
}

fn same(path: &std::path::Path, q: &str, expected: &[&str]) {
    let (b, _) = fresh(path)
        .ask(q, None)
        .unwrap_or_else(|e| panic!("{q}: {e}"));
    assert_eq!(rows(&b), want(expected), "{q}");
}

fn refused(path: &std::path::Path, q: &str) -> String {
    match fresh(path).ask(q, None) {
        Ok((b, _)) => panic!("{q}: answered {:?}", rows(&b)),
        Err(e) => e.to_string(),
    }
}

#[test]
fn a_window_reads_its_partition_sorted_on_its_own_order_peers_and_all() {
    let path = snail("order", 19);
    same(
        &path,
        "SELECT id, rank() OVER w, dense_rank() OVER w, percent_rank() OVER w, cume_dist() OVER w, \
         count(*) OVER w, sum(id) OVER w FROM a.w WINDOW w AS (ORDER BY v) ORDER BY id",
        &[
            "1,4,4,0.42857142857142855,0.625,5,20",
            "2,7,6,0.8571428571428571,1.0,8,36",
            "3,4,4,0.42857142857142855,0.625,5,20",
            "4,1,1,0.0,0.125,1,4",
            "5,3,3,0.2857142857142857,0.375,3,16",
            "6,7,6,0.8571428571428571,1.0,8,36",
            "7,2,2,0.14285714285714285,0.25,2,11",
            "8,6,5,0.7142857142857143,0.75,6,28",
        ],
    );
    same(
        &path,
        "SELECT id, row_number() OVER (PARTITION BY g ORDER BY v DESC NULLS LAST, id), \
         first_value(id) OVER (PARTITION BY g ORDER BY v NULLS FIRST, id), \
         lag(id) OVER (PARTITION BY g ORDER BY s DESC, id), \
         lead(id, 2, -1) OVER (ORDER BY s NULLS FIRST, id) FROM a.w ORDER BY id",
        &[
            "1,2,2,3,3",
            "2,4,2,1,8",
            "3,3,2,-,-1",
            "4,2,6,-,5",
            "5,1,6,6,1",
            "6,3,6,4,7",
            "7,1,7,-,-1",
            "8,1,2,2,6",
        ],
    );
    // Over a step, and named windows, one naming another.
    same(
        &path,
        "WITH x AS (SELECT id, g, v FROM a.w WHERE id > 1) SELECT id, dense_rank() OVER (ORDER BY g DESC) AS r, \
         sum(v) OVER (PARTITION BY g ORDER BY v NULLS FIRST, id) AS s FROM x ORDER BY id",
        &["2,3,-", "3,3,5", "4,2,1", "5,2,4", "6,2,-", "7,1,2", "8,3,12"],
    );
    same(
        &path,
        "SELECT id, sum(id) OVER o AS a, max(id) OVER (p ORDER BY id ROWS 1 PRECEDING) AS b FROM a.w \
         WINDOW p AS (PARTITION BY g), o AS (p ORDER BY v) ORDER BY id",
        &["1,4,1", "2,14,2", "3,4,3", "4,4,4", "5,9,5", "6,15,6", "7,7,7", "8,12,8"],
    );
}

#[test]
fn frames_run_offset_from_the_current_row_by_rows_range_and_groups() {
    let path = snail("frames", 19);
    same(
        &path,
        "SELECT id, sum(id) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING), \
         sum(id) OVER (ORDER BY v RANGE BETWEEN 1 PRECEDING AND 2 FOLLOWING), \
         count(*) OVER (ORDER BY v GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW), \
         min(id) OVER (ORDER BY v DESC RANGE BETWEEN 2 PRECEDING AND 1 PRECEDING), \
         sum(id) OVER (ORDER BY d RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM a.w ORDER BY id",
        &[
            "1,3,12,3,8,7",
            "2,6,8,3,2,11",
            "3,9,12,3,8,3",
            "4,12,16,1,5,5",
            "5,15,16,2,1,5",
            "6,18,8,3,2,6",
            "7,21,16,2,5,7",
            "8,15,8,3,-,10",
        ],
    );
    same(
        &path,
        "SELECT id, sum(id) OVER (ORDER BY v, id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING EXCLUDE CURRENT ROW), \
         sum(id) OVER (ORDER BY v RANGE BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING EXCLUDE GROUP), \
         first_value(id) OVER (ORDER BY v RANGE BETWEEN CURRENT ROW AND 2 FOLLOWING EXCLUDE TIES), \
         last_value(id) OVER (ORDER BY v GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING EXCLUDE TIES), \
         nth_value(id, 2) OVER (ORDER BY v ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING EXCLUDE GROUP) \
         FROM a.w ORDER BY id",
        &[
            "1,8,32,1,8,7",
            "2,14,28,2,2,7",
            "3,9,32,3,8,7",
            "4,7,32,4,7,5",
            "5,8,31,5,3,7",
            "6,2,28,6,6,7",
            "7,9,29,7,5,5",
            "8,5,28,8,6,7",
        ],
    );
    // Along the table's order a frame that leaves a row out is read whole too, and a named window
    // takes a frame that excludes.
    same(
        &path,
        "SELECT id, sum(id) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW EXCLUDE CURRENT ROW), \
         count(*) OVER (o ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING EXCLUDE CURRENT ROW) FROM a.w \
         WINDOW o AS (ORDER BY id) ORDER BY id",
        &["1,-,1", "2,1,2", "3,3,2", "4,6,2", "5,10,2", "6,15,2", "7,21,2", "8,28,1"],
    );
}

#[test]
fn a_running_window_filtered_by_kind_reaches_forward_and_back() {
    // warren-scout-pg's spelling: the next and the previous row of a partition that holds a value.
    let path = snail("scout", 19);
    same(
        &path,
        "SELECT id, min(id) FILTER (WHERE v IS NOT NULL) OVER (PARTITION BY g ORDER BY id \
         ROWS BETWEEN 1 FOLLOWING AND UNBOUNDED FOLLOWING), max(id) FILTER (WHERE v IS NOT NULL) \
         OVER (PARTITION BY g ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND 1 PRECEDING) \
         FROM a.w ORDER BY id",
        &[
            "1,3,-", "2,3,1", "3,8,1", "4,5,-", "5,-,4", "6,-,5", "7,-,-", "8,-,3",
        ],
    );
}

#[test]
fn ignore_nulls_is_read_on_19_and_refused_in_18_s_words_on_18() {
    let q = "SELECT id, last_value(v) IGNORE NULLS OVER (ORDER BY id), lag(v) IGNORE NULLS OVER (ORDER BY id), \
         lead(v, 2) IGNORE NULLS OVER (ORDER BY id), first_value(v) IGNORE NULLS OVER (ORDER BY id \
         ROWS BETWEEN 1 FOLLOWING AND UNBOUNDED FOLLOWING), nth_value(v, 2) IGNORE NULLS OVER (ORDER BY id \
         ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING), lag(v) RESPECT NULLS OVER (ORDER BY id) \
         FROM a.w ORDER BY id";
    same(
        &snail("nulls-19", 19),
        q,
        &[
            "1,5,-,1,5,5,-",
            "2,5,5,1,5,5,5",
            "3,5,5,3,1,5,-",
            "4,1,5,2,3,5,5",
            "5,3,1,7,2,5,1",
            "6,3,3,7,2,5,3",
            "7,2,3,-,7,5,-",
            "8,7,2,-,-,5,2",
        ],
    );
    // 18 reads a word after a call outside any parentheses as its item's alias, and stops at NULLS;
    // inside parentheses it stops at the word itself.
    let old = snail("nulls-18", 18);
    assert_eq!(refused(&old, q), "syntax error at or near \"NULLS\"");
    assert_eq!(
        refused(
            &old,
            "SELECT id, (lag(v) RESPECT NULLS OVER (ORDER BY id)) AS x FROM a.w"
        ),
        "syntax error at or near \"RESPECT\""
    );
    assert_eq!(
        refused(
            &old,
            "SELECT id, 1 + lag(v) IGNORE NULLS OVER (ORDER BY id) AS x FROM a.w"
        ),
        "syntax error at or near \"NULLS\""
    );
    assert_eq!(
        refused(
            &old,
            "SELECT id, coalesce(lag(v) IGNORE NULLS OVER (ORDER BY id), 0) AS x FROM a.w"
        ),
        "syntax error at or near \"IGNORE\""
    );
}

#[test]
fn a_window_over_a_join_reads_its_rows_whole() {
    let path = snail("join", 19);
    same(
        &path,
        "SELECT w.id, k.label, rank() OVER (PARTITION BY k.label ORDER BY w.v NULLS FIRST) AS r, \
         count(*) OVER (PARTITION BY k.label) AS n FROM a.w w JOIN a.k k ON k.g = w.g ORDER BY 1",
        &[
            "1,one,3,5",
            "2,one,1,5",
            "3,one,3,5",
            "4,two,2,3",
            "5,two,3,3",
            "6,two,1,3",
            "7,one,2,5",
            "8,one,5,5",
        ],
    );
}

#[test]
fn a_window_postgresql_refuses_is_refused_in_its_words() {
    let path = snail("refused", 19);
    for (w, why) in [
        (
            "sum(v) OVER (ORDER BY v, id RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING)",
            "RANGE with offset PRECEDING/FOLLOWING requires exactly one ORDER BY column",
        ),
        (
            "sum(v) OVER (ORDER BY s RANGE BETWEEN 1 PRECEDING AND CURRENT ROW)",
            "RANGE with offset PRECEDING/FOLLOWING is not supported for column type text",
        ),
        (
            "sum(v) OVER (ORDER BY v RANGE BETWEEN 1.5 PRECEDING AND CURRENT ROW)",
            "RANGE with offset PRECEDING/FOLLOWING is not supported for column type integer and \
             offset type numeric",
        ),
        (
            "sum(v) OVER (ROWS BETWEEN -1 PRECEDING AND CURRENT ROW)",
            "frame starting offset must not be negative",
        ),
        (
            "sum(v) OVER (ROWS BETWEEN NULL PRECEDING AND CURRENT ROW)",
            "frame starting offset must not be null",
        ),
        (
            "sum(v) OVER (ORDER BY v RANGE BETWEEN -1 PRECEDING AND CURRENT ROW)",
            "invalid preceding or following size in window function",
        ),
        (
            "sum(v) OVER (ROWS BETWEEN v PRECEDING AND CURRENT ROW)",
            "argument of ROWS must not contain variables",
        ),
        (
            "sum(v) OVER (GROUPS 1 PRECEDING)",
            "GROUPS mode requires an ORDER BY clause",
        ),
        (
            "sum(v) OVER (ROWS UNBOUNDED FOLLOWING)",
            "frame start cannot be UNBOUNDED FOLLOWING",
        ),
        (
            "sum(v) OVER (ROWS BETWEEN 1 FOLLOWING AND CURRENT ROW)",
            "frame starting from following row cannot have preceding rows",
        ),
        (
            "lag(v) FILTER (WHERE v > 1) OVER ()",
            "FILTER is not implemented for non-aggregate window functions",
        ),
        (
            "sum(v) IGNORE NULLS OVER ()",
            "aggregate functions do not accept RESPECT/IGNORE NULLS",
        ),
        (
            "row_number() IGNORE NULLS OVER ()",
            "function row_number does not allow RESPECT/IGNORE NULLS",
        ),
        (
            "sum(DISTINCT v) OVER ()",
            "DISTINCT is not implemented for window functions",
        ),
    ] {
        let e = refused(&path, &format!("SELECT id, {w} AS x FROM a.w ORDER BY id"));
        assert_eq!(e, why, "{w}");
    }
    for (q, why) in [
        (
            "SELECT id, sum(v) OVER (w ORDER BY id) AS x FROM a.w WINDOW w AS (ORDER BY g)",
            "cannot override ORDER BY clause of window \"w\"",
        ),
        (
            "SELECT id, sum(v) OVER (w PARTITION BY id) AS x FROM a.w WINDOW w AS (ORDER BY g)",
            "cannot override PARTITION BY clause of window \"w\"",
        ),
        (
            "SELECT id, sum(v) OVER (w) AS x FROM a.w WINDOW w AS (ORDER BY g ROWS 1 PRECEDING)",
            "cannot copy window \"w\" because it has a frame clause",
        ),
        (
            "SELECT id, sum(v) OVER x AS x FROM a.w WINDOW w AS ()",
            "window \"x\" does not exist",
        ),
        (
            "SELECT id, sum(v) OVER w AS x FROM a.w WINDOW w AS (ORDER BY g), w AS (ORDER BY id)",
            "window \"w\" is already defined",
        ),
    ] {
        assert_eq!(refused(&path, q), why, "{q}");
    }
}
