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
//! around it, a FULL JOIN's equality holding it among them; a type, a domain, a relation or a
//! function of a name taken refused in 18.6's words; and DDL in a transaction naming and making
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
         CREATE TYPE k7.range7 AS RANGE (subtype = integer);",
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
