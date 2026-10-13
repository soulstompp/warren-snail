//! Statements whose steps read a table along its order: windows over the wind log, its station
//! codes as bytes and its turns as numerics, a table declared into a file that holds others, and a
//! step over text.

mod weather;

use arrow::array::Int64Array;
use warren_snail::catalog::Catalog;
use warren_snail::read::Snail;
use warren_snail::values::Values;
use warren_snail::write::Rows;
use weather::*;

#[test]
fn the_wind_is_read_run_by_run_along_the_log_s_order() {
    let rows = winds(3000);
    let path = wind_snail("wind", &rows);
    let mut snail = Snail::open(&path).unwrap();
    let (b, _) = snail.ask_with(WIND, None, &wind_params()).unwrap();
    let want = turned_as_asked(&rows);
    let winds: std::collections::BTreeSet<String> =
        want.iter().filter_map(|r| r[3].clone()).collect();
    assert_eq!(winds.len(), 4, "every way a run ends: {winds:?}");
    assert_eq!(as_written(&b), want);
    let (s, _) = snail
        .ask("SELECT count(*), sum(w.turn) FROM weather.wind w", None)
        .unwrap();
    let tenths: i64 = rows.iter().map(|r| r.2).sum();
    assert_eq!(
        as_written(&s),
        [[
            Some("3000".to_string()),
            Some(warren_snail::read::numeric_text(tenths as i128, 1))
        ]]
    );
    let e = snail
        .ask(
            "SELECT count(*) FROM weather.wind w WHERE w.station = '\\xff'",
            None,
        )
        .unwrap_err()
        .to_string();
    assert!(e.contains("a condition on bytea"), "{e}");
}

#[test]
fn rows_given_as_arrow_take_bytes_as_bytes_and_a_numeric_from_another_scale() {
    use arrow::array::{ArrayRef, BinaryViewArray, Decimal128Array};
    use std::sync::Arc;
    let catalog = Catalog::parse(WIND_DDL).unwrap();
    let batch = |turns: Vec<i128>| {
        arrow::array::RecordBatch::try_from_iter([
            ("seq", Arc::new(Int64Array::from(vec![1, 2])) as ArrayRef),
            (
                "station",
                Arc::new(BinaryViewArray::from(vec![&b"\x08"[..], &b"\xff\x00"[..]])) as ArrayRef,
            ),
            (
                "turn",
                Arc::new(
                    Decimal128Array::from(turns)
                        .with_precision_and_scale(10, 3)
                        .unwrap(),
                ) as ArrayRef,
            ),
        ])
        .unwrap()
    };
    let rows = Rows::from_batches(&catalog.tables[0], &[batch(vec![12300, -500])]).unwrap();
    let Values::Utf8(stations) = &rows.columns[1] else {
        panic!("bytea is held as bytes")
    };
    let stations: Vec<Vec<u8>> = stations
        .iter()
        .map(|k| k.as_ref().unwrap().data().to_vec())
        .collect();
    assert_eq!(stations, [vec![0x08], vec![0xff, 0x00]]);
    let Values::Int(tenths) = &rows.columns[2] else {
        panic!("a numeric is held as its smallest units")
    };
    assert_eq!(tenths, &[Some(123), Some(-5)]);
    // 12.345 has no place among tenths.
    let e = Rows::from_batches(&catalog.tables[0], &[batch(vec![12345, 0])])
        .err()
        .unwrap()
        .to_string();
    assert!(e.contains("a value is no numeric(5,1)"), "{e}");
}

