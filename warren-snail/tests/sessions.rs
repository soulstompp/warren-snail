//! Writers beside writers, as PostgreSQL's READ COMMITTED has them: each statement reads what was
//! committed when it began, with its transaction's own changes; a statement that waited for a block
//! checks the rows it found again, in their newest versions; writers waiting in a ring fail one of
//! their transactions; and a transaction one of whose statements failed takes only its end.
#![cfg(feature = "standalone")]

mod weather;

use std::sync::mpsc;
use std::time::Duration;
use warren_snail::read::Snail;
use weather::*;

fn count(s: &mut Snail, q: &str) -> i64 {
    one(&s.ask(q, None).unwrap().0)
}

/// A writer's statement run on a thread of its own, its result sent back when it ends.
fn later(
    path: &std::path::Path,
    statements: Vec<String>,
) -> mpsc::Receiver<Vec<Result<warren_snail::read::Done, String>>> {
    let (tx, rx) = mpsc::channel();
    let p = path.to_path_buf();
    std::thread::spawn(move || {
        let mut w = Snail::open_writing(&p).unwrap();
        let done = statements
            .iter()
            .map(|st| w.execute(st).map_err(|e| e.to_string()))
            .collect();
        tx.send(done).unwrap();
    });
    rx
}

const ALL: &str = "SELECT count(*) FROM weather.readings r";

#[test]
fn each_statement_of_a_transaction_reads_what_was_committed_when_it_began_with_its_own_changes() {
    let s = sample(5_000);
    let path = snail("sessions-committed", &s);
    let (network, station) = stations_of(&s)[0];
    let reading = |id: i64| {
        format!(
            "INSERT INTO weather.readings (reading_id, network_id, station_no, feed, observed_at, checked_at) \
             VALUES ({id}, {network}, {station}, 'AWS Main', '2022-06-15 10:00:00+00', NULL)"
        )
    };
    let station_ = |no: i64| {
        format!(
            "INSERT INTO weather.stations (network_id, station_no, name) VALUES (9100, {no}, 'Sines {no}')"
        )
    };
    let stations = "SELECT count(*) FROM weather.stations s";
    let n = count(&mut Snail::open(&path).unwrap(), stations);
    let mut one_ = Snail::open_writing(&path).unwrap();
    let mut two = Snail::open_writing(&path).unwrap();
    one_.execute("BEGIN").unwrap();
    one_.execute(&reading(5_000_001)).unwrap();
    assert_eq!(count(&mut one_, ALL), 5_001);
    // Another writer commits a station: the transaction's next statements read it, beside its own
    // rows.
    two.execute(&station_(1)).unwrap();
    // A reading written through that station, as a composed INSERT … SELECT: the statement finds it.
    let d = one_
        .execute(
            "INSERT INTO weather.readings (reading_id, network_id, station_no, feed, observed_at, checked_at) \
             SELECT 5000009, s.network_id, s.station_no, 'AWS Main', '2022-06-15 10:00:00+00', NULL \
             FROM weather.stations s WHERE s.network_id = 9100 AND s.station_no = 1",
        )
        .unwrap();
    assert_eq!(d.rows, 1);
    assert_eq!(count(&mut one_, stations), n + 1);
    one_.execute(&reading(5_000_002)).unwrap();
    assert_eq!(count(&mut one_, ALL), 5_003);
    // Another station committed: the transaction's next read, before any statement of its own, reads it.
    two.execute(&station_(3)).unwrap();
    assert_eq!(count(&mut one_, stations), n + 2);
    one_.execute(&station_(2)).unwrap();
    assert_eq!(count(&mut one_, stations), n + 3);
    // A reader reads only what was committed.
    let mut r = Snail::open(&path).unwrap();
    assert_eq!(
        (count(&mut r, ALL), count(&mut r, stations)),
        (5_000, n + 2)
    );
    one_.execute("COMMIT").unwrap();
    let mut r = Snail::open(&path).unwrap();
    assert_eq!(
        (count(&mut r, ALL), count(&mut r, stations)),
        (5_003, n + 3)
    );
    r.census().unwrap();
}

