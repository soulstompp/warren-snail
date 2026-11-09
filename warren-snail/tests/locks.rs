//! The writes A8 reads, and the locks on rows: an UPDATE setting columns to expressions of the
//! row's old values, an INSERT from a question over a join, a question among a writer's statements
//! reading the transaction's own rows, the locking clauses taken as PostgreSQL's READ COMMITTED
//! takes them between two sessions, and REPEATABLE READ holding the transaction's first snapshot.
#![cfg(feature = "standalone")]

mod weather;

use arrow::array::{Array, RecordBatch};
use std::sync::mpsc;
use std::time::Duration;
use warren_snail::catalog::Catalog;
use warren_snail::read::{Done, Snail};
use weather::{tmp, write};

const DDL: &str = "CREATE SCHEMA a; 
CREATE TABLE a.inv (id integer NOT NULL, qty integer, price numeric(10,2), note text,
    total numeric(12,2) GENERATED ALWAYS AS (qty * price) STORED, PRIMARY KEY (id));
CREATE TABLE a.t (id integer NOT NULL, g integer, name text, PRIMARY KEY (id));
CREATE TABLE a.u (id integer NOT NULL, t_id integer, w integer, PRIMARY KEY (id));
CREATE TABLE a.dst (id integer NOT NULL, w integer, s text, PRIMARY KEY (id));
CREATE TABLE a.q (id integer NOT NULL, done boolean NOT NULL, PRIMARY KEY (id));
";

const ROWS: [&str; 5] = [
    "INSERT INTO a.inv (id, qty, price, note) VALUES (1, 10, 2.50, 'a'), (2, 20, 3.75, 'b'), (3, 30, 1.00, 'c')",
    "INSERT INTO a.t (id, g, name) VALUES (1, 1, 'one'), (2, 2, 'two'), (3, 5, 'three')",
    "INSERT INTO a.u (id, t_id, w) VALUES (10, 1, 100), (11, 1, 110), (12, 2, 120), (13, 3, 130)",
    "INSERT INTO a.q (id, done) VALUES (1, false), (2, false), (3, false), (4, false)",
    "INSERT INTO a.dst (id, w, s) VALUES (1, 1, 'x')",
];