#[test]
#[cfg(feature = "standalone")]
fn a_table_declared_in_a_file_that_holds_others_takes_rows_and_reads_its_runs() {
    let s = sample(2_000);
    let path = snail("declared", &s);
    let mut w = Snail::open_writing(&path).unwrap();
    // A table declared, then rolled back, leaves the catalog as it was.
    w.execute("BEGIN").unwrap();
    w.execute("CREATE TABLE weather.scratch (a bigint NOT NULL, PRIMARY KEY (a))")
        .unwrap();
    w.execute("ROLLBACK").unwrap();
    assert!(w.catalog.table("weather.scratch").is_none());
    let d = w.execute(WIND_DDL.trim_end_matches(';')).unwrap();
    assert_eq!((d.statement, d.generation), ("CREATE TABLE", 2));
    let rows = winds(3000);
    // The lines arrive in reverse, 250 at a time: the table's order is not their arrival.
    for chunk in rows.chunks(250).rev() {
        let values: Vec<String> = chunk
            .iter()
            .map(|(seq, station, tenths)| {
                let hex: String = station.iter().map(|b| format!("{b:02x}")).collect();
                format!(
                    "({seq}, '\\x{hex}', {})",
                    warren_snail::read::numeric_text(*tenths as i128, 1)
                )
            })
            .collect();
        w.execute(&format!(
            "INSERT INTO weather.wind (seq, station, turn) VALUES {}",
            values.join(", ")
        ))
        .unwrap();
    }
    // An index on a table that holds rows is refused, and so is a turn with more digits than its
    // numeric holds.
    let e = w
        .execute("CREATE INDEX w_turn ON weather.wind (turn, seq)")
        .unwrap_err()
        .to_string();
    assert!(e.contains("holds rows"), "{e}");
    let e = w
        .execute("INSERT INTO weather.wind (seq, station, turn) VALUES (1, '\\x00', 12345.6)")
        .unwrap_err()
        .to_string();
    assert!(e.contains("more digits"), "{e}");
    let want = turned_as_asked(&rows);
    // Each insert cut the block it grew at the commit where one of its parts would fill a page: as
    // many blocks as the same lines written afresh.
    let mut r = Snail::open(&path).unwrap();
    r.census().unwrap();
    let fresh = Snail::open(&wind_snail("wind-fresh", &rows)).unwrap();
    assert!(wind_blocks(&r) > 1);
    assert_eq!(wind_blocks(&r), wind_blocks(&fresh));
    let (b, _) = r.ask_with(WIND, None, &wind_params()).unwrap();
    assert_eq!(as_written(&b), want);
    // Read on its key, the pages read only for rows past a block's sorted ones prune it, and it is
    // cut again by the counts.
    let first = rows[0].0;
    let (n, round) = w
        .ask(
            &format!(
                "SELECT count(*) FROM weather.wind w WHERE w.seq <= {}",
                first + 10
            ),
            None,
        )
        .unwrap();
    assert_eq!(
        one(&n),
        rows.iter().filter(|x| x.0 <= first + 10).count() as i64
    );
    assert_eq!(round.blocks_pruned, 1);
    assert!(wind_blocks(&w) > 1, "{} blocks", wind_blocks(&w));
    // A row written after a block's last lies in its tail; deleted, it is still there. Read whole,
    // the block is read a page for that row alone, and pruned; read whole again, it is not.
    let next = rows.iter().map(|x| x.0).max().unwrap() + 1;
    w.execute(&format!(
        "INSERT INTO weather.wind (seq, station, turn) VALUES ({next}, '\\x00', 0.0)"
    ))
    .unwrap();
    w.execute(&format!("DELETE FROM weather.wind WHERE seq = {next}"))
        .unwrap();
    let (b, round) = w.ask_with(WIND, None, &wind_params()).unwrap();
    assert_eq!(as_written(&b), want);
    assert_eq!(round.blocks_pruned, 1, "{round:?}");
    let (b, round) = w.ask_with(WIND, None, &wind_params()).unwrap();
    assert_eq!(as_written(&b), want);
    assert_eq!(round.blocks_pruned, 0);
    drop(w);
    r.census().unwrap();
    let (b, _) = r.ask_with(WIND, None, &wind_params()).unwrap();
    assert_eq!(as_written(&b), want);
    // The stations' readings are as they were.
    let (n, _) = r
        .ask("SELECT count(*) FROM weather.readings r", None)
        .unwrap();
    assert_eq!(one(&n), 2_000);
}

#[test]
fn a_step_along_the_order_holds_the_rows_of_its_runs_never_of_its_table() {
    let rows = winds(30_000);
    let path = wind_snail("wind-long", &rows);
    let mut snail = Snail::open(&path).unwrap();
    let blocks = wind_blocks(&snail);
    let (b, round) = snail.ask_with(WIND, None, &wind_params()).unwrap();
    assert_eq!(as_written(&b), turned_as_asked(&rows));
    // Read block by block, it holds a block's rows in each step, and its runs: never the table.
    assert!(blocks > 4, "{blocks} blocks");
    assert!(
        round.rows_held > 0 && round.rows_held < rows.len() as u64,
        "{} rows held of {}",
        round.rows_held,
        rows.len()
    );
}

#[test]
fn a_pipeline_over_text_reads_each_block_by_its_own_dictionary() {
    let s = sample(20_000);
    let path = snail("pipeline-text", &s);
    let mut snail = Snail::open(&path).unwrap();
    let (plain, _) = snail
        .ask(
            "SELECT r.feed, count(*) FROM weather.readings r GROUP BY r.feed",
            None,
        )
        .unwrap();
    let (piped, round) = snail
        .ask(
            "WITH f AS (SELECT r.feed AS feed FROM weather.readings r) SELECT feed, count(*) FROM f GROUP BY feed",
            None,
        )
        .unwrap();
    assert!(round.blocks_read > 1);
    assert_eq!(bag(&piped), bag(&plain));
}

#[test]
#[cfg(feature = "standalone")]
fn a_row_of_bytes_and_a_numeric_is_updated_and_deleted() {
    let mut rows = winds(400);
    let path = wind_snail("wind-changed", &rows);
    let mut w = Snail::open_writing(&path).unwrap();
    let (changed, gone) = (rows[10].0, rows[20].0);
    w.execute(&format!(
        "UPDATE weather.wind SET turn = -12.5 WHERE seq = {changed}"
    ))
    .unwrap();
    w.execute(&format!("DELETE FROM weather.wind WHERE seq = {gone}"))
        .unwrap();
    rows[10].2 = -125;
    rows.remove(20);
    let afresh = wind_snail("wind-changed-afresh", &rows);
    let all = "SELECT w.seq, w.station, w.turn FROM weather.wind w";
    let mut got = as_written(&w.ask(all, None).unwrap().0);
    let mut want = as_written(&Snail::open(&afresh).unwrap().ask(all, None).unwrap().0);
    got.sort();
    want.sort();
    assert_eq!(got.len(), 399);
    assert_eq!(got, want);
}
