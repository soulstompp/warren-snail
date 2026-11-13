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
        assert_eq!(refused(&mut w, sql), (Some(code), weather::hinted(words)), "{sql}");
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
            weather::hinted("column \"b\" is of type bigint but expression is of type text")
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

#[test]
fn a_set_in_a_one_row_values_inserts_a_row_for_each_value() {
    let mut w = snail("set");
    run(
        &mut w,
        &["CREATE SCHEMA u1", "CREATE TABLE u1.v (n integer PRIMARY KEY)"],
    );
    let inserted = |w: &mut Snail, sql: &str| w.execute(sql).unwrap().rows;
    assert_eq!(inserted(&mut w, "INSERT INTO u1.v VALUES (generate_series(1, 3))"), 3);
    assert_eq!(
        inserted(&mut w, "INSERT INTO u1.v (n) VALUES (generate_series(1, 0))"),
        0
    );
    assert_eq!(
        rows(&mut w, "INSERT INTO u1.v VALUES (unnest(ARRAY[7, 8])) RETURNING n"),
        "7;8"
    );
    // Several rows take no set; one row's sets run beside each other, the shorter giving NULL.
    assert_eq!(
        refused(&mut w, "INSERT INTO u1.v VALUES (generate_series(1, 2)), (5)"),
        (
            Some("0A000"),
            "set-returning functions are not allowed in VALUES".to_string()
        )
    );
    assert_eq!(
        refused(
            &mut w,
            "INSERT INTO u1.v VALUES (generate_series(1, 2) + generate_series(10, 12)) RETURNING n"
        ),
        (
            Some("23502"),
            "null value in column \"n\" of relation \"v\" violates not-null constraint: Failing \
             row contains (null)."
                .to_string()
        )
    );
    assert_eq!(rows(&mut w, "SELECT n FROM u1.v ORDER BY n"), "1;2;3;7;8");
    // Each row's DEFAULTs, and the draw a bare set's last call makes past its rows.
    run(
        &mut w,
        &["CREATE TABLE u1.w (n integer PRIMARY KEY, m integer DEFAULT 0, s serial)"],
    );
    assert_eq!(
        rows(
            &mut w,
            "INSERT INTO u1.w VALUES (generate_series(1, 2), DEFAULT) RETURNING n, m, s"
        ),
        "1|0|1;2|0|2"
    );
    assert_eq!(rows(&mut w, "SELECT nextval('u1.w_s_seq')"), "4");
    run(&mut w, &["INSERT INTO u1.w (n) VALUES (generate_series(11, 10))"]);
    assert_eq!(rows(&mut w, "SELECT nextval('u1.w_s_seq')"), "6");
    assert_eq!(
        rows(
            &mut w,
            "INSERT INTO u1.w (s, n) VALUES (DEFAULT, generate_series(21, 22) + 0) RETURNING n, s"
        ),
        "21|7;22|8"
    );
    assert_eq!(rows(&mut w, "SELECT nextval('u1.w_s_seq')"), "9");
    assert_eq!(
        rows(&mut w, "SELECT * FROM u1.w ORDER BY n"),
        "1|0|1;2|0|2;21|0|7;22|0|8"
    );
}

#[test]
fn returning_s_sequence_calls_are_made_as_each_row_is_written() {
    let mut w = snail("returning-calls");
    run(
        &mut w,
        &[
            "CREATE SCHEMA w20",
            "CREATE TABLE w20.q (n integer PRIMARY KEY, m serial)",
            "CREATE TABLE w20.src (k integer PRIMARY KEY)",
            "CREATE TABLE w20.r (k integer PRIMARY KEY, v integer)",
            "INSERT INTO w20.src VALUES (1), (2), (3), (4)",
        ],
    );
    for (sql, want) in [
        (
            "INSERT INTO w20.q (n) VALUES (71), (72) RETURNING n, m, currval('w20.q_m_seq')",
            "71|1|1;72|2|2",
        ),
        (
            "INSERT INTO w20.q (n) VALUES (generate_series(31, 32)) \
             RETURNING n, m, currval('w20.q_m_seq')",
            "31|3|3;32|4|4",
        ),
        ("SELECT nextval('w20.q_m_seq')", "6"),
        (
            "INSERT INTO w20.q (n) SELECT k + 80 FROM w20.src \
             RETURNING n, m, currval('w20.q_m_seq')",
            "81|7|7;82|8|8;83|9|9;84|10|10",
        ),
        (
            "INSERT INTO w20.q (n) VALUES (91), (92) RETURNING n, nextval('w20.q_m_seq')",
            "91|12;92|14",
        ),
        (
            "INSERT INTO w20.q (n) VALUES (generate_series(41, 42)) \
             RETURNING n, m, nextval('w20.q_m_seq')",
            "41|15|16;42|17|18",
        ),
        ("SELECT nextval('w20.q_m_seq')", "20"),
        (
            "INSERT INTO w20.q (n) VALUES (51), (52) \
             RETURNING n, m + currval('w20.q_m_seq'), lastval()",
            "51|42|21;52|44|22",
        ),
        (
            "INSERT INTO w20.r VALUES (nextval('w20.q_m_seq'), 1), (5, 2) \
             RETURNING k, currval('w20.q_m_seq')",
            "23|23;5|23",
        ),
        (
            "INSERT INTO w20.q (n) VALUES (101), (102) \
             RETURNING n, m, CASE WHEN n > 101 THEN currval('w20.q_m_seq') END",
            "101|24|NULL;102|25|25",
        ),
        ("SELECT nextval('w20.q_m_seq')", "26"),
    ] {
        assert_eq!(rows(&mut w, sql), want, "{sql}");
    }
}

