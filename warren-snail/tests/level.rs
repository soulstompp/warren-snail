//! The rows on each place of the level an order leads with, as the directory counts them, against
//! the rows the table holds.

mod weather;

use std::collections::BTreeMap;
use warren_snail::catalog::Catalog;
use warren_snail::places::KeyValue;
use warren_snail::read::{LevelCounts, Snail};
use warren_snail::values::Values;
use warren_snail::write::Rows;
use weather::{fresh, ints64, tmp, write};

const DDL: &str = "CREATE TABLE s.t (id bigint NOT NULL, k integer, w integer NOT NULL, \
                   m integer NOT NULL, PRIMARY KEY (id));
                   CREATE INDEX t_k ON s.t (k, id);
                   ALTER TABLE s.t CLUSTER ON t_k;
                   CREATE INDEX t_w ON s.t (w, id);
                   CREATE INDEX t_m ON s.t (m, id);";

const ROWS: i64 = 20_000;

/// Row `i`'s k: eight values, the lower ones the commoner, NULL on every ninth row.
fn k_of(i: i64) -> Option<i64> {
    (i % 9 != 0).then(|| [0, 0, 0, 0, 1, 1, 1, 2, 2, 3, 4, 5, 6, 7][(i % 14) as usize])
}

/// Row `i`'s w: 3,000 values, a few rows each.
fn w_of(i: i64) -> i64 {
    i * 7919 % 3000
}

/// Row `i`'s m: three values of thousands of rows each, and forty odd values of ten rows each among them.
fn m_of(i: i64) -> i64 {
    if i % 50 == 0 {
        1 + 2 * ((i / 50) % 40)
    } else {
        2 * (i % 3)
    }
}

fn table(name: &str) -> std::path::PathBuf {
    let path = tmp(name);
    let catalog = Catalog::parse(DDL).unwrap();
    let rows = Rows {
        columns: vec![
            Values::Int((0..ROWS).map(Some).collect()),
            Values::Int((0..ROWS).map(k_of).collect()),
            Values::Int((0..ROWS).map(|i| Some(w_of(i))).collect()),
            Values::Int((0..ROWS).map(|i| Some(m_of(i))).collect()),
        ],
    };
    write(&path, &catalog, &[("s.t".into(), rows)]).unwrap();
    path
}

/// The rows on each value, counted from the plain rows.
fn plain(values: impl Iterator<Item = i64>) -> BTreeMap<i64, u64> {
    let mut out = BTreeMap::new();
    for v in values {
        *out.entry(v).or_insert(0) += 1;
    }
    out
}

/// Each place's key as a number.
fn keyed(l: &LevelCounts) -> Vec<(i64, u64, u64)> {
    l.places
        .as_ref()
        .unwrap()
        .iter()
        .map(|p| match p.key {
            KeyValue::Int(v) => (v, p.low, p.high),
            ref other => panic!("{other:?}"),
        })
        .collect()
}

/// A count by value read from the snail itself.
fn grouped(snail: &mut Snail, column: &str) -> BTreeMap<i64, u64> {
    let (b, _) = snail
        .ask(
            &format!("SELECT t.{column}, count(*) FROM s.t t WHERE t.{column} IS NOT NULL GROUP BY t.{column}"),
            None,
        )
        .unwrap();
    let keys = ints64(b.column(0).as_ref());
    let counts = ints64(b.column(1).as_ref());
    keys.into_iter()
        .zip(counts)
        .map(|(k, n)| (k.unwrap(), n.unwrap() as u64))
        .collect()
}

/// A level whose places each fill blocks of their own is counted from the entries alone, every place
/// exactly, as many rows as the table holds on it, and its NULLs beside them.
#[test]
fn a_level_whose_places_fill_blocks_is_counted_exactly_from_its_entries() {
    let path = table("level-whole.snail");
    let mut snail = fresh(&path);
    let l = snail.level_counts("s.t", "k", u64::MAX).unwrap().unwrap();
    let held = plain((0..ROWS).filter_map(k_of));
    assert_eq!((l.order.as_str(), l.unique), ("t_k", false));
    assert_eq!(l.rows, ROWS as u64);
    assert_eq!(
        l.nulls,
        (0..ROWS).filter(|&i| k_of(i).is_none()).count() as u64
    );
    assert!(l.whole, "{l:?}");
    let counted: BTreeMap<i64, u64> = keyed(&l)
        .into_iter()
        .map(|(v, low, high)| {
            assert_eq!(low, high, "{v}");
            (v, low)
        })
        .collect();
    assert_eq!(counted, held);
    assert_eq!(counted, grouped(&mut snail, "k"));
    // Counted above the blocks, it reads nothing below the order's top, so no page is too many.
    let top = snail.level_counts("s.t", "k", 0).unwrap().unwrap();
    assert_eq!(top.places, l.places);
    assert_eq!(top.directory_pages, l.directory_pages);
}

