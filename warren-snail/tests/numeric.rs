//! numeric past 18 digits: from 19 to 38 held as the integer of its smallest unit in 128 bits and cut
//! by its decimal digits, any other held as its text and keyed on its class; NaN in a numeric(p, s)
//! as 10^p on its pages, named by `out`; formats 7 and 8 refusing what 0.1.x cannot read.

mod weather;

use arrow::array::{Array, Decimal128Array, StringArray};
use warren_snail::catalog::Catalog;
use warren_snail::read::{BlockRow, Snail, NAN_KEY};
use warren_snail::values::Values;
use warren_snail::write::Rows;
use weather::{tmp, write};

fn blocks_of(rows: &[BlockRow], order: &str, column: &str) -> Vec<u64> {
    rows.iter()
        .filter(|r| r.order == order && r.column == column)
        .map(|r| r.rows)
        .collect()
}

fn texts(a: &dyn Array) -> Vec<Option<String>> {
    let a = a.as_any().downcast_ref::<StringArray>().unwrap();
    (0..a.len())
        .map(|i| (!a.is_null(i)).then(|| a.value(i).to_string()))
        .collect()
}

fn decimals(a: &dyn Array) -> Vec<Option<i128>> {
    let a = a.as_any().downcast_ref::<Decimal128Array>().unwrap();
    (0..a.len())
        .map(|i| (!a.is_null(i)).then(|| a.value(i)))
        .collect()
}

#[test]
fn a_numeric_of_19_to_38_digits_is_cut_by_its_decimal_digits() {
    // 10,000 numbers a step of 10^20 apart, half of them negative, 16 bytes each, 256 a page: each
    // thousand of them fills a page and each hundred does not, so the cut stops at the thousands,
    // where the numbers' bits would cut at powers of two.
    let path = tmp("numeric-cut.snail");
    let catalog =
        Catalog::parse("CREATE TABLE s.k (v numeric(38,0) NOT NULL, PRIMARY KEY (v));").unwrap();
    let step = 10i128.pow(20);
    let rows = Rows {
        columns: vec![Values::Wide(
            (-5000..5000).map(|i| Some(i * step)).collect(),
        )],
    };
    write(&path, &catalog, &[("s.k".into(), rows)]).unwrap();
    let mut s = Snail::open(&path).unwrap();
    let blocks = s.blocks().unwrap();
    assert_eq!(blocks_of(&blocks, "k_pkey", "v"), [1000; 10]);
    // A question on one thousand reads its block alone.
    let (b, round) = s
        .ask(
            "SELECT v FROM s.k WHERE v >= -2000e20 AND v < -1000e20 ORDER BY v",
            None,
        )
        .unwrap();
    assert_eq!(b.num_rows(), 1000);
    assert_eq!(round.blocks_read, 1);
    assert_eq!(
        decimals(b.column(0).as_ref())[0],
        Some(-2000 * step),
        "the block's first number"
    );
}

#[test]
fn a_numeric_held_as_its_text_is_cut_by_its_class_and_answers_its_own_text() {
    // Unconstrained: 1,000 numbers from -500.5 to 499.5, -0.5 not among them, each written three
    // ways, 1.5, 1.50 and 1.500 one class; the class key's digits cut the order, and each row
    // answers its own text.
    let path = tmp("numeric-text.snail");
    let catalog = Catalog::parse(
        "CREATE TABLE s.t (id integer NOT NULL, v numeric NOT NULL, PRIMARY KEY (id)); \
         CREATE INDEX t_v ON s.t (v, id);",
    )
    .unwrap();
    let mut ids = Vec::new();
    let mut vs = Vec::new();
    for i in 0..3000i64 {
        let n = i / 3;
        let zeros = "0".repeat((i % 3) as usize);
        ids.push(Some(i));
        vs.push(Some(format!("{}.5{zeros}", n - 500)));
    }
    let rows = Rows {
        columns: vec![Values::Int(ids), Values::Text(vs)],
    };
    write(&path, &catalog, &[("s.t".into(), rows)]).unwrap();
    let mut s = Snail::open(&path).unwrap();
    let blocks = s.blocks().unwrap();
    let along = blocks_of(&blocks, "t_v", "v");
    assert!(along.len() > 1, "{along:?}");
    assert_eq!(along.iter().sum::<u64>(), 3000);
    // An equality fixes the level: one block read.
    let (b, round) = s
        .ask(
            "SELECT id, v FROM s.t WHERE v = 12.5 ORDER BY id",
            Some("t_v"),
        )
        .unwrap();
    assert_eq!(round.blocks_read, 1);
    assert_eq!(
        texts(b.column(1).as_ref()),
        [
            Some("12.5".into()),
            Some("12.50".into()),
            Some("12.500".into())
        ]
    );
    let (b, _) = s
        .ask(
            "SELECT v, count(*) FROM s.t WHERE v > -2 AND v < 2 GROUP BY v ORDER BY v",
            Some("t_v"),
        )
        .unwrap();
    // Each group answers the text of its first row, the least id.
    assert_eq!(
        texts(b.column(0).as_ref()),
        [Some("-1.5".into()), Some("0.5".into()), Some("1.5".into())]
    );
    // An order on the class: a negative number's longer digits first.
    let (b, _) = s
        .ask(
            "SELECT v FROM s.t WHERE v < -498 AND v > -500 ORDER BY v",
            Some("t_v"),
        )
        .unwrap();
    assert_eq!(
        texts(b.column(0).as_ref()),
        [
            Some("-499.5".into()),
            Some("-499.50".into()),
            Some("-499.500".into()),
            Some("-498.5".into()),
            Some("-498.50".into()),
            Some("-498.500".into())
        ]
    );
}