#[test]
fn an_insert_s_rows_are_any_question_postgresql_takes() {
    let mut w = snail("any");
    run(
        &mut w,
        &[
            "CREATE SCHEMA a1",
            "CREATE TABLE a1.t (id integer PRIMARY KEY, s text, v varchar(3), n numeric DEFAULT 7)",
            "CREATE TABLE a1.u (id integer PRIMARY KEY, s text)",
            "INSERT INTO a1.u VALUES (1, 'a'), (2, 'b'), (3, 'c')",
        ],
    );
    for (sql, want) in [
        (
            "INSERT INTO a1.t (id, s) SELECT id, s FROM a1.u UNION ALL SELECT 10, 'ten'",
            "1|a|NULL|7;2|b|NULL|7;3|c|NULL|7;10|ten|NULL|7",
        ),
        (
            "INSERT INTO a1.t (id, s) SELECT id, s FROM a1.u UNION SELECT 1, 'a' UNION SELECT 4, 'd'",
            "1|a|NULL|7;2|b|NULL|7;3|c|NULL|7;4|d|NULL|7",
        ),
        (
            "INSERT INTO a1.t (id, s) SELECT id, s FROM a1.u EXCEPT SELECT 2, 'b'",
            "1|a|NULL|7;3|c|NULL|7",
        ),
        (
            "INSERT INTO a1.t (id, s) SELECT id, s FROM a1.u INTERSECT SELECT 3, 'c'",
            "3|c|NULL|7",
        ),
        (
            "INSERT INTO a1.t (id) SELECT 1 UNION ALL SELECT '2'",
            "1|NULL|NULL|7;2|NULL|NULL|7",
        ),
        (
            "INSERT INTO a1.t (id, v) SELECT 1, 'abc  ' UNION ALL SELECT 2, 'x'",
            "1|NULL|abc|7;2|NULL|x|7",
        ),
        (
            "INSERT INTO a1.t (id, s) SELECT id, s FROM a1.u UNION ALL SELECT 10, 'ten' \
             ORDER BY 1 DESC LIMIT 2",
            "3|c|NULL|7;10|ten|NULL|7",
        ),
        (
            "INSERT INTO a1.t (id, s) WITH w AS (SELECT id + 20 AS id, s FROM a1.u) \
             SELECT id, s FROM w WHERE id > 21",
            "22|b|NULL|7;23|c|NULL|7",
        ),
        (
            "INSERT INTO a1.t (id, s) WITH w AS (SELECT 30 AS id) SELECT id, 'w' FROM w \
             UNION ALL SELECT 31, 'x'",
            "30|w|NULL|7;31|x|NULL|7",
        ),
        (
            "INSERT INTO a1.t (id) WITH w AS (SELECT 40 AS id) VALUES ((SELECT id FROM w))",
            "40|NULL|NULL|7",
        ),
        (
            "INSERT INTO a1.t (id, s) VALUES (1, 'a'), (2, 'b'), (3, 'c') ORDER BY 1 DESC LIMIT 2",
            "2|b|NULL|7;3|c|NULL|7",
        ),
        ("INSERT INTO a1.t (id, s) VALUES (9, 'z') OFFSET 0", "9|z|NULL|7"),
        ("INSERT INTO a1.t (id, s) (SELECT 1, 'p')", "1|p|NULL|7"),
        (
            "INSERT INTO a1.t (id, s) (SELECT 2, 'q') UNION ALL (VALUES (3, 'r'))",
            "2|q|NULL|7;3|r|NULL|7",
        ),
        (
            "INSERT INTO a1.t (id, s) VALUES (4, 's') UNION ALL VALUES (5, 't')",
            "4|s|NULL|7;5|t|NULL|7",
        ),
        (
            "INSERT INTO a1.t (id, s) SELECT 6, 'u' UNION ALL (SELECT 7, 'v' UNION ALL SELECT 8, 'w')",
            "6|u|NULL|7;7|v|NULL|7;8|w|NULL|7",
        ),
        (
            "INSERT INTO a1.t (id, s, n) SELECT 1, NULL, 1.5 UNION ALL SELECT 2, NULL, 2",
            "1|NULL|NULL|1.5;2|NULL|NULL|2",
        ),
    ] {
        run(&mut w, &["DELETE FROM a1.t", sql]);
        assert_eq!(rows(&mut w, "SELECT * FROM a1.t ORDER BY id"), want, "{sql}");
    }
    assert_eq!(
        rows(
            &mut w,
            "INSERT INTO a1.t (id, s) SELECT 60, 'x' UNION ALL SELECT 61, 'y' RETURNING id, n"
        ),
        "60|7;61|7"
    );
    for (sql, code, words) in [
        (
            "INSERT INTO a1.t (id) SELECT '1' UNION ALL SELECT '2'",
            "42804",
            "column \"id\" is of type integer but expression is of type text",
        ),
        (
            "INSERT INTO a1.t (id, n) SELECT 1, NULL UNION ALL SELECT 2, NULL",
            "42804",
            "column \"n\" is of type numeric but expression is of type text",
        ),
        (
            "INSERT INTO a1.t (id) VALUES ('7'), ('8') LIMIT 2",
            "42804",
            "column \"id\" is of type integer but expression is of type text",
        ),
        (
            "INSERT INTO a1.t (id, v) SELECT 1, 'abcd' UNION ALL SELECT 2, 'x'",
            "22001",
            "value too long for type character varying(3)",
        ),
        (
            "INSERT INTO a1.t (id) SELECT 1, 'x' UNION ALL SELECT 2, 'y'",
            "42601",
            "INSERT has more expressions than target columns",
        ),
        (
            "INSERT INTO a1.t (id, s) SELECT 1 UNION ALL SELECT 2",
            "42601",
            "INSERT has more target columns than expressions",
        ),
        (
            "INSERT INTO a1.t (id, s) SELECT 1 UNION ALL SELECT 2, 'y'",
            "42601",
            "each UNION query must have the same number of columns",
        ),
        (
            "INSERT INTO a1.t (id, s) SELECT 1, 'a' INTERSECT SELECT 2",
            "42601",
            "each INTERSECT query must have the same number of columns",
        ),
        (
            "INSERT INTO a1.t (id) WITH w AS (SELECT 41 AS id) VALUES (DEFAULT)",
            "42601",
            "DEFAULT is not allowed in this context",
        ),
        (
            "INSERT INTO a1.t (id, s) VALUES (5, 'a'), (6, DEFAULT) LIMIT 2",
            "42601",
            "DEFAULT is not allowed in this context",
        ),
        (
            "INSERT INTO a1.t (id, s) WITH w AS (DELETE FROM a1.u WHERE id = 3 RETURNING *) \
             SELECT id + 50, s FROM w",
            "0A000",
            "WITH clause containing a data-modifying statement must be at the top level",
        ),
    ] {
        assert_eq!(refused(&mut w, sql), (Some(code), weather::hinted(words)), "{sql}");
    }
    assert_eq!(rows(&mut w, "SELECT count(*) FROM a1.u"), "3");
}

