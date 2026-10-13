//! A round handed out a block at a time, and a question priced from the directory alone, against the
//! round `ask` reads.

mod weather;

use weather::*;

/// The questions on one table: rows along every kind of order, a count, groups.
fn questions(s: &Sample) -> Vec<String> {
    let (network, station) = busiest(s);
    let id = in_order(s)[700].2;
    vec![
        format!("SELECT r.checked_at FROM weather.readings r WHERE {MONTH} = 12 AND {SPAN}"),
        format!("SELECT count(*) FROM weather.readings r WHERE {MONTH} = 12 AND {SPAN}"),
        format!(
            "SELECT r.reading_id, r.observed_at, r.checked_at FROM weather.readings r \
             WHERE r.network_id = {network} AND r.station_no = {station}"
        ),
        format!("SELECT r.feed FROM weather.readings r WHERE r.reading_id = {id}"),
        "SELECT r.reading_id, r.feed FROM weather.readings r".to_string(),
        "SELECT r.feed, count(*) FROM weather.readings r GROUP BY r.feed ORDER BY r.feed"
            .to_string(),
        format!(
            "SELECT s.station_no, s.name FROM weather.stations s WHERE s.network_id = {network}"
        ),
        "SELECT s.network_id FROM weather.stations s WHERE s.name = 'no such station'".to_string(),
    ]
}

#[test]
fn a_round_handed_out_a_block_at_a_time_is_the_round_ask_reads() {
    let s = sample(20_000);
    let path = snail("scanned", &s);
    for q in questions(&s) {
        let (want, asked) = fresh(&path).ask(&q, None).unwrap();
        let snail = fresh(&path);
        let mut scan = snail.scan(&q, None).unwrap();
        let mut rows: Vec<Vec<Option<String>>> = Vec::new();
        let mut batches = 0;
        while let Some(b) = snail.next_block(&mut scan).unwrap() {
            rows.extend(bag(&b));
            batches += 1;
        }
        assert!(snail.next_block(&mut scan).unwrap().is_none(), "{q}");
        rows.sort();
        assert_eq!(rows, bag(&want), "{q}");
        let round = scan.round();
        assert_eq!(
            (
                round.order.as_str(),
                round.directory_pages,
                round.column_pages_predicted,
                round.column_pages_read,
                round.rows,
            ),
            (
                asked.order.as_str(),
                asked.directory_pages,
                asked.column_pages_predicted,
                asked.column_pages_read,
                asked.rows,
            ),
            "{q}"
        );
        // A question of rows comes a block at a time; one that groups or counts, in one batch.
        if q.contains("count(") {
            assert_eq!(batches, 1, "{q}");
        } else {
            assert_eq!(batches as u64, asked.blocks_read, "{q}");
        }
    }
}

#[test]
fn a_question_priced_reads_no_column_page_and_bounds_its_rows() {
    let s = sample(20_000);
    let path = snail("priced", &s);
    for q in questions(&s) {
        let (answer, asked) = fresh(&path).ask(&q, None).unwrap();
        let snail = fresh(&path);
        let before = snail.pages_read();
        let price = snail.price(&q, None).unwrap();
        assert_eq!(snail.pages_read() - before, price.directory_pages, "{q}");
        assert_eq!(
            (
                price.order.as_str(),
                price.directory_pages,
                price.column_pages
            ),
            (
                asked.order.as_str(),
                asked.directory_pages,
                asked.column_pages_predicted
            ),
            "{q}"
        );
        assert_eq!(price.blocks, asked.blocks_read, "{q}");
        // The rows the question holds lie within the counts' bounds: an answer's rows where it
        // returns rows, its count where it counts.
        let held = if q.starts_with("SELECT count(*)") {
            one(&answer) as u64
        } else if q.contains("count(") {
            continue;
        } else {
            answer.num_rows() as u64
        };
        assert!(
            price.rows_low <= held && held <= price.rows_high,
            "{q}: {} ≤ {held} ≤ {}",
            price.rows_low,
            price.rows_high
        );
        assert!(price.rows_high <= price.table_rows, "{q}");
    }
    // A count the counts answer is bounded exactly.
    let q = format!("SELECT count(*) FROM weather.readings r WHERE {MONTH} = 12 AND {SPAN}");
    let price = fresh(&path).price(&q, None).unwrap();
    let (n, _) = fresh(&path).ask(&q, None).unwrap();
    assert_eq!(
        (price.rows_low, price.rows_high),
        (one(&n) as u64, one(&n) as u64)
    );
    let all = fresh(&path)
        .price("SELECT r.reading_id FROM weather.readings r", None)
        .unwrap();
    assert_eq!(
        (all.rows_low, all.rows_high, all.table_rows),
        (20_000, 20_000, 20_000)
    );
}

