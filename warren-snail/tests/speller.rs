//! The PostgreSQL warren-speller writes for MySQL's statements, answered as PostgreSQL 18.6 answers
//! it: each refusal with its SQLSTATE and in its words, a qualifier an alias hides looked up as
//! PostgreSQL's parser looks it up, a CHECK holding BETWEEN or an ARRAY of literals read on every
//! row, a boolean's casts to and from an integer, a column's default set and dropped, a write
//! returning rows of what it changed, its `old` and `new` among them. Every word,
//! code and answer is 18.6's over the same statements (`session.nu --suite speller --codes`). A
//! snail writes on its own only with `standalone`, as `tests/ddl.rs`.
#![cfg(feature = "standalone")]

mod weather;

use warren_snail::catalog::Catalog;
use warren_snail::read::Snail;
use weather::{tmp, write};

const DDL: &str = "CREATE TABLE s.base (id integer NOT NULL, PRIMARY KEY (id));
     CREATE TABLE items (id integer NOT NULL, PRIMARY KEY (id));";

/// A snail holding no rows, its tables declared by statements as a host declares them.
fn snail(name: &str) -> Snail {
    let path = tmp(&format!("speller-{name}.snail"));
    write(&path, &Catalog::parse(DDL).unwrap(), &[]).unwrap();
    Snail::open_writing(&path).unwrap()
}

