//! The functions: the list of every function PostgreSQL defines from 14 to 19, each a type's or the
//! server's own; the numbers' functions in a pipeline, each in the form PostgreSQL resolves, an
//! integer's answer held to its type's range; what the pipeline refuses before any row is read; and
//! a question the question path does not read, handed to the pipeline.

mod weather;

#[cfg(feature = "standalone")]
use arrow::array::{Array, AsArray, RecordBatch};
#[cfg(feature = "standalone")]
use warren_snail::catalog::Catalog;
use warren_snail::functions::{all, server_family, Class};
#[cfg(feature = "standalone")]
use warren_snail::read::Snail;
#[cfg(feature = "standalone")]
use warren_snail::write::Rows;
#[cfg(feature = "standalone")]
use weather::{tmp, write};

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
const DDL: &str = "CREATE TABLE s.t (id integer NOT NULL, a integer, b smallint, c bigint, \
                   d double precision, n numeric, ok boolean, PRIMARY KEY (id));";

#[cfg(feature = "standalone")]
fn numbers(name: &str) -> Snail {
    snail_of(
        name,
        DDL,
        &[
            "INSERT INTO s.t VALUES (1, 2147483647, 32767, 9223372036854775807, 2.5, 2.5, true), \
           (2, -2147483648, -32768, -9223372036854775807, -2, 2, false), \
           (3, NULL, NULL, NULL, NULL, NULL, NULL)",
        ],
    )
}

#[cfg(feature = "standalone")]
/// An answer's column of text.
fn texts(b: &RecordBatch, c: usize) -> Vec<Option<String>> {
    let a = b.column(c).as_string::<i32>();
    (0..a.len())
        .map(|r| (!a.is_null(r)).then(|| a.value(r).to_string()))
        .collect()
}

#[cfg(feature = "standalone")]
/// The text of one expression over the table's first row, through a step.
fn one(snail: &mut Snail, expr: &str) -> warren_snail::Result<Option<String>> {
    let q = format!(
        "WITH t AS (SELECT id, a, b, c, d, n, ok FROM s.t WHERE id = 1) SELECT ({expr})::text AS x FROM t"
    );
    let (b, _) = snail.ask(&q, None)?;
    Ok(texts(&b, 0).remove(0))
}

#[cfg(feature = "standalone")]
fn refused(snail: &mut Snail, expr: &str) -> String {
    match one(snail, expr) {
        Ok(v) => panic!("{expr} gave {v:?}, where PostgreSQL refuses it"),
        Err(e) => e.to_string(),
    }
}

#[test]
fn the_list_names_each_release_s_functions_by_its_census_count() {
    // The census's count of every function SQL calls on 14 to 19 (FINDINGS-28.md).
    for (release, count) in [
        (14, 2685),
        (15, 2720),
        (16, 2753),
        (17, 2777),
        (18, 2845),
        (19, 2888),
    ] {
        assert_eq!(
            all().iter().filter(|f| f.in_release(release)).count(),
            count,
            "the functions of {release}"
        );
    }
    let mut seen = std::collections::HashSet::new();
    for f in all() {
        assert!(seen.insert(f.signature), "{} is listed twice", f.signature);
        if f.held() {
            assert_eq!(f.class, Class::Type, "{} is the server's own", f.signature);
        }
    }
    assert!(all().iter().any(|f| f.held()));
    assert_eq!(server_family("pg_backend_pid", 19), Some("sessions"));
    assert_eq!(
        server_family("pg_relation_size", 19),
        Some("statistics and sizes")
    );
    // age(xid) is the server's own, age of a timestamp a type's: the name is not refused whole.
    assert_eq!(server_family("age", 19), None);
    assert_eq!(server_family("sqrt", 19), None);
}

#[test]
#[cfg(feature = "standalone")]
fn a_function_of_the_server_s_own_is_refused_by_name_as_its_class() {
    let mut snail = numbers("server-own");
    for (call, class) in [
        ("pg_backend_pid()", "sessions"),
        ("version()", "the server itself"),
        ("pg_relation_size(1259)", "statistics and sizes"),
        ("pg_try_advisory_lock(1)", "locks"),
    ] {
        let e = refused(&mut snail, call);
        assert!(
            e.contains("is the server's own") && e.contains(class),
            "{call}: {e}"
        );
    }
}

