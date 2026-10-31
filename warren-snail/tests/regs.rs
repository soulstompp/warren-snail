//! The reg* types and oid8. A reg* value is an oid, compared, ordered and grouped as one, printed
//! and read as the name of a built-in object of the release the file's OIDs came from; any other
//! OID prints bare. oid8 is an unsigned 64-bit number.

mod weather;

#[cfg(feature = "standalone")]
use arrow::array::{Array, UInt32Array, UInt64Array};
use warren_snail::catalog::Catalog;
#[cfg(feature = "standalone")]
use warren_snail::read::Snail;
use weather::tmp;
#[cfg(feature = "standalone")]
use weather::{exact, ints64, texts};

#[cfg(feature = "standalone")]
const DDL: &str = "CREATE TABLE s.r (n integer NOT NULL, c regclass, t regtype, p regproc, \
                   pr regprocedure, o regoper, ro regrole, k regconfig, m money, PRIMARY KEY (n)); \
                   CREATE INDEX r_c ON s.r (c, n);";

#[cfg(feature = "standalone")]
const ROWS: &str = "INSERT INTO s.r (n, c, t, p, pr, o, ro, k, m) VALUES \
    (1, 'pg_class', 'int4', 'now', 'abs(integer)', '||/', '3373', 'simple', '1.00'), \
    (2, '4294967295', 'character varying(10)', '-', '3813', '0', '-', '13264', NULL), \
    (3, '-', '-', '99999999', '-', '99999999', '99999999', '-', NULL), \
    (4, 'PG_CATALOG.\"pg_proc\"', '_int4', 'pg_catalog.now', 'pg_catalog.now()', 'pg_catalog.||/', NULL, NULL, NULL), \
    (5, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL)";

