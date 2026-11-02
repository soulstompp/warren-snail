//! `||`, `upper` and `lower` typed as PostgreSQL's parser types them: a literal beside a type with no
//! `||` of its own read as text, beside one with its own read in that type before any row; a string
//! beside anything but an array text's; an array's in the array of its sides' common type; no form,
//! or two PostgreSQL cannot choose between, refused in its words before any row. Every answer is
//! PostgreSQL 18.6's over the same rows.

mod weather;

use arrow::array::{Array, RecordBatch};
use arrow::util::display::array_value_to_string;
use warren_snail::catalog::Catalog;
use warren_snail::values::Values;
use warren_snail::write::Rows;
use weather::{fresh, tmp, write};

const DDL: &str = "CREATE TABLE c.t (id integer NOT NULL, b bigint, i integer, s text, \
                   v varchar(3), p char(3), m name, PRIMARY KEY (id));";

/// The table's snail: a row of values, and a row of NULLs.
fn snail(name: &str) -> std::path::PathBuf {
    let path = tmp(&format!("concat-{name}.snail"));
    let int = |v: &[Option<i64>]| Values::Int(v.to_vec());
    let text = |v: &str| Values::Text(vec![Some(v.to_string()), None]);
    write(
        &path,
        &Catalog::parse(DDL).unwrap(),
        &[(
            "c.t".into(),
            Rows {
                columns: vec![
                    int(&[Some(1), Some(2)]),
                    int(&[Some(10), None]),
                    int(&[Some(7), None]),
                    text("ab"),
                    text("xy"),
                    text("p  "),
                    text("nm"),
                ],
            },
        )],
    )
    .unwrap();
    path
}