fn run(w: &mut Snail, sql: &str) {
    w.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
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
fn each_refusal_carries_postgresqls_sqlstate_in_its_words() {
    let mut w = snail("codes");
    run(
        &mut w,
        "CREATE TABLE s.f (id integer NOT NULL, i smallint, j integer, k bigint, n numeric(3, 1), \
         r real, v varchar(2), d date, iv interval, CONSTRAINT \"f_pkey \" PRIMARY KEY (id))",
    );
    run(&mut w, "CREATE TABLE s.p (id integer NOT NULL, CONSTRAINT p_pkey PRIMARY KEY (id))");
    run(
        &mut w,
        "CREATE TABLE s.c (id integer NOT NULL, p integer REFERENCES s.p (id) ON DELETE RESTRICT, \
         n integer NOT NULL, PRIMARY KEY (id))",
    );
    run(&mut w, "INSERT INTO s.p VALUES (1)");
    run(&mut w, "INSERT INTO s.c VALUES (1, 1, 0)");
    for (sql, code, words) in [
        ("INSERT INTO s.f (id, i) VALUES (1, 32768)", "22003", "smallint out of range"),
        ("INSERT INTO s.f (id, j) VALUES (1, 2147483648)", "22003", "integer out of range"),
        ("INSERT INTO s.f (id, k) VALUES (1, 9223372036854775808)", "22003", "bigint out of range"),
        (
            "INSERT INTO s.f (id, j) VALUES (1, '2147483648')",
            "22003",
            "value \"2147483648\" is out of range for type integer",
        ),
        (
            "INSERT INTO s.f (id, n) VALUES (1, 100)",
            "22003",
            "numeric field overflow: A field with precision 3, scale 1 must round to an absolute \
             value less than 10^2.",
        ),
        (
            "INSERT INTO s.f (id, r) VALUES (1, '1e39')",
            "22003",
            "\"1e39\" is out of range for type real",
        ),
        (
            "INSERT INTO s.f (id, r) VALUES (1, 1e39)",
            "22003",
            "\"1000000000000000000000000000000000000000\" is out of range for type real",
        ),
        (
            "INSERT INTO s.f (id, v) VALUES (1, 'abc')",
            "22001",
            "value too long for type character varying(2)",
        ),
        (
            "INSERT INTO s.f (id, j) VALUES (1, 'x')",
            "22P02",
            "invalid input syntax for type integer: \"x\"",
        ),
        (
            "INSERT INTO s.f (id, d) VALUES (1, 'x')",
            "22007",
            "invalid input syntax for type date: \"x\"",
        ),
        (
            "INSERT INTO s.f (id, d) VALUES (1, '2026-13-01')",
            "22008",
            "date/time field value out of range: \"2026-13-01\"",
        ),
        (
            "INSERT INTO s.f (id, iv) VALUES (1, 'x')",
            "22007",
            "invalid input syntax for type interval: \"x\"",
        ),
        (
            "INSERT INTO s.c VALUES (2, 9, 0)",
            "23503",
            "insert or update on table \"c\" violates foreign key constraint \"c_p_fkey\": Key \
             (p)=(9) is not present in table \"p\".",
        ),
        (
            "INSERT INTO s.c VALUES (3, 1, NULL)",
            "23502",
            "null value in column \"n\" of relation \"c\" violates not-null constraint: Failing \
             row contains (3, 1, null).",
        ),
        (
            "DELETE FROM s.p WHERE id = 1",
            "23001",
            "update or delete on table \"p\" violates RESTRICT setting of foreign key constraint \
             \"c_p_fkey\" on table \"c\": Key (id)=(1) is referenced from table \"c\".",
        ),
        (
            "CREATE TABLE s.f (id integer)",
            "42P07",
            "relation \"f\" already exists",
        ),
        (
            "CREATE SEQUENCE s.bad AS smallint MAXVALUE 40000",
            "22023",
            "MAXVALUE (40000) is out of range for sequence data type smallint",
        ),
    ] {
        assert_eq!(refused(&mut w, sql), (Some(code), words.to_string()), "{sql}");
    }
    // A numeric constant cast to an integer by assignment, half away from zero.
    run(&mut w, "INSERT INTO s.f (id, j, i) VALUES (1, 1.5, -2.5)");
    run(&mut w, "INSERT INTO s.f (id, j) VALUES (2, 7)");
    assert_eq!(rows(&mut w, "SELECT id, i, j FROM s.f ORDER BY id"), ["1|-3|2", "2|NULL|7"]);
    for (sql, code, words) in [
        (
            "INSERT INTO s.f (id, j) VALUES (1, 1)",
            "23505",
            "duplicate key value violates unique constraint \"f_pkey \": Key (id)=(1) already \
             exists.",
        ),
        (
            "SELECT (SELECT f.id FROM s.f f)",
            "21000",
            "more than one row returned by a subquery used as an expression",
        ),
        ("SELECT nosuch FROM s.f", "42703", "column \"nosuch\" does not exist"),
        ("SELECT * FROM s.nosuch", "42P01", "relation \"s.nosuch\" does not exist"),
        ("SELECT true + 1", "42883", "operator does not exist: boolean + integer"),
        (
            "ALTER TABLE s.f ADD CONSTRAINT \"f_pkey \" CHECK (id > 0)",
            "42710",
            "constraint \"f_pkey \" for relation \"f\" already exists",
        ),
        (
            "ALTER TABLE s.f ADD CONSTRAINT f_c CHECK (j > 5)",
            "23514",
            "check constraint \"f_c\" of relation \"f\" is violated by some row",
        ),
    ] {
        assert_eq!(refused(&mut w, sql), (Some(code), words.to_string()), "{sql}");
    }
}

#[test]
fn a_qualifier_an_alias_hides_is_looked_up_as_postgresqls_parser_looks_it_up() {
    let mut w = snail("qualifiers");
    for (sql, words) in [
        // A name alone finds a table only where the search path, public, holds it.
        (
            "SELECT x.id FROM s.base x WHERE base.id = 1",
            "missing FROM-clause entry for table \"base\"",
        ),
        (
            "SELECT s.base.id FROM s.base x",
            "invalid reference to FROM-clause entry for table \"base\"",
        ),
        (
            "SELECT items.id FROM items x",
            "invalid reference to FROM-clause entry for table \"items\"",
        ),
        (
            "SELECT nowhere.id FROM s.base x",
            "missing FROM-clause entry for table \"nowhere\"",
        ),
        // A WITH query by its name, in a join of steps.
        (
            "WITH c AS (SELECT id FROM s.base), d AS (SELECT id FROM s.base) SELECT c.id FROM c x \
             JOIN d ON d.id = x.id",
            "invalid reference to FROM-clause entry for table \"c\"",
        ),
    ] {
        assert_eq!(refused(&mut w, sql), (Some("42P01"), words.to_string()), "{sql}");
    }
}

#[test]
fn a_check_holding_between_or_an_array_of_literals_is_read_on_every_row() {
    let mut w = snail("checks");
    run(
        &mut w,
        "CREATE TABLE s.bw (id integer NOT NULL, a smallint CHECK (a BETWEEN -128 AND 127), b \
         integer CHECK (b NOT BETWEEN 10 AND 20), e interval(0) CHECK (e BETWEEN '-838:59:59' AND \
         '838:59:59' AND extract(day FROM e) = 0), CONSTRAINT \"bw_pkey \" PRIMARY KEY (id))",
    );
    run(
        &mut w,
        "INSERT INTO s.bw VALUES (1, -128, 9, '838:59:59'), (2, 127, 21, NULL), (3, NULL, NULL, \
         NULL)",
    );
    for (sql, words) in [
        (
            "INSERT INTO s.bw (id, a) VALUES (4, 128)",
            "new row for relation \"bw\" violates check constraint \"bw_a_check\": Failing row \
             contains (4, 128, null, null).",
        ),
        (
            "INSERT INTO s.bw (id, b) VALUES (5, 10)",
            "new row for relation \"bw\" violates check constraint \"bw_b_check\": Failing row \
             contains (5, null, 10, null).",
        ),
        (
            "INSERT INTO s.bw (id, e) VALUES (6, '1 day')",
            "new row for relation \"bw\" violates check constraint \"bw_e_check\": Failing row \
             contains (6, null, null, 1 day).",
        ),
    ] {
        assert_eq!(refused(&mut w, sql), (Some("23514"), words.to_string()), "{sql}");
    }
    run(
        &mut w,
        "CREATE TABLE s.ar (id integer NOT NULL, h text CHECK (string_to_array(h, ',') <@ \
         ARRAY['a', 'b']), n integer CHECK (n = ANY (ARRAY[1, 2, 3])), CONSTRAINT \"ar_pkey \" \
         PRIMARY KEY (id))",
    );
    run(&mut w, "INSERT INTO s.ar VALUES (1, 'a,b', 1), (2, '', 3), (3, NULL, NULL)");
    for (sql, words) in [
        (
            "INSERT INTO s.ar (id, h) VALUES (4, 'a,c')",
            "new row for relation \"ar\" violates check constraint \"ar_h_check\": Failing row \
             contains (4, a,c, null).",
        ),
        // Named after the one column it reads, inside ANY.
        (
            "INSERT INTO s.ar (id, n) VALUES (5, 4)",
            "new row for relation \"ar\" violates check constraint \"ar_n_check\": Failing row \
             contains (5, null, 4).",
        ),
    ] {
        assert_eq!(refused(&mut w, sql), (Some("23514"), words.to_string()), "{sql}");
    }
    assert_eq!(
        rows(&mut w, "SELECT id, h, n FROM s.ar ORDER BY id"),
        ["1|a,b|1", "2||3", "3|NULL|NULL"]
    );
}

#[test]
fn between_is_read_as_its_two_comparisons_in_a_step() {
    let mut w = snail("between");
    run(
        &mut w,
        "CREATE TABLE s.t (id integer NOT NULL, a integer, b integer, PRIMARY KEY (id))",
    );
    run(
        &mut w,
        "INSERT INTO s.t VALUES (1, -1, 9), (2, 5, 21), (3, NULL, NULL), (4, 5, 0), (5, 9, 5)",
    );
    assert_eq!(
        rows(
            &mut w,
            "WITH x AS (SELECT id, a BETWEEN 0 AND 5 AS inside, b NOT BETWEEN 0 AND 10 AS outside \
             FROM s.t) SELECT id, inside, outside FROM x ORDER BY id"
        ),
        ["1|f|f", "2|t|t", "3|NULL|NULL", "4|t|f", "5|f|f"]
    );
    // An aggregate standing in a BETWEEN alone groups the step.
    assert_eq!(
        rows(
            &mut w,
            "WITH x AS (SELECT 1 AS one FROM s.t HAVING count(*) BETWEEN 1 AND 9) SELECT one FROM x"
        ),
        ["1"]
    );
    assert!(rows(
        &mut w,
        "WITH x AS (SELECT 1 AS one FROM s.t HAVING count(*) BETWEEN 6 AND 9) SELECT one FROM x"
    )
    .is_empty());
    // And over a join of steps, which the pipeline reads.
    let joined = |range: &str| {
        format!(
            "WITH c AS (SELECT id, a FROM s.t), d AS (SELECT id FROM s.t) SELECT 1 AS one FROM c \
             JOIN d ON d.id = c.id HAVING count(*) BETWEEN {range}"
        )
    };
    assert_eq!(rows(&mut w, &joined("1 AND 9")), ["1"]);
    assert!(rows(&mut w, &joined("6 AND 9")).is_empty());
    // An aggregate inside one, in a HAVING.
    assert_eq!(
        rows(
            &mut w,
            "WITH x AS (SELECT a, count(*) AS n FROM s.t GROUP BY a HAVING count(*) BETWEEN 2 AND \
             3) SELECT a, n FROM x"
        ),
        ["5|2"]
    );
}

#[test]
fn a_boolean_casts_to_and_from_an_integer_alone() {
    let mut w = snail("booleans");
    run(
        &mut w,
        "CREATE TABLE s.b (id integer NOT NULL, f boolean, s smallint, l bigint, PRIMARY KEY (id))",
    );
    run(&mut w, "INSERT INTO s.b VALUES (1, true, 1, 1), (2, false, 0, 0), (3, NULL, NULL, NULL)");
    assert_eq!(
        rows(
            &mut w,
            "WITH x AS (SELECT id, CAST(f AS integer) AS n, CAST(id - 2 AS boolean) AS t FROM s.b) \
             SELECT id, n, t FROM x ORDER BY id"
        ),
        ["1|1|t", "2|0|f", "3|NULL|t"]
    );
    for (sql, words) in [
        (
            "WITH x AS (SELECT CAST(s AS boolean) AS t FROM s.b) SELECT t FROM x",
            "cannot cast type smallint to boolean",
        ),
        (
            "WITH x AS (SELECT CAST(f AS bigint) AS t FROM s.b) SELECT t FROM x",
            "cannot cast type boolean to bigint",
        ),
    ] {
        assert_eq!(refused(&mut w, sql), (Some("42846"), words.to_string()), "{sql}");
    }
}

#[test]
fn a_columns_default_is_set_and_dropped_on_a_table_holding_rows() {
    let mut w = snail("defaults");
    run(
        &mut w,
        "CREATE TABLE s.d (id integer NOT NULL, a integer DEFAULT 5, g integer GENERATED ALWAYS AS \
         (id * 2) STORED, i integer GENERATED ALWAYS AS IDENTITY, CONSTRAINT \"d_pkey \" PRIMARY \
         KEY (id))",
    );
    run(&mut w, "INSERT INTO s.d (id) VALUES (1)");
    run(&mut w, "ALTER TABLE s.d ALTER COLUMN a SET DEFAULT 7");
    run(&mut w, "INSERT INTO s.d (id) VALUES (2)");
    run(&mut w, "ALTER TABLE s.d ALTER a DROP DEFAULT");
    run(&mut w, "INSERT INTO s.d (id) VALUES (3)");
    assert_eq!(
        rows(&mut w, "SELECT id, a, g FROM s.d ORDER BY id"),
        ["1|5|2", "2|7|4", "3|NULL|6"]
    );
    for (sql, code, words) in [
        (
            "ALTER TABLE s.d ALTER COLUMN a SET DEFAULT 'x'",
            "22P02",
            "invalid input syntax for type integer: \"x\"",
        ),
        (
            "ALTER TABLE s.d ALTER COLUMN g SET DEFAULT 1",
            "42601",
            "column \"g\" of relation \"d\" is a generated column",
        ),
        (
            "ALTER TABLE s.d ALTER COLUMN i DROP DEFAULT",
            "42601",
            "column \"i\" of relation \"d\" is an identity column",
        ),
        (
            "ALTER TABLE s.d ALTER COLUMN nosuch SET DEFAULT 1",
            "42703",
            "column \"nosuch\" of relation \"d\" does not exist",
        ),
    ] {
        assert_eq!(refused(&mut w, sql), (Some(code), words.to_string()), "{sql}");
    }
}

#[test]
fn a_write_returns_rows_of_what_it_changed_its_old_and_new_among_them() {
    let mut w = snail("returning");
    run(
        &mut w,
        "CREATE TABLE s.w (k integer NOT NULL, a integer, b text, CONSTRAINT \"w_pkey \" PRIMARY \
         KEY (k))",
    );
    run(&mut w, "INSERT INTO s.w VALUES (1, 10, 'x'), (2, 20, 'y'), (3, NULL, 'z')");
    for (sql, want) in [
        ("INSERT INTO s.w (k, a) VALUES (4, 40) RETURNING *", vec!["4|40|NULL"]),
        (
            "INSERT INTO s.w (k, a) VALUES (5, 50), (6, 60) RETURNING k, a + 1, old.k, new.a AS n",
            vec!["5|51|NULL|50", "6|61|NULL|60"],
        ),
        (
            "UPDATE s.w SET a = a * 2 WHERE k <= 2 RETURNING k, old.a, new.a, a",
            vec!["1|10|20|20", "2|20|40|40"],
        ),
        ("UPDATE s.w SET b = 'q' WHERE k = 9 RETURNING k", vec![]),
        ("DELETE FROM s.w WHERE k = 6 RETURNING old.*, new.k", vec!["6|60|NULL|NULL"]),
        ("DELETE FROM s.w WHERE k = 5 RETURNING k, a", vec!["5|50"]),
        ("UPDATE s.w SET a = 1 WHERE k = 1 RETURNING old.*, new.*", vec!["1|20|x|1|1|x"]),
        ("UPDATE s.w AS x SET a = 2 WHERE x.k = 1 RETURNING x.k, old.a", vec!["1|1"]),
        (
            "UPDATE s.w SET a = 3 WHERE k = 1 RETURNING (old.a IS DISTINCT FROM new.a) AS moved, \
             record_image_ne(ROW(old.a), ROW(new.a))",
            vec!["t|t"],
        ),
        (
            "UPDATE s.w SET a = 3 WHERE k = 1 RETURNING record_image_ne(ROW(old.a, old.b), \
             ROW(new.a, new.b))",
            vec!["f"],
        ),
    ] {
        assert_eq!(rows(&mut w, sql), want, "{sql}");
    }
    // Each column named as PostgreSQL names it.
    let done = w
        .execute("UPDATE s.w SET a = 4 WHERE k = 1 RETURNING k, a + 1, old.a, record_image_ne(ROW(old.a), ROW(new.a))")
        .unwrap();
    let schema = done.answer.expect("an answer").schema();
    let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
    assert_eq!(names, ["k", "?column?", "a", "record_image_ne"]);
    // Two records compared by their images, a NULL field the same as a NULL field.
    assert_eq!(
        rows(
            &mut w,
            "SELECT record_image_ne(ROW(1.0::numeric), ROW(1.00::numeric)), \
             record_image_ne(ROW(1, 'a'), ROW(1, 'a')), record_image_ne(ROW(NULL::integer), \
             ROW(NULL::integer)), record_image_ne(ROW(NULL::integer), ROW(1))"
        ),
        ["t|f|f|t"]
    );
}
