//! What warren-slobol's own regime writes, answered as PostgreSQL 18.6 answers it: a value an
//! UPDATE sets by a subquery, read on the row it changes; a constant or a NULL among the items of a
//! question on one table. Every answer is 18.6's over the same statements (`session.nu`'s way, on
//! slobol's probe of 2026-11-08). A snail writes on its own only with `standalone`, as
//! `tests/ddl.rs`.
#![cfg(feature = "standalone")]

mod weather;

use warren_snail::catalog::Catalog;
use warren_snail::read::Snail;
use weather::{tmp, write};

const DDL: &str = "CREATE TABLE accounts (acct varchar(6) NOT NULL, balance numeric(9,2), \
     third_party boolean, PRIMARY KEY (acct));
     CREATE TABLE trans (seq bigint NOT NULL, acct varchar(6), amount numeric(9,2), \
     approver text, approver__absent text, PRIMARY KEY (seq));";

/// A snail of slobol's probe tables and rows.
fn snail(name: &str) -> Snail {
    let path = tmp(&format!("slobol-{name}.snail"));
    write(&path, &Catalog::parse(DDL).unwrap(), &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    run(
        &mut w,
        "INSERT INTO accounts (acct, balance, third_party) VALUES ('100', 50.00, false), \
         ('200', 0.00, true), ('300', 10.00, false)",
    );
    run(
        &mut w,
        "INSERT INTO trans (seq, acct, amount, approver, approver__absent) VALUES \
         (1, '100', 10.00, 'KF', 'present'), (2, '100', 5.00, NULL, 'unmeasured'), \
         (3, '200', 7.00, NULL, 'unmeasured'), (4, '200', 1.00, 'AB', 'present'), \
         (5, '200', 2.00, 'AB', 'present'), (6, '300', 3.00, NULL, 'inapplicable')",
    );
    w
}

fn run(w: &mut Snail, sql: &str) {
    w.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
}

/// An answer's rows, each value as PostgreSQL's output function writes it, `|` between them.
fn rows(w: &mut Snail, sql: &str) -> Vec<String> {
    let done = w.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    let b = done.answer.expect("an answer");
    warren_snail::read::answer_text(&b)
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

#[test]
fn a_value_set_by_a_subquery_is_read_on_the_row_it_changes() {
    let mut w = snail("set");
    let balances = "SELECT acct, balance FROM accounts ORDER BY acct";
    for (sql, want) in [
        (
            "UPDATE accounts SET balance = balance + (SELECT sum(t.amount) FROM trans t \
             WHERE t.acct = accounts.acct) WHERE acct IN (SELECT acct FROM trans)",
            ["100|65.00", "200|10.00", "300|13.00"],
        ),
        (
            "UPDATE accounts SET balance = (SELECT count(*) FROM trans t WHERE t.acct = accounts.acct)",
            ["100|2.00", "200|3.00", "300|1.00"],
        ),
        (
            "UPDATE accounts SET balance = (SELECT max(amount) FROM trans) WHERE acct = '100'",
            ["100|10.00", "200|3.00", "300|1.00"],
        ),
    ] {
        run(&mut w, sql);
        assert_eq!(rows(&mut w, balances), want, "{sql}");
    }
    let e = w
        .execute("UPDATE accounts SET third_party = (SELECT amount FROM trans WHERE seq = 1)")
        .err()
        .expect("refused");
    assert_eq!(
        (e.sqlstate(), e.to_string()),
        (
            Some("42804"),
            weather::hinted("column \"third_party\" is of type boolean but expression is of type numeric")
        )
    );
}

#[test]
fn a_constant_or_a_null_among_a_questions_items_on_one_table_is_read() {
    let mut w = snail("constants");
    for (sql, want) in [
        (
            "SELECT acct, 'limit' AS field FROM accounts ORDER BY acct",
            &["100|limit", "200|limit", "300|limit"][..],
        ),
        (
            "SELECT seq, 'limit' AS field, approver__absent AS reason FROM trans \
             WHERE approver__absent = 'unmeasured' ORDER BY seq",
            &["2|limit|unmeasured", "3|limit|unmeasured"],
        ),
        (
            "SELECT seq, NULL AS approver, CAST(NULL AS text) AS cast FROM trans WHERE seq < 3 \
             ORDER BY seq",
            &["1|NULL|NULL", "2|NULL|NULL"],
        ),
        (
            "SELECT seq, CAST(NULL AS text) AS approver FROM trans WHERE seq = 1",
            &["1|NULL"],
        ),
        (
            "SELECT seq, 1 AS one, 2.5 AS half, true AS yes FROM trans WHERE seq = 1",
            &["1|1|2.5|t"],
        ),
        (
            "SELECT acct, 'n' AS field, count(*) FROM trans GROUP BY acct ORDER BY acct",
            &["100|n|2", "200|n|3", "300|n|1"],
        ),
        ("SELECT count(*) FROM trans", &["6"]),
    ] {
        assert_eq!(rows(&mut w, sql), want, "{sql}");
    }
}
