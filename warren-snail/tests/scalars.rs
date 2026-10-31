//! The fixed scalars keyed as numbers: oid, xid, cid, xid8, pg_lsn, `"char"` and money. Each is
//! held in the signed range of its width, cut by its bits as an integer is, compared, grouped,
//! ordered and summed as PostgreSQL does, and read and written as PostgreSQL reads and writes it.

mod weather;

#[cfg(feature = "standalone")]
use arrow::array::{Array, UInt32Array, UInt64Array, UInt8Array};
use warren_snail::catalog::Catalog;
#[cfg(feature = "standalone")]
use warren_snail::catalog::ColumnType;
use warren_snail::read::{BlockRow, Snail};
#[cfg(feature = "standalone")]
use warren_snail::scalars::scalar_parse;
use warren_snail::scalars::unsigned_key;
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
fn an_oid_is_cut_by_its_bits_as_an_integer_is() {
    // 4,096 oids spread evenly over their 32 bits, across the top bit: held in the signed range,
    // they cut as 4,096 integers spread so do, 1,024 rows of 4 bytes a page.
    let catalog = Catalog::parse(
        "CREATE TABLE s.o (id oid NOT NULL, PRIMARY KEY (id)); \
         CREATE TABLE s.i (id integer NOT NULL, PRIMARY KEY (id));",
    )
    .unwrap();
    let step = 1u64 << 20;
    let oids = Values::Int(
        (0..4096)
            .map(|i| Some(unsigned_key(i * step, 32)))
            .collect(),
    );
    let ints = Values::Int(
        (0..4096)
            .map(|i| Some((i * step) as i64 - (1i64 << 31)))
            .collect(),
    );
    let path = tmp("oid-cut.snail");
    write(
        &path,
        &catalog,
        &[
            (
                "s.o".into(),
                Rows {
                    columns: vec![oids],
                },
            ),
            (
                "s.i".into(),
                Rows {
                    columns: vec![ints],
                },
            ),
        ],
    )
    .unwrap();
    let blocks = Snail::open(&path).unwrap().blocks().unwrap();
    assert_eq!(blocks_of(&blocks, "o_pkey", "id"), [1024; 4]);
    assert_eq!(
        blocks_of(&blocks, "o_pkey", "id"),
        blocks_of(&blocks, "i_pkey", "id")
    );
}

#[cfg(feature = "standalone")]
const DDL: &str = "CREATE TABLE s.k (n integer NOT NULL, o oid, x xid, c cid, x8 xid8, l pg_lsn, \
                   ch \"char\", m money, PRIMARY KEY (n)); \
                   CREATE INDEX k_o ON s.k (o DESC, n); \
                   CREATE INDEX k_l ON s.k (l, n); \
                   CREATE INDEX k_ch ON s.k (ch, n); \
                   CREATE INDEX k_m ON s.k (m, n);";

#[cfg(feature = "standalone")]
/// Rows of s.k: each column's ends, the values either side of its top bit, and a NULL.
const ROWS: &str = "INSERT INTO s.k (n, o, x, c, x8, l, ch, m) VALUES \
    (1, 1, '1', '1', '1', '0/1', 'a', '1.00'), \
    (2, 4294967295, '4294967295', '4294967295', '18446744073709551615', 'FFFFFFFF/FFFFFFFF', '\\377', '92233720368547758.07'), \
    (3, 2147483648, '2147483648', '2147483648', '9223372036854775808', '80000000/0', '\\200', '-92233720368547758.08'), \
    (4, 2147483647, '2147483647', '0', '9223372036854775807', '7FFFFFFF/FFFFFFFF', 'z', '-0.01'), \
    (5, 0, '0', '5', '0', '0/0', '', '0'), \
    (6, NULL, NULL, NULL, NULL, NULL, NULL, NULL)";

