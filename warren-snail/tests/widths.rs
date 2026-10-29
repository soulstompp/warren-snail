//! The widths the cut counts, and the boolean: a smallint is the 4 bytes the file writes; a NULL takes
//! none, and a column holding NULLs among rows its values fill less than a page of does not decide
//! them; a boolean never decides, takes a page of its own in each block, and is held, keyed, written
//! and read as PostgreSQL reads it.

mod weather;

use arrow::array::{Array, BooleanArray};
use warren_snail::catalog::Catalog;
use warren_snail::read::{BlockRow, Snail};
use warren_snail::values::Values;
use warren_snail::write::Rows;
use weather::{tmp, write};

fn written(name: &str, ddl: &str, table: &str, columns: Vec<Values>) -> Snail {
    let path = tmp(&format!("{name}.snail"));
    let catalog = Catalog::parse(ddl).unwrap();
    write(&path, &catalog, &[(table.to_string(), Rows { columns })]).unwrap();
    Snail::open(&path).unwrap()
}

/// The rows of each block of an order, in its order.
fn blocks_of(rows: &[BlockRow], order: &str, column: &str) -> Vec<u64> {
    rows.iter()
        .filter(|r| r.order == order && r.column == column)
        .map(|r| r.rows)
        .collect()
}

/// The disk pages a column takes in each block of an order.
fn pages_of(rows: &[BlockRow], order: &str, column: &str) -> Vec<u64> {
    rows.iter()
        .filter(|r| r.order == order && r.column == column)
        .map(|r| r.disk_pages)
        .collect()
}

#[test]
fn a_smallint_is_cut_on_the_four_bytes_the_file_writes() {
    // 4,096 values of a 4-byte column: a page holds 1,024, so the cut stops at the quarters. At 2
    // bytes a smallint would fill a page only at 2,048 rows, and its blocks would be the halves.
    let values = || Values::Int((0..4096).map(Some).collect());
    let small = written(
        "smallint",
        "CREATE TABLE s.t (a smallint NOT NULL, PRIMARY KEY (a));",
        "s.t",
        vec![values()],
    );
    let int = written(
        "smallint-integer",
        "CREATE TABLE s.t (a integer NOT NULL, PRIMARY KEY (a));",
        "s.t",
        vec![values()],
    );
    let (s, i) = (small.blocks().unwrap(), int.blocks().unwrap());
    assert_eq!(blocks_of(&s, "t_pkey", "a"), [1024, 1024, 1024, 1024]);
    assert_eq!(blocks_of(&s, "t_pkey", "a"), blocks_of(&i, "t_pkey", "a"));
    assert_eq!(pages_of(&s, "t_pkey", "a"), pages_of(&i, "t_pkey", "a"));
}

/// 8,192 rows of a bigint key and a nullable integer, the integer NULL on the rows `null` names.
fn keyed_with_nulls(name: &str, null: impl Fn(i64) -> bool) -> Snail {
    written(
        name,
        "CREATE TABLE s.t (a bigint NOT NULL, b integer, PRIMARY KEY (a));",
        "s.t",
        vec![
            Values::Int((0..8192).map(Some).collect()),
            Values::Int((0..8192).map(|i| (!null(i)).then_some(i)).collect()),
        ],
    )
}

#[test]
fn a_column_decides_only_through_its_values_and_not_where_it_holds_nulls_and_fills_no_page() {
    // With every value of b present, b's 4 bytes are the narrowest: blocks of 1,024 rows.
    let full = keyed_with_nulls("nulls-none", |_| false).blocks().unwrap();
    assert_eq!(blocks_of(&full, "t_pkey", "a"), [1024; 8]);
    // NULL on every other row, b's values fill a page only every 2,048 rows. It does not decide, and
    // the bigint's page of 512 rows cuts the blocks; b takes a page of its own in each.
    let half = keyed_with_nulls("nulls-half", |i| i % 2 == 1)
        .blocks()
        .unwrap();
    assert_eq!(blocks_of(&half, "t_pkey", "a"), [512; 16]);
    assert_eq!(pages_of(&half, "t_pkey", "b"), [1; 16]);
    assert!(half
        .iter()
        .filter(|r| r.column == "b")
        .all(|r| r.nulls == 256));
    // NULL only from row 4,096 on: b decides the rows that hold none of its NULLs, and not the rest.
    let late = keyed_with_nulls("nulls-late", |i| i >= 4096 && i % 2 == 1)
        .blocks()
        .unwrap();
    let mut want = vec![1024; 4];
    want.extend([512; 8]);
    assert_eq!(blocks_of(&late, "t_pkey", "a"), want);
}

