//! Questions asked of the stations and their readings, on every order that serves them: each answer
//! against the rows the sample holds, and each read against the pages its directory predicted.

mod weather;

use chrono::Datelike;
use std::collections::BTreeMap;
use warren_snail::catalog::Catalog;
use warren_snail::places::month_utc;
use warren_snail::read::{instant, Snail};
use warren_snail::values::Values;
use warren_snail::write::Rows;
use weather::*;

#[test]
fn the_three_questions_on_every_order_that_serves_them() {
    let s = sample(40_000);
    let path = snail("questions", &s);
    let mut snail = Snail::open(&path).unwrap();
    assert_eq!(snail.pages_at_open, 2 + 1);

    let december: Vec<usize> = (0..s.observed_at.len())
        .filter(|&i| month_utc(s.observed_at[i]) == 12)
        .collect();
    let mut want: Vec<Option<i64>> = december.iter().map(|&i| s.checked_at[i]).collect();
    want.sort();

    let q1 = format!("SELECT r.checked_at FROM weather.readings r WHERE {MONTH} = 12 AND {SPAN}");
    let q2 = format!("SELECT count(*) FROM weather.readings r WHERE {MONTH} = 12 AND {SPAN}");
    for order in ["r_station", "r_month"] {
        let (b, round) = snail.ask(&q1, Some(order)).unwrap();
        let mut got = timestamps(b.column(0).as_ref());
        got.sort();
        assert_eq!(got, want, "{order}");
        assert_eq!(
            round.column_pages_read,
            round.column_pages_predicted - round.column_pages_held,
            "{order}: {round:?}"
        );

        // A count on the month reads no column page.
        let (b, round) = snail.ask(&q2, Some(order)).unwrap();
        assert_eq!(one(&b) as usize, december.len(), "{order}");
        assert_eq!(round.column_pages_read, 0, "{order}: {round:?}");
        assert_eq!(round.column_pages_predicted, 0, "{order}");
    }
    let (_, round) = snail.ask(&q1, None).unwrap();
    assert_eq!(round.order, "r_month");
    assert_eq!(round.served, ["r_station", "r_month"]);

    // One station's readings: one block of the table's order.
    let (network, station) = busiest(&s);
    let mut want: Vec<(i64, i64, Option<i64>)> = (0..s.network_id.len())
        .filter(|&i| s.network_id[i] == network && s.station_no[i] == station)
        .map(|i| (s.reading_id[i], s.observed_at[i], s.checked_at[i]))
        .collect();
    want.sort();
    let q3 = format!(
        "SELECT r.reading_id, r.observed_at, r.checked_at FROM weather.readings r \
         WHERE r.network_id = {network} AND r.station_no = {station} AND {SPAN}"
    );
    let (b, round) = snail.ask(&q3, None).unwrap();
    assert_eq!(round.order, "r_station");
    assert_eq!(round.served, ["r_station"]);
    exact(&q3, &round);
    let ids = ints64(b.column(0).as_ref());
    let at = timestamps(b.column(1).as_ref());
    let checked = timestamps(b.column(2).as_ref());
    let mut got: Vec<(i64, i64, Option<i64>)> = (0..b.num_rows())
        .map(|i| (ids[i].unwrap(), at[i].unwrap(), checked[i]))
        .collect();
    got.sort();
    assert_eq!(got, want);
    assert_eq!(round.blocks_read, 1, "{round:?}");
}

#[test]
fn every_station_with_three_readings_or_more_has_them_all() {
    let s = sample(40_000);
    let path = snail("stations", &s);
    let mut snail = Snail::open(&path).unwrap();
    let mut by_station: BTreeMap<(i64, i64), Vec<i64>> = Default::default();
    for i in 0..s.network_id.len() {
        by_station
            .entry((s.network_id[i], s.station_no[i]))
            .or_default()
            .push(s.reading_id[i]);
    }
    let mut asked = 0;
    for ((network, station), mut want) in by_station.into_iter().filter(|(_, v)| v.len() >= 3) {
        want.sort();
        let q = format!(
            "SELECT r.reading_id FROM weather.readings r \
             WHERE r.network_id = {network} AND r.station_no = {station}"
        );
        let (b, round) = snail.ask(&q, None).unwrap();
        let mut got: Vec<i64> = ints64(b.column(0).as_ref()).into_iter().flatten().collect();
        got.sort();
        assert_eq!(got, want, "{q}");
        exact(&q, &round);
        asked += 1;
    }
    assert!(asked > 1000, "{asked}");
}

