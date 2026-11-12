//! A statement's constants folded before any row is read, as PostgreSQL 18.6's planner folds them:
//! a constant's failure the statement's over rows and over none, in a select list, an aggregate,
//! a FILTER, a set-returning function's arguments, an ARRAY, a ROW, ORDER BY, GROUP BY, OFFSET,
//! LIMIT, a window's ORDER BY, a join's ON, HAVING, an IN list, beside a volatile or a stable call,
//! and in an INSERT … SELECT, an UPDATE's SET, its RETURNING and a DELETE's WHERE; the fold
//! stopping where AND, OR, CASE and COALESCE stop it; the clauses read in the planner's order; only
//! immutable calls folded; and a prepared statement folded when it is executed. Every answer and
//! refusal was given by 18.6 over the same statements, in the same order.
#![cfg(feature = "standalone")]

mod weather;

use warren_snail::catalog::Catalog;
use warren_snail::read::Snail;
use weather::{tmp, write};

const DDL: &str = "CREATE SCHEMA fo;
     CREATE TABLE fo.src (k integer NOT NULL, v integer, PRIMARY KEY (k));
     CREATE TABLE fo.none (k integer NOT NULL, v integer, PRIMARY KEY (k));";

/// A snail holding four rows in `fo.src` and none in `fo.none`, at a path of the test's name.
fn snail(name: &str) -> Snail {
    let path = tmp(&format!("folded-{name}.snail"));
    write(&path, &Catalog::parse(DDL).unwrap(), &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute("INSERT INTO fo.src VALUES (1, 10), (2, 20), (3, 30), (4, 40)")
        .unwrap();
    w
}

/// A statement's answer, each row's values as PostgreSQL's output function writes them, `|`
/// between them; or its refusal, its SQLSTATE and its words.
fn outcome(w: &mut Snail, sql: &str) -> Result<Vec<String>, (Option<&'static str>, String)> {
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

fn zero() -> Result<Vec<String>, (Option<&'static str>, String)> {
    Err((Some("22012"), "division by zero".into()))
}

fn syntax(ty: &str) -> Result<Vec<String>, (Option<&'static str>, String)> {
    Err((
        Some("22P02"),
        format!("invalid input syntax for type {ty}: \"x\""),
    ))
}

fn rows(r: &[&str]) -> Result<Vec<String>, (Option<&'static str>, String)> {
    Ok(r.iter().map(|s| s.to_string()).collect())
}

/// Each statement answered or refused as 18.6 answers or refuses it, in its order; every one that
/// is not, named.
fn each(w: &mut Snail, asked: &[(&str, Result<Vec<String>, (Option<&'static str>, String)>)]) {
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
fn a_constant_is_folded_before_any_row() {
    let mut w = snail("before");
    each(
        &mut w,
        &[
            ("SELECT 1/0 FROM fo.src WHERE false", zero()),
            ("SELECT 1/0 FROM fo.none", zero()),
            ("SELECT sum(1/0) FROM fo.none", zero()),
            (
                "SELECT count(*) FILTER (WHERE 1/0 = 1) FROM fo.none",
                zero(),
            ),
            ("SELECT generate_series(1, 1/0) FROM fo.none", zero()),
            ("SELECT ARRAY[1, 1/0] FROM fo.none", zero()),
            ("SELECT (1, 1/0) FROM fo.none", zero()),
            ("SELECT 1/0 IS NULL FROM fo.none", zero()),
            ("SELECT nullif(1, 1/0) FROM fo.none", zero()),
            ("SELECT greatest(1, 1/0) FROM fo.none", zero()),
            (
                "SELECT length(NULL::text || (1/0)::text) FROM fo.none",
                zero(),
            ),
            ("SELECT k FROM fo.none ORDER BY 1/0", zero()),
            ("SELECT 1/0 AS x FROM fo.none GROUP BY x", zero()),
            ("SELECT k FROM fo.none OFFSET 1/0", zero()),
            ("SELECT k FROM fo.none LIMIT 1/0", zero()),
            (
                "SELECT row_number() OVER (ORDER BY 1/0) FROM fo.none",
                zero(),
            ),
            ("SELECT s.k FROM fo.src s JOIN fo.none n ON 1/0 = 1", zero()),
            ("SELECT count(*) FROM fo.none HAVING 1/0 = 1", zero()),
            ("SELECT k FROM fo.src WHERE k IN (1, 1/0) AND false", zero()),
            ("SELECT random() + 1/0 FROM fo.none", zero()),
            (
                "SELECT now() + (1/0) * interval '1 day' FROM fo.none",
                zero(),
            ),
            (
                "INSERT INTO fo.none SELECT 1/0, k FROM fo.src WHERE false",
                zero(),
            ),
            ("UPDATE fo.src SET v = 1/0 WHERE false", zero()),
            ("UPDATE fo.src SET v = 1 WHERE false RETURNING 1/0", zero()),
            ("DELETE FROM fo.src WHERE 1/0 = 1 AND false", zero()),
        ],
    );
}

#[test]
fn the_fold_stops_where_postgresql_s_stops() {
    let mut w = snail("stops");
    let each_one = rows(&["1", "1", "1", "1"]);
    each(
        &mut w,
        &[
            ("SELECT k FROM fo.src WHERE false AND 1/0 = 1", rows(&[])),
            ("SELECT k FROM fo.src WHERE true OR 1/0 = 1 ORDER BY k", rows(&["1", "2", "3", "4"])),
            ("SELECT k FROM fo.src WHERE k = 1 OR true OR 1/0 = 1 ORDER BY k", rows(&["1", "2", "3", "4"])),
            ("SELECT CASE WHEN false THEN 1/0 ELSE 1 END FROM fo.src ORDER BY k", each_one.clone()),
            ("SELECT CASE 1 WHEN 2 THEN 1/0 ELSE 0 END FROM fo.src ORDER BY k", rows(&["0", "0", "0", "0"])),
            (
                "SELECT CASE WHEN k > 0 THEN 1 WHEN true THEN 2 ELSE 1/0 END FROM fo.src ORDER BY k",
                each_one.clone(),
            ),
            ("SELECT coalesce(1, 1/0) FROM fo.src ORDER BY k", each_one),
            ("SELECT coalesce(k, 1, 1/0) FROM fo.src ORDER BY k", rows(&["1", "2", "3", "4"])),
            ("SELECT count(*) FROM fo.src HAVING false AND 1/0 = 1", rows(&[])),
            ("SELECT k FROM fo.src WHERE 1/0 = 1 AND false", zero()),
            ("SELECT k FROM fo.src WHERE k = 1 OR 1/0 = 1", zero()),
            ("SELECT CASE WHEN k > 100 THEN 1/0 END FROM fo.src", zero()),
            ("SELECT CASE WHEN k > 100 THEN 1/0 END FROM fo.none", zero()),
            ("SELECT CASE k WHEN 1 THEN 1 ELSE 1/0 END FROM fo.none", zero()),
        ],
    );
}

#[test]
fn the_fold_reads_the_statement_in_postgresql_s_order() {
    let mut w = snail("order");
    each(
        &mut w,
        &[
            ("SELECT 1/0, 'x'::text::integer FROM fo.none", zero()),
            ("SELECT 'x'::text::integer, 1/0 FROM fo.none", syntax("integer")),
            ("SELECT k FROM fo.none WHERE 'x'::text::integer = 1 ORDER BY 1/0", zero()),
            (
                "SELECT n.k FROM fo.src s JOIN fo.none n ON 'x'::text::integer = 1 WHERE 1/0 = 1",
                syntax("integer"),
            ),
            (
                "SELECT 1 FROM fo.src a JOIN fo.src b ON 1/0 = 1 JOIN fo.src c ON 'x'::text::integer = 1",
                zero(),
            ),
            (
                "SELECT 1 FROM fo.src a JOIN (fo.src b JOIN fo.src c ON 'x'::text::integer = 1) ON 1/0 = 1",
                syntax("integer"),
            ),
            ("SELECT count(*) FROM fo.none WHERE 1/0 = 1 HAVING 'x'::text::integer = 1", zero()),
            ("SELECT k FROM fo.none OFFSET 'x'::text::bigint LIMIT 1/0", syntax("bigint")),
            ("SELECT 1/0 AS x FROM fo.none GROUP BY x LIMIT 'x'::text::bigint", syntax("bigint")),
            ("SELECT 1/0 AS x FROM fo.none GROUP BY x HAVING 'x'::text::integer = 1", syntax("integer")),
            ("UPDATE fo.src SET v = 1/0 WHERE 'x'::text::integer = 1 RETURNING k", zero()),
            ("UPDATE fo.src SET v = 1 WHERE 1/0 = 1 RETURNING 'x'::text::integer", syntax("integer")),
        ],
    );
}

#[test]
fn only_immutable_calls_are_folded() {
    let mut w = snail("immutable");
    each(
        &mut w,
        &[
            ("SELECT 'x'::text::timestamptz FROM fo.none", rows(&[])),
            ("SELECT 'x'::text::timestamp FROM fo.none", rows(&[])),
            ("SELECT 'x'::text::date FROM fo.none", rows(&[])),
            (
                "SELECT now() < 'x'::text::timestamptz FROM fo.none",
                rows(&[]),
            ),
            ("SELECT 'x'::text::integer FROM fo.none", syntax("integer")),
            ("SELECT 'x'::text::numeric FROM fo.none", syntax("numeric")),
            ("SELECT 'x'::text::boolean FROM fo.none", syntax("boolean")),
            ("SELECT k FROM fo.none WHERE random() < 1/0", zero()),
            (
                "SELECT clock_timestamp() + (1/0) * interval '1 day' FROM fo.none",
                zero(),
            ),
        ],
    );
}

/// After setseed(0.5), 18.6's draws for each statement, then the draw `SELECT random()` makes after
/// it: a volatile call drawn for each row as the rows come, never once while the statement's
/// constants are folded, beside a constant folded or not, and nothing drawn where the fold refuses.
#[test]
fn a_volatile_call_is_drawn_for_each_row_never_while_folding() {
    let mut w = snail("random");
    let (s1, s5, s9) = (
        "0.9851677175347999",
        "0.6476186144084",
        "0.5145227426983392",
    );
    let four = [
        s1,
        "0.825301858027981",
        "0.12974610012450416",
        "0.16356291958601088",
    ];
    let asked = [
        ("SELECT random() FROM fo.src", rows(&four), s5),
        (
            "SELECT k, random() FROM fo.src WHERE random() < 2",
            rows(&[
                "1|0.825301858027981",
                "2|0.16356291958601088",
                "3|0.8822771983038762",
                "4|0.15619865764623442",
            ]),
            s9,
        ),
        (
            "SELECT random(1, 10) FROM fo.src",
            rows(&["3", "3", "3", "3"]),
            s9,
        ),
        (
            "SELECT k FROM fo.src WHERE random() < 2 AND 1 + 1 = 2",
            rows(&["1", "2", "3", "4"]),
            s5,
        ),
        (
            "SELECT random() + (1 + 1) FROM fo.src",
            rows(&[
                "2.9851677175348",
                "2.8253018580279807",
                "2.129746100124504",
                "2.1635629195860107",
            ]),
            s5,
        ),
        (
            "SELECT k, random(1, 2 + 8) FROM fo.src WHERE random(1, 10) > 0",
            rows(&["1|3", "2|3", "3|8", "4|9"]),
            "0.7495564594841835",
        ),
        (
            "SELECT k FROM fo.src WHERE random() < 0.5",
            rows(&["3", "4"]),
            s5,
        ),
        (
            "SELECT k FROM fo.none WHERE random() < 2 AND 1/0 = 1",
            zero(),
            s1,
        ),
        ("SELECT random() < 2, 1/0 FROM fo.src", zero(), s1),
    ];
    let mut wrong = Vec::new();
    for (sql, want, next) in asked {
        w.execute("SELECT setseed(0.5)").unwrap();
        let got = outcome(&mut w, sql);
        let after = outcome(&mut w, "SELECT random()");
        if got != want || after != rows(&[next]) {
            wrong.push(format!(
                "{sql}\n  got {got:?}, then {after:?}\n  want {want:?}, then {next}"
            ));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// A value relabelled to another type it is binary-coercible to keeps its text only where both
/// types write it alike: a reg type's name is not its OID's digits, for the catalog's objects and
/// the file's own; a varchar's text is its text.
#[test]
fn a_relabelled_constant_is_written_as_its_new_type_writes_it() {
    let path = tmp("folded-reg.snail");
    let ddl = "CREATE SCHEMA s; CREATE TABLE s.u (id integer PRIMARY KEY);
         CREATE TABLE t (id integer PRIMARY KEY); CREATE TYPE mood AS ENUM ('a', 'b');";
    write(&path, &Catalog::parse(ddl).unwrap(), &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    each(
        &mut w,
        &[
            ("SELECT 'plpgsql_validator'::regproc::oid > 12000", rows(&["t"])),
            (
                "SELECT 'pg_class'::regclass::oid, 'pg_class'::regclass::oid::text, \
                 'pg_class'::regclass::oid = 1259",
                rows(&["1259|1259|t"]),
            ),
            (
                "SELECT 'int4'::regtype::oid, 'int4'::regtype::oid::text, 23::oid::regtype, \
                 23::regtype::text",
                rows(&["23|23|integer|integer"]),
            ),
            (
                "SELECT 't'::regclass::oid::text = 't'::regclass::oid::text, \
                 's.u'::regclass::oid > 16000, 'mood'::regtype::oid > 16000",
                rows(&["t|t|t"]),
            ),
            (
                "SELECT 't'::regclass::text, 's'::regnamespace::oid > 16000, 'mood[]'::regtype::text",
                rows(&["t|t|mood[]"]),
            ),
            ("SELECT id FROM t WHERE false AND 't'::regclass::oid > 0", rows(&[])),
            (
                "SELECT 'abc'::varchar(5)::text || 'd', length('abc'::varchar(5)), \
                 'abc'::varchar(2)::text",
                rows(&["abcd|3|ab"]),
            ),
            ("SELECT length('ab'::varchar) + 1/0 FROM t WHERE false", zero()),
            ("SELECT 'ab'::varchar::text || (1/0)::text FROM t WHERE false", zero()),
        ],
    );
}

#[test]
fn a_prepared_statement_folds_at_execute() {
    let mut w = snail("prepared");
    each(
        &mut w,
        &[
            (
                "PREPARE f1p AS SELECT 1/0 FROM fo.src WHERE false",
                rows(&[]),
            ),
            ("EXECUTE f1p", zero()),
            (
                "PREPARE f1q(integer) AS SELECT 1/$1 FROM fo.src WHERE false",
                rows(&[]),
            ),
            ("EXECUTE f1q(1)", rows(&[])),
            ("EXECUTE f1q(0)", zero()),
            (
                "PREPARE f1u AS UPDATE fo.src SET v = 1/0 WHERE false",
                rows(&[]),
            ),
            ("EXECUTE f1u", zero()),
            ("DEALLOCATE ALL", rows(&[])),
            (
                "SELECT k, v FROM fo.src ORDER BY k",
                rows(&["1|10", "2|20", "3|30", "4|40"]),
            ),
        ],
    );
    // A question asked with its parameters handed, as the extended protocol binds them: folded
    // bound to their values.
    let handed = |w: &mut Snail, sql: &str, p: &str| {
        w.ask_with(sql, None, &[p.to_string()])
            .map(|(b, _)| b.num_rows())
    };
    let refused = handed(&mut w, "SELECT 1/$1::integer FROM fo.src WHERE false", "0").unwrap_err();
    assert_eq!(
        (refused.sqlstate(), refused.to_string()),
        (Some("22012"), "division by zero".into())
    );
    assert_eq!(
        handed(&mut w, "SELECT 1/$1::integer FROM fo.src WHERE false", "1").unwrap(),
        0
    );
    assert_eq!(
        handed(
            &mut w,
            "SELECT k FROM fo.src WHERE false AND 1/$1::integer = 1",
            "0"
        )
        .unwrap(),
        0
    );
}

#[test]
fn a_subquery_is_folded_as_postgresql_plans_it() {
    let mut w = snail("levels");
    let all = || rows(&["1", "2", "3", "4"]);
    each(
        &mut w,
        &[
            // A sublink after the constants of the clause it stands in, unless the fold dropped it.
            ("SELECT (SELECT 1/0) FROM fo.src WHERE false", zero()),
            ("SELECT (SELECT 1/0), 'x'::text::integer FROM fo.none", syntax("integer")),
            ("SELECT (SELECT 1/0) FROM fo.none WHERE 'x'::text::integer = 1", zero()),
            ("SELECT (SELECT 'x'::text::integer), (SELECT 1/0) FROM fo.none", syntax("integer")),
            ("SELECT (SELECT 1/0) FROM fo.none ORDER BY 'x'::text::integer", syntax("integer")),
            ("SELECT k FROM fo.none WHERE k = (SELECT 1/0) OR 'x'::text::integer = 1", syntax("integer")),
            ("SELECT k FROM fo.none WHERE k = 1 AND k = (SELECT 1/0) AND 'x'::text::integer = 1", syntax("integer")),
            ("SELECT k FROM fo.src WHERE k = (SELECT 1/0)", zero()),
            ("SELECT (SELECT 1/0 FROM fo.none) FROM fo.none", zero()),
            ("SELECT ARRAY(SELECT 1/0) FROM fo.none", zero()),
            ("SELECT k FROM fo.src WHERE false OR k IN (SELECT 1/0)", zero()),
            // An EXISTS's query folded with its select list thrown away where nothing in it counts
            // rows, its LIMIT folded first.
            ("SELECT k FROM fo.none WHERE EXISTS (SELECT 1 FROM fo.src WHERE 1/0 = 1)", zero()),
            ("SELECT EXISTS (SELECT 1 FROM fo.src LIMIT 1/0)", zero()),
            ("SELECT EXISTS (SELECT 1/0 FROM fo.src OFFSET 0)", zero()),
            ("SELECT EXISTS (SELECT max(1/0) FROM fo.src)", zero()),
            // IN, = ANY and EXISTS of the query around them joined to it before anything is folded.
            ("SELECT k FROM fo.src WHERE k IN (SELECT 1/0) AND false", zero()),
            ("SELECT k FROM fo.src WHERE false AND k IN (SELECT 1/0)", zero()),
            ("SELECT k FROM fo.src WHERE 'x'::text::integer = 1 AND k IN (SELECT 1/0)", syntax("integer")),
            ("SELECT k FROM fo.src WHERE 'x'::text::integer = 1 AND k IN (SELECT 1/0 LIMIT 1)", syntax("integer")),
            ("SELECT k FROM fo.src n WHERE n.k = ANY (SELECT 1/0) AND false", zero()),
            ("SELECT k FROM fo.src n WHERE k IN (SELECT 1/0) AND n.v IN (SELECT 'x'::text::integer)", zero()),
            ("SELECT k FROM fo.src n WHERE k IN (SELECT 'x'::text::integer LIMIT 1) AND n.v IN (SELECT 1/0)", zero()),
            ("SELECT k FROM fo.src n WHERE k IN (SELECT 1/0 FROM fo.src s WHERE 'x'::text::integer = s.k)", syntax("integer")),
            (
                "SELECT k FROM fo.src n WHERE k IN (SELECT s.k FROM fo.src s WHERE 'x'::text::integer = s.k AND s.v = 1/0)",
                syntax("integer"),
            ),
            (
                "SELECT 1 FROM fo.src s WHERE s.k IN (SELECT n.k FROM fo.src n WHERE n.v = s.v AND 1/0 = 1) AND false",
                zero(),
            ),
            (
                "SELECT k FROM fo.src n WHERE false AND EXISTS (SELECT 1 FROM fo.src s WHERE s.k = n.k AND 1/0 = 1)",
                zero(),
            ),
            (
                "SELECT k FROM fo.src n WHERE NOT EXISTS (SELECT 1 FROM fo.src s WHERE s.k = n.k AND 1/0 = 1) AND false",
                zero(),
            ),
            (
                "SELECT k FROM fo.src n WHERE EXISTS (SELECT 1 FROM fo.src s WHERE s.k = n.k AND 1/0 = 1) AND 'x'::text::integer = 1",
                syntax("integer"),
            ),
            (
                "SELECT 1 FROM fo.src s WHERE EXISTS (SELECT 1 FROM fo.src n WHERE n.k = s.k LIMIT 1) AND 'x'::text::integer = 1",
                syntax("integer"),
            ),
            (
                "SELECT 1 FROM fo.src s WHERE 'x'::text::integer = 1 AND EXISTS (SELECT 1 FROM fo.src n WHERE n.k = s.k LIMIT 1/0)",
                zero(),
            ),
            // A FROM subquery pulled up: its WHERE in its place, its outputs only where they are read.
            ("SELECT x FROM (SELECT 1/0 AS x FROM fo.src) q WHERE false", zero()),
            ("SELECT q.k FROM (SELECT k, 1/0 AS x FROM fo.none) q", rows(&[])),
            ("SELECT x FROM (SELECT 'x'::text::integer AS x FROM fo.src) q WHERE 1/0 = 1", syntax("integer")),
            ("SELECT 1 FROM (SELECT k FROM fo.src WHERE 'x'::text::integer = 1) q WHERE 1/0 = 1", syntax("integer")),
            ("SELECT (SELECT 1/0) FROM (SELECT k FROM fo.src WHERE 'x'::text::integer = 1) q", zero()),
            (
                "SELECT 1 FROM (SELECT k FROM fo.src WHERE k = 1 OR k = (SELECT 1/0)) q WHERE 'x'::text::integer = 1",
                zero(),
            ),
            ("SELECT x FROM (SELECT y AS x FROM (SELECT 1/0 AS y FROM fo.src) a) b WHERE false", zero()),
            ("SELECT x FROM (SELECT 1/0 AS x FROM fo.src) q JOIN fo.src s ON true WHERE false", zero()),
            ("SELECT j.x FROM ((SELECT 1/0 AS x FROM fo.src) q JOIN fo.src s ON true) j WHERE false", zero()),
            ("SELECT x FROM (SELECT 1/0 AS x FROM fo.src) q LEFT JOIN fo.src s ON true WHERE false", zero()),
            ("SELECT s.k FROM fo.src s LEFT JOIN (SELECT 1/0 AS x FROM fo.src) q ON q.x = s.k WHERE false", zero()),
            // One 18.6 does not pull up: planned after the query around it, its unread outputs dropped,
            // never where it is the query's one relation under a WHERE that folds to FALSE.
            ("SELECT x FROM (SELECT 1/0 AS x FROM fo.src LIMIT 1) q, fo.src s WHERE false", zero()),
            ("SELECT 1 FROM (SELECT 1/0 AS x FROM fo.none LIMIT 1) q", rows(&[])),
            ("SELECT x FROM (SELECT 1/0 AS x FROM fo.src LIMIT 5) q, fo.src s WHERE 'x'::text::integer = s.k", syntax("integer")),
            ("SELECT x FROM (SELECT 1/0 AS x FROM fo.src LIMIT 5) q, fo.src s LIMIT 'x'::text::bigint", syntax("bigint")),
            ("SELECT 1 FROM (SELECT 1/0 AS x FROM fo.src ORDER BY 1) q, fo.src s WHERE false", zero()),
            ("SELECT 1 FROM (SELECT DISTINCT 1/0 AS x FROM fo.src) q, fo.src s WHERE false", zero()),
            ("SELECT 1 FROM (SELECT k, random() + 1/0 AS x FROM fo.src) q, fo.src s WHERE false", zero()),
            // WITH: read once and not MATERIALIZED, read in place; else planned whole first.
            ("WITH c AS (SELECT 1/0 AS x FROM fo.src) SELECT x FROM c WHERE false", zero()),
            ("WITH c AS MATERIALIZED (SELECT 1/0 AS x FROM fo.src) SELECT 1 FROM c WHERE false", zero()),
            ("WITH c AS (SELECT 1/0 AS x FROM fo.src) SELECT 1 FROM c a, c b WHERE false", zero()),
            (
                "WITH c AS (SELECT 'x'::text::integer AS x FROM fo.src), d AS MATERIALIZED (SELECT 1/0 AS y FROM fo.src) SELECT c.x, d.y FROM c, d",
                zero(),
            ),
            ("WITH c AS MATERIALIZED (SELECT 1/0 AS x) SELECT 'x'::text::integer, c.x FROM c", zero()),
            ("WITH c AS (SELECT 1/0 AS x, random() AS r FROM fo.src) SELECT 1 FROM c WHERE false", zero()),
            ("WITH c AS (SELECT 1/0 AS x FROM fo.src), d AS (SELECT x FROM c) SELECT x FROM d WHERE false", zero()),
            ("WITH c AS (SELECT 1/0 AS x FROM fo.src) SELECT (SELECT x FROM c LIMIT 1) FROM fo.none WHERE false", zero()),
            ("WITH c AS (SELECT 1/0 AS x FROM fo.src) SELECT 1 FROM c WHERE false AND EXISTS (SELECT x FROM c)", zero()),
            // A set operation's arms in their order, after the query's own LIMIT; a UNION ALL's arms
            // that 18.6 lays out beside the query before those it plans on their own.
            ("SELECT 1/0 FROM fo.src WHERE false UNION ALL SELECT 1", zero()),
            ("SELECT 1/0 FROM fo.none UNION SELECT 'x'::text::integer FROM fo.none", zero()),
            ("SELECT 'x'::text::integer FROM fo.none UNION SELECT 1/0 FROM fo.none", syntax("integer")),
            ("SELECT k FROM fo.none UNION ALL SELECT k FROM fo.none LIMIT 1/0", zero()),
            ("SELECT 1/0 FROM fo.src WHERE false UNION ALL SELECT 'x'::text::integer FROM fo.src", syntax("integer")),
            ("SELECT 'x'::text::integer FROM fo.src UNION ALL SELECT 1/0 FROM fo.src", syntax("integer")),
            ("SELECT 1/0 FROM fo.none UNION SELECT 1 FROM fo.none LIMIT 'x'::text::bigint", syntax("bigint")),
            ("SELECT 1/0 FROM fo.none UNION ALL SELECT 1 FROM fo.none LIMIT 'x'::text::bigint", syntax("bigint")),
            ("SELECT 1::bigint FROM fo.src WHERE false UNION ALL SELECT 'x'::text::integer FROM fo.src", syntax("integer")),
            ("SELECT 1/0 FROM fo.src WHERE false INTERSECT SELECT 'x'::text::integer FROM fo.src", zero()),
            ("SELECT 1 FROM (SELECT 1/0 AS x FROM fo.src UNION ALL SELECT 1 FROM fo.src) u WHERE false", zero()),
            // What 18.6 never plans, over no rows: a subquery the fold dropped, an output nothing
            // reads, a WITH query nothing reads, an EXISTS's select list, the one relation under a
            // FALSE.
            ("SELECT k FROM fo.none WHERE k = (SELECT 1/0 FROM fo.none) AND false", rows(&[])),
            ("SELECT k FROM fo.none WHERE k = (SELECT 1/0 FROM fo.none) OR true", rows(&[])),
            ("SELECT CASE WHEN true THEN 1 ELSE (SELECT 1/0 FROM fo.none) END FROM fo.none", rows(&[])),
            ("SELECT k FROM fo.src WHERE true OR k IN (SELECT 1/0 FROM fo.none) ORDER BY k", all()),
            ("SELECT k FROM fo.src n WHERE n.k NOT IN (SELECT 1/0 FROM fo.none) AND false", rows(&[])),
            ("SELECT k FROM fo.src n WHERE n.k = ALL (SELECT 1/0 FROM fo.none) AND false", rows(&[])),
            ("SELECT k FROM fo.src n WHERE false AND EXISTS (SELECT 1/0 FROM fo.none)", rows(&[])),
            ("SELECT k FROM fo.none WHERE (SELECT 1/0 FROM fo.none) IS NULL AND false", rows(&[])),
            ("SELECT k FROM fo.none WHERE (SELECT 1/0 FROM fo.none) IS NULL", zero()),
            ("SELECT EXISTS (SELECT 1/0 FROM fo.none)", rows(&["f"])),
            ("SELECT EXISTS (SELECT 1/0 FROM fo.none LIMIT 1)", rows(&["f"])),
            ("SELECT 1 FROM (SELECT 1/0 AS x FROM fo.none) q WHERE false", rows(&[])),
            ("SELECT x FROM (SELECT 1/0 AS x FROM fo.none LIMIT 1) q WHERE false", rows(&[])),
            ("SELECT x FROM (SELECT 1/0 AS x FROM fo.none LIMIT 1) q, fo.none s WHERE false", zero()),
            ("SELECT x FROM (SELECT 1/0 AS x FROM fo.none LIMIT 1) q WHERE 'x'::text::integer = 1", syntax("integer")),
            (
                "SELECT 1 FROM (SELECT 1/0 AS x FROM fo.none WHERE k > 1 UNION ALL SELECT 1 FROM fo.none) u WHERE false",
                rows(&[]),
            ),
            (
                "SELECT 1 FROM (SELECT 1/0 AS x FROM fo.none WHERE k > 1 UNION ALL SELECT 1 FROM fo.none) u, fo.none s WHERE false",
                rows(&[]),
            ),
            ("WITH c AS MATERIALIZED (SELECT 1/0 AS x FROM fo.none) SELECT k FROM fo.src WHERE false", rows(&[])),
            ("WITH c AS (SELECT 1/0 AS x FROM fo.none) SELECT 1 FROM c WHERE false", rows(&[])),
            ("WITH c AS (SELECT 1/0 AS x FROM fo.none), d AS (SELECT x FROM c) SELECT 1 FROM d WHERE false", rows(&[])),
            ("WITH c AS NOT MATERIALIZED (SELECT 1/0 AS x FROM fo.none) SELECT 1 FROM c a, c b WHERE false", rows(&[])),
            // A write's subqueries alike.
            ("UPDATE fo.src SET v = (SELECT 1/0) WHERE false", zero()),
            ("INSERT INTO fo.none SELECT x, 1 FROM (SELECT 1/0 AS x FROM fo.src) q WHERE false", zero()),
            ("INSERT INTO fo.none SELECT 'x'::text::integer, 1 FROM fo.src WHERE 1/0 = 1", syntax("integer")),
            ("INSERT INTO fo.none SELECT k, 1 FROM fo.src WHERE 1/0 = 1 RETURNING 'x'::text::integer", syntax("integer")),
            ("INSERT INTO fo.none SELECT k, 'x'::text::integer FROM fo.src WHERE 1/0 = 1 LIMIT 5", syntax("integer")),
            ("SELECT k, v FROM fo.src ORDER BY k", rows(&["1|10", "2|20", "3|30", "4|40"])),
            ("SELECT count(*) FROM fo.none", rows(&["0"])),
        ],
    );
}

const FUNCTIONS: &str = "CREATE SCHEMA fo;
     CREATE TABLE fo.src (k integer NOT NULL, v integer, PRIMARY KEY (k));
     CREATE TABLE fo.none (k integer NOT NULL, v integer, PRIMARY KEY (k));
     CREATE FUNCTION fo.fv(integer) RETURNS integer LANGUAGE sql VOLATILE AS 'SELECT 1/$1';
     CREATE FUNCTION fo.fi(integer) RETURNS integer LANGUAGE sql IMMUTABLE AS 'SELECT 1/$1';
     CREATE FUNCTION fo.fc(integer) RETURNS integer LANGUAGE sql VOLATILE AS 'SELECT CASE WHEN $1 > 0 THEN 1 ELSE 1/0 END';
     CREATE FUNCTION fo.fcs(integer) RETURNS integer LANGUAGE sql STRICT VOLATILE AS 'SELECT CASE WHEN $1 > 0 THEN 1 ELSE 1/0 END';
     CREATE FUNCTION fo.fst(integer) RETURNS integer LANGUAGE sql STABLE AS 'SELECT 1/$1';
     CREATE FUNCTION fo.fss(integer) RETURNS integer LANGUAGE sql STRICT VOLATILE AS 'SELECT 1/$1';
     CREATE FUNCTION fo.fu(integer, integer) RETURNS integer LANGUAGE sql VOLATILE AS 'SELECT $1';
     CREATE FUNCTION fo.ft(integer) RETURNS integer LANGUAGE sql VOLATILE AS 'SELECT $1 + $1';
     CREATE FUNCTION fo.fn(integer) RETURNS integer LANGUAGE sql VOLATILE AS 'SELECT fo.fv($1)';
     CREATE FUNCTION fo.fr(integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN 1/$1;
     CREATE FUNCTION fo.fm(integer) RETURNS integer LANGUAGE sql IMMUTABLE AS 'SELECT 1/$1 + (random() * 0)::integer';";

#[test]
fn a_file_s_function_is_folded_as_postgresql_inlines_or_calls_it() {
    let path = tmp("folded-functions.snail");
    write(&path, &Catalog::parse(FUNCTIONS).unwrap(), &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute("INSERT INTO fo.src VALUES (1, 10), (2, 20), (3, 30), (4, 40)")
        .unwrap();
    each(
        &mut w,
        &[
            ("SELECT fo.fv(0) FROM fo.none", zero()),
            ("SELECT fo.fi(0) FROM fo.none", zero()),
            ("SELECT fo.fc(1) FROM fo.none", rows(&[])),
            ("SELECT fo.fc(k) FROM fo.none", zero()),
            ("SELECT fo.fcs(k) FROM fo.none", rows(&[])),
            ("SELECT fo.fcs(1) FROM fo.none", rows(&[])),
            ("SELECT fo.fst(0) FROM fo.none", zero()),
            ("SELECT fo.fv(k) FROM fo.none", rows(&[])),
            ("SELECT fo.fi(k) FROM fo.none WHERE false", rows(&[])),
            ("SELECT fo.fss(NULL) FROM fo.none", rows(&[])),
            ("SELECT fo.fss(0) FROM fo.none", zero()),
            ("SELECT fo.fu(1, 1/0) FROM fo.none", zero()),
            ("SELECT fo.fu(k, 1/0) FROM fo.none", zero()),
            ("SELECT fo.ft((SELECT 1/0)) FROM fo.none", zero()),
            (
                "SELECT fo.fu(k, (SELECT 1/0 FROM fo.none)) FROM fo.none",
                rows(&[]),
            ),
            (
                "SELECT fo.fu(1, (SELECT 1/0 FROM fo.none)) FROM fo.none",
                rows(&[]),
            ),
            (
                "SELECT fo.ft((SELECT 1/0 FROM fo.none)) FROM fo.none",
                zero(),
            ),
            ("SELECT fo.fn(0) FROM fo.none", zero()),
            ("SELECT fo.fn(k) FROM fo.none", rows(&[])),
            ("SELECT fo.fr(0) FROM fo.none", zero()),
            ("SELECT fo.fm(0) FROM fo.none", zero()),
            ("SELECT fo.fi(0), 'x'::text::integer FROM fo.none", zero()),
            (
                "SELECT 'x'::text::integer, fo.fi(0) FROM fo.none",
                syntax("integer"),
            ),
            ("SELECT fo.fv(0), 'x'::text::integer FROM fo.none", zero()),
            ("SELECT fo.fc(k), 'x'::text::integer FROM fo.none", zero()),
            (
                "SELECT 'x'::text::integer, fo.fc(k) FROM fo.none",
                syntax("integer"),
            ),
        ],
    );
}
