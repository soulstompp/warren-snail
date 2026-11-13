//! The reader's DISTINCT ON, answered as PostgreSQL 18.6 answers it: the first row of each run of
//! rows its keys tie in, sorted by the ORDER BY, whose items before its first other than a key hold
//! every key; each key named by its place, its output name or its expression; its keys equal by
//! their types' `=` under their collations; over a join, after grouping and windows, before LIMIT
//! and OFFSET, inside a subquery, a WITH and an IN list; a sequence drawn only for the rows it
//! keeps; and SELECT ALL, every row. Every answer and refusal is 18.6's over the same statements
//! (`session.nu --suite reader`, case 12c), but where a test says it is the snail's own. A snail
//! writes on its own only with `standalone`, as `tests/ddl.rs`.
#![cfg(feature = "standalone")]

mod weather;

use warren_snail::catalog::Catalog;
use warren_snail::read::Snail;
use warren_snail::room::Sizes;
use warren_snail::values::Values;
use warren_snail::write::Rows;
use weather::{tmp, write};

const DDL: &str = "CREATE SCHEMA dn;
     CREATE TABLE dn.a (id integer NOT NULL, g integer, k text, name text, PRIMARY KEY (id));
     CREATE TABLE dn.b (id integer NOT NULL, a_id integer NOT NULL, v integer, PRIMARY KEY (id));
     CREATE TABLE dn.n (id integer NOT NULL, x numeric, f float8, PRIMARY KEY (id));";

const ROWS: [&str; 3] = [
    "INSERT INTO dn.a VALUES (1, 10, 'x', 'one'), (2, 20, 'y', 'two'), (3, 10, 'x', 'three'), \
     (4, 20, 'x', 'four'), (5, 30, 'y', 'five'), (6, NULL, NULL, 'six'), (7, NULL, NULL, 'seven')",
    "INSERT INTO dn.b VALUES (1, 1, 100), (2, 1, 50), (3, 2, NULL), (4, 3, 300), (5, 5, 500), \
     (6, 5, 600)",
    "INSERT INTO dn.n VALUES (1, 1.00, 0), (2, 1.0, '-0'), (3, 2.5, 'NaN'), (4, 2.50, 'NaN'), \
     (5, NULL, NULL), (6, NULL, 1)",
];

