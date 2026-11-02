//! What a read's plan holds counts in its round's room: a block read whole is priced from its entry
//! and its map read once, when the block comes; a block read in part keeps its map's pages while the
//! room lets it and reads them again when it is reached, the pages read again known before any column
//! page is read.

mod weather;

use warren_snail::catalog::Catalog;
use warren_snail::read::{Round, Snail};
use warren_snail::room::Sizes;
use warren_snail::values::{push_datum, Values};
use warren_snail::write::{Rows, Written};
use weather::{exact, fresh, tmp, write};

const DDL: &str = "CREATE TABLE s.a (id bigint NOT NULL, PRIMARY KEY (id));\n\
     CREATE TABLE s.b (id bigint NOT NULL, a_id bigint NOT NULL, v bigint NOT NULL, \
     PRIMARY KEY (id)); CREATE INDEX b_a ON s.b (a_id, id) INCLUDE (v);";

const N: i64 = 1 << 18;

fn int_rows(catalog: &Catalog, table: &str, row: impl Fn(i64, usize) -> i64, n: i64) -> Rows {
    let t = catalog.table(table).unwrap();
    Rows {
        columns: t
            .columns
            .iter()
            .enumerate()
            .map(|(c, col)| {
                let mut v = Values::new(col.ty);
                for i in 0..n {
                    push_datum(
                        &mut v,
                        &warren_snail::composed::scalar_in(col.ty, &row(i, c).to_string()).unwrap(),
                    );
                }
                v
            })
            .collect(),
    }
}

/// A snail of 2^18 rows on 64 keys; and what writing it wrote of each order.
fn shop(name: &str) -> (std::path::PathBuf, Vec<Written>) {
    let path = tmp(&format!("plans-{name}.snail"));
    let catalog = Catalog::parse(DDL).unwrap();
    let a = int_rows(&catalog, "s.a", |i, _| i, 64);
    let b = int_rows(
        &catalog,
        "s.b",
        |i, c| match c {
            0 => i,
            1 => i % 64,
            _ => N - i,
        },
        N,
    );
    let written = write(&path, &catalog, &[("s.a".into(), a), ("s.b".into(), b)]).unwrap();
    (path, written)
}

fn whole_pages_read(snail: &Snail, before: u64, r: &Round) {
    assert_eq!(
        snail.pages_read() - before,
        r.directory_pages - r.directory_pages_held + r.column_pages_read,
        "{r:?}"
    );
}

#[test]
fn a_whole_read_is_priced_from_its_entries_and_reads_each_map_once_as_its_block_comes() {
    let (path, written) = shop("whole");
    let w = written.iter().find(|w| w.order == "b_pkey").unwrap();
    let q = "SELECT b.id, b.v FROM s.b b";
    let snail = fresh(&path);
    let before = snail.pages_read();
    let price = snail.price(q, Some("b_pkey")).unwrap();
    // Priced, it reads the directory above its maps and none of them, and knows the pages of each.
    assert_eq!(price.directory_pages, w.index_pages, "{price:?}");
    assert_eq!(price.map_pages, w.map_pages, "{price:?}");
    assert_eq!(snail.pages_read() - before, w.index_pages);
    let mut snail = fresh(&path);
    let before = snail.pages_read();
    let (b, r) = snail.ask(q, Some("b_pkey")).unwrap();
    exact(q, &r);
    assert_eq!(b.num_rows() as i64, N);
    assert_eq!(r.directory_pages, w.index_pages + w.map_pages, "{r:?}");
    whole_pages_read(&snail, before, &r);
    // At any room: a whole read holds no map to read it again.
    let mut snail = fresh(&path);
    snail.size_rounds(Sizes::of(4096));
    let (_, small) = snail.ask(q, Some("b_pkey")).unwrap();
    exact(q, &small);
    assert_eq!(small.directory_pages, r.directory_pages, "{small:?}");
}