#[test]
#[cfg(feature = "standalone")]
fn the_pipeline_s_integers_are_held_to_their_type_s_range() {
    let mut snail = numbers("int-range");
    assert_eq!(refused(&mut snail, "a + 1"), "integer out of range");
    assert_eq!(
        refused(&mut snail, "b + 1::smallint"),
        "smallint out of range"
    );
    assert_eq!(
        refused(&mut snail, "-2147483648 - 1"),
        "integer out of range"
    );
    assert_eq!(
        refused(&mut snail, "abs(-2147483648)"),
        "integer out of range"
    );
    assert_eq!(refused(&mut snail, "c + 1"), "bigint out of range");
    // An integer beside a bigint, or a smallint beside an integer, is the wider.
    assert_eq!(
        one(&mut snail, "a + 1::bigint").unwrap().as_deref(),
        Some("2147483648")
    );
    assert_eq!(one(&mut snail, "b + 1").unwrap().as_deref(), Some("32768"));
    assert_eq!(
        one(&mut snail, "a * 2::bigint").unwrap().as_deref(),
        Some("4294967294")
    );
    // A step's column keeps its type into the next step.
    let (r, _) = snail
        .ask(
            "WITH t AS (SELECT id, a FROM s.t), u AS (SELECT id, a + 0 AS z FROM t) \
             SELECT id, z + 1 AS x FROM u WHERE id = 2",
            None,
        )
        .unwrap();
    assert_eq!(r.num_rows(), 1);
    let e = snail
        .ask(
            "WITH t AS (SELECT id, a FROM s.t), u AS (SELECT id, a + 0 AS z FROM t) \
             SELECT id, z + 1 AS x FROM u WHERE id = 1",
            None,
        )
        .unwrap_err();
    assert_eq!(e.to_string(), "integer out of range");
}

#[test]
#[cfg(feature = "standalone")]
fn the_numbers_functions_take_the_form_postgresql_resolves() {
    let mut snail = numbers("forms");
    for (expr, want) in [
        // An integer to a function of double precision and of numeric goes to double precision.
        ("round(5)", "5"),
        ("sqrt(2)", "1.4142135623730951"),
        ("sqrt(2.0)", "1.414213562373095"),
        ("sqrt('2')", "1.4142135623730951"),
        ("round(2.5)", "3"),
        ("round(2.5::float8)", "2"),
        ("round(d)", "2"),
        ("round(n)", "3"),
        ("round(n, 3)", "2.500"),
        ("div(7, 2)", "3"),
        ("mod(-7, 3)", "-1"),
        ("power(2, 0.5)", "1.4142135623730950"),
        ("power(2.0, 3)", "8.0000000000000000"),
        ("to_hex(-1)", "ffffffff"),
        ("to_hex(-1::bigint)", "ffffffffffffffff"),
        ("gcd(a, 0)", "2147483647"),
        ("sind(30)", "0.5"),
        ("cbrt(d)", "1.3572088082974532"),
        ("factorial(20)", "2432902008176640000"),
        ("width_bucket(d, 0, 10, 4)", "2"),
        ("a IN (1, 2147483647)", "true"),
    ] {
        assert_eq!(
            one(&mut snail, expr).unwrap().as_deref(),
            Some(want),
            "{expr}"
        );
    }
    assert_eq!(one(&mut snail, "sqrt(NULL)").unwrap(), None);
    assert_eq!(one(&mut snail, "a NOT IN (1, NULL)").unwrap(), None);
    assert_eq!(
        refused(&mut snail, "gcd(1::int2, 1::int2)"),
        "function gcd(smallint, smallint) is not unique"
    );
    assert_eq!(
        refused(&mut snail, "round(d, 1)"),
        "function round(double precision, integer) does not exist"
    );
    assert_eq!(
        refused(&mut snail, "sqrt(-d)"),
        "cannot take square root of a negative number"
    );
    assert_eq!(
        refused(&mut snail, "ln(0)"),
        "cannot take logarithm of zero"
    );
}

