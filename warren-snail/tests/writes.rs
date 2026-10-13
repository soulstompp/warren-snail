//! Rows written into a snail by SQL statements: each answer afterwards as a snail written afresh
//! from the same rows gives it, writers beside writers and readers, and a commit that never
//! finished.

mod weather;

#[cfg(feature = "standalone")]
use std::collections::BTreeMap;
#[cfg(feature = "standalone")]
use warren_snail::read::instant;
use warren_snail::read::Snail;
use weather::*;

#[test]
#[cfg(feature = "standalone")]
fn rows_written_into_a_snail_answer_as_a_snail_written_afresh_from_the_same_rows() {
    let s = sample(20_000);
    let path = snail("writes", &s);
    let mut m = Model::of(&s);
    let mut w = Snail::open_writing(&path).unwrap();
    drop(Snail::open_writing(&path).unwrap());
    let at = |t: &str| instant(t).unwrap();

    // A network's new stations, then a reading written through its station.
    let d = w
        .execute(
            "INSERT INTO weather.stations (network_id, station_no, name) \
             VALUES (9001, 1, 'Sagres 1'), (9001, 2, 'Lisboa Geofísico 9')",
        )
        .unwrap();
    assert_eq!((d.statement, d.rows, d.generation), ("INSERT", 2, 2));
    m.stations.push((9001, 1, "Sagres 1".into()));
    m.stations.push((9001, 2, "Lisboa Geofísico 9".into()));
    let d = w
        .execute(
            "INSERT INTO weather.readings (reading_id, network_id, station_no, feed, observed_at, checked_at) \
             SELECT 2000001, s.network_id, s.station_no, 'AWS Main', '2022-12-24 10:00:00+00', NULL \
             FROM weather.stations s WHERE s.network_id = 9001 AND s.station_no = 1",
        )
        .unwrap();
    assert_eq!(d.rows, 1);
    m.readings.push((
        2000001,
        9001,
        1,
        "AWS Main".into(),
        at("2022-12-24 10:00:00+00"),
        None,
    ));

    // A reading is checked; a station is renamed, moving its row on the name's index; a reading
    // moves to another station, moving it on the table's order; a reading is deleted.
    // An update writes only the orders that hold a column it changes: the check the table's order
    // and the month's index; the feed the table's order alone; the name both of the stations'.
    let waiting = m.readings.iter().position(|r| r.5.is_none()).unwrap();
    let id = m.readings[waiting].0;
    let d = w
        .execute(&format!(
            "UPDATE weather.readings SET checked_at = '2023-01-03 12:00:00+00' WHERE reading_id = {id}"
        ))
        .unwrap();
    assert_eq!(d.orders, 2, "{d:?}");
    m.readings[waiting].5 = Some(at("2023-01-03 12:00:00+00"));
    let d = w
        .execute(&format!(
            "UPDATE weather.readings SET feed = 'AWS Backup' WHERE reading_id = {id}"
        ))
        .unwrap();
    assert_eq!(d.orders, 1, "{d:?}");
    m.readings[waiting].3 = "AWS Backup".into();
    let (n, st, _) = m.stations[100].clone();
    let d = w
        .execute(&format!(
            "UPDATE weather.stations SET name = 'Porto Santo 99' WHERE network_id = {n} AND station_no = {st}"
        ))
        .unwrap();
    assert_eq!(d.orders, 2, "{d:?}");
    m.stations[100].2 = "Porto Santo 99".into();
    let moved = 5000;
    let id = m.readings[moved].0;
    let (n2, st2, _) = m.stations[m.stations.len() / 2].clone();
    let d = w
        .execute(&format!(
            "UPDATE weather.readings SET network_id = {n2}, station_no = {st2} WHERE reading_id = {id}"
        ))
        .unwrap();
    assert_eq!((d.rows, d.orders), (1, 3));
    m.readings[moved].1 = n2;
    m.readings[moved].2 = st2;
    // Each order that moved a row marked it dead and chained it to its replacement; every entry
    // counts the dead rows its page map marks.
    let blocks = w.blocks().unwrap();
    for order in ["r_station", "r_month", "readings_pkey", "s_name"] {
        assert!(
            blocks.iter().any(|x| x.order == order && x.chains > 0),
            "{order} chains nothing"
        );
    }
    assert!(blocks.iter().all(|x| x.dead == x.dead_marked));
    let id = m.readings[7000].0;
    let d = w
        .execute(&format!(
            "DELETE FROM weather.readings WHERE reading_id = {id}"
        ))
        .unwrap();
    assert_eq!(d.rows, 1);
    m.readings.remove(7000);

    // More rows than one last page has room for, all from one station: a part of the block now
    // fills a page, so the commit cuts the block as a block written afresh is cut, and a read
    // narrowed to them prunes nothing.
    let observed = |i: i64| format!("2023-0{}-1{} 08:00:00+00", 1 + i % 9, i % 10);
    let values: Vec<String> = (0..1500)
        .map(|i| {
            format!(
                "({}, 9001, 1, 'observer', '{}', NULL)",
                3_000_000 + i,
                observed(i)
            )
        })
        .collect();
    let d = w
        .execute(&format!(
            "INSERT INTO weather.readings (reading_id, network_id, station_no, feed, observed_at, checked_at) VALUES {}",
            values.join(", ")
        ))
        .unwrap();
    assert!(d.rewritten > 0, "{d:?}");
    for i in 0..1500i64 {
        m.readings.push((
            3_000_000 + i,
            9001,
            1,
            "observer".into(),
            at(&observed(i)),
            None,
        ));
    }
    let q =
        "SELECT r.reading_id FROM weather.readings r WHERE r.network_id = 9001 AND r.station_no = 1";
    let (found, round) = w.ask(q, Some("r_station")).unwrap();
    assert_eq!(
        (found.num_rows(), round.blocks_pruned),
        (1501, 0),
        "{round:?}"
    );

    // A transaction rolled back changes nothing; one committed is one generation.
    let listed = |s: &Snail| -> Vec<(String, u64, u64, u64)> {
        s.blocks()
            .unwrap()
            .into_iter()
            .map(|x| (x.order, x.block, x.rows, x.dead))
            .collect()
    };
    let standing = listed(&w);
    let before = w.execute("BEGIN").unwrap().generation;
    w.execute("DELETE FROM weather.readings WHERE network_id = 9001")
        .unwrap();
    let (gone, _) = w
        .ask(
            "SELECT count(*) FROM weather.readings r WHERE r.network_id = 9001",
            None,
        )
        .unwrap();
    assert_eq!(one(&gone), 0);
    assert_eq!(w.execute("ROLLBACK").unwrap().generation, before);
    assert_eq!(listed(&w), standing);
    w.execute("BEGIN").unwrap();
    let id = m.readings[42].0;
    w.execute(&format!(
        "DELETE FROM weather.readings WHERE reading_id = {id}"
    ))
    .unwrap();
    m.readings.remove(42);
    w.execute(
        "UPDATE weather.stations SET name = 'Faro 0' WHERE network_id = 9001 AND station_no = 2",
    )
    .unwrap();
    m.stations
        .iter_mut()
        .filter(|x| (x.0, x.1) == (9001, 2))
        .for_each(|x| x.2 = "Faro 0".into());
    assert_eq!(w.execute("COMMIT").unwrap().generation, before + 1);

    // A unique key is kept unique, a block's first key among them: the key finds it in the block
    // it begins.
    let e = w
        .execute("INSERT INTO weather.stations (network_id, station_no, name) VALUES (9001, 1, 'Faro 1')")
        .unwrap_err()
        .to_string();
    assert!(e.contains("unique"), "{e}");
    let first = blocks_first(&w, "stations_pkey", 2);
    let e = w
        .execute(&format!(
            "INSERT INTO weather.stations (network_id, station_no, name) VALUES ({}, {}, 'Faro 1')",
            first.0, first.1
        ))
        .unwrap_err()
        .to_string();
    assert!(e.contains("unique"), "{e}");

    let join =
        "JOIN weather.stations s ON s.network_id = r.network_id AND s.station_no = r.station_no";
    let questions: Vec<String> = vec![
        "SELECT count(*) FROM weather.readings r".to_string(),
        "SELECT r.feed, count(*) FROM weather.readings r GROUP BY r.feed".into(),
        format!("SELECT {MONTH} AS m, count(*) FROM weather.readings r WHERE {SPAN} GROUP BY 1"),
        format!("SELECT count(*) FROM weather.readings r WHERE {MONTH} = 12 AND {SPAN}"),
        format!("SELECT r.checked_at FROM weather.readings r WHERE {MONTH} = 12 AND {SPAN}"),
        "SELECT r.reading_id, r.station_no, r.checked_at FROM weather.readings r WHERE r.network_id = 9001".into(),
        format!("SELECT r.reading_id, r.feed FROM weather.readings r WHERE r.network_id = {n} AND r.station_no = {st}"),
        format!("SELECT s.station_no, s.name FROM weather.stations s WHERE s.network_id = {n}"),
        format!("SELECT r.reading_id, r.feed FROM weather.readings r WHERE r.network_id = {n2} AND r.station_no = {st2}"),
        "SELECT s.network_id, s.station_no FROM weather.stations s WHERE s.name = 'Porto Santo 99'".into(),
        format!(
            "SELECT r.reading_id, r.checked_at FROM weather.readings r WHERE {MONTH} = 12 \
             AND r.observed_at >= '2022-12-24 00:00:00+00' AND r.observed_at < '2022-12-25 00:00:00+00'"
        ),
        "SELECT s.name, count(*) FROM weather.stations s GROUP BY s.name".into(),
        "SELECT count(*) FROM weather.stations s WHERE s.name >= 'Lisboa' AND s.name < 'Lisbon'".into(),
        "SELECT r.reading_id FROM weather.readings r WHERE r.reading_id >= 2000000".into(),
        format!("SELECT s.name, count(*) FROM weather.readings r {join} WHERE {SPAN} GROUP BY s.name"),
    ];

    let afresh = m.written("writes-afresh");
    // Every order holds the live rows' counts, NULLs and text bytes a snail written afresh holds.
    let totals = |s: &Snail| {
        let mut t: BTreeMap<(String, String), (u64, u64, u64)> = Default::default();
        for x in s.blocks().unwrap() {
            let e = t.entry((x.order.clone(), x.column.clone())).or_default();
            e.0 += x.rows;
            e.1 += x.nulls;
            e.2 += x.bytes.unwrap_or(0);
        }
        t
    };
    assert_eq!(totals(&w), totals(&Snail::open(&afresh).unwrap()));
    same_answers(&mut w, &mut Snail::open(&afresh).unwrap(), &questions);
    drop(w);
    Snail::open(&path).unwrap().census().unwrap();
    // The file as committed, read by a snail opened after.
    same_answers(
        &mut Snail::open(&path).unwrap(),
        &mut Snail::open(&afresh).unwrap(),
        &questions,
    );
}

