//! The time types: date, time, timetz, timestamp and interval held beside timestamptz, each keyed by
//! its order and read and written as PostgreSQL's input and output functions read and write it,
//! their infinities places at the ends, an instant from the snail's last microsecond on refused by
//! name; their operators and casts in a pipeline, and the settings the snail answers under.

mod weather;

#[cfg(feature = "standalone")]
use arrow::array::{Array, RecordBatch};
use warren_snail::catalog::Catalog;
#[cfg(feature = "standalone")]
use warren_snail::catalog::ColumnType;
#[cfg(feature = "standalone")]
use warren_snail::read::Snail;
#[cfg(feature = "standalone")]
use warren_snail::scalars::{field_scalar, interval_at, scalar_key_at, scalar_text};
use warren_snail::write::Rows;
use weather::tmp;
#[cfg(feature = "standalone")]
use weather::write;

#[cfg(feature = "standalone")]
/// An answer's column, each value as PostgreSQL's output for its type writes it.
fn texts(b: &RecordBatch, c: usize) -> Vec<Option<String>> {
    let a = b.column(c);
    let field = b.schema().field(c).clone();
    let ty = field_scalar(&field).unwrap_or_else(|| match field.data_type() {
        arrow::datatypes::DataType::Timestamp(_, Some(_)) => ColumnType::Timestamptz,
        other => panic!("{other} is no time type"),
    });
    (0..a.len())
        .map(|r| match ty {
            ColumnType::Interval { .. } => {
                interval_at(a.as_ref(), r).map(warren_snail::times::interval_out)
            }
            ty => scalar_key_at(ty, a.as_ref(), r).map(|k| scalar_text(ty, k)),
        })
        .collect()
}

#[cfg(feature = "standalone")]
/// A snail of one table, `s.t`, made empty from its DDL, then the statements run on it.
fn snail_of(name: &str, ddl: &str, statements: &[&str]) -> Snail {
    let path = tmp(&format!("{name}.snail"));
    let catalog = Catalog::parse(ddl).unwrap();
    let empty = Rows {
        columns: catalog.tables[0]
            .columns
            .iter()
            .map(|c| warren_snail::values::Values::new(c.ty))
            .collect(),
    };
    write(&path, &catalog, &[("s.t".into(), empty)]).unwrap();
    let mut snail = Snail::open_writing(&path).unwrap();
    for s in statements {
        snail.execute(s).unwrap();
    }
    snail
}

#[cfg(feature = "standalone")]
fn table(ty: &str) -> String {
    format!(
        "CREATE TABLE s.t (id integer NOT NULL, v {ty}, PRIMARY KEY (id)); \
         CREATE INDEX t_v ON s.t (v, id);"
    )
}

#[cfg(feature = "standalone")]
fn some(v: &[&str]) -> Vec<Option<String>> {
    v.iter().map(|s| Some(s.to_string())).collect()
}

#[cfg(feature = "standalone")]
#[test]
fn each_time_type_sorts_and_writes_as_postgresql_does_its_ends_and_infinities_among_them() {
    // Each type's values in PostgreSQL's order (types/time-ranges-composed.md), written in other
    // spellings where PostgreSQL's input reads them.
    let cases: [(&str, &[&str], &[&str]); 5] = [
        (
            "date",
            &[
                "'infinity'",
                "'2024-3-5'",
                "'4714-11-24 BC'",
                "'-infinity'",
                "'5874897-12-31'",
                "'0044-03-15 BC'",
            ],
            &[
                "-infinity",
                "4714-11-24 BC",
                "0044-03-15 BC",
                "2024-03-05",
                "5874897-12-31",
                "infinity",
            ],
        ),
        (
            "time",
            &[
                "'24:00'",
                "'allballs'",
                "'1:04:05.5 pm'",
                "'23:59:59.999999'",
            ],
            &["00:00:00", "13:04:05.5", "23:59:59.999999", "24:00:00"],
        ),
        (
            "timetz",
            &[
                "'11:00+00'",
                "'12:00+01'",
                "'10:00-01'",
                "'23:00-05'",
                "'01:00+00'",
                "'00:00+14'",
                "'00:00-12'",
            ],
            &[
                "00:00:00+14",
                "01:00:00+00",
                "12:00:00+01",
                "11:00:00+00",
                "10:00:00-01",
                "00:00:00-12",
                "23:00:00-05",
            ],
        ),
        (
            "timestamp",
            &[
                "'infinity'",
                "'10000-01-01'",
                "'0044-03-15 12:00 BC'",
                "'2024-03-05T13:04:05.5'",
                "'-infinity'",
            ],
            &[
                "-infinity",
                "0044-03-15 12:00:00 BC",
                "2024-03-05 13:04:05.5",
                "10000-01-01 00:00:00",
                "infinity",
            ],
        ),
        (
            "interval",
            &[
                "'1 mon'",
                "'29 days 25 hours'",
                "'2 mons -31 days'",
                "'1 mon -1 microsecond'",
                "'-1 mon 61 days'",
                "'infinity'",
                "'-infinity'",
            ],
            &[
                "-infinity",
                "2 mons -31 days",
                "1 mon -00:00:00.000001",
                "1 mon",
                "29 days 25:00:00",
                "-1 mons +61 days",
                "infinity",
            ],
        ),
    ];
    for (ty, written, sorted) in cases {
        let values: Vec<String> = written
            .iter()
            .enumerate()
            .map(|(i, v)| format!("({i}, {v})"))
            .collect();
        let insert = format!("INSERT INTO s.t (id, v) VALUES {}", values.join(", "));
        let mut snail = snail_of(&format!("time-{ty}"), &table(ty), &[&insert]);
        for order in ["t_pkey", "t_v"] {
            let (b, _) = snail
                .ask("SELECT v FROM s.t ORDER BY v", Some(order))
                .unwrap();
            assert_eq!(texts(&b, 0), some(sorted), "{ty} along {order}");
        }
        // Each value compared with its own text: one row, along the index.
        for (i, want) in sorted.iter().enumerate() {
            let q = format!("SELECT v FROM s.t WHERE v = '{want}'");
            let (b, _) = snail.ask(&q, Some("t_v")).unwrap();
            assert_eq!(texts(&b, 0), some(&[want]), "{ty} {i}");
        }
    }
}

