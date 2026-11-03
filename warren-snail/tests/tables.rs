//! A snail of any number of tables. While the root holds what grows with the schema, the file is laid
//! out as it always was; once it would not, the tables' orders and money columns, the catalog's text
//! and the sequences lie in pages the root names, the commit still one small root write, and a round
//! on one table of hundreds reads and writes that table's path. Formats 7 and 8 refuse a schema their
//! root does not hold, by name.

mod weather;

use arrow::array::{Array, Int64Array};
use std::path::{Path, PathBuf};
use warren_snail::carry::{self, Commit, Host};
use warren_snail::catalog::Catalog;
use warren_snail::file::Reader;
use warren_snail::read::Snail;
use warren_snail::values::Values;
use warren_snail::write::Rows;
use weather::{fresh, span, tmp, write};

/// Tables enough that no root holds them.
const MANY: usize = 600;

/// `n` tables of two columns, `s.table_<i>`.
fn ddl(n: usize) -> String {
    (0..n)
        .map(|i| format!("CREATE TABLE s.table_{i:05} (a bigint PRIMARY KEY, b text);\n"))
        .collect()
}

fn name(i: usize) -> String {
    format!("s.table_{i:05}")
}

/// `n` rows of a table: `a` from `from`, `b` its text.
fn rows(from: i64, n: i64) -> Rows {
    Rows {
        columns: vec![
            Values::Int((from..from + n).map(Some).collect()),
            Values::Text((from..from + n).map(|a| Some(format!("b{a}"))).collect()),
        ],
    }
}

/// A snail of `n` tables, those in `full` holding rows 1 to 100.
fn written(file: &str, n: usize, full: &[usize]) -> PathBuf {
    let path = tmp(&format!("tables-{file}.snail"));
    let tables: Vec<(String, Rows)> = full.iter().map(|&i| (name(i), rows(1, 100))).collect();
    write(&path, &Catalog::parse(&ddl(n)).unwrap(), &tables).unwrap();
    path
}

fn is_cut(path: &Path) -> bool {
    Reader::open(path).unwrap().root().unwrap().is_cut()
}

/// A question's one count.
fn count(snail: &mut Snail, sql: &str) -> i64 {
    let (b, _) = snail.ask(sql, None).unwrap();
    b.column(0)
        .as_any()
        .downcast_ref::<Int64Array>()
        .unwrap()
        .value(0)
}

/// Every page of the file past the root's is named, free or waiting, once: the pages named.
fn whole(path: &Path) -> u64 {
    fresh(path).census().unwrap().named
}

#[test]
fn a_schema_the_root_holds_is_laid_out_as_before() {
    let path = written("few", 10, &[3]);
    assert!(!is_cut(&path));
    let mut snail = fresh(&path);
    assert_eq!(count(&mut snail, "SELECT count(*) FROM s.table_00003"), 100);
    whole(&path);
    // The root holds 43 of these tables with its reused runs at their most and room for a commit's
    // marks (`snail.nu tables`), and cuts at 44.
    assert!(!is_cut(&written("fits", 43, &[])));
    assert!(is_cut(&written("cut", 44, &[])));
}

#[test]
fn a_schema_past_the_root_is_read_through_the_pages_it_names() {
    let path = written("many", MANY, &[0, 300, MANY - 1]);
    assert!(is_cut(&path));
    let mut snail = fresh(&path);
    let opened = snail.pages_read();
    for i in [0, 300, MANY - 1] {
        let sql = format!("SELECT count(*) FROM {}", name(i));
        assert_eq!(count(&mut snail, &sql), 100);
    }
    assert_eq!(count(&mut snail, "SELECT count(*) FROM s.table_00007"), 0);
    // A point read of one table reads the root, its order's directory and a column page: no page
    // of another table's.
    let before = snail.pages_read();
    let (b, _) = snail
        .ask("SELECT b FROM s.table_00300 WHERE a = 42", None)
        .unwrap();
    assert_eq!(b.num_rows(), 1);
    let point = snail.pages_read() - before;
    assert!(
        point <= 6,
        "{point} pages for a point read, {opened} to open"
    );
    whole(&path);
}

