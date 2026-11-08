//! A transaction of one snapshot on two versions of each block it writes: the one its snapshot reads
//! with its own rows, and the newest, which its commit writes with those same rows. Two transactions
//! writing different rows of one block both commit, and only the same row changed since the snapshot
//! is refused, by the table's key; a block cut or let go since the snapshot among them.
#![cfg(feature = "standalone")]

mod weather;

use std::path::{Path, PathBuf};
use warren_snail::catalog::Catalog;
use warren_snail::read::{Done, Snail};
use warren_snail::values::{push_datum, Values};
use warren_snail::write::Rows;
use weather::{one, tmp, write};

const DDL: &str = "CREATE TABLE s.t (id integer NOT NULL, v integer NOT NULL, PRIMARY KEY (id));";
const ROWS: i32 = 2048;
/// Two rows of the second block, which holds every sixteenth id from 16,384.
const C: i32 = 17600;
const D: i32 = 17616;
const LEVELS: [&str; 2] = ["REPEATABLE READ", "SERIALIZABLE"];

fn made(name: &str) -> PathBuf {
    let path = tmp(&format!("beside-{name}.snail"));
    let catalog = Catalog::parse(DDL).unwrap();
    let table = catalog.table("s.t").unwrap();
    let columns = table
        .columns
        .iter()
        .map(|c| {
            let mut v = Values::new(c.ty);
            for i in 0..ROWS {
                push_datum(
                    &mut v,
                    &warren_snail::composed::scalar_in(c.ty, &(i * 16).to_string()).unwrap(),
                );
            }
            v
        })
        .collect();
    write(&path, &catalog, &[("s.t".into(), Rows { columns })]).unwrap();
    path
}

fn open(path: &Path) -> Snail {
    Snail::open_writing(path).unwrap()
}

