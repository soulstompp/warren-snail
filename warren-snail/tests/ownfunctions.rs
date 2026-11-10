//! The functions a snail's own DDL declares, called wherever a statement calls them, as PostgreSQL
//! 18.6 calls a function written in SQL: inlined where its planner inlines it, each argument then
//! computed only where the body reaches it; otherwise called, each argument computed once and a
//! STRICT function answering NULL for a NULL argument without its body. A call in a select list,
//! WHERE, ORDER BY, GROUP BY, an aggregate, an UPDATE's SET, a DELETE's WHERE, an INSERT … SELECT, a
//! CHECK, a DEFAULT, a generated column, a view and a domain; what RETURNS takes; what CREATE
//! FUNCTION refuses when the function is made; a recursion past the stack. Every expected row and
//! refusal was given by 18.6 after the same DDL and statements.

mod weather;

use warren_snail::catalog::Catalog;
use warren_snail::read::Snail;
use weather::tmp;

/// A snail of release 18 written from a DDL, holding no rows.
fn written(name: &str, ddl: &str) -> std::path::PathBuf {
    let path = tmp(&format!("own-{name}.snail"));
    let catalog = Catalog::parse(ddl).unwrap();
    warren_snail::write::write_released(&path, &catalog, &[], &[], weather::span(), 18).unwrap();
    path
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

/// A statement's answer, run in the session: none for one that answers no rows.
#[cfg(feature = "standalone")]
fn rows(w: &mut Snail, sql: &str) -> Vec<String> {
    let done = w.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    done.answer.map(|b| joined(w, &b)).unwrap_or_default()
}

/// A statement's answer held to the rows 18.6 gave.
#[cfg(feature = "standalone")]
fn asked(w: &mut Snail, sql: &str, want: &[&str]) {
    assert_eq!(rows(w, sql), want, "{sql}");
}

/// A statement's refusal, its words.
#[cfg(feature = "standalone")]
fn refused(w: &mut Snail, sql: &str) -> String {
    w.execute(sql)
        .err()
        .unwrap_or_else(|| panic!("{sql} was taken"))
        .to_string()
}

/// A question asked of a snail opened to read.
fn question(s: &mut Snail, sql: &str) -> Result<Vec<String>, String> {
    let (b, _) = s.ask(sql, None).map_err(|e| e.0)?;
    Ok(joined(s, &b))
}

const PLUS: &str = "CREATE SCHEMA c1; \
     CREATE TABLE c1.t (id integer PRIMARY KEY, v integer, w integer); \
     CREATE FUNCTION c1.plus(integer, integer) RETURNS integer LANGUAGE sql IMMUTABLE AS 'SELECT $1 + $2';";

#[test]
fn a_call_reads_its_arguments_types_as_postgresql_s_implicit_casts_take_them() {
    let mut s = Snail::open(&written("plus-read", PLUS)).unwrap();
    assert_eq!(
        question(&mut s, "SELECT c1.plus(1, 2), pg_typeof(c1.plus(1, 2))").unwrap(),
        ["3|integer"]
    );
    assert_eq!(question(&mut s, "SELECT c1.plus('1', '2')").unwrap(), ["3"]);
    assert_eq!(
        question(&mut s, "SELECT c1.plus(1::smallint, 2::smallint)").unwrap(),
        ["3"]
    );
    assert_eq!(question(&mut s, "SELECT c1.plus(NULL, 1)").unwrap(), ["NULL"]);
    for (sql, why) in [
        ("SELECT c1.plus(1.5, 2)", "function c1.plus(numeric, integer) does not exist"),
        ("SELECT c1.plus(1)", "function c1.plus(integer) does not exist"),
        ("SELECT c1.plus(1, 2, 3)", "function c1.plus(integer, integer, integer) does not exist"),
        ("SELECT plus(1, 2)", "function plus(integer, integer) does not exist"),
        ("SELECT c1.plus(2147483647, 1)", "integer out of range"),
    ] {
        assert_eq!(question(&mut s, sql).unwrap_err(), why, "{sql}");
    }
}

#[cfg(feature = "standalone")]
#[test]
fn a_call_is_read_wherever_a_statement_takes_an_expression() {
    let mut w = Snail::open_writing(&written("plus", PLUS)).unwrap();
    rows(&mut w, "INSERT INTO c1.t VALUES (1, 10, 1), (2, 20, 2), (3, NULL, 3), (4, 0, 0)");
    asked(&mut w, "SELECT id, c1.plus(v, w) FROM c1.t ORDER BY id", &["1|11", "2|22", "3|NULL", "4|0"]);
    asked(&mut w, "SELECT id FROM c1.t WHERE c1.plus(v, w) = 11", &["1"]);
    asked(
        &mut w,
        "SELECT id FROM c1.t ORDER BY c1.plus(v, w) DESC NULLS LAST, id",
        &["2", "1", "4", "3"],
    );
    asked(
        &mut w,
        "SELECT c1.plus(v, w) AS p, count(*) FROM c1.t GROUP BY c1.plus(v, w) ORDER BY 1",
        &["0|1", "11|1", "22|1", "NULL|1"],
    );
    asked(&mut w, "SELECT sum(c1.plus(v, w)), max(c1.plus(w, w)) FROM c1.t", &["33|6"]);
    asked(
        &mut w,
        "SELECT id FROM c1.t GROUP BY id HAVING c1.plus(id, 1) > 3 ORDER BY id",
        &["3", "4"],
    );
    asked(
        &mut w,
        "SELECT id, c1.plus(c1.plus(v, w), id) FROM c1.t ORDER BY id",
        &["1|12", "2|24", "3|NULL", "4|4"],
    );
    rows(&mut w, "UPDATE c1.t SET w = c1.plus(w, 100) WHERE id = 2");
    rows(&mut w, "INSERT INTO c1.t SELECT c1.plus(id, 10), v, w FROM c1.t WHERE id < 3");
    rows(&mut w, "DELETE FROM c1.t WHERE c1.plus(v, w) IS NULL");
    asked(
        &mut w,
        "SELECT * FROM c1.t ORDER BY id",
        &["1|10|1", "2|20|102", "4|0|0", "11|10|1", "12|20|102"],
    );
}

const STRICT: &str = "CREATE SCHEMA c2; \
     CREATE TABLE c2.t (id integer PRIMARY KEY, v integer, w integer); \
     CREATE FUNCTION c2.coal(integer) RETURNS integer LANGUAGE sql STRICT AS 'SELECT coalesce($1, -1)'; \
     CREATE FUNCTION c2.coaln(integer) RETURNS integer LANGUAGE sql AS 'SELECT coalesce($1, -1)'; \
     CREATE FUNCTION c2.first(integer, integer) RETURNS integer LANGUAGE sql AS 'SELECT $1'; \
     CREATE FUNCTION c2.firsts(integer, integer) RETURNS integer LANGUAGE sql STRICT AS 'SELECT $1'; \
     CREATE FUNCTION c2.pick(boolean, integer, integer) RETURNS integer LANGUAGE sql AS 'SELECT CASE WHEN $1 THEN $2 ELSE $3 END'; \
     CREATE FUNCTION c2.picks(boolean, integer, integer) RETURNS integer LANGUAGE sql STRICT AS 'SELECT CASE WHEN $1 THEN $2 ELSE $3 END'; \
     CREATE FUNCTION c2.sum2(integer, integer) RETURNS integer LANGUAGE sql STRICT AS 'SELECT $1 + 100 / $2';";

#[cfg(feature = "standalone")]
#[test]
fn a_strict_function_called_answers_null_and_one_inlined_reads_only_what_its_body_reaches() {
    let mut w = Snail::open_writing(&written("strict", STRICT)).unwrap();
    rows(&mut w, "INSERT INTO c2.t VALUES (1, 10, 1), (2, 20, 2), (3, NULL, 3), (4, 0, 0), (5, NULL, 0)");
    // Called: a STRICT body that is not strict answers NULL for a NULL argument.
    asked(
        &mut w,
        "SELECT id, c2.coal(v), c2.coaln(v) FROM c2.t WHERE id < 5 ORDER BY id",
        &["1|10|10", "2|20|20", "3|NULL|-1", "4|0|0"],
    );
    // Inlined, a parameter the body never names is never computed; called, it is.
    asked(
        &mut w,
        "SELECT id, c2.first(v, 100 / w) FROM c2.t WHERE id < 5 ORDER BY id",
        &["1|10", "2|20", "3|NULL", "4|0"],
    );
    assert_eq!(
        refused(&mut w, "SELECT id, c2.firsts(v, 100 / w) FROM c2.t ORDER BY id"),
        "division by zero"
    );
    // Inlined, a CASE's branch not taken computes nothing; called STRICT, every argument is.
    asked(
        &mut w,
        "SELECT id, c2.pick(w = 0, 0, 100 / w) FROM c2.t WHERE id < 5 ORDER BY id",
        &["1|100", "2|50", "3|33", "4|0"],
    );
    assert_eq!(
        refused(&mut w, "SELECT id, c2.picks(w = 0, 0, 100 / w) FROM c2.t ORDER BY id"),
        "division by zero"
    );
    // A STRICT body strict throughout is inlined: a NULL argument does not keep its body from
    // dividing by zero.
    asked(
        &mut w,
        "SELECT id, c2.sum2(v, w) FROM c2.t WHERE id < 4 ORDER BY id",
        &["1|110", "2|70", "3|NULL"],
    );
    assert_eq!(
        refused(&mut w, "SELECT id, c2.sum2(v, w) FROM c2.t WHERE id = 5"),
        "division by zero"
    );
}

#[cfg(feature = "standalone")]
#[test]
fn an_argument_named_twice_is_drawn_once_and_then_every_argument_is_computed() {
    let ddl = "CREATE SCHEMA c3; CREATE SEQUENCE c3.q; \
         CREATE TABLE c3.t (id integer PRIMARY KEY, w integer); \
         CREATE FUNCTION c3.twice(integer) RETURNS integer LANGUAGE sql AS 'SELECT $1 + $1'; \
         CREATE FUNCTION c3.dbl(integer, integer) RETURNS integer LANGUAGE sql AS 'SELECT $1 + $1';";
    let mut w = Snail::open_writing(&written("twice", ddl)).unwrap();
    asked(&mut w, "SELECT c3.twice(nextval('c3.q')::integer)", &["2"]);
    asked(&mut w, "SELECT c3.twice(nextval('c3.q')::integer)", &["4"]);
    rows(&mut w, "INSERT INTO c3.t VALUES (1, 0)");
    // A cheap argument named twice leaves the function inlined, the unused one never computed;
    // a drawn one has it called, every argument computed.
    asked(&mut w, "SELECT c3.dbl(id, 100 / w) FROM c3.t", &["2"]);
    assert_eq!(
        refused(&mut w, "SELECT c3.dbl(nextval('c3.q')::integer, 100 / w) FROM c3.t"),
        "division by zero"
    );
    asked(&mut w, "SELECT c3.dbl(nextval('c3.q')::integer, w) FROM c3.t", &["8"]);
}

#[test]
fn returns_takes_the_body_s_value_as_postgresql_assigns_it() {
    let ddl = "CREATE SCHEMA c4; \
         CREATE FUNCTION c4.big(integer) RETURNS bigint LANGUAGE sql AS 'SELECT $1'; \
         CREATE FUNCTION c4.num(integer) RETURNS numeric LANGUAGE sql AS 'SELECT $1 / 2'; \
         CREATE FUNCTION c4.small(integer) RETURNS integer LANGUAGE sql AS 'SELECT $1::bigint'; \
         CREATE FUNCTION c4.txt(text) RETURNS text LANGUAGE sql IMMUTABLE AS 'SELECT upper($1) || ''!'''; \
         CREATE FUNCTION c4.unkt(integer) RETURNS text LANGUAGE sql AS 'SELECT ''5'''; \
         CREATE FUNCTION c4.narrow(bigint) RETURNS integer LANGUAGE sql AS 'SELECT $1 * 3'; \
         CREATE FUNCTION c4.round2(numeric) RETURNS integer LANGUAGE sql AS 'SELECT $1 * 2';";
    let mut s = Snail::open(&written("returns", ddl)).unwrap();
    for (sql, want) in [
        ("SELECT c4.big(1), pg_typeof(c4.big(1))", "1|bigint"),
        ("SELECT c4.big(2147483647) + 1", "2147483648"),
        ("SELECT c4.num(3), pg_typeof(c4.num(3))", "1|numeric"),
        ("SELECT c4.small(5), pg_typeof(c4.small(5))", "5|integer"),
        ("SELECT c4.txt('ab'), c4.txt(NULL), pg_typeof(c4.txt('ab'))", "AB!|NULL|text"),
        ("SELECT c4.unkt(1), pg_typeof(c4.unkt(1))", "5|text"),
        ("SELECT c4.narrow(5), pg_typeof(c4.narrow(5))", "15|integer"),
        ("SELECT c4.round2(1.25), c4.round2(1.3)", "3|3"),
    ] {
        assert_eq!(question(&mut s, sql).unwrap(), [want], "{sql}");
    }
    assert_eq!(
        question(&mut s, "SELECT c4.narrow(1000000000)").unwrap_err(),
        "integer out of range"
    );
}

#[test]
fn create_function_refuses_a_body_as_postgresql_refuses_it_when_it_is_made() {
    for (body, returns, why) in [
        ("SELECT ''a''::text", "integer", "return type mismatch in function declared to return integer: Actual return type is text."),
        ("SELECT ''5''", "integer", "return type mismatch in function declared to return integer: Actual return type is text."),
        ("SELECT x", "integer", "column \"x\" does not exist"),
        ("SELECT t.x", "integer", "missing FROM-clause entry for table \"t\""),
        ("SELECT c4.nope($1)", "integer", "function c4.nope(integer) does not exist"),
        ("SELECT $2", "integer", "there is no parameter $2"),
    ] {
        let ddl = format!(
            "CREATE SCHEMA c4; CREATE FUNCTION c4.f(integer) RETURNS {returns} LANGUAGE sql AS '{body}';"
        );
        let e = Catalog::parse(&ddl).err().unwrap_or_else(|| panic!("{body} was taken"));
        assert!(e.to_string().contains(why), "{body}: {e}");
    }
    let twice = "CREATE SCHEMA c4; \
         CREATE FUNCTION c4.f(integer) RETURNS integer LANGUAGE sql AS 'SELECT 1'; \
         CREATE FUNCTION c4.f(integer) RETURNS integer LANGUAGE sql AS 'SELECT 2';";
    let e = Catalog::parse(twice).expect_err("the second taken");
    assert!(
        e.to_string().contains("function \"f\" already exists with same argument types"),
        "{e}"
    );
}

#[test]
fn a_recursion_past_the_stack_is_refused_as_postgresql_refuses_it() {
    let ddl = "CREATE SCHEMA c4; \
         CREATE FUNCTION c4.self(integer) RETURNS integer LANGUAGE sql AS 'SELECT c4.self($1 + 1)'; \
         CREATE FUNCTION c4.down(integer) RETURNS integer LANGUAGE sql AS 'SELECT CASE WHEN $1 <= 0 THEN 0 ELSE 1 + c4.down($1 - 1) END';";
    let path = written("recursion", ddl);
    // On a stack wider than the calls may take, so the refusal is the snail's, never the thread's.
    std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(move || {
            let mut s = Snail::open(&path).unwrap();
            assert_eq!(
                question(&mut s, "SELECT c4.down(0), c4.down(1), c4.down(3)").unwrap(),
                ["0|1|3"]
            );
            assert_eq!(
                question(&mut s, "SELECT c4.self(1)").unwrap_err(),
                "stack depth limit exceeded"
            );
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn write_checks_its_rows_against_a_check_that_calls_the_file_s_function() {
    use warren_snail::values::Values;
    use warren_snail::write::Rows;
    let ddl = "CREATE SCHEMA c6; \
         CREATE FUNCTION c6.pos(integer) RETURNS boolean LANGUAGE sql IMMUTABLE AS 'SELECT $1 > 0'; \
         CREATE TABLE c6.c (id integer PRIMARY KEY, v integer CHECK (c6.pos(v)));";
    let catalog = Catalog::parse(ddl).unwrap();
    let rows = |v: Option<i64>| Rows {
        columns: vec![Values::Int(vec![Some(1), Some(2)]), Values::Int(vec![Some(5), v])],
    };
    let taken = tmp("own-write-taken.snail");
    warren_snail::write::write(&taken, &catalog, &[("c6.c".into(), rows(None))]).unwrap();
    let mut s = Snail::open(&taken).unwrap();
    assert_eq!(question(&mut s, "SELECT * FROM c6.c ORDER BY id").unwrap(), ["1|5", "2|NULL"]);
    let refused = tmp("own-write-refused.snail");
    let e = warren_snail::write::write(&refused, &catalog, &[("c6.c".into(), rows(Some(-5)))])
        .expect_err("a row its CHECK refuses taken");
    assert!(
        e.to_string()
            .starts_with("new row for relation \"c\" violates check constraint \"c_v_check\""),
        "{e}"
    );
}

const PLACES: &str = "CREATE SCHEMA c6; \
     CREATE FUNCTION c6.plus(integer, integer) RETURNS integer LANGUAGE sql IMMUTABLE AS 'SELECT $1 + $2'; \
     CREATE FUNCTION c6.pos(integer) RETURNS boolean LANGUAGE sql IMMUTABLE AS 'SELECT $1 > 0'; \
     CREATE TABLE c6.c (id integer PRIMARY KEY, v integer CHECK (c6.pos(v))); \
     CREATE TABLE c6.d (id integer PRIMARY KEY, v integer DEFAULT c6.plus(1, 2)); \
     CREATE TABLE c6.g (id integer PRIMARY KEY, v integer, x integer GENERATED ALWAYS AS (c6.plus(v, 1)) STORED); \
     CREATE VIEW c6.vw AS SELECT id, c6.plus(v, x) AS p FROM c6.g; \
     CREATE DOMAIN c6.posint AS integer CHECK (c6.pos(VALUE));";

#[cfg(feature = "standalone")]
#[test]
fn a_call_in_a_check_a_default_a_generated_column_a_view_and_a_domain() {
    let mut w = Snail::open_writing(&written("places", PLACES)).unwrap();
    rows(&mut w, "INSERT INTO c6.c VALUES (1, 5)");
    assert!(refused(&mut w, "INSERT INTO c6.c VALUES (2, -5)")
        .starts_with("new row for relation \"c\" violates check constraint \"c_v_check\""));
    rows(&mut w, "INSERT INTO c6.c VALUES (3, NULL)");
    asked(&mut w, "SELECT * FROM c6.c ORDER BY id", &["1|5", "3|NULL"]);
    rows(&mut w, "INSERT INTO c6.d (id) VALUES (1)");
    asked(&mut w, "SELECT * FROM c6.d", &["1|3"]);
    rows(&mut w, "INSERT INTO c6.g (id, v) VALUES (1, 5), (2, NULL)");
    rows(&mut w, "UPDATE c6.g SET v = 10 WHERE id = 1");
    asked(&mut w, "SELECT * FROM c6.g ORDER BY id", &["1|10|11", "2|NULL|NULL"]);
    asked(&mut w, "SELECT * FROM c6.vw ORDER BY id", &["1|21", "2|NULL"]);
    asked(
        &mut w,
        "SELECT pg_get_viewdef('c6.vw'::regclass)",
        &[" SELECT id,\n    c6.plus(v, x) AS p\n   FROM c6.g;"],
    );
    asked(
        &mut w,
        "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = 'c6.c'::regclass AND contype = 'c'",
        &["CHECK (c6.pos(v))"],
    );
    asked(
        &mut w,
        "SELECT pg_get_expr(adbin, adrelid) FROM pg_attrdef WHERE adrelid = 'c6.d'::regclass",
        &["c6.plus(1, 2)"],
    );
    asked(&mut w, "SELECT 5::c6.posint", &["5"]);
    assert!(refused(&mut w, "SELECT (-5)::c6.posint")
        .starts_with("value for domain c6.posint violates check constraint \"posint_check\""));
}

#[cfg(feature = "standalone")]
#[test]
fn a_function_a_session_creates_is_called_in_the_next_statement() {
    let mut w = Snail::open_writing(&written("session", "CREATE SCHEMA c9;")).unwrap();
    rows(
        &mut w,
        "CREATE FUNCTION c9.twice(integer) RETURNS integer LANGUAGE sql IMMUTABLE AS 'SELECT $1 * 2'",
    );
    asked(&mut w, "SELECT c9.twice(21)", &["42"]);
    assert_eq!(
        refused(&mut w, "CREATE FUNCTION c9.bad(integer) RETURNS integer LANGUAGE sql AS 'SELECT y'"),
        "column \"y\" does not exist"
    );
}

#[cfg(feature = "standalone")]
#[test]
fn a_function_an_index_keys_on_stays_a_level() {
    let ddl = "CREATE SCHEMA c7; \
         CREATE FUNCTION c7.clock(timestamptz) RETURNS timestamp LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$ SELECT $1 AT TIME ZONE 'UTC' $$; \
         CREATE TABLE c7.p (id integer PRIMARY KEY, at timestamptz NOT NULL); \
         CREATE INDEX p_clock ON c7.p (c7.clock(at));";
    let mut w = Snail::open_writing(&written("level", ddl)).unwrap();
    rows(&mut w, "INSERT INTO c7.p VALUES (1, '2024-03-05 13:04:00+00'), (2, '2024-12-31 23:30:00+00')");
    asked(
        &mut w,
        "SELECT id, c7.clock(at) FROM c7.p ORDER BY id",
        &["1|2024-03-05 13:04:00", "2|2024-12-31 23:30:00"],
    );
    asked(&mut w, "SELECT id FROM c7.p WHERE c7.clock(at) >= '2024-06-01' ORDER BY id", &["2"]);
    asked(
        &mut w,
        "SELECT id, c7.clock(at) + interval '1 hour' FROM c7.p ORDER BY id",
        &["1|2024-03-05 14:04:00", "2|2025-01-01 00:30:00"],
    );
}

#[cfg(feature = "standalone")]
#[test]
fn a_body_more_volatile_than_its_function_says_is_called_every_argument_computed() {
    let ddl = "CREATE SCHEMA c8; \
         CREATE TABLE c8.t (id integer PRIMARY KEY, at timestamptz, w integer); \
         CREATE FUNCTION c8.same(timestamptz, integer) RETURNS timestamptz LANGUAGE sql IMMUTABLE AS 'SELECT $1'; \
         CREATE FUNCTION c8.later(timestamptz, integer) RETURNS timestamptz LANGUAGE sql IMMUTABLE AS 'SELECT $1 + interval ''1 day'''; \
         CREATE FUNCTION c8.laterv(timestamptz, integer) RETURNS timestamptz LANGUAGE sql STABLE AS 'SELECT $1 + interval ''1 day''';";
    let mut w = Snail::open_writing(&written("volatile", ddl)).unwrap();
    rows(&mut w, "INSERT INTO c8.t VALUES (1, '2024-01-01 00:00+00', 1), (2, '2024-01-02 00:00+00', 0)");
    asked(
        &mut w,
        "SELECT id, c8.same(at, 100 / w) FROM c8.t ORDER BY id",
        &["1|2024-01-01 00:00:00+00", "2|2024-01-02 00:00:00+00"],
    );
    assert_eq!(
        refused(&mut w, "SELECT id, c8.later(at, 100 / w) FROM c8.t ORDER BY id"),
        "division by zero"
    );
    asked(
        &mut w,
        "SELECT id, c8.laterv(at, 100 / w) FROM c8.t ORDER BY id",
        &["1|2024-01-02 00:00:00+00", "2|2024-01-03 00:00:00+00"],
    );
}
