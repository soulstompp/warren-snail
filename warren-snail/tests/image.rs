//! The record image: `*=` and `*<>` over whole records, each field compared by its own bytes where
//! `=` compares its class, as a plain question, a WHERE and a WITH step; two row constructors
//! compared a pair at a time; what PostgreSQL refuses, in its words; and `*<`, `*<=`, `*>`, `*>=` and
//! `record_image_ops` refused by name. Each expected answer is PostgreSQL's, read on 18.6 and 19 RC1
//! (`ledgers/ddlfix.md`).

mod weather;

use arrow::array::{Array, RecordBatch};
use warren_snail::catalog::Catalog;
use warren_snail::read::Snail;
use weather::{tmp, write};

fn made(name: &str, ddl: &str) -> std::path::PathBuf {
    let path = tmp(&format!("image-{name}.snail"));
    write(&path, &Catalog::parse(ddl).unwrap(), &[]).unwrap();
    path
}

fn empty(name: &str) -> Snail {
    Snail::open(&made(name, "CREATE SCHEMA s; CREATE TABLE s.e (id integer PRIMARY KEY);")).unwrap()
}

/// An answer's rows, each cell as arrow writes it, NULL as "NULL".
fn rows(s: &mut Snail, sql: &str) -> Vec<Vec<String>> {
    let (b, _): (RecordBatch, _) = s.ask(sql, None).unwrap_or_else(|e| panic!("{sql}: {e}"));
    (0..b.num_rows())
        .map(|r| {
            b.columns()
                .iter()
                .map(|a| match a.is_null(r) {
                    true => "NULL".to_string(),
                    false => arrow::util::display::array_value_to_string(a, r).unwrap(),
                })
                .collect()
        })
        .collect()
}

fn refused(s: &mut Snail, sql: &str) -> String {
    match s.ask(sql, None) {
        Ok(_) => panic!("{sql} was taken"),
        Err(e) => e.to_string(),
    }
}

/// `*=`, `*<>` and `=` over the records of a row constructor of each value, in a step.
fn asked(s: &mut Snail, a: &str, b: &str) -> [String; 3] {
    let sql = format!(
        "SELECT x *= y AS i, x *<> y AS n, x = y AS e FROM (SELECT ROW({a}) AS x, ROW({b}) AS y) q"
    );
    let r = rows(s, &sql);
    [r[0][0].clone(), r[0][1].clone(), r[0][2].clone()]
}

#[test]
fn equal_values_whose_bytes_differ_are_parted_and_only_those() {
    let mut s = empty("parted");
    for (a, b) in [
        ("1.0", "1.00"),
        ("'0'::numeric", "'0.0'::numeric"),
        ("'-0'::float8", "'0'::float8"),
        ("'NaN'::float8", "-('NaN'::float8)"),
        ("'NaN'::float8", "'Infinity'::float8 - 'Infinity'::float8"),
        ("'-0'::float4", "'0'::float4"),
        ("'NaN'::float4", "-('NaN'::float4)"),
        ("'a'::bpchar", "'a '::bpchar"),
        ("'1 day'::interval", "'24 hours'::interval"),
        ("'1 mon'::interval", "'30 days'::interval"),
        ("'{\"a\":1.0}'::jsonb", "'{\"a\":1}'::jsonb"),
        ("'[1.0, 2]'::jsonb", "'[1.00, 2]'::jsonb"),
        ("ARRAY[1.0]", "ARRAY[1.00]"),
        ("'{-0}'::float8[]", "'{0}'::float8[]"),
        ("numrange(1.0, 2)", "numrange(1.00, 2)"),
        ("'{[1.0,2)}'::nummultirange", "'{[1.00,2)}'::nummultirange"),
    ] {
        assert_eq!(asked(&mut s, a, b), ["false", "true", "true"], "{a}, {b}");
    }
    // Of one type by unlike modifiers.
    let image = |s: &mut Snail, a: &str, b: &str| {
        let sql = format!("SELECT x *= y AS i FROM (SELECT ROW({a}) AS x, ROW({b}) AS y) q");
        rows(s, &sql)[0][0].clone()
    };
    assert_eq!(image(&mut s, "'a'::char(3)", "'a'::char(5)"), "false");
    assert_eq!(image(&mut s, "1.50::numeric(5,2)", "1.5::numeric(5,1)"), "false");
    assert_eq!(image(&mut s, "1.50::numeric(5,2)", "15.0::numeric(5,1)"), "false");
    assert_eq!(image(&mut s, "1.50::numeric(5,2)", "1.50::numeric(6,2)"), "true");
    assert_eq!(image(&mut s, "B'1010'::bit(4)", "B'10100'::bit(5)"), "false");
}

