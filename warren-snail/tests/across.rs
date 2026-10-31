//! Several snails opened together, the weather's stations in one and its readings in another: a
//! join across them answered as one snail holding both answers it, each snail read its predicted
//! pages; every root read twice, taken where none changed; a name two snails hold refused; a
//! transaction writing both, one root deciding, and a crash between the roots leaving each snail
//! whole and the round across them all or nothing; writers waiting in a ring across the files.

mod weather;

use std::path::{Path, PathBuf};
use warren_snail::catalog::Catalog;
use warren_snail::read::{Round, Snails};
use weather::*;

/// The stations alone, and the readings alone, each as the weather's DDL declares it.
const STATIONS: &str = "
CREATE TABLE weather.stations (
    network_id integer NOT NULL,
    station_no integer NOT NULL,
    name character varying(80) NOT NULL,
    PRIMARY KEY (network_id, station_no)
);
CREATE INDEX s_name ON weather.stations (name, network_id, station_no);
";

const READINGS: &str = "
CREATE FUNCTION weather.clock(timestamptz) RETURNS timestamp
    LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$ SELECT $1 AT TIME ZONE 'UTC' $$;
CREATE TABLE weather.readings (
    reading_id bigint NOT NULL,
    network_id integer NOT NULL,
    station_no integer NOT NULL,
    feed character varying(32) NOT NULL,
    observed_at timestamptz NOT NULL,
    checked_at timestamptz,
    PRIMARY KEY (reading_id)
);
CREATE INDEX r_station ON weather.readings (network_id, station_no);
ALTER TABLE weather.readings CLUSTER ON r_station;
CREATE INDEX r_month ON weather.readings
    ((extract(month FROM weather.clock(observed_at))::smallint), observed_at) INCLUDE (checked_at);
";

/// The sample's stations in one snail and its readings in another, and both in a third.
struct Split {
    stations: PathBuf,
    readings: PathBuf,
    both: PathBuf,
}

fn split(name: &str, s: &Sample) -> Split {
    let one = |file: &str, ddl: &str, table: &str, rows: Rows| {
        let path = tmp(&format!("{name}-{file}.snail"));
        write(
            &path,
            &Catalog::parse(ddl).unwrap(),
            &[(table.into(), rows)],
        )
        .unwrap();
        path
    };
    Split {
        stations: one("stations", STATIONS, "weather.stations", stations(s)),
        readings: one("readings", READINGS, "weather.readings", readings(s)),
        both: snail(&format!("{name}-both"), s),
    }
}

use warren_snail::write::Rows;

fn paths(s: &Split) -> [&Path; 2] {
    [&s.stations, &s.readings]
}

/// The snails opened together, holding no page between rounds.
fn together(s: &Split) -> Snails {
    #[cfg(feature = "standalone")]
    return Snails::open_holding(&paths(s), 0).unwrap();
    #[cfg(not(feature = "standalone"))]
    Snails::open(&paths(s)).unwrap()
}

/// Every snail's part of a round across them read its predicted pages, less those held.
fn each_exact(q: &str, round: &Round) {
    assert_eq!(round.snails.len(), 2, "{q}: {round:?}");
    for part in &round.snails {
        assert_eq!(
            part.column_pages_read,
            part.column_pages_predicted - part.column_pages_held,
            "{q}: {round:?}"
        );
    }
    exact(q, round);
}

/// A station's readings, joined across the two.
fn joined(network: i64, station: i64) -> String {
    format!(
        "SELECT count(*) FROM weather.stations s JOIN weather.readings r \
         ON r.network_id = s.network_id AND r.station_no = s.station_no \
         WHERE s.network_id = {network} AND s.station_no = {station}"
    )
}

#[test]
fn a_join_across_two_snails_answers_as_one_snail_holding_both_and_each_reads_its_predicted_pages() {
    let s = sample(20_000);
    let split = split("across-joins", &s);
    let mut both = fresh(&split.both);
    let mut across = together(&split);
    let (network, station) = busiest(&s);
    let name = commonest_name(&s);
    let questions = [
        joined(network, station),
        format!(
            "SELECT s.name, count(*) FROM weather.stations s JOIN weather.readings r \
             ON r.network_id = s.network_id AND r.station_no = s.station_no \
             WHERE s.network_id = {network} GROUP BY s.name"
        ),
        "SELECT r.reading_id, s.name FROM weather.readings r JOIN weather.stations s \
         ON s.network_id = r.network_id AND s.station_no = r.station_no \
         WHERE r.observed_at >= '2022-03-01 00:00:00+00' AND r.observed_at < '2022-03-02 00:00:00+00'"
            .to_string(),
        format!(
            "SELECT r.feed, count(*) FROM weather.stations s JOIN weather.readings r \
             ON r.network_id = s.network_id AND r.station_no = s.station_no \
             WHERE s.name = '{name}' GROUP BY r.feed ORDER BY r.feed"
        ),
        "SELECT s.network_id, s.station_no, r.reading_id FROM weather.stations s \
         LEFT JOIN weather.readings r ON r.network_id = s.network_id AND r.station_no = s.station_no \
         AND r.feed = 'AWS Backup' WHERE s.network_id < 40"
            .to_string(),
    ];
    for q in &questions {
        let (want, alone) = both.ask(q, None).unwrap_or_else(|e| panic!("{q}: {e}"));
        let (got, round) = across.ask(q).unwrap_or_else(|e| panic!("{q}: {e}"));
        assert_eq!(bag(&got), bag(&want), "{q}");
        assert!(want.num_rows() > 0, "{q}");
        // Each table read along the same order, its pages as one snail reads them.
        assert_eq!(round.order, alone.order, "{q}");
        assert_eq!(
            round.column_pages_predicted, alone.column_pages_predicted,
            "{q}"
        );
        each_exact(q, &round);
        assert!(
            round.snails.iter().all(|p| p.column_pages_predicted > 0),
            "{q}: {round:?}"
        );
        // Every root read twice: two pages a copy pair, two snails.
        assert_eq!(round.pages_at_start, 8, "{q}: {round:?}");
        assert!(round.snails.iter().all(|p| p.pages_at_start == 4));
    }
}

