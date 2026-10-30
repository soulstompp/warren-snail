//! real and double precision: each value held as its own bits and keyed on its class, `-0` with `0`
//! and every NaN one place above Infinity; cut by the key's bits, compared, grouped, ordered and
//! summed as PostgreSQL does, a real beside a number written out compared as a double precision.

mod weather;

#[cfg(feature = "standalone")]
use arrow::array::{Array, Float32Array, Float64Array};
use warren_snail::catalog::Catalog;
use warren_snail::read::{BlockRow, Snail};
use warren_snail::values::Values;
use warren_snail::write::Rows;
#[cfg(feature = "standalone")]
use weather::{exact, ints64};
use weather::{tmp, write};

fn blocks_of(rows: &[BlockRow], order: &str, column: &str) -> Vec<u64> {
    rows.iter()
        .filter(|r| r.order == order && r.column == column)
        .map(|r| r.rows)
        .collect()
}

#[test]
fn a_real_is_cut_by_its_key_s_bits_on_the_four_bytes_the_file_writes() {
    // 4,096 reals spread evenly over the keys of the finite reals below 2: 1,024 rows of 4 bytes
    // fill a page, so the cut stops at four parts, as for an integer.
    let catalog = Catalog::parse("CREATE TABLE s.f (v real NOT NULL, PRIMARY KEY (v));").unwrap();
    let reals: Vec<Option<u32>> = (0..4096u32).map(|i| Some(i << 18)).collect();
    let path = tmp("real-cut.snail");
    write(
        &path,
        &catalog,
        &[(
            "s.f".into(),
            Rows {
                columns: vec![Values::Float4(reals)],
            },
        )],
    )
    .unwrap();
    let blocks = Snail::open(&path).unwrap().blocks().unwrap();
    assert_eq!(blocks_of(&blocks, "f_pkey", "v"), [1024; 4]);
}

#[cfg(feature = "standalone")]
const DDL: &str = "CREATE TABLE s.f (n integer NOT NULL, r real, d double precision, \
                   PRIMARY KEY (n)); \
                   CREATE INDEX f_r ON s.f (r DESC, n); \
                   CREATE INDEX f_d ON s.f (d, n);";

