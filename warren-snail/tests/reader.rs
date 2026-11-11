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
            (Some(*code), words.to_string()),
            "{sql}"
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
        "CREATE SCHEMA s; CREATE TABLE s.t (id integer NOT NULL, g integer, PRIMARY KEY (id));",
    )
    .unwrap();
    let t = Rows {
        columns: vec![
            Values::Int((1..=n).map(Some).collect()),
            Values::Int((1..=n).map(|i| Some(i % 7)).collect()),
        ],
    };
    write(&path, &catalog, &[("s.t".into(), t)]).unwrap();
    let mut s = Snail::open_writing(&path).unwrap();
    let mut first: Vec<(i64, String)> = Vec::new();
    for r in rows(&mut s, "SELECT g, id FROM s.t") {
        let (g, _) = r.split_once('|').unwrap();
        let g: i64 = g.parse().unwrap();
        if !first.iter().any(|(k, _)| *k == g) {
            first.push((g, r));
        }
    }
    first.sort();
    let want: Vec<String> = first.into_iter().map(|(_, r)| r).collect();
    for room in [256 << 10, 64 << 10] {
        s.size_rounds(Sizes::of(room));
        assert_eq!(
            rows(&mut s, "SELECT DISTINCT ON (g) g, id FROM s.t"),
            want,
            "{room}"
        );
    }
}