#[test]
fn a_value_is_its_own_image_however_it_was_spelled() {
    let mut s = empty("spelled");
    for (a, b) in [
        ("'a'::char(3)", "'a  '::char(3)"),
        ("'1e2'::jsonb", "'100'::jsonb"),
        ("'{\"a\":1,\"b\":2}'::jsonb", "'{\"b\":2,\"a\":1}'::jsonb"),
        ("'[1,3)'::int4range", "'[1,2]'::int4range"),
        ("'2020-01-01 00:00+00'::timestamptz", "'2020-01-01 01:00+01'::timestamptz"),
        ("'10.0.0.0/8'::cidr", "'10/8'::cidr"),
        ("'a:1 b:2'::tsvector", "to_tsvector('simple', 'a b')"),
    ] {
        assert_eq!(asked(&mut s, a, b), ["true", "false", "true"], "{a}, {b}");
    }
    // A type with no equality has an image all the same, and `=` is refused.
    for (a, b, same) in [
        ("'$.a'::jsonpath", "'$.\"a\"'::jsonpath", "true"),
        ("'{\"a\":1}'::json", "'{\"a\":1}'::json", "true"),
        ("'{\"a\":1}'::json", "'{\"a\": 1}'::json", "false"),
        ("'(1,2)'::point", "'(1.0,2.0)'::point", "true"),
        ("'(0,0)'::point", "'(0,-0)'::point", "false"),
        ("'((0,0),(1,1))'::path", "'[(0,0),(1,1)]'::path", "false"),
        ("'((0,0),(1,1))'::box", "'((1,1),(0,0))'::box", "true"),
    ] {
        let sql = format!("SELECT x *= y AS i FROM (SELECT ROW({a}) AS x, ROW({b}) AS y) q");
        assert_eq!(rows(&mut s, &sql), [[same]], "{a}, {b}");
    }
}

#[test]
fn values_both_their_bytes_and_their_class_part_are_parted() {
    let mut s = empty("unequal");
    for (a, b) in [
        ("'a'::text", "'b'::text"),
        ("'12:00+01'::timetz", "'11:00+00'::timetz"),
        ("'b & a'::tsquery", "'a & b'::tsquery"),
        ("'{1,2}'::int[]", "'[0:1]={1,2}'::int[]"),
        ("'{1,2}'::int[]", "'{{1,2}}'::int[]"),
        ("'[1,2)'::int4range", "'[1,2]'::int4range"),
        ("'(1,2)'::numrange", "'[1,2]'::numrange"),
        ("'{1,NULL}'::int[]", "'{1,2}'::int[]"),
        ("'{}'::int[]", "'{NULL}'::int[]"),
        ("'empty'::int4range", "'[1,2)'::int4range"),
    ] {
        let [i, n, _] = asked(&mut s, a, b);
        assert_eq!([i, n], ["false", "true"], "{a}, {b}");
    }
}

#[test]
fn a_record_inside_a_record_carries_its_row_type() {
    let mut s = empty("nested");
    let image = |s: &mut Snail, a: &str, b: &str| {
        let sql = format!("SELECT x *= y AS i FROM (SELECT {a} AS x, {b} AS y) q");
        rows(s, &sql)[0][0].clone()
    };
    assert_eq!(image(&mut s, "ROW(ROW(1))", "ROW(ROW(1))"), "true");
    assert_eq!(image(&mut s, "ROW(ROW(1.0))", "ROW(ROW(1.00))"), "false");
    assert_eq!(image(&mut s, "ROW(ROW(1))", "ROW(ROW(1, 2))"), "false");
    // At the top, the same values of one type by unlike modifiers are the same record; inside, two
    // row types.
    assert_eq!(image(&mut s, "ROW('a'::varchar(3))", "ROW('a'::varchar(5))"), "true");
    assert_eq!(
        image(&mut s, "ROW(ROW('a'::varchar(3)))", "ROW(ROW('a'::varchar(5)))"),
        "false"
    );
    assert_eq!(image(&mut s, "ROW()", "ROW()"), "true");
}

