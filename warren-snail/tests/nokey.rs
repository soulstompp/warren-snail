//! A table whose order is not unique, rows alike sharing a place, written as PostgreSQL 18.6 writes
//! a heap: a DELETE or an UPDATE by WHERE changes every row alike it finds, a join meeting a row
//! more than once changes it once, by its ctid, WHERE CURRENT OF changes one row of those holding
//! the cursor's row's values, and a statement run again after another writer changes no more rows
//! alike than it found, one gone since refused at REPEATABLE READ. Every answer and refusal is
//! 18.6's over the same statements, each table both under CLUSTER ON an index not unique and with
//! no key at all, laid out along every column it orders: an order no index of the catalog is. A
//! snail writes on its own only with `standalone`, as `tests/ddl.rs`.
#![cfg(feature = "standalone")]

mod weather;

use std::path::PathBuf;
use warren_snail::catalog::{Catalog, GIVEN};
use warren_snail::read::Snail;
use warren_snail::values::Values;
use warren_snail::write::Rows;
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

/// `DUP` with no key: each table laid out along every column it orders.
const DUP_KEYLESS: &str = "CREATE SCHEMA s;
     CREATE TABLE s.ix (a integer NOT NULL, b text, c integer);
     CREATE INDEX ix_b ON s.ix (b);
     CREATE TABLE s.j (a integer NOT NULL, b text, c integer);
     CREATE TABLE s.k (a integer NOT NULL, b text, c integer);";

#[test]
fn rows_alike_are_each_deleted_and_updated() {
    for (form, ddl) in [("clustered", DUP), ("keyless", DUP_KEYLESS)] {
        each_deleted_and_updated(form, ddl);
    }
}

