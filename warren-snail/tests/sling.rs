//! The PostgreSQL warren-sling writes, answered as PostgreSQL 18.6 answers it: an ORDER BY naming a
//! column of the select list by its name or its place, read once, whatever names the column's own
//! expression holds; an aggregate inside an IN list, a BETWEEN, a LIKE, a COLLATE or a SUBSTRING,
//! grouped or not; an aggregate in ORDER BY, a column of the select list or none; and two serializable
//! writers of one block, each of its own row by the table's key, both committing, as 18.6 commits them
//! where the row's page has room, a write skew refused and a key moved into a read gap met; and a
//! read alone between a writer's texts committing nothing, as 18.6's commits nothing, the block it
//! found wasted pruned by the writer's next commit that writes; an OR anywhere in a WHERE or an ON,
//! two writers each reading its own rows by an OR of their keys both committing, as 18.6 commits them
//! reading by its index; and a row value IN a list of rows, read as the OR of its rows' comparisons.
//! Every answer and refusal is 18.6's over the same statements (`session.nu --suite sling`, and
//! `sessions.nu --only serializable` for the writers; 18.6 with its index, through `dblink`, for
//! the writers by an OR). A snail writes on its own only with `standalone`, as `tests/ddl.rs`.
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