fn made(name: &str) -> std::path::PathBuf {
    let path = tmp(&format!("locks-{name}.snail"));
    write(&path, &Catalog::parse(DDL).unwrap(), &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    for st in ROWS {
        w.execute(st).unwrap_or_else(|e| panic!("{st}: {e}"));
    }
    path
}

fn run(w: &mut Snail, sql: &str) -> Done {
    w.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn refused(w: &mut Snail, sql: &str) -> String {
    w.execute(sql)
        .err()
        .unwrap_or_else(|| panic!("{sql} was taken"))
        .to_string()
}

/// An answer's rows, each cell as arrow writes it, NULL as `NULL`, in its order.
fn shown(b: &RecordBatch) -> Vec<String> {
    (0..b.num_rows())
        .map(|r| {
            b.columns()
                .iter()
                .map(|a| match a.is_null(r) {
                    true => "NULL".to_string(),
                    false => arrow::util::display::array_value_to_string(a, r).unwrap(),
                })
                .collect::<Vec<_>>()
                .join(",")
        })
        .collect()
}

fn rows(s: &mut Snail, sql: &str) -> Vec<String> {
    shown(&s.ask(sql, None).unwrap_or_else(|e| panic!("{sql}: {e}")).0)
}

/// A writer's statements run on a thread of its own, their results sent back as they end.
fn later(path: &std::path::Path, statements: Vec<String>) -> mpsc::Receiver<Result<Done, String>> {
    let (tx, rx) = mpsc::channel();
    let p = path.to_path_buf();
    std::thread::spawn(move || {
        let mut w = Snail::open_writing(&p).unwrap();
        for st in &statements {
            if tx.send(w.execute(st).map_err(|e| e.to_string())).is_err() {
                return;
            }
        }
        // Held open until the test lets it go, so that its locks stand until then.
        let _ = tx.send(Err("held".into()));
        std::thread::sleep(Duration::from_millis(50));
    });
    rx
}

fn waits(rx: &mpsc::Receiver<Result<Done, String>>) -> bool {
    std::thread::sleep(Duration::from_millis(300));
    rx.try_recv().is_err()
}

fn next(rx: &mpsc::Receiver<Result<Done, String>>) -> Result<Done, String> {
    rx.recv_timeout(Duration::from_secs(10))
        .expect("a statement ended")
}

#[test]
fn an_update_sets_each_column_to_an_expression_of_the_row_s_old_values() {
    let path = made("set");
    let mut w = Snail::open_writing(&path).unwrap();
    // Every expression reads the row as it was: the note reads qty before it is set.
    let d = w
        .execute_with(
            "UPDATE a.inv SET qty = qty - $1, price = price * 2, note = note || '-' || qty WHERE id = $2",
            &["3".into(), "1".into()],
        )
        .unwrap();
    assert_eq!(d.rows, 1);
    // Two columns given each other's values; a DEFAULT beside an expression.
    run(
        &mut w,
        "UPDATE a.inv SET qty = id, id = qty + 100, note = DEFAULT WHERE id = 2",
    );
    run(
        &mut w,
        "UPDATE a.inv SET price = price + 0.005 WHERE id = 3",
    );
    assert_eq!(
        rows(
            &mut w,
            "SELECT i.id, i.qty, i.price, i.note, i.total FROM a.inv i ORDER BY i.id"
        ),
        [
            "1,7,{value: 5.00, nan: false},a-10,{value: 35.00, nan: false}",
            "3,30,{value: 1.01, nan: false},c,{value: 30.30, nan: false}",
            "120,2,{value: 3.75, nan: false},NULL,{value: 7.50, nan: false}",
        ]
    );
    // Refused as PostgreSQL refuses them: a type no assignment casts, before any row is read; a
    // value out of the column's range; a column set twice; a generated column.
    for (sql, why) in [
        (
            "UPDATE a.inv SET qty = note WHERE id = 1",
            "column \"qty\" is of type integer but expression is of type text",
        ),
        (
            "UPDATE a.inv SET qty = note WHERE id = 99",
            "column \"qty\" is of type integer but expression is of type text",
        ),
        (
            "UPDATE a.inv SET qty = qty * 1000000000 WHERE id = 1",
            "integer out of range",
        ),
        (
            "UPDATE a.inv SET qty = 1, qty = qty WHERE id = 1",
            "multiple assignments to same column \"qty\"",
        ),
        (
            "UPDATE a.inv SET total = qty WHERE id = 1",
            "column \"total\" can only be updated to DEFAULT",
        ),
    ] {
        let e = refused(&mut w, sql);
        assert!(e.contains(why), "{sql}: {e}");
    }
    assert_eq!(
        rows(&mut w, "SELECT i.qty FROM a.inv i WHERE i.id = 1"),
        ["7"]
    );
}

#[test]
fn an_insert_takes_expressions_over_a_join_and_its_parameters() {
    let path = made("insert");
    let mut w = Snail::open_writing(&path).unwrap();
    let d = w
        .execute_with(
            "INSERT INTO a.dst (id, w, s) SELECT u.id + 100, u.w * $1, t.name || '-' || u.w \
             FROM a.t t JOIN a.u u ON u.t_id = t.id WHERE t.g < $2",
            &["2".into(), "3".into()],
        )
        .unwrap();
    assert_eq!(d.rows, 3);
    assert_eq!(
        rows(&mut w, "SELECT d.id, d.w, d.s FROM a.dst d ORDER BY d.id"),
        [
            "1,1,x",
            "110,200,one-100",
            "111,220,one-110",
            "112,240,two-120"
        ]
    );
    let e = refused(
        &mut w,
        "INSERT INTO a.dst (id, w) SELECT u.id, t.name FROM a.t t JOIN a.u u ON u.t_id = t.id",
    );
    assert!(
        e.contains("column \"w\" is of type integer but expression is of type text"),
        "{e}"
    );
    let e = w
        .execute_with("INSERT INTO a.dst (id) VALUES ($2)", &["5".into()])
        .unwrap_err();
    assert_eq!(e.to_string(), "there is no parameter $2");
}

#[test]
fn a_question_among_a_writer_s_statements_reads_the_transaction_s_own_rows() {
    let path = made("question");
    let mut w = Snail::open_writing(&path).unwrap();
    let mut other = Snail::open_writing(&path).unwrap();
    run(&mut w, "BEGIN");
    run(
        &mut w,
        "INSERT INTO a.t (id, g, name) VALUES (4, 4, 'four')",
    );
    let d = run(
        &mut w,
        "SELECT t.id, t.name FROM a.t t WHERE t.g >= 4 ORDER BY t.id",
    );
    assert_eq!((d.statement, d.rows), ("SELECT", 2));
    assert_eq!(shown(d.answer.as_ref().unwrap()), ["3,three", "4,four"]);
    let d = other
        .execute("SELECT t.id FROM a.t t WHERE t.g >= 4")
        .unwrap();
    assert_eq!(d.rows, 1, "another writer reads what is committed");
    let d = w
        .execute_with("SELECT t.name FROM a.t t WHERE t.id = $1", &["4".into()])
        .unwrap();
    assert_eq!(shown(d.answer.as_ref().unwrap()), ["four"]);
    // A question that fails fails the transaction, as PostgreSQL's does.
    assert!(w.execute("SELECT t.nothing FROM a.t t").is_err());
    assert!(refused(&mut w, "SELECT t.id FROM a.t t").contains("current transaction is aborted"));
    run(&mut w, "ROLLBACK");
    assert_eq!(run(&mut w, "SELECT t.id FROM a.t t").rows, 3);
}

#[test]
fn a_row_locked_for_update_holds_a_writer_of_it_to_the_transaction_s_end_and_no_other() {
    let path = made("for-update");
    let mut a = Snail::open_writing(&path).unwrap();
    run(&mut a, "BEGIN");
    let d = run(
        &mut a,
        "SELECT i.id, i.qty FROM a.inv i WHERE i.id = 1 FOR UPDATE",
    );
    assert_eq!(shown(d.answer.as_ref().unwrap()), ["1,10"]);
    // A writer of the row waits; a writer of another row of the same block does not.
    let of_it = later(
        &path,
        vec!["UPDATE a.inv SET qty = qty + 1 WHERE id = 1".into()],
    );
    let beside = later(
        &path,
        vec!["UPDATE a.inv SET qty = qty + 1 WHERE id = 2".into()],
    );
    assert_eq!(next(&beside).unwrap().rows, 1);
    assert!(waits(&of_it), "a writer of the locked row did not wait");
    run(&mut a, "UPDATE a.inv SET qty = qty * 2 WHERE id = 1");
    run(&mut a, "COMMIT");
    // It waited, then read the row in its newest version.
    assert_eq!(next(&of_it).unwrap().rows, 1);
    let mut r = Snail::open(&path).unwrap();
    assert_eq!(
        rows(&mut r, "SELECT i.id, i.qty FROM a.inv i ORDER BY i.id"),
        ["1,21", "2,21", "3,30"]
    );
    // A statement alone locks its rows and lets them go as it ends.
    run(&mut a, "SELECT i.id FROM a.inv i WHERE i.id = 3 FOR UPDATE");
    let mut b = Snail::open_writing(&path).unwrap();
    assert_eq!(
        run(
            &mut b,
            "SELECT i.id FROM a.inv i WHERE i.id = 3 FOR UPDATE NOWAIT"
        )
        .rows,
        1
    );
}

#[test]
fn nowait_fails_in_postgresql_s_words_and_skip_locked_skips_only_the_rows_held() {
    let path = made("nowait");
    let mut a = Snail::open_writing(&path).unwrap();
    let mut b = Snail::open_writing(&path).unwrap();
    // A queue taken from by two workers: each takes the first row no other holds.
    let take =
        "SELECT q.id FROM a.q q WHERE q.done = false ORDER BY q.id LIMIT 1 FOR UPDATE SKIP LOCKED";
    run(&mut a, "BEGIN");
    assert_eq!(shown(run(&mut a, take).answer.as_ref().unwrap()), ["1"]);
    run(&mut a, "UPDATE a.q SET done = true WHERE id = 1");
    run(&mut b, "BEGIN");
    assert_eq!(shown(run(&mut b, take).answer.as_ref().unwrap()), ["2"]);
    let e = refused(
        &mut b,
        "SELECT q.id FROM a.q q WHERE q.id = 1 FOR UPDATE NOWAIT",
    );
    assert_eq!(e, "could not obtain lock on row in relation \"q\"");
    assert!(refused(&mut b, "SELECT q.id FROM a.q q").contains("current transaction is aborted"));
    run(&mut b, "ROLLBACK");
    // Skipped rows count toward no LIMIT; rows past it are not locked.
    let d = run(
        &mut b,
        "SELECT q.id FROM a.q q ORDER BY q.id LIMIT 2 FOR UPDATE SKIP LOCKED",
    );
    assert_eq!(shown(d.answer.as_ref().unwrap()), ["2", "3"]);
    run(&mut a, "COMMIT");
    let mut r = Snail::open(&path).unwrap();
    assert_eq!(
        rows(&mut r, "SELECT q.id FROM a.q q WHERE q.done = true"),
        ["1"]
    );
}

#[test]
fn each_strength_meets_another_as_postgresql_s_row_locks_meet() {
    let path = made("strengths");
    let mut a = Snail::open_writing(&path).unwrap();
    let mut b = Snail::open_writing(&path).unwrap();
    let held = |w: &mut Snail, s: &str| -> bool {
        w.execute(&format!("SELECT t.id FROM a.t t WHERE t.id = 1 {s} NOWAIT"))
            .is_err()
    };
    // PostgreSQL's table of conflicting row-level locks.
    let strengths = [
        "FOR KEY SHARE",
        "FOR SHARE",
        "FOR NO KEY UPDATE",
        "FOR UPDATE",
    ];
    let conflicts = [
        [false, false, false, true],
        [false, false, true, true],
        [false, true, true, true],
        [true, true, true, true],
    ];
    for (i, first) in strengths.iter().enumerate() {
        for (j, second) in strengths.iter().enumerate() {
            run(&mut a, "BEGIN");
            run(
                &mut a,
                &format!("SELECT t.id FROM a.t t WHERE t.id = 1 {first}"),
            );
            assert_eq!(
                held(&mut b, second),
                conflicts[i][j],
                "{first} then {second}"
            );
            run(&mut a, "COMMIT");
        }
    }
    // A writer takes NO KEY UPDATE where it leaves the key alone, UPDATE where it changes it or
    // deletes the row.
    run(&mut a, "BEGIN");
    run(&mut a, "UPDATE a.t SET name = 'uno' WHERE id = 1");
    assert!(!held(&mut b, "FOR KEY SHARE"));
    assert!(held(&mut b, "FOR SHARE"));
    run(&mut a, "ROLLBACK");
    run(&mut a, "BEGIN");
    run(&mut a, "UPDATE a.t SET id = 9 WHERE id = 1");
    assert!(held(&mut b, "FOR KEY SHARE"));
    run(&mut a, "ROLLBACK");
    // A writer leaving the key alone does not wait for a KEY SHARE.
    run(&mut a, "BEGIN");
    run(
        &mut a,
        "SELECT t.id FROM a.t t WHERE t.id = 1 FOR KEY SHARE",
    );
    let w = later(
        &path,
        vec!["UPDATE a.t SET name = 'uno' WHERE id = 1".into()],
    );
    assert_eq!(next(&w).unwrap().rows, 1);
    let w = later(&path, vec!["DELETE FROM a.t WHERE id = 1".into()]);
    assert!(waits(&w), "a delete did not wait for a KEY SHARE");
    run(&mut a, "COMMIT");
    assert_eq!(next(&w).unwrap().rows, 1);
}

#[test]
fn writers_waiting_in_a_ring_of_row_locks_fail_one_with_a_deadlock() {
    let path = made("ring");
    let mut a = Snail::open_writing(&path).unwrap();
    run(&mut a, "BEGIN");
    run(&mut a, "SELECT t.id FROM a.t t WHERE t.id = 1 FOR UPDATE");
    let (tx, rx) = mpsc::channel();
    let p = path.clone();
    let (go, wait) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        let mut b = Snail::open_writing(&p).unwrap();
        b.execute("BEGIN").unwrap();
        b.execute("SELECT t.id FROM a.t t WHERE t.id = 2 FOR SHARE")
            .unwrap();
        tx.send(Ok(Done::default())).unwrap();
        tx.send(
            b.execute("SELECT t.id FROM a.t t WHERE t.id = 1 FOR SHARE")
                .map_err(|e| e.to_string()),
        )
        .unwrap();
        let _ = wait.recv_timeout(Duration::from_secs(10));
    });
    next(&rx).unwrap();
    assert!(waits(&rx), "the second did not wait");
    let e = refused(&mut a, "UPDATE a.t SET name = 'x' WHERE id = 2");
    assert!(e.starts_with("deadlock detected"), "{e}");
    run(&mut a, "ROLLBACK");
    assert_eq!(next(&rx).unwrap().rows, 1);
    go.send(()).unwrap();
}

