//! The PostgreSQL warren-speller writes for MySQL's statements, answered as PostgreSQL 18.6 answers
//! it: each refusal with its SQLSTATE and in its words, a qualifier an alias hides looked up as
//! PostgreSQL's parser looks it up, a CHECK holding BETWEEN or an ARRAY of literals read on every
//! row, a boolean's casts to and from an integer, a column's default set and dropped, a write
//! returning rows of what it changed, its `old` and `new` among them, a write through a view reaching
//! its table, each CHECK OPTION met, an INSERT of fewer values than columns, the replica role
//! switching the foreign keys off, each parameter typed where PostgreSQL's parse analysis settles it
//! and refused where it cannot, the session's prepared statements in pg_prepared_statements, a
//! locking clause, a name written twice and a relation not held each refused with its code, a
//! literal read as the type its place gives it (a truth, a CASE's or a COALESCE's type, the other
//! side's integer, two rows' columns compared in turn), an element of an array by a subscript an
//! aggregate gives, a function in a subquery's FROM reading the row around it, the collation a call
//! is made under, an index keyed under its column's own collation. Every
//! word, code and answer is 18.6's over the same statements (`session.nu --suite speller --codes`). A
//! snail writes on its own only with `standalone`, as `tests/ddl.rs`.
#![cfg(feature = "standalone")]

mod weather;

use warren_snail::catalog::Catalog;
use warren_snail::read::Snail;
use weather::{tmp, write};

const DDL: &str = "CREATE SCHEMA s; CREATE TABLE s.base (id integer NOT NULL, PRIMARY KEY (id));
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

/// An answer's rows, each value as PostgreSQL's output function writes it under the session's
/// TimeZone, `|` between them.
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
    // Writes inside one WITH find their rows on one snapshot, the question reading it too; a write
    // nothing reads runs all the same.
    run(&mut w, "INSERT INTO s.w (k, a) VALUES (5, 50), (6, 60)");
    for (sql, want) in [
        (
            "WITH u AS (UPDATE s.w SET a = 0 WHERE k = 1 RETURNING k), d AS (DELETE FROM s.w WHERE \
             k = 5 RETURNING k) SELECT (SELECT count(*) FROM u) AS updated, (SELECT count(*) FROM \
             d) AS deleted, (SELECT count(*) FROM s.w) AS seen",
            vec!["1|1|6"],
        ),
        ("SELECT count(*) FROM s.w", vec!["5"]),
        (
            "WITH i AS (INSERT INTO s.w (k, a) VALUES (7, 70) RETURNING k) SELECT k FROM s.w \
             WHERE k = 7",
            vec![],
        ),
        ("WITH d AS (DELETE FROM s.w WHERE k = 7 RETURNING k) SELECT 1", vec!["1"]),
        ("SELECT count(*) FROM s.w WHERE k = 7", vec!["0"]),
        (
            "WITH updated AS (UPDATE s.w SET a = 10 WHERE k <= 3 RETURNING \
             record_image_ne(ROW(old.a), ROW(new.a)) AS changed) SELECT count(*) FILTER (WHERE \
             changed) AS changed, count(*) AS matched FROM updated",
            vec!["3|3"],
        ),
    ] {
        assert_eq!(rows(&mut w, sql), want, "{sql}");
    }
    assert_eq!(
        refused(&mut w, "WITH d AS (DELETE FROM s.w WHERE k = 6) SELECT count(*) FROM d"),
        (Some("0A000"), "WITH query \"d\" does not have a RETURNING clause".to_string())
    );
    assert_eq!(rows(&mut w, "SELECT count(*) FROM s.w WHERE k = 6"), ["1"]);
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

#[test]
fn on_conflict_passes_over_or_sets_the_row_its_key_meets() {
    let mut w = snail("conflict");
    run(
        &mut w,
        "CREATE TABLE s.w (k integer NOT NULL, a integer, b text, CONSTRAINT \"w_pkey \" PRIMARY \
         KEY (k))",
    );
    run(&mut w, "INSERT INTO s.w VALUES (1, 0, 'x'), (2, 40, 'y')");
    // MySQL's REPLACE as the speller writes it: a row's two counts where it replaced one.
    let replace = |k: i32| {
        format!(
            "WITH replaced AS (INSERT INTO s.w (k, a, b) VALUES ({k}, 5, 'r') ON CONFLICT (\"k\") \
             DO UPDATE SET \"k\" = EXCLUDED.\"k\", a = EXCLUDED.a, b = EXCLUDED.b RETURNING \
             (old.\"k\" IS NOT NULL) AS deleted) SELECT count(*) + count(*) FILTER (WHERE deleted) \
             AS affected FROM replaced"
        )
    };
    for (sql, want) in [
        (
            "INSERT INTO s.w (k, a) VALUES (1, 100) ON CONFLICT (k) DO UPDATE SET a = EXCLUDED.a + \
             s.w.a RETURNING k, a, old.a"
                .to_string(),
            vec!["1|100|0"],
        ),
        (
            "INSERT INTO s.w (k, a) VALUES (1, 100) ON CONFLICT (k) DO UPDATE SET a = 0 WHERE s.w.a \
             > 1000 RETURNING k"
                .to_string(),
            vec![],
        ),
        (
            "INSERT INTO s.w (k, a) VALUES (1, 100), (8, 80) ON CONFLICT (k) DO NOTHING RETURNING k"
                .to_string(),
            vec!["8"],
        ),
        (
            "INSERT INTO s.w (k, a) VALUES (9, 1), (9, 2) ON CONFLICT (k) DO NOTHING RETURNING k, a"
                .to_string(),
            vec!["9|1"],
        ),
        (
            "INSERT INTO s.w (k, a) VALUES (11, 1) ON CONFLICT DO NOTHING RETURNING k".to_string(),
            vec!["11"],
        ),
        (
            "INSERT INTO s.w AS x (k, a) VALUES (11, 5), (12, 6) ON CONFLICT (k) DO UPDATE SET a = \
             x.a + EXCLUDED.a WHERE x.a < 3 RETURNING k, a, old.a, new.a"
                .to_string(),
            vec!["11|6|1|6", "12|6|NULL|6"],
        ),
        (replace(2), vec!["2"]),
        (replace(20), vec!["1"]),
    ] {
        assert_eq!(rows(&mut w, &sql), want, "{sql}");
    }
    run(&mut w, "INSERT INTO s.w (k, a) VALUES (1, 7) ON CONFLICT ON CONSTRAINT \"w_pkey \" DO NOTHING");
    for (sql, code, words) in [
        (
            "INSERT INTO s.w (k, a) VALUES (10, 1), (10, 2) ON CONFLICT (k) DO UPDATE SET a = \
             EXCLUDED.a",
            "21000",
            "ON CONFLICT DO UPDATE command cannot affect row a second time",
        ),
        (
            "INSERT INTO s.w (k, a) VALUES (13, 1) ON CONFLICT (a) DO NOTHING",
            "42P10",
            "there is no unique or exclusion constraint matching the ON CONFLICT specification",
        ),
        (
            "INSERT INTO s.w (k, a) VALUES (13, 1) ON CONFLICT DO UPDATE SET a = 2",
            "42601",
            "ON CONFLICT DO UPDATE requires inference specification or constraint name",
        ),
        (
            "INSERT INTO s.w (k, a) VALUES (13, 1) ON CONFLICT ON CONSTRAINT nosuch DO NOTHING",
            "42704",
            "constraint \"nosuch\" for table \"w\" does not exist",
        ),
    ] {
        assert_eq!(refused(&mut w, sql), (Some(code), words.to_string()), "{sql}");
    }
    assert_eq!(
        rows(&mut w, "SELECT k, a, b FROM s.w ORDER BY k"),
        [
            "1|100|x",
            "2|5|r",
            "8|80|NULL",
            "9|1|NULL",
            "11|6|NULL",
            "12|6|NULL",
            "20|5|r"
        ]
    );
}

#[test]
fn update_from_and_delete_using_change_each_row_a_join_meets_once() {
    let mut w = snail("joined");
    run(&mut w, "CREATE TABLE s.t (k integer NOT NULL, a integer, PRIMARY KEY (k))");
    run(&mut w, "CREATE TABLE s.u (k integer NOT NULL, y integer, PRIMARY KEY (k))");
    run(&mut w, "CREATE TABLE s.v (id integer NOT NULL, t_k integer, PRIMARY KEY (id))");
    run(&mut w, "INSERT INTO s.t VALUES (1, NULL), (2, 5), (3, 1), (4, 2)");
    run(&mut w, "INSERT INTO s.u VALUES (1, 100), (2, 200)");
    run(&mut w, "INSERT INTO s.v VALUES (1, 1), (2, 1), (3, 3)");
    run(&mut w, "UPDATE s.t AS a SET a = b.y FROM s.u AS b WHERE b.k = a.k");
    assert_eq!(
        rows(&mut w, "SELECT k, a FROM s.t ORDER BY k"),
        ["1|100", "2|200", "3|1", "4|2"]
    );
    for (sql, want) in [
        // Two rows of v meet k 1: it is set once.
        (
            "UPDATE s.t AS a SET a = 7 FROM s.v AS v WHERE v.t_k = a.k RETURNING a.k",
            vec!["1", "3"],
        ),
        (
            "WITH updated AS (UPDATE s.t AS a SET a = b.y FROM s.u AS b WHERE b.k = a.k RETURNING \
             record_image_ne(ROW(old.a), ROW(new.a)) AS changed) SELECT count(*) FILTER (WHERE \
             changed) AS changed, count(*) AS matched FROM updated",
            vec!["1|2"],
        ),
        ("DELETE FROM s.t AS a USING s.v AS v WHERE v.t_k = a.k RETURNING a.k", vec!["1", "3"]),
        ("SELECT k, a FROM s.t ORDER BY k", vec!["2|200", "4|2"]),
    ] {
        assert_eq!(rows(&mut w, sql), want, "{sql}");
    }
}

