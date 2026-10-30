//! tid, the first value held as a group of leaves: its block number and its offset, each a leaf of
//! its own, with its own unit, encoding, summary and pages in each block; keyed block then offset,
//! read and written as PostgreSQL's tidin and tidout, compared, ordered and grouped as PostgreSQL
//! does.

mod weather;

use arrow::array::{Array, RecordBatch};
use warren_snail::catalog::{Catalog, ColumnType};
use warren_snail::read::{BlockRow, Snail};
use warren_snail::scalars::{scalar_key_at, scalar_text, tid_key};
use warren_snail::values::Values;
use warren_snail::write::Rows;
use weather::{exact, ints64, parquet_of, tmp, write};

/// A tid column of an answer, each value as tidout writes it.
fn tids(b: &RecordBatch, c: usize) -> Vec<Option<String>> {
    let a = b.column(c);
    (0..a.len())
        .map(|r| {
            scalar_key_at(ColumnType::Tid, a.as_ref(), r).map(|k| scalar_text(ColumnType::Tid, k))
        })
        .collect()
}

fn ns(b: &RecordBatch) -> Vec<i64> {
    ints64(b.column(0).as_ref()).into_iter().flatten().collect()
}

fn of<'a>(rows: &'a [BlockRow], order: &str, column: &str) -> Vec<&'a BlockRow> {
    rows.iter()
        .filter(|r| r.order == order && r.column == column)
        .collect()
}

/// A snail of one table, `s.t`, its rows given column by column.
fn snail_of(name: &str, ddl: &str, columns: Vec<Values>) -> std::path::PathBuf {
    let path = tmp(&format!("{name}.snail"));
    let catalog = Catalog::parse(ddl).unwrap();
    write(&path, &catalog, &[("s.t".into(), Rows { columns })]).unwrap();
    path
}

/// The values of the type's table in snail-tests/types/fixed.sql, by id from 1, NULL second.
fn table() -> Vec<Option<i64>> {
    vec![
        Some(tid_key(1, 2)),
        None,
        Some(tid_key(4294967295, 65535)),
        Some(tid_key(0, 0)),
        Some(tid_key(0, 1)),
        Some(tid_key(1, 0)),
        Some(tid_key(0, 65535)),
        Some(tid_key(2147483648, 1)),
        Some(tid_key(0, 2)),
        Some(tid_key(4294967295, 0)),
    ]
}

const DDL: &str = "CREATE TABLE s.t (id integer NOT NULL, v tid, PRIMARY KEY (id)); \
                   CREATE INDEX t_v ON s.t (v, id); \
                   CREATE INDEX t_vd ON s.t (v DESC, id);";

fn fixed(name: &str) -> std::path::PathBuf {
    snail_of(
        name,
        DDL,
        vec![
            Values::Int((1..=10).map(Some).collect()),
            Values::Int(table()),
        ],
    )
}

#[test]
fn a_tid_is_held_as_two_leaves_its_block_then_its_offset() {
    let path = fixed("tid-leaves");
    let blocks = Snail::open(&path).unwrap().blocks().unwrap();
    let names = |order: &str| -> Vec<String> {
        blocks
            .iter()
            .filter(|r| r.order == order)
            .map(|r| r.column.clone())
            .collect()
    };
    assert_eq!(names("t_pkey"), ["id", "v.block", "v.offset"]);
    assert_eq!(names("t_v"), ["v.block", "v.offset", "id"]);
    // Each leaf holds the column's NULL, and pages of its own.
    for leaf in ["v.block", "v.offset"] {
        let r = of(&blocks, "t_pkey", leaf);
        assert_eq!((r[0].nulls, r[0].data_pages), (1, 1), "{leaf}");
    }
}

