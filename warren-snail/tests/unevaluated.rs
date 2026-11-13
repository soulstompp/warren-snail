//! What PostgreSQL 18.6's plan never reaches is never evaluated over rows either: a subquery a
//! constant AND, OR, CASE or COALESCE drops, read neither while its shape is found nor as a row
//! asks; an EXISTS's select list; an argument an inlined function's body never names. Every answer
//! and refusal was given by 18.6 over the same statements, in the same order.
#![cfg(feature = "standalone")]

mod weather;

use warren_snail::catalog::Catalog;
use warren_snail::read::Snail;
use weather::{tmp, write};

const DDL: &str = "CREATE SCHEMA fo;
     CREATE TABLE fo.src (k integer NOT NULL, v integer, PRIMARY KEY (k));
     CREATE TABLE fo.none (k integer NOT NULL, v integer, PRIMARY KEY (k));
     CREATE FUNCTION fo.fu(integer, integer) RETURNS integer LANGUAGE sql VOLATILE AS 'SELECT $1';";

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