#[test]
fn two_nulls_are_one_image_and_a_null_record_is_null() {
    let mut s = empty("nulls");
    let image = |s: &mut Snail, a: &str, b: &str| {
        let sql = format!("SELECT x *= y AS i, x *<> y AS n FROM (SELECT {a} AS x, {b} AS y) q");
        rows(s, &sql)[0].clone()
    };
    assert_eq!(image(&mut s, "ROW(NULL)", "ROW(NULL)"), ["true", "false"]);
    assert_eq!(image(&mut s, "ROW(NULL)", "ROW('x')"), ["false", "true"]);
    assert_eq!(image(&mut s, "ROW(NULL, 1)", "ROW(NULL, 1)"), ["true", "false"]);
    assert_eq!(image(&mut s, "ROW('x', 1)", "ROW(NULL, 1)"), ["false", "true"]);
    assert_eq!(rows(&mut s, "SELECT NULL *= NULL AS x"), [["NULL"]]);
    assert_eq!(rows(&mut s, "SELECT ROW(1) *= NULL AS x"), [["NULL"]]);
    assert_eq!(rows(&mut s, "SELECT NULL *<> ROW(1) AS x"), [["NULL"]]);
}

#[test]
fn two_row_constructors_are_compared_a_pair_at_a_time() {
    let mut s = empty("pairs");
    assert_eq!(
        refused(&mut s, "SELECT ROW(1.0) *= ROW(1.00) AS x"),
        "operator does not exist: numeric *= numeric"
    );
    assert_eq!(
        refused(&mut s, "SELECT ROW(1.0) *<> ROW(1.00) AS x"),
        "operator does not exist: numeric *<> numeric"
    );
    assert_eq!(
        refused(&mut s, "SELECT ROW(1, 2) *= ROW(1) AS x"),
        "unequal number of entries in row expressions"
    );
    assert_eq!(
        refused(&mut s, "SELECT ROW() *= ROW() AS x"),
        "cannot compare rows of zero length"
    );
    for (sql, want) in [
        ("SELECT ROW(ROW(1.0)) *= ROW(ROW(1.00)) AS x", "false"),
        ("SELECT ROW(ROW(1.0), ROW(2)) *= ROW(ROW(1.00), ROW(2)) AS x", "false"),
        ("SELECT ROW(ROW(1.0), ROW(2)) *<> ROW(ROW(1.00), ROW(2)) AS x", "true"),
        ("SELECT ROW(ROW(1), ROW(2)) *= ROW(ROW(1), ROW(2)) AS x", "true"),
        ("SELECT ROW(ROW(1), ROW(2)) *<> ROW(ROW(1), ROW(2)) AS x", "false"),
        // Their pairs joined by AND and OR, a NULL among them.
        ("SELECT ROW(ROW(1), NULL) *= ROW(ROW(1), ROW(2)) AS x", "NULL"),
        ("SELECT ROW(ROW(1), NULL) *<> ROW(ROW(1), ROW(2)) AS x", "NULL"),
        ("SELECT ROW(ROW(2), NULL) *= ROW(ROW(1), ROW(2)) AS x", "false"),
        ("SELECT ROW(ROW(2), NULL) *<> ROW(ROW(1), ROW(2)) AS x", "true"),
    ] {
        assert_eq!(rows(&mut s, sql), [[want]], "{sql}");
    }
    // A pair that answers stops the walk before a later pair's refusal.
    assert_eq!(
        rows(
            &mut s,
            "SELECT ROW(ROW(2), ROW(1)) *= ROW(ROW(1), ROW(1::bigint)) AS x"
        ),
        [["false"]]
    );
    assert_eq!(
        refused(
            &mut s,
            "SELECT ROW(ROW(1), ROW(1)) *= ROW(ROW(1), ROW(1::bigint)) AS x"
        ),
        "cannot compare dissimilar column types integer and bigint at record column 1"
    );
}

