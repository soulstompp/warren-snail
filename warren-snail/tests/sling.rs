//! The PostgreSQL warren-sling writes, answered as PostgreSQL 18.6 answers it: an ORDER BY naming a
//! column of the select list by its name or its place, read once, whatever names the column's own
//! expression holds; an aggregate inside an IN list, a BETWEEN, a LIKE, a COLLATE or a SUBSTRING,
//! grouped or not; an aggregate in ORDER BY, a column of the select list or none. Every answer and
//! refusal is 18.6's over the same statements (`session.nu --suite sling`). A snail writes on its own
//! only with `standalone`, as `tests/ddl.rs`.
#![cfg(feature = "standalone")]

mod weather;

use std::time::{Duration, Instant};
use warren_snail::catalog::Catalog;
use warren_snail::read::Snail;
use weather::{tmp, write};

const DDL: &str = "CREATE SCHEMA s;
     CREATE TABLE s.trans (seq integer NOT NULL, acct varchar(6) NOT NULL, amount numeric(9,2),
         PRIMARY KEY (seq));
     CREATE VIEW s.v AS SELECT seq, acct, amount FROM s.trans;";

const ROWS: &str = "INSERT INTO s.trans VALUES (3, 'A1', 1.00), (1, 'A2', 2.00), (2, 'A1', 3.50), \
     (5, 'A2', NULL), (4, 'A3', 4.00)";

