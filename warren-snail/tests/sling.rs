//! The PostgreSQL warren-sling writes, answered as PostgreSQL 18.6 answers it: an ORDER BY naming a
//! column of the select list by its name or its place, read once, whatever names the column's own
//! expression holds; an aggregate inside an IN list, a BETWEEN, a LIKE, a COLLATE or a SUBSTRING,
//! grouped or not; an aggregate in ORDER BY, a column of the select list or none; and two serializable
//! writers of one block, each of its own row by the table's key, both committing, as 18.6 commits them
//! where the row's page has room, a write skew refused and a key moved into a read gap met; and a
//! read alone between a writer's texts committing nothing, as 18.6's commits nothing, the block it
//! found wasted pruned by the writer's next commit that writes; an OR anywhere in a WHERE or an ON,
//! two writers each reading its own rows by an OR of their keys both committing, as 18.6 commits them
//! reading by its index; and a row value IN a list of rows, read as the OR of its rows' comparisons;
//! and a subquery in a join's ON, correlated to either side or to the query around, a FULL JOIN on
//! one side's subquery answered and any other refused, and an ON inside a subquery reading the query
//! around it, a FULL JOIN's equality holding it among them; an outer query's column read anywhere in
//! a join's ON inside a subquery, and a subquery in FROM reading a relation laid before it, a LATERAL
//! reference to a RIGHT or FULL JOIN's left side and an aggregate or a window in an ON refused in
//! 18.6's words; a type, a domain, a relation or a function of a name taken refused in 18.6's words;
//! and DDL in a transaction naming and making
//! against the catalog as committed, at each level, a name another committed since refused in its
//! kind's words, one dropped since made again, DDL over a table made since reading none of its rows,
//! and writers of different names both committing. Every answer and refusal is 18.6's over the same
//! statements (`session.nu --suite sling`, and `sessions.nu --only serializable` for the writers,
//! `sessions.nu --only "as committed"` for the DDL; 18.6 with its index, through `dblink`, for the
//! writers by an OR). A snail writes on its own only with `standalone`, as `tests/ddl.rs`.
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
        .args([
            "exec",
            path.to_str().unwrap(),
            statements.to_str().unwrap(),
            "--out",
        ])
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

