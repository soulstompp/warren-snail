//! A column's cast in a WHERE, read as a condition on each row: a json's text in an UPDATE's and a
//! DELETE's WHERE, a regclass's text in a question's, by its name or its table's alias, under each
//! comparison, LIKE, IN, a test for NULL and an OR. Every answer is 18.6's over the same
//! statements. A snail writes on its own only with `standalone`, as `tests/ddl.rs`.
#![cfg(feature = "standalone")]

mod weather;

use std::path::PathBuf;
use warren_snail::catalog::Catalog;
use warren_snail::read::Snail;
use weather::{tmp, write};

/// A snail of `ddl`, its rows inserted, at a path of the test's name.
fn declared(name: &str, ddl: &str, inserts: &[&str]) -> PathBuf {
    let path = tmp(&format!("casts-{name}.snail"));
    write(&path, &Catalog::parse(ddl).unwrap(), &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    for i in inserts {
        w.execute(i).unwrap_or_else(|e| panic!("{i}: {e}"));
    }
    path
}

/// The rows a write changed.
fn changed(w: &mut Snail, sql: &str) -> u64 {
    w.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}")).rows
}

/// A batch's rows, each value as PostgreSQL's output function writes it, `|` between them.
fn joined(s: &Snail, b: &arrow::record_batch::RecordBatch) -> Vec<String> {
    s.text_of(b)
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

/// A statement's answer held to the rows 18.6 gave.
fn asked(w: &mut Snail, sql: &str, want: &[&str]) {
    let done = w.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    let b = done.answer.expect("an answer");
    assert_eq!(joined(w, &b), want, "{sql}");
}

#[test]
fn a_jsons_cast_in_a_writes_where_finds_its_rows() {
    let path = declared(
        "json",
        "CREATE SCHEMA n8; CREATE TABLE n8.kj (id integer PRIMARY KEY, j json);",
        &["INSERT INTO n8.kj VALUES (1, '[1]'), (2, '[2]'), (3, NULL)"],
    );
    let mut w = Snail::open_writing(&path).unwrap();
    assert_eq!(changed(&mut w, "UPDATE n8.kj SET j = '[4]' WHERE j::text = '[1]'"), 1);
    asked(&mut w, "SELECT id, j FROM n8.kj ORDER BY id", &["1|[4]", "2|[2]", "3|NULL"]);
    asked(&mut w, "SELECT id FROM n8.kj WHERE j::text = '[2]'", &["2"]);
    asked(&mut w, "SELECT id FROM n8.kj WHERE j::text <> '[2]' ORDER BY id", &["1"]);
    asked(
        &mut w,
        "SELECT id FROM n8.kj WHERE CAST(j AS text) LIKE '[%' ORDER BY id",
        &["1", "2"],
    );
    assert_eq!(changed(&mut w, "DELETE FROM n8.kj WHERE j::text = '[2]'"), 1);
    asked(&mut w, "SELECT id, j FROM n8.kj ORDER BY id", &["1|[4]", "3|NULL"]);
}

#[test]
fn a_regclass_cast_to_text_in_a_where_reads_each_rows_name() {
    let path = declared(
        "regclass",
        "CREATE SCHEMA ac; CREATE TABLE ac.o30 (a integer PRIMARY KEY); \
         CREATE TABLE ac.keepo30 (id integer PRIMARY KEY, r regclass);",
        &["INSERT INTO ac.keepo30 VALUES (1, 'ac.o30'), (2, 'ac.keepo30')"],
    );
    let mut w = Snail::open_writing(&path).unwrap();
    for (sql, want) in [
        ("SELECT count(*) FROM ac.keepo30 WHERE r::text = 'ac.o30'", &["1"][..]),
        ("SELECT count(*) FROM ac.keepo30 k WHERE k.r::text = 'ac.o30'", &["1"]),
        ("SELECT id FROM ac.keepo30 WHERE r::text LIKE 'ac.%' ORDER BY id", &["1", "2"]),
        ("SELECT id FROM ac.keepo30 WHERE r::text > 'ac.o' ORDER BY id", &["1"]),
        ("SELECT id FROM ac.keepo30 WHERE r::text IN ('ac.keepo30', 'x') ORDER BY id", &["2"]),
        ("SELECT id FROM ac.keepo30 WHERE r::text IS NOT NULL ORDER BY id", &["1", "2"]),
        (
            "SELECT id FROM ac.keepo30 WHERE r::text = 'ac.o30' OR id = 2 ORDER BY id",
            &["1", "2"],
        ),
        ("SELECT id FROM ac.keepo30 WHERE r::oid > 0 ORDER BY id", &["1", "2"]),
    ] {
        asked(&mut w, sql, want);
    }
    drop(w);
    // Asked of a snail opened to read, as a question.
    let mut s = Snail::open(&path).unwrap();
    let (b, _) = s
        .ask("SELECT count(*) FROM ac.keepo30 WHERE r::text = 'ac.o30'", None)
        .unwrap();
    assert_eq!(joined(&s, &b), ["1"]);
}