#[cfg(feature = "standalone")]
fn written(name: &str) -> std::path::PathBuf {
    let path = tmp(&format!("{name}.snail"));
    write(&path, &Catalog::parse(DDL).unwrap(), &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute(
        "INSERT INTO s.f (n, r, d) VALUES (1, '0.1', '0.1'), (2, '-0', '-0'), (3, '0', '0'), \
         (4, 'NaN', 'NaN'), (5, '-NaN', '-NaN'), (6, 'Infinity', 'Infinity'), \
         (7, '-Infinity', '-Infinity'), (8, '1.5', '1.5'), (9, '1e-45', '5e-324'), (10, NULL, NULL)",
    )
    .unwrap();
    drop(w);
    path
}

#[cfg(feature = "standalone")]
fn ns(b: &arrow::array::RecordBatch) -> Vec<i64> {
    ints64(b.column(0).as_ref()).into_iter().flatten().collect()
}

#[test]
#[cfg(feature = "standalone")]
fn a_float_is_compared_grouped_ordered_and_summed_as_postgresql_does() {
    let path = written("floats-ask");
    let mut snail = Snail::open(&path).unwrap();
    let mut ask = |q: &str, order: Option<&str>| {
        let (b, round) = snail.ask(q, order).unwrap();
        exact(q, &round);
        b
    };
    // -0 is 0, and every NaN is one NaN, above Infinity.
    assert_eq!(
        ns(&ask(
            "SELECT f.n FROM s.f f WHERE f.r = '0' ORDER BY f.n",
            Some("f_r")
        )),
        [2, 3]
    );
    assert_eq!(
        ns(&ask(
            "SELECT f.n FROM s.f f WHERE f.d > 'Infinity' ORDER BY f.n",
            Some("f_d")
        )),
        [4, 5]
    );
    assert_eq!(
        ns(&ask(
            "SELECT f.n, f.d FROM s.f f WHERE f.d IS NOT NULL ORDER BY f.d, f.n",
            Some("f_d")
        )),
        [7, 2, 3, 9, 1, 8, 6, 4, 5]
    );
    // A real beside a number written out is compared as a double precision: no real is 0.1.
    assert_eq!(
        ns(&ask("SELECT f.n FROM s.f f WHERE f.r = 0.1", None)),
        Vec::<i64>::new()
    );
    assert_eq!(
        ns(&ask(
            "SELECT f.n FROM s.f f WHERE f.r > 0.1 AND f.r < 1.5 ORDER BY f.n",
            None
        )),
        [1]
    );
    assert_eq!(ns(&ask("SELECT f.n FROM s.f f WHERE f.r = 1.5", None)), [8]);
    // A double compared with 0.1 finds its own.
    assert_eq!(ns(&ask("SELECT f.n FROM s.f f WHERE f.d = 0.1", None)), [1]);
    // Grouped on the class: -0 and 0 are one group, the NaNs another.
    let (b, _) = snail
        .ask("SELECT f.r, count(*) FROM s.f f GROUP BY f.r", None)
        .unwrap();
    assert_eq!(b.num_rows(), 8);
    // Its own bits come out: -0 as -0.
    let (b, _) = snail
        .ask("SELECT f.d FROM s.f f WHERE f.n = 2", None)
        .unwrap();
    let d = b.column(0).as_any().downcast_ref::<Float64Array>().unwrap();
    assert_eq!(d.value(0).to_bits(), (-0.0f64).to_bits());
    // min and max, sum and avg on the question path, in the column's own type; a sum of finite
    // numbers that overflows is refused.
    let (b, _) = snail
        .ask(
            "SELECT min(f.r), max(f.r), sum(f.r), avg(f.d) FROM s.f f \
             WHERE f.r >= '0.1' AND f.r <= '1.5'",
            None,
        )
        .unwrap();
    let r = |i: usize| {
        b.column(i)
            .as_any()
            .downcast_ref::<Float32Array>()
            .unwrap()
            .value(0)
    };
    assert_eq!((r(0), r(1), r(2)), (0.1f32, 1.5f32, 0.1f32 + 1.5f32));
    let avg = b
        .column(3)
        .as_any()
        .downcast_ref::<Float64Array>()
        .unwrap()
        .value(0);
    assert_eq!(avg, (0.1f64 + 1.5) / 2.0);
    let (b, _) = snail.ask("SELECT max(f.d) FROM s.f f", None).unwrap();
    assert!(b
        .column(0)
        .as_any()
        .downcast_ref::<Float64Array>()
        .unwrap()
        .value(0)
        .is_nan());
}

#[test]
#[cfg(feature = "standalone")]
fn a_pipeline_reads_a_float_s_operators_and_casts_as_postgresql_does() {
    let path = written("floats-pipe");
    let mut snail = Snail::open(&path).unwrap();
    let mut texts = |q: &str| -> Vec<Option<String>> {
        let (b, _) = snail.ask(q, None).unwrap();
        weather::texts(b.column(0).as_ref())
    };
    let step = "WITH t AS (SELECT n, r, d FROM s.f)";
    assert_eq!(
        texts(&format!("{step} SELECT d::text AS x, n FROM t ORDER BY n")),
        [
            "0.1",
            "-0",
            "0",
            "NaN",
            "NaN",
            "Infinity",
            "-Infinity",
            "1.5",
            "5e-324"
        ]
        .map(|s| Some(s.to_string()))
        .into_iter()
        .chain([None])
        .collect::<Vec<_>>()
    );
    // Two reals add as reals; a real and an integer as doubles.
    assert_eq!(
        texts(&format!(
            "{step} SELECT (r + r)::text AS x, n FROM t WHERE n = 1"
        )),
        [Some("0.2".into())]
    );
    assert_eq!(
        texts(&format!(
            "{step} SELECT (r + 1)::text AS x, n FROM t WHERE n = 1"
        )),
        [Some("1.1000000014901161".into())]
    );
    assert_eq!(
        texts(&format!(
            "{step} SELECT CAST(d AS integer)::text AS x, n FROM t WHERE n = 8"
        )),
        [Some("2".into())]
    );
    // A float rounds half to even into an integer.
    assert_eq!(
        texts(&format!(
            "{step} SELECT CAST(d + 1 AS integer)::text AS x, n FROM t WHERE n = 8"
        )),
        [Some("2".into())]
    );
    assert_eq!(
        texts(&format!(
            "{step} SELECT avg(d)::text AS x FROM t WHERE n = 1 OR n = 8"
        )),
        [Some("0.8".into())]
    );
    for q in [
        format!("{step} SELECT r * '3e38' AS x FROM t WHERE n = 8"),
        format!("{step} SELECT d / 0 AS x FROM t WHERE n = 1"),
        // Zero over zero is refused as a division by zero, not taken as NaN.
        format!("{step} SELECT d / 0 AS x FROM t WHERE n = 3"),
        format!("{step} SELECT d * 1e-320 AS x FROM t WHERE n = 9"),
        format!("{step} SELECT CAST(d AS real) AS x FROM t WHERE n = 9"),
        format!("{step} SELECT CAST(d AS integer) AS x FROM t WHERE n = 4"),
    ] {
        assert!(snail.ask(&q, None).is_err(), "{q}");
    }
}

#[test]
#[cfg(feature = "standalone")]
fn a_float_goes_out_as_its_own_bits_and_comes_back() {
    let path = written("floats-out");
    let dir = tmp("floats-out-parquet");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    let outs = w.out(&dir).unwrap();
    let f = outs.iter().find(|t| t.table == "s.f").unwrap();
    let (batches, _) = weather::parquet_of(&f.file);
    let d = batches[0].column_by_name("d").unwrap();
    let d = d.as_any().downcast_ref::<Float64Array>().unwrap();
    // -0 and -NaN keep their sign bit.
    assert_eq!(d.value(1).to_bits(), 0x8000_0000_0000_0000);
    assert_eq!(d.value(4).to_bits(), 0xfff8_0000_0000_0000);
    let catalog = Catalog::parse(DDL).unwrap();
    let rows = Rows::from_batches(catalog.table("s.f").unwrap(), &batches).unwrap();
    let back = tmp("floats-back.snail");
    write(&back, &catalog, &[("s.f".into(), rows)]).unwrap();
    let mut again = Snail::open(&back).unwrap();
    let (b, _) = again
        .ask("SELECT f.n FROM s.f f WHERE f.r = '1e-45'", None)
        .unwrap();
    assert_eq!(ns(&b), [9]);
}

#[test]
#[cfg(feature = "standalone")]
fn a_float_sum_finite_numbers_make_infinite_is_refused() {
    let path = tmp("floats-over.snail");
    let catalog =
        Catalog::parse("CREATE TABLE s.g (n integer NOT NULL, r real, PRIMARY KEY (n));").unwrap();
    write(&path, &catalog, &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute("INSERT INTO s.g (n, r) VALUES (1, '3e38'), (2, '3e38'), (3, '-1')")
        .unwrap();
    assert!(w.ask("SELECT sum(g.r) FROM s.g g", None).is_err());
    assert!(w
        .ask(
            "WITH t AS (SELECT n, r FROM s.g) SELECT sum(r) AS x FROM t",
            None
        )
        .is_err());
    let (b, _) = w
        .ask("SELECT sum(g.r) FROM s.g g WHERE g.n > 1", None)
        .unwrap();
    assert_eq!(
        b.column(0)
            .as_any()
            .downcast_ref::<Float32Array>()
            .unwrap()
            .value(0),
        3e38f32 - 1.0
    );
}

/// What a file says of a float column of a row group: its statistics' NULLs, least and greatest as
/// bytes, and whether a column index holds its pages.
type Said = (Option<u64>, Option<Vec<u8>>, Option<Vec<u8>>, bool);

/// What a file says of each float column of its first row group.
fn float_statistics(path: &std::path::Path) -> Vec<Said> {
    use parquet::file::metadata::{PageIndexPolicy, ParquetMetaDataReader};
    use parquet::file::page_index::column_index::ColumnIndexMetaData;
    let file = std::fs::File::open(path).unwrap();
    let meta = ParquetMetaDataReader::new()
        .with_page_index_policy(PageIndexPolicy::Optional)
        .parse_and_finish(&file)
        .unwrap();
    let rg = meta.row_group(0);
    let index = meta.column_index();
    (1..rg.num_columns())
        .map(|c| {
            let s = rg.column(c).statistics().unwrap();
            let indexed = index.is_some_and(|i| !matches!(i[0][c], ColumnIndexMetaData::NONE));
            (
                s.null_count_opt(),
                s.min_bytes_opt().map(<[u8]>::to_vec),
                s.max_bytes_opt().map(<[u8]>::to_vec),
                indexed,
            )
        })
        .collect()
}

#[test]
fn a_float_s_statistics_out_are_those_the_parquet_library_writes() {
    use arrow::array::{Float32Array, Float64Array, Int32Array};
    use parquet::arrow::ArrowWriter;
    // NULLs, NaNs, both zeros, the infinities: each case its own table, one row group.
    let nan = f64::NAN;
    let cases: [&[Option<f64>]; 7] = [
        &[
            Some(1.5),
            None,
            Some(nan),
            Some(-0.0),
            Some(0.0),
            Some(-2.0),
        ],
        &[Some(nan), Some(nan), None],
        &[Some(-0.0)],
        &[Some(0.0), Some(1.0)],
        &[Some(-1.0), Some(-0.0)],
        &[None, None],
        &[
            Some(f64::INFINITY),
            Some(f64::NEG_INFINITY),
            Some(nan),
            Some(5e-324),
        ],
    ];
    for (i, values) in cases.iter().enumerate() {
        let ddl =
            "CREATE TABLE s.f (id integer NOT NULL, r real, d double precision, PRIMARY KEY (id));";
        let catalog = Catalog::parse(ddl).unwrap();
        let ids: Vec<i32> = (0..values.len() as i32).collect();
        let path = tmp(&format!("float-statistics-{i}.snail"));
        write(
            &path,
            &catalog,
            &[(
                "s.f".into(),
                Rows {
                    columns: vec![
                        Values::Int(ids.iter().map(|&x| Some(i64::from(x))).collect()),
                        Values::Float4(
                            values
                                .iter()
                                .map(|v| v.map(|f| (f as f32).to_bits()))
                                .collect(),
                        ),
                        Values::Float8(values.iter().map(|v| v.map(f64::to_bits)).collect()),
                    ],
                },
            )],
        )
        .unwrap();
        let dir = tmp(&format!("float-statistics-{i}-out"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let outs = Snail::open(&path).unwrap().out(&dir).unwrap();
        // The same values through the plain library's own writer.
        let theirs = dir.join("parquet.parquet");
        let batch = arrow::array::RecordBatch::try_from_iter(vec![
            (
                "id",
                std::sync::Arc::new(Int32Array::from(ids.clone())) as _,
            ),
            (
                "r",
                std::sync::Arc::new(Float32Array::from(
                    values
                        .iter()
                        .map(|v| v.map(|f| f as f32))
                        .collect::<Vec<_>>(),
                )) as _,
            ),
            (
                "d",
                std::sync::Arc::new(Float64Array::from(values.to_vec())) as _,
            ),
        ])
        .unwrap();
        let mut w = ArrowWriter::try_new(
            std::fs::File::create(&theirs).unwrap(),
            batch.schema(),
            None,
        )
        .unwrap();
        w.write(&batch).unwrap();
        w.close().unwrap();
        assert_eq!(
            float_statistics(&outs[0].file),
            float_statistics(&theirs),
            "case {i}: {values:?}"
        );
    }
}