#[test]
#[cfg(feature = "standalone")]
fn an_aggregate_with_no_form_for_its_type_is_refused_over_no_rows_too() {
    let mut snail = numbers("no-rows");
    for (agg, want) in [
        ("pg_backend_pid()", "pg_backend_pid is the server's own (sessions): a snail answers only what its file holds"),
        ("sum(ok)", "function sum(boolean) does not exist"),
        ("min(ok)", "function min(boolean) does not exist"),
        ("avg(ok)", "function avg(boolean) does not exist"),
        ("bool_and(a)", "function bool_and(integer) does not exist"),
    ] {
        let e = snail
            .ask(
                &format!(
                    "WITH t AS (SELECT id, a, ok FROM s.t) SELECT {agg} AS x FROM t WHERE id < 0"
                ),
                None,
            )
            .unwrap_err();
        assert_eq!(e.to_string(), want);
    }
}

#[test]
#[cfg(feature = "standalone")]
fn a_question_the_question_path_does_not_read_is_handed_to_the_pipeline() {
    let mut snail = numbers("handed");
    let (b, _) = snail
        .ask(
            "SELECT id, sqrt(abs(d))::text AS x FROM s.t WHERE id < 3 ORDER BY id",
            None,
        )
        .unwrap();
    assert_eq!(
        texts(&b, 1),
        [
            Some("1.5811388300841898".to_string()),
            Some("1.4142135623730951".to_string())
        ]
    );
    // What the question path reads stays on it.
    let (c, round) = snail.ask("SELECT count(*) FROM s.t", None).unwrap();
    assert_eq!(c.num_rows(), 1);
    assert!(!round.order.is_empty());
}

#[cfg(feature = "standalone")]
const TIMES: &str = "CREATE TABLE s.t (id integer NOT NULL, d date, tm time, ts timestamp, \
                     tstz timestamptz, iv interval, PRIMARY KEY (id));";

#[cfg(feature = "standalone")]
/// The text of one expression over the times' first row, through a step.
fn at(snail: &mut Snail, expr: &str) -> warren_snail::Result<Option<String>> {
    let q = format!(
        "WITH w AS (SELECT id, d, tm, ts, tstz, iv FROM s.t WHERE id = 1) SELECT ({expr})::text AS x FROM w"
    );
    let (b, _) = snail.ask(&q, None)?;
    Ok(texts(&b, 0).remove(0))
}

#[test]
#[cfg(feature = "standalone")]
fn the_functions_of_time_answer_as_postgresql_s_under_utc() {
    let mut snail = snail_of(
        "times",
        TIMES,
        &[
            "INSERT INTO s.t VALUES (1, '2024-02-29', '12:34:56.5', '2024-02-29 12:34:56.789012', \
           '2021-01-03 18:45:00+00', '1 year 2 mons 40 days 25:00:01.5'), \
           (2, 'infinity', NULL, '-infinity', 'infinity', 'infinity')",
        ],
    );
    for (expr, want) in [
        ("extract(year FROM d)", "2024"),
        ("extract(epoch FROM ts)", "1709210096.789012"),
        ("extract(second FROM tm)", "56.500000"),
        ("extract(isoyear FROM tstz)", "2020"),
        ("extract(week FROM tstz)", "53"),
        ("date_part('day', iv)", "40"),
        ("date_part('quarter', d)", "1"),
        ("date_part('dow', tstz)", "0"),
        ("date_trunc('month', ts)", "2024-02-01 00:00:00"),
        ("date_trunc('week', tstz)", "2020-12-28 00:00:00+00"),
        ("date_trunc('hour', iv)", "1 year 2 mons 40 days 25:00:00"),
        ("date_trunc('day', d)", "2024-02-29 00:00:00+00"),
        ("age(ts, tstz)", "3 years 1 mon 25 days 17:49:56.789012"),
        ("make_date(2024, 2, 29)", "2024-02-29"),
        ("make_interval(days => 3, secs => 1.5)", "3 days 00:00:01.5"),
        (
            "make_timestamptz(2024, 1, 1, 0, 0, 0, '+05:30')",
            "2023-12-31 18:30:00+00",
        ),
        (
            "date_bin('15 minutes', ts, '2001-01-01')",
            "2024-02-29 12:30:00",
        ),
        ("justify_days(iv)", "1 year 3 mons 10 days 25:00:01.5"),
        ("to_timestamp(1.5)", "1970-01-01 00:00:01.5+00"),
        ("\"timestamp\"(d, tm)", "2024-02-29 12:34:56.5"),
        ("(ts, iv) OVERLAPS (tstz, tstz)", "false"),
        ("(ts, iv) OVERLAPS (ts, ts)", "true"),
        ("isfinite(d)", "true"),
        ("age(ts) = age(date_trunc('day', now()), ts)", "true"),
    ] {
        assert_eq!(
            at(&mut snail, expr).unwrap().as_deref(),
            Some(want),
            "{expr}"
        );
    }
    // The statement reads one clock: now() is the same in every row and every step.
    let (b, _) = snail
        .ask(
            "WITH w AS (SELECT id, now() AS a FROM s.t) \
             SELECT (a = now() AND a = current_timestamp AND now() <= clock_timestamp())::text AS x FROM w",
            None,
        )
        .unwrap();
    assert_eq!(texts(&b, 0), [Some("true".into()), Some("true".into())]);
    assert_eq!(
        at(&mut snail, "date_trunc('day', '2020-01-01 10:00')")
            .unwrap_err()
            .to_string(),
        "function date_trunc(unknown, unknown) is not unique"
    );
    // An untyped NULL beside ends of two types leaves PostgreSQL more than one form.
    assert_eq!(
        at(&mut snail, "(d, ts) OVERLAPS (ts, NULL)")
            .unwrap_err()
            .to_string(),
        "function pg_catalog.overlaps(date, timestamp without time zone, timestamp without time \
         zone, unknown) is not unique"
    );
    let e = at(&mut snail, "age('5'::xid)").unwrap_err().to_string();
    assert!(e.contains("is the server's own (sessions)"), "{e}");
    let e = at(&mut snail, "date_part('bogus', ts)")
        .unwrap_err()
        .to_string();
    assert_eq!(
        e,
        "unit \"bogus\" not recognized for type timestamp without time zone"
    );
}