#[test]
#[cfg(not(feature = "standalone"))]
fn a_snail_built_for_postgresql_refuses_every_write() {
    let s = sample(1_000);
    let path = snail("refused", &s);
    let e = Snail::open_writing(&path).err().unwrap().to_string();
    assert!(e.contains("writes nothing"), "{e}");
}

#[test]
#[cfg(feature = "standalone")]
fn a_table_given_no_rows_is_written_empty_and_takes_rows() {
    let s = sample(2_000);
    let path = tmp("no-readings.snail");
    let catalog = warren_snail::catalog::Catalog::parse(DDL).unwrap();
    let written = write(
        &path,
        &catalog,
        &[("weather.stations".into(), stations(&s))],
    )
    .unwrap();
    let empty: Vec<&str> = written
        .iter()
        .filter(|w| w.rows == 0)
        .map(|w| w.order.as_str())
        .collect();
    assert_eq!(empty, ["r_station", "readings_pkey", "r_month"]);
    let (n, st) = stations_of(&s)[0];
    let mut w = Snail::open_writing(&path).unwrap();
    // Each count asked along every order that holds its columns.
    let counts = [
        (
            format!("SELECT count(*) FROM weather.readings r WHERE {MONTH} = 12"),
            ["r_station", "r_month"],
        ),
        (
            "SELECT count(*) FROM weather.readings r WHERE r.reading_id >= 0".to_string(),
            ["r_station", "readings_pkey"],
        ),
    ];
    for (q, orders) in &counts {
        for order in orders {
            assert_eq!(one(&w.ask(q, Some(order)).unwrap().0), 0, "{q} on {order}");
        }
    }
    w.execute(&format!(
        "INSERT INTO weather.readings (reading_id, network_id, station_no, feed, observed_at, checked_at) \
         VALUES (1, {n}, {st}, 'AWS Main', '2022-12-24 06:00:00+00', NULL), \
         (2, {n}, {st}, 'observer', '2022-11-02 18:00:00+00', '2022-11-03 09:00:00+00')"
    ))
    .unwrap();
    for ((q, orders), want) in counts.iter().zip([1, 2]) {
        for order in orders {
            assert_eq!(
                one(&w.ask(q, Some(order)).unwrap().0),
                want,
                "{q} on {order}"
            );
        }
    }
    let (b, _) = w
        .ask(
            &format!(
                "SELECT r.reading_id, r.feed FROM weather.readings r \
                 WHERE r.network_id = {n} AND r.station_no = {st}"
            ),
            None,
        )
        .unwrap();
    assert_eq!(
        bag(&b),
        [
            vec![Some("1".into()), Some("AWS Main".into())],
            vec![Some("2".into()), Some("observer".into())]
        ]
    );
}

