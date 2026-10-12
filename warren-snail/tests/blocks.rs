//! How a table's order is cut into blocks and read, on small tables made for each case.

mod weather;

use warren_snail::catalog::Catalog;
use warren_snail::read::Snail;
use warren_snail::values::Values;
use warren_snail::write::Rows;
use weather::{fresh, ints64, tmp, write};

#[test]
fn text_narrower_than_the_key_decides_the_cut_on_write() {
    // 4,096 rows of an 8-byte key, 512 to a page, beside one byte of text a row, 5 written plain.
    let path = tmp("narrow.snail");
    let catalog =
        Catalog::parse("CREATE TABLE s.t (a bigint NOT NULL, b text NOT NULL, PRIMARY KEY (a));")
            .unwrap();
    let rows = Rows {
        columns: vec![
            Values::Int((0..4096).map(Some).collect()),
            Values::Text((0..4096).map(|_| Some("x".into())).collect()),
        ],
    };
    write(&path, &catalog, &[("s.t".into(), rows)]).unwrap();
    let blocks = Snail::open(&path).unwrap().blocks().unwrap();
    let rows: Vec<u64> = blocks
        .iter()
        .filter(|b| b.column == "b")
        .map(|b| b.rows)
        .collect();
    assert_eq!(rows, [1024, 1024, 1024, 1024]);
}

#[test]
fn an_order_is_out_once_the_directory_pages_it_reads_next_pass_another_order_s_every_page() {
    // `k` holds 50 values of 601 bytes, 1,200 rows each, so the index on it is 50 blocks whose
    // entries take a third of a page each, and its directory stands four levels high. A point read
    // on the primary key may find its row in any of the index's blocks.
    let path = tmp("deep.snail");
    let catalog = Catalog::parse(
        "CREATE TABLE s.t (a integer NOT NULL, b integer NOT NULL, k text NOT NULL, PRIMARY KEY (a));
         CREATE INDEX s_k ON s.t (k, a) INCLUDE (b);",
    )
    .unwrap();
    let n = 60_000i64;
    let rows = Rows {
        columns: vec![
            Values::Int((0..n).map(Some).collect()),
            Values::Int((0..n).map(|i| Some(i * 3)).collect()),
            Values::Text(
                (0..n)
                    .map(|i| {
                        let letter = (b'A' + (i * 7919 % 50) as u8) as char;
                        Some(format!("{letter}{}", "x".repeat(600)))
                    })
                    .collect(),
            ),
        ],
    };
    let written = write(&path, &catalog, &[("s.t".into(), rows)]).unwrap();
    let pages = written
        .iter()
        .find(|w| w.order == "s_k")
        .unwrap()
        .index_pages;
    assert_eq!(pages, 26, "{written:?}");
    let mut snail = fresh(&path);
    let q = "SELECT t.b FROM s.t t WHERE t.a = 5";
    let (b, round) = snail.ask(q, None).unwrap();
    assert_eq!(round.order, "t_pkey", "{round:?}");
    assert_eq!(ints64(b.column(0).as_ref()), [Some(15)]);
    assert_eq!(
        round.column_pages_read,
        round.column_pages_predicted - round.column_pages_held,
        "{round:?}"
    );
    // The index is out above its blocks, having read less than half its directory: the directory
    // pages under it are more than every page along the key.
    let index = round.consulted.iter().find(|c| c.order == "s_k").unwrap();
    assert_eq!(index.exact, None, "{round:?}");
    assert!(index.directory_pages * 2 < pages, "{round:?}");
}

/// A table of 3,000 rows on (a, b), a always 1 and b counting from 0.
#[cfg(feature = "standalone")]
fn counted(name: &str) -> std::path::PathBuf {
    let path = tmp(name);
    let catalog = Catalog::parse(
        "CREATE TABLE s.t (a integer NOT NULL, b bigint NOT NULL);
         CREATE INDEX o ON s.t (a, b);
         ALTER TABLE s.t CLUSTER ON o;",
    )
    .unwrap();
    let rows = Rows {
        columns: vec![
            Values::Int((0..3000).map(|_| Some(1)).collect()),
            Values::Int((0..3000).map(Some).collect()),
        ],
    };
    write(&path, &catalog, &[("s.t".into(), rows)]).unwrap();
    path
}