#[test]
fn a_boolean_never_decides_and_takes_a_page_of_its_own_in_each_block() {
    // A bigint alone fills a page at 512 rows. Beside it a boolean, a bit a row, would fill one only
    // at 32,768; it never decides, so the blocks are the bigint's, and it takes a page in each.
    let n = 8192;
    let with = written(
        "boolean-beside",
        "CREATE TABLE s.t (a bigint NOT NULL, f boolean NOT NULL, PRIMARY KEY (a));",
        "s.t",
        vec![
            Values::Int((0..n).map(Some).collect()),
            Values::Int((0..n).map(|i| Some(i % 3 % 2)).collect()),
        ],
    )
    .blocks()
    .unwrap();
    let alone = written(
        "boolean-without",
        "CREATE TABLE s.t (a bigint NOT NULL, PRIMARY KEY (a));",
        "s.t",
        vec![Values::Int((0..n).map(Some).collect())],
    )
    .blocks()
    .unwrap();
    assert_eq!(
        blocks_of(&with, "t_pkey", "a"),
        blocks_of(&alone, "t_pkey", "a")
    );
    assert_eq!(blocks_of(&with, "t_pkey", "a"), [512; 16]);
    assert_eq!(pages_of(&with, "t_pkey", "f"), [1; 16]);
    assert!(with.iter().filter(|r| r.column == "f").all(|r| r.k == 1));
}

const FLAGS: &str = "CREATE TABLE s.flags (id integer NOT NULL, v boolean, w boolean NOT NULL, \
     PRIMARY KEY (id)); CREATE INDEX f_w ON s.flags (w, id);";

/// Ten rows: v true, NULL and false in turn, w true on the even ids.
fn flags(name: &str) -> std::path::PathBuf {
    let path = tmp(&format!("{name}.snail"));
    let catalog = Catalog::parse(FLAGS).unwrap();
    let v = |i: i64| match i % 3 {
        0 => Some(1),
        1 => None,
        _ => Some(0),
    };
    write(
        &path,
        &catalog,
        &[(
            "s.flags".to_string(),
            Rows {
                columns: vec![
                    Values::Int((0..10).map(Some).collect()),
                    Values::Int((0..10).map(v).collect()),
                    Values::Int((0..10).map(|i| Some((i % 2 == 0) as i64)).collect()),
                ],
            },
        )],
    )
    .unwrap();
    path
}

fn ids(b: &arrow::array::RecordBatch) -> Vec<i64> {
    weather::ints64(b.column(0).as_ref())
        .into_iter()
        .map(Option::unwrap)
        .collect()
}

fn truths(a: &dyn Array) -> Vec<Option<bool>> {
    let a = a.as_any().downcast_ref::<BooleanArray>().unwrap();
    (0..a.len())
        .map(|i| (!a.is_null(i)).then(|| a.value(i)))
        .collect()
}