#[test]
fn nan_lies_on_a_numeric_s_pages_as_10_to_its_precision_and_out_names_it() {
    let path = tmp("numeric-nan.snail");
    let catalog = Catalog::parse(
        "CREATE TABLE s.n (id integer NOT NULL, a numeric(5,2), w numeric(30,2), \
         PRIMARY KEY (id));",
    )
    .unwrap();
    let rows = Rows {
        columns: vec![
            Values::Int((0..4).map(Some).collect()),
            Values::Int(vec![Some(150), Some(100_000), None, Some(-99_999)]),
            Values::Wide(vec![
                Some(10i128.pow(30)),
                Some(12_345),
                Some(-(10i128.pow(30) - 1)),
                None,
            ]),
        ],
    };
    write(&path, &catalog, &[("s.n".into(), rows)]).unwrap();
    let mut s = Snail::open_writing(&path).unwrap();
    let (b, _) = s.ask("SELECT id, a, w FROM s.n ORDER BY id", None).unwrap();
    assert_eq!(
        decimals(b.column(1).as_ref()),
        [Some(150), Some(100_000), None, Some(-99_999)]
    );
    // NaN above +Infinity, and every number below it.
    let (b, _) = s
        .ask("SELECT id FROM s.n WHERE a > 'Infinity'", None)
        .unwrap();
    assert_eq!(weather::ints64(b.column(0).as_ref()), [Some(1)]);
    let (b, _) = s
        .ask("SELECT id FROM s.n WHERE a < 1e40 ORDER BY id", None)
        .unwrap();
    assert_eq!(weather::ints64(b.column(0).as_ref()), [Some(0), Some(3)]);
    let (b, _) = s
        .ask("SELECT id FROM s.n WHERE a <= 1.5 ORDER BY id", None)
        .unwrap();
    assert_eq!(weather::ints64(b.column(0).as_ref()), [Some(0), Some(3)]);
    let (b, _) = s
        .ask("SELECT id FROM s.n WHERE w < 'NaN' ORDER BY id", None)
        .unwrap();
    assert_eq!(weather::ints64(b.column(0).as_ref()), [Some(1), Some(2)]);
    let (b, _) = s
        .ask("SELECT max(w) AS m, sum(a) AS s FROM s.n", None)
        .unwrap();
    assert_eq!(decimals(b.column(0).as_ref()), [Some(10i128.pow(30))]);
    assert_eq!(texts(b.column(1).as_ref()), [Some("NaN".into())]);

    let dir = tmp("numeric-nan-parquet");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let outs = s.out(&dir).unwrap();
    let file = &outs.iter().find(|t| t.table == "s.n").unwrap().file;
    let reader =
        parquet::file::reader::SerializedFileReader::new(std::fs::File::open(file).unwrap())
            .unwrap();
    let meta = parquet::file::reader::FileReader::metadata(&reader).clone();
    let named = meta
        .file_metadata()
        .key_value_metadata()
        .unwrap()
        .iter()
        .find(|k| k.key == NAN_KEY)
        .and_then(|k| k.value.clone())
        .unwrap();
    assert_eq!(
        named,
        format!("a\t100000\nw\t{}", 10i128.pow(30)),
        "each column holding a NaN, and its integer"
    );
    let schema = meta.file_metadata().schema_descr();
    assert_eq!(
        (
            schema.column(1).physical_type(),
            schema.column(2).physical_type()
        ),
        (
            parquet::basic::Type::INT64,
            parquet::basic::Type::FIXED_LEN_BYTE_ARRAY
        )
    );
    assert_eq!(schema.column(2).type_length(), 13);
    // A decimal's least and greatest, signed as Parquet orders it.
    let w = meta.row_group(0).column(2).statistics().unwrap();
    assert_eq!(
        (w.min_bytes_opt().unwrap(), w.max_bytes_opt().unwrap()),
        (
            &(-(10i128.pow(30) - 1)).to_be_bytes()[3..],
            &10i128.pow(30).to_be_bytes()[3..]
        )
    );
    // Back by write, NaN NaN again.
    let (batches, _) = weather::parquet_of(file);
    let table = catalog.table("s.n").unwrap();
    let back = tmp("numeric-nan-back.snail");
    write(
        &back,
        &catalog,
        &[("s.n".into(), Rows::from_batches(table, &batches).unwrap())],
    )
    .unwrap();
    let (b, _) = Snail::open(&back)
        .unwrap()
        .ask("SELECT id FROM s.n WHERE w = 'NaN'", None)
        .unwrap();
    assert_eq!(weather::ints64(b.column(0).as_ref()), [Some(0)]);
}