/// A snail of s.r, its OIDs from `release`.
#[cfg(feature = "standalone")]
fn written(name: &str, release: u8) -> std::path::PathBuf {
    let path = tmp(&format!("{name}.snail"));
    let catalog = Catalog::parse(DDL).unwrap();
    warren_snail::write::write_released(&path, &catalog, &[], &[], weather::span(), release)
        .unwrap();
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
fn a_reg_type_is_compared_ordered_and_grouped_as_an_oid() {
    let path = written("regs-ask", 19);
    let mut snail = Snail::open(&path).unwrap();
    let mut ask = |q: &str, order: Option<&str>| {
        let (b, round) = snail.ask(q, order).unwrap();
        exact(q, &round);
        b
    };
    // A quoted literal beside a reg* type is read as an oid, and an integer is cast to one.
    assert_eq!(
        ns(&ask("SELECT r.n FROM s.r r WHERE r.c = '1259'", None)),
        [1]
    );
    assert_eq!(ns(&ask("SELECT r.n FROM s.r r WHERE r.c = -1", None)), [2]);
    // Ordered as its number, unsigned, along the key and along an index on it.
    for order in ["r_pkey", "r_c"] {
        assert_eq!(
            ns(&ask(
                "SELECT r.n, r.c FROM s.r r WHERE r.c IS NOT NULL ORDER BY r.c, r.n",
                Some(order)
            )),
            [3, 4, 1, 2],
            "{order}"
        );
    }
    assert_eq!(
        ns(&ask(
            "SELECT r.n FROM s.r r WHERE r.c > 2147483647 ORDER BY r.n",
            Some("r_c")
        )),
        [2]
    );
    // min and max are oid's, an oid.
    let b = ask("SELECT min(r.c), max(r.c) FROM s.r r", None);
    let u32s = |i: usize| {
        b.column(i)
            .as_any()
            .downcast_ref::<UInt32Array>()
            .unwrap()
            .value(0)
    };
    assert_eq!((u32s(0), u32s(1)), (0, 4294967295));
    assert_eq!(
        b.schema()
            .field(0)
            .metadata()
            .get("pg_type")
            .map(String::as_str),
        Some("oid")
    );
    let b = ask("SELECT r.c, count(*) FROM s.r r GROUP BY r.c", None);
    assert_eq!(b.num_rows(), 5);
    assert_eq!(
        b.schema()
            .field(0)
            .metadata()
            .get("pg_type")
            .map(String::as_str),
        Some("regclass")
    );
    for q in [
        "SELECT r.n FROM s.r r WHERE r.c = 'pg_class'",
        "SELECT r.n FROM s.r r WHERE r.c = 1.5",
        "SELECT sum(r.c) FROM s.r r",
    ] {
        assert!(snail.ask(q, None).is_err(), "{q}");
    }
}

#[test]
#[cfg(feature = "standalone")]
fn a_literal_cast_to_a_reg_type_is_read_by_that_type_s_input_and_compared_as_an_oid() {
    let path = written("regs-cast", 19);
    let mut snail = Snail::open(&path).unwrap();
    for (q, want) in [
        (
            "SELECT r.n FROM s.r r WHERE r.c = 'pg_class'::regclass",
            vec![1],
        ),
        (
            "SELECT r.n FROM s.r r WHERE r.c < 'pg_class'::regclass ORDER BY r.n",
            vec![3, 4],
        ),
        // Another reg* type's literal, and an oid's, compare as oids.
        ("SELECT r.n FROM s.r r WHERE r.c = 'int4'::regtype", vec![]),
        ("SELECT r.n FROM s.r r WHERE r.t = 'int4'::regtype", vec![1]),
        ("SELECT r.n FROM s.r r WHERE r.c = '1255'::oid", vec![4]),
    ] {
        let (b, round) = snail.ask(q, None).unwrap();
        exact(q, &round);
        assert_eq!(ns(&b), want, "{q}");
    }
    // A name no object of the release holds is refused, as regclass's input refuses it.
    assert!(snail
        .ask("SELECT r.n FROM s.r r WHERE r.c = 'nosuch'::regclass", None)
        .is_err());
}

#[test]
#[cfg(feature = "standalone")]
fn a_reg_value_standing_alone_prints_a_built_in_object_s_name_and_any_other_oid_bare() {
    let path = written("regs-text", 19);
    let mut snail = Snail::open(&path).unwrap();
    let texts = |snail: &mut Snail, q: &str| -> Vec<Vec<Option<String>>> {
        let (b, _) = snail.ask(q, None).unwrap();
        (0..b.num_columns())
            .map(|i| texts(b.column(i).as_ref()))
            .collect()
    };
    let c = texts(
        &mut snail,
        "WITH t AS (SELECT n, c, t, p, pr, o, ro, k FROM s.r) SELECT c::text AS c, t::text AS t, \
         p::text AS p, pr::text AS pr, o::text AS o, ro::text AS ro, k::text AS k FROM t",
    );
    let s = |v: &[&str]| -> Vec<Option<String>> {
        v.iter()
            .map(|x| (!x.is_empty()).then(|| x.to_string()))
            .collect()
    };
    assert_eq!(c[0], s(&["pg_class", "4294967295", "-", "pg_proc", ""]));
    assert_eq!(
        c[1],
        s(&["integer", "character varying", "-", "integer[]", ""])
    );
    assert_eq!(c[2], s(&["now", "-", "99999999", "now", ""]));
    assert_eq!(
        c[3],
        s(&["abs(integer)", "xmltext(text)", "-", "now()", ""])
    );
    assert_eq!(c[4], s(&["||/", "0", "99999999", "||/", ""]));
    // A role, and an object initdb makes from 12000 on, print bare.
    assert_eq!(c[5], s(&["3373", "-", "99999999", "", ""]));
    assert_eq!(c[6], s(&["simple", "13264", "-", "", ""]));
    // Casts as PostgreSQL's: to and from an oid and the integers, a function's two to each other;
    // text to regclass by name alone, a literal by regclass's input.
    let c = texts(
        &mut snail,
        "WITH t AS (SELECT n, c, p FROM s.r) SELECT CAST(c AS oid)::text AS a, CAST(c AS integer)::text AS b, \
         CAST(p AS regprocedure)::text AS d, CAST(CAST(1259 AS regclass) AS text) AS e, \
         CAST('-' AS regclass)::text AS f FROM t WHERE n = 2",
    );
    assert_eq!(
        c,
        [
            s(&["4294967295"]),
            s(&["-1"]),
            s(&["-"]),
            s(&["pg_class"]),
            s(&["-"])
        ]
    );
    for q in [
        "WITH t AS (SELECT n, c FROM s.r) SELECT CAST(c AS regtype) AS x FROM t",
        "WITH t AS (SELECT n, c FROM s.r) SELECT c + 1 AS x FROM t",
        "WITH t AS (SELECT n, c FROM s.r) SELECT CAST(CAST(c AS text) AS regclass) AS x FROM t WHERE n = 3",
        "WITH t AS (SELECT n, c FROM s.r) SELECT CAST('pg_monitor' AS regrole) AS x FROM t",
        "WITH t AS (SELECT n, c FROM s.r) SELECT CAST('english' AS regconfig) AS x FROM t",
        "WITH t AS (SELECT n, c FROM s.r) SELECT CAST('abs' AS regproc) AS x FROM t",
    ] {
        assert!(snail.ask(q, None).is_err(), "{q}");
    }
    // A reg* value read back from its own text, a name through text by regclass(text).
    let c = texts(
        &mut snail,
        "WITH t AS (SELECT n, c FROM s.r) SELECT CAST(CAST(CAST(c AS text) AS regclass) AS oid)::text AS x FROM t WHERE n = 1 OR n = 4",
    );
    assert_eq!(c, [s(&["1259", "1255"])]);
    // In a pipeline a reg* value is compared as an oid, with an oid and with a literal read as one,
    // and its min an oid.
    let mut ns_of = |q: &str| ns(&snail.ask(q, None).unwrap().0);
    assert_eq!(
        ns_of("WITH t AS (SELECT n, c FROM s.r) SELECT n FROM t WHERE c = CAST(1259 AS oid)"),
        [1]
    );
    assert_eq!(
        ns_of("WITH t AS (SELECT n, c FROM s.r) SELECT n FROM t WHERE c = '1259'"),
        [1]
    );
    assert!(snail
        .ask(
            "WITH t AS (SELECT n, c FROM s.r) SELECT n FROM t WHERE c = 'pg_class'",
            None
        )
        .is_err());
    let (b, _) = snail
        .ask(
            "WITH t AS (SELECT n, c FROM s.r) SELECT min(c) AS x FROM t",
            None,
        )
        .unwrap();
    assert_eq!(
        b.schema()
            .field(0)
            .metadata()
            .get("pg_type")
            .map(String::as_str),
        Some("oid")
    );
    // A text assigned to a regclass is read by regclass(text): a name alone.
    let path = tmp("regs-assign.snail");
    let catalog = Catalog::parse(
        "CREATE TABLE s.x (n integer NOT NULL, t text, PRIMARY KEY (n)); \
         CREATE TABLE s.y (n integer NOT NULL, c regclass, PRIMARY KEY (n));",
    )
    .unwrap();
    weather::write(&path, &catalog, &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute("INSERT INTO s.x (n, t) VALUES (1, 'pg_class'), (2, '1259')")
        .unwrap();
    w.execute("INSERT INTO s.y (n, c) SELECT n, t FROM s.x WHERE n = 1")
        .unwrap();
    assert!(w
        .execute("INSERT INTO s.y (n, c) SELECT n, t FROM s.x WHERE n = 2")
        .is_err());
    let (b, _) = w.ask("SELECT y.c FROM s.y y", None).unwrap();
    assert_eq!(
        b.column(0)
            .as_any()
            .downcast_ref::<UInt32Array>()
            .unwrap()
            .value(0),
        1259
    );
}

#[test]
#[cfg(feature = "standalone")]
fn the_file_records_the_release_its_oids_came_from_and_names_them_by_its_catalog() {
    let ddl = "CREATE TABLE s.p (n integer NOT NULL, pr regprocedure, PRIMARY KEY (n));";
    for (release, text) in [
        (14, "pg_is_in_backup()"),
        (16, "3813"),
        (19, "xmltext(text)"),
    ] {
        let path = tmp(&format!("regs-release-{release}.snail"));
        let catalog = Catalog::parse(ddl).unwrap();
        warren_snail::write::write_released(&path, &catalog, &[], &[], weather::span(), release)
            .unwrap();
        let mut w = Snail::open_writing(&path).unwrap();
        w.execute("INSERT INTO s.p (n, pr) VALUES (1, 3813)")
            .unwrap();
        drop(w);
        let mut snail = Snail::open(&path).unwrap();
        assert_eq!(snail.release(), release);
        let (b, _) = snail
            .ask(
                "WITH t AS (SELECT n, pr FROM s.p) SELECT pr::text AS x FROM t",
                None,
            )
            .unwrap();
        assert_eq!(texts(b.column(0).as_ref()), [Some(text.to_string())]);
        assert_eq!(
            b.schema().field(0).metadata().get("pg_release"),
            None,
            "text carries no release"
        );
        let (b, _) = snail.ask("SELECT p.pr FROM s.p p", None).unwrap();
        assert_eq!(
            b.schema().field(0).metadata().get("pg_release"),
            Some(&release.to_string())
        );
    }
    // regdatabase is 19's; a release the snail carries no catalog for is refused.
    let catalog =
        Catalog::parse("CREATE TABLE s.d (n integer NOT NULL, d regdatabase, PRIMARY KEY (n));")
            .unwrap();
    let path = tmp("regs-database-18.snail");
    let e = warren_snail::write::write_released(&path, &catalog, &[], &[], weather::span(), 18)
        .unwrap_err();
    assert!(
        e.to_string().contains("regdatabase is PostgreSQL 19's"),
        "{e}"
    );
    let path = tmp("regs-release-13.snail");
    assert!(warren_snail::write::write_released(
        &path,
        &Catalog::parse("CREATE TABLE s.e (n integer NOT NULL, PRIMARY KEY (n));").unwrap(),
        &[],
        &[],
        weather::span(),
        13
    )
    .is_err());
}

#[test]
#[cfg(feature = "standalone")]
fn the_file_records_each_money_column_s_lc_monetary() {
    let path = written("regs-money", 19);
    let mut w = Snail::open_writing(&path).unwrap();
    assert_eq!(
        w.monetary(),
        [("s.r".to_string(), "m".to_string(), "C".to_string())]
    );
    w.execute("CREATE TABLE s.q (n integer NOT NULL, a money, b money, PRIMARY KEY (n))")
        .unwrap();
    drop(w);
    let snail = Snail::open(&path).unwrap();
    assert_eq!(
        snail
            .monetary()
            .iter()
            .map(|(t, c, l)| format!("{t}.{c} {l}"))
            .collect::<Vec<_>>(),
        ["s.r.m C", "s.q.a C", "s.q.b C"]
    );
}

#[test]
fn a_snail_of_format_7_or_8_refuses_a_reg_type_and_oid8_by_name() {
    for (ty, name) in [("regclass", "regclass"), ("oid8", "oid8")] {
        let catalog = Catalog::parse(&format!(
            "CREATE TABLE s.f (n integer NOT NULL, v {ty}, PRIMARY KEY (n));"
        ))
        .unwrap();
        let path = tmp(&format!("regs-format-8-{ty}.snail"));
        let e = warren_snail::write::write_format_eight(&path, &catalog, &[], weather::span())
            .unwrap_err()
            .to_string();
        assert!(e.contains(name) && e.contains("cannot hold"), "{e}");
    }
}

#[cfg(feature = "standalone")]
const OID8: &str = "CREATE TABLE s.e (n integer NOT NULL, e oid8, o oid, PRIMARY KEY (n)); \
                    CREATE INDEX e_e ON s.e (e DESC, n); \
                    CREATE TABLE s.f (n integer NOT NULL, e oid8, o oid, PRIMARY KEY (n));";

#[test]
#[cfg(feature = "standalone")]
fn an_oid8_is_held_unsigned_in_64_bits_and_read_as_postgresql_19_reads_it() {
    let path = tmp("oid8.snail");
    write_oid8(&path);
    let mut snail = Snail::open(&path).unwrap();
    let mut ask = |q: &str, order: Option<&str>| {
        let (b, round) = snail.ask(q, order).unwrap();
        exact(q, &round);
        b
    };
    // An integer beside an oid8 comes through int8, its bits as they are: -1 is the largest.
    assert_eq!(
        ns(&ask(
            "SELECT e.n FROM s.e e WHERE e.e = -1 ORDER BY e.n",
            None
        )),
        [1, 6]
    );
    for order in ["e_pkey", "e_e"] {
        assert_eq!(
            ns(&ask(
                "SELECT e.n, e.e FROM s.e e WHERE e.e >= '9223372036854775808' ORDER BY e.e, e.n",
                Some(order)
            )),
            [3, 1, 6]
        );
    }
    let b = ask("SELECT min(e.e), max(e.e) FROM s.e e", None);
    let u64s = |i: usize| {
        b.column(i)
            .as_any()
            .downcast_ref::<UInt64Array>()
            .unwrap()
            .value(0)
    };
    assert_eq!((u64s(0), u64s(1)), (0, u64::MAX));
    let (b, _) = snail
        .ask(
            "WITH t AS (SELECT n, e, o FROM s.e) SELECT e::text AS a, CAST(e AS bigint)::text AS b, \
             CAST(-1 AS oid8)::text AS c, CAST(o AS oid8)::text AS d FROM t WHERE n = 1",
            None,
        )
        .unwrap();
    assert_eq!(
        (0..4)
            .map(|i| texts(b.column(i).as_ref()).remove(0).unwrap())
            .collect::<Vec<_>>(),
        [
            "18446744073709551615",
            "-1",
            "18446744073709551615",
            "4294967295"
        ]
    );
    for q in [
        "WITH t AS (SELECT n, e FROM s.e) SELECT CAST(e AS integer) AS x FROM t",
        "WITH t AS (SELECT n, e FROM s.e) SELECT sum(e) AS x FROM t",
        "SELECT e.n FROM s.e e WHERE e.e = 1.5",
        "SELECT e.n FROM s.e e WHERE e.e = '18446744073709551616'",
    ] {
        assert!(snail.ask(q, None).is_err(), "{q}");
    }
}

#[cfg(feature = "standalone")]
fn write_oid8(path: &std::path::Path) {
    let catalog = Catalog::parse(OID8).unwrap();
    weather::write(path, &catalog, &[]).unwrap();
    let mut w = Snail::open_writing(path).unwrap();
    w.execute(
        "INSERT INTO s.e (n, e, o) VALUES (1, '18446744073709551615', 4294967295), \
         (2, '0x10', 0), (3, '9223372036854775808', NULL), (4, 0, NULL), (5, NULL, NULL), \
         (6, -1, NULL)",
    )
    .unwrap();
    // oid to oid8 by assignment, and an integer through int8.
    w.execute("INSERT INTO s.f (n, e) SELECT n, o FROM s.e WHERE n = 1")
        .unwrap();
    let (b, _) = w
        .ask(
            "WITH t AS (SELECT n, e FROM s.f) SELECT e::text AS x FROM t",
            None,
        )
        .unwrap();
    assert_eq!(
        texts(b.column(0).as_ref()),
        [Some("4294967295".to_string())]
    );
    assert!(w
        .execute("INSERT INTO s.f (n, o) SELECT n, e FROM s.e WHERE n = 2")
        .is_err());
}

#[test]
#[cfg(feature = "standalone")]
fn a_reg_type_is_cut_by_its_bits_as_an_oid_is_and_goes_out_naming_its_release() {
    use warren_snail::scalars::unsigned_key;
    use warren_snail::values::Values;
    use warren_snail::write::Rows;
    // 4,096 regclass values spread evenly over their 32 bits cut as 4,096 oids spread so.
    let catalog = Catalog::parse(
        "CREATE TABLE s.c (id regclass NOT NULL, PRIMARY KEY (id)); \
         CREATE TABLE s.o (id oid NOT NULL, PRIMARY KEY (id));",
    )
    .unwrap();
    let spread = || {
        Values::Int(
            (0..4096)
                .map(|i| Some(unsigned_key(i * (1u64 << 20), 32)))
                .collect(),
        )
    };
    let path = tmp("regs-cut.snail");
    let rows = |v| Rows { columns: vec![v] };
    warren_snail::write::write_released(
        &path,
        &catalog,
        &[
            ("s.c".into(), rows(spread())),
            ("s.o".into(), rows(spread())),
        ],
        &[],
        weather::span(),
        16,
    )
    .unwrap();
    let mut snail = Snail::open_writing(&path).unwrap();
    let blocks = snail.blocks().unwrap();
    let of = |order: &str| {
        blocks
            .iter()
            .filter(|r| r.order == order)
            .map(|r| r.rows)
            .collect::<Vec<_>>()
    };
    assert_eq!(of("c_pkey"), [1024; 4]);
    assert_eq!(of("c_pkey"), of("o_pkey"));
    let dir = tmp("regs-out");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let outs = snail.out(&dir).unwrap();
    let release = |table: &str| {
        let t = outs.iter().find(|t| t.table == table).unwrap();
        let reader =
            parquet::file::reader::SerializedFileReader::new(std::fs::File::open(&t.file).unwrap())
                .unwrap();
        parquet::file::reader::FileReader::metadata(&reader)
            .file_metadata()
            .key_value_metadata()
            .unwrap()
            .iter()
            .find(|k| k.key == warren_snail::read::RELEASE_KEY)
            .and_then(|k| k.value.clone())
    };
    assert_eq!(release("s.c"), Some("16".to_string()));
    assert_eq!(release("s.o"), Some("16".to_string()));
}