#[test]
fn a_write_refused_leaves_no_file_behind() {
    let mut s = sample(2_000);
    s.feed[1_500] = "x".repeat(33);
    let path = tmp("refused-write.snail");
    let catalog = warren_snail::catalog::Catalog::parse(DDL).unwrap();
    // In files of 8 pages, the stations written before the refusal pass into further files.
    let e = warren_snail::write::write_spanned(&path, &catalog, &tables(&s), &[], 8)
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("is varchar(32) and a row holds 33 characters"),
        "{e}"
    );
    let dir = path.parent().unwrap();
    let left: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|f| f.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|f| f.starts_with("refused-write.snail"))
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

#[test]
#[cfg(feature = "standalone")]
fn a_commit_stopped_short_of_its_root_leaves_the_file_as_it_was() {
    use warren_snail::file::Stop;
    let s = sample(5_000);
    let count = |path: &std::path::Path| -> i64 {
        let (b, _) = Snail::open(path)
            .unwrap()
            .ask("SELECT count(*) FROM weather.readings r", None)
            .unwrap();
        one(&b)
    };
    let gone = s.network_id.iter().filter(|&&n| n < 500).count() as i64;
    for stop in [Stop::BeforeRoot, Stop::HalfRoot] {
        let path = snail(&format!("stopped-{stop:?}"), &s);
        let mut w = Snail::open_writing(&path).unwrap();
        w.stop_at(Some(stop));
        let e = w
            .execute("DELETE FROM weather.readings WHERE network_id < 500")
            .unwrap_err()
            .to_string();
        assert!(e.contains("stopped"), "{e}");
        drop(w);
        assert_eq!(count(&path), 5_000, "{stop:?}");
        let mut w = Snail::open_writing(&path).unwrap();
        let d = w
            .execute("DELETE FROM weather.readings WHERE network_id < 500")
            .unwrap();
        assert_eq!((d.rows as i64, d.generation), (gone, 2), "{stop:?}");
        drop(w);
        assert_eq!(count(&path), 5_000 - gone, "{stop:?}");
    }
}