/// Two writers of the accounts, as warren-sling's sessions write them: one block holds both rows.
fn accounts(name: &str) -> (Snail, Snail, std::path::PathBuf) {
    let path = tmp(&format!("sling-{name}.snail"));
    let ddl = "CREATE SCHEMA s; CREATE TABLE s.accounts (acct varchar(6) NOT NULL, \
               balance numeric(9,2), PRIMARY KEY (acct));";
    write(&path, &Catalog::parse(ddl).unwrap(), &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute("INSERT INTO s.accounts VALUES ('A00001', 10.00), ('A00002', 20.00)")
        .unwrap();
    drop(w);
    (
        Snail::open_writing(&path).unwrap(),
        Snail::open_writing(&path).unwrap(),
        path,
    )
}

#[test]
fn two_serializable_writers_of_their_own_rows_of_one_block_both_commit() {
    // Each reads and writes its own row by the table's key, the other whole between: 18.6 commits
    // both, each UPDATE in place.
    let (mut a, mut b, path) = accounts("own-rows");
    a.execute("BEGIN ISOLATION LEVEL SERIALIZABLE").unwrap();
    assert_eq!(
        rows(
            &mut a,
            "SELECT acct, balance + 1.00 FROM s.accounts WHERE acct = 'A00001'"
        ),
        ["A00001|11.00"]
    );
    b.execute("BEGIN ISOLATION LEVEL SERIALIZABLE").unwrap();
    assert_eq!(
        rows(
            &mut b,
            "SELECT acct, balance + 2.00 FROM s.accounts WHERE acct = 'A00002'"
        ),
        ["A00002|22.00"]
    );
    b.execute("UPDATE s.accounts SET balance = balance + 2.00 WHERE acct = 'A00002'")
        .unwrap();
    b.execute("COMMIT").unwrap();
    a.execute("UPDATE s.accounts SET balance = balance + 1.00 WHERE acct = 'A00001'")
        .unwrap();
    a.execute("COMMIT").unwrap();
    let mut c = Snail::open_writing(&path).unwrap();
    assert_eq!(
        rows(&mut c, "SELECT acct, balance FROM s.accounts ORDER BY acct"),
        ["A00001|11.00", "A00002|22.00"]
    );
}

#[test]
fn two_serializable_writers_each_of_the_other_s_row_by_key_are_a_write_skew_refused() {
    let (mut a, mut b, path) = accounts("skew");
    a.execute("BEGIN ISOLATION LEVEL SERIALIZABLE").unwrap();
    b.execute("BEGIN ISOLATION LEVEL SERIALIZABLE").unwrap();
    rows(
        &mut a,
        "SELECT balance FROM s.accounts WHERE acct = 'A00001'",
    );
    rows(
        &mut b,
        "SELECT balance FROM s.accounts WHERE acct = 'A00002'",
    );
    a.execute("UPDATE s.accounts SET balance = 0 WHERE acct = 'A00002'")
        .unwrap();
    a.execute("COMMIT").unwrap();
    let refused = b
        .execute("UPDATE s.accounts SET balance = 0 WHERE acct = 'A00001'")
        .and_then(|_| b.execute("COMMIT"))
        .unwrap_err();
    assert_eq!(refused.sqlstate(), Some("40001"), "{refused}");
    let mut c = Snail::open_writing(&path).unwrap();
    assert_eq!(
        rows(&mut c, "SELECT acct, balance FROM s.accounts ORDER BY acct"),
        ["A00001|10.00", "A00002|0.00"]
    );
}

#[test]
fn an_update_moving_a_key_lands_in_the_gap_a_read_of_that_key_marked() {
    // a reads a key no row has, inside the block; b moves its own row's key into that gap, to a key a
    // never read, and commits; a's INSERT lands in the gap b read: two edges, and a, its other
    // committed first, is refused, as 18.6 refuses it.
    let (mut a, mut b, path) = accounts("moved");
    a.execute("INSERT INTO s.accounts VALUES ('A00009', 90.00)")
        .unwrap();
    a.execute("BEGIN ISOLATION LEVEL SERIALIZABLE").unwrap();
    b.execute("BEGIN ISOLATION LEVEL SERIALIZABLE").unwrap();
    assert!(rows(
        &mut a,
        "SELECT balance FROM s.accounts WHERE acct = 'A00003'"
    )
    .is_empty());
    rows(
        &mut b,
        "SELECT balance FROM s.accounts WHERE acct = 'A00002'",
    );
    b.execute("UPDATE s.accounts SET acct = 'A00005' WHERE acct = 'A00002'")
        .unwrap();
    b.execute("COMMIT").unwrap();
    let refused = a
        .execute("INSERT INTO s.accounts VALUES ('A00004', 1.00)")
        .and_then(|_| a.execute("COMMIT"))
        .unwrap_err();
    assert_eq!(refused.sqlstate(), Some("40001"), "{refused}");
    let mut c = Snail::open_writing(&path).unwrap();
    assert_eq!(
        rows(&mut c, "SELECT acct, balance FROM s.accounts ORDER BY acct"),
        ["A00001|10.00", "A00005|20.00", "A00009|90.00"]
    );
}

/// A host counting the commits handed it.
struct Counted(std::sync::Arc<std::sync::atomic::AtomicU64>);

impl warren_snail::carry::Host for Counted {
    fn carry(&mut self, _: &warren_snail::carry::Commit) -> warren_snail::Result<()> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

/// The disk pages of each block of `s.designs`' `written` column.
fn written_pages(w: &Snail) -> Vec<u64> {
    w.blocks()
        .unwrap()
        .into_iter()
        .filter(|b| b.table == "s.designs" && b.column == "written")
        .map(|b| b.disk_pages)
        .collect()
}

/// The answers, the row counts and the refusal are 18.6's over the same statements; the commits
/// handed to the host, the generations and the pages are the snail's own: a read alone writes no
/// commit, as 18.6's writes no commit record.
#[test]
fn a_read_alone_commits_nothing_and_its_prune_rides_the_writer_s_next_commit() {
    let path = tmp("sling-ride.snail");
    let ddl = "CREATE SCHEMA s;
         CREATE TABLE s.designs (name varchar(32) NOT NULL, ordinal integer NOT NULL, written text,
             PRIMARY KEY (name));
         CREATE TABLE s.filled (set_name varchar(32) NOT NULL, way varchar(8),
             PRIMARY KEY (set_name));";
    write(&path, &Catalog::parse(ddl).unwrap(), &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    let handed = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    w.carried_by(Box::new(Counted(handed.clone()))).unwrap();
    let commits = || handed.load(std::sync::atomic::Ordering::SeqCst);
    // Two texts as warren-sling sends them, each a row of its own: the second lands in the block's
    // tail.
    for row in ["('trans', 1, 'set trans')", "('totals', 2, 'set totals')"] {
        w.execute("BEGIN ISOLATION LEVEL SERIALIZABLE").unwrap();
        w.ask("SELECT 1", None).unwrap();
        w.execute(&format!("INSERT INTO s.designs VALUES {row}"))
            .unwrap();
        w.execute("COMMIT").unwrap();
    }
    assert_eq!((commits(), written_pages(&w)), (2, vec![2]));
    let at = w.generation();
    // Read alone, twice: each pays the tail's page and commits nothing.
    for _ in 0..2 {
        let (b, round) = w
            .ask("SELECT written FROM s.designs ORDER BY ordinal, name", None)
            .unwrap();
        let got: Vec<Option<String>> = w.text_of(&b).unwrap().into_iter().flatten().collect();
        assert_eq!(
            got,
            [Some("set trans".to_string()), Some("set totals".to_string())]
        );
        assert_eq!(round.pages_written, 0, "{round:?}");
        assert_eq!(
            (w.generation(), Snail::open(&path).unwrap().generation(), commits()),
            (at, at, 2)
        );
    }
    // A statement alone that writes nothing, and one refused, commit nothing either.
    let done = w
        .execute("UPDATE s.designs SET ordinal = 3 WHERE name = 'none'")
        .unwrap();
    assert_eq!(done.rows, 0);
    let (state, _) = refused(&mut w, "INSERT INTO s.designs VALUES ('trans', 9, 'again')");
    assert_eq!(state, Some("23505"));
    assert_eq!((w.generation(), commits()), (at, 2));
    // The next text that writes commits once, the block the reads found pruned in it.
    let d = w
        .execute("INSERT INTO s.filled VALUES ('trans', 'sling')")
        .unwrap();
    assert_eq!(
        (d.blocks_pruned, w.generation(), commits(), written_pages(&w)),
        (1, at + 1, 3, vec![1])
    );
    assert_eq!(
        rows(&mut w, "SELECT name, ordinal, written FROM s.designs ORDER BY name"),
        ["totals|2|set totals", "trans|1|set trans"]
    );
    // A commit that writes the very block a read found wasted prunes it as it writes it.
    w.execute("INSERT INTO s.designs VALUES ('patch', 3, 'set patch')")
        .unwrap();
    assert_eq!(written_pages(&w), [2]);
    w.ask("SELECT written FROM s.designs ORDER BY ordinal, name", None)
        .unwrap();
    let d = w
        .execute("INSERT INTO s.designs VALUES ('push', 4, 'set push')")
        .unwrap();
    assert_eq!(
        (d.blocks_pruned, commits(), written_pages(&w)),
        (1, 5, vec![1])
    );
    assert_eq!(
        rows(&mut w, "SELECT name, ordinal, written FROM s.designs ORDER BY name"),
        [
            "patch|3|set patch",
            "push|4|set push",
            "totals|2|set totals",
            "trans|1|set trans"
        ]
    );
}

/// A snail of the DDL given, its rows inserted, at a path of the test's name.
fn declared(name: &str, ddl: &str, inserts: &[&str]) -> std::path::PathBuf {
    let path = tmp(&format!("sling-{name}.snail"));
    write(&path, &Catalog::parse(ddl).unwrap(), &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    for i in inserts {
        w.execute(i).unwrap();
    }
    path
}

const K4: &str = "CREATE SCHEMA k4;
     CREATE TABLE k4.accounts (acct varchar(6) NOT NULL, region varchar(2), \"limit\" numeric(9,2),
         PRIMARY KEY (acct));
     CREATE TABLE k4.r (a integer NOT NULL, b varchar(4) NOT NULL, v numeric(9,2), PRIMARY KEY (a, b));
     CREATE VIEW k4.v AS SELECT acct FROM k4.accounts WHERE acct = 'A00002' OR \"limit\" < 60;";

const K4_ROWS: [&str; 2] = [
    "INSERT INTO k4.accounts VALUES ('A00001', 'EU', 150.00), ('A00002', 'US', NULL), \
     ('A00003', 'EU', NULL), ('A00004', 'US', 50.00)",
    "INSERT INTO k4.r VALUES (1, 'x', 1.00), (1, 'y', 2.00), (2, 'x', 3.00)",
];

#[test]
fn an_or_in_a_where_or_an_on_is_answered_as_postgresql_answers_it() {
    let asked: &[(&str, &[&str])] = &[
        (
            "SELECT acct FROM k4.accounts WHERE (acct = 'A00001') OR (acct = 'A00002') ORDER BY acct",
            &["A00001", "A00002"],
        ),
        (
            "SELECT acct FROM k4.accounts WHERE region = 'EU' OR \"limit\" > 100 ORDER BY acct",
            &["A00001", "A00003"],
        ),
        (
            "SELECT acct FROM k4.accounts WHERE region = 'US' AND (\"limit\" > 10 OR \"limit\" IS NULL) \
             ORDER BY acct",
            &["A00002", "A00004"],
        ),
        (
            "SELECT a, b FROM k4.r WHERE (a = 1 AND b = 'y') OR (a = 2 AND b = 'x') ORDER BY a, b",
            &["1|y", "2|x"],
        ),
        (
            "SELECT acct FROM k4.accounts WHERE NOT (\"limit\" > 100 OR region = 'EU') ORDER BY acct",
            &["A00004"],
        ),
        (
            "SELECT acct, \"limit\" > 100 OR region = 'EU' AS x FROM k4.accounts ORDER BY acct",
            &["A00001|t", "A00002|NULL", "A00003|t", "A00004|f"],
        ),
        (
            "SELECT acct FROM k4.accounts WHERE acct = 'A00001' OR region = 'US' ORDER BY acct",
            &["A00001", "A00002", "A00004"],
        ),
        (
            "SELECT acct FROM k4.accounts WHERE acct = 'A00001' OR (acct = 'A00009' OR acct = NULL) \
             OR acct = 'A00001'",
            &["A00001"],
        ),
        (
            "SELECT a, b FROM k4.r WHERE (a = 1 AND b = 'y' AND v > 5) OR (a = 2 AND b = 'x') \
             ORDER BY a, b",
            &["2|x"],
        ),
        (
            "SELECT r.a, c.acct FROM k4.r r JOIN k4.accounts c ON (r.a = 1 AND c.acct = 'A00001') \
             OR (r.a = 2 AND c.region = 'US') ORDER BY 1, 2",
            &["1|A00001", "1|A00001", "2|A00002", "2|A00004"],
        ),
        (
            "SELECT r.a, r.b, c.acct FROM k4.r r LEFT JOIN k4.accounts c ON r.v > 2 \
             AND (c.\"limit\" IS NULL OR c.acct = 'A00001') ORDER BY 1, 2, 3",
            &["1|x|NULL", "1|y|NULL", "2|x|A00001", "2|x|A00002", "2|x|A00003"],
        ),
        (
            "SELECT count(*) FROM k4.accounts a JOIN k4.accounts b ON a.acct = b.acct \
             OR a.region = b.region",
            &["8"],
        ),
        ("SELECT acct FROM k4.v ORDER BY acct", &["A00002", "A00004"]),
        (
            "SELECT acct FROM k4.accounts WHERE (acct = 'A00001' OR acct = 'A00002') \
             AND (region = 'EU' OR region IS NULL)",
            &["A00001"],
        ),
        (
            "UPDATE k4.accounts SET \"limit\" = 1.00 WHERE acct = 'A00002' OR acct = 'A00003' \
             RETURNING acct",
            &["A00002", "A00003"],
        ),
        (
            "DELETE FROM k4.r WHERE (a = 1 AND b = 'x') OR (a = 2 AND b = 'x') RETURNING a, b",
            &["1|x", "2|x"],
        ),
    ];
    let path = declared("or-ended", K4, &K4_ROWS);
    ended(&path, "or", asked);
    let path = declared("or", K4, &K4_ROWS);
    let mut w = Snail::open_writing(&path).unwrap();
    answered(&mut w, asked);
    assert_eq!(
        refused(&mut w, "SELECT acct FROM k4.accounts WHERE acct = 'A00001' OR 5"),
        (
            Some("42804"),
            "argument of OR must be type boolean, not type integer".into()
        )
    );
}

/// Two writers of four accounts, one block holding them all.
fn four_accounts(name: &str) -> (Snail, Snail, std::path::PathBuf) {
    let ddl = "CREATE SCHEMA s; CREATE TABLE s.accounts (acct varchar(6) NOT NULL, \
               balance numeric(9,2), PRIMARY KEY (acct));";
    let path = declared(
        name,
        ddl,
        &["INSERT INTO s.accounts VALUES ('A00001', 10.00), ('A00002', 20.00), \
           ('A00003', 30.00), ('A00004', 40.00)"],
    );
    (
        Snail::open_writing(&path).unwrap(),
        Snail::open_writing(&path).unwrap(),
        path,
    )
}

/// Each writer reads two of its own accounts by an OR of their keys, the other whole between, and
/// writes the first: both commit, as 18.6 commits them reading by its index.
fn each_its_own_by_an_or(
    name: &str,
    a_reads: &str,
    a_read: &[&str],
    b_reads: &str,
    b_read: &[&str],
) {
    let (mut a, mut b, path) = four_accounts(name);
    a.execute("BEGIN ISOLATION LEVEL SERIALIZABLE").unwrap();
    assert_eq!(rows(&mut a, a_reads), a_read);
    b.execute("BEGIN ISOLATION LEVEL SERIALIZABLE").unwrap();
    assert_eq!(rows(&mut b, b_reads), b_read);
    b.execute("UPDATE s.accounts SET balance = balance + 2.00 WHERE acct = 'A00002'")
        .unwrap();
    b.execute("COMMIT").unwrap();
    a.execute("UPDATE s.accounts SET balance = balance + 1.00 WHERE acct = 'A00001'")
        .unwrap();
    a.execute("COMMIT").unwrap();
    let mut c = Snail::open_writing(&path).unwrap();
    assert_eq!(
        rows(&mut c, "SELECT acct, balance FROM s.accounts ORDER BY acct"),
        ["A00001|11.00", "A00002|22.00", "A00003|30.00", "A00004|40.00"]
    );
}

#[test]
fn two_serializable_writers_each_reading_its_own_rows_by_an_or_of_keys_both_commit() {
    each_its_own_by_an_or(
        "own-by-or",
        "SELECT acct, balance FROM s.accounts WHERE acct = 'A00001' OR acct = 'A00003' ORDER BY acct",
        &["A00001|10.00", "A00003|30.00"],
        "SELECT acct, balance FROM s.accounts WHERE acct = 'A00004' OR acct = 'A00002' ORDER BY acct",
        &["A00002|20.00", "A00004|40.00"],
    );
}

#[test]
fn a_pipeline_s_read_by_an_or_of_keys_marks_only_those_keys() {
    each_its_own_by_an_or(
        "own-by-or-piped",
        "SELECT acct, balance + 1.00 FROM s.accounts WHERE acct = 'A00001' OR acct = 'A00003' \
         ORDER BY acct",
        &["A00001|11.00", "A00003|31.00"],
        "SELECT acct, balance + 2.00 FROM s.accounts WHERE acct = 'A00002' OR acct = 'A00004' \
         ORDER BY acct",
        &["A00002|22.00", "A00004|42.00"],
    );
}

#[test]
fn an_or_of_both_keys_read_by_each_writer_is_a_write_skew_refused() {
    let (mut a, mut b, path) = accounts("skew-by-or");
    a.execute("BEGIN ISOLATION LEVEL SERIALIZABLE").unwrap();
    b.execute("BEGIN ISOLATION LEVEL SERIALIZABLE").unwrap();
    assert_eq!(
        rows(
            &mut a,
            "SELECT acct, balance FROM s.accounts WHERE acct = 'A00001' OR acct = 'A00002' \
             ORDER BY acct"
        ),
        ["A00001|10.00", "A00002|20.00"]
    );
    assert_eq!(
        rows(
            &mut b,
            "SELECT acct, balance FROM s.accounts WHERE acct = 'A00002' OR acct = 'A00001' \
             ORDER BY acct"
        ),
        ["A00001|10.00", "A00002|20.00"]
    );
    a.execute("UPDATE s.accounts SET balance = 0 WHERE acct = 'A00002'")
        .unwrap();
    a.execute("COMMIT").unwrap();
    let refused = b
        .execute("UPDATE s.accounts SET balance = 0 WHERE acct = 'A00001'")
        .and_then(|_| b.execute("COMMIT"))
        .unwrap_err();
    assert_eq!(refused.sqlstate(), Some("40001"), "{refused}");
    let mut c = Snail::open_writing(&path).unwrap();
    assert_eq!(
        rows(&mut c, "SELECT acct, balance FROM s.accounts ORDER BY acct"),
        ["A00001|10.00", "A00002|0.00"]
    );
}

#[test]
fn an_insert_at_a_key_an_or_found_no_row_at_meets_its_gap() {
    // a reads two keys no row has, inside the block, by an OR; b moves its own row's key into that
    // gap and commits; a's INSERT lands in the gap b read: two edges, and a is refused, as 18.6
    // refuses it.
    let (mut a, mut b, path) = accounts("moved-by-or");
    a.execute("INSERT INTO s.accounts VALUES ('A00009', 90.00)")
        .unwrap();
    a.execute("BEGIN ISOLATION LEVEL SERIALIZABLE").unwrap();
    b.execute("BEGIN ISOLATION LEVEL SERIALIZABLE").unwrap();
    assert!(rows(
        &mut a,
        "SELECT balance FROM s.accounts WHERE acct = 'A00003' OR acct = 'A00007'"
    )
    .is_empty());
    assert_eq!(
        rows(&mut b, "SELECT balance FROM s.accounts WHERE acct = 'A00002'"),
        ["20.00"]
    );
    b.execute("UPDATE s.accounts SET acct = 'A00005' WHERE acct = 'A00002'")
        .unwrap();
    b.execute("COMMIT").unwrap();
    let refused = a
        .execute("INSERT INTO s.accounts VALUES ('A00004', 1.00)")
        .and_then(|_| a.execute("COMMIT"))
        .unwrap_err();
    assert_eq!(refused.sqlstate(), Some("40001"), "{refused}");
    let mut c = Snail::open_writing(&path).unwrap();
    assert_eq!(
        rows(&mut c, "SELECT acct, balance FROM s.accounts ORDER BY acct"),
        ["A00001|10.00", "A00005|20.00", "A00009|90.00"]
    );
}

const K5: &str = "CREATE SCHEMA k5;
     CREATE TABLE k5.r (a integer NOT NULL, b varchar(4) NOT NULL, v numeric(9,2), PRIMARY KEY (a, b));
     CREATE TABLE k5.u (id integer NOT NULL, seq integer, tag varchar(4), PRIMARY KEY (id));
     CREATE VIEW k5.v AS SELECT a, b FROM k5.r WHERE (a, b) IN ((1, 'y'), (2, 'x'));
     CREATE VIEW k5.w AS SELECT a, b, (a, b) = (1, 'x') AS one FROM k5.r
         WHERE (a, b) IS DISTINCT FROM (1, 'y');";

const K5_ROWS: [&str; 2] = [
    "INSERT INTO k5.r VALUES (1, 'x', 1.00), (1, 'y', 2.00), (2, 'x', 3.00)",
    "INSERT INTO k5.u VALUES (1, 1, 'p'), (2, 2, 'q'), (3, NULL, 'p')",
];

#[test]
fn a_row_value_in_a_list_of_rows_is_answered_as_postgresql_answers_it() {
    let asked: &[(&str, &[&str])] = &[
        (
            "SELECT a, b FROM k5.r WHERE (a, b) IN ((1, 'y'), (2, 'x')) ORDER BY a, b",
            &["1|y", "2|x"],
        ),
        (
            "SELECT a, b FROM k5.r WHERE (a, b) NOT IN ((1, 'y'), (2, 'x')) ORDER BY a, b",
            &["1|x"],
        ),
        (
            "SELECT a, b FROM k5.r WHERE (a, b) IN ((1, NULL), (2, 'x')) ORDER BY a, b",
            &["2|x"],
        ),
        ("SELECT a, b FROM k5.r WHERE (a, b) NOT IN ((1, NULL), (2, 'x'))", &[]),
        (
            "SELECT a, b FROM k5.r WHERE (a, b) NOT IN ((3, NULL)) ORDER BY a, b",
            &["1|x", "1|y", "2|x"],
        ),
        (
            "SELECT id FROM k5.u WHERE (seq, tag) IN ((1, 'p'), (NULL, 'p')) ORDER BY id",
            &["1"],
        ),
        (
            "SELECT a, b FROM k5.r WHERE (a, b) IN ((1, 'y'), NULL) ORDER BY a, b",
            &["1|y"],
        ),
        (
            "SELECT a, b, (a, b) IN ((1, 'y')) AS hit FROM k5.r ORDER BY a, b",
            &["1|x|f", "1|y|t", "2|x|f"],
        ),
        (
            "SELECT a, b FROM k5.r WHERE (a + 1, upper(b)) IN ((2, 'Y'), (3, 'X')) ORDER BY a, b",
            &["1|y", "2|x"],
        ),
        (
            "SELECT a, b FROM k5.r WHERE ROW(a, b) IN (ROW(1, 'y')) AND v > 1 ORDER BY a, b",
            &["1|y"],
        ),
        (
            "SELECT r.a, r.b, u.id FROM k5.r r JOIN k5.u u ON (r.a, r.b) IN ((u.seq, 'x'), \
             (u.seq, 'y')) ORDER BY 1, 2, 3",
            &["1|x|1", "1|y|1", "2|x|2"],
        ),
        ("SELECT a, b FROM k5.v ORDER BY a, b", &["1|y", "2|x"]),
        (
            "SELECT pg_get_viewdef('k5.v'::regclass)",
            &[" SELECT a,\n    b\n   FROM k5.r\n  WHERE (((a = 1) AND ((b)::text = 'y'::text)) OR \
               ((a = 2) AND ((b)::text = 'x'::text)));"],
        ),
        (
            "DELETE FROM k5.u WHERE (id, tag) IN ((1, 'p'), (3, NULL)) RETURNING id",
            &["1"],
        ),
        ("SELECT id FROM k5.u WHERE (seq, 1) = (2, 1) ORDER BY id", &["2"]),
        ("SELECT id FROM k5.u WHERE (seq, tag) <> (1, 'p') ORDER BY id", &["2"]),
        (
            "SELECT id, (seq, tag) = (NULL, 'p') AS x FROM k5.u ORDER BY id",
            &["2|f", "3|NULL"],
        ),
        ("SELECT a, b FROM k5.r WHERE (a, b) NOT IN ((1, 'y'), NULL)", &[]),
        (
            "SELECT a, b FROM k5.r WHERE (a, b) IN ((1, 'y'), (1, 'y')) ORDER BY a, b",
            &["1|y"],
        ),
        ("SELECT a, b, one FROM k5.w ORDER BY a, b", &["1|x|t", "2|x|f"]),
    ];
    let path = declared("rows-in-ended", K5, &K5_ROWS);
    ended(&path, "rows-in", asked);
    let path = declared("rows-in", K5, &K5_ROWS);
    let mut w = Snail::open_writing(&path).unwrap();
    answered(&mut w, asked);
    let widths = (
        Some("42601"),
        "unequal number of entries in row expressions".to_string(),
    );
    let varchar = (
        Some("42883"),
        "operator does not exist: character varying = integer".to_string(),
    );
    for (sql, want) in [
        ("SELECT a FROM k5.r WHERE (a, b) IN ((1, 'y', 3))", widths.clone()),
        ("SELECT a FROM k5.r WHERE (a, b) IN ((1, 2))", varchar.clone()),
        (
            "SELECT a FROM k5.r WHERE (a, b) IN ((1, 'y'), 2)",
            (Some("42883"), "operator does not exist: record = integer".into()),
        ),
        (
            "SELECT a FROM k5.r WHERE ROW() IN (ROW())",
            (Some("0A000"), "cannot compare rows of zero length".into()),
        ),
        ("SELECT a FROM k5.r WHERE (a, b) = (1, 'y', 3)", widths.clone()),
        (
            "SELECT a FROM k5.r WHERE (a, b) IN ((1, 'y'), 'x')",
            (
                Some("0A000"),
                "input of anonymous composite types is not implemented".into(),
            ),
        ),
        ("SELECT a FROM k5.r WHERE (a, b) IN ((1, 2), (1, 'y', 3))", varchar),
        ("SELECT a FROM k5.r WHERE (a, b) IN ((1, 'y', 3), (1, 2))", widths.clone()),
        ("SELECT a FROM k5.r WHERE false AND (a, b) IN ((1, 'y', 3))", widths.clone()),
        ("SELECT a FROM k5.r WHERE false AND (a, b) = (1, 'y', 3)", widths.clone()),
        ("DELETE FROM k5.r WHERE (a, b) IN ((1, 'y', 3))", widths),
    ] {
        assert_eq!(refused(&mut w, sql), want, "{sql}");
    }
}

#[test]
fn a_row_in_a_list_of_keys_marks_each_key_as_its_or_does() {
    // Each writer reads two rows of its own by a row IN a list of their keys of two columns, the
    // other whole between, and writes one of them by its key: 18.6 commits both, reading by its
    // index. The second's list holds a NULL, which keeps no row and reads nothing.
    let ddl = "CREATE SCHEMA s; CREATE TABLE s.r (a integer NOT NULL, b varchar(4) NOT NULL, \
               v numeric(9,2), PRIMARY KEY (a, b));";
    let path = declared(
        "rows-in-keys",
        ddl,
        &["INSERT INTO s.r VALUES (1, 'x', 1.00), (1, 'y', 2.00), (2, 'x', 3.00), (2, 'y', 4.00)"],
    );
    let (mut a, mut b) = (
        Snail::open_writing(&path).unwrap(),
        Snail::open_writing(&path).unwrap(),
    );
    a.execute("BEGIN ISOLATION LEVEL SERIALIZABLE").unwrap();
    assert_eq!(
        rows(
            &mut a,
            "SELECT a, b, v FROM s.r WHERE (a, b) IN ((1, 'x'), (2, 'y')) ORDER BY a, b"
        ),
        ["1|x|1.00", "2|y|4.00"]
    );
    b.execute("BEGIN ISOLATION LEVEL SERIALIZABLE").unwrap();
    assert_eq!(
        rows(
            &mut b,
            "SELECT a, b, v FROM s.r WHERE (a, b) IN ((2, 'x'), (1, 'y'), NULL) ORDER BY a, b"
        ),
        ["1|y|2.00", "2|x|3.00"]
    );
    b.execute("UPDATE s.r SET v = v + 2.00 WHERE (a, b) = (1, 'y')")
        .unwrap();
    b.execute("COMMIT").unwrap();
    a.execute("UPDATE s.r SET v = v + 1.00 WHERE (a, b) = (1, 'x')")
        .unwrap();
    a.execute("COMMIT").unwrap();
    let mut c = Snail::open_writing(&path).unwrap();
    assert_eq!(
        rows(&mut c, "SELECT a, b, v FROM s.r ORDER BY a, b"),
        ["1|x|2.00", "1|y|4.00", "2|x|3.00", "2|y|4.00"]
    );
}

const K6: &str = "CREATE SCHEMA k6;
     CREATE TABLE k6.stated (acct varchar(9) NOT NULL, balance numeric(12,2), PRIMARY KEY (acct));
     CREATE TABLE k6.tiers (lower numeric(12,2) NOT NULL, rate numeric(6,4) NOT NULL,
         PRIMARY KEY (lower));
     CREATE TABLE k6.owners (acct varchar(9) NOT NULL, owner varchar(8) NOT NULL, PRIMARY KEY (acct));
     CREATE TABLE k6.caps (owner varchar(8) NOT NULL, cap numeric(12,2), PRIMARY KEY (owner));
     CREATE VIEW k6.v AS SELECT s.acct, t.rate FROM k6.stated s JOIN k6.tiers t
         ON t.lower = (SELECT max(u.lower) FROM k6.tiers u WHERE u.lower <= s.balance);";

const K6_ROWS: [&str; 4] = [
    "INSERT INTO k6.stated VALUES ('ACT-A-001', 60.00), ('ACT-A-002', 9999.00), \
     ('ACT-B-001', 1.00), ('ACT-B-002', 50000.00), ('ACT-C-001', NULL), ('ACT-C-002', -5.00)",
    "INSERT INTO k6.tiers VALUES (0.00, 0.0050), (10000.00, 0.0150), (100000.00, 0.0200)",
    "INSERT INTO k6.owners VALUES ('ACT-A-001', 'ann'), ('ACT-A-002', 'ann'), ('ACT-B-001', 'bob'), \
     ('ACT-B-002', 'bob'), ('ACT-C-001', 'cy')",
    "INSERT INTO k6.caps VALUES ('ann', 5000.00), ('bob', NULL)",
];

/// The tier each account's balance reaches, by the greatest lower bound at or below it.
const TIER: &str = "(SELECT max(u.lower) FROM k6.tiers u WHERE u.lower <= s.balance)";

#[test]
fn a_subquery_in_a_join_s_on_is_answered_as_postgresql_answers_it() {
    let rated = ["ACT-A-001|0.0050", "ACT-A-002|0.0050", "ACT-B-001|0.0050", "ACT-B-002|0.0150"];
    let lowest = ["ACT-A-001|0.00", "ACT-A-002|0.00", "ACT-B-001|0.00", "ACT-B-002|10000.00"];
    let sql: Vec<String> = vec![
        format!(
            "SELECT s.acct, t.rate FROM k6.stated s JOIN k6.tiers t ON t.lower = {TIER} \
             ORDER BY s.acct"
        ),
        format!(
            "SELECT s.acct, t.rate FROM k6.stated s JOIN k6.tiers t ON {TIER} = t.lower \
             ORDER BY s.acct"
        ),
        format!(
            "SELECT s.acct, t.rate FROM k6.stated s LEFT JOIN k6.tiers t ON t.lower = {TIER} \
             ORDER BY s.acct"
        ),
        format!(
            "SELECT s.acct, t.lower FROM k6.stated s RIGHT JOIN k6.tiers t ON t.lower = {TIER} \
             ORDER BY t.lower, s.acct"
        ),
        format!(
            "SELECT s.acct, t.lower FROM k6.stated s FULL JOIN k6.tiers t ON s.balance > 100 \
             AND t.lower = {TIER} ORDER BY t.lower, s.acct"
        ),
        format!(
            "SELECT t.lower, s.acct FROM k6.tiers t LEFT JOIN k6.stated s ON t.lower = {TIER} \
             ORDER BY t.lower, s.acct"
        ),
        "SELECT s.acct, t.lower FROM k6.stated s JOIN k6.tiers t ON t.rate = (SELECT min(u.rate) \
         FROM k6.tiers u WHERE u.lower <= s.balance AND u.lower >= t.lower) ORDER BY s.acct, t.lower"
            .into(),
        "SELECT s.acct, t.lower FROM k6.stated s JOIN k6.tiers t ON t.lower <= s.balance \
         AND NOT EXISTS (SELECT 1 FROM k6.tiers u WHERE u.lower > t.lower AND u.lower <= s.balance) \
         ORDER BY s.acct"
            .into(),
        "SELECT s.acct, t.lower FROM k6.stated s LEFT JOIN k6.tiers t ON EXISTS (SELECT 1 \
         FROM k6.owners o WHERE o.acct = s.acct AND o.owner = 'ann') AND t.lower = 0 ORDER BY s.acct"
            .into(),
        "SELECT s.acct, t.lower FROM k6.stated s JOIN k6.tiers t ON t.lower IN (SELECT u.lower \
         FROM k6.tiers u WHERE u.lower <= s.balance ORDER BY u.lower DESC LIMIT 1) ORDER BY s.acct"
            .into(),
        "SELECT s.acct, t.lower FROM k6.stated s JOIN k6.tiers t ON t.lower = (SELECT min(lower) \
         FROM k6.tiers) WHERE s.balance IS NULL"
            .into(),
        "SELECT o.acct, c.cap FROM k6.owners o JOIN k6.caps c ON c.owner = o.owner \
         AND (SELECT s.balance FROM k6.stated s WHERE s.acct = o.acct) <= c.cap ORDER BY o.acct"
            .into(),
        "SELECT o.acct, c.owner FROM k6.owners o LEFT JOIN k6.caps c ON c.owner = o.owner \
         AND c.cap IS NOT DISTINCT FROM (SELECT max(c2.cap) FROM k6.caps c2 \
         WHERE c2.owner = o.owner) ORDER BY o.acct"
            .into(),
        format!(
            "SELECT o.owner, s.acct, t.rate FROM k6.owners o JOIN k6.stated s ON s.acct = o.acct \
             JOIN k6.tiers t ON t.lower = {TIER} ORDER BY s.acct"
        ),
        "SELECT o.owner, s.acct, c.cap FROM k6.owners o JOIN k6.stated s ON s.acct = o.acct \
         LEFT JOIN k6.caps c ON c.owner = o.owner AND c.cap >= (SELECT s2.balance \
         FROM k6.stated s2 WHERE s2.acct = s.acct) ORDER BY s.acct"
            .into(),
        "SELECT o.acct, (SELECT t.lower FROM k6.stated s JOIN k6.tiers t ON s.acct = o.acct \
         AND t.lower = (SELECT max(u.lower) FROM k6.tiers u WHERE u.lower <= s.balance \
         AND o.owner <> 'bob')) AS lower FROM k6.owners o ORDER BY o.acct"
            .into(),
        format!(
            "SELECT s.acct, t.lower FROM k6.stated s JOIN k6.tiers t ON t.lower = {TIER} \
             OR (s.balance IS NULL AND t.lower = 0) ORDER BY s.acct"
        ),
        "SELECT acct, rate FROM k6.v ORDER BY acct".into(),
        "SELECT pg_get_viewdef('k6.v'::regclass)".into(),
        format!(
            "DELETE FROM k6.owners o USING k6.stated s JOIN k6.tiers t ON t.lower = {TIER} \
             WHERE o.acct = s.acct AND t.rate > 0.01 RETURNING o.acct"
        ),
    ];
    let want: [&[&str]; 20] = [
        &rated,
        &rated,
        &[
            rated[0], rated[1], rated[2], rated[3], "ACT-C-001|NULL", "ACT-C-002|NULL",
        ],
        &[lowest[0], lowest[1], lowest[2], lowest[3], "NULL|100000.00"],
        &[
            "ACT-A-002|0.00",
            "ACT-B-002|10000.00",
            "NULL|100000.00",
            "ACT-A-001|NULL",
            "ACT-B-001|NULL",
            "ACT-C-001|NULL",
            "ACT-C-002|NULL",
        ],
        &[
            "0.00|ACT-A-001",
            "0.00|ACT-A-002",
            "0.00|ACT-B-001",
            "10000.00|ACT-B-002",
            "100000.00|NULL",
        ],
        &[lowest[0], lowest[1], lowest[2], "ACT-B-002|0.00", lowest[3]],
        &lowest,
        &[
            "ACT-A-001|0.00",
            "ACT-A-002|0.00",
            "ACT-B-001|NULL",
            "ACT-B-002|NULL",
            "ACT-C-001|NULL",
            "ACT-C-002|NULL",
        ],
        &lowest,
        &["ACT-C-001|0.00"],
        &["ACT-A-001|5000.00"],
        &[
            "ACT-A-001|ann",
            "ACT-A-002|ann",
            "ACT-B-001|bob",
            "ACT-B-002|bob",
            "ACT-C-001|NULL",
        ],
        &[
            "ann|ACT-A-001|0.0050",
            "ann|ACT-A-002|0.0050",
            "bob|ACT-B-001|0.0050",
            "bob|ACT-B-002|0.0150",
        ],
        &[
            "ann|ACT-A-001|5000.00",
            "ann|ACT-A-002|NULL",
            "bob|ACT-B-001|NULL",
            "bob|ACT-B-002|NULL",
            "cy|ACT-C-001|NULL",
        ],
        &[
            "ACT-A-001|0.00",
            "ACT-A-002|0.00",
            "ACT-B-001|NULL",
            "ACT-B-002|NULL",
            "ACT-C-001|NULL",
        ],
        &[lowest[0], lowest[1], lowest[2], lowest[3], "ACT-C-001|0.00"],
        &rated,
        &[" SELECT s.acct,\n    t.rate\n   FROM (k6.stated s\n     JOIN k6.tiers t ON ((t.lower = \
            ( SELECT max(u.lower) AS max\n           FROM k6.tiers u\n          WHERE \
            (u.lower <= s.balance)))));"],
        &["ACT-B-002"],
    ];
    let asked: Vec<(&str, &[&str])> = sql.iter().map(String::as_str).zip(want).collect();
    let path = declared("on-subquery-ended", K6, &K6_ROWS);
    ended(&path, "on-subquery", &asked);
    let path = declared("on-subquery", K6, &K6_ROWS);
    let mut w = Snail::open_writing(&path).unwrap();
    answered(&mut w, &asked);
}

#[test]
fn a_full_join_on_a_subquery_of_one_side_is_answered_and_any_other_refused() {
    let met = [
        "ACT-A-001|0.00",
        "ACT-A-002|0.00",
        "ACT-B-001|0.00",
        "ACT-B-002|10000.00",
        "NULL|100000.00",
        "ACT-C-001|NULL",
        "ACT-C-002|NULL",
    ];
    let sql = [
        format!(
            "SELECT s.acct, t.lower FROM k6.stated s FULL JOIN k6.tiers t ON s.balance > 100 \
             AND t.lower = {TIER} ORDER BY t.lower, s.acct"
        ),
        format!(
            "SELECT s.acct, t.lower FROM k6.stated s FULL JOIN k6.tiers t ON t.lower = {TIER} \
             ORDER BY t.lower, s.acct"
        ),
        format!(
            "SELECT s.acct, t.lower FROM k6.stated s FULL JOIN k6.tiers t ON {TIER} = t.lower \
             ORDER BY t.lower, s.acct"
        ),
    ];
    let want: [&[&str]; 3] = [
        &[
            "ACT-A-002|0.00",
            "ACT-B-002|10000.00",
            "NULL|100000.00",
            "ACT-A-001|NULL",
            "ACT-B-001|NULL",
            "ACT-C-001|NULL",
            "ACT-C-002|NULL",
        ],
        &met,
        &met,
    ];
    let asked: Vec<(&str, &[&str])> = sql.iter().map(String::as_str).zip(want).collect();
    let path = declared("full-on-subquery", K6, &K6_ROWS);
    let mut w = Snail::open_writing(&path).unwrap();
    answered(&mut w, &asked);
    let unmet = (
        Some("0A000"),
        "FULL JOIN is only supported with merge-joinable or hash-joinable join conditions".into(),
    );
    for sql in [
        "SELECT s.acct FROM k6.stated s FULL JOIN k6.tiers t ON t.lower <= s.balance \
         AND NOT EXISTS (SELECT 1 FROM k6.tiers u WHERE u.lower > t.lower)",
        "SELECT s.acct FROM k6.stated s FULL JOIN k6.tiers t ON t.lower = (SELECT max(u.lower) \
         FROM k6.tiers u WHERE u.lower <= s.balance AND u.rate <= t.rate)",
        "SELECT s.acct FROM k6.stated s FULL JOIN k6.tiers t ON t.lower = (SELECT min(lower) \
         FROM k6.tiers)",
    ] {
        assert_eq!(refused(&mut w, sql), unmet, "{sql}");
    }
}

#[test]
fn a_subquery_in_an_on_is_refused_in_postgresql_s_words() {
    let path = declared("on-subquery-refused", K6, &K6_ROWS);
    let mut w = Snail::open_writing(&path).unwrap();
    for (sql, want) in [
        (
            "SELECT s.acct FROM k6.stated s JOIN k6.tiers t ON t.lower = (SELECT u.lower \
             FROM k6.tiers u WHERE u.lower <= s.balance)",
            (
                Some("21000"),
                "more than one row returned by a subquery used as an expression",
            ),
        ),
        (
            "SELECT s.acct FROM k6.stated s JOIN k6.tiers t ON t.lower = (SELECT max(s.balance))",
            (Some("42803"), "aggregate functions are not allowed in JOIN conditions"),
        ),
        (
            "SELECT s.acct FROM k6.owners o, k6.stated s JOIN k6.tiers t ON t.lower = \
             (SELECT max(u.lower) FROM k6.tiers u WHERE o.acct = s.acct)",
            (
                Some("42P01"),
                "invalid reference to FROM-clause entry for table \"o\": There is an entry for \
                 table \"o\", but it cannot be referenced from this part of the query.",
            ),
        ),
        (
            "SELECT s.acct FROM k6.stated s JOIN k6.tiers t ON t.lower = (SELECT max(u.lower) \
             FROM k6.tiers u WHERE o.acct = s.acct) JOIN k6.owners o ON o.acct = s.acct",
            (Some("42P01"), "missing FROM-clause entry for table \"o\""),
        ),
        (
            "SELECT s.acct FROM k6.stated s JOIN k6.tiers t ON t.lower = (SELECT u.lower, u.rate \
             FROM k6.tiers u LIMIT 1)",
            (Some("42601"), "subquery must return only one column"),
        ),
    ] {
        assert_eq!(refused(&mut w, sql), (want.0, want.1.to_string()), "{sql}");
    }
}

#[test]
fn an_aggregate_of_the_query_around_is_refused_in_its_clause_s_words() {
    // An aggregate reading only the query around a subquery is that query's: refused, as 18.6
    // refused each on 28818 (2026-11-11), in the words of the clause of that query the subquery
    // stands in, wherever in the subquery it stands.
    let path = declared("around-aggregate-refused", K6, &K6_ROWS);
    let mut w = Snail::open_writing(&path).unwrap();
    let not_in = |clause: &str| {
        (
            Some("42803"),
            format!("aggregate functions are not allowed in {clause}"),
        )
    };
    for (sql, want) in [
        (
            "SELECT s.acct FROM k6.stated s WHERE EXISTS (SELECT 1 FROM k6.tiers t \
             WHERE t.lower < max(s.balance))",
            not_in("WHERE"),
        ),
        (
            "SELECT s.acct FROM k6.stated s WHERE s.balance IN (SELECT max(s.balance) \
             FROM k6.tiers t)",
            not_in("WHERE"),
        ),
        (
            "SELECT s.acct FROM k6.stated s WHERE s.balance IN (SELECT max(s.balance))",
            not_in("WHERE"),
        ),
        (
            "SELECT s.acct FROM k6.stated s WHERE s.balance > (SELECT min(t.lower) \
             FROM k6.tiers t WHERE t.lower < max(s.balance))",
            not_in("WHERE"),
        ),
        (
            "SELECT s.acct FROM k6.stated s WHERE s.balance = ANY (SELECT max(s.balance) \
             FROM k6.tiers t)",
            not_in("WHERE"),
        ),
        (
            "SELECT s.acct FROM k6.stated s JOIN k6.tiers t ON t.lower = (SELECT max(s.balance))",
            not_in("JOIN conditions"),
        ),
        (
            "SELECT count(*) FROM k6.stated s GROUP BY (SELECT max(s.balance))",
            not_in("GROUP BY"),
        ),
    ] {
        assert_eq!(refused(&mut w, sql), want, "{sql}");
    }
}

#[test]
fn a_call_of_no_function_and_a_later_arm_s_name_are_refused_in_18_6_s_words() {
    // As 18.6 refused and answered each on 28818 (2026-11-12).
    let path = declared(
        "no-function",
        "CREATE SCHEMA o;
         CREATE TABLE o.t (k integer NOT NULL, b text, PRIMARY KEY (k));
         CREATE TABLE o.ls (acct text NOT NULL, seq integer, PRIMARY KEY (acct));",
        &["INSERT INTO o.ls VALUES ('x', 1), ('y', 2)"],
    );
    let mut w = Snail::open_writing(&path).unwrap();
    for (sql, call) in [
        ("SELECT nosuch(1)", "nosuch(integer)"),
        ("SELECT nosuch('a')", "nosuch(unknown)"),
        ("SELECT nosuch()", "nosuch()"),
        ("SELECT NoSuch(1, 2.5)", "nosuch(integer, numeric)"),
        ("SELECT k FROM o.t WHERE nosuch(k) > 0", "nosuch(integer)"),
        ("SELECT nosuch(b) FROM o.t", "nosuch(text)"),
    ] {
        let words = format!(
            "function {call} does not exist\nHINT:  No function matches the given name and \
             argument types. You might need to add explicit type casts."
        );
        assert_eq!(refused(&mut w, sql), said("42883", &words), "{sql}");
    }
    let grammar = "SELECT coalesce(1, 2), nullif(1, 2), greatest(1, 2), least(1, 2)";
    assert_eq!(rows(&mut w, grammar), ["1|1|2|1"]);
    // A set operation's ORDER BY naming a column of a later arm alone.
    let one = |k: usize| {
        format!(
            "column \"acct\" does not exist\nDETAIL:  There is a column named \"acct\" in table \
             \"*SELECT* {k}\", but it cannot be referenced from this part of the query."
        )
    };
    let many =
        "column \"acct\" does not exist\nDETAIL:  There are columns named \"acct\", but they \
         are in tables that cannot be referenced from this part of the query.\nHINT:  Try using a \
         table-qualified name.";
    for (sql, words) in [
        (
            "SELECT acct AS a FROM o.ls UNION SELECT acct FROM o.ls ORDER BY acct",
            one(2),
        ),
        (
            "SELECT acct AS a FROM o.ls UNION SELECT acct AS b FROM o.ls UNION SELECT acct \
             FROM o.ls ORDER BY acct",
            one(3),
        ),
        (
            "SELECT acct AS a FROM o.ls UNION SELECT acct FROM o.ls UNION SELECT acct FROM o.ls \
             ORDER BY acct",
            many.to_string(),
        ),
        (
            "SELECT acct AS a FROM o.ls UNION (SELECT acct AS b FROM o.ls INTERSECT SELECT acct \
             FROM o.ls) ORDER BY acct",
            one(3),
        ),
        (
            "SELECT acct AS a FROM o.ls UNION SELECT acct FROM o.ls ORDER BY seq",
            "column \"seq\" does not exist".to_string(),
        ),
    ] {
        assert_eq!(refused(&mut w, sql), said("42703", &words), "{sql}");
    }
    let ordered = "SELECT acct AS a FROM o.ls UNION SELECT acct FROM o.ls ORDER BY a DESC";
    assert_eq!(rows(&mut w, ordered), ["y", "x"]);
}

#[test]
fn any_in_a_select_list_and_rows_by_order_are_answered() {
    // As 18.6 answered each on 28818 (2026-11-12): ANY and ALL over an array in a select list; two
    // rows compared by order pair by pair, the first pair not equal deciding, NULL where a pair
    // read before one decides holds one; BETWEEN of rows as its two comparisons.
    let path = declared(
        "rows-by-order",
        "CREATE SCHEMA k11;
         CREATE TABLE k11.src (k integer NOT NULL, v integer, s text, PRIMARY KEY (k));",
        &["INSERT INTO k11.src VALUES (1, 10, 'a'), (2, NULL, 'b'), (3, 30, 'c'), (4, 5, NULL)"],
    );
    let mut w = Snail::open_writing(&path).unwrap();
    answered(
        &mut w,
        &[
            (
                "SELECT k, k = ANY (ARRAY[1, 3]) FROM k11.src ORDER BY k",
                &["1|t", "2|f", "3|t", "4|f"],
            ),
            (
                "SELECT k > ALL (ARRAY[1, 2]), k = ANY ('{}'::integer[]) FROM k11.src ORDER BY k",
                &["f|f", "f|f", "t|f", "t|f"],
            ),
            (
                "SELECT k, k = ANY (ARRAY[1, NULL]), k <> ALL (ARRAY[2, 3]), v > ANY (ARRAY[6, 20]) \
                 FROM k11.src ORDER BY k",
                &["1|t|t|t", "2|NULL|f|NULL", "3|NULL|f|t", "4|NULL|t|f"],
            ),
            (
                "SELECT (1, 2) < (1, 3), ROW(1, 2) BETWEEN ROW(0, 0) AND ROW(2, 2), \
                 (1, NULL) < (2, 0), (1, NULL) < (1, 0), (1, 2) >= (1, 2)",
                &["t|t|t|NULL|t"],
            ),
            (
                "SELECT (NULL, 1) < (2, 0), (2, NULL) > (1, 5), \
                 ROW(1, NULL) BETWEEN ROW(0, 0) AND ROW(2, 2), (1, 3) <= (1, 2)",
                &["NULL|t|t|f"],
            ),
            ("SELECT k FROM k11.src WHERE (k, 1) < (3, 0) ORDER BY k", &["1", "2"]),
            (
                "SELECT k, (k, v) > (2, 0), (v, k) <= (10, 1) FROM k11.src ORDER BY k",
                &["1|f|t", "2|NULL|NULL", "3|t|f", "4|t|t"],
            ),
            (
                "SELECT k FROM k11.src WHERE (k, v) BETWEEN (1, 0) AND (3, 20) ORDER BY k",
                &["1", "2"],
            ),
            ("SELECT ROW(1, 2) NOT BETWEEN ROW(0, 0) AND ROW(0, 5)", &["t"]),
            (
                "SELECT (1, 'a') < (1, 'b'), ROW(1, 2) BETWEEN SYMMETRIC ROW(2, 2) AND ROW(0, 0), \
                 (s, k) > ('a', 9) FROM k11.src ORDER BY k",
                &["t|t|f", "t|t|t", "t|t|t", "t|t|NULL"],
            ),
            (
                "SELECT (1, 2.5) < (1, 3), ('2026-01-01'::date, 1) < ('2026-01-02'::date, 0)",
                &["t|t"],
            ),
            (
                "SELECT 2 BETWEEN SYMMETRIC 3 AND 1, (1, 2) BETWEEN SYMMETRIC (2, 2) AND (0, 0), \
                 (1, 2) NOT BETWEEN SYMMETRIC (2, 2) AND (1, 3)",
                &["t|t|t"],
            ),
        ],
    );
    for sql in [
        "SELECT (1, 2) < (1, 2, 3)",
        "SELECT ROW(1, 2) BETWEEN ROW(0, 0) AND ROW(1)",
    ] {
        assert_eq!(
            refused(&mut w, sql),
            said("42601", "unequal number of entries in row expressions"),
            "{sql}"
        );
    }
}

#[test]
fn a_join_s_on_inside_a_subquery_reads_the_query_around_it() {
    let asked: &[(&str, &[&str])] = &[
        (
            "SELECT o.acct, (SELECT t.lower FROM k6.stated s JOIN k6.tiers t ON s.acct = o.acct \
             AND t.lower = (SELECT max(u.lower) FROM k6.tiers u WHERE u.lower <= s.balance \
             AND o.owner <> 'bob')) AS lower FROM k6.owners o ORDER BY o.acct",
            &[
                "ACT-A-001|0.00",
                "ACT-A-002|0.00",
                "ACT-B-001|NULL",
                "ACT-B-002|NULL",
                "ACT-C-001|NULL",
            ],
        ),
        (
            "SELECT o.acct, (SELECT t.lower FROM k6.stated s JOIN k6.tiers t ON s.acct = o.acct \
             AND t.lower = 0) AS lower FROM k6.owners o ORDER BY o.acct",
            &[
                "ACT-A-001|0.00",
                "ACT-A-002|0.00",
                "ACT-B-001|0.00",
                "ACT-B-002|0.00",
                "ACT-C-001|0.00",
            ],
        ),
    ];
    let path = declared("on-around", K6, &K6_ROWS);
    let mut w = Snail::open_writing(&path).unwrap();
    answered(&mut w, asked);
}

#[test]
fn a_full_join_s_on_inside_a_subquery_meets_by_an_equality_holding_the_query_around_it() {
    let counted: &[&str] = &[
        "ACT-A-001|9",
        "ACT-A-002|9",
        "ACT-B-001|9",
        "ACT-B-002|9",
        "ACT-C-001|8",
    ];
    let asked: &[(&str, &[&str])] = &[
        (
            "SELECT o.acct, (SELECT count(*) FROM k6.stated s FULL JOIN k6.tiers t \
             ON s.balance = t.lower + length(o.owner) - 3) AS n FROM k6.owners o ORDER BY o.acct",
            counted,
        ),
        (
            "SELECT o.acct, (SELECT count(*) FROM k6.stated s FULL JOIN k6.tiers t \
             ON s.balance + length(o.owner) = t.lower + 3) AS n FROM k6.owners o ORDER BY o.acct",
            counted,
        ),
        (
            "SELECT o.acct, (SELECT count(*) FROM k6.stated s FULL JOIN k6.tiers t \
             ON s.balance = t.lower AND t.rate > length(o.owner) / 1000.0) AS n FROM k6.owners o \
             ORDER BY o.acct",
            &[
                "ACT-A-001|9",
                "ACT-A-002|9",
                "ACT-B-001|9",
                "ACT-B-002|9",
                "ACT-C-001|9",
            ],
        ),
    ];
    let path = declared("full-on-around", K6, &K6_ROWS);
    let mut w = Snail::open_writing(&path).unwrap();
    answered(&mut w, asked);
    let unmet = (
        Some("0A000"),
        "FULL JOIN is only supported with merge-joinable or hash-joinable join conditions".into(),
    );
    for sql in [
        "SELECT o.acct, (SELECT count(*) FROM k6.stated s FULL JOIN k6.tiers t \
         ON s.balance = length(o.owner) - 3) AS n FROM k6.owners o",
        "SELECT o.acct, (SELECT count(*) FROM k6.stated s FULL JOIN k6.tiers t \
         ON length(o.owner) - 3 = t.lower) AS n FROM k6.owners o",
    ] {
        assert_eq!(refused(&mut w, sql), unmet, "{sql}");
    }
}

const K8: &str = "CREATE SCHEMA k8;
     CREATE TABLE k8.accounts (account varchar(10) NOT NULL, bank varchar(8) NOT NULL,
         PRIMARY KEY (account));
     CREATE TABLE k8.tr (request varchar(8) NOT NULL, from_account varchar(10) NOT NULL,
         to_account varchar(10) NOT NULL, PRIMARY KEY (request));
     CREATE TABLE k8.closed (account varchar(10) NOT NULL, PRIMARY KEY (account));
     CREATE VIEW k8.apart AS SELECT t.request FROM k8.tr t WHERE EXISTS (SELECT 1
         FROM k8.accounts s JOIN k8.accounts r ON r.account = t.to_account
         WHERE s.account = t.from_account AND s.bank <> r.bank);";

const K8_ROWS: [&str; 2] = [
    "INSERT INTO k8.accounts VALUES ('A1', 'BA'), ('A2', 'BA'), ('C1', 'BC')",
    "INSERT INTO k8.tr VALUES ('T1', 'A1', 'A2'), ('T2', 'A1', 'C1'), ('T3', 'A1', 'X9')",
];

/// warren-sling's ask 10: whether a transfer's two accounts are at different banks, the transfer's
/// own column read in a join's ON in an EXISTS.
const APART: &str = "SELECT t.request FROM k8.tr t WHERE EXISTS (SELECT 1 FROM k8.accounts s \
     JOIN k8.accounts r ON r.account = t.to_account WHERE s.account = t.from_account \
     AND s.bank <> r.bank) ORDER BY 1";

#[test]
fn an_outer_column_in_a_join_s_on_inside_a_subquery_is_answered_as_postgresql_answers_it() {
    let three = |n: &'static str| -> Vec<String> {
        ["T1", "T2", "T3"].iter().map(|r| format!("{r}|{n}")).collect()
    };
    let (n3, n0) = (three("3"), three("0"));
    let n3: Vec<&str> = n3.iter().map(String::as_str).collect();
    let n0: Vec<&str> = n0.iter().map(String::as_str).collect();
    let banks = ["T1|BA", "T2|BC", "T3|NULL"];
    let viewdef = concat!(
        " SELECT request\n",
        "   FROM k8.tr t\n",
        "  WHERE (EXISTS ( SELECT 1\n",
        "           FROM (k8.accounts s\n",
        "             JOIN k8.accounts r ON (((r.account)::text = (t.to_account)::text)))\n",
        "          WHERE (((s.account)::text = (t.from_account)::text) AND ",
        "((s.bank)::text <> (r.bank)::text))));"
    );
    let asked: &[(&str, &[&str])] = &[
        (APART, &["T2"]),
        (
            "SELECT t.request FROM k8.tr t WHERE EXISTS (SELECT 1 FROM k8.accounts s \
             JOIN k8.accounts r ON r.bank <> s.bank AND r.account = t.to_account \
             WHERE s.account = t.from_account) ORDER BY 1",
            &["T2"],
        ),
        (
            "SELECT t.request FROM k8.tr t WHERE EXISTS (SELECT 1 FROM k8.accounts s \
             JOIN k8.accounts r ON r.account = to_account WHERE s.account = from_account \
             AND s.bank <> r.bank) ORDER BY 1",
            &["T2"],
        ),
        (
            "SELECT t.request FROM k8.tr t WHERE t.request IN (SELECT t.request \
             FROM k8.accounts s JOIN k8.accounts r ON r.account = t.to_account \
             WHERE s.account = t.from_account AND s.bank = r.bank) ORDER BY 1",
            &["T1"],
        ),
        (
            "SELECT t.request FROM k8.tr t WHERE t.to_account = ANY (SELECT r.account \
             FROM k8.accounts s JOIN k8.accounts r ON r.bank = s.bank \
             AND s.account = t.from_account) ORDER BY 1",
            &["T1"],
        ),
        (
            "SELECT t.request FROM k8.tr t WHERE t.to_account NOT IN (SELECT r.account \
             FROM k8.accounts s JOIN k8.accounts r ON r.bank = s.bank \
             AND s.account = t.from_account) ORDER BY 1",
            &["T2", "T3"],
        ),
        (
            "SELECT t.request FROM k8.tr t WHERE NOT EXISTS (SELECT 1 FROM k8.accounts s \
             LEFT JOIN k8.accounts r ON r.account = t.to_account WHERE s.account = t.from_account \
             AND r.account IS NULL) ORDER BY 1",
            &["T1", "T2"],
        ),
        (
            "SELECT t.request FROM k8.tr t WHERE NOT EXISTS (SELECT 1 FROM k8.accounts s \
             JOIN k8.accounts r ON t.to_account = 'X9' WHERE s.account = t.from_account) ORDER BY 1",
            &["T1", "T2"],
        ),
        (
            "SELECT t.request, (SELECT r.bank FROM k8.accounts s JOIN k8.accounts r \
             ON r.account = t.to_account WHERE s.account = t.from_account) AS bank \
             FROM k8.tr t ORDER BY 1",
            &banks,
        ),
        (
            "SELECT t.request, x.bank FROM k8.tr t, LATERAL (SELECT r.bank FROM k8.accounts s \
             JOIN k8.accounts r ON r.account = t.to_account WHERE s.account = t.from_account) x \
             ORDER BY 1",
            &banks[..2],
        ),
        (
            "SELECT t.request, x.bank FROM k8.tr t LEFT JOIN LATERAL (SELECT r.bank \
             FROM k8.accounts s JOIN k8.accounts r ON r.account = t.to_account \
             WHERE s.account = t.from_account) x ON true ORDER BY 1",
            &banks,
        ),
        (
            "SELECT t.request, (SELECT count(*) FROM k8.accounts s LEFT JOIN k8.accounts r \
             ON r.account = t.to_account) AS n FROM k8.tr t ORDER BY 1",
            n3.as_slice(),
        ),
        (
            "SELECT t.request, (SELECT count(r.account) FROM k8.accounts s RIGHT JOIN \
             k8.accounts r ON s.account = t.to_account) AS n FROM k8.tr t ORDER BY 1",
            n3.as_slice(),
        ),
        (
            "SELECT t.request FROM k8.tr t WHERE EXISTS (SELECT 1 FROM k8.accounts s \
             RIGHT JOIN k8.accounts r ON r.account = t.to_account AND s.account = t.from_account \
             WHERE s.bank <> r.bank) ORDER BY 1",
            &["T2"],
        ),
        (
            "SELECT t.request, (SELECT count(*) FROM k8.accounts s FULL JOIN k8.accounts r \
             ON s.account = r.account AND r.account = t.to_account) AS n FROM k8.tr t ORDER BY 1",
            &["T1|5", "T2|5", "T3|6"],
        ),
        (
            "SELECT t.request, (SELECT count(*) FROM k8.accounts s FULL JOIN k8.accounts r \
             ON s.bank = r.bank || t.request) AS n FROM k8.tr t ORDER BY 1",
            &["T1|6", "T2|6", "T3|6"],
        ),
        (
            "SELECT t.request FROM k8.tr t WHERE EXISTS (SELECT 1 FROM k8.accounts a \
             WHERE a.account = t.from_account AND EXISTS (SELECT 1 FROM k8.accounts s \
             JOIN k8.accounts r ON r.account = t.to_account AND s.account = a.account \
             WHERE s.bank = r.bank)) ORDER BY 1",
            &["T1"],
        ),
        (
            "SELECT t.request, (SELECT (SELECT count(*) FROM k8.accounts s LEFT JOIN k8.accounts r \
             ON r.account = t.to_account AND r.bank = a.bank) FROM k8.accounts a \
             WHERE a.account = t.from_account) AS n FROM k8.tr t ORDER BY 1",
            n3.as_slice(),
        ),
        (
            "SELECT t.request FROM k8.tr t WHERE EXISTS (SELECT 1 FROM k8.accounts s \
             JOIN k8.accounts r ON s.bank <> r.bank AND (r.account = t.to_account \
             OR r.account = t.from_account) WHERE s.account = 'C1') ORDER BY 1",
            &["T1", "T2", "T3"],
        ),
        (
            "SELECT t.request, (SELECT count(*) FROM k8.accounts s JOIN k8.accounts r \
             ON r.bank = s.bank JOIN k8.tr u ON u.to_account = r.account \
             AND u.request <> t.request) AS n FROM k8.tr t ORDER BY 1",
            &["T1|1", "T2|2", "T3|3"],
        ),
        (
            "SELECT t.request, a.bank FROM k8.tr t JOIN k8.accounts a ON EXISTS (SELECT 1 \
             FROM k8.accounts s JOIN k8.accounts r ON r.account = t.to_account \
             AND s.account = a.account WHERE s.bank = r.bank) ORDER BY 1, 2",
            &["T1|BA", "T1|BA", "T2|BC"],
        ),
        (
            "SELECT t.request, (SELECT count(*) FROM k8.closed c RIGHT JOIN k8.accounts s \
             ON c.account = t.to_account) AS n FROM k8.tr t ORDER BY 1",
            n3.as_slice(),
        ),
        (
            "SELECT t.request, (SELECT count(*) FROM k8.closed c JOIN k8.accounts s \
             ON c.account = t.to_account) AS n FROM k8.tr t ORDER BY 1",
            n0.as_slice(),
        ),
        (
            "SELECT a.account, (SELECT count(*) FROM k8.accounts s JOIN k8.accounts r \
             ON r.account = t.to_account) AS n FROM k8.accounts a LEFT JOIN k8.tr t \
             ON t.to_account = a.account ORDER BY 1",
            &["A1|0", "A2|3", "C1|3"],
        ),
        ("SELECT request FROM k8.apart ORDER BY 1", &["T2"]),
        ("SELECT pg_get_viewdef('k8.apart'::regclass)", &[viewdef]),
        (
            "UPDATE k8.tr t SET to_account = 'A2' WHERE EXISTS (SELECT 1 FROM k8.accounts s \
             JOIN k8.accounts r ON r.account = t.to_account WHERE s.account = t.from_account \
             AND s.bank <> r.bank) RETURNING request, to_account",
            &["T2|A2"],
        ),
        (
            "DELETE FROM k8.tr t WHERE NOT EXISTS (SELECT 1 FROM k8.accounts s \
             JOIN k8.accounts r ON r.account = t.to_account WHERE s.account = t.from_account) \
             RETURNING request",
            &["T3"],
        ),
    ];
    let path = declared("on-outer-ended", K8, &K8_ROWS);
    ended(&path, "on-outer", asked);
    let path = declared("on-outer", K8, &K8_ROWS);
    let mut w = Snail::open_writing(&path).unwrap();
    answered(&mut w, asked);
}

/// PostgreSQL's words for a subquery in FROM reading a relation laid before it, `relation` by the
/// name it was asked by, `entry` the one found, with its HINT where LATERAL would have let it.
fn not_lateral(relation: &str, entry: &str, hint: bool) -> (Option<&'static str>, String) {
    let words = format!(
        "invalid reference to FROM-clause entry for table \"{relation}\": There is an entry for \
         table \"{entry}\", but it cannot be referenced from this part of the query."
    );
    match hint {
        true => said(
            "42P01",
            &format!(
                "{words}\nHINT:  To reference that table, you must mark this subquery with LATERAL."
            ),
        ),
        false => said("42P01", &words),
    }
}

#[test]
fn a_subquery_in_from_reading_a_relation_laid_before_it_is_refused_in_postgresql_s_words() {
    let path = declared("from-before", K8, &K8_ROWS);
    let mut w = Snail::open_writing(&path).unwrap();
    let column = "column \"to_account\" does not exist: There is a column named \"to_account\" \
         in table \"t\", but it cannot be referenced from this part of the query.";
    for (sql, want) in [
        (
            "SELECT t.request, x.bank FROM k8.tr t, (SELECT r.bank FROM k8.accounts s \
             JOIN k8.accounts r ON r.account = t.to_account WHERE s.account = t.from_account) x",
            not_lateral("t", "t", true),
        ),
        (
            "SELECT t.request, x.n FROM k8.tr t JOIN (SELECT count(*) AS n FROM k8.accounts s \
             JOIN k8.accounts r ON r.account = t.to_account) x ON true",
            not_lateral("t", "t", true),
        ),
        (
            "SELECT t.request, x.n FROM k8.tr t RIGHT JOIN (SELECT count(*) AS n FROM k8.accounts s \
             JOIN k8.accounts r ON r.account = t.to_account) x ON true",
            not_lateral("t", "t", false),
        ),
        (
            "SELECT t.request, x.n FROM k8.tr t FULL JOIN (k8.accounts a JOIN (SELECT count(*) AS n \
             FROM k8.accounts s JOIN k8.accounts r ON r.account = t.to_account) x ON true) ON true",
            not_lateral("t", "t", false),
        ),
        (
            "SELECT t.request, x.n FROM k8.tr t FULL JOIN (k8.accounts a JOIN (SELECT count(*) AS n \
             FROM k8.accounts s JOIN k8.accounts r ON r.account = a.account) x ON true) ON true",
            not_lateral("a", "a", true),
        ),
        (
            "SELECT t.request, x.n FROM k8.tr t RIGHT JOIN k8.accounts a ON true, (SELECT count(*) \
             AS n FROM k8.accounts s JOIN k8.accounts r ON r.account = t.to_account) x",
            not_lateral("t", "t", true),
        ),
        (
            "SELECT t.request, x.n FROM k8.tr t, (SELECT d.n FROM (SELECT count(*) AS n \
             FROM k8.accounts s JOIN k8.accounts r ON r.account = t.to_account) d) x",
            not_lateral("t", "t", true),
        ),
        (
            "SELECT t.request, j.n FROM k8.tr t, (k8.accounts a JOIN (SELECT t.request AS n) y \
             ON true) j",
            not_lateral("t", "t", true),
        ),
        (
            "SELECT t.request, x.n FROM k8.tr t, (SELECT r.account AS n FROM k8.accounts s \
             JOIN k8.accounts r ON r.account = t.to_account UNION SELECT 'z') x",
            not_lateral("t", "t", true),
        ),
        (
            "SELECT t.request FROM k8.tr t WHERE EXISTS (SELECT 1 FROM k8.tr u, (SELECT 1 \
             FROM k8.accounts s JOIN k8.accounts r ON r.account = u.to_account) x)",
            not_lateral("u", "u", true),
        ),
        (
            "SELECT t.request, x.n FROM k8.tr t, (SELECT count(*) AS n FROM k8.accounts s \
             JOIN k8.accounts r ON r.account = t.nothere) x",
            not_lateral("t", "t", true),
        ),
        (
            "SELECT x.n FROM (SELECT count(*) AS n FROM k8.accounts s JOIN k8.accounts r \
             ON r.account = t.to_account) x, k8.tr t",
            said("42P01", "missing FROM-clause entry for table \"t\""),
        ),
        (
            "SELECT t.request, x.n FROM k8.tr t, (SELECT count(*) AS n FROM k8.accounts s \
             JOIN k8.accounts r ON r.account = to_account) x",
            said(
                "42703",
                &format!(
                    "{column}\nHINT:  To reference that column, you must mark this subquery with \
                     LATERAL."
                ),
            ),
        ),
        (
            "SELECT t.request, x.n FROM k8.tr t FULL JOIN (SELECT count(*) AS n FROM k8.accounts s \
             JOIN k8.accounts r ON r.account = to_account) x ON true",
            said("42703", column),
        ),
        (
            "SELECT x.n FROM k8.tr t, k8.tr u, (SELECT count(*) AS n FROM k8.accounts s \
             JOIN k8.accounts r ON r.account = to_account) x",
            said(
                "42703",
                "column \"to_account\" does not exist: There are columns named \"to_account\", \
                 but they are in tables that cannot be referenced from this part of the query.\n\
                 HINT:  Try using a table-qualified name.",
            ),
        ),
        (
            "SELECT x.n FROM k8.tr t(a, b, c), (SELECT count(*) AS n FROM k8.accounts s \
             JOIN k8.accounts r ON r.account = c) x",
            said(
                "42703",
                "column \"c\" does not exist: There is a column named \"c\" in table \"t\", but it \
                 cannot be referenced from this part of the query.\nHINT:  To reference that \
                 column, you must mark this subquery with LATERAL.",
            ),
        ),
        (
            "SELECT 1 FROM k8.tr t WHERE EXISTS (SELECT 1 FROM k8.accounts t, k8.accounts u, \
             (SELECT count(*) AS n FROM k8.accounts s JOIN k8.accounts r \
             ON r.account = t.bank) x)",
            said(
                "42703",
                "column t.bank does not exist: There is a column named \"bank\" in table \"t\", \
                 but it cannot be referenced from this part of the query.\nHINT:  To reference \
                 that column, you must mark this subquery with LATERAL.",
            ),
        ),
    ] {
        assert_eq!(refused(&mut w, sql), want, "{sql}");
    }
}

#[test]
fn a_lateral_reference_to_a_right_or_full_join_s_left_side_is_refused_in_postgresql_s_words() {
    let path = declared("lateral-outer-join", K8, &K8_ROWS);
    let mut w = Snail::open_writing(&path).unwrap();
    let combining = |name: &str| {
        said(
            "42P10",
            &format!(
                "invalid reference to FROM-clause entry for table \"{name}\": The combining JOIN \
                 type must be INNER or LEFT for a LATERAL reference."
            ),
        )
    };
    for (sql, name) in [
        (
            "SELECT t.request, x.n FROM k8.tr t RIGHT JOIN LATERAL (SELECT count(*) AS n \
             FROM k8.accounts s JOIN k8.accounts r ON r.account = t.to_account) x ON true",
            "t",
        ),
        (
            "SELECT t.request, x.n FROM k8.tr t FULL JOIN LATERAL (SELECT count(*) AS n \
             FROM k8.accounts s JOIN k8.accounts r ON r.account = t.to_account) x ON true",
            "t",
        ),
        (
            "SELECT t.request, x.n FROM k8.tr t, LATERAL (SELECT count(*) AS n FROM k8.accounts s \
             RIGHT JOIN LATERAL (SELECT s.bank AS b) y ON true) x",
            "s",
        ),
        (
            "SELECT t.request, g FROM k8.tr t RIGHT JOIN generate_series(1, length(t.request)) g \
             ON true",
            "t",
        ),
        (
            "SELECT t.request, x.n FROM k8.tr t LEFT JOIN k8.accounts a ON true RIGHT JOIN LATERAL \
             (SELECT count(*) AS n FROM k8.accounts s JOIN k8.accounts r \
             ON r.account = a.account) x ON true",
            "a",
        ),
    ] {
        assert_eq!(refused(&mut w, sql), combining(name), "{sql}");
    }
}

#[test]
fn an_aggregate_or_a_window_in_a_join_s_on_is_refused_in_postgresql_s_words() {
    let path = declared("on-aggregate", K8, &K8_ROWS);
    let mut w = Snail::open_writing(&path).unwrap();
    let aggregate = said("42803", "aggregate functions are not allowed in JOIN conditions");
    for (sql, want) in [
        (
            "SELECT (SELECT count(*) FROM k8.accounts s JOIN k8.accounts r \
             ON r.account = max(t.to_account || r.account)) AS n FROM k8.tr t",
            aggregate.clone(),
        ),
        (
            "SELECT (SELECT count(*) FROM k8.accounts s JOIN k8.accounts r ON count(*) > 0) AS n \
             FROM k8.tr t",
            aggregate.clone(),
        ),
        (
            "SELECT (SELECT count(*) FROM k8.accounts s JOIN k8.accounts r \
             ON r.account = t.to_account AND row_number() OVER () > 0) AS n FROM k8.tr t",
            said("42P20", "window functions are not allowed in JOIN conditions"),
        ),
        (
            "SELECT 1 FROM k8.accounts s JOIN k8.accounts r ON r.account = max(s.account)",
            aggregate.clone(),
        ),
        (
            "SELECT 1 FROM k8.accounts s FULL JOIN k8.accounts r ON count(*) > 0",
            aggregate.clone(),
        ),
        (
            "SELECT 1 FROM k8.accounts s JOIN k8.accounts r ON s.account = r.account \
             AND (SELECT count(*) FROM k8.tr t WHERE t.to_account = s.account) > 0 \
             AND count(*) > 0",
            aggregate,
        ),
        (
            "SELECT t.from_account, (SELECT count(*) FROM k8.accounts s JOIN k8.accounts r \
             ON r.account = t.to_account) AS n FROM k8.tr t GROUP BY t.from_account",
            said(
                "42803",
                "subquery uses ungrouped column \"t.to_account\" from outer query",
            ),
        ),
    ] {
        assert_eq!(refused(&mut w, sql), want, "{sql}");
    }
}

/// A statement's refusal's SQLSTATE and words, as `refused` gives them.
fn said(code: &'static str, words: &str) -> (Option<&'static str>, String) {
    (Some(code), words.to_string())
}

/// PostgreSQL's words for a relation whose row type would take a type's name.
fn row_type(name: &str) -> String {
    format!(
        "type \"{name}\" already exists\nHINT:  A relation has an associated type of the same \
         name, so you must use a name that doesn't conflict with any existing type."
    )
}

#[test]
fn type_and_function_refusals_are_18_6_s_words() {
    let path = declared(
        "type-words",
        "CREATE SCHEMA k7;
         CREATE TYPE k7.mood AS ENUM ('a', 'b');
         CREATE DOMAIN k7.dom AS integer;
         CREATE TYPE k7.pair AS (a integer, b integer);
         CREATE TABLE k7.t (k integer NOT NULL, PRIMARY KEY (k));
         CREATE VIEW k7.v AS SELECT k FROM k7.t;
         CREATE SEQUENCE k7.q;
         CREATE FUNCTION k7.f(integer) RETURNS integer LANGUAGE sql RETURN $1;
         CREATE TYPE k7.range7 AS RANGE (subtype = integer);
         CREATE TABLE k7.vc (id integer NOT NULL, b varchar(10), PRIMARY KEY (id));",
        &[],
    );
    let mut w = Snail::open_writing(&path).unwrap();
    let taken = |n: &str| said("42710", &format!("type \"{n}\" already exists"));
    for (sql, want) in [
        ("CREATE TYPE k7.mood AS ENUM ('c')", taken("mood")),
        ("CREATE TYPE k7.dom AS ENUM ('c')", taken("dom")),
        ("CREATE TYPE k7.t AS ENUM ('c')", taken("t")),
        ("CREATE TYPE k7.v AS ENUM ('c')", taken("v")),
        ("CREATE TYPE k7.pair AS ENUM ('c')", taken("pair")),
        ("CREATE DOMAIN k7.mood AS text", taken("mood")),
        ("CREATE DOMAIN k7.t AS text", taken("t")),
        ("CREATE DOMAIN k7.v AS text", taken("v")),
        ("CREATE TYPE k7.t AS (x integer)", taken("t")),
        (
            "CREATE TABLE k7.mood (k integer NOT NULL, PRIMARY KEY (k))",
            said("42710", &row_type("mood")),
        ),
        (
            "CREATE TABLE k7.dom (k integer NOT NULL, PRIMARY KEY (k))",
            said("42710", &row_type("dom")),
        ),
        (
            "CREATE TABLE k7.pair (k integer NOT NULL, PRIMARY KEY (k))",
            said("42P07", "relation \"pair\" already exists"),
        ),
        (
            "CREATE TABLE k7.range7 (k integer NOT NULL, PRIMARY KEY (k))",
            said("42710", &row_type("range7")),
        ),
        (
            "CREATE TABLE IF NOT EXISTS k7.mood (k integer NOT NULL, PRIMARY KEY (k))",
            said("42710", &row_type("mood")),
        ),
        ("CREATE VIEW k7.mood AS SELECT 1 AS one", said("42710", &row_type("mood"))),
        ("CREATE SEQUENCE k7.dom", said("42710", &row_type("dom"))),
        (
            "CREATE FUNCTION k7.f(integer) RETURNS integer LANGUAGE sql RETURN 2",
            said("42723", "function \"f\" already exists with same argument types"),
        ),
    ] {
        assert_eq!(refused(&mut w, sql), want, "{sql}");
    }
    // A sequence has no row type, and an index none either: each takes a type's name.
    w.execute("CREATE TYPE k7.q AS ENUM ('c')").unwrap();
    w.execute("CREATE INDEX mood ON k7.t (k)").unwrap();
    // A call no form of a function takes names the function as the call writes it, in its schema;
    // no operator takes a number beside a varchar. Each as 18.6 refused it on 28818 (2026-11-11),
    // with its HINT.
    w.execute("INSERT INTO k7.vc VALUES (1, '2'), (2, 'x')").unwrap();
    let no_function = |call: &str| {
        said(
            "42883",
            &format!(
                "function {call} does not exist\nHINT:  No function matches the given name and \
                 argument types. You might need to add explicit type casts."
            ),
        )
    };
    let no_operator = |op: &str| {
        said(
            "42883",
            &format!(
                "operator does not exist: {op}\nHINT:  No operator matches the given name and \
                 argument types. You might need to add explicit type casts."
            ),
        )
    };
    for (sql, want) in [
        (
            "SELECT pg_catalog.length(1, 2, 3)",
            no_function("pg_catalog.length(integer, integer, integer)"),
        ),
        ("SELECT pg_catalog.nosuch(1)", no_function("pg_catalog.nosuch(integer)")),
        ("SELECT public.length('a')", no_function("public.length(unknown)")),
        (
            "SELECT k FROM k7.t WHERE pg_catalog.length(k, 1) > 0",
            no_function("pg_catalog.length(integer, integer)"),
        ),
        (
            "SELECT public.length(b) FROM k7.vc",
            no_function("public.length(character varying)"),
        ),
        ("SELECT k7.f(1, 2)", no_function("k7.f(integer, integer)")),
        (
            "SELECT id FROM k7.vc WHERE b = 2",
            no_operator("character varying = integer"),
        ),
        (
            "SELECT id FROM k7.vc WHERE b < 2",
            no_operator("character varying < integer"),
        ),
        (
            "SELECT id FROM k7.vc WHERE 2 = b",
            no_operator("integer = character varying"),
        ),
        (
            "SELECT id FROM k7.vc WHERE 2 < b",
            no_operator("integer < character varying"),
        ),
        (
            "SELECT id FROM k7.vc WHERE b = 2::bigint",
            no_operator("character varying = bigint"),
        ),
    ] {
        assert_eq!(refused(&mut w, sql), want, "{sql}");
    }
    // A function of pg_catalog's named in it is read as its bare name is.
    assert_eq!(
        rows(&mut w, "SELECT pg_catalog.length(b) FROM k7.vc ORDER BY id"),
        ["1", "1"]
    );
    assert_eq!(
        rows(&mut w, "SELECT id FROM k7.vc WHERE pg_catalog.length(b) > 0 ORDER BY id"),
        ["1", "2"]
    );
}

/// Two writers of one snail of the tables the transactions name as committed, at a path of the
/// test's name.
fn named(name: &str) -> (Snail, Snail, std::path::PathBuf) {
    let path = declared(
        name,
        "CREATE TABLE t (k varchar(4) NOT NULL, w numeric(9,2), y integer, PRIMARY KEY (k));
         CREATE TABLE d7 (k integer NOT NULL, PRIMARY KEY (k));
         CREATE TABLE t8 (k varchar(4) NOT NULL, y integer, PRIMARY KEY (k));",
        &[],
    );
    (
        Snail::open_writing(&path).unwrap(),
        Snail::open_writing(&path).unwrap(),
        path,
    )
}

#[test]
fn a_name_committed_since_is_refused() {
    let (mut a, mut b, path) = named("committed-since");
    let exists = |n: &str| said("42P07", &format!("relation \"{n}\" already exists"));
    // warren-sling's ask: at SERIALIZABLE the catalog read is the snapshot's, the name made as
    // committed.
    a.execute("BEGIN ISOLATION LEVEL SERIALIZABLE").unwrap();
    assert_eq!(rows(&mut a, "SELECT 1"), ["1"]);
    b.execute("BEGIN ISOLATION LEVEL SERIALIZABLE").unwrap();
    b.execute("CREATE TABLE notes (k varchar(4) NOT NULL, w numeric(9,2), PRIMARY KEY (k))")
        .unwrap();
    b.execute("INSERT INTO notes VALUES ('n1', 1)").unwrap();
    b.execute("COMMIT").unwrap();
    assert_eq!(
        rows(&mut a, "SELECT count(*) FROM pg_class WHERE relname = 'notes'"),
        ["0"]
    );
    assert_eq!(
        refused(
            &mut a,
            "CREATE TABLE notes (k varchar(4) NOT NULL, v numeric(5), PRIMARY KEY (k))"
        ),
        exists("notes")
    );
    a.execute("ROLLBACK").unwrap();
    assert_eq!(rows(&mut a, "SELECT k || ':' || w FROM notes"), ["n1:1.00"]);
    // At REPEATABLE READ alike.
    a.execute("BEGIN ISOLATION LEVEL REPEATABLE READ").unwrap();
    assert_eq!(rows(&mut a, "SELECT 1"), ["1"]);
    b.execute("CREATE TABLE notes2 (k varchar(4) NOT NULL, PRIMARY KEY (k))")
        .unwrap();
    assert_eq!(
        rows(&mut a, "SELECT count(*) FROM pg_class WHERE relname = 'notes2'"),
        ["0"]
    );
    assert_eq!(
        refused(&mut a, "CREATE TABLE notes2 (k varchar(4) NOT NULL, PRIMARY KEY (k))"),
        exists("notes2")
    );
    a.execute("ROLLBACK").unwrap();
    // At READ COMMITTED each statement reads the catalog as committed.
    a.execute("BEGIN").unwrap();
    assert_eq!(rows(&mut a, "SELECT 1"), ["1"]);
    b.execute("CREATE TABLE notes3 (k varchar(4) NOT NULL, PRIMARY KEY (k))")
        .unwrap();
    assert_eq!(
        rows(&mut a, "SELECT count(*) FROM pg_class WHERE relname = 'notes3'"),
        ["1"]
    );
    assert_eq!(
        refused(&mut a, "CREATE TABLE notes3 (k varchar(4) NOT NULL, PRIMARY KEY (k))"),
        exists("notes3")
    );
    a.execute("ROLLBACK").unwrap();
    // And after its own DDL.
    a.execute("BEGIN").unwrap();
    a.execute("CREATE TABLE own3 (k integer NOT NULL, PRIMARY KEY (k))")
        .unwrap();
    b.execute("CREATE TABLE notes4 (k varchar(4) NOT NULL, PRIMARY KEY (k))")
        .unwrap();
    assert_eq!(
        refused(&mut a, "CREATE TABLE notes4 (k varchar(4) NOT NULL, PRIMARY KEY (k))"),
        exists("notes4")
    );
    a.execute("ROLLBACK").unwrap();
    drop((a, b));
    let mut c = Snail::open_writing(&path).unwrap();
    assert_eq!(
        rows(
            &mut c,
            "SELECT string_agg(relname, ',' ORDER BY relname) FROM pg_class \
             WHERE relname IN ('notes', 'notes2', 'notes3', 'notes4', 'own3')"
        ),
        ["notes,notes2,notes3,notes4"]
    );
}

#[test]
fn each_kind_committed_since_is_refused_in_its_words() {
    let (mut a, mut b, path) = named("each-kind");
    a.execute("BEGIN ISOLATION LEVEL SERIALIZABLE").unwrap();
    assert_eq!(rows(&mut a, "SELECT 1"), ["1"]);
    b.execute("BEGIN").unwrap();
    for sql in [
        "CREATE SEQUENCE s6",
        "CREATE VIEW v6 AS SELECT 1 AS one",
        "CREATE INDEX t_w6 ON t (w)",
        "CREATE SCHEMA sc6",
        "CREATE TYPE mood6 AS ENUM ('a')",
        "CREATE DOMAIN dom6 AS integer",
        "CREATE FUNCTION f6(integer) RETURNS integer LANGUAGE sql RETURN $1",
        "ALTER TABLE t8 ADD CONSTRAINT c8 CHECK (y > 0)",
        "DROP TABLE d7",
        "COMMIT",
    ] {
        b.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    a.execute("SAVEPOINT a").unwrap();
    for (sql, want) in [
        ("CREATE SEQUENCE s6", said("42P07", "relation \"s6\" already exists")),
        (
            "CREATE VIEW v6 AS SELECT 2 AS two",
            said("42P07", "relation \"v6\" already exists"),
        ),
        (
            "CREATE INDEX t_w6 ON t (y)",
            said("42P07", "relation \"t_w6\" already exists"),
        ),
        ("CREATE SCHEMA sc6", said("42P06", "schema \"sc6\" already exists")),
        (
            "CREATE TYPE mood6 AS ENUM ('b')",
            said("42710", "type \"mood6\" already exists"),
        ),
        ("CREATE DOMAIN dom6 AS text", said("42710", "type \"dom6\" already exists")),
        (
            "CREATE FUNCTION f6(integer) RETURNS integer LANGUAGE sql RETURN 2",
            said("42723", "function \"f6\" already exists with same argument types"),
        ),
        (
            "CREATE TABLE mood6 (k integer NOT NULL, PRIMARY KEY (k))",
            said("42710", &row_type("mood6")),
        ),
        (
            "ALTER TABLE t8 ADD CONSTRAINT c8 CHECK (y > 1)",
            said("42710", "constraint \"c8\" for relation \"t8\" already exists"),
        ),
        ("DROP TABLE d7", said("42P01", "table \"d7\" does not exist")),
    ] {
        assert_eq!(refused(&mut a, sql), want, "{sql}");
        a.execute("ROLLBACK TO a").unwrap();
    }
    let done = a
        .execute("CREATE TABLE IF NOT EXISTS v6 (k integer NOT NULL, PRIMARY KEY (k))")
        .unwrap();
    assert_eq!(
        done.notices,
        ["NOTICE:  relation \"v6\" already exists, skipping"]
    );
    let done = a.execute("DROP TABLE IF EXISTS d7").unwrap();
    assert_eq!(done.notices, ["NOTICE:  table \"d7\" does not exist, skipping"]);
    // What another made since is named as committed.
    a.execute("CREATE TABLE sc6.inside (k integer NOT NULL, PRIMARY KEY (k))")
        .unwrap();
    a.execute("CREATE TABLE typed6 (k integer NOT NULL, m mood6, d dom6, PRIMARY KEY (k))")
        .unwrap();
    assert_eq!(rows(&mut a, "SELECT nextval('s6')"), ["1"]);
    assert_eq!(rows(&mut a, "SELECT f6(7)"), ["7"]);
    a.execute("COMMIT").unwrap();
    drop((a, b));
    let mut c = Snail::open_writing(&path).unwrap();
    assert_eq!(
        rows(
            &mut c,
            "SELECT string_agg(relname, ',' ORDER BY relname) FROM pg_class \
             WHERE relname IN ('inside', 'typed6', 'd7')"
        ),
        ["inside,typed6"]
    );
}

#[test]
fn ddl_over_a_table_made_since_reads_none() {
    let (mut a, mut b, path) = named("made-since");
    a.execute("BEGIN ISOLATION LEVEL SERIALIZABLE").unwrap();
    assert_eq!(rows(&mut a, "SELECT 1"), ["1"]);
    b.execute("CREATE TABLE notes5 (k varchar(4) NOT NULL, w numeric(9,2), PRIMARY KEY (k))")
        .unwrap();
    b.execute("CREATE TABLE notes8 (k varchar(4) NOT NULL, w numeric(9,2), PRIMARY KEY (k))")
        .unwrap();
    b.execute("INSERT INTO notes8 VALUES ('n1', 1)").unwrap();
    for sql in [
        "CREATE INDEX notes5_w ON notes5 (w)",
        "CREATE TABLE child5 (c integer NOT NULL, k varchar(4) REFERENCES notes5 (k), \
         PRIMARY KEY (c))",
        "CREATE VIEW v5 AS SELECT k FROM notes5",
        "INSERT INTO notes5 VALUES ('n2', 2)",
    ] {
        a.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    assert_eq!(rows(&mut a, "SELECT count(*) FROM v5"), ["1"]);
    a.execute("CREATE VIEW v8 AS SELECT k FROM notes8").unwrap();
    assert_eq!(rows(&mut a, "SELECT count(*) FROM v8"), ["0"]);
    a.execute("INSERT INTO notes8 VALUES ('n2', 2)").unwrap();
    assert_eq!(
        rows(&mut a, "SELECT string_agg(k, ',' ORDER BY k) FROM v8"),
        ["n2"]
    );
    a.execute("COMMIT").unwrap();
    drop((a, b));
    let mut c = Snail::open_writing(&path).unwrap();
    assert_eq!(
        rows(&mut c, "SELECT string_agg(k, ',' ORDER BY k) FROM notes8"),
        ["n1,n2"]
    );
    assert_eq!(
        rows(&mut c, "SELECT string_agg(k, ',' ORDER BY k) FROM notes5"),
        ["n2"]
    );
}

#[test]
fn a_name_dropped_since_is_made_again() {
    let (mut a, mut b, path) = named("dropped-since");
    b.execute("CREATE TABLE d9 (k integer NOT NULL, PRIMARY KEY (k))")
        .unwrap();
    b.execute("INSERT INTO d9 VALUES (1)").unwrap();
    a.execute("BEGIN ISOLATION LEVEL SERIALIZABLE").unwrap();
    assert_eq!(rows(&mut a, "SELECT 1"), ["1"]);
    b.execute("DROP TABLE d9").unwrap();
    assert_eq!(
        rows(&mut a, "SELECT count(*) FROM pg_class WHERE relname = 'd9'"),
        ["1"]
    );
    a.execute("CREATE TABLE d9 (k integer NOT NULL, v integer, PRIMARY KEY (k))")
        .unwrap();
    a.execute("INSERT INTO d9 VALUES (2, 2)").unwrap();
    assert_eq!(
        rows(&mut a, "SELECT string_agg(k || ':' || v, ',') FROM d9"),
        ["2:2"]
    );
    a.execute("COMMIT").unwrap();
    drop((a, b));
    let mut c = Snail::open_writing(&path).unwrap();
    assert_eq!(
        rows(&mut c, "SELECT string_agg(k || ':' || v, ',') FROM d9"),
        ["2:2"]
    );
}

#[test]
fn writers_of_different_names_both_commit() {
    let (mut a, mut b, path) = named("both-commit");
    a.execute("BEGIN ISOLATION LEVEL SERIALIZABLE").unwrap();
    assert_eq!(rows(&mut a, "SELECT 1"), ["1"]);
    a.execute("CREATE TABLE b13 (k integer NOT NULL, v integer, PRIMARY KEY (k))")
        .unwrap();
    b.execute("BEGIN ISOLATION LEVEL SERIALIZABLE").unwrap();
    b.execute("CREATE TABLE a13 (k integer NOT NULL, PRIMARY KEY (k))")
        .unwrap();
    b.execute("INSERT INTO a13 VALUES (7)").unwrap();
    b.execute("COMMIT").unwrap();
    // Its own statements declared on the other's catalog first.
    a.execute("CREATE INDEX b13_v ON b13 (v)").unwrap();
    a.execute("INSERT INTO b13 VALUES (1, 1)").unwrap();
    // And again at its commit, on the catalog another's commit moved since.
    b.execute("CREATE TABLE c13 (k integer NOT NULL, PRIMARY KEY (k))")
        .unwrap();
    a.execute("COMMIT").unwrap();
    assert_eq!(
        rows(
            &mut b,
            "SELECT string_agg(relname, ',' ORDER BY relname) FROM pg_class \
             WHERE relname IN ('a13', 'b13', 'b13_v', 'c13')"
        ),
        ["a13,b13,b13_v,c13"]
    );
    drop((a, b));
    let mut c = Snail::open_writing(&path).unwrap();
    assert_eq!(
        rows(
            &mut c,
            "SELECT (SELECT count(*) FROM a13) || ',' || (SELECT count(*) FROM b13)"
        ),
        ["1,1"]
    );
}

/// What a statement on another thread came to: the rows it wrote, or its refusal.
type Ended = Result<u64, (Option<&'static str>, String)>;

/// A writer's statements run on a thread of its own, each one's outcome sent back as it ends; the
/// snail held open until the test lets it go, so that its locks stand until then.
fn later(path: &std::path::Path, statements: &[&str]) -> std::sync::mpsc::Receiver<Ended> {
    let (tx, rx) = std::sync::mpsc::channel();
    let p = path.to_path_buf();
    let statements: Vec<String> = statements.iter().map(|s| s.to_string()).collect();
    std::thread::spawn(move || {
        let mut w = Snail::open_writing(&p).unwrap();
        for st in &statements {
            let ended = w
                .execute(st)
                .map(|d| d.rows)
                .map_err(|e| (e.sqlstate(), e.to_string()));
            if tx.send(ended).is_err() {
                return;
            }
        }
        while tx.send(Err((None, "held".into()))).is_ok() {
            std::thread::sleep(Duration::from_millis(20));
        }
    });
    rx
}

/// Whether the thread's next statement is still waiting.
fn waits(rx: &std::sync::mpsc::Receiver<Ended>) -> bool {
    std::thread::sleep(Duration::from_millis(300));
    rx.try_recv().is_err()
}

/// The thread's next statement's outcome, once it ends.
fn next(rx: &std::sync::mpsc::Receiver<Ended>) -> Ended {
    rx.recv_timeout(Duration::from_secs(10))
        .expect("a statement ended")
}

/// PostgreSQL's refusal of a second row of a catalog's unique key, as the snail writes it.
fn duplicate(index: &str, key: &str) -> (Option<&'static str>, String) {
    let columns = match index {
        "pg_type_typname_nsp_index" => "typname, typnamespace",
        "pg_class_relname_nsp_index" => "relname, relnamespace",
        "pg_namespace_nspname_index" => "nspname",
        _ => "proname, proargtypes, pronamespace",
    };
    said(
        "23505",
        &format!(
            "duplicate key value violates unique constraint \"{index}\": Key ({columns})=({key}) \
             already exists."
        ),
    )
}

#[test]
fn a_name_being_made_waits_then_23505() {
    let (mut a, _b, path) = named("being-made");
    // At SERIALIZABLE the name is not yet committed as its CREATE TABLE looks: it waits on the
    // other's row type, and meets it.
    a.execute("BEGIN").unwrap();
    a.execute("CREATE TABLE notes9 (k varchar(4) NOT NULL, PRIMARY KEY (k))")
        .unwrap();
    let rx = later(
        &path,
        &[
            "BEGIN ISOLATION LEVEL SERIALIZABLE",
            "SELECT 1",
            "CREATE TABLE notes9 (k varchar(4) NOT NULL, v integer, PRIMARY KEY (k))",
        ],
    );
    assert_eq!((next(&rx), next(&rx)), (Ok(0), Ok(1)));
    assert!(waits(&rx), "a name another writer is making");
    a.execute("COMMIT").unwrap();
    assert_eq!(
        next(&rx),
        Err(duplicate("pg_type_typname_nsp_index", "notes9, 2200"))
    );
    drop(rx);
    // Each kind by the first of the catalog's rows it meets.
    for (theirs, ours, index, key) in [
        ("CREATE SEQUENCE s21", "CREATE SEQUENCE s21", "pg_class_relname_nsp_index", "s21, 2200"),
        (
            "CREATE INDEX i21 ON t (w)",
            "CREATE INDEX i21 ON t8 (y)",
            "pg_class_relname_nsp_index",
            "i21, 2200",
        ),
        ("CREATE SCHEMA sc21", "CREATE SCHEMA sc21", "pg_namespace_nspname_index", "sc21"),
        (
            "CREATE TYPE mood21 AS ENUM ('a')",
            "CREATE TYPE mood21 AS ENUM ('b')",
            "pg_type_typname_nsp_index",
            "mood21, 2200",
        ),
        (
            "CREATE DOMAIN dom21 AS integer",
            "CREATE TABLE dom21 (k integer NOT NULL, PRIMARY KEY (k))",
            "pg_type_typname_nsp_index",
            "dom21, 2200",
        ),
        (
            "CREATE FUNCTION f21(integer) RETURNS integer LANGUAGE sql RETURN 1",
            "CREATE FUNCTION f21(integer) RETURNS integer LANGUAGE sql RETURN 2",
            "pg_proc_proname_args_nsp_index",
            "f21, 23, 2200",
        ),
        (
            "CREATE VIEW v21 AS SELECT 1 AS one",
            "CREATE TABLE v21 (k integer NOT NULL, PRIMARY KEY (k))",
            "pg_type_typname_nsp_index",
            "v21, 2200",
        ),
    ] {
        a.execute("BEGIN").unwrap();
        a.execute(theirs).unwrap();
        let rx = later(&path, &[ours]);
        assert!(waits(&rx), "{ours}");
        a.execute("COMMIT").unwrap();
        assert_eq!(next(&rx), Err(duplicate(index, key)), "{ours}");
    }
}

#[test]
fn a_name_being_made_waits_then_is_made_on_rollback() {
    let (mut a, _b, path) = named("being-made-back");
    a.execute("BEGIN").unwrap();
    a.execute("CREATE TABLE notes10 (k varchar(4) NOT NULL, PRIMARY KEY (k))")
        .unwrap();
    let rx = later(
        &path,
        &[
            "BEGIN ISOLATION LEVEL SERIALIZABLE",
            "SELECT 1",
            "CREATE TABLE notes10 (k varchar(4) NOT NULL, v integer, PRIMARY KEY (k))",
            "INSERT INTO notes10 VALUES ('a', 1)",
            "COMMIT",
        ],
    );
    assert_eq!((next(&rx), next(&rx)), (Ok(0), Ok(1)));
    assert!(waits(&rx));
    a.execute("ROLLBACK").unwrap();
    assert_eq!((next(&rx), next(&rx), next(&rx)), (Ok(0), Ok(1), Ok(0)));
    assert_eq!(rows(&mut a, "SELECT count(*) FROM notes10 WHERE v = 1"), ["1"]);
    // Two writers each making a name the other then makes wait in a ring: the second to wait fails,
    // and the first makes its name.
    a.execute("BEGIN").unwrap();
    a.execute("CREATE TABLE r2 (k integer NOT NULL, PRIMARY KEY (k))")
        .unwrap();
    let rx = later(
        &path,
        &[
            "BEGIN",
            "CREATE TABLE r1 (k integer NOT NULL, PRIMARY KEY (k))",
            "CREATE TABLE r2 (k integer NOT NULL, PRIMARY KEY (k))",
            "COMMIT",
        ],
    );
    assert_eq!((next(&rx), next(&rx)), (Ok(0), Ok(0)));
    assert!(waits(&rx));
    let (code, words) = refused(&mut a, "CREATE TABLE r1 (k integer NOT NULL, PRIMARY KEY (k))");
    assert_eq!(code, Some("40P01"), "{words}");
    a.execute("ROLLBACK").unwrap();
    assert_eq!((next(&rx), next(&rx)), (Ok(0), Ok(0)));
    assert_eq!(
        rows(&mut a, "SELECT count(*) FROM pg_class WHERE relname IN ('r1', 'r2')"),
        ["2"]
    );
}

#[test]
fn ddl_of_a_table_being_changed_waits_then_answers() {
    let (mut a, _b, path) = named("being-changed");
    let held_off = |a: &mut Snail, ours: &str, theirs: &str, end: &str| {
        a.execute("BEGIN").unwrap();
        a.execute(ours).unwrap_or_else(|e| panic!("{ours}: {e}"));
        let rx = later(&path, &[theirs]);
        assert!(waits(&rx), "{theirs} waits on {ours}");
        a.execute(end).unwrap();
        next(&rx)
    };
    // An ALTER and a DROP of a table another is changing wait on its lock, then answer as
    // committed, at SERIALIZABLE too.
    a.execute("BEGIN").unwrap();
    a.execute("ALTER TABLE t8 ADD CONSTRAINT c9 CHECK (y > 0)")
        .unwrap();
    let rx = later(
        &path,
        &[
            "BEGIN ISOLATION LEVEL SERIALIZABLE",
            "SELECT 1",
            "ALTER TABLE t8 ADD CONSTRAINT c9 CHECK (y > 1)",
        ],
    );
    assert_eq!((next(&rx), next(&rx)), (Ok(0), Ok(1)));
    assert!(waits(&rx), "an ALTER of a table another alters");
    a.execute("COMMIT").unwrap();
    assert_eq!(
        next(&rx),
        Err(said("42710", "constraint \"c9\" for relation \"t8\" already exists"))
    );
    drop(rx);
    a.execute("CREATE TABLE d15 (k integer NOT NULL, PRIMARY KEY (k))")
        .unwrap();
    assert_eq!(
        held_off(&mut a, "DROP TABLE d15", "DROP TABLE d15", "COMMIT"),
        Err(said("42P01", "table \"d15\" does not exist"))
    );
    // A DROP waits on a writer of its rows; CREATE INDEX on a writer, and a writer on it; a foreign
    // key holds off writers of the table it references, and COMMENT ON an index made on its table.
    assert_eq!(
        held_off(&mut a, "INSERT INTO d7 VALUES (1)", "DROP TABLE d7", "COMMIT"),
        Ok(0)
    );
    assert_eq!(
        rows(&mut a, "SELECT count(*) FROM pg_class WHERE relname = 'd7'"),
        ["0"]
    );
    assert_eq!(
        held_off(
            &mut a,
            "INSERT INTO t8 VALUES ('a', 1)",
            "CREATE INDEX t8_y ON t8 (y)",
            "ROLLBACK"
        ),
        Ok(0)
    );
    assert_eq!(
        held_off(
            &mut a,
            "CREATE INDEX t_y ON t (y)",
            "INSERT INTO t VALUES ('b', 1, 1)",
            "ROLLBACK"
        ),
        Ok(1)
    );
    assert_eq!(
        held_off(
            &mut a,
            "CREATE TABLE child (c integer NOT NULL, k varchar(4) REFERENCES t (k), \
             PRIMARY KEY (c))",
            "INSERT INTO t VALUES ('c', 1, 1)",
            "ROLLBACK"
        ),
        Ok(1)
    );
    assert_eq!(
        held_off(
            &mut a,
            "COMMENT ON TABLE t8 IS 'eight'",
            "CREATE INDEX t8_y2 ON t8 (y)",
            "ROLLBACK"
        ),
        Ok(0)
    );
    // A writer that waited on a DROP finds the table gone, in PostgreSQL's words.
    a.execute("CREATE TABLE d16 (k integer NOT NULL, PRIMARY KEY (k))")
        .unwrap();
    assert_eq!(
        held_off(&mut a, "DROP TABLE d16", "INSERT INTO d16 VALUES (1)", "COMMIT"),
        Err(said("42P01", "relation \"d16\" does not exist"))
    );
    assert_eq!(rows(&mut a, "SELECT count(*) FROM t"), ["2"]);
    // A table's lock is its own wherever the catalog lays it: a DROP before it moves it.
    a.execute("BEGIN").unwrap();
    a.execute("DROP TABLE t").unwrap();
    a.execute("INSERT INTO t8 VALUES ('z', 5)").unwrap();
    let rx = later(&path, &["ALTER TABLE t8 ADD CONSTRAINT c11 CHECK (y > 0)"]);
    assert!(waits(&rx), "an ALTER of a table another writes");
    a.execute("ROLLBACK").unwrap();
    assert_eq!(next(&rx), Ok(0));
}

#[test]
fn a_schema_an_object_is_being_made_in_holds_off_its_drop() {
    let (mut a, _b, path) = named("being-made-in");
    // Each kind made in a schema holds off another's DROP SCHEMA of it, as 18.6's lock on the
    // schema does; on its rollback the schema is dropped.
    let kinds = [
        "CREATE TABLE {s}.x (k integer NOT NULL, PRIMARY KEY (k))",
        "CREATE VIEW {s}.v AS SELECT 1 AS one",
        "CREATE SEQUENCE {s}.q",
        "CREATE TYPE {s}.e AS ENUM ('a')",
        "CREATE FUNCTION {s}.f() RETURNS integer LANGUAGE sql RETURN 1",
        "CREATE DOMAIN {s}.d AS integer",
    ];
    for (i, made) in kinds.iter().enumerate() {
        let (s, made) = (format!("sc{i}"), made.replace("{s}", &format!("sc{i}")));
        a.execute(&format!("CREATE SCHEMA {s}")).unwrap();
        a.execute("BEGIN").unwrap();
        a.execute(&made).unwrap_or_else(|e| panic!("{made}: {e}"));
        let rx = later(&path, &[&format!("DROP SCHEMA {s}")]);
        assert!(waits(&rx), "{made}");
        a.execute("ROLLBACK").unwrap();
        assert_eq!(next(&rx), Ok(0), "{made}");
    }
    // On its commit the DROP is refused, or takes what it made with CASCADE.
    a.execute("CREATE SCHEMA s9").unwrap();
    a.execute("BEGIN").unwrap();
    a.execute("CREATE TABLE s9.x (k integer NOT NULL, PRIMARY KEY (k))")
        .unwrap();
    let rx = later(&path, &["DROP SCHEMA s9"]);
    assert!(waits(&rx), "a DROP SCHEMA of a schema a table is made in");
    assert_eq!(a.execute("INSERT INTO s9.x VALUES (1)").unwrap().rows, 1);
    a.execute("COMMIT").unwrap();
    assert_eq!(
        next(&rx),
        Err(said(
            "2BP01",
            "cannot drop schema s9 because other objects depend on it: table s9.x depends on \
             schema s9"
        ))
    );
    drop(rx);
    a.execute("BEGIN").unwrap();
    a.execute("CREATE TABLE s9.y (k integer NOT NULL, PRIMARY KEY (k))")
        .unwrap();
    let rx = later(&path, &["DROP SCHEMA s9 CASCADE"]);
    assert!(waits(&rx), "a DROP SCHEMA CASCADE");
    a.execute("COMMIT").unwrap();
    assert_eq!(next(&rx), Ok(0));
    assert_eq!(
        rows(&mut a, "SELECT count(*) FROM pg_class WHERE relname IN ('x', 'y')"),
        ["0"]
    );
    // A schema another is dropping holds off what is made in it: refused once the DROP commits,
    // made on its rollback. Two tables made in one schema never wait for each other.
    for (end, ended) in [
        ("COMMIT", Err(said("3F000", "schema \"s11\" does not exist"))),
        ("ROLLBACK", Ok(0)),
    ] {
        a.execute("CREATE SCHEMA IF NOT EXISTS s11").unwrap();
        a.execute("BEGIN").unwrap();
        a.execute("DROP SCHEMA s11").unwrap();
        let rx = later(&path, &["CREATE TABLE s11.a (k integer NOT NULL, PRIMARY KEY (k))"]);
        assert!(waits(&rx), "a table made in a schema being dropped");
        a.execute(end).unwrap();
        assert_eq!(next(&rx), ended, "{end}");
    }
    a.execute("BEGIN").unwrap();
    a.execute("CREATE TABLE s11.b (k integer NOT NULL, PRIMARY KEY (k))")
        .unwrap();
    let rx = later(&path, &["CREATE TABLE s11.c (k integer NOT NULL, PRIMARY KEY (k))"]);
    assert_eq!(next(&rx), Ok(0));
    a.execute("COMMIT").unwrap();
}

#[test]
fn ddl_reading_a_relation_another_is_changing_waits_then_answers() {
    let (mut a, _b, path) = named("being-read");
    let held_off = |a: &mut Snail, ours: &[&str], theirs: &str, end: &str| {
        a.execute("BEGIN").unwrap();
        for st in ours {
            a.execute(st).unwrap_or_else(|e| panic!("{st}: {e}"));
        }
        let rx = later(&path, &[theirs]);
        assert!(waits(&rx), "{theirs} waits on {ours:?}");
        a.execute(end).unwrap();
        next(&rx)
    };
    // A view of a table another is dropping waits on its lock, then is refused as committed; of one
    // another is altering, at any level of its question, it is made once the other ends.
    assert_eq!(
        held_off(&mut a, &["DROP TABLE d7"], "CREATE VIEW v10 AS SELECT k FROM d7", "COMMIT"),
        Err(said("42P01", "relation \"d7\" does not exist"))
    );
    let locked = ["ALTER TABLE t ADD CONSTRAINT c30 CHECK (y > 0)"];
    assert_eq!(
        held_off(
            &mut a,
            &locked,
            "CREATE VIEW v13 AS WITH c AS (SELECT k FROM t) SELECT k FROM c",
            "ROLLBACK"
        ),
        Ok(0)
    );
    // The view's lock stands to its transaction's end, on a table its subquery reads too; and a
    // view of a view another is dropping waits as on a table.
    assert_eq!(
        held_off(
            &mut a,
            &["CREATE VIEW v12 AS SELECT k FROM t WHERE y IN (SELECT y FROM t8)"],
            "DROP TABLE t8",
            "ROLLBACK"
        ),
        Ok(0)
    );
    assert_eq!(
        held_off(&mut a, &["DROP VIEW v13"], "CREATE VIEW v16 AS SELECT k FROM v13", "COMMIT"),
        Err(said("42P01", "relation \"v13\" does not exist"))
    );
    // A sequence OWNED BY a table, and a function whose body reads one, wait alike.
    for theirs in [
        "CREATE SEQUENCE s20 OWNED BY t.y",
        "CREATE FUNCTION f20() RETURNS bigint LANGUAGE sql RETURN (SELECT count(*) FROM t)",
    ] {
        assert_eq!(held_off(&mut a, &locked, theirs, "ROLLBACK"), Ok(0), "{theirs}");
    }
    a.execute("CREATE SEQUENCE s21").unwrap();
    assert_eq!(
        held_off(&mut a, &locked, "ALTER SEQUENCE s21 OWNED BY t.y", "ROLLBACK"),
        Ok(0)
    );
    for (made, theirs, gone) in [
        ("d22", "CREATE SEQUENCE s22 OWNED BY d22.k", "d22"),
        (
            "d23",
            "CREATE FUNCTION f23() RETURNS bigint LANGUAGE sql RETURN (SELECT count(*) FROM d23)",
            "d23",
        ),
    ] {
        a.execute(&format!("CREATE TABLE {made} (k integer NOT NULL, PRIMARY KEY (k))"))
            .unwrap();
        let drop = format!("DROP TABLE {made}");
        assert_eq!(
            held_off(&mut a, &[drop.as_str()], theirs, "COMMIT"),
            Err(said("42P01", &format!("relation \"{gone}\" does not exist"))),
            "{theirs}"
        );
    }
    // Two views of one table, and a writer of it, do not wait on another's view.
    a.execute("BEGIN").unwrap();
    a.execute("CREATE VIEW v14 AS SELECT k FROM t").unwrap();
    let rx = later(
        &path,
        &["CREATE VIEW v15 AS SELECT k FROM t", "INSERT INTO t VALUES ('z', 1, 1)"],
    );
    assert_eq!((next(&rx), next(&rx)), (Ok(0), Ok(1)));
    a.execute("COMMIT").unwrap();
    drop(rx);
    assert_eq!(
        rows(
            &mut a,
            "SELECT count(*) FROM pg_class WHERE relname IN ('v10', 'v12', 'v13', 'v14', 'v15', \
             'v16', 't8', 's20', 's21')"
        ),
        ["4"]
    );
}

#[test]
fn a_question_names_as_committed_and_reads_its_snapshot() {
    let (mut a, mut b, path) = named("question-committed");
    b.execute("CREATE TABLE d17 (k integer NOT NULL, PRIMARY KEY (k))")
        .unwrap();
    b.execute("INSERT INTO d17 VALUES (1)").unwrap();
    a.execute("BEGIN ISOLATION LEVEL SERIALIZABLE").unwrap();
    assert_eq!(rows(&mut a, "SELECT 1"), ["1"]);
    b.execute("CREATE TABLE notes11 (k varchar(4) NOT NULL, PRIMARY KEY (k))")
        .unwrap();
    b.execute("INSERT INTO notes11 VALUES ('n1')").unwrap();
    b.execute("DROP TABLE d17").unwrap();
    // A table another made since is named, reading its snapshot's rows of it; one another dropped
    // since is not found.
    assert_eq!(rows(&mut a, "SELECT count(*) FROM notes11"), ["0"]);
    a.execute("SAVEPOINT a").unwrap();
    assert_eq!(
        refused(&mut a, "SELECT count(*) FROM d17"),
        said("42P01", "relation \"d17\" does not exist")
    );
    a.execute("ROLLBACK TO a").unwrap();
    // The catalog tables hold the snapshot's objects and its own, before its own DDL and after.
    let classes = "SELECT string_agg(relname, ',' ORDER BY relname) FROM pg_class \
                   WHERE relname IN ('notes11', 'd17', 'own11')";
    assert_eq!(rows(&mut a, classes), ["d17"]);
    a.execute("CREATE TABLE own11 (k integer NOT NULL, PRIMARY KEY (k))")
        .unwrap();
    assert_eq!(rows(&mut a, classes), ["d17,own11"]);
    assert_eq!(rows(&mut a, "SELECT count(*) FROM notes11"), ["0"]);
    a.execute("INSERT INTO notes11 VALUES ('n2')").unwrap();
    assert_eq!(rows(&mut a, "SELECT count(*) FROM notes11"), ["1"]);
    a.execute("COMMIT").unwrap();
    assert_eq!(rows(&mut a, "SELECT count(*) FROM notes11"), ["2"]);
    assert_eq!(rows(&mut a, classes), ["notes11,own11"]);
    // At REPEATABLE READ alike; at READ COMMITTED each statement reads as committed.
    a.execute("BEGIN ISOLATION LEVEL REPEATABLE READ").unwrap();
    assert_eq!(rows(&mut a, "SELECT 1"), ["1"]);
    b.execute("CREATE TABLE notes12 (k varchar(4) NOT NULL, PRIMARY KEY (k))")
        .unwrap();
    b.execute("INSERT INTO notes12 VALUES ('n1')").unwrap();
    // A write names as committed too, before any question.
    a.execute("INSERT INTO notes12 VALUES ('n2')").unwrap();
    assert_eq!(rows(&mut a, "SELECT count(*) FROM notes12"), ["1"]);
    assert_eq!(
        rows(&mut a, "SELECT count(*) FROM pg_class WHERE relname = 'notes12'"),
        ["0"]
    );
    a.execute("ROLLBACK").unwrap();
    a.execute("BEGIN").unwrap();
    assert_eq!(rows(&mut a, "SELECT 1"), ["1"]);
    b.execute("CREATE TABLE notes13 (k varchar(4) NOT NULL, PRIMARY KEY (k))")
        .unwrap();
    b.execute("INSERT INTO notes13 VALUES ('n1')").unwrap();
    assert_eq!(rows(&mut a, "SELECT count(*) FROM notes13"), ["1"]);
    assert_eq!(
        rows(&mut a, "SELECT count(*) FROM pg_class WHERE relname = 'notes13'"),
        ["1"]
    );
    a.execute("ROLLBACK").unwrap();
    drop((a, b));
    let mut c = Snail::open_writing(&path).unwrap();
    assert_eq!(
        rows(&mut c, "SELECT string_agg(k, ',' ORDER BY k) FROM notes11"),
        ["n1,n2"]
    );
}

#[test]
fn writers_of_the_tables_each_made_write_them_beside_each_other() {
    let (mut a, _b, path) = named("each-made");
    // A table a transaction made is named by no other writer until it commits, whatever OID each
    // writer's catalog gives what it made.
    a.execute("BEGIN ISOLATION LEVEL SERIALIZABLE").unwrap();
    assert_eq!(rows(&mut a, "SELECT 1"), ["1"]);
    a.execute("CREATE TABLE b15 (k integer NOT NULL, v integer, PRIMARY KEY (k))")
        .unwrap();
    a.execute("CREATE INDEX b15_v ON b15 (v)").unwrap();
    a.execute("INSERT INTO b15 VALUES (1, 1)").unwrap();
    let rx = later(
        &path,
        &[
            "BEGIN ISOLATION LEVEL SERIALIZABLE",
            "CREATE TABLE a15 (k integer NOT NULL, PRIMARY KEY (k))",
            "INSERT INTO a15 VALUES (7)",
            "COMMIT",
        ],
    );
    assert_eq!(
        (next(&rx), next(&rx), next(&rx), next(&rx)),
        (Ok(0), Ok(0), Ok(1), Ok(0))
    );
    a.execute("COMMIT").unwrap();
    assert_eq!(
        rows(
            &mut a,
            "SELECT (SELECT count(*) FROM a15) || ',' || (SELECT count(*) FROM b15 WHERE v = 1)"
        ),
        ["1,1"]
    );
}
