//! The catalog's functions PostgreSQL's views and its clients call, over a snail's catalog, each
//! answered as PostgreSQL 18.6 answers it after the same DDL and statements: pg_sequence_last_value,
//! where each sequence stands as its session's draws, a CACHE run at a time, and its sets leave it,
//! call by call within one statement and on the newest root whatever a transaction's snapshot.
//! Every expected row and code was given by 18.6.

mod weather;

use warren_snail::catalog::Catalog;
use warren_snail::read::Snail;
use weather::tmp;

/// A snail of release 18 written from a DDL, holding no rows.
fn written(name: &str, ddl: &str) -> std::path::PathBuf {
    let path = tmp(&format!("calls-{name}.snail"));
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

/// A statement's refusal: its SQLSTATE and its words.
#[cfg(feature = "standalone")]
fn refused(w: &mut Snail, sql: &str) -> (Option<&'static str>, String) {
    let e = w
        .execute(sql)
        .err()
        .unwrap_or_else(|| panic!("{sql} was taken"));
    (e.sqlstate(), e.to_string())
}

const SEQUENCES: &str = "CREATE SCHEMA s; \
     CREATE TABLE s.t (id serial PRIMARY KEY, n int GENERATED ALWAYS AS IDENTITY, v text); \
     CREATE SEQUENCE s.q START 5 INCREMENT 3 MINVALUE 2 MAXVALUE 1000 CACHE 4 CYCLE; \
     CREATE SEQUENCE s.r AS smallint; \
     CREATE SEQUENCE s.c MINVALUE 1 MAXVALUE 10 START 9 CACHE 4 CYCLE; \
     CREATE SEQUENCE s.e MAXVALUE 10 START 8 CACHE 4; \
     CREATE TABLE s.u (id bigint DEFAULT nextval('s.e'), v int PRIMARY KEY); \
     CREATE SEQUENCE s.d INCREMENT -2 MINVALUE -7 MAXVALUE 0 START 0 CACHE 3 CYCLE;";

const PG_SEQUENCES: &str = "SELECT sequencename, data_type::text, start_value, min_value, \
     max_value, increment_by, cycle, cache_size, last_value FROM pg_sequences \
     WHERE schemaname = 's' ORDER BY 1";

#[test]
fn pg_sequences_holds_each_sequence_as_its_ddl_leaves_it() {
    let mut s = Snail::open(&written("sequences-read", SEQUENCES)).unwrap();
    let (b, _) = s
        .ask(PG_SEQUENCES, None)
        .unwrap_or_else(|e| panic!("{}", e.0));
    assert_eq!(
        joined(&s, &b),
        [
            "c|bigint|9|1|10|1|t|4|NULL",
            "d|bigint|0|-7|0|-2|t|3|NULL",
            "e|bigint|8|1|10|1|f|4|NULL",
            "q|bigint|5|2|1000|3|t|4|NULL",
            "r|smallint|1|1|32767|1|f|1|NULL",
            "t_id_seq|integer|1|1|2147483647|1|f|1|NULL",
            "t_n_seq|integer|1|1|2147483647|1|f|1|NULL",
        ]
    );
}

#[cfg(feature = "standalone")]
#[test]
fn pg_sequence_last_value_reads_where_each_sequence_stands_as_postgresql_does() {
    let mut w = Snail::open_writing(&written("sequences", SEQUENCES)).unwrap();
    // A draw that fetches stands on the last of its CACHE run; one its session held moves nothing;
    // a rollback gives nothing back.
    asked(&mut w, "SELECT nextval('s.q')", &["5"]);
    asked(&mut w, "SELECT pg_sequence_last_value('s.q')", &["14"]);
    asked(&mut w, "BEGIN", &[]);
    asked(
        &mut w,
        "SELECT nextval('s.q'), nextval('s.q'), nextval('s.q'), nextval('s.q')",
        &["8|11|14|17"],
    );
    asked(&mut w, "ROLLBACK", &[]);
    asked(&mut w, "SELECT pg_sequence_last_value('s.q')", &["26"]);
    // A set stands where it set it, NULL where not called.
    asked(
        &mut w,
        "SELECT setval('s.r', 10, false), pg_sequence_last_value('s.r')",
        &["10|NULL"],
    );
    asked(&mut w, "SELECT pg_sequence_last_value('s.r')", &["NULL"]);
    asked(&mut w, "SELECT setval('s.r', 10)", &["10"]);
    asked(
        &mut w,
        "SELECT pg_sequence_last_value('s.r'), pg_sequence_last_value(NULL)",
        &["10|NULL"],
    );
    // A run ends at a cycling sequence's end, the next fetch cycling.
    asked(
        &mut w,
        "SELECT nextval('s.c') FROM (VALUES (1), (2), (3), (4), (5), (6)) g(n)",
        &["9", "10", "1", "2", "3", "4"],
    );
    asked(&mut w, "SELECT pg_sequence_last_value('s.c')", &["4"]);
    asked(
        &mut w,
        "SELECT nextval('s.c'), pg_sequence_last_value('s.c')",
        &["5|8"],
    );
    // A write whose draws run out uses up those before the end.
    let words = "nextval: reached maximum value of sequence \"e\" (10)";
    assert_eq!(
        refused(&mut w, "INSERT INTO s.u (v) VALUES (1), (2), (3), (4), (5)"),
        (Some("2200H"), words.to_string())
    );
    assert_eq!(rows(&mut w, "SELECT pg_sequence_last_value('s.e')"), ["10"]);
    assert_eq!(
        refused(&mut w, "SELECT nextval('s.e')"),
        (Some("2200H"), words.to_string())
    );
    // Within one statement it stands where each call leaves it.
    asked(
        &mut w,
        "SELECT nextval('s.d') FROM (VALUES (1), (2), (3), (4), (5), (6), (7)) g(n)",
        &["0", "-2", "-4", "-6", "0", "-2", "-4"],
    );
    asked(
        &mut w,
        "SELECT pg_sequence_last_value('s.d') AS before, nextval('s.d') AS drawn, \
         pg_sequence_last_value('s.d') AS after FROM (VALUES (1), (2), (3), (4)) g(n)",
        &["-4|-6|-6", "-6|0|-4", "-4|-2|-4", "-4|-4|-4"],
    );
    // It reads the newest root, whatever the transaction's snapshot.
    asked(&mut w, "BEGIN ISOLATION LEVEL REPEATABLE READ", &[]);
    asked(&mut w, "SELECT pg_sequence_last_value('s.r')", &["10"]);
    asked(
        &mut w,
        "SELECT nextval('s.r'), pg_sequence_last_value('s.r')",
        &["11|11"],
    );
    asked(&mut w, "SELECT pg_sequence_last_value('s.r')", &["11"]);
    asked(&mut w, "COMMIT", &[]);
    for (sql, code, words) in [
        (
            "SELECT pg_sequence_last_value('s.t')",
            "42809",
            "cannot open relation \"t\": This operation is not supported for tables.",
        ),
        (
            "SELECT pg_sequence_last_value('s.t_pkey')",
            "42809",
            "cannot open relation \"t_pkey\": This operation is not supported for indexes.",
        ),
        (
            "SELECT pg_sequence_last_value(99999)",
            "XX000",
            "could not open relation with OID 99999",
        ),
        (
            "SELECT pg_sequence_last_value(0)",
            "XX000",
            "could not open relation with OID 0",
        ),
        (
            "SELECT pg_sequence_last_value('pg_class')",
            "42809",
            "cannot open relation \"pg_class\": This operation is not supported for tables.",
        ),
        (
            "SELECT pg_sequence_last_value('s.nope')",
            "42P01",
            "relation \"s.nope\" does not exist",
        ),
    ] {
        assert_eq!(
            refused(&mut w, sql),
            (Some(code), words.to_string()),
            "{sql}"
        );
    }
    asked(&mut w, "INSERT INTO s.t (v) VALUES ('a'), ('b')", &[]);
    asked(
        &mut w,
        "SELECT pg_sequence_last_value(c.oid), pg_sequence_last_value(c.oid::regclass) \
         FROM pg_class c WHERE c.relname = 't_n_seq'",
        &["2|2"],
    );
    asked(
        &mut w,
        "SELECT sequencename, last_value FROM pg_sequences WHERE schemaname = 's' ORDER BY 1",
        &[
            "c|8",
            "d|-4",
            "e|10",
            "q|26",
            "r|11",
            "t_id_seq|2",
            "t_n_seq|2",
        ],
    );
}