#[test]
fn an_order_read_whole_that_loses_reads_no_map() {
    // Two orders hold what the question reads, each whole: the one left out reads none of its maps.
    let (path, written) = shop("loses");
    let index = |name: &str| written.iter().find(|w| w.order == name).unwrap();
    let q = "SELECT b.id, b.v, b.a_id FROM s.b b";
    let mut snail = fresh(&path);
    let (_, r) = snail.ask(q, None).unwrap();
    exact(q, &r);
    assert_eq!(r.served.len(), 2, "{r:?}");
    let read = index(&r.order);
    let other = r.consulted.iter().find(|c| c.order != r.order).unwrap();
    let other_written = index(&other.order);
    assert!(other.directory_pages <= other_written.index_pages, "{r:?}");
    assert_eq!(
        r.directory_pages,
        other.directory_pages + read.index_pages + read.map_pages,
        "{r:?}"
    );
}

const KEPT: &str = "SELECT a.id AS c1, b.id AS c2, b.v AS c3 FROM s.a a \
                    JOIN s.b b ON b.a_id = a.id WHERE a.id < 8";

#[test]
fn within_the_room_a_read_in_part_reads_its_maps_once() {
    let (path, _) = shop("within");
    let mut snail = fresh(&path);
    let before = snail.pages_read();
    let (b, r) = snail.ask(KEPT, None).unwrap();
    exact(KEPT, &r);
    assert_eq!(b.num_rows() as i64, N / 8);
    whole_pages_read(&snail, before, &r);
}

#[test]
fn past_the_room_a_read_in_part_lets_its_maps_go_and_reads_them_again_when_it_reaches_them() {
    let (path, _) = shop("past");
    let mut big = fresh(&path);
    let (answer, whole) = big.ask(KEPT, None).unwrap();
    exact(KEPT, &whole);
    // Room for two map pages in a plan: a quarter of 32 KiB.
    let mut snail = fresh(&path);
    snail.size_rounds(Sizes::of(32 << 10));
    let before = snail.pages_read();
    let (b, small) = snail.ask(KEPT, None).unwrap();
    exact(KEPT, &small);
    assert_eq!(b.num_rows(), answer.num_rows());
    assert_eq!(
        (small.order.as_str(), small.column_pages_predicted, small.blocks_read),
        (whole.order.as_str(), whole.column_pages_predicted, whole.blocks_read),
        "{small:?}"
    );
    // Each read again is known before it is read, counted among the directory pages, and read from
    // the file past what the round keeps.
    assert!(
        small.directory_pages > whole.directory_pages,
        "{small:?} {whole:?}"
    );
    assert_eq!(small.directory_pages, small.directory_pages_predicted);
    whole_pages_read(&snail, before, &small);
    // Given a round's room of nothing, every map a block read in part is read twice.
    let mut none = fresh(&path);
    none.size_rounds(Sizes::of(0));
    let (_, r) = none.ask(KEPT, None).unwrap();
    exact(KEPT, &r);
    assert!(r.directory_pages >= small.directory_pages, "{r:?}");
}

#[test]
fn a_plan_counts_in_the_most_a_round_holds() {
    let (path, _) = shop("counted");
    // A greatest value keeps one row of every block it reads whole, and holds its plan beside it.
    let q = "SELECT max(b.v) FROM s.b b";
    let mut snail = fresh(&path);
    let (b, r) = snail.ask(q, Some("b_pkey")).unwrap();
    exact(q, &r);
    assert_eq!(b.num_rows(), 1);
    assert!(r.blocks_read >= 256, "{r:?}");
    assert!(r.bytes_held >= r.blocks_read * 32, "{r:?}");
}

#[test]
fn a_scan_handed_out_a_block_at_a_time_keeps_its_plan_to_the_room_its_snail_gives() {
    let (path, written) = shop("scan");
    let w = written.iter().find(|w| w.order == "b_pkey").unwrap();
    let q = "SELECT b.id FROM s.b b";
    let mut snail = fresh(&path);
    snail.size_rounds(Sizes::of(4096));
    let before = snail.pages_read();
    let mut scan = snail.scan(q, Some("b_pkey")).unwrap();
    let mut rows = 0;
    while let Some(b) = snail.next_block(&mut scan).unwrap() {
        rows += b.num_rows() as i64;
    }
    let r = scan.round();
    assert_eq!(rows, N);
    assert_eq!(r.directory_pages, w.index_pages + w.map_pages, "{r:?}");
    assert_eq!(r.directory_pages, r.directory_pages_predicted, "{r:?}");
    assert_eq!(r.column_pages_read, r.column_pages_predicted, "{r:?}");
    assert_eq!(snail.pages_read() - before, r.directory_pages + r.column_pages_read);
}