#[test]
fn what_postgresql_refuses_is_refused_in_its_words() {
    let mut s = empty("words");
    for (sql, why) in [
        ("SELECT 1 *= 1 AS x", "operator does not exist: integer *= integer"),
        ("SELECT 2*=3 AS x", "operator does not exist: integer *= integer"),
        ("SELECT 1 *= NULL AS x", "operator does not exist: integer *= unknown"),
        ("SELECT NULL *= 1 AS x", "operator does not exist: unknown *= integer"),
        ("SELECT 1 *= 'a' AS x", "operator does not exist: integer *= unknown"),
        ("SELECT ROW(1) *= 1 AS x", "operator does not exist: record *= integer"),
        ("SELECT 1 *<> ROW(1) AS x", "operator does not exist: integer *<> record"),
        ("SELECT 'a'::text *= 'a'::text AS x", "operator does not exist: text *= text"),
        ("SELECT ARRAY[1] *= ARRAY[1] AS x", "operator does not exist: integer[] *= integer[]"),
        ("SELECT 'a' *= 'a' AS x", "input of anonymous composite types is not implemented"),
        ("SELECT ROW(1) *= '(1)' AS x", "input of anonymous composite types is not implemented"),
        ("SELECT '(1)' *= ROW(1) AS x", "input of anonymous composite types is not implemented"),
    ] {
        assert_eq!(refused(&mut s, sql), why, "{sql}");
    }
    let two = |a: &str, b: &str| format!("SELECT x *= y AS i FROM (SELECT {a} AS x, {b} AS y) q");
    for (a, b, why) in [
        ("ROW(1)", "ROW(1::bigint)", "cannot compare dissimilar column types integer and bigint at record column 1"),
        ("ROW('a'::varchar(3))", "ROW('a'::text)", "cannot compare dissimilar column types character varying and text at record column 1"),
        ("ROW(B'101'::bit(3))", "ROW(B'101'::bit varying)", "cannot compare dissimilar column types bit and bit varying at record column 1"),
        ("ROW('x')", "ROW(1)", "cannot compare dissimilar column types unknown and integer at record column 1"),
        ("ROW(NULL)", "ROW(1)", "cannot compare dissimilar column types unknown and integer at record column 1"),
        ("ROW(1, 1)", "ROW(1, 1::bigint)", "cannot compare dissimilar column types integer and bigint at record column 2"),
        ("ROW(1)", "ROW(1, 2)", "cannot compare record types with different numbers of columns"),
        ("ROW(1, 2)", "ROW(1)", "cannot compare record types with different numbers of columns"),
    ] {
        assert_eq!(refused(&mut s, &two(a, b)), why, "{a}, {b}");
    }
    // A pair that differs ahead of them answers.
    assert_eq!(rows(&mut s, &two("ROW(1, 1)", "ROW(2)")), [["false"]]);
    assert_eq!(rows(&mut s, &two("ROW(2, 1)", "ROW(1, 1::bigint)")), [["false"]]);
}

#[test]
fn the_operator_is_one_word_as_postgresql_s_lexer_reads_it() {
    let mut s = empty("lexed");
    let on = "(SELECT ROW(1.0) AS a, ROW(1.00) AS b) q";
    assert_eq!(rows(&mut s, &format!("SELECT a*=b AS x FROM {on}")), [["false"]]);
    assert_eq!(rows(&mut s, &format!("SELECT a*<>b AS x FROM {on}")), [["true"]]);
    // Apart, or with anything between them, it is `*` and a comparison.
    for form in ["a * = b", "a * <> b", "a */**/= b", "a *!= b", "a *== b"] {
        let e = refused(&mut s, &format!("SELECT {form} AS x FROM {on}"));
        assert!(e.contains("sql parser error"), "{form}: {e}");
    }
    assert!(refused(&mut s, &format!("SELECT *= 1 AS x FROM {on}")).contains("sql parser error"));
    // It binds as any other operator, ahead of a comparison.
    assert_eq!(
        rows(&mut s, &format!("SELECT true = a *= a AS x FROM {on}")),
        [["true"]]
    );
    assert_eq!(
        rows(&mut s, &format!("SELECT a *= a = true AS x FROM {on}")),
        [["true"]]
    );
}