/// A level whose places lie several to a page is counted down to the pages: each place a page begins
/// on, its rows between the pages that hold it alone and every page that may hold it; and with too few
/// pages given, not at all.
#[test]
fn a_level_whose_places_lie_several_to_a_page_is_bounded_by_its_pages() {
    let path = table("level-pages.snail");
    let snail = fresh(&path);
    let l = snail.level_counts("s.t", "w", u64::MAX).unwrap().unwrap();
    let held = plain((0..ROWS).map(w_of));
    assert_eq!((l.order.as_str(), l.unique, l.nulls), ("t_w", false, 0));
    assert!(!l.whole, "{l:?}");
    let places = keyed(&l);
    assert!(places.windows(2).all(|w| w[0].0 < w[1].0));
    // The ends are listed: the first page's first row and the last block's last.
    assert_eq!((places[0].0, places[places.len() - 1].0), (0, 2999));
    assert!(
        places.len() > 10 && places.len() < held.len(),
        "{}",
        places.len()
    );
    for &(v, low, high) in &places {
        let n = held[&v];
        assert!(low <= n && n <= high, "{v}: {low} {n} {high}");
    }
    let (low, high): (u64, u64) = places.iter().fold((0, 0), |a, p| (a.0 + p.1, a.1 + p.2));
    assert!(low <= ROWS as u64 && ROWS as u64 <= high, "{low} {high}");
    // Its blocks lie below the top: given none of their pages, the places are not read.
    let none = snail.level_counts("s.t", "w", 0).unwrap().unwrap();
    assert_eq!((none.places, none.rows, none.nulls), (None, ROWS as u64, 0));
    assert!(none.directory_pages < l.directory_pages);
}

/// A column that is a key alone is not walked: every row holds its own value.
#[test]
fn a_key_held_once_is_not_walked() {
    let path = table("level-key.snail");
    let snail = fresh(&path);
    let l = snail.level_counts("s.t", "id", u64::MAX).unwrap().unwrap();
    assert_eq!(
        (l.order.as_str(), l.unique, l.places, l.rows, l.nulls),
        ("t_pkey", true, None, ROWS as u64, 0)
    );
    assert_eq!(
        snail.level_counts("s.t", "id", 0).unwrap().unwrap().unique,
        true
    );
    // No order leads with a column no index names first.
    let catalog = Catalog::parse(
        "CREATE TABLE s.u (a integer NOT NULL, b integer NOT NULL, PRIMARY KEY (a, b));",
    )
    .unwrap();
    let path = tmp("level-none.snail");
    let rows = Rows {
        columns: vec![
            Values::Int((0..10).map(Some).collect()),
            Values::Int((0..10).map(Some).collect()),
        ],
    };
    write(&path, &catalog, &[("s.u".into(), rows)]).unwrap();
    assert_eq!(
        fresh(&path).level_counts("s.u", "b", u64::MAX).unwrap(),
        None
    );
}

/// A rescan's key holds the rows its place holds: where a value fills blocks of its own, exactly.
#[test]
fn a_rescan_s_key_holds_the_rows_on_its_place() {
    let path = table("level-point.snail");
    let snail = fresh(&path);
    let p = snail
        .price_point("SELECT t.w FROM s.t t", &["k"])
        .unwrap()
        .unwrap();
    assert_eq!((p.order.as_str(), p.levels, p.unique), ("t_k", 1, false));
    assert_eq!(p.rows_low, p.rows_high, "{p:?}");
    assert_eq!(p.rows_estimate, p.rows_low, "{p:?}");
    let held = plain((0..ROWS).filter_map(k_of));
    assert!(held.values().any(|&n| n == p.rows_low), "{p:?} {held:?}");
    let p = snail
        .price_point("SELECT t.id FROM s.t t", &["id"])
        .unwrap()
        .unwrap();
    // A key held once: the counts bound its rows to the page it lies on, the estimate one.
    assert!(p.unique && p.rows_estimate == 1 && p.rows_low <= 1, "{p:?}");
}