#[test]
fn a_question_on_one_snail_s_tables_is_read_as_that_snail_reads_it() {
    let s = sample(5_000);
    let split = split("across-one", &s);
    let mut alone = fresh(&split.readings);
    let mut across = together(&split);
    let q = format!("SELECT count(*), max(r.observed_at) FROM weather.readings r WHERE {SPAN}");
    let (want, r1) = alone.ask(&q, None).unwrap();
    let (got, r2) = across.ask(&q).unwrap();
    assert_eq!(bag(&got), bag(&want));
    assert_eq!(
        (r2.order.as_str(), r2.column_pages_predicted),
        (r1.order.as_str(), r1.column_pages_predicted)
    );
    // Its part is the readings' snail's; the stations' read only its root, twice.
    assert_eq!(
        r2.snails[1].column_pages_predicted,
        r1.column_pages_predicted
    );
    assert_eq!(
        (r2.snails[0].pages_at_start, r2.snails[0].column_pages_read),
        (4, 0)
    );
    // One snail opened alone reads its root once.
    #[cfg(feature = "standalone")]
    let mut one = Snails::open_holding(&[&split.readings], 0).unwrap();
    #[cfg(not(feature = "standalone"))]
    let mut one = Snails::open(&[&split.readings]).unwrap();
    let (_, r3) = one.ask(&q).unwrap();
    assert_eq!(r3.pages_at_start, 2);
    assert!(r3.snails.is_empty());
}

/// Snails opened apart and read together, on the roots they took at one moment, answer a join as
/// the snails opened together do, each table read along the same order in the same pages; and the
/// join priced from their directories alone reads no column page: the table read first exactly as
/// the round reads it, each table kept to its keys between the fewest pages and the most.
#[test]
fn snails_read_together_answer_as_snails_opened_together_and_their_join_is_priced_within_its_bounds(
) {
    use warren_snail::read::{answer_together, at_one_moment, price_together};
    let s = sample(20_000);
    let split = split("together-joins", &s);
    let (network, station) = busiest(&s);
    let name = commonest_name(&s);
    let questions = [
        joined(network, station),
        format!(
            "SELECT s.name, count(*) FROM weather.stations s JOIN weather.readings r \
             ON r.network_id = s.network_id AND r.station_no = s.station_no \
             WHERE s.network_id = {network} GROUP BY s.name"
        ),
        format!(
            "SELECT r.feed, count(*) FROM weather.stations s JOIN weather.readings r \
             ON r.network_id = s.network_id AND r.station_no = s.station_no \
             WHERE s.name = '{name}' GROUP BY r.feed"
        ),
        "SELECT s.name, r.reading_id FROM weather.readings r JOIN weather.stations s \
         ON s.network_id = r.network_id AND s.station_no = r.station_no \
         WHERE r.observed_at >= '2022-03-01 00:00:00+00' AND r.observed_at < '2022-03-02 00:00:00+00'"
            .to_string(),
        "SELECT s.network_id, s.station_no, r.reading_id FROM weather.stations s \
         LEFT JOIN weather.readings r ON r.network_id = s.network_id AND r.station_no = s.station_no \
         AND r.feed = 'AWS Backup' WHERE s.network_id < 40"
            .to_string(),
    ];
    for q in &questions {
        let mut across = together(&split);
        let (want, opened) = across.ask(q).unwrap_or_else(|e| panic!("{q}: {e}"));
        let mut apart = [fresh(&split.stations), fresh(&split.readings)];
        assert_eq!(at_one_moment(&mut apart).unwrap(), vec![2, 2], "{q}");
        let refs = [&apart[0], &apart[1]];
        let before: Vec<u64> = refs.iter().map(|s| s.pages_read()).collect();
        let priced = price_together(&refs, q).unwrap_or_else(|e| panic!("{q}: {e}"));
        let read: u64 = refs
            .iter()
            .zip(&before)
            .map(|(s, b)| s.pages_read() - b)
            .sum();
        assert_eq!(read, priced.directory_pages(), "{q}: {priced:?}");
        let (got, round) = answer_together(&refs, q).unwrap_or_else(|e| panic!("{q}: {e}"));
        assert_eq!(bag(&got), bag(&want), "{q}");
        assert_eq!(round.order, opened.order, "{q}");
        assert_eq!(
            round.column_pages_predicted, opened.column_pages_predicted,
            "{q}"
        );
        each_exact(q, &round);
        assert_eq!(priced.order(), round.order, "{q}: {priced:?}");
        for (k, t) in priced.tables.iter().enumerate() {
            let part = &round.snails[usize::from(t.table == "weather.readings")];
            let pages = part.column_pages_predicted;
            assert!(
                t.column_pages_low <= pages && pages <= t.column_pages_high,
                "{q}: {t:?} against {pages}"
            );
            assert!(
                t.column_pages_low <= t.column_pages_estimate
                    && t.column_pages_estimate <= t.column_pages_high,
                "{q}: {t:?}"
            );
            if k == 0 || !t.kept {
                assert_eq!(t.column_pages_low, pages, "{q}: {t:?}");
            }
        }
    }
    // The first question's readings are kept to one station's keys: priced from the round of one
    // key, far under their whole read.
    let mut apart = [fresh(&split.stations), fresh(&split.readings)];
    at_one_moment(&mut apart).unwrap();
    let priced = price_together(&[&apart[0], &apart[1]], &questions[0]).unwrap();
    let kept = &priced.tables[1];
    assert!(kept.kept, "{priced:?}");
    assert_eq!(
        kept.key_column_pages
            .map(|p| p * priced.tables[0].rows_estimate),
        Some(kept.column_pages_estimate)
    );
    assert!(
        kept.column_pages_estimate < kept.column_pages_high,
        "{priced:?}"
    );
}

