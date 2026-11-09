//! A transaction's savepoints, as PostgreSQL's subtransactions: ROLLBACK TO puts the transaction back
//! as it stood, the pages staged since dropped and the locks taken since let go; RELEASE keeps what
//! was done since; a statement that fails in a savepoint lets go of what it and the statements since
//! did, and the transaction goes on from ROLLBACK TO. Held at READ COMMITTED, REPEATABLE READ and
//! SERIALIZABLE, through a crash, and across several snails; COMMIT AND CHAIN, ROLLBACK AND CHAIN and
//! LOCK TABLE beside them.
#![cfg(feature = "standalone")]

mod weather;

use std::sync::mpsc;
use std::time::Duration;
use warren_snail::read::Snail;
use weather::*;

const STATIONS: &str = "SELECT count(*) FROM weather.stations s";

fn count(s: &mut Snail, q: &str) -> i64 {
    one(&s.ask(q, None).unwrap().0)
}

fn station(no: i64) -> String {
    format!("INSERT INTO weather.stations (network_id, station_no, name) VALUES (9300, {no}, 'Faro {no}')")
}

fn named(s: &mut Snail, no: i64) -> i64 {
    count(
        s,
        &format!("SELECT count(*) FROM weather.stations s WHERE s.network_id = 9300 AND s.station_no = {no}"),
    )
}

fn begin(level: &str) -> String {
    format!("BEGIN ISOLATION LEVEL {level}")
}

const LEVELS: [&str; 3] = ["READ COMMITTED", "REPEATABLE READ", "SERIALIZABLE"];

#[test]
fn rollback_to_puts_the_transaction_back_as_it_stood_and_release_keeps_what_was_done_since() {
    let s = sample(2_000);
    for level in LEVELS {
        let path = snail(&format!("savepoint-back-{}", level.replace(' ', "-")), &s);
        let n = count(&mut fresh(&path), STATIONS);
        let mut w = Snail::open_writing(&path).unwrap();
        w.execute(&begin(level)).unwrap();
        w.execute(&station(1)).unwrap();
        w.execute("SAVEPOINT a").unwrap();
        w.execute(&station(2)).unwrap();
        w.execute("SAVEPOINT b").unwrap();
        w.execute(&station(3)).unwrap();
        assert_eq!(count(&mut w, STATIONS), n + 3, "{level}");
        // Back to a: b is gone with what was done since a; a stays.
        let d = w.execute("ROLLBACK TO SAVEPOINT a").unwrap();
        assert_eq!(d.statement, "ROLLBACK");
        assert_eq!(count(&mut w, STATIONS), n + 1, "{level}");
        let e = w.execute("ROLLBACK TO b").unwrap_err().to_string();
        assert_eq!(e, "savepoint \"b\" does not exist");
        // That failed the transaction, back to a again: it takes only ROLLBACK TO or its end.
        let e = w.execute(&station(4)).unwrap_err().to_string();
        assert!(e.contains("current transaction is aborted"), "{e}");
        w.execute("ROLLBACK TO a").unwrap();
        w.execute(&station(5)).unwrap();
        w.execute("SAVEPOINT c").unwrap();
        w.execute(&station(6)).unwrap();
        w.execute("RELEASE c").unwrap();
        assert!(w.execute("RELEASE c").is_err(), "{level}");
        w.execute("ROLLBACK TO a").unwrap();
        w.execute(&station(7)).unwrap();
        w.execute("RELEASE SAVEPOINT a").unwrap();
        w.execute("COMMIT").unwrap();
        let mut r = fresh(&path);
        assert_eq!(count(&mut r, STATIONS), n + 2, "{level}");
        for (no, held) in [(1, 1), (2, 0), (3, 0), (5, 0), (6, 0), (7, 1)] {
            assert_eq!(named(&mut r, no), held, "{level}: station {no}");
        }
        r.census().unwrap();
    }
}