/// A snail holding the case's rows, at a path of the test's name.
fn snail(name: &str) -> Snail {
    let path = tmp(&format!("reader-{name}.snail"));
    write(&path, &Catalog::parse(DDL).unwrap(), &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    for sql in ROWS {
        w.execute(sql).unwrap();
    }
    w
}

/// A statement's refusal: its SQLSTATE and its words.
fn refused(w: &mut Snail, sql: &str) -> (Option<&'static str>, String) {
    let e = w
        .execute(sql)
        .err()
        .unwrap_or_else(|| panic!("{sql} was taken"));
    (e.sqlstate(), e.to_string())
}

/// An answer's rows, each value as PostgreSQL's output function writes it, `|` between them.
fn rows(w: &mut Snail, sql: &str) -> Vec<String> {
    let done = w.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    let b = done.answer.expect("an answer");
    w.text_of(&b)
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

/// Each statement answered as 18.6 answers it, in its order.
fn answered(w: &mut Snail, asked: &[(&str, &[&str])]) {
    for (sql, want) in asked {
        assert_eq!(rows(w, sql), *want, "{sql}");
    }
}

#[test]
fn distinct_on_keeps_each_group_s_first_row_in_the_order_by_s_order() {
    let mut w = snail("first");
    answered(
        &mut w,
        &[
            (
                "SELECT DISTINCT ON (g) g, id FROM dn.a ORDER BY g, id",
                &["10|1", "20|2", "30|5", "NULL|6"],
            ),
            (
                "SELECT DISTINCT ON (g) g, id FROM dn.a ORDER BY g, id DESC",
                &["10|3", "20|4", "30|5", "NULL|7"],
            ),
            (
                "SELECT DISTINCT ON (g) g, name FROM dn.a ORDER BY g DESC, name",
                &["NULL|seven", "30|five", "20|four", "10|one"],
            ),
            (
                "SELECT DISTINCT ON (g) g, id FROM dn.a ORDER BY g NULLS FIRST, id DESC",
                &["NULL|7", "10|3", "20|4", "30|5"],
            ),
            (
                "SELECT DISTINCT ON (g) * FROM dn.a ORDER BY g, id DESC",
                &[
                    "3|10|x|three",
                    "4|20|x|four",
                    "5|30|y|five",
                    "7|NULL|NULL|seven",
                ],
            ),
            (
                "SELECT DISTINCT ON (g) g FROM dn.a ORDER BY g",
                &["10", "20", "30", "NULL"],
            ),
            (
                "SELECT DISTINCT ON (g) g FROM dn.a ORDER BY g DESC",
                &["NULL", "30", "20", "10"],
            ),
            (
                "SELECT DISTINCT ON (g) id FROM dn.a ORDER BY g DESC NULLS LAST, id",
                &["5", "2", "1", "6"],
            ),
        ],
    );
}

#[test]
fn distinct_on_keys_lead_the_order_by_in_any_order() {
    let mut w = snail("lead");
    answered(
        &mut w,
        &[
            (
                "SELECT DISTINCT ON (g, k) g, k, id FROM dn.a ORDER BY g, k, id",
                &["10|x|1", "20|x|4", "20|y|2", "30|y|5", "NULL|NULL|6"],
            ),
            (
                "SELECT DISTINCT ON (g, k) g, k, id FROM dn.a ORDER BY k, g, id DESC",
                &["10|x|3", "20|x|4", "20|y|2", "30|y|5", "NULL|NULL|7"],
            ),
            (
                "SELECT DISTINCT ON (k, g) g, k, name FROM dn.a ORDER BY g DESC, k, name",
                &[
                    "NULL|NULL|seven",
                    "30|y|five",
                    "20|x|four",
                    "20|y|two",
                    "10|x|one",
                ],
            ),
            // The key the ORDER BY leaves out sorts after it, ascending.
            (
                "SELECT DISTINCT ON (g, k) g, k FROM dn.a ORDER BY k DESC",
                &["NULL|NULL", "20|y", "30|y", "10|x", "20|x"],
            ),
            // A key the ORDER BY sorts by again later is passed over there.
            (
                "SELECT DISTINCT ON (g) g, id FROM dn.a ORDER BY g, id, g DESC",
                &["10|1", "20|2", "30|5", "NULL|6"],
            ),
            (
                "SELECT DISTINCT ON (g, g) g, id FROM dn.a ORDER BY g, id",
                &["10|1", "20|2", "30|5", "NULL|6"],
            ),
            (
                "SELECT DISTINCT ON (g) g FROM dn.a ORDER BY g, g DESC, id",
                &["10", "20", "30", "NULL"],
            ),
            (
                "SELECT DISTINCT ON (g, upper(k)) g FROM dn.a ORDER BY g",
                &["10", "20", "20", "30", "NULL"],
            ),
        ],
    );
}

#[test]
fn distinct_on_names_a_key_by_place_name_or_expression() {
    let mut w = snail("named");
    answered(
        &mut w,
        &[
            // A key the select list does not return, sorted by the ORDER BY beside it.
            (
                "SELECT DISTINCT ON (g) id FROM dn.a ORDER BY g, id",
                &["1", "2", "5", "6"],
            ),
            (
                "SELECT DISTINCT ON (1) g, id FROM dn.a ORDER BY 1, 2 DESC",
                &["10|3", "20|4", "30|5", "NULL|7"],
            ),
            (
                "SELECT DISTINCT ON (grp) g AS grp, id FROM dn.a ORDER BY grp, id",
                &["10|1", "20|2", "30|5", "NULL|6"],
            ),
            (
                "SELECT DISTINCT ON (id % 3) id % 3 AS r, id FROM dn.a ORDER BY id % 3, id DESC",
                &["0|6", "1|7", "2|5"],
            ),
            (
                "SELECT DISTINCT ON (length(name)) length(name) AS l, name FROM dn.a \
                 ORDER BY length(name), name",
                &["3|one", "4|five", "5|seven"],
            ),
            (
                "SELECT DISTINCT ON (coalesce(g, 0)) coalesce(g, 0) AS g0, name FROM dn.a \
                 ORDER BY coalesce(g, 0) DESC, name DESC",
                &["30|five", "20|two", "10|three", "0|six"],
            ),
            // A column named under its relation's name or not, in parentheses, cast to its own
            // type, or a function's name in any case, is one key.
            (
                "SELECT DISTINCT ON (g) a.id FROM dn.a a ORDER BY a.g, a.id",
                &["1", "2", "5", "6"],
            ),
            (
                "SELECT DISTINCT ON (a.g) id FROM dn.a a ORDER BY g, id",
                &["1", "2", "5", "6"],
            ),
            (
                "SELECT DISTINCT ON ((g)) g, id FROM dn.a ORDER BY g, id",
                &["10|1", "20|2", "30|5", "NULL|6"],
            ),
            (
                "SELECT DISTINCT ON (g::integer) g, id FROM dn.a ORDER BY g, id",
                &["10|1", "20|2", "30|5", "NULL|6"],
            ),
            (
                "SELECT DISTINCT ON (g) g, id FROM dn.a ORDER BY g::integer, id",
                &["10|1", "20|2", "30|5", "NULL|6"],
            ),
            (
                "SELECT DISTINCT ON (LENGTH(name)) length(name) AS l, name FROM dn.a \
                 ORDER BY length(name), name",
                &["3|one", "4|five", "5|seven"],
            ),
            // An output name is read before a column of the FROM.
            (
                "SELECT DISTINCT ON (g) g AS id, id AS g FROM dn.a ORDER BY g",
                &["10|1", "20|2", "10|3", "20|4", "30|5", "NULL|6", "NULL|7"],
            ),
            (
                "SELECT DISTINCT ON (upper(k)) id FROM dn.a ORDER BY upper(k), id DESC",
                &["4", "5", "7"],
            ),
            (
                "SELECT DISTINCT g FROM dn.a a ORDER BY a.g",
                &["10", "20", "30", "NULL"],
            ),
            (
                "SELECT ALL g FROM dn.a ORDER BY g",
                &["10", "10", "20", "20", "30", "NULL", "NULL"],
            ),
            ("SELECT count(*) FROM (SELECT ALL g FROM dn.a) d", &["7"]),
            (
                "SELECT ALL g FROM (SELECT g FROM dn.a) d ORDER BY g",
                &["10", "10", "20", "20", "30", "NULL", "NULL"],
            ),
        ],
    );
}

#[test]
fn distinct_on_refuses_in_18_6_s_words() {
    let mut w = snail("refused");
    let unmatched = || {
        (
            Some("42P10"),
            "SELECT DISTINCT ON expressions must match initial ORDER BY expressions".to_string(),
        )
    };
    for sql in [
        "SELECT DISTINCT ON (g) g, id FROM dn.a ORDER BY id",
        "SELECT DISTINCT ON (g, k) g, k, id FROM dn.a ORDER BY g, id",
        "SELECT DISTINCT ON (g) g, id FROM dn.a ORDER BY g + 0, id",
        "SELECT DISTINCT ON (g, k) g, k, id FROM dn.a ORDER BY k DESC, id",
        "SELECT DISTINCT ON (k) k, id FROM dn.a ORDER BY k COLLATE \"C\", id",
        "SELECT DISTINCT ON (g) g, id FROM dn.a ORDER BY id, g",
    ] {
        assert_eq!(refused(&mut w, sql), unmatched(), "{sql}");
    }
    let asked: &[(&str, &str, &str)] = &[
        (
            "SELECT DISTINCT ON (5) g, id FROM dn.a ORDER BY g",
            "42P10",
            "DISTINCT ON position 5 is not in select list",
        ),
        (
            "SELECT DISTINCT ON (0) g FROM dn.a",
            "42P10",
            "DISTINCT ON position 0 is not in select list",
        ),
        (
            "SELECT DISTINCT ON ('a') g FROM dn.a",
            "42601",
            "non-integer constant in DISTINCT ON",
        ),
        (
            "SELECT DISTINCT ON (x) g AS x, id AS x FROM dn.a",
            "42702",
            "DISTINCT ON \"x\" is ambiguous",
        ),
        (
            "SELECT DISTINCT ON (k::json) k FROM dn.a",
            "42883",
            "could not identify an equality operator for type json",
        ),
        (
            "SELECT DISTINCT ON (g) g FROM dn.a ORDER BY g FOR UPDATE",
            "0A000",
            "FOR UPDATE is not allowed with DISTINCT clause",
        ),
    ];
    for (sql, code, words) in asked {
        assert_eq!(
            refused(&mut w, sql),
            (Some(*code), weather::hinted(words)),
            "{sql}"
        );
    }
}

// A COLLATE over a type no collation orders is refused as 18.6 refuses it while typing the
// expression: before DISTINCT ON's words and before the collation's name is looked up, the type
// named as format_type_be names it, a domain by its own name.
#[test]
fn a_collation_on_an_integer_is_refused_before_distinct_on_s_words() {
    let mut w = snail("collate-integer");
    let refusals: &[(&str, &str)] = &[
        ("SELECT DISTINCT ON (g) g, id FROM dn.a ORDER BY g COLLATE \"C\"", "integer"),
        ("SELECT DISTINCT ON (g) g, id FROM dn.a ORDER BY id COLLATE \"C\"", "integer"),
        ("SELECT g FROM dn.a ORDER BY g COLLATE \"C\"", "integer"),
        ("SELECT g COLLATE \"C\" FROM dn.a", "integer"),
        ("SELECT id FROM dn.a WHERE g COLLATE \"C\" = 1", "integer"),
        ("SELECT count(*) FROM dn.a GROUP BY g COLLATE \"C\"", "integer"),
        ("SELECT id FROM dn.a ORDER BY (g + 1) COLLATE \"C\"", "integer"),
        ("SELECT ARRAY[g] COLLATE \"C\" FROM dn.a", "integer[]"),
        ("SELECT x FROM dn.n ORDER BY x COLLATE \"C\"", "numeric"),
        ("SELECT 1 COLLATE \"C\"", "integer"),
        ("SELECT 1 COLLATE \"nosuch\"", "integer"),
        ("SELECT id FROM dn.a ORDER BY id COLLATE \"nosuch\"", "integer"),
        ("SELECT true COLLATE \"C\"", "boolean"),
        ("SELECT ROW(1, 2) COLLATE \"C\"", "record"),
        ("SELECT sum(id) COLLATE \"C\" FROM dn.a", "bigint"),
        ("SELECT id FROM dn.a LIMIT 1 COLLATE \"C\"", "integer"),
    ];
    for (sql, ty) in refusals {
        assert_eq!(
            refused(&mut w, sql),
            (Some("42804"), format!("collations are not supported by type {ty}")),
            "{sql}"
        );
    }
    answered(
        &mut w,
        &[
            ("SELECT '1' COLLATE \"C\" AS x", &["1"]),
            ("SELECT NULL COLLATE \"C\" AS x", &["NULL"]),
            (
                "SELECT k COLLATE \"C\" AS x FROM dn.a WHERE k IS NOT NULL ORDER BY 1",
                &["x", "x", "x", "y", "y"],
            ),
        ],
    );
    let path = tmp("reader-collate-types.snail");
    let ddl = "CREATE SCHEMA cz; CREATE DOMAIN cz.d AS integer; \
               CREATE TABLE cz.t (id integer NOT NULL, d cz.d, m numeric(10,2), ts timestamp(3), \
               b bit(3), vb bit varying(5), PRIMARY KEY (id));";
    write(&path, &Catalog::parse(ddl).unwrap(), &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    for (sql, ty) in [
        ("SELECT d COLLATE \"C\" FROM cz.t", "cz.d"),
        ("SELECT m COLLATE \"C\" FROM cz.t", "numeric"),
        ("SELECT ARRAY[m] COLLATE \"C\" FROM cz.t", "numeric[]"),
        ("SELECT ts COLLATE \"C\" FROM cz.t", "timestamp without time zone"),
        ("SELECT b COLLATE \"C\" FROM cz.t", "bit"),
        ("SELECT vb COLLATE \"C\" FROM cz.t", "bit varying"),
    ] {
        assert_eq!(
            refused(&mut w, sql),
            (Some("42804"), format!("collations are not supported by type {ty}")),
            "{sql}"
        );
    }
}

// A COLLATE after an operand that binds tighter, a cast or a subscript, is read as 18.6 reads it:
// answered, or refused 42804 by the cast's type; in a view too, printed as 18.6 prints it.
#[test]
fn a_collate_after_a_cast_is_read_as_postgresql_reads_it() {
    let mut w = snail("collate-cast");
    answered(
        &mut w,
        &[
            ("SELECT id::text COLLATE \"C\" AS x FROM dn.a WHERE id < 4 ORDER BY 1", &["1", "2", "3"]),
            (
                "SELECT lower(name::text COLLATE \"C\") AS x FROM dn.a WHERE id < 3 ORDER BY 1",
                &["one", "two"],
            ),
            (
                "SELECT name FROM dn.a ORDER BY name::varchar COLLATE \"C\" DESC LIMIT 3",
                &["two", "three", "six"],
            ),
            (
                "SELECT name FROM dn.a WHERE name::text COLLATE \"C\" < 'o' ORDER BY 1",
                &["five", "four"],
            ),
            ("SELECT 'a' || name::text COLLATE \"C\" AS x FROM dn.a WHERE id = 1", &["aone"]),
            ("SELECT (ARRAY[k, name])[2] COLLATE \"C\" AS x FROM dn.a WHERE id = 1", &["one"]),
            (
                "SELECT pg_collation_for(name::text COLLATE \"POSIX\") AS x FROM dn.a WHERE id = 1",
                &["\"POSIX\""],
            ),
        ],
    );
    for (sql, ty) in [
        ("SELECT NULL::integer COLLATE \"C\"", "integer"),
        ("SELECT g::integer COLLATE \"C\" FROM dn.a", "integer"),
        ("SELECT g::numeric COLLATE \"C\" FROM dn.a", "numeric"),
    ] {
        assert_eq!(
            refused(&mut w, sql),
            (Some("42804"), format!("collations are not supported by type {ty}")),
            "{sql}"
        );
    }
    for (name, view, collation) in [
        ("v1", "SELECT k COLLATE \"C\" AS x FROM dn.a", "\"C\""),
        ("v2", "SELECT k::text COLLATE \"C\" AS x FROM dn.a", "\"C\""),
        ("v3", "SELECT k::text COLLATE \"POSIX\" AS x FROM dn.a", "\"POSIX\""),
    ] {
        w.execute(&format!("CREATE VIEW dn.{name} AS {view}"))
            .unwrap_or_else(|e| panic!("{view}: {e}"));
        assert_eq!(
            rows(&mut w, &format!("SELECT pg_get_viewdef('dn.{name}'::regclass)")),
            [format!(" SELECT (k COLLATE {collation}) AS x\n   FROM dn.a;")],
            "{view}"
        );
    }
}

#[test]
fn distinct_on_groups_nulls_and_equal_values_as_one() {
    // numeric 1.00 and 1.0 are one key, each row printing its own; float8 0 and -0 one, NaN one.
    let mut w = snail("equal");
    answered(
        &mut w,
        &[
            (
                "SELECT DISTINCT ON (x) x, id FROM dn.n ORDER BY x, id DESC",
                &["1.0|2", "2.50|4", "NULL|6"],
            ),
            (
                "SELECT DISTINCT ON (x) x, id FROM dn.n ORDER BY x DESC, id",
                &["NULL|5", "2.5|3", "1.00|1"],
            ),
            (
                "SELECT DISTINCT ON (x) x, id FROM dn.n ORDER BY x NULLS FIRST, id",
                &["NULL|5", "1.00|1", "2.5|3"],
            ),
            (
                "SELECT DISTINCT ON (x) x, id FROM dn.n ORDER BY x USING >, id",
                &["NULL|5", "2.5|3", "1.00|1"],
            ),
            (
                "SELECT DISTINCT ON (f) f, id FROM dn.n ORDER BY f, id",
                &["0|1", "1|6", "NaN|3", "NULL|5"],
            ),
            (
                "SELECT DISTINCT ON (f) f, id FROM dn.n ORDER BY f, id DESC",
                &["-0|2", "1|6", "NaN|4", "NULL|5"],
            ),
        ],
    );
}

#[test]
fn distinct_on_is_cut_by_limit_and_offset_after_its_groups() {
    let mut w = snail("cut");
    answered(
        &mut w,
        &[
            (
                "SELECT DISTINCT ON (g) g, id FROM dn.a ORDER BY g, id LIMIT 2",
                &["10|1", "20|2"],
            ),
            (
                "SELECT DISTINCT ON (g) g, id FROM dn.a ORDER BY g, id LIMIT 2 OFFSET 1",
                &["20|2", "30|5"],
            ),
            (
                "SELECT DISTINCT ON (g) g, id FROM dn.a ORDER BY g, id OFFSET 3",
                &["NULL|6"],
            ),
            (
                "SELECT DISTINCT ON (g) g, id FROM dn.a ORDER BY g, id LIMIT 0",
                &[],
            ),
            (
                "SELECT DISTINCT ON (g) g, id FROM dn.a ORDER BY g, id FETCH FIRST 1 ROW WITH TIES",
                &["10|1"],
            ),
            (
                "SELECT DISTINCT ON (g) g, id FROM dn.a ORDER BY g, id NULLS FIRST LIMIT 1 \
                 OFFSET 1",
                &["20|2"],
            ),
        ],
    );
}

#[test]
fn distinct_on_over_a_join() {
    let mut w = snail("join");
    answered(
        &mut w,
        &[
            (
                "SELECT DISTINCT ON (a.id) a.id, a.name, b.v FROM dn.a a JOIN dn.b b \
                 ON b.a_id = a.id ORDER BY a.id, b.v DESC NULLS LAST",
                &["1|one|100", "2|two|NULL", "3|three|300", "5|five|600"],
            ),
            (
                "SELECT DISTINCT ON (a.g) a.g, b.v FROM dn.a a LEFT JOIN dn.b b ON b.a_id = a.id \
                 ORDER BY a.g, b.v",
                &["10|50", "20|NULL", "30|500", "NULL|NULL"],
            ),
            (
                "SELECT DISTINCT ON (b.a_id) b.a_id, a.name, b.v FROM dn.b b JOIN dn.a a \
                 ON a.id = b.a_id ORDER BY b.a_id, b.v NULLS FIRST",
                &["1|one|50", "2|two|NULL", "3|three|300", "5|five|500"],
            ),
            (
                "SELECT DISTINCT ON (a.g, a.k) a.g, a.k, b.v FROM dn.a a LEFT JOIN dn.b b \
                 ON b.a_id = a.id ORDER BY a.g, a.k, b.v DESC NULLS LAST",
                &[
                    "10|x|300",
                    "20|x|NULL",
                    "20|y|NULL",
                    "30|y|600",
                    "NULL|NULL|NULL",
                ],
            ),
            // A key under its relation's name and the ORDER BY's bare name are one column.
            (
                "SELECT DISTINCT ON (b.a_id) a.name, b.v FROM dn.b b JOIN dn.a a \
                 ON a.id = b.a_id ORDER BY a_id, v DESC",
                &["one|100", "two|NULL", "three|300", "five|600"],
            ),
        ],
    );
}

#[test]
fn distinct_on_after_grouping_and_windows() {
    let mut w = snail("grouped");
    answered(
        &mut w,
        &[
            (
                "SELECT DISTINCT ON (g) g, count(*) AS n FROM dn.a GROUP BY g, k \
                 ORDER BY g, count(*) DESC",
                &["10|2", "20|1", "30|1", "NULL|2"],
            ),
            (
                "SELECT DISTINCT ON (g) g, row_number() OVER (ORDER BY id) AS r FROM dn.a \
                 ORDER BY g, r DESC",
                &["10|3", "20|4", "30|5", "NULL|7"],
            ),
            (
                "SELECT DISTINCT ON (count(*)) count(*), g FROM dn.a GROUP BY g \
                 ORDER BY count(*), g",
                &["1|30", "2|10"],
            ),
            (
                "SELECT DISTINCT ON (g) g, count(*) FROM dn.a GROUP BY g, k HAVING count(*) > 0 \
                 ORDER BY g, count(*) DESC",
                &["10|2", "20|1", "30|1", "NULL|2"],
            ),
            // A key of its own, an aggregate, folded with the group.
            (
                "SELECT DISTINCT ON (max(id)) g FROM dn.a GROUP BY g",
                &["10", "20", "30", "NULL"],
            ),
        ],
    );
}

#[test]
fn distinct_on_inside_a_subquery_a_with_and_an_in_list() {
    let mut w = snail("inside");
    answered(
        &mut w,
        &[
            (
                "SELECT count(*) FROM (SELECT DISTINCT ON (g) g FROM dn.a) d",
                &["4"],
            ),
            (
                "SELECT name FROM (SELECT DISTINCT ON (g) g, name FROM dn.a ORDER BY g, name) d \
                 ORDER BY name",
                &["five", "four", "one", "seven"],
            ),
            (
                "WITH d AS (SELECT DISTINCT ON (g) g, name FROM dn.a ORDER BY g, name DESC) \
                 SELECT name FROM d ORDER BY name",
                &["five", "six", "three", "two"],
            ),
            (
                "WITH d AS (SELECT DISTINCT ON (g) g FROM dn.a) SELECT count(*) FROM d",
                &["4"],
            ),
            (
                "SELECT name FROM dn.a WHERE id IN (SELECT DISTINCT ON (g) id FROM dn.a \
                 ORDER BY g, id DESC) ORDER BY name",
                &["five", "four", "seven", "three"],
            ),
            (
                "SELECT DISTINCT ON (g) g, id FROM dn.a WHERE g = 10 ORDER BY g, id DESC",
                &["10|3"],
            ),
            (
                "SELECT DISTINCT ON (g) g, id FROM dn.a WHERE false ORDER BY g, id",
                &[],
            ),
            // With no ORDER BY, the rows sort by the key.
            (
                "SELECT DISTINCT ON (g) g FROM dn.a",
                &["10", "20", "30", "NULL"],
            ),
        ],
    );
}

#[test]
fn distinct_on_draws_a_sequence_only_for_its_kept_rows() {
    let mut w = snail("drawn");
    w.execute("CREATE SEQUENCE dn.s").unwrap();
    answered(
        &mut w,
        &[
            (
                "SELECT DISTINCT ON (g) g, nextval('dn.s') FROM dn.a ORDER BY g, id",
                &["10|1", "20|2", "30|3", "NULL|4"],
            ),
            ("SELECT nextval('dn.s')", &["5"]),
            // A set an ORDER BY item makes is made before the rows are kept.
            (
                "SELECT DISTINCT ON (g) g, unnest(ARRAY[1, 2]) AS u FROM dn.a ORDER BY g, u",
                &["10|1", "20|1", "30|1", "NULL|1"],
            ),
            (
                "SELECT DISTINCT ON (u) unnest(ARRAY[2, 1, 2]) AS u FROM dn.a ORDER BY u",
                &["1", "2"],
            ),
        ],
    );
}

#[cfg(feature = "collations")]
#[test]
fn distinct_on_groups_by_the_key_s_collation() {
    let mut w = snail("collated");
    for sql in [
        "CREATE COLLATION dn.ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false)",
        "CREATE TABLE dn.c (id integer NOT NULL, t text COLLATE dn.ci, PRIMARY KEY (id))",
        "INSERT INTO dn.c VALUES (1, 'x'), (2, 'X'), (3, 'y'), (4, 'Y')",
    ] {
        w.execute(sql).unwrap();
    }
    answered(
        &mut w,
        &[
            (
                "SELECT DISTINCT ON (t) t, id FROM dn.c ORDER BY t, id DESC",
                &["X|2", "Y|4"],
            ),
            (
                "SELECT DISTINCT ON (t COLLATE \"C\") t, id FROM dn.c ORDER BY t COLLATE \"C\", id",
                &["X|2", "Y|4", "x|1", "y|3"],
            ),
        ],
    );
}

/// The snail's own answer, not 18.6's: where the ORDER BY leaves rows tied, 18.6 keeps the one its
/// plan's sort puts first and promises none. The snail keeps the first to come, in any room.
#[test]
fn distinct_on_keeps_the_first_row_to_come_where_the_order_by_leaves_a_tie() {
    let mut w = snail("ties");
    answered(
        &mut w,
        &[(
            "SELECT DISTINCT ON (g) g, id FROM dn.a",
            &["10|1", "20|2", "30|5", "NULL|6"],
        )],
    );
    // Past the room: within two rooms, and in runs.
    let path = tmp("reader-ties-past.snail");
    let n = 20_000;
    let catalog = Catalog::parse(
        "CREATE SCHEMA s; CREATE TABLE s.t (id integer NOT NULL, g integer, PRIMARY KEY (id));
         CREATE TABLE s.o (k integer NOT NULL, PRIMARY KEY (k));",
    )
    .unwrap();
    let t = Rows {
        columns: vec![
            Values::Int((1..=n).map(Some).collect()),
            Values::Int((1..=n).map(|i| Some(i % 7)).collect()),
        ],
    };
    let o = Rows {
        columns: vec![Values::Int((0..7).map(Some).collect())],
    };
    write(&path, &catalog, &[("s.t".into(), t), ("s.o".into(), o)]).unwrap();
    let mut s = Snail::open_writing(&path).unwrap();
    // The first row of each key as the rows come.
    let firsts = |rows: Vec<String>| -> Vec<String> {
        let mut first: Vec<(i64, String)> = Vec::new();
        for r in rows {
            let (g, _) = r.split_once('|').unwrap();
            let g: i64 = g.parse().unwrap();
            if !first.iter().any(|(k, _)| *k == g) {
                first.push((g, r));
            }
        }
        first.sort();
        first.into_iter().map(|(_, r)| r).collect()
    };
    let want = firsts(rows(&mut s, "SELECT g, id FROM s.t"));
    for room in [256 << 10, 64 << 10] {
        s.size_rounds(Sizes::of(room));
        assert_eq!(
            rows(&mut s, "SELECT DISTINCT ON (g) g, id FROM s.t"),
            want,
            "{room}"
        );
    }
    // Over a join, how many rows come not known: those written within two rooms stand as one part
    // once the runs begin, its ties after the first run's and before the next's.
    let join = "FROM s.o o JOIN s.t t ON t.g = o.k";
    let want = firsts(rows(&mut s, &format!("SELECT t.g, t.id {join}")));
    assert_eq!(
        rows(
            &mut s,
            &format!("SELECT DISTINCT ON (t.g) t.g, t.id {join}")
        ),
        want,
        "{join}"
    );
}

// FROM's three, case 12a: a subquery written without an alias, a join with an alias, and the
// functions in FROM.

const FR_DDL: &str = "CREATE SCHEMA fr;
     CREATE TABLE fr.a (id integer NOT NULL, g integer, name text, PRIMARY KEY (id));
     CREATE TABLE fr.b (id integer NOT NULL, a_id integer, v integer, PRIMARY KEY (id));";

const FR_ROWS: [&str; 2] = [
    "INSERT INTO fr.a VALUES (1, 10, 'one'), (2, 10, 'two'), (3, 20, 'three'), (4, NULL, 'four')",
    "INSERT INTO fr.b VALUES (1, 1, 100), (2, 1, 200), (3, 3, 300), (4, 9, 400)",
];

/// A snail holding case 12a's rows, at a path of the test's name.
fn fr(name: &str) -> Snail {
    let path = tmp(&format!("reader-fr-{name}.snail"));
    write(&path, &Catalog::parse(FR_DDL).unwrap(), &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    for sql in FR_ROWS {
        w.execute(sql).unwrap();
    }
    w
}

/// PostgreSQL's words for a call no form of a function takes, with their HINT.
macro_rules! no_function {
    ($call:literal) => {
        concat!(
            "function ",
            $call,
            " does not exist\nHINT:  No function matches the given name and argument types. You \
             might need to add explicit type casts."
        )
    };
}

/// Each statement refused as 18.6 refuses it, its SQLSTATE and its words, in its order.
fn refusals(w: &mut Snail, asked: &[(&str, &str, &str)]) {
    for (sql, code, words) in asked {
        assert_eq!(
            refused(w, sql),
            (Some(*code), weather::hinted(words)),
            "{sql}"
        );
    }
}

/// 18.6's refusal of a reference to a relation it holds where the reference cannot reach it, with
/// its DETAIL.
fn unreachable_entry(written: &str, entry: &str) -> String {
    format!(
        "invalid reference to FROM-clause entry for table \"{written}\": There is an entry for \
         table \"{entry}\", but it cannot be referenced from this part of the query."
    )
}

#[test]
fn an_unaliased_subquery_answers_bare_names_only() {
    let mut w = fr("unnamed");
    answered(
        &mut w,
        &[
            (
                "SELECT id, name FROM (SELECT id, name FROM fr.a WHERE g = 10) ORDER BY id",
                &["1|one", "2|two"],
            ),
            (
                "SELECT a.name, v FROM fr.a a JOIN (SELECT a_id, v FROM fr.b) ON a.id = a_id \
                 ORDER BY v",
                &["one|100", "one|200", "three|300"],
            ),
            (
                "SELECT n FROM (SELECT id + 1 AS n FROM (SELECT id FROM fr.a) WHERE id < 3) \
                 ORDER BY n",
                &["2", "3"],
            ),
            (
                "SELECT a.id, m FROM fr.a a, LATERAL (SELECT max(b.v) AS m FROM fr.b b \
                 WHERE b.a_id = a.id) ORDER BY a.id",
                &["1|200", "2|NULL", "3|300", "4|NULL"],
            ),
        ],
    );
    let hidden = unreachable_entry("unnamed_subquery", "unnamed_subquery");
    refusals(
        &mut w,
        &[
            ("SELECT unnamed_subquery.id FROM (SELECT id FROM fr.a)", "42P01", &hidden),
            ("SELECT unnamed_subquery.* FROM (SELECT id FROM fr.a)", "42P01", &hidden),
            (
                "SELECT unnamed_subquery.m FROM fr.a a, LATERAL (SELECT max(b.v) AS m \
                 FROM fr.b b WHERE b.a_id = a.id)",
                "42P01",
                &hidden,
            ),
            (
                "SELECT x.id FROM (SELECT id FROM fr.a) WHERE id > 3",
                "42P01",
                "missing FROM-clause entry for table \"x\"",
            ),
            (
                "SELECT * FROM (SELECT 1) AS t(a, b)",
                "42P10",
                "table \"t\" has 1 columns available but 2 columns specified",
            ),
        ],
    );
}

#[test]
fn an_unaliased_subquery_counts_in_ambiguity() {
    let mut w = fr("ambiguous");
    answered(
        &mut w,
        &[
            ("SELECT * FROM (SELECT 1 AS x), (SELECT 2 AS x)", &["1|2"]),
            (
                "SELECT x, y FROM (SELECT id AS x FROM fr.a) JOIN (SELECT a_id AS y FROM fr.b) \
                 ON x = y ORDER BY x, y",
                &["1|1", "1|1", "3|3"],
            ),
        ],
    );
    refusals(
        &mut w,
        &[
            (
                "SELECT id FROM fr.a JOIN (SELECT id FROM fr.b) ON true",
                "42702",
                "column reference \"id\" is ambiguous",
            ),
            (
                "SELECT x FROM (SELECT 1 AS x), (SELECT 1 AS x)",
                "42702",
                "column reference \"x\" is ambiguous",
            ),
        ],
    );
}

#[test]
fn a_join_s_alias_hides_the_names_inside_it() {
    let mut w = fr("hides");
    answered(
        &mut w,
        &[
            (
                "SELECT j.name, j.v FROM (fr.a a JOIN fr.b b ON a.id = b.a_id) AS j ORDER BY j.v",
                &["one|100", "one|200", "three|300"],
            ),
            (
                "SELECT name, v FROM (fr.a a JOIN fr.b b ON a.id = b.a_id) j ORDER BY v",
                &["one|100", "one|200", "three|300"],
            ),
            (
                "SELECT j.v, b.id FROM (fr.a a JOIN fr.b b ON a.id = b.a_id) AS j \
                 JOIN fr.b b ON b.id = j.a_id ORDER BY 1, 2",
                &["100|1", "200|1", "300|3"],
            ),
            (
                "SELECT a.v FROM (fr.a x JOIN fr.b y ON x.id = y.a_id) AS a ORDER BY 1",
                &["100", "200", "300"],
            ),
            (
                "SELECT j.v FROM ((fr.a a JOIN fr.b b ON a.id = b.a_id) AS i \
                 JOIN fr.a c ON c.id = i.a_id) AS j ORDER BY 1",
                &["100", "200", "300"],
            ),
            (
                "SELECT j.name, count(*) FROM (fr.a a JOIN fr.b b ON a.id = b.a_id) AS j \
                 GROUP BY j.name ORDER BY j.name",
                &["one|2", "three|1"],
            ),
            // A function inside it reads a relation before it.
            (
                "SELECT x.id, j.s FROM fr.a x, \
                 (fr.b b CROSS JOIN unnest(ARRAY[x.id, x.id * 10]) s) AS j \
                 WHERE j.a_id = x.id ORDER BY 1, 2",
                &["1|1", "1|1", "1|10", "1|10", "3|3", "3|30"],
            ),
        ],
    );
    refusals(
        &mut w,
        &[
            (
                "SELECT a.name FROM (fr.a a JOIN fr.b b ON a.id = b.a_id) AS j",
                "42P01",
                &unreachable_entry("a", "a"),
            ),
            (
                "SELECT b.v FROM (fr.a a JOIN fr.b b ON a.id = b.a_id) AS j",
                "42P01",
                &unreachable_entry("b", "b"),
            ),
            (
                "SELECT i.v FROM ((fr.a a JOIN fr.b b ON a.id = b.a_id) AS i \
                 JOIN fr.a c ON c.id = i.a_id) AS j",
                "42P01",
                &unreachable_entry("i", "i"),
            ),
            (
                "SELECT j.id FROM (fr.a a JOIN fr.b b ON a.id = b.a_id) AS j",
                "42702",
                "column reference \"id\" is ambiguous",
            ),
        ],
    );
}

#[test]
fn a_join_s_column_list_renames_the_leftmost() {
    let mut w = fr("renames");
    answered(
        &mut w,
        &[
            (
                "SELECT j.x, j.z, j.id, j.v FROM (fr.a a JOIN fr.b b ON a.id = b.a_id) \
                 AS j (x, y, z) ORDER BY j.x, j.v",
                &["1|one|1|100", "1|one|2|200", "3|three|3|300"],
            ),
            (
                "SELECT * FROM (fr.a a JOIN fr.b b ON a.id = b.a_id) AS j (p, q, r, s, t, u) \
                 ORDER BY u",
                &["1|10|one|1|1|100", "1|10|one|2|1|200", "3|20|three|3|3|300"],
            ),
            (
                "SELECT j.id, j.g, j.v FROM (fr.a JOIN fr.b USING (id)) AS j ORDER BY j.id",
                &["1|10|100", "2|10|200", "3|20|300", "4|NULL|400"],
            ),
            (
                "SELECT j.x FROM (fr.a JOIN fr.b USING (id)) AS j (x) ORDER BY j.x",
                &["1", "2", "3", "4"],
            ),
            (
                "SELECT j.* FROM (fr.a a JOIN fr.b b ON a.id = b.a_id) AS j (x) WHERE j.x = 3",
                &["3|20|three|3|3|300"],
            ),
        ],
    );
    refusals(
        &mut w,
        &[
            (
                "SELECT * FROM (fr.a a JOIN fr.b b ON a.id = b.a_id) AS j (p, q, r, s, t, u, w)",
                "42P10",
                "join expression \"j\" has 6 columns available but 7 columns specified",
            ),
            (
                "SELECT * FROM (fr.a a JOIN fr.b b USING (id)) AS j (p, q, r, s, t, u)",
                "42P10",
                "join expression \"j\" has 5 columns available but 6 columns specified",
            ),
            (
                "SELECT j.* FROM (fr.b b CROSS JOIN unnest(ARRAY[1, 2]) s) AS j (p, q, r, t, u)",
                "42P10",
                "join expression \"j\" has 4 columns available but 5 columns specified",
            ),
        ],
    );
}

#[test]
fn generate_series_counts_integers_to_their_bounds() {
    let mut w = fr("integers");
    // A series that never ended would be cut here, not hang the test.
    w.execute("SET statement_timeout = '2s'").unwrap();
    answered(
        &mut w,
        &[
            ("SELECT * FROM generate_series(1, 4)", &["1", "2", "3", "4"]),
            ("SELECT * FROM generate_series(1, 10, 3)", &["1", "4", "7", "10"]),
            ("SELECT * FROM generate_series(5, 1, -2)", &["5", "3", "1"]),
            ("SELECT * FROM generate_series(1, 0)", &[]),
            ("SELECT * FROM generate_series(1, NULL::integer)", &[]),
            ("SELECT * FROM generate_series(1, 5, NULL)", &[]),
            ("SELECT * FROM generate_series(NULL, 5, 0)", &[]),
            // A sum past the type's last value ends the series.
            (
                "SELECT * FROM generate_series(2147483600, 2147483647, 20)",
                &["2147483600", "2147483620", "2147483640"],
            ),
            (
                "SELECT * FROM generate_series(-2147483600, -2147483648, -20)",
                &["-2147483600", "-2147483620", "-2147483640"],
            ),
            (
                "SELECT * FROM generate_series(9223372036854775800::bigint, 9223372036854775807, 5)",
                &["9223372036854775800", "9223372036854775805"],
            ),
            (
                "SELECT * FROM generate_series(-9223372036854775800::bigint, \
                 -9223372036854775807, -5)",
                &["-9223372036854775800", "-9223372036854775805"],
            ),
            ("SELECT * FROM generate_series(1, '5')", &["1", "2", "3", "4", "5"]),
            (
                "SELECT sum(s), count(*) FROM generate_series(1, 100) s",
                &["5050|100"],
            ),
        ],
    );
    refusals(
        &mut w,
        &[
            (
                "SELECT * FROM generate_series(1, 5, 0)",
                "22023",
                "step size cannot equal zero",
            ),
            (
                "SELECT * FROM generate_series(1::bigint, 2, 0)",
                "22023",
                "step size cannot equal zero",
            ),
            (
                "SELECT * FROM generate_series(1, 'x')",
                "22P02",
                "invalid input syntax for type integer: \"x\"",
            ),
        ],
    );
}

#[test]
fn generate_series_steps_a_numeric_by_the_larger_scale() {
    let mut w = fr("numerics");
    answered(
        &mut w,
        &[
            ("SELECT * FROM generate_series(1.5, 3.5)", &["1.5", "2.5", "3.5"]),
            ("SELECT * FROM generate_series(0.1, 0.5, 0.15)", &["0.1", "0.25", "0.40"]),
            ("SELECT * FROM generate_series(1, 2, 0.5)", &["1", "1.5", "2.0"]),
            ("SELECT * FROM generate_series(3.0, 1, -1)", &["3.0", "2.0", "1.0"]),
            ("SELECT * FROM generate_series(1.00, 1.5, 0.25)", &["1.00", "1.25", "1.50"]),
            ("SELECT * FROM generate_series(5::numeric, 1, -1.5)", &["5", "3.5", "2.0"]),
            ("SELECT * FROM generate_series(0.5, 2)", &["0.5", "1.5"]),
            ("SELECT * FROM generate_series('NaN'::numeric, NULL)", &[]),
        ],
    );
    refusals(
        &mut w,
        &[
            (
                "SELECT * FROM generate_series('NaN'::numeric, 3)",
                "22023",
                "start value cannot be NaN",
            ),
            (
                "SELECT * FROM generate_series('Infinity'::numeric, 3)",
                "22023",
                "start value cannot be infinity",
            ),
            (
                "SELECT * FROM generate_series(1::numeric, 'NaN')",
                "22023",
                "stop value cannot be NaN",
            ),
            (
                "SELECT * FROM generate_series(1::numeric, 'Infinity')",
                "22023",
                "stop value cannot be infinity",
            ),
            (
                "SELECT * FROM generate_series(1::numeric, 2, 'NaN')",
                "22023",
                "step size cannot be NaN",
            ),
            (
                "SELECT * FROM generate_series(1::numeric, 2, '-Infinity')",
                "22023",
                "step size cannot be infinity",
            ),
            (
                "SELECT * FROM generate_series(1::numeric, 2, 0)",
                "22023",
                "step size cannot equal zero",
            ),
        ],
    );
}

#[test]
fn generate_series_adds_its_step_to_the_timestamp_before() {
    let mut w = fr("timestamps");
    answered(
        &mut w,
        &[
            (
                "SELECT * FROM generate_series('2026-01-30'::timestamp, '2026-05-01', '1 month')",
                &[
                    "2026-01-30 00:00:00",
                    "2026-02-28 00:00:00",
                    "2026-03-28 00:00:00",
                    "2026-04-28 00:00:00",
                ],
            ),
            (
                "SELECT * FROM generate_series('2026-01-31'::timestamp, '2026-06-01', '1 month')",
                &[
                    "2026-01-31 00:00:00",
                    "2026-02-28 00:00:00",
                    "2026-03-28 00:00:00",
                    "2026-04-28 00:00:00",
                    "2026-05-28 00:00:00",
                ],
            ),
            (
                "SELECT * FROM generate_series('2026-01-01 00:00'::timestamp, '2026-01-01 03:00', \
                 '90 minutes')",
                &[
                    "2026-01-01 00:00:00",
                    "2026-01-01 01:30:00",
                    "2026-01-01 03:00:00",
                ],
            ),
            (
                "SELECT * FROM generate_series('2026-01-03'::timestamp, '2026-01-01', '-1 day')",
                &[
                    "2026-01-03 00:00:00",
                    "2026-01-02 00:00:00",
                    "2026-01-01 00:00:00",
                ],
            ),
            (
                "SELECT * FROM generate_series('2026-01-01'::timestamp, '2026-01-01 00:00:01', \
                 '0.4 seconds')",
                &[
                    "2026-01-01 00:00:00",
                    "2026-01-01 00:00:00.4",
                    "2026-01-01 00:00:00.8",
                ],
            ),
            (
                "SELECT * FROM generate_series('2026-01-01 00:00'::timestamp, '2026-01-01 01:00', \
                 '-30 minutes')",
                &[],
            ),
            (
                "SELECT * FROM generate_series('2026-01-01'::date, '2026-01-03'::date, '1 day')",
                &[
                    "2026-01-01 00:00:00+00",
                    "2026-01-02 00:00:00+00",
                    "2026-01-03 00:00:00+00",
                ],
            ),
        ],
    );
    refusals(
        &mut w,
        &[
            (
                "SELECT * FROM generate_series('2026-01-01'::timestamp, '2026-01-02', '0 days')",
                "22023",
                "step size cannot equal zero",
            ),
            // A month counts as 30 days in the step's sign.
            (
                "SELECT * FROM generate_series('2026-01-01'::timestamp, '2026-01-02', \
                 '1 month -30 days')",
                "22023",
                "step size cannot equal zero",
            ),
            (
                "SELECT * FROM generate_series('2026-01-01'::timestamp, '2026-01-02', 'infinity')",
                "22023",
                "step size cannot be infinite",
            ),
            // The value after the first is made before the first is given.
            (
                "SELECT * FROM generate_series('2026-01-01'::timestamp, '2026-01-01', \
                 '300000 years')",
                "22008",
                "timestamp out of range",
            ),
        ],
    );
}

#[cfg(feature = "zones")]
#[test]
fn generate_series_keeps_the_local_clock_across_dst() {
    let mut w = fr("dst");
    w.execute("SET TimeZone = 'Europe/Lisbon'").unwrap();
    answered(
        &mut w,
        &[
            (
                "SELECT * FROM generate_series('2026-03-28 12:00'::timestamptz, \
                 '2026-03-30 12:00', '1 day')",
                &[
                    "2026-03-28 12:00:00+00",
                    "2026-03-29 12:00:00+01",
                    "2026-03-30 12:00:00+01",
                ],
            ),
            (
                "SELECT * FROM generate_series('2026-03-28 12:00'::timestamptz, \
                 '2026-03-30 12:00', '24 hours')",
                &["2026-03-28 12:00:00+00", "2026-03-29 13:00:00+01"],
            ),
        ],
    );
    w.execute("SET TimeZone = 'UTC'").unwrap();
    answered(
        &mut w,
        &[
            (
                "SELECT * FROM generate_series('2026-03-07 12:00+00'::timestamptz, \
                 '2026-03-10 12:00+00', '1 day', 'America/New_York')",
                &[
                    "2026-03-07 12:00:00+00",
                    "2026-03-08 11:00:00+00",
                    "2026-03-09 11:00:00+00",
                    "2026-03-10 11:00:00+00",
                ],
            ),
            (
                "SELECT * FROM generate_series('2026-01-01'::timestamptz, '2026-01-02', \
                 '12 hours', 'UTC')",
                &[
                    "2026-01-01 00:00:00+00",
                    "2026-01-01 12:00:00+00",
                    "2026-01-02 00:00:00+00",
                ],
            ),
            (
                "SELECT * FROM generate_series('2026-01-01'::timestamptz, '2026-01-02', '1 day', \
                 NULL)",
                &[],
            ),
        ],
    );
    refusals(
        &mut w,
        &[(
            "SELECT * FROM generate_series('2026-01-01'::timestamptz, '2026-01-02', '1 day', \
             'Mars/Olympus')",
            "22023",
            "time zone \"Mars/Olympus\" not recognized",
        )],
    );
}

#[test]
fn generate_series_resolves_its_form() {
    let mut w = fr("forms");
    let typed = |args: &str| format!("SELECT pg_typeof(g) FROM generate_series({args}) g LIMIT 1");
    for (args, ty) in [
        ("1, 2", "integer"),
        ("1::smallint, 2", "integer"),
        ("'1', 2", "integer"),
        ("1, 2::bigint", "bigint"),
        ("1, 2, 0.5", "numeric"),
        ("1::numeric, 2::smallint", "numeric"),
        ("'2026-01-01'::timestamp, '2026-01-02', '1 day'", "timestamp without time zone"),
        ("'2026-01-01'::timestamp, '2026-01-02'::date, '1 day'", "timestamp without time zone"),
        ("'2026-01-01'::date, '2026-01-02'::date, '1 day'", "timestamp with time zone"),
        ("'2026-01-01', '2026-01-02', '1 day'::interval", "timestamp with time zone"),
        (
            "'2026-01-01'::timestamp, '2026-01-02'::timestamptz, '1 day'",
            "timestamp with time zone",
        ),
        ("'2026-01-01'::timestamp, '2026-01-02', '1 day', 'UTC'", "timestamp with time zone"),
    ] {
        answered(&mut w, &[(&typed(args), &[ty])]);
    }
    for (args, code, words) in [
        ("1::smallint, 2::smallint", "42725", "function generate_series(smallint, smallint) is not unique"),
        ("1::smallint, '2'", "42725", "function generate_series(smallint, unknown) is not unique"),
        ("'1', '2'", "42725", "function generate_series(unknown, unknown) is not unique"),
        (
            "'1', '2', '1'",
            "42725",
            "function generate_series(unknown, unknown, unknown) is not unique",
        ),
        (
            "1.5::float8, 2",
            "42883",
            no_function!("generate_series(double precision, integer)"),
        ),
        (
            "'2026-01-01'::timestamp, '2026-01-02'",
            "42883",
            no_function!("generate_series(timestamp without time zone, unknown)"),
        ),
        (
            "1, 2, '1 day'::interval",
            "42883",
            no_function!("generate_series(integer, integer, interval)"),
        ),
    ] {
        refusals(&mut w, &[(&typed(args), code, words)]);
    }
}

#[test]
fn a_function_in_from_is_made_for_each_row_before_it() {
    let mut w = fr("lateral");
    answered(
        &mut w,
        &[
            (
                "SELECT a.id, s FROM fr.a a, LATERAL generate_series(1, a.id) s ORDER BY a.id, s",
                &["1|1", "2|1", "2|2", "3|1", "3|2", "3|3", "4|1", "4|2", "4|3", "4|4"],
            ),
            (
                "SELECT a.id, s FROM fr.a a CROSS JOIN generate_series(a.id, 3) s ORDER BY 1, 2",
                &["1|1", "1|2", "1|3", "2|2", "2|3", "3|3"],
            ),
            (
                "SELECT a.id, s FROM fr.a a LEFT JOIN LATERAL generate_series(1, a.g / 10) s \
                 ON true ORDER BY a.id, s",
                &["1|1", "2|1", "3|1", "3|2", "4|NULL"],
            ),
            (
                "SELECT a.id, o.v, o.n FROM fr.a a, \
                 LATERAL generate_series(a.id, a.id + 1) WITH ORDINALITY o(v, n) ORDER BY 1, 3",
                &[
                    "1|1|1", "1|2|2", "2|2|1", "2|3|2", "3|3|1", "3|4|2", "4|4|1", "4|5|2",
                ],
            ),
            (
                "SELECT a.id, (SELECT count(*) FROM generate_series(1, a.id)) FROM fr.a a \
                 ORDER BY 1",
                &["1|1", "2|2", "3|3", "4|4"],
            ),
            (
                "SELECT s, count(a.id) FROM generate_series(5, 25, 10) s \
                 LEFT JOIN fr.a a ON a.g = s + 5 GROUP BY s ORDER BY s",
                &["5|2", "15|1", "25|0"],
            ),
        ],
    );
}

#[test]
fn an_endless_series_runs_until_the_timeout() {
    let mut w = fr("timeout");
    // A function in FROM runs to its end before its first row is read, past the room written.
    w.size_rounds(Sizes::of(64 << 10));
    answered(
        &mut w,
        &[(
            "SELECT a.id, count(*) FROM fr.a a, generate_series(1, 50000) s GROUP BY a.id \
             ORDER BY 1",
            &["1|50000", "2|50000", "3|50000", "4|50000"],
        )],
    );
    w.execute("SET statement_timeout = '100ms'").unwrap();
    let cancelled = "canceling statement due to statement timeout";
    refusals(
        &mut w,
        &[
            ("SELECT count(*) FROM generate_series(1, 10000000)", "57014", cancelled),
            (
                "SELECT * FROM generate_series('2026-01-01'::timestamp, 'infinity', '1 second') \
                 LIMIT 2",
                "57014",
                cancelled,
            ),
        ],
    );
    w.execute("RESET statement_timeout").unwrap();
    answered(&mut w, &[("SELECT count(*) FROM generate_series(1, 3)", &["3"])]);
}

#[test]
fn a_function_in_from_is_named_and_counted() {
    let mut w = fr("named");
    answered(
        &mut w,
        &[
            ("SELECT g.n FROM generate_series(1, 3) AS g(n)", &["1", "2", "3"]),
            ("SELECT g FROM generate_series(1, 3) AS g", &["1", "2", "3"]),
            ("SELECT g.g FROM generate_series(1, 3) g", &["1", "2", "3"]),
            ("SELECT generate_series FROM generate_series(1, 2)", &["1", "2"]),
            (
                "SELECT * FROM generate_series(10, 30, 10) WITH ORDINALITY",
                &["10|1", "20|2", "30|3"],
            ),
            (
                "SELECT * FROM generate_series(4, 1, -1) WITH ORDINALITY t",
                &["4|1", "3|2", "2|3", "1|4"],
            ),
            (
                "SELECT t.t, t.ordinality FROM generate_series(4, 1, -1) WITH ORDINALITY t \
                 ORDER BY 2 DESC LIMIT 1",
                &["1|4"],
            ),
            (
                "SELECT t.v, t.n FROM generate_series(4, 1, -1) WITH ORDINALITY AS t(v, n) \
                 ORDER BY t.n DESC",
                &["1|4", "2|3", "3|2", "4|1"],
            ),
            // The alias names the column, which a bare name reads first.
            ("SELECT g FROM generate_series(1, 2) WITH ORDINALITY g", &["1", "2"]),
            // A lone function's whole row is its value.
            ("SELECT u FROM unnest(ARRAY[5, 6]) AS u(v)", &["5", "6"]),
            ("SELECT pg_typeof(g) FROM generate_series(1::bigint, 1) g", &["bigint"]),
        ],
    );
    refusals(
        &mut w,
        &[
            (
                "SELECT g.generate_series FROM generate_series(1, 3) g",
                "42703",
                "column g.generate_series does not exist",
            ),
            (
                "SELECT * FROM generate_series(1, 3) AS g(a, b)",
                "42P10",
                "table \"g\" has 1 columns available but 2 columns specified",
            ),
            (
                "SELECT * FROM generate_series(1, 3) WITH ORDINALITY AS g(a, b, c)",
                "42P10",
                "table \"g\" has 2 columns available but 3 columns specified",
            ),
        ],
    );
    // Past a lone function with no ordinality, the name reads the whole row, a record.
    answered(
        &mut w,
        &[
            (
                "SELECT u FROM unnest(ARRAY[5, 6]) WITH ORDINALITY AS u(v, n)",
                &["(5,1)", "(6,2)"],
            ),
            (
                "SELECT t FROM unnest(ARRAY[1, 2], ARRAY['a', 'b']) AS t(n, s)",
                &["(1,a)", "(2,b)"],
            ),
        ],
    );
}

#[test]
fn functions_side_by_side_run_as_long_as_the_longest() {
    let mut w = fr("side");
    answered(
        &mut w,
        &[
            (
                "SELECT * FROM unnest(ARRAY[1, 2], ARRAY['a', 'b', 'c']) AS t(n, s)",
                &["1|a", "2|b", "NULL|c"],
            ),
            (
                "SELECT * FROM unnest(ARRAY[1, 2], ARRAY['a', 'b', 'c'])",
                &["1|a", "2|b", "NULL|c"],
            ),
            (
                "SELECT * FROM unnest(ARRAY[1, 2], ARRAY['a', 'b', 'c']) WITH ORDINALITY \
                 AS t(n, s, o)",
                &["1|a|1", "2|b|2", "NULL|c|3"],
            ),
            (
                "SELECT * FROM unnest(NULL::integer[], ARRAY[1, 2]) AS t(p, q)",
                &["NULL|1", "NULL|2"],
            ),
            ("SELECT * FROM unnest('{}'::integer[], '{}'::text[]) AS t(p, q)", &[]),
            (
                "SELECT * FROM unnest(ARRAY[[1, 2], [3, 4]], ARRAY['a']) AS t(p, q)",
                &["1|a", "2|NULL", "3|NULL", "4|NULL"],
            ),
            (
                "SELECT * FROM unnest('{[1,3), [5,7)}'::int4multirange, ARRAY[true]) AS t(r, b)",
                &["[1,3)|t", "[5,7)|NULL"],
            ),
            (
                "SELECT * FROM unnest('cat:3 fat:2'::tsvector, ARRAY[1]) AS t",
                &["cat|{3}|{D}|1", "fat|{2}|{D}|NULL"],
            ),
            (
                "SELECT a.id, t.p, t.q FROM fr.a a, \
                 unnest(ARRAY[a.id, a.id * 10], ARRAY['x']) t(p, q) WHERE a.id < 3 ORDER BY 1, 2",
                &["1|1|x", "1|10|NULL", "2|2|x", "2|20|NULL"],
            ),
            (
                "SELECT * FROM unnest(ARRAY[1, 2], ARRAY['a', 'b'], ARRAY[1.5]) AS t(x, y, z)",
                &["1|a|1.5", "2|b|NULL"],
            ),
            (
                "SELECT count(*), count(t.y), sum(t.x) FROM unnest(ARRAY[1, 2, 3], ARRAY['a']) \
                 AS t(x, y)",
                &["3|1|6"],
            ),
            (
                "SELECT t.x, b.v FROM unnest(ARRAY[1, 3], ARRAY['p', 'q']) AS t(x, y) \
                 JOIN fr.b b ON b.a_id = t.x ORDER BY 1, 2",
                &["1|100", "1|200", "3|300"],
            ),
            (
                "SELECT pg_typeof(x), pg_typeof(y) FROM unnest(ARRAY[1, 2], ARRAY['a']) \
                 AS t(x, y) LIMIT 1",
                &["integer|text"],
            ),
        ],
    );
    refusals(
        &mut w,
        &[
            (
                "SELECT t.unnest FROM unnest(ARRAY[1, 2], ARRAY['a', 'b', 'c']) AS t",
                "42702",
                "column reference \"unnest\" is ambiguous",
            ),
            (
                "SELECT * FROM unnest(ARRAY[1, 2], ARRAY['a', 'b']) AS t(x, y, z)",
                "42P10",
                "table \"t\" has 2 columns available but 3 columns specified",
            ),
            (
                "SELECT * FROM unnest(ARRAY[1, 2], 5) AS t(x, y)",
                "42883",
                no_function!("pg_catalog.unnest(integer)"),
            ),
            (
                "SELECT * FROM unnest(ARRAY[1, 2], NULL) AS t(x, y)",
                "42725",
                "function pg_catalog.unnest(unknown) is not unique",
            ),
            (
                "SELECT * FROM unnest(NULL) AS t(x)",
                "42725",
                "function unnest(unknown) is not unique",
            ),
        ],
    );
}

#[test]
fn rows_from_reads_its_functions_side_by_side() {
    let mut w = fr("rows-from");
    answered(
        &mut w,
        &[
            (
                "SELECT * FROM ROWS FROM (generate_series(1, 3), generate_series(10, 11)) AS t(a, b)",
                &["1|10", "2|11", "3|NULL"],
            ),
            (
                "SELECT * FROM ROWS FROM (generate_series(1, 2), unnest(ARRAY['x', 'y', 'z'])) \
                 WITH ORDINALITY AS t(a, b, n)",
                &["1|x|1", "2|y|2", "NULL|z|3"],
            ),
            ("SELECT t FROM ROWS FROM (generate_series(1, 2)) AS t", &["1", "2"]),
            (
                "SELECT * FROM ROWS FROM (unnest(ARRAY[1, 2], ARRAY['a', 'b', 'c'])) AS t(x, y)",
                &["1|a", "2|b", "NULL|c"],
            ),
            (
                "SELECT a.id, t.* FROM fr.a a, ROWS FROM (generate_series(1, a.id), \
                 generate_series(10, 10 + a.id)) AS t(s, u) WHERE a.id <= 2 ORDER BY 1, 2, 3",
                &["1|1|10", "1|NULL|11", "2|1|10", "2|2|11", "2|NULL|12"],
            ),
            (
                "SELECT a.id, t.* FROM fr.a a CROSS JOIN LATERAL ROWS FROM \
                 (generate_series(1, a.id)) AS t(s) WHERE a.id <= 2 ORDER BY 1, 2",
                &["1|1", "2|1", "2|2"],
            ),
            (
                "SELECT * FROM fr.a a JOIN ROWS FROM (generate_series(1, 2)) AS t(s) \
                 ON a.id = t.s ORDER BY 1",
                &["1|10|one|1", "2|10|two|2"],
            ),
            // `rows` before FROM in a select list is a name.
            ("SELECT x, rows FROM (SELECT 1 AS x, 2 AS rows) s", &["1|2"]),
        ],
    );
    refusals(
        &mut w,
        &[
            (
                "SELECT generate_series FROM ROWS FROM (generate_series(1, 2), \
                 generate_series(3, 4))",
                "42702",
                "column reference \"generate_series\" is ambiguous",
            ),
            ("SELECT * FROM ROWS FROM ()", "42601", "syntax error at or near \")\""),
        ],
    );
    // Split from a text, each statement reads again as PostgreSQL's: ROWS FROM as written.
    assert_eq!(
        warren_snail::read::statement::split(
            "SELECT * FROM ROWS FROM (generate_series(1, 2)) AS t(a); \
             SELECT * FROM jsonb_to_record('{}') AS (x integer)"
        )
        .unwrap(),
        [
            "SELECT * FROM ROWS FROM (generate_series(1, 2)) AS t(a)",
            "SELECT * FROM jsonb_to_record('{}') AS (x integer)",
        ]
    );
}

#[test]
fn a_column_definition_list_types_a_record() {
    let mut w = fr("defined");
    answered(
        &mut w,
        &[
            (
                "SELECT * FROM jsonb_to_recordset('[{\"x\": 1, \"y\": \"a\"}, {\"x\": 2}]') \
                 AS t(x integer, y text)",
                &["1|a", "2|NULL"],
            ),
            (
                "SELECT * FROM json_to_recordset('[{\"x\": 1, \"y\": \"a\"}, {\"x\": 2}]') \
                 AS t(x integer, y text)",
                &["1|a", "2|NULL"],
            ),
            (
                "SELECT x FROM jsonb_to_record('{\"x\": 1, \"y\": \"a\"}') AS (x integer, y text)",
                &["1"],
            ),
            (
                "SELECT jsonb_to_record.x FROM jsonb_to_record('{\"x\": 1, \"y\": \"a\"}') \
                 AS (x integer, y text)",
                &["1"],
            ),
            (
                "SELECT * FROM jsonb_to_record('{\"x\": \"1.5\", \"y\": [1, 2], \"z\": {\"q\": 1}, \
                 \"w\": \"2026-01-02\"}') AS t(x numeric, y integer[], z jsonb, w date)",
                &["1.5|{1,2}|{\"q\": 1}|2026-01-02"],
            ),
            (
                "SELECT pg_typeof(x), pg_typeof(y) FROM jsonb_to_record('{\"x\": 1}') \
                 AS t(x int8, y timestamptz)",
                &["bigint|timestamp with time zone"],
            ),
            (
                "SELECT * FROM jsonb_to_record('{\"x\": {\"id\": 1, \"v\": 2}}') AS t(x fr.b)",
                &["(1,,2)"],
            ),
            ("SELECT * FROM jsonb_to_record(NULL) AS t(x integer)", &["NULL"]),
            ("SELECT * FROM jsonb_to_recordset(NULL) AS t(x integer)", &[]),
            (
                "SELECT * FROM jsonb_to_record('{\"X\": 1, \"x\": 2}') AS t(\"X\" integer, x integer)",
                &["1|2"],
            ),
            (
                "SELECT a.id, r.* FROM fr.a a, jsonb_to_record(jsonb_build_object('k', a.name)) \
                 AS r(k text) ORDER BY 1",
                &["1|one", "2|two", "3|three", "4|four"],
            ),
            (
                "SELECT * FROM ROWS FROM (jsonb_to_record('{\"a\": 1, \"b\": \"x\"}') \
                 AS (a integer, b text), generate_series(1, 2)) AS t",
                &["1|x|1", "NULL|NULL|2"],
            ),
            (
                "SELECT * FROM ROWS FROM (jsonb_to_recordset('[{\"a\": 1}, {\"a\": 2}]') \
                 AS (a integer), json_to_record('{\"c\": true}') AS (c boolean))",
                &["1|t", "2|NULL"],
            ),
            (
                "SELECT * FROM ROWS FROM (jsonb_to_record('{\"a\": 1}')) AS t(a integer)",
                &["1"],
            ),
        ],
    );
    refusals(
        &mut w,
        &[
            (
                "SELECT * FROM json_to_record('{\"x\": \"abc\"}') AS t(x integer)",
                "22P02",
                "invalid input syntax for type integer: \"abc\"",
            ),
            (
                "SELECT * FROM jsonb_to_record('{\"x\": \"abc\"}') AS t(x varchar(1))",
                "22001",
                "value too long for type character varying(1)",
            ),
            (
                "SELECT * FROM jsonb_to_recordset('{\"x\": 1}') AS t(x integer)",
                "22023",
                "cannot call jsonb_to_recordset on a non-array",
            ),
            (
                "SELECT * FROM json_to_recordset('{\"x\": 1}') AS t(x integer)",
                "22023",
                "cannot call json_to_recordset on an object",
            ),
            (
                "SELECT * FROM jsonb_to_recordset('[1]') AS t(x integer)",
                "22023",
                "argument of jsonb_to_recordset must be an array of objects",
            ),
            (
                "SELECT * FROM jsonb_to_record('[1]') AS t(x integer)",
                "22023",
                "cannot call populate_composite on an array",
            ),
        ],
    );
}

#[test]
fn a_column_definition_list_refused_as_postgresql_refuses() {
    let mut w = fr("undefined");
    let required = "a column definition list is required for functions returning \"record\"";
    let only = "a column definition list is only allowed for functions returning \"record\"";
    refusals(
        &mut w,
        &[
            ("SELECT * FROM jsonb_to_recordset('[{\"x\": 1}]')", "42601", required),
            ("SELECT * FROM ROWS FROM (jsonb_to_record('{\"a\": 1}'))", "42601", required),
            ("SELECT * FROM generate_series(1, 2) AS t(x integer)", "42601", only),
            ("SELECT * FROM ROWS FROM (generate_series(1, 2) AS (x integer))", "42601", only),
            (
                "SELECT * FROM json_each('{\"a\": 1}') AS t(k text, v json)",
                "42601",
                "a column definition list is redundant for a function with OUT parameters",
            ),
            (
                "SELECT * FROM jsonb_populate_recordset(NULL::fr.b, '[{\"id\": 7}]') \
                 AS t(x integer)",
                "42601",
                "a column definition list is redundant for a function returning a named \
                 composite type",
            ),
            (
                "SELECT * FROM generate_series(1, 2) WITH ORDINALITY AS t(x integer)",
                "42601",
                "WITH ORDINALITY cannot be used with a column definition list\nHINT:  Put the \
                 column definition list inside ROWS FROM().",
            ),
            (
                "SELECT * FROM unnest(ARRAY[1, 2], ARRAY['a', 'b']) AS t(x integer, y text)",
                "42601",
                "UNNEST() with multiple arguments cannot have a column definition list\nHINT:  \
                 Use separate UNNEST() calls inside ROWS FROM(), and attach a column definition \
                 list to each one.",
            ),
            (
                "SELECT * FROM ROWS FROM (jsonb_to_record('{\"a\": 1}'), generate_series(1, 2)) \
                 AS t(a integer)",
                "42601",
                "ROWS FROM() with multiple functions cannot have a column definition list\nHINT:  \
                 Put a separate column definition list for each function inside ROWS FROM().",
            ),
            (
                "SELECT * FROM ROWS FROM (jsonb_to_record('{\"a\": 1}') AS (a integer)) \
                 AS t(x integer)",
                "42601",
                "multiple column definition lists are not allowed for the same function",
            ),
            (
                "SELECT * FROM jsonb_to_record('{\"x\": 1}') AS t(x integer, x text)",
                "42701",
                "column name \"x\" specified more than once",
            ),
            (
                "SELECT * FROM jsonb_to_record('{\"x\": 1}') AS t(x, y integer)",
                "42601",
                "syntax error at or near \"integer\"",
            ),
            (
                "SELECT * FROM jsonb_to_record('{\"x\": 1}') AS t(x integer, y)",
                "42601",
                "syntax error at or near \")\"",
            ),
            (
                "SELECT * FROM jsonb_to_record('{\"x\": 1}') AS t(x nosuchtype)",
                "42704",
                "type \"nosuchtype\" does not exist",
            ),
            (
                "SELECT * FROM jsonb_to_record('{\"x\": 1}') AS t(x record)",
                "42809",
                "record type has not been registered",
            ),
            (
                "SELECT jsonb_to_record('{\"a\": 1}')",
                "0A000",
                "could not determine row type for result of jsonb_to_record\nHINT:  Provide a \
                 non-null record argument, or call the function in the FROM clause using a \
                 column definition list.",
            ),
        ],
    );
}

#[test]
fn a_column_definition_list_carries_a_collation() {
    let mut w = fr("collated");
    answered(
        &mut w,
        &[
            ("SELECT x FROM jsonb_to_record('{\"x\":\"b\"}') AS t(x text COLLATE \"C\")", &["b"]),
            (
                "SELECT pg_collation_for(x) FROM jsonb_to_record('{\"x\":\"b\"}') \
                 AS t(x text COLLATE \"C\")",
                &["\"C\""],
            ),
            (
                "SELECT x FROM jsonb_to_recordset('[{\"x\":\"b\"},{\"x\":\"B\"},{\"x\":\"a\"}]') \
                 AS t(x text COLLATE \"C\") ORDER BY x",
                &["B", "a", "b"],
            ),
            (
                "SELECT * FROM ROWS FROM (jsonb_to_record('{\"x\":\"b\"}') AS (x text COLLATE \"C\"), \
                 generate_series(1, 2)) AS r",
                &["b|1", "NULL|2"],
            ),
            (
                "SELECT pg_collation_for(x) FROM ROWS FROM (jsonb_to_record('{\"x\":\"b\"}') \
                 AS (x text COLLATE \"C\"), generate_series(1, 2)) AS r",
                &["\"C\"", "\"C\""],
            ),
            (
                "SELECT pg_collation_for(x), x FROM jsonb_to_record('{\"x\":\"b\"}') \
                 AS (x text COLLATE \"C\")",
                &["\"C\"|b"],
            ),
            (
                "SELECT pg_collation_for(x), pg_collation_for(y) FROM \
                 jsonb_to_record('{\"x\":\"b\",\"y\":\"c\"}') t(x text COLLATE pg_catalog.\"C\", y text)",
                &["\"C\"|\"default\""],
            ),
            (
                "SELECT pg_collation_for(x) FROM jsonb_to_record('{\"x\":\"b\"}') \
                 AS t(x varchar(3) COLLATE \"POSIX\")",
                &["\"POSIX\""],
            ),
            (
                "SELECT pg_collation_for(x) FROM jsonb_to_record('{\"x\":[\"b\"]}') \
                 AS t(x text[] COLLATE \"C\")",
                &["\"C\""],
            ),
            (
                "SELECT count(*) FROM jsonb_to_recordset('[{\"x\":\"b\"},{\"x\":\"B\"}]') \
                 AS t(x text COLLATE \"C\") WHERE x < 'a'",
                &["1"],
            ),
        ],
    );
    refusals(
        &mut w,
        &[
            (
                "SELECT * FROM jsonb_to_record('{\"x\":1}') AS t(x integer COLLATE \"C\")",
                "42804",
                "collations are not supported by type integer",
            ),
            (
                "SELECT * FROM jsonb_to_record('{\"x\":1}') AS t(x nosuchtype COLLATE \"C\")",
                "42704",
                "type \"nosuchtype\" does not exist",
            ),
            (
                "SELECT * FROM jsonb_to_record('{\"x\":1}') AS t(x integer, x text COLLATE \"C\")",
                "42701",
                "column name \"x\" specified more than once",
            ),
            (
                "SELECT * FROM generate_series(1, 2) AS t(x integer COLLATE \"C\")",
                "42601",
                "a column definition list is only allowed for functions returning \"record\"",
            ),
            (
                "SELECT * FROM jsonb_to_record('{\"x\":\"b\"}') AS t(x COLLATE \"C\")",
                "42601",
                "syntax error at or near \"COLLATE\"",
            ),
        ],
    );
    // A collation the machine's libraries are asked for: read and compared under ICU's, and one
    // they do not hold refused, as 18.6 refuses it before the type's. Without the feature, each
    // refused by name.
    let unheld = [
        "SELECT * FROM jsonb_to_record('{\"x\":\"b\"}') AS t(x text COLLATE nosuch)",
        "SELECT * FROM jsonb_to_record('{\"x\":1}') AS t(x integer COLLATE nosuch)",
    ];
    #[cfg(feature = "collations")]
    {
        answered(
            &mut w,
            &[
                (
                    "SELECT x FROM jsonb_to_recordset('[{\"x\":\"b\"},{\"x\":\"B\"},{\"x\":\"a\"}]') \
                     AS t(x text COLLATE \"und-x-icu\") ORDER BY x",
                    &["a", "b", "B"],
                ),
                (
                    "SELECT pg_collation_for(x) FROM jsonb_to_record('{\"x\":\"b\"}') \
                     AS t(x text COLLATE \"und-x-icu\")",
                    &["\"und-x-icu\""],
                ),
            ],
        );
        for sql in unheld {
            assert_eq!(
                refused(&mut w, sql),
                (
                    Some("42704"),
                    "collation \"nosuch\" for encoding \"UTF8\" does not exist".to_string()
                ),
                "{sql}"
            );
        }
    }
    #[cfg(not(feature = "collations"))]
    for sql in unheld {
        let words = refused(&mut w, sql).1;
        assert!(words.contains("without the feature collations"), "{sql}: {words}");
    }
}

#[test]
fn a_column_definition_list_refuses_setof() {
    let mut w = fr("setof");
    let setof = |c: &str| format!("column \"{c}\" cannot be declared SETOF");
    let (x, y) = (setof("x"), setof("y"));
    refusals(
        &mut w,
        &[
            ("SELECT * FROM jsonb_to_record('{\"x\":1}') AS t(x SETOF integer)", "42P16", &x),
            (
                "SELECT * FROM jsonb_to_record('{\"x\":1}') AS t(x integer, y SETOF text)",
                "42P16",
                &y,
            ),
            ("SELECT * FROM jsonb_to_record('{\"x\":1}') AS t(x SETOF nosuchtype)", "42P16", &x),
            (
                "SELECT * FROM jsonb_to_record('{\"x\":1}') AS t(x SETOF integer COLLATE \"C\")",
                "42P16",
                &x,
            ),
            (
                "SELECT * FROM ROWS FROM (jsonb_to_record('{\"x\":1}') AS (x SETOF integer))",
                "42P16",
                &x,
            ),
            ("SELECT * FROM jsonb_to_record('{\"x\":1}') AS (x SETOF integer)", "42P16", &x),
            (
                "SELECT * FROM jsonb_to_record('{\"x\":1}') AS t(x SETOF integer, x text)",
                "42P16",
                &x,
            ),
            (
                "SELECT * FROM jsonb_to_record('{\"x\":1}') AS t(x integer COLLATE \"C\", \
                 y SETOF text)",
                "42804",
                "collations are not supported by type integer",
            ),
            (
                "SELECT * FROM generate_series(1, 2) AS t(x SETOF integer)",
                "42601",
                "a column definition list is only allowed for functions returning \"record\"",
            ),
        ],
    );
}

// Each refusal's first line: 18.6's HINT follows it.
#[test]
fn json_to_record_s_forms_are_refused_as_postgresql_refuses_them() {
    let mut w = fr("record-forms");
    let asked: &[(&str, &str, &str)] = &[
        (
            "SELECT * FROM jsonb_to_record('{\"x\":1}', 2)",
            "42883",
            "function jsonb_to_record(unknown, integer) does not exist",
        ),
        (
            "SELECT * FROM jsonb_to_record(1) AS t(x integer)",
            "42883",
            "function jsonb_to_record(integer) does not exist",
        ),
        (
            "SELECT * FROM jsonb_to_record('{\"x\":1}', '{}')",
            "42883",
            "function jsonb_to_record(unknown, unknown) does not exist",
        ),
        (
            "SELECT * FROM jsonb_to_recordset() AS t(x integer)",
            "42883",
            "function jsonb_to_recordset() does not exist",
        ),
        (
            "SELECT * FROM json_to_record('{\"x\":1}'::jsonb) AS t(x integer)",
            "42883",
            "function json_to_record(jsonb) does not exist",
        ),
        (
            "SELECT * FROM json_to_recordset('[{\"x\":1}]'::jsonb) AS t(x integer)",
            "42883",
            "function json_to_recordset(jsonb) does not exist",
        ),
        (
            "SELECT * FROM jsonb_to_recordset('[{\"x\":1}]'::json) AS t(x integer)",
            "42883",
            "function jsonb_to_recordset(json) does not exist",
        ),
        (
            "SELECT * FROM jsonb_to_record(NULL::text) AS t(x integer)",
            "42883",
            "function jsonb_to_record(text) does not exist",
        ),
        (
            "SELECT * FROM json_to_record('{\"x\":1}', true) AS t(x integer)",
            "42883",
            "function json_to_record(unknown, boolean) does not exist",
        ),
    ];
    for (sql, code, words) in asked {
        let (state, said) = refused(&mut w, sql);
        assert_eq!((state, said.lines().next()), (Some(*code), Some(*words)), "{sql}");
    }
}

#[test]
fn a_view_over_rows_from_answers_its_rows() {
    // One view made by the file's DDL, the others by statements; each read again once the file is
    // opened anew.
    let path = tmp("reader-fr-views.snail");
    let ddl = format!(
        "{FR_DDL} CREATE VIEW fr.rf AS SELECT * FROM ROWS FROM (generate_series(1, 2), \
         generate_series(5, 7)) AS r(a, b);"
    );
    write(&path, &Catalog::parse(&ddl).unwrap(), &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    for sql in [
        "CREATE VIEW fr.na AS SELECT * FROM jsonb_to_record('{\"x\":1}') AS (x integer)",
        "CREATE VIEW fr.rf2 AS SELECT * FROM ROWS FROM (jsonb_to_record('{\"x\":1}') AS (x integer), \
         generate_series(1, 2)) WITH ORDINALITY AS r",
        "CREATE VIEW fr.co AS SELECT x FROM \
         jsonb_to_recordset('[{\"x\":\"b\"},{\"x\":\"B\"},{\"x\":\"a\"}]') AS t(x text COLLATE \"C\")",
    ] {
        w.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    let asked: &[(&str, &[&str])] = &[
        ("SELECT * FROM fr.rf", &["1|5", "2|6", "NULL|7"]),
        ("SELECT * FROM fr.na", &["1"]),
        ("SELECT * FROM fr.rf2", &["1|1|1", "NULL|2|2"]),
        (
            "SELECT x, pg_collation_for(x) FROM fr.co ORDER BY x",
            &["B|\"C\"", "a|\"C\"", "b|\"C\""],
        ),
    ];
    answered(&mut w, asked);
    drop(w);
    let mut w = Snail::open_writing(&path).unwrap();
    answered(&mut w, asked);
}

#[test]
fn the_other_sets_in_from() {
    let mut w = fr("others");
    answered(
        &mut w,
        &[
            ("SELECT * FROM string_to_table('a|b||c', '|')", &["a", "b", "", "c"]),
            ("SELECT * FROM string_to_table('a|b||c', '|', '')", &["a", "b", "NULL", "c"]),
            ("SELECT * FROM string_to_table(NULL, '|')", &[]),
            ("SELECT * FROM string_to_table('abc', NULL)", &["a", "b", "c"]),
            (
                "SELECT * FROM string_to_table('a,b', ',') WITH ORDINALITY AS t(x, n)",
                &["a|1", "b|2"],
            ),
            ("SELECT string_to_table('a|b', '|')", &["a", "b"]),
            ("SELECT * FROM ts_parse(3722, 'cat')", &["1|cat"]),
            ("SELECT * FROM ts_parse(3722::oid, 'cat 12')", &["1|cat", "12| ", "22|12"]),
            (
                "SELECT * FROM ts_token_type(3722) WHERE tokid <= 3",
                &[
                    "1|asciiword|Word, all ASCII",
                    "2|word|Word, all letters",
                    "3|numword|Word, letters and digits",
                ],
            ),
            ("SELECT * FROM ts_token_type(NULL::oid)", &[]),
            (
                "SELECT word, catcode, barelabel FROM pg_get_keywords() \
                 WHERE word IN ('all', 'select', 'zone') ORDER BY word",
                &["all|R|t", "select|R|t", "zone|U|t"],
            ),
            (
                "SELECT word, catdesc, baredesc FROM pg_get_keywords() WHERE word = 'zone'",
                &["zone|unreserved|can be bare label"],
            ),
            ("SELECT k.word FROM pg_get_keywords() AS k WHERE k.word = 'all'", &["all"]),
        ],
    );
    refusals(
        &mut w,
        &[
            (
                "SELECT * FROM ts_parse(9999, 'cat')",
                "XX000",
                "cache lookup failed for text search parser 9999",
            ),
            (
                "SELECT * FROM ts_parse('3722', 'cat')",
                "42704",
                "text search parser \"3722\" does not exist",
            ),
            (
                "SELECT * FROM ts_parse('nosuch', 'cat')",
                "42704",
                "text search parser \"nosuch\" does not exist",
            ),
            (
                "SELECT * FROM ts_token_type(9999)",
                "XX000",
                "cache lookup failed for text search parser 9999",
            ),
        ],
    );
    // 18.6 answers both: ts_stat's rows (b|1|1, a|1|1) and no channels. The snail refuses each in
    // its class's words: a query run by its text, and what only a server has.
    for (sql, words) in [
        (
            "SELECT * FROM ts_stat('SELECT ''a b a''::tsvector')",
            "ts_stat runs a query by its text: it is not read",
        ),
        (
            "SELECT * FROM pg_listening_channels()",
            "pg_listening_channels is the server's own (sessions): a snail answers only what its \
             file holds",
        ),
    ] {
        assert_eq!(refused(&mut w, sql).1, words, "{sql}");
    }
}

#[test]
fn every_abbreviation_of_the_set_is_read() {
    let mut w = fr("abbreviations");
    answered(
        &mut w,
        &[
            (
                "SELECT '2024-03-05 13:04 EST'::timestamptz, '2024-07-05 13:04 PDT'::timestamptz, \
                 '2024-07-05 13:04:05 MET DST'::timestamptz",
                &["2024-03-05 18:04:00+00|2024-07-05 20:04:00+00|2024-07-05 11:04:05+00"],
            ),
            (
                "SELECT '2024-07-05 13:04:05+00'::timestamptz AT TIME ZONE 'EST', \
                 timezone('PDT', '2024-07-05 13:04:05+00'::timestamptz)",
                &["2024-07-05 08:04:05|2024-07-05 06:04:05"],
            ),
            ("SELECT '13:04 EST'::timetz", &["13:04:00-05"]),
            (
                "SELECT to_timestamp('2024-07-05 13:04 pdt', 'YYYY-MM-DD HH24:MI TZ')",
                &["2024-07-05 20:04:00+00"],
            ),
            (
                "SELECT date_trunc('day', '2024-07-05 02:04+00'::timestamptz, 'PST'), \
                 make_timestamptz(2024, 7, 5, 13, 4, 5, 'PDT')",
                &["2024-07-04 08:00:00+00|2024-07-05 20:04:05+00"],
            ),
        ],
    );
    // One naming a zone is read by its rules under the feature zones: MSK, Moscow's, the meaning
    // it had at the time read (three hours east in the summer of 2010, when Moscow kept four); and
    // ART, which Buenos Aires's rules no longer name, as the zone's offset.
    let dynamic: &[(&str, &[&str])] = &[
        (
            "SELECT '2010-07-05 13:04 MSK'::timestamptz, '2012-01-05 13:04 MSK'::timestamptz, \
             '2024-07-05 13:04 ART'::timestamptz",
            &["2010-07-05 10:04:00+00|2012-01-05 09:04:00+00|2024-07-05 16:04:00+00"],
        ),
        (
            "SELECT '2024-07-05 13:04:05+00'::timestamptz AT TIME ZONE 'MSK', \
             '2024-07-05 13:04:05'::timestamp AT TIME ZONE 'msk'",
            &["2024-07-05 16:04:05|2024-07-05 10:04:05+00"],
        ),
        ("SELECT '2024-07-05 13:04 MSK'::timetz", &["13:04:00+03"]),
        (
            "SELECT to_timestamp('2010-07-05 13:04 MSK', 'YYYY-MM-DD HH24:MI TZ'), \
             make_timestamptz(2010, 7, 5, 13, 4, 5, 'MSK')",
            &["2010-07-05 10:04:00+00|2010-07-05 10:04:05+00"],
        ),
        (
            "SELECT abbrev, utc_offset, is_dst FROM pg_timezone_abbrevs_abbrevs() \
             WHERE abbrev IN ('EST', 'PDT', 'MSK', 'ART', 'CETDST', 'ZULU') ORDER BY abbrev",
            &[
                "ART|-03:00:00|f",
                "CETDST|02:00:00|t",
                "EST|-05:00:00|f",
                "MSK|03:00:00|f",
                "PDT|-07:00:00|t",
                "ZULU|00:00:00|f",
            ],
        ),
        ("SELECT count(*) FROM pg_timezone_abbrevs_abbrevs()", &["195"]),
    ];
    #[cfg(feature = "zones")]
    {
        answered(&mut w, dynamic);
        refusals(
            &mut w,
            &[(
                "SELECT '2024-07-05 13:04 MSK DST'::timestamptz",
                "22007",
                "invalid input syntax for type timestamp with time zone: \"2024-07-05 13:04 MSK \
                 DST\"",
            )],
        );
    }
    // Without it, refused by name.
    #[cfg(not(feature = "zones"))]
    for (sql, _) in dynamic {
        let words = refused(&mut w, sql).1;
        assert!(words.contains("read only under the feature zones"), "{sql}: {words}");
    }
}

#[cfg(feature = "zones")]
#[test]
fn the_session_zone_s_abbreviations_are_read_first() {
    let mut w = fr("zone-first");
    w.execute("SET TimeZone = 'Asia/Shanghai'").unwrap();
    answered(
        &mut w,
        &[(
            "SELECT '2024-07-05 13:04 CST'::timestamptz, \
             '2024-07-05 13:04:05+00'::timestamptz AT TIME ZONE 'CST'",
            &["2024-07-05 13:04:00+08|2024-07-05 21:04:05"],
        )],
    );
    w.execute("SET TimeZone = 'Europe/Dublin'").unwrap();
    answered(
        &mut w,
        &[(
            "SELECT '2024-01-05 13:04 IST'::timestamptz, '2024-07-05 13:04 IST'::timestamptz",
            &["2024-01-05 12:04:00+00|2024-07-05 13:04:00+01"],
        )],
    );
    w.execute("SET TimeZone = 'Europe/Lisbon'").unwrap();
    answered(
        &mut w,
        &[
            (
                "SELECT '1900-01-05 13:04 LMT'::timestamptz, '2024-07-05 13:04 WEST'::timestamptz",
                &["1900-01-05 13:04:00-00:36:45|2024-07-05 13:04:00+01"],
            ),
            (
                "SELECT * FROM pg_timezone_abbrevs_zone() ORDER BY abbrev",
                &[
                    "CEST|02:00:00|t",
                    "CET|01:00:00|f",
                    "LMT|-00:36:45|f",
                    "WEMT|02:00:00|t",
                    "WEST|01:00:00|t",
                    "WET|00:00:00|f",
                ],
            ),
        ],
    );
    w.execute("SET TimeZone = 'UTC'").unwrap();
    answered(
        &mut w,
        &[
            ("SELECT * FROM pg_timezone_abbrevs_zone()", &["UTC|00:00:00|f"]),
            (
                "SELECT name, utc_offset, is_dst FROM pg_timezone_names() \
                 WHERE name = 'Asia/Kolkata'",
                &["Asia/Kolkata|05:30:00|f"],
            ),
        ],
    );
    refusals(
        &mut w,
        &[(
            "SELECT * FROM pg_timezone_abbrevs()",
            "42883",
            no_function!("pg_timezone_abbrevs()"),
        )],
    );
}

/// A snail of case 12a's tables, holding none of its rows, written as release `release`'s.
#[cfg(feature = "zones")]
fn fr_of(name: &str, release: u8) -> Snail {
    let path = tmp(&format!("reader-fr-{name}.snail"));
    let mut catalog = Catalog::parse(FR_DDL).unwrap();
    catalog.with_release(release).unwrap();
    warren_snail::write::write_released(&path, &catalog, &[], &[], weather::span(), release)
        .unwrap();
    Snail::open_writing(&path).unwrap()
}

// Before 18 PostgreSQL reads its set alone: CST is the set's, six hours west, under any zone, and
// the set is pg_timezone_abbrevs()'s. No server of 14 to 17 is asked here: these are the snail's
// own, read from 18.6's source, which reads the session zone's own first from 18 on.
#[cfg(feature = "zones")]
#[test]
fn before_18_the_set_alone_is_read() {
    let mut w = fr_of("release-17", 17);
    answered(
        &mut w,
        &[(
            "SELECT abbrev, utc_offset FROM pg_timezone_abbrevs() WHERE abbrev = 'CST'",
            &["CST|-06:00:00"],
        )],
    );
    w.execute("SET TimeZone = 'Asia/Shanghai'").unwrap();
    answered(
        &mut w,
        &[("SELECT '2024-07-05 13:04 CST'::timestamptz", &["2024-07-06 03:04:00+08"])],
    );
    refusals(
        &mut w,
        &[(
            "SELECT * FROM pg_timezone_abbrevs_abbrevs()",
            "42883",
            no_function!("pg_timezone_abbrevs_abbrevs()"),
        )],
    );
}

// The views over those functions, read from their definitions for the file's release: from 18 on
// pg_timezone_abbrevs lists the statement zone's own, then the set's not among them.
#[cfg(feature = "zones")]
#[test]
fn the_zones_views_answer_as_postgresql_s() {
    let mut w = fr("zones-views");
    w.execute("SET TimeZone = 'UTC'").unwrap();
    answered(
        &mut w,
        &[
            (
                "SELECT abbrev, utc_offset, is_dst FROM pg_timezone_abbrevs \
                 WHERE abbrev IN ('UTC', 'EST', 'CET', 'MSK')",
                &["CET|01:00:00|f", "EST|-05:00:00|f", "MSK|03:00:00|f", "UTC|00:00:00|f"],
            ),
            (
                "SELECT name, abbrev FROM pg_timezone_names WHERE name IN ('UTC', 'Etc/GMT+5')",
                &["Etc/GMT+5|-05", "UTC|UTC"],
            ),
            (
                "SELECT name, abbrev, utc_offset, is_dst FROM pg_catalog.pg_timezone_names \
                 WHERE name = 'Asia/Kolkata'",
                &["Asia/Kolkata|IST|05:30:00|f"],
            ),
            ("SELECT count(*) FROM pg_timezone_abbrevs", &["195"]),
            ("SELECT abbrev FROM pg_timezone_abbrevs LIMIT 3", &["ACDT", "ACSST", "ACST"]),
        ],
    );
    w.execute("SET TimeZone = 'Europe/Lisbon'").unwrap();
    answered(
        &mut w,
        &[
            (
                "SELECT abbrev, utc_offset, is_dst FROM pg_timezone_abbrevs \
                 WHERE abbrev IN ('WET', 'WEST', 'LMT')",
                &["LMT|-00:36:45|f", "WEST|01:00:00|t", "WET|00:00:00|f"],
            ),
            ("SELECT count(*) FROM pg_timezone_abbrevs", &["198"]),
            ("SELECT count(*) FROM pg_timezone_abbrevs a WHERE abbrev = 'CET'", &["1"]),
        ],
    );
    w.execute("SET TimeZone = 'Asia/Shanghai'").unwrap();
    answered(
        &mut w,
        &[(
            "SELECT abbrev, utc_offset, is_dst FROM pg_timezone_abbrevs WHERE abbrev = 'CST'",
            &["CST|08:00:00|f"],
        )],
    );
    // Before 18 the view is the set's function alone: CST the set's under any zone (the snail's
    // own, as above).
    let mut w = fr_of("zones-views-17", 17);
    w.execute("SET TimeZone = 'Asia/Shanghai'").unwrap();
    answered(
        &mut w,
        &[(
            "SELECT abbrev, utc_offset, is_dst FROM pg_timezone_abbrevs WHERE abbrev = 'CST'",
            &["CST|-06:00:00|f"],
        )],
    );
}

// The select list's set-returning functions, case 12b: each set's values a row, in levels and in
// lockstep, the items beside the deepest made once more as its sets end, and every place 18.6
// refuses a set refused in its words.

const SR_DDL: &str = "CREATE SCHEMA sr;
     CREATE TABLE sr.t (id integer NOT NULL, n integer, tags text[], note text, PRIMARY KEY (id));
     CREATE SEQUENCE sr.q;";

const SR_ROWS: [&str; 1] = [
    "INSERT INTO sr.t VALUES (1, 2, '{a,b}', 'x1y22'), (2, 0, '{}', 'none'), (3, 3, NULL, 'z333'), \
     (4, NULL, '{c}', NULL)",
];

/// A snail holding case 12b's rows, at a path of the test's name.
fn sr(name: &str) -> Snail {
    let path = tmp(&format!("reader-sr-{name}.snail"));
    write(&path, &Catalog::parse(SR_DDL).unwrap(), &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    for sql in SR_ROWS {
        w.execute(sql).unwrap();
    }
    w
}

/// 18.6's refusal, with its HINT naming a LATERAL FROM item on a line of its own.
fn lateral(words: &str) -> String {
    format!(
        "{words}\nHINT:  You might be able to move the set-returning function into a LATERAL FROM \
         item."
    )
}

#[test]
fn a_set_in_the_select_list_gives_a_row_for_each_value() {
    let mut w = sr("each-value");
    answered(
        &mut w,
        &[
            ("SELECT generate_series(1, 3)", &["1", "2", "3"]),
            ("SELECT generate_series(1, 3) AS g, 'x' AS c", &["1|x", "2|x", "3|x"]),
            (
                "SELECT id, generate_series(1, n) AS k FROM sr.t ORDER BY id, k",
                &["1|1", "1|2", "3|1", "3|2", "3|3"],
            ),
            (
                "SELECT id, n * 10 + generate_series(1, n) AS k FROM sr.t ORDER BY id, k",
                &["1|21", "1|22", "3|31", "3|32", "3|33"],
            ),
            (
                "SELECT generate_series(1, 3) AS g, id FROM sr.t WHERE id < 3 ORDER BY id, g",
                &["1|1", "2|1", "3|1", "1|2", "2|2", "3|2"],
            ),
            ("SELECT generate_series(1, 4) AS g ORDER BY g DESC", &["4", "3", "2", "1"]),
            ("SELECT string_to_table('a,b', ',')", &["a", "b"]),
        ],
    );
}

#[test]
fn a_set_beside_an_aggregate_or_a_window_is_made_after_them() {
    let mut w = sr("after-folds");
    answered(
        &mut w,
        &[
            ("SELECT count(*), generate_series(1, 2) AS g FROM sr.t", &["4|1", "4|2"]),
            (
                "SELECT n, count(*), generate_series(1, 2) AS g FROM sr.t GROUP BY n ORDER BY n, g",
                &[
                    "0|1|1", "0|1|2", "2|1|1", "2|1|2", "3|1|1", "3|1|2", "NULL|1|1", "NULL|1|2",
                ],
            ),
            (
                "SELECT generate_series(1, count(*)::integer) AS g FROM sr.t",
                &["1", "2", "3", "4"],
            ),
            (
                "SELECT row_number() OVER (ORDER BY id) AS r, generate_series(1, 2) AS g FROM sr.t \
                 ORDER BY r, g",
                &["1|1", "1|2", "2|1", "2|2", "3|1", "3|2", "4|1", "4|2"],
            ),
        ],
    );
}

#[test]
fn a_set_under_distinct_a_limit_or_in_a_subquery() {
    let mut w = sr("under");
    answered(
        &mut w,
        &[
            ("SELECT count(*) FROM (SELECT generate_series(1, n) FROM sr.t) d", &["5"]),
            (
                "SELECT x FROM (SELECT generate_series(1, 3) AS x) d ORDER BY x DESC",
                &["3", "2", "1"],
            ),
            ("SELECT generate_series(1, 10) AS g LIMIT 3", &["1", "2", "3"]),
            ("SELECT generate_series(1, 10) AS g OFFSET 7", &["8", "9", "10"]),
            (
                "SELECT id, generate_series(1, 3) AS k FROM sr.t ORDER BY id, k LIMIT 4",
                &["1|1", "1|2", "1|3", "2|1"],
            ),
            ("SELECT DISTINCT unnest(ARRAY[3, 1, 3, 2]) AS v ORDER BY v", &["1", "2", "3"]),
            ("SELECT DISTINCT generate_series(1, n) AS k FROM sr.t ORDER BY k", &["1", "2", "3"]),
            ("SELECT count(DISTINCT x) FROM (SELECT unnest(ARRAY[1, 1, 2]) AS x) d", &["2"]),
            (
                "SELECT id FROM sr.t WHERE id IN (SELECT generate_series(1, 2)) ORDER BY id",
                &["1", "2"],
            ),
            ("SELECT (SELECT generate_series(1, 1)) AS one", &["1"]),
        ],
    );
}

#[test]
fn a_set_in_order_by_repeats_each_row() {
    let mut w = sr("order-by");
    answered(
        &mut w,
        &[
            (
                "SELECT id FROM sr.t ORDER BY generate_series(1, 2), id",
                &["1", "2", "3", "4", "1", "2", "3", "4"],
            ),
            (
                "SELECT n FROM sr.t GROUP BY n ORDER BY generate_series(1, 2), n",
                &["0", "2", "3", "NULL", "0", "2", "3", "NULL"],
            ),
        ],
    );
}

#[test]
fn sets_of_one_level_run_in_lockstep_the_shorter_padded_with_null() {
    let mut w = sr("lockstep");
    answered(
        &mut w,
        &[
            (
                "SELECT generate_series(1, 3) AS a, generate_series(1, 2) AS b",
                &["1|1", "2|2", "3|NULL"],
            ),
            (
                "SELECT generate_series(1, 2) AS a, unnest(ARRAY['x', 'y', 'z']) AS b",
                &["1|x", "2|y", "NULL|z"],
            ),
            (
                "SELECT generate_series(1, 0) AS a, generate_series(1, 2) AS b",
                &["NULL|1", "NULL|2"],
            ),
            ("SELECT generate_series(1, 0) AS a, unnest('{}'::integer[]) AS b", &[]),
            ("SELECT generate_series(1, 3) + generate_series(1, 2) AS s", &["2", "4", "NULL"]),
            (
                "SELECT id, unnest(tags) AS tag, generate_series(1, 2) AS k FROM sr.t ORDER BY id, k",
                &[
                    "1|a|1", "1|b|2", "2|NULL|1", "2|NULL|2", "3|NULL|1", "3|NULL|2", "4|c|1",
                    "4|NULL|2",
                ],
            ),
            (
                "SELECT id, pg_catalog.generate_series(1, 2) FROM sr.t WHERE id = 1",
                &["1|1", "1|2"],
            ),
        ],
    );
}

#[test]
fn a_nested_set_runs_once_for_each_value_beneath_it() {
    let mut w = sr("nested");
    answered(
        &mut w,
        &[
            (
                "SELECT generate_series(1, generate_series(1, 3)) AS g",
                &["1", "1", "2", "1", "2", "3"],
            ),
            (
                "SELECT generate_series(generate_series(1, 2), 3) AS g",
                &["1", "2", "3", "2", "3"],
            ),
            (
                "SELECT unnest(ARRAY[generate_series(1, 2), 10]) AS u",
                &["1", "10", "2", "10"],
            ),
            (
                "SELECT generate_series(1, generate_series(1, generate_series(1, 2))) AS g",
                &["1", "1", "1", "2"],
            ),
        ],
    );
}

#[test]
fn a_row_goes_where_a_deeper_set_gives_none() {
    let mut w = sr("row-goes");
    answered(
        &mut w,
        &[
            (
                "SELECT generate_series(1, generate_series(1, 2)) AS a, generate_series(1, 3) AS b",
                &["1|1", "1|2", "2|2"],
            ),
            (
                "SELECT generate_series(1, generate_series(1, 2)) + generate_series(10, 12) AS s",
                &["11", "12", "13"],
            ),
            (
                "SELECT id, generate_series(1, generate_series(0, n)) AS g FROM sr.t ORDER BY id, g",
                &["1|1", "1|1", "1|2", "3|1", "3|1", "3|1", "3|2", "3|2", "3|3"],
            ),
        ],
    );
}

#[test]
fn a_set_among_another_calls_arguments_is_read() {
    let mut w = sr("among");
    answered(
        &mut w,
        &[
            ("SELECT abs(generate_series(-2, 0)) AS a", &["2", "1", "0"]),
            ("SELECT (generate_series(1, 2) || 'x') AS t", &["1x", "2x"]),
            ("SELECT ARRAY[generate_series(1, 2), 10] AS a", &["{1,10}", "{2,10}"]),
            ("SELECT greatest(generate_series(1, 3), 2) AS g", &["2", "2", "3"]),
            ("SELECT nullif(generate_series(1, 3), 2) AS n", &["1", "NULL", "3"]),
            ("SELECT unnest(ARRAY['ab', 'cd']) LIKE 'a%' AS l", &["t", "f"]),
            ("SELECT unnest(ARRAY[1, NULL]) IS NULL AS n", &["f", "t"]),
            ("SELECT generate_series(1, 2)::text AS t", &["1", "2"]),
            ("SELECT ROW(generate_series(1, 2), 'x') AS r", &["(1,x)", "(2,x)"]),
            ("SELECT generate_series(1, 3) = ANY (ARRAY[2, 3]) AS a", &["f", "t", "t"]),
            ("SELECT 2 = ANY (ARRAY[generate_series(1, 3), 5]) AS a", &["f", "t", "f"]),
            ("SELECT 2 IN (generate_series(1, 3), 5) AS i", &["f", "t", "f"]),
            ("SELECT generate_series(1, 3) IN (1, 2) AS i", &["t", "t", "f"]),
            ("SELECT 2 NOT IN (generate_series(1, 3), 5) AS i", &["t", "f", "t"]),
            ("SELECT generate_series(1, 2) IS DISTINCT FROM 1 AS d", &["f", "t"]),
            ("SELECT (generate_series(1, 2), 1) IS DISTINCT FROM (1, 1) AS d", &["f", "t"]),
            ("SELECT (generate_series(1, 2), 1) IS NOT DISTINCT FROM (1, 1) AS d", &["t", "f"]),
        ],
    );
}

#[test]
fn the_items_beside_a_set_are_made_once_more_as_its_sets_end() {
    let mut w = sr("ending-pass");
    answered(
        &mut w,
        &[
            (
                "SELECT nextval('sr.q') AS v, generate_series(1, n) AS g FROM sr.t",
                &["1|1", "2|2", "5|1", "6|2", "7|3"],
            ),
            ("SELECT currval('sr.q')", &["9"]),
            (
                "SELECT nextval('sr.q') AS v, generate_series(1, 2) + 0 AS g FROM sr.t WHERE id = 1",
                &["10|1", "11|2"],
            ),
            ("SELECT currval('sr.q')", &["11"]),
            (
                "SELECT nextval('sr.q') AS v, generate_series(1, generate_series(1, 2)) AS g",
                &["12|1", "14|1", "15|2"],
            ),
            ("SELECT currval('sr.q')", &["16"]),
            (
                "SELECT nextval('sr.q') AS v, (generate_series(1, 2)) AS g FROM sr.t WHERE id = 1",
                &["17|1", "18|2"],
            ),
            ("SELECT currval('sr.q')", &["19"]),
            ("SELECT nextval('sr.q') AS v, generate_series(1, 0) FROM sr.t", &[]),
            ("SELECT currval('sr.q')", &["23"]),
            (
                "SELECT nextval('sr.q') AS v, generate_series(1, 2) AS g, \
                 generate_series(1, generate_series(1, 2)) + 0 AS h",
                &["24|1|1", "25|2|1", "26|2|2"],
            ),
            ("SELECT currval('sr.q')", &["26"]),
        ],
    );
    refusals(
        &mut w,
        &[
            (
                "SELECT 1 / (id - id) AS x, generate_series(1, 0) FROM sr.t",
                "22012",
                "division by zero",
            ),
            (
                "SELECT generate_series(1, 0) AS g, 1 / (id - id) AS x FROM sr.t",
                "22012",
                "division by zero",
            ),
        ],
    );
    answered(
        &mut w,
        &[("SELECT 1 / (id - id) + 0 * generate_series(1, 0) AS x FROM sr.t", &[])],
    );
}

#[test]
fn a_set_where_18_6_refuses_one_is_refused_in_its_words() {
    let mut w = sr("refused");
    let not_in = |clause: &str| format!("set-returning functions are not allowed in {clause}");
    let argument = |of: &str| format!("argument of {of} must not return a set");
    let row = "row comparison operator must not return a set".to_string();
    let aggregate = lateral("aggregate function calls cannot contain set-returning function calls");
    let asked: Vec<(&str, &str, String)> = vec![
        ("SELECT n FROM sr.t GROUP BY n HAVING generate_series(1, 2) > 1", "0A000", not_in("HAVING")),
        ("SELECT sum(generate_series(1, 3))", "0A000", aggregate.clone()),
        (
            "SELECT count(*) FILTER (WHERE generate_series(1, 2) > 1) FROM sr.t",
            "0A000",
            not_in("FILTER"),
        ),
        (
            "SELECT lag(generate_series(1, 2)) OVER ()",
            "0A000",
            lateral("window function calls cannot contain set-returning function calls"),
        ),
        (
            "SELECT CASE WHEN id > 1 THEN generate_series(1, 2) END FROM sr.t",
            "0A000",
            lateral(&not_in("CASE")),
        ),
        ("SELECT COALESCE(generate_series(1, 2), 0)", "0A000", lateral(&not_in("COALESCE"))),
        ("SELECT id FROM sr.t WHERE id = generate_series(1, 2)", "0A000", not_in("WHERE")),
        (
            "SELECT (SELECT generate_series(1, 2)) AS two",
            "21000",
            "more than one row returned by a subquery used as an expression".into(),
        ),
        (
            "SELECT sum(generate_series(1, 3)) FILTER (WHERE generate_series(1, 2) > 1)",
            "0A000",
            not_in("FILTER"),
        ),
        (
            "SELECT array_agg(id ORDER BY generate_series(1, 2)) FROM sr.t",
            "0A000",
            aggregate.clone(),
        ),
        (
            "SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY generate_series(1, 3))",
            "0A000",
            aggregate.clone(),
        ),
        (
            "SELECT a.id FROM sr.t a JOIN sr.t b ON a.id = generate_series(1, 2)",
            "0A000",
            not_in("JOIN conditions"),
        ),
        ("SELECT id FROM sr.t LIMIT generate_series(1, 2)", "0A000", not_in("LIMIT")),
        ("SELECT id FROM sr.t OFFSET generate_series(1, 2)", "0A000", not_in("OFFSET")),
        (
            "SELECT count(*) OVER (ORDER BY id ROWS generate_series(1, 2) PRECEDING) FROM sr.t",
            "0A000",
            not_in("window definitions"),
        ),
        (
            "SELECT * FROM (VALUES (generate_series(1, 2)), (3)) v",
            "0A000",
            not_in("VALUES"),
        ),
        ("SELECT generate_series(1, 2) > 1 AND true AS b", "42804", argument("AND")),
        ("SELECT generate_series(1, 2) > 1 OR true AS b", "42804", argument("OR")),
        ("SELECT NOT generate_series(1, 2) > 1 AS b", "42804", argument("NOT")),
        ("SELECT (generate_series(1, 2) > 1) IS TRUE AS b", "42804", argument("IS TRUE")),
        ("SELECT (generate_series(1, 2) > 1) IS NOT TRUE AS b", "42804", argument("IS NOT TRUE")),
        ("SELECT (generate_series(1, 2) > 1) IS FALSE AS b", "42804", argument("IS FALSE")),
        (
            "SELECT (generate_series(1, 2) > 1) IS NOT FALSE AS b",
            "42804",
            argument("IS NOT FALSE"),
        ),
        ("SELECT (generate_series(1, 2) > 1) IS UNKNOWN AS b", "42804", argument("IS UNKNOWN")),
        (
            "SELECT (generate_series(1, 2) > 1) IS NOT UNKNOWN AS b",
            "42804",
            argument("IS NOT UNKNOWN"),
        ),
        ("SELECT generate_series(1, 3) BETWEEN 1 AND 2 AS b", "42804", argument("AND")),
        ("SELECT generate_series(1, 3) NOT BETWEEN 1 AND 2 AS b", "42804", argument("OR")),
        (
            "SELECT CASE WHEN generate_series(1, 2) > 1 THEN 1 END AS c",
            "42804",
            argument("CASE/WHEN"),
        ),
        (
            "SELECT CASE generate_series(1, 2) WHEN 1 THEN 'a' END AS c",
            "0A000",
            lateral(&not_in("CASE")),
        ),
        ("SELECT (generate_series(1, 2), 1) = (1, 1) AS b", "42804", row.clone()),
        ("SELECT (generate_series(1, 2), 1) <> (1, 1) AS b", "42804", row.clone()),
        ("SELECT ROW(generate_series(1, 2), 1) < ROW(2, 2) AS b", "42804", row.clone()),
        ("SELECT (generate_series(1, 2), 1) IN ((1, 1), (2, 2)) AS i", "42804", row.clone()),
        ("SELECT generate_series(1, 3) IN (SELECT 2) AS i", "42804", row.clone()),
        ("SELECT generate_series(1, 3) IN (1) AS b", "42804", argument("IN")),
        ("SELECT 2 IN (generate_series(1, 3), id) AS b FROM sr.t", "42804", argument("IN")),
        ("SELECT generate_series(1, 3) IN (id, 5) AS b FROM sr.t", "42804", argument("IN")),
        ("SELECT 1 IN (generate_series(1, 2), 1) AND true AS b", "42804", argument("AND")),
        (
            "SELECT id, generate_series(1, 2) FROM sr.t FOR UPDATE",
            "0A000",
            "FOR UPDATE is not allowed with set-returning functions in the target list".into(),
        ),
        ("UPDATE sr.t SET n = generate_series(1, 2) WHERE id = 1", "0A000", not_in("UPDATE")),
        (
            "UPDATE sr.t SET n = 1 WHERE id = 1 RETURNING generate_series(1, 2)",
            "0A000",
            not_in("RETURNING"),
        ),
        ("UPDATE sr.t SET n = 1 WHERE id = generate_series(1, 2)", "0A000", not_in("WHERE")),
        (
            "DELETE FROM sr.t WHERE id = 99 RETURNING generate_series(1, 2)",
            "0A000",
            not_in("RETURNING"),
        ),
        ("DELETE FROM sr.t WHERE id = generate_series(1, 2)", "0A000", not_in("WHERE")),
        (
            "INSERT INTO sr.t VALUES (99, 1) RETURNING generate_series(1, 2)",
            "0A000",
            not_in("RETURNING"),
        ),
        (
            "INSERT INTO sr.t VALUES (10, generate_series(1, 2)), (11, 1)",
            "0A000",
            not_in("VALUES"),
        ),
        // A set-returning function the snail does not read, where 18.6 refuses any.
        ("SELECT id FROM sr.t WHERE pg_listening_channels() IS NULL", "0A000", not_in("WHERE")),
        (
            "SELECT n FROM sr.t GROUP BY n HAVING count(*) > 0 AND ts_stat('SELECT 1') IS NULL",
            "0A000",
            not_in("HAVING"),
        ),
        (
            "SELECT id FROM sr.t LIMIT pg_listening_channels()::text::integer",
            "0A000",
            not_in("LIMIT"),
        ),
        (
            "UPDATE sr.t SET note = pg_listening_channels()::text WHERE id = 1",
            "0A000",
            not_in("UPDATE"),
        ),
        // The first refused in 18.6's parser's order: the FROM, the select list, WHERE, HAVING,
        // ORDER BY; a write's WHERE, RETURNING, then the values it sets.
        (
            "SELECT CASE WHEN true THEN generate_series(1, 2) END FROM sr.t \
             WHERE id = generate_series(1, 2)",
            "0A000",
            lateral(&not_in("CASE")),
        ),
        (
            "SELECT id FROM sr.t WHERE id = generate_series(1, 2) \
             ORDER BY sum(generate_series(1, 3))",
            "0A000",
            not_in("WHERE"),
        ),
        (
            "SELECT n FROM sr.t GROUP BY n HAVING generate_series(1, 2) > 1 \
             ORDER BY sum(generate_series(1, 2))",
            "0A000",
            not_in("HAVING"),
        ),
        (
            "SELECT CASE WHEN true THEN generate_series(1, 2) END FROM sr.t a \
             JOIN sr.t b ON a.id = generate_series(1, 2)",
            "0A000",
            not_in("JOIN conditions"),
        ),
        (
            "UPDATE sr.t SET n = generate_series(1, 2) WHERE id = 1 RETURNING generate_series(1, 2)",
            "0A000",
            not_in("RETURNING"),
        ),
    ];
    for (sql, code, words) in &asked {
        assert_eq!(refused(&mut w, sql), (Some(*code), words.clone()), "{sql}");
    }
    answered(&mut w, &[("SELECT count(*) FROM sr.t", &["4"])]);
}

#[test]
fn a_set_in_group_by_is_made_before_the_groups() {
    let mut w = sr("group-by-set");
    answered(
        &mut w,
        &[
            ("SELECT generate_series(1, 3) AS g GROUP BY 1 ORDER BY 1", &["1", "2", "3"]),
            (
                "SELECT generate_series(1, 2) AS g, count(*) FROM sr.t GROUP BY 1 ORDER BY 1",
                &["1|4", "2|4"],
            ),
            (
                "SELECT generate_series(1, 3) AS g, generate_series(1, 2) AS h FROM sr.t GROUP BY 1 \
                 ORDER BY 1, 2",
                &["1|1", "1|2", "2|1", "2|2", "3|1", "3|2"],
            ),
            (
                "SELECT (generate_series(1, 3) + 0) * 10 AS x FROM sr.t \
                 GROUP BY generate_series(1, 3) + 0 ORDER BY 1",
                &["10", "20", "30"],
            ),
            (
                "SELECT generate_series(1, 3) AS g FROM sr.t GROUP BY generate_series(1, 3) + 0 \
                 ORDER BY 1",
                &["1", "1", "1", "2", "2", "2", "3", "3", "3"],
            ),
            (
                "SELECT n, generate_series(1, n) AS k, count(*) FROM sr.t GROUP BY n, k \
                 ORDER BY n, k",
                &["2|1|1", "2|2|1", "3|1|1", "3|2|1", "3|3|1"],
            ),
            (
                "SELECT generate_series(1, 2) AS g, count(*) FROM sr.t \
                 GROUP BY generate_series(1, 2) ORDER BY 1",
                &["1|4", "2|4"],
            ),
            (
                "SELECT n, generate_series(1, 2) AS g, sum(id) FROM sr.t \
                 GROUP BY n, generate_series(1, 2) ORDER BY 1, 2",
                &["0|1|2", "0|2|2", "2|1|1", "2|2|1", "3|1|3", "3|2|3", "NULL|1|4", "NULL|2|4"],
            ),
            (
                "SELECT generate_series(1, generate_series(1, 2)) AS g, count(*) FROM sr.t \
                 GROUP BY 1 ORDER BY 1",
                &["1|8", "2|4"],
            ),
            (
                "SELECT count(*) FROM sr.t GROUP BY generate_series(1, 2), generate_series(1, 3) \
                 ORDER BY 1",
                &["4", "4", "4"],
            ),
            (
                "SELECT g, count(*) FROM (SELECT generate_series(1, n) AS g FROM sr.t GROUP BY 1, n) \
                 s GROUP BY g ORDER BY g",
                &["1|2", "2|2", "3|1"],
            ),
        ],
    );
}

#[test]
fn a_set_in_a_windows_keys_is_made_before_the_window() {
    let mut w = sr("window-key-set");
    answered(
        &mut w,
        &[
            (
                "SELECT generate_series(1, 2) AS g, id, row_number() OVER (PARTITION BY \
                 generate_series(1, 2) ORDER BY id) AS r FROM sr.t ORDER BY 1, 2",
                &[
                    "1|1|1", "1|2|2", "1|3|3", "1|4|4", "2|1|1", "2|2|2", "2|3|3", "2|4|4",
                ],
            ),
            (
                "SELECT id, generate_series(1, 2) AS g, generate_series(1, 3) AS h, row_number() \
                 OVER (PARTITION BY generate_series(1, 2) ORDER BY id) AS r FROM sr.t WHERE id = 1 \
                 ORDER BY 2, 3",
                &["1|1|1|1", "1|1|2|1", "1|1|3|1", "1|2|1|1", "1|2|2|1", "1|2|3|1"],
            ),
            (
                "SELECT id, row_number() OVER (ORDER BY generate_series(1, 2), id) AS r FROM sr.t \
                 ORDER BY 2",
                &["1|1", "2|2", "3|3", "4|4", "1|5", "2|6", "3|7", "4|8"],
            ),
            (
                "SELECT id, count(*) OVER (PARTITION BY generate_series(1, n)) AS c FROM sr.t \
                 ORDER BY 1, 2",
                &["1|2", "1|2", "3|1", "3|2", "3|2"],
            ),
        ],
    );
}

#[test]
fn a_set_no_sort_key_holds_is_made_after_the_sort() {
    let mut w = sr("after-sort");
    let drawn = "SELECT nextval('sr.q') AS v, generate_series(1, 2) AS g FROM sr.t";
    let at = |tail: &str| format!("{drawn} {tail}");
    let asked: Vec<(String, &[&str])> = vec![
        (
            "SELECT id, generate_series(1, 10 / (4 - id)) AS g FROM sr.t ORDER BY id LIMIT 2".into(),
            &["1|1", "1|2"],
        ),
        (at("ORDER BY id DESC"), &["1|1", "2|2", "4|1", "5|2", "7|1", "8|2", "10|1", "11|2"]),
        ("SELECT currval('sr.q')".into(), &["12"]),
        (
            at("ORDER BY id DESC, g"),
            &["13|1", "14|2", "15|1", "16|2", "17|1", "18|2", "19|1", "20|2"],
        ),
        ("SELECT currval('sr.q')".into(), &["20"]),
        (at("ORDER BY id LIMIT 3"), &["21|1", "22|2", "24|1"]),
        ("SELECT currval('sr.q')".into(), &["24"]),
        (at("ORDER BY id OFFSET 5"), &["32|2", "34|1", "35|2"]),
        ("SELECT currval('sr.q')".into(), &["36"]),
        (at("ORDER BY id FETCH FIRST 1 ROW WITH TIES"), &["37|1", "38|2"]),
        ("SELECT currval('sr.q')".into(), &["40"]),
        (at("ORDER BY id LIMIT 2"), &["41|1", "42|2"]),
        ("SELECT currval('sr.q')".into(), &["42"]),
        (at("LIMIT 3"), &["43|1", "44|2", "46|1"]),
        ("SELECT currval('sr.q')".into(), &["46"]),
        (
            "SELECT id, generate_series(1, 2) * 10 AS g FROM sr.t ORDER BY id LIMIT 3".into(),
            &["1|10", "1|20", "2|10"],
        ),
        (
            "SELECT id, generate_series(1, generate_series(1, 2)) AS g FROM sr.t ORDER BY id \
             LIMIT 3"
                .into(),
            &["1|1", "1|1", "1|2"],
        ),
        (
            "SELECT id, generate_series(1, n) AS g FROM sr.t ORDER BY id OFFSET 1 LIMIT 3".into(),
            &["1|2", "3|1", "3|2"],
        ),
    ];
    let asked: Vec<(&str, &[&str])> = asked.iter().map(|(q, a)| (q.as_str(), *a)).collect();
    answered(&mut w, &asked);
}

#[test]
fn a_grouped_or_windowed_step_makes_its_sets_after_the_sort() {
    let mut w = sr("grouped-after-sort");
    answered(
        &mut w,
        &[
            (
                "SELECT n, generate_series(1, 10 / (3 - n)) AS g FROM sr.t GROUP BY n ORDER BY n \
                 LIMIT 2",
                &["0|1", "0|2"],
            ),
            (
                "SELECT id, row_number() OVER (ORDER BY id) AS r, generate_series(1, 10 / (4 - id)) \
                 AS g FROM sr.t ORDER BY id LIMIT 2",
                &["1|1|1", "1|1|2"],
            ),
            (
                "SELECT n, nextval('sr.q') AS v, generate_series(1, 2) AS g FROM sr.t GROUP BY n \
                 ORDER BY n",
                &["0|1|1", "0|2|2", "2|4|1", "2|5|2", "3|7|1", "3|8|2", "NULL|10|1", "NULL|11|2"],
            ),
            ("SELECT currval('sr.q')", &["12"]),
            (
                "SELECT n, nextval('sr.q') AS v FROM sr.t GROUP BY n ORDER BY n DESC NULLS LAST",
                &["3|13", "2|14", "0|15", "NULL|16"],
            ),
            (
                "SELECT n, generate_series(1, count(*)::integer + 1) AS g FROM sr.t GROUP BY n \
                 ORDER BY n LIMIT 3",
                &["0|1", "0|2", "2|1"],
            ),
            (
                "SELECT id, generate_series(1, (row_number() OVER (ORDER BY id))::integer) AS g \
                 FROM sr.t ORDER BY id DESC LIMIT 3",
                &["4|1", "4|2", "4|3"],
            ),
            (
                "SELECT id, nextval('sr.q') + count(*) OVER () AS v FROM sr.t ORDER BY id DESC",
                &["4|21", "3|22", "2|23", "1|24"],
            ),
        ],
    );
}

#[test]
fn a_long_set_is_cut_without_being_made_whole() {
    let mut w = sr("long-set");
    let began = std::time::Instant::now();
    answered(
        &mut w,
        &[
            ("SELECT generate_series(1, 100000000) LIMIT 3", &["1", "2", "3"]),
            ("SELECT generate_series(1, 100000000) + 0 AS g LIMIT 3", &["1", "2", "3"]),
            (
                "SELECT id, generate_series(1, 100000000) AS g FROM sr.t ORDER BY id LIMIT 3",
                &["1|1", "1|2", "1|3"],
            ),
            (
                "SELECT generate_series(1, generate_series(1, 100000000)) AS g LIMIT 4",
                &["1", "1", "2", "1"],
            ),
            (
                "SELECT generate_series(1, 100000000) AS g, generate_series(1, 2) AS h LIMIT 3",
                &["1|1", "2|2", "3|NULL"],
            ),
            ("SELECT generate_series(1, 100000000) AS g OFFSET 5 LIMIT 2", &["6", "7"]),
        ],
    );
    assert!(began.elapsed() < std::time::Duration::from_secs(1), "{:?}", began.elapsed());
}

/// A snail of one table and no rows, written as release `release`'s.
fn keyed_of(name: &str, release: u8) -> Snail {
    let path = tmp(&format!("reader-keywords-{name}.snail"));
    let mut catalog = Catalog::parse("CREATE TABLE k (id integer PRIMARY KEY);").unwrap();
    catalog.with_release(release).unwrap();
    warren_snail::write::write_released(&path, &catalog, &[], &[], weather::span(), release)
        .unwrap();
    Snail::open_writing(&path).unwrap()
}

// 18.6 over a file of 18, 19 RC1 over one of 19, the same statements in the same order: 19's list
// holds five keywords more, `ignore` and `respect` labels only after AS; a keyword either list says
// wants AS is refused as a bare label under both.
#[test]
fn the_keywords_are_the_file_s_release_s() {
    let five = "SELECT word, catcode, barelabel FROM pg_get_keywords() \
         WHERE word IN ('ignore', 'lsn', 'repack', 'respect', 'wait') ORDER BY word";
    let syntax = |w: &mut Snail, sql: &str, word: &str| {
        let near = format!("syntax error at or near \"{word}\"");
        assert_eq!(refused(w, sql), (Some("42601"), near), "{sql}");
    };
    let bare = ["year", "filter", "day", "within"];
    let mut w = keyed_of("18", 18);
    answered(
        &mut w,
        &[
            ("SELECT count(*) FROM pg_get_keywords()", &["494"]),
            (five, &[]),
            ("SELECT 1 ignore", &["1"]),
            ("SELECT 1 respect", &["1"]),
            ("SELECT 1 wait", &["1"]),
            ("SELECT quote_ident('wait') AS a, quote_ident('ignore') AS b", &["wait|ignore"]),
            ("SELECT 1 AS ignore", &["1"]),
            ("SELECT 1 lsn", &["1"]),
        ],
    );
    for word in bare {
        syntax(&mut w, &format!("SELECT 1 {word}"), word);
    }
    answered(&mut w, &[("SELECT x ignore FROM (SELECT 1 AS x) q", &["1"])]);
    // A name, so a zone's: without `zones` the snail reads UTC alone.
    #[cfg(feature = "zones")]
    refusals(
        &mut w,
        &[
            ("SET TIME ZONE wait", "22023", "invalid value for parameter \"TimeZone\": \"wait\""),
            ("SET TIME ZONE ignore", "22023", "invalid value for parameter \"TimeZone\": \"ignore\""),
        ],
    );
    let mut w = keyed_of("19", 19);
    answered(
        &mut w,
        &[
            ("SELECT count(*) FROM pg_get_keywords()", &["499"]),
            (five, &["ignore|U|f", "lsn|U|t", "repack|U|t", "respect|U|f", "wait|U|t"]),
        ],
    );
    syntax(&mut w, "SELECT 1 ignore", "ignore");
    syntax(&mut w, "SELECT 1 respect", "respect");
    answered(
        &mut w,
        &[
            ("SELECT 1 wait", &["1"]),
            ("SELECT quote_ident('wait') AS a, quote_ident('ignore') AS b", &["wait|ignore"]),
            ("SELECT 1 AS ignore", &["1"]),
            ("SELECT 1 lsn", &["1"]),
        ],
    );
    for word in bare {
        syntax(&mut w, &format!("SELECT 1 {word}"), word);
    }
    syntax(&mut w, "SELECT x ignore FROM (SELECT 1 AS x) q", "ignore");
    syntax(&mut w, "SET TIME ZONE wait", "wait");
    syntax(&mut w, "SET TIME ZONE ignore", "ignore");
}

// After setseed, 18.6's values, drawn in its order: WHERE's for a row before the select list's, an
// item no sort key after the sort for each row the LIMIT reads, its OFFSET's and the row a WITH TIES
// compares among them; a WHERE read no further than the last row its LIMIT keeps; a subquery of no
// column once. setseed answers void, written as nothing.
#[test]
fn random_draws_as_postgresql_draws_after_setseed() {
    let mut w = snail("random");
    let seeded = |w: &mut Snail, seed: &str| {
        assert_eq!(rows(w, &format!("SELECT setseed({seed})")), [""], "setseed({seed})");
    };
    seeded(&mut w, "0.5");
    answered(
        &mut w,
        &[("SELECT random(), random()", &["0.9851677175347999|0.825301858027981"])],
    );
    seeded(&mut w, "0.5");
    answered(
        &mut w,
        &[(
            "SELECT random(1, 10), random(1::bigint, 10::bigint), random(1.5, 2.5), \
             random_normal(), random_normal(10, 2)",
            &["3|3|2.5|-1.5976119949210055|10.106215605031105"],
        )],
    );
    seeded(&mut w, "-1");
    answered(
        &mut w,
        &[(
            "SELECT random() FROM generate_series(1, 3)",
            &["0.725656831544149", "0.21342431605981593", "0.08668744483804192"],
        )],
    );
    seeded(&mut w, "0");
    answered(
        &mut w,
        &[
            (
                "SELECT random(0.000, 1.0), random(-5, 5), random(1, 1), random(5.5, 5.5), \
                 random(0, 9223372036854775807), random(-2147483648, 2147483647)",
                &["0.891|1|1|5.5|6165647961682858565|128965569"],
            ),
            ("SELECT random(1.25, 9999999.123456)", &["6924021.826976"]),
            ("SELECT random(-1.5, 100000000000000000000.5)", &["50034793462446462500.2"]),
        ],
    );
    seeded(&mut w, "0.25");
    answered(
        &mut w,
        &[
            (
                "SELECT random(0, 0.001), random(0, 99999999), random(0.0001, 0.0002), \
                 random(123.45678, 98765.4321)",
                &["0.000|28257152|0.0001|59189.93442"],
            ),
            (
                "SELECT random(1, '2'), random(1::smallint, 2), random_normal(1), \
                 random_normal(0, -1)",
                &["1|1|0.5226302289302078|0.3021904140852716"],
            ),
            ("SELECT random() BETWEEN 0 AND 1, random() >= 0 AND random() < 1", &["t|t"]),
            (
                "SELECT pg_typeof(random()), pg_typeof(random(1, 2)), pg_typeof(random(1::bigint, \
                 2)), pg_typeof(random(1, 2.0)), pg_typeof(random_normal()), \
                 pg_typeof(setseed(0))",
                &["double precision|integer|bigint|numeric|double precision|void"],
            ),
            (
                "SELECT setseed(0) IS NULL, setseed(0)::text, ''::void, pg_typeof('x'::void)",
                &["f|||void"],
            ),
            (
                "SELECT setseed(NULL), random(NULL, 1), random_normal(NULL, 1), random(1, \
                 NULL::bigint)",
                &["NULL|NULL|NULL|NULL"],
            ),
            ("SELECT setseed(0.5), random()", &["|0.9851677175347999"]),
            (
                "SELECT random(), setseed(0.5), random()",
                &["0.825301858027981||0.9851677175347999"],
            ),
        ],
    );
    let first = "0.9851677175347999";
    let third = "0.12974610012450416";
    let fourth = "0.16356291958601088";
    let fifth = "0.6476186144084";
    for (sql, want, after) in [
        (
            "SELECT random() AS r FROM generate_series(1, 3) g ORDER BY g DESC",
            vec![first, "0.825301858027981", third],
            None,
        ),
        (
            "SELECT g, random() AS r FROM generate_series(1, 4) g ORDER BY g DESC LIMIT 2 OFFSET 1",
            vec!["3|0.825301858027981", "2|0.12974610012450416"],
            Some(fourth),
        ),
        (
            "SELECT random() AS r, g FROM generate_series(1, 3) g ORDER BY r",
            vec!["0.12974610012450416|3", "0.825301858027981|2", "0.9851677175347999|1"],
            None,
        ),
        (
            "SELECT random() FROM generate_series(1, 2) g WHERE random() < 2",
            vec!["0.825301858027981", fourth],
            None,
        ),
        (
            "SELECT random() FROM generate_series(1, 5) g WHERE random() < 2 LIMIT 2",
            vec!["0.825301858027981", fourth],
            Some(fifth),
        ),
        (
            "SELECT g FROM generate_series(1, 5) g WHERE random() < 0.5 LIMIT 1",
            vec!["3"],
            Some(fourth),
        ),
        (
            "SELECT g / 2 AS h, random() FROM generate_series(1, 6) g ORDER BY g / 2 FETCH FIRST \
             2 ROWS WITH TIES",
            vec!["0|0.9851677175347999", "1|0.825301858027981", "1|0.12974610012450416"],
            Some(fifth),
        ),
        ("SELECT random() FROM generate_series(1, 3) LIMIT 0 OFFSET 1", vec![], Some(first)),
        (
            "SELECT DISTINCT g % 2 AS k, random() < 2 AS b FROM generate_series(1, 4) g ORDER BY 1",
            vec!["0|t", "1|t"],
            Some(fifth),
        ),
        (
            "SELECT id, random() FROM dn.a WHERE random() < 2 ORDER BY id DESC",
            vec![
                "7|0.15619865764623442",
                "6|0.5145227426983392",
                "5|0.7712969548127826",
                "4|0.48403393317276766",
                "3|0.9709894670705799",
                "2|0.45481208559326447",
                "1|0.5385999182909114",
            ],
            Some("0.7495564594841835"),
        ),
        ("SELECT id FROM dn.a WHERE random() < 2 LIMIT 2", vec!["1", "2"], Some(third)),
        (
            "SELECT id, (SELECT random()) FROM dn.a ORDER BY id DESC LIMIT 2",
            vec!["7|0.9851677175347999", "6|0.9851677175347999"],
            None,
        ),
        (
            "WITH w AS (SELECT random() AS x FROM generate_series(1, 2)) SELECT x FROM w",
            vec![first, "0.825301858027981"],
            None,
        ),
    ] {
        seeded(&mut w, "0.5");
        assert_eq!(rows(&mut w, sql), want, "{sql}");
        if let Some(after) = after {
            assert_eq!(rows(&mut w, "SELECT random()"), [after], "after {sql}");
        }
    }
    seeded(&mut w, "0.5");
    w.execute("INSERT INTO dn.b SELECT g + 10, 1, random(1, 100) FROM generate_series(1, 3) g")
        .unwrap();
    let written = "SELECT id, v FROM dn.b WHERE id > 10 ORDER BY id";
    answered(&mut w, &[(written, &["11|17", "12|21", "13|83"])]);
    w.execute("UPDATE dn.b SET v = random(1, 100) WHERE id > 10")
        .unwrap();
    answered(&mut w, &[(written, &["11|18", "12|20", "13|66"])]);
    refusals(
        &mut w,
        &[
            ("SELECT setseed(2)", "22023", "setseed parameter 2 is out of allowed range [-1,1]"),
            ("SELECT setseed('NaN')", "22023", "setseed parameter NaN is out of allowed range [-1,1]"),
            ("SELECT setseed(-1.5)", "22023", "setseed parameter -1.5 is out of allowed range [-1,1]"),
            (
                "SELECT setseed(1.00000001)",
                "22023",
                "setseed parameter 1 is out of allowed range [-1,1]",
            ),
            ("SELECT random(10, 1)", "22023", "lower bound must be less than or equal to upper bound"),
            ("SELECT random(2.5, 1.5)", "22023", "lower bound must be less than or equal to upper bound"),
            (
                "SELECT random(10::bigint, 1::bigint)",
                "22023",
                "lower bound must be less than or equal to upper bound",
            ),
            ("SELECT random('NaN'::numeric, 1)", "22023", "lower bound cannot be NaN"),
            ("SELECT random(1, 'NaN'::numeric)", "22023", "upper bound cannot be NaN"),
            ("SELECT random('-Infinity'::numeric, 1)", "22023", "lower bound cannot be infinity"),
            ("SELECT random(1, 'Infinity'::numeric)", "22023", "upper bound cannot be infinity"),
            ("SELECT random('1', '2')", "42725", "function random(unknown, unknown) is not unique"),
            (
                "SELECT random(1::smallint, 2::smallint)",
                "42725",
                "function random(smallint, smallint) is not unique",
            ),
            (
                "SELECT random(1::smallint, '2')",
                "42725",
                "function random(smallint, unknown) is not unique",
            ),
            (
                "SELECT random(1.5::float8, 2)",
                "42883",
                "function random(double precision, integer) does not exist\nHINT:  No function \
                 matches the given name and argument types. You might need to add explicit type \
                 casts.",
            ),
            (
                "SELECT random(1)",
                "42883",
                "function random(integer) does not exist\nHINT:  No function matches the given \
                 name and argument types. You might need to add explicit type casts.",
            ),
            (
                "SELECT random_normal('x')",
                "22P02",
                "invalid input syntax for type double precision: \"x\"",
            ),
            ("SELECT setseed(0) = setseed(0)", "42883", "operator does not exist: void = void"),
            ("SELECT 1::void", "42846", "cannot cast type integer to void"),
        ],
    );
    // Beside grouping, a window or a set, drawn after the sort in the sorted order, each row the
    // cut reads, the ending pass's draw among them; the draw after each 18.6's.
    for (sql, want, after) in [
        (
            "SELECT g % 2 AS k, random() FROM generate_series(1, 4) g GROUP BY g % 2 ORDER BY 1",
            vec!["0|0.9851677175347999", "1|0.825301858027981"],
            "0.12974610012450416",
        ),
        (
            "SELECT id, random(), row_number() OVER (ORDER BY id) FROM dn.a ORDER BY id LIMIT 2",
            vec!["1|0.9851677175347999|1", "2|0.825301858027981|2"],
            "0.12974610012450416",
        ),
        (
            "SELECT generate_series(1, 2) AS s, random() ORDER BY 1",
            vec!["1|0.9851677175347999", "2|0.825301858027981"],
            "0.12974610012450416",
        ),
        (
            "SELECT g % 2 AS k, random() FROM generate_series(1, 4) g GROUP BY g % 2 ORDER BY 1 \
             DESC LIMIT 1",
            vec!["1|0.9851677175347999"],
            "0.825301858027981",
        ),
        (
            "SELECT id, random_normal() AS r FROM dn.a ORDER BY id DESC LIMIT 2",
            vec!["7|2.5832426701605056", "6|-0.45134209986141144"],
            "0.6476186144084",
        ),
        (
            "SELECT generate_series(1, 2) AS s, random() AS r FROM dn.a WHERE id < 3 ORDER BY id \
             DESC LIMIT 3",
            vec!["1|0.9851677175347999", "2|0.825301858027981", "1|0.16356291958601088"],
            "0.6476186144084",
        ),
    ] {
        seeded(&mut w, "0.5");
        assert_eq!(rows(&mut w, sql), want, "{sql}");
        assert_eq!(rows(&mut w, "SELECT random()"), [after], "after {sql}");
    }
}

// void, what setseed answers, has no operator, no equality and no ordering: each comparison, each
// DISTINCT, grouping and UNION refused as 18.6's parser refuses it, before any row, over a table of
// no rows and of one alike. IS NULL and UNION ALL ask neither.
#[test]
fn void_is_compared_and_kept_distinct_by_nothing() {
    let path = tmp("reader-void.snail");
    write(&path, &Catalog::parse(DDL).unwrap(), &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    let unequal = "could not identify an equality operator for type void";
    let compared = [
        ("SELECT 1 FROM dn.a WHERE setseed(0) = setseed(0)", "42883", "operator does not exist: void = void"),
        ("SELECT 1 FROM dn.a WHERE setseed(0) <> setseed(0)", "42883", "operator does not exist: void <> void"),
        ("SELECT 1 FROM dn.a WHERE setseed(0) < setseed(0)", "42883", "operator does not exist: void < void"),
        (
            "SELECT 1 FROM dn.a WHERE setseed(0) IS DISTINCT FROM setseed(0)",
            "42883",
            "operator does not exist: void = void",
        ),
        ("SELECT 1 FROM dn.a WHERE setseed(0) = 1", "42883", "operator does not exist: void = integer"),
        ("SELECT 1 FROM dn.a WHERE 1 = setseed(0)", "42883", "operator does not exist: integer = void"),
        ("SELECT 1 FROM dn.a WHERE setseed(0) = 'x'", "42883", "operator does not exist: void = unknown"),
        ("SELECT 1 FROM dn.a WHERE setseed(0) IN (setseed(0))", "42883", "operator does not exist: void = void"),
        (
            "SELECT 1 FROM dn.a WHERE setseed(0) BETWEEN setseed(0) AND setseed(0)",
            "42883",
            "operator does not exist: void >= void",
        ),
        (
            "SELECT 1 FROM dn.a WHERE NULLIF(setseed(0), setseed(0)) IS NULL",
            "42883",
            "operator does not exist: void = void",
        ),
        (
            "SELECT CASE setseed(0) WHEN setseed(0) THEN 1 END FROM dn.a",
            "42883",
            "operator does not exist: void = void",
        ),
        (
            "SELECT 1 FROM dn.a WHERE false AND setseed(0) = setseed(0)",
            "42883",
            "operator does not exist: void = void",
        ),
        ("SELECT DISTINCT setseed(0) FROM dn.a", "42883", unequal),
        ("SELECT setseed(0) FROM dn.a GROUP BY 1", "42883", unequal),
        ("SELECT count(DISTINCT setseed(0)) FROM dn.a", "42883", unequal),
        (
            "SELECT setseed(0) FROM dn.a ORDER BY 1",
            "42883",
            "could not identify an ordering operator for type void\nHINT:  Use an explicit \
             ordering operator or modify the query.",
        ),
        ("SELECT DISTINCT ON (setseed(0)) 1 FROM dn.a", "42883", unequal),
        ("SELECT setseed(0) FROM dn.a UNION SELECT setseed(0) FROM dn.a", "42883", unequal),
    ];
    refusals(&mut w, &compared);
    answered(
        &mut w,
        &[
            ("SELECT 1 FROM dn.a WHERE setseed(0) IS NULL", &[]),
            ("SELECT setseed(0) FROM dn.a UNION ALL SELECT setseed(0) FROM dn.a", &[]),
        ],
    );
    w.execute("INSERT INTO dn.a VALUES (1, 10, 'x', 'one')").unwrap();
    refusals(
        &mut w,
        &[
            ("SELECT 1 FROM dn.a WHERE setseed(0) = setseed(0)", "42883", "operator does not exist: void = void"),
            ("SELECT DISTINCT setseed(0) FROM dn.a", "42883", unequal),
            ("SELECT count(DISTINCT setseed(0)) FROM dn.a", "42883", unequal),
            ("SELECT setseed(0) FROM dn.a GROUP BY 1", "42883", unequal),
            ("SELECT setseed(0) FROM dn.a UNION SELECT setseed(0) FROM dn.a", "42883", unequal),
        ],
    );
    answered(
        &mut w,
        &[
            ("SELECT 1 FROM dn.a WHERE setseed(0) IS NULL", &[]),
            ("SELECT setseed(0) FROM dn.a UNION ALL SELECT setseed(0) FROM dn.a", &["", ""]),
        ],
    );
}

// The array functions as `||` reads them, each in the array type of its arguments' common type.
#[test]
fn array_append_prepend_and_cat_answer_as_postgresql() {
    let mut w = snail("joined-arrays");
    answered(
        &mut w,
        &[
            (
                "SELECT array_append(ARRAY[1, 2], 3), array_prepend(0, ARRAY[1, 2]), \
                 array_cat(ARRAY[1], ARRAY[2, 3])",
                &["{1,2,3}|{0,1,2}|{1,2,3}"],
            ),
            (
                "SELECT array_append(ARRAY[1, 2], NULL), array_append(NULL::integer[], 3), \
                 array_append(NULL, 3), array_prepend(NULL, ARRAY[1]), array_prepend(1, NULL), \
                 array_cat(ARRAY[1], NULL), array_cat(NULL, ARRAY[1]), \
                 array_cat(NULL::integer[], NULL)",
                &["{1,2,NULL}|{3}|{3}|{NULL,1}|{1}|{1}|{1}|NULL"],
            ),
            (
                "SELECT array_append(ARRAY[1, 2], 3.5), array_prepend('x', ARRAY['a', 'b']), \
                 array_append(ARRAY['a'], 'b'), array_cat(ARRAY[[1, 2]], ARRAY[3, 4]), \
                 array_cat(ARRAY[1, 2], ARRAY[[3, 4]]), array_cat(ARRAY[[1]], ARRAY[[2]])",
                &["{1,2,3.5}|{x,a,b}|{a,b}|{{1,2},{3,4}}|{{1,2},{3,4}}|{{1},{2}}"],
            ),
            (
                "SELECT array_append('{1,2}', 3), array_append('{}'::int[], 3), \
                 array_prepend(0, '{}'::int[]), array_cat('{}'::int[], ARRAY[1])",
                &["{1,2,3}|{3}|{0}|{1}"],
            ),
            (
                "SELECT array_append(NULL, NULL), pg_typeof(array_append(NULL, NULL)), \
                 array_cat(NULL, NULL), pg_typeof(array_cat(NULL, NULL))",
                &["{NULL}|text[]|NULL|text[]"],
            ),
            (
                "SELECT array_append(ARRAY[1, 2], 3) = ARRAY[1, 2] || 3, \
                 array_cat(ARRAY['{1,2}'::int[]], ARRAY['{3,4}'::int[]])",
                &["t|{{1,2},{3,4}}"],
            ),
            (
                "SELECT array_append('[0:1]={1,2}'::int[], 3), array_prepend(0, \
                 '[5:6]={1,2}'::int[]), array_cat('[2:3]={1,2}'::int[], ARRAY[3])",
                &["[0:2]={1,2,3}|[5:7]={0,1,2}|[2:4]={1,2,3}"],
            ),
            (
                "SELECT id, array_append(ARRAY[id], g), array_prepend(name, ARRAY[k]), \
                 array_cat(ARRAY[g], ARRAY[id, id]) FROM dn.a WHERE id IN (1, 6) ORDER BY id",
                &["1|{1,10}|{one,x}|{10,1,1}", "6|{6,NULL}|{six,NULL}|{NULL,6,6}"],
            ),
        ],
    );
    let dimensions = |why: &str| {
        format!(
            "cannot concatenate incompatible arrays\nDETAIL:  {why} not compatible for \
             concatenation."
        )
    };
    let one = "argument must be empty or one-dimensional array";
    let none = |call: &str| {
        format!(
            "function {call} does not exist\nHINT:  No function matches the given name and \
             argument types. You might need to add explicit type casts."
        )
    };
    for (sql, code, words) in [
        ("SELECT array_append(ARRAY[[1, 2]], 3)", "22000", one.to_string()),
        ("SELECT array_prepend(0, ARRAY[[1, 2]])", "22000", one.to_string()),
        (
            "SELECT array_cat(ARRAY[[1, 2]], ARRAY[[3]])",
            "2202E",
            dimensions("Arrays with differing element dimensions are"),
        ),
        (
            "SELECT array_cat(ARRAY[[[1]]], ARRAY[2])",
            "2202E",
            dimensions("Arrays of 3 and 1 dimensions are"),
        ),
        (
            "SELECT array_cat(ARRAY[[1, 2]], ARRAY[3])",
            "2202E",
            dimensions("Arrays with differing dimensions are"),
        ),
        (
            "SELECT array_cat('[0:0]={1}'::int[], ARRAY[[2]])",
            "2202E",
            dimensions("Arrays with differing dimensions are"),
        ),
        (
            "SELECT array_append(ARRAY[1], 'x')",
            "22P02",
            "invalid input syntax for type integer: \"x\"".to_string(),
        ),
        (
            "SELECT array_append(ARRAY[1], 'x'::text)",
            "42883",
            none("array_append(integer[], text)"),
        ),
        ("SELECT array_append(1, 2)", "42883", none("array_append(integer, integer)")),
        (
            "SELECT array_prepend(ARRAY[1], ARRAY[2])",
            "42883",
            none("array_prepend(integer[], integer[])"),
        ),
        ("SELECT array_cat(ARRAY[1], 2)", "42883", none("array_cat(integer[], integer)")),
    ] {
        assert_eq!(refused(&mut w, sql), (Some(code), words), "{sql}");
    }
}

// A column named by its table's schema and name, as 18.6 reads it: the table's where its FROM names
// it without an alias, in every clause and in a subquery around which it stands; the file's own
// database's name before them passed over, as 18.6 passes over its own. Elsewhere refused in 18.6's
// words: an entry it finds, or the alias it meant.
#[test]
fn a_column_named_by_its_schema_and_table() {
    let mut w = snail("by-schema");
    answered(
        &mut w,
        &[
            (
                "SELECT DISTINCT ON (dn.a.g) dn.a.id FROM dn.a ORDER BY dn.a.g, dn.a.id",
                &["1", "2", "5", "6"],
            ),
            ("SELECT dn.a.id FROM dn.a WHERE id < 3 ORDER BY id", &["1", "2"]),
            ("SELECT snail.dn.a.id FROM dn.a WHERE id < 3 ORDER BY id", &["1", "2"]),
            ("SELECT snail.dn.a.id FROM dn.a, (SELECT 1) s WHERE dn.a.id = 1", &["1"]),
            (
                "SELECT dn.a.g, count(*) FROM dn.a GROUP BY dn.a.g ORDER BY dn.a.g",
                &["10|2", "20|2", "30|1", "NULL|2"],
            ),
            (
                "SELECT dn.a.id, row_number() OVER (ORDER BY dn.a.id DESC) FROM dn.a ORDER BY 1 \
                 LIMIT 2",
                &["1|7", "2|6"],
            ),
            (
                "SELECT dn.a.id, dn.a.g + 1 FROM dn.a WHERE dn.a.g > 15 ORDER BY dn.a.id",
                &["2|21", "4|21", "5|31"],
            ),
            ("SELECT dn.a.* FROM dn.a WHERE id = 1", &["1|10|x|one"]),
            ("SELECT dn.a.id FROM dn.a JOIN dn.b USING (id) WHERE id = 1", &["1"]),
            ("SELECT dn.a.id FROM dn.a ORDER BY dn.a.id DESC LIMIT 1", &["7"]),
            (
                "SELECT (SELECT dn.a.name FROM dn.a b WHERE b.id = dn.a.id + 1) FROM dn.a WHERE \
                 dn.a.id = 1",
                &["one"],
            ),
            ("SELECT count(dn.a.g), sum(dn.a.g) FROM dn.a", &["5|90"]),
            ("SELECT dn.a.id FROM dn.a, dn.a b WHERE dn.a.id = b.id AND b.id < 2", &["1"]),
            ("SELECT dn.a.id FROM dn.a WHERE dn.a.g IS NULL ORDER BY 1", &["6", "7"]),
            (
                "SELECT dn.a.id, dn.b.v FROM dn.a JOIN dn.b ON dn.b.a_id = dn.a.id WHERE dn.a.id \
                 = 5 ORDER BY dn.b.v",
                &["5|500", "5|600"],
            ),
        ],
    );
    let alias = |of: &str, meant: &str| {
        format!(
            "invalid reference to FROM-clause entry for table \"{of}\"\nHINT:  Perhaps you meant \
             to reference the table alias \"{meant}\"."
        )
    };
    refusals(
        &mut w,
        &[
            ("SELECT dn.a.id FROM dn.a a", "42P01", &unreachable_entry("a", "a")),
            ("SELECT dn.a.id FROM dn.a x", "42P01", &alias("a", "x")),
            ("SELECT dn.a.* FROM dn.a x", "42P01", &alias("a", "x")),
            ("SELECT public.a.id FROM dn.a", "42P01", &unreachable_entry("a", "a")),
            ("SELECT dn.c.id FROM dn.a", "42P01", "missing FROM-clause entry for table \"c\""),
            ("SELECT dn.a.zz FROM dn.a", "42703", "column a.zz does not exist"),
            ("SELECT dn.a.id FROM (SELECT * FROM dn.a) a", "42P01", &unreachable_entry("a", "a")),
            (
                "SELECT other.dn.a.id FROM dn.a",
                "0A000",
                "cross-database references are not implemented: other.dn.a.id",
            ),
            (
                "SELECT other.dn.a.id FROM dn.a, (SELECT 1) s",
                "0A000",
                "cross-database references are not implemented: other.dn.a.id",
            ),
            (
                "SELECT x.dn.a.id.y FROM dn.a",
                "42601",
                "improper qualified name (too many dotted names): x.dn.a.id.y",
            ),
        ],
    );
}

// 18.6 over a file of 18, 19 RC1 over one of 19, the same statements in the same order: a view's
// labels and a body's after AS are read as a statement's, under the file's release, a label written
// without AS printed after one.
#[test]
fn a_view_s_and_a_body_s_labels_are_the_file_s_release_s() {
    let syntax = |w: &mut Snail, sql: &str, word: &str| {
        let near = format!("syntax error at or near \"{word}\"");
        assert_eq!(refused(w, sql), (Some("42601"), near), "{sql}");
    };
    let taken = |w: &mut Snail, sql: &str| {
        w.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    };
    let v4 = "CREATE VIEW v4 AS SELECT 1 \"Quoted\", 2 x, id FROM k WHERE id IN (SELECT 3 y)";
    let printed = " SELECT 1 AS \"Quoted\",\n    2 AS x,\n    id\n   FROM k\n  WHERE (id IN ( SELECT 3 AS y));";
    let mut w = keyed_of("labels-18", 18);
    taken(&mut w, "CREATE VIEW v1 AS SELECT 1 x");
    answered(
        &mut w,
        &[
            ("SELECT * FROM v1", &["1"]),
            ("SELECT pg_get_viewdef('v1'::regclass)", &[" SELECT 1 AS x;"]),
        ],
    );
    taken(&mut w, "CREATE VIEW v2 AS SELECT 1 respect");
    syntax(&mut w, "CREATE VIEW v3 AS SELECT 1 year", "year");
    taken(&mut w, v4);
    answered(&mut w, &[("SELECT pg_get_viewdef('v4'::regclass)", &[printed])]);
    syntax(
        &mut w,
        "CREATE FUNCTION f1() RETURNS integer LANGUAGE sql AS $$ SELECT 1 year $$",
        "year",
    );
    taken(&mut w, "CREATE FUNCTION f2() RETURNS integer LANGUAGE sql AS $$ SELECT 1 respect $$");
    answered(&mut w, &[("SELECT f2()", &["1"])]);
    taken(&mut w, "CREATE FUNCTION f3() RETURNS integer LANGUAGE sql AS $$ SELECT 1 x $$");
    answered(&mut w, &[("SELECT f3()", &["1"])]);
    let mut w = keyed_of("labels-19", 19);
    taken(&mut w, "CREATE VIEW v1 AS SELECT 1 x");
    answered(
        &mut w,
        &[
            ("SELECT * FROM v1", &["1"]),
            ("SELECT pg_get_viewdef('v1'::regclass)", &[" SELECT 1 AS x;"]),
        ],
    );
    syntax(&mut w, "CREATE VIEW v2 AS SELECT 1 respect", "respect");
    syntax(&mut w, "CREATE VIEW v3 AS SELECT 1 year", "year");
    taken(&mut w, v4);
    answered(&mut w, &[("SELECT pg_get_viewdef('v4'::regclass)", &[printed])]);
    syntax(
        &mut w,
        "CREATE FUNCTION f1() RETURNS integer LANGUAGE sql AS $$ SELECT 1 year $$",
        "year",
    );
    syntax(
        &mut w,
        "CREATE FUNCTION f2() RETURNS integer LANGUAGE sql AS $$ SELECT 1 respect $$",
        "respect",
    );
    taken(&mut w, "CREATE FUNCTION f3() RETURNS integer LANGUAGE sql AS $$ SELECT 1 x $$");
    answered(&mut w, &[("SELECT f3()", &["1"])]);
    // A DDL's file: read whole, then under its release.
    let ddl = |view: &str| format!("CREATE TABLE k (id integer PRIMARY KEY); {view};");
    let e = Catalog::parse(&ddl("CREATE VIEW v AS SELECT 1 year")).unwrap_err();
    assert_eq!((e.sqlstate(), e.to_string()), (Some("42601"), "syntax error at or near \"year\"".into()));
    let mut c = Catalog::parse(&ddl("CREATE VIEW v AS SELECT 1 respect, 2 x")).unwrap();
    c.clone().with_release(18).unwrap();
    let e = c.with_release(19).unwrap_err();
    assert_eq!(e.to_string(), "syntax error at or near \"respect\"");
}

// A built-in called by its parameters' names, as 18.6's parser reads named and mixed notation:
// each argument at its parameter's place, those left out their DEFAULTs; refused in its words where
// no form takes the names, or the call writes one twice or one by place after them.
#[test]
fn a_built_in_called_by_its_parameters_names() {
    let mut w = snail("named-calls");
    answered(
        &mut w,
        &[
            (
                "SELECT random_normal(stddev => 0) AS a, random_normal(mean => 5, stddev => 0) AS \
                 b, random_normal(stddev => 0, mean => 5) AS c, random_normal(5, stddev => 0) AS d",
                &["0|5|5|5"],
            ),
            ("SELECT setseed(0.5)", &[""]),
            (
                "SELECT random_normal(mean => 5) AS a, random_normal(stddev => 2) AS b",
                &["7.583242670160505|-0.9026841997228229"],
            ),
            (
                "SELECT parse_ident(str => 'a.b') AS a, parse_ident('a.b', strict => false) AS b, \
                 parse_ident(strict => true, str => 'x') AS c",
                &["{a,b}|{a,b}|{x}"],
            ),
            (
                "SELECT jsonb_set('{\"a\":1}', '{a}', '2', create_if_missing => false) AS a, \
                 jsonb_set('{\"a\":1}', '{b}', '2', create_if_missing => false) AS b",
                &["{\"a\": 2}|{\"a\": 1}"],
            ),
            (
                "SELECT jsonb_path_exists(target => '{\"a\":1}', path => '$.a') AS a, \
                 jsonb_path_query_first('{\"a\":2}', '$.a', silent => true) AS b",
                &["t|2"],
            ),
            (
                "SELECT make_interval(days => 1, hours => 2) AS a, make_date(year => 2020, month \
                 => 1, day => 2) AS b, json_strip_nulls('{\"a\":null,\"b\":1}'::json, \
                 strip_in_arrays => true) AS c",
                &["1 day 02:00:00|2020-01-02|{\"b\":1}"],
            ),
            (
                "SELECT id, make_interval(days => id) AS d FROM dn.a WHERE id < 3 ORDER BY id",
                &["1|1 day", "2|2 days"],
            ),
            (
                "SELECT pg_typeof(random_normal(mean => 5::numeric, stddev => 0)) AS t",
                &["double precision"],
            ),
        ],
    );
    let none = |call: &str| {
        format!(
            "function {call} does not exist\nHINT:  No function matches the given name and \
             argument types. You might need to add explicit type casts."
        )
    };
    for (sql, code, words) in [
        ("SELECT random_normal(foo => 1)", "42883", none("random_normal(foo => integer)")),
        (
            "SELECT random_normal(mean => 1, 2)",
            "42601",
            "positional argument cannot follow named argument".to_string(),
        ),
        (
            "SELECT random_normal(mean => 1, mean => 2)",
            "42601",
            "argument name \"mean\" used more than once".to_string(),
        ),
        ("SELECT left(str => 'abc', n => 2)", "42883", none("left(str => unknown, n => integer)")),
        (
            "SELECT jsonb_set(jsonb_in => '{}', path => '{a}')",
            "42883",
            none("jsonb_set(jsonb_in => unknown, path => unknown)"),
        ),
    ] {
        assert_eq!(refused(&mut w, sql), (Some(code), words), "{sql}");
    }
}

// SET seed seeds the session's draws as setseed does, LOCAL too, and shows none; it cannot be reset,
// and a value outside -1 to 1, or none a real reads, is refused in 18.6's words.
#[test]
fn set_seed_seeds_the_draws_as_setseed_does() {
    let mut w = snail("set-seed");
    let first = "0.9851677175347999";
    for (sql, want) in [
        ("SET SEED TO 0.5", None),
        ("SELECT random() AS a, random() AS b", Some(vec!["0.9851677175347999|0.825301858027981"])),
        ("SET seed = 0.5", None),
        ("SELECT random() AS a", Some(vec![first])),
        ("SET SESSION seed TO -1", None),
        ("SELECT random() AS a", Some(vec!["0.725656831544149"])),
        ("SHOW seed", Some(vec!["unavailable"])),
        ("SELECT current_setting('seed') AS s", Some(vec!["unavailable"])),
        ("SELECT random() AS a", Some(vec!["0.21342431605981593"])),
        ("BEGIN", None),
        ("SET LOCAL seed = 0.5", None),
        ("SELECT random() AS a", Some(vec![first])),
        ("COMMIT", None),
        ("SELECT random() AS a", Some(vec!["0.825301858027981"])),
        ("SELECT set_config('seed', '0.5', false) AS s", Some(vec!["unavailable"])),
        ("SELECT random() AS a", Some(vec![first])),
        ("SET seed = '0.5'", None),
        ("SELECT random() AS a", Some(vec![first])),
        ("SET seed = 1e-1", None),
        ("SELECT random() AS a", Some(vec!["0.7345507212519062"])),
    ] {
        match want {
            Some(want) => assert_eq!(rows(&mut w, sql), want, "{sql}"),
            None => {
                w.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
            }
        }
    }
    refusals(
        &mut w,
        &[
            ("SET seed = 2", "22023", "2 is outside the valid range for parameter \"seed\" (-1 .. 1)"),
            ("SET seed = 'abc'", "22023", "invalid value for parameter \"seed\": \"abc\""),
            ("SET seed TO DEFAULT", "0A000", "parameter \"seed\" cannot be reset"),
            ("RESET seed", "0A000", "parameter \"seed\" cannot be reset"),
            ("SET seed = 0.5, 0.2", "22023", "SET seed takes only one argument"),
        ],
    );
}

// A reference no relation answers to, refused in 18.6's words on every path: a step's of one table
// (DISTINCT ON, an aggregate), a FROM's of several, the question path's; a missing column with the
// HINT naming the column of the relations in reach spelled most like it, two where two tie.
#[test]
fn a_missing_relation_or_column_is_refused_in_postgresql_s_words() {
    let mut w = snail("missing-names");
    let meant = |words: &str, column: &str| {
        format!("{words}\nHINT:  Perhaps you meant to reference the column \"{column}\".")
    };
    let missing_b = "missing FROM-clause entry for table \"b\"".to_string();
    for (sql, code, words) in [
        ("SELECT DISTINCT ON (b.g) b.id FROM dn.a ORDER BY b.g", "42P01", missing_b.clone()),
        ("SELECT count(b.g) FROM dn.a", "42P01", missing_b.clone()),
        (
            "SELECT DISTINCT ON (dn.a.g) dn.a.id FROM dn.a AS a ORDER BY dn.a.g",
            "42P01",
            unreachable_entry("a", "a"),
        ),
        (
            "SELECT DISTINCT ON (dn.a.g) dn.a.id FROM dn.a AS x ORDER BY dn.a.g",
            "42P01",
            "invalid reference to FROM-clause entry for table \"a\"\nHINT:  Perhaps you meant to \
             reference the table alias \"x\"."
                .to_string(),
        ),
        (
            "SELECT DISTINCT ON (g) a.nope FROM dn.a ORDER BY g",
            "42703",
            meant("column a.nope does not exist", "a.name"),
        ),
        ("SELECT count(a.nam) FROM dn.a", "42703", meant("column a.nam does not exist", "a.name")),
        (
            "SELECT DISTINCT ON (g) nam FROM dn.a ORDER BY g",
            "42703",
            meant("column \"nam\" does not exist", "a.name"),
        ),
        (
            "SELECT DISTINCT ON (g) zz FROM dn.a ORDER BY g",
            "42703",
            "column \"zz\" does not exist".to_string(),
        ),
        (
            "SELECT a.nam FROM dn.a a, (SELECT 1) s",
            "42703",
            meant("column a.nam does not exist", "a.name"),
        ),
        (
            "SELECT nam FROM dn.a, (SELECT 1) s",
            "42703",
            meant("column \"nam\" does not exist", "a.name"),
        ),
        (
            "SELECT a.idd FROM dn.a a JOIN dn.b b ON a.id = b.id, (SELECT 1) s",
            "42703",
            meant("column a.idd does not exist", "a.id"),
        ),
        ("SELECT a.nope FROM dn.a a", "42703", meant("column a.nope does not exist", "a.name")),
        ("SELECT nam FROM dn.a", "42703", meant("column \"nam\" does not exist", "a.name")),
        ("SELECT dn.a.nope FROM dn.a", "42703", meant("column a.nope does not exist", "a.name")),
        (
            "SELECT ids FROM dn.a JOIN dn.b ON true",
            "42703",
            "column \"ids\" does not exist\nHINT:  Perhaps you meant to reference the column \
             \"a.id\" or the column \"b.id\"."
                .to_string(),
        ),
        (
            "SELECT idd FROM dn.a a JOIN dn.b b ON true JOIN dn.n n ON true",
            "42703",
            "column \"idd\" does not exist".to_string(),
        ),
        ("SELECT zz FROM dn.a", "42703", "column \"zz\" does not exist".to_string()),
        ("SELECT b.nam FROM dn.a a", "42P01", missing_b),
    ] {
        assert_eq!(refused(&mut w, sql), (Some(code), words), "{sql}");
    }
}

// A column added to a table of no rows, as 18.6 adds it: at its end, with its DEFAULT, NOT NULL and
// IF NOT EXISTS, several a comma apart; refused in 18.6's words where its name is taken. A table that
// holds rows keeps its columns (R12's), and a constraint, a serial, an identity or a generation
// added with the column is refused by name, each the snail's own.
#[test]
fn a_column_is_added_to_a_table_of_no_rows() {
    let mut w = snail("add-column");
    for sql in [
        "CREATE TABLE dn.e (id integer NOT NULL, v text, PRIMARY KEY (id))",
        "ALTER TABLE dn.e ADD COLUMN w integer",
        "ALTER TABLE dn.e ADD x text DEFAULT 'x' NOT NULL",
    ] {
        w.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    let done = w
        .execute("ALTER TABLE dn.e ADD COLUMN IF NOT EXISTS w integer")
        .unwrap();
    assert_eq!(
        done.notices,
        ["NOTICE:  column \"w\" of relation \"e\" already exists, skipping"]
    );
    for sql in [
        "INSERT INTO dn.e (id, v, w) VALUES (1, 'a', 10)",
        "INSERT INTO dn.e VALUES (2, 'b', 20, 'y')",
        "CREATE TABLE dn.f (id integer NOT NULL, PRIMARY KEY (id))",
        "ALTER TABLE dn.f ADD COLUMN q integer, ADD r text DEFAULT 'r'",
        "INSERT INTO dn.f (id) VALUES (1)",
    ] {
        w.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    answered(
        &mut w,
        &[
            ("SELECT * FROM dn.e ORDER BY id", &["1|a|10|x", "2|b|20|y"]),
            (
                "SELECT column_name, data_type, column_default, is_nullable FROM \
                 information_schema.columns WHERE table_schema = 'dn' AND table_name = 'e' ORDER \
                 BY ordinal_position",
                &[
                    "id|integer|NULL|NO",
                    "v|text|NULL|YES",
                    "w|integer|NULL|YES",
                    "x|text|'x'::text|NO",
                ],
            ),
            ("SELECT * FROM dn.f", &["1|NULL|r"]),
            (
                "SELECT attname, atttypid::regtype FROM pg_attribute WHERE attrelid = \
                 'dn.f'::regclass AND attnum > 0 ORDER BY attnum",
                &["id|integer", "q|integer", "r|text"],
            ),
        ],
    );
    refusals(
        &mut w,
        &[
            (
                "ALTER TABLE dn.e ADD COLUMN w integer",
                "42701",
                "column \"w\" of relation \"e\" already exists",
            ),
            (
                "ALTER TABLE dn.e ADD COLUMN ctid integer",
                "42701",
                "column name \"ctid\" conflicts with a system column name",
            ),
        ],
    );
    let own = "a constraint, a serial, an identity or a generation added with a column is not read";
    for (sql, words) in [
        (
            "ALTER TABLE dn.a ADD COLUMN z integer",
            "dn.a: it holds rows, so its columns and orders stay as written".to_string(),
        ),
        (
            "ALTER TABLE dn.f ADD COLUMN p integer UNIQUE",
            format!("ALTER TABLE dn.f ADD COLUMN p integer UNIQUE: {own}"),
        ),
        (
            "ALTER TABLE dn.f ADD COLUMN s serial",
            format!("ALTER TABLE dn.f ADD COLUMN s serial: {own}"),
        ),
    ] {
        assert_eq!(refused(&mut w, sql).1, words, "{sql}");
    }
}

// A cast of an aggregate's value or of a column on one table, which the question path holds no
// level for, is the pipeline's: answered as 18.6 casts it, on a snail open to read alone as on one
// open to write.
#[test]
fn a_cast_the_question_path_reads_no_level_of_is_answered() {
    let path = tmp("reader-casts.snail");
    write(&path, &Catalog::parse(DDL).unwrap(), &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    for sql in ROWS {
        w.execute(sql).unwrap();
    }
    drop(w);
    let mut r = Snail::open(&path).unwrap();
    let asked: [(&str, &[&str]); 6] = [
        ("SELECT count(*)::text FROM dn.a", &["7"]),
        ("SELECT id::text FROM dn.a ORDER BY id", &["1", "2", "3", "4", "5", "6", "7"]),
        ("SELECT count(*)::text AS c FROM dn.a WHERE id > 3", &["4"]),
        ("SELECT (g * 2)::text FROM dn.a WHERE id = 1", &["20"]),
        ("SELECT sum(g)::numeric(10,2) FROM dn.a", &["90.00"]),
        ("SELECT max(name)::varchar(3) FROM dn.a", &["two"]),
    ];
    for (sql, want) in asked {
        let (b, _) = r.ask(sql, None).unwrap_or_else(|e| panic!("{sql}: {e}"));
        let got: Vec<String> = r
            .text_of(&b)
            .unwrap()
            .into_iter()
            .map(|row| row.into_iter().map(|v| v.unwrap_or_default()).collect::<Vec<_>>().join("|"))
            .collect();
        assert_eq!(got, want, "{sql}");
    }
    drop(r);
    let mut w = Snail::open_writing(&path).unwrap();
    answered(&mut w, &asked);
}

/// PostgreSQL's refusal of a sort key of a type it finds no ordering for, with its HINT.
fn no_ordering(ty: &str) -> String {
    format!(
        "could not identify an ordering operator for type {ty}\nHINT:  Use an explicit ordering \
         operator or modify the query."
    )
}

/// PostgreSQL's refusal of a key that groups or finds DISTINCT values, of a type it finds no
/// equality for.
fn no_equality(ty: &str) -> String {
    format!("could not identify an equality operator for type {ty}")
}

#[test]
fn a_key_of_no_ordering_is_refused_before_any_row() {
    let mut w = snail("unordered");
    for sql in [
        "CREATE TABLE dn.jj (id integer PRIMARY KEY, v json, p point)",
        "CREATE TYPE dn.c AS (x json)",
    ] {
        w.execute(sql).unwrap();
    }
    // Over dn.a's rows, whose k no json reads, and over dn.jj's none: refused as each is read.
    let asked = [
        ("SELECT id FROM dn.a ORDER BY k::json", no_ordering("json")),
        ("SELECT id FROM dn.a WHERE false ORDER BY k::json DESC, id", no_ordering("json")),
        ("SELECT id FROM dn.jj ORDER BY p", no_ordering("point")),
        ("SELECT id, v FROM dn.jj ORDER BY 2", no_ordering("json")),
        ("SELECT id FROM dn.a ORDER BY ARRAY[k::json]", no_ordering("json[]")),
        ("SELECT id FROM dn.a ORDER BY ROW(k::json)::dn.c", no_ordering("dn.c")),
        ("SELECT DISTINCT k::json FROM dn.a", no_equality("json")),
        ("SELECT DISTINCT v FROM dn.jj", no_equality("json")),
        ("SELECT v FROM dn.jj GROUP BY v", no_equality("json")),
        ("SELECT p FROM dn.jj GROUP BY 1", no_equality("point")),
        ("SELECT id, k::json FROM dn.a ORDER BY 2", no_ordering("json")),
        ("SELECT k::json FROM dn.a GROUP BY 1", no_equality("json")),
        ("SELECT max(id) FROM dn.a GROUP BY ROW(k::json)::dn.c", no_equality("dn.c")),
        ("SELECT DISTINCT ON (ARRAY[k::json]) id FROM dn.a", no_equality("json[]")),
        ("SELECT id, rank() OVER (ORDER BY k::json) FROM dn.a", no_ordering("json")),
        ("SELECT id, rank() OVER (PARTITION BY p) FROM dn.jj", no_equality("point")),
        ("SELECT count(DISTINCT k::json) FROM dn.a", no_equality("json")),
        ("SELECT array_agg(id ORDER BY v) FROM dn.jj", no_ordering("json")),
        // In the order PostgreSQL's parser reads them: an aggregate's keys with the select list,
        // the ORDER BY, then GROUP BY and DISTINCT, then LIMIT, then a window's ORDER BY before
        // its PARTITION BY.
        ("SELECT DISTINCT k FROM dn.a ORDER BY k::json", no_ordering("json")),
        ("SELECT id FROM dn.a GROUP BY id, k::json ORDER BY id::text::xml", no_ordering("xml")),
        (
            "SELECT id, count(DISTINCT k::json) FROM dn.a GROUP BY id ORDER BY k::xml",
            no_equality("json"),
        ),
        ("SELECT id, rank() OVER (ORDER BY k::xml) FROM dn.a ORDER BY k::json", no_ordering("json")),
        ("SELECT id FROM dn.a ORDER BY k::json LIMIT 'x'", no_ordering("json")),
        (
            "SELECT id, rank() OVER (PARTITION BY k::json ORDER BY k::xml) FROM dn.a",
            no_ordering("xml"),
        ),
        ("SELECT v, p FROM dn.jj GROUP BY v, p ORDER BY 2", no_ordering("point")),
        (
            "SELECT k::json AS kj, k::xml AS kx FROM dn.a GROUP BY 1, 2 ORDER BY 2",
            no_ordering("xml"),
        ),
        ("SELECT count(*) FROM dn.jj GROUP BY id ORDER BY v", no_ordering("json")),
    ];
    for (sql, words) in asked {
        assert_eq!(refused(&mut w, sql), (Some("42883"), words), "{sql}");
    }
    // A row constructor's record is compared only as two rows meet.
    answered(
        &mut w,
        &[
            ("SELECT id FROM dn.a WHERE false ORDER BY ROW(k::json)", &[]),
            ("SELECT DISTINCT ON (ROW(k::json)) id FROM dn.a WHERE false", &[]),
        ],
    );
    // xid has an equality and no ordering: DISTINCT answers; a sort is refused, an aggregate's
    // DISTINCT with its DETAIL.
    answered(&mut w, &[("SELECT DISTINCT '1'::xid FROM dn.a", &["1"])]);
    assert_eq!(
        refused(&mut w, "SELECT id FROM dn.a ORDER BY '1'::xid"),
        (Some("42883"), no_ordering("xid"))
    );
    assert_eq!(
        refused(&mut w, "SELECT count(DISTINCT k::xid) FROM dn.a WHERE false"),
        (
            Some("42883"),
            "could not identify an ordering operator for type xid\nDETAIL:  Aggregates with \
             DISTINCT must be able to sort their inputs."
                .to_string()
        )
    );
}

#[test]
fn a_values_list_is_a_question_of_its_own() {
    let mut w = snail("values");
    answered(
        &mut w,
        &[
            ("VALUES (1, 'a'), (2, 'b') ORDER BY 1 DESC", &["2|b", "1|a"]),
            ("VALUES (1)", &["1"]),
            ("VALUES (1, 'a'), (2, 'b') ORDER BY column2 DESC LIMIT 1", &["2|b"]),
            ("VALUES (1) UNION ALL VALUES (2)", &["1", "2"]),
            ("VALUES (1, 2) OFFSET 0 FETCH FIRST 1 ROW ONLY", &["1|2"]),
            ("VALUES (1, NULL), (NULL, 'x')", &["1|NULL", "NULL|x"]),
            ("VALUES (1), (2.5)", &["1", "2.5"]),
            ("VALUES (3), (1), (2) ORDER BY column1 + 0 DESC", &["3", "2", "1"]),
            ("VALUES (3), (1), (2) ORDER BY \"*VALUES*\".column1", &["1", "2", "3"]),
            ("VALUES (3), (1), (2) ORDER BY 1 LIMIT 2 OFFSET 1", &["2", "3"]),
            ("WITH w AS (VALUES (2), (1) ORDER BY 1 LIMIT 1) SELECT * FROM w", &["1"]),
            (
                "SELECT * FROM (VALUES (2, 'b'), (1, 'a') ORDER BY 1 DESC LIMIT 1) AS v(n, s)",
                &["2|b"],
            ),
            ("(VALUES (2), (1) ORDER BY 1) UNION ALL VALUES (3)", &["1", "2", "3"]),
            ("VALUES (1), (NULL) ORDER BY 1 NULLS FIRST", &["NULL", "1"]),
        ],
    );
    refusals(
        &mut w,
        &[
            ("VALUES (1), ('a')", "22P02", "invalid input syntax for type integer: \"a\""),
            (
                "VALUES (generate_series(1, 2))",
                "0A000",
                "set-returning functions are not allowed in VALUES",
            ),
            ("VALUES (1, 'a'), (2)", "42601", "VALUES lists must all be the same length"),
            ("VALUES (1), (2) FOR UPDATE", "0A000", "FOR UPDATE cannot be applied to VALUES"),
            ("VALUES (1) FOR SHARE", "0A000", "FOR SHARE cannot be applied to VALUES"),
            // Its values, its ORDER BY and its LIMIT are read before its locking clause.
            (
                "VALUES (1), ('a') FOR UPDATE",
                "22P02",
                "invalid input syntax for type integer: \"a\"",
            ),
            (
                "VALUES (1) ORDER BY 2 FOR UPDATE",
                "42P10",
                "ORDER BY position 2 is not in select list",
            ),
            ("VALUES (1) ORDER BY x", "42703", "column \"x\" does not exist"),
            (
                "VALUES (1, 'a') ORDER BY column3",
                "42703",
                "column \"column3\" does not exist\nHINT:  Perhaps you meant to reference the \
                 column \"*VALUES*.column1\" or the column \"*VALUES*.column2\".",
            ),
            ("VALUES (1) LIMIT 'x'", "22P02", "invalid input syntax for type bigint: \"x\""),
        ],
    );
    assert_eq!(
        refused(&mut w, "VALUES ('{}'::json) ORDER BY 1"),
        (Some("42883"), no_ordering("json"))
    );
}

#[test]
fn distinct_orders_by_a_cast_to_its_own_type() {
    let mut w = snail("distinct-cast");
    answered(
        &mut w,
        &[
            ("SELECT DISTINCT g FROM dn.a ORDER BY g::integer", &["10", "20", "30", "NULL"]),
            (
                "SELECT DISTINCT g FROM dn.a ORDER BY (g)::integer DESC",
                &["NULL", "30", "20", "10"],
            ),
            (
                "SELECT DISTINCT g, k FROM dn.a ORDER BY k::text, g::integer DESC",
                &["20|x", "10|x", "30|y", "20|y", "NULL|NULL"],
            ),
            ("SELECT DISTINCT g AS x FROM dn.a ORDER BY g::integer", &["10", "20", "30", "NULL"]),
            (
                "SELECT DISTINCT g + 1 AS x FROM dn.a ORDER BY (g + 1)::integer",
                &["11", "21", "31", "NULL"],
            ),
            (
                "SELECT DISTINCT g FROM dn.a ORDER BY g::integer::integer DESC",
                &["NULL", "30", "20", "10"],
            ),
            (
                "SELECT DISTINCT count(*) AS n FROM dn.a GROUP BY g ORDER BY count(*)::bigint",
                &["1", "2"],
            ),
        ],
    );
    let words = "for SELECT DISTINCT, ORDER BY expressions must appear in select list";
    refusals(
        &mut w,
        &[
            ("SELECT DISTINCT g FROM dn.a ORDER BY g::bigint", "42P10", words),
            ("SELECT DISTINCT k FROM dn.a ORDER BY k::varchar", "42P10", words),
            ("SELECT DISTINCT g FROM dn.a ORDER BY id", "42P10", words),
            (
                "SELECT DISTINCT count(*) AS n FROM dn.a GROUP BY g ORDER BY count(*)::integer",
                "42P10",
                words,
            ),
        ],
    );
}

#[test]
fn select_all_is_no_distinct_anywhere() {
    let mut w = snail("all");
    answered(
        &mut w,
        &[
            (
                "SELECT ALL g FROM dn.a ORDER BY g FOR UPDATE",
                &["10", "10", "20", "20", "30", "NULL", "NULL"],
            ),
            (
                "SELECT ALL g FROM dn.a WHERE g IS NOT NULL ORDER BY g, id FOR SHARE",
                &["10", "10", "20", "20", "30"],
            ),
        ],
    );
    // A view of SELECT ALL is updatable.
    w.execute("CREATE VIEW dn.va AS SELECT ALL id, k FROM dn.a").unwrap();
    assert_eq!(w.execute("UPDATE dn.va SET k = k WHERE id = 1").unwrap().rows, 1);
    answered(
        &mut w,
        &[
            ("SELECT k FROM dn.va WHERE id = 1", &["x"]),
            // An arm of SELECT ALL leaves its literal to the set operation's type.
            ("SELECT 1 UNION SELECT ALL '2' ORDER BY 1", &["1", "2"]),
        ],
    );
    // A cursor over SELECT ALL is updatable where it stands.
    w.execute("BEGIN").unwrap();
    w.execute("DECLARE c CURSOR FOR SELECT ALL id, k FROM dn.a WHERE id = 3").unwrap();
    answered(&mut w, &[("FETCH 1 FROM c", &["3|x"])]);
    assert_eq!(
        w.execute("UPDATE dn.a SET k = 'z' WHERE CURRENT OF c").unwrap().rows,
        1
    );
    w.execute("COMMIT").unwrap();
    answered(&mut w, &[("SELECT id, k FROM dn.a WHERE k = 'z'", &["3|z"])]);
    // An INSERT … SELECT ALL draws each row's DEFAULTs after its calls for that row.
    w.execute("CREATE TABLE dn.t (id serial PRIMARY KEY, b bigint, k integer)").unwrap();
    w.execute(
        "INSERT INTO dn.t (b, k) SELECT ALL nextval('dn.t_id_seq'), id FROM dn.a WHERE id < 4 \
         ORDER BY id",
    )
    .unwrap();
    answered(
        &mut w,
        &[("SELECT id, b, k FROM dn.t ORDER BY id", &["2|1|1", "4|3|2", "6|5|3"])],
    );
}

// The grouping sets, case 12d: GROUP BY ROLLUP, CUBE and GROUPING SETS, a group for each set, an
// expression a set lacks NULL wherever it is named, GROUPING()'s bit for each argument its set
// lacks; GROUP BY DISTINCT and ALL; refused where 18.6 refuses. Every statement orders by
// GROUPING() first or returns rows alike, as the order with no ORDER BY is 18.6's plan's.

const GS_DDL: &str = "CREATE SCHEMA gs;
     CREATE TABLE gs.sale (id integer NOT NULL, region text, product text, qty integer, PRIMARY KEY (id));
     CREATE TABLE gs.region (code text NOT NULL, name text, PRIMARY KEY (code));";

const GS_ROWS: [&str; 2] = [
    "INSERT INTO gs.sale VALUES (1, 'N', 'a', 10), (2, 'N', 'b', 20), (3, 'S', 'a', 5), \
     (4, 'S', 'a', NULL), (5, NULL, 'b', 7), (6, 'S', NULL, 3), (7, 'N', 'a', 10)",
    "INSERT INTO gs.region VALUES ('N', 'North'), ('S', 'South')",
];

/// A snail holding case 12d's rows, at a path of the test's name.
fn gs(name: &str) -> Snail {
    let path = tmp(&format!("reader-gs-{name}.snail"));
    write(&path, &Catalog::parse(GS_DDL).unwrap(), &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    for sql in GS_ROWS {
        w.execute(sql).unwrap();
    }
    w
}

#[test]
fn a_rollup_gives_each_prefix_then_the_total() {
    let mut w = gs("rollup");
    answered(
        &mut w,
        &[
            (
                "SELECT region, sum(qty) FROM gs.sale GROUP BY ROLLUP (region) ORDER BY \
                 GROUPING(region), region",
                &["N|40", "S|8", "NULL|7", "NULL|55"],
            ),
            (
                "SELECT region, product, sum(qty), GROUPING(region, product) AS g FROM gs.sale \
                 GROUP BY ROLLUP (region, product) ORDER BY g, region, product",
                &[
                    "N|a|20|0", "N|b|20|0", "S|a|5|0", "S|NULL|3|0", "NULL|b|7|0", "N|NULL|40|1",
                    "S|NULL|8|1", "NULL|NULL|7|1", "NULL|NULL|55|3",
                ],
            ),
            (
                "SELECT region, product, count(*) FROM gs.sale GROUP BY ROLLUP ((region, product)) \
                 ORDER BY GROUPING(region, product), region, product",
                &["N|a|2", "N|b|1", "S|a|2", "S|NULL|1", "NULL|b|1", "NULL|NULL|7"],
            ),
            (
                "SELECT region, product, qty, count(*) FROM gs.sale GROUP BY ROLLUP (region, \
                 product, qty) ORDER BY GROUPING(region, product, qty), region, product, qty",
                &[
                    "N|a|10|2", "N|b|20|1", "S|a|5|1", "S|a|NULL|1", "S|NULL|3|1", "NULL|b|7|1",
                    "N|a|NULL|2", "N|b|NULL|1", "S|a|NULL|2", "S|NULL|NULL|1", "NULL|b|NULL|1",
                    "N|NULL|NULL|3", "S|NULL|NULL|3", "NULL|NULL|NULL|1", "NULL|NULL|NULL|7",
                ],
            ),
            // Places and output names are read inside a set.
            (
                "SELECT region, product, count(*) FROM gs.sale GROUP BY ROLLUP (1, 2) ORDER BY \
                 GROUPING(region, product), 1, 2",
                &[
                    "N|a|2", "N|b|1", "S|a|2", "S|NULL|1", "NULL|b|1", "N|NULL|3", "S|NULL|3",
                    "NULL|NULL|1", "NULL|NULL|7",
                ],
            ),
            (
                "SELECT s % 2 AS p, count(*), sum(s) FROM generate_series(1, 5) s GROUP BY ROLLUP \
                 (s % 2) ORDER BY GROUPING(s % 2), p",
                &["0|2|6", "1|3|9", "NULL|5|15"],
            ),
        ],
    );
}

#[test]
fn a_cube_gives_every_subset() {
    let mut w = gs("cube");
    answered(
        &mut w,
        &[
            (
                "SELECT region, product, count(*), GROUPING(region) AS gr, GROUPING(product) AS gp \
                 FROM gs.sale GROUP BY CUBE (region, product) ORDER BY gr, gp, region, product",
                &[
                    "N|a|2|0|0", "N|b|1|0|0", "S|a|2|0|0", "S|NULL|1|0|0", "NULL|b|1|0|0",
                    "N|NULL|3|0|1", "S|NULL|3|0|1", "NULL|NULL|1|0|1", "NULL|a|4|1|0",
                    "NULL|b|2|1|0", "NULL|NULL|1|1|0", "NULL|NULL|7|1|1",
                ],
            ),
            (
                "SELECT region, product, sum(qty) FROM gs.sale GROUP BY CUBE ((region, product)) \
                 ORDER BY GROUPING(region, product), region, product",
                &["N|a|20", "N|b|20", "S|a|5", "S|NULL|3", "NULL|b|7", "NULL|NULL|55"],
            ),
        ],
    );
}

#[test]
fn every_fold_folds_each_set_from_the_rows() {
    let mut w = gs("folds");
    answered(
        &mut w,
        &[
            (
                "SELECT region, count(*), count(qty), sum(qty), min(qty), max(qty), count(DISTINCT \
                 qty), count(DISTINCT product) FROM gs.sale GROUP BY ROLLUP (region) ORDER BY \
                 GROUPING(region), region",
                &["N|3|3|40|10|20|2|2", "S|3|2|8|3|5|2|1", "NULL|1|1|7|7|7|1|1", "NULL|7|6|55|3|20|5|2"],
            ),
            (
                "SELECT region, product, count(*), count(qty), sum(qty), min(qty), max(qty), \
                 count(DISTINCT qty) FROM gs.sale GROUP BY CUBE (region, product) ORDER BY \
                 GROUPING(region, product), region, product",
                &[
                    "N|a|2|2|20|10|10|1", "N|b|1|1|20|20|20|1", "S|a|2|1|5|5|5|1",
                    "S|NULL|1|1|3|3|3|1", "NULL|b|1|1|7|7|7|1", "N|NULL|3|3|40|10|20|2",
                    "S|NULL|3|2|8|3|5|2", "NULL|NULL|1|1|7|7|7|1", "NULL|a|4|3|25|5|10|2",
                    "NULL|b|2|2|27|7|20|2", "NULL|NULL|1|1|3|3|3|1", "NULL|NULL|7|6|55|3|20|5",
                ],
            ),
            (
                "SELECT product, avg(qty), string_agg(region, ',' ORDER BY region), array_agg(id \
                 ORDER BY id) FROM gs.sale GROUP BY GROUPING SETS ((product), ()) ORDER BY \
                 GROUPING(product), product",
                &[
                    "a|8.3333333333333333|N,N,S,S|{1,3,4,7}",
                    "b|13.5000000000000000|N|{2,5}",
                    "NULL|3.0000000000000000|S|{6}",
                    "NULL|9.1666666666666667|N,N,N,S,S,S|{1,2,3,4,5,6,7}",
                ],
            ),
            (
                "SELECT region, count(*) FILTER (WHERE qty > 5) AS big, bool_and(qty > 4) AS all4 \
                 FROM gs.sale GROUP BY ROLLUP (region) ORDER BY GROUPING(region), region",
                &["N|3|t", "S|0|f", "NULL|1|t", "NULL|4|f"],
            ),
            // A direct argument is read once a group, as an ordered set reads it.
            (
                "SELECT percentile_disc(0.5) WITHIN GROUP (ORDER BY qty), region FROM gs.sale \
                 GROUP BY ROLLUP (region) ORDER BY GROUPING(region), region",
                &["10|N", "3|S", "7|NULL", "7|NULL"],
            ),
        ],
    );
}

#[test]
fn grouping_sets_give_each_set_listed_repeats_kept() {
    let mut w = gs("sets");
    answered(
        &mut w,
        &[
            (
                "SELECT region, product, count(qty) FROM gs.sale GROUP BY GROUPING SETS ((region), \
                 (product), ()) ORDER BY GROUPING(region, product), region, product",
                &["N|NULL|3", "S|NULL|2", "NULL|NULL|1", "NULL|a|3", "NULL|b|2", "NULL|NULL|1", "NULL|NULL|6"],
            ),
            (
                "SELECT region, product, count(*) FROM gs.sale GROUP BY GROUPING SETS (region, \
                 product) ORDER BY GROUPING(region, product), region, product",
                &["N|NULL|3", "S|NULL|3", "NULL|NULL|1", "NULL|a|4", "NULL|b|2", "NULL|NULL|1"],
            ),
            ("SELECT count(*) FROM gs.sale GROUP BY GROUPING SETS (())", &["7"]),
            ("SELECT count(*) FROM gs.sale GROUP BY ()", &["7"]),
            (
                "SELECT region, count(*) FROM gs.sale GROUP BY GROUPING SETS ((region), (region)) \
                 ORDER BY region",
                &["N|3", "N|3", "S|3", "S|3", "NULL|1", "NULL|1"],
            ),
            (
                "SELECT region, product, count(*) FROM gs.sale GROUP BY GROUPING SETS ((region, \
                 product), ROLLUP (region), CUBE (product)) ORDER BY GROUPING(region, product), \
                 region, product",
                &[
                    "N|a|2", "N|b|1", "S|a|2", "S|NULL|1", "NULL|b|1", "N|NULL|3", "S|NULL|3",
                    "NULL|NULL|1", "NULL|a|4", "NULL|b|2", "NULL|NULL|1", "NULL|NULL|7",
                    "NULL|NULL|7",
                ],
            ),
            (
                "SELECT region, count(*) FROM gs.sale GROUP BY GROUPING SETS ((region), ROLLUP \
                 (region)) ORDER BY GROUPING(region), region",
                &["N|3", "N|3", "S|3", "S|3", "NULL|1", "NULL|1", "NULL|7"],
            ),
            // GROUPING SETS among another's elements are its elements there.
            (
                "SELECT region, product, count(*) FROM gs.sale GROUP BY GROUPING SETS ((region), \
                 GROUPING SETS ((product), ())) ORDER BY GROUPING(region, product), region, product",
                &["N|NULL|3", "S|NULL|3", "NULL|NULL|1", "NULL|a|4", "NULL|b|2", "NULL|NULL|1", "NULL|NULL|7"],
            ),
            (
                "SELECT region, product, count(*) FROM gs.sale GROUP BY GROUPING SETS (GROUPING \
                 SETS ((region, product)), ()) ORDER BY GROUPING(region, product), region, product",
                &["N|a|2", "N|b|1", "S|a|2", "S|NULL|1", "NULL|b|1", "NULL|NULL|7"],
            ),
        ],
    );
}

#[test]
fn columns_and_sets_multiply() {
    let mut w = gs("multiply");
    answered(
        &mut w,
        &[
            (
                "SELECT region, product, count(*) FROM gs.sale GROUP BY region, ROLLUP (product) \
                 ORDER BY GROUPING(region, product), region, product",
                &["N|a|2", "N|b|1", "S|a|2", "S|NULL|1", "NULL|b|1", "N|NULL|3", "S|NULL|3", "NULL|NULL|1"],
            ),
            (
                "SELECT region, product, count(*) FROM gs.sale GROUP BY ROLLUP (region), CUBE \
                 (product) ORDER BY GROUPING(region, product), region, product",
                &[
                    "N|a|2", "N|b|1", "S|a|2", "S|NULL|1", "NULL|b|1", "N|NULL|3", "S|NULL|3",
                    "NULL|NULL|1", "NULL|a|4", "NULL|b|2", "NULL|NULL|1", "NULL|NULL|7",
                ],
            ),
            (
                "SELECT region, product, count(*) FROM gs.sale GROUP BY region, GROUPING SETS \
                 ((product), ()) ORDER BY GROUPING(region, product), region, product",
                &["N|a|2", "N|b|1", "S|a|2", "S|NULL|1", "NULL|b|1", "N|NULL|3", "S|NULL|3", "NULL|NULL|1"],
            ),
            (
                "SELECT region, product, qty, count(*) FROM gs.sale GROUP BY region, CUBE \
                 (product, qty) ORDER BY GROUPING(product, qty), region, product, qty",
                &[
                    "N|a|10|2", "N|b|20|1", "S|a|5|1", "S|a|NULL|1", "S|NULL|3|1", "NULL|b|7|1",
                    "N|a|NULL|2", "N|b|NULL|1", "S|a|NULL|2", "S|NULL|NULL|1", "NULL|b|NULL|1",
                    "N|NULL|10|2", "N|NULL|20|1", "S|NULL|3|1", "S|NULL|5|1", "S|NULL|NULL|1",
                    "NULL|NULL|7|1", "N|NULL|NULL|3", "S|NULL|NULL|3", "NULL|NULL|NULL|1",
                ],
            ),
            (
                "SELECT r.name, s.product, sum(s.qty) FROM gs.sale s JOIN gs.region r ON r.code = \
                 s.region GROUP BY ROLLUP (r.name, s.product) ORDER BY GROUPING(r.name, \
                 s.product), r.name, s.product",
                &[
                    "North|a|20", "North|b|20", "South|a|5", "South|NULL|3", "North|NULL|40",
                    "South|NULL|8", "NULL|NULL|48",
                ],
            ),
            (
                "SELECT x, count(*) FROM (SELECT region AS x FROM gs.sale) d GROUP BY ROLLUP (x) \
                 ORDER BY GROUPING(x), x",
                &["N|3", "S|3", "NULL|1", "NULL|7"],
            ),
            (
                "SELECT region, count(*) FROM gs.sale GROUP BY ROLLUP (region), region ORDER BY \
                 region",
                &["N|3", "N|3", "S|3", "S|3", "NULL|1", "NULL|1"],
            ),
            // At the top a list is its expressions, and `()` beside other items adds nothing.
            (
                "SELECT region, product, count(*) FROM gs.sale GROUP BY (region, product) ORDER BY \
                 region, product",
                &["N|a|2", "N|b|1", "S|a|2", "S|NULL|1", "NULL|b|1"],
            ),
            (
                "SELECT region, count(*) FROM gs.sale GROUP BY (), region ORDER BY region",
                &["N|3", "S|3", "NULL|1"],
            ),
        ],
    );
}

#[test]
fn an_empty_set_answers_over_no_rows() {
    let mut w = gs("empty");
    answered(
        &mut w,
        &[
            ("SELECT region, count(*) FROM gs.sale WHERE false GROUP BY ROLLUP (region)", &["NULL|0"]),
            (
                "SELECT region, count(*) FROM gs.sale WHERE false GROUP BY GROUPING SETS ((region), \
                 (), ())",
                &["NULL|0", "NULL|0"],
            ),
            ("SELECT count(*) FROM gs.sale WHERE false GROUP BY ()", &["0"]),
            (
                "SELECT region, product, count(*) FROM gs.sale WHERE false GROUP BY ROLLUP \
                 (region), ROLLUP (product)",
                &["NULL|NULL|0"],
            ),
        ],
    );
}

#[test]
fn a_key_decides_only_through_the_columns_every_set_holds() {
    let mut w = gs("key");
    assert_eq!(
        refused(&mut w, "SELECT region, count(*) FROM gs.sale GROUP BY ROLLUP (id)"),
        (
            Some("42803"),
            "column \"sale.region\" must appear in the GROUP BY clause or be used in an aggregate \
             function"
                .to_string()
        )
    );
    let each = ["1|N|1", "2|N|1", "3|S|1", "4|S|1", "5|NULL|1", "6|S|1", "7|N|1"];
    answered(
        &mut w,
        &[(
            "SELECT id, region, count(*) FROM gs.sale GROUP BY id, ROLLUP (product) ORDER BY \
             GROUPING(product), id",
            &[each, each].concat(),
        )],
    );
}

#[test]
fn group_by_distinct_drops_repeated_sets_and_all_keeps_them() {
    let mut w = gs("distinct");
    answered(
        &mut w,
        &[
            (
                "SELECT region, count(*) FROM gs.sale GROUP BY DISTINCT ROLLUP (region), ROLLUP \
                 (region) ORDER BY GROUPING(region), region",
                &["N|3", "S|3", "NULL|1", "NULL|7"],
            ),
            (
                "SELECT region, count(*) FROM gs.sale GROUP BY ALL ROLLUP (region), ROLLUP \
                 (region) ORDER BY GROUPING(region), region, 2",
                &["N|3", "N|3", "N|3", "S|3", "S|3", "S|3", "NULL|1", "NULL|1", "NULL|1", "NULL|7"],
            ),
            (
                "SELECT region, count(*) FROM gs.sale GROUP BY DISTINCT GROUPING SETS ((region), \
                 (region)) ORDER BY region",
                &["N|3", "S|3", "NULL|1"],
            ),
            (
                "SELECT region, count(*) FROM gs.sale GROUP BY ALL GROUPING SETS ((region), \
                 (region)) ORDER BY region",
                &["N|3", "N|3", "S|3", "S|3", "NULL|1", "NULL|1"],
            ),
            (
                "SELECT region, count(*) FROM gs.sale GROUP BY DISTINCT ROLLUP (region), CUBE \
                 (region) ORDER BY GROUPING(region), region, 2",
                &["N|3", "S|3", "NULL|1", "NULL|7"],
            ),
        ],
    );
}

/// GROUP BY ALL or DISTINCT with no item after it, refused as 18.6's grammar refuses it, at what
/// follows; under 19 a bare GROUP BY ALL is 19's own, not read.
#[test]
fn a_bare_group_by_all_or_distinct_is_a_syntax_error() {
    let path = tmp("reader-gs-bare.snail");
    let catalog = Catalog::parse(GS_DDL).unwrap();
    warren_snail::write::write_released(&path, &catalog, &[], &[], weather::span(), 18).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    for sql in GS_ROWS {
        w.execute(sql).unwrap();
    }
    for (sql, near) in [
        ("SELECT region, count(*) FROM gs.sale GROUP BY ALL;", "\";\""),
        ("SELECT region, count(*) FROM gs.sale GROUP BY ALL", "end of input"),
        ("SELECT region, count(*) FROM gs.sale GROUP BY ALL ORDER BY 1", "\"ORDER\""),
        (
            "SELECT region, count(*) FROM gs.sale GROUP BY ALL HAVING count(*) > 1",
            "\"HAVING\"",
        ),
        ("SELECT * FROM (SELECT region FROM gs.sale GROUP BY ALL) q", "\")\""),
        ("SELECT region FROM gs.sale GROUP BY ALL LIMIT 1", "\"LIMIT\""),
        ("SELECT region, count(*) FROM gs.sale GROUP BY DISTINCT;", "\";\""),
    ] {
        let at = match near {
            "end of input" => "syntax error at end of input".to_string(),
            n => format!("syntax error at or near {n}"),
        };
        assert_eq!(refused(&mut w, sql), (Some("42601"), at), "{sql}");
    }
    answered(
        &mut w,
        &[(
            "SELECT region, count(*) FROM gs.sale GROUP BY ALL region ORDER BY 1",
            &["N|3", "S|3", "NULL|1"],
        )],
    );
    let mut w = gs("bare-19");
    assert_eq!(
        refused(&mut w, "SELECT region, count(*) FROM gs.sale GROUP BY DISTINCT"),
        (Some("42601"), "syntax error at end of input".to_string())
    );
    assert_eq!(
        refused(&mut w, "SELECT region, count(*) FROM gs.sale GROUP BY ALL").1,
        "GROUP BY ALL and its modifiers are not read"
    );
}

#[test]
fn grouping_sets_a_bit_for_each_argument_the_set_lacks() {
    let mut w = gs("bits");
    answered(
        &mut w,
        &[
            (
                "SELECT region, product, GROUPING(region), GROUPING(product), GROUPING(region, \
                 product), GROUPING(product, region) FROM gs.sale GROUP BY CUBE (region, product) \
                 ORDER BY 5, 1, 2",
                &[
                    "N|a|0|0|0|0", "N|b|0|0|0|0", "S|a|0|0|0|0", "S|NULL|0|0|0|0",
                    "NULL|b|0|0|0|0", "N|NULL|0|1|1|2", "S|NULL|0|1|1|2", "NULL|NULL|0|1|1|2",
                    "NULL|a|1|0|2|1", "NULL|b|1|0|2|1", "NULL|NULL|1|0|2|1", "NULL|NULL|1|1|3|3",
                ],
            ),
            (
                "SELECT region, GROUPING(region) FROM gs.sale GROUP BY region ORDER BY region",
                &["N|0", "S|0", "NULL|0"],
            ),
            (
                "SELECT region, count(*), GROUPING(region) FROM gs.sale GROUP BY region HAVING \
                 GROUPING(region) = 0 ORDER BY region",
                &["N|3|0", "S|3|0", "NULL|1|0"],
            ),
        ],
    );
}

#[test]
fn a_set_s_null_stands_where_its_expression_is_named() {
    let mut w = gs("nulls");
    answered(
        &mut w,
        &[
            (
                "SELECT CASE WHEN GROUPING(region) = 1 THEN 'all' ELSE coalesce(region, '(none)') \
                 END AS r, sum(qty) FROM gs.sale GROUP BY ROLLUP (region) ORDER BY \
                 GROUPING(region), r",
                &["(none)|7", "N|40", "S|8", "all|55"],
            ),
            (
                "SELECT coalesce(region, 'all') AS r, sum(qty) FROM gs.sale GROUP BY ROLLUP \
                 (region) ORDER BY GROUPING(region), r",
                &["N|40", "S|8", "all|7", "all|55"],
            ),
            (
                "SELECT upper(region) AS r, count(*) FROM gs.sale GROUP BY ROLLUP (upper(region)) \
                 ORDER BY GROUPING(upper(region)), r",
                &["N|3", "S|3", "NULL|1", "NULL|7"],
            ),
            // An expression a set holds is its group's value though a column inside it is lacking.
            (
                "SELECT upper(region), region, count(*) FROM gs.sale GROUP BY GROUPING SETS \
                 ((upper(region)), (region)) ORDER BY 1, 2",
                &["N|NULL|3", "S|NULL|3", "NULL|N|3", "NULL|S|3", "NULL|NULL|1", "NULL|NULL|1"],
            ),
            // A subquery reads a lacking column NULL.
            (
                "SELECT region, (SELECT region) AS r, count(*) FROM gs.sale GROUP BY ROLLUP \
                 (region) ORDER BY GROUPING(region), region",
                &["N|N|3", "S|S|3", "NULL|NULL|1", "NULL|NULL|7"],
            ),
        ],
    );
}

#[test]
fn having_order_by_distinct_and_limit_read_every_set_s_rows() {
    let mut w = gs("having");
    answered(
        &mut w,
        &[
            (
                "SELECT region, product, sum(qty) FROM gs.sale GROUP BY CUBE (region, product) \
                 HAVING GROUPING(region, product) = 1 ORDER BY region",
                &["N|NULL|40", "S|NULL|8", "NULL|NULL|7"],
            ),
            (
                "SELECT region, product, count(*) FROM gs.sale GROUP BY ROLLUP (region, product) \
                 HAVING count(*) > 1 ORDER BY GROUPING(region, product), region, product",
                &["N|a|2", "S|a|2", "N|NULL|3", "S|NULL|3", "NULL|NULL|7"],
            ),
            (
                "SELECT region, sum(qty) FROM gs.sale GROUP BY ROLLUP (region) HAVING region IS \
                 NULL ORDER BY GROUPING(region)",
                &["NULL|7", "NULL|55"],
            ),
            (
                "SELECT region, product, sum(qty) AS s FROM gs.sale GROUP BY ROLLUP (region, \
                 product) ORDER BY s DESC NULLS LAST, GROUPING(region, product), region, product",
                &[
                    "NULL|NULL|55", "N|NULL|40", "N|a|20", "N|b|20", "S|NULL|8", "NULL|b|7",
                    "NULL|NULL|7", "S|a|5", "S|NULL|3",
                ],
            ),
            (
                "SELECT region, product, count(*) FROM gs.sale GROUP BY CUBE (region, product) \
                 ORDER BY GROUPING(region, product), region, product LIMIT 3",
                &["N|a|2", "N|b|1", "S|a|2"],
            ),
            (
                "SELECT DISTINCT count(*) AS n FROM gs.sale GROUP BY CUBE (region, product) ORDER \
                 BY n",
                &["1", "2", "3", "4", "7"],
            ),
            (
                "SELECT region, product FROM gs.sale GROUP BY CUBE (region, product) HAVING \
                 GROUPING(region, product) = 3",
                &["NULL|NULL"],
            ),
            (
                "SELECT region, count(*) FROM gs.sale GROUP BY ROLLUP (region) ORDER BY region \
                 NULLS FIRST, 2",
                &["NULL|1", "NULL|7", "N|3", "S|3"],
            ),
        ],
    );
}

#[test]
fn grouping_is_refused_where_postgresql_refuses_it() {
    let mut w = gs("refused");
    let grouping_error = "arguments to GROUPING must be grouping expressions of the associated \
                          query level";
    refusals(
        &mut w,
        &[
            ("SELECT GROUPING(qty) FROM gs.sale GROUP BY ROLLUP (region)", "42803", grouping_error),
            (
                "SELECT region FROM gs.sale WHERE GROUPING(region) = 0 GROUP BY ROLLUP (region)",
                "42803",
                "grouping operations are not allowed in WHERE",
            ),
            ("SELECT GROUPING(region) FROM gs.sale", "42803", grouping_error),
            (
                "SELECT region, sum(GROUPING(region)) FROM gs.sale GROUP BY ROLLUP (region)",
                "42803",
                "aggregate function calls cannot be nested",
            ),
            (
                "SELECT region, count(*) FROM gs.sale GROUP BY GROUPING(region)",
                "42803",
                "grouping operations are not allowed in GROUP BY",
            ),
            (
                "SELECT region, count(*) FROM gs.sale GROUP BY ROLLUP (region, GROUPING(region))",
                "42803",
                "grouping operations are not allowed in GROUP BY",
            ),
            (
                "SELECT GROUPING(region) AS g, count(*) FROM gs.sale GROUP BY 1",
                "42803",
                "aggregate functions are not allowed in GROUP BY",
            ),
            (
                "SELECT region, count(*) FILTER (WHERE GROUPING(region) = 0) FROM gs.sale GROUP \
                 BY ROLLUP (region)",
                "42803",
                "grouping operations are not allowed in FILTER",
            ),
            (
                "SELECT region FROM gs.sale GROUP BY ROLLUP (region) LIMIT GROUPING(region)",
                "42803",
                "grouping operations are not allowed in LIMIT",
            ),
            (
                "SELECT s.region FROM gs.sale s JOIN gs.region r ON GROUPING(s.region) = 0 GROUP \
                 BY ROLLUP (s.region)",
                "42803",
                "grouping operations are not allowed in JOIN conditions",
            ),
            (
                "SELECT region, sum(count(*)) FROM gs.sale GROUP BY ROLLUP (region)",
                "42803",
                "aggregate function calls cannot be nested",
            ),
            (
                "SELECT region, GROUPING(count(*)) FROM gs.sale GROUP BY ROLLUP (region)",
                "42803",
                "aggregate function calls cannot be nested",
            ),
            (
                "SELECT region, count(*) FROM gs.sale GROUP BY ROLLUP ((), region)",
                "42601",
                "syntax error at or near \")\"",
            ),
        ],
    );
    let many = vec!["region"; 32].join(", ");
    assert_eq!(
        refused(
            &mut w,
            &format!("SELECT region, GROUPING({many}) FROM gs.sale GROUP BY ROLLUP (region)")
        ),
        (Some("54023"), "GROUPING must have fewer than 32 arguments".to_string())
    );
}

#[test]
fn the_sets_are_limited_as_postgresql_limits_them() {
    let mut w = gs("limits");
    refusals(
        &mut w,
        &[
            (
                "SELECT count(*) FROM gs.sale GROUP BY CUBE (id, region, product, qty, id + 1, id \
                 + 2, id + 3, id + 4, id + 5, id + 6, id + 7, id + 8, id + 9)",
                "54011",
                "CUBE is limited to 12 elements",
            ),
            (
                "SELECT count(*) FROM gs.sale GROUP BY CUBE (id, region, product, qty, id + 1, id \
                 + 2), CUBE (id + 3, id + 4, id + 5, id + 6, id + 7, id + 8, id + 9)",
                "54001",
                "too many grouping sets present (maximum 4096)",
            ),
            // Each before a column no set holds.
            (
                "SELECT qty, count(*) FROM gs.sale GROUP BY CUBE (id, region, product, id + 1, id \
                 + 2, id + 3, id + 4, id + 5, id + 6, id + 7, id + 8, id + 9, id + 10)",
                "54011",
                "CUBE is limited to 12 elements",
            ),
            (
                "SELECT qty, count(*) FROM gs.sale GROUP BY CUBE (id, region, product, id + 1, id \
                 + 2, id + 3, id + 4), CUBE (id + 5, id + 6, id + 7, id + 8, id + 9, id + 10)",
                "54001",
                "too many grouping sets present (maximum 4096)",
            ),
        ],
    );
    // Twelve elements are taken, a pair counting once; so are thirteen in a ROLLUP.
    answered(
        &mut w,
        &[(
            "SELECT count(*) FROM gs.sale GROUP BY ROLLUP (id, region, product, qty, id + 1, id + \
             2, id + 3, id + 4, id + 5, id + 6, id + 7, id + 8, id + 9) HAVING count(*) > 6",
            &["7"],
        )],
    );
    let rows = rows(
        &mut w,
        "SELECT count(*) FROM gs.sale GROUP BY CUBE ((id, region), product, qty, id + 1, id + 2, id \
         + 3, id + 4, id + 5, id + 6, id + 7, id + 8, id + 9) HAVING count(*) = 7",
    );
    assert_eq!(rows, ["7"]);
}

// A relation's name alone reads its whole row, as 18.6 reads a lone name no column bears: a
// table's of its row type, a subquery's or a function's a record; a column of the name wins.

const WR_DDL: &str = "CREATE SCHEMA wr;
     CREATE TABLE wr.a (id integer NOT NULL, g integer, k integer, PRIMARY KEY (id));";

#[test]
fn a_relation_s_name_reads_its_whole_row() {
    let path = tmp("reader-whole-rows.snail");
    write(&path, &Catalog::parse(WR_DDL).unwrap(), &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute("INSERT INTO wr.a VALUES (1, 1, 10), (2, 1, 20), (3, 2, 30), (4, 2, 40), (5, NULL, 50)")
        .unwrap();
    answered(
        &mut w,
        &[
            ("SELECT a FROM wr.a a WHERE a.id < 3 ORDER BY a.id", &["(1,1,10)", "(2,1,20)"]),
            ("SELECT row_to_json(a) FROM wr.a a WHERE a.id = 1", &["{\"id\":1,\"g\":1,\"k\":10}"]),
            ("SELECT (a).k FROM wr.a a WHERE a.id = 1", &["10"]),
            (
                "SELECT a IS NULL, a = a, a IS NOT DISTINCT FROM a FROM wr.a a WHERE a.id = 5",
                &["f|t|t"],
            ),
            ("SELECT count(a) FROM wr.a a", &["5"]),
            ("SELECT a FROM wr.a WHERE id = 1", &["(1,1,10)"]),
            (
                "SELECT q FROM (SELECT id, k FROM wr.a WHERE id < 3) q ORDER BY q",
                &["(1,10)", "(2,20)"],
            ),
            ("SELECT t FROM unnest(ARRAY[5, 6]) WITH ORDINALITY AS t(v, n)", &["(5,1)", "(6,2)"]),
            ("SELECT f FROM generate_series(1, 2) f", &["1", "2"]),
            ("SELECT a FROM wr.a a ORDER BY a DESC LIMIT 2", &["(5,,50)", "(4,2,40)"]),
            (
                "SELECT a.g, array_agg(a ORDER BY a.id) FROM wr.a a GROUP BY a.g ORDER BY a.g",
                &[
                    "1|{\"(1,1,10)\",\"(2,1,20)\"}",
                    "2|{\"(3,2,30)\",\"(4,2,40)\"}",
                    "NULL|{\"(5,,50)\"}",
                ],
            ),
            ("SELECT g FROM wr.a g WHERE id = 1", &["1"]),
            (
                "SELECT t FROM jsonb_to_record('{\"x\":1,\"y\":\"a\"}') AS t(x integer, y text)",
                &["(1,a)"],
            ),
            ("SELECT a, a.k FROM wr.a a WHERE a.id = 2", &["(2,1,20)|20"]),
            ("SELECT a.id FROM wr.a a WHERE a IS NOT NULL AND a.id = 4", &["4"]),
            // A row is NOT NULL where each of its columns is.
            (
                "SELECT a.id, a IS NOT NULL FROM wr.a a ORDER BY a.id",
                &["1|t", "2|t", "3|t", "4|t", "5|f"],
            ),
            (
                "SELECT row_to_json(q) FROM (SELECT id, k FROM wr.a WHERE id = 1) q",
                &["{\"id\":1,\"k\":10}"],
            ),
            ("SELECT q FROM (SELECT id FROM wr.a WHERE id = 1) q", &["(1)"]),
            ("SELECT a FROM wr.a a, wr.a b WHERE a.id = b.id AND b.id = 1", &["(1,1,10)"]),
            ("SELECT a::text FROM wr.a a WHERE id = 1", &["(1,1,10)"]),
        ],
    );
    assert_eq!(
        refused(&mut w, "SELECT wr.a FROM wr.a"),
        (Some("42P01"), "missing FROM-clause entry for table \"wr\"".to_string())
    );
}

#[test]
fn a_row_constructor_s_record_is_compared_field_by_field_as_rows_meet() {
    let mut w = snail("records-meet");
    w.execute("CREATE TABLE dn.jj (id integer PRIMARY KEY, v json, p point)")
        .unwrap();
    let none: &[&str] = &[];
    answered(
        &mut w,
        &[
            ("SELECT DISTINCT ROW(v) FROM dn.jj", none),
            ("SELECT id FROM dn.jj ORDER BY ROW(v)", none),
            ("SELECT ROW(v) FROM dn.jj GROUP BY 1", none),
        ],
    );
    w.execute("INSERT INTO dn.jj VALUES (1, '{}', '(1,2)'), (2, '[]', '(3,4)')")
        .unwrap();
    let no_comparison = |ty: &str| {
        (
            Some("42883"),
            format!("could not identify a comparison function for type {ty}"),
        )
    };
    for (sql, ty) in [
        ("SELECT DISTINCT ROW(v) FROM dn.jj", "json"),
        ("SELECT id FROM dn.jj ORDER BY ROW(v)", "json"),
        ("SELECT ROW(v) FROM dn.jj GROUP BY 1", "json"),
        ("SELECT count(*) FROM dn.jj GROUP BY ROW(v)", "json"),
        ("SELECT DISTINCT ROW(p) FROM dn.jj", "point"),
        ("SELECT id FROM dn.jj ORDER BY ROW(p) DESC", "point"),
        ("SELECT DISTINCT ROW(1, v) FROM dn.jj", "json"),
        ("SELECT id FROM dn.jj ORDER BY ROW(1, v)", "json"),
        (
            "SELECT ROW(('\"' || k || '\"')::json) FROM dn.a WHERE id < 3 GROUP BY 1",
            "json",
        ),
        (
            "SELECT DISTINCT ROW(g, ('\"' || k || '\"')::json) FROM dn.a ORDER BY 1",
            "json",
        ),
    ] {
        assert_eq!(refused(&mut w, sql), no_comparison(ty), "{sql}");
    }
    // Rows that differ before the field are never compared there.
    answered(
        &mut w,
        &[
            ("SELECT DISTINCT ROW(v) FROM dn.jj WHERE id = 1", &["({})"]),
            ("SELECT id FROM dn.jj WHERE id = 1 ORDER BY ROW(v)", &["1"]),
            (
                "SELECT DISTINCT ROW(id, v) FROM dn.jj ORDER BY 1",
                &["(1,{})", "(2,[])"],
            ),
            ("SELECT id FROM dn.jj ORDER BY ROW(id, v) DESC", &["2", "1"]),
            (
                "SELECT ROW(id, v) FROM dn.jj GROUP BY 1 ORDER BY 1",
                &["(1,{})", "(2,[])"],
            ),
            (
                "SELECT DISTINCT ROW(id, ('\"' || k || '\"')::json) FROM dn.a ORDER BY 1",
                &[
                    "(1,\"\"\"x\"\"\")",
                    "(2,\"\"\"y\"\"\")",
                    "(3,\"\"\"x\"\"\")",
                    "(4,\"\"\"x\"\"\")",
                    "(5,\"\"\"y\"\"\")",
                    "(6,)",
                    "(7,)",
                ],
            ),
        ],
    );
}

#[test]
fn a_literal_and_a_cast_are_read_as_the_statement_is_typed() {
    let mut w = snail("typed-literals");
    let syntax = |ty: &str, s: &str| {
        (
            Some("22P02"),
            format!("invalid input syntax for type {ty}: \"{s}\""),
        )
    };
    let xid = || (Some("42846"), "cannot cast type integer to xid".to_string());
    for (sql, want) in [
        ("SELECT id + 'x' FROM dn.a WHERE false", syntax("integer", "x")),
        ("SELECT id::xid FROM dn.a WHERE false", xid()),
        ("SELECT id + 'x' FROM dn.a ORDER BY k::json", syntax("integer", "x")),
        (
            "SELECT id FROM dn.a ORDER BY k::json, id + 'x'",
            (
                Some("42883"),
                "could not identify an ordering operator for type json\nHINT:  Use an explicit \
                 ordering operator or modify the query."
                    .to_string(),
            ),
        ),
        ("SELECT 'x'::integer FROM dn.a WHERE false", syntax("integer", "x")),
        ("SELECT 1 FROM dn.a WHERE false AND id = 'y'", syntax("integer", "y")),
        ("SELECT CASE WHEN false THEN id + 'x' END FROM dn.a", syntax("integer", "x")),
        ("SELECT id FROM dn.a WHERE id IN (1, 'z')", syntax("integer", "z")),
        (
            "SELECT nosuch, id + 'x' FROM dn.a",
            (Some("42703"), "column \"nosuch\" does not exist".to_string()),
        ),
        ("SELECT id + 'x', nosuch FROM dn.a", syntax("integer", "x")),
        ("SELECT id FROM dn.a WHERE false ORDER BY id::xid", xid()),
        ("SELECT id FROM dn.a GROUP BY id HAVING id > 'q'", syntax("integer", "q")),
        ("SELECT 1/0, id + 'x' FROM dn.a", syntax("integer", "x")),
        ("SELECT ARRAY[id, 'x'] FROM dn.a WHERE false", syntax("integer", "x")),
        (
            "SELECT row_number() OVER (ORDER BY id + 'x') FROM dn.a WHERE false",
            syntax("integer", "x"),
        ),
        ("INSERT INTO dn.b VALUES (9, 1, 'x')", syntax("integer", "x")),
        ("UPDATE dn.b SET v = 'x' WHERE false", syntax("integer", "x")),
        (
            "SELECT a_id::xid FROM dn.b WHERE false",
            (Some("42846"), "cannot cast type integer to xid".to_string()),
        ),
    ] {
        assert_eq!(refused(&mut w, sql), want, "{sql}");
    }
    let none: &[&str] = &[];
    answered(
        &mut w,
        &[
            ("SELECT id::text::xid FROM dn.a WHERE false", none),
            ("SELECT id FROM dn.a WHERE id = '1'", &["1"]),
            ("SELECT g + '5' FROM dn.a WHERE id = 1", &["15"]),
            ("SELECT id || 'x' FROM dn.a WHERE id = 1", &["1x"]),
        ],
    );
}

#[test]
fn a_where_comparing_constants_alone_is_read_as_any_condition() {
    let mut w = snail("constants-compared");
    let all: &[&str] = &["1", "2", "3", "4", "5", "6", "7"];
    answered(
        &mut w,
        &[
            ("SELECT id FROM dn.a WHERE 'pg_class'::regclass::oid = 1259", all),
            ("SELECT id FROM dn.a WHERE 1 = 1 ORDER BY id", all),
            ("SELECT id FROM dn.a WHERE 2 > 1 AND id = 1", &["1"]),
            ("SELECT id FROM dn.a WHERE 1 = 2", &[]),
            ("SELECT id FROM dn.a WHERE 'a' = 'a' AND g = 10 ORDER BY id", &["1", "3"]),
            ("SELECT id FROM dn.a WHERE NULL::int = 1", &[]),
            ("SELECT id FROM dn.a WHERE 1 = 1 AND 2 = 2 AND id < 3 ORDER BY id", &["1", "2"]),
            ("SELECT id FROM dn.a WHERE id = 1 OR 1 = 2", &["1"]),
            ("SELECT id FROM dn.a WHERE 1 IS NOT NULL AND id < 3 ORDER BY id", &["1", "2"]),
            ("SELECT id FROM dn.a WHERE NULL IS NULL AND id = 4", &["4"]),
        ],
    );
}

#[test]
fn a_values_list_reads_its_subqueries() {
    let mut w = snail("values-subqueries");
    answered(
        &mut w,
        &[
            ("VALUES ((SELECT max(g) FROM dn.a)), (2) ORDER BY 1", &["2", "30"]),
            ("SELECT * FROM (VALUES ((SELECT 1))) v", &["1"]),
            ("VALUES (1, (SELECT count(*) FROM dn.a))", &["1|7"]),
            ("WITH v AS (VALUES ((SELECT min(id) FROM dn.a))) SELECT * FROM v", &["1"]),
            (
                "VALUES (EXISTS (SELECT 1 FROM dn.a WHERE id = 9)), (2 IN (SELECT id FROM dn.a))",
                &["f", "t"],
            ),
            (
                "VALUES ((SELECT name FROM dn.a WHERE id = 3), 'x'), ('y', (SELECT k FROM dn.a \
                 WHERE id = 2))",
                &["three|x", "y|y"],
            ),
            (
                "SELECT * FROM (VALUES ((SELECT g FROM dn.a WHERE id = 6)), (5)) v(c) ORDER BY c",
                &["5", "NULL"],
            ),
        ],
    );
    for (sql, want) in [
        (
            "SELECT * FROM (VALUES ((SELECT id FROM dn.a))) v",
            (Some("21000"), "more than one row returned by a subquery used as an expression"),
        ),
        (
            "SELECT * FROM (VALUES ((SELECT id, g FROM dn.a WHERE id = 1))) v",
            (Some("42601"), "subquery must return only one column"),
        ),
        ("SELECT * FROM (VALUES (1), ((SELECT 1/0))) v", (Some("22012"), "division by zero")),
    ] {
        assert_eq!(refused(&mut w, sql), (want.0, want.1.to_string()), "{sql}");
    }
}

#[test]
fn a_relation_s_star_cast_is_its_whole_row() {
    let mut w = snail("star-cast");
    answered(
        &mut w,
        &[
            ("SELECT a.*::text FROM dn.a a WHERE a.id = 1", &["(1,10,x,one)"]),
            ("SELECT length(a.*::text) FROM dn.a a WHERE a.id = 1", &["12"]),
            ("SELECT count(*) FROM dn.a a WHERE length(a.*::text) > 12", &["3"]),
            ("SELECT (a.*)::text FROM dn.a a WHERE a.id = 6", &["(6,,,six)"]),
            ("SELECT CAST(a.* AS text) FROM dn.a a WHERE a.id = 1", &["(1,10,x,one)"]),
            ("SELECT a.*::dn.a FROM dn.a a WHERE a.id = 1", &["(1,10,x,one)"]),
            ("SELECT dn.a.*::text FROM dn.a WHERE id = 2", &["(2,20,y,two)"]),
            ("SELECT a.* :: text FROM dn.a a WHERE a.id = 3", &["(3,10,x,three)"]),
            (
                "SELECT b.id, a.*::text FROM dn.a a JOIN dn.b b ON b.a_id = a.id WHERE b.id < 3 \
                 ORDER BY b.id",
                &["1|(1,10,x,one)", "2|(1,10,x,one)"],
            ),
        ],
    );
}

#[test]
fn a_relation_s_star_an_operator_follows_is_its_whole_row() {
    let path = tmp("reader-star-operators.snail");
    write(&path, &Catalog::parse(WR_DDL).unwrap(), &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute("INSERT INTO wr.a VALUES (1, 1, 10), (2, 1, 20), (3, 2, 30), (4, 2, 40), (5, NULL, 50)")
        .unwrap();
    answered(
        &mut w,
        &[
            (
                "SELECT a.id, a.* IS NOT NULL FROM wr.a a ORDER BY a.id",
                &["1|t", "2|t", "3|t", "4|t", "5|f"],
            ),
            ("SELECT wr.a.* IS NULL FROM wr.a WHERE id = 1", &["f"]),
            (
                "SELECT a.id, a.* IS NULL FROM wr.a a ORDER BY a.id",
                &["1|f", "2|f", "3|f", "4|f", "5|f"],
            ),
            ("SELECT a.* = a.* FROM wr.a a WHERE id = 5", &["t"]),
            (
                "SELECT a.id, a.* > b.* FROM wr.a a, wr.a b WHERE b.id = 2 ORDER BY a.id",
                &["1|f", "2|f", "3|t", "4|t", "5|t"],
            ),
            (
                "SELECT a.* IS NOT NULL AND a.id > 2 FROM wr.a a ORDER BY a.id",
                &["f", "f", "t", "t", "f"],
            ),
            ("SELECT NOT a.* IS NULL FROM wr.a a WHERE id = 1", &["t"]),
            ("SELECT a.* IS NOT DISTINCT FROM a.* FROM wr.a a WHERE id = 5", &["t"]),
            ("SELECT a.*, a.* IS NULL FROM wr.a a WHERE id = 5", &["5|NULL|50|f"]),
            (
                "SELECT a.* NOT IN (SELECT b FROM wr.a b WHERE b.id = 2) FROM wr.a a WHERE id = 1",
                &["t"],
            ),
            (
                "SELECT a.id, a.* IS DISTINCT FROM b.* FROM wr.a a JOIN wr.a b ON b.id = 2 ORDER \
                 BY 1",
                &["1|t", "2|f", "3|t", "4|t", "5|t"],
            ),
            ("SELECT coalesce(a.* IS NULL, true) FROM wr.a a WHERE id = 1", &["f"]),
            ("SELECT a.*::text || 'x' FROM wr.a a WHERE id = 1", &["(1,1,10)x"]),
            ("SELECT count(*) FILTER (WHERE a.* IS NOT NULL) FROM wr.a a", &["4"]),
            (
                "SELECT a.id FROM wr.a a ORDER BY a.* IS NULL, a.id DESC",
                &["5", "4", "3", "2", "1"],
            ),
            (
                "SELECT a.* IS NOT NULL FROM wr.a a WHERE id = 5 UNION ALL SELECT true",
                &["f", "t"],
            ),
            ("SELECT a.*IS NULL FROM wr.a a WHERE id = 1", &["f"]),
        ],
    );
    // Written against the star, an operator's character is one operator with it, as PostgreSQL's
    // lexer reads `*=` and `*||`: a syntax error.
    for sql in [
        "SELECT a.*= a.* FROM wr.a a WHERE id = 1",
        "SELECT a.*||'x' FROM wr.a a WHERE id = 1",
    ] {
        assert_eq!(refused(&mut w, sql).0, Some("42601"), "{sql}");
    }
    // Named as PostgreSQL names each: a cast by the relation the star names, an operator's not.
    for (sql, name) in [
        ("SELECT a.*::text FROM wr.a a WHERE id = 1", "a"),
        ("SELECT CAST(a.* AS text) FROM wr.a a WHERE id = 1", "a"),
        ("SELECT a::text FROM wr.a a WHERE id = 1", "a"),
        ("SELECT a.* IS NULL FROM wr.a a WHERE id = 1", "?column?"),
    ] {
        let b = w.execute(sql).unwrap().answer.unwrap();
        assert_eq!(b.schema().field(0).name(), name, "{sql}");
    }
}

#[test]
fn a_star_of_no_relation_is_refused_as_a_missing_entry() {
    let path = tmp("reader-stars-unheld.snail");
    write(&path, &Catalog::parse(WR_DDL).unwrap(), &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute("INSERT INTO wr.a VALUES (1, 1, 10), (2, 1, 20)").unwrap();
    let missing = |t: &str| (Some("42P01"), format!("missing FROM-clause entry for table \"{t}\""));
    for (sql, want) in [
        ("SELECT ROW(x.*) FROM wr.a a", missing("x")),
        ("SELECT count(x.*) FROM wr.a a", missing("x")),
        ("SELECT a.id FROM wr.a a ORDER BY x.*", missing("x")),
        ("SELECT x.* IS NULL FROM wr.a a", missing("x")),
        ("SELECT (x.*) IS NULL FROM wr.a a", missing("x")),
        ("SELECT a.id FROM wr.a a WHERE x.* IS NULL", missing("x")),
        ("SELECT a.id FROM wr.a a, wr.a b WHERE row_to_json(x.*) IS NULL", missing("x")),
        (
            "SELECT a.id FROM wr.a a WHERE EXISTS (SELECT 1 FROM wr.a b WHERE row_to_json(x.*) IS \
             NOT NULL)",
            missing("x"),
        ),
        ("SELECT row_to_json(wr.b.*) FROM wr.a", missing("b")),
    ] {
        assert_eq!(refused(&mut w, sql), want, "{sql}");
    }
    assert_eq!(refused(&mut w, "SELECT ROW(wr.a.*) FROM wr.a a").0, Some("42P01"));
}

#[test]
fn a_fixed_length_type_s_subscript_is_its_element() {
    let path = tmp("reader-raw-subscripts.snail");
    let ddl = "CREATE SCHEMA gx;
         CREATE TABLE gx.g (id integer PRIMARY KEY, p point, b box, l lseg, ln line, n name);";
    write(&path, &Catalog::parse(ddl).unwrap(), &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute(
        "INSERT INTO gx.g VALUES (1, '(3,4)', '(1,2),(3,4)', '[(1,2),(5,6)]', '{1,2,3}', 'abc'), \
         (2, '(1,9)', '(0,0),(2,2)', '[(0,0),(1,1)]', '{1,-1,0}', 'de'), \
         (3, NULL, NULL, NULL, NULL, NULL), \
         (4, '(2,-1)', '(5,5),(6,7)', '[(9,9),(8,8)]', '{0,1,5}', 'z')",
    )
    .unwrap();
    answered(
        &mut w,
        &[
            ("SELECT id FROM gx.g ORDER BY p[0]", &["2", "4", "1", "3"]),
            (
                "SELECT id, p[0], p[1] FROM gx.g ORDER BY id",
                &["1|3|4", "2|1|9", "3|NULL|NULL", "4|2|-1"],
            ),
            ("SELECT id FROM gx.g ORDER BY p[1] DESC", &["3", "2", "1", "4"]),
            ("SELECT p[2], p[-1], p[0][0] FROM gx.g WHERE id = 1", &["NULL|NULL|NULL"]),
            ("SELECT p['0'], p[1.6] FROM gx.g WHERE id = 1", &["3|NULL"]),
            ("SELECT p[id - 1] FROM gx.g ORDER BY id", &["3", "9", "NULL", "NULL"]),
            (
                "SELECT pg_typeof(p[0]), pg_typeof(b[0]), pg_typeof(n[0]) FROM gx.g WHERE id = 1",
                &["double precision|point|\"char\""],
            ),
            ("SELECT b[0], b[1], b[0][1] FROM gx.g WHERE id = 1", &["(3,4)|(1,2)|NULL"]),
            ("SELECT l[0], l[1] FROM gx.g WHERE id = 1", &["(1,2)|(5,6)"]),
            ("SELECT ln[0], ln[1], ln[2], ln[3] FROM gx.g WHERE id = 1", &["1|2|3|NULL"]),
            ("SELECT n[0], n[2], n[3], n[64] FROM gx.g WHERE id = 1", &["a|c||NULL"]),
            ("SELECT n[0] = 'a', n[3] = '' FROM gx.g WHERE id = 1", &["t|t"]),
            ("SELECT id FROM gx.g WHERE p[0] > 2 ORDER BY id", &["1"]),
            ("SELECT (point '(7,8)')[1]", &["8"]),
            ("SELECT sum(p[0]) FROM gx.g", &["6"]),
            ("SELECT (b[0])[1] FROM gx.g WHERE id = 1", &["4"]),
            ("SELECT p[0:1] FROM gx.g WHERE id = 3", &["NULL"]),
            ("SELECT p[NULL] FROM gx.g WHERE id = 1", &["NULL"]),
            ("SELECT id, p[0] FROM gx.g WHERE p[1] < 5 ORDER BY p[0]", &["4|2", "1|3"]),
        ],
    );
    // Named as PostgreSQL names a subscript: by what it is taken from.
    let b = w.execute("SELECT p[0], g.b[1], (b[0])[1] FROM gx.g g WHERE id = 1").unwrap();
    let schema = b.answer.unwrap().schema();
    let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
    assert_eq!(names, ["p", "b", "b"]);
    for (sql, want) in [
        (
            "SELECT p[0:1] FROM gx.g WHERE id = 1",
            (Some("0A000"), "slices of fixed-length arrays not implemented"),
        ),
        (
            "SELECT p['x'] FROM gx.g WHERE id = 1",
            (Some("22P02"), "invalid input syntax for type integer: \"x\""),
        ),
        (
            "SELECT p[true] FROM gx.g WHERE id = 1",
            (Some("42804"), "array subscript must have type integer"),
        ),
        (
            "SELECT p[0] FROM gx.g WHERE id = 1 UNION ALL SELECT 'x'::text",
            (Some("42804"), "UNION types double precision and text cannot be matched"),
        ),
    ] {
        assert_eq!(refused(&mut w, sql), (want.0, want.1.to_string()), "{sql}");
    }
    // A view's subscripts, analyzed and printed as 18.6 prints them.
    w.execute("CREATE VIEW gx.v AS SELECT id, p[0] AS x, b[1] AS lo FROM gx.g").unwrap();
    answered(
        &mut w,
        &[
            (
                "SELECT * FROM gx.v ORDER BY id",
                &["1|3|(1,2)", "2|1|(0,0)", "3|NULL|NULL", "4|2|(5,5)"],
            ),
            (
                "SELECT pg_get_viewdef('gx.v'::regclass)",
                &[" SELECT id,\n    p[0] AS x,\n    b[1] AS lo\n   FROM gx.g;"],
            ),
        ],
    );
}

#[test]
fn an_arm_s_sorted_literal_is_text_before_its_set_operation_is_typed() {
    let mut w = snail("sorted-literal");
    let unmatched = |op: &str, a: &str, b: &str| {
        (
            Some("42804"),
            format!("{op} types {a} and {b} cannot be matched"),
        )
    };
    for (sql, want) in [
        ("SELECT 1 UNION SELECT DISTINCT '2'", unmatched("UNION", "integer", "text")),
        ("SELECT 1 UNION SELECT DISTINCT '2'::text", unmatched("UNION", "integer", "text")),
        ("SELECT DISTINCT '2' UNION SELECT 1", unmatched("UNION", "text", "integer")),
        ("SELECT 1 INTERSECT SELECT DISTINCT 'x'", unmatched("INTERSECT", "integer", "text")),
        ("SELECT 1 EXCEPT SELECT DISTINCT '2'", unmatched("EXCEPT", "integer", "text")),
        ("SELECT 1 UNION ALL SELECT DISTINCT '2'", unmatched("UNION", "integer", "text")),
        ("SELECT 1 UNION SELECT DISTINCT ON (1) '2'", unmatched("UNION", "integer", "text")),
        ("SELECT 1 UNION (SELECT '2' ORDER BY 1)", unmatched("UNION", "integer", "text")),
        ("SELECT 1 UNION SELECT '2' GROUP BY 1", unmatched("UNION", "integer", "text")),
        ("SELECT 1 UNION SELECT DISTINCT NULL", unmatched("UNION", "integer", "text")),
    ] {
        assert_eq!(refused(&mut w, sql), want, "{sql}");
    }
    answered(
        &mut w,
        &[
            ("SELECT 1 UNION SELECT ALL '2' ORDER BY 1", &["1", "2"]),
            ("SELECT 1, 1 UNION ALL (SELECT '2', 2 ORDER BY 2)", &["1|1", "2|2"]),
            ("SELECT 'a' UNION SELECT DISTINCT 'b' ORDER BY 1", &["a", "b"]),
        ],
    );
}

/// A relation's star where an expression or a call's argument stands reads its whole row, as its
/// name does; among ROW's arguments its columns; in a subquery's select list, where the
/// subquery's FROM names no relation so, the columns of the query's around it.
#[test]
fn a_relation_s_star_reads_its_whole_row() {
    let path = tmp("reader-whole-stars.snail");
    write(&path, &Catalog::parse(WR_DDL).unwrap(), &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute("INSERT INTO wr.a VALUES (1, 1, 10), (2, 1, 20), (3, 2, 30), (4, 2, 40), (5, NULL, 50)")
        .unwrap();
    answered(
        &mut w,
        &[
            (
                "SELECT ROW(a.*) FROM wr.a a WHERE a.id < 3 ORDER BY a.id",
                &["(1,1,10)", "(2,1,20)"],
            ),
            ("SELECT count(a.*) FROM wr.a a", &["5"]),
            ("SELECT row_to_json(a.*) FROM wr.a a WHERE a.id = 1", &["{\"id\":1,\"g\":1,\"k\":10}"]),
            (
                "SELECT a.id FROM wr.a a WHERE a.id < 3 AND EXISTS (SELECT a.*) ORDER BY a.id",
                &["1", "2"],
            ),
            ("SELECT a.id FROM wr.a a ORDER BY a.* DESC", &["5", "4", "3", "2", "1"]),
            ("SELECT ROW(a.*, 0) FROM wr.a a WHERE a.id = 1", &["(1,1,10,0)"]),
            (
                "SELECT row_to_json(a.*), row_to_json(b.*) FROM wr.a a JOIN wr.a b ON b.id = a.id \
                 + 1 WHERE a.id = 1",
                &["{\"id\":1,\"g\":1,\"k\":10}|{\"id\":2,\"g\":1,\"k\":20}"],
            ),
            (
                "SELECT a.id FROM wr.a a WHERE EXISTS (SELECT a.* FROM wr.a b WHERE b.g = a.g AND \
                 b.id <> a.id) ORDER BY a.id",
                &["1", "2", "3", "4"],
            ),
            (
                "SELECT a.id, (SELECT count(*) FROM (SELECT a.* FROM wr.a a WHERE a.id < 3) q) FROM \
                 wr.a a WHERE a.id = 1",
                &["1|2"],
            ),
            (
                "SELECT a.id, (SELECT max(q.x) FROM (SELECT a.* FROM (VALUES (7), (8)) a(x)) q) \
                 FROM wr.a a WHERE a.id = 1",
                &["1|8"],
            ),
            ("SELECT count(DISTINCT a.*) FROM wr.a a", &["5"]),
        ],
    );
}