#[test]
fn two_snails_holding_one_name_are_refused_when_opened_together() {
    let s = sample(500);
    let split = split("across-names", &s);
    let e = Snails::open(&[&split.both, &split.readings])
        .err()
        .expect("both hold the readings")
        .to_string();
    assert!(
        e.starts_with("relation \"weather.r_month\" is held by")
            || e.starts_with("relation \"weather.readings\" is held by"),
        "{e}"
    );
    // An index's name is a relation's in its table's schema, as PostgreSQL holds it.
    let other = tmp("across-names-other.snail");
    let ddl = "CREATE TABLE weather.other (a integer NOT NULL, PRIMARY KEY (a)); \
               CREATE INDEX s_name ON weather.other (a);";
    write(
        &other,
        &Catalog::parse(ddl).unwrap(),
        &[(
            "weather.other".into(),
            Rows {
                columns: vec![ints(&[1, 2])],
            },
        )],
    )
    .unwrap();
    let e = Snails::open(&[&split.stations, &other])
        .err()
        .expect("both hold s_name")
        .to_string();
    assert!(
        e.starts_with("relation \"weather.s_name\" is held by"),
        "{e}"
    );
    // A function two snails declare alike is one; declared otherwise, refused.
    let clock = tmp("across-names-clock.snail");
    let ddl = format!(
        "{} CREATE TABLE weather.more (a integer NOT NULL, PRIMARY KEY (a));",
        READINGS.split("CREATE TABLE").next().unwrap()
    );
    write(
        &clock,
        &Catalog::parse(&ddl).unwrap(),
        &[(
            "weather.more".into(),
            Rows {
                columns: vec![ints(&[1])],
            },
        )],
    )
    .unwrap();
    assert!(Snails::open(&[&split.readings, &clock]).is_ok());
    let odd = tmp("across-names-odd.snail");
    let ddl = "CREATE FUNCTION weather.clock(timestamptz) RETURNS timestamp \
               LANGUAGE sql IMMUTABLE AS $$ SELECT $1 AT TIME ZONE 'Europe/Lisbon' $$; \
               CREATE TABLE weather.odd (a integer NOT NULL, PRIMARY KEY (a));";
    if let Ok(catalog) = Catalog::parse(ddl) {
        write(
            &odd,
            &catalog,
            &[(
                "weather.odd".into(),
                Rows {
                    columns: vec![ints(&[1])],
                },
            )],
        )
        .unwrap();
        let e = Snails::open(&[&split.readings, &odd])
            .err()
            .expect("the clocks differ")
            .to_string();
        assert!(e.contains("is declared otherwise by"), "{e}");
    }
}

#[cfg(feature = "standalone")]
mod written {
    use super::*;
    use warren_snail::read::{Snail, StopAcross};

    fn count_of(s: &mut Snails, q: &str) -> i64 {
        one(&s.ask(q).unwrap_or_else(|e| panic!("{q}: {e}")).0)
    }

    fn station(network: i64, no: i64) -> String {
        format!(
            "INSERT INTO weather.stations (network_id, station_no, name) VALUES ({network}, {no}, 'Sagres {no}')"
        )
    }

    fn reading(id: i64, network: i64, no: i64) -> String {
        format!(
            "INSERT INTO weather.readings (reading_id, network_id, station_no, feed, observed_at, checked_at) \
             VALUES ({id}, {network}, {no}, 'AWS Main', '2022-06-15 10:00:00+00', NULL)"
        )
    }

    fn alone_count(path: &Path, q: &str) -> i64 {
        one(&Snail::open(path).unwrap().ask(q, None).unwrap().0)
    }

    const ALL_READINGS: &str = "SELECT count(*) FROM weather.readings r";
    const ALL_STATIONS: &str = "SELECT count(*) FROM weather.stations s";

    /// Snails opened apart, a commit on each landing between their opens: the moment reads every
    /// root again, finds the first changed, and takes both afresh, so the pair read is one a moment
    /// held.
    #[test]
    fn snails_opened_apart_take_their_roots_at_one_moment() {
        use warren_snail::read::{answer_together, at_one_moment};
        let s = sample(5_000);
        let split = split("together-moment", &s);
        let (network, station) = busiest(&s);
        let q = joined(network, station);
        let stations = fresh(&split.stations);
        let first = stations.generation();
        Snail::open_writing(&split.stations)
            .unwrap()
            .execute(&format!(
                "DELETE FROM weather.stations WHERE network_id = {network} AND station_no = {station}"
            ))
            .unwrap();
        Snail::open_writing(&split.readings)
            .unwrap()
            .execute(&reading(9_000_001, network, station))
            .unwrap();
        let mut apart = [stations, fresh(&split.readings)];
        let pages = at_one_moment(&mut apart).unwrap();
        assert_eq!(apart[0].generation(), first + 1);
        // Each root read again, then afresh and again; the stations' catalog once more.
        assert!(pages[0] > 6 && pages[1] == 6, "{pages:?}");
        let (batch, _) = answer_together(&[&apart[0], &apart[1]], &q).unwrap();
        assert_eq!(one(&batch), 0, "a pair of roots no moment held");
    }