/// Rows deleted leave the counts: a place all of whose rows are gone is no place, and every other
/// holds its live rows, as the snail's own count of them says.
#[test]
#[cfg(feature = "standalone")]
fn rows_deleted_leave_the_counts() {
    let path = table("level-dead.snail");
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute("DELETE FROM s.t WHERE k = 1").unwrap();
    w.execute("DELETE FROM s.t WHERE k = 0 AND id < 5000")
        .unwrap();
    drop(w);
    let mut snail = fresh(&path);
    let l = snail.level_counts("s.t", "k", u64::MAX).unwrap().unwrap();
    let live = |i: &i64| match k_of(*i) {
        Some(1) => false,
        Some(0) => *i >= 5000,
        _ => true,
    };
    assert_eq!(l.rows, (0..ROWS).filter(live).count() as u64);
    let held = plain((0..ROWS).filter(live).filter_map(k_of));
    let places = keyed(&l);
    for &(v, low, high) in &places {
        let n = held[&v];
        assert!(low <= n && n <= high, "{v}: {low} {n} {high}");
    }
    assert_eq!(
        places.iter().map(|p| p.0).collect::<Vec<_>>(),
        held.keys().copied().collect::<Vec<_>>()
    );
    let grouped = grouped(&mut snail, "k");
    assert_eq!(grouped, held);
}

/// Where a block holds places a page or more deep beside thin ones, a page lying wholly on one place
/// counts it, its dead rows left out; rows written after a block's sorted rows may lie on any of its
/// places.
#[test]
#[cfg(feature = "standalone")]
fn pages_count_their_live_rows_and_a_block_s_new_rows_may_lie_on_any_of_its_places() {
    let path = table("level-tail.snail");
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute("DELETE FROM s.t WHERE m = 4 AND id % 7 = 2")
        .unwrap();
    w.execute("INSERT INTO s.t (id, k, w, m) VALUES (100000, 1, 1, 4), (100001, 1, 1, 4)")
        .unwrap();
    drop(w);
    let mut snail = fresh(&path);
    let l = snail.level_counts("s.t", "m", u64::MAX).unwrap().unwrap();
    let mut held = plain(
        (0..ROWS)
            .filter(|i| !(m_of(*i) == 4 && i % 7 == 2))
            .map(m_of),
    );
    *held.get_mut(&4).unwrap() += 2;
    assert!(!l.whole, "{l:?}");
    // Every place listed holds rows, the thick ones among them; a thin place may lie inside a page.
    let places = keyed(&l);
    for &(v, low, high) in &places {
        let n = held[&v];
        assert!(low <= n && n <= high, "{v}: {low} {n} {high}");
    }
    assert!([0, 2, 4].iter().all(|v| places.iter().any(|p| p.0 == *v)));
    // The block holding the last of 4's rows and the thin places after them is counted by its pages.
    let four = places.iter().find(|p| p.0 == 4).unwrap();
    assert!(four.1 > 0 && four.1 < four.2, "{four:?}");
    assert_eq!(grouped(&mut snail, "m"), held);
}

/// A table of 8,000 rows on (g, id), each g a thousand of them: a block holds two places, and its
/// pages of ids, 512 rows each, lie wholly on one place or across the two.
#[cfg(feature = "standalone")]
fn paired(name: &str) -> std::path::PathBuf {
    let path = tmp(name);
    let catalog = Catalog::parse(
        "CREATE TABLE s.p (id bigint NOT NULL, g integer NOT NULL, PRIMARY KEY (id));
         CREATE INDEX p_g ON s.p (g, id);",
    )
    .unwrap();
    let rows = Rows {
        columns: vec![
            Values::Int((0..8000).map(Some).collect()),
            Values::Int((0..8000).map(|i| Some(i / 1000)).collect()),
        ],
    };
    write(&path, &catalog, &[("s.p".into(), rows)]).unwrap();
    path
}

/// A page's dead rows are not its place's, and rows written after a block's sorted rows count for
/// each of its places: nine rows in ten deleted, then rows of one place inserted, every place's live
/// rows lie between its bounds.
#[test]
#[cfg(feature = "standalone")]
fn dead_rows_are_no_place_s_and_new_rows_may_be_any_of_their_block_s() {
    let path = paired("level-paired.snail");
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute("DELETE FROM s.p WHERE id % 10 <> 0").unwrap();
    let values: Vec<String> = (0..300).map(|i| format!("({}, 1)", 100_000 + i)).collect();
    w.execute(&format!(
        "INSERT INTO s.p (id, g) VALUES {}",
        values.join(", ")
    ))
    .unwrap();
    drop(w);
    let snail = fresh(&path);
    let dead: u64 = snail
        .blocks()
        .unwrap()
        .iter()
        .filter(|b| b.order == "p_g" && b.column == "id")
        .map(|b| b.dead_marked)
        .sum();
    assert!(dead > 0, "the deleted rows stand dead in their blocks");
    let l = snail.level_counts("s.p", "g", u64::MAX).unwrap().unwrap();
    assert_eq!(l.rows, 800 + 300);
    let held = plain(
        (0..8000)
            .filter(|i| i % 10 == 0)
            .map(|i| i / 1000)
            .chain([1; 300]),
    );
    for (v, low, high) in keyed(&l) {
        let n = held[&v];
        assert!(low <= n && n <= high, "{v}: {low} {n} {high}");
    }
}
