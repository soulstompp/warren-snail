//! An INSERT's VALUES taking any expression, and an INSERT … SELECT's draws, as PostgreSQL 18.6
//! makes them: each VALUES cell typed against its column and computed, a scalar subquery read at
//! the statement's snapshot, a literal its column does not read assigned as 18.6 assigns it, and
//! each nextval, currval and lastval in the place 18.6's row-by-row order gives it among the
//! DEFAULTs' draws: a VALUES list of several rows in its list's order, one row in the table's, an
//! INSERT … SELECT's row after its question's calls for that row, a DISTINCT question's after the
//! question whole. Every answer and refusal is 18.6's over the same statements (`session.nu --suite
//! inserts`). A snail writes on its own only with `standalone`, as `tests/ddl.rs`.
#![cfg(feature = "standalone")]

mod weather;

use warren_snail::catalog::Catalog;
use warren_snail::read::Snail;
use weather::{tmp, write};

const DDL: &str = "CREATE SCHEMA s;
     CREATE TABLE s.base (id integer NOT NULL, PRIMARY KEY (id));";

/// A snail of the one table, at a path of the test's name, open to write.
fn snail(name: &str) -> Snail {
    let path = tmp(&format!("inserts-{name}.snail"));
    write(&path, &Catalog::parse(DDL).unwrap(), &[]).unwrap();
    Snail::open_writing(&path).unwrap()
}