#[test]
#[cfg(feature = "standalone")]
fn a_row_written_into_a_last_page_is_read_wherever_its_key_lies() {
    // The cut keeps b's 0 to 1,023 as one block and 1,024 to 2,999 as another, a in one page of
    // codes and b in five. A row keyed (1, 1100) lies in the second block's first pages and is
    // written after its last rows, into pages of their own that every read of the block reads.
    let path = counted("last-page.snail");
    let blocks = Snail::open(&path).unwrap().blocks().unwrap();
    let pages = |column: &str| -> Vec<u64> {
        blocks
            .iter()
            .filter(|b| b.column == column)
            .map(|b| b.data_pages)
            .collect()
    };
    assert_eq!((pages("a"), pages("b")), (vec![2, 1], vec![3, 5]));

    let mut w = Snail::open_writing(&path).unwrap();
    let d = w
        .execute("INSERT INTO s.t (a, b) VALUES (1, 1100)")
        .unwrap();
    assert_eq!(d.rewritten, 0, "{d:?}");
    w.execute("DELETE FROM s.t WHERE a = 1 AND b = 1101")
        .unwrap();
    // The read narrowed to (1, 1100) reads the last pages only for the row written there: a page
    // of waste, so it prunes the block, and the read after reads it in order.
    for (q, want, pruned) in [
        (
            "SELECT t.b FROM s.t t WHERE t.a = 1 AND t.b = 1100",
            vec![1100, 1100],
            1,
        ),
        (
            "SELECT t.b FROM s.t t WHERE t.a = 1 AND t.b >= 1100 AND t.b <= 1102",
            vec![1100, 1100, 1102],
            0,
        ),
    ] {
        let (b, round) = w.ask(q, None).unwrap();
        let mut got: Vec<i64> = ints64(b.column(0).as_ref()).into_iter().flatten().collect();
        got.sort();
        assert_eq!((got, round.blocks_pruned), (want, pruned), "{q}: {round:?}");
        assert_eq!(
            round.column_pages_read,
            round.column_pages_predicted - round.column_pages_held,
            "{q}: {round:?}"
        );
    }
}

#[test]
#[cfg(feature = "standalone")]
fn a_read_prunes_a_block_on_the_first_page_its_dead_rows_waste_and_never_before() {
    let path = counted("prune.snail");
    // The second block's `b`: its rows and disk pages, and the dead rows at which a read of the
    // whole block first reads a page more than its live rows fill.
    let blocks = Snail::open(&path).unwrap().blocks().unwrap();
    let b = blocks.iter().filter(|x| x.column == "b").nth(1).unwrap();
    let (n, p) = (b.rows, b.disk_pages);
    let threshold = (0..n)
        .find(|&d| p - (p * (n - d)).div_ceil(n) >= 1)
        .unwrap();
    assert_eq!((n, p, threshold), (1976, 5, 396));

    let mut w = Snail::open_writing(&path).unwrap();
    let q = "SELECT t.b FROM s.t t WHERE t.a = 1 AND t.b >= 1024";
    w.execute(&format!(
        "DELETE FROM s.t WHERE a = 1 AND b >= 1024 AND b < {}",
        1024 + threshold - 1
    ))
    .unwrap();
    let (got, round) = w.ask(q, None).unwrap();
    assert_eq!(got.num_rows() as u64, n - (threshold - 1));
    assert_eq!(round.blocks_pruned, 0, "a row short of a page: {round:?}");
    w.execute(&format!(
        "DELETE FROM s.t WHERE a = 1 AND b = {}",
        1024 + threshold - 1
    ))
    .unwrap();
    let (got, round) = w.ask(q, None).unwrap();
    assert_eq!(got.num_rows() as u64, n - threshold);
    assert_eq!(round.blocks_pruned, 1, "{round:?}");
    let (again, round) = w.ask(q, None).unwrap();
    assert_eq!((again.num_rows(), round.blocks_pruned), (got.num_rows(), 0));
    // A block nobody reads keeps its dead rows.
    w.execute("DELETE FROM s.t WHERE a = 1 AND b < 600")
        .unwrap();
    let (_, round) = w.ask(q, None).unwrap();
    assert_eq!(round.blocks_pruned, 0, "{round:?}");
}