fn insert(i: usize, a: i64) -> String {
    format!("INSERT INTO {} (a, b) VALUES ({a}, 'n{a}')", name(i))
}

/// A host that keeps the root, as PostgreSQL does: each commit, its pages and its root, replayed
/// by it before it returns, its transaction's root tagged.
struct Keeping(PathBuf, u64);

impl Host for Keeping {
    fn carry(&mut self, commit: &Commit) -> warren_snail::Result<()> {
        assert_eq!(
            carry::replay_held(&self.0, commit, &mut |_| Ok(()))?,
            carry::Replayed::Landed
        );
        Ok(())
    }

    fn tag(&self) -> Option<u64> {
        Some(self.1)
    }

    fn keeps_root(&self) -> bool {
        true
    }
}

/// A judge that takes the tags given as committed.
struct Said(Vec<u64>);

impl carry::Judge for Said {
    fn taken(&self, tag: u64) -> warren_snail::Result<bool> {
        Ok(self.0.contains(&tag))
    }
}

/// A judge that hands the root its host takes.
struct Handing(Vec<u8>);

impl carry::Judge for Handing {
    fn taken(&self, _: u64) -> warren_snail::Result<bool> {
        Err(warren_snail::Error::new("asked no word on a copy"))
    }

    fn root(&self) -> warren_snail::Result<Option<Vec<u8>>> {
        Ok(Some(self.0.clone()))
    }
}

#[test]
fn a_host_keeps_a_cut_root_and_a_writer_handed_it_begins_from_it() {
    use std::rc::Rc;
    let path = written("hosted", MANY, &[300]);
    let g = fresh(&path).generation();
    let mut w = Snail::open_carried(
        &path,
        Box::new(Keeping(path.clone(), 7)),
        Rc::new(Said(vec![7])),
    )
    .unwrap();
    w.execute(&insert(300, 1001)).unwrap();
    drop(w);
    let mut judged = Snail::open_judged(&path, Rc::new(Said(vec![7]))).unwrap();
    assert_eq!(judged.generation(), g + 1);
    assert_eq!(
        count(&mut judged, "SELECT count(*) FROM s.table_00300"),
        101
    );
    drop(judged);
    // A writer outside the host, handed the root its host takes, writes its pages from it: a
    // table made, the catalog's and the list's pages among them.
    let root = carry::taken_root(&path, &Said(vec![7])).unwrap();
    let mut w = Snail::open_carried(
        &path,
        Box::new(Keeping(path.clone(), 8)),
        Rc::new(Handing(root)),
    )
    .unwrap();
    w.execute("CREATE TABLE s.extra (a bigint PRIMARY KEY, b text)")
        .unwrap();
    drop(w);
    let mut judged = Snail::open_judged(&path, Rc::new(Said(vec![7, 8]))).unwrap();
    assert_eq!(judged.generation(), g + 2);
    assert_eq!(count(&mut judged, "SELECT count(*) FROM s.extra"), 0);
    assert_eq!(
        count(&mut judged, "SELECT count(*) FROM s.table_00300"),
        101
    );
    assert!(is_cut(&path));
    judged.census().unwrap();
}

#[cfg(feature = "standalone")]
mod written {
    use super::*;
    use std::sync::{Arc, Mutex};
    use warren_snail::read::{Snails, StopAcross};

    fn rows_of(path: &Path, i: usize) -> i64 {
        count(
            &mut fresh(path),
            &format!("SELECT count(*) FROM {}", name(i)),
        )
    }

