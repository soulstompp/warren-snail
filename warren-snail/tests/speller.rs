//! The PostgreSQL warren-speller writes for MySQL's statements, answered as PostgreSQL 18.6 answers
//! it: each refusal with its SQLSTATE and in its words, a qualifier an alias hides looked up as
//! PostgreSQL's parser looks it up, a CHECK holding BETWEEN or an ARRAY of literals read on every
//! row, a boolean's casts to and from an integer, a column's default set and dropped, a write
//! returning rows of what it changed, its `old` and `new` among them, a write through a view reaching
//! its table, each CHECK OPTION met, an INSERT of fewer values than columns. Every word,
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