/// An answer's rows, each value as text, a NULL as `-`, its columns joined by `|`.
fn rows(b: &RecordBatch) -> Vec<String> {
    (0..b.num_rows())
        .map(|r| {
            b.columns()
                .iter()
                .map(|c| match c.is_null(r) {
                    true => "-".to_string(),
                    false => array_value_to_string(c, r).unwrap(),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

fn same(path: &std::path::Path, q: &str, expected: &[&str]) {
    let (b, round) = fresh(path)
        .ask(q, None)
        .unwrap_or_else(|e| panic!("{q}: {e}"));
    assert_eq!(rows(&b), expected, "{q}");
    assert_eq!(
        round.column_pages_read,
        round.column_pages_predicted - round.column_pages_held,
        "{q}: {round:?}"
    );
}

fn refused(path: &std::path::Path, q: &str, words: &str) {
    match fresh(path).ask(q, None) {
        Ok((b, _)) => panic!("{q}: answered {:?}", rows(&b)),
        Err(e) => {
            let said = e.to_string();
            assert_eq!(said.lines().next(), Some(words), "{q}: {said}");
        }
    }
}

#[test]
fn a_literal_beside_a_column_with_no_concat_of_its_own_is_text() {
    let path = snail("literal");
    for (q, first) in [
        ("SELECT 'n=' || t.b AS e", "n=10"),
        ("SELECT t.b || '!' AS e", "10!"),
        ("SELECT 'n=' || t.i AS e", "n=7"),
        ("SELECT t.id || ' ' || t.b AS e", "1 10"),
        ("SELECT 'x' || (t.b + 1) AS e", "x11"),
        ("SELECT t.p || 'z ' AS e", "pz "),
        ("SELECT 'z ' || t.p AS e", "z p"),
        ("SELECT t.m || 'z' AS e", "nmz"),
    ] {
        same(
            &path,
            &format!("{q} FROM c.t t ORDER BY t.id"),
            &[first, "-"],
        );
    }
    // A question with nothing else about it, its condition and its grouping among them.
    same(
        &path,
        "SELECT t.b || 'x' AS e FROM c.t t WHERE t.id = 1",
        &["10x"],
    );
    same(
        &path,
        "SELECT t.s || 'x' AS e FROM c.t t WHERE t.id = 1",
        &["abx"],
    );
    same(
        &path,
        "SELECT t.id FROM c.t t WHERE t.b || 'x' = '10x'",
        &["1"],
    );
    same(
        &path,
        "SELECT t.id FROM c.t t WHERE t.s || t.b = 'ab10'",
        &["1"],
    );
    same(
        &path,
        "SELECT t.b || 'x' AS a, count(*) AS n FROM c.t t GROUP BY t.b || 'x' ORDER BY 1",
        &["10x|1", "-|1"],
    );
    same(
        &path,
        "SELECT t.s || 'x' AS a, count(*) AS n FROM c.t t GROUP BY t.s || 'x' ORDER BY 1",
        &["abx|1", "-|1"],
    );
    // A parameter is a literal, its type left to the operator.
    let (b, _) = fresh(&path)
        .ask_with(
            "SELECT $1 || t.b AS e FROM c.t t ORDER BY t.id",
            None,
            &["p".to_string()],
        )
        .unwrap();
    assert_eq!(rows(&b), ["p10", "-"]);
}

#[test]
fn a_string_beside_anything_but_an_array_is_text() {
    let path = snail("string");
    for (q, first) in [
        ("SELECT t.s || t.b AS e", "ab10"),
        ("SELECT t.b || t.s AS e", "10ab"),
        ("SELECT t.v || t.b AS e", "xy10"),
        ("SELECT t.v || t.p AS e", "xyp"),
        ("SELECT upper(t.v) || t.b AS e", "XY10"),
    ] {
        same(
            &path,
            &format!("{q} FROM c.t t ORDER BY t.id"),
            &[first, "-"],
        );
    }
    // Each value as its type writes it, literal, text or varchar beside it.
    for (value, its_text) in [
        ("1::int2", "1"),
        ("1.50::numeric", "1.50"),
        ("1.5::float4", "1.5"),
        ("1.5::float8", "1.5"),
        ("true", "true"),
        (
            "'00000000-0000-0000-0000-000000000001'::uuid",
            "00000000-0000-0000-0000-000000000001",
        ),
        ("'2020-01-02'::date", "2020-01-02"),
        ("'2020-01-02 03:04:05'::timestamp", "2020-01-02 03:04:05"),
        (
            "'2020-01-02 03:04:05+00'::timestamptz",
            "2020-01-02 03:04:05+00",
        ),
        ("'1 day'::interval", "1 day"),
        ("'03:04:05'::time", "03:04:05"),
        ("'03:04:05+01'::timetz", "03:04:05+01"),
        ("1::oid", "1"),
        ("1::money", "$1.00"),
        ("'1.2.3.4'::inet", "1.2.3.4/32"),
        ("'08:00:2b:01:02:03'::macaddr", "08:00:2b:01:02:03"),
        ("'{\"a\":1}'::json", "{\"a\":1}"),
        ("'(0,1)'::tid", "(0,1)"),
        ("int4range(1,3)", "[1,3)"),
        ("'(1,2)'::point", "(1,2)"),
    ] {
        same(
            &path,
            &format!("SELECT {value} || 'z' AS e"),
            &[&format!("{its_text}z")],
        );
        same(
            &path,
            &format!("SELECT 'z ' || {value} AS e"),
            &[&format!("z {its_text}")],
        );
        same(
            &path,
            &format!("SELECT 'z'::text || {value} AS e"),
            &[&format!("z{its_text}")],
        );
        same(
            &path,
            &format!("SELECT {value} || 'z'::varchar(3) AS e"),
            &[&format!("{its_text}z")],
        );
    }
    // A type's own form is passed over where a string stands beside it.
    same(
        &path,
        "SELECT 'z'::text || '{\"a\":1}'::jsonb AS a, '{\"a\":1}'::jsonb || 'z'::varchar(3) AS b, \
         'z'::text || 'a'::tsvector AS c, 'z'::text || B'101'::bit(3) AS d, \
         'x'::\"char\" || 1 AS e, 1 || 'x'::\"char\" AS f",
        &["z{\"a\": 1}|{\"a\": 1}z|z'a'|z101|x1|1x"],
    );
    // Over a NULL it is NULL, a type with no form of its own beside an untyped NULL among them.
    same(
        &path,
        "SELECT '(1,2)'::point || NULL AS a, NULL || '(1,2)'::point AS b, NULL::int8 || 'z' AS c",
        &["-|-|-"],
    );
}

#[test]
fn a_type_s_own_form_reads_a_literal_in_that_type() {
    let path = snail("own");
    same(
        &path,
        "SELECT ('\\x01'::bytea || 'z')::text AS a, (B'101'::bit(3) || '1')::text AS b, \
         ('{\"a\":1}'::jsonb || '{\"b\":2}')::text AS c, ('a'::tsvector || 'b')::text AS d, \
         ('a'::tsquery || 'b')::text AS e",
        &["\\x017a|1011|{\"a\": 1, \"b\": 2}|'a' 'b'|'a' | 'b'"],
    );
    // Before any row.
    refused(
        &path,
        "SELECT t.s::jsonb || 'z' FROM c.t t WHERE false",
        "invalid input syntax for type json",
    );
}

#[test]
fn an_array_s_concat_is_in_the_array_of_its_sides_common_type() {
    let path = snail("arrays");
    same(
        &path,
        "SELECT (ARRAY[1] || 1.5)::text AS a, (ARRAY[1] || 5000000000::int8)::text AS b, \
         (ARRAY[1] || ARRAY[2::int8])::text AS c, (ARRAY[1::int8] || ARRAY[2])::text AS d, \
         (ARRAY['a'::char(2)] || 'b'::text)::text AS e",
        &["{1,1.5}|{1,5000000000}|{1,2}|{1,2}|{\"a \",b}"],
    );
}

#[test]
fn the_types_are_those_postgresql_resolves() {
    let path = snail("types");
    for (e, ty) in [
        ("'a' || 'b'", "text"),
        ("NULL || NULL", "text"),
        ("1 || 'a'", "text"),
        ("'a'::varchar(3) || 'b'", "text"),
        ("'a'::char(3) || 'b'", "text"),
        ("'a'::name || 'b'", "text"),
        ("'\\x01'::bytea || 'z'", "bytea"),
        ("B'1'::bit(1) || '1'", "bit varying"),
        ("'{}'::jsonb || '{}'", "jsonb"),
        ("'a'::tsvector || 'b'", "tsvector"),
        ("'a'::tsquery || 'b'", "tsquery"),
        ("'\\x01'::bytea || '\\x02'::bytea", "bytea"),
        ("B'1'::bit(1) || B'0'::varbit", "bit varying"),
        ("'{}'::jsonb || '{}'::jsonb", "jsonb"),
        ("ARRAY[1] || 2", "integer[]"),
        ("ARRAY[1] || 2::int8", "bigint[]"),
        ("ARRAY[1] || 1.5", "numeric[]"),
        ("ARRAY[1] || ARRAY[2::int8]", "bigint[]"),
        ("'b'::varchar || ARRAY['a'::text]", "character varying[]"),
        ("upper('a')", "text"),
        ("upper('a'::varchar(3))", "text"),
        ("upper('a'::\"char\")", "text"),
        ("lower('a'::name)", "text"),
        ("upper(int4range(1,3))", "integer"),
    ] {
        same(&path, &format!("SELECT pg_typeof({e})::text AS t"), &[ty]);
    }
    same(
        &path,
        "SELECT pg_typeof(t.v || 'b')::text AS a, pg_typeof(t.b || 'b')::text AS b, \
         pg_typeof(upper(t.v))::text AS c, pg_typeof(t.s || t.v)::text AS d \
         FROM c.t t WHERE t.id = 1",
        &["text|text|text|text"],
    );
    // A set operation meets the type found.
    for q in [
        "SELECT 'a'::varchar(3) || 'b' UNION SELECT 1",
        "SELECT upper('a') UNION SELECT 1",
    ] {
        refused(&path, q, "UNION types text and integer cannot be matched");
    }
}

#[test]
fn upper_and_lower_read_a_string_and_a_range_s_bound() {
    let path = snail("upper");
    same(
        &path,
        "SELECT upper(t.s) AS a, lower(t.m) AS b, upper(t.v) AS c, upper(t.p) AS d \
         FROM c.t t ORDER BY t.id",
        &["AB|nm|XY|P", "-|-|-|-"],
    );
    same(
        &path,
        "SELECT upper('a'::\"char\") AS a, lower('A'::\"char\") AS b, upper(NULL) AS c, \
         lower('Ab') AS d, upper(int4range(1,3)) AS e",
        &["A|a|-|ab|3"],
    );
    for (q, words) in [
        (
            "SELECT upper(t.b) FROM c.t t WHERE false",
            "function upper(bigint) does not exist",
        ),
        (
            "SELECT lower(t.i) FROM c.t t WHERE false",
            "function lower(integer) does not exist",
        ),
        ("SELECT upper(1)", "function upper(integer) does not exist"),
        (
            "SELECT lower(true)",
            "function lower(boolean) does not exist",
        ),
        ("SELECT upper(1::oid)", "function upper(oid) does not exist"),
        (
            "SELECT upper('{}'::jsonb)",
            "function upper(jsonb) does not exist",
        ),
        (
            "SELECT upper(ARRAY[1])",
            "function upper(integer[]) does not exist",
        ),
    ] {
        refused(&path, q, words);
    }
}

#[test]
fn where_no_form_takes_them_or_two_do_the_refusal_is_postgresql_s_before_any_row() {
    let path = snail("refused");
    for (q, words) in [
        (
            "SELECT t.b || t.b FROM c.t t WHERE false",
            "operator does not exist: bigint || bigint",
        ),
        (
            "SELECT t.i || t.b FROM c.t t WHERE false",
            "operator does not exist: integer || bigint",
        ),
        (
            "SELECT 1 || 1.5",
            "operator does not exist: integer || numeric",
        ),
        (
            "SELECT true || 1",
            "operator does not exist: boolean || integer",
        ),
        (
            "SELECT '\\x01'::bytea || '{}'::jsonb",
            "operator does not exist: bytea || jsonb",
        ),
        (
            "SELECT '{}'::jsonb || 'a'::tsvector",
            "operator does not exist: jsonb || tsvector",
        ),
        (
            "SELECT 'a'::tsvector || 'a'::tsquery",
            "operator does not exist: tsvector || tsquery",
        ),
        (
            "SELECT B'1'::bit(1) || '\\x01'::bytea",
            "operator does not exist: bit || bytea",
        ),
        (
            "SELECT 'z'::text || ARRAY[1,2]",
            "operator does not exist: text || integer[]",
        ),
        (
            "SELECT ARRAY[1,2] || 'z'::varchar(3)",
            "operator does not exist: integer[] || character varying",
        ),
        (
            "SELECT ARRAY[1] || ARRAY['a']",
            "operator does not exist: integer[] || text[]",
        ),
        (
            "SELECT ARRAY[true] || 1",
            "operator does not exist: boolean[] || integer",
        ),
        (
            "SELECT 'x'::\"char\" || ARRAY['a']",
            "operator does not exist: \"char\" || text[]",
        ),
        (
            "SELECT 'x'::\"char\" || 'z'",
            "operator is not unique: \"char\" || unknown",
        ),
        (
            "SELECT 'z' || 'x'::\"char\"",
            "operator is not unique: unknown || \"char\"",
        ),
        (
            "SELECT 'z'::text || 'x'::\"char\"",
            "operator is not unique: text || \"char\"",
        ),
        (
            "SELECT 'x'::\"char\" || 'z'::varchar(3)",
            "operator is not unique: \"char\" || character varying",
        ),
        (
            "SELECT 'x'::\"char\" || 'x'::\"char\"",
            "operator is not unique: \"char\" || \"char\"",
        ),
    ] {
        refused(&path, q, words);
    }
}