#[test]
fn a_statement_that_fails_in_a_savepoint_lets_go_of_what_was_done_since_and_the_transaction_goes_on(
) {
    let s = sample(2_000);
    for level in LEVELS {
        let path = snail(&format!("savepoint-failed-{}", level.replace(' ', "-")), &s);
        let n = count(&mut fresh(&path), STATIONS);
        let mut w = Snail::open_writing(&path).unwrap();
        w.execute(&begin(level)).unwrap();
        w.execute(&station(1)).unwrap();
        w.execute("SAVEPOINT a").unwrap();
        w.execute(&station(2)).unwrap();
        // The same key again: refused at the statement's end.
        assert!(w.execute(&station(1)).is_err(), "{level}");
        let e = w.ask(STATIONS, None).unwrap_err().to_string();
        assert!(e.contains("current transaction is aborted"), "{level}: {e}");
        assert!(w.execute("SAVEPOINT b").is_err());
        assert!(w.execute("RELEASE a").is_err());
        w.execute("ROLLBACK TO a").unwrap();
        assert_eq!(count(&mut w, STATIONS), n + 1, "{level}");
        // A statement the snail refuses fails the transaction too.
        assert!(w.execute("SELEKT 1").is_err());
        assert!(w.execute(&station(3)).is_err());
        w.execute("ROLLBACK TO a").unwrap();
        w.execute(&station(3)).unwrap();
        w.execute("COMMIT").unwrap();
        let mut r = fresh(&path);
        assert_eq!(count(&mut r, STATIONS), n + 2, "{level}");
        assert_eq!(
            (named(&mut r, 1), named(&mut r, 2), named(&mut r, 3)),
            (1, 0, 1)
        );
        r.census().unwrap();
        // With no savepoint, a failed statement leaves only the end, which rolls back.
        let mut w = Snail::open_writing(&path).unwrap();
        w.execute(&begin(level)).unwrap();
        w.execute(&station(8)).unwrap();
        assert!(w.execute(&station(8)).is_err());
        assert!(w.execute("ROLLBACK TO a").is_err());
        assert_eq!(w.execute("COMMIT").unwrap().statement, "ROLLBACK");
        assert_eq!(named(&mut fresh(&path), 8), 0);
    }
}

#[test]
fn a_savepoint_rolled_back_drops_the_pages_staged_since_so_the_commit_writes_what_one_without_them_writes(
) {
    let s = sample(2_000);
    let with = snail("savepoint-pages-with", &s);
    let without = snail("savepoint-pages-without", &s);
    let mut a = Snail::open_writing(&with).unwrap();
    a.execute("BEGIN").unwrap();
    a.execute(&station(1)).unwrap();
    a.execute("SAVEPOINT a").unwrap();
    for no in 2..40 {
        a.execute(&station(no)).unwrap();
    }
    a.execute("UPDATE weather.readings SET checked_at = '2023-01-01 00:00:00+00'")
        .unwrap();
    a.execute("ROLLBACK TO a").unwrap();
    a.execute(&station(40)).unwrap();
    let wrote = a.execute("COMMIT").unwrap().pages_written;
    let mut b = Snail::open_writing(&without).unwrap();
    b.execute("BEGIN").unwrap();
    b.execute(&station(1)).unwrap();
    b.execute(&station(40)).unwrap();
    assert_eq!(b.execute("COMMIT").unwrap().pages_written, wrote);
    for path in [&with, &without] {
        let r = fresh(path);
        r.census().unwrap();
    }
    assert_eq!(snail_bytes(&with), snail_bytes(&without));
}

