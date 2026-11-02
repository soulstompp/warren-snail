//! A question priced as one rescan in which some of its columns each take one value, against the
//! rounds read for the values the table holds.

mod weather;

use weather::*;

/// The keys of the sample's stations that lie in an order's middle block: from its first key up to
/// the next block's.
fn in_middle_block(s: &Sample, snail: &warren_snail::read::Snail, order: &str) -> Vec<(i64, i64)> {
    let blocks = snail
        .blocks()
        .unwrap()
        .iter()
        .filter(|b| b.order == order)
        .map(|b| b.block)
        .max()
        .unwrap()
        + 1;
    let from = blocks_first(snail, order, blocks / 2);
    let to = blocks_first(snail, order, blocks / 2 + 1);
    stations_of(s)
        .into_iter()
        .filter(|&k| from <= k && k < to)
        .collect()
}

/// Along a key the order holds once, every value of the middle block is read in exactly the pages its
/// point price says, and along a key it holds many times, in its directory pages: the point is the
/// round a rescan reads.
#[test]
fn a_rescan_is_priced_as_the_point_read_of_its_order_s_middle_block() {
    let s = sample(20_000);
    let path = snail("point_priced", &s);
    let snail = fresh(&path);
    let p = snail
        .price_point(
            "SELECT s.name FROM weather.stations s",
            &["network_id", "station_no"],
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        (p.order.as_str(), p.levels, p.unique),
        ("stations_pkey", 2, true)
    );
    let keys = in_middle_block(&s, &snail, "stations_pkey");
    assert!(keys.len() > 100, "{}", keys.len());
    for (n, st) in keys {
        let q = format!(
            "SELECT s.name FROM weather.stations s WHERE s.network_id = {n} AND s.station_no = {st}"
        );
        let (_, r) = fresh(&path).ask(&q, None).unwrap();
        exact(&q, &r);
        assert_eq!(
            (r.order.as_str(), r.directory_pages, r.column_pages_read),
            (p.order.as_str(), p.directory_pages, p.column_pages),
            "{q}"
        );
    }
    let p = snail
        .price_point(
            "SELECT r.reading_id, r.observed_at FROM weather.readings r",
            &["network_id", "station_no"],
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        (p.order.as_str(), p.levels, p.unique),
        ("r_station", 2, false)
    );
    for (n, st) in in_middle_block(&s, &snail, "r_station") {
        let q = format!(
            "SELECT r.reading_id, r.observed_at FROM weather.readings r \
             WHERE r.network_id = {n} AND r.station_no = {st}"
        );
        let (_, r) = fresh(&path).ask(&q, None).unwrap();
        assert_eq!(
            (r.order.as_str(), r.directory_pages),
            (p.order.as_str(), p.directory_pages),
            "{q}"
        );
    }
}

/// A value fixes a level only where it extends the levels the question's own equalities fix along an
/// order that holds every column the question reads; elsewhere a rescan reads what the question alone
/// reads, and nothing is priced as a point.
#[test]
fn a_value_that_fixes_no_further_level_prices_no_point() {
    let s = sample(4_000);
    let path = snail("point_none", &s);
    let snail = fresh(&path);
    let point = |sql: &str, given: &[&str]| snail.price_point(sql, given).unwrap();
    // No order leads with the feed; the key of the readings' strand holds no feed.
    assert_eq!(
        point("SELECT r.reading_id FROM weather.readings r", &["feed"]),
        None
    );
    assert_eq!(
        point("SELECT r.feed FROM weather.readings r", &["reading_id"]),
        None
    );
    // The strand serves a question of its own columns, and holds each key once.
    let p = point(
        "SELECT r.reading_id FROM weather.readings r",
        &["reading_id"],
    )
    .unwrap();
    assert_eq!(
        (p.order.as_str(), p.levels, p.unique),
        ("readings_pkey", 1, true)
    );
    // A network fixed by the question: its station's value fixes the second level; the network's
    // value fixes nothing the question does not. A network no station holds is in no block.
    let network = stations_of(&s)[stations_of(&s).len() / 3].0;
    let fixed = format!("SELECT s.name FROM weather.stations s WHERE s.network_id = {network}");
    let p = point(&fixed, &["station_no"]).unwrap();
    assert_eq!(
        (p.order.as_str(), p.levels, p.unique),
        ("stations_pkey", 2, true)
    );
    assert_eq!(point(&fixed, &["network_id"]), None);
    assert_eq!(
        point(
            "SELECT s.name FROM weather.stations s WHERE s.network_id = 0",
            &["station_no"]
        ),
        None
    );
    // A network's value alone fixes the first of the key's two levels: a rescan finds its stations.
    let p = point("SELECT s.name FROM weather.stations s", &["network_id"]).unwrap();
    assert_eq!(
        (p.order.as_str(), p.levels, p.unique),
        ("stations_pkey", 1, false)
    );
    // A station's number alone leads no order.
    assert_eq!(
        point("SELECT s.name FROM weather.stations s", &["station_no"]),
        None
    );
    assert!(snail
        .price_point("SELECT s.name FROM weather.stations s", &["colour"])
        .is_err());
}