    #[test]
    fn a_write_to_one_table_of_hundreds_writes_that_tables_path() {
        let path = written("writes", MANY, &[300]);
        let named = whole(&path);
        // A reader opened before the writes reads the generation it opened on, and the newest once
        // it begins a round again.
        let mut before = fresh(&path);
        let mut w = Snail::open_writing(&path).unwrap();
        let one = w.execute(&insert(300, 1001)).unwrap();
        assert!(one.pages_written <= 10, "{one:?}");
        // A table made: its own path and the catalog's last page, not the catalog whole.
        let made = w
            .execute("CREATE TABLE s.extra (a bigint PRIMARY KEY, b text)")
            .unwrap();
        assert!(made.pages_written <= 10, "{made:?}");
        assert_eq!(
            count(&mut before, "SELECT count(*) FROM s.table_00300"),
            100
        );
        // Begun again, it reads the root and the pages written since: the catalog's two, the list's
        // leaves the two commits wrote, and its table's.
        let at = before.pages_read();
        assert_eq!(
            count(&mut before, "SELECT count(*) FROM s.table_00300"),
            101
        );
        let renewed = before.pages_read() - at;
        assert!(renewed <= 12, "{renewed} pages begun again");
        assert_eq!(count(&mut before, "SELECT count(*) FROM s.extra"), 0);
        drop(before);
        w.execute("INSERT INTO s.extra (a, b) VALUES (1, 'one')")
            .unwrap();
        // A transaction across two tables far apart in the list.
        for st in ["BEGIN", &insert(1, 1), &insert(MANY - 2, 1), "COMMIT"] {
            w.execute(st).unwrap();
        }
        // Pages freed are taken again.
        let mut reused = 0;
        for a in 0..8 {
            reused += w.execute(&insert(300, 2000 + a)).unwrap().pages_reused;
            reused += w
                .execute(&format!("DELETE FROM s.table_00300 WHERE a = {}", 2000 + a))
                .unwrap()
                .pages_reused;
        }
        assert!(reused > 0);
        drop(w);
        assert!(is_cut(&path));
        assert!(whole(&path) < named + 64);
        assert_eq!(rows_of(&path, 300), 101);
        assert_eq!(rows_of(&path, 1), 1);
        assert_eq!(rows_of(&path, MANY - 2), 1);
        assert_eq!(count(&mut fresh(&path), "SELECT count(*) FROM s.extra"), 1);
    }

    #[test]
    fn a_snail_its_tables_outgrow_cuts_its_root_and_keeps_its_sequences_and_money() {
        let path = tmp("tables-grows.snail");
        let ddl = format!(
            "{}CREATE TABLE s.counted (id serial PRIMARY KEY, m money);\n",
            ddl(40)
        );
        write(&path, &Catalog::parse(&ddl).unwrap(), &[]).unwrap();
        let mut w = Snail::open_writing(&path).unwrap();
        for m in ["1.50", "1.75", "2.00"] {
            w.execute(&format!("INSERT INTO s.counted (m) VALUES ('{m}')"))
                .unwrap();
        }
        assert!(!is_cut(&path));
        let mut made = 0;
        while !is_cut(&path) {
            assert!(made < 400, "no root cut after {made} tables");
            w.execute(&format!(
                "CREATE TABLE s.more_{made:03} (a bigint PRIMARY KEY, b text)"
            ))
            .unwrap();
            made += 1;
        }
        w.execute("INSERT INTO s.counted (m) VALUES ('2.25')")
            .unwrap();
        w.execute(&format!(
            "INSERT INTO s.more_{:03} (a, b) VALUES (1, 'x')",
            made - 1
        ))
        .unwrap();
        drop(w);
        whole(&path);
        let mut r = fresh(&path);
        assert_eq!(
            count(&mut r, "SELECT count(*) FROM s.counted WHERE id = 4"),
            1
        );
        assert_eq!(
            count(&mut r, "SELECT count(*) FROM s.counted WHERE m > '2.10'"),
            1
        );
        assert_eq!(
            count(
                &mut r,
                &format!("SELECT count(*) FROM s.more_{:03}", made - 1)
            ),
            1
        );
        assert_eq!(count(&mut r, "SELECT count(*) FROM s.table_00039"), 0);
    }