#[test]
fn an_insert_of_a_set_operation_draws_in_18_6_s_order() {
    let mut w = snail("set-draws");
    run(
        &mut w,
        &[
            "CREATE SCHEMA a2",
            "CREATE SEQUENCE a2.q",
            "CREATE TABLE a2.d (id bigint DEFAULT nextval('a2.q'), x bigint)",
        ],
    );
    for (from, sql, want) in [
        (
            "SELECT setval('a2.q', 1, false)",
            "INSERT INTO a2.d (x) SELECT nextval('a2.q') UNION ALL SELECT nextval('a2.q') \
             FROM generate_series(1, 2)",
            "2|1;4|3;6|5",
        ),
        (
            "SELECT setval('a2.q', 100)",
            "INSERT INTO a2.d (x) SELECT nextval('a2.q') UNION SELECT nextval('a2.q')",
            "103|101;104|102",
        ),
        (
            "SELECT setval('a2.q', 200)",
            "INSERT INTO a2.d (x) SELECT nextval('a2.q') UNION ALL SELECT nextval('a2.q') \
             ORDER BY 1 DESC",
            "204|201;203|202",
        ),
        (
            "SELECT setval('a2.q', 300)",
            "INSERT INTO a2.d (x) VALUES (nextval('a2.q')), (nextval('a2.q')) \
             UNION ALL SELECT nextval('a2.q')",
            "302|301;304|303;306|305",
        ),
        (
            "SELECT setval('a2.q', 500)",
            "INSERT INTO a2.d (x) (SELECT nextval('a2.q') FROM generate_series(1, 3) LIMIT 2) \
             UNION ALL SELECT nextval('a2.q')",
            "502|501;504|503;506|505",
        ),
    ] {
        run(&mut w, &["DELETE FROM a2.d", from, sql]);
        assert_eq!(rows(&mut w, "SELECT id, x FROM a2.d ORDER BY x"), want, "{sql}");
    }
    run(&mut w, &["DELETE FROM a2.d", "SELECT setval('a2.q', 400)"]);
    assert_eq!(
        rows(
            &mut w,
            "INSERT INTO a2.d (x) SELECT 1 UNION ALL SELECT 2 RETURNING id, x, nextval('a2.q')"
        ),
        "401|1|402;403|2|404"
    );
}