#[test]
fn a_locking_clause_postgresql_refuses_is_refused_in_its_words() {
    let path = made("refused");
    let mut w = Snail::open_writing(&path).unwrap();
    for (sql, why) in [
        (
            "SELECT DISTINCT t.g FROM a.t t FOR UPDATE",
            "FOR UPDATE is not allowed with DISTINCT clause",
        ),
        (
            "SELECT t.g FROM a.t t GROUP BY t.g FOR SHARE",
            "FOR SHARE is not allowed with GROUP BY clause",
        ),
        (
            "SELECT count(*) FROM a.t t FOR KEY SHARE",
            "FOR KEY SHARE is not allowed with aggregate functions",
        ),
        (
            "SELECT t.id FROM a.t t LEFT JOIN a.u u ON u.t_id = t.id FOR NO KEY UPDATE",
            "FOR NO KEY UPDATE cannot be applied to the nullable side of an outer join",
        ),
        (
            "SELECT t.id FROM a.t t FOR UPDATE OF x",
            "relation \"x\" in FOR UPDATE clause not found in FROM clause",
        ),
        (
            "SELECT t.id FROM a.t t FOR UPDATE OF a.t",
            "FOR UPDATE must specify unqualified relation names",
        ),
    ] {
        let e = refused(&mut w, sql);
        assert_eq!(e, why, "{sql}");
    }
    // OF names the relations locked: the outer join's nullable side left unlocked.
    assert_eq!(
        run(&mut w, "SELECT t.id, u.w FROM a.t t LEFT JOIN a.u u ON u.t_id = t.id ORDER BY t.id, u.w FOR UPDATE OF t").rows,
        4
    );
    let mut r = Snail::open(&path).unwrap();
    let e = r
        .ask("SELECT t.id FROM a.t t FOR SHARE", None)
        .unwrap_err()
        .to_string();
    assert!(e.contains("open it to write"), "{e}");
}