#[test]
fn a_tid_goes_out_as_a_group_of_its_leaves_sorted_on_both_and_comes_back_by_their_pages() {
    use parquet::file::reader::{FileReader, SerializedFileReader};
    let path = snail_of(
        "tid-out",
        "CREATE TABLE s.t (id integer NOT NULL, v tid NOT NULL, PRIMARY KEY (v));",
        vec![
            Values::Int((0..3000).map(Some).collect()),
            Values::Int(
                (0..3000)
                    .map(|i| Some(tid_key(i / 7, (i % 7) as u16)))
                    .collect(),
            ),
        ],
    );
    let dir = tmp("tid-out-parquet");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let outs = Snail::open(&path).unwrap().out(&dir).unwrap();
    let file = &outs[0].file;
    let reader = SerializedFileReader::new(std::fs::File::open(file).unwrap()).unwrap();
    let schema = reader.metadata().file_metadata().schema_descr();
    let paths: Vec<String> = (0..schema.num_columns())
        .map(|i| schema.column(i).path().string())
        .collect();
    assert_eq!(paths, ["id", "v.block", "v.offset"]);
    // The table's order, v, sorts on its block, then its offset.
    for g in 0..reader.metadata().num_row_groups() {
        let sorting = reader.metadata().row_group(g).sorting_columns().unwrap();
        let on: Vec<i32> = sorting.iter().map(|s| s.column_idx).collect();
        assert_eq!(on, [1, 2]);
    }
    // arrow reads the group as a struct of an unsigned 32-bit block and an unsigned 16-bit offset.
    let (batches, groups) = parquet_of(file);
    let v = batches[0].schema().field_with_name("v").unwrap().clone();
    assert_eq!(
        v.data_type().to_string(),
        "Struct(\"block\": non-null UInt32, \"offset\": non-null UInt16)"
    );
    let back_rows: Vec<Option<String>> = batches.iter().flat_map(|b| tids(b, 1)).collect();
    assert_eq!(back_rows[8], Some("(1,1)".into()));
    // Written back, each leaf comes in by its pages.
    let catalog =
        Catalog::parse("CREATE TABLE s.t (id integer NOT NULL, v tid NOT NULL, PRIMARY KEY (v));")
            .unwrap();
    let table = catalog.table("s.t").unwrap();
    let rows = Rows::from_batches(table, &batches).unwrap();
    let given = warren_snail::write::source(file, table).unwrap();
    let back = tmp("tid-out-back.snail");
    let written = warren_snail::write::write_spanned(
        &back,
        &catalog,
        &[("s.t".into(), rows)],
        &[("s.t".into(), given)],
        weather::span(),
    )
    .unwrap();
    let pkey = written.iter().find(|w| w.order == "t_pkey").unwrap();
    assert_eq!(pkey.blocks, groups);
    let leaves: Vec<(&str, usize)> = pkey
        .columns
        .iter()
        .map(|c| (c.name.as_str(), c.moved_blocks))
        .collect();
    assert_eq!(
        leaves,
        [("id", groups), ("v.block", groups), ("v.offset", groups)]
    );
    let (b, _) = Snail::open(&back)
        .unwrap()
        .ask("SELECT t.v FROM s.t t WHERE t.id = 8", None)
        .unwrap();
    assert_eq!(tids(&b, 0), [Some("(1,1)".into())]);
}

#[test]
fn a_tid_is_cut_on_its_block_s_32_bits_then_its_offset_s_16() {
    let catalog = "CREATE TABLE s.t (v tid NOT NULL, PRIMARY KEY (v));";
    let cut = |name: &str, keys: Vec<i64>| -> Vec<u64> {
        let path = snail_of(
            name,
            catalog,
            vec![Values::Int(keys.into_iter().map(Some).collect())],
        );
        of(
            &Snail::open(&path).unwrap().blocks().unwrap(),
            "t_pkey",
            "v.block",
        )
        .iter()
        .map(|r| r.rows)
        .collect()
    };
    // 4,096 tids spread over their block numbers' 32 bits, across the top bit, cut as 4,096
    // integers spread so: each leaf 4 bytes, 1,024 rows a page.
    let blocks = (0..4096u32).map(|i| tid_key(i << 20, 0)).collect();
    assert_eq!(cut("tid-cut-blocks", blocks), [1024; 4]);
    // In one block, 4,096 offsets spread over their 16 bits are cut on them.
    let offsets = (0..4096u32).map(|i| tid_key(7, (i << 4) as u16)).collect();
    assert_eq!(cut("tid-cut-offsets", offsets), [1024; 4]);
}

#[test]
fn each_leaf_takes_its_own_encoding_and_reads_back_beside_the_other() {
    // One block of 2,047 rows, three block numbers and every offset distinct: the block's leaf takes a
    // dictionary, the offset's stays plain, so their pages hold different rows.
    let n = 2047;
    let path = snail_of(
        "tid-encodings",
        "CREATE TABLE s.t (id integer NOT NULL, v tid, PRIMARY KEY (id));",
        vec![
            Values::Int((0..n).map(Some).collect()),
            Values::Int(
                (0..n)
                    .map(|i| (i % 500 != 9).then(|| tid_key((i / 1000) as u32 * 1000, i as u16)))
                    .collect(),
            ),
        ],
    );
    let mut snail = Snail::open(&path).unwrap();
    let blocks = snail.blocks().unwrap();
    let block = of(&blocks, "t_pkey", "v.block");
    let offset = of(&blocks, "t_pkey", "v.offset");
    assert!(block.iter().all(|r| r.dictionary_pages > 0));
    assert!(offset.iter().all(|r| r.dictionary_pages == 0));
    assert!(block
        .iter()
        .zip(&offset)
        .any(|(b, o)| b.data_pages != o.data_pages));
    for (lo, hi) in [(0, 2046), (1500, 1600), (1015, 1030), (1009, 1009)] {
        let q = format!(
            "SELECT t.id, t.v FROM s.t t WHERE t.id >= {lo} AND t.id <= {hi} ORDER BY t.id"
        );
        let (b, round) = snail.ask(&q, None).unwrap();
        exact(&q, &round);
        let want: Vec<Option<String>> = (lo..=hi)
            .map(|i| (i % 500 != 9).then(|| format!("({},{i})", (i / 1000) * 1000)))
            .collect();
        assert_eq!(tids(&b, 1), want, "{q}");
    }
}