    /// A host whose majority is one follower: each commit applied to it before it returns.
    struct Follower(PathBuf, Arc<Mutex<Vec<Commit>>>);

    impl Host for Follower {
        fn carry(&mut self, commit: &Commit) -> warren_snail::Result<()> {
            carry::apply(&self.0, commit)?;
            self.1.lock().unwrap().push(commit.clone());
            Ok(())
        }
    }

    #[test]
    fn a_cut_root_is_carried_to_a_follower_page_for_page() {
        let leader = written("leader", MANY, &[300]);
        let follower = tmp("tables-follower.snail");
        carry::copy(&leader, &follower).unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut w = Snail::open_writing(&leader).unwrap();
        w.carried_by(Box::new(Follower(follower.clone(), log.clone())))
            .unwrap();
        let statements = [
            insert(300, 1001),
            "CREATE TABLE s.extra (a bigint PRIMARY KEY, b text)".to_string(),
            "INSERT INTO s.extra (a, b) VALUES (1, 'one')".to_string(),
            "DELETE FROM s.table_00300 WHERE a = 7".to_string(),
        ];
        for st in &statements {
            w.execute(st).unwrap();
            assert_eq!(
                std::fs::read(&leader).unwrap(),
                std::fs::read(&follower).unwrap(),
                "{st}"
            );
        }
        for st in ["BEGIN", &insert(1, 1), &insert(MANY - 2, 1), "COMMIT"] {
            w.execute(st).unwrap();
        }
        drop(w);
        assert_eq!(
            std::fs::read(&leader).unwrap(),
            std::fs::read(&follower).unwrap()
        );
        // Each commit carried its root and the pages of the lists it wrote, a page for each.
        assert!(log.lock().unwrap().iter().all(|c| c.pages.len() <= 12));
        assert_eq!(rows_of(&follower, 300), 100);
        assert_eq!(rows_of(&follower, MANY - 2), 1);
        whole(&follower);
    }

    #[test]
    fn a_transaction_across_snails_one_of_them_cut_is_all_or_nothing() {
        let a = written("across-a", MANY, &[300]);
        let b = tmp("tables-across-b.snail");
        write(
            &b,
            &Catalog::parse("CREATE TABLE t.other (a bigint PRIMARY KEY, b text);").unwrap(),
            &[("t.other".into(), rows(1, 10))],
        )
        .unwrap();
        let both = [a.as_path(), b.as_path()];
        {
            let mut w = Snails::open_writing(&both).unwrap();
            w.execute("BEGIN").unwrap();
            w.execute(&insert(300, 5001)).unwrap();
            w.execute("INSERT INTO t.other (a, b) VALUES (11, 'x')")
                .unwrap();
            w.stop_at(Some(StopAcross::BeforeDecision));
            assert!(w.execute("COMMIT").is_err());
        }
        assert_eq!(rows_of(&a, 300), 100);
        {
            let mut w = Snails::open_writing(&both).unwrap();
            w.execute("BEGIN").unwrap();
            w.execute(&insert(300, 5002)).unwrap();
            w.execute("INSERT INTO t.other (a, b) VALUES (12, 'y')")
                .unwrap();
            w.stop_at(Some(StopAcross::AfterDecision));
            assert!(w.execute("COMMIT").is_err());
        }
        // The round across them reads every snail as the transaction left it.
        let mut across = Snails::open(&both).unwrap();
        let n = |s: &mut Snails, q: &str| {
            s.ask(q)
                .unwrap()
                .0
                .column(0)
                .as_any()
                .downcast_ref::<Int64Array>()
                .unwrap()
                .value(0)
        };
        assert_eq!(n(&mut across, "SELECT count(*) FROM s.table_00300"), 101);
        assert_eq!(n(&mut across, "SELECT count(*) FROM t.other"), 11);
        drop(across);
        drop(Snails::open_writing(&both).unwrap());
        whole(&a);
        whole(&b);
    }