#[test]
fn a_boolean_is_compared_grouped_and_ordered_as_postgresql_does() {
    let path = flags("flags");
    let mut snail = Snail::open(&path).unwrap();
    let ask = |snail: &mut Snail, q: &str| snail.ask(q, None).unwrap().0;
    let q = |w: &str| format!("SELECT f.id FROM s.flags f WHERE {w} ORDER BY f.id");
    for (w, want) in [
        ("f.v", vec![0, 3, 6, 9]),
        ("f.v = true", vec![0, 3, 6, 9]),
        ("f.v IS TRUE", vec![0, 3, 6, 9]),
        ("f.v = 'yes'", vec![0, 3, 6, 9]),
        ("f.v = ' On '", vec![0, 3, 6, 9]),
        ("NOT f.v", vec![2, 5, 8]),
        ("f.v IS FALSE", vec![2, 5, 8]),
        ("f.v = 'of'", vec![2, 5, 8]),
        ("f.v < true", vec![2, 5, 8]),
        ("f.v >= false", vec![0, 2, 3, 5, 6, 8, 9]),
        ("f.w AND f.v", vec![0, 6]),
        ("f.id > 4 AND NOT f.w", vec![5, 7, 9]),
    ] {
        assert_eq!(ids(&ask(&mut snail, &q(w))), want, "{w}");
    }
    for refused in ["f.v = 1", "f.v = 'o'", "f.v = 'truth'", "f.v IS NOT TRUE"] {
        assert!(snail.ask(&q(refused), None).is_err(), "{refused}");
    }

    // On the index f_w, w is a level: false's rows then true's, each by id.
    let (b, round) = snail
        .ask("SELECT f.id FROM s.flags f WHERE f.w = false", Some("f_w"))
        .unwrap();
    assert_eq!(ids(&b), [1, 3, 5, 7, 9]);
    assert_eq!(round.order, "f_w");

    let b = ask(
        &mut snail,
        "SELECT f.v, count(*) FROM s.flags f GROUP BY f.v ORDER BY f.v",
    );
    assert_eq!(
        truths(b.column(0).as_ref()),
        [Some(false), Some(true), None]
    );
    assert_eq!(
        weather::ints64(b.column(1).as_ref()),
        [Some(3), Some(4), Some(3)]
    );
    let b = ask(
        &mut snail,
        "SELECT f.id, f.v FROM s.flags f WHERE f.id < 3 ORDER BY f.id",
    );
    assert_eq!(
        truths(b.column(1).as_ref()),
        [Some(true), None, Some(false)]
    );
    assert!(snail.ask("SELECT sum(f.v) FROM s.flags f", None).is_err());

    // The pipeline reads the same column as truths.
    let b = ask(
        &mut snail,
        "WITH x AS (SELECT f.v, f.w FROM s.flags f) \
         SELECT bool_and(x.w), bool_or(x.w), count(*) FROM x WHERE x.v = 'y'::boolean",
    );
    assert_eq!(truths(b.column(0).as_ref()), [Some(false)]);
    assert_eq!(truths(b.column(1).as_ref()), [Some(true)]);
}

#[test]
#[cfg(feature = "standalone")]
fn a_boolean_is_written_as_postgresql_reads_its_input() {
    let path = flags("flags-writes");
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute("INSERT INTO s.flags (id, v, w) VALUES (10, 'on', 'off'), (11, ' FALSE ', TRUE)")
        .unwrap();
    w.execute("UPDATE s.flags SET v = 'n' WHERE id = 0")
        .unwrap();
    w.execute("UPDATE s.flags SET w = true WHERE id = 1")
        .unwrap();
    for refused in [
        "INSERT INTO s.flags (id, v, w) VALUES (12, 'maybe', true)",
        "INSERT INTO s.flags (id, v, w) VALUES (12, 2, true)",
        "INSERT INTO s.flags (id, v, w) VALUES (12, true, NULL)",
    ] {
        assert!(w.execute(refused).is_err(), "{refused}");
    }
    // An answer of integers is no boolean, though its value is 1, as PostgreSQL refuses it by its
    // type; nor is a boolean answer an integer.
    for (refused, why) in [
        (
            "INSERT INTO s.flags (id, v, w) SELECT f.id, f.id, f.w FROM s.flags f WHERE f.id = 1",
            "s.flags.v is boolean, and the answer gives Int32",
        ),
        (
            "INSERT INTO s.flags (v, id, w) SELECT f.w, f.w, f.w FROM s.flags f WHERE f.id = 1",
            "s.flags.id is integer, and the answer gives Boolean",
        ),
    ] {
        let e = w.execute(refused).err().unwrap().to_string();
        assert!(e.contains(why), "{refused}: {e}");
    }
    drop(w);
    let mut snail = Snail::open(&path).unwrap();
    let (b, _) = snail
        .ask("SELECT f.id, f.v, f.w FROM s.flags f ORDER BY f.id", None)
        .unwrap();
    let got: Vec<(i64, Option<bool>, Option<bool>)> = ids(&b)
        .into_iter()
        .zip(truths(b.column(1).as_ref()))
        .zip(truths(b.column(2).as_ref()))
        .map(|((i, v), w)| (i, v, w))
        .filter(|r| [0, 1, 10, 11].contains(&r.0))
        .collect();
    assert_eq!(
        got,
        [
            (0, Some(false), Some(true)),
            (1, None, Some(true)),
            (10, Some(true), Some(false)),
            (11, Some(false), Some(true)),
        ]
    );
    // w moved on f_w for row 1: its true rows are now ids 0, 1, 2, 4, 6, 8 and 11.
    let (b, _) = snail
        .ask(
            "SELECT f.id FROM s.flags f WHERE f.w ORDER BY f.id",
            Some("f_w"),
        )
        .unwrap();
    assert_eq!(ids(&b), [0, 1, 2, 4, 6, 8, 11]);
}