#[test]
fn a_sum_past_128_bits_is_held_whole() {
    let path = tmp("numeric-sum.snail");
    let catalog = Catalog::parse(
        "CREATE TABLE s.m (id integer NOT NULL, v numeric(38,0), PRIMARY KEY (id)); \
         CREATE TABLE s.h (id integer NOT NULL, v numeric(38,0), PRIMARY KEY (id));",
    )
    .unwrap();
    let big = 10i128.pow(38) - 1;
    let rows = Rows {
        columns: vec![
            Values::Int((0..1000).map(Some).collect()),
            Values::Wide((0..1000).map(|_| Some(big)).collect()),
        ],
    };
    let h = Rows {
        columns: vec![
            Values::Int(vec![Some(1), Some(2)]),
            Values::Wide(vec![Some(6 * 10i128.pow(37)); 2]),
        ],
    };
    write(&path, &catalog, &[("s.m".into(), rows), ("s.h".into(), h)]).unwrap();
    let (b, _) = Snail::open(&path)
        .unwrap()
        .ask("SELECT sum(v) AS s, avg(v) AS a FROM s.m", None)
        .unwrap();
    // 1,000 times 10^38 - 1, past what 38 digits hold, so its text.
    assert_eq!(texts(b.column(0).as_ref()), [Some(format!("{big}000"))]);
    assert_eq!(texts(b.column(1).as_ref()), [Some(big.to_string())]);
    // So is a sum within 128 bits and past 38 digits.
    let (b, _) = Snail::open(&path)
        .unwrap()
        .ask("SELECT sum(v) AS s FROM s.h", None)
        .unwrap();
    assert_eq!(
        texts(b.column(0).as_ref()),
        [Some(format!("12{}", "0".repeat(37)))]
    );
}

#[test]
fn a_snail_of_format_eight_refuses_a_numeric_past_18_digits_and_a_nan_by_name() {
    for (ddl, column) in [
        (
            "CREATE TABLE s.k (id integer NOT NULL, v numeric(19,0), PRIMARY KEY (id));",
            "s.k.v is numeric(19,0)",
        ),
        (
            "CREATE TABLE s.k (id integer NOT NULL, v numeric, PRIMARY KEY (id));",
            "s.k.v is numeric",
        ),
    ] {
        let catalog = Catalog::parse(ddl).unwrap();
        let e = warren_snail::write::write_format_eight(
            &tmp("numeric-eight.snail"),
            &catalog,
            &[],
            weather::span(),
        )
        .err()
        .unwrap()
        .to_string();
        assert!(
            e.contains(&format!("{column}, which a snail of format 8 cannot hold")),
            "{e}"
        );
    }
    let catalog =
        Catalog::parse("CREATE TABLE s.k (id integer NOT NULL, v numeric(5,2), PRIMARY KEY (id));")
            .unwrap();
    let rows = Rows {
        columns: vec![
            Values::Int(vec![Some(1), Some(2)]),
            Values::Int(vec![Some(150), Some(100_000)]),
        ],
    };
    let e = warren_snail::write::write_format_eight(
        &tmp("numeric-eight-nan.snail"),
        &catalog,
        &[("s.k".into(), rows)],
        weather::span(),
    )
    .err()
    .unwrap()
    .to_string();
    assert!(
        e.contains("s.k.v holds NaN, which a snail of format 8 cannot hold"),
        "{e}"
    );
    // A NaN written into a snail of format 8 by a statement is refused alike.
    let path = tmp("numeric-eight-insert.snail");
    warren_snail::write::write_format_eight(&path, &catalog, &[], weather::span()).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute("INSERT INTO s.k (id, v) VALUES (1, 1.5)")
        .unwrap();
    let e = w
        .execute("INSERT INTO s.k (id, v) VALUES (2, 'NaN')")
        .unwrap_err()
        .to_string();
    assert!(e.contains("holds NaN"), "{e}");
}