const BUYS: &str = "CREATE FUNCTION s.clock(timestamptz) RETURNS timestamp IMMUTABLE LANGUAGE sql \
                    AS $$ SELECT $1 AT TIME ZONE 'UTC' $$;
                    CREATE TABLE s.b (buy_id bigint NOT NULL, set_id integer NOT NULL, paid integer, \
                    at timestamptz NOT NULL, PRIMARY KEY (buy_id));
                    CREATE INDEX b_set ON s.b (set_id, buy_id);
                    ALTER TABLE s.b CLUSTER ON b_set;
                    CREATE INDEX b_month ON s.b ((extract(month FROM s.clock(at))::smallint), at);";

/// Rows `n`: each buy's set a hash of it, about thirteen rows a set of 3,000.
fn buys() -> warren_snail::write::Rows {
    let n = 40_000i64;
    warren_snail::write::Rows {
        columns: vec![
            warren_snail::values::Values::Int((1..=n).map(Some).collect()),
            warren_snail::values::Values::Int(
                (1..=n)
                    .map(|i| Some((i * 2_654_435_761 & 0x7fff_ffff) % 3000 + 1))
                    .collect(),
            ),
            warren_snail::values::Values::Int(
                (1..=n).map(|i| (i % 7 != 0).then_some(i % 100)).collect(),
            ),
            warren_snail::values::Values::Int((1..=n).map(|i| Some(i * 2_220_000_000)).collect()),
        ],
    }
}

/// A rescan is priced on a key of the block the order's middle row lies in, however its directory
/// packs its entries, so that the price is what a rescan reads and the last block, holding the
/// thin keys joined at the order's end, never stands for the rest. In format 9, whose entries are
/// smaller, and in format 8.
#[test]
fn a_rescan_is_priced_where_the_order_s_middle_row_lies_in_either_format() {
    let catalog = warren_snail::catalog::Catalog::parse(BUYS).unwrap();
    let nine = tmp("point-buys-9.snail");
    write(&nine, &catalog, &[("s.b".into(), buys())]).unwrap();
    let eight = tmp("point-buys-8.snail");
    warren_snail::write::write_format_eight(&eight, &catalog, &[("s.b".into(), buys())], span())
        .unwrap();
    for (path, format) in [(nine, 9), (eight, 8)] {
        let snail = fresh(&path);
        let mut rounds = std::collections::BTreeMap::<u64, u64>::new();
        for k in 1..=3000 {
            let q = format!("SELECT b.set_id FROM s.b b WHERE b.set_id = {k}");
            *rounds
                .entry(snail.price(&q, None).unwrap().column_pages)
                .or_insert(0) += 1;
        }
        let commonest = rounds.iter().max_by_key(|(_, keys)| **keys).unwrap().0;
        let point = snail
            .price_point("SELECT b.set_id FROM s.b b", &["set_id"])
            .unwrap()
            .unwrap();
        assert_eq!(
            point.column_pages, *commonest,
            "format {format}: {point:?} {rounds:?}"
        );
    }
}