#[test]
#[cfg(feature = "standalone")]
fn a_round_reads_the_file_as_it_stood_when_the_round_began() {
    let s = sample(5_000);
    let path = snail("as-it-stood", &s);
    let q = "SELECT count(*) FROM weather.readings r";
    let mut before = Snail::open(&path).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    let d = w
        .execute("DELETE FROM weather.readings WHERE network_id < 500")
        .unwrap();
    // Opened before the write, its first round reads the generation it opened on; the next reads
    // the one written since.
    assert_eq!(one(&before.ask(q, None).unwrap().0), 5_000);
    assert_eq!(one(&before.ask(q, None).unwrap().0), 5_000 - d.rows as i64);
    assert_eq!(one(&w.ask(q, None).unwrap().0), 5_000 - d.rows as i64);
}

#[test]
#[cfg(feature = "standalone")]
fn writers_of_different_blocks_write_side_by_side_and_both_commits_hold() {
    let s = sample(20_000);
    let path = snail("side-by-side", &s);
    let order = in_order(&s);
    let (a, b) = (order[10].2, order[order.len() - 10].2);
    let mut one = Snail::open_writing(&path).unwrap();
    let mut two = Snail::open_writing(&path).unwrap();
    one.execute("BEGIN").unwrap();
    one.execute(&format!(
        "UPDATE weather.readings SET feed = 'uno' WHERE reading_id = {a}"
    ))
    .unwrap();
    // The transaction writes the block it staged again.
    one.execute(&format!(
        "UPDATE weather.readings SET feed = 'one' WHERE reading_id = {a}"
    ))
    .unwrap();
    // The second writer's block is not the first's: it commits while the first holds its own.
    let d = two
        .execute(&format!(
            "UPDATE weather.readings SET feed = 'two' WHERE reading_id = {b}"
        ))
        .unwrap();
    assert_eq!(d.generation, 2);
    let d = one.execute("COMMIT").unwrap();
    assert_eq!(d.generation, 3);
    let mut r = Snail::open(&path).unwrap();
    assert_eq!(
        (feed_of(&mut r, a), feed_of(&mut r, b)),
        ("one".into(), "two".into())
    );
    r.census().unwrap();
}