/// Each row's values as text: a decimal at its scale, text as it is.
fn row_texts(b: &arrow::array::RecordBatch) -> Vec<Vec<Option<String>>> {
    (0..b.num_rows())
        .map(|r| {
            b.columns()
                .iter()
                .map(|a| weather::cell(a.as_ref(), r))
                .collect()
        })
        .collect()
}

#[test]
fn a_pipeline_computes_a_numeric_at_numeric_c_s_scales() {
    // Each answer read from PostgreSQL 18.6, 2026-10-30.
    let path = tmp("numeric-pipeline.snail");
    let catalog =
        Catalog::parse("CREATE TABLE s.p (id integer NOT NULL, v numeric(5,2), PRIMARY KEY (id));")
            .unwrap();
    let rows = Rows {
        columns: vec![
            Values::Int(vec![Some(1), Some(2), Some(3)]),
            Values::Int(vec![Some(150), Some(-99_999), Some(100_000)]),
        ],
    };
    write(&path, &catalog, &[("s.p".into(), rows)]).unwrap();
    let mut s = Snail::open(&path).unwrap();
    let (b, _) = s
        .ask(
            "WITH t AS (SELECT id, v FROM s.p) \
             SELECT id, v / 3 AS q, v % 0.7 AS r, v * v AS p, v + 1 AS s FROM t ORDER BY id",
            None,
        )
        .unwrap();
    let text = |r: [&str; 5]| r.iter().map(|x| Some(x.to_string())).collect::<Vec<_>>();
    assert_eq!(
        row_texts(&b),
        [
            text(["1", "0.50000000000000000000", "0.10", "2.2500", "2.50"]),
            text([
                "2",
                "-333.3300000000000000",
                "-0.39",
                "999980.0001",
                "-998.99"
            ]),
            text(["3", "NaN", "NaN", "NaN", "NaN"]),
        ]
    );
    // A quoted literal beside a numeric is one, whatever its scale.
    let (b, _) = s
        .ask(
            "WITH t AS (SELECT id, v FROM s.p) SELECT id FROM t WHERE v = '1.5000' ORDER BY id",
            None,
        )
        .unwrap();
    assert_eq!(weather::ints64(b.column(0).as_ref()), [Some(1)]);
    // Numbers of more than one scale come out as each one's text.
    let (b, _) = s
        .ask(
            "WITH t AS (SELECT id, v FROM s.p) \
             SELECT id, CASE WHEN id = 1 THEN 1.5 WHEN id = 2 THEN 2 ELSE 0.25 END AS x \
             FROM t ORDER BY id",
            None,
        )
        .unwrap();
    assert_eq!(
        texts(b.column(1).as_ref()),
        [Some("1.5".into()), Some("2".into()), Some("0.25".into())]
    );
}