#[cfg(feature = "standalone")]
#[test]
fn an_instant_from_the_snail_s_last_microsecond_on_is_refused_by_name() {
    let mut snail = snail_of("time-top", &table("timestamptz"), &[]);
    for (i, (v, taken)) in [
        ("294247-01-10 04:00:54.775806+00", true),
        ("294247-01-10 04:00:54.775807+00", false),
        ("294276-12-31 23:59:59.999999+00", false),
        ("10000-01-01 00:00:00+00", true),
        ("0044-03-15 12:00:00+00 BC", true),
        ("4714-11-24 00:00:00+00 BC", true),
    ]
    .into_iter()
    .enumerate()
    {
        let r = snail.execute(&format!("INSERT INTO s.t (id, v) VALUES ({i}, '{v}')"));
        match (r, taken) {
            (Ok(_), true) => {}
            (Err(e), false) => assert!(
                e.to_string().contains("294247-01-10 04:00:54.775806"),
                "{v}: {e}"
            ),
            (r, _) => panic!("{v}: {r:?}"),
        }
    }
    let (b, _) = snail.ask("SELECT v FROM s.t ORDER BY v", None).unwrap();
    assert_eq!(
        texts(&b, 0),
        some(&[
            "4714-11-24 00:00:00+00 BC",
            "0044-03-15 12:00:00+00 BC",
            "10000-01-01 00:00:00+00",
            "294247-01-10 04:00:54.775806+00"
        ])
    );
}

#[cfg(feature = "standalone")]
#[test]
fn equal_intervals_share_a_place_group_as_one_and_sum_as_postgresql_sums_them() {
    let mut snail = snail_of(
        "time-equal",
        &table("interval"),
        &["INSERT INTO s.t (id, v) VALUES (1, '1 day'), (2, '24 hours'), (3, '1 mon'), (4, '30 days'), (5, '720 hours')"],
    );
    let (b, _) = snail
        .ask("SELECT v, count(*) FROM s.t GROUP BY v ORDER BY v", None)
        .unwrap();
    let counts = b
        .column(1)
        .as_any()
        .downcast_ref::<arrow::array::Int64Array>()
        .unwrap();
    assert_eq!((b.num_rows(), counts.value(0), counts.value(1)), (2, 2, 3));
    let (b, _) = snail
        .ask("SELECT count(*) FROM s.t WHERE v = '1 mon'", Some("t_v"))
        .unwrap();
    let n = b
        .column(0)
        .as_any()
        .downcast_ref::<arrow::array::Int64Array>()
        .unwrap();
    assert_eq!(n.value(0), 3);
    // Of two equal intervals, min and max keep the later, as PostgreSQL's interval_smaller and
    // interval_larger do; their sum adds field by field.
    for q in [
        "SELECT min(v) AS x FROM s.t WHERE id <= 2",
        "SELECT max(v) AS x FROM s.t WHERE id <= 2",
    ] {
        let (b, _) = snail.ask(q, Some("t_pkey")).unwrap();
        assert_eq!(texts(&b, 0), some(&["24:00:00"]), "{q}");
    }
    let (b, _) = snail.ask("SELECT sum(v) AS x FROM s.t", None).unwrap();
    assert_eq!(texts(&b, 0), some(&["1 mon 31 days 744:00:00"]));
}

