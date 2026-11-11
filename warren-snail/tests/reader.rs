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

/// Each statement refused as 18.6 refuses it, its SQLSTATE and its words, in its order.
fn refusals(w: &mut Snail, asked: &[(&str, &str, &str)]) {
    for (sql, code, words) in asked {
        assert_eq!(
            refused(w, sql),
            (Some(*code), words.to_string()),
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
            "function generate_series(double precision, integer) does not exist",
        ),
        (
            "'2026-01-01'::timestamp, '2026-01-02'",
            "42883",
            "function generate_series(timestamp without time zone, unknown) does not exist",
        ),
        (
            "1, 2, '1 day'::interval",
            "42883",
            "function generate_series(integer, integer, interval) does not exist",
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
    // 18.6 reads these as the whole row, a record: (5,1) and (6,2); (1,a) and (2,b). The snail
    // refuses a whole-row reference past a lone function with no ordinality as it refuses a
    // table's, in words of its own.
    refusals(
        &mut w,
        &[
            (
                "SELECT u FROM unnest(ARRAY[5, 6]) WITH ORDINALITY AS u(v, n)",
                "42703",
                "column \"u\" does not exist",
            ),
            (
                "SELECT t FROM unnest(ARRAY[1, 2], ARRAY['a', 'b']) AS t(n, s)",
                "42703",
                "column \"t\" does not exist",
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
                "function pg_catalog.unnest(integer) does not exist",
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