#[test]
#[cfg(feature = "standalone")]
fn a_writer_of_a_block_another_writer_holds_waits_its_turn_and_a_reader_never_waits() {
    let s = sample(20_000);
    let path = snail("turns", &s);
    let order = in_order(&s);
    let (a, a2) = (order[10].2, order[11].2);
    let before = feed_of(&mut Snail::open(&path).unwrap(), a2);
    let mut one = Snail::open_writing(&path).unwrap();
    one.execute("BEGIN").unwrap();
    one.execute(&format!(
        "UPDATE weather.readings SET feed = 'one' WHERE reading_id = {a}"
    ))
    .unwrap();
    let p = path.clone();
    let waiter = std::thread::spawn(move || {
        let mut two = Snail::open_writing(&p).unwrap();
        let t = std::time::Instant::now();
        two.execute(&format!(
            "UPDATE weather.readings SET feed = 'two' WHERE reading_id = {a2}"
        ))
        .unwrap();
        t.elapsed()
    });
    // A reader reads the block the first writer holds, at once, as it stood.
    let p = path.clone();
    let reader = std::thread::spawn(move || {
        let t = std::time::Instant::now();
        let got = feed_of(&mut Snail::open(&p).unwrap(), a2);
        (got, t.elapsed())
    });
    let (got, read_in) = reader.join().unwrap();
    assert_eq!(got, before);
    assert!(
        read_in < std::time::Duration::from_millis(250),
        "{read_in:?}"
    );
    std::thread::sleep(std::time::Duration::from_millis(400));
    one.execute("COMMIT").unwrap();
    let waited = waiter.join().unwrap();
    assert!(
        waited >= std::time::Duration::from_millis(300),
        "{waited:?}"
    );
    let mut r = Snail::open(&path).unwrap();
    assert_eq!(
        (feed_of(&mut r, a), feed_of(&mut r, a2)),
        ("one".into(), "two".into())
    );
}