#[cfg(feature = "standalone")]
#[test]
fn the_pipeline_adds_takes_and_casts_time_as_postgresql_does() {
    let mut snail = snail_of(
        "time-arith",
        &table("date"),
        &["INSERT INTO s.t (id, v) VALUES (1, '2024-01-31'), (2, '2024-03-05'), (3, 'infinity')"],
    );
    let step = "WITH t AS (SELECT id, v FROM s.t)";
    for (expr, want) in [
        ("v + 1", ["2024-02-01", "2024-03-06", "infinity"]),
        (
            "v + '1 mon'::interval",
            ["2024-02-29 00:00:00", "2024-04-05 00:00:00", "infinity"],
        ),
        (
            "CAST(v AS timestamptz)",
            [
                "2024-01-31 00:00:00+00",
                "2024-03-05 00:00:00+00",
                "infinity",
            ],
        ),
        (
            "v + '12:30'::time",
            ["2024-01-31 12:30:00", "2024-03-05 12:30:00", "infinity"],
        ),
    ] {
        let q = format!("{step} SELECT id, {expr} AS x FROM t ORDER BY id");
        let (b, _) = snail.ask(&q, None).unwrap();
        assert_eq!(texts(&b, 1), some(&want), "{expr}");
    }
    let q = format!("{step} SELECT id, v - '2000-01-01'::date AS x FROM t WHERE id = 2");
    let (b, _) = snail.ask(&q, None).unwrap();
    let days = b
        .column(1)
        .as_any()
        .downcast_ref::<arrow::array::Int64Array>()
        .unwrap();
    assert_eq!(days.value(0), 8830);
    // A date beside a timestamp is compared at its midnight.
    let q = format!("{step} SELECT id FROM t WHERE v = '2024-03-05 00:00'::timestamp");
    assert_eq!(snail.ask(&q, None).unwrap().0.num_rows(), 1);
    // An instant at a fixed offset: '+02' is a zone two hours west, as POSIX writes it.
    let q = format!(
        "{step} SELECT id, CAST(v AS timestamptz) AT TIME ZONE '+02' AS x FROM t WHERE id = 1"
    );
    let (b, _) = snail.ask(&q, None).unwrap();
    assert_eq!(texts(&b, 1), some(&["2024-01-30 22:00:00"]));
    #[cfg(not(feature = "zones"))]
    {
        let q = format!(
            "{step} SELECT id, CAST(v AS timestamptz) AT TIME ZONE 'Europe/Lisbon' AS x FROM t"
        );
        let e = snail.ask(&q, None).unwrap_err().to_string();
        assert!(e.contains("read only under the feature zones"), "{e}");
    }
}

#[cfg(feature = "standalone")]
#[test]
fn a_value_is_rounded_to_its_column_s_precision_at_the_write() {
    let mut snail = snail_of(
        "time-rounded",
        "CREATE TABLE s.t (id integer NOT NULL, v timestamp(1), w timestamp, PRIMARY KEY (id));",
        &[
            "INSERT INTO s.t (id, v, w) VALUES (1, '1999-12-31 23:59:59.95', '2024-01-01 00:00:00.05')",
            "INSERT INTO s.t (id, v) SELECT 2, w FROM s.t WHERE id = 1",
        ],
    );
    let (b, _) = snail
        .ask("SELECT id, v FROM s.t ORDER BY id", None)
        .unwrap();
    assert_eq!(
        texts(&b, 1),
        some(&["1999-12-31 23:59:59.9", "2024-01-01 00:00:00.1"])
    );
    // A literal beside the column is read as a timestamp of no precision, never rounded to its.
    for q in [
        "SELECT id FROM s.t WHERE v = '1999-12-31 23:59:59.95'",
        "WITH t AS (SELECT id, v FROM s.t) SELECT id FROM t WHERE v = '1999-12-31 23:59:59.95'",
    ] {
        assert_eq!(snail.ask(q, None).unwrap().0.num_rows(), 0, "{q}");
    }
}

#[cfg(feature = "standalone")]
#[test]
fn a_setting_set_to_the_value_the_snail_answers_under_does_nothing_and_any_other_is_refused() {
    let mut snail = snail_of("time-set", &table("date"), &[]);
    for s in [
        "SET TimeZone = 'UTC'",
        "SET TIME ZONE 'UTC'",
        "SET datestyle = 'ISO, MDY'",
        "SET DateStyle = ISO",
        "SET IntervalStyle = postgres",
        "SET extra_float_digits = 1",
        "SET standard_conforming_strings = on",
    ] {
        snail.execute(s).unwrap();
    }
    for (s, says) in [
        ("SET TimeZone = 'Europe/Lisbon'", "timezone UTC"),
        ("SET datestyle = 'ISO, DMY'", "datestyle ISO, MDY"),
        ("SET DateStyle = 'SQL'", "datestyle ISO, MDY"),
        ("SET IntervalStyle = iso_8601", "intervalstyle postgres"),
        ("SET extra_float_digits = 3", "extra_float_digits 1"),
        ("SET application_name = 'x'", "is not read"),
    ] {
        let e = snail.execute(s).unwrap_err().to_string();
        assert!(e.contains(says), "{s}: {e}");
    }
}