#[test]
fn the_order_of_the_image_is_refused_by_name() {
    let mut s = Snail::open(&made(
        "order",
        "CREATE SCHEMA s; CREATE TYPE s.c AS (n numeric);\nCREATE TABLE s.t (id integer PRIMARY KEY, a s.c, b s.c);",
    ))
    .unwrap();
    for op in ["*<", "*<=", "*>", "*>="] {
        for sql in [
            format!("SELECT a {op} b AS x FROM s.t"),
            format!("SELECT id FROM s.t WHERE a {op} b"),
            format!("WITH q AS (SELECT a, b FROM s.t) SELECT a {op} b AS x FROM q"),
            format!("SELECT x {op} y AS z FROM (SELECT ROW(1) AS x, ROW(1) AS y) q"),
        ] {
            assert_eq!(
                refused(&mut s, &sql),
                format!(
                    "the operator {op} is not read: it orders records by PostgreSQL's in-memory \
                     layout of each type, which the snail does not keep"
                ),
                "{sql}"
            );
        }
    }
    let e = Catalog::parse(
        "CREATE SCHEMA s; CREATE TYPE s.c AS (n numeric);\nCREATE TABLE s.t (id integer PRIMARY KEY, a s.c);\n\
         CREATE INDEX i ON s.t (a record_image_ops);",
    )
    .err()
    .unwrap()
    .to_string();
    assert!(e.contains("record_image_ops is not read"), "{e}");
    assert!(e.contains("in-memory layout"), "{e}");
}

#[test]
fn before_any_some_and_all_over_an_array_it_is_asked_of_each_element() {
    let mut s = empty("quantified");
    let on = "(SELECT ROW(1.0) AS a, ROW(1.00) AS b) q";
    for (form, want) in [
        ("a *= ANY(ARRAY[b])", "false"),
        ("a *= ANY(ARRAY[b, a])", "true"),
        ("a *= SOME(ARRAY[a])", "true"),
        ("a *= ALL(ARRAY[b, a])", "false"),
        ("a *= ALL(ARRAY[a, a])", "true"),
        ("a *<> ANY(ARRAY[a, b])", "true"),
        ("a *<> ANY(ARRAY[a])", "false"),
        ("a *<> ALL(ARRAY[b])", "true"),
        ("a *<> ALL(ARRAY[a, b])", "false"),
        ("a *= ANY(ARRAY[b, NULL])", "NULL"),
        ("a *= ANY(ARRAY[a, NULL])", "true"),
        ("a *= ALL(ARRAY[a, NULL])", "NULL"),
        ("a *= ALL(ARRAY[b, NULL])", "false"),
        ("a *= ANY(NULL)", "NULL"),
        ("NULL *= ANY(ARRAY[a])", "NULL"),
        ("a *= ANY('{}')", "false"),
        ("a *= ALL('{}')", "true"),
    ] {
        assert_eq!(rows(&mut s, &format!("SELECT {form} AS x FROM {on}")), [[want]], "{form}");
    }
    for (sql, why) in [
        ("SELECT 1 *= ANY(ARRAY[1]) AS x", "operator does not exist: integer *= integer"),
        ("SELECT ROW(1) *= ANY(ARRAY[1]) AS x", "operator does not exist: record *= integer"),
        ("SELECT ROW(1) *= ANY(ROW(1)) AS x", "op ANY/ALL (array) requires array on right side"),
        ("SELECT ROW(1) *= ALL(1) AS x", "op ANY/ALL (array) requires array on right side"),
    ] {
        assert_eq!(refused(&mut s, sql), why, "{sql}");
    }
    assert!(refused(&mut s, "SELECT ROW(1) *< ANY(ARRAY[ROW(1)]) AS x").contains("in-memory layout"));
    assert!(refused(&mut s, "SELECT ROW(1) *= ANY(SELECT ROW(1)) AS x").contains("found: *="));
}