#[test]
fn a_statement_that_waited_for_a_block_checks_the_rows_it_found_again_in_their_newest_versions() {
    let s = sample(5_000);
    let path = snail("sessions-again", &s);
    let (network, station) = busiest(&s);
    let rows: Vec<i64> = in_order(&s)
        .iter()
        .filter(|r| (r.0, r.1) == (network, station))
        .map(|r| r.2)
        .collect();
    assert!(rows.len() >= 3, "{rows:?}");
    let (a, b) = (rows[0], rows[1]);
    let feed = feed_of(&mut Snail::open(&path).unwrap(), a);
    let mut one_ = Snail::open_writing(&path).unwrap();
    // The first writer, in a transaction, changes a row's feed, deletes another and inserts a row
    // for the same station, holding its block.
    one_.execute("BEGIN").unwrap();
    one_.execute(&format!(
        "UPDATE weather.readings SET feed = 'one' WHERE reading_id = {a}"
    ))
    .unwrap();
    one_.execute(&format!(
        "DELETE FROM weather.readings WHERE reading_id = {b}"
    ))
    .unwrap();
    one_.execute(&format!(
        "INSERT INTO weather.readings (reading_id, network_id, station_no, feed, observed_at, checked_at) \
         VALUES (6000001, {network}, {station}, 'AWS Main', '2022-06-15 10:00:00+00', NULL)"
    ))
    .unwrap();
    // Two more writers' statements wait for that block, each having found its rows first.
    // A row whose feed the first changed, found by its old feed, no longer meets it.
    let gone = later(
        &path,
        vec![format!(
            "UPDATE weather.readings SET feed = 'two' WHERE reading_id = {a} AND feed = '{feed}'"
        )],
    );
    // Of the station's rows, the one deleted is gone, and the one inserted was not there when the
    // statement began.
    let every = later(
        &path,
        vec![format!(
            "UPDATE weather.readings SET checked_at = '2023-01-01 00:00:00+00' \
             WHERE network_id = {network} AND station_no = {station}"
        )],
    );
    std::thread::sleep(Duration::from_millis(400));
    assert!(
        gone.try_recv().is_err() && every.try_recv().is_err(),
        "a writer did not wait"
    );
    one_.execute("COMMIT").unwrap();
    let gone = gone.recv_timeout(Duration::from_secs(10)).unwrap();
    let every = every.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(gone[0].as_ref().unwrap().rows, 0);
    assert_eq!(every[0].as_ref().unwrap().rows, rows.len() as u64 - 1);
    let mut r = Snail::open(&path).unwrap();
    assert_eq!(feed_of(&mut r, a), "one");
    let checked =
        "SELECT count(*) FROM weather.readings r WHERE r.checked_at = '2023-01-01 00:00:00+00'";
    assert_eq!(count(&mut r, checked), rows.len() as i64 - 1);
    assert_eq!(
        count(
            &mut r,
            "SELECT count(*) FROM weather.readings r WHERE r.reading_id = 6000001 \
             AND r.checked_at = '2023-01-01 00:00:00+00'"
        ),
        0
    );
    r.census().unwrap();
}

#[test]
fn writers_waiting_in_a_ring_fail_one_with_a_deadlock_and_the_other_goes_on() {
    let s = sample(20_000);
    let path = snail("sessions-ring", &s);
    let order = in_order(&s);
    // Two rows in blocks far apart on the table's order.
    let (a, b) = (order[10].2, order[19_000].2);
    fn set(id: i64, feed: &str) -> String {
        format!("UPDATE weather.readings SET feed = '{feed}' WHERE reading_id = {id}")
    }
    // The second writer holds b's block in a transaction.
    let (held_tx, held_rx) = mpsc::channel();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let (out_tx, out_rx) = mpsc::channel();
    let p = path.clone();
    std::thread::spawn(move || {
        let mut two = Snail::open_writing(&p).unwrap();
        two.execute("BEGIN").unwrap();
        two.execute(&set(b, "two")).unwrap();
        held_tx.send(()).unwrap();
        go_rx.recv().unwrap();
        // It now waits for a's block: the ring closes, and its transaction fails.
        let ring = two.execute(&set(a, "two")).map_err(|e| e.to_string());
        let after = two.execute(&set(b, "again")).map_err(|e| e.to_string());
        let end = two
            .execute("COMMIT")
            .map(|d| d.statement)
            .map_err(|e| e.to_string());
        out_tx.send((ring, after, end)).unwrap();
    });
    held_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    // The first writer holds a's block, then waits for b's.
    let rx = later(
        &path,
        vec![
            "BEGIN".into(),
            set(a, "one"),
            set(b, "one"),
            "COMMIT".into(),
        ],
    );
    std::thread::sleep(Duration::from_millis(400));
    go_tx.send(()).unwrap();
    let (ring, after, end) = out_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let e = ring.unwrap_err();
    assert!(e.starts_with("deadlock detected"), "{e}");
    let e = after.unwrap_err();
    assert!(e.contains("current transaction is aborted"), "{e}");
    assert_eq!(end, Ok("ROLLBACK"));
    let done = rx.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(done.iter().all(Result::is_ok), "{done:?}");
    let mut r = Snail::open(&path).unwrap();
    assert_eq!(
        (feed_of(&mut r, a), feed_of(&mut r, b)),
        ("one".into(), "one".into())
    );
}