#[test]
fn a_condition_on_a_tid_rules_blocks_out_by_its_leaves_summaries() {
    // A tid rising with the table's key, 100 offsets a block number: 8,192 rows in 8 blocks of the
    // order on id, each block 1,024 rows of its three 4-byte leaves.
    let n = 8192;
    let path = snail_of(
        "tid-ruled",
        "CREATE TABLE s.t (id integer NOT NULL, v tid NOT NULL, PRIMARY KEY (id));",
        vec![
            Values::Int((0..n).map(Some).collect()),
            Values::Int(
                (0..n)
                    .map(|i| Some(tid_key((i / 100) as u32, (i % 100) as u16)))
                    .collect(),
            ),
        ],
    );
    let mut snail = Snail::open(&path).unwrap();
    assert_eq!(of(&snail.blocks().unwrap(), "t_pkey", "v.block").len(), 8);
    for (q, count, blocks) in [
        // Only the first block may hold one: its two leaves read.
        ("SELECT count(*) FROM s.t t WHERE t.v < '(10,0)'", 1000, 1),
        ("SELECT count(*) FROM s.t t WHERE t.v >= '(81,0)'", 92, 1),
        // A block's block numbers all past the bound hold none, though its offsets are below.
        ("SELECT count(*) FROM s.t t WHERE t.v < '(0,50)'", 50, 1),
    ] {
        let (b, round) = snail.ask(q, Some("t_pkey")).unwrap();
        exact(q, &round);
        assert_eq!(ns(&b), [count], "{q}");
        assert_eq!(round.blocks_read, blocks, "{q}");
    }
    // Every block's tids lie past (0,0): the count is the counts', no page read.
    let q = "SELECT count(*) FROM s.t t WHERE t.v >= '(0,0)'";
    let (b, round) = snail.ask(q, Some("t_pkey")).unwrap();
    assert_eq!((ns(&b), round.column_pages_read), (vec![n], 0));
}

#[test]
fn a_tid_is_compared_ordered_and_grouped_as_postgresql_does() {
    let path = fixed("tid-ask");
    let mut snail = Snail::open(&path).unwrap();
    let mut ask = |q: &str, order: Option<&str>| {
        let (b, round) = snail.ask(q, order).unwrap();
        exact(q, &round);
        b
    };
    // ORDER BY as PostgreSQL 18.6 gave it, along every order.
    for order in ["t_pkey", "t_v", "t_vd"] {
        let b = ask(
            "SELECT t.id, t.v FROM s.t t ORDER BY t.v, t.id",
            Some(order),
        );
        assert_eq!(ns(&b), [4, 5, 9, 7, 6, 1, 8, 10, 3, 2], "{order}");
        let b = ask(
            "SELECT t.id, t.v FROM s.t t ORDER BY t.v DESC, t.id",
            Some(order),
        );
        assert_eq!(ns(&b), [2, 3, 10, 8, 1, 6, 7, 9, 5, 4], "{order}");
        let b = ask(
            "SELECT t.id FROM s.t t WHERE t.v < '(1,0)' ORDER BY t.id",
            Some(order),
        );
        assert_eq!(ns(&b), [4, 5, 7, 9], "{order}");
        let b = ask(
            "SELECT t.id FROM s.t t WHERE '(1,0)' <= t.v ORDER BY t.id",
            Some(order),
        );
        assert_eq!(ns(&b), [1, 3, 6, 8, 10], "{order}");
        let b = ask(
            "SELECT t.id FROM s.t t WHERE t.v = '(-1,0)' ORDER BY t.id",
            Some(order),
        );
        assert_eq!(ns(&b), [10], "{order}");
        let b = ask(
            "SELECT count(*) FROM s.t t WHERE t.v <> '(0,65535)'",
            Some(order),
        );
        assert_eq!(ns(&b), [8], "{order}");
    }
    let b = ask("SELECT min(t.v) AS x, max(t.v) AS y FROM s.t t", None);
    assert_eq!(
        (tids(&b, 0), tids(&b, 1)),
        (
            vec![Some("(0,0)".into())],
            vec![Some("(4294967295,65535)".into())]
        )
    );
    let step = "WITH t AS (SELECT id, v FROM s.t)";
    let b = ask(
        &format!("{step} SELECT min(t.v) AS x, max(t.v) AS y FROM t WHERE t.id > 4"),
        None,
    );
    assert_eq!(
        (tids(&b, 0), tids(&b, 1)),
        (
            vec![Some("(0,1)".into())],
            vec![Some("(4294967295,0)".into())]
        )
    );
    let b = ask(
        &format!(
            "{step} SELECT t.id FROM t WHERE t.v > '(0,65535)' AND t.v <> '(1,2)' ORDER BY t.id"
        ),
        None,
    );
    assert_eq!(ns(&b), [3, 6, 8, 10]);
    let b = ask(
        &format!("{step} SELECT t.id, t.v::text AS x FROM t WHERE t.id = 8"),
        None,
    );
    assert_eq!(
        weather::texts(b.column(1).as_ref()),
        [Some("(2147483648,1)".into())]
    );
    // A tid has no arithmetic, no sum and no cast but through text.
    for q in [
        "SELECT t.id FROM s.t t WHERE t.v = 5".to_string(),
        "SELECT sum(t.v) FROM s.t t".to_string(),
        "SELECT avg(t.v) FROM s.t t".to_string(),
        format!("{step} SELECT t.v + 1 FROM t"),
        format!("{step} SELECT CAST(t.v AS integer) FROM t"),
        format!("{step} SELECT CAST(t.id AS tid) FROM t"),
        format!("{step} SELECT sum(t.v) FROM t"),
    ] {
        assert!(snail.ask(&q, None).is_err(), "{q}");
    }
}

