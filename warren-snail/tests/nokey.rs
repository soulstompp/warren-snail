//! A table whose order is not unique, rows alike sharing a place, written as PostgreSQL 18.6 writes
//! a heap: a DELETE or an UPDATE by WHERE changes every row alike it finds, a join meeting a row
//! more than once changes it once, by its ctid, WHERE CURRENT OF changes one row of those holding
//! the cursor's row's values, and a statement run again after another writer changes no more rows
//! alike than it found, one gone since refused at REPEATABLE READ. Every answer and refusal is
//! 18.6's over the same statements, each table under CLUSTER ON an index not unique. A snail writes
//! on its own only with `standalone`, as `tests/ddl.rs`.
#![cfg(feature = "standalone")]

mod weather;

use std::path::PathBuf;
use warren_snail::catalog::Catalog;
use warren_snail::read::Snail;
use weather::{tmp, write};

/// A snail of `ddl`, its rows inserted, at a path of the test's name.
fn declared(name: &str, ddl: &str, inserts: &[&str]) -> PathBuf {
    let path = tmp(&format!("nokey-{name}.snail"));
    write(&path, &Catalog::parse(ddl).unwrap(), &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    for i in inserts {
        w.execute(i).unwrap_or_else(|e| panic!("{i}: {e}"));
    }
    path
}

/// The rows a write changed.
fn changed(w: &mut Snail, sql: &str) -> u64 {
    w.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}")).rows
}