#[test]
fn a_block_locked_since_a_savepoint_is_let_go_at_rollback_to_and_one_locked_before_it_is_held() {
    let s = sample(2_000);
    let path = snail("savepoint-locks", &s);
    let (network, station) = busiest(&s);
    let ids: Vec<i64> = in_order(&s)
        .into_iter()
        .filter(|&(n, st, _)| (n, st) == (network, station))
        .map(|(_, _, id)| id)
        .take(2)
        .collect();
    let set = |id: i64, feed: &str| {
        format!("UPDATE weather.readings SET feed = '{feed}' WHERE reading_id = {id}")
    };
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute("BEGIN").unwrap();
    w.execute("SAVEPOINT a").unwrap();
    w.execute(&set(ids[0], "AWS Main")).unwrap();
    w.execute("ROLLBACK TO a").unwrap();
    // Another writer of that block takes it at once.
    let (tx, rx) = mpsc::channel();
    let p = path.clone();
    let other = set(ids[1], "AWS Spare");
    std::thread::spawn(move || {
        let mut o = Snail::open_writing(&p).unwrap();
        tx.send(o.execute(&other).map(|d| d.rows).map_err(|e| e.to_string()))
            .unwrap();
    });
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(10)).unwrap(),
        Ok(1),
        "the block's lock was let go at ROLLBACK TO"
    );
    // A block written before the savepoint stays locked through ROLLBACK TO.
    w.execute(&set(ids[0], "AWS Main")).unwrap();
    w.execute("SAVEPOINT b").unwrap();
    w.execute("ROLLBACK TO b").unwrap();
    let (tx, rx) = mpsc::channel();
    let p = path.clone();
    let other = set(ids[1], "AWS Third");
    std::thread::spawn(move || {
        let mut o = Snail::open_writing(&p).unwrap();
        tx.send(o.execute(&other).map(|d| d.rows).map_err(|e| e.to_string()))
            .unwrap();
    });
    assert!(
        rx.recv_timeout(Duration::from_millis(1500)).is_err(),
        "the block written before the savepoint is held"
    );
    w.execute("COMMIT").unwrap();
    assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), Ok(1));
    let mut r = fresh(&path);
    assert_eq!(feed_of(&mut r, ids[0]), "AWS Main");
    assert_eq!(feed_of(&mut r, ids[1]), "AWS Third");
}

#[test]
fn a_crash_after_rollback_to_leaves_the_file_as_it_was() {
    let s = sample(2_000);
    let path = snail("savepoint-crash", &s);
    let n = count(&mut fresh(&path), STATIONS);
    let before = snail_bytes(&path);
    {
        let mut w = Snail::open_writing(&path).unwrap();
        w.execute("BEGIN").unwrap();
        w.execute(&station(1)).unwrap();
        w.execute("SAVEPOINT a").unwrap();
        w.execute(&station(2)).unwrap();
        w.execute("ROLLBACK TO a").unwrap();
        // The writer ends with no COMMIT.
    }
    assert_eq!(count(&mut fresh(&path), STATIONS), n);
    assert_eq!(snail_bytes(&path), before);
    for stop in [
        warren_snail::file::Stop::BeforeRoot,
        warren_snail::file::Stop::HalfRoot,
    ] {
        let mut w = Snail::open_writing(&path).unwrap();
        w.execute("BEGIN").unwrap();
        w.execute(&station(1)).unwrap();
        w.execute("SAVEPOINT a").unwrap();
        w.execute(&station(2)).unwrap();
        w.execute("ROLLBACK TO a").unwrap();
        w.stop_at(Some(stop));
        assert!(w.execute("COMMIT").is_err());
        drop(w);
        let mut r = fresh(&path);
        assert_eq!(count(&mut r, STATIONS), n, "{stop:?}");
    }
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute("BEGIN").unwrap();
    w.execute(&station(1)).unwrap();
    w.execute("SAVEPOINT a").unwrap();
    w.execute(&station(2)).unwrap();
    w.execute("ROLLBACK TO a").unwrap();
    w.execute("COMMIT").unwrap();
    let mut r = fresh(&path);
    assert_eq!(count(&mut r, STATIONS), n + 1);
    assert_eq!((named(&mut r, 1), named(&mut r, 2)), (1, 0));
}

