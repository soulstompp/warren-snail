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

/// PostgreSQL's words for a call no form of a function takes, with their HINT.
fn no_function(call: &str) -> String {
    format!(
        "function {call} does not exist\nHINT:  No function matches the given name and argument \
         types. You might need to add explicit type casts."
    )
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
        ("SELECT c1.plus(1.5, 2)", no_function("c1.plus(numeric, integer)")),
        ("SELECT c1.plus(1)", no_function("c1.plus(integer)")),
        ("SELECT c1.plus(1, 2, 3)", no_function("c1.plus(integer, integer, integer)")),
        ("SELECT plus(1, 2)", no_function("plus(integer, integer)")),
        ("SELECT c1.plus(2147483647, 1)", "integer out of range".to_string()),
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
        ("SELECT ''a''::text", "integer", "return type mismatch in function declared to return integer\nDETAIL:  Actual return type is text."),
        ("SELECT ''5''", "integer", "return type mismatch in function declared to return integer\nDETAIL:  Actual return type is text."),
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

// A body whose subquery reads a relation is made as 18.6 makes it: a name is the column of a
// relation in reach before it is a parameter, a parameter the subquery's relations do not hold is
// read as one, and a column no relation in reach holds is refused in 18.6's words. Each printed as
// 18.6 prints it.
#[test]
fn a_body_s_subquery_reads_its_relations_columns_before_the_parameters() {
    let tables = "CREATE SCHEMA dn; CREATE TABLE dn.a (id integer PRIMARY KEY, g integer, k integer); \
         CREATE TABLE dn.b (id integer PRIMARY KEY, v integer);";
    let made = [
        "dn.f1() RETURNS integer LANGUAGE sql RETURN (SELECT max(g) FROM dn.a)",
        "dn.f2(x integer) RETURNS integer LANGUAGE sql RETURN (SELECT max(g) FROM dn.a WHERE id = x)",
        "dn.f3() RETURNS integer LANGUAGE sql AS 'SELECT (SELECT max(g) FROM dn.a)'",
        "dn.f4() RETURNS bigint LANGUAGE sql RETURN (SELECT sum(k) OVER (ORDER BY id ROWS BETWEEN \
         1 PRECEDING AND CURRENT ROW) FROM dn.a LIMIT 1)",
        "dn.f5(g integer) RETURNS integer LANGUAGE sql RETURN (SELECT max(g) FROM dn.a)",
        "dn.f6(x integer) RETURNS integer LANGUAGE sql RETURN (SELECT max(a.g) FROM dn.a a WHERE \
         a.id = x)",
        "dn.f7(x integer) RETURNS bigint LANGUAGE sql RETURN (SELECT count(*) FROM dn.a WHERE g = x \
         AND EXISTS (SELECT 1 FROM dn.b WHERE b.v = a.g))",
        "dn.f8(x integer) RETURNS integer LANGUAGE sql RETURN x + (SELECT max(v) FROM dn.b WHERE \
         b.id = x)",
        "dn.f9(x integer) RETURNS integer LANGUAGE sql RETURN (SELECT max(g) FROM dn.a WHERE id = \
         f9.x)",
        "dn.f10(x integer) RETURNS integer LANGUAGE sql AS 'SELECT (SELECT max(g) FROM dn.a WHERE \
         id = $1)'",
        "dn.f11(k integer) RETURNS integer LANGUAGE sql RETURN (SELECT max(v) FROM dn.b WHERE v > k)",
    ];
    let ddl = format!(
        "{tables} {}",
        made.iter().map(|f| format!("CREATE FUNCTION {f};")).collect::<String>()
    );
    let path = written("subquery-bodies", &ddl);
    let mut s = Snail::open(&path).unwrap();
    for (f, body) in [
        ("f2", "RETURN (SELECT max(a.g) AS max FROM dn.a WHERE (a.id = f2.x))"),
        ("f5", "RETURN (SELECT max(a.g) AS max FROM dn.a)"),
        (
            "f7",
            "RETURN (SELECT count(*) AS count FROM dn.a WHERE ((a.g = f7.x) AND (EXISTS (SELECT 1 \
             FROM dn.b WHERE (b.v = a.g)))))",
        ),
        ("f8", "RETURN (x + (SELECT max(b.v) AS max FROM dn.b WHERE (b.id = f8.x)))"),
        ("f11", "RETURN (SELECT max(b.v) AS max FROM dn.b WHERE (b.v > f11.k))"),
    ] {
        let q = format!("SELECT pg_get_functiondef('dn.{f}'::regproc)");
        let got = question(&mut s, &q).unwrap_or_else(|e| panic!("{q}: {e}"));
        let last = got[0].lines().last().unwrap_or_default().to_string();
        assert_eq!(last, body, "{f}");
    }
    for (body, why) in [
        ("RETURN (SELECT max(zz) FROM dn.a)", "column \"zz\" does not exist"),
        ("RETURN (SELECT max(q.g) FROM dn.a)", "missing FROM-clause entry for table \"q\""),
        ("RETURN (SELECT max(a.zz) FROM dn.a a)", "column a.zz does not exist"),
        (
            "RETURN (SELECT max(gg) FROM dn.a)",
            "column \"gg\" does not exist\nHINT:  Perhaps you meant to reference the column \"a.g\".",
        ),
    ] {
        let ddl = format!("{tables} CREATE FUNCTION dn.e() RETURNS integer LANGUAGE sql {body};");
        let e = Catalog::parse(&ddl).err().unwrap_or_else(|| panic!("{body} was taken"));
        assert_eq!(e.to_string(), why, "{body}");
    }
    let ddl = format!(
        "{tables} CREATE FUNCTION dn.e(x integer) RETURNS integer LANGUAGE sql \
         RETURN (SELECT max(g) FROM dn.a WHERE id = y);"
    );
    assert_eq!(Catalog::parse(&ddl).unwrap_err().to_string(), "column \"y\" does not exist");
}

#[test]
fn create_function_s_and_a_named_call_s_refusals_carry_postgresql_s_codes() {
    // As 18.6 refused each on 28818 (2026-11-12), its VERBOSITY verbose.
    for (ddl, code) in [
        ("c4.bad(integer) RETURNS integer LANGUAGE sql AS 'SELECT ''a''::text'", "42P13"),
        ("c4.unk(integer) RETURNS integer LANGUAGE sql AS 'SELECT ''5'''", "42P13"),
        ("c10.e1(a integer DEFAULT 1, b integer) RETURNS integer RETURN a", "42P13"),
        ("c10.e2(a integer, a integer) RETURNS integer RETURN a", "42P13"),
        ("c10.e4(a integer DEFAULT true) RETURNS integer RETURN a", "42804"),
        ("c10.e6(a integer DEFAULT (SELECT 1)) RETURNS integer RETURN a", "0A000"),
        ("c10.e8(a integer) RETURNS integer LANGUAGE plpgsql RETURN a", "42P13"),
        ("c10.e9(a integer) RETURNS integer LANGUAGE sql AS 'SELECT 1' RETURN a", "42P13"),
        ("c10.e10(a integer) RETURNS integer RETURN 'x'::text", "42P13"),
        ("c10.e12(a integer) RETURNS integer BEGIN ATOMIC END", "42P13"),
        ("c10.e14(a integer) RETURNS integer RETURN 'x'", "42P13"),
        ("c10.e23(a integer) RETURNS integer AS 'SELECT a + 1'", "42P13"),
    ] {
        let ddl = format!("CREATE SCHEMA c4; CREATE SCHEMA c10; CREATE FUNCTION {ddl};");
        let e = Catalog::parse(&ddl).err().unwrap_or_else(|| panic!("{ddl} was taken"));
        assert_eq!(e.sqlstate(), Some(code), "{ddl}: {e}");
    }
    let path = written(
        "named-codes",
        "CREATE SCHEMA c9; \
         CREATE FUNCTION c9.named(a integer, b integer DEFAULT 2) RETURNS integer RETURN a + b;",
    );
    let mut s = Snail::open(&path).unwrap();
    for sql in [
        "SELECT c9.named(a => 1, a => 2)",
        "SELECT c9.named(a => 1, 2)",
    ] {
        let e = s.ask(sql, None).unwrap_err();
        assert_eq!(e.sqlstate(), Some("42601"), "{sql}: {e}");
    }
}

#[test]
fn a_recursion_past_the_stack_is_refused_as_postgresql_refuses_it() {
    let ddl = "CREATE SCHEMA c4; \
         CREATE FUNCTION c4.self(integer) RETURNS integer LANGUAGE sql AS 'SELECT c4.self($1 + 1)'; \
         CREATE FUNCTION c4.down(integer) RETURNS integer LANGUAGE sql AS 'SELECT CASE WHEN $1 <= 0 THEN 0 ELSE 1 + c4.down($1 - 1) END';";
    let path = written("recursion", ddl);
    let too_deep = "stack depth limit exceeded\nHINT:  Increase the configuration parameter \
         \"max_stack_depth\" (currently 2048kB), after ensuring the platform's stack depth limit is \
         adequate.";
    // On a thread its host gave a stack of its own and said so, as PostgreSQL 18.6 answered and
    // refused each on 28818 (2026-11-12): 8422 levels answered, 8423 refused.
    let given = path.clone();
    std::thread::Builder::new()
        .stack_size(1 << 30)
        .spawn(move || {
            warren_snail::read::stack_given(1 << 30);
            let mut s = Snail::open(&given).unwrap();
            assert_eq!(
                question(&mut s, "SELECT c4.down(0), c4.down(1), c4.down(3)").unwrap(),
                ["0|1|3"]
            );
            assert_eq!(question(&mut s, "SELECT c4.down(8422)").unwrap(), ["8422"]);
            for sql in [
                "SELECT c4.down(8423)",
                "SELECT c4.down(10000)",
                "SELECT c4.self(1)",
            ] {
                let e = s.ask(sql, None).unwrap_err();
                assert_eq!(
                    (e.sqlstate(), e.to_string().as_str()),
                    (Some("54001"), too_deep),
                    "{sql}"
                );
            }
        })
        .unwrap()
        .join()
        .unwrap();
    // On a thread whose host said nothing, a mebibyte from the outermost call: refused sooner, in
    // the same words, never past the thread's stack; a release build answers 300 levels on it.
    let mut s = Snail::open(&path).unwrap();
    let held = if cfg!(debug_assertions) { 3 } else { 300 };
    let asked = format!("SELECT c4.down({held})");
    assert_eq!(question(&mut s, &asked).unwrap(), [held.to_string()]);
    let past = question(&mut s, "SELECT c4.down(8422)").unwrap_err();
    assert_eq!(past, too_deep);
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

/// The four forms of a function's DDL (ledgers/pgcatalog.md, *Part 7*): a body after RETURN and in
/// BEGIN ATOMIC, a parameter's DEFAULT, named parameters, arrays.
const FORMS: &str = "CREATE SCHEMA n; \
     CREATE FUNCTION n.ret(a integer) RETURNS integer LANGUAGE sql RETURN a + 1; \
     CREATE FUNCTION n.atom(a integer, b integer) RETURNS integer LANGUAGE sql BEGIN ATOMIC SELECT a * b; END; \
     CREATE FUNCTION n.atom3(a integer) RETURNS integer BEGIN ATOMIC RETURN a + 1; END; \
     CREATE FUNCTION n.alone(a integer) RETURNS integer STRICT BEGIN ATOMIC SELECT a; END; \
     CREATE FUNCTION n.cased(a integer) RETURNS text BEGIN ATOMIC SELECT CASE WHEN a > 0 THEN 'up' ELSE 'down' END; END; \
     CREATE FUNCTION n.dflt(integer, integer DEFAULT 10) RETURNS integer LANGUAGE sql AS 'SELECT $1 + $2'; \
     CREATE FUNCTION n.named(a integer, b text DEFAULT 'x') RETURNS text LANGUAGE sql AS 'SELECT b || a::text'; \
     CREATE FUNCTION n.mix(a integer, integer) RETURNS integer RETURN a + $2; \
     CREATE FUNCTION n.eq(a integer = 5, b integer = 6) RETURNS integer RETURN a * 10 + b; \
     CREATE FUNCTION n.sd(a integer, b integer DEFAULT NULL) RETURNS integer LANGUAGE sql STRICT AS 'SELECT $1 + coalesce($2, 0)'; \
     CREATE FUNCTION n.calls(a integer) RETURNS text RETURN n.named(b => 'k', a => a); \
     CREATE FUNCTION n.arr(integer[]) RETURNS integer LANGUAGE sql AS 'SELECT $1[1]'; \
     CREATE FUNCTION n.arrn(xs text[], i integer) RETURNS text LANGUAGE sql RETURN xs[i]; \
     CREATE FUNCTION n.d2a(a int[] DEFAULT '{1,2}', b numeric(5,2) DEFAULT 1.5) RETURNS text RETURN a::text || b::text;";

#[test]
fn a_body_after_return_or_in_begin_atomic_is_called_and_its_file_opens_again() {
    let mut s = Snail::open(&written("forms-bodies", FORMS)).unwrap();
    assert_eq!(
        question(&mut s, "SELECT n.ret(1), n.atom(2, 3), n.atom3(2), n.alone(NULL), n.cased(1), n.cased(-1)").unwrap(),
        ["2|6|3|NULL|up|down"]
    );
    assert_eq!(
        question(&mut s, "SELECT n.arr(ARRAY[7, 8]), n.arr('{9,10}'), n.arrn(ARRAY['p', 'q'], 2), n.d2a()").unwrap(),
        ["7|9|q|{1,2}1.5"]
    );
}

#[test]
fn a_call_gives_its_arguments_by_place_then_by_name_and_the_defaults_fill_the_rest() {
    let mut s = Snail::open(&written("forms-calls", FORMS)).unwrap();
    assert_eq!(
        question(
            &mut s,
            "SELECT n.dflt(1), n.dflt(1, 2), n.named(5), n.named(b => 'z', a => 1), n.named(a => 2), \
             n.named(3, b => 'w'), n.named(a := 4), n.named(A => 1), n.eq(), n.eq(b => 1), n.eq(7), \
             n.sd(1), n.calls(9), n.mix(1, 2)"
        )
        .unwrap(),
        ["11|3|x5|z1|x2|w3|x4|x1|56|51|76|NULL|k9|3"]
    );
    for (sql, why) in [
        ("SELECT n.named(c => 1)", no_function("n.named(c => integer)")),
        ("SELECT n.dflt()", no_function("n.dflt()")),
        ("SELECT n.named(b => 'q')", no_function("n.named(b => unknown)")),
        ("SELECT n.named(1, a => 2)", no_function("n.named(integer, a => integer)")),
        ("SELECT n.dflt(a => 1)", no_function("n.dflt(a => integer)")),
        (
            "SELECT n.named(a => 1, a => 2)",
            "argument name \"a\" used more than once".to_string(),
        ),
        (
            "SELECT n.named(a => 1, 2)",
            "positional argument cannot follow named argument".to_string(),
        ),
    ] {
        assert_eq!(question(&mut s, sql).unwrap_err(), why, "{sql}");
    }
}

#[cfg(feature = "standalone")]
#[test]
fn a_default_is_drawn_as_an_argument_and_named_arguments_in_their_parameters_order() {
    let ddl = "CREATE SCHEMA d; CREATE SEQUENCE d.q; \
         CREATE FUNCTION d.vd(a integer DEFAULT nextval('d.q')::integer) RETURNS integer RETURN a + a; \
         CREATE FUNCTION d.twos(a integer, b integer) RETURNS integer LANGUAGE sql STRICT AS 'SELECT coalesce($1, 0) * 10 + coalesce($2, 0)';";
    let mut w = Snail::open_writing(&written("forms-drawn", ddl)).unwrap();
    asked(&mut w, "SELECT d.vd()", &["2"]);
    asked(&mut w, "SELECT d.vd()", &["4"]);
    asked(
        &mut w,
        "SELECT d.twos(b => nextval('d.q')::integer, a => nextval('d.q')::integer)",
        &["34"],
    );
}

#[test]
fn the_four_forms_stand_in_pg_proc_and_print_as_postgresql_prints_them() {
    let mut s = Snail::open(&written("forms-catalog", FORMS)).unwrap();
    let of = |s: &mut Snail, what: &str, name: &str| {
        question(s, &format!("SELECT {what} FROM pg_proc p WHERE p.proname = '{name}'"))
            .unwrap()
            .join(";")
    };
    assert_eq!(of(&mut s, "p.proargnames, p.pronargdefaults, p.prosrc", "named"), "{a,b}|1|SELECT b || a::text");
    assert_eq!(of(&mut s, "p.proargnames, p.pronargdefaults, p.prosrc", "mix"), "{a,\"\"}|0|");
    assert_eq!(of(&mut s, "p.proargnames, pg_get_expr(p.proargdefaults, 0)", "dflt"), "NULL|10");
    assert_eq!(of(&mut s, "pg_get_expr(p.proargdefaults, 0)", "eq"), "5, 6");
    assert_eq!(of(&mut s, "pg_get_expr(p.proargdefaults, 0)", "sd"), "NULL::integer");
    assert_eq!(of(&mut s, "pg_get_expr(p.proargdefaults, 0)", "d2a"), "'{1,2}'::integer[], 1.5");
    assert_eq!(
        of(&mut s, "pg_get_function_arguments(p.oid), pg_get_function_identity_arguments(p.oid), pg_get_function_arg_default(p.oid, 2)", "named"),
        "a integer, b text DEFAULT 'x'::text|a integer, b text|'x'::text"
    );
    assert_eq!(of(&mut s, "pg_get_function_arguments(p.oid)", "d2a"), "a integer[] DEFAULT '{1,2}'::integer[], b numeric DEFAULT 1.5");
    for (name, body) in [
        ("ret", "RETURN (a + 1)"),
        ("atom", "BEGIN ATOMIC\n SELECT (a * b);\nEND"),
        ("atom3", "BEGIN ATOMIC\n RETURN (a + 1);\nEND"),
        ("alone", "BEGIN ATOMIC\n SELECT a AS a;\nEND"),
        ("mix", "RETURN (a + \"\")"),
        ("calls", "RETURN n.named(b => 'k'::text, a => a)"),
        ("arrn", "RETURN (xs)[i]"),
        ("named", "NULL"),
    ] {
        assert_eq!(of(&mut s, "pg_get_function_sqlbody(p.oid)", name), body, "{name}");
    }
    assert_eq!(
        of(&mut s, "pg_get_functiondef(p.oid)", "ret"),
        "CREATE OR REPLACE FUNCTION n.ret(a integer)\n RETURNS integer\n LANGUAGE sql\nRETURN (a + 1)\n"
    );
    assert_eq!(
        question(&mut s, "SELECT count(*) FROM pg_depend d JOIN pg_proc p ON p.oid = d.objid WHERE p.proname = 'calls' AND d.refobjid = 'n.named'::regproc").unwrap(),
        ["1"]
    );
}

#[test]
fn a_view_calling_by_name_keeps_its_columns_in_the_catalog_s_facts() {
    let ddl = format!("{FORMS} CREATE VIEW n.v AS SELECT n.named(b => 'z', a => 1) AS x, n.dflt(1) AS y;");
    let facts = Catalog::parse(&ddl).unwrap().facts();
    let columns = facts
        .iter()
        .find(|f| f.kind == "view" && f.object == "n.v" && f.fact == "columns")
        .map(|f| f.value.clone());
    assert_eq!(columns.as_deref(), Some("x,y"));
}

#[test]
fn create_function_refuses_the_four_forms_in_postgresql_s_words() {
    for (f, why) in [
        ("e1(a integer DEFAULT 1, b integer) RETURNS integer RETURN a", "input parameters after one with a default value must also have defaults"),
        ("e2(a integer, a integer) RETURNS integer RETURN a", "parameter name \"a\" used more than once"),
        ("e3(a integer DEFAULT 'x') RETURNS integer RETURN a", "invalid input syntax for type integer: \"x\""),
        ("e4(a integer DEFAULT true) RETURNS integer RETURN a", "argument of DEFAULT must be type integer, not type boolean"),
        ("e5(a integer, b integer DEFAULT a) RETURNS integer RETURN a", "column \"a\" does not exist"),
        ("e6(a integer DEFAULT (SELECT 1)) RETURNS integer RETURN a", "cannot use subquery in DEFAULT expression"),
        ("e7(a integer DEFAULT count(*)) RETURNS integer RETURN a", "aggregate functions are not allowed in DEFAULT expressions"),
        ("e8(a integer) RETURNS integer LANGUAGE plpgsql RETURN a", "inline SQL function body only valid for language SQL"),
        ("e9(a integer) RETURNS integer LANGUAGE sql AS 'SELECT 1' RETURN a", "duplicate function body specified"),
        ("e10(a integer) RETURNS integer RETURN 'x'::text", "Actual return type is text."),
        ("e37(a integer DEFAULT generate_series(1, 2)) RETURNS integer RETURN a", "set-returning functions are not allowed in DEFAULT expressions"),
        ("e11(a integer) RETURNS integer RETURN c", "column \"c\" does not exist"),
        ("e12(a integer) RETURNS integer BEGIN ATOMIC END", "Function's final statement must be SELECT or INSERT/UPDATE/DELETE/MERGE RETURNING."),
        ("e14(a integer) RETURNS integer RETURN 'x'", "Actual return type is text."),
        ("e20(a integer) RETURNS integer RETURN a IMMUTABLE", "syntax error at or near \"IMMUTABLE\""),
        ("e23(a integer) RETURNS integer AS 'SELECT a + 1'", "no language specified"),
    ] {
        let ddl = format!("CREATE SCHEMA r; CREATE FUNCTION r.{f};");
        let e = Catalog::parse(&ddl).unwrap_err().to_string();
        assert!(e.contains(why), "{f}: {e}");
    }
}