#[test]
fn a_transaction_one_of_whose_statements_failed_takes_only_its_end_which_rolls_it_back() {
    let s = sample(2_000);
    let path = snail("sessions-aborted", &s);
    let id = in_order(&s)[5].2;
    let mut w = Snail::open_writing(&path).unwrap();
    let generation = w.generation();
    w.execute("BEGIN").unwrap();
    w.execute(&format!(
        "UPDATE weather.readings SET feed = 'kept?' WHERE reading_id = {id}"
    ))
    .unwrap();
    assert!(w
        .execute("INSERT INTO weather.stations (network_id, station_no, name) VALUES (1, 1, NULL)")
        .is_err());
    for st in [
        format!("UPDATE weather.readings SET feed = 'x' WHERE reading_id = {id}"),
        "BEGIN".to_string(),
    ] {
        let e = w.execute(&st).unwrap_err().to_string();
        assert!(e.contains("current transaction is aborted"), "{e}");
    }
    assert!(w.ask(ALL, None).is_err());
    let done = w.execute("COMMIT").unwrap();
    assert_eq!((done.statement, done.generation), ("ROLLBACK", generation));
    assert_ne!(feed_of(&mut Snail::open(&path).unwrap(), id), "kept?");
    // The next transaction begins as any does.
    w.execute("BEGIN").unwrap();
    w.execute("ROLLBACK").unwrap();
}

#[test]
fn a_commit_that_fails_lets_go_of_its_blocks() {
    let s = sample(2_000);
    let path = snail("sessions-failed", &s);
    let id = in_order(&s)[5].2;
    let mut w = Snail::open_writing(&path).unwrap();
    w.stop_at(Some(warren_snail::file::Stop::BeforeRoot));
    assert!(w
        .execute(&format!(
            "UPDATE weather.readings SET feed = 'lost' WHERE reading_id = {id}"
        ))
        .is_err());
    // Another writer takes the same block without waiting for it.
    let rx = later(
        &path,
        vec![format!(
            "UPDATE weather.readings SET feed = 'next' WHERE reading_id = {id}"
        )],
    );
    let done = rx.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(done[0].is_ok(), "{done:?}");
    assert_eq!(feed_of(&mut Snail::open(&path).unwrap(), id), "next");
}

#[test]
fn two_writers_giving_an_empty_table_its_first_rows_take_turns_and_both_rows_stand() {
    let s = sample(2_000);
    let path = tmp("sessions-empty.snail");
    let catalog = warren_snail::catalog::Catalog::parse(DDL).unwrap();
    write(
        &path,
        &catalog,
        &[("weather.stations".into(), stations(&s))],
    )
    .unwrap();
    let (n, st) = stations_of(&s)[0];
    let insert = |id: i64| {
        format!(
            "INSERT INTO weather.readings (reading_id, network_id, station_no, feed, observed_at, checked_at) \
             VALUES ({id}, {n}, {st}, 'AWS Main', '2022-06-15 10:00:00+00', NULL)"
        )
    };
    let mut one_ = Snail::open_writing(&path).unwrap();
    one_.execute("BEGIN").unwrap();
    one_.execute(&insert(1)).unwrap();
    let rx = later(&path, vec![insert(2)]);
    std::thread::sleep(Duration::from_millis(400));
    assert!(rx.try_recv().is_err(), "the second writer did not wait");
    one_.execute("COMMIT").unwrap();
    let done = rx.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(done[0].is_ok(), "{done:?}");
    let mut r = Snail::open(&path).unwrap();
    assert_eq!(count(&mut r, ALL), 2);
    r.census().unwrap();
}