    /// A crash after the deciding root of a transaction across the two: snails opened apart and
    /// taken at one moment read the root it carries for the readings, as a round across them does.
    #[test]
    fn snails_opened_apart_read_the_root_a_decision_among_them_carries() {
        use warren_snail::read::{answer_together, at_one_moment};
        let s = sample(5_000);
        let split = split("together-decided", &s);
        let r0 = alone_count(&split.readings, ALL_READINGS);
        {
            let mut w = Snails::open_writing(&paths(&split)).unwrap();
            w.execute("BEGIN").unwrap();
            w.execute(&station(9600, 1)).unwrap();
            w.execute(&reading(9_000_011, 9600, 1)).unwrap();
            w.stop_at(Some(StopAcross::AfterDecision));
            assert!(w.execute("COMMIT").is_err());
        }
        assert_eq!(alone_count(&split.readings, ALL_READINGS), r0);
        let mut apart = [fresh(&split.stations), fresh(&split.readings)];
        at_one_moment(&mut apart).unwrap();
        let refs = [&apart[0], &apart[1]];
        assert_eq!(one(&answer_together(&refs, &joined(9600, 1)).unwrap().0), 1);
        assert_eq!(
            one(&answer_together(&refs, ALL_READINGS).unwrap().0),
            r0 + 1
        );
    }

    #[test]
    fn a_round_takes_the_roots_where_none_changed_between_its_two_readings() {
        let s = sample(5_000);
        let split = split("across-moment", &s);
        let (network, station) = busiest(&s);
        let q = joined(network, station);
        let mut across = together(&split);
        let before = count_of(&mut across, &q);
        assert!(before > 0);
        // Between the first pass's two roots, the station is deleted, then a reading of it is
        // written: two commits in turn, each a transaction of its own. No moment holds the station
        // beside the new reading.
        let (a, b) = (split.stations.clone(), split.readings.clone());
        across.between_roots(Some(Box::new(move || {
            Snail::open_writing(&a)
                .unwrap()
                .execute(&format!(
                    "DELETE FROM weather.stations WHERE network_id = {network} AND station_no = {station}"
                ))
                .unwrap();
            Snail::open_writing(&b)
                .unwrap()
                .execute(&reading(9_000_001, network, station))
                .unwrap();
        })));
        let (batch, round) = across.ask(&q).unwrap();
        assert_eq!(one(&batch), 0, "a pair of roots no moment held: {round:?}");
        // The first pass found a root changed, so the round read both again: four pages a snail
        // twice, and each catalog once on its new generation.
        assert_eq!(round.pages_at_start, 18, "{round:?}");
        let (_, round) = across.ask(&q).unwrap();
        assert_eq!(round.pages_at_start, 8);
    }

    #[test]
    fn a_transaction_across_two_snails_commits_both_one_root_deciding() {
        let s = sample(5_000);
        let split = split("across-commit", &s);
        let mut w = Snails::open_writing(&paths(&split)).unwrap();
        let (n0, r0) = (
            alone_count(&split.stations, ALL_STATIONS),
            alone_count(&split.readings, ALL_READINGS),
        );
        let g0 = w.generations();
        w.execute("BEGIN").unwrap();
        w.execute(&station(9500, 1)).unwrap();
        w.execute(&reading(9_000_001, 9500, 1)).unwrap();
        w.execute(&reading(9_000_002, 9500, 1)).unwrap();
        // The transaction reads its own rows across both; nothing is committed meanwhile.
        assert_eq!(count_of(&mut w, &joined(9500, 1)), 2);
        assert_eq!(alone_count(&split.readings, ALL_READINGS), r0);
        let d = w.execute("COMMIT").unwrap();
        assert_eq!(d.statement, "COMMIT");
        assert_eq!(w.generations(), [g0[0] + 1, g0[1] + 1]);
        // The stations' root, first written, decided it, naming the readings' new root.
        let decided = Snail::open(&split.stations).unwrap().decided().to_vec();
        assert_eq!(decided.len(), 1);
        assert_eq!(decided[0].at, g0[0] + 1);
        assert_eq!(
            decided[0]
                .roots
                .iter()
                .map(|r| (r.holds.as_str(), r.generation))
                .collect::<Vec<_>>(),
            [("weather.readings", g0[1] + 1)]
        );
        assert!(Snail::open(&split.readings).unwrap().decided().is_empty());
        assert_eq!(
            (
                alone_count(&split.stations, ALL_STATIONS),
                alone_count(&split.readings, ALL_READINGS)
            ),
            (n0 + 1, r0 + 2)
        );
        assert_eq!(count_of(&mut together(&split), &joined(9500, 1)), 2);
        for p in paths(&split) {
            Snail::open(p).unwrap().census().unwrap();
        }
        // Rolled back, neither changes; a statement that fails fails the transaction on both.
        w.execute("BEGIN").unwrap();
        w.execute(&station(9500, 2)).unwrap();
        w.execute(&reading(9_000_003, 9500, 2)).unwrap();
        w.execute("ROLLBACK").unwrap();
        w.execute("BEGIN").unwrap();
        w.execute(&station(9500, 3)).unwrap();
        let e = w
            .execute(&reading(9_000_001, 9500, 3))
            .unwrap_err()
            .to_string();
        assert!(e.contains("unique"), "{e}");
        let e = w.execute(&station(9500, 4)).unwrap_err().to_string();
        assert!(e.contains("current transaction is aborted"), "{e}");
        assert_eq!(w.execute("COMMIT").unwrap().statement, "ROLLBACK");
        assert_eq!(
            (
                alone_count(&split.stations, ALL_STATIONS),
                alone_count(&split.readings, ALL_READINGS)
            ),
            (n0 + 1, r0 + 2)
        );
        // A transaction that writes one snail commits there alone.
        w.execute("BEGIN").unwrap();
        w.execute(&reading(9_000_004, 9500, 1)).unwrap();
        w.execute("COMMIT").unwrap();
        assert_eq!(w.generations(), [g0[0] + 1, g0[1] + 2]);
        // The next transaction across them that the stations decide drops the decision before it,
        // every root of which is written.
        w.execute("BEGIN").unwrap();
        w.execute(&station(9500, 5)).unwrap();
        w.execute(&reading(9_000_005, 9500, 5)).unwrap();
        w.execute("COMMIT").unwrap();
        let decided = Snail::open(&split.stations).unwrap().decided().to_vec();
        assert_eq!(
            decided.iter().map(|d| d.at).collect::<Vec<_>>(),
            [g0[0] + 2]
        );
        assert_eq!(count_of(&mut together(&split), &joined(9500, 5)), 1);
        // Decided by the readings, first written, the stations' own decision, its roots written,
        // is dropped by the root the transaction writes there.
        w.execute("BEGIN").unwrap();
        w.execute(&reading(9_000_006, 9500, 6)).unwrap();
        w.execute(&station(9500, 6)).unwrap();
        w.execute("COMMIT").unwrap();
        assert!(Snail::open(&split.stations).unwrap().decided().is_empty());
        assert_eq!(Snail::open(&split.readings).unwrap().decided().len(), 1);
        // A statement reads only the snail it writes.
        let e = w
            .execute(
                "INSERT INTO weather.readings (reading_id, network_id, station_no, feed, observed_at, checked_at) \
                 SELECT 9000007, s.network_id, s.station_no, 'AWS Main', '2022-06-15 10:00:00+00', NULL \
                 FROM weather.stations s WHERE s.network_id = 9500 AND s.station_no = 6",
            )
            .unwrap_err()
            .to_string();
        assert!(e.contains("reads only the snail it writes"), "{e}");
        // The pages that carried the decisions dropped are freed: every page is named once, or free.
        for p in paths(&split) {
            Snail::open(p).unwrap().census().unwrap();
        }
    }