/// Each statement taken in turn.
fn run(w: &mut Snail, statements: &[&str]) {
    for sql in statements {
        w.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

/// A statement's refusal: its SQLSTATE and its words.
fn refused(w: &mut Snail, sql: &str) -> (Option<&'static str>, String) {
    let e = w
        .execute(sql)
        .err()
        .unwrap_or_else(|| panic!("{sql} was taken"));
    (e.sqlstate(), e.to_string())
}

/// An answer's rows, each value as PostgreSQL's output function writes it, `|` between them, `;`
/// between rows.
fn rows(w: &mut Snail, sql: &str) -> String {
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
        .collect::<Vec<_>>()
        .join(";")
}

#[test]
fn a_values_row_s_expressions_are_computed_and_assigned_as_postgresql_does() {
    let mut w = snail("computed");
    run(
        &mut w,
        &[
            "CREATE SCHEMA v1",
            "CREATE TABLE v1.t (id integer PRIMARY KEY, n integer, s text, b boolean, \
             d numeric(9,1), vc varchar(3))",
            "INSERT INTO v1.t VALUES (1, 2 + 3, upper('a') || 'b', 1 < 2, 2.25 * 2, 'ab')",
            "INSERT INTO v1.t VALUES (2, abs(-4) * (1 + 1), lower('X'), NOT true, 1.0 / 3, \
             substr('abcdef', 2, 3)), (3, coalesce(NULL, 7), 'x' || 3, NULL IS NULL, \
             round(2.45, 1), repeat('z', 2))",
            "INSERT INTO v1.t VALUES (4, CASE WHEN 1 > 2 THEN 1 ELSE 2 END, concat('a', 1, NULL), \
             'yes'::boolean, '5'::numeric + 0.05, 'abc'::varchar(2))",
            "INSERT INTO v1.t VALUES (6, 1.5 + 1, NULL, NULL, -(2.35), NULL)",
            "INSERT INTO v1.t (id, n) VALUES (7, length('abcd')), (8, -length('ab'))",
        ],
    );
    assert_eq!(
        rows(&mut w, "SELECT * FROM v1.t ORDER BY id"),
        "1|5|Ab|t|4.5|ab;2|8|x|f|0.3|bcd;3|7|x3|t|2.5|zz;4|2|a1|t|5.1|ab;\
         6|3|NULL|NULL|-2.4|NULL;7|4|NULL|NULL|NULL|NULL;8|-2|NULL|NULL|NULL|NULL"
    );
    for (sql, code, words) in [
        (
            "INSERT INTO v1.t VALUES (10, 1 / 0, 'a', true, 1, 'a')",
            "22012",
            "division by zero",
        ),
        (
            "INSERT INTO v1.t VALUES (11, 'x' || 1, 'a', true, 1, 'a')",
            "42804",
            "column \"n\" is of type integer but expression is of type text",
        ),
        (
            "INSERT INTO v1.t VALUES (12, 2147483647 + 1, 'a', true, 1, 'a')",
            "22003",
            "integer out of range",
        ),
        (
            "INSERT INTO v1.t VALUES (13, 1, 'a', true, 1, repeat('a', 4))",
            "22001",
            "value too long for type character varying(3)",
        ),
        (
            "INSERT INTO v1.t VALUES (14, 1, 'a', 1 + 1, 1, 'a')",
            "42804",
            "column \"b\" is of type boolean but expression is of type integer",
        ),
        (
            "INSERT INTO v1.t VALUES (17, 1, 'a', true, 1, 'a'), (18, 1 / 0, 'a', true, 1, 'a')",
            "22012",
            "division by zero",
        ),
    ] {
        assert_eq!(refused(&mut w, sql), (Some(code), words.to_string()), "{sql}");
    }
    assert_eq!(rows(&mut w, "SELECT count(*) FROM v1.t"), "7");
}

#[test]
fn a_literal_its_column_does_not_read_is_assigned_as_postgresql_assigns_it() {
    let mut w = snail("assigned");
    run(
        &mut w,
        &[
            "CREATE SCHEMA v1",
            "CREATE TABLE v1.t (id integer PRIMARY KEY, n integer, s text, b boolean, \
             d numeric(9,1), vc varchar(3))",
            "INSERT INTO v1.t VALUES (5, 2.5, 7, 'true', 3, 12)",
            "CREATE TABLE v1.a (id integer PRIMARY KEY, a integer[])",
            "INSERT INTO v1.a VALUES (1, ARRAY[1, 1 + 1]), (2, ARRAY[3 - 2, 2])",
        ],
    );
    assert_eq!(rows(&mut w, "SELECT * FROM v1.t"), "5|3|7|t|3.0|12");
    assert_eq!(rows(&mut w, "SELECT * FROM v1.a ORDER BY id"), "1|{1,2};2|{1,2}");
}

#[test]
fn a_scalar_subquery_in_values_reads_the_statement_s_snapshot() {
    let mut w = snail("subquery");
    run(
        &mut w,
        &[
            "CREATE SCHEMA v2",
            "CREATE TABLE v2.t (id integer PRIMARY KEY, n bigint)",
            "INSERT INTO v2.t VALUES (1, 10), (2, 20)",
            "INSERT INTO v2.t VALUES ((SELECT max(id) FROM v2.t) + 1, (SELECT count(*) FROM v2.t))",
            "INSERT INTO v2.t VALUES ((SELECT max(id) FROM v2.t) + 1, (SELECT sum(n) FROM v2.t)), \
             ((SELECT max(id) FROM v2.t) + 2, (SELECT count(*) FROM v2.t))",
            "INSERT INTO v2.t VALUES (10, (SELECT n FROM v2.t WHERE id = 99))",
        ],
    );
    assert_eq!(
        refused(&mut w, "INSERT INTO v2.t VALUES (11, (SELECT n FROM v2.t))"),
        (
            Some("21000"),
            "more than one row returned by a subquery used as an expression".to_string()
        )
    );
    assert_eq!(
        rows(&mut w, "SELECT * FROM v2.t ORDER BY id"),
        "1|10;2|20;3|2;4|32;5|3;10|NULL"
    );
}

#[test]
fn a_values_list_of_several_rows_draws_in_its_list_s_order_then_the_rest() {
    let mut w = snail("several");
    run(
        &mut w,
        &[
            "CREATE SCHEMA v3",
            "CREATE TABLE v3.t (id serial PRIMARY KEY, b bigint, c bigint)",
            "INSERT INTO v3.t (b) VALUES (nextval('v3.t_id_seq')), (nextval('v3.t_id_seq'))",
            "INSERT INTO v3.t (b, id) VALUES (nextval('v3.t_id_seq'), DEFAULT), \
             (nextval('v3.t_id_seq'), DEFAULT)",
            "INSERT INTO v3.t (b, c) VALUES (currval('v3.t_id_seq'), nextval('v3.t_id_seq')), \
             (lastval(), currval('v3.t_id_seq'))",
            "INSERT INTO v3.t (b) VALUES (setval('v3.t_id_seq', 300)), (2)",
            "CREATE SEQUENCE v3.other",
            "CREATE TABLE v3.u (a bigint DEFAULT nextval('v3.other') PRIMARY KEY, b bigint, \
             c bigint DEFAULT nextval('v3.other'))",
            "INSERT INTO v3.u (b) VALUES (nextval('v3.other')), (nextval('v3.other'))",
            "INSERT INTO v3.u (c, b) VALUES (DEFAULT, nextval('v3.other')), \
             (DEFAULT, nextval('v3.other'))",
            "INSERT INTO v3.u VALUES (nextval('v3.other'), DEFAULT, nextval('v3.other') * 10), \
             (DEFAULT, nextval('v3.other'), DEFAULT)",
        ],
    );
    assert_eq!(
        rows(&mut w, "SELECT * FROM v3.t ORDER BY id"),
        "2|1|NULL;4|3|NULL;6|5|NULL;8|7|NULL;10|8|9;11|10|10;301|300|NULL;302|2|NULL"
    );
    assert_eq!(
        rows(&mut w, "SELECT * FROM v3.u ORDER BY a"),
        "2|1|3;5|4|6;9|8|7;12|11|10;13|NULL|140;15|16|17"
    );
}

#[test]
fn one_values_row_draws_in_the_table_s_column_order() {
    let mut w = snail("one");
    run(
        &mut w,
        &[
            "CREATE SCHEMA v3",
            "CREATE TABLE v3.t (id serial PRIMARY KEY, b bigint, c bigint)",
            "INSERT INTO v3.t (b) VALUES (nextval('v3.t_id_seq'))",
            "INSERT INTO v3.t (b, id) VALUES (nextval('v3.t_id_seq'), DEFAULT)",
            "INSERT INTO v3.t (b, c) VALUES (currval('v3.t_id_seq'), nextval('v3.t_id_seq'))",
            "INSERT INTO v3.t (c, b) VALUES (nextval('v3.t_id_seq'), nextval('v3.t_id_seq') * 100)",
            "INSERT INTO v3.t (b) VALUES (setval('v3.t_id_seq', 200))",
        ],
    );
    assert_eq!(
        rows(&mut w, "SELECT * FROM v3.t ORDER BY id"),
        "1|2|NULL;3|4|NULL;5|5|6;7|800|9;10|200|NULL"
    );
    assert_eq!(rows(&mut w, "SELECT currval('v3.t_id_seq'), lastval()"), "200|200");
}

#[test]
fn a_values_row_refused_stops_before_the_next_row_draws() {
    let mut w = snail("stops");
    run(
        &mut w,
        &[
            "CREATE SCHEMA v5",
            "CREATE TABLE v5.t (id serial PRIMARY KEY, n integer CHECK (n > 0), b bigint)",
        ],
    );
    assert_eq!(
        refused(
            &mut w,
            "INSERT INTO v5.t (n, b) VALUES (-1, nextval('v5.t_id_seq')), (1, nextval('v5.t_id_seq'))"
        ),
        (
            Some("23514"),
            "new row for relation \"t\" violates check constraint \"t_n_check\": Failing row \
             contains (2, -1, 1)."
                .to_string()
        )
    );
    assert_eq!(rows(&mut w, "SELECT nextval('v5.t_id_seq')"), "3");
    assert_eq!(
        refused(
            &mut w,
            "INSERT INTO v5.t (n, b) VALUES (1, nextval('v5.t_id_seq')), (2, 1 / (SELECT 0)), \
             (3, nextval('v5.t_id_seq'))"
        ),
        (Some("22012"), "division by zero".to_string())
    );
    assert_eq!(rows(&mut w, "SELECT nextval('v5.t_id_seq')"), "6");
    // A cell its column refuses is refused before any row draws.
    assert_eq!(
        refused(
            &mut w,
            "INSERT INTO v5.t (n, b) VALUES (1, nextval('v5.t_id_seq')), (2, 'x' || 1)"
        ),
        (
            Some("42804"),
            "column \"b\" is of type bigint but expression is of type text".to_string()
        )
    );
    assert_eq!(rows(&mut w, "SELECT nextval('v5.t_id_seq')"), "7");
}

#[test]
fn an_insert_select_draws_each_row_s_defaults_after_its_calls() {
    let mut w = snail("select");
    run(
        &mut w,
        &[
            "CREATE SCHEMA d1",
            "CREATE TABLE d1.src (k integer PRIMARY KEY, x integer)",
            "INSERT INTO d1.src VALUES (3, 30), (1, 10), (2, 20)",
            "CREATE TABLE d1.t (id serial PRIMARY KEY, b bigint, k integer)",
            "INSERT INTO d1.t (b, k) SELECT nextval('d1.t_id_seq'), k FROM d1.src ORDER BY k",
            "INSERT INTO d1.t (k, b) SELECT k, nextval('d1.t_id_seq') * 10 + nextval('d1.t_id_seq') \
             FROM d1.src WHERE k > 1 ORDER BY k",
            "INSERT INTO d1.t (b, k) SELECT nextval('d1.t_id_seq'), count(*) FROM d1.src",
            "CREATE TABLE d1.g (id integer GENERATED ALWAYS AS IDENTITY PRIMARY KEY, b bigint, \
             k integer)",
            "INSERT INTO d1.g (b, k) SELECT nextval(pg_get_serial_sequence('d1.g', 'id')), k \
             FROM d1.src ORDER BY k",
        ],
    );
    assert_eq!(
        rows(&mut w, "SELECT * FROM d1.t ORDER BY id"),
        "2|1|1;4|3|2;6|5|3;9|78|2;12|111|3;14|13|3"
    );
    assert_eq!(rows(&mut w, "SELECT * FROM d1.g ORDER BY id"), "2|1|1;4|3|2;6|5|3");
}

#[test]
fn an_insert_select_sorted_or_cut_draws_in_its_sorted_order() {
    let mut w = snail("sorted");
    run(
        &mut w,
        &[
            "CREATE SCHEMA d1",
            "CREATE TABLE d1.src (k integer PRIMARY KEY, x integer)",
            "INSERT INTO d1.src VALUES (3, 30), (1, 10), (2, 20)",
            "CREATE TABLE d1.t (id serial PRIMARY KEY, b bigint, k integer)",
            "INSERT INTO d1.t (b, k) SELECT nextval('d1.t_id_seq'), k FROM d1.src ORDER BY k DESC",
            "INSERT INTO d1.t (b, k) SELECT currval('d1.t_id_seq'), k FROM d1.src ORDER BY k",
            "INSERT INTO d1.t (b, k) SELECT nextval('d1.t_id_seq'), k FROM d1.src ORDER BY k LIMIT 2",
        ],
    );
    assert_eq!(
        rows(&mut w, "SELECT * FROM d1.t ORDER BY id"),
        "2|1|3;4|3|2;6|5|1;7|6|1;8|7|2;9|8|3;11|10|1;13|12|2"
    );
}

#[test]
fn a_distinct_question_s_defaults_are_drawn_after_it() {
    let mut w = snail("distinct");
    run(
        &mut w,
        &[
            "CREATE SCHEMA d4",
            "CREATE TABLE d4.src (k integer PRIMARY KEY, x integer)",
            "INSERT INTO d4.src VALUES (3, 30), (1, 10), (2, 20), (4, 10)",
            "CREATE TABLE d4.t (id serial PRIMARY KEY, b bigint, k integer)",
            "INSERT INTO d4.t (b, k) SELECT nextval('d4.t_id_seq'), 1 FROM d4.src",
            "INSERT INTO d4.t (k, b) SELECT DISTINCT x, currval('d4.t_id_seq') FROM d4.src ORDER BY x",
        ],
    );
    assert_eq!(
        rows(&mut w, "SELECT * FROM d4.t ORDER BY id"),
        "2|1|1;4|3|1;6|5|1;8|7|1;9|8|10;10|8|20;11|8|30"
    );
}

#[test]
fn a_value_given_a_generated_column_is_refused_in_postgresql_s_words() {
    let mut w = snail("generated");
    run(
        &mut w,
        &[
            "CREATE SCHEMA v4",
            "CREATE TABLE v4.g (id integer PRIMARY KEY, n integer, \
             m integer GENERATED ALWAYS AS (n * 10) STORED)",
            "INSERT INTO v4.g (id, n) VALUES (1, 2 + 3)",
        ],
    );
    assert_eq!(
        refused(&mut w, "INSERT INTO v4.g VALUES (3, 1, 1 + 1)"),
        (
            Some("428C9"),
            "cannot insert a non-DEFAULT value into column \"m\": Column \"m\" is a generated \
             column."
                .to_string()
        )
    );
    assert_eq!(rows(&mut w, "SELECT * FROM v4.g"), "1|5|50");
}