#[test]
fn repeatable_read_reads_the_first_statement_s_snapshot_and_refuses_a_row_changed_since() {
    let path = made("repeatable");
    let mut a = Snail::open_writing(&path).unwrap();
    let mut b = Snail::open_writing(&path).unwrap();
    let count = "SELECT count(*) FROM a.t t";
    run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    // The snapshot is the first statement's, not the BEGIN's.
    run(
        &mut b,
        "INSERT INTO a.t (id, g, name) VALUES (4, 4, 'four')",
    );
    assert_eq!(rows(&mut a, count), ["4"]);
    run(
        &mut b,
        "INSERT INTO a.t (id, g, name) VALUES (5, 5, 'five')",
    );
    run(&mut b, "UPDATE a.t SET name = 'TWO' WHERE id = 2");
    assert_eq!(rows(&mut a, count), ["4"]);
    assert_eq!(
        rows(&mut a, "SELECT t.name FROM a.t t WHERE t.id = 2"),
        ["two"]
    );
    // A row locked that no other changed is locked, read in the snapshot.
    let d = run(
        &mut a,
        "SELECT t.id, t.name FROM a.t t WHERE t.id = 1 FOR UPDATE",
    );
    assert_eq!(shown(d.answer.as_ref().unwrap()), ["1,one"]);
    // A row another changed since is refused, and the transaction with it.
    let e = refused(&mut a, "UPDATE a.t SET g = 20 WHERE id = 2");
    assert_eq!(e, "could not serialize access due to concurrent update");
    assert!(refused(&mut a, count).contains("current transaction is aborted"));
    run(&mut a, "ROLLBACK");
    assert_eq!(rows(&mut a, count), ["5"]);
    // A transaction's own writes are read in its snapshot; another's after it are not.
    run(&mut a, "START TRANSACTION ISOLATION LEVEL REPEATABLE READ");
    run(&mut a, "UPDATE a.t SET g = 10 WHERE id = 1");
    let w = later(
        &path,
        vec!["INSERT INTO a.u (id, t_id, w) VALUES (14, 1, 140)".into()],
    );
    assert_eq!(next(&w).unwrap().rows, 1);
    assert_eq!(rows(&mut a, "SELECT t.g FROM a.t t WHERE t.id = 1"), ["10"]);
    assert_eq!(rows(&mut a, "SELECT count(*) FROM a.u u"), ["4"]);
    run(&mut a, "COMMIT");
    assert_eq!(rows(&mut a, "SELECT count(*) FROM a.u u"), ["5"]);
    // A row locked that another changed since is refused; so is a delete of a row another deleted.
    run(&mut a, "BEGIN");
    run(&mut a, "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(rows(&mut a, count), ["5"]);
    run(&mut b, "UPDATE a.t SET g = 3 WHERE id = 3");
    let e = refused(&mut a, "SELECT t.id FROM a.t t WHERE t.id = 3 FOR SHARE");
    assert_eq!(e, "could not serialize access due to concurrent update");
    run(&mut a, "ROLLBACK");
    run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(rows(&mut a, count), ["5"]);
    run(&mut b, "DELETE FROM a.t WHERE id = 5");
    let e = refused(&mut a, "DELETE FROM a.t WHERE id = 5");
    assert_eq!(e, "could not serialize access due to concurrent delete");
    run(&mut a, "ROLLBACK");
    // The session's level: each transaction after reads its first snapshot.
    run(
        &mut a,
        "SET SESSION CHARACTERISTICS AS TRANSACTION ISOLATION LEVEL REPEATABLE READ",
    );
    run(&mut a, "BEGIN");
    assert_eq!(rows(&mut a, count), ["4"]);
    run(&mut b, "INSERT INTO a.t (id, g, name) VALUES (6, 6, 'six')");
    assert_eq!(rows(&mut a, count), ["4"]);
    run(&mut a, "COMMIT");
    assert_eq!(rows(&mut a, count), ["5"]);
}

#[test]
fn repeatable_read_waits_for_a_writer_of_its_row_and_is_refused_once_that_writer_commits() {
    let path = made("repeatable-waits");
    let mut b = Snail::open_writing(&path).unwrap();
    run(&mut b, "BEGIN");
    run(&mut b, "UPDATE a.t SET name = 'one' WHERE id = 3");
    let (tx, rx) = mpsc::channel();
    let p = path.clone();
    std::thread::spawn(move || {
        let mut a = Snail::open_writing(&p).unwrap();
        a.execute("BEGIN ISOLATION LEVEL REPEATABLE READ").unwrap();
        a.execute("SELECT count(*) FROM a.t t WHERE t.id = 3")
            .unwrap();
        tx.send(Ok(Done::default())).unwrap();
        let _ = tx.send(
            a.execute("UPDATE a.t SET name = 'two' WHERE id = 3")
                .map_err(|e| e.to_string()),
        );
    });
    next(&rx).unwrap();
    assert!(waits(&rx), "the update did not wait");
    run(&mut b, "COMMIT");
    assert_eq!(
        next(&rx).unwrap_err(),
        "could not serialize access due to concurrent update"
    );
}