#[test]
fn a_question_on_two_tables_is_neither_priced_nor_scanned_alone() {
    let s = sample(2_000);
    let path = snail("scanned-join", &s);
    let q = "SELECT r.reading_id, s.name FROM weather.stations s \
             JOIN weather.readings r ON r.network_id = s.network_id AND r.station_no = s.station_no";
    let snail = fresh(&path);
    assert!(snail.price(q, None).is_err());
    assert!(snail.scan(q, None).is_err());
}

#[test]
fn a_question_priced_estimates_its_rows_from_the_pages_it_reads() {
    let s = sample(20_000);
    let path = snail("estimated", &s);
    let (network, station) = busiest(&s);
    let ids = in_order(&s);
    let (a, z) = (ids[100].2.min(ids[400].2), ids[100].2.max(ids[400].2));
    let mut questions: Vec<(String, Option<&str>)> =
        questions(&s).into_iter().map(|q| (q, None)).collect();
    questions.extend([
        (
            format!("SELECT r.reading_id FROM weather.readings r WHERE r.reading_id >= {a} AND r.reading_id < {z}"),
            None,
        ),
        (
            format!("SELECT count(*) FROM weather.readings r WHERE r.reading_id >= {a} AND r.reading_id < {z}"),
            None,
        ),
        (
            format!("SELECT r.reading_id FROM weather.readings r WHERE r.network_id = {network} AND r.station_no = {station}"),
            None,
        ),
        // December off the levels of the order read: the month's counts estimate each block.
        (
            format!("SELECT r.reading_id FROM weather.readings r WHERE r.network_id >= {network} AND {MONTH} = 12"),
            Some("r_station"),
        ),
    ]);
    for (q, order) in questions {
        let (answer, _) = fresh(&path).ask(&q, order).unwrap();
        let held = if q.starts_with("SELECT count(*)") {
            one(&answer) as u64
        } else if q.contains("count(") {
            continue;
        } else {
            answer.num_rows() as u64
        };
        let price = fresh(&path).price(&q, order).unwrap();
        let (low, est, high) = (price.rows_low, price.rows_estimate, price.rows_high);
        assert!(low <= est && est <= high, "{q}: {low} ≤ {est} ≤ {high}");
        if low == high {
            assert_eq!(est, low, "{q}");
        }
        // Far closer than the bounds: within a twentieth of the rows between them.
        assert!(
            est.abs_diff(held) * 20 <= high - low,
            "{q}: {est} for {held}, between {low} and {high}"
        );
    }
}

#[test]
fn a_question_priced_names_the_columns_it_holds_no_key_for() {
    let s = sample(2_000);
    let path = snail("keyless", &s);
    let keyless = |q: &str, order: Option<&str>| fresh(&path).price(q, order).unwrap().keyless;
    // Text off the levels of every order: read as a condition.
    assert_eq!(
        keyless(
            "SELECT r.reading_id FROM weather.readings r WHERE r.feed = 'observer'",
            None
        ),
        ["feed"]
    );
    // An equality on a text level keys it; a range on it does not.
    let (network, station) = stations_of(&s)[0];
    let station_name = name(network, station);
    assert!(keyless(
        &format!("SELECT s.station_no FROM weather.stations s WHERE s.name = '{station_name}'"),
        Some("s_name")
    )
    .is_empty());
    assert_eq!(
        keyless(
            "SELECT s.station_no FROM weather.stations s WHERE s.name >= 'F' AND s.name < 'G'",
            Some("s_name")
        ),
        ["name"]
    );
    assert!(keyless(
        "SELECT r.feed FROM weather.readings r WHERE r.reading_id = 5",
        None
    )
    .is_empty());
}