#[test]
fn a_savepoint_takes_no_new_level_and_rollback_to_puts_back_the_modes_named_since() {
    let s = sample(1_000);
    let path = snail("savepoint-modes", &s);
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute("BEGIN").unwrap();
    w.execute("SAVEPOINT a").unwrap();
    let e = w
        .execute("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
        .unwrap_err()
        .to_string();
    assert_eq!(
        e,
        "SET TRANSACTION ISOLATION LEVEL must not be called in a subtransaction"
    );
    w.execute("ROLLBACK TO a").unwrap();
    // The level it runs at, named again, is taken.
    w.execute("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .unwrap();
    w.execute("SET TRANSACTION READ ONLY").unwrap();
    let e = w.execute(&station(1)).unwrap_err().to_string();
    assert!(e.contains("read-only transaction"), "{e}");
    w.execute("ROLLBACK TO a").unwrap();
    w.execute(&station(1)).unwrap();
    w.execute("COMMIT").unwrap();
    assert_eq!(named(&mut fresh(&path), 1), 1);
    for st in ["SAVEPOINT a", "RELEASE a", "ROLLBACK TO a"] {
        let e = w.execute(st).unwrap_err().to_string();
        assert!(e.ends_with("can only be used in transaction blocks"), "{e}");
    }
}

#[test]
fn a_sequences_advance_in_a_savepoint_rolled_back_is_never_given_back() {
    let path = tmp("savepoint-sequence.snail");
    let catalog = warren_snail::catalog::Catalog::parse(
        "CREATE SCHEMA s; CREATE TABLE s.t (id bigint GENERATED ALWAYS AS IDENTITY, v integer NOT NULL, PRIMARY KEY (id));",
    )
    .unwrap();
    write(&path, &catalog, &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute("BEGIN").unwrap();
    w.execute("INSERT INTO s.t (v) VALUES (1)").unwrap();
    w.execute("SAVEPOINT a").unwrap();
    w.execute("INSERT INTO s.t (v) VALUES (2)").unwrap();
    w.execute("ROLLBACK TO a").unwrap();
    w.execute("INSERT INTO s.t (v) VALUES (3)").unwrap();
    w.execute("COMMIT").unwrap();
    let (b, _) = fresh(&path)
        .ask("SELECT t.id, t.v FROM s.t t ORDER BY t.id", None)
        .unwrap();
    assert_eq!(
        bag(&b),
        [
            vec![Some("1".into()), Some("1".into())],
            vec![Some("3".into()), Some("3".into())]
        ]
    );
}

#[test]
fn commit_and_chain_and_rollback_and_chain_begin_the_next_transaction_with_the_same_modes() {
    let s = sample(1_000);
    let path = snail("savepoint-chain", &s);
    let mut w = Snail::open_writing(&path).unwrap();
    let e = w.execute("COMMIT AND CHAIN").unwrap_err().to_string();
    assert_eq!(e, "COMMIT AND CHAIN can only be used in transaction blocks");
    w.execute("BEGIN ISOLATION LEVEL REPEATABLE READ, READ WRITE")
        .unwrap();
    w.execute(&station(1)).unwrap();
    assert_eq!(w.execute("COMMIT AND CHAIN").unwrap().statement, "COMMIT");
    assert_eq!(named(&mut fresh(&path), 1), 1);
    // The chained transaction runs at REPEATABLE READ: it holds its first read's generation.
    let n = count(&mut w, STATIONS);
    Snail::open_writing(&path)
        .unwrap()
        .execute(&station(2))
        .unwrap();
    assert_eq!(count(&mut w, STATIONS), n);
    w.execute(&station(3)).unwrap();
    assert_eq!(
        w.execute("ROLLBACK AND CHAIN").unwrap().statement,
        "ROLLBACK"
    );
    assert_eq!(named(&mut fresh(&path), 3), 0);
    // A failed transaction chained by COMMIT rolls back and chains.
    assert!(w.execute(&station(1)).is_err());
    assert_eq!(
        w.execute("COMMIT AND NO CHAIN").unwrap().statement,
        "ROLLBACK"
    );
    assert!(
        w.execute("COMMIT AND CHAIN").is_err(),
        "no transaction is begun"
    );
    w.execute("BEGIN READ ONLY").unwrap();
    assert!(w.execute(&station(4)).is_err());
    assert_eq!(w.execute("COMMIT AND CHAIN").unwrap().statement, "ROLLBACK");
    let e = w.execute(&station(4)).unwrap_err().to_string();
    assert!(e.contains("read-only transaction"), "{e}");
    w.execute("ROLLBACK").unwrap();
}

#[test]
fn lock_table_holds_off_the_writers_its_mode_conflicts_with_until_the_transaction_ends() {
    let s = sample(1_000);
    let path = snail("savepoint-lock-table", &s);
    let mut w = Snail::open_writing(&path).unwrap();
    let e = w
        .execute("LOCK TABLE weather.stations IN SHARE MODE")
        .unwrap_err()
        .to_string();
    assert_eq!(e, "LOCK TABLE can only be used in transaction blocks");
    w.execute("BEGIN").unwrap();
    let e = w
        .execute("LOCK TABLE weather.stations")
        .unwrap_err()
        .to_string();
    assert!(e.contains("a snail's reader never waits"), "{e}");
    w.execute("ROLLBACK").unwrap();
    w.execute("BEGIN").unwrap();
    w.execute("LOCK TABLE weather.stations IN SHARE MODE")
        .unwrap();
    let later = |st: Vec<String>| {
        let (tx, rx) = mpsc::channel();
        let p = path.clone();
        std::thread::spawn(move || {
            let mut o = Snail::open_writing(&p).unwrap();
            let done: Vec<Result<u64, String>> = st
                .iter()
                .map(|s| o.execute(s).map(|d| d.rows).map_err(|e| e.to_string()))
                .collect();
            tx.send(done).unwrap();
        });
        rx
    };
    // SHARE beside SHARE, and a reader, at once; a writer waits.
    let shared = later(vec![
        "BEGIN".into(),
        "LOCK TABLE weather.stations IN SHARE MODE".into(),
        "COMMIT".into(),
    ]);
    assert!(shared
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .iter()
        .all(|d| d.is_ok()));
    assert!(count(&mut fresh(&path), STATIONS) > 0);
    let nowait = later(vec![
        "BEGIN".into(),
        "LOCK TABLE weather.stations IN ROW EXCLUSIVE MODE NOWAIT".into(),
    ]);
    assert_eq!(
        nowait.recv_timeout(Duration::from_secs(10)).unwrap()[1],
        Err("could not obtain lock on relation \"stations\"".into())
    );
    let writer = later(vec![station(1)]);
    assert!(writer.recv_timeout(Duration::from_millis(1500)).is_err());
    // A writer of another table goes on.
    let other = later(vec![
        "UPDATE weather.readings SET checked_at = NULL WHERE reading_id = -1".into(),
    ]);
    assert!(other.recv_timeout(Duration::from_secs(10)).is_ok());
    w.execute("COMMIT").unwrap();
    assert_eq!(
        writer.recv_timeout(Duration::from_secs(10)).unwrap(),
        [Ok(1)]
    );
    // A lock taken in a savepoint rolled back is let go.
    w.execute("BEGIN").unwrap();
    w.execute("SAVEPOINT a").unwrap();
    w.execute("LOCK TABLE weather.stations IN EXCLUSIVE MODE")
        .unwrap();
    w.execute("ROLLBACK TO a").unwrap();
    let writer = later(vec![station(2)]);
    assert_eq!(
        writer.recv_timeout(Duration::from_secs(10)).unwrap(),
        [Ok(1)]
    );
    w.execute("COMMIT").unwrap();
}

#[test]
fn a_savepoint_across_two_snails_puts_both_back() {
    let (a, b) = (
        tmp("savepoint-across-a.snail"),
        tmp("savepoint-across-b.snail"),
    );
    for (path, ddl) in [
        (
            &a,
            "CREATE SCHEMA a; CREATE TABLE a.t (k integer NOT NULL, PRIMARY KEY (k));",
        ),
        (
            &b,
            "CREATE SCHEMA b; CREATE TABLE b.u (k integer NOT NULL, PRIMARY KEY (k));",
        ),
    ] {
        let catalog = warren_snail::catalog::Catalog::parse(ddl).unwrap();
        write(path, &catalog, &[]).unwrap();
    }
    let mut w = warren_snail::read::Snails::open_writing(&[a.as_path(), b.as_path()]).unwrap();
    w.execute("BEGIN").unwrap();
    w.execute("INSERT INTO a.t (k) VALUES (1)").unwrap();
    w.execute("SAVEPOINT s").unwrap();
    w.execute("INSERT INTO b.u (k) VALUES (1)").unwrap();
    w.execute("INSERT INTO a.t (k) VALUES (2)").unwrap();
    w.execute("ROLLBACK TO s").unwrap();
    w.execute("INSERT INTO b.u (k) VALUES (2)").unwrap();
    w.execute("SAVEPOINT t").unwrap();
    assert!(w.execute("INSERT INTO b.u (k) VALUES (2)").is_err());
    assert!(w.execute("INSERT INTO a.t (k) VALUES (3)").is_err());
    w.execute("ROLLBACK TO t").unwrap();
    w.execute("INSERT INTO a.t (k) VALUES (4)").unwrap();
    w.execute("COMMIT").unwrap();
    drop(w);
    let keys = |path: &std::path::Path, q: &str| {
        let (batch, _) = fresh(path).ask(q, None).unwrap();
        bag(&batch)
            .iter()
            .map(|r| r[0].clone().unwrap().parse::<i64>().unwrap())
            .collect::<Vec<_>>()
    };
    assert_eq!(keys(&a, "SELECT t.k FROM a.t t ORDER BY t.k"), [1, 4]);
    assert_eq!(keys(&b, "SELECT u.k FROM b.u u ORDER BY u.k"), [2]);
    fresh(&a).census().unwrap();
    fresh(&b).census().unwrap();
}

#[test]
fn two_writers_each_waiting_for_the_others_table_fail_one_with_a_deadlock() {
    let s = sample(1_000);
    let path = snail("savepoint-lock-ring", &s);
    let mut a = Snail::open_writing(&path).unwrap();
    a.execute("BEGIN").unwrap();
    a.execute("LOCK TABLE weather.stations IN SHARE ROW EXCLUSIVE MODE")
        .unwrap();
    let (tx, rx) = mpsc::channel();
    let p = path.clone();
    std::thread::spawn(move || {
        let mut b = Snail::open_writing(&p).unwrap();
        b.execute("BEGIN").unwrap();
        b.execute("LOCK TABLE weather.readings IN SHARE ROW EXCLUSIVE MODE")
            .unwrap();
        tx.send(()).unwrap();
        let r = b
            .execute("LOCK TABLE weather.stations IN SHARE ROW EXCLUSIVE MODE")
            .map_err(|e| e.to_string());
        tx.send(()).unwrap();
        let _ = b.execute("ROLLBACK");
        r
    });
    rx.recv_timeout(Duration::from_secs(10)).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let e = a
        .execute("LOCK TABLE weather.readings IN SHARE ROW EXCLUSIVE MODE")
        .unwrap_err()
        .to_string();
    assert!(e.starts_with("deadlock detected"), "{e}");
    a.execute("ROLLBACK").unwrap();
    rx.recv_timeout(Duration::from_secs(10)).unwrap();
}

/// A snail of three tables, a parent, a child referencing it, and one apart.
fn keyed(name: &str) -> std::path::PathBuf {
    let path = tmp(&format!("{name}.snail"));
    let catalog = warren_snail::catalog::Catalog::parse(
        "CREATE SCHEMA s; CREATE TABLE s.p (id integer NOT NULL, PRIMARY KEY (id));
         CREATE TABLE s.c (id integer NOT NULL, r integer REFERENCES s.p, PRIMARY KEY (id));
         CREATE TABLE s.q (id integer NOT NULL, PRIMARY KEY (id));",
    )
    .unwrap();
    write(&path, &catalog, &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute("INSERT INTO s.p (id) VALUES (1), (2), (3)").unwrap();
    path
}

/// Statements run on a writer of their own, on a thread, their outcomes sent back at the end.
fn on_thread(
    path: &std::path::Path,
    st: Vec<String>,
) -> mpsc::Receiver<Vec<Result<u64, String>>> {
    let (tx, rx) = mpsc::channel();
    let p = path.to_path_buf();
    std::thread::spawn(move || {
        let mut o = Snail::open_writing(&p).unwrap();
        let done = st
            .iter()
            .map(|s| o.execute(s).map(|d| d.rows).map_err(|e| e.to_string()))
            .collect();
        let _ = o.execute("ROLLBACK");
        tx.send(done).unwrap();
    });
    rx
}

#[test]
fn a_foreign_keys_check_holds_its_parents_table_in_row_share() {
    let path = keyed("savepoint-fk-share");
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute("BEGIN").unwrap();
    w.execute("LOCK TABLE s.p IN EXCLUSIVE MODE").unwrap();
    let child = on_thread(&path, vec!["INSERT INTO s.c (id, r) VALUES (1, 1)".into()]);
    assert!(child.recv_timeout(Duration::from_millis(1500)).is_err(), "it waits for EXCLUSIVE");
    w.execute("COMMIT").unwrap();
    assert_eq!(child.recv_timeout(Duration::from_secs(10)).unwrap(), [Ok(1)]);
    // SHARE conflicts with no ROW SHARE: the check goes on.
    w.execute("BEGIN").unwrap();
    w.execute("LOCK TABLE s.p IN SHARE MODE").unwrap();
    let child = on_thread(&path, vec!["INSERT INTO s.c (id, r) VALUES (2, 2)".into()]);
    assert_eq!(child.recv_timeout(Duration::from_secs(10)).unwrap(), [Ok(1)]);
    w.execute("COMMIT").unwrap();
}

#[test]
fn writers_waiting_in_a_ring_through_three_tables_locks_fail_one_with_a_deadlock() {
    let path = keyed("savepoint-lock-ring-three");
    let lock = |t: &str| format!("LOCK TABLE s.{t} IN SHARE ROW EXCLUSIVE MODE");
    let mut a = Snail::open_writing(&path).unwrap();
    a.execute("BEGIN").unwrap();
    a.execute(&lock("p")).unwrap();
    let b = on_thread(&path, vec!["BEGIN".into(), lock("c"), lock("p")]);
    std::thread::sleep(Duration::from_millis(300));
    let c = on_thread(&path, vec!["BEGIN".into(), lock("q"), lock("c")]);
    std::thread::sleep(Duration::from_millis(300));
    // a waits for q, held by c, which waits for c's table, held by b, which waits for p, a's.
    let e = a.execute(&lock("q")).unwrap_err().to_string();
    assert!(e.starts_with("deadlock detected"), "{e}");
    a.execute("ROLLBACK").unwrap();
    let b = b.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(b[2], Ok(0), "b takes p once a lets go");
    let c = c.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(c[2], Ok(0), "c takes c's table once b ends");
}

#[test]
fn lock_timeout_and_statement_timeout_cancel_a_wait_in_postgresqls_words() {
    let path = keyed("savepoint-timeouts");
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute("BEGIN").unwrap();
    w.execute("UPDATE s.p SET id = 10 WHERE id = 1").unwrap();
    let other = on_thread(
        &path,
        vec![
            "SET lock_timeout = '200ms'".into(),
            "UPDATE s.p SET id = 20 WHERE id = 1".into(),
            "RESET lock_timeout".into(),
            "SET statement_timeout = 200".into(),
            "UPDATE s.p SET id = 30 WHERE id = 1".into(),
            "BEGIN".into(),
            "SET LOCAL lock_timeout = '100ms'".into(),
            "UPDATE s.p SET id = 40 WHERE id = 1".into(),
            "SELECT 1".into(),
        ],
    );
    let got = other.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(got[1], Err("canceling statement due to lock timeout".into()));
    assert_eq!(got[4], Err("canceling statement due to statement timeout".into()));
    assert_eq!(got[7], Err("canceling statement due to lock timeout".into()));
    assert!(got[8].as_ref().unwrap_err().contains("current transaction is aborted"));
    w.execute("ROLLBACK").unwrap();
}

#[test]
fn prepared_statements_cursors_copy_and_explain_read_across_snails() {
    let (a, b) = (tmp("session-across-a.snail"), tmp("session-across-b.snail"));
    for (path, ddl) in [
        (&a, "CREATE SCHEMA a; CREATE TABLE a.t (k integer NOT NULL, v text, PRIMARY KEY (k));"),
        (&b, "CREATE SCHEMA b; CREATE TABLE b.u (k integer NOT NULL, w text, PRIMARY KEY (k));"),
    ] {
        let catalog = warren_snail::catalog::Catalog::parse(ddl).unwrap();
        write(path, &catalog, &[]).unwrap();
    }
    let mut w = warren_snail::read::Snails::open_writing(&[a.as_path(), b.as_path()]).unwrap();
    w.execute_copy("COPY a.t FROM STDIN (FORMAT csv)", b"1,one\n2,two\n3,three\n")
        .unwrap();
    w.execute("INSERT INTO b.u (k, w) VALUES (1, 'uno'), (2, 'dos')").unwrap();
    w.execute("PREPARE j (integer) AS SELECT t.v, u.w FROM a.t t JOIN b.u u ON u.k = t.k WHERE t.k = $1")
        .unwrap();
    let d = w.execute("EXECUTE j(2)").unwrap();
    assert_eq!(bag(d.answer.as_ref().unwrap()), [vec![Some("two".into()), Some("dos".into())]]);
    // The session's prepared statements are read on one snail at a time: across several, a file's
    // own types would name one OID twice.
    for sql in [
        "SELECT name FROM pg_prepared_statements",
        "SELECT name FROM pg_prepared_statement()",
    ] {
        let e = w.execute(sql).unwrap_err().to_string();
        assert!(e.contains("read on one snail at a time"), "{sql}: {e}");
    }
    w.execute("BEGIN").unwrap();
    w.execute("DECLARE c SCROLL CURSOR FOR SELECT t.k, t.v FROM a.t t ORDER BY t.k")
        .unwrap();
    let d = w.execute("FETCH LAST FROM c").unwrap();
    assert_eq!(bag(d.answer.as_ref().unwrap()), [vec![Some("3".into()), Some("three".into())]]);
    w.execute("FETCH PRIOR FROM c").unwrap();
    w.execute("UPDATE a.t SET v = 'TWO' WHERE CURRENT OF c").unwrap();
    w.execute("COMMIT").unwrap();
    assert!(w.execute("FETCH c").is_err(), "a rollback or a commit closes it");
    let out = w.execute("COPY (SELECT t.k, t.v FROM a.t t ORDER BY t.k) TO STDOUT").unwrap();
    assert_eq!(out.copied.unwrap(), b"1\tone\n2\tTWO\n3\tthree\n");
    let d = w.execute("EXPLAIN (ANALYZE) SELECT t.v, u.w FROM a.t t JOIN b.u u ON u.k = t.k").unwrap();
    assert!(d.rows > 0);
    let d = w.execute("EXPLAIN SELECT u.w FROM b.u u WHERE u.k = 1").unwrap();
    assert!(cell(d.answer.unwrap().column(0).as_ref(), 0).unwrap().starts_with("Snail round along"));
    drop(w);
    // Opened alone, the session's statements as it was handed them.
    let mut one = warren_snail::read::Snails::open_writing(&[a.as_path()]).unwrap();
    one.execute("PREPARE p (bigint) AS SELECT t.v FROM a.t t WHERE t.k = $1;").unwrap();
    let d = one
        .execute("SELECT name, statement, parameter_types, result_types FROM pg_prepared_statements")
        .unwrap();
    assert_eq!(
        one.text_of(d.answer.as_ref().unwrap()).unwrap(),
        [[
            Some("p".to_string()),
            Some("PREPARE p (bigint) AS SELECT t.v FROM a.t t WHERE t.k = $1;".to_string()),
            Some("{bigint}".to_string()),
            Some("{text}".to_string()),
        ]]
    );
    one.execute("DEALLOCATE p").unwrap();
    let d = one.execute("SELECT count(*) FROM pg_prepared_statement()").unwrap();
    assert_eq!(one.text_of(d.answer.as_ref().unwrap()).unwrap(), [[Some("0".to_string())]]);
}
