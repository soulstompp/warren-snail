//! A step's rows are handed to the step that reads them as they are made, as PostgreSQL 18.6's
//! nodes pull their rows one at a time: a derived table's draws take turns with the query's around
//! it, and a UNION ALL's arms past the cut its reader makes are never read. Every answer and refusal
//! was given by 18.6 over the same statements, in the same order.
#![cfg(feature = "standalone")]

mod weather;

use warren_snail::catalog::Catalog;
use warren_snail::read::Snail;
use weather::{fresh, tmp, write};

const DDL: &str = "CREATE SCHEMA fo;
     CREATE TABLE fo.src (k integer NOT NULL, v integer, PRIMARY KEY (k));
     CREATE SEQUENCE fo.q;
     CREATE FUNCTION fo.fu(integer, integer) RETURNS integer LANGUAGE sql VOLATILE AS 'SELECT $1';";

/// A snail holding four rows in `fo.src`, at a path of the test's name.
fn snail(name: &str) -> Snail {
    let path = tmp(&format!("pulled-{name}.snail"));
    write(&path, &Catalog::parse(DDL).unwrap(), &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute("INSERT INTO fo.src VALUES (1, 10), (2, 20), (3, 30), (4, 40)")
        .unwrap();
    w
}

type Outcome = Result<Vec<String>, (Option<&'static str>, String)>;

/// A statement's answer, each row's values as PostgreSQL's output function writes them, `|`
/// between them, a NULL as nothing; or its refusal, its SQLSTATE and its words.
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
                        .map(|v| v.unwrap_or_default())
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

const RESTART: &str = "SELECT setval('fo.q', 1, false)";
const DRAWN: &str = "SELECT currval('fo.q')";

/// Each statement answered or refused as 18.6 answers or refuses it, in its order, the sequence
/// set back to its start before each that asks it to be; every one that is not, named.
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

/// The statements, each after the sequence is set back, with 18.6's answer and, where given, the
/// sequence's value after it.
fn drawn(w: &mut Snail, asked: &[(&str, Outcome, Option<&str>)]) {
    let mut all = Vec::new();
    for (sql, want, after) in asked {
        all.push((RESTART, rows(&["1"])));
        all.push((*sql, want.clone()));
        if let Some(n) = after {
            all.push((DRAWN, rows(&[n])));
        }
    }
    each(w, &all);
}

#[test]
fn a_derived_table_s_rows_take_turns_with_the_query_around_it() {
    let mut w = snail("derived");
    drawn(
        &mut w,
        &[
            (
                "SELECT x.a, nextval('fo.q') FROM (SELECT nextval('fo.q') AS a FROM generate_series(1, 2)) x",
                rows(&["1|2", "3|4"]),
                None,
            ),
            (
                "SELECT x.a, nextval('fo.q') FROM (SELECT nextval('fo.q') AS a FROM fo.src) x",
                rows(&["1|2", "3|4", "5|6", "7|8"]),
                None,
            ),
            (
                "SELECT x.k, x.a, nextval('fo.q') FROM (SELECT k, nextval('fo.q') AS a FROM fo.src) x WHERE x.k > 2",
                rows(&["3|1|2", "4|3|4"]),
                None,
            ),
            (
                "WITH x AS (SELECT nextval('fo.q') AS a FROM generate_series(1, 2)) SELECT x.a, nextval('fo.q') FROM x",
                rows(&["1|2", "3|4"]),
                None,
            ),
            (
                "WITH x AS MATERIALIZED (SELECT nextval('fo.q') AS a FROM generate_series(1, 2)) SELECT x.a, nextval('fo.q') FROM x",
                rows(&["1|2", "3|4"]),
                None,
            ),
            (
                "SELECT y.a, y.b, nextval('fo.q') FROM (SELECT x.a, nextval('fo.q') AS b FROM (SELECT nextval('fo.q') AS a FROM generate_series(1, 2)) x) y",
                rows(&["1|2|3", "4|5|6"]),
                None,
            ),
            (
                "SELECT string_agg(x.a || ':' || nextval('fo.q'), ',') FROM (SELECT nextval('fo.q') AS a FROM generate_series(1, 2)) x",
                rows(&["1:2,3:4"]),
                None,
            ),
            (
                "SELECT x.a, nextval('fo.q') FROM (SELECT fo.fu(nextval('fo.q')::integer, 0) AS a FROM generate_series(1, 2)) x",
                rows(&["1|2", "3|4"]),
                None,
            ),
            (
                "SELECT x.a, nextval('fo.q') FROM (VALUES (nextval('fo.q')), (nextval('fo.q'))) x(a)",
                rows(&["1|2", "3|4"]),
                None,
            ),
            (
                "SELECT x.a, nextval('fo.q') FROM (SELECT a FROM (SELECT nextval('fo.q') AS a FROM generate_series(1, 2)) z) x",
                rows(&["1|2", "3|4"]),
                None,
            ),
            (
                "SELECT x.a, nextval('fo.q') FROM (SELECT nextval('fo.q') AS a FROM generate_series(1, 2)) x WHERE x.a > 0",
                rows(&["1|2", "3|4"]),
                None,
            ),
            (
                "SELECT x.a, nextval('fo.q') FROM (SELECT nextval('fo.q') AS a FROM fo.src) x WHERE x.a % 2 = 1",
                rows(&["1|2", "3|4", "5|6", "7|8"]),
                None,
            ),
            (
                "SELECT x.a, nextval('fo.q') FROM (SELECT nextval('fo.q') AS a FROM fo.src WHERE k < 3) x",
                rows(&["1|2", "3|4"]),
                None,
            ),
            // The subquery's own cut, its window and its sort on what draws nothing read as they
            // come; its sort on what draws, its DISTINCT and its grouping make it whole first.
            (
                "SELECT x.a, nextval('fo.q') FROM (SELECT nextval('fo.q') AS a FROM generate_series(1, 3) LIMIT 2) x",
                rows(&["1|2", "3|4"]),
                None,
            ),
            (
                "SELECT x.a, nextval('fo.q') FROM (SELECT nextval('fo.q') AS a, row_number() OVER () FROM generate_series(1, 2)) x",
                rows(&["1|2", "3|4"]),
                None,
            ),
            (
                "SELECT x.k, x.a, nextval('fo.q') FROM (SELECT k, nextval('fo.q') AS a FROM fo.src ORDER BY k) x",
                rows(&["1|1|2", "2|3|4", "3|5|6", "4|7|8"]),
                None,
            ),
            (
                "SELECT x.a, nextval('fo.q') FROM (SELECT nextval('fo.q') AS a FROM generate_series(1, 2) ORDER BY 1 DESC) x",
                rows(&["2|3", "1|4"]),
                None,
            ),
            (
                "SELECT x.a, nextval('fo.q') FROM (SELECT DISTINCT nextval('fo.q') AS a FROM generate_series(1, 2)) x ORDER BY 1",
                rows(&["1|3", "2|4"]),
                None,
            ),
            // Sorted around it, the query's draws come after the sort.
            (
                "SELECT x.a, nextval('fo.q') FROM (SELECT nextval('fo.q') AS a FROM generate_series(1, 2)) x ORDER BY x.a DESC",
                rows(&["2|3", "1|4"]),
                None,
            ),
            (
                "SELECT x.a, nextval('fo.q') FROM (SELECT nextval('fo.q') AS a, k FROM fo.src) x ORDER BY x.k",
                rows(&["1|5", "2|6", "3|7", "4|8"]),
                None,
            ),
            (
                "SELECT x.a, nextval('fo.q') FROM (SELECT nextval('fo.q') AS a FROM generate_series(1, 2)) x GROUP BY x.a ORDER BY 1",
                rows(&["1|3", "2|4"]),
                None,
            ),
            // A cut around it reads none of its rows past the last it keeps; its OFFSET's rows are
            // made, the query's draws with them.
            (
                "SELECT x.a FROM (SELECT nextval('fo.q') AS a FROM fo.src) x LIMIT 2",
                rows(&["1", "2"]),
                Some("2"),
            ),
            (
                "SELECT x.a, nextval('fo.q') FROM (SELECT nextval('fo.q') AS a FROM generate_series(1, 2)) x LIMIT 1",
                rows(&["1|2"]),
                Some("2"),
            ),
            (
                "SELECT x.a FROM (SELECT nextval('fo.q') AS a FROM fo.src) x WHERE x.a > 2 LIMIT 1",
                rows(&["3"]),
                Some("3"),
            ),
            (
                "SELECT y.a FROM (SELECT x.a FROM (SELECT nextval('fo.q') AS a FROM fo.src) x LIMIT 3) y LIMIT 1",
                rows(&["1"]),
                Some("1"),
            ),
            (
                "SELECT count(*) FROM (SELECT nextval('fo.q') AS a FROM fo.src LIMIT 2) x",
                rows(&["2"]),
                Some("2"),
            ),
            (
                "SELECT x.a, nextval('fo.q') FROM (SELECT nextval('fo.q') AS a FROM fo.src) x LIMIT 3 OFFSET 1",
                rows(&["3|4", "5|6", "7|8"]),
                Some("8"),
            ),
        ],
    );
    each(
        &mut w,
        &[
            ("SELECT setseed(0.5)", rows(&[""])),
            (
                "SELECT round(x.r::numeric, 6), round(random()::numeric, 6) FROM (SELECT random() AS r FROM generate_series(1, 2)) x",
                rows(&["0.985168|0.825302", "0.129746|0.163563"]),
            ),
            // A row past the cut is never made.
            ("SELECT x.a FROM (SELECT 1/(k - 2) AS a FROM fo.src) x WHERE random() >= 0 LIMIT 1", rows(&["-1"])),
            ("SELECT x.a, x.k FROM (SELECT k, 1/(k - 2) AS a FROM fo.src) x WHERE random() >= 0 LIMIT 1", rows(&["-1|1"])),
            ("SELECT x FROM (SELECT k, 1/(k - 2) AS x, random() AS r FROM fo.src) q LIMIT 1", rows(&["-1"])),
            ("SELECT x FROM (SELECT k, 1/(k - 2) AS x FROM fo.src LIMIT 3) q LIMIT 1", rows(&["-1"])),
            ("SELECT x FROM (SELECT DISTINCT k, 1/(k - 2) AS x FROM fo.src) q LIMIT 1", zero()),
        ],
    );
}

#[test]
fn a_union_all_s_arms_past_its_reader_s_cut_are_never_read() {
    let mut w = snail("union");
    drawn(
        &mut w,
        &[
            ("SELECT nextval('fo.q') UNION ALL SELECT nextval('fo.q') LIMIT 1", rows(&["1"]), Some("1")),
            (
                "SELECT nextval('fo.q') FROM generate_series(1, 3) UNION ALL SELECT nextval('fo.q') FROM generate_series(1, 3) LIMIT 2",
                rows(&["1", "2"]),
                Some("2"),
            ),
            ("SELECT * FROM (SELECT nextval('fo.q') UNION ALL SELECT nextval('fo.q')) x LIMIT 1", rows(&["1"]), Some("1")),
            (
                "SELECT nextval('fo.q') UNION ALL SELECT nextval('fo.q') UNION ALL SELECT nextval('fo.q') LIMIT 2",
                rows(&["1", "2"]),
                Some("2"),
            ),
            (
                "(SELECT nextval('fo.q') UNION ALL SELECT nextval('fo.q')) UNION ALL SELECT nextval('fo.q') LIMIT 1",
                rows(&["1"]),
                Some("1"),
            ),
            ("SELECT nextval('fo.q') UNION ALL VALUES (nextval('fo.q')) LIMIT 1", rows(&["1"]), Some("1")),
            (
                "SELECT x FROM (SELECT nextval('fo.q') AS x UNION ALL SELECT nextval('fo.q')) s WHERE x > 0 LIMIT 1",
                rows(&["1"]),
                Some("1"),
            ),
            ("SELECT nextval('fo.q') UNION ALL SELECT nextval('fo.q') FETCH FIRST 1 ROW ONLY", rows(&["1"]), Some("1")),
            ("SELECT (SELECT nextval('fo.q') UNION ALL SELECT nextval('fo.q') LIMIT 1)", rows(&["1"]), Some("1")),
            // A cut of none draws none: the session's last draw stands.
            ("SELECT nextval('fo.q') UNION ALL SELECT nextval('fo.q') LIMIT 0", rows(&[]), Some("1")),
            ("SELECT nextval('fo.q') UNION ALL SELECT nextval('fo.q') OFFSET 1 LIMIT 1", rows(&["2"]), Some("2")),
            // The arms beside a draw around them take turns with it.
            (
                "SELECT x.a, nextval('fo.q') FROM (SELECT nextval('fo.q') AS a FROM generate_series(1, 2) UNION ALL SELECT nextval('fo.q') FROM generate_series(1, 2)) x",
                rows(&["1|2", "3|4", "5|6", "7|8"]),
                None,
            ),
            (
                "SELECT x.a, nextval('fo.q') FROM (SELECT nextval('fo.q') AS a FROM generate_series(1, 2) UNION ALL SELECT 9) x",
                rows(&["1|2", "3|4", "9|5"]),
                None,
            ),
            // UNION, INTERSECT and a sort read every arm.
            ("SELECT 1 FROM (SELECT nextval('fo.q') UNION SELECT nextval('fo.q') LIMIT 1) x", rows(&["1"]), Some("2")),
            ("SELECT nextval('fo.q') UNION ALL SELECT nextval('fo.q') ORDER BY 1 LIMIT 1", rows(&["1"]), Some("2")),
            ("SELECT nextval('fo.q') INTERSECT ALL SELECT nextval('fo.q') LIMIT 1", rows(&[]), Some("2")),
        ],
    );
    each(
        &mut w,
        &[
            ("SELECT 1/(k - 2) FROM fo.src WHERE k = 1 UNION ALL SELECT 1/(k - 2) FROM fo.src LIMIT 1", rows(&["-1"])),
            ("SELECT k FROM fo.src UNION ALL SELECT 1/(k - 2) FROM fo.src LIMIT 4", rows(&["1", "2", "3", "4"])),
            ("SELECT k FROM fo.src UNION ALL SELECT 1/(k - 2) FROM fo.src LIMIT 5", rows(&["1", "2", "3", "4", "-1"])),
            ("SELECT k FROM fo.src WHERE k < 3 UNION ALL SELECT k FROM fo.src WHERE 1/(k - 2) > 0 LIMIT 2", rows(&["1", "2"])),
            (
                "SELECT x.a FROM (SELECT 1/(k - 2) AS a FROM fo.src WHERE k = 1 UNION ALL SELECT 1/(k - 2) FROM fo.src) x LIMIT 1",
                rows(&["-1"]),
            ),
            // Each arm's rows read in the operation's types as they come.
            (
                "SELECT x / 2 FROM (SELECT 3 AS x UNION ALL SELECT 2.5) s LIMIT 2",
                rows(&["1.5000000000000000", "1.25000000000000000000"]),
            ),
        ],
    );
}

#[test]
fn a_set_s_rows_are_handed_on_as_they_are_made() {
    let mut w = snail("sets");
    drawn(
        &mut w,
        &[
            (
                "SELECT x.a, nextval('fo.q') FROM (SELECT nextval('fo.q') AS a, generate_series(1, 2) AS g) x",
                rows(&["1|2", "3|4"]),
                None,
            ),
            ("SELECT nextval('fo.q'), generate_series(1, 3) LIMIT 2", rows(&["1|1", "2|2"]), Some("2")),
            (
                "SELECT nextval('fo.q'), generate_series(1, 2) FROM fo.src LIMIT 3",
                rows(&["1|1", "2|2", "4|1"]),
                Some("4"),
            ),
            (
                "SELECT g FROM (SELECT generate_series(1, 100000000) AS g) x LIMIT 3",
                rows(&["1", "2", "3"]),
                None,
            ),
        ],
    );
}

// A scan stops once the cut its rows reach has them all: the blocks past it are never read, their
// pages predicted and left unread, the rest read as predicted.
#[test]
fn a_scan_stops_once_its_cut_has_its_rows() {
    let path = tmp("pulled-scan.snail");
    let ddl = "CREATE SCHEMA fo; CREATE TABLE fo.big (k integer NOT NULL, v integer, PRIMARY KEY (k));";
    write(&path, &Catalog::parse(ddl).unwrap(), &[]).unwrap();
    Snail::open_writing(&path)
        .unwrap()
        .execute("INSERT INTO fo.big SELECT g, g * 10 FROM generate_series(1, 20000) g")
        .unwrap();
    let asked = |q: &str| {
        let mut s = fresh(&path);
        let (b, round) = s.ask(q, None).unwrap_or_else(|e| panic!("{q}: {e}"));
        weather::exact(q, &round);
        let rows: Vec<String> = s
            .text_of(&b)
            .unwrap()
            .into_iter()
            .map(|r| {
                r.into_iter()
                    .map(Option::unwrap_or_default)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect();
        (rows, round)
    };
    let (_, all) = asked("SELECT k, v FROM fo.big");
    assert!(all.blocks_read > 2, "{all:?}");
    for (q, want) in [
        ("SELECT k FROM fo.big LIMIT 3", vec!["1", "2", "3"]),
        ("SELECT k, v FROM fo.big WHERE v > 100 LIMIT 3", vec!["11|110", "12|120", "13|130"]),
        ("SELECT x.k FROM (SELECT k FROM fo.big) x LIMIT 3", vec!["1", "2", "3"]),
        ("SELECT k, generate_series(1, 3) FROM fo.big LIMIT 4", vec!["1|1", "1|2", "1|3", "2|1"]),
        ("SELECT k FROM fo.big UNION ALL SELECT k FROM fo.big LIMIT 3", vec!["1", "2", "3"]),
    ] {
        let (got, round) = asked(q);
        assert_eq!(got, want, "{q}");
        assert_eq!(round.blocks_read, 1, "{q}: {round:?}");
        assert!(round.column_pages_unread > 0, "{q}: {round:?}");
    }
    // A sort reads every row first.
    let (got, round) = asked("SELECT k FROM fo.big ORDER BY v DESC LIMIT 3");
    assert_eq!(got, ["20000", "19999", "19998"]);
    assert_eq!(round.blocks_read, all.blocks_read, "{round:?}");
    assert_eq!(round.column_pages_unread, 0, "{round:?}");
}