#[test]
fn a_row_constructor_compared_otherwise_stays_unread() {
    let mut s = empty("otherwise");
    for sql in [
        "SELECT ROW(1) < ROW(2) AS x",
        "SELECT ROW(1) BETWEEN ROW(0) AND ROW(2) AS x",
    ] {
        assert!(refused(&mut s, sql).ends_with("is not read"), "{sql}");
    }
    // By `=` and `<>`, and in a list of rows, their pairs' comparisons, as 18.6 answers them.
    for (sql, want) in [
        ("SELECT ROW(1, NULL) = ROW(1, NULL) AS x", "NULL"),
        ("SELECT (1, 2) <> (1, 3) AS x", "true"),
        ("SELECT (1, 2) IN ((1, 2)) AS x", "true"),
    ] {
        assert_eq!(rows(&mut s, sql), [[want]], "{sql}");
    }
    assert_eq!(rows(&mut s, "SELECT ROW(1, 2.50)::text AS x"), [["(1,2.50)"]]);
    assert_eq!(rows(&mut s, "SELECT (ROW(1, 2)).f2 AS x"), [["2"]]);
    assert_eq!(rows(&mut s, "SELECT (1, 'a')::text AS x"), [["(1,a)"]]);
}

#[test]
fn a_string_holding_the_operator_asks_nothing_of_it() {
    let catalog = Catalog::parse("CREATE SCHEMA s; CREATE TABLE s.t (id bigint PRIMARY KEY, n text);").unwrap();
    let path = tmp("image-string.snail");
    let n = 8192i64;
    write(
        &path,
        &catalog,
        &[(
            "s.t".into(),
            warren_snail::write::Rows {
                columns: vec![
                    warren_snail::values::Values::Int((0..n).map(Some).collect()),
                    warren_snail::values::Values::Text((0..n).map(|_| Some("a*=b".into())).collect()),
                ],
            },
        )],
    )
    .unwrap();
    let mut s = Snail::open(&path).unwrap();
    let pages = |s: &mut Snail, sql: &str| {
        let (b, round) = s.ask(sql, None).unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert_eq!(b.num_rows(), 1, "{sql}");
        round.column_pages_predicted
    };
    let along = pages(&mut s, "SELECT id FROM s.t WHERE id = 7 AND n = 'a*=b'");
    let whole = pages(
        &mut s,
        "WITH q AS (SELECT id, n FROM s.t) SELECT id FROM q WHERE id = 7 AND n = 'a*=b'",
    );
    assert!(along < whole, "{along} pages against {whole}");
}

#[cfg(feature = "standalone")]
mod written {
    use super::*;

    const DDL: &str = "CREATE SCHEMA s; CREATE TYPE s.c AS (n numeric);\n\
        CREATE TYPE s.g AS (f double precision, r real, i interval, h char(3), j jsonb);\n\
        CREATE TABLE s.t (id integer PRIMARY KEY, a s.c, b s.c, g s.g, h s.g);";

    fn snail(name: &str) -> Snail {
        let mut w = Snail::open_writing(&made(name, DDL)).unwrap();
        w.execute(
            "INSERT INTO s.t VALUES \
             (1, ROW(1.0)::s.c, ROW(1.00)::s.c, \
              ROW('-0'::float8, '-0'::float4, '1 day', 'a', '{\"a\":1.0}')::s.g, \
              ROW('0'::float8, '0'::float4, '24 hours', 'a  ', '{\"a\":1}')::s.g), \
             (2, ROW(1.0)::s.c, ROW(1.0)::s.c, \
              ROW('1'::float8, '1'::float4, '1 day', 'a', '1')::s.g, \
              ROW('1'::float8, '1'::float4, '1 day', 'a', '1')::s.g), \
             (3, NULL, ROW(1.0)::s.c, NULL, NULL), \
             (4, ROW(NULL)::s.c, ROW(NULL)::s.c, NULL, NULL)",
        )
        .unwrap();
        w
    }