#[test]
fn every_order_is_priced_from_its_directory_and_the_cheapest_is_read() {
    let s = sample(40_000);
    let path = snail("priced", &s);
    let mut snail = fresh(&path);
    // Each block lists each column's pages.
    for b in snail.blocks().unwrap() {
        assert_eq!(
            b.pages_listed, b.disk_pages,
            "{} block {} {}",
            b.order, b.block, b.column
        );
    }

    let (network, station) = busiest(&s);
    let mut ids = s.reading_id.clone();
    ids.sort();
    let (from, to) = (ids[20_000], ids[20_300]);
    let questions = [
        format!("SELECT r.checked_at FROM weather.readings r WHERE {MONTH} = 12 AND {SPAN}"),
        format!("SELECT count(*) FROM weather.readings r WHERE {MONTH} = 12 AND {SPAN}"),
        format!(
            "SELECT r.reading_id, r.observed_at, r.checked_at FROM weather.readings r \
             WHERE r.network_id = {network} AND r.station_no = {station} AND {SPAN}"
        ),
        format!(
            "SELECT r.network_id, r.station_no FROM weather.readings r \
             WHERE r.reading_id >= {from} AND r.reading_id < {to}"
        ),
        format!(
            "SELECT r.checked_at FROM weather.readings r WHERE {MONTH} = 12 \
             AND r.observed_at >= '2022-12-05 00:00:00+00' AND r.observed_at < '2022-12-09 00:00:00+00'"
        ),
    ];
    let mut partial = false;
    for q in &questions {
        let (_, round) = snail.ask(q, None).unwrap();
        // (order, column pages, directory pages), each order read alone
        let mut alone = Vec::new();
        for o in &round.served {
            let (_, r) = snail.ask(q, Some(o)).unwrap();
            exact(&format!("{q} along {o}"), &r);
            assert!(
                r.column_pages_top_low <= r.column_pages_predicted
                    && r.column_pages_predicted <= r.column_pages_top_high,
                "{o}, at its top: {r:?}"
            );
            partial |= r.column_pages_low < r.column_pages_predicted
                || r.column_pages_predicted < r.column_pages_high;
            alone.push((o.clone(), r.column_pages_predicted, r.directory_pages));
        }
        // The order read is the one with the fewest pages, and the round read no directory page
        // past those of the orders it consulted.
        let fewest = alone.iter().map(|a| (a.1, a.2)).min().unwrap();
        let read = alone.iter().find(|a| a.0 == round.order).unwrap();
        assert_eq!((read.1, read.2), fewest, "{q}: {round:?}");
        assert_eq!(
            round.directory_pages,
            round
                .consulted
                .iter()
                .map(|c| c.directory_pages)
                .sum::<u64>(),
            "{q}"
        );
        // An order priced exactly read what it reads alone; one left out read less of its directory.
        for c in &round.consulted {
            let a = alone.iter().find(|a| a.0 == c.order).unwrap();
            match c.exact {
                Some(e) => assert_eq!((e, c.directory_pages), (a.1, a.2), "{q}: {c:?}"),
                None => assert!(c.directory_pages < a.2, "{q}: {c:?} read its page maps"),
            }
        }
    }
    assert!(partial, "no round read part of a block");
    // December's checks: the table's order is left out above its blocks.
    let (_, december) = snail.ask(&questions[0], None).unwrap();
    let r_station = december
        .consulted
        .iter()
        .find(|c| c.order == "r_station")
        .unwrap();
    assert_eq!(r_station.exact, None, "{december:?}");
}

#[test]
fn a_unique_order_refuses_two_rows_with_one_key() {
    let mut s = sample(2000);
    s.reading_id[7] = s.reading_id[3];
    let path = tmp("unique.snail");
    let catalog = Catalog::parse(DDL).unwrap();
    let e = write(&path, &catalog, &tables(&s)).unwrap_err().to_string();
    assert!(e.contains("readings_pkey, which is unique"), "{e}");
}

#[test]
fn a_value_longer_than_its_varchar_is_refused() {
    let mut s = sample(2000);
    s.feed[11] = "x".repeat(33);
    let path = tmp("long.snail");
    let catalog = Catalog::parse(DDL).unwrap();
    let e = write(&path, &catalog, &tables(&s)).unwrap_err().to_string();
    assert!(
        e.contains("feed is varchar(32) and a row holds 33 characters"),
        "{e}"
    );
    // The length is counted in characters, not bytes.
    s.feed[11] = "é".repeat(32);
    let _ = std::fs::remove_file(&path);
    write(&path, &catalog, &tables(&s)).unwrap();
}