#[test]
fn a_write_through_a_view_reaches_its_table_each_check_option_met() {
    let mut w = snail("through");
    run(&mut w, "CREATE TABLE s.u (id integer NOT NULL, a integer, b text DEFAULT 'd', PRIMARY KEY (id))");
    run(&mut w, "CREATE TABLE s.w (id integer NOT NULL, z integer, PRIMARY KEY (id))");
    run(&mut w, "INSERT INTO s.u VALUES (1, 5), (2, 50), (3, 7)");
    run(&mut w, "INSERT INTO s.w VALUES (1, 100), (3, 4), (2, 8)");
    run(&mut w, "CREATE VIEW s.v AS SELECT id AS k, a AS n, a + 1 AS m FROM s.u t WHERE t.a < 10");
    run(&mut w, "CREATE VIEW s.cv AS SELECT id, a FROM s.u WHERE a < 10 WITH CHECK OPTION");
    run(&mut w, "CREATE VIEW s.pu AS SELECT id, a FROM s.u WHERE a < 10");
    run(&mut w, "CREATE VIEW s.uc AS SELECT id, a FROM s.pu WHERE a > 0 WITH CASCADED CHECK OPTION");
    run(&mut w, "CREATE VIEW s.ul AS SELECT id, a FROM s.pu WHERE a > 0 WITH LOCAL CHECK OPTION");
    run(&mut w, "CREATE VIEW s.e AS SELECT a + 1 AS m, 2 AS c FROM s.u");
    run(&mut w, "CREATE VIEW s.g AS SELECT a, count(*) AS c FROM s.u GROUP BY a");
    run(&mut w, "CREATE VIEW s.og AS SELECT a FROM s.g");
    for (sql, want) in [
        // An ON CONFLICT's update meets a row the view does not show.
        (
            "INSERT INTO s.v (k, n) VALUES (2, 1) ON CONFLICT (k) DO UPDATE SET n = 3 RETURNING *",
            vec!["2|3|4"],
        ),
        (
            "INSERT INTO s.v AS q (k, n) VALUES (1, 9) ON CONFLICT (k) DO UPDATE SET n = q.n + \
             excluded.m RETURNING k, n, m",
            vec!["1|15|16"],
        ),
        ("INSERT INTO s.cv VALUES (2, 1) ON CONFLICT (id) DO UPDATE SET a = 4 RETURNING *", vec!["2|4"]),
        ("INSERT INTO s.v (k, n) VALUES (4, 1) RETURNING k, n, m", vec!["4|1|2"]),
        ("SELECT id, a, b FROM s.u ORDER BY id", vec!["1|15|d", "2|4|d", "3|7|d", "4|1|d"]),
        // UPDATE … FROM finds only the rows the view shows.
        (
            "WITH d AS (UPDATE s.v SET n = w.z FROM s.w WHERE v.k = w.id RETURNING v.k, old.m, new.m) \
             SELECT * FROM d ORDER BY k",
            vec!["2|5|9", "3|8|5"],
        ),
        (
            "WITH d AS (UPDATE s.v x SET n = n + 1 FROM s.w WHERE x.k = w.id AND w.z < 10 \
             RETURNING x.k, x.n) SELECT * FROM d ORDER BY k",
            vec!["2|9", "3|5"],
        ),
        ("DELETE FROM s.v USING s.w WHERE v.k = w.id AND w.z = 100 RETURNING old.*", vec![]),
        ("UPDATE s.v SET n = 2 WHERE k = 4 RETURNING k, old.n, new.n, old.m, new.m", vec!["4|1|2|2|3"]),
        ("DELETE FROM s.e WHERE m = 3 RETURNING *", vec!["3|2"]),
        ("SELECT id, a FROM s.u ORDER BY id", vec!["1|15", "2|9", "3|5"]),
        ("INSERT INTO s.ul VALUES (21, 50) RETURNING *", vec!["21|50"]),
        (
            "SELECT table_name, check_option, is_updatable, is_insertable_into FROM \
             information_schema.views WHERE table_schema = 's' ORDER BY 1",
            vec![
                "cv|CASCADED|YES|YES",
                "e|NONE|NO|NO",
                "g|NONE|NO|NO",
                "og|NONE|NO|NO",
                "pu|NONE|YES|YES",
                "uc|CASCADED|YES|YES",
                "ul|LOCAL|YES|YES",
                "v|NONE|YES|YES",
            ],
        ),
        (
            "SELECT table_name, column_name, is_updatable FROM information_schema.columns WHERE \
             table_schema = 's' AND table_name IN ('v', 'e', 'u') ORDER BY 1, ordinal_position",
            vec![
                "e|m|NO", "e|c|NO", "u|id|YES", "u|a|YES", "u|b|YES", "v|k|YES", "v|n|YES",
                "v|m|NO",
            ],
        ),
        // DELETE alone through a view none of whose columns is its relation's.
        (
            "SELECT pg_relation_is_updatable('s.e'::regclass, false), \
             pg_relation_is_updatable('s.og'::regclass, false), \
             pg_relation_is_updatable('s.v'::regclass, false), \
             pg_relation_is_updatable('s.u'::regclass, false), \
             pg_column_is_updatable('s.v'::regclass, 3::smallint, false), \
             pg_column_is_updatable('s.v'::regclass, 1::smallint, false), \
             pg_relation_is_updatable(0, false), pg_relation_is_updatable('s.u_pkey'::regclass, false)",
            vec!["16|0|28|28|f|t|0|0"],
        ),
        (
            "SELECT relname, reloptions FROM pg_class WHERE relname IN ('cv', 'ul', 'v') ORDER BY 1",
            vec!["cv|{check_option=cascaded}", "ul|{check_option=local}", "v|NULL"],
        ),
    ] {
        assert_eq!(rows(&mut w, sql), want, "{sql}");
    }
    for (sql, code, words) in [
        (
            "INSERT INTO s.cv VALUES (5, 70)",
            "44000",
            "new row violates check option for view \"cv\": Failing row contains (5, 70, d).",
        ),
        (
            "UPDATE s.cv SET a = w.z * 2 FROM s.w WHERE cv.id = w.id",
            "44000",
            "new row violates check option for view \"cv\": Failing row contains (2, 16, d).",
        ),
        (
            "INSERT INTO s.cv VALUES (2, 1) ON CONFLICT (id) DO UPDATE SET a = 99",
            "44000",
            "new row violates check option for view \"cv\": Failing row contains (2, 99, d).",
        ),
        // CASCADED checks the views beneath; LOCAL its own alone.
        (
            "INSERT INTO s.uc VALUES (20, 50)",
            "44000",
            "new row violates check option for view \"pu\": Failing row contains (20, 50, d).",
        ),
        (
            "INSERT INTO s.ul VALUES (22, -1)",
            "44000",
            "new row violates check option for view \"ul\": Failing row contains (22, -1, d).",
        ),
        (
            "INSERT INTO s.v (k, m) VALUES (11, 1)",
            "0A000",
            "cannot insert into column \"m\" of view \"v\": View columns that are not columns of \
             their base relation are not updatable.",
        ),
        (
            "INSERT INTO s.v (k, n) VALUES (1, 2) ON CONFLICT (k) DO UPDATE SET m = 1",
            "0A000",
            "cannot insert into column \"m\" of view \"v\": View columns that are not columns of \
             their base relation are not updatable.",
        ),
        (
            "UPDATE s.v SET m = 1",
            "0A000",
            "cannot update column \"m\" of view \"v\": View columns that are not columns of their \
             base relation are not updatable.",
        ),
        (
            "INSERT INTO s.e VALUES (1, 2)",
            "55000",
            "cannot insert into view \"e\": Views that have no updatable columns are not \
             automatically updatable.",
        ),
        (
            "UPDATE s.g SET a = 1",
            "55000",
            "cannot update view \"g\": Views containing GROUP BY are not automatically updatable.",
        ),
        (
            "DELETE FROM s.og",
            "55000",
            "cannot delete from view \"g\": Views containing GROUP BY are not automatically updatable.",
        ),
        (
            "INSERT INTO s.v (k, n) VALUES (1, 2) ON CONFLICT (m) DO NOTHING",
            "42P10",
            "there is no unique or exclusion constraint matching the ON CONFLICT specification",
        ),
        (
            "INSERT INTO s.v (k, n) VALUES (1, 2) ON CONFLICT ON CONSTRAINT u_pkey DO NOTHING",
            "42704",
            "constraint \"u_pkey\" for table \"v\" does not exist",
        ),
        (
            "CREATE VIEW s.gc AS SELECT a, count(*) AS c FROM s.u GROUP BY a WITH CHECK OPTION",
            "0A000",
            "WITH CHECK OPTION is supported only on automatically updatable views",
        ),
    ] {
        assert_eq!(refused(&mut w, sql), (Some(code), words.to_string()), "{sql}");
    }
}

#[test]
fn an_insert_of_fewer_values_fills_the_first_columns_and_the_rest_by_default() {
    let mut w = snail("lengths");
    run(&mut w, "CREATE TABLE s.u (id integer NOT NULL, a integer, b text DEFAULT 'd', PRIMARY KEY (id))");
    run(&mut w, "INSERT INTO s.u VALUES (1, 5), (2, 50)");
    run(&mut w, "INSERT INTO s.u VALUES (3)");
    run(&mut w, "INSERT INTO s.u SELECT id + 8, 2 FROM s.u WHERE id = 1");
    for (sql, words) in [
        ("INSERT INTO s.u VALUES (4, 1), (5)", "VALUES lists must all be the same length"),
        ("INSERT INTO s.u VALUES (6, 1, 'x', 9)", "INSERT has more expressions than target columns"),
        ("INSERT INTO s.u (id, a) VALUES (7)", "INSERT has more target columns than expressions"),
        ("INSERT INTO s.u (id) VALUES (8, 1)", "INSERT has more expressions than target columns"),
        ("INSERT INTO s.u SELECT 10, 2, 'y', 1", "INSERT has more expressions than target columns"),
        ("INSERT INTO s.u (id, a) SELECT 11", "INSERT has more target columns than expressions"),
    ] {
        assert_eq!(refused(&mut w, sql), (Some("42601"), words.to_string()), "{sql}");
    }
    assert_eq!(
        rows(&mut w, "SELECT * FROM s.u ORDER BY id"),
        ["1|5|d", "2|50|d", "3|NULL|d", "9|2|d"]
    );
}

#[test]
fn the_sequence_functions_answer_as_the_session_holds_its_sequences() {
    let mut w = snail("sequences");
    run(&mut w, "CREATE TABLE s.f (id integer NOT NULL, a integer, PRIMARY KEY (id))");
    run(&mut w, "CREATE SEQUENCE s.fs AS integer MAXVALUE 5 OWNED BY s.f.id");
    run(&mut w, "CREATE SEQUENCE s.fa");
    run(&mut w, "CREATE SEQUENCE s.fc CACHE 10");
    run(
        &mut w,
        "CREATE TABLE s.fg (id integer GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, a integer)",
    );
    run(&mut w, "CREATE TABLE s.\"F\" (id integer PRIMARY KEY, \"Col\" integer)");
    run(&mut w, "CREATE SEQUENCE s.\"o \" OWNED BY s.\"F\".\"Col\"");
    let undefined = |s: &str| format!("{s} is not yet defined in this session");
    assert_eq!(
        refused(&mut w, "SELECT currval('s.fs')"),
        (Some("55000"), undefined("currval of sequence \"fs\""))
    );
    assert_eq!(
        refused(&mut w, "SELECT lastval()"),
        (Some("55000"), undefined("lastval"))
    );
    for (sql, want) in [
        ("SELECT nextval('s.fs'), currval('s.fs'), lastval()", "1|1|1"),
        (
            "SELECT pg_typeof(nextval('s.fs')), pg_typeof(setval('s.fs', 2)), pg_typeof(lastval())",
            "bigint|bigint|bigint",
        ),
        ("SELECT currval('s.fs')", "2"),
        ("SELECT setval('s.fs', 3, false), currval('s.fs'), nextval('s.fs')", "3|2|3"),
        ("SELECT setval('s.fs', 4, true), currval('s.fs'), lastval()", "4|4|4"),
        ("SELECT nextval('s.fs')", "5"),
        ("SELECT setval('s.fs', 100, NULL), setval(NULL, 1)", "NULL|NULL"),
    ] {
        assert_eq!(rows(&mut w, sql), [want], "{sql}");
    }
    for (sql, code, words) in [
        (
            "SELECT nextval('s.fs')",
            "2200H",
            "nextval: reached maximum value of sequence \"fs\" (5)",
        ),
        (
            "SELECT setval('s.fs', 9)",
            "22003",
            "setval: value 9 is out of bounds for sequence \"fs\" (1..5)",
        ),
        ("SELECT nextval('s.nosuch')", "42P01", "relation \"s.nosuch\" does not exist"),
        (
            "SELECT nextval('s.f')",
            "42809",
            "cannot open relation \"f\": This operation is not supported for tables.",
        ),
        (
            "SELECT nextval('pg_tables')",
            "42809",
            "cannot open relation \"pg_tables\": This operation is not supported for views.",
        ),
        (
            "SELECT nextval('a.b.c')",
            "0A000",
            "cross-database references are not implemented: \"a.b.c\"",
        ),
        (
            "SELECT setval('s.fs', 1, 'x')",
            "22P02",
            "invalid input syntax for type boolean: \"x\"",
        ),
    ] {
        assert_eq!(refused(&mut w, sql), (Some(code), words.to_string()), "{sql}");
    }
    // setval moves no lastval; a CACHE's values are the session's until a setval throws them away.
    for (sql, want) in [
        ("SELECT nextval('s.fa'), setval('s.fs', 3, true), lastval()", "1|3|1"),
        ("SELECT nextval('s.fc'), nextval('s.fc')", "1|2"),
        ("SELECT setval('s.fc', 100, false), nextval('s.fc')", "100|100"),
        (
            "SELECT nextval('s.fc'), setval('s.fc', 5), nextval('s.fc'), currval('s.fc')",
            "101|5|6|6",
        ),
        ("SELECT nextval('s.fc')", "7"),
        ("SELECT nextval('s.fc')", "8"),
        ("SELECT currval('s.fc')", "8"),
        ("SELECT nextval('S.FA'), nextval('s.fa'::regclass)", "2|3"),
    ] {
        assert_eq!(rows(&mut w, sql), [want], "{sql}");
    }
    // Nothing the session holds of its sequences is rolled back, and a DEFAULT's draw counts.
    run(&mut w, "BEGIN");
    assert_eq!(rows(&mut w, "SELECT nextval('s.fa')"), ["4"]);
    run(&mut w, "ROLLBACK");
    assert_eq!(rows(&mut w, "SELECT currval('s.fa'), nextval('s.fa')"), ["4|5"]);
    assert_eq!(rows(&mut w, "INSERT INTO s.fg (a) VALUES (1) RETURNING id"), ["1"]);
    assert_eq!(rows(&mut w, "SELECT lastval(), currval('s.fg_id_seq')"), ["1|1"]);
    run(&mut w, "BEGIN READ ONLY");
    assert_eq!(
        refused(&mut w, "SELECT nextval('s.fa')"),
        (
            Some("25006"),
            "cannot execute nextval() in a read-only transaction".to_string()
        )
    );
    run(&mut w, "ROLLBACK");
    run(&mut w, "BEGIN READ ONLY");
    assert_eq!(rows(&mut w, "SELECT currval('s.fa'), lastval()"), ["5|1"]);
    run(&mut w, "ROLLBACK");
    assert_eq!(
        rows(
            &mut w,
            "SELECT pg_get_serial_sequence('s.f', 'id'), pg_get_serial_sequence('s.fg', 'id'), \
             pg_get_serial_sequence('s.f', 'a'), pg_get_serial_sequence('s.\"F\"', 'Col')"
        ),
        ["s.fs|s.fg_id_seq|NULL|s.\"o \""]
    );
    assert_eq!(
        refused(&mut w, "SELECT pg_get_serial_sequence('s.\"F\"', 'col')"),
        (
            Some("42703"),
            "column \"col\" of relation \"F\" does not exist".to_string()
        )
    );
    assert_eq!(
        refused(&mut w, "SELECT pg_get_serial_sequence('s.nosuch', 'id')"),
        (Some("42P01"), "relation \"s.nosuch\" does not exist".to_string())
    );
    assert_eq!(
        rows(
            &mut w,
            "SELECT setval(pg_get_serial_sequence('s.fg', 'id'), 41), \
             nextval(pg_get_serial_sequence('s.fg', 'id'))"
        ),
        ["41|42"]
    );
}