fn each_deleted_and_updated(form: &str, ddl: &str) {
    let path = declared(&format!("dup-{form}"), ddl, &[]);
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
            "{form} {t}"
        );
        assert_eq!(
            rows(&mut w, &format!("SELECT count(*) AS n FROM {t}")),
            ["1"],
            "{form} {t}"
        );
        assert_eq!(
            rows(&mut w, &format!("SELECT a, b, c FROM {t} ORDER BY a")),
            ["2|q|NULL"],
            "{form} {t}"
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

/// `DUP2` with no key.
const DUP2_KEYLESS: &str = "CREATE SCHEMA s;
     CREATE TABLE s.k (a integer NOT NULL, b text);
     CREATE TABLE s.u (a integer NOT NULL, n integer);";

/// Each form of `DUP2`, by its name.
const FORMS: [(&str, &str); 2] = [("clustered", DUP2), ("keyless", DUP2_KEYLESS)];

#[test]
fn a_join_meeting_rows_alike_changes_each_once() {
    for (form, ddl) in FORMS {
        a_join_changes_each_once(form, ddl);
    }
}

fn a_join_changes_each_once(form: &str, ddl: &str) {
    let path = declared(
        &format!("join-{form}"),
        ddl,
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
    for (form, ddl) in FORMS {
        a_ctid_through_a_join(form, ddl);
    }
}

fn a_ctid_through_a_join(form: &str, ddl: &str) {
    let path = declared(
        &format!("ctid-{form}"),
        ddl,
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
    for (form, ddl, m) in [
        (
            "clustered",
            DUP2,
            "CREATE INDEX m_o ON s.m (a); ALTER TABLE s.m CLUSTER ON m_o;",
        ),
        ("keyless", DUP2_KEYLESS, ""),
    ] {
        current_of_one_of_two(form, ddl, m);
    }
}

fn current_of_one_of_two(form: &str, ddl: &str, m: &str) {
    let path = declared(
        &format!("current-of-{form}"),
        ddl,
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
        &format!("current-of-keyed-{form}"),
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
        &format!("current-of-types-{form}"),
        &format!(
            "CREATE SCHEMA s;
             CREATE TABLE s.m (a integer NOT NULL, b text, j json, p point);
             {m}"
        ),
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

/// A row in a block the transaction staged has a ctid of its own, past every block the root names:
/// read in a select list, met by WHERE ctid = …, and so by CURRENT OF a cursor declared after it.
#[test]
fn a_ctid_of_a_block_the_transaction_staged_is_read() {
    for (form, m) in [
        ("clustered", "CREATE INDEX m_a ON s.m (a); ALTER TABLE s.m CLUSTER ON m_a;"),
        ("keyless", ""),
    ] {
        a_staged_block_s_ctid(form, m);
    }
    for (form, ddl) in FORMS {
        a_staged_ctid(form, ddl);
    }
}

fn a_staged_block_s_ctid(form: &str, m: &str) {
    let path = declared(
        &format!("staged-block-ctid-{form}"),
        &format!("CREATE SCHEMA s; CREATE TABLE s.m (a integer NOT NULL, b text); {m}"),
        &[],
    );
    let mut w = Snail::open_writing(&path).unwrap();
    changed(&mut w, "BEGIN");
    changed(&mut w, "INSERT INTO s.m VALUES (1, 'x'), (2, 'y'), (2, 'y'), (3, 'z')");
    assert_eq!(
        rows(&mut w, "SELECT ctid::text IS NOT NULL AS c FROM s.m WHERE a = 2"),
        ["t", "t"]
    );
    assert_eq!(changed(&mut w, "UPDATE s.m SET b = 'q' WHERE a = 2"), 2);
    assert_eq!(
        rows(&mut w, "SELECT ctid::text IS NOT NULL AS c FROM s.m WHERE a = 2"),
        ["t", "t"]
    );
    assert_eq!(rows(&mut w, "SELECT count(DISTINCT ctid) FROM s.m"), ["4"]);
    assert_eq!(
        rows(
            &mut w,
            "SELECT a, b FROM s.m WHERE ctid = (SELECT ctid FROM s.m WHERE a = 2 LIMIT 1)"
        ),
        ["2|q"]
    );
    changed(&mut w, "DECLARE e CURSOR FOR SELECT a, b FROM s.m WHERE a = 1");
    assert_eq!(rows(&mut w, "FETCH 1 FROM e"), ["1|x"]);
    assert_eq!(changed(&mut w, "UPDATE s.m SET b = 'w' WHERE CURRENT OF e"), 1);
    changed(&mut w, "DECLARE f CURSOR FOR SELECT a, b FROM s.m WHERE a = 2");
    assert_eq!(rows(&mut w, "FETCH 1 FROM f"), ["2|q"]);
    assert_eq!(changed(&mut w, "DELETE FROM s.m WHERE CURRENT OF f"), 1);
    let all = "SELECT a, b FROM s.m ORDER BY a, b";
    assert_eq!(rows(&mut w, all), ["1|w", "2|q", "3|z"]);
    changed(&mut w, "COMMIT");
    assert_eq!(rows(&mut w, all), ["1|w", "2|q", "3|z"]);
}

fn a_staged_ctid(form: &str, ddl: &str) {
    let path = declared(
        &format!("staged-ctid-{form}"),
        ddl,
        &[
            "INSERT INTO s.k SELECT g / 1000, 'r' FROM generate_series(1, 5000) g",
            "INSERT INTO s.k VALUES (2, 'y'), (2, 'y')",
        ],
    );
    let mut w = Snail::open_writing(&path).unwrap();
    changed(&mut w, "BEGIN");
    assert_eq!(
        changed(&mut w, "UPDATE s.k SET b = 'q' WHERE a = 2 AND b = 'y'"),
        2
    );
    assert_eq!(
        rows(
            &mut w,
            "SELECT ctid::text IS NOT NULL AS c FROM s.k WHERE a = 2 AND b = 'q'"
        ),
        ["t", "t"]
    );
    assert_eq!(
        rows(&mut w, "SELECT count(DISTINCT ctid), count(*) FROM s.k"),
        ["5002|5002"]
    );
    assert_eq!(
        rows(
            &mut w,
            "SELECT a, b FROM s.k WHERE ctid = (SELECT ctid FROM s.k WHERE a = 2 AND b = 'q' LIMIT 1)"
        ),
        ["2|q"]
    );
    changed(
        &mut w,
        "DECLARE e CURSOR FOR SELECT a, b FROM s.k WHERE a = 2 AND b = 'q'",
    );
    assert_eq!(rows(&mut w, "FETCH 1 FROM e"), ["2|q"]);
    assert_eq!(
        changed(&mut w, "UPDATE s.k SET b = 'w' WHERE CURRENT OF e"),
        1
    );
    changed(
        &mut w,
        "DECLARE f CURSOR FOR SELECT a, b FROM s.k WHERE a = 2 AND b = 'q'",
    );
    assert_eq!(rows(&mut w, "FETCH 1 FROM f"), ["2|q"]);
    assert_eq!(changed(&mut w, "DELETE FROM s.k WHERE CURRENT OF f"), 1);
    let grouped = "SELECT a, b, count(*) FROM s.k WHERE a = 2 GROUP BY a, b ORDER BY a, b";
    assert_eq!(rows(&mut w, grouped), ["2|r|1000", "2|w|1"]);
    changed(&mut w, "COMMIT");
    assert_eq!(rows(&mut w, grouped), ["2|r|1000", "2|w|1"]);
    assert_eq!(rows(&mut w, "SELECT count(*) FROM s.k"), ["5001"]);
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
    for (form, ddl) in FORMS {
        deleted_since_refuses(form, ddl);
    }
}

fn deleted_since_refuses(form: &str, ddl: &str) {
    let path = declared(
        &format!("rr-{form}"),
        ddl,
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
            "{form} {write}"
        );
        changed(&mut a, "ROLLBACK");
        assert_eq!(
            rows(&mut a, "SELECT a, b FROM s.k ORDER BY a, b"),
            left,
            "{form} {write}"
        );
    }
}

#[test]
fn a_statement_run_again_changes_no_more_rows_alike_than_it_found() {
    for (form, ddl) in FORMS {
        run_again_changes_no_more(form, ddl);
    }
}

fn run_again_changes_no_more(form: &str, ddl: &str) {
    let path = declared(
        &format!("rerun-{form}"),
        ddl,
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

/// A table whose DDL names no key lies along every column it stores whose type has a B-tree, in the
/// columns' order, ascending, NULLs last, rows alike sharing a place, and `write` says so. On
/// `exec` an empty one takes an index and CLUSTER ON, in a transaction too; one holding rows keeps
/// its order, as any table holding rows keeps its orders.
#[test]
fn a_keyless_table_is_ordered_on_b_tree_columns() {
    let ddl = "CREATE SCHEMA s; CREATE TABLE s.nk (a integer, p point, b text);
               CREATE TABLE s.q (id serial, b text);";
    let words: Vec<Option<String>> = Catalog::parse(ddl)
        .unwrap()
        .tables
        .iter()
        .map(|t| t.laid_out_on_arrival())
        .collect();
    assert_eq!(
        words,
        [
            Some(
                "s.nk: laid out along every column it orders, since the DDL names no key: equal \
                 rows share a place"
                    .to_string()
            ),
            Some(
                "s.q: laid out along every column it orders, on id first, a key handed out as rows \
                 arrive, since the DDL names no key: its blocks follow the rows' arrival, and each \
                 insert meets the newest"
                    .to_string()
            )
        ]
    );
    let path = declared(
        "ordered",
        ddl,
        &[
            "INSERT INTO s.nk VALUES (2, '(0,0)', 'x'), (1, '(1,1)', 'y'), (NULL, NULL, 'a'), \
           (1, '(2,2)', NULL), (1, '(3,3)', 'y')",
        ],
    );
    let mut w = Snail::open_writing(&path).unwrap();
    // The snail's own order, which no ORDER BY asks for: a, then b, NULLs last.
    assert_eq!(
        rows(&mut w, "SELECT a, b FROM s.nk"),
        ["1|y", "1|y", "1|NULL", "2|x", "NULL|a"]
    );
    let plan = rows(&mut w, "EXPLAIN ANALYZE SELECT a FROM s.nk WHERE a = 1");
    assert!(
        plan[0].starts_with("Snail round along (every column it orders): 3 rows"),
        "{plan:?}"
    );
    // A row moved to another place by its level, and one by a column of no B-tree in place.
    assert_eq!(changed(&mut w, "UPDATE s.nk SET a = 3 WHERE b = 'x'"), 1);
    assert_eq!(
        changed(
            &mut w,
            "UPDATE s.nk SET p = '(9,9)' WHERE a = 1 AND b = 'y'"
        ),
        2
    );
    assert_eq!(
        rows(
            &mut w,
            "SELECT a, p::text AS p, b FROM s.nk ORDER BY a NULLS FIRST, b NULLS FIRST, p::text"
        ),
        [
            "NULL|NULL|a",
            "1|(2,2)|NULL",
            "1|(9,9)|y",
            "1|(9,9)|y",
            "3|(0,0)|x"
        ]
    );
    // Holding rows, it keeps its order.
    let e = w.execute("CREATE INDEX nk_b ON s.nk (b)").unwrap_err();
    assert_eq!(
        e.to_string(),
        "s.nk: it holds rows, so its columns and orders stay as written"
    );
    // Empty, on `exec`: an index beside its order, then CLUSTER ON names another.
    changed(&mut w, "CREATE TABLE s.later (a integer, b text)");
    let names = |w: &Snail, t: &str| -> Vec<String> {
        let t = w.catalog.table(t).unwrap();
        t.orders.iter().map(|o| o.name.clone()).collect()
    };
    assert_eq!(names(&w, "s.later"), [GIVEN]);
    changed(&mut w, "CREATE INDEX later_a ON s.later (a)");
    assert_eq!(names(&w, "s.later"), [GIVEN, "later_a"]);
    changed(&mut w, "ALTER TABLE s.later CLUSTER ON later_a");
    assert_eq!(names(&w, "s.later"), ["later_a"]);
    assert_eq!(
        rows(
            &mut w,
            "SELECT i.indisclustered FROM pg_index i WHERE i.indrelid = 's.later'::regclass"
        ),
        ["t"]
    );
    // In one transaction, with rows after.
    for s in [
        "BEGIN",
        "CREATE TABLE s.t2 (a integer, b text)",
        "CREATE INDEX t2_a ON s.t2 (a)",
        "ALTER TABLE s.t2 CLUSTER ON t2_a",
        "INSERT INTO s.t2 VALUES (2, 'y'), (1, 'x'), (1, 'x')",
        "COMMIT",
    ] {
        changed(&mut w, s);
    }
    assert_eq!(names(&w, "s.t2"), ["t2_a"]);
    assert_eq!(
        rows(&mut w, "SELECT a, b FROM s.t2 ORDER BY a, b"),
        ["1|x", "1|x", "2|y"]
    );
}

/// The order a table with no key is given is no index: 18.6's catalog holds no pg_class, pg_index
/// or pg_indexes row for it, and `relhasindex` is the table's other indexes'.
#[test]
fn the_order_a_keyless_table_is_given_is_no_index() {
    let path = declared(
        "catalog",
        "CREATE SCHEMA s;
         CREATE TABLE s.nk (a integer, b text);
         CREATE TABLE s.uq (x integer UNIQUE, y text);
         CREATE TABLE s.ix (a integer NOT NULL, b text, c integer);
         CREATE INDEX ix_b ON s.ix (b);",
        &[],
    );
    let mut w = Snail::open_writing(&path).unwrap();
    for (sql, want) in [
        (
            "SELECT c.relname, c.relhasindex, c.relkind FROM pg_class c JOIN pg_namespace n \
             ON n.oid = c.relnamespace WHERE n.nspname = 's' ORDER BY 1",
            &["ix|t|r", "ix_b|f|i", "nk|f|r", "uq|t|r", "uq_x_key|f|i"][..],
        ),
        (
            "SELECT i.indexrelid::regclass::text, i.indrelid::regclass::text, i.indisunique, \
             i.indisclustered FROM pg_index i JOIN pg_class c ON c.oid = i.indrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = 's' ORDER BY 1",
            &["s.ix_b|s.ix|f|f", "s.uq_x_key|s.uq|t|f"][..],
        ),
        (
            "SELECT tablename, indexname FROM pg_indexes WHERE schemaname = 's' ORDER BY 1, 2",
            &["ix|ix_b", "uq|uq_x_key"][..],
        ),
        (
            "SELECT attrelid::regclass::text, attname, attnum FROM pg_attribute \
             WHERE attrelid IN ('s.nk'::regclass) AND attnum > 0 ORDER BY 1, 3",
            &["s.nk|a|1", "s.nk|b|2"][..],
        ),
    ] {
        assert_eq!(rows(&mut w, sql), want, "{sql}");
    }
    // Nor a fact of an index, nor a relation's name: two snails each holding a table with no key
    // in one schema are opened together.
    assert!(w
        .catalog
        .facts()
        .iter()
        .filter(|f| f.kind == "index")
        .all(|f| f.object == "s.uq.uq_x_key" || f.object == "s.ix.ix_b"));
    let other = declared(
        "catalog-other",
        "CREATE SCHEMA s; CREATE TABLE s.other (a integer, b text);",
        &["INSERT INTO s.other VALUES (1, 'o')"],
    );
    changed(&mut w, "INSERT INTO s.nk VALUES (1, 'n')");
    drop(w);
    let mut both = warren_snail::read::Snails::open(&[path.as_path(), other.as_path()]).unwrap();
    let (b, _) = both
        .ask("SELECT n.b, o.b FROM s.nk n JOIN s.other o ON o.a = n.a")
        .unwrap();
    assert_eq!(b.num_rows(), 1);
}

/// The DDL of the tables with no column of a B-tree, or none at all, that the tests below write.
const NOB: &str = "CREATE SCHEMA s;
     CREATE TABLE s.nob (p point, j json);
     CREATE TABLE s.none ();
     CREATE TABLE s.geo (p point, x xid, j json);
     CREATE INDEX geo_p ON s.geo USING gist (p);
     CREATE TABLE s.nn (p point NOT NULL, j json);
     CREATE TABLE s.w (j json);
     CREATE TABLE s.v (p point, n integer GENERATED ALWAYS AS (2));";

/// A table with no column of a B-tree lies as its rows arrive, along a number of its own: each row
/// a write lands is numbered past the table's last, in the statement's order, and `write` numbers
/// the rows it is given from 1; an UPDATE keeps a row where it lies. Every answer is 18.6's, rows
/// asked for in no order aside, which lie in the snail's.
#[test]
fn a_table_with_no_b_tree_column_lies_in_arrival() {
    let c = Catalog::parse(NOB).unwrap();
    let t = c.table("s.nob").unwrap();
    assert_eq!(t.own_columns().len(), 2);
    assert_eq!(t.arrival(), Some(2));
    let o = &t.orders[0];
    assert!(o.given() && !o.unique && t.orders.len() == 1);
    assert_eq!(o.levels, [warren_snail::catalog::Level::Column(2)]);
    assert_eq!(o.opclasses, ["int8_ops"]);
    assert_eq!(c.table("s.none").unwrap().arrival(), Some(0));
    // A GiST strand ends in the number, as a strand ends in its table's key.
    let g = c.table("s.geo").unwrap();
    assert!(g.orders[1].columns.contains(&3));
    assert_eq!(
        t.laid_out_on_arrival().as_deref(),
        Some(
            "s.nob: laid out as its rows arrive, since the DDL names no key and no column of it \
             has a B-tree: its blocks follow the rows' arrival, and each insert meets the newest"
        )
    );
    // `write` numbers the rows it is given in their order.
    let path = tmp("nokey-arrival.snail");
    let given = Rows {
        columns: vec![Values::Text(
            ["[3]", "[1]", "[2]"].map(|j| Some(j.to_string())).to_vec(),
        )],
    };
    write(&path, &c, &[("s.w".into(), given)]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    assert_eq!(
        rows(&mut w, "SELECT j::text AS j FROM s.w"),
        ["[3]", "[1]", "[2]"]
    );
    // Each statement's rows past the last, in its order; an UPDATE keeps its row's number. (Rows
    // a block holds past its sorted ones are read after them until a writer prunes it, so the
    // number is read by its name, as the snail's own finders read it.)
    changed(&mut w, "INSERT INTO s.w VALUES ('[0]'), ('[9]')");
    changed(&mut w, "INSERT INTO s.w VALUES ('[5]')");
    let four = "UPDATE s.w SET j = '[4]' WHERE ctid = \
                (SELECT ctid FROM s.w WHERE j::text = '[1]' LIMIT 1)";
    assert_eq!(changed(&mut w, four), 1);
    let one = "DELETE FROM s.w WHERE ctid = (SELECT ctid FROM s.w WHERE j::text = '[3]' LIMIT 1)";
    assert_eq!(changed(&mut w, one), 1);
    changed(&mut w, "INSERT INTO s.w VALUES ('[6]')");
    let numbered = format!(
        "SELECT j::text AS j, \"{}\" AS n FROM s.w ORDER BY 2",
        warren_snail::catalog::ARRIVAL
    );
    assert_eq!(
        rows(&mut w, &numbered),
        ["[4]|2", "[2]|3", "[0]|4", "[9]|5", "[5]|6", "[6]|7"]
    );
    assert_eq!(
        rows(&mut w, "SELECT j::text AS j FROM s.w ORDER BY 1"),
        ["[0]", "[2]", "[4]", "[5]", "[6]", "[9]"]
    );
    // 18.6's answers (`tmp/bf13/nk2/arr.sql`).
    changed(
        &mut w,
        "INSERT INTO s.nob VALUES ('(1,2)', '{\"a\": 1}'), ('(1,2)', '{\"a\": 1}'), (NULL, NULL)",
    );
    assert_eq!(
        rows(&mut w, "SELECT count(*), count(p) FROM s.nob"),
        ["3|2"]
    );
    let one = "DELETE FROM s.nob WHERE ctid = (SELECT ctid FROM s.nob WHERE p IS NULL LIMIT 1)";
    assert_eq!(changed(&mut w, one), 1);
    changed(
        &mut w,
        "INSERT INTO s.geo VALUES ('(0,0)', '5', '{\"a\":1}'), ('(5,5)', '7', '{\"a\":2}'), \
         ('(1,1)', NULL, '[]'), ('(1,1)', NULL, '[]')",
    );
    assert_eq!(
        rows(
            &mut w,
            "SELECT p::text AS p, x::text AS x FROM s.geo ORDER BY p <-> point '(5,5)' LIMIT 1"
        ),
        ["(5,5)|7"]
    );
    assert_eq!(
        rows(
            &mut w,
            "SELECT p::text AS p FROM s.geo WHERE p <@ box '((-1,-1),(1,1))' ORDER BY 1"
        ),
        ["(0,0)", "(1,1)", "(1,1)"]
    );
    assert_eq!(
        changed(
            &mut w,
            "UPDATE s.geo SET j = '{\"b\":3}' WHERE p ~= point '(5,5)'"
        ),
        1
    );
    assert_eq!(
        changed(&mut w, "DELETE FROM s.geo WHERE p ~= point '(1,1)'"),
        2
    );
    assert_eq!(
        rows(
            &mut w,
            "SELECT p::text AS p, x::text AS x, j::text AS j FROM s.geo ORDER BY p::text"
        ),
        ["(0,0)|5|{\"a\":1}", "(5,5)|7|{\"b\":3}"]
    );
}

/// The number a table with no column of a B-tree lies along is no column of it: no star, row,
/// COPY, INSERT's list of columns, alias's list or catalog row holds it, nor NATURAL meets it. A
/// table of no column at all takes rows of none. Every answer and refusal is 18.6's
/// (`tmp/bf13/nk2/arr.sql`).
#[test]
fn the_arrival_number_is_no_column_of_the_table() {
    let path = declared(
        "arrival-no-column",
        NOB,
        &["INSERT INTO s.nob VALUES ('(1,2)', '{\"a\": 1}'), ('(1,2)', '{\"a\": 1}'), (NULL, NULL)"],
    );
    let mut w = Snail::open_writing(&path).unwrap();
    assert_eq!(
        rows(
            &mut w,
            "INSERT INTO s.nob VALUES ('(3,4)', '[2]') RETURNING *"
        ),
        ["(3,4)|[2]"]
    );
    let done = w
        .execute("SELECT * FROM s.nob ORDER BY p::text NULLS FIRST, j::text")
        .unwrap();
    assert_eq!(done.answer.unwrap().num_columns(), 2);
    assert_eq!(
        rows(&mut w, "SELECT n FROM s.nob n ORDER BY n::text"),
        [
            "(\"(1,2)\",\"{\"\"a\"\": 1}\")",
            "(\"(1,2)\",\"{\"\"a\"\": 1}\")",
            "(\"(3,4)\",[2])",
            "(,)"
        ]
    );
    assert_eq!(
        rows(
            &mut w,
            "SELECT row_to_json(n)::text FROM s.nob n ORDER BY 1"
        ),
        [
            "{\"p\":\"(1,2)\",\"j\":{\"a\": 1}}",
            "{\"p\":\"(1,2)\",\"j\":{\"a\": 1}}",
            "{\"p\":\"(3,4)\",\"j\":[2]}",
            "{\"p\":null,\"j\":null}"
        ]
    );
    for (sql, says) in [
        (
            "SELECT * FROM s.nob AS x(a, b, c)",
            "table \"x\" has 2 columns available but 3 columns specified",
        ),
        (
            "INSERT INTO s.nob VALUES ('(0,0)', '{}', 1)",
            "INSERT has more expressions than target columns",
        ),
        (
            "INSERT INTO s.nn VALUES (NULL, '{}')",
            "null value in column \"p\" of relation \"nn\" violates not-null constraint",
        ),
    ] {
        let e = w.execute(sql).unwrap_err().to_string();
        assert!(e.starts_with(says), "{sql}: {e}");
    }
    let e = w
        .execute("INSERT INTO s.nn VALUES (NULL, '{}')")
        .unwrap_err();
    assert!(
        e.to_string().contains("Failing row contains (null, {})."),
        "{e}"
    );
    assert_eq!(
        changed(
            &mut w,
            "INSERT INTO s.nob SELECT p, j FROM s.nob WHERE p IS NULL"
        ),
        1
    );
    let copied = w
        .execute("COPY (SELECT * FROM s.nob ORDER BY p::text NULLS FIRST, j::text) TO STDOUT")
        .unwrap();
    assert_eq!(
        String::from_utf8(copied.copied.unwrap()).unwrap(),
        "\\N\t\\N\n\\N\t\\N\n(1,2)\t{\"a\": 1}\n(1,2)\t{\"a\": 1}\n(3,4)\t[2]\n"
    );
    for (sql, want) in [
        (
            "SELECT c.relname, c.relnatts, c.relhasindex FROM pg_class c JOIN pg_namespace n \
             ON n.oid = c.relnamespace WHERE n.nspname = 's' AND c.relkind = 'r' \
             AND c.relname <> 'w' ORDER BY 1",
            &["geo|3|t", "nn|2|f", "nob|2|f", "none|0|f", "v|2|f"][..],
        ),
        (
            "SELECT attrelid::regclass::text, attname, attnum FROM pg_attribute \
             WHERE attrelid IN ('s.nob'::regclass, 's.none'::regclass, 's.geo'::regclass) \
             AND attnum > 0 ORDER BY 1, 3",
            &[
                "s.geo|p|1",
                "s.geo|x|2",
                "s.geo|j|3",
                "s.nob|p|1",
                "s.nob|j|2",
            ][..],
        ),
        (
            "SELECT count(*) FROM s.nob a NATURAL JOIN s.none b",
            &["0"][..],
        ),
    ] {
        assert_eq!(rows(&mut w, sql), want, "{sql}");
    }
    // A table of no column: rows of none.
    changed(&mut w, "INSERT INTO s.none DEFAULT VALUES");
    changed(&mut w, "INSERT INTO s.none DEFAULT VALUES");
    assert_eq!(rows(&mut w, "SELECT count(*) FROM s.none"), ["2"]);
    assert_eq!(
        rows(&mut w, "SELECT count(*) FROM s.nob NATURAL JOIN s.none"),
        ["10"]
    );
    let none = w.execute("SELECT * FROM s.none").unwrap().answer.unwrap();
    assert_eq!((none.num_rows(), none.num_columns()), (2, 0));
    let one = "DELETE FROM s.none WHERE ctid = (SELECT ctid FROM s.none LIMIT 1)";
    assert_eq!(changed(&mut w, one), 1);
    assert_eq!(rows(&mut w, "SELECT count(*) FROM s.none"), ["1"]);
    // COPY's columns are the table's; its rows lie in the snail's order, not the heap's.
    let copied = w
        .execute_copy("COPY s.nob FROM STDIN", b"(5,5)\t{\"c\": 3}\n\\N\t[7]\n")
        .unwrap();
    assert_eq!(copied.rows, 2);
    let out = w.execute("COPY s.nob TO STDOUT").unwrap().copied.unwrap();
    let mut lines: Vec<&str> = std::str::from_utf8(&out).unwrap().lines().collect();
    lines.sort_unstable();
    let mut theirs = [
        "(1,2)\t{\"a\": 1}",
        "(1,2)\t{\"a\": 1}",
        "\\N\t\\N",
        "(3,4)\t[2]",
        "\\N\t\\N",
        "(5,5)\t{\"c\": 3}",
        "\\N\t[7]",
    ];
    theirs.sort_unstable();
    assert_eq!(lines, theirs);
    // A virtual column beside it, and the catalog's facts of the columns.
    changed(&mut w, "INSERT INTO s.v (p) VALUES ('(1,1)')");
    assert_eq!(rows(&mut w, "SELECT * FROM s.v"), ["(1,1)|2"]);
    let columns: Vec<String> = w
        .catalog
        .facts()
        .into_iter()
        .filter(|f| f.kind == "table" && f.object == "s.nob")
        .map(|f| format!("{}={}", f.fact, f.value))
        .collect();
    assert_eq!(columns, ["columns=2"]);
}