/// An answer's rows, each value as PostgreSQL's output function writes it, `|` between them.
fn rows(w: &mut Snail, sql: &str) -> Vec<String> {
    let done = w.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    let b = done.answer.expect("an answer");
    w.text_of(&b)
        .unwrap()
        .into_iter()
        .map(|r| {
            r.into_iter()
                .map(|v| v.unwrap_or_else(|| "NULL".into()))
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

const DUP: &str = "CREATE SCHEMA s;
     CREATE TABLE s.ix (a integer NOT NULL, b text, c integer);
     CREATE INDEX ix_b ON s.ix (b);
     CREATE INDEX ix_o ON s.ix (a, b, c);
     ALTER TABLE s.ix CLUSTER ON ix_o;
     CREATE TABLE s.j (a integer NOT NULL, b text, c integer);
     CREATE INDEX j_o ON s.j (a, b, c);
     ALTER TABLE s.j CLUSTER ON j_o;
     CREATE TABLE s.k (a integer NOT NULL, b text, c integer);
     CREATE INDEX k_o ON s.k (a, b, c);
     ALTER TABLE s.k CLUSTER ON k_o;";

#[test]
fn rows_alike_are_each_deleted_and_updated() {
    let path = declared("dup", DUP, &[]);
    let mut w = Snail::open_writing(&path).unwrap();
    // Written in one statement, beside an index of their own, and in two.
    for (t, inserts) in [
        (
            "s.ix",
            &["INSERT INTO s.ix VALUES (1, 'r', 7), (1, 'r', 7), (1, 'p', 8), (2, 'q', NULL)"][..],
        ),
        (
            "s.j",
            &["INSERT INTO s.j VALUES (1, 'r', 7), (1, 'r', 7), (1, 'p', 8), (2, 'q', NULL)"][..],
        ),
        (
            "s.k",
            &[
                "INSERT INTO s.k VALUES (1, 'r', 7)",
                "INSERT INTO s.k VALUES (1, 'r', 7)",
                "INSERT INTO s.k VALUES (1, 'p', 8), (2, 'q', NULL)",
            ][..],
        ),
    ] {
        for i in inserts {
            changed(&mut w, i);
        }
        assert_eq!(
            changed(&mut w, &format!("DELETE FROM {t} WHERE a = 1")),
            3,
            "{t}"
        );
        assert_eq!(
            rows(&mut w, &format!("SELECT count(*) AS n FROM {t}")),
            ["1"],
            "{t}"
        );
        assert_eq!(
            rows(&mut w, &format!("SELECT a, b, c FROM {t} ORDER BY a")),
            ["2|q|NULL"],
            "{t}"
        );
    }
    assert_eq!(
        rows(
            &mut w,
            "SELECT b, count(*) AS n FROM s.ix GROUP BY b ORDER BY b"
        ),
        ["q|1"]
    );
    changed(&mut w, "INSERT INTO s.k VALUES (3, 'x', 1), (3, 'x', 1)");
    assert_eq!(changed(&mut w, "UPDATE s.k SET c = 2 WHERE a = 3"), 2);
    assert_eq!(
        rows(&mut w, "SELECT a, b, c FROM s.k ORDER BY a, c"),
        ["2|q|NULL", "3|x|2", "3|x|2"]
    );
}

const DUP2: &str = "CREATE SCHEMA s;
     CREATE TABLE s.k (a integer NOT NULL, b text);
     CREATE INDEX k_o ON s.k (a, b);
     ALTER TABLE s.k CLUSTER ON k_o;
     CREATE TABLE s.u (a integer NOT NULL, n integer);
     CREATE INDEX u_o ON s.u (a, n);
     ALTER TABLE s.u CLUSTER ON u_o;";

#[test]
fn a_join_meeting_rows_alike_changes_each_once() {
    let path = declared(
        "join",
        DUP2,
        &[
            "INSERT INTO s.k VALUES (1, 'x'), (1, 'x'), (2, 'y'), (2, 'y'), (3, 'z')",
            "INSERT INTO s.u VALUES (1, 10), (1, 11), (2, 20)",
        ],
    );
    let mut w = Snail::open_writing(&path).unwrap();
    assert_eq!(
        changed(
            &mut w,
            "UPDATE s.k SET b = 'w' FROM s.u WHERE u.a = s.k.a AND u.a = 1"
        ),
        2
    );
    assert_eq!(
        rows(&mut w, "SELECT a, b FROM s.k ORDER BY a, b"),
        ["1|w", "1|w", "2|y", "2|y", "3|z"]
    );
    assert_eq!(
        changed(
            &mut w,
            "DELETE FROM s.k USING s.u WHERE u.a = s.k.a AND u.a = 2"
        ),
        2
    );
    assert_eq!(
        rows(&mut w, "SELECT a, b FROM s.k ORDER BY a, b"),
        ["1|w", "1|w", "3|z"]
    );
}

/// Each row its own ctid, rows alike among them, met by a join as often as the join meets it.
#[test]
fn a_ctid_is_read_through_a_join() {
    let path = declared(
        "ctid",
        DUP2,
        &[
            "INSERT INTO s.k VALUES (1, 'x'), (1, 'x'), (2, 'y')",
            "INSERT INTO s.u VALUES (1, 10), (1, 11), (2, 20)",
        ],
    );
    let mut w = Snail::open_writing(&path).unwrap();
    assert_eq!(
        rows(
            &mut w,
            "SELECT count(k.ctid) AS n, count(DISTINCT k.ctid) AS d FROM s.k k \
             JOIN s.u u ON u.a = k.a"
        ),
        ["5|3"]
    );
    assert_eq!(
        rows(
            &mut w,
            "SELECT k.a, count(DISTINCT k.ctid) AS d FROM s.k k JOIN s.u u ON u.a = k.a \
             GROUP BY k.a ORDER BY 1"
        ),
        ["1|2", "2|1"]
    );
}

/// A select list's truth over a ctid, a tableoid or a column, a test for NULL or a comparison, on a
/// table with no unique key and on one with a key.
#[test]
fn a_truth_over_a_ctid_or_a_column_is_returned() {
    let path = declared(
        "truth",
        "CREATE SCHEMA s;
         CREATE TABLE s.k (a integer NOT NULL, b text);
         CREATE INDEX k_o ON s.k (a, b);
         ALTER TABLE s.k CLUSTER ON k_o;
         CREATE TABLE s.t (id integer PRIMARY KEY, b text);",
        &[
            "INSERT INTO s.k VALUES (1, 'x'), (1, 'x'), (2, 'y')",
            "INSERT INTO s.t VALUES (1, 'x'), (2, NULL)",
        ],
    );
    let mut w = Snail::open_writing(&path).unwrap();
    for (sql, want) in [
        ("SELECT k.ctid IS NOT NULL AS has FROM s.k k", &["t", "t", "t"][..]),
        (
            "SELECT ctid IS NULL AS c, a FROM s.k ORDER BY a",
            &["f|1", "f|1", "f|2"][..],
        ),
        ("SELECT k.ctid = k.ctid AS c FROM s.k k", &["t", "t", "t"][..]),
        (
            "SELECT (k.ctid IS NOT NULL) AND k.a > 1 AS c FROM s.k k ORDER BY 1",
            &["f", "f", "t"][..],
        ),
        (
            "SELECT k.tableoid = 's.k'::regclass AS c FROM s.k k",
            &["t", "t", "t"][..],
        ),
        (
            "SELECT t.ctid IS NOT NULL AS c, t.id FROM s.t t ORDER BY t.id",
            &["t|1", "t|2"][..],
        ),
        ("SELECT b IS NOT NULL AS c FROM s.t ORDER BY id", &["t", "f"][..]),
        (
            "SELECT t.b IS NULL AS c, id FROM s.t t ORDER BY id",
            &["f|1", "t|2"][..],
        ),
        ("SELECT id = 1 AS c FROM s.t ORDER BY id", &["t", "f"][..]),
        ("SELECT id > 1 FROM s.t ORDER BY id", &["f", "t"][..]),
        ("SELECT NOT (b IS NULL) AS c FROM s.t ORDER BY id", &["t", "f"][..]),
        (
            "SELECT b IS DISTINCT FROM 'x' AS c FROM s.t ORDER BY id",
            &["f", "t"][..],
        ),
        (
            "SELECT id > 1 OR b IS NULL AS c FROM s.t ORDER BY id",
            &["f", "t"][..],
        ),
    ] {
        assert_eq!(rows(&mut w, sql), want, "{sql}");
    }
}

#[test]
fn current_of_changes_one_of_two_rows_alike() {
    let path = declared(
        "current-of",
        DUP2,
        &["INSERT INTO s.k VALUES (1, 'w'), (1, 'w'), (3, 'z')"],
    );
    let mut w = Snail::open_writing(&path).unwrap();
    changed(&mut w, "BEGIN");
    changed(
        &mut w,
        "DECLARE c CURSOR FOR SELECT a, b FROM s.k WHERE a = 1 FOR UPDATE",
    );
    assert_eq!(rows(&mut w, "FETCH 1 FROM c"), ["1|w"]);
    assert_eq!(
        changed(&mut w, "UPDATE s.k SET b = 'v' WHERE CURRENT OF c"),
        1
    );
    changed(&mut w, "COMMIT");
    assert_eq!(
        rows(&mut w, "SELECT a, b FROM s.k ORDER BY a, b"),
        ["1|v", "1|w", "3|z"]
    );
    // A cursor over an aggregate keeps no row's values: its one row is a group, here as on a table
    // with a key.
    changed(&mut w, "BEGIN");
    changed(&mut w, "DECLARE g CURSOR FOR SELECT count(*) AS n FROM s.k");
    assert_eq!(rows(&mut w, "FETCH 1 FROM g"), ["3"]);
    changed(&mut w, "COMMIT");
    let path = declared(
        "current-of-keyed",
        "CREATE SCHEMA s; CREATE TABLE s.t (id integer PRIMARY KEY, b text);",
        &["INSERT INTO s.t VALUES (1, 'x'), (2, 'y')"],
    );
    let mut t = Snail::open_writing(&path).unwrap();
    changed(&mut t, "BEGIN");
    changed(&mut t, "DECLARE h CURSOR FOR SELECT count(*) AS n FROM s.t");
    assert_eq!(rows(&mut t, "FETCH 1 FROM h"), ["2"]);
    changed(&mut t, "COMMIT");
    // A row with NULLs, a json and a point beside them: each compared as 18.6 writes it.
    let path = declared(
        "current-of-types",
        "CREATE SCHEMA s;
         CREATE TABLE s.m (a integer NOT NULL, b text, j json, p point);
         CREATE INDEX m_o ON s.m (a);
         ALTER TABLE s.m CLUSTER ON m_o;",
        &["INSERT INTO s.m VALUES (1, NULL, '{\"x\": 1}', '(1,2)'), (1, NULL, '{\"x\": 1}', '(1,2)'), \
           (1, 'y', NULL, NULL), (2, NULL, '[1]', '(0,0)')"],
    );
    let mut w = Snail::open_writing(&path).unwrap();
    let all = "SELECT a, b, j::text AS j, p::text AS p FROM s.m ORDER BY a, b, j::text";
    changed(&mut w, "BEGIN");
    changed(
        &mut w,
        "DECLARE c CURSOR FOR SELECT a, b FROM s.m WHERE b IS NULL AND a = 1 FOR UPDATE",
    );
    assert_eq!(rows(&mut w, "FETCH 1 FROM c"), ["1|NULL"]);
    assert_eq!(
        changed(&mut w, "UPDATE s.m SET b = 'v' WHERE CURRENT OF c"),
        1
    );
    changed(&mut w, "COMMIT");
    assert_eq!(
        rows(&mut w, all),
        [
            "1|v|{\"x\": 1}|(1,2)",
            "1|y|NULL|NULL",
            "1|NULL|{\"x\": 1}|(1,2)",
            "2|NULL|[1]|(0,0)"
        ]
    );
    changed(
        &mut w,
        "INSERT INTO s.m VALUES (3, 'q', '[2, 3]', '(3,3)'), (3, 'q', '[2, 3]', '(3,3)')",
    );
    changed(&mut w, "BEGIN");
    changed(
        &mut w,
        "DECLARE d CURSOR FOR SELECT a FROM s.m WHERE a = 3 FOR UPDATE",
    );
    assert_eq!(rows(&mut w, "FETCH 1 FROM d"), ["3"]);
    assert_eq!(changed(&mut w, "DELETE FROM s.m WHERE CURRENT OF d"), 1);
    changed(&mut w, "COMMIT");
    changed(&mut w, "BEGIN");
    changed(
        &mut w,
        "DECLARE e CURSOR FOR SELECT a, b FROM s.m WHERE a = 2 FOR UPDATE",
    );
    assert_eq!(rows(&mut w, "FETCH 1 FROM e"), ["2|NULL"]);
    assert_eq!(
        changed(&mut w, "UPDATE s.m SET b = 'w' WHERE CURRENT OF e"),
        1
    );
    changed(&mut w, "COMMIT");
    assert_eq!(
        rows(&mut w, all),
        [
            "1|v|{\"x\": 1}|(1,2)",
            "1|y|NULL|NULL",
            "1|NULL|{\"x\": 1}|(1,2)",
            "2|w|[1]|(0,0)",
            "3|q|[2, 3]|(3,3)"
        ]
    );
}

/// A cursor whose select list holds a `*` keeps each row's key, or each row's values where its
/// table has none, past its own columns, as one naming them does.
#[test]
fn current_of_reads_a_cursor_over_a_star() {
    let path = declared(
        "current-of-star",
        "CREATE SCHEMA s;
         CREATE TABLE s.m (a integer NOT NULL, b text, j json, p point);
         CREATE INDEX m_o ON s.m (a);
         ALTER TABLE s.m CLUSTER ON m_o;
         CREATE TABLE s.t (id integer PRIMARY KEY, b text);",
        &[
            "INSERT INTO s.m VALUES (1, NULL, '{\"x\": 1}', '(1,2)'), \
             (1, NULL, '{\"x\": 1}', '(1,2)'), (2, NULL, '[1]', '(0,0)')",
            "INSERT INTO s.t VALUES (1, 'x'), (2, 'y'), (3, 'z')",
        ],
    );
    let mut w = Snail::open_writing(&path).unwrap();
    changed(&mut w, "BEGIN");
    changed(
        &mut w,
        "DECLARE e CURSOR FOR SELECT * FROM s.m WHERE a = 2 FOR UPDATE",
    );
    assert_eq!(rows(&mut w, "FETCH 1 FROM e"), ["2|NULL|[1]|(0,0)"]);
    assert_eq!(
        changed(&mut w, "UPDATE s.m SET b = 'w' WHERE CURRENT OF e"),
        1
    );
    changed(&mut w, "COMMIT");
    changed(&mut w, "BEGIN");
    changed(
        &mut w,
        "DECLARE f CURSOR FOR SELECT m.* FROM s.m m WHERE a = 1",
    );
    assert_eq!(rows(&mut w, "FETCH 1 FROM f"), ["1|NULL|{\"x\": 1}|(1,2)"]);
    assert_eq!(changed(&mut w, "DELETE FROM s.m WHERE CURRENT OF f"), 1);
    changed(&mut w, "COMMIT");
    assert_eq!(
        rows(
            &mut w,
            "SELECT a, b, j::text AS j, p::text AS p FROM s.m ORDER BY a, b"
        ),
        ["1|NULL|{\"x\": 1}|(1,2)", "2|w|[1]|(0,0)"]
    );
    changed(&mut w, "BEGIN");
    changed(
        &mut w,
        "DECLARE g CURSOR FOR SELECT * FROM s.t ORDER BY id DESC FOR UPDATE",
    );
    assert_eq!(rows(&mut w, "FETCH 1 FROM g"), ["3|z"]);
    assert_eq!(
        changed(&mut w, "UPDATE s.t SET b = 'v' WHERE CURRENT OF g"),
        1
    );
    changed(
        &mut w,
        "DECLARE h CURSOR FOR SELECT *, b AS id FROM s.t ORDER BY 1",
    );
    assert_eq!(rows(&mut w, "FETCH 2 FROM h"), ["1|x|x", "2|y|y"]);
    assert_eq!(changed(&mut w, "DELETE FROM s.t WHERE CURRENT OF h"), 1);
    changed(&mut w, "COMMIT");
    assert_eq!(
        rows(&mut w, "SELECT id, b FROM s.t ORDER BY id"),
        ["1|x", "3|v"]
    );
    // The key kept past the cursor's columns is named apart from them: ORDER BY reads the name
    // its select list gives.
    changed(&mut w, "BEGIN");
    changed(
        &mut w,
        "DECLARE k CURSOR FOR SELECT b AS id FROM s.t ORDER BY id DESC FOR UPDATE",
    );
    assert_eq!(rows(&mut w, "FETCH 1 FROM k"), ["x"]);
    assert_eq!(
        changed(&mut w, "UPDATE s.t SET b = 'u' WHERE CURRENT OF k"),
        1
    );
    changed(&mut w, "COMMIT");
    assert_eq!(
        rows(&mut w, "SELECT id, b FROM s.t ORDER BY id"),
        ["1|u", "3|v"]
    );
}

#[test]
fn a_row_alike_deleted_since_refuses_at_repeatable_read() {
    let path = declared(
        "rr",
        DUP2,
        &["INSERT INTO s.k VALUES (1, 'x'), (1, 'x'), (2, 'y')"],
    );
    let (mut a, mut b) = (
        Snail::open_writing(&path).unwrap(),
        Snail::open_writing(&path).unwrap(),
    );
    // Another deletes one of the rows alike the transaction's snapshot holds, then the last.
    for (write, before, left) in [
        ("DELETE FROM s.k WHERE a = 1", "3", &["1|x", "2|y"][..]),
        ("UPDATE s.k SET b = 'q' WHERE a = 1", "2", &["2|y"][..]),
    ] {
        changed(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
        assert_eq!(rows(&mut a, "SELECT count(*) AS n FROM s.k"), [before]);
        let one = "DELETE FROM s.k WHERE ctid = (SELECT ctid FROM s.k WHERE a = 1 LIMIT 1)";
        assert_eq!(changed(&mut b, one), 1);
        let e = a
            .execute(write)
            .err()
            .unwrap_or_else(|| panic!("{write} was taken"));
        assert_eq!(
            (e.sqlstate(), e.to_string().as_str()),
            (
                Some("40001"),
                "could not serialize access due to concurrent delete"
            ),
            "{write}"
        );
        changed(&mut a, "ROLLBACK");
        assert_eq!(
            rows(&mut a, "SELECT a, b FROM s.k ORDER BY a, b"),
            left,
            "{write}"
        );
    }
}

#[test]
fn a_statement_run_again_changes_no_more_rows_alike_than_it_found() {
    let path = declared(
        "rerun",
        DUP2,
        &["INSERT INTO s.k VALUES (1, 'x'), (1, 'x'), (2, 'y')"],
    );
    let mut a = Snail::open_writing(&path).unwrap();
    changed(&mut a, "BEGIN");
    changed(&mut a, "INSERT INTO s.k VALUES (1, 'x')");
    let q = path.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let t = std::thread::spawn(move || {
        let mut b = Snail::open_writing(&q).unwrap();
        let r = b
            .execute("DELETE FROM s.k WHERE a = 1")
            .map(|d| d.rows)
            .map_err(|e| e.to_string());
        tx.send(r).unwrap();
    });
    std::thread::sleep(std::time::Duration::from_millis(300));
    changed(&mut a, "COMMIT");
    let deleted = rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
    t.join().unwrap();
    assert_eq!(deleted, Ok(2));
    assert_eq!(
        rows(&mut a, "SELECT a, b FROM s.k ORDER BY a, b"),
        ["1|x", "2|y"]
    );
}

/// A statement waiting on another writer that updated the rows alike it found follows each to the
/// row standing in its place, and changes it where it still meets the statement's conditions.
#[test]
fn a_statement_run_again_follows_rows_alike_another_updated() {
    let path = declared(
        "rerun-updated",
        DUP2,
        &["INSERT INTO s.k VALUES (1, 'x'), (1, 'x'), (1, 'x'), (2, 'y'), (3, 'p'), (3, 'p')"],
    );
    let mut a = Snail::open_writing(&path).unwrap();
    for (theirs, ours, n, left) in [
        (
            "UPDATE s.k SET b = 'z' WHERE a = 1",
            "DELETE FROM s.k WHERE a = 1",
            3,
            &["2|y", "3|p", "3|p"][..],
        ),
        (
            "UPDATE s.k SET b = 'q' WHERE a = 3",
            "UPDATE s.k SET b = 'w' WHERE a = 3",
            2,
            &["2|y", "3|w", "3|w"][..],
        ),
    ] {
        changed(&mut a, "BEGIN");
        changed(&mut a, theirs);
        let q = path.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let t = std::thread::spawn(move || {
            let mut b = Snail::open_writing(&q).unwrap();
            let r = b.execute(ours).map(|d| d.rows).map_err(|e| e.to_string());
            tx.send(r).unwrap();
        });
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(rx.try_recv().is_err(), "{ours} waits");
        changed(&mut a, "COMMIT");
        let done = rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        t.join().unwrap();
        assert_eq!(done, Ok(n), "{ours}");
        assert_eq!(
            rows(&mut a, "SELECT a, b FROM s.k ORDER BY a, b"),
            left,
            "{ours}"
        );
    }
}