    #[test]
    fn a_crash_after_the_deciding_root_leaves_each_snail_whole_and_the_round_across_them_all() {
        let s = sample(5_000);
        let split = split("across-crash-after", &s);
        let (n0, r0) = (
            alone_count(&split.stations, ALL_STATIONS),
            alone_count(&split.readings, ALL_READINGS),
        );
        {
            let mut w = Snails::open_writing(&paths(&split)).unwrap();
            w.execute("BEGIN").unwrap();
            w.execute(&station(9600, 1)).unwrap();
            w.execute(&reading(9_000_011, 9600, 1)).unwrap();
            w.stop_at(Some(StopAcross::AfterDecision));
            let e = w.execute("COMMIT").unwrap_err().to_string();
            assert!(e.contains("stopped after the deciding root"), "{e}");
        }
        // Each snail read alone is a whole generation: the stations' holds the transaction's
        // station, its reading on the null side of the join; the readings' does not hold it yet.
        assert_eq!(alone_count(&split.stations, ALL_STATIONS), n0 + 1);
        assert_eq!(alone_count(&split.readings, ALL_READINGS), r0);
        for p in paths(&split) {
            Snail::open(p).unwrap().census().unwrap();
        }
        // Read alone, the stations' root marks its newest rows as a transaction's whose root in the
        // readings is not yet taken there, which tells its station from one no reading names.
        let readings_at = Snail::open(&split.readings).unwrap().generation();
        let decided = Snail::open(&split.stations).unwrap().decided().to_vec();
        assert_eq!(
            decided
                .iter()
                .flat_map(|d| d.roots.iter().map(|r| r.generation))
                .collect::<Vec<_>>(),
            [readings_at + 1]
        );
        // The round across them reads the readings' new root, which the deciding root names.
        let mut across = together(&split);
        assert_eq!(count_of(&mut across, &joined(9600, 1)), 1);
        assert_eq!(count_of(&mut across, ALL_READINGS), r0 + 1);
        // A writer of the readings alone takes no commit; one of the stations alone does, and
        // carries the decision.
        let e = Snail::open_writing(&split.readings)
            .unwrap()
            .execute(&reading(9_000_012, 9600, 1))
            .unwrap_err()
            .to_string();
        assert!(e.contains("open the two together"), "{e}");
        Snail::open_writing(&split.stations)
            .unwrap()
            .execute(&station(9600, 2))
            .unwrap();
        assert_eq!(count_of(&mut together(&split), &joined(9600, 1)), 1);
        // Opened together to write, the readings' root is written.
        drop(Snails::open_writing(&paths(&split)).unwrap());
        assert_eq!(alone_count(&split.readings, ALL_READINGS), r0 + 1);
        Snail::open_writing(&split.readings)
            .unwrap()
            .execute(&reading(9_000_012, 9600, 1))
            .unwrap();
        assert_eq!(count_of(&mut together(&split), &joined(9600, 1)), 2);
        for p in paths(&split) {
            Snail::open(p).unwrap().census().unwrap();
        }
    }

    #[test]
    fn a_root_torn_after_the_deciding_root_leaves_its_pending_copy_and_a_writer_alone_all() {
        let s = sample(5_000);
        let split = split("across-torn", &s);
        let (n0, r0) = (
            alone_count(&split.stations, ALL_STATIONS),
            alone_count(&split.readings, ALL_READINGS),
        );
        {
            let mut w = Snails::open_writing(&paths(&split)).unwrap();
            // A first transaction lands whole: the readings' root lies over its parent.
            w.execute("BEGIN").unwrap();
            w.execute(&station(9650, 1)).unwrap();
            w.execute(&reading(9_000_030, 9650, 1)).unwrap();
            w.execute("COMMIT").unwrap();
            // The second is decided, and the readings' root torn as it is written over its parent.
            w.execute("BEGIN").unwrap();
            w.execute(&station(9650, 2)).unwrap();
            w.execute(&reading(9_000_031, 9650, 2)).unwrap();
            w.stop_at(Some(StopAcross::HalfFinal));
            let e = w.execute("COMMIT").unwrap_err().to_string();
            assert!(e.contains("stopped halfway through a root"), "{e}");
        }
        // Its pending copy, whole beside the torn one, is its root, so read alone the readings hold
        // the transaction's reading.
        assert_eq!(alone_count(&split.readings, ALL_READINGS), r0 + 2);
        // A writer of it alone writes on from that root, its root into the torn copy, never over
        // the pending copy it begins from: torn again, the root stands.
        let mut alone = Snail::open_writing(&split.readings).unwrap();
        alone.stop_at(Some(warren_snail::file::Stop::HalfRoot));
        alone.execute(&reading(9_000_032, 9650, 2)).unwrap_err();
        drop(alone);
        assert_eq!(alone_count(&split.readings, ALL_READINGS), r0 + 2);
        Snail::open_writing(&split.readings)
            .unwrap()
            .execute(&reading(9_000_032, 9650, 2))
            .unwrap();
        // The round across both: all of each transaction, as PostgreSQL commits them.
        let mut across = together(&split);
        assert_eq!(count_of(&mut across, &joined(9650, 1)), 1);
        assert_eq!(count_of(&mut across, &joined(9650, 2)), 2);
        assert_eq!(count_of(&mut across, ALL_STATIONS), n0 + 2);
        assert_eq!(count_of(&mut across, ALL_READINGS), r0 + 3);
        drop(Snails::open_writing(&paths(&split)).unwrap());
        assert_eq!(count_of(&mut together(&split), &joined(9650, 2)), 2);
        for p in paths(&split) {
            Snail::open(p).unwrap().census().unwrap();
        }
    }