#[test]
#[cfg(feature = "standalone")]
fn pages_a_running_round_names_are_never_freed_and_are_reused_once_it_ends() {
    let s = sample(20_000);
    let path = snail("freed", &s);
    let order = in_order(&s);
    let id = order[10].2;
    let q = format!(
        "SELECT r.reading_id, r.feed FROM weather.readings r WHERE r.network_id = {} AND r.station_no = {}",
        order[10].0, order[10].1
    );
    let set = |i: u32| format!("UPDATE weather.readings SET feed = 'f{i}' WHERE reading_id = {id}");
    let mut held = Snail::open(&path).unwrap();
    // The pages free when the snail was written: none in one file, the pages passed over at each
    // file's end in several.
    let free = held.census().unwrap().free;
    let mut w = Snail::open_writing(&path).unwrap();
    let (first, _) = w.ask(&q, None).unwrap();
    let mut reused = 0;
    for i in 0..20 {
        reused += w.execute(&set(i)).unwrap().pages_reused;
    }
    assert!(
        reused <= free,
        "a round holds the first generation: {reused} pages reused, {free} free before it"
    );
    // Its first round reads the generation it opened on, every page still as it was.
    let (got, _) = held.ask(&q, None).unwrap();
    assert_eq!(bag(&got), bag(&first));
    drop(held);
    let len = snail_bytes(&path);
    for i in 20..40 {
        reused += w.execute(&set(i)).unwrap().pages_reused;
    }
    assert!(reused > 0);
    assert!(
        snail_bytes(&path) < len + 20 * 8 * 4096,
        "the file grows less once freed pages are reused"
    );
    // A snail that holds the pages it read, and reads again after its pages were freed and reused,
    // drops the ones reused: it reads them as they are now.
    let mut reading = Snail::open(&path).unwrap();
    assert_eq!(feed_of(&mut reading, id), "f39");
    for i in 40..43 {
        w.execute(&set(i)).unwrap();
    }
    // Its round moves to the newest generation and holds that one alone, so the pages it no longer
    // reads are freed and reused.
    assert_eq!(feed_of(&mut reading, id), "f42");
    // Read after every commit, it drops exactly the pages each commit reused, as the root lists
    // them, and keeps the rest.
    let mut later = 0;
    for i in 43..63 {
        later += w.execute(&set(i)).unwrap().pages_reused;
        assert_eq!(feed_of(&mut reading, id), format!("f{i}"));
    }
    assert!(later > 0);
    let mut r = Snail::open(&path).unwrap();
    let c = r.census().unwrap();
    assert!(c.free + c.waiting > 0, "{c:?}");
    assert_eq!(feed_of(&mut r, id), "f62");
    let (all, _) = r
        .ask("SELECT count(*) FROM weather.readings r", None)
        .unwrap();
    assert_eq!(one(&all), 20_000);
}