#[test]
#[cfg(feature = "standalone")]
fn a_snail_of_format_seven_or_eight_refuses_a_boolean_by_name() {
    let catalog = Catalog::parse(FLAGS).unwrap();
    let path = tmp("flags-eight.snail");
    let e = warren_snail::write::write_format_eight(&path, &catalog, &[], weather::span())
        .err()
        .unwrap()
        .to_string();
    assert!(
        e.contains("s.flags.v is boolean, which a snail of format 8 cannot hold")
            && e.contains("`out`"),
        "{e}"
    );
    for name in ["seven", "eight"] {
        let path = tmp(&format!("ints-{name}.snail"));
        let ints =
            Catalog::parse("CREATE TABLE s.t (a integer NOT NULL, PRIMARY KEY (a));").unwrap();
        if name == "seven" {
            warren_snail::write::write_format_seven(&path, &ints, &[], weather::span()).unwrap();
        } else {
            warren_snail::write::write_format_eight(&path, &ints, &[], weather::span()).unwrap();
        }
        let mut w = Snail::open_writing(&path).unwrap();
        let e = w
            .execute("CREATE TABLE s.u (a integer NOT NULL, f boolean, PRIMARY KEY (a))")
            .err()
            .unwrap()
            .to_string();
        assert!(
            e.contains("s.u.f is boolean, which a snail of format"),
            "{e}"
        );
        w.execute("CREATE TABLE s.u (a integer NOT NULL, PRIMARY KEY (a))")
            .unwrap();
    }
}

#[test]
fn a_null_takes_no_width_in_a_column_s_unit_or_in_its_pages() {
    // On t_g a block is a day's 1,024 rows, all of g = 0, so the furthest-out place is the block. b
    // is NULL on every other row: its 512 values are 4,096 bytes, a unit of one page, where at its
    // full width the NULLs would make 8,192 and a unit of two. Its pages are guessed at the 4 bytes a
    // row its values take on the mean, so a block's 1,024 rows take two data pages, not the three of
    // 8.
    let n = 8192;
    let day = 86_400_000_000;
    let snail = written(
        "nulls-unit",
        "CREATE TABLE s.t (a bigint NOT NULL, g integer NOT NULL, at timestamptz NOT NULL, b bigint, \
         PRIMARY KEY (a)); CREATE INDEX t_g ON s.t (g, at) INCLUDE (b);",
        "s.t",
        vec![
            Values::Int((0..n).map(Some).collect()),
            Values::Int((0..n).map(|_| Some(0)).collect()),
            Values::Int(
                (0..n)
                    .map(|i| Some(1_577_836_800_000_000 + i / 1024 * day + i % 1024))
                    .collect(),
            ),
            Values::Int((0..n).map(|i| (i % 2 == 0).then_some(i)).collect()),
        ],
    );
    let rows = snail.blocks().unwrap();
    assert_eq!(blocks_of(&rows, "t_g", "b"), [1024; 8]);
    let b: Vec<&BlockRow> = rows
        .iter()
        .filter(|r| r.order == "t_g" && r.column == "b")
        .collect();
    assert!(b.iter().all(|r| r.k == 1 && r.data_pages == 2), "{b:?}");
}

