//! A sample of a table's rows, each set of them as likely as any other by the counts, read from the
//! column pages that hold them alone.

mod weather;

use std::collections::BTreeSet;
use warren_snail::catalog::Catalog;
use warren_snail::read::Snail;
use warren_snail::values::Values;
use warren_snail::write::Rows;
use weather::{fresh, ints64, tmp, write};

const DDL: &str = "CREATE SCHEMA s; CREATE TABLE s.t (id bigint NOT NULL, w integer NOT NULL, t text NOT NULL, \
                   PRIMARY KEY (id));";

const ROWS: i64 = 20_000;

const QUESTION: &str = "SELECT t.id, t.w, t.t FROM s.t t";

fn w_of(i: i64) -> i64 {
    i * 7919 % 3000
}

fn t_of(i: i64) -> String {
    format!("row number {i}")
}

fn table(name: &str) -> std::path::PathBuf {
    let path = tmp(name);
    let catalog = Catalog::parse(DDL).unwrap();
    let rows = Rows {
        columns: vec![
            Values::Int((0..ROWS).map(Some).collect()),
            Values::Int((0..ROWS).map(|i| Some(w_of(i))).collect()),
            Values::Text((0..ROWS).map(|i| Some(t_of(i))).collect()),
        ],
    };
    write(&path, &catalog, &[("s.t".into(), rows)]).unwrap();
    path
}

/// A seeded source of numbers from 0 up to 1.
fn random(seed: u64) -> impl FnMut() -> f64 {
    let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        (x >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// A sample's rows, each checked to be the row the table holds under its id, and its round.
fn sampled(snail: &Snail, rows: u64, seed: u64) -> (Vec<i64>, u64, warren_snail::read::Round) {
    let mut draw = random(seed);
    let (mut scan, live) = snail.sample(QUESTION, rows, &mut draw).unwrap();
    let mut ids = Vec::new();
    while let Some(b) = snail.next_block(&mut scan).unwrap() {
        let id = ints64(b.column(0).as_ref());
        let w = ints64(b.column(1).as_ref());
        let t = weather::texts(b.column(2).as_ref());
        for r in 0..b.num_rows() {
            let i = id[r].unwrap();
            assert_eq!(w[r].unwrap(), w_of(i), "row {i}");
            assert_eq!(t[r].as_deref(), Some(t_of(i).as_str()), "row {i}");
            ids.push(i);
        }
    }
    (ids, live, scan.round())
}

/// A sample holds as many rows as it asks, each one of the table's, none twice; it reads the pages
/// it predicted, and fewer of them the fewer rows it asks.
#[test]
fn a_sample_holds_the_rows_asked_and_reads_only_their_pages() {
    let path = table("sample-pages.snail");
    let snail = fresh(&path);
    let whole = snail.price(QUESTION, None).unwrap().column_pages;
    let (ids, live, round) = sampled(&snail, 30, 1);
    assert_eq!((ids.len(), live), (30, ROWS as u64));
    assert_eq!(ids.iter().collect::<BTreeSet<_>>().len(), 30);
    assert_eq!(round.column_pages_read, round.column_pages_predicted);
    // A page a row a column at most, far below the whole read.
    let read = round.column_pages_read;
    assert!(read <= 3 * 30 && read < whole / 2, "{read} of {whole}");
    let (_, _, more) = sampled(&snail, 3000, 1);
    assert!(more.column_pages_read > round.column_pages_read);
    assert!(more.column_pages_read <= whole);
}

/// Each row is as likely as any other: over the table's ids in ten equal parts, a sample of 4,000
/// lies within four standard deviations of its share in each part, and two seeds draw other rows.
#[test]
fn a_sample_s_rows_lie_evenly_over_the_table() {
    let path = table("sample-even.snail");
    let snail = fresh(&path);
    let (ids, _, _) = sampled(&snail, 4000, 2);
    assert_eq!(ids.len(), 4000);
    let mut parts = [0u64; 10];
    for i in &ids {
        parts[(*i * 10 / ROWS) as usize] += 1;
    }
    for (p, n) in parts.iter().enumerate() {
        assert!((*n as i64 - 400).abs() < 4 * 19, "part {p}: {n}");
    }
    let (other, _, _) = sampled(&snail, 4000, 3);
    assert_ne!(ids, other);
}

/// A sample of as many rows as the table holds, or more, is every row, the live count exact.
#[test]
fn a_sample_past_the_table_is_every_row() {
    let path = table("sample-all.snail");
    let snail = fresh(&path);
    let (ids, live, _) = sampled(&snail, 2 * ROWS as u64, 4);
    assert_eq!(live, ROWS as u64);
    assert_eq!(ids.iter().collect::<BTreeSet<_>>().len(), ROWS as usize);
    let whole = snail.price(QUESTION, None).unwrap().column_pages;
    assert_eq!(sampled(&snail, ROWS as u64, 4).2.column_pages_read, whole);
}

/// A condition keeps it from the whole table, so a sample of one is refused.
#[test]
fn a_sample_of_a_part_is_refused() {
    let path = table("sample-part.snail");
    let snail = fresh(&path);
    let mut draw = random(5);
    let e = snail
        .sample("SELECT t.id FROM s.t t WHERE t.id < 5", 2, &mut draw)
        .err()
        .unwrap();
    assert!(e.to_string().contains("whole table"), "{e}");
}

/// Only live rows are drawn and counted: rows deleted are in no sample.
#[test]
#[cfg(feature = "standalone")]
fn a_sample_draws_no_dead_row() {
    let path = table("sample-dead.snail");
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute("DELETE FROM s.t WHERE id % 2 = 0").unwrap();
    drop(w);
    let snail = fresh(&path);
    let (ids, live, _) = sampled(&snail, 2 * ROWS as u64, 6);
    assert_eq!(live, ROWS as u64 / 2);
    assert_eq!(ids.len(), ROWS as usize / 2);
    assert!(ids.iter().all(|i| i % 2 == 1));
    let (some, _, _) = sampled(&snail, 500, 7);
    assert_eq!(some.len(), 500);
    assert!(some.iter().all(|i| i % 2 == 1));
}
