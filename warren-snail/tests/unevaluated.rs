//! What PostgreSQL 18.6's plan never reaches is never evaluated over rows either: a subquery a
//! constant AND, OR, CASE or COALESCE drops, read neither while its shape is found nor as a row
//! asks; an EXISTS's select list; an argument an inlined function's body never names; a level its
//! one-time conditions shut, and what is below it; a WITH query nothing reads; a subquery's output
//! nothing reads; a select list past the rows its LIMIT keeps. Every answer and refusal was given
//! by 18.6 over the same statements, in the same order.
#![cfg(feature = "standalone")]

mod weather;

use warren_snail::catalog::Catalog;
use warren_snail::read::Snail;
use weather::{tmp, write};

const DDL: &str = "CREATE SCHEMA fo;
     CREATE TABLE fo.src (k integer NOT NULL, v integer, PRIMARY KEY (k));
     CREATE TABLE fo.none (k integer NOT NULL, v integer, PRIMARY KEY (k));
     CREATE FUNCTION fo.fu(integer, integer) RETURNS integer LANGUAGE sql VOLATILE AS 'SELECT $1';
     CREATE FUNCTION fo.flim() RETURNS integer LANGUAGE sql VOLATILE AS 'SELECT 1/(k - 2) FROM fo.src';";