#[test]
fn a_draw_comes_in_the_rows_order_after_the_sort_for_the_rows_the_cut_reaches() {
    let mut w = snail("drawn");
    run(&mut w, "CREATE TABLE s.t (id integer PRIMARY KEY, a integer)");
    run(&mut w, "CREATE SEQUENCE s.q");
    run(&mut w, "INSERT INTO s.t VALUES (1, 10), (2, 20), (3, 30)");
    for (sql, want) in [
        (
            "SELECT x, nextval('s.q') FROM (VALUES (3), (1), (2)) v(x) ORDER BY x",
            &["1|1", "2|2", "3|3"][..],
        ),
        (
            "SELECT nextval('s.q'), x FROM (VALUES (2), (1)) v(x) ORDER BY 1 DESC",
            &["5|1", "4|2"],
        ),
        (
            "SELECT nextval('s.q') FROM (VALUES (1), (2), (3), (4), (5)) v(x) LIMIT 2",
            &["6", "7"],
        ),
        ("SELECT x, nextval('s.q') FROM (VALUES (1), (2)) v(x) OFFSET 1", &["2|9"]),
        (
            "SELECT x, nextval('s.q') FROM (VALUES (3), (1), (2)) v(x) ORDER BY x OFFSET 1 LIMIT 1",
            &["2|11"],
        ),
        ("SELECT (SELECT nextval('s.q')) FROM (VALUES (1), (2)) v(x)", &["12", "12"]),
        ("SELECT nextval('s.q') WHERE false", &[]),
        ("SELECT currval('s.q')", &["12"]),
        (
            "INSERT INTO s.t (id, a) SELECT nextval('s.q') + 100, t.a FROM s.t t WHERE t.a >= 20 \
             ORDER BY t.a DESC RETURNING id, a",
            &["113|30", "114|20"],
        ),
        (
            "UPDATE s.t SET a = nextval('s.q') WHERE id < 3 RETURNING id, a",
            &["1|15", "2|16"],
        ),
        (
            "SELECT id, a FROM s.t ORDER BY id",
            &["1|15", "2|16", "3|30", "113|30", "114|20"],
        ),
    ] {
        assert_eq!(rows(&mut w, sql), want, "{sql}");
    }
    // A DEFAULT drawn beside a call is drawn once, as the statement's run that stands draws it.
    run(
        &mut w,
        "CREATE TABLE s.g (id integer GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, a bigint)",
    );
    for want in ["1|17", "2|18"] {
        assert_eq!(
            rows(
                &mut w,
                "INSERT INTO s.g (a) SELECT nextval('s.q') FROM s.t t WHERE t.id = 3 RETURNING id, a"
            ),
            [want]
        );
    }
    assert_eq!(rows(&mut w, "SELECT currval('s.g_id_seq'), lastval()"), ["2|2"]);
    // Another writer's draws land past a snapshot held: the values drawn are the file's, and a
    // course that turns on one is the course its value gives.
    let path = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("speller-drawn.snail");
    let mut other = Snail::open_writing(&path).unwrap();
    run(&mut w, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(rows(&mut w, "SELECT count(*) FROM s.t"), ["5"]);
    assert_eq!(rows(&mut other, "SELECT nextval('s.q'), nextval('s.q')"), ["19|20"]);
    assert_eq!(
        rows(&mut w, "SELECT CASE WHEN nextval('s.q') > 19 THEN nextval('s.q') END"),
        ["22"]
    );
    assert_eq!(rows(&mut w, "SELECT currval('s.q')"), ["22"]);
    run(&mut w, "COMMIT");
    // A course that turns on such a value and then calls otherwise draws no value it does not use.
    run(&mut w, "CREATE SEQUENCE s.r");
    run(&mut w, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(rows(&mut w, "SELECT count(*) FROM s.t"), ["5"]);
    assert_eq!(rows(&mut other, "SELECT nextval('s.q'), nextval('s.q')"), ["23|24"]);
    assert_eq!(
        rows(
            &mut w,
            "SELECT CASE WHEN nextval('s.q') > 23 THEN nextval('s.r') + nextval('s.q') \
             ELSE nextval('s.q') END"
        ),
        ["27"]
    );
    assert_eq!(rows(&mut w, "SELECT currval('s.q'), currval('s.r')"), ["26|1"]);
    run(&mut w, "COMMIT");
    // An INSERT run again makes its rows again, over the values the file gave.
    run(&mut w, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(rows(&mut w, "SELECT count(*) FROM s.t"), ["5"]);
    assert_eq!(rows(&mut other, "SELECT nextval('s.q'), nextval('s.q')"), ["27|28"]);
    assert_eq!(
        rows(
            &mut w,
            "INSERT INTO s.g (a) SELECT nextval('s.q') FROM s.t t WHERE t.id = 3 RETURNING id, a"
        ),
        ["3|29"]
    );
    run(&mut w, "COMMIT");
}

/// What a statement said beside its answer, each line as PostgreSQL's client prints it.
fn said(w: &mut Snail, sql: &str) -> Vec<String> {
    w.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}")).notices
}

#[test]
fn a_name_in_a_schema_no_statement_made_is_refused_but_in_a_files_kept_statements() {
    let mut w = snail("unmade");
    run(&mut w, "CREATE TYPE s.mood AS ENUM ('a')");
    let unmade = (Some("3F000"), "schema \"nx\" does not exist".to_string());
    // Each as PostgreSQL 18.6 refuses it: the schema before anything else of the statement.
    for sql in [
        "CREATE TABLE nx.t (id integer PRIMARY KEY)",
        "CREATE TABLE nx.t (id integer PRIMARY KEY, m nosuchtype)",
        "CREATE TABLE IF NOT EXISTS nx.t (id integer PRIMARY KEY)",
        "CREATE TYPE nx.e AS ENUM ('a')",
        "CREATE DOMAIN nx.d AS integer",
        "CREATE SEQUENCE nx.q",
        "CREATE VIEW nx.v AS SELECT 1 AS a",
        "CREATE TYPE s.r AS RANGE (subtype = integer, multirange_type_name = nx.rm)",
        "CREATE TABLE s.u (id integer PRIMARY KEY, m nx.mood)",
        "CREATE TABLE s.u (id integer PRIMARY KEY, m nx.mood[])",
        "CREATE TABLE s.u (id integer PRIMARY KEY REFERENCES nx.p (id))",
        "CREATE INDEX i ON nx.t (id)",
        "ALTER TABLE nx.t ALTER COLUMN id SET DEFAULT 1",
        "DROP TABLE nx.t",
        "SELECT 'nx.mood'::regtype",
        "SELECT 'nx'::regnamespace",
        "SELECT 1::nx.mood",
        "SELECT nx.f(1)",
    ] {
        assert_eq!(refused(&mut w, sql), unmade, "{sql}");
    }
    // A name the schema does not hold, as 18.6 refuses it there.
    for (sql, code, words) in [
        ("CREATE TABLE s.u (id integer PRIMARY KEY, m s.nope)", "42704", "type \"s.nope\" does not exist"),
        ("CREATE TABLE s.u (id integer PRIMARY KEY DEFAULT nextval('nx.q'))", "42P01", "relation \"nx.q\" does not exist"),
        ("CREATE INDEX i ON s.missing (id)", "42P01", "relation \"s.missing\" does not exist"),
        ("INSERT INTO nx.t VALUES (1)", "42P01", "relation \"nx.t\" does not exist"),
    ] {
        assert_eq!(refused(&mut w, sql), (Some(code), words.to_string()), "{sql}");
    }
    run(&mut w, "CREATE TABLE public.base (id integer PRIMARY KEY)");
    assert_eq!(
        refused(&mut w, "CREATE TABLE base (id integer PRIMARY KEY)"),
        (Some("42P07"), "relation \"base\" already exists".into())
    );
    assert_eq!(
        said(&mut w, "ALTER TABLE IF EXISTS nx.t ALTER COLUMN id SET DEFAULT 1"),
        ["NOTICE:  relation \"t\" does not exist, skipping"]
    );
    assert_eq!(
        said(&mut w, "DROP TABLE IF EXISTS nx.t"),
        ["NOTICE:  schema \"nx\" does not exist, skipping"]
    );
    // A DDL handed to a snail says its CREATE SCHEMA, as PostgreSQL needs.
    let e = Catalog::parse("CREATE TABLE k.t (id integer PRIMARY KEY);").unwrap_err();
    assert_eq!((e.sqlstate(), e.to_string().as_str()), (Some("3F000"), "schema \"k\" does not exist"));
    // A file's kept statements read as they were written: a schema they name stands as one made,
    // through its tables' drop, and goes out with its CREATE SCHEMA.
    let path = tmp("speller-kept-schema.snail");
    let kept = Catalog::parse_kept("CREATE TABLE k.t (id integer PRIMARY KEY);", usize::MAX).unwrap();
    write(&path, &kept, &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    run(&mut w, "CREATE TABLE k.u (id integer PRIMARY KEY)");
    run(&mut w, "DROP TABLE k.t, k.u");
    run(&mut w, "CREATE TABLE k.v (id integer PRIMARY KEY)");
    assert_eq!(rows(&mut w, "SELECT count(*) FROM pg_namespace WHERE nspname = 'k'"), ["1"]);
    assert!(w.catalog.ddl_of("k.v").unwrap().starts_with("CREATE SCHEMA k;\n"));
}

#[test]
fn a_schema_is_made_and_dropped_with_what_depends_on_it() {
    let mut w = snail("schemas");
    run(&mut w, "CREATE SCHEMA a");
    run(&mut w, "CREATE TABLE a.t (id integer PRIMARY KEY)");
    run(&mut w, "CREATE SEQUENCE a.q OWNED BY a.t.id");
    run(&mut w, "INSERT INTO a.t (id) VALUES (1)");
    run(&mut w, "CREATE SCHEMA b");
    run(&mut w, "CREATE VIEW b.v AS SELECT id FROM a.t");
    run(&mut w, "CREATE VIEW b.w AS SELECT id FROM b.v");
    // Each answer as 18.6 gives it, its DETAIL after a colon.
    assert_eq!(
        refused(&mut w, "DROP SCHEMA a"),
        (
            Some("2BP01"),
            "cannot drop schema a because other objects depend on it: table a.t depends on schema \
             a\nview b.v depends on table a.t\nview b.w depends on view b.v"
                .into()
        )
    );
    assert_eq!(
        refused(&mut w, "DROP SCHEMA a, b"),
        (
            Some("2BP01"),
            "cannot drop desired object(s) because other objects depend on them: table a.t \
             depends on schema a\nview b.v depends on table a.t\nview b.w depends on view b.v"
                .into()
        )
    );
    assert_eq!(
        refused(&mut w, "CREATE SCHEMA a"),
        (Some("42P06"), "schema \"a\" already exists".into())
    );
    assert_eq!(
        said(&mut w, "CREATE SCHEMA IF NOT EXISTS a"),
        ["NOTICE:  schema \"a\" already exists, skipping"]
    );
    assert_eq!(
        refused(&mut w, "CREATE SCHEMA pg_a"),
        (
            Some("42939"),
            "unacceptable schema name \"pg_a\": The prefix \"pg_\" is reserved for system schemas."
                .into()
        )
    );
    assert_eq!(
        refused(&mut w, "DROP SCHEMA pg_catalog CASCADE"),
        (
            Some("2BP01"),
            "cannot drop schema pg_catalog because it is required by the database system".into()
        )
    );
    run(&mut w, "CREATE SEQUENCE a.c CACHE 5");
    assert_eq!(rows(&mut w, "SELECT nextval('a.q'), nextval('a.c')"), ["1|1"]);
    // A cascade's NOTICE counts what it reports: the sequence a.t's column owns goes with it,
    // unreported.
    assert_eq!(
        said(&mut w, "DROP SCHEMA a CASCADE"),
        ["NOTICE:  drop cascades to 4 other objects"]
    );
    assert_eq!(
        refused(&mut w, "SELECT currval('a.q')"),
        (Some("42P01"), "relation \"a.q\" does not exist".into())
    );
    // Made again, a sequence of the name has handed the session nothing, its cache none.
    run(&mut w, "CREATE SCHEMA a");
    run(&mut w, "CREATE SEQUENCE a.q");
    run(&mut w, "CREATE SEQUENCE a.c CACHE 5");
    assert_eq!(
        refused(&mut w, "SELECT currval('a.q')"),
        (Some("55000"), "currval of sequence \"q\" is not yet defined in this session".into())
    );
    assert_eq!(
        refused(&mut w, "SELECT lastval()"),
        (Some("55000"), "lastval is not yet defined in this session".into())
    );
    assert_eq!(rows(&mut w, "SELECT nextval('a.c')"), ["1"]);
    run(&mut w, "DROP SCHEMA a CASCADE");
    assert_eq!(
        refused(&mut w, "SELECT * FROM b.v"),
        (Some("42P01"), "relation \"b.v\" does not exist".into())
    );
    assert_eq!(
        rows(&mut w, "SELECT nspname FROM pg_namespace WHERE nspname IN ('a', 'b') ORDER BY 1"),
        ["b"]
    );
    assert_eq!(
        said(&mut w, "DROP SCHEMA IF EXISTS a, b"),
        ["NOTICE:  schema \"a\" does not exist, skipping"]
    );
    assert_eq!(rows(&mut w, "SELECT count(*) FROM pg_namespace WHERE nspname = 'b'"), ["0"]);
    // Made again, a table of the name is a table of its own, its sequence drawn from the start.
    run(&mut w, "CREATE SCHEMA a");
    run(&mut w, "CREATE TABLE a.t (id integer PRIMARY KEY, n text)");
    run(&mut w, "INSERT INTO a.t (id, n) VALUES (7, 'x')");
    assert_eq!(rows(&mut w, "SELECT id, n FROM a.t"), ["7|x"]);
    // `public` dropped as PostgreSQL drops it: refused while a table lies in it, then taken with
    // everything in it; a name with no schema then made nowhere, until public is made again.
    let (code, e) = refused(&mut w, "DROP SCHEMA public");
    assert_eq!(code, Some("2BP01"), "{e}");
    assert!(e.contains("table items depends on schema public"), "{e}");
    assert_eq!(
        said(&mut w, "DROP SCHEMA public CASCADE"),
        ["NOTICE:  drop cascades to table items"]
    );
    assert_eq!(rows(&mut w, "SELECT count(*) FROM pg_namespace WHERE nspname = 'public'"), ["0"]);
    assert_eq!(
        refused(&mut w, "CREATE TABLE gone (id integer PRIMARY KEY)"),
        (Some("3F000"), "no schema has been selected to create in".into())
    );
    assert_eq!(
        refused(&mut w, "CREATE TABLE public.gone (id integer PRIMARY KEY)"),
        (Some("3F000"), "schema \"public\" does not exist".into())
    );
    assert_eq!(
        refused(&mut w, "SELECT 'public'::regnamespace"),
        (Some("3F000"), "schema \"public\" does not exist".into())
    );
    run(&mut w, "CREATE SCHEMA public");
    run(&mut w, "CREATE TABLE back (id integer PRIMARY KEY)");
    assert_eq!(
        rows(
            &mut w,
            "SELECT n.nspname, n.oid <> 2200 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relname = 'back'"
        ),
        ["public|t"]
    );
    // The release's own, refused by name.
    assert!(refused(&mut w, "DROP SCHEMA information_schema CASCADE").1.contains("release's own"));
    assert!(refused(&mut w, "CREATE SCHEMA c AUTHORIZATION u").1.contains("a snail has none"));
}

#[test]
fn a_function_a_key_and_an_identity_are_dropped_as_postgresql_drops_them() {
    let path = tmp("speller-dropped-kinds.snail");
    let ddl = "CREATE SCHEMA f;
        CREATE FUNCTION f.g(integer) RETURNS integer LANGUAGE sql IMMUTABLE AS 'SELECT $1 + 1';
        CREATE TABLE k (id integer PRIMARY KEY, k integer CHECK (f.g(k) > 0));
        CREATE TABLE f.r (id integer GENERATED ALWAYS AS IDENTITY PRIMARY KEY, n integer);";
    write(&path, &Catalog::parse(ddl).unwrap(), &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    assert_eq!(
        refused(&mut w, "DROP SCHEMA f"),
        (
            Some("2BP01"),
            "cannot drop schema f because other objects depend on it: function f.g(integer) \
             depends on schema f\nconstraint k_k_check on table k depends on function \
             f.g(integer)\ntable f.r depends on schema f"
                .into()
        )
    );
    assert_eq!(
        refused(&mut w, "DROP INDEX f.r_pkey"),
        (
            Some("2BP01"),
            "cannot drop index f.r_pkey because constraint r_pkey on table f.r requires it".into()
        )
    );
    assert_eq!(
        refused(&mut w, "DROP SEQUENCE f.r_id_seq"),
        (
            Some("2BP01"),
            "cannot drop sequence f.r_id_seq because column id of table f.r requires it".into()
        )
    );
    assert_eq!(
        refused(&mut w, "DROP TABLE pg_class"),
        (Some("42501"), "permission denied: \"pg_class\" is a system catalog".into())
    );
    assert_eq!(
        refused(&mut w, "DROP TABLE pg_tables"),
        (Some("42809"), "\"pg_tables\" is not a table".into())
    );
    assert_eq!(
        refused(&mut w, "DROP TABLE nosuch.t"),
        (Some("3F000"), "schema \"nosuch\" does not exist".into())
    );
    assert_eq!(
        refused(&mut w, "DROP TABLE f.r_pkey"),
        (Some("42809"), "\"r_pkey\" is not a table".into())
    );
    assert_eq!(
        said(&mut w, "DROP SCHEMA f CASCADE"),
        ["NOTICE:  drop cascades to 3 other objects"]
    );
    // The CHECK went with the function, the table it was on kept.
    assert_eq!(
        rows(&mut w, "SELECT conname FROM pg_constraint WHERE conrelid = 'k'::regclass ORDER BY 1"),
        ["k_id_not_null", "k_pkey"]
    );
    run(&mut w, "INSERT INTO k (id, k) VALUES (1, -5)");
    // The DDL a table is read back from, where its DDL drops anything, is the whole of it, and
    // reads to the table as it stands.
    let ddl = w.catalog.ddl_of("k").unwrap();
    let read = Catalog::parse(&ddl).unwrap();
    assert!(read.table("k").is_some_and(|t| t.checks.is_empty()));
}

#[test]
fn a_cascade_takes_the_parts_of_a_table_it_leaves_in_a_write_beside_it() {
    let mut w = snail("cascade-parts");
    run(&mut w, "CREATE SCHEMA d");
    run(&mut w, "CREATE SEQUENCE d.s");
    run(&mut w, "CREATE TABLE d.p (id integer PRIMARY KEY)");
    run(
        &mut w,
        "CREATE TABLE c (id integer PRIMARY KEY DEFAULT nextval('d.s'), p integer REFERENCES d.p (id))",
    );
    run(&mut w, "INSERT INTO d.p (id) VALUES (1)");
    run(&mut w, "INSERT INTO c (p) VALUES (1)");
    // The write's own rows, before the drop and after it, every one kept.
    run(&mut w, "BEGIN");
    run(&mut w, "INSERT INTO c (id, p) VALUES (2, 1)");
    assert_eq!(
        said(&mut w, "DROP SCHEMA d CASCADE"),
        ["NOTICE:  drop cascades to 4 other objects"]
    );
    run(&mut w, "INSERT INTO c (id, p) VALUES (5, 9)");
    run(&mut w, "COMMIT");
    assert_eq!(rows(&mut w, "SELECT id, p FROM c ORDER BY id"), ["1|1", "2|1", "5|9"]);
    // The DEFAULT went with the sequence, the foreign key with the table it referenced.
    assert_eq!(
        refused(&mut w, "INSERT INTO c (p) VALUES (9)"),
        (
            Some("23502"),
            "null value in column \"id\" of relation \"c\" violates not-null constraint: Failing \
             row contains (null, 9)."
                .into()
        )
    );
}

#[test]
fn a_table_declared_inside_an_unfinished_write_takes_every_write() {
    let mut w = snail("declared-inside");
    for end in ["ROLLBACK", "COMMIT"] {
        run(&mut w, "BEGIN");
        run(
            &mut w,
            "CREATE TABLE copy (slobol__place bigint NOT NULL, amount numeric(9,2), PRIMARY KEY \
             (slobol__place))",
        );
        run(&mut w, "INSERT INTO copy (slobol__place, amount) VALUES (1, 12.50)");
        run(&mut w, "INSERT INTO copy (slobol__place, amount) VALUES (2, 1.00)");
        run(&mut w, "INSERT INTO copy (slobol__place, amount) VALUES (3, 2.00), (4, 3.00)");
        run(&mut w, "DELETE FROM copy WHERE slobol__place IN (1)");
        run(&mut w, "UPDATE copy SET amount = amount + 1 WHERE slobol__place IN (2, 3)");
        assert_eq!(
            rows(&mut w, "SELECT slobol__place, amount FROM copy ORDER BY 1"),
            ["2|2.00", "3|3.00", "4|3.00"]
        );
        run(&mut w, end);
    }
    assert_eq!(rows(&mut w, "SELECT count(*) FROM copy"), ["3"]);
    // A failed statement fails its transaction, which then reads no name until it ends.
    run(&mut w, "BEGIN");
    run(&mut w, "CREATE TABLE made (id integer PRIMARY KEY)");
    assert!(w.execute("SELECT nosuch FROM made").is_err());
    assert_eq!(
        refused(&mut w, "INSERT INTO made (id) VALUES (1)"),
        (
            Some("25P02"),
            "current transaction is aborted, commands ignored until end of transaction block".into()
        )
    );
    run(&mut w, "ROLLBACK");
    // Under REPEATABLE READ: a table dropped and made again under its name, written beside
    // another, every write kept by the commit, the dropped table's rows gone with it.
    run(&mut w, "CREATE TABLE other (id integer PRIMARY KEY)");
    run(&mut w, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    run(&mut w, "INSERT INTO other VALUES (1)");
    run(&mut w, "INSERT INTO copy (slobol__place, amount) VALUES (9, 1.00)");
    run(&mut w, "DROP TABLE copy");
    run(
        &mut w,
        "CREATE TABLE copy (slobol__place bigint NOT NULL, note text, PRIMARY KEY (slobol__place))",
    );
    run(&mut w, "INSERT INTO copy VALUES (1, 'a'), (2, 'b')");
    run(&mut w, "INSERT INTO copy VALUES (3, 'c')");
    run(&mut w, "DELETE FROM copy WHERE slobol__place IN (2)");
    run(&mut w, "COMMIT");
    assert_eq!(rows(&mut w, "SELECT * FROM copy ORDER BY 1"), ["1|a", "3|c"]);
    assert_eq!(rows(&mut w, "SELECT * FROM other"), ["1"]);
    run(&mut w, "DROP TABLE copy, other");
    assert_eq!(
        refused(&mut w, "SELECT * FROM copy"),
        (Some("42P01"), "relation \"copy\" does not exist".into())
    );
}

#[test]
fn the_replica_role_neither_checks_nor_acts_on_a_foreign_key() {
    let mut w = snail("replica");
    run(&mut w, "CREATE TABLE s.u (id integer PRIMARY KEY)");
    run(
        &mut w,
        "CREATE TABLE s.t (id integer PRIMARY KEY, u integer REFERENCES s.u (id) ON DELETE RESTRICT)",
    );
    run(
        &mut w,
        "CREATE TABLE s.c (id integer PRIMARY KEY, u integer REFERENCES s.u (id) ON DELETE CASCADE \
         ON UPDATE CASCADE)",
    );
    run(
        &mut w,
        "CREATE TABLE s.df (id integer PRIMARY KEY, u integer REFERENCES s.u (id) DEFERRABLE \
         INITIALLY DEFERRED)",
    );
    run(
        &mut w,
        "CREATE TABLE s.ck (id integer PRIMARY KEY, v integer CHECK (v > 0))",
    );
    run(&mut w, "INSERT INTO s.u VALUES (1), (2), (3)");
    run(&mut w, "INSERT INTO s.t VALUES (1, 1)");
    run(&mut w, "INSERT INTO s.c VALUES (1, 2)");
    assert_eq!(rows(&mut w, "SHOW session_replication_role"), ["origin"]);
    assert_eq!(
        refused(&mut w, "SET session_replication_role = on"),
        (
            Some("22023"),
            "invalid value for parameter \"session_replication_role\": \"on\"".into()
        )
    );
    run(&mut w, "SET session_replication_role = 'REPLICA'");
    assert_eq!(rows(&mut w, "SHOW session_replication_role"), ["replica"]);
    // No key is checked, and none acts: an orphan taken, a RESTRICT and a CASCADE passed over.
    run(&mut w, "INSERT INTO s.t VALUES (2, 77)");
    run(&mut w, "DELETE FROM s.u WHERE id = 1");
    run(&mut w, "UPDATE s.u SET id = 20 WHERE id = 2");
    assert_eq!(rows(&mut w, "SELECT id, u FROM s.t ORDER BY id"), ["1|1", "2|77"]);
    assert_eq!(rows(&mut w, "SELECT id, u FROM s.c ORDER BY id"), ["1|2"]);
    // Every other constraint holds, and a key added is still checked on the rows it finds.
    assert_eq!(refused(&mut w, "INSERT INTO s.ck VALUES (1, -1)").0, Some("23514"));
    assert_eq!(refused(&mut w, "INSERT INTO s.t VALUES (1, 3)").0, Some("23505"));
    assert_eq!(
        refused(
            &mut w,
            "ALTER TABLE s.t ADD CONSTRAINT t_again FOREIGN KEY (u) REFERENCES s.u (id)"
        ),
        (
            Some("23503"),
            "insert or update on table \"t\" violates foreign key constraint \"t_again\": Key \
             (u)=(1) is not present in table \"u\"."
                .into()
        )
    );
    // local fires the keys as origin does.
    run(&mut w, "SET session_replication_role = local");
    assert_eq!(refused(&mut w, "INSERT INTO s.t VALUES (3, 99)").0, Some("23503"));
    run(&mut w, "RESET session_replication_role");
    assert_eq!(rows(&mut w, "SHOW session_replication_role"), ["origin"]);
    // A deferred check is the role's where its row was written, not where the commit is.
    run(&mut w, "BEGIN");
    run(&mut w, "INSERT INTO s.df VALUES (1, 66)");
    run(&mut w, "SET LOCAL session_replication_role = replica");
    run(&mut w, "INSERT INTO s.df VALUES (3, 68)");
    assert_eq!(refused(&mut w, "COMMIT").0, Some("23503"));
    run(&mut w, "BEGIN");
    run(&mut w, "SET LOCAL session_replication_role = replica");
    run(&mut w, "INSERT INTO s.df VALUES (2, 67)");
    run(&mut w, "SET LOCAL session_replication_role = origin");
    run(&mut w, "COMMIT");
    // A row written under origin and replaced under replica is checked as neither.
    run(&mut w, "BEGIN");
    run(&mut w, "INSERT INTO s.df VALUES (5, 70)");
    run(&mut w, "SET LOCAL session_replication_role = replica");
    run(&mut w, "UPDATE s.df SET u = 71 WHERE id = 5");
    run(&mut w, "COMMIT");
    assert_eq!(rows(&mut w, "SELECT id, u FROM s.df ORDER BY id"), ["2|67", "5|71"]);
    assert_eq!(rows(&mut w, "SHOW session_replication_role"), ["origin"]);
    // A role set in a transaction rolled back goes with it.
    run(&mut w, "BEGIN");
    run(&mut w, "SET session_replication_role = replica");
    run(&mut w, "ROLLBACK");
    assert_eq!(refused(&mut w, "INSERT INTO s.t VALUES (4, 98)").0, Some("23503"));
}

#[test]
fn the_session_time_zone_reads_and_writes_instants_as_postgresql_does() {
    let mut w = snail("zone");
    // Each SET's value as PostgreSQL's check_timezone reads it and SHOW writes it back.
    for (set, want) in [
        ("SET TIME ZONE 5.5", "<+05:30>-05:30"),
        ("SET TIME ZONE 5.123", "<+05:07:22>-05:07:22"),
        ("SET TIME ZONE -7", "<-07>+07"),
        ("SET TimeZone = '0x10'", "<+16>-16"),
        ("SET TIME ZONE INTERVAL '+05:30:20' HOUR TO MINUTE", "<+05:30>-05:30"),
        ("SET TimeZone = 'interval ''-02:00'''", "<-02>+02"),
        ("SET TimeZone = 'utc+3'", "UTC+3"),
        ("SET TIME ZONE UTC", "UTC"),
    ] {
        run(&mut w, set);
        assert_eq!(rows(&mut w, "SHOW TimeZone"), [want], "{set}");
    }
    // UTC's own names, spelled as PostgreSQL's directory spells them; under the feature zones they
    // are the directory's own.
    #[cfg(not(feature = "zones"))]
    for (set, want) in [("SET TimeZone = 'zulu'", "Zulu"), ("SET TimeZone = ':UTC'", "UTC")] {
        run(&mut w, set);
        assert_eq!(rows(&mut w, "SHOW TimeZone"), [want], "{set}");
    }
    for (set, code, words) in [
        (
            "SET TIME ZONE 168",
            "22023",
            "invalid value for parameter \"TimeZone\": \"168\": UTC timezone offset is out of range.",
        ),
        (
            "SET TIME ZONE INTERVAL '1 day'",
            "22023",
            "invalid value for parameter \"TimeZone\": \"INTERVAL '1 day'\": Cannot specify days in \
             time zone interval.",
        ),
        (
            "SET TIME ZONE INTERVAL '05:30' HOUR TO SECOND",
            "42601",
            "time zone interval must be HOUR or HOUR TO MINUTE",
        ),
        ("SET TIME ZONE TRUE", "42601", "syntax error at or near \"TRUE\""),
        ("SET TimeZone = ''", "22023", "invalid value for parameter \"TimeZone\": \"\""),
        ("SET TimeZone = 'UTC', 'GMT'", "22023", "SET timezone takes only one argument"),
    ] {
        assert_eq!(refused(&mut w, set), (Some(code), words.to_string()), "{set}");
    }
    // A build without the feature zones, on by default, refuses a named zone by name.
    #[cfg(not(feature = "zones"))]
    assert!(refused(&mut w, "SET TimeZone = 'Europe/Lisbon'").1.contains("feature zones"));
    run(&mut w, "CREATE TABLE s.z (id integer PRIMARY KEY, d timestamptz, l timestamp)");
    run(&mut w, "INSERT INTO s.z VALUES (1, '2026-11-08 12:00:00+00', '2026-11-08 12:00:00')");
    run(&mut w, "SET TimeZone = '<+05:30>-05:30'");
    run(&mut w, "INSERT INTO s.z VALUES (2, '2026-07-01 12:00:00', '2026-07-01 12:00:00')");
    // Every answer as 18.6 gave it under the same statements.
    assert_eq!(
        rows(&mut w, "SELECT id, d FROM s.z ORDER BY id"),
        ["1|2026-11-08 17:30:00+05:30", "2|2026-07-01 12:00:00+05:30"]
    );
    assert_eq!(
        rows(
            &mut w,
            "WITH x AS (SELECT id, CAST(d AS timestamp) AS s, CAST(d AS date) AS a, CAST(d AS timetz) \
             AS z FROM s.z) SELECT id, s, a, z FROM x ORDER BY id"
        ),
        [
            "1|2026-11-08 17:30:00|2026-11-08|17:30:00+05:30",
            "2|2026-07-01 12:00:00|2026-07-01|12:00:00+05:30"
        ]
    );
    // A day on the zone's clock that is not UTC's.
    assert_eq!(
        rows(
            &mut w,
            "WITH x AS (SELECT CAST(timestamptz '2026-07-01 20:00+00' AS date) AS a) SELECT a FROM x"
        ),
        ["2026-07-02"]
    );
    assert_eq!(rows(&mut w, "SELECT id FROM s.z WHERE l < d ORDER BY id"), ["1"]);
    assert_eq!(
        rows(&mut w, "SELECT id FROM s.z WHERE d < '2026-07-01 12:00:01' ORDER BY id"),
        ["2"]
    );
    assert_eq!(
        rows(&mut w, "SELECT d - interval '1 month 1 day 1 hour' FROM s.z WHERE id = 1"),
        ["2026-10-07 16:30:00+05:30"]
    );
    assert_eq!(
        rows(
            &mut w,
            "SELECT extract(hour FROM d), extract(timezone FROM d), extract(timezone_minute FROM d) \
             FROM s.z WHERE id = 1"
        ),
        ["17|19800|30"]
    );
    assert_eq!(
        rows(&mut w, "SELECT date_trunc('day', d), date_trunc('week', d) FROM s.z WHERE id = 1"),
        ["2026-11-08 00:00:00+05:30|2026-11-02 00:00:00+05:30"]
    );
    assert_eq!(
        rows(&mut w, "SELECT to_char(d, 'YYYY-MM-DD HH24:MI:SS TZ OF') FROM s.z WHERE id = 1"),
        ["2026-11-08 17:30:00 +05:30 +05:30"]
    );
    assert_eq!(
        rows(
            &mut w,
            "SELECT to_timestamp('2026-07-01 12:00', 'YYYY-MM-DD HH24:MI'), timestamp '2026-07-01 \
             12:00' AT TIME ZONE 'UTC'"
        ),
        ["2026-07-01 12:00:00+05:30|2026-07-01 17:30:00+05:30"]
    );
    assert_eq!(
        rows(
            &mut w,
            "SELECT current_date = CAST(now() AS date), localtimestamp = CAST(now() AS timestamp)"
        ),
        ["t|t"]
    );
    // A row's text is written under the zone the session stands in when it is written.
    let b = w.execute("SELECT d FROM s.z WHERE id = 1").unwrap().answer.unwrap();
    run(&mut w, "RESET TimeZone");
    assert_eq!(w.text_of(&b).unwrap(), [[Some("2026-11-08 12:00:00+00".to_string())]]);
    assert_eq!(warren_snail::read::answer_text(&b).unwrap(), w.text_of(&b).unwrap());
}

#[test]
fn a_ddl_literal_its_zone_or_its_clock_reads_is_kept_as_its_value_where_it_is_declared() {
    let mut w = snail("folded");
    run(&mut w, "SET TimeZone = '<+05>-05'");
    run(
        &mut w,
        "CREATE TABLE s.t (id integer PRIMARY KEY, \
         d timestamptz CONSTRAINT t_d CHECK (d >= '2026-01-01 00:00'), \
         e timestamptz DEFAULT '2026-06-01 00:00', \
         z timetz CONSTRAINT t_z CHECK (z <> '12:00'), \
         r tstzrange CONSTRAINT t_r CHECK (r <> '[2026-01-01, 2026-02-01)'), \
         a timestamptz[] DEFAULT '{2026-01-01 10:00}', \
         g boolean GENERATED ALWAYS AS (d > '2026-01-02') STORED, \
         n date DEFAULT 'today', l timestamp DEFAULT 'now', \
         k date CONSTRAINT t_k CHECK (k >= '2026-01-01'), x text DEFAULT '2026-01-01')",
    );
    run(
        &mut w,
        "CREATE DOMAIN s.dd AS timestamptz DEFAULT '2026-03-01 00:00' CONSTRAINT dd_c CHECK \
         (VALUE > '2000-01-01')",
    );
    run(&mut w, "CREATE INDEX t_part ON s.t (id) WHERE d < '2026-07-01'");
    run(
        &mut w,
        "CREATE VIEW s.v AS SELECT id, d, '2026-01-01'::timestamptz AS x, timestamptz \
         '2026-01-01 01:00' AS y FROM s.t WHERE d < '2026-03-01'",
    );
    run(&mut w, "ALTER TABLE s.t ALTER COLUMN e SET DEFAULT '2026-07-01 00:00'");
    run(&mut w, "ALTER TABLE s.t ADD CONSTRAINT t_e CHECK (e > '2020-01-01')");
    run(&mut w, "CREATE DOMAIN s.de AS timestamptz");
    run(
        &mut w,
        "CREATE TABLE s.q (id integer PRIMARY KEY, q s.de[] DEFAULT '{2026-01-01 10:00}', \
         m tstzmultirange DEFAULT '{[2026-01-01, 2026-02-01)}' CONSTRAINT q_m CHECK (m <> \
         '{[2026-03-01, 2026-04-01)}'))",
    );
    // Read under another zone, each is the value it was where it was declared, as 18.6 prints it.
    run(&mut w, "SET TimeZone = 'UTC'");
    assert_eq!(
        rows(
            &mut w,
            "SELECT adnum, pg_get_expr(adbin, adrelid) FROM pg_attrdef WHERE adrelid = \
             's.q'::regclass ORDER BY adnum"
        ),
        [
            "2|'{\"2026-01-01 05:00:00+00\"}'::s.de[]",
            "3|'{[\"2025-12-31 19:00:00+00\",\"2026-01-31 19:00:00+00\")}'::tstzmultirange",
        ]
    );
    assert_eq!(
        rows(&mut w, "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = 'q_m'"),
        ["CHECK ((m <> '{[\"2026-02-28 19:00:00+00\",\"2026-03-31 19:00:00+00\")}'::tstzmultirange))"]
    );
    assert_eq!(
        rows(
            &mut w,
            "SELECT conname, pg_get_constraintdef(oid) FROM pg_constraint WHERE conname IN \
             ('dd_c', 't_d', 't_e', 't_k', 't_r', 't_z') ORDER BY conname"
        ),
        [
            "dd_c|CHECK ((VALUE > '1999-12-31 19:00:00+00'::timestamp with time zone))",
            "t_d|CHECK ((d >= '2025-12-31 19:00:00+00'::timestamp with time zone))",
            "t_e|CHECK ((e > '2019-12-31 19:00:00+00'::timestamp with time zone))",
            "t_k|CHECK ((k >= '2026-01-01'::date))",
            "t_r|CHECK ((r <> '[\"2025-12-31 19:00:00+00\",\"2026-01-31 19:00:00+00\")'::tstzrange))",
            "t_z|CHECK ((z <> '12:00:00+05'::time with time zone))",
        ]
    );
    assert_eq!(
        rows(
            &mut w,
            "SELECT adnum, pg_get_expr(adbin, adrelid) FROM pg_attrdef WHERE adrelid = \
             's.t'::regclass AND adnum NOT IN (8, 9) ORDER BY adnum"
        ),
        [
            "3|'2026-06-30 19:00:00+00'::timestamp with time zone",
            "6|'{\"2026-01-01 05:00:00+00\"}'::timestamp with time zone[]",
            "7|(d > '2026-01-01 19:00:00+00'::timestamp with time zone)",
            "11|'2026-01-01'::text",
        ]
    );
    // The clock's literals, kept as the date and the instant they read where declared.
    assert_eq!(
        rows(
            &mut w,
            "SELECT adnum, pg_get_expr(adbin, adrelid) ~ '^''20[0-9-]+ ?[0-9:.]*''::' FROM \
             pg_attrdef WHERE adrelid = 's.t'::regclass AND adnum IN (8, 9) ORDER BY adnum"
        ),
        ["8|t", "9|t"]
    );
    assert_eq!(
        rows(&mut w, "SELECT pg_get_indexdef('s.t_part'::regclass)"),
        ["CREATE INDEX t_part ON s.t USING btree (id) WHERE (d < '2026-06-30 19:00:00+00'::timestamp \
          with time zone)"]
    );
    assert_eq!(
        rows(&mut w, "SELECT pg_get_expr(typdefaultbin, 0) FROM pg_type WHERE typname = 'dd'"),
        ["'2026-02-28 19:00:00+00'::timestamp with time zone"]
    );
    // And each row is taken or refused as 18.6 takes or refuses it.
    run(&mut w, "INSERT INTO s.t (id, d) VALUES (1, '2025-12-31 20:00:00+00')");
    assert_eq!(
        refused(&mut w, "INSERT INTO s.t (id, d) VALUES (2, '2025-12-31 18:00:00+00')").0,
        Some("23514")
    );
    assert_eq!(
        refused(&mut w, "INSERT INTO s.t (id, d, z) VALUES (3, '2026-01-01 00:00:00+00', '12:00+05')").0,
        Some("23514")
    );
    run(&mut w, "INSERT INTO s.t (id, d, z) VALUES (4, '2026-01-01 00:00:00+00', '12:00+00')");
    run(&mut w, "INSERT INTO s.t (id, d) VALUES (5, '2026-02-28 21:00:00+00')");
    assert_eq!(
        rows(
            &mut w,
            "SELECT id, d, e, a, g, l = (SELECT l FROM s.t WHERE id = 1) FROM s.t ORDER BY id"
        ),
        [
            "1|2025-12-31 20:00:00+00|2026-06-30 19:00:00+00|{\"2026-01-01 05:00:00+00\"}|f|t",
            "4|2026-01-01 00:00:00+00|2026-06-30 19:00:00+00|{\"2026-01-01 05:00:00+00\"}|f|t",
            "5|2026-02-28 21:00:00+00|2026-06-30 19:00:00+00|{\"2026-01-01 05:00:00+00\"}|t|t",
        ]
    );
    assert_eq!(
        rows(&mut w, "SELECT id, x, y FROM s.v ORDER BY id"),
        [
            "1|2025-12-31 19:00:00+00|2025-12-31 20:00:00+00",
            "4|2025-12-31 19:00:00+00|2025-12-31 20:00:00+00",
        ]
    );
    run(&mut w, "SET TimeZone = '<+05>-05'");
    assert_eq!(
        rows(&mut w, "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = 't_d'"),
        ["CHECK ((d >= '2026-01-01 00:00:00+05'::timestamp with time zone))"]
    );
    assert_eq!(
        rows(&mut w, "SELECT id, e FROM s.t ORDER BY id"),
        ["1|2026-07-01 00:00:00+05", "4|2026-07-01 00:00:00+05", "5|2026-07-01 00:00:00+05"]
    );
}

#[test]
fn a_parameter_is_typed_where_postgresql_settles_it_and_refused_where_it_cannot_be() {
    let mut w = snail("params");
    run(
        &mut w,
        "CREATE TABLE s.p (k integer NOT NULL, a varchar(5), n numeric(6,2), b boolean, PRIMARY KEY (k))",
    );
    run(&mut w, "INSERT INTO s.p VALUES (1, 'ab', 1.50, true), (2, 'cd', 2.25, false)");
    // PREPARE refused where 18.6's parse analysis cannot settle a parameter, in its words.
    for (sql, code, words) in [
        ("PREPARE a AS SELECT $1 + $2", "42725", "operator is not unique: unknown + unknown"),
        ("PREPARE a AS SELECT -$1", "42725", "operator is not unique: - unknown"),
        ("PREPARE a AS SELECT $1 IS NULL", "42P18", "could not determine data type of parameter $1"),
        ("PREPARE a AS SELECT $2", "42P18", "could not determine data type of parameter $1"),
        ("PREPARE a AS SELECT $1 IS NULL, $1 = 1", "42P08", "could not determine data type of parameter $1"),
        (
            "PREPARE a AS SELECT $1 WHERE $1 = 1",
            "42P08",
            "inconsistent types deduced for parameter $1: integer versus text",
        ),
        (
            "PREPARE a AS SELECT k FROM s.p WHERE a = $1 AND k = $1",
            "42883",
            "operator does not exist: integer = text",
        ),
        (
            "PREPARE a AS INSERT INTO s.p (k, a) VALUES ($1, $1)",
            "42P08",
            "inconsistent types deduced for parameter $1: integer versus character varying",
        ),
        (
            "PREPARE a AS UPDATE s.p SET k = $1 WHERE $1 IS NULL",
            "42P08",
            "could not determine data type of parameter $1",
        ),
        (
            "PREPARE a (int) AS SELECT k FROM s.p WHERE a = $1",
            "42883",
            "operator does not exist: character varying = integer",
        ),
        ("PREPARE a (nosuchtype) AS SELECT $1", "42704", "type \"nosuchtype\" does not exist"),
        ("PREPARE a AS SELECT $0", "42P02", "there is no parameter $0"),
    ] {
        assert_eq!(refused(&mut w, sql), (Some(code), words.to_string()), "{sql}");
    }
    // Each answered as its parameters' types answer it.
    run(&mut w, "PREPARE r AS SELECT abs($1)");
    assert_eq!(rows(&mut w, "EXECUTE r ('-2.5')"), ["2.5"]);
    run(&mut w, "PREPARE g (numeric) AS SELECT $1 + $2");
    assert_eq!(rows(&mut w, "EXECUTE g (1.5, 2)"), ["3.5"]);
    run(&mut w, "PREPARE i (bigint) AS SELECT k FROM s.p WHERE k = $1");
    assert_eq!(rows(&mut w, "EXECUTE i (2)"), ["2"]);
    run(&mut w, "PREPARE v (varchar(3)) AS SELECT $1");
    assert_eq!(rows(&mut w, "EXECUTE v ('abcdef')"), ["abcdef"]);
    run(&mut w, "PREPARE u AS UPDATE s.p SET n = n + $1 WHERE k = $2 RETURNING n * $3");
    assert_eq!(rows(&mut w, "EXECUTE u ('1.25', 1, 2)"), ["5.50"]);
    // EXECUTE's arguments read as EvaluateParams reads them.
    run(&mut w, "PREPARE t AS SELECT $1");
    run(&mut w, "PREPARE f AS UPDATE s.p SET b = $1 WHERE k = 1");
    for (sql, code, words) in [
        ("EXECUTE t ($1)", "42P02", "there is no parameter $1"),
        ("EXECUTE t ((SELECT 3))", "0A000", "cannot use subquery in EXECUTE parameter"),
        ("EXECUTE t (k)", "42703", "column \"k\" does not exist"),
        (
            "EXECUTE t",
            "42601",
            "wrong number of parameters for prepared statement \"t\": Expected 1 parameters but got 0.",
        ),
        (
            "EXECUTE f (1)",
            "42804",
            "parameter $1 of type integer cannot be coerced to the expected type boolean",
        ),
        ("PREPARE t AS SELECT 2", "42P05", "prepared statement \"t\" already exists"),
        ("EXECUTE nosuch (1)", "26000", "prepared statement \"nosuch\" does not exist"),
        ("DEALLOCATE nosuch", "26000", "prepared statement \"nosuch\" does not exist"),
    ] {
        assert_eq!(refused(&mut w, sql), (Some(code), words.to_string()), "{sql}");
    }
    // A host's statement given its parameters is read as a protocol's Parse reads it.
    let one = |v: &str| vec![v.to_string()];
    let e = w.execute_with("SELECT $1 + $2", &[one("1"), one("2")].concat()).unwrap_err();
    assert_eq!((e.sqlstate(), e.0.as_str()), (Some("42725"), "operator is not unique: unknown + unknown"));
    let e = w.execute_with("SELECT $1 IS NULL", &one("1")).unwrap_err();
    assert_eq!(e.sqlstate(), Some("42P18"));
    let e = w.ask_with("SELECT $1 WHERE $1 = 1", None, &one("1")).unwrap_err();
    assert_eq!(e.sqlstate(), Some("42P08"));
    let (b, _) = w.ask_with("SELECT k FROM s.p WHERE k = $1", None, &one("2")).unwrap();
    assert_eq!(b.num_rows(), 1);
}

#[test]
fn pg_prepared_statements_holds_the_sessions_statements_as_postgresql_shows_them() {
    let mut w = snail("prepared-view");
    run(&mut w, "CREATE DOMAIN s.dom AS varchar(4) CHECK (VALUE <> 'x')");
    run(&mut w, "CREATE TABLE s.t (k integer NOT NULL, a varchar(5), m s.dom, PRIMARY KEY (k))");
    assert_eq!(rows(&mut w, "SELECT count(*) FROM pg_prepared_statements"), ["0"]);
    // Each statement as the session was handed it, its parameters' types and its rows' as 18.6's
    // parse analysis settles them: none for a write without RETURNING.
    for sql in [
        "PREPARE q1 AS SELECT k FROM s.t WHERE k = $1 AND a = $2;",
        "PREPARE q2 AS INSERT INTO s.t (k, a) VALUES ($1, $2);",
        "PREPARE q3 (bigint, s.dom) AS SELECT $1, $2, 'x', NULL, m FROM s.t;",
        "PREPARE q4 AS DELETE FROM s.t WHERE k = $1 RETURNING k, m;",
        "PREPARE q5 AS SELECT FROM s.t;",
        "PREPARE   q6   AS   SELECT 1  ;",
    ] {
        run(&mut w, sql);
    }
    assert_eq!(
        rows(
            &mut w,
            "SELECT name, statement, parameter_types, result_types, from_sql FROM \
             pg_prepared_statements ORDER BY name"
        ),
        [
            "q1|PREPARE q1 AS SELECT k FROM s.t WHERE k = $1 AND a = $2;|{integer,text}|{integer}|t",
            "q2|PREPARE q2 AS INSERT INTO s.t (k, a) VALUES ($1, $2);|{integer,\"character varying\"}|NULL|t",
            "q3|PREPARE q3 (bigint, s.dom) AS SELECT $1, $2, 'x', NULL, m FROM s.t;|{bigint,s.dom}|{bigint,s.dom,text,text,s.dom}|t",
            "q4|PREPARE q4 AS DELETE FROM s.t WHERE k = $1 RETURNING k, m;|{integer}|{integer,s.dom}|t",
            "q5|PREPARE q5 AS SELECT FROM s.t;|{}|{}|t",
            "q6|PREPARE   q6   AS   SELECT 1  ;|{}|{integer}|t",
        ]
    );
    assert_eq!(
        rows(
            &mut w,
            "SELECT name, parameter_types[2], cardinality(result_types) FROM \
             pg_prepared_statement() ORDER BY name"
        ),
        [
            "q1|text|1",
            "q2|character varying|NULL",
            "q3|s.dom|5",
            "q4|NULL|2",
            "q5|NULL|0",
            "q6|NULL|1",
        ]
    );
    assert_eq!(
        rows(
            &mut w,
            "SELECT pg_typeof(prepare_time), pg_typeof(parameter_types) FROM \
             pg_prepared_statements WHERE name = 'q1'"
        ),
        ["timestamp with time zone|regtype[]"]
    );
    // Its time the PREPARE statement's own; a PREPARE outlives the transaction it was made in.
    run(&mut w, "BEGIN");
    run(&mut w, "PREPARE r1 AS SELECT 2");
    assert_eq!(
        rows(
            &mut w,
            "SELECT prepare_time >= now(), prepare_time < statement_timestamp() FROM \
             pg_prepared_statements WHERE name = 'r1'"
        ),
        ["t|t"]
    );
    run(&mut w, "ROLLBACK");
    run(&mut w, "DEALLOCATE q1");
    assert_eq!(
        rows(&mut w, "SELECT string_agg(name, ',' ORDER BY name) FROM pg_prepared_statements"),
        ["q2,q3,q4,q5,q6,r1"]
    );
    // A write reads them as a question does.
    run(&mut w, "CREATE TABLE s.names (n text NOT NULL, PRIMARY KEY (n))");
    run(
        &mut w,
        "INSERT INTO s.names (n) SELECT name FROM pg_prepared_statements WHERE name LIKE 'q_'",
    );
    assert_eq!(
        rows(&mut w, "SELECT string_agg(n, ',' ORDER BY n) FROM s.names"),
        ["q2,q3,q4,q5,q6"]
    );
    // A plan cache's counts, which a snail has none of, refused by name wherever they are read.
    for (sql, words) in [
        (
            "SELECT * FROM pg_prepared_statements",
            "generic_plans is a plan cache's count, only a running server's, and a snail has no \
             server: it is not read",
        ),
        (
            "SELECT custom_plans FROM pg_prepared_statements",
            "custom_plans is a plan cache's count, only a running server's, and a snail has no \
             server: it is not read",
        ),
        (
            "SELECT p.generic_plans FROM pg_prepared_statement() p",
            "generic_plans is a plan cache's count, only a running server's, and a snail has no \
             server: it is not read",
        ),
        (
            "SELECT pg_prepared_statement()",
            "pg_prepared_statement() in a select list gives its rows whole, generic_plans and \
             custom_plans among them, a plan cache's counts, and a snail has no plan cache: it is \
             read in FROM, by its columns",
        ),
    ] {
        assert_eq!(refused(&mut w, sql), (None, words.to_string()), "{sql}");
    }
    assert_eq!(
        refused(&mut w, "SELECT pg_prepared_statement(1)"),
        (Some("42883"), "function pg_prepared_statement(integer) does not exist".to_string())
    );
    // A statement whose types were not settled as PostgreSQL's parse analysis settles them refuses
    // the view while the session holds it, never answered with a guess.
    run(&mut w, "PREPARE z AS TABLE s.t");
    assert_eq!(
        refused(&mut w, "SELECT name FROM pg_prepared_statements"),
        (
            None,
            "the types of prepared statement \"z\" were not settled as PostgreSQL's parse \
             analysis settles them: pg_prepared_statement() is not read"
                .to_string()
        )
    );
    run(&mut w, "DEALLOCATE z");
    run(&mut w, "DEALLOCATE ALL");
    assert_eq!(rows(&mut w, "SELECT count(*) FROM pg_prepared_statements"), ["0"]);
}

#[test]
fn a_locking_clause_a_name_written_twice_and_a_relation_not_held_carry_their_codes() {
    let mut w = snail("named");
    run(&mut w, "CREATE TABLE s.t (id integer NOT NULL, a integer, PRIMARY KEY (id))");
    run(&mut w, "CREATE TABLE s.b (id integer NOT NULL, t_id integer, PRIMARY KEY (id))");
    run(&mut w, "INSERT INTO s.t VALUES (1, NULL), (2, 2)");
    run(&mut w, "INSERT INTO s.b VALUES (1, 1)");
    for (sql, code, words) in [
        ("SELECT id FROM s.nosuch", "42P01", "relation \"s.nosuch\" does not exist"),
        ("SELECT id FROM \"NoSuch\"", "42P01", "relation \"NoSuch\" does not exist"),
        ("SELECT id FROM nosuch WHERE id = 1 ORDER BY id", "42P01", "relation \"nosuch\" does not exist"),
        ("SELECT count(*) FROM s.nosuch", "42P01", "relation \"s.nosuch\" does not exist"),
        (
            "WITH w AS (SELECT id FROM s.nosuch) SELECT id FROM w",
            "42P01",
            "relation \"s.nosuch\" does not exist",
        ),
        ("SELECT row_number() OVER () FROM s.nosuch", "42P01", "relation \"s.nosuch\" does not exist"),
        (
            "SELECT t.id FROM s.t t JOIN s.nosuch n ON n.id = t.id",
            "42P01",
            "relation \"s.nosuch\" does not exist",
        ),
        ("SELECT count(*) FROM s.nosuch FOR UPDATE", "42P01", "relation \"s.nosuch\" does not exist"),
        ("COPY s.nosuch TO STDOUT", "42P01", "relation \"s.nosuch\" does not exist"),
        ("LOCK TABLE s.nosuch", "25P01", "LOCK TABLE can only be used in transaction blocks"),
        ("SAVEPOINT x", "25P01", "SAVEPOINT can only be used in transaction blocks"),
        ("SELECT count(*) FROM s.t FOR UPDATE", "0A000", "FOR UPDATE is not allowed with aggregate functions"),
        ("SELECT a FROM s.t GROUP BY a FOR SHARE", "0A000", "FOR SHARE is not allowed with GROUP BY clause"),
        (
            "SELECT t.id FROM s.t t LEFT JOIN s.b b ON b.t_id = t.id FOR UPDATE",
            "0A000",
            "FOR UPDATE cannot be applied to the nullable side of an outer join",
        ),
        (
            "SELECT t.id FROM s.t t FOR UPDATE OF b",
            "42P01",
            "relation \"b\" in FOR UPDATE clause not found in FROM clause",
        ),
        ("SELECT id FROM s.t LIMIT -1 FOR UPDATE", "2201W", "LIMIT must not be negative"),
        ("SELECT id FROM s.t OFFSET -1 FOR UPDATE", "2201X", "OFFSET must not be negative"),
        ("CREATE TABLE s.u (id integer, \"ID\" integer, Id integer)", "42701", "column \"id\" specified more than once"),
        ("CREATE VIEW s.v (x, x) AS SELECT id, a FROM s.t", "42701", "column \"x\" specified more than once"),
        ("INSERT INTO s.t (id, a, ID) VALUES (3, 4, 5)", "42701", "column \"id\" specified more than once"),
        ("INSERT INTO s.t (id, id, nosuch) VALUES (3, 4, 5)", "42701", "column \"id\" specified more than once"),
        ("UPDATE s.t SET a = 1, a = 2", "42601", "multiple assignments to same column \"a\""),
        ("SELECT x.id FROM s.t x JOIN s.b x ON true", "42712", "table name \"x\" specified more than once"),
        (
            "WITH w AS (SELECT 1), w AS (SELECT 2) SELECT * FROM w",
            "42712",
            "WITH query name \"w\" specified more than once",
        ),
        (
            "WITH w AS (SELECT id FROM s.t), w AS (SELECT a FROM s.t) SELECT * FROM w",
            "42712",
            "WITH query name \"w\" specified more than once",
        ),
        (
            "WITH w AS (SELECT id FROM s.t), w AS (SELECT a FROM s.t) SELECT id FROM w",
            "42712",
            "WITH query name \"w\" specified more than once",
        ),
        (
            "SELECT * FROM s.t JOIN s.b USING (id, id)",
            "42701",
            "column name \"id\" appears more than once in USING clause",
        ),
        (
            "SELECT * FROM (SELECT 1 AS a, 2 AS a) l NATURAL JOIN (SELECT 1 AS a) r",
            "42702",
            "common column name \"a\" appears more than once in left table",
        ),
    ] {
        assert_eq!(refused(&mut w, sql), (Some(code), words.to_string()), "{sql}");
    }
    // In a transaction block LOCK TABLE looks its tables up before its mode is read.
    run(&mut w, "BEGIN");
    assert_eq!(
        refused(&mut w, "LOCK TABLE s.nosuch IN ACCESS EXCLUSIVE MODE"),
        (Some("42P01"), "relation \"s.nosuch\" does not exist".to_string())
    );
    run(&mut w, "ROLLBACK");
    assert_eq!(rows(&mut w, "SELECT count(*) FROM s.t"), ["2"]);
}

#[test]
fn a_literal_is_read_as_the_type_its_place_gives_it() {
    let mut w = snail("literal");
    run(
        &mut w,
        "CREATE TABLE s.t (id integer NOT NULL, a integer, b text, ts timestamp(0), \
         n numeric(6,2), f boolean, PRIMARY KEY (id))",
    );
    run(&mut w, "CREATE TABLE s.e (id integer NOT NULL, PRIMARY KEY (id))");
    run(
        &mut w,
        "INSERT INTO s.t VALUES (1, 1, 'x', '2026-01-01 00:00', NULL, true), \
         (2, 2, 'y', '2026-01-01 00:00', 1.5, false), (3, NULL, NULL, NULL, NULL, NULL)",
    );
    // A literal standing as a condition, in a question and a write, alone and under NOT, AND, OR,
    // a CASE's WHEN, HAVING, ON and IS TRUE.
    for (sql, answer) in [
        ("SELECT id FROM s.t WHERE 't' ORDER BY id", vec!["1", "2", "3"]),
        ("SELECT id FROM s.t WHERE 'yes' AND a > 1 ORDER BY id", vec!["2"]),
        ("SELECT id FROM s.t WHERE a > 1 OR 'on' ORDER BY id", vec!["1", "2", "3"]),
        ("SELECT id FROM s.t WHERE NOT 'off' ORDER BY id", vec!["1", "2", "3"]),
        ("SELECT id FROM s.t WHERE NOT 't' ORDER BY id", vec![]),
        ("SELECT id FROM s.t WHERE 't' IS TRUE ORDER BY id", vec!["1", "2", "3"]),
        ("WITH w AS (SELECT id FROM s.t) SELECT id FROM w WHERE 'f' ORDER BY id", vec![]),
        ("SELECT count(*) FROM s.t HAVING 't'", vec!["3"]),
        ("SELECT x.id FROM s.t x JOIN s.t y ON 't' WHERE x.id = y.id ORDER BY x.id", vec!["1", "2", "3"]),
        ("SELECT CASE WHEN 't' THEN 1 ELSE 0 END, NOT 't', 't' AND 'f', 'f' OR 't'", vec!["1|f|f|t"]),
        (
            "SELECT count(*) FILTER (WHERE 'f'), count(*) FILTER (WHERE 't') FROM s.t",
            vec!["0|3"],
        ),
        (
            "SELECT id, count(*) FILTER (WHERE 'off') OVER () FROM s.t ORDER BY id",
            vec!["1|0", "2|0", "3|0"],
        ),
    ] {
        assert_eq!(rows(&mut w, sql), answer, "{sql}");
    }
    assert_eq!(w.execute("UPDATE s.t SET a = 10 WHERE 'f'").unwrap().rows, 0);
    // A CASE's arms in its type: a literal read by its input without the column's modifier, an
    // integer promoted to a numeric; a COALESCE's too, its NULL argument's place taken by its type.
    for (sql, answer) in [
        (
            "SELECT id, CASE WHEN a > 1 THEN '2026-06-01 00:00:00.6' ELSE ts END FROM s.t ORDER BY id",
            vec!["1|2026-01-01 00:00:00", "2|2026-06-01 00:00:00.6", "3|NULL"],
        ),
        (
            "SELECT id, CASE WHEN a > 1 THEN 1 ELSE 1.5 END / 3 FROM s.t ORDER BY id",
            vec!["1|0.50000000000000000000", "2|0.33333333333333333333", "3|0.50000000000000000000"],
        ),
        (
            "SELECT id, coalesce(ts, '2026-06-01 00:00:00.6') FROM s.t ORDER BY id",
            vec!["1|2026-01-01 00:00:00", "2|2026-01-01 00:00:00", "3|2026-06-01 00:00:00.6"],
        ),
        (
            "SELECT id, coalesce(n, 0) + '1.25' FROM s.t ORDER BY id",
            vec!["1|1.25", "2|2.75", "3|1.25"],
        ),
        (
            "SELECT id FROM s.t WHERE ROW(a, b) IS DISTINCT FROM ROW(2, 'y') ORDER BY id",
            vec!["1", "3"],
        ),
        (
            "SELECT id FROM s.t WHERE (a, b) IS NOT DISTINCT FROM (NULL, NULL) ORDER BY id",
            vec!["3"],
        ),
    ] {
        assert_eq!(rows(&mut w, sql), answer, "{sql}");
    }
    // The same in an UPDATE's SET and in ON CONFLICT's, each typed before any row.
    run(&mut w, "UPDATE s.t SET ts = CASE WHEN a > 1 THEN '2026-06-01 00:00:00.6' ELSE ts END");
    run(&mut w, "UPDATE s.t SET n = coalesce(n, 0) + '1.25' WHERE id = 1");
    run(&mut w, "UPDATE s.t SET f = ROW(a, b) IS DISTINCT FROM ROW(1, 'x')");
    run(
        &mut w,
        "INSERT INTO s.t (id) VALUES (3) ON CONFLICT (id) DO UPDATE SET n = coalesce(excluded.n, 0) + '0.5'",
    );
    assert_eq!(
        rows(&mut w, "SELECT id, ts, n, f FROM s.t ORDER BY id"),
        ["1|2026-01-01 00:00:00|1.25|f", "2|2026-06-01 00:00:01|1.50|t", "3|NULL|0.50|t"]
    );
    // Each refused as PostgreSQL's parser refuses it, before any row.
    for (sql, code, words) in [
        ("SELECT id FROM s.e WHERE 'x'", "22P02", "invalid input syntax for type boolean: \"x\""),
        ("SELECT id FROM s.e WHERE 1", "42804", "argument of WHERE must be type boolean, not type integer"),
        ("SELECT NOT 1", "42804", "argument of NOT must be type boolean, not type integer"),
        ("SELECT CASE WHEN 1 THEN 1 END", "42804", "argument of CASE/WHEN must be type boolean, not type integer"),
        ("SELECT id FROM s.e WHERE 1 AND true", "42804", "argument of AND must be type boolean, not type integer"),
        ("SELECT count(*) FROM s.e HAVING 1", "42804", "argument of HAVING must be type boolean, not type integer"),
        ("SELECT count(*) FILTER (WHERE 1) FROM s.e", "42804", "argument of FILTER must be type boolean, not type integer"),
        ("SELECT CASE WHEN true THEN 1 ELSE 'a' END", "22P02", "invalid input syntax for type integer: \"a\""),
        (
            "UPDATE s.t SET ts = CASE WHEN a > 1 THEN '2026-06-01' ELSE '2026-01-01' END",
            "42804",
            "column \"ts\" is of type timestamp without time zone but expression is of type text",
        ),
        ("SELECT id, a + '3000000000' FROM s.t", "22003", "value \"3000000000\" is out of range for type integer"),
        ("SELECT coalesce(a, 0) + '1.25' FROM s.t", "22P02", "invalid input syntax for type integer: \"1.25\""),
        ("SELECT id FROM s.t WHERE ROW(a, b) IS DISTINCT FROM ROW(2)", "42601", "unequal number of entries in row expressions"),
        ("SELECT id FROM s.t WHERE ROW(a, b) IS DISTINCT FROM ROW('x', 1)", "22P02", "invalid input syntax for type integer: \"x\""),
        ("SELECT id FROM s.t WHERE ROW(a, b) IS DISTINCT FROM ROW(2, 3)", "42883", "operator does not exist: text = integer"),
        ("WITH w AS (SELECT id, b FROM s.t) SELECT id FROM w WHERE b = 3", "42883", "operator does not exist: text = integer"),
    ] {
        assert_eq!(refused(&mut w, sql), (Some(code), words.to_string()), "{sql}");
    }
}

#[test]
fn a_subscript_an_aggregate_gives_and_a_function_reading_the_row_around_its_subquery_are_read() {
    let mut w = snail("positions");
    run(
        &mut w,
        "CREATE TABLE s.t (id integer NOT NULL, sz varchar(6), g integer, n numeric(4,1), \
         st varchar(20), PRIMARY KEY (id))",
    );
    run(
        &mut w,
        "INSERT INTO s.t VALUES (1, 'medium', 1, 1.4, 'a,c'), (2, 'small', 1, 2.6, 'b'), \
         (3, 'large', 2, NULL, ''), (4, NULL, 2, 0.5, NULL), (5, 'small', 3, 3.0, 'a,b,c')",
    );
    // An element of an array by a subscript an aggregate gives: in a select list, over groups, in
    // HAVING and ORDER BY, a slice's bounds, under a function; NULL over no rows.
    for (sql, answer) in [
        (
            "SELECT (ARRAY['small', 'medium', 'large'])[min(array_position(ARRAY['small', 'medium', 'large'], sz))] FROM s.t",
            vec!["small"],
        ),
        ("SELECT (ARRAY[10, 20, 30, 40, 50])[min(id)], (ARRAY[10, 20, 30, 40, 50])[max(id)] FROM s.t", vec!["10|50"]),
        ("SELECT (ARRAY['x'])[min(id)] FROM s.t WHERE id > 9", vec!["NULL"]),
        (
            "SELECT g, (ARRAY['small', 'medium', 'large'])[max(array_position(ARRAY['small', 'medium', 'large'], sz))] \
             FROM s.t GROUP BY g ORDER BY g",
            vec!["1|medium", "2|large", "3|small"],
        ),
        ("SELECT g FROM s.t GROUP BY g HAVING (ARRAY[false, true, false])[count(*)] ORDER BY g", vec!["1", "2"]),
        ("SELECT g FROM s.t GROUP BY g ORDER BY (ARRAY[3, 1, 2])[min(id)] NULLS FIRST, g", vec!["3", "2", "1"]),
        ("SELECT (ARRAY[1, 2, 3, 4, 5])[min(id):max(id) - 2] FROM s.t", vec!["{1,2,3}"]),
        (
            "SELECT upper((ARRAY['small', 'medium', 'large'])[min(array_position(ARRAY['small', 'medium', 'large'], sz))]) FROM s.t",
            vec!["SMALL"],
        ),
        // Each subscript coerced to integer as by assignment: a numeric rounded, a double to even,
        // an oid's bits, a quoted literal by integer's input.
        ("SELECT (ARRAY['a', 'b', 'c'])[avg(g)], (ARRAY['a', 'b', 'c'])[max(n)] FROM s.t", vec!["b|c"]),
        (
            "SELECT (ARRAY['a', 'b', 'c'])[2.5], (ARRAY['a', 'b', 'c'])[CAST(2.5 AS double precision)], \
             (ARRAY['a', 'b', 'c'])[CAST(2 AS oid)], (ARRAY['a', 'b', 'c'])['2']",
            vec!["c|b|b|b"],
        ),
    ] {
        assert_eq!(rows(&mut w, sql), answer, "{sql}");
    }
    // A set-returning function in a subquery's FROM reading the row of the query around it: in a
    // select list, WHERE and ORDER BY, by EXISTS and IN, with its ordinality.
    for (sql, answer) in [
        (
            "SELECT id, (SELECT count(*) FROM unnest(string_to_array(st, ',')) AS m) FROM s.t ORDER BY id",
            vec!["1|2", "2|1", "3|0", "4|0", "5|3"],
        ),
        (
            "SELECT id, (SELECT bit_or(1 << (array_position(ARRAY['a', 'b', 'c'], m) - 1)) \
             FROM unnest(string_to_array(st, ',')) AS m) FROM s.t ORDER BY id",
            vec!["1|5", "2|2", "3|NULL", "4|NULL", "5|7"],
        ),
        (
            "SELECT x.id FROM s.t AS x WHERE (SELECT sum(1 << (array_position(ARRAY['a', 'b', 'c'], m) - 1)) \
             FROM unnest(string_to_array(x.st, ',')) AS m) & 4 = 4 ORDER BY x.id",
            vec!["1", "5"],
        ),
        (
            "SELECT id FROM s.t ORDER BY (SELECT count(*) FROM unnest(string_to_array(st, ',')) AS m) DESC, id",
            vec!["5", "1", "2", "3", "4"],
        ),
        (
            "SELECT id FROM s.t WHERE EXISTS (SELECT 1 FROM unnest(string_to_array(st, ',')) AS m WHERE m = 'b') ORDER BY id",
            vec!["2", "5"],
        ),
        (
            "SELECT id FROM s.t WHERE 'c' IN (SELECT m FROM unnest(string_to_array(st, ',')) AS m) ORDER BY id",
            vec!["1", "5"],
        ),
        (
            "SELECT id, (SELECT max(n) FROM unnest(string_to_array(st, ',')) WITH ORDINALITY AS u(m, n) \
             WHERE m = 'c') FROM s.t ORDER BY id",
            vec!["1|2", "2|NULL", "3|NULL", "4|NULL", "5|3"],
        ),
        (
            "SELECT id, (SELECT count(*) FROM regexp_split_to_table(st, ',') AS m) FROM s.t ORDER BY id",
            vec!["1|2", "2|1", "3|1", "4|0", "5|3"],
        ),
    ] {
        assert_eq!(rows(&mut w, sql), answer, "{sql}");
    }
    // The same in an UPDATE's SET and a DELETE's WHERE.
    run(
        &mut w,
        "UPDATE s.t SET st = (SELECT string_agg(m, ',' ORDER BY m DESC) \
         FROM unnest(string_to_array(st, ',')) AS m) WHERE id IN (1, 5)",
    );
    let deleted = w
        .execute("DELETE FROM s.t WHERE (SELECT count(*) FROM unnest(string_to_array(st, ',')) AS m) = 0")
        .unwrap();
    assert_eq!(deleted.rows, 2);
    assert_eq!(rows(&mut w, "SELECT id, st FROM s.t ORDER BY id"), ["1|c,a", "2|b", "5|c,b,a"]);
    // A parameter as a subscript is typed integer, its argument assigned to it.
    run(&mut w, "PREPARE p11 AS SELECT (ARRAY['a', 'b', 'c'])[$1]");
    assert_eq!(rows(&mut w, "EXECUTE p11('2')"), ["b"]);
    assert_eq!(rows(&mut w, "EXECUTE p11(2.5)"), ["c"]);
    // An aggregate reading only the query around a subquery is that query's, never refused as the
    // subquery's own.
    let around = "SELECT g FROM s.t GROUP BY g HAVING EXISTS (SELECT 1 FROM s.t AS u \
                  WHERE u.id = (ARRAY[1, 3, 5])[count(t.id)])";
    assert_ne!(w.execute(around).err().and_then(|e| e.sqlstate()), Some("42803"), "{around}");
    // Each refused as PostgreSQL refuses it.
    for (sql, code, words) in [
        ("SELECT (ARRAY['a', 'b'])[sz] FROM s.t WHERE false", "42804", "array subscript must have type integer"),
        ("PREPARE p12 AS SELECT (ARRAY['a', 'b'])[sz] FROM s.t WHERE false", "42804", "array subscript must have type integer"),
        (
            "SELECT g FROM s.t GROUP BY g HAVING EXISTS (SELECT 1 FROM s.t AS u WHERE u.id = (ARRAY[1, 3, 5])[count(*)])",
            "42803",
            "aggregate functions are not allowed in WHERE",
        ),
        ("SELECT (ARRAY['a', 'b', 'c'])['x']", "22P02", "invalid input syntax for type integer: \"x\""),
        ("SELECT (ARRAY['a', 'b', 'c'])[1e10]", "22003", "integer out of range"),
        ("SELECT id FROM s.t WHERE (ARRAY[1, 2])[min(id)] = 1", "42803", "aggregate functions are not allowed in WHERE"),
        ("SELECT id FROM s.t WHERE min(id) = 1", "42803", "aggregate functions are not allowed in WHERE"),
        (
            "SELECT id FROM s.t WHERE (ARRAY[1, 2])[row_number() OVER ()] = 1",
            "42P20",
            "window functions are not allowed in WHERE",
        ),
        (
            "SELECT (ARRAY[10, 20, 30])[g] FROM s.t GROUP BY sz",
            "42803",
            "column \"t.g\" must appear in the GROUP BY clause or be used in an aggregate function",
        ),
        (
            "SELECT id, (SELECT m FROM unnest(string_to_array(st, ',')) AS m) FROM s.t ORDER BY id",
            "21000",
            "more than one row returned by a subquery used as an expression",
        ),
    ] {
        assert_eq!(refused(&mut w, sql), (Some(code), words.to_string()), "{sql}");
    }
}

#[test]
fn pg_collation_for_names_a_calls_collation_and_an_index_keys_a_column_under_its_own() {
    let mut w = snail("collation-for");
    run(&mut w, "CREATE COLLATION s.bytes (provider = libc, locale = 'C')");
    run(&mut w, "CREATE COLLATION s.\"Mixed Case\" (provider = libc, locale = 'C')");
    run(&mut w, "CREATE COLLATION public.speller_bytes (provider = libc, locale = 'C')");
    run(
        &mut w,
        "CREATE TABLE s.t (id integer NOT NULL, a text, b varchar(8) COLLATE \"C\", \
         c text COLLATE s.bytes, f text COLLATE public.speller_bytes, n integer, nm name, \
         PRIMARY KEY (id))",
    );
    // An index under its column's own collation keys the column; under another of byte order it
    // keeps the COLLATE PostgreSQL writes.
    run(&mut w, "CREATE INDEX t_b ON s.t (b COLLATE \"C\")");
    run(&mut w, "CREATE INDEX t_c ON s.t (c COLLATE s.bytes)");
    run(&mut w, "CREATE INDEX t_nm ON s.t (nm COLLATE \"C\")");
    run(&mut w, "CREATE INDEX t_ac ON s.t (a COLLATE \"C\")");
    run(&mut w, "CREATE INDEX t_ab ON s.t (a COLLATE s.bytes)");
    run(
        &mut w,
        "INSERT INTO s.t VALUES (1, 'abc', 'abc', 'abc', 'abc', 1, 'abc'), \
         (2, 'ABC', 'ABC ', 'b', 'b', 2, 'b')",
    );
    for (sql, answer) in [
        (
            "SELECT pg_get_indexdef('s.t_b'::regclass), pg_get_indexdef('s.t_c'::regclass), \
             pg_get_indexdef('s.t_nm'::regclass)",
            vec![
                "CREATE INDEX t_b ON s.t USING btree (b)|CREATE INDEX t_c ON s.t USING btree (c)|\
                 CREATE INDEX t_nm ON s.t USING btree (nm)",
            ],
        ),
        (
            "SELECT pg_get_indexdef('s.t_ac'::regclass), pg_get_indexdef('s.t_ab'::regclass)",
            vec![
                "CREATE INDEX t_ac ON s.t USING btree (a COLLATE \"C\")|\
                 CREATE INDEX t_ab ON s.t USING btree (a COLLATE s.bytes)",
            ],
        ),
        // The collation a call over its argument is made under, by its name as PostgreSQL writes
        // it: quoted where it must be, its schema before it unless that is public.
        (
            "SELECT pg_collation_for(a), pg_collation_for(b), pg_collation_for(c), \
             pg_collation_for(f), pg_collation_for(nm) FROM s.t WHERE id = 1",
            vec!["\"default\"|\"C\"|s.bytes|speller_bytes|\"C\""],
        ),
        (
            "SELECT pg_collation_for('x'), pg_collation_for('x'::text), \
             pg_collation_for(a COLLATE s.\"Mixed Case\"), pg_collation_for(a COLLATE \"C.utf8\") \
             FROM s.t WHERE id = 1",
            vec!["NULL|\"default\"|s.\"Mixed Case\"|\"C.utf8\""],
        ),
        (
            "SELECT pg_collation_for(b || a), pg_collation_for(b || c), pg_collation_for(upper(c)), \
             pg_typeof(pg_collation_for(b)) FROM s.t WHERE id = 1",
            vec!["\"C\"|NULL|s.bytes|text"],
        ),
        ("SELECT id FROM s.t WHERE pg_collation_for(b) = '\"C\"' ORDER BY id", vec!["1", "2"]),
    ] {
        assert_eq!(rows(&mut w, sql), answer, "{sql}");
    }
    // A column under a collation of the DDL's ordering as its own is a level the question path
    // reads along an index.
    let (b, _) = w
        .ask("SELECT id FROM s.t WHERE a COLLATE s.bytes = 'abc'", Some("t_ab"))
        .unwrap_or_else(|e| panic!("along t_ab: {e}"));
    assert_eq!(w.text_of(&b).unwrap(), [[Some("1".to_string())]]);
    assert_eq!(
        refused(&mut w, "SELECT pg_collation_for(n) FROM s.t"),
        (Some("42804"), "collations are not supported by type integer".to_string())
    );
    // Listed among the functions of a type the snail answers.
    assert!(warren_snail::functions::all()
        .iter()
        .any(|f| f.signature == "pg_collation_for(\"any\")" && f.held()));
}