/// A snail holding the five transactions, at a path of the test's name.
fn snail(name: &str) -> (std::path::PathBuf, Snail) {
    let path = tmp(&format!("sling-{name}.snail"));
    write(&path, &Catalog::parse(DDL).unwrap(), &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute(ROWS).unwrap();
    (path, w)
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

/// `exec` runs the statements to their end in a program of its own, so that one reading without end,
/// or recursing until its stack is spent, fails here by its name and never takes the tests with it.
fn ended(path: &std::path::Path, name: &str, asked: &[(&str, &[&str])]) {
    let statements = tmp(&format!("sling-{name}.sql"));
    let text: String = asked.iter().map(|(sql, _)| format!("{sql};\n")).collect();
    std::fs::write(&statements, text).unwrap();
    let done = tmp(&format!("sling-{name}.parquet"));
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_warren-snail"))
        .args(["exec", path.to_str().unwrap(), statements.to_str().unwrap(), "--out"])
        .arg(&done)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if start.elapsed() > Duration::from_secs(120) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("exec ran past two minutes on {statements:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut said = String::new();
    std::io::Read::read_to_string(child.stderr.as_mut().unwrap(), &mut said).unwrap();
    assert!(status.success(), "exec ended {status}: {said}");
}

#[test]
fn an_order_by_names_a_column_of_the_select_list_once() {
    // The column's expression names the FROM's columns, its own name among them: the name is
    // never the select list's again.
    let asked: &[(&str, &[&str])] = &[
        (
            "SELECT (seq) AS seq, acct FROM s.trans ORDER BY seq",
            &["1|A2", "2|A1", "3|A1", "4|A3", "5|A2"],
        ),
        (
            "SELECT seq AS seq, acct FROM s.trans ORDER BY seq DESC",
            &["5|A2", "4|A3", "3|A1", "2|A1", "1|A2"],
        ),
        (
            "SELECT acct AS seq, seq AS acct FROM s.trans ORDER BY seq, acct",
            &["A1|2", "A1|3", "A2|1", "A2|5", "A3|4"],
        ),
        ("SELECT seq AS seq FROM s.trans ORDER BY 1", &["1", "2", "3", "4", "5"]),
        ("SELECT (seq) AS seq FROM s.trans ORDER BY (seq)", &["1", "2", "3", "4", "5"]),
        (
            "SELECT -seq AS seq, acct FROM s.trans ORDER BY seq",
            &["-5|A2", "-4|A3", "-3|A1", "-2|A1", "-1|A2"],
        ),
        (
            "WITH t AS (SELECT seq, acct FROM s.trans) SELECT (seq) AS seq, acct FROM t ORDER BY seq",
            &["1|A2", "2|A1", "3|A1", "4|A3", "5|A2"],
        ),
        (
            "SELECT (acct) AS acct, count(*) FROM s.trans GROUP BY acct ORDER BY acct",
            &["A1|2", "A2|2", "A3|1"],
        ),
        (
            "SELECT (seq) AS seq, acct FROM s.trans WHERE seq > 2 ORDER BY seq DESC LIMIT 2",
            &["5|A2", "4|A3"],
        ),
    ];
    let (path, w) = snail("names");
    drop(w);
    ended(&path, "names", asked);
    answered(&mut Snail::open_writing(&path).unwrap(), asked);
}

#[test]
fn an_aggregate_inside_an_in_list_is_its_group_s_value() {
    let (_, mut w) = snail("in-lists");
    answered(
        &mut w,
        &[
            (
                "SELECT acct, 'a' IN (CASE WHEN count(*) > 0 THEN 'a' ELSE 'b' END, 'c') \
                 FROM s.trans GROUP BY acct ORDER BY acct",
                &["A1|t", "A2|t", "A3|t"],
            ),
            (
                "SELECT 'a' IN (CASE WHEN count(*) > 0 THEN 'a' ELSE 'b' END, 'c') FROM s.trans",
                &["t"],
            ),
            // One group over no rows: its count 0.
            (
                "SELECT 'a' IN (CASE WHEN count(*) > 0 THEN 'a' ELSE 'b' END, 'c') FROM s.trans \
                 WHERE false",
                &["f"],
            ),
            (
                "SELECT acct, 'A1' IN (min(acct), 'c') FROM s.trans GROUP BY acct ORDER BY acct",
                &["A1|t", "A2|f", "A3|f"],
            ),
            (
                "SELECT acct, 2 NOT IN (count(*), 9) FROM s.trans GROUP BY acct ORDER BY acct",
                &["A1|f", "A2|f", "A3|t"],
            ),
            (
                "SELECT acct FROM s.trans GROUP BY acct HAVING 2 IN (count(*), 9) ORDER BY acct",
                &["A1", "A2"],
            ),
            (
                "SELECT acct, count(*) FROM s.trans GROUP BY acct ORDER BY count(*) IN (1, 3), acct",
                &["A1|2", "A2|2", "A3|1"],
            ),
            (
                "SELECT acct, NULL IN (count(*), 1), count(*) IN (NULL, 2) FROM s.trans \
                 GROUP BY acct ORDER BY acct",
                &["A1|NULL|t", "A2|NULL|t", "A3|NULL|NULL"],
            ),
            (
                "SELECT region, 'a' IN (CASE WHEN count(*) > 0 THEN 'a' ELSE 'b' END, 'c') \
                 FROM (VALUES ('EA', 1.50), ('EB', NULL)) a(region, x) GROUP BY region \
                 ORDER BY region",
                &["EA|t", "EB|t"],
            ),
            (
                "SELECT region, sum(x) / count(x) IN (1.50, 2) FROM (VALUES ('EA', 1.50), \
                 ('EB', NULL)) a(region, x) GROUP BY region ORDER BY region",
                &["EA|t", "EB|NULL"],
            ),
            (
                "SELECT acct, seq IN (min(seq), 4) FROM s.trans GROUP BY acct, seq \
                 ORDER BY acct, seq",
                &["A1|t", "A1|t", "A2|t", "A2|t", "A3|t"],
            ),
            (
                "SELECT acct, 1 IN (count(*) OVER (PARTITION BY acct), 9) FROM s.trans \
                 ORDER BY acct, seq",
                &["A1|f", "A1|f", "A2|f", "A2|f", "A3|t"],
            ),
            // Inside the other expressions a fold may stand in.
            (
                "SELECT acct, count(*) BETWEEN 2 AND 3, min(acct) LIKE 'A1%' FROM s.trans \
                 GROUP BY acct ORDER BY acct",
                &["A1|t|t", "A2|t|f", "A3|f|f"],
            ),
            (
                "SELECT acct, substring(min(acct) from 2), min(acct) COLLATE \"C\" FROM s.trans \
                 GROUP BY acct ORDER BY acct",
                &["A1|1|A1", "A2|2|A2", "A3|3|A3"],
            ),
        ],
    );
    assert_eq!(
        refused(&mut w, "SELECT acct IN (min(acct), 'c') FROM s.trans"),
        (
            Some("42803"),
            "column \"trans.acct\" must appear in the GROUP BY clause or be used in an aggregate \
             function"
                .into()
        )
    );
}

#[test]
fn an_aggregate_in_order_by_is_read_whether_the_select_list_returns_it_or_not() {
    let (_, mut w) = snail("order-folds");
    answered(
        &mut w,
        &[
            (
                "SELECT acct, count(*) AS n FROM s.trans GROUP BY acct ORDER BY count(*), acct",
                &["A3|1", "A1|2", "A2|2"],
            ),
            (
                "SELECT acct, max(seq) FROM s.trans GROUP BY acct ORDER BY max(seq)",
                &["A1|3", "A3|4", "A2|5"],
            ),
            (
                "SELECT acct, sum(amount) AS total FROM s.trans GROUP BY acct ORDER BY min(seq)",
                &["A2|2.00", "A1|4.50", "A3|4.00"],
            ),
            (
                "SELECT acct, count(*) AS n FROM s.trans GROUP BY acct ORDER BY min(seq)",
                &["A2|2", "A1|2", "A3|1"],
            ),
            (
                "SELECT acct FROM s.trans GROUP BY acct ORDER BY max(seq) DESC",
                &["A2", "A3", "A1"],
            ),
            (
                "SELECT acct, count(*) AS n FROM s.trans GROUP BY acct \
                 ORDER BY count(*) DESC, min(seq)",
                &["A2|2", "A1|2", "A3|1"],
            ),
            (
                "SELECT acct FROM s.trans GROUP BY acct ORDER BY min(seq) + 1",
                &["A2", "A1", "A3"],
            ),
            ("SELECT count(*) FROM s.trans ORDER BY min(seq)", &["5"]),
            (
                "WITH t AS (SELECT seq, acct, amount FROM s.trans) SELECT acct, sum(amount) \
                 FROM t GROUP BY acct ORDER BY min(seq)",
                &["A2|2.00", "A1|4.50", "A3|4.00"],
            ),
            (
                "SELECT a.acct, count(*) FROM s.trans a JOIN s.trans b ON a.seq = b.seq \
                 GROUP BY a.acct ORDER BY min(b.seq)",
                &["A2|2", "A1|2", "A3|1"],
            ),
            (
                "SELECT acct, count(*) FROM s.v GROUP BY acct ORDER BY min(seq)",
                &["A2|2", "A1|2", "A3|1"],
            ),
        ],
    );
    assert_eq!(
        refused(
            &mut w,
            "SELECT DISTINCT acct FROM s.trans GROUP BY acct ORDER BY min(seq)"
        ),
        (
            Some("42P10"),
            "for SELECT DISTINCT, ORDER BY expressions must appear in select list".into()
        )
    );
}