    #[test]
    fn two_columns_of_one_composite_as_a_plain_question_a_where_and_a_step() {
        let mut s = snail("places");
        let all = [
            ["1", "false", "true"],
            ["2", "true", "false"],
            ["3", "NULL", "NULL"],
            ["4", "true", "false"],
        ];
        assert_eq!(
            rows(&mut s, "SELECT id, a *= b AS i, a *<> b AS n FROM s.t ORDER BY id"),
            all
        );
        assert_eq!(
            rows(
                &mut s,
                "WITH q AS (SELECT id, a, b FROM s.t) SELECT id, a *= b AS i, a *<> b AS n FROM q ORDER BY id"
            ),
            all
        );
        assert_eq!(
            rows(&mut s, "SELECT id FROM s.t WHERE a *= b ORDER BY id"),
            [["2"], ["4"]]
        );
        assert_eq!(rows(&mut s, "SELECT id FROM s.t WHERE a *<> b ORDER BY id"), [["1"]]);
        assert_eq!(
            rows(&mut s, "SELECT id FROM s.t WHERE NOT a *= b AND id < 3 ORDER BY id"),
            [["1"]]
        );
        assert_eq!(
            rows(
                &mut s,
                "WITH q AS (SELECT id FROM s.t WHERE a *<> b) SELECT id FROM q ORDER BY id"
            ),
            [["1"]]
        );
        assert_eq!(
            rows(&mut s, "WITH q AS (SELECT id, a, b FROM s.t WHERE id = 1) SELECT id, a = b AS e FROM q"),
            [["1", "true"]]
        );
        // The fields' own bytes, back from the file.
        assert_eq!(
            rows(&mut s, "WITH q AS (SELECT g, h FROM s.t WHERE id = 1) SELECT g *= h AS i, g = h AS e FROM q"),
            [["false", "true"]]
        );
        assert_eq!(
            rows(&mut s, "WITH q AS (SELECT g, h FROM s.t WHERE id = 2) SELECT g *= h AS i, g = h AS e FROM q"),
            [["true", "true"]]
        );
        // Beside a row constructor, the fields' types must agree.
        assert_eq!(
            rows(&mut s, "SELECT a *= ROW(1.0) AS i, ROW(1.0) *= b AS j FROM s.t WHERE id = 1"),
            [["true", "false"]]
        );
        assert_eq!(
            refused(&mut s, "SELECT a *= ROW(1) AS i FROM s.t WHERE id = 1"),
            "cannot compare dissimilar column types numeric and integer at record column 1"
        );
        // A row constructor standing alone, in a question of one table.
        assert_eq!(
            rows(&mut s, "SELECT (a, b)::text AS x FROM s.t WHERE id = 1"),
            [["(\"(1.0)\",\"(1.00)\")"]]
        );
        // Over two tables' columns.
        assert_eq!(
            rows(
                &mut s,
                "SELECT x.id, y.id AS o FROM s.t x JOIN s.t y ON x.a *<> y.b ORDER BY 1, 2"
            ),
            [
                ["1", "1"],
                ["1", "4"],
                ["2", "1"],
                ["2", "4"],
                ["4", "1"],
                ["4", "2"],
                ["4", "3"]
            ]
        );
    }

    #[test]
    fn a_literal_or_a_scalar_beside_a_column_is_refused_before_any_row() {
        let mut s = Snail::open(&made("before", DDL)).unwrap();
        assert_eq!(
            refused(&mut s, "SELECT id *= id AS x FROM s.t"),
            "operator does not exist: integer *= integer"
        );
        assert_eq!(
            refused(&mut s, "SELECT id FROM s.t WHERE id *= 1"),
            "operator does not exist: integer *= integer"
        );
        assert_eq!(
            refused(&mut s, "SELECT a *= 1 AS x FROM s.t"),
            "operator does not exist: s.c *= integer"
        );
        assert_eq!(
            refused(&mut s, "SELECT a *= '(1)' AS x FROM s.t"),
            "input of anonymous composite types is not implemented"
        );
        assert_eq!(
            refused(&mut s, "WITH q AS (SELECT id FROM s.t) SELECT id *= id AS x FROM q"),
            "operator does not exist: integer *= integer"
        );
    }

    #[test]
    fn the_writes_find_their_rows_by_the_image() {
        let mut w = snail("writes");
        w.execute("DELETE FROM s.t WHERE a *<> b").unwrap();
        assert_eq!(rows(&mut w, "SELECT id FROM s.t ORDER BY id"), [["2"], ["3"], ["4"]]);
        w.execute("UPDATE s.t SET id = 20 WHERE a *= b AND id = 2").unwrap();
        assert_eq!(rows(&mut w, "SELECT id FROM s.t ORDER BY id"), [["3"], ["4"], ["20"]]);
        let e = w.execute("DELETE FROM s.t WHERE a *< b").err().unwrap().to_string();
        assert!(e.contains("the operator *< is not read"), "{e}");
    }