    #[test]
    fn a_crash_before_the_deciding_root_leaves_each_snail_whole_and_the_round_across_them_none() {
        let s = sample(5_000);
        let split = split("across-crash-before", &s);
        let (n0, r0) = (
            alone_count(&split.stations, ALL_STATIONS),
            alone_count(&split.readings, ALL_READINGS),
        );
        {
            let mut w = Snails::open_writing(&paths(&split)).unwrap();
            w.execute("BEGIN").unwrap();
            w.execute(&station(9700, 1)).unwrap();
            w.execute(&reading(9_000_021, 9700, 1)).unwrap();
            w.stop_at(Some(StopAcross::BeforeDecision));
            let e = w.execute("COMMIT").unwrap_err().to_string();
            assert!(e.contains("stopped before the deciding root"), "{e}");
        }
        assert_eq!(alone_count(&split.stations, ALL_STATIONS), n0);
        assert_eq!(alone_count(&split.readings, ALL_READINGS), r0);
        let mut across = together(&split);
        assert_eq!(count_of(&mut across, &joined(9700, 1)), 0);
        assert_eq!(count_of(&mut across, ALL_READINGS), r0);
        let e = Snail::open_writing(&split.readings)
            .unwrap()
            .execute(&reading(9_000_022, 9700, 1))
            .unwrap_err()
            .to_string();
        assert!(e.contains("open the two together"), "{e}");
        // Opened together to write, the pending root no deciding root named is let go.
        drop(Snails::open_writing(&paths(&split)).unwrap());
        Snail::open_writing(&split.readings)
            .unwrap()
            .execute(&reading(9_000_022, 9700, 1))
            .unwrap();
        assert_eq!(alone_count(&split.readings, ALL_READINGS), r0 + 1);
        assert_eq!(count_of(&mut together(&split), &joined(9700, 1)), 0);
        for p in paths(&split) {
            Snail::open(p).unwrap().census().unwrap();
        }
    }

    #[test]
    fn writers_waiting_in_a_ring_across_two_snails_fail_one_with_a_deadlock() {
        use std::sync::mpsc;
        use std::time::Duration;
        let s = sample(5_000);
        let split = split("across-ring", &s);
        let (network, station) = stations_of(&s)[0];
        let b = in_order(&s)[4_000].2;
        let set_station = |name: &str| {
            format!(
                "UPDATE weather.stations SET name = '{name}' WHERE network_id = {network} AND station_no = {station}"
            )
        };
        let set_reading = |feed: &str| {
            format!("UPDATE weather.readings SET feed = '{feed}' WHERE reading_id = {b}")
        };
        // The second writer holds the reading's block.
        let (held_tx, held_rx) = mpsc::channel();
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let (out_tx, out_rx) = mpsc::channel();
        let p = paths(&split).map(Path::to_path_buf);
        let (sta, rea) = (set_station("two"), set_reading("two"));
        std::thread::spawn(move || {
            let mut two = Snails::open_writing(&[&p[0], &p[1]]).unwrap();
            two.execute("BEGIN").unwrap();
            two.execute(&rea).unwrap();
            held_tx.send(()).unwrap();
            go_rx.recv().unwrap();
            // It now waits for the station's block, which the first writer holds while it waits
            // for the reading's: the ring closes across the two files.
            let ring = two.execute(&sta).map_err(|e| e.to_string());
            let end = two
                .execute("COMMIT")
                .map(|d| d.statement)
                .map_err(|e| e.to_string());
            out_tx.send((ring, end)).unwrap();
        });
        held_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let (one_tx, one_rx) = mpsc::channel();
        let p = paths(&split).map(Path::to_path_buf);
        let (sta, rea) = (set_station("one"), set_reading("one"));
        std::thread::spawn(move || {
            let mut one_ = Snails::open_writing(&[&p[0], &p[1]]).unwrap();
            let done: Vec<Result<&'static str, String>> =
                ["BEGIN".to_string(), sta, rea, "COMMIT".into()]
                    .iter()
                    .map(|st| {
                        one_.execute(st)
                            .map(|d| d.statement)
                            .map_err(|e| e.to_string())
                    })
                    .collect();
            one_tx.send(done).unwrap();
        });
        std::thread::sleep(Duration::from_millis(400));
        go_tx.send(()).unwrap();
        let (ring, end) = out_rx.recv_timeout(Duration::from_secs(20)).unwrap();
        let e = ring.unwrap_err();
        assert!(e.starts_with("deadlock detected"), "{e}");
        assert_eq!(end, Ok("ROLLBACK"));
        let done = one_rx.recv_timeout(Duration::from_secs(20)).unwrap();
        assert!(done.iter().all(Result::is_ok), "{done:?}");
        let mut r = Snail::open(&split.readings).unwrap();
        assert_eq!(feed_of(&mut r, b), "one");
        let (batch, _) = Snail::open(&split.stations)
            .unwrap()
            .ask(
                &format!(
                    "SELECT s.name FROM weather.stations s WHERE s.network_id = {network} AND s.station_no = {station}"
                ),
                None,
            )
            .unwrap();
        assert_eq!(texts(batch.column(0).as_ref())[0].as_deref(), Some("one"));
    }

