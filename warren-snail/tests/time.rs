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
        "CREATE SCHEMA s; CREATE TABLE s.t (id integer NOT NULL, v {ty}, PRIMARY KEY (id)); \
         CREATE INDEX t_v ON s.t (v, id);"
    )
}

#[cfg(feature = "standalone")]
fn some(v: &[&str]) -> Vec<Option<String>> {
    v.iter().map(|s| Some(s.to_string())).collect()
}

/// The tests that read a zone through the directory zones are read from, one at a time: it is the
/// process's, and one of them sets another. A name POSIX reads (`+02`, an interval's offset) is
/// looked for there first.
#[cfg(feature = "standalone")]
static ZONE_FILES: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(feature = "standalone")]
fn zone_files() -> std::sync::MutexGuard<'static, ()> {
    ZONE_FILES.lock().unwrap_or_else(|e| e.into_inner())
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
    let _zones = zone_files();
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
    // A named zone, refused by name by a build without the feature zones, on by default.
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
        "CREATE SCHEMA s; CREATE TABLE s.t (id integer NOT NULL, v timestamp(1), w timestamp, PRIMARY KEY (id));",
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
    let _zones = zone_files();
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
        // TimeZone is the session's: a named zone is read only under the feature zones, on by
        // default, and refused by name by a build without it.
        #[cfg(not(feature = "zones"))]
        ("SET TimeZone = 'Europe/Lisbon'", "read only under the feature zones"),
        ("SET datestyle = 'ISO, DMY'", "datestyle ISO, MDY"),
        ("SET DateStyle = 'SQL'", "datestyle ISO, MDY"),
        ("SET IntervalStyle = iso_8601", "intervalstyle postgres"),
        ("SET extra_float_digits = 3", "extra_float_digits 1"),
        ("SET search_path = public", "has no search path"),
    ] {
        let e = snail.execute(s).unwrap_err().to_string();
        assert!(e.contains(says), "{s}: {e}");
    }
}

#[test]
fn a_snail_of_format_8_refuses_an_infinite_instant_on_a_level_by_name() {
    let catalog =
        Catalog::parse("CREATE SCHEMA s; CREATE TABLE s.t (at timestamptz NOT NULL, PRIMARY KEY (at));").unwrap();
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
        "CREATE SCHEMA s; CREATE TABLE s.t (id integer NOT NULL, d date, t time, s timestamp, PRIMARY KEY (id));",
        &["INSERT INTO s.t (id, d, t, s) VALUES (1, '-infinity', '24:00', '2024-01-01'), (2, '2024-01-01', '12:00', '2024-01-02')"],
    );
    let dir = tmp("time-out-dir");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let out = snail.out(&dir).unwrap();
    assert_eq!(out[0].extremes, ["d", "t"]);
}

/// The feature zones is among the crate's default features, so that a default build reads a named
/// zone; a build without the default features refuses one by name.
#[test]
fn zones_are_among_the_default_features() {
    let manifest =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).unwrap();
    let default = manifest
        .lines()
        .find(|l| l.starts_with("default = ["))
        .expect("the default features");
    assert!(default.contains("\"standalone\"") && default.contains("\"zones\""), "{default}");
}