#[cfg(feature = "standalone")]
fn written(name: &str) -> std::path::PathBuf {
    let path = tmp(&format!("{name}.snail"));
    let catalog = Catalog::parse(DDL).unwrap();
    write(&path, &catalog, &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute(ROWS).unwrap();
    drop(w);
    path
}

#[cfg(feature = "standalone")]
fn ns(b: &arrow::array::RecordBatch) -> Vec<i64> {
    ints64(b.column(0).as_ref()).into_iter().flatten().collect()
}

#[test]
#[cfg(feature = "standalone")]
fn the_scalars_are_compared_ordered_grouped_and_summed_as_postgresql_does() {
    let path = written("scalars-ask");
    let mut snail = Snail::open(&path).unwrap();
    let mut ask = |q: &str, order: Option<&str>| {
        let (b, round) = snail.ask(q, order).unwrap();
        exact(q, &round);
        b
    };
    // An oid, a pg_lsn and an xid8 sort unsigned: their largest is past their top bit.
    for (q, order) in [
        (
            "SELECT k.n FROM s.k k WHERE k.o > 2147483647 ORDER BY k.n",
            "k_o",
        ),
        (
            "SELECT k.n FROM s.k k WHERE k.l > '7FFFFFFF/FFFFFFFF' ORDER BY k.n",
            "k_l",
        ),
    ] {
        assert_eq!(ns(&ask(q, Some(order))), [2, 3], "{q}");
    }
    assert_eq!(
        ns(&ask(
            "SELECT k.n FROM s.k k WHERE k.x8 >= '9223372036854775808' ORDER BY k.n",
            None
        )),
        [2, 3]
    );
    // An integer beside an oid is cast to one, its bits as they are: -1 is the largest.
    assert_eq!(ns(&ask("SELECT k.n FROM s.k k WHERE k.o = -1", None)), [2]);
    // A "char" sorts its byte unsigned, so the bytes past 127 come after 'z', and '' first.
    assert_eq!(
        ns(&ask(
            "SELECT k.n, k.ch FROM s.k k WHERE k.ch IS NOT NULL ORDER BY k.ch",
            Some("k_ch")
        )),
        [5, 1, 4, 3, 2]
    );
    assert_eq!(
        ns(&ask(
            "SELECT k.n FROM s.k k WHERE k.ch > 'z' ORDER BY k.n",
            None
        )),
        [2, 3]
    );
    // money compares as its cents, read from any spelling cash_in takes.
    assert_eq!(
        ns(&ask(
            "SELECT k.n FROM s.k k WHERE k.m < '$0.00' ORDER BY k.n",
            Some("k_m")
        )),
        [3, 4]
    );
    assert_eq!(
        ns(&ask("SELECT k.n FROM s.k k WHERE k.m = ' ( 0.01 ) '", None)),
        [4]
    );
    // An xid is compared by = and <> only, an integer beside it as xideqint4 reads it; a cid by
    // = alone.
    assert_eq!(ns(&ask("SELECT k.n FROM s.k k WHERE k.x = -1", None)), [2]);
    assert_eq!(
        ns(&ask(
            "SELECT k.n FROM s.k k WHERE k.x <> '1' ORDER BY k.n",
            None
        )),
        [2, 3, 4, 5]
    );
    for q in [
        "SELECT k.n FROM s.k k WHERE k.x < '5'",
        "SELECT k.n FROM s.k k WHERE -1 = k.x",
        "SELECT k.n, k.x FROM s.k k ORDER BY k.x",
        "SELECT k.n FROM s.k k WHERE k.c <> '5'",
        "SELECT k.n FROM s.k k WHERE k.c = 5",
        "SELECT k.n FROM s.k k WHERE k.m = 1",
        "SELECT min(k.x) FROM s.k k",
    ] {
        assert!(snail.ask(q, None).is_err(), "{q}");
    }
    // A sum of money is refused where its running sum leaves money's range, row by row, as
    // PostgreSQL's is: 1.00 and the largest already do, though the least would bring it back.
    assert!(snail.ask("SELECT sum(k.m) FROM s.k k", None).is_err());
    let mut ask = |q: &str| snail.ask(q, None).unwrap().0;
    // min and max on the question path, in the column's own type.
    let b = ask("SELECT min(k.o), max(k.o), min(k.l), max(k.m), min(k.ch), max(k.ch) FROM s.k k");
    let u32s = |i: usize| {
        b.column(i)
            .as_any()
            .downcast_ref::<UInt32Array>()
            .unwrap()
            .value(0)
    };
    assert_eq!((u32s(0), u32s(1)), (0, 4294967295));
    assert_eq!(
        b.column(2)
            .as_any()
            .downcast_ref::<UInt64Array>()
            .unwrap()
            .value(0),
        0
    );
    assert_eq!(
        ints64(b.column(3).as_ref()),
        [Some(9_223_372_036_854_775_807)]
    );
    // A "char"'s min and max go by its text, as PostgreSQL takes them through min(text): '' the
    // least, and 'z' past every byte it writes as a backslash and three digits.
    assert_eq!(
        (
            weather::texts(b.column(4).as_ref()),
            weather::texts(b.column(5).as_ref())
        ),
        (vec![Some(String::new())], vec![Some("z".to_string())])
    );
    let b = ask("SELECT sum(k.m) FROM s.k k WHERE k.n > 3");
    assert_eq!(ints64(b.column(0).as_ref()), [Some(-1)]);
    // Grouping on a "char" puts its NULL in a group of its own.
    let b = ask("SELECT k.ch, count(*) FROM s.k k GROUP BY k.ch");
    assert_eq!(b.num_rows(), 6);
    let chars = b.column(0).as_any().downcast_ref::<UInt8Array>().unwrap();
    assert_eq!(chars.iter().filter(Option::is_none).count(), 1);
}

#[test]
#[cfg(feature = "standalone")]
fn a_pipeline_reads_the_scalars_and_their_operators_as_postgresql_does() {
    let path = written("scalars-pipe");
    let mut snail = Snail::open(&path).unwrap();
    let mut texts = |q: &str| -> Vec<Option<String>> {
        let (b, _) = snail.ask(q, None).unwrap();
        weather::texts(b.column(0).as_ref())
    };
    let step = "WITH t AS (SELECT n, o, x, x8, l, ch, m FROM s.k)";
    assert_eq!(
        texts(&format!("{step} SELECT l::text AS v, l FROM t ORDER BY l")),
        [
            Some("0/00000000".into()),
            Some("0/00000001".into()),
            Some("7FFFFFFF/FFFFFFFF".into()),
            Some("80000000/00000000".into()),
            Some("FFFFFFFF/FFFFFFFF".into()),
            None
        ]
    );
    assert_eq!(
        texts(&format!(
            "{step} SELECT (m + '1.50')::text AS v FROM t WHERE n = 1"
        )),
        [Some("$2.50".into())]
    );
    assert_eq!(
        texts(&format!(
            "{step} SELECT (m * 3)::text AS v FROM t WHERE n = 4"
        )),
        [Some("-$0.03".into())]
    );
    assert_eq!(
        texts(&format!(
            "{step} SELECT (l - '0/0')::text AS v FROM t WHERE n = 2"
        )),
        [Some("18446744073709551615".into())]
    );
    // A pg_lsn plus a number of bytes rounded half away from zero, as PostgreSQL 18.6 gave it.
    assert_eq!(
        texts(&format!(
            "{step} SELECT (l + 1.5)::text AS v FROM t WHERE l = '0/1'"
        )),
        [Some("0/00000003".into())]
    );
    assert_eq!(
        texts(&format!("{step} SELECT ch::text AS v, n FROM t ORDER BY n")),
        ["a", "\\377", "\\200", "z", ""]
            .map(|s| Some(s.to_string()))
            .into_iter()
            .chain([None])
            .collect::<Vec<_>>()
    );
    assert_eq!(
        texts(&format!(
            "{step} SELECT CAST(o AS integer)::text AS v FROM t WHERE n = 2"
        )),
        [Some("-1".into())]
    );
    for q in [
        format!("{step} SELECT m + 1 AS v FROM t"),
        format!("{step} SELECT x AS v FROM t ORDER BY x"),
        format!("{step} SELECT n FROM t WHERE x < '5'"),
        format!("{step} SELECT max(x) AS v FROM t"),
        format!("{step} SELECT sum(o) AS v FROM t"),
        format!("{step} SELECT m + m AS v FROM t WHERE n = 2"),
        format!("{step} SELECT l + 1 AS v FROM t WHERE n = 2"),
    ] {
        assert!(snail.ask(&q, None).is_err(), "{q}");
    }
}

#[test]
#[cfg(feature = "standalone")]
fn a_value_is_assigned_to_a_scalar_as_postgresql_s_casts_take_it() {
    let path = tmp("scalars-assign.snail");
    let catalog = Catalog::parse(
        "CREATE TABLE s.a (n integer NOT NULL, o oid, m money, ch \"char\", x xid, PRIMARY KEY (n));",
    )
    .unwrap();
    write(&path, &catalog, &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute(
        "INSERT INTO s.a (n, o, m, ch) VALUES (1, -1, 5, 'é'), (2, 3000000000, 1.005, 'abc')",
    )
    .unwrap();
    for bad in [
        "INSERT INTO s.a (n, o) VALUES (3, -3000000000)",
        "INSERT INTO s.a (n, x) VALUES (3, 5)",
        "INSERT INTO s.a (n, m) VALUES (3, '1.5x')",
        "INSERT INTO s.a (n, o) VALUES (3, '08')",
    ] {
        assert!(w.execute(bad).is_err(), "{bad}");
    }
    let (b, _) = w
        .ask("WITH t AS (SELECT n, o, m, ch FROM s.a) SELECT o::text AS o, m::text AS m, ch::text AS ch, n FROM t ORDER BY n", None)
        .unwrap();
    let col = |i: usize| weather::texts(b.column(i).as_ref());
    assert_eq!(
        col(0),
        [Some("4294967295".into()), Some("3000000000".into())]
    );
    assert_eq!(col(1), [Some("$5.00".into()), Some("$1.01".into())]);
    assert_eq!(col(2), [Some("\\303".into()), Some("a".into())]);
    // An index on an xid is refused, as PostgreSQL has no B-tree on one.
    assert!(Catalog::parse("CREATE TABLE s.b (x xid NOT NULL, PRIMARY KEY (x));").is_err());
    assert!(Catalog::parse("CREATE TABLE s.b (x xid); CREATE INDEX b_x ON s.b (x);").is_err());
}

#[test]
#[cfg(feature = "standalone")]
fn the_scalars_go_out_as_unsigned_integers_and_come_back() {
    let path = written("scalars-out");
    let dir = tmp("scalars-out-parquet");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    let outs = w.out(&dir).unwrap();
    let k = outs.iter().find(|t| t.table == "s.k").unwrap();
    let (batches, _) = weather::parquet_of(&k.file);
    let o = batches[0].column_by_name("o").unwrap();
    let o = o.as_any().downcast_ref::<UInt32Array>().unwrap();
    assert_eq!(
        (0..o.len())
            .map(|i| o.is_valid(i).then(|| o.value(i)))
            .collect::<Vec<_>>(),
        [
            Some(1),
            Some(4294967295),
            Some(2147483648),
            Some(2147483647),
            Some(0),
            None
        ]
    );
    // Each row group's least and greatest are its own bits, as Parquet orders an unsigned integer.
    let meta = parquet::file::reader::FileReader::metadata(
        &parquet::file::reader::SerializedFileReader::new(std::fs::File::open(&k.file).unwrap())
            .unwrap(),
    )
    .clone();
    let at = meta
        .file_metadata()
        .schema_descr()
        .columns()
        .iter()
        .position(|c| c.name() == "l")
        .unwrap();
    let stats = meta.row_group(0).column(at).statistics().unwrap();
    assert_eq!(
        (
            stats.min_bytes_opt().unwrap(),
            stats.max_bytes_opt().unwrap()
        ),
        (&0u64.to_le_bytes()[..], &u64::MAX.to_le_bytes()[..])
    );
    let catalog = Catalog::parse(DDL).unwrap();
    let rows = Rows::from_batches(catalog.table("s.k").unwrap(), &batches).unwrap();
    let back = tmp("scalars-back.snail");
    write(&back, &catalog, &[("s.k".into(), rows)]).unwrap();
    let mut again = Snail::open(&back).unwrap();
    let (b, _) = again
        .ask(
            "SELECT k.n FROM s.k k WHERE k.l = 'FFFFFFFF/FFFFFFFF'",
            None,
        )
        .unwrap();
    assert_eq!(ns(&b), [2]);
    let key = scalar_parse(
        ColumnType::Money(warren_snail::monetary::Monetary::C),
        "-92233720368547758.08",
    )
    .unwrap();
    assert_eq!(key, i64::MIN);
}

#[test]
#[cfg(feature = "standalone")]
fn a_mac_address_is_held_in_its_bytes_compared_as_them_and_goes_out_as_them() {
    use arrow::array::FixedSizeBinaryArray;
    let path = tmp("mac.snail");
    let catalog = Catalog::parse(
        "CREATE TABLE s.m (n integer NOT NULL, a macaddr, b macaddr8, PRIMARY KEY (n)); \
         CREATE INDEX m_a ON s.m (a, n); \
         CREATE TABLE s.e (n integer NOT NULL, b macaddr8, PRIMARY KEY (n));",
    )
    .unwrap();
    write(&path, &catalog, &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute(
        "INSERT INTO s.m (n, a, b) VALUES (1, '08:00:2b:01:02:03', '08:00:2b:01:02:03'), \
         (2, 'ff:ff:ff:ff:ff:ff', '00:00:00:00:00:00:00:01'), (3, '80:00:00:00:00:00', NULL), \
         (4, '00:00:00:00:00:00', 'ff:ff:ff:ff:ff:ff:ff:ff'), (5, NULL, NULL)",
    )
    .unwrap();
    // Compared byte by byte: a first byte past 127 sorts after one below it.
    let (b, round) = w
        .ask(
            "SELECT m.n FROM s.m m WHERE m.a > '08:00:2b:01:02:03' ORDER BY m.n",
            Some("m_a"),
        )
        .unwrap();
    exact("a > 08:00:2b:01:02:03", &round);
    assert_eq!(ns(&b), [2, 3]);
    let (b, _) = w.ask("SELECT m.a FROM s.m m WHERE m.n = 1", None).unwrap();
    let a = b
        .column(0)
        .as_any()
        .downcast_ref::<FixedSizeBinaryArray>()
        .unwrap();
    assert_eq!(a.value(0), [0x08, 0x00, 0x2b, 0x01, 0x02, 0x03]);
    let (b, _) = w
        .ask(
            "WITH t AS (SELECT n, a, b FROM s.m) SELECT (a & 'ff:ff:ff:00:00:00')::text AS x, \
             (~b)::text AS y, n FROM t ORDER BY n",
            None,
        )
        .unwrap();
    assert_eq!(
        weather::texts(b.column(0).as_ref())[0].as_deref(),
        Some("08:00:2b:00:00:00")
    );
    assert_eq!(
        weather::texts(b.column(1).as_ref())[0].as_deref(),
        Some("f7:ff:d4:00:01:fe:fd:fc")
    );
    // A macaddr taken into a macaddr8 takes FF:FE in its middle.
    w.execute("INSERT INTO s.e (n, b) SELECT m.n, m.a FROM s.m m WHERE m.n = 3")
        .unwrap();
    let (b, _) = w
        .ask(
            "WITH t AS (SELECT n, b FROM s.e) SELECT b::text AS x, n FROM t WHERE n = 3",
            None,
        )
        .unwrap();
    assert_eq!(
        weather::texts(b.column(0).as_ref())[0].as_deref(),
        Some("80:00:00:ff:fe:00:00:00")
    );
    // An operator over a NULL is NULL.
    let (b, _) = w
        .ask(
            "WITH t AS (SELECT n, a FROM s.m) SELECT (a & 'ff:ff:ff:00:00:00')::text AS x, n FROM t \
             ORDER BY n",
            None,
        )
        .unwrap();
    assert_eq!(weather::texts(b.column(0).as_ref())[4], None);
    assert!(w.ask("SELECT min(m.a) FROM s.m m", None).is_err());
}