#[test]
fn a_boolean_level_cuts_its_false_rows_before_its_true_rows() {
    // 8,192 rows, f false on the even ones: on (f, a) each side fills pages of its own, so no block
    // holds both, false's come first, and f is the first rung of every block's place. So a question
    // on f alone reads only its side's blocks.
    let n = 8192;
    let mut snail = written(
        "boolean-level",
        "CREATE TABLE s.t (a bigint NOT NULL, f boolean NOT NULL, PRIMARY KEY (a)); \
         CREATE INDEX t_f ON s.t (f, a);",
        "s.t",
        vec![
            Values::Int((0..n).map(Some).collect()),
            Values::Int((0..n).map(|i| Some(i % 2)).collect()),
        ],
    );
    let (b, round) = snail
        .ask("SELECT t.a FROM s.t t WHERE t.f", Some("t_f"))
        .unwrap();
    assert_eq!(b.num_rows(), 4096);
    assert_eq!(round.blocks_read, 8, "{round:?}");
    let rows = snail.blocks().unwrap();
    let f: Vec<&BlockRow> = rows
        .iter()
        .filter(|r| r.order == "t_f" && r.column == "f")
        .collect();
    assert_eq!(f.len(), 16);
    let side = |key: &str| {
        key.trim_start_matches('[')
            .split(',')
            .next()
            .unwrap()
            .to_string()
    };
    for (i, r) in f.iter().enumerate() {
        assert_eq!(side(&r.first), side(&r.last), "{r:?}");
        assert_eq!(side(&r.first), if i < 8 { "0" } else { "1" }, "{r:?}");
        assert_eq!(side(&r.place), side(&r.first), "{r:?}");
    }
}

#[test]
#[cfg(feature = "standalone")]
fn a_boolean_goes_out_as_parquet_s_boolean_and_comes_back_by_its_pages() {
    use parquet::file::metadata::{PageIndexPolicy, ParquetMetaDataReader};
    use parquet::file::statistics::Statistics;
    let path = flags("flags-out");
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("flags-out");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let outs = Snail::open_writing(&path).unwrap().out(&dir).unwrap();
    let file = &outs[0].file;
    let (batches, _) = weather::parquet_of(file);
    let got: Vec<Option<bool>> = batches
        .iter()
        .flat_map(|b| truths(b.column_by_name("v").unwrap().as_ref()))
        .collect();
    let want: Vec<Option<bool>> = (0..10)
        .map(|i| match i % 3 {
            0 => Some(true),
            1 => None,
            _ => Some(false),
        })
        .collect();
    assert_eq!(got, want);
    let meta = ParquetMetaDataReader::new()
        .with_page_index_policy(PageIndexPolicy::Required)
        .parse_and_finish(&std::fs::File::open(file).unwrap())
        .unwrap();
    let v = meta.row_group(0).column(1);
    match v.statistics() {
        Some(Statistics::Boolean(s)) => {
            assert_eq!((s.min_opt(), s.max_opt()), (Some(&false), Some(&true)));
            assert_eq!(s.null_count_opt(), Some(3));
        }
        other => panic!("{other:?}"),
    }
    let catalog = Catalog::parse(FLAGS).unwrap();
    let table = catalog.table("s.flags").unwrap();
    let rows = Rows::from_batches(table, &batches).unwrap();
    let source = warren_snail::write::source(file, table).unwrap();
    let back = tmp("flags-back.snail");
    for w in warren_snail::write::write_spanned(
        &back,
        &catalog,
        &[("s.flags".to_string(), rows)],
        &[("s.flags".to_string(), source)],
        weather::span(),
    )
    .unwrap()
    {
        if w.order == "flags_pkey" {
            assert!(
                w.columns.iter().all(|c| c.moved_blocks == w.blocks),
                "{w:?}"
            );
        }
    }
    let (b, _) = Snail::open(&back)
        .unwrap()
        .ask("SELECT f.id, f.v FROM s.flags f ORDER BY f.id", None)
        .unwrap();
    assert_eq!(truths(b.column(1).as_ref()), want);
}