#[cfg(all(feature = "zones", feature = "standalone"))]
#[test]
fn under_zones_an_instant_at_a_named_zone_reads_its_rules() {
    let _zones = zone_files();
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

#[cfg(all(feature = "zones", feature = "standalone"))]
#[test]
fn a_slim_zone_file_answers_sling_s_statement_as_postgresql_18_6_does() {
    let _zones = zone_files();
    /// The directory zones were read from, set again however the test ends.
    struct Back(std::path::PathBuf);
    impl Drop for Back {
        fn drop(&mut self) {
            warren_snail::zones::set_directory(self.0.clone());
        }
    }
    let _back = Back(warren_snail::zones::directory());
    // tests/zones-slim: compiled files of IANA's 2026c (public domain), the bytes jiff-tzdb 0.1.8
    // holds, as warren-sling writes its zones' directory from them; slim, each Baltic zone's last
    // transition changing nothing where the EU's rules begin again.
    warren_snail::zones::set_directory(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/zones-slim"),
    );
    let mut snail = snail_of("time-slim", &table("timestamptz"), &[]);
    // Each instant the snail once read by the footer's rule before the file's end, and the first
    // change the rule makes after it: the zone, the instant, and 18.6's answer on its own files.
    let asked = "\
        Europe/Vilnius | 2000-03-26 01:00:00+00        | 2000-03-26 03:00:00
        Europe/Vilnius | 2000-07-15 12:00:00+00        | 2000-07-15 14:00:00
        Europe/Vilnius | 2000-10-29 00:59:59.999999+00 | 2000-10-29 02:59:59.999999
        Europe/Vilnius | 2001-03-25 01:00:00+00        | 2001-03-25 03:00:00
        Europe/Vilnius | 2001-07-15 12:00:00+00        | 2001-07-15 14:00:00
        Europe/Vilnius | 2001-10-28 00:59:59.999999+00 | 2001-10-28 02:59:59.999999
        Europe/Vilnius | 2002-03-31 01:00:00+00        | 2002-03-31 03:00:00
        Europe/Vilnius | 2002-07-15 12:00:00+00        | 2002-07-15 14:00:00
        Europe/Vilnius | 2002-10-27 00:59:59.999999+00 | 2002-10-27 02:59:59.999999
        Europe/Vilnius | 2003-03-30 01:00:00+00        | 2003-03-30 04:00:00
        Europe/Riga    | 2000-03-26 01:00:00+00        | 2000-03-26 03:00:00
        Europe/Riga    | 2000-07-15 12:00:00+00        | 2000-07-15 14:00:00
        Europe/Riga    | 2000-10-29 00:59:59.999999+00 | 2000-10-29 02:59:59.999999
        Europe/Riga    | 2001-03-25 01:00:00+00        | 2001-03-25 04:00:00
        Europe/Tallinn | 2000-03-26 01:00:00+00        | 2000-03-26 03:00:00
        Europe/Tallinn | 2000-07-15 12:00:00+00        | 2000-07-15 14:00:00
        Europe/Tallinn | 2000-10-29 00:59:59.999999+00 | 2000-10-29 02:59:59.999999
        Europe/Tallinn | 2001-03-25 01:00:00+00        | 2001-03-25 03:00:00
        Europe/Tallinn | 2001-07-15 12:00:00+00        | 2001-07-15 14:00:00
        Europe/Tallinn | 2001-10-28 00:59:59.999999+00 | 2001-10-28 02:59:59.999999
        Europe/Tallinn | 2002-03-31 01:00:00+00        | 2002-03-31 04:00:00";
    for row in asked.lines() {
        let [zone, instant, local] = row.split('|').map(str::trim).collect::<Vec<_>>()[..] else {
            panic!("{row}");
        };
        snail.execute("BEGIN").unwrap();
        snail
            .execute(&format!("SET LOCAL TimeZone = '{zone}'"))
            .unwrap();
        let q = format!("SELECT CAST(TIMESTAMPTZ '{instant}' AS timestamp)");
        let (b, _) = snail.ask(&q, None).unwrap();
        snail.execute("COMMIT").unwrap();
        assert_eq!(texts(&b, 0), some(&[local]), "{zone} {instant}");
    }
}

#[cfg(feature = "standalone")]
#[test]
fn at_time_zone_reads_an_abbreviation_first_as_postgresql_does() {
    let _zones = zone_files();
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
    // Every other abbreviation of PostgreSQL's set at its own offset, never read as the zone file
    // of its name: EST five hours west, CET one east.
    for (zone, local, instant) in [
        ("EST", "2024-07-05 08:04:05", "2024-07-05 13:04:05+00"),
        ("cet", "2024-07-05 14:04:05", "2024-07-05 13:04:05+00"),
        ("PDT", "2024-07-05 06:04:05", "2024-07-05 13:04:05+00"),
    ] {
        let q = format!("{step} SELECT id, v AT TIME ZONE '{zone}' AS x FROM t");
        let (b, _) = snail.ask(&q, None).unwrap();
        assert_eq!(texts(&b, 1), some(&[local]), "{zone}");
        let q =
            format!("{step} SELECT id, timezone('{zone}', v AT TIME ZONE '{zone}') AS x FROM t");
        let (b, _) = snail.ask(&q, None).unwrap();
        assert_eq!(texts(&b, 1), some(&[instant]), "{zone}");
    }
    // One naming a zone is read by that zone's rules, only under the feature zones: MSK, Moscow's,
    // three hours east.
    let q = format!("{step} SELECT id, v AT TIME ZONE 'MSK' AS x FROM t");
    #[cfg(feature = "zones")]
    {
        let (b, _) = snail.ask(&q, None).unwrap();
        assert_eq!(texts(&b, 1), some(&["2024-07-05 16:04:05"]));
    }
    #[cfg(not(feature = "zones"))]
    {
        let e = snail.ask(&q, None).unwrap_err().to_string();
        assert!(e.contains("Europe/Moscow") && e.contains("zones"), "{e}");
    }
}

#[cfg(feature = "standalone")]
#[test]
fn at_time_zone_an_interval_is_its_seconds_east_of_utc_as_postgresql_reads_it() {
    let _zones = zone_files();
    let mut s = instants("time-at-interval");
    // timezone(interval, timestamptz) and timezone(interval, timestamp), qualified or not: the
    // interval's whole seconds east of UTC, an infinite value as it is, NULL for NULL.
    for (sql, want) in [
        (
            "SELECT timestamptz '2026-01-31 12:00:00+00' AT TIME ZONE interval '-05:00' hour to minute",
            "2026-01-31 07:00:00",
        ),
        ("SELECT at AT TIME ZONE interval '-05:00' hour to minute FROM s.t", "2026-01-31 17:00:00+00"),
        ("SELECT at AT TIME ZONE interval '+05:30' FROM s.t", "2026-01-31 06:30:00+00"),
        ("SELECT (at AT TIME ZONE 'UTC') AT TIME ZONE interval '+05:30' FROM s.t", "2026-01-31 17:30:00"),
        (
            "SELECT timezone(interval '2 hours', at), timezone(interval '-2 hours', at::timestamptz) FROM s.t",
            "2026-01-31 10:00:00+00|2026-01-31 10:00:00",
        ),
        (
            "SELECT at AT TIME ZONE '-05:00'::interval, at AT TIME ZONE CAST('3:00' AS interval hour to minute) FROM s.t",
            "2026-01-31 17:00:00+00|2026-01-31 09:00:00+00",
        ),
        (
            "SELECT pg_typeof(at AT TIME ZONE interval '1' hour), pg_typeof(at::timestamptz AT TIME ZONE interval '1' hour) FROM s.t",
            "timestamp with time zone|timestamp without time zone",
        ),
        (
            "SELECT at AT TIME ZONE interval '1.9 seconds', at AT TIME ZONE interval '-1.9 seconds', at AT TIME ZONE interval '25 hours' FROM s.t",
            "2026-01-31 11:59:59+00|2026-01-31 12:00:01+00|2026-01-30 11:00:00+00",
        ),
        (
            "SELECT timestamp 'infinity' AT TIME ZONE interval 'infinity', timestamptz '-infinity' AT TIME ZONE interval '1 day'",
            "infinity|-infinity",
        ),
        ("SELECT at AT TIME ZONE NULL::interval, NULL::timestamp AT TIME ZONE interval '1 hour' FROM s.t", "NULL|NULL"),
        (
            "SELECT id FROM s.t WHERE at AT TIME ZONE interval '-05:00' hour to minute = timestamptz '2026-01-31 17:00:00+00'",
            "I1",
        ),
        // A date is the instant of its midnight, whichever zone is named.
        (
            "SELECT date '2026-01-31' AT TIME ZONE interval '1 hour', date '2026-01-31' AT TIME ZONE 'UTC'",
            "2026-01-31 01:00:00|2026-01-31 00:00:00",
        ),
        // A zone's name over a column is read as over a constant.
        (
            "SELECT at AT TIME ZONE '+05:30', at AT TIME ZONE 'UTC' FROM s.t",
            "2026-01-31 17:30:00+00|2026-01-31 12:00:00+00",
        ),
    ] {
        assert_eq!(answer_rows(&mut s, sql), [want], "{sql}");
    }
    for (sql, code, words) in [
        (
            "SELECT at AT TIME ZONE interval '1 day' FROM s.t",
            "22023",
            "interval time zone \"1 day\" must not include months or days",
        ),
        (
            "SELECT at AT TIME ZONE interval '1 mon 2 hours' FROM s.t",
            "22023",
            "interval time zone \"1 mon 02:00:00\" must not include months or days",
        ),
        (
            "SELECT at::timestamptz AT TIME ZONE interval '-1 day -02:00:00' FROM s.t",
            "22023",
            "interval time zone \"-1 days -02:00:00\" must not include months or days",
        ),
        (
            "SELECT at AT TIME ZONE interval 'infinity' FROM s.t",
            "22023",
            "interval time zone \"infinity\" must be finite",
        ),
        (
            "SELECT at::timestamptz AT TIME ZONE interval '-infinity' FROM s.t",
            "22023",
            "interval time zone \"-infinity\" must be finite",
        ),
        (
            "SELECT timestamp '4714-11-24 00:00:00 BC' AT TIME ZONE interval '05:00'",
            "22008",
            "timestamp out of range",
        ),
        (
            "SELECT timestamptz '4714-11-24 00:00:00+00 BC' AT TIME ZONE interval '-05:00'",
            "22008",
            "timestamp out of range",
        ),
    ] {
        assert_eq!(refusal(&mut s, sql), (Some(code), words.to_string()), "{sql}");
    }
}

#[cfg(feature = "standalone")]
#[test]
fn a_transaction_reads_one_clock_its_start_wherever_postgresql_reads_it() {
    let mut s = snail_of(
        "one-clock",
        "CREATE SCHEMA s; CREATE TABLE s.t (id integer NOT NULL, at timestamptz, d date, \
         by timestamptz DEFAULT now(), PRIMARY KEY (id));",
        &[],
    );
    let one = |s: &mut Snail, q: &str| -> bool {
        let (b, _) = s.ask(q, None).unwrap();
        let a = b
            .column(0)
            .as_any()
            .downcast_ref::<arrow::array::BooleanArray>()
            .unwrap();
        a.value(0)
    };
    let step = "WITH q AS (SELECT id, at, d, by FROM s.t)";
    s.execute("BEGIN").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    s.execute("INSERT INTO s.t (id, at, d) VALUES (1, 'now', 'today'), (2, 'now', 'tomorrow')")
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    // Each statement of the transaction reads its start: now() and its kin, a literal's `now`,
    // `today` and `tomorrow`, and a DEFAULT now(); statement_timestamp() the statement's own, and
    // clock_timestamp() the machine's.
    for q in [
        "SELECT (count(*) = 2) AS x FROM q WHERE at = now() AND by = transaction_timestamp()",
        "SELECT (count(*) = 2) AS x FROM q WHERE at = 'now'::timestamptz AND by = current_timestamp",
        "SELECT (count(*) = 1) AS x FROM q WHERE d = 'today'::date AND d = current_date",
        "SELECT (count(*) = 1) AS x FROM q WHERE d = 'tomorrow'::date AND d = 'yesterday'::date + 2",
        "SELECT (count(*) = 2) AS x FROM q WHERE now() < statement_timestamp()",
        "SELECT (count(*) = 2) AS x FROM q WHERE statement_timestamp() <= clock_timestamp()",
    ] {
        assert!(one(&mut s, &format!("{step} {q}")), "{q}");
    }
    s.execute("COMMIT").unwrap();
    // A statement alone is its own transaction: now() is its start.
    assert!(one(
        &mut s,
        &format!("{step} SELECT (count(*) = 2) AS x FROM q WHERE now() > at AND now() = statement_timestamp()")
    ));
}

#[cfg(feature = "standalone")]
/// A statement's answer, each row's values as PostgreSQL's output functions write them, `|` between.
fn answer_rows(s: &mut Snail, sql: &str) -> Vec<String> {
    let done = s.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    let b = done.answer.expect("an answer");
    s.text_of(&b)
        .unwrap()
        .into_iter()
        .map(|r| {
            r.into_iter()
                .map(|v| v.unwrap_or_else(|| "NULL".into()))
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

#[cfg(feature = "standalone")]
/// A statement's refusal: its SQLSTATE and its words.
fn refusal(s: &mut Snail, sql: &str) -> (Option<&'static str>, String) {
    let e = s.execute(sql).err().unwrap_or_else(|| panic!("{sql} was taken"));
    (e.sqlstate(), e.to_string())
}

#[cfg(feature = "standalone")]
/// A snail of one table of instants, `s.t`, holding one row.
fn instants(name: &str) -> Snail {
    snail_of(
        name,
        "CREATE SCHEMA s; CREATE TABLE s.t (id varchar(4) NOT NULL, at timestamp NOT NULL, PRIMARY KEY (id));",
        &["INSERT INTO s.t VALUES ('I1', '2026-01-31 12:00:00')"],
    )
}

#[cfg(feature = "standalone")]
#[test]
fn an_interval_literal_with_a_qualifier_is_read_as_postgresql_reads_it() {
    let mut s = instants("interval-qualified");
    // warren-sling's statements, each answered as 18.6 answers it.
    for (sql, want) in [
        ("SELECT at + interval '1 day 2:03:04' hour to minute FROM s.t", "2026-02-01 14:03:00"),
        ("SELECT at + interval '2' hour FROM s.t", "2026-01-31 14:00:00"),
        ("SELECT at + interval '1:2' minute to second FROM s.t", "2026-01-31 12:01:02"),
        ("SELECT at + interval '1.2345' second(2) FROM s.t", "2026-01-31 12:00:01.23"),
        ("SELECT at + interval '1 year 2 months 3 days' month FROM s.t", "2027-03-31 12:00:00"),
        ("SELECT at + interval '1 2' day to hour FROM s.t", "2026-02-01 14:00:00"),
        ("SELECT at + interval '2.5' day FROM s.t", "2026-02-02 12:00:00"),
        ("SELECT at + interval '-1.9 hours' hour FROM s.t", "2026-01-31 11:00:00"),
    ] {
        assert_eq!(answer_rows(&mut s, sql), [want], "{sql}");
    }
    // Each qualifier: the fields below its last zeroed, a bare number in its last field's unit,
    // `a:b` as minutes and seconds under MINUTE TO SECOND, the seconds rounded half away from zero.
    let all = "'1 year 2 months 3 days 4:05:06.789'";
    for (fields, want) in [
        ("year", "1 year"),
        ("month", "1 year 2 mons"),
        ("day", "1 year 2 mons 3 days"),
        ("hour", "1 year 2 mons 3 days 04:00:00"),
        ("minute", "1 year 2 mons 3 days 04:05:00"),
        ("second", "1 year 2 mons 3 days 04:05:06.789"),
        ("second(1)", "1 year 2 mons 3 days 04:05:06.8"),
        ("year to month", "1 year 2 mons"),
        ("day to hour", "1 year 2 mons 3 days 04:00:00"),
        ("day to minute", "1 year 2 mons 3 days 04:05:00"),
        ("day to second(2)", "1 year 2 mons 3 days 04:05:06.79"),
        ("hour to minute", "1 year 2 mons 3 days 04:05:00"),
        ("hour to second(0)", "1 year 2 mons 3 days 04:05:07"),
        ("minute to second(1)", "1 year 2 mons 3 days 04:05:06.8"),
    ] {
        let sql = format!("SELECT interval {all} {fields}");
        assert_eq!(answer_rows(&mut s, &sql), [want], "{sql}");
    }
    for (sql, want) in [
        (
            "SELECT interval '7' year to month, interval '7' day to hour, interval '7' day to minute, \
             interval '7' day to second, interval '7' hour to minute, interval '7' minute to second",
            "7 mons|07:00:00|00:07:00|00:00:07|00:07:00|00:00:07",
        ),
        (
            "SELECT interval '1:2' hour, interval '1:2' minute, interval '1:2' second, \
             interval '3:4.5' minute to second, interval '1:2:3' minute to second, interval '1-2' year to month",
            "01:00:00|01:02:00|01:02:00|00:03:04.5|01:02:03|1 year 2 mons",
        ),
        (
            "SELECT interval '1.25' second(1), interval '-1.25' second(1), interval '2.5' second(0), \
             interval '-2.5' second(0), interval '-1.9995' second(3), interval '-0.5' hour",
            "00:00:01.3|-00:00:01.3|00:00:03|-00:00:03|-00:00:02|00:00:00",
        ),
        (
            "SELECT interval 'infinity' hour, interval '-13 months' year, interval '2147483648' hour, \
             interval E'2' minute, INTERVAL '1' HOUR TO MINUTE, interval '1 2:3:4' DAY to Second ( 1 )",
            "infinity|-1 years|2147483648:00:00|00:02:00|00:01:00|1 day 02:03:04",
        ),
        // A word sqlparser reads as a unit and PostgreSQL's grammar does not is the item's label.
        (
            "SELECT pg_typeof(interval '2' hour), interval '2' \"hour\", interval '1' week",
            "interval|00:00:02|00:00:01",
        ),
    ] {
        assert_eq!(answer_rows(&mut s, sql), [want], "{sql}");
    }
    let done = s.execute("SELECT interval '1' week").unwrap();
    assert_eq!(done.answer.unwrap().schema().field(0).name(), "week");
    // The label stands only in a select list or a RETURNING list, before what may follow an item.
    for (sql, want) in [
        ("SELECT interval '1' weeks, 2", vec!["00:00:01|2"]),
        ("SELECT interval '1' epoch FROM s.t", vec!["00:00:01"]),
        (
            "SELECT interval '1' years UNION SELECT interval '2' years ORDER BY 1",
            vec!["00:00:01", "00:00:02"],
        ),
        ("SELECT * FROM (SELECT interval '1' hours) v", vec!["00:00:01"]),
        ("SELECT (SELECT interval '1' week)", vec!["00:00:01"]),
        ("UPDATE s.t SET at = at WHERE id = 'I1' RETURNING interval '1' minutes", vec!["00:00:01"]),
        ("SELECT interval(2) '1.234' weeks", vec!["00:00:01.23"]),
    ] {
        assert_eq!(answer_rows(&mut s, sql), want, "{sql}");
    }
    // Split from a text, each statement is the one written, read as the cast where it is read.
    let parts = warren_snail::read::statement::split(
        "SELECT interval '2' hour; SELECT interval(2) '1.2345', interval '1' week",
    )
    .unwrap();
    assert_eq!(
        parts,
        ["SELECT interval '2' hour", "SELECT interval(2) '1.2345', interval '1' week"]
    );
    assert_eq!(answer_rows(&mut s, &parts[1]), ["00:00:01.23|00:00:01"]);
}

#[cfg(feature = "standalone")]
#[test]
fn an_interval_s_qualifier_is_read_beside_a_cast_a_column_and_a_function_s_body() {
    let _zones = zone_files();
    let mut s = instants("interval-qualified-beside");
    // A string constant cast to an interval of named fields is read under them; a value of text,
    // or a parameter, is read whole, then cut.
    for (sql, want) in [
        (
            "SELECT '2'::interval hour, ('2')::interval hour, CAST('1:2' AS interval minute to second), \
             CAST('1 day 2:03:04.5678' AS interval day to second(2))",
            "02:00:00|02:00:00|00:01:02|1 day 02:03:04.57",
        ),
        (
            "SELECT ('2'::text)::interval hour, CAST('1:2'::text AS interval minute to second), \
             CAST(interval '1 day 2:03:04' AS interval hour), (interval '90' minute)::interval hour",
            "00:00:00|01:02:00|1 day 02:00:00|01:00:00",
        ),
        (
            "SELECT '{\"1 day 2:03\"}'::interval hour[], - interval '1 day 2:03' hour",
            "{\"1 day 02:00:00\"}|-1 days -02:00:00",
        ),
    ] {
        assert_eq!(answer_rows(&mut s, sql), [want], "{sql}");
    }
    s.execute("PREPARE p AS SELECT $1::interval hour").unwrap();
    assert_eq!(answer_rows(&mut s, "EXECUTE p('2')"), ["00:00:00"]);
    // A column declared with fields reads a literal under them, its DEFAULT's among them, and
    // keeps them in its type.
    for sql in [
        "CREATE TABLE s.q (k integer NOT NULL, h interval hour DEFAULT '7', dm interval day to minute, \
         ms interval minute to second(1), PRIMARY KEY (k))",
        "INSERT INTO s.q VALUES (1, '1 day 2:03:04', '7', '1:2.345'), (2, interval '5' hour, '1:30', '1:30')",
        "INSERT INTO s.q (k) VALUES (3)",
        "INSERT INTO s.q SELECT length(id) + 2, '4' FROM s.t",
        "UPDATE s.q SET h = '9', dm = interval '1 day 2:03:04' hour WHERE k = 2",
    ] {
        s.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    assert_eq!(
        answer_rows(&mut s, "SELECT * FROM s.q ORDER BY k"),
        [
            "1|1 day 02:00:00|00:07:00|00:01:02.3",
            "2|09:00:00|1 day 02:00:00|00:01:30",
            "3|07:00:00|NULL|NULL",
            "4|04:00:00|NULL|NULL",
        ]
    );
    assert_eq!(
        answer_rows(
            &mut s,
            "SELECT attname, format_type(atttypid, atttypmod) FROM pg_attribute \
             WHERE attrelid = 's.q'::regclass AND attnum > 1 ORDER BY attnum"
        ),
        ["h|interval hour", "dm|interval day to minute", "ms|interval minute to second(1)"]
    );
    // A literal beside the column is read under the fields its cast names.
    assert_eq!(
        answer_rows(&mut s, "SELECT k FROM s.q WHERE h > interval '1' hour ORDER BY k"),
        ["1", "2", "3", "4"]
    );
    assert_eq!(
        answer_rows(&mut s, "SELECT k FROM s.q WHERE h > interval '7' hour ORDER BY k"),
        ["1", "2"]
    );
    // A view keeps the literal as the cast PostgreSQL's grammar makes of it.
    s.execute(
        "CREATE VIEW s.v AS SELECT k, interval '2' hour AS two, h + interval '1.2345' second(2) AS s FROM s.q",
    )
    .unwrap();
    assert_eq!(
        answer_rows(&mut s, "SELECT pg_get_viewdef('s.v'::regclass)"),
        [" SELECT k,\n    '02:00:00'::interval hour AS two,\n    (h + '00:00:01.23'::interval second(2)) AS s\n   FROM s.q;"]
    );
    // A function's body, after AS or after RETURN.
    s.execute("CREATE FUNCTION s.fq() RETURNS interval LANGUAGE sql AS $$ SELECT interval '2' hour $$")
        .unwrap();
    s.execute("CREATE FUNCTION s.fr() RETURNS interval LANGUAGE sql RETURN interval '2' minute")
        .unwrap();
    assert_eq!(answer_rows(&mut s, "SELECT s.fq(), s.fr()"), ["02:00:00|00:02:00"]);
    // SET TIME ZONE's interval is the setting's, its precision among it.
    s.execute("SET TIME ZONE INTERVAL(2) '+05:30'").unwrap();
    assert_eq!(answer_rows(&mut s, "SHOW TimeZone"), ["<+05:30>-05:30"]);
}

#[cfg(feature = "standalone")]
#[test]
fn a_cast_s_fields_read_a_constant_s_days_as_postgresql_reads_them() {
    let mut s = instants("interval-cast-days");
    // A bare number in the last field's unit, a day among them; `a b` as a day and hours where
    // the last field is HOUR; text read whole, then cut.
    for (sql, want) in [
        (
            "SELECT '1'::interval day, '1 2'::interval day to hour, interval '1' day",
            "1 day|1 day 02:00:00|1 day",
        ),
        (
            "SELECT '1 2'::interval hour, '2 1'::interval day to hour, '-1 2'::interval day to hour, \
             '1 -2'::interval day to hour, '1 2.5'::interval day to hour",
            "1 day 02:00:00|2 days 01:00:00|-1 days +02:00:00|1 day -02:00:00|1 day 02:00:00",
        ),
        (
            "SELECT '25:00'::interval day, '1.5'::interval day, ('1'::text)::interval day",
            "00:00:00|1 day|00:00:00",
        ),
        (
            "SELECT CAST('1' AS interval day), CAST('1 2' AS interval day to hour), \
             '-1'::interval day, '1 2:3'::interval day to minute",
            "1 day|1 day 02:00:00|-1 days|1 day 02:03:00",
        ),
    ] {
        assert_eq!(answer_rows(&mut s, sql), [want], "{sql}");
    }
    for sql in [
        "SELECT '1 2'::interval day",
        "SELECT '1 2'::interval day to minute",
        "SELECT '1 2'::interval minute",
        "SELECT '1 2'::interval day to second",
        "SELECT '1 2'::interval year to month",
        "SELECT ('1 2'::text)::interval day to hour",
    ] {
        assert_eq!(
            refusal(&mut s, sql),
            (Some("22007"), "invalid input syntax for type interval: \"1 2\"".to_string()),
            "{sql}"
        );
    }
}

#[cfg(feature = "standalone")]
#[test]
fn a_seconds_precision_past_6_is_taken_as_6_with_postgresql_s_warning() {
    let _zones = zone_files();
    let mut s = instants("precision-past-6");
    let warned = |kind: &str| format!("WARNING:  {kind} precision reduced to maximum allowed, 6");
    let (i7, t7, z7) = (warned("INTERVAL(7)"), warned("TIMESTAMP(7)"), warned("TIME(7) WITH TIME ZONE"));
    // Each statement, the WARNINGs it gives as 18.6 gives them, and its answer where it has one.
    let asked: Vec<(&str, Vec<String>, Option<&str>)> = vec![
        ("SELECT interval '1' second(7)", vec![i7.clone()], Some("00:00:01")),
        ("SELECT '1'::interval hour to second(7)", vec![i7.clone()], Some("00:00:01")),
        ("SELECT interval(7) '1.5'", vec![i7.clone()], Some("00:00:01.5")),
        (
            "SELECT '1'::interval(8), '2020-01-01 00:00:00.123456789'::timestamp(7), '12:00:00.1234567'::time(9)",
            vec![warned("INTERVAL(8)"), t7.clone(), warned("TIME(9)")],
            Some("00:00:01|2020-01-01 00:00:00.123457|12:00:00.123457"),
        ),
        (
            "SELECT '2020-01-01 00:00:00.1234567+00'::timestamptz(7), CAST('2020-01-01' AS timestamp(7) with time zone), '12:00+01'::timetz(7), '12:00+01'::time(7) with time zone",
            vec![
                warned("TIMESTAMP(7) WITH TIME ZONE"),
                warned("TIMESTAMP(7) WITH TIME ZONE"),
                z7.clone(),
                z7.clone(),
            ],
            Some("2020-01-01 00:00:00.123457+00|2020-01-01 00:00:00+00|12:00:00+01|12:00:00+01"),
        ),
        (
            "SELECT current_timestamp(7) > '2000-01-01', localtimestamp(7) > '2000-01-01', current_time(7) IS NOT NULL, localtime(7) IS NOT NULL",
            vec![warned("TIMESTAMP(7) WITH TIME ZONE"), t7.clone(), z7.clone(), warned("TIME(7)")],
            Some("t|t|t|t"),
        ),
        (
            "SELECT localtimestamp(0) = date_trunc('second', localtimestamp(0)), \
             current_timestamp(0) = date_trunc('second', current_timestamp(0))",
            vec![],
            Some("t|t"),
        ),
        ("SELECT '1'::interval second(6), '1'::interval day to second(6)", vec![], Some("00:00:01|00:00:01")),
        (
            "SELECT '1'::interval second(7) AS a WHERE '1'::interval second(8) > '0'",
            vec![i7.clone(), warned("INTERVAL(8)")],
            Some("00:00:01"),
        ),
        // A table's columns' types twice, as PostgreSQL reads its columns and then lays them out,
        // before what their expressions name.
        (
            "CREATE TABLE s.w (k integer NOT NULL, a timestamp(7), b interval day to second(9), \
             c time(7) with time zone, PRIMARY KEY (k))",
            vec![
                t7.clone(),
                warned("INTERVAL(9)"),
                z7.clone(),
                t7.clone(),
                warned("INTERVAL(9)"),
                z7.clone(),
            ],
            None,
        ),
        (
            "CREATE TABLE s.d (k integer NOT NULL, a timestamp(7) DEFAULT '2020-01-01'::timestamp(8), \
             b time(9) CHECK (b > '00:00'::time(7)), PRIMARY KEY (k))",
            vec![
                t7.clone(),
                warned("TIME(9)"),
                t7.clone(),
                warned("TIME(9)"),
                warned("TIMESTAMP(8)"),
                warned("TIME(7)"),
            ],
            None,
        ),
        (
            "INSERT INTO s.w VALUES (1, '2020-01-01 00:00:00.1234567', '1.2345678', '12:00:00.1234567+00')",
            vec![],
            None,
        ),
        (
            "INSERT INTO s.w VALUES (2, '2020-01-01'::timestamp(8), NULL, NULL)",
            vec![warned("TIMESTAMP(8)")],
            None,
        ),
        ("UPDATE s.w SET a = '2021-01-01'::timestamp(7) WHERE k = 2", vec![t7.clone()], None),
        (
            "SELECT * FROM s.w ORDER BY k",
            vec![],
            Some("1|2020-01-01 00:00:00.123457|00:00:01.234568|12:00:00.123457+00"),
        ),
        ("CREATE VIEW s.wv AS SELECT '1'::interval second(7) AS x", vec![i7.clone()], None),
        ("SELECT * FROM s.wv", vec![], Some("00:00:01")),
        ("PREPARE wp AS SELECT $1::timestamp(7)", vec![t7.clone()], None),
        ("EXECUTE wp('2020-01-01')", vec![], Some("2020-01-01 00:00:00")),
        // A body after AS is read where CREATE FUNCTION checks it and at each call; one in SQL's
        // own words only where it is declared.
        (
            "CREATE FUNCTION s.f7() RETURNS interval LANGUAGE sql AS $$ SELECT '1'::interval second(7) $$",
            vec![i7.clone()],
            None,
        ),
        ("SELECT s.f7(), s.f7()", vec![i7.clone(), i7.clone()], Some("00:00:01|00:00:01")),
        (
            "CREATE FUNCTION s.g7() RETURNS interval LANGUAGE sql RETURN '1'::interval second(7)",
            vec![i7.clone()],
            None,
        ),
        ("SELECT s.g7()", vec![], Some("00:00:01")),
        (
            "CREATE FUNCTION s.fa(a timestamp(7)) RETURNS time(8) LANGUAGE sql RETURN a::time",
            vec![t7.clone(), warned("TIME(8)")],
            None,
        ),
        ("SET TIME ZONE INTERVAL(7) '+05:30'", vec![i7.clone()], None),
        ("RESET TimeZone", vec![], None),
    ];
    for (sql, warnings, answer) in asked {
        let done = s.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert_eq!(done.notices, warnings, "{sql}");
        if let Some(want) = answer {
            let b = done.answer.expect("an answer");
            let rows: Vec<String> = s
                .text_of(&b)
                .unwrap()
                .into_iter()
                .map(|r| r.into_iter().map(|v| v.unwrap_or_else(|| "NULL".into())).collect::<Vec<_>>().join("|"))
                .collect();
            assert_eq!(rows[0], want, "{sql}");
        }
    }
    assert_eq!(
        answer_rows(
            &mut s,
            "SELECT format_type(atttypid, atttypmod) FROM pg_attribute WHERE attrelid = 's.w'::regclass AND attnum > 1 ORDER BY attnum"
        ),
        ["timestamp(6) without time zone", "interval day to second(6)", "time(6) with time zone"]
    );
}

#[cfg(feature = "standalone")]
#[test]
fn an_insert_of_constants_alone_inserts_a_row_for_each_its_question_returns() {
    let mut s = instants("insert-constants");
    // With a FROM or without, its constants read as the columns they fill, the rest DEFAULT.
    for sql in [
        "CREATE TABLE s.q (k integer NOT NULL, h interval hour DEFAULT '7', n text, PRIMARY KEY (k))",
        "INSERT INTO s.q SELECT 1, '4'",
        "INSERT INTO s.q SELECT 2, '1 day 2:03', 'x' FROM s.t",
        "INSERT INTO s.q SELECT 3, '5' FROM s.t WHERE false",
        "INSERT INTO s.q (k, n) SELECT 4, 'y'",
    ] {
        s.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    assert_eq!(
        answer_rows(&mut s, "INSERT INTO s.q (n, k) SELECT 'z', 5 FROM s.t RETURNING k, h, n"),
        ["5|07:00:00|z"]
    );
    for sql in [
        "INSERT INTO s.q SELECT 7 FROM s.t GROUP BY id",
        "INSERT INTO s.q SELECT 1, '9' FROM s.t ON CONFLICT (k) DO NOTHING",
    ] {
        s.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    let (code, words) = refusal(&mut s, "INSERT INTO s.q SELECT 1, '9'");
    assert_eq!(code, Some("23505"));
    assert!(words.starts_with("duplicate key value violates unique constraint \"q_pkey\""), "{words}");
    assert_eq!(
        answer_rows(&mut s, "SELECT * FROM s.q ORDER BY k"),
        [
            "1|04:00:00|NULL",
            "2|1 day 02:00:00|x",
            "4|07:00:00|y",
            "5|07:00:00|z",
            "7|07:00:00|NULL",
        ]
    );
}

#[cfg(feature = "standalone")]
#[test]
fn a_constant_its_question_makes_text_is_assigned_as_text_as_postgresql_assigns_it() {
    let mut s = instants("insert-made-text");
    s.execute(
        "CREATE TABLE s.q (k integer NOT NULL, h interval hour DEFAULT '7', n text, v varchar(3), \
         PRIMARY KEY (k))",
    )
    .unwrap();
    // A string or NULL of no type a DISTINCT, or a DISTINCT ON, ORDER BY or GROUP BY naming its
    // place or label, makes text: refused where its column takes no text.
    let not_text = |column: &str, ty: &str| {
        format!(
            "column \"{column}\" is of type {ty} but expression is of type text\nHINT:  You will \
             need to rewrite or cast the expression."
        )
    };
    for (sql, column, ty) in [
        ("INSERT INTO s.q SELECT DISTINCT 6, '6' FROM s.t", "h", "interval"),
        ("INSERT INTO s.q SELECT DISTINCT 6, NULL FROM s.t", "h", "interval"),
        ("INSERT INTO s.q SELECT DISTINCT '8' FROM s.t", "k", "integer"),
        ("INSERT INTO s.q SELECT DISTINCT 9, E'6' FROM s.t", "h", "interval"),
        ("INSERT INTO s.q SELECT 10, '6' FROM s.t ORDER BY 2", "h", "interval"),
        ("INSERT INTO s.q SELECT 11, '6' FROM s.t GROUP BY 2", "h", "interval"),
        ("INSERT INTO s.q SELECT DISTINCT ON (2) 13, '6' FROM s.t", "h", "interval"),
        ("INSERT INTO s.q SELECT DISTINCT length(id), '6' FROM s.t", "h", "interval"),
        ("INSERT INTO s.q SELECT DISTINCT 18, '6' WHERE false", "h", "interval"),
        ("INSERT INTO s.q SELECT 19, '6' AS x FROM s.t ORDER BY x", "h", "interval"),
    ] {
        assert_eq!(refusal(&mut s, sql), (Some("42804"), not_text(column, ty)), "{sql}");
    }
    for sql in [
        "INSERT INTO s.q SELECT DISTINCT 6, NULL::interval FROM s.t",
        "INSERT INTO s.q (k, n, v) SELECT DISTINCT 7, 'x', 'y' FROM s.t",
        "INSERT INTO s.q SELECT DISTINCT ON (1) 12, '6' FROM s.t",
    ] {
        s.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    assert_eq!(
        answer_rows(&mut s, "SELECT * FROM s.q ORDER BY k"),
        ["6|NULL|NULL|NULL", "7|07:00:00|x|y", "12|06:00:00|NULL|NULL"]
    );
}

#[cfg(feature = "standalone")]
#[test]
fn on_conflict_after_a_select_with_no_from_is_read_as_postgresql_reads_it() {
    let mut s = instants("on-conflict-no-from");
    for sql in [
        "CREATE TABLE s.c (k integer NOT NULL, h interval hour DEFAULT '7', n text, PRIMARY KEY (k))",
        "INSERT INTO s.c SELECT 1, '1'",
        "INSERT INTO s.c SELECT 8, '9' ON CONFLICT (k) DO NOTHING",
        "INSERT INTO s.c SELECT 8, '10' ON CONFLICT (k) DO NOTHING",
        "INSERT INTO s.c SELECT 1, '2', NULL ON CONFLICT (k) DO UPDATE SET n = 'u'",
        "INSERT INTO s.c SELECT 9 k ON CONFLICT (k) DO NOTHING",
        "INSERT INTO s.c SELECT 10 AS k ON CONFLICT (k) DO NOTHING",
    ] {
        s.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    assert_eq!(
        answer_rows(
            &mut s,
            "INSERT INTO s.c SELECT 8, '3' ON CONFLICT (k) DO UPDATE SET h = excluded.h RETURNING k, h"
        ),
        ["8|03:00:00"]
    );
    for sql in [
        "INSERT INTO s.c SELECT 11, '4', 'x' ON CONFLICT DO NOTHING",
        "INSERT INTO s.c SELECT 12, now() - now() ON CONFLICT (k) DO NOTHING",
    ] {
        s.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    assert_eq!(
        answer_rows(&mut s, "SELECT * FROM s.c ORDER BY k"),
        [
            "1|01:00:00|u",
            "8|03:00:00|NULL",
            "9|07:00:00|NULL",
            "10|07:00:00|NULL",
            "11|04:00:00|x",
            "12|00:00:00|NULL",
        ]
    );
    // Split from a text, as written.
    assert_eq!(
        warren_snail::read::statement::split("INSERT INTO s.c SELECT 13, '1' ON CONFLICT (k) DO NOTHING; SELECT 1")
            .unwrap(),
        ["INSERT INTO s.c SELECT 13, '1' ON CONFLICT (k) DO NOTHING", "SELECT 1"]
    );
}

#[cfg(feature = "standalone")]
#[test]
fn at_time_zone_a_time_with_time_zone_moves_its_zone_as_postgresql_does() {
    let _zones = zone_files();
    let mut s = instants("timetz-at-zone");
    // timetz_izone and timetz_zone: the same moment of the day on the other zone's clock, a zone's
    // name read at the transaction's start; a time a timetz on the session's zone.
    for (sql, want) in [
        (
            "SELECT timetz '12:00+00' AT TIME ZONE interval '1 hour', timetz '12:00+00' AT TIME ZONE 'UTC'",
            "13:00:00+01|12:00:00+00",
        ),
        (
            "SELECT timetz '23:30+05' AT TIME ZONE interval '-03:00', timetz '01:00-02' AT TIME ZONE \
             interval '+05:30', timetz '12:00+00' AT TIME ZONE interval '1.9 seconds'",
            "15:30:00-03|08:30:00+05:30|12:00:01+00:00:01",
        ),
        (
            "SELECT timetz '12:00+00' AT TIME ZONE 'EST', timetz '12:00+00' AT TIME ZONE '+02'",
            "07:00:00-05|10:00:00-02",
        ),
        (
            "SELECT timezone('UTC', timetz '12:00+03'), timezone(interval '2 hours', timetz '12:00+03'), \
             pg_typeof(timetz '12:00+03' AT TIME ZONE 'UTC')",
            "09:00:00+00|11:00:00+02|time with time zone",
        ),
        (
            "SELECT timetz '12:00+00' AT TIME ZONE NULL::text, NULL::timetz AT TIME ZONE interval '1 hour'",
            "NULL|NULL",
        ),
        (
            "SELECT time '12:00' AT TIME ZONE 'UTC', time '12:00' AT TIME ZONE interval '2 hours'",
            "12:00:00+00|14:00:00+02",
        ),
        (
            "SELECT timetz '24:00+00' AT TIME ZONE interval '1 hour', timetz '00:00+00' AT TIME ZONE \
             interval '-1 hour'",
            "01:00:00+01|23:00:00-01",
        ),
    ] {
        assert_eq!(answer_rows(&mut s, sql), [want], "{sql}");
    }
    #[cfg(feature = "zones")]
    assert_eq!(
        answer_rows(
            &mut s,
            "SELECT timetz '12:00+00' AT TIME ZONE 'Asia/Kolkata', timetz '12:00+00' AT TIME ZONE 'MSK'"
        ),
        ["17:30:00+05:30|15:00:00+03"]
    );
    for (sql, words) in [
        (
            "SELECT timetz '12:00+00' AT TIME ZONE interval '1 day'",
            "interval time zone \"1 day\" must not include months or days",
        ),
        (
            "SELECT timetz '12:00+00' AT TIME ZONE interval 'infinity'",
            "interval time zone \"infinity\" must be finite",
        ),
    ] {
        assert_eq!(refusal(&mut s, sql), (Some("22023"), words.to_string()), "{sql}");
    }
    #[cfg(feature = "zones")]
    assert_eq!(
        refusal(&mut s, "SELECT timetz '12:00+00' AT TIME ZONE 'Mars/Olympus'"),
        (Some("22023"), "time zone \"Mars/Olympus\" not recognized".to_string())
    );
}

#[cfg(feature = "standalone")]
#[test]
fn an_interval_s_qualifier_is_refused_where_postgresql_refuses_it_in_its_words() {
    let mut s = instants("interval-qualified-refused");
    for (sql, code, words) in [
        ("SELECT interval 'abc' hour", "22007", "invalid input syntax for type interval: \"abc\""),
        ("SELECT interval '1 2 3' day to hour", "22007", "invalid input syntax for type interval: \"1 2 3\""),
        ("SELECT interval '1 2' year", "22007", "invalid input syntax for type interval: \"1 2\""),
        ("SELECT interval '2147483648' day", "22015", "interval field value out of range: \"2147483648\""),
        ("SELECT interval '2147483647' year", "22008", "interval out of range"),
        ("SELECT interval '1' year to second", "42601", "syntax error at or near \"second\""),
        ("SELECT interval '1' month to day", "42601", "syntax error at or near \"to\""),
        ("SELECT interval '1' hour(2)", "42601", "syntax error at or near \"(\""),
        ("SELECT interval '1' second(2, 2)", "42601", "syntax error at or near \",\""),
        ("SELECT interval '1' second(1.5)", "42601", "syntax error at or near \"1.5\""),
        ("SELECT interval '1' day to second(2) to minute", "42601", "syntax error at or near \"to\""),
        ("SELECT interval '1' minute to hour", "42601", "syntax error at or near \"hour\""),
        ("SELECT interval '1' second to second", "42601", "syntax error at or near \"to\""),
        ("SELECT interval '1' year to", "42601", "syntax error at end of input"),
        ("SELECT interval '2' hour hour", "42601", "syntax error at or near \"hour\""),
        ("SELECT interval 1 hour", "42601", "syntax error at or near \"1\""),
        ("SELECT interval(2) '1.2345' hour", "42601", "syntax error at or near \"hour\""),
        ("SELECT '1'::interval hour(2)", "42601", "syntax error at or near \"(\""),
        ("SELECT '1'::interval(2) hour", "42601", "syntax error at or near \"hour\""),
        ("SELECT '1'::interval year to second", "42601", "syntax error at or near \"second\""),
        // A word sqlparser reads as a unit, where PostgreSQL's grammar takes no label, or after one.
        ("SELECT interval '1' years AS x", "42601", "syntax error at or near \"AS\""),
        ("SELECT interval '1' years x", "42601", "syntax error at or near \"x\""),
        ("SELECT interval '1' years as x", "42601", "syntax error at or near \"as\""),
        ("SELECT interval '1' years + 1", "42601", "syntax error at or near \"+\""),
        ("SELECT interval '1' week week", "42601", "syntax error at or near \"week\""),
        ("SELECT interval(2) '1.234' weeks AS x", "42601", "syntax error at or near \"AS\""),
        (
            "UPDATE s.t SET at = at WHERE id = 'I1' RETURNING interval '1' minutes AS m",
            "42601",
            "syntax error at or near \"AS\"",
        ),
        (
            "SELECT 1 WHERE interval '1' years > interval '1' day",
            "42601",
            "syntax error at or near \"years\"",
        ),
        ("SELECT (interval '1' years)", "42601", "syntax error at or near \"years\""),
        ("SELECT 1 ORDER BY interval '1' years", "42601", "syntax error at or near \"years\""),
        (
            "SELECT CASE WHEN true THEN interval '1' week END",
            "42601",
            "syntax error at or near \"week\"",
        ),
        (
            "SELECT interval '1' day BETWEEN interval '1' hours AND interval '2' days",
            "42601",
            "syntax error at or near \"hours\"",
        ),
        ("SELECT ARRAY[interval '1' week]", "42601", "syntax error at or near \"week\""),
        ("VALUES (interval '1' week)", "42601", "syntax error at or near \"week\""),
        (
            "CREATE TABLE s.bad (k integer NOT NULL, h interval month to day, PRIMARY KEY (k))",
            "42601",
            "syntax error at or near \"to\"",
        ),
    ] {
        assert_eq!(refusal(&mut s, sql), (Some(code), words.to_string()), "{sql}");
    }
    // A text holding one is split as psql sends each statement, as written, that one refused when
    // it is read.
    let parts =
        warren_snail::read::statement::split("SELECT interval '1' hour(2); SELECT interval '2' hour")
            .unwrap();
    assert_eq!(parts, ["SELECT interval '1' hour(2)", "SELECT interval '2' hour"]);
    assert_eq!(refusal(&mut s, &parts[0]).1, "syntax error at or near \"(\"");
    assert_eq!(answer_rows(&mut s, &parts[1]), ["02:00:00"]);
}