#[test]
#[cfg(feature = "standalone")]
fn the_least_bigint_is_one_constant_with_its_minus() {
    // PostgreSQL's grammar folds the minus into the number, so the least bigint is read whole; a
    // cast binds first, and -9223372036854775808::bigint is out of range.
    let mut snail = snail_of(
        "least-bigint",
        DDL,
        &["INSERT INTO s.t (id, c) VALUES (1, -9223372036854775808), (2, 0)"],
    );
    let (b, _) = snail
        .ask("SELECT id FROM s.t WHERE c = -9223372036854775808", None)
        .unwrap();
    assert_eq!(b.num_rows(), 1);
    assert_eq!(
        one(&mut snail, "c").unwrap().as_deref(),
        Some("-9223372036854775808")
    );
    assert_eq!(
        one(&mut snail, "-9223372036854775808").unwrap().as_deref(),
        Some("-9223372036854775808")
    );
    assert_eq!(
        refused(&mut snail, "-9223372036854775808::bigint"),
        "bigint out of range"
    );
}

#[cfg(feature = "standalone")]
const FOLDS: &str =
    "CREATE TABLE s.t (id integer NOT NULL, g integer, a integer, d double precision, \
                     n numeric, b boolean, PRIMARY KEY (id));";

#[cfg(feature = "standalone")]
fn folds(name: &str) -> Snail {
    snail_of(
        name,
        FOLDS,
        &[
            "INSERT INTO s.t VALUES (1, 1, 5, 1.5, 1.25, true), (2, 1, NULL, NULL, NULL, NULL), \
           (3, 2, -3, 2.5, 3.5, false), (4, 2, 7, -1, 0.5, true), (5, 1, 2, 4, 2, true), \
           (6, 2, 2, 0.25, 7, true)",
        ],
    )
}

/// Each row of a question through a step, every value as its text.
#[cfg(feature = "standalone")]
fn rows_of(snail: &mut Snail, select: &str) -> Vec<Vec<Option<String>>> {
    let q = format!("WITH w AS (SELECT id, g, a, d, n, b FROM s.t) {select}");
    let (b, _) = snail.ask(&q, None).unwrap();
    let columns: Vec<Vec<Option<String>>> = (0..b.num_columns())
        .map(|c| {
            let a = arrow::compute::cast(b.column(c), &arrow::datatypes::DataType::Utf8).unwrap();
            let a = a.as_string::<i32>();
            (0..a.len())
                .map(|r| (!a.is_null(r)).then(|| a.value(r).to_string()))
                .collect()
        })
        .collect();
    (0..b.num_rows())
        .map(|r| columns.iter().map(|c| c[r].clone()).collect())
        .collect()
}