    /// SET CONSTRAINTS across two snails: each name reaches the snail holding it, found as that
    /// snail's own SET CONSTRAINTS finds it, and one neither holds is refused in PostgreSQL's words.
    #[test]
    fn set_constraints_across_snails_reaches_the_snail_holding_each_name() {
        let one = tmp("constraints-across-one.snail");
        let two = tmp("constraints-across-two.snail");
        let made = |path: &Path, ddl: &str| {
            write(path, &Catalog::parse(ddl).unwrap(), &[]).unwrap();
        };
        made(
            &one,
            "CREATE TABLE parent (id integer PRIMARY KEY);
             CREATE TABLE child (id integer PRIMARY KEY, p integer,
                 CONSTRAINT k_fk FOREIGN KEY (p) REFERENCES parent (id) DEFERRABLE);",
        );
        made(
            &two,
            "CREATE TABLE s.parent (id integer PRIMARY KEY);
             CREATE TABLE s.child (id integer PRIMARY KEY, q integer, CONSTRAINT k_check CHECK (id > 0),
                 CONSTRAINT s_fk FOREIGN KEY (q) REFERENCES s.parent (id) DEFERRABLE);",
        );
        let mut w = Snails::open_writing(&[&one, &two]).unwrap();
        for (sql, why) in [
            (
                "SET CONSTRAINTS s_fk IMMEDIATE",
                "constraint \"s_fk\" does not exist",
            ),
            (
                "SET CONSTRAINTS s.nope IMMEDIATE",
                "constraint \"nope\" does not exist",
            ),
            (
                "SET CONSTRAINTS t.k_fk IMMEDIATE",
                "schema \"t\" does not exist",
            ),
            (
                "SET CONSTRAINTS s.k_check DEFERRED",
                "constraint \"k_check\" is not deferrable",
            ),
        ] {
            w.execute("BEGIN").unwrap();
            let e = w
                .execute(sql)
                .err()
                .unwrap_or_else(|| panic!("{sql} was taken"));
            assert_eq!(e.to_string(), why, "{sql}");
            w.execute("ROLLBACK").unwrap();
        }
        w.execute("BEGIN").unwrap();
        w.execute("SET CONSTRAINTS k_fk, s.s_fk DEFERRED").unwrap();
        w.execute("INSERT INTO child (id, p) VALUES (1, 9)")
            .unwrap();
        w.execute("INSERT INTO s.child (id, q) VALUES (1, 8)")
            .unwrap();
        let e = w.execute("SET CONSTRAINTS s.s_fk IMMEDIATE").unwrap_err();
        assert!(e.to_string().contains("Key (q)=(8) is not present"), "{e}");
        w.execute("ROLLBACK").unwrap();
    }

    /// A snail holding `ring.t<i>`, keys enough for its order to be cut into several blocks; and the
    /// first key of its first block and of its last.
    fn ring_table(name: &str, i: usize) -> (PathBuf, i64, i64) {
        let path = tmp(&format!("{name}-{i}.snail"));
        let ddl = format!(
            "CREATE TABLE ring.t{i} (k integer NOT NULL, v integer NOT NULL, PRIMARY KEY (k));"
        );
        let keys: Vec<i64> = (0..4096).collect();
        write(
            &path,
            &Catalog::parse(&ddl).unwrap(),
            &[(
                format!("ring.t{i}"),
                Rows {
                    columns: vec![ints(&keys), ints(&[0; 4096])],
                },
            )],
        )
        .unwrap();
        let blocks = Snail::open(&path).unwrap().blocks().unwrap();
        let last = blocks.iter().map(|b| b.block).max().unwrap();
        assert!(last > 0, "{name}: one block");
        let first = |n: u64| numbers(&blocks.iter().find(|b| b.block == n).unwrap().first)[0];
        (path, first(0), first(last))
    }

    /// The value of a ring table's row.
    fn ring_value(path: &Path, i: usize, k: i64) -> i64 {
        let q = format!("SELECT r.v FROM ring.t{i} r WHERE r.k = {k}");
        one(&Snail::open(path).unwrap().ask(&q, None).unwrap().0)
    }

