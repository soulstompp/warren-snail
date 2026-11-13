//! WHERE CURRENT OF a cursor declared without FOR UPDATE or SHARE, taken only where the cursor's
//! rows come along one of its table's orders, forward or backward, as PostgreSQL 18.6's plan reads
//! them by a scan; where its ORDER BY is no order's, refused in 18.6's words, as its plan sorts.
//! Every answer and refusal is 18.6's over the same statements. A snail writes on its own only with
//! `standalone`, as `tests/ddl.rs`.
#![cfg(feature = "standalone")]

mod weather;

use std::path::PathBuf;
use warren_snail::catalog::Catalog;
use warren_snail::read::Snail;
use weather::{tmp, write};

const DDL: &str = "CREATE SCHEMA w;
     CREATE TABLE w.t (id integer PRIMARY KEY, b text, c integer);
     CREATE INDEX t_c ON w.t (c);
     CREATE INDEX t_cd ON w.t (c DESC, b);
     CREATE TABLE w.n (a integer NOT NULL, b text);";

fn declared(name: &str) -> PathBuf {
    let path = tmp(&format!("cursors-{name}.snail"));
    write(&path, &Catalog::parse(DDL).unwrap(), &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    for sql in [
        "INSERT INTO w.t VALUES (1, 'x', 10), (2, 'y', 20), (3, 'z', 30)",
        "INSERT INTO w.n VALUES (1, 'x'), (2, 'y')",
    ] {
        w.execute(sql).unwrap();
    }
    path
}

/// 18.6's refusal of WHERE CURRENT OF a cursor its plan sorts, on the table the write names.
fn refused(table: &str) -> Result<String, String> {
    Err(format!(
        "cursor \"k\" is not a simply updatable scan of table \"{table}\""
    ))
}

/// The rows a write through a cursor's question returns, FETCH 1 taken first where `fetched`, or
/// its refusal; the transaction rolled back.
fn current(w: &mut Snail, question: &str, fetched: bool, write: &str) -> Result<String, String> {
    w.execute("BEGIN").unwrap();
    w.execute(&format!("DECLARE k CURSOR FOR {question}"))
        .unwrap_or_else(|e| panic!("{question}: {e}"));
    if fetched {
        w.execute("FETCH 1 FROM k").unwrap();
    }
    let got = w.execute(write).map_err(|e| e.to_string()).map(|d| {
        let b = d.answer.expect("RETURNING's rows");
        w.text_of(&b)
            .unwrap()
            .into_iter()
            .map(|r| {
                r.into_iter()
                    .map(|v| v.unwrap_or_else(|| "NULL".into()))
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect::<Vec<_>>()
            .join(",")
    });
    w.execute("ROLLBACK").unwrap();
    got
}

const UPDATE: &str = "UPDATE w.t SET b = 'u' WHERE CURRENT OF k RETURNING *";

#[test]
fn a_cursor_without_for_update_is_current_only_along_an_order() {
    let path = declared("along");
    let mut w = Snail::open_writing(&path).unwrap();
    let taken = |row: &str| Ok(row.to_string());
    for (question, got) in [
        // Along the primary key, forward and backward, and by the select list's place.
        ("SELECT * FROM w.t ORDER BY id", taken("1|u|10")),
        ("SELECT * FROM w.t ORDER BY id DESC", taken("3|u|30")),
        ("SELECT * FROM w.t ORDER BY 1 DESC", taken("3|u|30")),
        ("SELECT * FROM w.t t ORDER BY t.id", taken("1|u|10")),
        ("SELECT * FROM w.t ORDER BY (id) DESC", taken("3|u|30")),
        // Along an index, forward and backward, and along one of two levels.
        ("SELECT * FROM w.t ORDER BY c", taken("1|u|10")),
        ("SELECT * FROM w.t ORDER BY c DESC", taken("3|u|30")),
        ("SELECT * FROM w.t ORDER BY c DESC, b", taken("3|u|30")),
        ("SELECT * FROM w.t ORDER BY c LIMIT 2", taken("1|u|10")),
        // A name the select list gives reads its item: `id` here is the table's b, and `b` its id.
        ("SELECT b AS id, id AS b FROM w.t ORDER BY b DESC", taken("3|u|30")),
        ("SELECT b AS id FROM w.t ORDER BY id DESC", refused("t")),
        // No order gives them: a column no index leads, a NULLs' place, a level past the key, an
        // expression.
        ("SELECT * FROM w.t ORDER BY b", refused("t")),
        ("SELECT * FROM w.t ORDER BY id NULLS FIRST", refused("t")),
        ("SELECT * FROM w.t ORDER BY id, b", refused("t")),
        ("SELECT * FROM w.t ORDER BY id + 0", refused("t")),
        // FOR UPDATE finds the row it locked, whatever the order.
        ("SELECT * FROM w.t ORDER BY b FOR UPDATE", taken("1|u|10")),
    ] {
        assert_eq!(current(&mut w, question, true, UPDATE), got, "{question}");
    }
    // The order is read before the cursor's place, as 18.6 reads its plan first.
    let sorted = "SELECT * FROM w.t ORDER BY b";
    assert_eq!(current(&mut w, sorted, false, UPDATE), refused("t"));
    let delete = "DELETE FROM w.t WHERE CURRENT OF k RETURNING *";
    assert_eq!(current(&mut w, sorted, true, delete), refused("t"));
    let swapped = "SELECT b AS id, id AS b FROM w.t ORDER BY b DESC";
    let set_c = "UPDATE w.t SET c = 99 WHERE CURRENT OF k RETURNING *";
    assert_eq!(current(&mut w, swapped, true, set_c), taken("3|z|99"));
    // A table with no key is laid out along every column it orders, an order no index of 18.6's is.
    let keyless = "UPDATE w.n SET b = 'u' WHERE CURRENT OF k RETURNING *";
    assert_eq!(
        current(&mut w, "SELECT * FROM w.n ORDER BY a", true, keyless),
        refused("n")
    );
    assert_eq!(
        current(&mut w, "SELECT * FROM w.n", true, keyless),
        taken("1|u")
    );
    // Refused as an invalid cursor state, 18.6's SQLSTATE.
    w.execute("BEGIN").unwrap();
    w.execute(&format!("DECLARE k CURSOR FOR {sorted}")).unwrap();
    w.execute("FETCH 1 FROM k").unwrap();
    let e = w.execute(UPDATE).unwrap_err();
    assert_eq!(e.sqlstate(), Some("24000"), "{e}");
    w.execute("ROLLBACK").unwrap();
}