#[test]
#[cfg(feature = "standalone")]
fn the_aggregates_past_those_the_types_carry_fold_as_postgresql_s() {
    let mut snail = folds("further");
    // PostgreSQL 19's answers over the same rows.
    for (agg, want) in [
        ("var_samp(a)", "14.3000000000000000"),
        ("stddev_pop(n)", "2.3000000000000000"),
        ("var_pop(d)", "3.0100000000000002"),
        ("corr(d, a)", "-0.5913344961224513"),
        ("regr_slope(d, a)", "-0.30332167832167833"),
        ("bit_or(a)", "-1"),
        ("bit_xor(a)", "-1"),
        ("count(DISTINCT a)", "4"),
        ("sum(DISTINCT a)", "11"),
        ("count(*) FILTER (WHERE b)", "4"),
        ("mode() WITHIN GROUP (ORDER BY a)", "2"),
        ("percentile_cont(0.5) WITHIN GROUP (ORDER BY d)", "1.5"),
        (
            "percentile_cont(0.3) WITHIN GROUP (ORDER BY d)",
            "0.49999999999999994",
        ),
        ("percentile_disc(0.5) WITHIN GROUP (ORDER BY n)", "2"),
        ("rank(2) WITHIN GROUP (ORDER BY a)", "2"),
        (
            "cume_dist(2) WITHIN GROUP (ORDER BY a)",
            "0.5714285714285714",
        ),
        ("any_value(a)", "5"),
        ("every(b)", "false"),
    ] {
        let got = rows_of(&mut snail, &format!("SELECT ({agg})::text AS x FROM w"));
        assert_eq!(got, [[Some(want.to_string())]], "{agg}");
    }
    let e = snail
        .ask(
            "WITH w AS (SELECT id, b FROM s.t) SELECT var_pop(b) AS x FROM w WHERE id < 0",
            None,
        )
        .unwrap_err();
    assert_eq!(e.to_string(), "function var_pop(boolean) does not exist");
    // Over no rows a hypothetical row ranks first.
    assert_eq!(
        rows_of(
            &mut snail,
            "SELECT (rank(3) WITHIN GROUP (ORDER BY a))::text AS x FROM w WHERE id < 0"
        ),
        [[Some("1".to_string())]]
    );
    // SELECT DISTINCT in a step, and handed to the pipeline from a question.
    assert_eq!(
        rows_of(&mut snail, "SELECT DISTINCT g FROM w ORDER BY g"),
        [[Some("1".to_string())], [Some("2".to_string())]]
    );
    let (b, _) = snail
        .ask("SELECT DISTINCT g::text AS g FROM s.t ORDER BY g", None)
        .unwrap();
    assert_eq!(b.num_rows(), 2);
}

#[test]
#[cfg(feature = "standalone")]
fn the_windows_read_their_partitions_whole_or_along_the_order() {
    let mut snail = folds("windows");
    let got = rows_of(
        &mut snail,
        "SELECT id, (ntile(2) OVER (ORDER BY id))::text AS a, (lag(a, 2, 0) OVER (ORDER BY id))::text AS b, \
         (lead(a) OVER (PARTITION BY g ORDER BY id))::text AS c, \
         (cume_dist() OVER (PARTITION BY g ORDER BY id))::text AS d, (sum(a) OVER (PARTITION BY g))::text AS e, \
         (last_value(a) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING))::text AS f, \
         (avg(d) OVER (ORDER BY id))::text AS h, (nth_value(a, 3) OVER (ORDER BY id))::text AS i FROM w ORDER BY id",
    );
    let want: Vec<Vec<Option<String>>> = [
        "1|1|0||0.3333333333333333|7|2|1.5|",
        "2|1|0|2|0.6666666666666666|7|2|1.5|",
        "3|1|5|7|0.3333333333333333|6|2|2|-3",
        "4|2||2|0.6666666666666666|6|2|1|-3",
        "5|2|-3||1|7|2|1.75|-3",
        "6|2|7||1|6|2|1.45|-3",
    ]
    .iter()
    .map(|l| {
        l.split('|')
            .map(|v| (!v.is_empty()).then(|| v.to_string()))
            .collect()
    })
    .collect();
    let got: Vec<Vec<Option<String>>> = got
        .into_iter()
        .map(|mut r| {
            r[0] = r[0].take().or(Some(String::new()));
            r
        })
        .collect();
    assert_eq!(got.len(), want.len());
    for (g, w) in got.iter().zip(&want) {
        assert_eq!(g[1..], w[1..], "row {:?}", w[0]);
    }
    // A float's running sum, folded as the rows come.
    assert_eq!(
        rows_of(
            &mut snail,
            "SELECT id, (sum(d) OVER (ORDER BY id))::text AS x FROM w ORDER BY id"
        )
        .into_iter()
        .map(|r| r[1].clone().unwrap())
        .collect::<Vec<_>>(),
        ["1.5", "1.5", "4", "3", "7", "7.25"]
    );
    let e = snail
        .ask(
            "WITH w AS (SELECT id, a FROM s.t) SELECT ntile(0) OVER (ORDER BY id) AS x FROM w",
            None,
        )
        .unwrap_err();
    assert_eq!(e.to_string(), "argument of ntile must be greater than zero");
}