#[test]
fn a_numeric_is_written_held_to_its_typmod_and_assigned_as_postgresql_assigns_it() {
    let path = tmp("numeric-assign.snail");
    let catalog = Catalog::parse(
        "CREATE TABLE s.a (id integer NOT NULL, t numeric(3,-1), m money, n numeric, \
         PRIMARY KEY (id)); \
         CREATE TABLE s.f (id integer NOT NULL, d double precision, PRIMARY KEY (id)); \
         CREATE TABLE s.b (id integer NOT NULL, m money, PRIMARY KEY (id));",
    )
    .unwrap();
    write(&path, &catalog, &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute("INSERT INTO s.a (id, t) VALUES (1, '125'), (2, 9994)")
        .unwrap();
    w.execute("INSERT INTO s.f (id, d) VALUES (3, 'NaN'), (4, '-Infinity')")
        .unwrap();
    w.execute("INSERT INTO s.a (id, n) SELECT f.id, f.d FROM s.f f")
        .unwrap();
    w.execute("INSERT INTO s.b (id, m) SELECT a.id, a.t FROM s.a a WHERE a.id = 1")
        .unwrap();
    let (b, _) = w
        .ask(
            "WITH b AS (SELECT id, m FROM s.b) SELECT m::text AS m FROM b",
            None,
        )
        .unwrap();
    assert_eq!(texts(b.column(0).as_ref()), [Some("$130.00".into())]);
    let (b, _) = w
        .ask(
            "WITH a AS (SELECT id, t, m, n FROM s.a) \
             SELECT id, t::text AS t, m::text AS m, n::text AS n FROM a ORDER BY id",
            None,
        )
        .unwrap();
    let row = |r: [Option<&str>; 4]| r.iter().map(|x| x.map(str::to_string)).collect::<Vec<_>>();
    assert_eq!(
        row_texts(&b),
        [
            row([Some("1"), Some("130"), None, None]),
            row([Some("2"), Some("9990"), None, None]),
            row([Some("3"), None, None, Some("NaN")]),
            row([Some("4"), None, None, Some("-Infinity")]),
        ]
    );
    let e = w
        .execute("INSERT INTO s.a (id, t) VALUES (5, 9995)")
        .unwrap_err()
        .to_string();
    assert!(e.contains("numeric field overflow"), "{e}");
}

#[test]
fn a_numeric_s_sum_by_a_text_s_codes_leaves_its_nan_to_the_rows() {
    // 2,000 rows of two stores, read by their codes: the sum of one of them holds a NaN.
    let path = tmp("numeric-run.snail");
    let catalog = Catalog::parse(
        "CREATE TABLE s.r (id integer NOT NULL, g text NOT NULL, v numeric(5,2), \
         PRIMARY KEY (id));",
    )
    .unwrap();
    let rows = Rows {
        columns: vec![
            Values::Int((0..2000).map(Some).collect()),
            Values::Text(
                (0..2000)
                    .map(|i| Some(if i % 2 == 0 { "a" } else { "b" }.to_string()))
                    .collect(),
            ),
            Values::Int(
                (0..2000)
                    .map(|i| Some(if i == 6 { 100_000 } else { 1 }))
                    .collect(),
            ),
        ],
    };
    write(&path, &catalog, &[("s.r".into(), rows)]).unwrap();
    let (b, _) = Snail::open(&path)
        .unwrap()
        .ask("SELECT g, sum(v) AS s FROM s.r GROUP BY g ORDER BY g", None)
        .unwrap();
    assert_eq!(
        texts(b.column(1).as_ref()),
        [Some("NaN".into()), Some("10.00".into())]
    );
}

#[test]
fn a_pipeline_divides_integers_as_postgresql_does() {
    let path = tmp("numeric-int.snail");
    let catalog =
        Catalog::parse("CREATE TABLE s.i (id integer NOT NULL, PRIMARY KEY (id));").unwrap();
    let rows = Rows {
        columns: vec![Values::Int(vec![Some(7), Some(-7)])],
    };
    write(&path, &catalog, &[("s.i".into(), rows)]).unwrap();
    // Each answer read from PostgreSQL 18.6, 2026-10-30: a quotient cut toward zero, int8mod by -1
    // always 0.
    let (b, _) = Snail::open(&path)
        .unwrap()
        .ask(
            "WITH t AS (SELECT id FROM s.i) \
             SELECT id, id / 2 AS q, id % 2 AS r, \
             CASE WHEN id > 0 THEN (id - 9223372036854775807 - 8) % -1 END AS m \
             FROM t ORDER BY id DESC",
            None,
        )
        .unwrap();
    let ints = |c: usize| weather::ints64(b.column(c).as_ref());
    assert_eq!(
        (ints(1), ints(2), ints(3)),
        (
            vec![Some(3), Some(-3)],
            vec![Some(1), Some(-1)],
            vec![Some(0), None]
        )
    );
}

#[test]
fn a_numeric_past_18_digits_is_assigned_from_an_answer() {
    let path = tmp("numeric-select.snail");
    let catalog = Catalog::parse(
        "CREATE TABLE s.x (id integer NOT NULL, v numeric(30,2), PRIMARY KEY (id)); \
         CREATE TABLE s.y (id integer NOT NULL, v numeric(30,4), PRIMARY KEY (id));",
    )
    .unwrap();
    write(&path, &catalog, &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute("INSERT INTO s.x (id, v) VALUES (1, '1234567890123456789012345.67'), (2, 'NaN')")
        .unwrap();
    w.execute("INSERT INTO s.y (id, v) SELECT x.id, x.v FROM s.x x")
        .unwrap();
    let (b, _) = w
        .ask(
            "WITH y AS (SELECT id, v FROM s.y) SELECT id, v::text AS v FROM y ORDER BY id",
            None,
        )
        .unwrap();
    assert_eq!(
        texts(b.column(1).as_ref()),
        [
            Some("1234567890123456789012345.6700".into()),
            Some("NaN".into())
        ]
    );
}