#[test]
#[cfg(feature = "standalone")]
fn a_tid_written_into_a_block_goes_into_each_leaf_s_tail_and_is_pruned_back_in_order() {
    let path = fixed("tid-writes");
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute("INSERT INTO s.t (id, v) VALUES (11, '(3,4)'), (12, NULL), (13, '(0,3)')")
        .unwrap();
    w.execute("UPDATE s.t SET v = '(5,6)' WHERE id = 1")
        .unwrap();
    w.execute("DELETE FROM s.t WHERE v = '(0,0)'").unwrap();
    let want = [
        (1, Some("(5,6)")),
        (2, None),
        (3, Some("(4294967295,65535)")),
        (5, Some("(0,1)")),
        (6, Some("(1,0)")),
        (7, Some("(0,65535)")),
        (8, Some("(2147483648,1)")),
        (9, Some("(0,2)")),
        (10, Some("(4294967295,0)")),
        (11, Some("(3,4)")),
        (12, None),
        (13, Some("(0,3)")),
    ];
    let check = |w: &mut Snail| {
        for order in ["t_pkey", "t_v", "t_vd"] {
            let (b, _) = w
                .ask("SELECT t.id, t.v FROM s.t t ORDER BY t.id", Some(order))
                .unwrap();
            let got: Vec<(i64, Option<String>)> = ns(&b).into_iter().zip(tids(&b, 1)).collect();
            let want: Vec<(i64, Option<String>)> = want
                .iter()
                .map(|&(i, v)| (i, v.map(str::to_string)))
                .collect();
            assert_eq!(got, want, "{order}");
        }
        let (b, _) = w
            .ask(
                "SELECT t.id FROM s.t t WHERE t.v < '(1,0)' ORDER BY t.id",
                Some("t_v"),
            )
            .unwrap();
        assert_eq!(ns(&b), [5, 7, 9, 13]);
    };
    // Each leaf's block holds the rows gone and a tail of the rows come.
    let blocks = w.blocks().unwrap();
    for leaf in ["v.block", "v.offset"] {
        let r = of(&blocks, "t_pkey", leaf);
        assert_eq!((r[0].dead_marked, r[0].data_pages), (2, 2), "{leaf}");
    }
    check(&mut w);
    // Read again, the block is pruned: each leaf laid back in the order, one page.
    let blocks = w.blocks().unwrap();
    for leaf in ["v.block", "v.offset"] {
        let r = of(&blocks, "t_pkey", leaf);
        assert_eq!((r[0].dead_marked, r[0].data_pages), (0, 1), "{leaf}");
    }
    drop(w);
    check(&mut Snail::open(&path).unwrap());
}

#[test]
#[cfg(feature = "standalone")]
fn a_snail_of_format_eight_refuses_a_tid_by_its_leaves() {
    let catalog =
        Catalog::parse("CREATE TABLE s.t (id integer NOT NULL, v tid, PRIMARY KEY (id));").unwrap();
    let path = tmp("tid-eight.snail");
    let e = warren_snail::write::write_format_eight(&path, &catalog, &[], weather::span())
        .err()
        .unwrap()
        .to_string();
    assert!(
        e.contains("s.t.v is tid, a group of 2 leaves, v.block and v.offset, which a snail of format 8 cannot hold")
            && e.contains("`out`"),
        "{e}"
    );
}
