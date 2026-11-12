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