#[test]
#[cfg(feature = "standalone")]
fn the_functions_of_strings_answer_as_postgresql_s() {
    let mut snail = numbers("strings");
    // PostgreSQL 19's answers.
    for (expr, want) in [
        (
            "format('%2$s %1$s|%-4s|%*s|%L|%I', 'a', 'b', 3, 'x', 'it''s', 'Foo')",
            "b a|b   |  x|'it''s'|\"Foo\"",
        ),
        ("quote_ident('user')", "\"user\""),
        ("quote_ident('abs')", "abs"),
        ("quote_literal(E'a\\\\b')", "E'a\\\\b'"),
        ("lpad('hi', 6, 'xy')", "xyxyhi"),
        ("rpad('hi', 1)", "h"),
        ("split_part('a,b,c', ',', -2)", "b"),
        ("translate('abcdef', 'abc', 'xy')", "xydef"),
        ("initcap('hello wORLD-foo 1st')", "Hello World-Foo 1st"),
        ("overlay('Txxxxas' placing 'hom' from 2 for 4)", "Thomas"),
        ("substring('Thomas' from 2 for 3)", "hom"),
        ("position('om' in 'Thomas')", "3"),
        ("trim(both 'x' from 'xxabcxx')", "abc"),
        ("unistr('\\0041\\+01F600')", "A😀"),
        ("md5('abc')", "900150983cd24fb0d6963f7d28e17f72"),
        (
            "encode(sha256('abc'::bytea), 'base64')",
            "ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0=",
        ),
        ("crc32c('abc'::bytea)", "910901175"),
        ("concat('a', NULL, 1, true)", "a1t"),
        ("concat_ws('-', 'a', NULL, 'b')", "a-b"),
        ("chr(128512)", "😀"),
        ("left('abcde', -2)", "abc"),
        ("reverse('é漢字')", "字漢é"),
    ] {
        assert_eq!(
            one(&mut snail, expr).unwrap().as_deref(),
            Some(want),
            "{expr}"
        );
    }
    assert_eq!(
        refused(&mut snail, "format('%z', 1)"),
        "unrecognized format() type specifier \"z\""
    );
    assert_eq!(
        refused(&mut snail, "split_part('a', ',', 0)"),
        "field position must not be zero"
    );
    let mut folded = folds("string-agg");
    assert_eq!(
        rows_of(
            &mut folded,
            "SELECT string_agg(a::text, ',' ORDER BY a DESC) AS x FROM w"
        ),
        [[Some("7,5,2,2,-3".to_string())]]
    );
}

#[test]
#[cfg(feature = "standalone")]
fn the_formatting_functions_write_and_read_as_postgresql_s() {
    let mut snail = numbers("formatting");
    for (expr, want) in [
        ("to_char(1234.5, 'FM9,999.00')", "1,234.50"),
        (
            "to_char('2024-02-29 13:05:09'::timestamp, 'Dy DD Mon YYYY HH12:MI:SS PM')",
            "Thu 29 Feb 2024 01:05:09 PM",
        ),
        ("to_char(interval '25 hours 3 minutes', 'HH24:MI')", "25:03"),
        ("to_number('1,234.5-', '9G999D9S')", "-1234.5"),
        ("to_date('29 02 2024', 'DD MM YYYY')", "2024-02-29"),
        (
            "to_timestamp('2024-02-29 13:05', 'YYYY-MM-DD HH24:MI')",
            "2024-02-29 13:05:00+00",
        ),
        ("to_char(42, 'FM999th')", "42nd"),
    ] {
        assert_eq!(
            one(&mut snail, expr).unwrap().as_deref(),
            Some(want),
            "{expr}"
        );
    }
}