    #[test]
    fn an_update_to_an_equal_value_of_other_bytes_writes_those_bytes() {
        let mut w = snail("equal");
        w.execute("UPDATE s.t SET a = b WHERE id = 1").unwrap();
        assert_eq!(
            rows(
                &mut w,
                "WITH q AS (SELECT id, a, b FROM s.t) SELECT a::text AS x, a *= b AS i FROM q WHERE id = 1"
            ),
            [["(1.00)", "true"]]
        );
        w.execute("UPDATE s.t SET g = h WHERE id = 1").unwrap();
        assert_eq!(
            rows(&mut w, "SELECT g *= h AS i FROM s.t WHERE id = 1"),
            [["true"]]
        );
    }

    #[test]
    fn two_snails_read_the_operator_across() {
        let (a, b) = (
            made("across-a", "CREATE SCHEMA s; CREATE TYPE s.c AS (n numeric);\nCREATE TABLE s.a (id integer PRIMARY KEY, v s.c);"),
            made("across-b", "CREATE SCHEMA s; CREATE TYPE s.c AS (n numeric);\nCREATE TABLE s.b (id integer PRIMARY KEY, v s.c);"),
        );
        Snail::open_writing(&a)
            .unwrap()
            .execute("INSERT INTO s.a VALUES (1, ROW(1.0)::s.c), (2, ROW(2.0)::s.c)")
            .unwrap();
        Snail::open_writing(&b)
            .unwrap()
            .execute("INSERT INTO s.b VALUES (1, ROW(1.00)::s.c), (2, ROW(2.0)::s.c)")
            .unwrap();
        let mut s = warren_snail::read::Snails::open(&[a.as_path(), b.as_path()]).unwrap();
        let (r, _) = s
            .ask("SELECT x.id FROM s.a x JOIN s.b y ON x.id = y.id WHERE x.v *= y.v")
            .unwrap();
        assert_eq!(r.num_rows(), 1);
        let (r, _) = s.ask("SELECT x.id FROM s.a x WHERE x.v *<> ROW(1.0)").unwrap();
        assert_eq!(r.num_rows(), 1);
        let (r, _) = s
            .ask("WITH q AS (SELECT id, v FROM s.b) SELECT id FROM q WHERE v *= ROW(2.0)")
            .unwrap();
        assert_eq!(r.num_rows(), 1);
    }

    #[test]
    fn a_check_and_a_view_read_the_operator() {
        let ddl = "CREATE SCHEMA s; CREATE TYPE s.c AS (n numeric);\n\
            CREATE TABLE s.t (id integer PRIMARY KEY, a s.c, b s.c, CONSTRAINT alike CHECK (a *= b));\n\
            CREATE VIEW s.v AS SELECT id, a *= b AS i FROM s.t;";
        let mut w = Snail::open_writing(&made("check", ddl)).unwrap();
        w.execute("INSERT INTO s.t VALUES (1, ROW(1.0)::s.c, ROW(1.0)::s.c)").unwrap();
        let e = w
            .execute("INSERT INTO s.t VALUES (2, ROW(1.0)::s.c, ROW(1.00)::s.c)")
            .err()
            .unwrap()
            .to_string();
        assert!(e.contains("violates check constraint \"alike\""), "{e}");
        assert_eq!(rows(&mut w, "SELECT id, i FROM s.v"), [["1", "true"]]);
    }
}

#[cfg(all(feature = "standalone", feature = "collations"))]
#[test]
fn text_under_a_collation_is_its_bytes() {
    let ddl = "CREATE SCHEMA s; CREATE COLLATION s.ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);\n\
        CREATE TYPE s.c AS (v text COLLATE s.ci);\n\
        CREATE TABLE s.t (id integer PRIMARY KEY, a s.c, b s.c);";
    let mut w = Snail::open_writing(&made("collated", ddl)).unwrap();
    w.execute("INSERT INTO s.t VALUES (1, ROW('a')::s.c, ROW('A')::s.c), (2, ROW('a')::s.c, ROW('a')::s.c)")
        .unwrap();
    assert_eq!(
        rows(&mut w, "SELECT id, a *= b AS i FROM s.t ORDER BY id"),
        [["1", "false"], ["2", "true"]]
    );
}