fn run(w: &mut Snail, sql: &str) -> Done {
    w.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn refused(w: &mut Snail, sql: &str) -> String {
    match w.execute(sql) {
        Ok(d) => panic!("{sql}: {d:?}"),
        Err(e) => e.to_string(),
    }
}

fn value(w: &mut Snail, id: i32) -> i64 {
    let sql = format!("SELECT sum(t.v) FROM s.t t WHERE t.id = {id}");
    one(&w.ask(&sql, None).unwrap_or_else(|e| panic!("{sql}: {e}")).0)
}

fn count(w: &mut Snail, sql: &str) -> i64 {
    one(&w.ask(sql, None).unwrap_or_else(|e| panic!("{sql}: {e}")).0)
}

/// Every row of a block read by the table's order: the second block's whole rows.
const SECOND_BLOCK: &str = "SELECT count(*) FROM s.t t WHERE t.id >= 16384";

/// The rows of a snail in a new reader, by a count of them and a sum of their values.
fn after(path: &Path) -> (i64, i64) {
    let mut c = Snail::open(path).unwrap();
    (
        count(&mut c, "SELECT count(*) FROM s.t t"),
        count(&mut c, "SELECT sum(t.v) FROM s.t t"),
    )
}

fn blocks_of(path: &Path, order: &str) -> Vec<u64> {
    Snail::open(path)
        .unwrap()
        .blocks()
        .unwrap()
        .iter()
        .filter(|r| r.column == "id" && r.order == order)
        .map(|r| r.rows)
        .collect()
}

#[test]
fn the_rows_written_lie_in_one_block() {
    let path = made("one-block");
    assert_eq!(blocks_of(&path, "t_pkey"), [1024, 1024]);
    let mut w = open(&path);
    run(&mut w, &format!("UPDATE s.t SET v = 1 WHERE id = {C}"));
    run(&mut w, &format!("UPDATE s.t SET v = 2 WHERE id = {D}"));
    assert_eq!(blocks_of(&path, "t_pkey"), [1024, 1024]);
}

/// A table whose rows lie in one block on its order and in two on a strand, so that each transaction
/// reads its own rows along the strand without looking at the other's.
const STRANDED: &str =
    "CREATE TABLE s.i (id integer NOT NULL, g integer NOT NULL, v integer NOT NULL, \
                        PRIMARY KEY (id)); CREATE INDEX i_g ON s.i (g);";

fn stranded(name: &str) -> PathBuf {
    let path = tmp(&format!("beside-{name}.snail"));
    let catalog = Catalog::parse(STRANDED).unwrap();
    let table = catalog.table("s.i").unwrap();
    let columns = table
        .columns
        .iter()
        .map(|c| {
            let mut v = Values::new(c.ty);
            for i in 0..ROWS {
                // Every second row's g far above the others'.
                let value = match c.name.as_str() {
                    "g" if i % 2 == 0 => i * 16,
                    "g" => i * 16 + 1_048_576,
                    _ => i * 16,
                };
                push_datum(
                    &mut v,
                    &warren_snail::composed::scalar_in(c.ty, &value.to_string()).unwrap(),
                );
            }
            v
        })
        .collect();
    write(&path, &catalog, &[("s.i".into(), Rows { columns })]).unwrap();
    path
}

#[test]
fn two_inserts_into_one_block_both_commit_each_read_seeing_its_own_snapshot_and_rows() {
    for level in LEVELS {
        let path = stranded(&format!("inserts-{}", level.replace(' ', "-")));
        let (mut a, mut b, mut c) = (open(&path), open(&path), open(&path));
        assert_eq!(blocks_of(&path, "i_pkey").len(), 2);
        assert_eq!(blocks_of(&path, "i_g").len(), 2);
        run(&mut a, &format!("BEGIN ISOLATION LEVEL {level}"));
        run(&mut b, &format!("BEGIN ISOLATION LEVEL {level}"));
        // Each takes its snapshot; a third's commit lands after it, in the block both write.
        let (low, high) = (
            "SELECT count(*) FROM s.i t WHERE t.g >= 0 AND t.g < 30000",
            "SELECT count(*) FROM s.i t WHERE t.g >= 1048592",
        );
        let by = Some("i_g");
        let lows = one(&a.ask(low, by).unwrap().0);
        let highs = one(&b.ask(high, by).unwrap().0);
        assert!(lows > 0 && highs > 0);
        run(&mut c, "INSERT INTO s.i (id, g, v) VALUES (17, 29999, 1)");
        run(&mut c, "INSERT INTO s.i (id, g, v) VALUES (33, 1099999, 1)");
        // Both rows lie in the first block of the table's order, each in its own of the strand's.
        run(&mut a, "INSERT INTO s.i (id, g, v) VALUES (1, 1, 1)");
        run(&mut b, "INSERT INTO s.i (id, g, v) VALUES (2, 1048600, 1)");
        // Each reads its snapshot and its own row, neither the third's nor the other's.
        assert_eq!(one(&a.ask(low, by).unwrap().0), lows + 1);
        assert_eq!(one(&b.ask(high, by).unwrap().0), highs + 1);
        let own = |w: &mut Snail, g: i32| {
            let sql = format!("SELECT count(*) FROM s.i t WHERE t.g = {g}");
            one(&w.ask(&sql, by).unwrap().0)
        };
        assert_eq!(own(&mut a, 1), 1, "{level}: its own row");
        assert_eq!(own(&mut b, 1_048_600), 1, "{level}: its own row");
        run(&mut a, "COMMIT");
        // The other's commit is not its to see.
        assert_eq!(one(&b.ask(high, by).unwrap().0), highs + 1);
        assert_eq!(own(&mut b, 1_048_601), 0, "{level}: a row none wrote");
        run(&mut b, "COMMIT");
        let mut r = Snail::open(&path).unwrap();
        assert_eq!(
            one(&r.ask("SELECT count(*) FROM s.i t", None).unwrap().0),
            i64::from(ROWS) + 4,
            "{level}"
        );
        for id in [1, 2, 17, 33] {
            let on_id = format!("SELECT count(*) FROM s.i t WHERE t.id = {id}");
            assert_eq!(one(&r.ask(&on_id, None).unwrap().0), 1, "{level}: {id}");
        }
    }
}

#[test]
fn at_repeatable_read_two_writers_of_different_rows_of_one_block_both_commit() {
    let path = made("different");
    let (mut a, mut b) = (open(&path), open(&path));
    let before = after(&path).1;
    run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    run(&mut b, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(value(&mut a, C), i64::from(C));
    assert_eq!(value(&mut b, D), i64::from(D));
    run(&mut a, &format!("UPDATE s.t SET v = 1 WHERE id = {C}"));
    // No wait for the first's end, as PostgreSQL has none: the second's row is its own.
    run(&mut b, &format!("UPDATE s.t SET v = 2 WHERE id = {D}"));
    assert_eq!((value(&mut a, C), value(&mut a, D)), (1, i64::from(D)));
    assert_eq!((value(&mut b, C), value(&mut b, D)), (i64::from(C), 2));
    // The later commit first.
    run(&mut b, "COMMIT");
    assert_eq!((value(&mut a, C), value(&mut a, D)), (1, i64::from(D)));
    run(&mut a, "COMMIT");
    assert_eq!(
        after(&path),
        (
            i64::from(ROWS),
            before - i64::from(C) - i64::from(D) + 1 + 2
        )
    );
    let mut r = Snail::open(&path).unwrap();
    assert_eq!((value(&mut r, C), value(&mut r, D)), (1, 2));
}

#[test]
fn an_update_a_delete_and_an_insert_each_of_its_own_row_of_one_block_all_commit() {
    for level in LEVELS {
        let path = made(&format!("kinds-{}", level.replace(' ', "-")));
        let (mut a, mut b, mut c) = (open(&path), open(&path), open(&path));
        run(&mut a, &format!("BEGIN ISOLATION LEVEL {level}"));
        run(&mut b, &format!("BEGIN ISOLATION LEVEL {level}"));
        run(&mut c, &format!("BEGIN ISOLATION LEVEL {level}"));
        run(
            &mut a,
            &format!("INSERT INTO s.t (id, v) VALUES ({}, 1)", C + 1),
        );
        run(
            &mut b,
            &format!("INSERT INTO s.t (id, v) VALUES ({}, 2)", C + 2),
        );
        run(
            &mut c,
            &format!("INSERT INTO s.t (id, v) VALUES ({}, 3)", C + 3),
        );
        for w in [&mut a, &mut b, &mut c] {
            run(w, "COMMIT");
        }
        assert_eq!(after(&path).0, i64::from(ROWS) + 3, "{level}");
        // By key, at REPEATABLE READ; SERIALIZABLE's reads mark the block they look in.
        if level == "REPEATABLE READ" {
            let (mut a, mut b, mut c) = (open(&path), open(&path), open(&path));
            run(&mut a, &format!("BEGIN ISOLATION LEVEL {level}"));
            run(&mut b, &format!("BEGIN ISOLATION LEVEL {level}"));
            run(&mut c, &format!("BEGIN ISOLATION LEVEL {level}"));
            run(&mut a, &format!("UPDATE s.t SET v = -1 WHERE id = {C}"));
            run(&mut b, &format!("DELETE FROM s.t WHERE id = {D}"));
            run(
                &mut c,
                &format!("INSERT INTO s.t (id, v) VALUES ({}, 4)", D + 1),
            );
            for w in [&mut b, &mut c, &mut a] {
                run(w, "COMMIT");
            }
            let mut r = Snail::open(&path).unwrap();
            assert_eq!(value(&mut r, C), -1);
            assert_eq!(
                count(
                    &mut r,
                    &format!("SELECT count(*) FROM s.t t WHERE t.id = {D}")
                ),
                0
            );
            assert_eq!(value(&mut r, D + 1), 4);
            assert_eq!(after(&path).0, i64::from(ROWS) + 3);
        }
    }
}

#[test]
fn at_serializable_two_updates_of_one_block_meet_as_their_scans_mark_the_block() {
    let path = made("marked");
    let (mut a, mut b) = (open(&path), open(&path));
    run(&mut a, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    run(&mut b, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    run(&mut a, &format!("UPDATE s.t SET v = 1 WHERE id = {C}"));
    run(&mut b, &format!("UPDATE s.t SET v = 2 WHERE id = {D}"));
    run(&mut a, "COMMIT");
    let e = refused(&mut b, "COMMIT");
    assert!(
        e.starts_with("could not serialize access due to read/write dependencies"),
        "{e}"
    );
    let mut r = Snail::open(&path).unwrap();
    assert_eq!((value(&mut r, C), value(&mut r, D)), (1, i64::from(D)));
}

#[test]
fn the_same_row_changed_since_is_refused_by_the_table_s_key_in_postgresql_s_words() {
    for level in LEVELS {
        let path = made(&format!("same-{}", level.replace(' ', "-")));
        let (mut a, mut b) = (open(&path), open(&path));
        let start = |a: &mut Snail| {
            run(a, &format!("BEGIN ISOLATION LEVEL {level}"));
            assert_eq!(count(a, "SELECT count(*) FROM s.t t WHERE t.id = 0"), 1);
        };
        // An update of a row updated since.
        start(&mut a);
        run(&mut b, &format!("UPDATE s.t SET v = 5 WHERE id = {C}"));
        assert_eq!(
            refused(&mut a, &format!("UPDATE s.t SET v = 6 WHERE id = {C}")),
            "could not serialize access due to concurrent update",
            "{level}"
        );
        run(&mut a, "ROLLBACK");
        // A delete of a row updated since.
        start(&mut a);
        run(&mut b, &format!("UPDATE s.t SET v = 7 WHERE id = {C}"));
        assert_eq!(
            refused(&mut a, &format!("DELETE FROM s.t WHERE id = {C}")),
            "could not serialize access due to concurrent update",
            "{level}"
        );
        run(&mut a, "ROLLBACK");
        // An update of a row deleted since.
        start(&mut a);
        run(&mut b, &format!("DELETE FROM s.t WHERE id = {D}"));
        assert_eq!(
            refused(&mut a, &format!("UPDATE s.t SET v = 6 WHERE id = {D}")),
            "could not serialize access due to concurrent delete",
            "{level}"
        );
        run(&mut a, "ROLLBACK");
        // A delete of a row deleted since.
        run(&mut b, &format!("INSERT INTO s.t (id, v) VALUES ({D}, 1)"));
        start(&mut a);
        run(&mut b, &format!("DELETE FROM s.t WHERE id = {D}"));
        assert_eq!(
            refused(&mut a, &format!("DELETE FROM s.t WHERE id = {D}")),
            "could not serialize access due to concurrent delete",
            "{level}"
        );
        run(&mut a, "ROLLBACK");
        // A row whose key another changed since moved: an update.
        run(&mut b, &format!("INSERT INTO s.t (id, v) VALUES ({D}, 1)"));
        start(&mut a);
        run(
            &mut b,
            &format!("UPDATE s.t SET id = {} WHERE id = {D}", D + 1),
        );
        assert_eq!(
            refused(&mut a, &format!("UPDATE s.t SET v = 6 WHERE id = {D}")),
            "could not serialize access due to concurrent update",
            "{level}"
        );
        run(&mut a, "ROLLBACK");
        // A row the statement leaves as it was, another having changed it since, is refused all the same.
        start(&mut a);
        run(&mut b, &format!("UPDATE s.t SET v = 8 WHERE id = {C}"));
        assert_eq!(
            refused(&mut a, &format!("UPDATE s.t SET v = 7 WHERE id = {C}")),
            "could not serialize access due to concurrent update",
            "{level}"
        );
        run(&mut a, "ROLLBACK");
        // Its own row is its own: a row it changed, changed again.
        start(&mut a);
        run(
            &mut b,
            &format!("UPDATE s.t SET v = 9 WHERE id = {}", C + 32),
        );
        run(&mut a, &format!("UPDATE s.t SET v = 10 WHERE id = {C}"));
        run(&mut a, &format!("UPDATE s.t SET v = 11 WHERE id = {C}"));
        assert_eq!(value(&mut a, C), 11);
        run(&mut a, &format!("DELETE FROM s.t WHERE id = {C}"));
        assert_eq!(
            count(
                &mut a,
                &format!("SELECT count(*) FROM s.t t WHERE t.id = {C}")
            ),
            0
        );
        run(&mut a, &format!("INSERT INTO s.t (id, v) VALUES ({C}, 12)"));
        assert_eq!(value(&mut a, C), 12);
        run(&mut a, "COMMIT");
        let mut r = Snail::open(&path).unwrap();
        assert_eq!(value(&mut r, C), 12, "{level}");
        assert_eq!(value(&mut r, C + 32), 9, "{level}");
    }
}

#[test]
fn a_key_another_committed_since_is_refused_and_one_it_deleted_since_is_free() {
    let path = made("keys");
    let (mut a, mut b) = (open(&path), open(&path));
    let key = C + 1;
    run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(count(&mut a, SECOND_BLOCK), 1024);
    run(
        &mut b,
        &format!("INSERT INTO s.t (id, v) VALUES ({key}, 1)"),
    );
    let e = refused(
        &mut a,
        &format!("INSERT INTO s.t (id, v) VALUES ({key}, 2)"),
    );
    assert!(
        e.contains("duplicate key value violates unique constraint")
            && e.ends_with(&format!(": Key (id)=({key}) already exists.")),
        "{e}"
    );
    run(&mut a, "ROLLBACK");
    // Deleted since: its key is free, the row it reads standing beside the row it wrote.
    run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(count(&mut a, SECOND_BLOCK), 1025);
    run(&mut b, &format!("DELETE FROM s.t WHERE id = {key}"));
    run(
        &mut a,
        &format!("INSERT INTO s.t (id, v) VALUES ({key}, 3)"),
    );
    assert_eq!(
        count(
            &mut a,
            &format!("SELECT count(*) FROM s.t t WHERE t.id = {key}")
        ),
        2
    );
    run(&mut a, "COMMIT");
    let mut r = Snail::open(&path).unwrap();
    assert_eq!(value(&mut r, key), 3);
    // Its own key twice is refused, the statement's end seeing both.
    run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    run(
        &mut a,
        &format!("INSERT INTO s.t (id, v) VALUES ({}, 3)", key + 1),
    );
    let e = refused(
        &mut a,
        &format!("INSERT INTO s.t (id, v) VALUES ({}, 4)", key + 1),
    );
    assert!(e.contains("duplicate key value violates unique constraint"), "{e}");
    run(&mut a, "ROLLBACK");
    // A row of its snapshot it took, its key written again.
    run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    run(&mut a, &format!("DELETE FROM s.t WHERE id = {C}"));
    run(&mut a, &format!("INSERT INTO s.t (id, v) VALUES ({C}, 5)"));
    run(&mut a, "COMMIT");
    assert_eq!(value(&mut r, C), 5);
}

#[test]
fn a_writer_of_a_key_waits_for_another_that_holds_it() {
    let path = made("key-waits");
    let key = C + 1;
    for (end, ok) in [("COMMIT", false), ("ROLLBACK", true)] {
        let key = if ok { key + 1 } else { key };
        let mut a = open(&path);
        run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
        run(
            &mut a,
            &format!("INSERT INTO s.t (id, v) VALUES ({key}, 1)"),
        );
        let q = path.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let t = std::thread::spawn(move || {
            let mut b = open(&q);
            b.execute("BEGIN ISOLATION LEVEL REPEATABLE READ").unwrap();
            let r = b
                .execute(&format!("INSERT INTO s.t (id, v) VALUES ({key}, 2)"))
                .map(|_| ())
                .map_err(|e| e.to_string());
            tx.send(r).unwrap();
        });
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(300))
                .is_err(),
            "it does not wait"
        );
        run(&mut a, end);
        let came = rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        assert_eq!(came.is_ok(), ok, "{came:?}");
        t.join().unwrap();
    }
}

#[test]
fn later_reads_never_mix_generations() {
    for level in LEVELS {
        let path = made(&format!("mixed-{}", level.replace(' ', "-")));
        let (mut a, mut b) = (open(&path), open(&path));
        run(&mut a, &format!("BEGIN ISOLATION LEVEL {level}"));
        assert_eq!(
            count(&mut a, "SELECT count(*) FROM s.t t WHERE t.id < 16384"),
            1024
        );
        // Another's rows land in the block a writes, and its row changes.
        run(&mut b, "INSERT INTO s.t (id, v) VALUES (17, 1)");
        run(&mut b, "UPDATE s.t SET v = -5 WHERE id = 32");
        run(&mut a, "INSERT INTO s.t (id, v) VALUES (33, 1)");
        // Its rows are its snapshot's and its own, whatever the newest holds.
        assert_eq!(
            count(&mut a, "SELECT count(*) FROM s.t t WHERE t.id < 16384"),
            1025
        );
        assert_eq!(
            count(&mut a, "SELECT count(*) FROM s.t t WHERE t.id = 17"),
            0
        );
        assert_eq!(value(&mut a, 32), 32);
        assert_eq!(value(&mut a, 33), 1);
        run(&mut a, "ROLLBACK");
        assert_eq!(
            count(&mut a, "SELECT count(*) FROM s.t t WHERE t.id < 16384"),
            1025
        );
        assert_eq!(
            count(&mut a, "SELECT count(*) FROM s.t t WHERE t.id = 33"),
            0
        );
    }
}

#[test]
fn a_block_cut_since_the_snapshot_takes_the_row_in_the_part_that_holds_it() {
    let path = made("cut");
    assert_eq!(blocks_of(&path, "t_pkey"), [1024, 1024]);
    let (mut a, mut x) = (open(&path), open(&path));
    run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(count(&mut a, SECOND_BLOCK), 1024);
    // Another's rows cut the first block in two.
    let values: Vec<String> = (0..1024).map(|i| format!("({}, 0)", 1 + i * 16)).collect();
    run(
        &mut x,
        &format!("INSERT INTO s.t (id, v) VALUES {}", values.join(", ")),
    );
    assert_eq!(blocks_of(&path, "t_pkey"), [1024, 1024, 1024]);
    // Rows of each part, as its snapshot reads them and as the newest holds them.
    let (low, high) = (1600, 16_000);
    run(&mut a, &format!("UPDATE s.t SET v = -1 WHERE id = {low}"));
    run(&mut a, &format!("UPDATE s.t SET v = -2 WHERE id = {high}"));
    run(&mut a, "INSERT INTO s.t (id, v) VALUES (15999, 3)");
    assert_eq!(
        count(&mut a, "SELECT count(*) FROM s.t t WHERE t.id < 16384"),
        1025,
        "its snapshot's block and its own row"
    );
    assert_eq!((value(&mut a, low), value(&mut a, high)), (-1, -2));
    run(&mut a, "COMMIT");
    let mut r = Snail::open(&path).unwrap();
    assert_eq!((value(&mut r, low), value(&mut r, high)), (-1, -2));
    assert_eq!(value(&mut r, 15999), 3);
    assert_eq!(
        count(&mut r, "SELECT count(*) FROM s.t t WHERE t.id < 16384"),
        2049
    );
    // A row that changed in a part is refused all the same.
    run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(count(&mut a, SECOND_BLOCK), 1024);
    run(&mut x, "UPDATE s.t SET v = -9 WHERE id = 1601");
    assert_eq!(
        refused(&mut a, "UPDATE s.t SET v = 0 WHERE id = 1601"),
        "could not serialize access due to concurrent update"
    );
    run(&mut a, "ROLLBACK");
}

#[test]
fn a_block_pruned_or_let_go_since_the_snapshot_is_met_too() {
    let path = made("pruned");
    let (mut a, mut x) = (open(&path), open(&path));
    // Pruned: another's deletes are read away, its rows at new places.
    run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(count(&mut a, SECOND_BLOCK), 1024);
    run(&mut x, "DELETE FROM s.t WHERE id < 8000 AND id > 0");
    x.ask("SELECT sum(t.v) FROM s.t t", None).unwrap();
    run(&mut a, "UPDATE s.t SET v = -3 WHERE id = 8000");
    assert_eq!(value(&mut a, 16), 16, "the snapshot's row, deleted since");
    assert_eq!(
        refused(&mut a, "UPDATE s.t SET v = 0 WHERE id = 16"),
        "could not serialize access due to concurrent delete"
    );
    run(&mut a, "ROLLBACK");
    run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(count(&mut a, SECOND_BLOCK), 1024);
    run(&mut a, "UPDATE s.t SET v = -3 WHERE id = 8000");
    run(&mut a, "COMMIT");
    let mut r = Snail::open(&path).unwrap();
    assert_eq!(value(&mut r, 8000), -3);
    // Let go: every row of the first block goes, and a row written where they lay lies in what remains.
    run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(count(&mut a, SECOND_BLOCK), 1024);
    run(&mut x, "DELETE FROM s.t WHERE id < 16384");
    x.ask("SELECT sum(t.v) FROM s.t t", None).unwrap();
    assert_eq!(
        blocks_of(&path, "t_pkey"),
        [1024],
        "the read pruned it away"
    );
    run(&mut a, "INSERT INTO s.t (id, v) VALUES (5, 5)");
    assert_eq!(value(&mut a, 5), 5);
    assert_eq!(value(&mut a, 8000), -3, "its snapshot's row");
    run(&mut a, "COMMIT");
    let mut r = Snail::open(&path).unwrap();
    assert_eq!(value(&mut r, 5), 5);
    assert_eq!(
        count(&mut r, "SELECT count(*) FROM s.t t WHERE t.id < 16384"),
        1
    );
}

#[test]
fn an_order_empty_on_one_version_and_not_on_the_other_takes_the_rows() {
    let path = tmp("beside-empty.snail");
    write(&path, &Catalog::parse(DDL).unwrap(), &[]).unwrap();
    let (mut a, mut b) = (open(&path), open(&path));
    run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(count(&mut a, "SELECT count(*) FROM s.t t"), 0);
    run(&mut b, "INSERT INTO s.t (id, v) VALUES (1, 1)");
    run(&mut a, "INSERT INTO s.t (id, v) VALUES (2, 2)");
    assert_eq!(count(&mut a, "SELECT count(*) FROM s.t t"), 1);
    run(&mut a, "COMMIT");
    assert_eq!(after(&path), (2, 3));
    // Rows on the snapshot, none on the newest.
    run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(count(&mut a, "SELECT count(*) FROM s.t t"), 2);
    run(&mut b, "DELETE FROM s.t");
    b.ask("SELECT sum(t.v) FROM s.t t", None).unwrap();
    run(&mut a, "INSERT INTO s.t (id, v) VALUES (3, 3)");
    assert_eq!(count(&mut a, "SELECT count(*) FROM s.t t"), 3);
    run(&mut a, "COMMIT");
    assert_eq!(after(&path), (1, 3));
}

#[test]
fn a_strand_is_written_on_each_version_and_its_commit_is_whole() {
    let path = stranded("strand");
    let (mut a, mut b) = (open(&path), open(&path));
    run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(count(&mut a, "SELECT count(*) FROM s.i t"), i64::from(ROWS));
    run(&mut b, "UPDATE s.i SET g = 5 WHERE id = 16");
    run(&mut b, "INSERT INTO s.i (id, g, v) VALUES (17, 6, 1)");
    // Its row on the strand as its snapshot holds it, then as it wrote it.
    run(&mut a, "UPDATE s.i SET g = 7 WHERE id = 32");
    let by = Some("i_g");
    let at = |w: &mut Snail, g: i32| {
        let sql = format!("SELECT count(*) FROM s.i t WHERE t.g = {g}");
        one(&w.ask(&sql, by).unwrap().0)
    };
    assert_eq!((at(&mut a, 7), at(&mut a, 5), at(&mut a, 6)), (1, 0, 0));
    run(&mut a, "COMMIT");
    let mut r = Snail::open(&path).unwrap();
    assert_eq!((at(&mut r, 7), at(&mut r, 5), at(&mut r, 6)), (1, 1, 1));
    assert_eq!(
        r.blocks()
            .unwrap()
            .iter()
            .filter(|x| x.order == "i_g" && x.column == "id")
            .map(|x| x.rows)
            .sum::<u64>(),
        r.blocks()
            .unwrap()
            .iter()
            .filter(|x| x.order == "i_pkey" && x.column == "id")
            .map(|x| x.rows)
            .sum::<u64>(),
    );
}

#[test]
fn a_commit_that_cuts_the_newest_block_cuts_it_and_the_version_it_read_stays_uncut() {
    let path = made("commit-cut");
    let mut a = open(&path);
    run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    let values: Vec<String> = (0..1024).map(|i| format!("({}, 0)", 1 + i * 16)).collect();
    run(
        &mut a,
        &format!("INSERT INTO s.t (id, v) VALUES {}", values.join(", ")),
    );
    assert_eq!(
        count(&mut a, "SELECT count(*) FROM s.t t WHERE t.id < 16384"),
        2048
    );
    assert_eq!(
        blocks_of(&path, "t_pkey"),
        [1024, 1024],
        "nothing written yet"
    );
    run(&mut a, "COMMIT");
    assert_eq!(blocks_of(&path, "t_pkey"), [1024, 1024, 1024]);
    assert_eq!(after(&path).0, 3072);
}

#[test]
fn a_transaction_s_rows_count_in_the_round_s_room() {
    let path = made("room");
    let mut a = open(&path);
    run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    let read = "SELECT count(*) FROM s.t t WHERE t.id = 0";
    let empty = a.ask(read, None).unwrap().1.bytes_held;
    let values: Vec<String> = (0..256).map(|i| format!("({}, 0)", 1 + i * 16)).collect();
    run(
        &mut a,
        &format!("INSERT INTO s.t (id, v) VALUES {}", values.join(", ")),
    );
    let held = a.ask(read, None).unwrap().1.bytes_held;
    assert!(
        held >= empty
            + 256 * 2 * std::mem::size_of::<warren_snail::read::statement::Datum>() as u64,
        "{empty} {held}"
    );
    run(&mut a, "COMMIT");
    assert_eq!(a.ask(read, None).unwrap().1.bytes_held, empty);
}

#[test]
fn a_foreign_key_at_one_snapshot_meets_rows_by_their_keys() {
    let path = tmp("beside-foreign.snail");
    let ddl = "CREATE TABLE s.p (id integer NOT NULL, v integer NOT NULL, PRIMARY KEY (id)); \
               CREATE TABLE s.c (id integer NOT NULL, r integer REFERENCES s.p (id), PRIMARY KEY (id));";
    write(&path, &Catalog::parse(ddl).unwrap(), &[]).unwrap();
    let (mut a, mut b) = (open(&path), open(&path));
    run(
        &mut b,
        "INSERT INTO s.p (id, v) VALUES (1, 1), (2, 2), (3, 3), (4, 4)",
    );
    // Another row of the parent's block changed since: the key stands.
    run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(count(&mut a, "SELECT count(*) FROM s.p p"), 4);
    run(&mut b, "UPDATE s.p SET v = 9 WHERE id = 2");
    run(&mut a, "INSERT INTO s.c (id, r) VALUES (1, 1)");
    run(&mut a, "COMMIT");
    // The key taken since: refused as PostgreSQL's FOR KEY SHARE on its snapshot's row is.
    run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(count(&mut a, "SELECT count(*) FROM s.p p"), 4);
    run(&mut b, "DELETE FROM s.p WHERE id = 3");
    assert_eq!(
        refused(&mut a, "INSERT INTO s.c (id, r) VALUES (2, 3)"),
        "could not serialize access due to concurrent update"
    );
    run(&mut a, "ROLLBACK");
    // A key its snapshot does not hold, written since: no key.
    run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(count(&mut a, "SELECT count(*) FROM s.p p"), 3);
    run(&mut b, "INSERT INTO s.p (id, v) VALUES (5, 5)");
    let e = refused(&mut a, "INSERT INTO s.c (id, r) VALUES (3, 5)");
    assert!(e.contains("violates foreign key constraint"), "{e}");
    run(&mut a, "ROLLBACK");
    // A child written since of a key it takes: refused, a row its snapshot does not see.
    run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(count(&mut a, "SELECT count(*) FROM s.p p"), 4);
    run(&mut b, "INSERT INTO s.c (id, r) VALUES (4, 4)");
    assert_eq!(
        refused(&mut a, "DELETE FROM s.p WHERE id = 4"),
        "could not serialize access due to concurrent update"
    );
    run(&mut a, "ROLLBACK");
    // Held: a writer of the key waits for the transaction that read it.
    run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    run(&mut a, "INSERT INTO s.c (id, r) VALUES (5, 5)");
    let q = path.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let t = std::thread::spawn(move || {
        let mut b = open(&q);
        let r = b
            .execute("DELETE FROM s.p WHERE id = 5")
            .map(|_| ())
            .map_err(|e| e.to_string());
        tx.send(r).unwrap();
    });
    assert!(rx
        .recv_timeout(std::time::Duration::from_millis(300))
        .is_err());
    run(&mut a, "COMMIT");
    let came = rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
    assert!(came
        .unwrap_err()
        .contains("violates foreign key constraint"));
    t.join().unwrap();
}

#[test]
fn an_exclude_at_one_snapshot_meets_what_is_committed_and_its_own_rows() {
    let path = tmp("beside-exclude.snail");
    let ddl = "CREATE TABLE s.x (id integer NOT NULL, during int4range, PRIMARY KEY (id), \
               EXCLUDE USING gist (during WITH &&));";
    write(&path, &Catalog::parse(ddl).unwrap(), &[]).unwrap();
    let (mut a, mut b) = (open(&path), open(&path));
    run(&mut b, "INSERT INTO s.x (id, during) VALUES (1, '[1,5)')");
    run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(count(&mut a, "SELECT count(*) FROM s.x x"), 1);
    run(&mut b, "INSERT INTO s.x (id, during) VALUES (2, '[10,15)')");
    run(&mut b, "DELETE FROM s.x WHERE id = 1");
    // Committed since: met; deleted since: free; its own: met.
    let e = refused(&mut a, "INSERT INTO s.x (id, during) VALUES (3, '[12,13)')");
    assert!(e.starts_with("conflicting key value"), "{e}");
    run(&mut a, "ROLLBACK");
    run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    run(&mut a, "INSERT INTO s.x (id, during) VALUES (4, '[2,3)')");
    let e = refused(&mut a, "INSERT INTO s.x (id, during) VALUES (5, '[2,4)')");
    assert!(e.starts_with("conflicting key value"), "{e}");
    run(&mut a, "ROLLBACK");
    run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(count(&mut a, "SELECT count(*) FROM s.x x"), 1);
    run(&mut b, "INSERT INTO s.x (id, during) VALUES (6, '[20,25)')");
    run(&mut a, "INSERT INTO s.x (id, during) VALUES (7, '[2,3)')");
    run(&mut a, "COMMIT");
    let mut r = Snail::open(&path).unwrap();
    assert_eq!(count(&mut r, "SELECT count(*) FROM s.x x"), 3);
}

#[test]
fn a_locked_row_is_held_to_the_newest_root_by_the_row_and_its_own_is_its_own() {
    let path = made("locked");
    let (mut a, mut b) = (open(&path), open(&path));
    run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(count(&mut a, SECOND_BLOCK), 1024);
    run(&mut b, &format!("UPDATE s.t SET v = 1 WHERE id = {D}"));
    // Another row of the block changed since, its own row in it.
    run(
        &mut a,
        &format!("INSERT INTO s.t (id, v) VALUES ({}, 3)", C + 1),
    );
    run(
        &mut a,
        &format!("SELECT t.id FROM s.t t WHERE t.id = {C} FOR UPDATE"),
    );
    run(
        &mut a,
        &format!("SELECT t.id FROM s.t t WHERE t.id = {} FOR UPDATE", C + 1),
    );
    assert_eq!(
        refused(
            &mut a,
            &format!("SELECT t.id FROM s.t t WHERE t.id = {D} FOR UPDATE")
        ),
        "could not serialize access due to concurrent update"
    );
    run(&mut a, "ROLLBACK");
}

#[test]
fn an_older_format_refuses_a_write_of_a_block_written_since_by_name() {
    let catalog = Catalog::parse(DDL).unwrap();
    let table = catalog.table("s.t").unwrap();
    let columns = table
        .columns
        .iter()
        .map(|c| {
            let mut v = Values::new(c.ty);
            for i in 0..ROWS {
                push_datum(
                    &mut v,
                    &warren_snail::composed::scalar_in(c.ty, &(i * 16).to_string()).unwrap(),
                );
            }
            v
        })
        .collect();
    let path = tmp("beside-older.snail");
    warren_snail::write::write_format_eight(
        &path,
        &catalog,
        &[("s.t".into(), Rows { columns })],
        weather::span(),
    )
    .unwrap();
    let (mut a, mut b) = (open(&path), open(&path));
    run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(count(&mut a, SECOND_BLOCK), 1024);
    run(&mut b, &format!("UPDATE s.t SET v = 1 WHERE id = {D}"));
    let e = refused(&mut a, &format!("UPDATE s.t SET v = 2 WHERE id = {C}"));
    assert!(
        e.starts_with("a snail of format 8 writes no row into a block"),
        "{e}"
    );
    assert!(e.contains("`out`"), "{e}");
    run(&mut a, "ROLLBACK");
    // A block no other wrote is written, and its later statement beside it.
    run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(count(&mut a, SECOND_BLOCK), 1024);
    run(&mut b, "UPDATE s.t SET v = 1 WHERE id = 16");
    run(&mut a, &format!("UPDATE s.t SET v = 2 WHERE id = {C}"));
    run(&mut a, &format!("UPDATE s.t SET v = 3 WHERE id = {D}"));
    run(&mut a, "COMMIT");
    let mut r = Snail::open(&path).unwrap();
    assert_eq!((value(&mut r, C), value(&mut r, D)), (2, 3));
}

#[test]
fn a_child_written_beside_a_key_taken_is_refused_at_the_commit() {
    let path = tmp("beside-keys-commit.snail");
    let ddl = "CREATE TABLE s.p (id integer NOT NULL, v integer NOT NULL, PRIMARY KEY (id)); \
               CREATE TABLE s.c (id integer NOT NULL, r integer REFERENCES s.p (id), PRIMARY KEY (id));";
    write(&path, &Catalog::parse(ddl).unwrap(), &[]).unwrap();
    let (mut a, mut b) = (open(&path), open(&path));
    run(&mut b, "INSERT INTO s.p (id, v) VALUES (1, 1), (2, 2)");
    run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    run(&mut a, "DELETE FROM s.p WHERE id = 1");
    // A writer at READ COMMITTED reads the key as the newest root holds it.
    run(&mut b, "INSERT INTO s.c (id, r) VALUES (1, 1)");
    assert_eq!(
        refused(&mut a, "COMMIT"),
        "could not serialize access due to concurrent update"
    );
    let mut r = Snail::open(&path).unwrap();
    assert_eq!(count(&mut r, "SELECT count(*) FROM s.p p"), 2);
}

#[test]
fn a_row_past_the_bytes_a_transaction_names_is_met_at_its_commit() {
    let path = made("past-named");
    let (mut a, mut b) = (open(&path), open(&path));
    run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    run(&mut a, "UPDATE s.t SET v = -1 WHERE id >= 16384");
    // Its rows are not named: another's commit of one of them does not wait.
    run(&mut b, &format!("UPDATE s.t SET v = -2 WHERE id = {C}"));
    assert_eq!(
        refused(&mut a, "COMMIT"),
        "could not serialize access due to concurrent update"
    );
    let mut r = Snail::open(&path).unwrap();
    assert_eq!((value(&mut r, C), value(&mut r, D)), (-2, i64::from(D)));
}

#[test]
fn a_table_another_declared_an_order_of_since_refuses_its_rows() {
    let path = tmp("beside-declared.snail");
    let ddl = "CREATE TABLE s.t (id integer NOT NULL, v integer NOT NULL, PRIMARY KEY (id)); \
               CREATE TABLE s.e (id integer NOT NULL, v integer NOT NULL, PRIMARY KEY (id));";
    write(&path, &Catalog::parse(ddl).unwrap(), &[]).unwrap();
    let (mut a, mut b) = (open(&path), open(&path));
    run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(count(&mut a, "SELECT count(*) FROM s.t t"), 0);
    run(&mut b, "CREATE INDEX e_v ON s.e (v)");
    run(&mut a, "INSERT INTO s.e (id, v) VALUES (1, 1)");
    assert_eq!(
        refused(&mut a, "COMMIT"),
        "could not serialize access due to concurrent update"
    );
    // On a table it left as it was, another's DDL does not stand in the way.
    run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(count(&mut a, "SELECT count(*) FROM s.t t"), 0);
    run(&mut b, "CREATE INDEX e_id ON s.e (id, v)");
    run(&mut a, "INSERT INTO s.t (id, v) VALUES (1, 1)");
    run(&mut a, "COMMIT");
    assert_eq!(count(&mut a, "SELECT count(*) FROM s.t t"), 1);
}

#[test]
fn a_table_declared_in_the_transaction_takes_its_rows_at_its_commit() {
    let path = made("declared-here");
    let (mut a, mut b) = (open(&path), open(&path));
    run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(count(&mut a, SECOND_BLOCK), 1024);
    run(
        &mut a,
        &format!("INSERT INTO s.t (id, v) VALUES ({}, 1)", C + 1),
    );
    run(
        &mut a,
        "CREATE TABLE s.n (id integer NOT NULL, v integer NOT NULL, PRIMARY KEY (id))",
    );
    run(&mut a, "INSERT INTO s.n (id, v) VALUES (1, 1), (2, 2)");
    run(&mut b, &format!("UPDATE s.t SET v = 5 WHERE id = {D}"));
    assert_eq!(count(&mut a, "SELECT count(*) FROM s.n n"), 2);
    run(&mut a, "COMMIT");
    let mut r = Snail::open(&path).unwrap();
    assert_eq!(count(&mut r, "SELECT count(*) FROM s.n n"), 2);
    assert_eq!((value(&mut r, C + 1), value(&mut r, D)), (1, 5));
}

#[test]
fn rows_of_every_kind_of_value_are_found_on_the_newest_version_by_their_values() {
    let path = tmp("beside-kinds.snail");
    let ddl = "CREATE TABLE s.k (id integer NOT NULL, t text, n numeric(30, 2), u uuid, a integer[], \
               i interval, f double precision, d timestamptz, r int4range, j jsonb, PRIMARY KEY (id));";
    write(&path, &Catalog::parse(ddl).unwrap(), &[]).unwrap();
    let (mut a, mut b) = (open(&path), open(&path));
    let row = |id: i32| {
        format!(
            "({id}, 'x{id}', {id}.25, '00000000-0000-0000-0000-00000000000{}', '{{1,{id}}}', \
             '{id} days', {id}.5, '2026-01-0{} 00:00:00+00', '[1,{}]', '{{\"a\": {id}}}')",
            id % 10,
            1 + id % 9,
            id + 1
        )
    };
    let values: Vec<String> = (1..=8).map(row).collect();
    run(
        &mut b,
        &format!(
            "INSERT INTO s.k (id, t, n, u, a, i, f, d, r, j) VALUES {}",
            values.join(", ")
        ),
    );
    run(&mut b, "INSERT INTO s.k (id) VALUES (9)");
    run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(count(&mut a, "SELECT count(*) FROM s.k k"), 9);
    // Another's row of the block changed since; rows of every kind of value, and one all NULLs.
    run(&mut b, "UPDATE s.k SET t = 'changed' WHERE id = 2");
    run(&mut a, "UPDATE s.k SET t = 'mine' WHERE id = 3");
    run(&mut a, "DELETE FROM s.k WHERE id = 4");
    run(&mut a, "UPDATE s.k SET t = 'mine' WHERE id = 9");
    assert_eq!(
        refused(&mut a, "UPDATE s.k SET t = 'mine' WHERE id = 2"),
        "could not serialize access due to concurrent update"
    );
    run(&mut a, "ROLLBACK");
    run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(count(&mut a, "SELECT count(*) FROM s.k k"), 9);
    run(&mut b, "UPDATE s.k SET t = 'changed' WHERE id = 1");
    run(&mut a, "UPDATE s.k SET t = 'mine' WHERE id = 3");
    run(&mut a, "DELETE FROM s.k WHERE id = 4");
    run(&mut a, "UPDATE s.k SET t = 'mine' WHERE id = 9");
    run(&mut a, "COMMIT");
    let mut r = Snail::open(&path).unwrap();
    assert_eq!(count(&mut r, "SELECT count(*) FROM s.k k"), 8);
    assert_eq!(
        count(&mut r, "SELECT count(*) FROM s.k k WHERE k.t = 'mine'"),
        2
    );
    assert_eq!(
        count(&mut r, "SELECT count(*) FROM s.k k WHERE k.t = 'changed'"),
        2
    );
}

#[test]
fn a_period_key_on_a_block_written_since_is_refused_by_the_block() {
    let path = tmp("beside-period.snail");
    let ddl = "CREATE TABLE s.tp (id int4range, valid int4range, PRIMARY KEY (id, valid WITHOUT OVERLAPS)); \
               CREATE TABLE s.tc (id integer NOT NULL, pid int4range, valid int4range, PRIMARY KEY (id), \
               FOREIGN KEY (pid, PERIOD valid) REFERENCES s.tp);";
    write(&path, &Catalog::parse(ddl).unwrap(), &[]).unwrap();
    let (mut a, mut b) = (open(&path), open(&path));
    run(
        &mut b,
        "INSERT INTO s.tp (id, valid) VALUES ('[1,2)', '[1,10)'), ('[5,6)', '[1,10)')",
    );
    run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(count(&mut a, "SELECT count(*) FROM s.tp p"), 2);
    run(
        &mut b,
        "UPDATE s.tp SET valid = '[1,20)' WHERE id = '[5,6)'",
    );
    assert_eq!(
        refused(
            &mut a,
            "INSERT INTO s.tc (id, pid, valid) VALUES (1, '[1,2)', '[2,4)')"
        ),
        "could not serialize access due to concurrent update"
    );
    run(&mut a, "ROLLBACK");
    // Beside a block no commit wrote, it is taken.
    run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    run(
        &mut a,
        "INSERT INTO s.tc (id, pid, valid) VALUES (1, '[1,2)', '[2,4)')",
    );
    run(&mut a, "COMMIT");
    let mut r = Snail::open(&path).unwrap();
    assert_eq!(count(&mut r, "SELECT count(*) FROM s.tc c"), 1);
}

#[test]
fn a_cascade_at_one_snapshot_takes_the_children_it_reads_and_its_commit_meets_another_s() {
    let path = tmp("beside-cascade.snail");
    let ddl = "CREATE TABLE s.p (id integer NOT NULL, PRIMARY KEY (id)); \
               CREATE TABLE s.c (id integer NOT NULL, r integer REFERENCES s.p (id) ON DELETE CASCADE, \
               PRIMARY KEY (id));";
    write(&path, &Catalog::parse(ddl).unwrap(), &[]).unwrap();
    let (mut a, mut b) = (open(&path), open(&path));
    run(&mut b, "INSERT INTO s.p (id) VALUES (1), (2), (3)");
    run(
        &mut b,
        "INSERT INTO s.c (id, r) VALUES (1, 1), (2, 1), (3, 2)",
    );
    run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(count(&mut a, "SELECT count(*) FROM s.c c"), 3);
    // Another's child of another key, in the block of the children it takes.
    run(&mut b, "INSERT INTO s.c (id, r) VALUES (4, 2)");
    run(&mut a, "DELETE FROM s.p WHERE id = 1");
    assert_eq!(count(&mut a, "SELECT count(*) FROM s.c c"), 1);
    run(&mut a, "COMMIT");
    let mut r = Snail::open(&path).unwrap();
    assert_eq!(count(&mut r, "SELECT count(*) FROM s.c c"), 2);
    // Another's child of the key it takes, written since: met by its commit.
    run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(count(&mut a, "SELECT count(*) FROM s.c c"), 2);
    run(&mut a, "DELETE FROM s.p WHERE id = 3");
    run(&mut b, "INSERT INTO s.c (id, r) VALUES (5, 3)");
    assert_eq!(
        refused(&mut a, "COMMIT"),
        "could not serialize access due to concurrent update"
    );
    assert_eq!(count(&mut r, "SELECT count(*) FROM s.c c"), 3);
}