    #[test]
    fn a_snail_in_files_of_few_pages_keeps_its_root_and_lists_in_place() {
        let path = tmp("tables-spanned.snail");
        let tables: Vec<(String, Rows)> = vec![(name(300), rows(1, 100))];
        warren_snail::write::write_spanned(
            &path,
            &Catalog::parse(&ddl(MANY)).unwrap(),
            &tables,
            &[],
            64,
        )
        .unwrap();
        assert!(is_cut(&path));
        let mut w = Snail::open_writing(&path).unwrap();
        w.execute(&insert(300, 1001)).unwrap();
        w.execute("CREATE TABLE s.extra (a bigint PRIMARY KEY, b text)")
            .unwrap();
        for st in ["BEGIN", &insert(1, 1), &insert(MANY - 2, 1), "COMMIT"] {
            w.execute(st).unwrap();
        }
        drop(w);
        whole(&path);
        assert_eq!(rows_of(&path, 300), 101);
        assert_eq!(rows_of(&path, MANY - 2), 1);
        let mut p = path.as_os_str().to_owned();
        p.push(".1");
        assert!(PathBuf::from(p).exists());
    }

    /// The pages a round reads and writes, and the file's size, at 1, 100, 1,000 and 10,000 tables:
    /// a table made, a one-row insert, a point read on a snail opened before it and on one opened
    /// after, and a commit across two tables. Printed, for the ledger.
    #[test]
    #[ignore]
    fn pages_at_each_schema_size() {
        for n in [1, 100, 1_000, 10_000] {
            let path = written(&format!("size-{n}"), n, &[n / 2]);
            let bytes = std::fs::metadata(&path).unwrap().len();
            let cut = is_cut(&path);
            let opened = fresh(&path).pages_read();
            let mut before = fresh(&path);
            let mut w = Snail::open_writing(&path).unwrap();
            let ddl = w
                .execute("CREATE TABLE s.extra (a bigint PRIMARY KEY, b text)")
                .unwrap();
            let one = w.execute(&insert(n / 2, 1001)).unwrap();
            let mut two = Vec::new();
            for st in ["BEGIN", &insert(0, 5001), &insert(n - 1, 5002), "COMMIT"] {
                two.push(w.execute(st).unwrap());
            }
            drop(w);
            let point = format!("SELECT b FROM {} WHERE a = 42", name(n / 2));
            let at = before.pages_read();
            before.ask(&point, None).unwrap();
            let first = before.pages_read() - at;
            let at = before.pages_read();
            before.ask(&point, None).unwrap();
            let renewed = before.pages_read() - at;
            let mut after = fresh(&path);
            let at = after.pages_read();
            after.ask(&point, None).unwrap();
            let point_after = after.pages_read() - at;
            println!(
                "tables {n}: cut {cut}, file {bytes} bytes, open {opened} pages; DDL read {} wrote {}; \
                 insert read {} wrote {}; point read {first}, after the commits {renewed}, on a snail \
                 opened after {point_after}; across two tables wrote {} ({} at COMMIT); file after {} bytes",
                ddl.pages_read,
                ddl.pages_written,
                one.pages_read,
                one.pages_written,
                two.iter().map(|d| d.pages_written).sum::<u64>(),
                two[3].pages_written,
                std::fs::metadata(&path).unwrap().len(),
            );
        }
    }
}

#[test]
fn formats_seven_and_eight_refuse_a_schema_their_root_does_not_hold() {
    let catalog = Catalog::parse(&ddl(MANY)).unwrap();
    for (format, written) in [
        (
            8,
            warren_snail::write::write_format_eight(
                &tmp("tables-eight.snail"),
                &catalog,
                &[],
                span(),
            ),
        ),
        (
            7,
            warren_snail::write::write_format_seven(
                &tmp("tables-seven.snail"),
                &catalog,
                &[],
                span(),
            ),
        ),
    ] {
        let e = written.unwrap_err().to_string();
        assert!(
            e.contains("does not fit one page")
                && e.contains(&format!("format {format} cannot cut")),
            "{e}"
        );
    }
}