#[test]
fn text_is_carried_and_compared_by_its_bytes_on_every_order_that_serves_it() {
    let s = sample(40_000);
    let path = snail("text", &s);
    let mut snail = fresh(&path);
    let stations = stations_of(&s);

    // The directory keeps each text column's bytes, block by block, as PostgreSQL's octet_length.
    let blocks = snail.blocks().unwrap();
    let bytes = |order: &str, column: &str| -> u64 {
        blocks
            .iter()
            .filter(|b| b.order == order && b.column == column)
            .map(|b| b.bytes.unwrap())
            .sum()
    };
    assert_eq!(
        bytes("r_station", "feed"),
        s.feed.iter().map(|v| v.len() as u64).sum::<u64>()
    );
    assert_eq!(
        bytes("stations_pkey", "name"),
        stations
            .iter()
            .map(|p| name(p.0, p.1).len() as u64)
            .sum::<u64>()
    );
    assert!(blocks
        .iter()
        .filter(|b| b.column != "feed" && b.column != "name")
        .all(|b| b.bytes.is_none()));
    // Five feeds: a dictionary page and one page of codes in every block of the table's order.
    for b in blocks
        .iter()
        .filter(|b| b.order == "r_station" && b.column == "feed")
    {
        assert_eq!(
            (b.dictionary_pages, b.disk_pages),
            (1, 2),
            "block {}",
            b.block
        );
    }

    // One station's readings with their feed, a point read on the table's order.
    let (network, station) = busiest(&s);
    let q = format!(
        "SELECT r.reading_id, r.feed FROM weather.readings r \
         WHERE r.network_id = {network} AND r.station_no = {station} AND {SPAN}"
    );
    let (b, round) = snail.ask(&q, None).unwrap();
    exact(&q, &round);
    assert_eq!(round.order, "r_station");
    let ids = ints64(b.column(0).as_ref());
    let feeds = texts(b.column(1).as_ref());
    let mut got: Vec<(i64, String)> = (0..b.num_rows())
        .map(|i| (ids[i].unwrap(), feeds[i].clone().unwrap()))
        .collect();
    got.sort();
    let mut want: Vec<(i64, String)> = (0..s.network_id.len())
        .filter(|&i| s.network_id[i] == network && s.station_no[i] == station)
        .map(|i| (s.reading_id[i], s.feed[i].clone()))
        .collect();
    want.sort();
    assert_eq!(got, want);

    // Counts of readings by their feed: equality, either side, and a range by bytes, in which every
    // capital comes before every small letter.
    type Case = (&'static str, fn(&str) -> bool);
    let cases: [Case; 4] = [
        ("r.feed = 'AWS Backup'", |v| v == "AWS Backup"),
        ("'AWS Backup' = r.feed", |v| v == "AWS Backup"),
        ("r.feed > 'AWS Main' AND r.feed <= 'observer'", |v| {
            v > "AWS Main" && v <= "observer"
        }),
        (
            "r.feed >= 'A' AND r.feed > 'AWS Main' AND r.feed < 'synop relay' AND r.feed <= 'zz'",
            |v| v > "AWS Main" && v < "synop relay",
        ),
    ];
    for (cond, holds) in cases {
        let q = format!("SELECT count(*) FROM weather.readings r WHERE {cond} AND {SPAN}");
        let (b, round) = snail.ask(&q, None).unwrap();
        exact(&q, &round);
        assert_eq!(round.served, ["r_station"], "{q}");
        assert!(round.column_pages_predicted > 0, "{q}: {round:?}");
        assert_eq!(
            one(&b) as usize,
            s.feed.iter().filter(|v| holds(v)).count(),
            "{q}"
        );
    }
    // A range no feed lies in reads no column page.
    let q = format!(
        "SELECT count(*) FROM weather.readings r WHERE r.feed > 'synop relay' AND r.feed < 'AWS' AND {SPAN}"
    );
    let (b, round) = snail.ask(&q, None).unwrap();
    assert_eq!((one(&b), round.column_pages_predicted), (0, 0));

    // Stations whose name lies in a range of bytes: a name that begins another, and names that go
    // on past ASCII.
    let q = "SELECT s.network_id, s.station_no, s.name FROM weather.stations s \
             WHERE s.name >= 'Lisboa' AND s.name < 'Lisbon'";
    let (b, round) = snail.ask(q, None).unwrap();
    exact(q, &round);
    let networks = ints64(b.column(0).as_ref());
    let numbers = ints64(b.column(1).as_ref());
    let names = texts(b.column(2).as_ref());
    let mut got: Vec<(i64, i64, String)> = (0..b.num_rows())
        .map(|i| {
            (
                networks[i].unwrap(),
                numbers[i].unwrap(),
                names[i].clone().unwrap(),
            )
        })
        .collect();
    got.sort();
    let want: Vec<(i64, i64, String)> = stations
        .iter()
        .map(|p| (p.0, p.1, name(p.0, p.1)))
        .filter(|(_, _, v)| v.as_str() >= "Lisboa" && v.as_str() < "Lisbon")
        .collect();
    assert!(["Lisboa 1", "Lisboa Gago", "Lisboa Geof", "Lisboa–"]
        .iter()
        .all(|p| want.iter().any(|w| w.2.starts_with(p))));
    assert_eq!(got, want);

    // One network's stations: one block.
    let network = stations[stations.len() / 2].0;
    let q = format!(
        "SELECT s.station_no, s.name FROM weather.stations s WHERE s.network_id = {network}"
    );
    let (b, round) = snail.ask(&q, None).unwrap();
    exact(&q, &round);
    let numbers = ints64(b.column(0).as_ref());
    let names = texts(b.column(1).as_ref());
    let mut got: Vec<(i64, String)> = (0..b.num_rows())
        .map(|i| (numbers[i].unwrap(), names[i].clone().unwrap()))
        .collect();
    got.sort();
    let want: Vec<(i64, String)> = stations
        .iter()
        .filter(|p| p.0 == network)
        .map(|p| (p.1, name(p.0, p.1)))
        .collect();
    assert_eq!(got, want);
    assert_eq!(round.blocks_read, 1, "{round:?}");
}

#[test]
fn the_directory_is_read_only_where_the_question_lies() {
    let s = sample(40_000);
    let path = tmp("levels.snail");
    let catalog = Catalog::parse(DDL).unwrap();
    let written = write(&path, &catalog, &tables(&s)).unwrap();
    let levels = |order: &str| written.iter().find(|w| w.order == order).unwrap().levels as u64;
    assert!(levels("r_station") >= 2, "{written:?}");
    let mut snail = Snail::open(&path).unwrap();

    // A point read reads one page a level, then its block's page map.
    let (network, station) = (s.network_id[0], s.station_no[0]);
    let q = format!(
        "SELECT r.reading_id FROM weather.readings r \
         WHERE r.network_id = {network} AND r.station_no = {station}"
    );
    let (_, round) = snail.ask(&q, Some("r_station")).unwrap();
    assert_eq!(round.directory_pages, levels("r_station") + 1, "{round:?}");
    let q = format!("SELECT s.name FROM weather.stations s WHERE s.network_id = {network}");
    let (_, round) = snail.ask(&q, Some("stations_pkey")).unwrap();
    assert_eq!(
        round.directory_pages,
        levels("stations_pkey") + 1,
        "{round:?}"
    );

    // December's count along the table's order is answered by its top's counts.
    let q = format!("SELECT count(*) FROM weather.readings r WHERE {MONTH} = 12 AND {SPAN}");
    let (b, round) = snail.ask(&q, Some("r_station")).unwrap();
    assert_eq!(
        (round.directory_pages, round.column_pages_predicted),
        (1, 0),
        "{round:?}"
    );
    assert_eq!(
        one(&b) as usize,
        s.observed_at
            .iter()
            .filter(|&&at| month_utc(at) == 12)
            .count()
    );

    // Each block's place, read back from its first key and its depth, is the place the cut gave it:
    // on the month's index, the half of the year, the month, then the year and the month again, as
    // deep as the cut went.
    let places = snail.blocks().unwrap();
    let mut seen = 0;
    for b in places
        .iter()
        .filter(|b| b.order == "r_month" && b.column == "observed_at")
    {
        let (place, first) = (numbers(&b.place), numbers(&b.first));
        let year = chrono::DateTime::from_timestamp_micros(first[1])
            .unwrap()
            .year() as i64;
        let sublevels = [(first[0] - 1) / 6, first[0], year, first[0]];
        assert!(
            place.len() >= 2 && place.len() <= 4,
            "block {}: {place:?}",
            b.block
        );
        assert_eq!(place, sublevels[..place.len()], "block {}", b.block);
        seen += 1;
    }
    assert_eq!(
        seen,
        written
            .iter()
            .find(|w| w.order == "r_month")
            .unwrap()
            .blocks
    );
}

#[test]
fn grouping_on_one_table_is_the_rows_grouped_and_a_count_by_month_reads_no_column_page() {
    let s = sample(40_000);
    let path = snail("groups", &s);
    let mut snail = Snail::open(&path).unwrap();
    let mut by_month = [0i64; 12];
    for &at in &s.observed_at {
        by_month[month_utc(at) as usize - 1] += 1;
    }

    // Every reading by month: the counts hold every month of every block, on both orders.
    let q = format!(
        "SELECT {MONTH} AS month, count(*) AS readings FROM weather.readings r GROUP BY 1 ORDER BY 1"
    );
    for order in ["r_station", "r_month"] {
        let (b, round) = snail.ask(&q, Some(order)).unwrap();
        assert_eq!(
            (round.column_pages_read, round.column_pages_predicted),
            (0, 0),
            "{order}: {round:?}"
        );
        assert_eq!(
            ints64(b.column(0).as_ref()),
            (1..=12).map(Some).collect::<Vec<_>>(),
            "{order}"
        );
        assert_eq!(
            ints64(b.column(1).as_ref()),
            by_month.iter().map(|&n| Some(n)).collect::<Vec<_>>(),
            "{order}"
        );
        assert_eq!(b.schema().field(0).name(), "month");
    }

    // A window written on the clock, by month, the latest first.
    let (from, to) = (
        instant("2022-01-01 00:00:00+00").unwrap(),
        instant("2023-01-01 00:00:00+00").unwrap(),
    );
    let mut in_2022 = [0i64; 12];
    for &at in s.observed_at.iter().filter(|&&at| at >= from && at < to) {
        in_2022[month_utc(at) as usize - 1] += 1;
    }
    let q = format!(
        "SELECT {MONTH}, count(*) FROM weather.readings r \
         WHERE weather.clock(r.observed_at) >= '2022-01-01' AND weather.clock(r.observed_at) < '2023-01-01' \
         GROUP BY 1 ORDER BY 1 DESC"
    );
    for order in ["r_station", "r_month"] {
        let (b, round) = snail.ask(&q, Some(order)).unwrap();
        assert_eq!(
            round.column_pages_read,
            round.column_pages_predicted - round.column_pages_held,
            "{order}: {round:?}"
        );
        assert_eq!(
            ints64(b.column(0).as_ref()),
            (1..=12).rev().map(Some).collect::<Vec<_>>(),
            "{order}"
        );
        assert_eq!(
            ints64(b.column(1).as_ref()),
            in_2022.iter().rev().map(|&n| Some(n)).collect::<Vec<_>>(),
            "{order}"
        );
        assert_eq!(
            (
                b.schema().field(0).name().as_str(),
                b.schema().field(1).name().as_str()
            ),
            ("extract", "count")
        );
    }

    // By network: a count and a sum, the busiest first, then by network.
    let mut by_network: BTreeMap<i64, (i64, i64)> = Default::default();
    for i in 0..s.network_id.len() {
        let e = by_network.entry(s.network_id[i]).or_default();
        e.0 += 1;
        e.1 += s.station_no[i];
    }
    let mut want: Vec<(i64, i64, i64)> = by_network
        .into_iter()
        .map(|(n, (count, sum))| (n, count, sum))
        .collect();
    want.sort_by(|x, y| y.1.cmp(&x.1).then(x.0.cmp(&y.0)));
    let q = "SELECT r.network_id, count(*) AS n, sum(r.station_no) AS stations \
             FROM weather.readings r GROUP BY r.network_id ORDER BY n DESC, 1";
    let (b, round) = snail.ask(q, None).unwrap();
    assert_eq!(
        round.column_pages_read,
        round.column_pages_predicted - round.column_pages_held,
        "{round:?}"
    );
    let got: Vec<(i64, i64, i64)> = (0..b.num_rows())
        .map(|i| {
            (
                ints64(b.column(0).as_ref())[i].unwrap(),
                ints64(b.column(1).as_ref())[i].unwrap(),
                ints64(b.column(2).as_ref())[i].unwrap(),
            )
        })
        .collect();
    assert_eq!(got, want);

    // By feed, text grouped by its bytes; and groups over no row.
    let q = "SELECT r.feed, count(*) FROM weather.readings r GROUP BY r.feed ORDER BY r.feed";
    let (b, _) = snail.ask(q, None).unwrap();
    assert_eq!(counted(&b), tally(&s, &|_| true, &|i| s.feed[i].clone()));
    let (b, _) = snail
        .ask(
            "SELECT count(*), sum(r.station_no) FROM weather.readings r WHERE r.feed = 'nowhere'",
            None,
        )
        .unwrap();
    assert_eq!(
        (ints64(b.column(0).as_ref()), ints64(b.column(1).as_ref())),
        (vec![Some(0)], vec![None])
    );
    let (b, _) = snail
        .ask(
            "SELECT r.feed, count(*) FROM weather.readings r WHERE r.feed = 'nowhere' GROUP BY 1",
            None,
        )
        .unwrap();
    assert_eq!(b.num_rows(), 0);

    // A column neither grouped nor aggregated is refused, as PostgreSQL refuses it.
    let e = snail
        .ask("SELECT r.feed, count(*) FROM weather.readings r", None)
        .unwrap_err()
        .to_string();
    assert!(e.contains("must appear in GROUP BY"), "{e}");
}

/// A join's round: its reads exact, and within its bounds at its top.
fn exact_at_its_top(q: &str, r: &warren_snail::read::Round) {
    exact(q, r);
    assert!(
        r.column_pages_top_low <= r.column_pages_predicted
            && r.column_pages_predicted <= r.column_pages_top_high,
        "{q}: {r:?}"
    );
}

#[test]
fn a_join_keeps_each_side_to_the_keys_the_other_holds() {
    let s = sample(40_000);
    let path = snail("join", &s);
    let mut snail = fresh(&path);

    // 2022's readings by station name: the readings read first, then the stations kept to theirs.
    let (from, to) = (
        instant("2022-01-01 00:00:00+00").unwrap(),
        instant("2023-01-01 00:00:00+00").unwrap(),
    );
    let mut by_name: BTreeMap<String, i64> = Default::default();
    for i in (0..s.observed_at.len()).filter(|&i| s.observed_at[i] >= from && s.observed_at[i] < to)
    {
        *by_name
            .entry(name(s.network_id[i], s.station_no[i]))
            .or_default() += 1;
    }
    let mut want: Vec<(String, i64)> = by_name.into_iter().collect();
    want.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let q = "SELECT s.name, count(*) AS readings FROM weather.readings r \
             JOIN weather.stations s ON s.network_id = r.network_id AND s.station_no = r.station_no \
             WHERE weather.clock(r.observed_at) >= '2022-01-01' AND weather.clock(r.observed_at) < '2023-01-01' \
             GROUP BY s.name ORDER BY readings DESC, s.name";
    let (b, round) = snail.ask(q, None).unwrap();
    exact_at_its_top(q, &round);
    assert!(
        round.order.starts_with("r_")
            && (round.order.ends_with("+stations_pkey") || round.order.ends_with("+s_name")),
        "{round:?}"
    );
    assert_eq!(counted(&b), want);

    // One name's readings by month: its stations read first, the readings kept to them.
    let commonest = commonest_name(&s);
    let mut months = [0i64; 12];
    for i in
        (0..s.network_id.len()).filter(|&i| name(s.network_id[i], s.station_no[i]) == commonest)
    {
        months[month_utc(s.observed_at[i]) as usize - 1] += 1;
    }
    let q = format!(
        "SELECT {MONTH} AS month, count(*) AS readings \
         FROM weather.stations s JOIN weather.readings r ON r.network_id = s.network_id AND r.station_no = s.station_no \
         WHERE s.name = '{commonest}' GROUP BY 1 ORDER BY 1"
    );
    let (b, round) = snail.ask(&q, None).unwrap();
    exact_at_its_top(&q, &round);
    assert!(
        round.order.starts_with("s_name+"),
        "the index on the name finds the stations: {round:?}"
    );
    let got: Vec<(i64, i64)> = ints64(b.column(0).as_ref())
        .into_iter()
        .map(Option::unwrap)
        .zip(ints64(b.column(1).as_ref()).into_iter().map(Option::unwrap))
        .collect();
    let want: Vec<(i64, i64)> = (1..=12).zip(months).filter(|(_, n)| *n > 0).collect();
    assert_eq!(got, want);
    // Named the other way round, the stations its condition narrows are still read first.
    let q = format!(
        "SELECT {MONTH} AS month, count(*) AS readings \
         FROM weather.readings r JOIN weather.stations s ON s.network_id = r.network_id AND s.station_no = r.station_no \
         WHERE s.name = '{commonest}' GROUP BY 1 ORDER BY 1"
    );
    let (b, round) = snail.ask(&q, None).unwrap();
    exact_at_its_top(&q, &round);
    assert!(
        round.order.starts_with("s_name+"),
        "the index on the name finds the stations: {round:?}"
    );
    assert_eq!(
        ints64(b.column(1).as_ref())
            .into_iter()
            .map(Option::unwrap)
            .sum::<i64>(),
        want.iter().map(|w| w.1).sum::<i64>()
    );

    // One network's readings with their station's name: one block of stations, then only the
    // reading blocks that hold the network's rows.
    let network = s.network_id[0];
    let mut want: Vec<(i64, String)> = (0..s.network_id.len())
        .filter(|&i| s.network_id[i] == network)
        .map(|i| (s.reading_id[i], name(s.network_id[i], s.station_no[i])))
        .collect();
    want.sort();
    let q = format!(
        "SELECT r.reading_id, s.name FROM weather.stations s \
         JOIN weather.readings r ON r.network_id = s.network_id AND r.station_no = s.station_no \
         WHERE s.network_id = {network}"
    );
    let (b, round) = snail.ask(&q, None).unwrap();
    exact_at_its_top(&q, &round);
    assert!(round.blocks_read <= 3, "{round:?}");
    // Within those blocks only the pages that hold the network's keys are read.
    let whole: u64 = snail
        .blocks()
        .unwrap()
        .iter()
        .filter(|r| {
            r.order == "r_station"
                && ["reading_id", "network_id", "station_no"].contains(&r.column.as_str())
        })
        .filter(|r| numbers(&r.first)[0] <= network && network <= numbers(&r.last)[0])
        .map(|r| r.disk_pages)
        .sum();
    let readings = round
        .consulted
        .iter()
        .find(|c| c.order == "r_station" && c.exact.is_some())
        .unwrap();
    assert!(
        readings.exact.unwrap() < whole,
        "{readings:?} against {whole} pages whole"
    );
    let ids = ints64(b.column(0).as_ref());
    let names = texts(b.column(1).as_ref());
    let mut got: Vec<(i64, String)> = (0..b.num_rows())
        .map(|i| (ids[i].unwrap(), names[i].clone().unwrap()))
        .collect();
    got.sort();
    assert_eq!(got, want);

    // What a join does not read is refused with its reason.
    for (q, why) in [
        ("SELECT count(*) FROM weather.readings r JOIN weather.stations s ON s.name = r.feed", "a join on"),
        ("SELECT count(*) FROM weather.readings r LEFT JOIN weather.stations s ON s.network_id = r.network_id", "inner JOIN"),
        ("SELECT network_id FROM weather.readings r JOIN weather.stations s ON s.network_id = r.network_id", "qualify it"),
        ("SELECT count(*) FROM weather.readings r JOIN weather.stations s ON r.station_no = 3", "joined on no key"),
    ] {
        let e = snail.ask(q, None).unwrap_err().to_string();
        assert!(e.contains(why), "{q}: {e}");
    }
}

#[test]
fn an_infinite_instant_is_carried_and_refused_only_on_a_level() {
    let catalog = Catalog::parse(DDL).unwrap();
    let path = tmp("infinite.snail");
    let mut s = sample(3000);
    s.checked_at[5] = Some(i64::MAX);
    s.checked_at[6] = Some(i64::MIN);
    write(&path, &catalog, &tables(&s)).unwrap();
    let mut snail = Snail::open(&path).unwrap();
    for (i, want) in [(5, i64::MAX), (6, i64::MIN)] {
        let q = format!(
            "SELECT r.checked_at FROM weather.readings r WHERE r.reading_id = {}",
            s.reading_id[i]
        );
        let (b, _) = snail.ask(&q, None).unwrap();
        assert_eq!(timestamps(b.column(0).as_ref()), [Some(want)]);
    }
    // An infinite instant has no month, as PostgreSQL extracts it, and no condition on its month
    // holds.
    let month = "extract(month FROM weather.clock(r.checked_at))::smallint";
    for i in [5, 6] {
        let id = s.reading_id[i];
        let q = format!("SELECT {month} AS m FROM weather.readings r WHERE r.reading_id = {id}");
        let (b, _) = snail.ask(&q, None).unwrap();
        assert_eq!((b.num_rows(), b.column(0).null_count()), (1, 1), "{q}");
        let q = format!(
            "SELECT r.reading_id FROM weather.readings r \
             WHERE {month} >= 1 AND {month} <= 12 AND r.reading_id = {id}"
        );
        assert_eq!(snail.ask(&q, None).unwrap().0.num_rows(), 0, "{q}");
    }

    // Where the month's index counts its months, an infinite instant is refused.
    s.observed_at[5] = i64::MAX;
    let _ = std::fs::remove_file(&path);
    let e = write(&path, &catalog, &tables(&s)).unwrap_err().to_string();
    assert!(
        e.contains("observed_at holds an infinite instant, whose month cannot be counted"),
        "{e}"
    );

    // Keyed on an instant whose months nothing counts, the level itself refuses it.
    let keyed =
        Catalog::parse("CREATE TABLE s.t (at timestamptz NOT NULL, PRIMARY KEY (at));").unwrap();
    let _ = std::fs::remove_file(&path);
    let rows = Rows {
        columns: vec![Values::Int(vec![Some(0), Some(i64::MAX)])],
    };
    let e = write(&path, &keyed, &[("s.t".into(), rows)])
        .unwrap_err()
        .to_string();
    assert!(e.contains("which a level of t_pkey cannot place"), "{e}");
}

#[test]
fn text_keys_an_index_and_an_equality_on_it_reads_only_its_place() {
    let s = sample(40_000);
    let path = snail("name_index", &s);
    let mut snail = Snail::open(&path).unwrap();
    let commonest = commonest_name(&s);
    let mut want: Vec<(i64, i64)> = stations_of(&s)
        .into_iter()
        .filter(|p| name(p.0, p.1) == commonest)
        .collect();
    want.sort();

    // An equality on the index's text level fixes it: its own block, a page of each column.
    let q = format!(
        "SELECT s.network_id, s.station_no FROM weather.stations s WHERE s.name = '{commonest}'"
    );
    for order in ["s_name", "stations_pkey"] {
        let (b, round) = snail.ask(&q, Some(order)).unwrap();
        assert_eq!(
            round.column_pages_read,
            round.column_pages_predicted - round.column_pages_held,
            "{order}: {round:?}"
        );
        let (networks, numbers) = (ints64(b.column(0).as_ref()), ints64(b.column(1).as_ref()));
        let mut got: Vec<(i64, i64)> = (0..b.num_rows())
            .map(|i| (networks[i].unwrap(), numbers[i].unwrap()))
            .collect();
        got.sort();
        assert_eq!(got, want, "{order}");
        if order == "s_name" {
            assert!(
                round.blocks_read <= 2 && round.column_pages_predicted <= 6,
                "{round:?}"
            );
        }
    }
    let (_, round) = snail.ask(&q, None).unwrap();
    assert_eq!(round.order, "s_name", "{round:?}");
    // The index is cut on the name's bytes past the first: some place's second sublevel is a byte of
    // a name, where a cut on the first byte alone goes on to the network's bits.
    let second: Vec<i64> = snail
        .blocks()
        .unwrap()
        .iter()
        .filter(|b| b.order == "s_name" && b.column == "name")
        .filter_map(|b| numbers(&b.place).get(1).copied())
        .collect();
    assert!(second.iter().any(|&b| b >= b'-' as i64), "{second:?}");

    // A name no station has reads no column page.
    let q = "SELECT s.network_id FROM weather.stations s WHERE s.name = 'no such station'";
    let (b, round) = snail.ask(q, Some("s_name")).unwrap();
    assert_eq!(
        (b.num_rows(), round.column_pages_predicted),
        (0, 0),
        "{round:?}"
    );

    // A range of bytes on the level is read as a condition, and is still answered whole.
    let q = "SELECT s.name, count(*) FROM weather.stations s \
             WHERE s.name >= 'Lisboa' AND s.name < 'Lisbon' GROUP BY 1 ORDER BY 1";
    let (b, round) = snail.ask(q, Some("s_name")).unwrap();
    assert_eq!(
        round.column_pages_read,
        round.column_pages_predicted - round.column_pages_held,
        "{round:?}"
    );
    let mut counts: BTreeMap<String, i64> = Default::default();
    for p in stations_of(&s) {
        let v = name(p.0, p.1);
        if v.as_str() >= "Lisboa" && v.as_str() < "Lisbon" {
            *counts.entry(v).or_default() += 1;
        }
    }
    assert_eq!(counted(&b), counts.into_iter().collect::<Vec<_>>());
}

#[cfg(feature = "standalone")]
#[test]
fn a_snail_held_open_reads_again_only_the_root_and_what_did_not_fit() {
    let s = sample(40_000);
    let path = snail("held", &s);
    let q = format!(
        "SELECT s.name, count(*) AS readings FROM weather.readings r \
         JOIN weather.stations s ON s.network_id = r.network_id AND s.station_no = r.station_no \
         WHERE {SPAN} GROUP BY s.name ORDER BY 1"
    );

    let mut snail = Snail::open(&path).unwrap();
    let (a1, r1) = snail.ask(&q, None).unwrap();
    assert_eq!((snail.pages_held(), r1.column_pages_held), (0, 0), "{r1:?}");
    let read = snail.pages_read();
    let (a2, r2) = snail.ask(&q, None).unwrap();
    assert_eq!(a2, a1);
    assert_eq!(
        (r2.pages_at_start, snail.pages_read() - read),
        (2, 2),
        "{r2:?}"
    );
    assert_eq!(
        (
            r2.order.as_str(),
            r2.directory_pages,
            r2.column_pages_predicted
        ),
        (
            r1.order.as_str(),
            r1.directory_pages,
            r1.column_pages_predicted
        )
    );
    assert_eq!(
        (
            r2.directory_pages_held,
            r2.column_pages_held,
            r2.column_pages_read
        ),
        (r2.directory_pages, r2.column_pages_predicted, 0)
    );

    // Holding half of what the first round touched, past the root: the second round reads the rest
    // from the file, and its column pages exactly as predicted.
    let touched = snail.pages_at_open - 2 + r1.directory_pages + r1.column_pages_predicted;
    let cap = touched / 2;
    let mut small = Snail::open_holding(&path, cap as usize * warren_snail::cut::PAGE).unwrap();
    small.ask(&q, None).unwrap();
    let read = small.pages_read();
    let (b2, s2) = small.ask(&q, None).unwrap();
    assert_eq!(b2, a1);
    assert_eq!(small.pages_read() - read, 2 + touched - cap, "{s2:?}");
    assert_eq!(
        s2.column_pages_read,
        s2.column_pages_predicted - s2.column_pages_held,
        "{s2:?}"
    );
    assert!(
        s2.column_pages_held > 0 && s2.column_pages_read > 0,
        "{s2:?}"
    );
}

#[cfg(feature = "standalone")]
#[test]
fn a_stand_alone_round_prefers_the_order_whose_pages_it_holds() {
    let s = sample(40_000);
    let path = snail("prefers", &s);
    let q = format!("SELECT r.checked_at FROM weather.readings r WHERE {MONTH} = 12 AND {SPAN}");
    let (first_answer, first) = Snail::open(&path).unwrap().ask(&q, None).unwrap();
    assert_eq!(first.order, "r_month", "{first:?}");

    let mut snail = Snail::open(&path).unwrap();
    let (_, along) = snail.ask(&q, Some("r_station")).unwrap();
    assert!(
        along.column_pages_predicted > first.column_pages_predicted,
        "{along:?}"
    );
    let (b, round) = snail.ask(&q, None).unwrap();
    assert_eq!(round.order, "r_station", "{round:?}");
    assert_eq!(
        (round.column_pages_held, round.column_pages_read),
        (round.column_pages_predicted, 0),
        "{round:?}"
    );
    let r_station = round
        .consulted
        .iter()
        .find(|c| c.order == "r_station")
        .unwrap();
    assert_eq!(r_station.exact, Some(r_station.held), "{round:?}");
    let sorted = |a: &dyn arrow::array::Array| {
        let mut v = timestamps(a);
        v.sort();
        v
    };
    assert_eq!(
        sorted(b.column(0).as_ref()),
        sorted(first_answer.column(0).as_ref())
    );
}

#[cfg(feature = "standalone")]
#[test]
fn a_stand_alone_round_counts_no_directory_page_it_holds_against_an_order() {
    let s = sample(40_000);
    let path = snail("held_directory", &s);
    let id = s.reading_id[20_000];
    let q = format!("SELECT r.reading_id FROM weather.readings r WHERE r.reading_id = {id}");
    let (_, first) = Snail::open(&path).unwrap().ask(&q, None).unwrap();
    assert_eq!(first.order, "readings_pkey", "{first:?}");
    let key = first
        .consulted
        .iter()
        .find(|c| c.order == "readings_pkey")
        .unwrap();

    // Along r_station the point read takes every block, and its directory alone is more pages
    // than the primary key reads in all.
    let mut snail = Snail::open(&path).unwrap();
    let (_, along) = snail.ask(&q, Some("r_station")).unwrap();
    assert!(
        along.directory_pages > key.directory_pages + key.high,
        "{along:?} {first:?}"
    );
    let (b, round) = snail.ask(&q, None).unwrap();
    assert_eq!(round.order, "r_station", "{round:?}");
    assert_eq!(
        (round.column_pages_held, round.column_pages_read),
        (round.column_pages_predicted, 0),
        "{round:?}"
    );
    assert_eq!(ints64(b.column(0).as_ref()), [Some(id)]);
}

#[test]
fn text_read_from_a_dictionary_groups_and_joins_as_its_text() {
    let s = sample(40_000);
    let path = snail("dictionary", &s);
    let mut snail = fresh(&path);
    let got = |b: &arrow::array::RecordBatch, keys: usize| -> Vec<(String, i64)> {
        let n = ints64(b.column(keys).as_ref());
        (0..b.num_rows())
            .map(|r| {
                let key: Vec<String> = (0..keys)
                    .map(|k| texts_or_ints(b.column(k).as_ref(), r).unwrap_or("NULL".into()))
                    .collect();
                (key.join("|"), n[r].unwrap())
            })
            .collect()
    };
    let feed = |i: usize| s.feed[i].clone();

    // Every block of the table's order has a dictionary of its own for the feed, so the groups
    // cross dictionaries.
    let q = format!(
        "SELECT r.feed, count(*) AS n FROM weather.readings r WHERE {SPAN} GROUP BY r.feed ORDER BY 1"
    );
    let (b, round) = snail.ask(&q, Some("r_station")).unwrap();
    assert!(round.blocks_read > 1, "{round:?}");
    assert_eq!(got(&b, 1), tally(&s, &|_| true, &feed));

    // A code beside another key.
    let q = format!(
        "SELECT r.feed, {MONTH} AS m, count(*) AS n FROM weather.readings r WHERE {SPAN} GROUP BY r.feed, {MONTH}"
    );
    let (b, _) = snail.ask(&q, Some("r_station")).unwrap();
    let mut both = got(&b, 2);
    both.sort();
    assert_eq!(
        both,
        tally(&s, &|_| true, &|i| format!(
            "{}|{}",
            s.feed[i],
            month_utc(s.observed_at[i])
        ))
    );

    // Joined: the feed read on the second side, then on the first.
    let few = 40;
    let want = tally(&s, &|i| s.network_id[i] <= few, &feed);
    let q = format!(
        "SELECT r.feed, count(*) AS n FROM weather.stations s \
         JOIN weather.readings r ON s.network_id = r.network_id AND s.station_no = r.station_no \
         WHERE s.network_id <= {few} AND {SPAN} GROUP BY r.feed ORDER BY 1"
    );
    let (b, round) = snail.ask(&q, None).unwrap();
    assert!(round.order.starts_with("stations"), "{round:?}");
    assert_eq!(got(&b, 1), want);
    let q = format!(
        "SELECT r.feed, count(*) AS n FROM weather.readings r \
         JOIN weather.stations s ON s.network_id = r.network_id AND s.station_no = r.station_no \
         WHERE r.network_id <= {few} AND {SPAN} GROUP BY r.feed ORDER BY 1"
    );
    let (b, round) = snail.ask(&q, None).unwrap();
    assert!(!round.order.starts_with("stations"), "{round:?}");
    assert_eq!(got(&b, 1), want);
}

#[test]
fn a_snail_cut_into_files_reads_as_one_file_does_and_a_block_written_whole_lies_in_one() {
    let s = sample(20_000);
    let one_file = snail_spanned("files-one", &s, warren_snail::file::FILE_PAGES);
    let many = snail_spanned("files-many", &s, 64);
    let mut second = many.as_os_str().to_owned();
    second.push(".1");
    assert!(std::path::PathBuf::from(second).exists());
    let (mut a, mut b) = (fresh(&one_file), fresh(&many));
    let c = b.census().unwrap();
    assert!(
        c.free > 0,
        "the pages passed over at a file's end are free: {c:?}"
    );
    let listed = |s: &Snail| -> Vec<(String, String, u64, u64, String, u64, u64)> {
        s.blocks()
            .unwrap()
            .into_iter()
            .map(|x| {
                (
                    x.table,
                    x.order,
                    x.block,
                    x.rows,
                    x.column,
                    x.data_pages,
                    x.disk_pages,
                )
            })
            .collect()
    };
    assert_eq!(listed(&a), listed(&b));
    assert!(b.blocks().unwrap().iter().all(|x| x.files == 1));
    let order = in_order(&s);
    let (network, station, _) = order[700];
    for q in [
        "SELECT count(*) FROM weather.readings r".to_string(),
        "SELECT r.feed, count(*) FROM weather.readings r GROUP BY r.feed".to_string(),
        format!(
            "SELECT r.reading_id, r.feed FROM weather.readings r \
             WHERE r.network_id = {network} AND r.station_no = {station}"
        ),
        "SELECT s.name, count(*) FROM weather.stations s GROUP BY s.name".to_string(),
    ] {
        let (x, rx) = a.ask(&q, None).unwrap();
        let (y, ry) = b.ask(&q, None).unwrap();
        assert_eq!(bag(&x), bag(&y), "{q}");
        assert_eq!(
            (rx.column_pages_read, rx.column_pages_predicted),
            (ry.column_pages_read, ry.column_pages_predicted),
            "{q}"
        );
        assert_eq!(ry.column_pages_read, ry.column_pages_predicted, "{q}");
    }
}

#[test]
fn a_join_kept_to_few_keys_walks_their_rows_alone_and_a_count_by_code_takes_runs_whole() {
    let s = sample(20_000);
    let path = snail("walked", &s);
    let mut r = fresh(&path);
    // The name with the fewest readings.
    let (names, _) = r
        .ask(
            "SELECT s.name, count(*) FROM weather.readings r \
             JOIN weather.stations s ON s.network_id = r.network_id AND s.station_no = r.station_no \
             GROUP BY s.name",
            None,
        )
        .unwrap();
    let (rarest, n) = bag(&names)
        .into_iter()
        .map(|row| {
            (
                row[0].clone().unwrap(),
                row[1].clone().unwrap().parse::<u64>().unwrap(),
            )
        })
        .min_by_key(|(_, n)| *n)
        .unwrap();
    let (by_month, round) = r
        .ask(
            &format!(
                "SELECT {MONTH} AS m, count(*) FROM weather.stations s \
                 JOIN weather.readings r ON r.network_id = s.network_id AND r.station_no = s.station_no \
                 WHERE s.name = '{rarest}' GROUP BY 1"
            ),
            None,
        )
        .unwrap();
    let total: u64 = bag(&by_month)
        .iter()
        .map(|row| row[1].clone().unwrap().parse::<u64>().unwrap())
        .sum();
    assert_eq!(total, n);
    // The readings walked are the name's stations' alone, not every row of the pages their keys
    // meet.
    assert!(
        round.rows_walked < 2_000,
        "{} rows walked",
        round.rows_walked
    );
    // Every reading by feed: each block's run of codes taken at once.
    let (feeds, round) = r
        .ask(
            "SELECT r.feed, count(*), sum(r.station_no) FROM weather.readings r GROUP BY r.feed",
            None,
        )
        .unwrap();
    let mut want: BTreeMap<String, (u64, i64)> = Default::default();
    for (feed, station) in s.feed.iter().zip(&s.station_no) {
        let e = want.entry(feed.clone()).or_default();
        e.0 += 1;
        e.1 += station;
    }
    let got: BTreeMap<String, (u64, i64)> = bag(&feeds)
        .into_iter()
        .map(|row| {
            (
                row[0].clone().unwrap(),
                (
                    row[1].clone().unwrap().parse().unwrap(),
                    row[2].clone().unwrap().parse().unwrap(),
                ),
            )
        })
        .collect();
    assert_eq!(got, want);
    assert_eq!((round.rows_walked, round.rows_taken), (0, 20_000));
}

#[test]
fn a_blocks_page_map_shares_a_page_with_its_neighbours_and_a_round_reads_each_such_page_once() {
    let s = sample(20_000);
    let path = tmp("packed.snail");
    let catalog = Catalog::parse(DDL).unwrap();
    let written = write(&path, &catalog, &tables(&s)).unwrap();
    let readings = written.iter().find(|w| w.order == "r_station").unwrap();
    // The blocks' maps lie in fewer pages than there are blocks.
    assert!(readings.blocks > 4, "{readings:?}");
    assert!(readings.map_pages < readings.blocks as u64, "{readings:?}");
    // Read whole along the table's order, a round reads the directory and each page of maps once.
    let q = "SELECT r.feed, count(*) FROM weather.readings r GROUP BY r.feed";
    let (_, round) = fresh(&path).ask(q, Some("r_station")).unwrap();
    exact(q, &round);
    assert_eq!(round.blocks_read, readings.blocks as u64);
    assert_eq!(
        round.directory_pages,
        readings.index_pages + readings.map_pages,
        "{round:?}"
    );
}