/// A snail holding four rows in `fo.src` and none in `fo.none`, at a path of the test's name.
fn snail(name: &str) -> Snail {
    let path = tmp(&format!("unevaluated-{name}.snail"));
    write(&path, &Catalog::parse(DDL).unwrap(), &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute("INSERT INTO fo.src VALUES (1, 10), (2, 20), (3, 30), (4, 40)")
        .unwrap();
    w
}

type Outcome = Result<Vec<String>, (Option<&'static str>, String)>;

/// A statement's answer, each row's values as PostgreSQL's output function writes them, `|`
/// between them; or its refusal, its SQLSTATE and its words.
fn outcome(w: &mut Snail, sql: &str) -> Outcome {
    let done = w.execute(sql).map_err(|e| (e.sqlstate(), e.to_string()))?;
    Ok(done
        .answer
        .map(|b| {
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
        })
        .unwrap_or_default())
}

fn zero() -> Outcome {
    Err((Some("22012"), "division by zero".into()))
}

fn rows(r: &[&str]) -> Outcome {
    Ok(r.iter().map(|s| s.to_string()).collect())
}

/// Each statement answered or refused as 18.6 answers or refuses it, in its order; every one that
/// is not, named.
fn each(w: &mut Snail, asked: &[(&str, Outcome)]) {
    let wrong: Vec<String> = asked
        .iter()
        .filter_map(|(sql, want)| {
            let got = outcome(w, sql);
            (&got != want).then(|| format!("{sql}\n  got {got:?}\n  want {want:?}"))
        })
        .collect();
    assert!(
        wrong.is_empty(),
        "{} of {}:\n{}",
        wrong.len(),
        asked.len(),
        wrong.join("\n")
    );
}

#[test]
fn a_subquery_a_constant_drops_is_never_read() {
    let mut w = snail("dropped");
    let all = || rows(&["1", "2", "3", "4"]);
    let ones = || rows(&["1", "1", "1", "1"]);
    each(
        &mut w,
        &[
            ("SELECT k FROM fo.src WHERE k = (SELECT 1/0) AND false", rows(&[])),
            ("SELECT k FROM fo.src WHERE true OR k IN (SELECT 1/0) ORDER BY k", all()),
            ("SELECT CASE WHEN true THEN 1 ELSE (SELECT 1/0) END FROM fo.src", ones()),
            ("SELECT k FROM fo.src n WHERE n.k NOT IN (SELECT 1/0) AND false", rows(&[])),
            ("SELECT k FROM fo.src n WHERE n.k = ALL (SELECT 1/0) AND false", rows(&[])),
            ("SELECT k FROM fo.src WHERE (SELECT 1/0) IS NULL AND false", rows(&[])),
            ("SELECT k FROM fo.src n WHERE false AND EXISTS (SELECT 1/0 FROM fo.src)", rows(&[])),
            ("SELECT k FROM fo.src WHERE k = (SELECT 1/0 FROM fo.src) OR true ORDER BY k", all()),
            ("SELECT CASE WHEN false THEN (SELECT 1/0) ELSE k END FROM fo.src ORDER BY 1", all()),
            ("SELECT coalesce(1, (SELECT 1/0)) FROM fo.src", ones()),
            ("SELECT k = (SELECT 1/0 FROM fo.src) AND false FROM fo.src", rows(&["f", "f", "f", "f"])),
            (
                "SELECT k FROM fo.src WHERE (k = (SELECT 1/0 FROM fo.src) OR true) AND k > 2 ORDER BY k",
                rows(&["3", "4"]),
            ),
            // What the constant does not settle is read.
            ("SELECT k = (SELECT 1/(v - 10) FROM fo.src WHERE k = 1) AND true FROM fo.src", zero()),
            ("SELECT k FROM fo.src WHERE k = (SELECT 1/(v - 10) FROM fo.src WHERE k = 1) OR false", zero()),
            ("SELECT k FROM fo.src WHERE false OR k = (SELECT 1/(v - 10) FROM fo.src WHERE k = 1)", zero()),
            ("SELECT k FROM fo.src WHERE (SELECT 1/(v - 10) FROM fo.src WHERE k = 1) IS NULL AND true", zero()),
            ("SELECT CASE WHEN k > 2 THEN (SELECT 1/(v - 10) FROM fo.src WHERE k = 1) END FROM fo.src", zero()),
            // A subquery of no FROM: its names read as its shape is found, its value as a row asks.
            (
                "SELECT k FROM fo.src WHERE k = (SELECT nosuch)",
                Err((Some("42703"), "column \"nosuch\" does not exist".into())),
            ),
            ("SELECT (SELECT k) FROM fo.src ORDER BY 1", all()),
            ("SELECT (SELECT 1/(k - 1)) FROM fo.src", zero()),
            ("SELECT k, (SELECT 1/(k - 1)) FROM fo.src WHERE k > 1 ORDER BY 1", rows(&["2|1", "3|0", "4|0"])),
            // An argument an inlined body never names.
            ("SELECT fo.fu(1, (SELECT 1/0))", rows(&["1"])),
            ("SELECT fo.fu(k, (SELECT 1/0)) FROM fo.src ORDER BY 1", all()),
            ("SELECT fo.fu(k, (SELECT 1/0 FROM fo.src)) FROM fo.src ORDER BY 1", all()),
            (
                "SELECT fo.fu(k, (SELECT 1/(s.k - 1) FROM fo.src s WHERE s.k = fo.src.k)) FROM fo.src ORDER BY 1",
                all(),
            ),
        ],
    );
}

#[test]
fn an_exists_reads_no_select_list() {
    let mut w = snail("exists");
    each(
        &mut w,
        &[
            ("SELECT EXISTS (SELECT 1/0)", rows(&["t"])),
            ("SELECT EXISTS (SELECT 1/0 FROM fo.src)", rows(&["t"])),
            ("SELECT EXISTS (SELECT 1/0 FROM fo.src LIMIT 1)", rows(&["t"])),
            ("SELECT EXISTS (SELECT 1/(k - 1) FROM fo.src LIMIT 2)", rows(&["t"])),
            ("SELECT EXISTS (SELECT DISTINCT 1/0 FROM fo.src)", rows(&["t"])),
            ("SELECT EXISTS (SELECT 1/0 FROM fo.src ORDER BY 1)", rows(&["t"])),
            ("SELECT EXISTS (SELECT 1/(k - 2) FROM fo.src GROUP BY 1/(k - 2))", rows(&["t"])),
            (
                "SELECT EXISTS (SELECT DISTINCT ON (1/(k - 1)) k FROM fo.src ORDER BY 1/(k - 1))",
                rows(&["t"]),
            ),
            (
                "SELECT k FROM fo.src s WHERE EXISTS (SELECT 1/0 FROM fo.src n WHERE n.k = s.k) ORDER BY 1",
                rows(&["1", "2", "3", "4"]),
            ),
            (
                "SELECT k FROM fo.src s WHERE NOT EXISTS (SELECT 1/0 FROM fo.src n WHERE n.k = s.k + 3) ORDER BY 1",
                rows(&["2", "3", "4"]),
            ),
            (
                "SELECT k, EXISTS (SELECT 1/0 FROM fo.src n WHERE n.k > s.k) FROM fo.src s ORDER BY 1",
                rows(&["1|t", "2|t", "3|t", "4|f"]),
            ),
            (
                "SELECT k FROM fo.src s WHERE EXISTS (SELECT 1/(n.k - 2) FROM fo.src n WHERE n.k > s.k LIMIT 1) ORDER BY 1",
                rows(&["1", "2", "3"]),
            ),
            (
                "SELECT k FROM fo.src s WHERE NOT EXISTS (SELECT 1/(n.k - 1) FROM fo.src n WHERE n.v = s.k) ORDER BY 1",
                rows(&["1", "2", "3", "4"]),
            ),
            // Where something in it counts its rows, its select list is made.
            ("SELECT EXISTS (SELECT 1/(k - 1) FROM fo.src OFFSET 0)", zero()),
            ("SELECT EXISTS (SELECT max(1/(k - 1)) FROM fo.src)", zero()),
            // Its select list's names read as PostgreSQL's parser reads them.
            (
                "SELECT EXISTS (SELECT nosuch)",
                Err((Some("42703"), "column \"nosuch\" does not exist".into())),
            ),
        ],
    );
}

#[test]
fn a_level_its_one_time_conditions_shut_reads_no_row() {
    let mut w = snail("shut");
    let none = || rows(&[]);
    each(
        &mut w,
        &[
            // A WHERE that folds FALSE or NULL: nothing below it runs.
            ("SELECT 1 FROM (SELECT 1/0 AS x FROM fo.src) q WHERE false", none()),
            ("SELECT x FROM (SELECT 1/0 AS x FROM fo.src LIMIT 1) q WHERE false", none()),
            ("WITH c AS (SELECT 1/0 AS x) SELECT k FROM fo.src WHERE false", none()),
            ("WITH c AS (SELECT 1/0 AS x FROM fo.src) SELECT 1 FROM c WHERE false", none()),
            (
                "WITH c AS (SELECT 1/0 AS x FROM fo.src) SELECT k FROM fo.src WHERE false AND EXISTS (SELECT x FROM c)",
                none(),
            ),
            (
                "SELECT 1 FROM (SELECT 1/0 AS x FROM fo.src WHERE k > 1 UNION ALL SELECT 1 FROM fo.src) u WHERE false",
                none(),
            ),
            ("SELECT 1 FROM (SELECT 1/0 AS x FROM fo.src) q, fo.src s WHERE false", none()),
            ("SELECT count(*) FROM (SELECT 1/0 AS x FROM fo.src) q WHERE false", rows(&["0"])),
            ("SELECT k FROM fo.src WHERE k/(k - 1) > 0 AND false", none()),
            ("SELECT k FROM fo.src WHERE NULL", none()),
            ("SELECT x FROM (SELECT 1/(k - 1) AS x FROM fo.src WHERE false) q", none()),
            ("SELECT * FROM (SELECT 1/(k - 1) AS x FROM fo.src) q JOIN fo.src s ON false", none()),
            ("SELECT * FROM fo.src s, (SELECT 1/(k - 1) AS x FROM fo.src) q WHERE false", none()),
            ("SELECT x FROM (SELECT 1/(k - 1) AS x FROM fo.src) q WHERE x > 0 AND false", none()),
            ("WITH c AS (SELECT 1/(k - 1) AS x FROM fo.src) SELECT x FROM c WHERE false", none()),
            ("SELECT k, (SELECT 1/(v - 10) FROM fo.src WHERE k = 1) FROM fo.src WHERE false", none()),
            ("SELECT x, row_number() OVER () FROM (SELECT 1/(k - 1) AS x FROM fo.src) q WHERE false", none()),
            ("SELECT k FROM fo.src WHERE false ORDER BY 1/(k - 1)", none()),
            ("SELECT k FROM fo.src WHERE false GROUP BY k", none()),
            ("SELECT generate_series(1, 2) FROM fo.src WHERE false", none()),
            ("SELECT 1 WHERE false", none()),
            ("SELECT count(*) WHERE false", rows(&["0"])),
            ("SELECT sum(1/(k - 1)) FROM fo.src WHERE false", rows(&["NULL"])),
            ("SELECT count(*) FROM (SELECT 1/(k - 1) FROM fo.src WHERE false) q", rows(&["0"])),
            ("SELECT (SELECT count(*) FROM fo.src WHERE false)", rows(&["0"])),
            (
                "SELECT k, (SELECT 1/(s.k - 1) FROM fo.src s WHERE s.k = fo.src.k AND false) FROM fo.src ORDER BY 1",
                rows(&["1|NULL", "2|NULL", "3|NULL", "4|NULL"]),
            ),
            ("SELECT max(k) FROM fo.src HAVING false", none()),
            // A one-time condition that does not hold (W27), before any row or after another.
            ("SELECT k FROM fo.src WHERE k/0 = 1 AND now() < '2000-01-01'::text::timestamptz", none()),
            ("SELECT k FROM fo.src WHERE now() < '2000-01-01'::text::timestamptz AND k/0 = 1", none()),
            ("SELECT count(*) FROM fo.src WHERE now() < '2000-01-01'::timestamptz", rows(&["0"])),
            (
                "SELECT x FROM (SELECT 1/(k - 1) AS x FROM fo.src) q WHERE now() < '2000-01-01'::timestamptz",
                none(),
            ),
            (
                "SELECT x FROM (SELECT 1/(k - 1) AS x FROM fo.src LIMIT 2) q WHERE now() < '2000-01-01'::timestamptz",
                none(),
            ),
            ("SELECT 1/(k - 1) FROM fo.src WHERE k > 0 AND now() < '2000-01-01'::timestamptz", none()),
            // Where rows are made all the same, what makes them is read.
            ("SELECT count(*), (SELECT max(k) FROM fo.src) FROM fo.src WHERE false", rows(&["0|4"])),
            ("SELECT 1/(k - 1) FROM fo.src WHERE false UNION ALL SELECT 2", rows(&["2"])),
            (
                "SELECT 1/(k - 1) FROM fo.src WHERE now() < '2000-01-01'::timestamptz UNION ALL SELECT 1/(k - 1) FROM fo.src WHERE k > 1",
                rows(&["1", "0", "0"]),
            ),
            ("SELECT * FROM (SELECT 1/(k - 1) AS x FROM fo.src) q LEFT JOIN fo.src s ON false", zero()),
            ("SELECT x FROM (SELECT 1/0 AS x FROM fo.src LIMIT 1) q, fo.src s WHERE false", zero()),
            ("SELECT k FROM fo.src WHERE k = 1 OR now() < '2000-01-01'::timestamptz", rows(&["1"])),
            ("SELECT k FROM fo.src WHERE now() > '2000-01-01'::timestamptz ORDER BY k", rows(&["1", "2", "3", "4"])),
            ("SELECT k FROM fo.src WHERE (SELECT k FROM fo.src WHERE false) IS NULL ORDER BY 1", rows(&["1", "2", "3", "4"])),
        ],
    );
}

#[test]
fn a_one_time_condition_holding_a_subquery_is_read_before_any_row() {
    let mut w = snail("gated");
    let zone = || {
        Err((
            Some("22007"),
            "invalid input syntax for type timestamp with time zone: \"x\"".into(),
        ))
    };
    each(
        &mut w,
        &[
            ("SELECT k FROM fo.none WHERE (SELECT true) AND now() < 'x'::text::timestamptz", zone()),
            ("SELECT k FROM fo.src WHERE (SELECT true) AND now() < 'x'::text::timestamptz", zone()),
            ("SELECT k FROM fo.src WHERE (SELECT false) AND now() < 'x'::text::timestamptz", rows(&[])),
            ("SELECT k FROM fo.src WHERE (SELECT k > 9 FROM fo.src LIMIT 1) AND k/0 = 1", rows(&[])),
            ("SELECT k FROM fo.src WHERE EXISTS (SELECT 1 FROM fo.none) AND k/0 = 1", rows(&[])),
            ("SELECT k FROM fo.src WHERE (SELECT NULL::boolean) AND k/0 = 1", rows(&[])),
            ("SELECT k FROM fo.src WHERE (SELECT min(v) FROM fo.src) > 5 AND k/(k - 1) > 0", zero()),
            ("SELECT k FROM fo.src WHERE (SELECT min(v) FROM fo.src) > 50 AND k/(k - 1) > 0", rows(&[])),
            (
                "SELECT k FROM fo.none WHERE (SELECT k FROM fo.src) > 0 AND now() < 'x'::text::timestamptz",
                Err((
                    Some("21000"),
                    "more than one row returned by a subquery used as an expression".into(),
                )),
            ),
            ("SELECT count(*) FROM fo.src WHERE (SELECT false) AND k/0 = 1", rows(&["0"])),
            ("SELECT k FROM fo.src WHERE now() < 'x'::text::timestamptz AND (SELECT 1/0 > 0)", zero()),
            ("SELECT k FROM fo.src WHERE (SELECT true) AND k > 2 ORDER BY 1", rows(&["3", "4"])),
        ],
    );
}

#[test]
fn a_with_query_nothing_reads_is_never_made() {
    let mut w = snail("withs");
    let all = || rows(&["1", "2", "3", "4"]);
    each(
        &mut w,
        &[
            ("WITH c AS MATERIALIZED (SELECT 1/0 AS x FROM fo.src) SELECT k FROM fo.src ORDER BY 1", all()),
            ("WITH c AS (SELECT 1/0 AS x FROM fo.src) SELECT k FROM fo.src ORDER BY 1", all()),
            (
                "WITH c AS (SELECT 1/0 AS x FROM fo.src), d AS (SELECT x FROM c) SELECT k FROM fo.src ORDER BY 1",
                all(),
            ),
            ("WITH c AS (SELECT 1/(k - 1) AS x FROM fo.src) SELECT k FROM fo.src ORDER BY 1", all()),
            (
                "WITH c AS MATERIALIZED (SELECT 1/(k - 1) AS x FROM fo.src) SELECT k FROM fo.src ORDER BY 1",
                all(),
            ),
            (
                "WITH c AS (SELECT 1/(k - 1) AS x FROM fo.src), d AS (SELECT x FROM c) SELECT k FROM fo.src ORDER BY 1",
                all(),
            ),
            (
                "WITH c AS (SELECT 1/(k - 1) AS x FROM fo.src) SELECT s.k FROM fo.src s JOIN fo.src t ON t.k = s.k ORDER BY 1",
                all(),
            ),
            (
                "WITH c AS (SELECT 1/(k - 1) AS x FROM fo.src) SELECT k FROM fo.src WHERE k IN (SELECT k FROM fo.src) ORDER BY 1",
                all(),
            ),
            ("WITH c AS MATERIALIZED (SELECT 1/0 AS x) SELECT k FROM fo.src WHERE false", rows(&[])),
            (
                "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 3), c AS (SELECT 1/(k - 1) FROM fo.src) SELECT n FROM t ORDER BY 1",
                rows(&["1", "2", "3"]),
            ),
            ("WITH c AS (SELECT k, 1/(k - 1) AS x FROM fo.src) SELECT count(*) FROM fo.src", rows(&["4"])),
            (
                "WITH c AS (SELECT k FROM fo.src UNION ALL SELECT 1/(k - 1) FROM fo.src) SELECT k FROM fo.src ORDER BY 1",
                all(),
            ),
            // One read, or read by one read, is made; one nothing reads is read as PostgreSQL's
            // parser reads it.
            (
                "WITH c AS (SELECT 1/(k - 1) AS x FROM fo.src) SELECT k FROM fo.src WHERE k IN (SELECT x FROM c) ORDER BY 1",
                zero(),
            ),
            ("WITH c AS (SELECT 1/(k - 1) AS x FROM fo.src), d AS (SELECT x FROM c) SELECT x FROM d", zero()),
            (
                "WITH c AS (SELECT nosuch FROM fo.src) SELECT k FROM fo.src",
                Err((Some("42703"), "column \"nosuch\" does not exist".into())),
            ),
        ],
    );
}

#[test]
fn an_output_nothing_reads_is_never_made() {
    let mut w = snail("outputs");
    let all = || rows(&["1", "2", "3", "4"]);
    each(
        &mut w,
        &[
            ("SELECT q.k FROM (SELECT k, 1/0 AS x FROM fo.src) q ORDER BY 1", all()),
            ("SELECT 1 FROM (SELECT k, 1/0 AS x FROM fo.src LIMIT 2) q", rows(&["1", "1"])),
            ("WITH c AS (SELECT k, 1/0 AS x FROM fo.src) SELECT k FROM c ORDER BY 1", all()),
            ("SELECT count(*) FROM (SELECT k, 1/0 AS x FROM fo.src) q", rows(&["4"])),
            (
                "SELECT s.k FROM fo.src s JOIN (SELECT k, 1/0 AS x FROM fo.src) q ON q.k = s.k ORDER BY 1",
                all(),
            ),
            ("SELECT q.k FROM (SELECT k, 1/(k - 1) AS x FROM fo.src) q ORDER BY 1", all()),
            (
                "SELECT q.k FROM (SELECT k, 1/(k - 1) AS x FROM fo.src UNION ALL SELECT k, 1 FROM fo.src) q ORDER BY 1",
                rows(&["1", "1", "2", "2", "3", "3", "4", "4"]),
            ),
            ("WITH c AS (SELECT k, 1/(k - 1) AS x FROM fo.src) SELECT k FROM c ORDER BY 1", all()),
            (
                "SELECT q.k FROM (SELECT DISTINCT ON (k) k, 1/(k - 1) AS x FROM fo.src ORDER BY k) q ORDER BY 1",
                all(),
            ),
            ("SELECT q.k FROM (SELECT k, 1/(k - 1) AS x FROM fo.src LIMIT 3) q ORDER BY 1", rows(&["1", "2", "3"])),
            ("SELECT count(*) FROM (SELECT k, 1/(k - 1) AS x FROM fo.src) q", rows(&["4"])),
            ("SELECT k FROM (SELECT k, 1/(k - 1) AS x FROM fo.src) q WHERE k > 2 ORDER BY 1", rows(&["3", "4"])),
            ("SELECT s.k FROM fo.src s JOIN (SELECT k, 1/(k - 1) AS x FROM fo.src) q USING (k) ORDER BY 1", all()),
            ("SELECT a FROM (SELECT k, 1/(k - 1) AS x FROM fo.src) q(a, b) ORDER BY 1", all()),
            ("SELECT k FROM (SELECT k, (SELECT 1/(k - 1)) AS x FROM fo.src) q ORDER BY 1", all()),
            ("SELECT k FROM (SELECT k, x FROM (SELECT k, 1/(k - 1) AS x FROM fo.src) a) b ORDER BY 1", all()),
            ("SELECT (SELECT max(q.k) FROM (SELECT k, 1/(k - 1) AS x FROM fo.src) q)", rows(&["4"])),
            (
                "SELECT k FROM (SELECT k, 1/(k - 1) AS x, row_number() OVER () AS r FROM fo.src) q ORDER BY 1",
                all(),
            ),
            ("SELECT k FROM (SELECT k, 1/(k - 1) FROM fo.src) q ORDER BY 1", all()),
            ("SELECT k FROM (SELECT k, 1/(k - 1) AS x FROM fo.src OFFSET 1) q ORDER BY 1", rows(&["2", "3", "4"])),
            (
                "SELECT k, (SELECT count(*) FROM (SELECT n.k, 1/(n.k - 1) AS x FROM fo.src n WHERE n.k = s.k) q) FROM fo.src s ORDER BY 1",
                rows(&["1|1", "2|1", "3|1", "4|1"]),
            ),
            ("SELECT x.k FROM (SELECT k, 1/(k - 1) AS x FROM fo.src) x ORDER BY 1", all()),
            // Where it is read, or its query or its readers keep it, it is made.
            ("SELECT q.k FROM (SELECT k, 1/(k - 1) AS x FROM fo.src UNION SELECT k, 1 FROM fo.src) q ORDER BY 1", zero()),
            ("WITH c AS MATERIALIZED (SELECT k, 1/(k - 1) AS x FROM fo.src) SELECT k FROM c ORDER BY 1", zero()),
            (
                "WITH c AS (SELECT k, 1/(k - 1) AS x FROM fo.src) SELECT a.k FROM c a, c b WHERE a.k = b.k ORDER BY 1",
                zero(),
            ),
            ("SELECT q.k FROM (SELECT DISTINCT k, 1/(k - 1) AS x FROM fo.src) q ORDER BY 1", zero()),
            ("SELECT q.k FROM (SELECT k, 1/(k - 1) AS x FROM fo.src ORDER BY 2) q ORDER BY 1", zero()),
            ("SELECT q.k FROM (SELECT k, 1/(k - 1) + random() AS x FROM fo.src) q ORDER BY 1", zero()),
            ("SELECT q FROM (SELECT k, 1/(k - 1) AS x FROM fo.src) q", zero()),
            ("SELECT q.* FROM (SELECT k, 1/(k - 1) AS x FROM fo.src) q", zero()),
            ("SELECT * FROM (SELECT k, 1/(k - 1) AS x FROM fo.src) q", zero()),
            ("SELECT k FROM (SELECT k, 1/(k - 1) AS x FROM fo.src) q WHERE x > 0 ORDER BY 1", zero()),
            ("SELECT k FROM (SELECT k, generate_series(1, 1/(k - 1)) AS x FROM fo.src) q ORDER BY 1", zero()),
            ("SELECT k FROM (SELECT k, 1/(k - 1) AS x FROM fo.src GROUP BY k, 1/(k - 1)) q ORDER BY 1", zero()),
        ],
    );
}

#[test]
fn a_select_list_is_made_only_for_the_rows_its_limit_keeps() {
    let mut w = snail("limit");
    each(
        &mut w,
        &[
            ("SELECT 1/(k - 2) FROM fo.src LIMIT 1", rows(&["-1"])),
            ("SELECT (SELECT 1/(k - 2) FROM fo.src LIMIT 1)", rows(&["-1"])),
            ("SELECT k, 1/(k - 2) FROM fo.src ORDER BY k LIMIT 1", rows(&["1|-1"])),
            ("SELECT fo.flim()", rows(&["-1"])),
            ("SELECT 1/(k - 2) FROM fo.src LIMIT 0", rows(&[])),
            ("SELECT EXISTS (SELECT 1/(k - 1) FROM fo.src LIMIT 0)", rows(&["f"])),
            ("SELECT 1/(k - 3) FROM fo.src LIMIT 2", rows(&["0", "-1"])),
            ("SELECT 1/(k - 3) FROM fo.src WHERE k > 0 LIMIT 2", rows(&["0", "-1"])),
            ("SELECT 1/(k - 2), generate_series(1, 2) FROM fo.src LIMIT 2", rows(&["-1|1", "-1|2"])),
            ("SELECT 1/(k - 2) FROM fo.src FETCH FIRST 1 ROW ONLY", rows(&["-1"])),
            (
                "SELECT (SELECT 1/(n.k - 2) FROM fo.src n WHERE n.k >= s.k LIMIT 1) FROM fo.src s WHERE s.k = 1",
                rows(&["-1"]),
            ),
            ("SELECT 1/(k - 2) FROM fo.src WHERE k < 5 LIMIT 1", rows(&["-1"])),
            ("SELECT k FROM fo.src WHERE 1/(k - 3) > -5 LIMIT 1", rows(&["1"])),
            ("SELECT k FROM fo.src WHERE 1/(k - 3) > -5 ORDER BY k LIMIT 1", rows(&["1"])),
            ("SELECT k, 1/(k - 2) FROM fo.src WHERE k > 0 ORDER BY k LIMIT 1", rows(&["1|-1"])),
            // The rows its OFFSET passes over are made, and what the LIMIT reaches.
            ("SELECT 1/(k - 2) FROM fo.src OFFSET 2 LIMIT 1", zero()),
            ("SELECT 1/(k - 3) FROM fo.src LIMIT 3", zero()),
            ("SELECT DISTINCT 1/(k - 2) FROM fo.src LIMIT 1", zero()),
            ("SELECT k FROM fo.src WHERE 1/(k - 1) > -5 LIMIT 1", zero()),
        ],
    );
}

#[test]
fn an_unread_output_s_aggregate_folds_no_row_and_its_query_groups_as_before() {
    let mut w = snail("aggregate");
    let all = || rows(&["1", "2", "3", "4"]);
    each(
        &mut w,
        &[
            ("SELECT q.k FROM (SELECT k, sum(1/(k - 1)) AS x FROM fo.src GROUP BY k) q ORDER BY 1", all()),
            (
                "SELECT q.g FROM (SELECT k % 2 AS g, max(1/(k - 1)) AS x FROM fo.src GROUP BY k % 2) q ORDER BY 1",
                rows(&["0", "1"]),
            ),
            ("SELECT q.k FROM (SELECT 1 AS k, sum(1/(k - 1)) AS x FROM fo.src) q", rows(&["1"])),
            ("SELECT q.k FROM (SELECT 1 AS k, sum(1/(k - 1)) AS x FROM fo.none) q", rows(&["1"])),
            ("SELECT count(*) FROM (SELECT k, sum(1/(k - 1)) AS x FROM fo.src GROUP BY k) q", rows(&["4"])),
            (
                "SELECT q.k FROM (SELECT k, count(*) FILTER (WHERE 1/(k - 1) > 0) AS x FROM fo.src GROUP BY k) q ORDER BY 1",
                all(),
            ),
            (
                "SELECT q.k FROM (SELECT k, array_agg(1/(k - 1) ORDER BY v) AS x FROM fo.src GROUP BY k) q ORDER BY 1",
                all(),
            ),
            ("WITH c AS (SELECT k, sum(1/(k - 1)) AS x FROM fo.src GROUP BY k) SELECT k FROM c ORDER BY 1", all()),
            (
                "SELECT q.k FROM (SELECT k, sum(1/(k - 1)) AS x, count(*) AS n FROM fo.src GROUP BY k) q WHERE n > 0 ORDER BY 1",
                all(),
            ),
            ("SELECT q.n FROM (SELECT sum(1/(k - 1)) AS x, count(*) AS n FROM fo.src) q", rows(&["4"])),
            ("SELECT q.k FROM (SELECT k, sum(1/(k - 1)) OVER () AS x FROM fo.src) q ORDER BY 1", all()),
            ("SELECT q.k FROM (SELECT k, sum(1/(k - 1)) AS x FROM fo.src GROUP BY k) q(k, y) ORDER BY 1", all()),
            ("SELECT q.k FROM (SELECT k, sum(1/(k - 1)) + k AS x FROM fo.src GROUP BY k) q ORDER BY 1", all()),
            ("SELECT q.k FROM (SELECT k, count(DISTINCT 1/(k - 1)) AS x FROM fo.src GROUP BY k) q ORDER BY 1", all()),
            (
                "SELECT q.k FROM (SELECT k, sum(1/(k - 1)) AS x FROM fo.src GROUP BY k UNION ALL SELECT k, 1::bigint FROM fo.src) q ORDER BY 1",
                rows(&["1", "1", "2", "2", "3", "3", "4", "4"]),
            ),
            // Where it is read, its query keeps it, or its arms are of other types than their
            // union's, it is made.
            (
                "SELECT q.k FROM (SELECT k, sum(1/(k - 1)) AS x FROM fo.src GROUP BY k HAVING sum(1/(k - 1)) > 0) q",
                zero(),
            ),
            ("SELECT q.k FROM (SELECT k, sum(1/(k - 1)) AS x FROM fo.src GROUP BY k ORDER BY x) q", zero()),
            ("SELECT q.k FROM (SELECT k, sum(1/(k - 1)) + random() AS x FROM fo.src GROUP BY k) q ORDER BY 1", zero()),
            ("SELECT q.k FROM (SELECT k, sum(1/(k - 1)) AS x FROM fo.src GROUP BY k) q WHERE x IS NULL ORDER BY 1", zero()),
            (
                "SELECT q.k FROM (SELECT k, sum(1/(k - 1)) AS x FROM fo.src GROUP BY k UNION ALL SELECT k, 1 FROM fo.src) q ORDER BY 1",
                zero(),
            ),
            (
                "SELECT q.k FROM (SELECT k, 1/(k - 1) AS x FROM fo.src UNION ALL SELECT k, 1::bigint FROM fo.src) q ORDER BY 1",
                zero(),
            ),
            (
                "SELECT q.k FROM (SELECT k, 1/(k - 1) AS x FROM fo.src UNION ALL SELECT k, 1::smallint FROM fo.src) q ORDER BY 1",
                zero(),
            ),
            (
                "SELECT q.k FROM (SELECT k, sum(1/(k - 1)) OVER (PARTITION BY 1/(k - 1)) AS x FROM fo.src) q ORDER BY 1",
                zero(),
            ),
            (
                "SELECT q.k FROM (SELECT k, row_number() OVER (ORDER BY 1/(k - 1)) AS x FROM fo.src) q ORDER BY 1",
                zero(),
            ),
        ],
    );
}

#[test]
fn a_natural_join_reads_only_the_columns_its_sides_share() {
    let mut w = snail("natural");
    let all = || rows(&["1", "2", "3", "4"]);
    each(
        &mut w,
        &[
            ("SELECT s.k FROM fo.src s NATURAL JOIN (SELECT k, 1/(k - 1) AS x FROM fo.src) q ORDER BY 1", all()),
            ("SELECT k FROM (SELECT k, 1/(k - 1) AS x FROM fo.src) q NATURAL JOIN fo.src ORDER BY 1", all()),
            ("SELECT s.k FROM fo.src s NATURAL LEFT JOIN (SELECT k, 1/(k - 1) AS x FROM fo.src) q ORDER BY 1", all()),
            ("SELECT s.k FROM fo.src s NATURAL FULL JOIN (SELECT k, 1/(k - 1) AS x FROM fo.src) q ORDER BY 1", all()),
            (
                "WITH c AS (SELECT k, 1/(k - 1) AS x FROM fo.src) SELECT s.k FROM fo.src s NATURAL JOIN c ORDER BY 1",
                all(),
            ),
            (
                "SELECT a.k FROM (SELECT k, 1/(k - 1) AS x FROM fo.src) a NATURAL JOIN (SELECT k, 2 AS y FROM fo.src) b ORDER BY 1",
                all(),
            ),
            (
                "SELECT s.k FROM fo.src s NATURAL JOIN fo.src t, (SELECT k AS z, 1/(k - 1) AS x FROM fo.src) q WHERE s.k = q.z ORDER BY 1",
                all(),
            ),
            // A name both sides share is read, and a star reads every column.
            ("SELECT s.k FROM fo.src s NATURAL JOIN (SELECT k, 1/(k - 1) AS v FROM fo.src) q ORDER BY 1", zero()),
            ("SELECT * FROM fo.src s NATURAL JOIN (SELECT k, 1/(k - 1) AS x FROM fo.src) q", zero()),
            (
                "SELECT a.k FROM (SELECT k, 1/(k - 1) AS x FROM fo.src) a NATURAL JOIN (SELECT k, 2 AS x FROM fo.src) b ORDER BY 1",
                zero(),
            ),
        ],
    );
}

#[test]
fn a_limit_over_a_subquery_read_alone_is_the_subquery_s() {
    let mut w = snail("pushed");
    each(
        &mut w,
        &[
            ("SELECT x FROM (SELECT 1/(k - 2) AS x FROM fo.src) q LIMIT 1", rows(&["-1"])),
            ("SELECT x FROM (SELECT k, 1/(k - 2) AS x FROM fo.src) q WHERE k > 0 LIMIT 1", rows(&["-1"])),
            ("SELECT q.x FROM (SELECT 1/(k - 2) AS x FROM fo.src WHERE k > 0) q LIMIT 1", rows(&["-1"])),
            ("SELECT x FROM (SELECT k, 1/(k - 2) AS x FROM fo.src) q ORDER BY k LIMIT 1", rows(&["-1"])),
            ("SELECT x, k FROM (SELECT k, 1/(k - 2) AS x FROM fo.src) q LIMIT 1", rows(&["-1|1"])),
            ("SELECT x + 1 AS y FROM (SELECT 1/(k - 2) AS x FROM fo.src) q LIMIT 1", rows(&["0"])),
            ("SELECT x FROM (SELECT 1/(k - 2) AS x FROM fo.src ORDER BY k) q LIMIT 1", rows(&["-1"])),
            ("SELECT x FROM (SELECT 1/(k - 2) AS x, v FROM fo.src) q WHERE v > 0 LIMIT 1", rows(&["-1"])),
            ("SELECT x FROM (SELECT 1/(k - 2) AS x FROM fo.src) q FETCH FIRST 1 ROW ONLY", rows(&["-1"])),
            // The rows its OFFSET passes over are made.
            ("SELECT x FROM (SELECT 1/(k - 2) AS x FROM fo.src) q LIMIT 1 OFFSET 2", zero()),
            // A WHERE over such a subquery, or a WITH query read once, is its own.
            ("SELECT x FROM (SELECT k, 1/(k - 2) AS x FROM fo.src) q WHERE k <> 2 ORDER BY 1", rows(&["-1", "0", "1"])),
            (
                "WITH c AS (SELECT k, 1/(k - 2) AS x FROM fo.src) SELECT x FROM c WHERE k <> 2 ORDER BY 1",
                rows(&["-1", "0", "1"]),
            ),
            ("SELECT count(*) FROM (SELECT k, 1/(k - 2) AS x FROM fo.src) q WHERE k <> 2", rows(&["3"])),
            ("SELECT x FROM (SELECT k, 1/(k - 2) AS x FROM fo.src ORDER BY k) q WHERE k <> 2", rows(&["-1", "1", "0"])),
            ("SELECT x FROM (SELECT k, 1/(k - 2) AS x FROM fo.src) q WHERE x > 0 ORDER BY 1", zero()),
            (
                "WITH c AS MATERIALIZED (SELECT k, 1/(k - 2) AS x FROM fo.src) SELECT x FROM c WHERE k <> 2 ORDER BY 1",
                zero(),
            ),
        ],
    );
}

#[test]
fn an_order_read_backward_under_a_limit_makes_only_the_rows_it_reaches() {
    let mut w = snail("backward");
    each(
        &mut w,
        &[
            ("SELECT k, 1/(k - 2) FROM fo.src ORDER BY k DESC LIMIT 1", rows(&["4|0"])),
            ("SELECT x FROM (SELECT k, 1/(k - 2) AS x FROM fo.src) q ORDER BY k DESC LIMIT 1", rows(&["0"])),
            ("SELECT 1/(k - 2) FROM fo.src ORDER BY k DESC LIMIT 2", rows(&["0", "1"])),
            ("SELECT k FROM fo.src WHERE 1/(k - 2) >= 0 ORDER BY k DESC LIMIT 1", rows(&["4"])),
            ("SELECT k, 1/(k - 2) FROM fo.src WHERE k < 4 ORDER BY k DESC LIMIT 1", rows(&["3|1"])),
            ("SELECT k, 1/(k - 3) FROM fo.src ORDER BY k DESC FETCH FIRST 1 ROW ONLY", rows(&["4|1"])),
            // No order of the table gives it, or the OFFSET passes the row over: made.
            ("SELECT k, 1/(k - 2) FROM fo.src ORDER BY k DESC NULLS LAST LIMIT 1", zero()),
            ("SELECT k, 1/(k - 2) FROM fo.src ORDER BY k DESC OFFSET 2 LIMIT 1", zero()),
            // A window read whole makes its select list only for the rows its LIMIT reaches.
            ("SELECT k, 1/(k - 2), row_number() OVER () FROM fo.src LIMIT 1", rows(&["1|-1|1"])),
            ("SELECT k, 1/(k - 2), sum(k) OVER () FROM fo.src LIMIT 1", rows(&["1|-1|10"])),
            // The snail's own: a window that orders its rows, beside a backward sort, or over a
            // WHERE read row by row, makes every row (18.6 answers `4|0|1`, `4|0|1` and `1|1`).
            ("SELECT k, 1/(k - 2), rank() OVER (ORDER BY k DESC) FROM fo.src LIMIT 1", zero()),
            ("SELECT k, 1/(k - 2), row_number() OVER () FROM fo.src ORDER BY k DESC LIMIT 1", zero()),
            ("SELECT k, row_number() OVER () FROM fo.src WHERE 1/(k - 2) > -5 LIMIT 1", zero()),
        ],
    );
}