#[test]
fn a_snail_of_format_8_refuses_an_infinite_instant_on_a_level_by_name() {
    let catalog =
        Catalog::parse("CREATE TABLE s.t (at timestamptz NOT NULL, PRIMARY KEY (at));").unwrap();
    let path = tmp("time-format8.snail");
    let _ = std::fs::remove_file(&path);
    let rows = Rows {
        columns: vec![warren_snail::values::Values::Int(vec![
            Some(0),
            Some(i64::MAX),
        ])],
    };
    let e = warren_snail::write::write_format_eight(
        &path,
        &catalog,
        &[("s.t".into(), rows)],
        warren_snail::file::FILE_PAGES,
    )
    .unwrap_err()
    .to_string();
    assert!(
        e.contains("holds an infinite instant on a level, which a snail of format 8 cannot hold"),
        "{e}"
    );
}

#[cfg(feature = "standalone")]
#[test]
fn out_names_the_columns_holding_an_infinity_or_24_00() {
    let mut snail = snail_of(
        "time-out",
        "CREATE TABLE s.t (id integer NOT NULL, d date, t time, s timestamp, PRIMARY KEY (id));",
        &["INSERT INTO s.t (id, d, t, s) VALUES (1, '-infinity', '24:00', '2024-01-01'), (2, '2024-01-01', '12:00', '2024-01-02')"],
    );
    let dir = tmp("time-out-dir");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let out = snail.out(&dir).unwrap();
    assert_eq!(out[0].extremes, ["d", "t"]);
}

#[cfg(all(feature = "zones", feature = "standalone"))]
#[test]
fn under_zones_an_instant_at_a_named_zone_reads_its_rules() {
    let mut snail = snail_of(
        "time-zones",
        &table("timestamptz"),
        &["INSERT INTO s.t (id, v) VALUES (1, '2024-01-05 13:04:05+00'), (2, '2024-07-05 13:04:05+00')"],
    );
    let step = "WITH t AS (SELECT id, v FROM s.t)";
    let q = format!("{step} SELECT id, v AT TIME ZONE 'Europe/Lisbon' AS x FROM t ORDER BY id");
    let (b, _) = snail.ask(&q, None).unwrap();
    assert_eq!(
        texts(&b, 1),
        some(&["2024-01-05 13:04:05", "2024-07-05 14:04:05"])
    );
    // And back: a time on Lisbon's clock as the instant it is.
    let q = format!(
        "{step} SELECT id, (v AT TIME ZONE 'Europe/Lisbon') AT TIME ZONE 'Europe/Lisbon' AS x FROM t ORDER BY id"
    );
    let (b, _) = snail.ask(&q, None).unwrap();
    assert_eq!(
        texts(&b, 1),
        some(&["2024-01-05 13:04:05+00", "2024-07-05 13:04:05+00"])
    );
}

#[cfg(feature = "standalone")]
#[test]
fn at_time_zone_reads_an_abbreviation_first_as_postgresql_does() {
    let mut snail = snail_of(
        "time-abbreviations",
        &table("timestamptz"),
        &["INSERT INTO s.t (id, v) VALUES (1, '2024-07-05 13:04:05+00')"],
    );
    let step = "WITH t AS (SELECT id, v FROM s.t)";
    // UTC's abbreviations, whatever their case, are UTC's clock: no zone file is read for them.
    for zone in ["Z", "ut", "UTC", "zulu", "GMT", "uct"] {
        let q = format!("{step} SELECT id, v AT TIME ZONE '{zone}' AS x FROM t");
        let (b, _) = snail.ask(&q, None).unwrap();
        assert_eq!(texts(&b, 1), some(&["2024-07-05 13:04:05"]), "{zone}");
        let q =
            format!("{step} SELECT id, timezone('{zone}', v AT TIME ZONE '{zone}') AS x FROM t");
        let (b, _) = snail.ask(&q, None).unwrap();
        assert_eq!(texts(&b, 1), some(&["2024-07-05 13:04:05+00"]), "{zone}");
    }
    // Any other abbreviation is refused by name, never read as the zone file of its name.
    for zone in ["EST", "cet"] {
        let q = format!("{step} SELECT id, v AT TIME ZONE '{zone}' AS x FROM t");
        let e = snail.ask(&q, None).unwrap_err().to_string();
        assert!(e.contains("abbreviation"), "{zone}: {e}");
    }
}