#[test]
#[cfg(feature = "standalone")]
fn a_program_killed_while_it_holds_a_round_lets_its_pages_go() {
    use std::io::BufRead;
    let s = sample(5_000);
    let path = snail("killed", &s);
    let id = in_order(&s)[10].2;
    // The pages free when the snail was written: none in one file, the pages passed over at each
    // file's end in several.
    let free = Snail::open(&path).unwrap().census().unwrap().free;
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_warren-snail"))
        .args(["hold", path.to_str().unwrap(), "--seconds", "60"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    std::io::BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line.trim(), "holding generation 1");
    let mut w = Snail::open_writing(&path).unwrap();
    let mut write = |i: i64| {
        w.execute(&format!(
            "UPDATE weather.readings SET feed = 'k{i}' WHERE reading_id = {id}"
        ))
        .unwrap()
        .pages_reused
    };
    // While the program holds its round, no page a commit frees is reused.
    let held: u64 = (0..5).map(&mut write).sum();
    assert!(held <= free, "{held} pages reused, {free} free before");
    child.kill().unwrap();
    child.wait().unwrap();
    // Once it is killed, they are.
    let after: u64 = (5..10).map(&mut write).sum();
    assert!(
        held + after > free,
        "{held} and {after} reused, {free} free before"
    );
}

#[test]
#[cfg(feature = "standalone")]
fn a_row_written_after_a_blocks_last_lies_in_a_tail_of_one_disk_page_and_the_pages_before_stay() {
    let s = sample(20_000);
    let path = snail("tail", &s);
    // Each column of each block of the readings: its counting unit, its data pages and their disk
    // pages.
    let listed = |s: &Snail| -> BTreeMap<(String, u64, String), (u64, u64, u64)> {
        s.blocks()
            .unwrap()
            .into_iter()
            .filter(|b| b.table == "weather.readings")
            .map(|b| {
                (
                    (b.order, b.block, b.column),
                    (b.k, b.data_pages, b.disk_pages),
                )
            })
            .collect()
    };
    let before = listed(&Snail::open(&path).unwrap());
    let (network, station) = busiest(&s);
    let mut w = Snail::open_writing(&path).unwrap();
    let insert = |w: &mut Snail, id: i64| {
        w.execute(&format!(
            "INSERT INTO weather.readings (reading_id, network_id, station_no, feed, observed_at, checked_at) \
             VALUES ({id}, {network}, {station}, 'AWS Main', '2022-06-15 10:00:00+00', NULL)"
        ))
        .unwrap()
    };
    // One row: each column of the block it lands in, on every order, gains one data page of one disk
    // page, whatever its counting unit, and every page before it stays as it lay.
    let d = insert(&mut w, 3_000_001);
    assert_eq!((d.rows, d.rewritten), (1, 0), "{d:?}");
    let once = listed(&w);
    assert_eq!(once.len(), before.len());
    let grown: Vec<_> = once
        .iter()
        .filter(|(at, now)| before[*at] != **now)
        .collect();
    assert!(!grown.is_empty());
    for (at, now) in &grown {
        let was = before[*at];
        assert_eq!(
            (now.0, now.1, now.2),
            (was.0, was.1 + 1, was.2 + 1),
            "{at:?}"
        );
    }
    let orders: std::collections::BTreeSet<&String> = grown.iter().map(|(at, _)| &at.0).collect();
    assert_eq!(orders.len(), 3, "{grown:?}");
    assert!(grown.iter().any(|(_, now)| now.0 > 1), "{grown:?}");
    // A second row goes into the same tail page, written afresh: no page more.
    insert(&mut w, 3_000_002);
    assert_eq!(listed(&w), once);
    let q = format!(
        "SELECT r.reading_id FROM weather.readings r \
         WHERE r.network_id = {network} AND r.station_no = {station}"
    );
    let mut r = fresh(&path);
    let (b, round) = r.ask(&q, None).unwrap();
    exact(&q, &round);
    let ids = ints64(b.column(0).as_ref());
    assert!(ids.contains(&Some(3_000_001)) && ids.contains(&Some(3_000_002)));
}

#[test]
#[cfg(feature = "standalone")]
fn a_block_statements_change_may_lie_in_two_files_and_a_block_written_whole_lies_in_one() {
    let s = sample(5_000);
    let path = snail_spanned("files-writes", &s, 64);
    let order = in_order(&s);
    let mut w = Snail::open_writing(&path).unwrap();
    // Readings checked one by one across the stations: each commit writes its pages where a free run
    // or the last file has room, and frees the pages it replaced.
    for (i, (_, _, id)) in order.iter().step_by(125).enumerate() {
        w.execute(&format!(
            "UPDATE weather.readings SET feed = 'f{i}' WHERE reading_id = {id}"
        ))
        .unwrap();
    }
    let files = |s: &Snail| -> Vec<u64> { s.blocks().unwrap().iter().map(|b| b.files).collect() };
    assert!(files(&w).iter().any(|&f| f > 1), "{:?}", files(&w));
    let questions = [
        (
            "SELECT r.feed, count(*) FROM weather.readings r GROUP BY r.feed",
            "r_station",
        ),
        (
            "SELECT r.observed_at, r.checked_at FROM weather.readings r \
             WHERE r.observed_at >= '2000-01-01 00:00:00+00'",
            "r_month",
        ),
        (
            "SELECT r.reading_id FROM weather.readings r WHERE r.reading_id > 0",
            "readings_pkey",
        ),
    ];
    let mut r = fresh(&path);
    let asked: Vec<_> = questions
        .iter()
        .map(|(q, o)| {
            let (b, round) = r.ask(q, Some(o)).unwrap();
            exact(q, &round);
            bag(&b)
        })
        .collect();
    // Read whole along each order, the blocks the statements changed are pruned: each written whole,
    // in one run, in one file.
    for ((q, o), want) in questions.iter().zip(&asked) {
        let (b, _) = w.ask(q, Some(o)).unwrap();
        assert_eq!(&bag(&b), want, "{q}");
    }
    let readings: Vec<u64> = w
        .blocks()
        .unwrap()
        .iter()
        .filter(|b| b.table == "weather.readings")
        .map(|b| b.files)
        .collect();
    assert!(readings.iter().all(|&f| f == 1), "{readings:?}");
    for ((q, o), want) in questions.iter().zip(&asked) {
        let (b, round) = fresh(&path).ask(q, Some(o)).unwrap();
        exact(q, &round);
        assert_eq!(&bag(&b), want, "{q}");
    }
}

/// Every block's entry of every order, read from the snail's directory as it stands.
#[cfg(feature = "standalone")]
fn block_entries(path: &std::path::Path) -> Vec<warren_snail::directory::Entry> {
    use warren_snail::directory::{decode_page, decode_top};
    let reader = warren_snail::file::Reader::open(path).unwrap();
    let root = reader.root().unwrap();
    let mut out = Vec::new();
    for (_, _, top) in &root.orders {
        let (head, mut level) = decode_top(&reader.read_extent(*top).unwrap()).unwrap();
        for _ in 1..head.height {
            level = level
                .iter()
                .flat_map(|e| decode_page(&reader.read_extent(e.child).unwrap(), &head).unwrap())
                .collect();
        }
        out.extend(level);
    }
    out
}

#[test]
#[cfg(feature = "standalone")]
fn a_snail_of_format_seven_is_read_and_written_as_0_1_0_lays_it_and_stays_of_format_seven() {
    let s = sample(5_000);
    let catalog = warren_snail::catalog::Catalog::parse(DDL).unwrap();
    let seven = tmp("seven.snail");
    warren_snail::write::write_format_seven(&seven, &catalog, &tables(&s), span()).unwrap();
    let eight = snail("eight", &s);
    let order = in_order(&s);
    let (network, station, id) = order[100];
    let questions: Vec<String> = vec![
        "SELECT r.feed, count(*) FROM weather.readings r GROUP BY r.feed".into(),
        format!(
            "SELECT r.reading_id, r.feed FROM weather.readings r \
             WHERE r.network_id = {network} AND r.station_no = {station}"
        ),
        "SELECT s.name, count(*) FROM weather.stations s GROUP BY s.name".into(),
        format!("SELECT r.feed FROM weather.readings r WHERE r.reading_id = {id}"),
    ];
    same_answers(&mut fresh(&seven), &mut fresh(&eight), &questions);
    // The same statements into both: inserted, checked, moved and deleted rows.
    let (w7, w8) = (
        &mut Snail::open_writing(&seven).unwrap(),
        &mut Snail::open_writing(&eight).unwrap(),
    );
    for st in [
        format!(
            "INSERT INTO weather.readings (reading_id, network_id, station_no, feed, observed_at, checked_at) \
             VALUES (4000001, {network}, {station}, 'AWS Main', '2022-06-15 10:00:00+00', NULL)"
        ),
        format!("UPDATE weather.readings SET feed = 'f7' WHERE reading_id = {id}"),
        format!(
            "UPDATE weather.readings SET station_no = {} WHERE reading_id = {}",
            order[3000].1, order[200].2
        ),
        format!("DELETE FROM weather.readings WHERE reading_id = {}", order[400].2),
    ] {
        w7.execute(&st).unwrap();
        w8.execute(&st).unwrap();
    }
    for q in &questions {
        w7.ask(q, None).unwrap();
        w8.ask(q, None).unwrap();
    }
    same_answers(&mut fresh(&seven), &mut fresh(&eight), &questions);
    Snail::open(&seven).unwrap().census().unwrap();
    Snail::open(&eight).unwrap().census().unwrap();
    // The format 7 snail still names its format, and every block's page map begins a run of its
    // own, as 0.1.0 reads it; this build's own packs them.
    let reader = warren_snail::file::Reader::open(&seven).unwrap();
    reader.root().unwrap();
    assert_eq!(reader.format(), 7);
    assert!(block_entries(&seven)
        .iter()
        .all(|e| e.child.bytes >> 32 == 0));
    assert!(block_entries(&eight)
        .iter()
        .any(|e| e.child.bytes >> 32 > 0));
}