    /// What a writer's second write and its end came to.
    type Ended = (Result<u64, String>, Result<&'static str, String>);

    /// Writers in a ring, each in a thread of its own: writer i opens the snails `files` names (one
    /// alone, or several together), begins, and writes `holds`. Once every one holds, each but the
    /// last writes `awaits` in turn, the next told to only once the writers waiting in the file
    /// `waits_in` names have grown by it; then the last. What each one's second write and end came
    /// to. A writer that never comes back fails the test.
    fn in_a_ring(writers: Vec<(Vec<PathBuf>, String, String, PathBuf)>) -> Vec<Ended> {
        use std::sync::mpsc;
        use std::time::{Duration, Instant};
        let mut held = Vec::new();
        let mut go = Vec::new();
        let mut ended = Vec::new();
        for (files, holds, awaits, _) in writers.clone() {
            let (held_tx, held_rx) = mpsc::channel();
            let (go_tx, go_rx) = mpsc::channel::<()>();
            let (end_tx, end_rx) = mpsc::channel();
            std::thread::spawn(move || {
                let mut execute: Box<
                    dyn FnMut(&str) -> warren_snail::Result<warren_snail::read::Done>,
                > = if files.len() == 1 {
                    let mut s = Snail::open_writing(&files[0]).unwrap();
                    Box::new(move |q| s.execute(q))
                } else {
                    let refs: Vec<&Path> = files.iter().map(PathBuf::as_path).collect();
                    let mut s = Snails::open_writing(&refs).unwrap();
                    Box::new(move |q| s.execute(q))
                };
                execute("BEGIN").unwrap();
                execute(&holds).unwrap();
                held_tx.send(()).unwrap();
                go_rx.recv().unwrap();
                let second = execute(&awaits).map(|d| d.rows).map_err(|e| e.to_string());
                let end = execute("COMMIT")
                    .map(|d| d.statement)
                    .map_err(|e| e.to_string());
                end_tx.send((second, end)).unwrap();
            });
            held.push(held_rx);
            go.push(go_tx);
            ended.push(end_rx);
        }
        for h in &held {
            h.recv_timeout(Duration::from_secs(20)).unwrap();
        }
        let mut waiting: std::collections::BTreeMap<PathBuf, u64> = Default::default();
        let last = writers.len() - 1;
        for (i, (_, _, _, waits_in)) in writers.iter().enumerate().take(last) {
            go[i].send(()).unwrap();
            let want = *waiting
                .entry(waits_in.clone())
                .and_modify(|n| *n += 1)
                .or_insert(1);
            let seen = std::fs::File::open(waits_in).unwrap();
            let since = Instant::now();
            while warren_snail::lock::waiting(&seen).unwrap() < want {
                assert!(
                    since.elapsed() < Duration::from_secs(20),
                    "writer {i} never waited"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        // The walks of those waiting ended before the ring was closed.
        std::thread::sleep(Duration::from_millis(100));
        go[last].send(()).unwrap();
        ended
            .iter()
            .enumerate()
            .map(|(i, e)| {
                e.recv_timeout(Duration::from_secs(20))
                    .unwrap_or_else(|_| panic!("writer {i} still waits"))
            })
            .collect()
    }

    /// The last writer was refused as a deadlock and rolled back; every other wrote a row and
    /// committed.
    fn the_last_refused(ended: &[Ended]) {
        let (last, rest) = ended.split_last().unwrap();
        let e = last.0.as_ref().unwrap_err();
        assert!(e.starts_with("deadlock detected"), "{e}");
        assert_eq!(last.1, Ok("ROLLBACK"));
        for (i, (second, end)) in rest.iter().enumerate() {
            assert_eq!((second, end), (&Ok(1), &Ok("COMMIT")), "writer {i}");
        }
    }

    /// `n` writers each of two of `n` snails, writer i holding a row of table i and waiting for the
    /// same row of table i + 1, which the next holds, the last for table 1's: the last closes the
    /// ring through snails it never opened, and is refused as a deadlock. The others commit in turn
    /// back round the ring, table 1 left as the first wrote it and each other as the writer before
    /// its own.
    fn ring_of_writers_of_two(name: &str, n: usize) {
        let tables: Vec<(PathBuf, i64, i64)> = (1..=n).map(|i| ring_table(name, i)).collect();
        let k = tables[0].1;
        let writers = (1..=n)
            .map(|i| {
                let j = i % n + 1;
                let set = |t: usize| format!("UPDATE ring.t{t} SET v = {i} WHERE k = {k}");
                (
                    vec![tables[i - 1].0.clone(), tables[j - 1].0.clone()],
                    set(i),
                    set(j),
                    tables[j - 1].0.clone(),
                )
            })
            .collect();
        the_last_refused(&in_a_ring(writers));
        for (i, (path, _, _)) in tables.iter().enumerate() {
            let t = i + 1;
            assert_eq!(
                ring_value(path, t, k),
                if t == 1 { 1 } else { t as i64 - 1 },
                "t{t}"
            );
        }
    }

    #[test]
    fn writers_waiting_in_a_ring_through_three_snails_fail_the_one_closing_it_where_it_never_opened_one(
    ) {
        ring_of_writers_of_two("ring-three", 3);
    }

    #[test]
    fn writers_waiting_in_a_ring_through_four_snails_fail_the_one_closing_it_where_it_never_opened_two(
    ) {
        ring_of_writers_of_two("ring-four", 4);
    }

    /// Four writers in a ring through two snails: the first writes only the first and closes the
    /// ring, a writer of the two holding the block of the first it waits for, a writer of the second
    /// alone holding the one that writer waits for there, and a writer of both, waiting for the
    /// first's block, holding the one the second-alone waits for. The first never opened the second
    /// snail, and is refused as a deadlock.
    #[test]
    fn a_writer_of_one_snail_closing_a_ring_through_a_snail_it_never_opened_fails_with_a_deadlock()
    {
        let (a, a0, a1) = ring_table("ring-alone", 1);
        let (b, b0, b1) = ring_table("ring-alone", 2);
        let set = |t: usize, k: i64, v: i64| format!("UPDATE ring.t{t} SET v = {v} WHERE k = {k}");
        let ended = in_a_ring(vec![
            (
                vec![a.clone(), b.clone()],
                set(1, a1, 2),
                set(2, b0, 2),
                b.clone(),
            ),
            (vec![b.clone()], set(2, b0, 3), set(2, b1, 3), b.clone()),
            (
                vec![b.clone(), a.clone()],
                set(2, b1, 4),
                set(1, a0, 4),
                a.clone(),
            ),
            (vec![a.clone()], set(1, a0, 1), set(1, a1, 1), a.clone()),
        ]);
        the_last_refused(&ended);
        assert_eq!(
            [
                ring_value(&a, 1, a0),
                ring_value(&a, 1, a1),
                ring_value(&b, 2, b0),
                ring_value(&b, 2, b1)
            ],
            [4, 2, 2, 3]
        );
    }
}
