//! The catalog's functions PostgreSQL's views and its clients call, over a snail's catalog, each
//! answered as PostgreSQL 18.6 answers it after the same DDL and statements: pg_sequence_last_value,
//! where each sequence stands as its session's draws, a CACHE run at a time, and its sets leave it,
//! call by call within one statement and on the newest root whatever a transaction's snapshot;
//! pg_table_is_visible, pg_type_is_visible and pg_function_is_visible, whether the search path,
//! pg_catalog then public, finds an object by its name alone, as the reg* types' output names it;
//! information_schema's views of what a write takes, in a file none of whose views is written
//! through; the reg* names of the objects initdb makes from 12000 on; information_schema's
//! functions called in a statement of its own. Every expected row and code was given by 18.6.

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

const VISIBLE: &str = "CREATE SCHEMA a; \
     CREATE TABLE public.t (id int PRIMARY KEY); CREATE TABLE a.t (id int PRIMARY KEY); \
     CREATE TABLE public.pg_class (x int PRIMARY KEY); CREATE TABLE public.tables (x int PRIMARY KEY); \
     CREATE VIEW public.v AS SELECT 1 AS one; CREATE SEQUENCE public.sq; \
     CREATE INDEX t_i ON public.t (id); \
     CREATE TYPE public.mood AS ENUM ('a'); CREATE TYPE public.int4 AS ENUM ('x'); \
     CREATE TYPE a.mood AS ENUM ('b'); CREATE TYPE public.cardinal_number AS ENUM ('c'); \
     CREATE TYPE public.pair AS (a int, b text); CREATE DOMAIN public.dm AS int; \
     CREATE FUNCTION public.f(integer) RETURNS integer LANGUAGE sql AS 'SELECT $1'; \
     CREATE FUNCTION a.f(integer) RETURNS integer LANGUAGE sql AS 'SELECT $1'; \
     CREATE FUNCTION public.lower(integer) RETURNS integer LANGUAGE sql AS 'SELECT $1'; \
     CREATE FUNCTION public.abs(integer) RETURNS integer LANGUAGE sql AS 'SELECT $1'; \
     CREATE FUNCTION public.g(text, integer) RETURNS integer LANGUAGE sql AS 'SELECT $2';";

#[test]
fn the_visibility_tests_answer_whether_the_search_path_finds_an_object_by_its_name() {
    let mut s = Snail::open(&written("visible", VISIBLE)).unwrap();
    let mut asked = |q: &str, want: &[&str]| {
        let (b, _) = s.ask(q, None).unwrap_or_else(|e| panic!("{q}: {}", e.0));
        assert_eq!(joined(&s, &b), want, "{q}");
    };
    // A file's object is visible where it is public's and pg_catalog holds none of its name: a
    // type's array by its own, a function by its name and its arguments.
    asked(
        "SELECT n.nspname||'.'||c.relname, pg_table_is_visible(c.oid), c.oid::regclass::text \
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname IN ('public', 'a') ORDER BY 1",
        &[
            "a.t|f|a.t",
            "a.t_pkey|f|a.t_pkey",
            "public.pair|t|pair",
            "public.pg_class|f|public.pg_class",
            "public.pg_class_pkey|t|pg_class_pkey",
            "public.sq|t|sq",
            "public.t|t|t",
            "public.t_i|t|t_i",
            "public.t_pkey|t|t_pkey",
            "public.tables|t|tables",
            "public.tables_pkey|t|tables_pkey",
            "public.v|t|v",
        ],
    );
    asked(
        "SELECT n.nspname||'.'||t.typname, pg_type_is_visible(t.oid), t.oid::regtype::text \
         FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace \
         WHERE n.nspname IN ('public', 'a') ORDER BY 1",
        &[
            "a._mood|f|a.mood[]",
            "a._t|f|a.t[]",
            "a.mood|f|a.mood",
            "a.t|f|a.t",
            "public._cardinal_number|t|cardinal_number[]",
            "public._dm|t|dm[]",
            "public._int4|f|public.int4[]",
            "public._mood|t|mood[]",
            "public._pair|t|pair[]",
            "public._pg_class|f|public.pg_class[]",
            "public._t|t|t[]",
            "public._tables|t|tables[]",
            "public._v|t|v[]",
            "public.cardinal_number|t|cardinal_number",
            "public.dm|t|dm",
            "public.int4|f|public.int4",
            "public.mood|t|mood",
            "public.pair|t|pair",
            "public.pg_class|f|public.pg_class",
            "public.t|t|t",
            "public.tables|t|tables",
            "public.v|t|v",
        ],
    );
    asked(
        "SELECT n.nspname||'.'||p.proname||'('||pg_get_function_identity_arguments(p.oid)||')', \
         pg_function_is_visible(p.oid), p.oid::regprocedure::text, p.oid::regproc::text \
         FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
         WHERE n.nspname IN ('public', 'a') ORDER BY 1",
        &[
            "a.f(integer)|f|a.f(integer)|a.f",
            "public.abs(integer)|f|public.abs(integer)|public.abs",
            "public.f(integer)|t|f(integer)|f",
            "public.g(text, integer)|t|g(text,integer)|g",
            "public.lower(integer)|t|lower(integer)|public.lower",
        ],
    );
    // A built-in object is visible where it is pg_catalog's.
    asked(
        "SELECT n.nspname, count(*), count(*) FILTER (WHERE pg_table_is_visible(c.oid)) \
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.oid < 16384 GROUP BY 1 ORDER BY 1",
        &[
            "information_schema|69|0",
            "pg_catalog|268|268",
            "pg_toast|78|0",
        ],
    );
    asked(
        "SELECT n.nspname, count(*), count(*) FILTER (WHERE pg_type_is_visible(t.oid)) \
         FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace \
         WHERE t.oid < 16384 GROUP BY 1 ORDER BY 1",
        &["information_schema|148|0", "pg_catalog|473|473"],
    );
    asked(
        "SELECT n.nspname, count(*), count(*) FILTER (WHERE pg_function_is_visible(p.oid)) \
         FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
         WHERE p.oid < 16384 GROUP BY 1 ORDER BY 1",
        &["information_schema|11|0", "pg_catalog|3402|3402"],
    );
    // NULL where the OID names no object of the kind.
    asked(
        "SELECT pg_table_is_visible(1259), pg_table_is_visible(99999), pg_table_is_visible(0), \
         pg_table_is_visible(NULL), pg_table_is_visible(23)",
        &["t|NULL|NULL|NULL|NULL"],
    );
    asked(
        "SELECT pg_type_is_visible('int4'::regtype), pg_type_is_visible('integer[]'::regtype), \
         pg_type_is_visible(83), pg_type_is_visible(99999), pg_type_is_visible(1259)",
        &["t|t|t|NULL|NULL"],
    );
    asked(
        "SELECT pg_function_is_visible('lower(text)'::regprocedure), pg_function_is_visible(870), \
         pg_function_is_visible(99999), pg_function_is_visible(23), pg_function_is_visible(NULL)",
        &["t|t|NULL|NULL|NULL"],
    );
}

const KEYS: &str = "CREATE SCHEMA s; \
     CREATE TABLE s.p (a int, b text, c int, PRIMARY KEY (b, a), UNIQUE (c)); \
     CREATE TABLE s.f (id int PRIMARY KEY, pb text, pa int, pc int REFERENCES s.p (c), \
     FOREIGN KEY (pb, pa) REFERENCES s.p (b, a)); \
     CREATE TABLE s.arr (id int PRIMARY KEY, xs int[], ts text[]); \
     CREATE FUNCTION s.fn(integer, text) RETURNS integer LANGUAGE sql AS 'SELECT $1';";

#[test]
fn information_schema_reads_each_array_element_and_its_place_as_postgresql_does() {
    let mut s = Snail::open(&written("keys", KEYS)).unwrap();
    let mut asked = |q: &str, want: &[&str]| {
        let (b, _) = s.ask(q, None).unwrap_or_else(|e| panic!("{q}: {}", e.0));
        assert_eq!(joined(&s, &b), want, "{q}");
    };
    // Each key's columns by their place in it, and a foreign key's by the place each column it
    // references has in that key's index.
    asked(
        "SELECT constraint_name, table_name, column_name, ordinal_position, \
         position_in_unique_constraint FROM information_schema.key_column_usage \
         WHERE table_schema = 's' ORDER BY 1, 4",
        &[
            "arr_pkey|arr|id|1|NULL",
            "f_pb_pa_fkey|f|pb|1|1",
            "f_pb_pa_fkey|f|pa|2|2",
            "f_pc_fkey|f|pc|1|1",
            "f_pkey|f|id|1|NULL",
            "p_c_key|p|c|1|NULL",
            "p_pkey|p|b|1|NULL",
            "p_pkey|p|a|2|NULL",
        ],
    );
    asked(
        "SELECT object_name, object_type, collection_type_identifier, data_type \
         FROM information_schema.element_types WHERE object_schema = 's' ORDER BY 1, 3",
        &["arr|TABLE|2|integer", "arr|TABLE|3|text"],
    );
    asked(
        "SELECT specific_schema, regexp_replace(specific_name, '_[0-9]+$', ''), \
         ordinal_position, parameter_mode, parameter_name, data_type, parameter_default, \
         dtd_identifier FROM information_schema.parameters \
         WHERE specific_schema = 's' OR specific_name LIKE 'pg_get_keywords\\_%' ORDER BY 1, 2, 3",
        &[
            "pg_catalog|pg_get_keywords|1|OUT|word|text|NULL|1",
            "pg_catalog|pg_get_keywords|2|OUT|catcode|\"char\"|NULL|2",
            "pg_catalog|pg_get_keywords|3|OUT|barelabel|boolean|NULL|3",
            "pg_catalog|pg_get_keywords|4|OUT|catdesc|text|NULL|4",
            "pg_catalog|pg_get_keywords|5|OUT|baredesc|text|NULL|5",
            "s|fn|1|IN|NULL|integer|NULL|1",
            "s|fn|2|IN|NULL|text|NULL|2",
        ],
    );
    asked(
        "SELECT count(*) FROM information_schema.triggered_update_columns",
        &["0"],
    );
    // In a file holding a view, whose step is named beside the system views': element_types'
    // columns past those the views its subquery reads refuse at their places, each view's
    // refusals its own and its subquery's kept beside its statement's.
    let viewed = format!("{KEYS} CREATE VIEW s.v AS SELECT id, xs FROM s.arr;");
    let mut s = Snail::open(&written("keys-view", &viewed)).unwrap();
    let q = "SELECT object_name || '.' || collection_type_identifier, data_type, udt_schema, \
         udt_name, dtd_identifier FROM information_schema.element_types \
         WHERE object_schema = 's' ORDER BY 1";
    let (b, _) = s.ask(q, None).unwrap_or_else(|e| panic!("{q}: {}", e.0));
    assert_eq!(
        joined(&s, &b),
        [
            "arr.2|integer|pg_catalog|int4|a2",
            "arr.3|text|pg_catalog|text|a3",
            "v.2|integer|pg_catalog|int4|a2",
        ],
        "{q}"
    );
}

const TRIGGERS: &str = "CREATE SCHEMA s; \
     CREATE TABLE s.p (a int, b text, c int, PRIMARY KEY (b, a), UNIQUE (c)); \
     CREATE TABLE s.f (id int PRIMARY KEY, pc int REFERENCES s.p (c) ON DELETE CASCADE \
     ON UPDATE SET NULL); \
     CREATE TABLE s.g (id int PRIMARY KEY, pb text, pa int, FOREIGN KEY (pb, pa) \
     REFERENCES s.p (b, a) DEFERRABLE INITIALLY DEFERRED); \
     CREATE TABLE public.q (id int PRIMARY KEY, k int UNIQUE DEFERRABLE, r int \
     REFERENCES public.q (id) ON DELETE RESTRICT ON UPDATE SET DEFAULT DEFERRABLE); \
     CREATE TABLE s.\"Odd Name\" (id int PRIMARY KEY, q_id int REFERENCES public.q (id) \
     ON DELETE SET NULL);";

#[test]
fn pg_get_triggerdef_writes_each_constraint_trigger_as_postgresql_does() {
    let mut s = Snail::open(&written("triggers", TRIGGERS)).unwrap();
    let mut asked = |q: &str, want: &[&str]| {
        let (b, _) = s.ask(q, None).unwrap_or_else(|e| panic!("{q}: {}", e.0));
        assert_eq!(joined(&s, &b), want, "{q}");
    };
    // Each trigger named by its own OID, written N here as the OIDs differ: its table qualified,
    // or named as the search path finds it where pretty; the other table so in either.
    asked(
        "SELECT c.relname||' '||k.conname||' '||g.tgfoid::regproc::text AS k, \
         regexp_replace(pg_get_triggerdef(g.oid), '_[0-9]+\"', '_N\"') AS plain, \
         regexp_replace(pg_get_triggerdef(g.oid, true), '_[0-9]+\"', '_N\"') AS pretty \
         FROM pg_trigger g JOIN pg_class c ON c.oid = g.tgrelid \
         JOIN pg_constraint k ON k.oid = g.tgconstraint ORDER BY 1",
        &[
            "Odd Name Odd Name_q_id_fkey \"RI_FKey_check_ins\"|CREATE CONSTRAINT TRIGGER \"RI_ConstraintTrigger_c_N\" AFTER INSERT ON s.\"Odd Name\" FROM q NOT DEFERRABLE INITIALLY IMMEDIATE FOR EACH ROW EXECUTE FUNCTION \"RI_FKey_check_ins\"()|CREATE CONSTRAINT TRIGGER \"RI_ConstraintTrigger_c_N\" AFTER INSERT ON s.\"Odd Name\" FROM q NOT DEFERRABLE INITIALLY IMMEDIATE FOR EACH ROW EXECUTE FUNCTION \"RI_FKey_check_ins\"()",
            "Odd Name Odd Name_q_id_fkey \"RI_FKey_check_upd\"|CREATE CONSTRAINT TRIGGER \"RI_ConstraintTrigger_c_N\" AFTER UPDATE ON s.\"Odd Name\" FROM q NOT DEFERRABLE INITIALLY IMMEDIATE FOR EACH ROW EXECUTE FUNCTION \"RI_FKey_check_upd\"()|CREATE CONSTRAINT TRIGGER \"RI_ConstraintTrigger_c_N\" AFTER UPDATE ON s.\"Odd Name\" FROM q NOT DEFERRABLE INITIALLY IMMEDIATE FOR EACH ROW EXECUTE FUNCTION \"RI_FKey_check_upd\"()",
            "f f_pc_fkey \"RI_FKey_check_ins\"|CREATE CONSTRAINT TRIGGER \"RI_ConstraintTrigger_c_N\" AFTER INSERT ON s.f FROM s.p NOT DEFERRABLE INITIALLY IMMEDIATE FOR EACH ROW EXECUTE FUNCTION \"RI_FKey_check_ins\"()|CREATE CONSTRAINT TRIGGER \"RI_ConstraintTrigger_c_N\" AFTER INSERT ON s.f FROM s.p NOT DEFERRABLE INITIALLY IMMEDIATE FOR EACH ROW EXECUTE FUNCTION \"RI_FKey_check_ins\"()",
            "f f_pc_fkey \"RI_FKey_check_upd\"|CREATE CONSTRAINT TRIGGER \"RI_ConstraintTrigger_c_N\" AFTER UPDATE ON s.f FROM s.p NOT DEFERRABLE INITIALLY IMMEDIATE FOR EACH ROW EXECUTE FUNCTION \"RI_FKey_check_upd\"()|CREATE CONSTRAINT TRIGGER \"RI_ConstraintTrigger_c_N\" AFTER UPDATE ON s.f FROM s.p NOT DEFERRABLE INITIALLY IMMEDIATE FOR EACH ROW EXECUTE FUNCTION \"RI_FKey_check_upd\"()",
            "g g_pb_pa_fkey \"RI_FKey_check_ins\"|CREATE CONSTRAINT TRIGGER \"RI_ConstraintTrigger_c_N\" AFTER INSERT ON s.g FROM s.p DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION \"RI_FKey_check_ins\"()|CREATE CONSTRAINT TRIGGER \"RI_ConstraintTrigger_c_N\" AFTER INSERT ON s.g FROM s.p DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION \"RI_FKey_check_ins\"()",
            "g g_pb_pa_fkey \"RI_FKey_check_upd\"|CREATE CONSTRAINT TRIGGER \"RI_ConstraintTrigger_c_N\" AFTER UPDATE ON s.g FROM s.p DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION \"RI_FKey_check_upd\"()|CREATE CONSTRAINT TRIGGER \"RI_ConstraintTrigger_c_N\" AFTER UPDATE ON s.g FROM s.p DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION \"RI_FKey_check_upd\"()",
            "p f_pc_fkey \"RI_FKey_cascade_del\"|CREATE CONSTRAINT TRIGGER \"RI_ConstraintTrigger_a_N\" AFTER DELETE ON s.p FROM s.f NOT DEFERRABLE INITIALLY IMMEDIATE FOR EACH ROW EXECUTE FUNCTION \"RI_FKey_cascade_del\"()|CREATE CONSTRAINT TRIGGER \"RI_ConstraintTrigger_a_N\" AFTER DELETE ON s.p FROM s.f NOT DEFERRABLE INITIALLY IMMEDIATE FOR EACH ROW EXECUTE FUNCTION \"RI_FKey_cascade_del\"()",
            "p f_pc_fkey \"RI_FKey_setnull_upd\"|CREATE CONSTRAINT TRIGGER \"RI_ConstraintTrigger_a_N\" AFTER UPDATE ON s.p FROM s.f NOT DEFERRABLE INITIALLY IMMEDIATE FOR EACH ROW EXECUTE FUNCTION \"RI_FKey_setnull_upd\"()|CREATE CONSTRAINT TRIGGER \"RI_ConstraintTrigger_a_N\" AFTER UPDATE ON s.p FROM s.f NOT DEFERRABLE INITIALLY IMMEDIATE FOR EACH ROW EXECUTE FUNCTION \"RI_FKey_setnull_upd\"()",
            "p g_pb_pa_fkey \"RI_FKey_noaction_del\"|CREATE CONSTRAINT TRIGGER \"RI_ConstraintTrigger_a_N\" AFTER DELETE ON s.p FROM s.g DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION \"RI_FKey_noaction_del\"()|CREATE CONSTRAINT TRIGGER \"RI_ConstraintTrigger_a_N\" AFTER DELETE ON s.p FROM s.g DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION \"RI_FKey_noaction_del\"()",
            "p g_pb_pa_fkey \"RI_FKey_noaction_upd\"|CREATE CONSTRAINT TRIGGER \"RI_ConstraintTrigger_a_N\" AFTER UPDATE ON s.p FROM s.g DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION \"RI_FKey_noaction_upd\"()|CREATE CONSTRAINT TRIGGER \"RI_ConstraintTrigger_a_N\" AFTER UPDATE ON s.p FROM s.g DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION \"RI_FKey_noaction_upd\"()",
            "q Odd Name_q_id_fkey \"RI_FKey_noaction_upd\"|CREATE CONSTRAINT TRIGGER \"RI_ConstraintTrigger_a_N\" AFTER UPDATE ON public.q FROM s.\"Odd Name\" NOT DEFERRABLE INITIALLY IMMEDIATE FOR EACH ROW EXECUTE FUNCTION \"RI_FKey_noaction_upd\"()|CREATE CONSTRAINT TRIGGER \"RI_ConstraintTrigger_a_N\" AFTER UPDATE ON q FROM s.\"Odd Name\" NOT DEFERRABLE INITIALLY IMMEDIATE FOR EACH ROW EXECUTE FUNCTION \"RI_FKey_noaction_upd\"()",
            "q Odd Name_q_id_fkey \"RI_FKey_setnull_del\"|CREATE CONSTRAINT TRIGGER \"RI_ConstraintTrigger_a_N\" AFTER DELETE ON public.q FROM s.\"Odd Name\" NOT DEFERRABLE INITIALLY IMMEDIATE FOR EACH ROW EXECUTE FUNCTION \"RI_FKey_setnull_del\"()|CREATE CONSTRAINT TRIGGER \"RI_ConstraintTrigger_a_N\" AFTER DELETE ON q FROM s.\"Odd Name\" NOT DEFERRABLE INITIALLY IMMEDIATE FOR EACH ROW EXECUTE FUNCTION \"RI_FKey_setnull_del\"()",
            "q q_k_key unique_key_recheck|CREATE CONSTRAINT TRIGGER \"Unique_ConstraintTrigger_N\" AFTER INSERT OR UPDATE ON public.q DEFERRABLE INITIALLY IMMEDIATE FOR EACH ROW EXECUTE FUNCTION unique_key_recheck()|CREATE CONSTRAINT TRIGGER \"Unique_ConstraintTrigger_N\" AFTER INSERT OR UPDATE ON q DEFERRABLE INITIALLY IMMEDIATE FOR EACH ROW EXECUTE FUNCTION unique_key_recheck()",
            "q q_r_fkey \"RI_FKey_check_ins\"|CREATE CONSTRAINT TRIGGER \"RI_ConstraintTrigger_c_N\" AFTER INSERT ON public.q FROM q DEFERRABLE INITIALLY IMMEDIATE FOR EACH ROW EXECUTE FUNCTION \"RI_FKey_check_ins\"()|CREATE CONSTRAINT TRIGGER \"RI_ConstraintTrigger_c_N\" AFTER INSERT ON q FROM q DEFERRABLE INITIALLY IMMEDIATE FOR EACH ROW EXECUTE FUNCTION \"RI_FKey_check_ins\"()",
            "q q_r_fkey \"RI_FKey_check_upd\"|CREATE CONSTRAINT TRIGGER \"RI_ConstraintTrigger_c_N\" AFTER UPDATE ON public.q FROM q DEFERRABLE INITIALLY IMMEDIATE FOR EACH ROW EXECUTE FUNCTION \"RI_FKey_check_upd\"()|CREATE CONSTRAINT TRIGGER \"RI_ConstraintTrigger_c_N\" AFTER UPDATE ON q FROM q DEFERRABLE INITIALLY IMMEDIATE FOR EACH ROW EXECUTE FUNCTION \"RI_FKey_check_upd\"()",
            "q q_r_fkey \"RI_FKey_restrict_del\"|CREATE CONSTRAINT TRIGGER \"RI_ConstraintTrigger_a_N\" AFTER DELETE ON public.q FROM q NOT DEFERRABLE INITIALLY IMMEDIATE FOR EACH ROW EXECUTE FUNCTION \"RI_FKey_restrict_del\"()|CREATE CONSTRAINT TRIGGER \"RI_ConstraintTrigger_a_N\" AFTER DELETE ON q FROM q NOT DEFERRABLE INITIALLY IMMEDIATE FOR EACH ROW EXECUTE FUNCTION \"RI_FKey_restrict_del\"()",
            "q q_r_fkey \"RI_FKey_setdefault_upd\"|CREATE CONSTRAINT TRIGGER \"RI_ConstraintTrigger_a_N\" AFTER UPDATE ON public.q FROM q NOT DEFERRABLE INITIALLY IMMEDIATE FOR EACH ROW EXECUTE FUNCTION \"RI_FKey_setdefault_upd\"()|CREATE CONSTRAINT TRIGGER \"RI_ConstraintTrigger_a_N\" AFTER UPDATE ON q FROM q NOT DEFERRABLE INITIALLY IMMEDIATE FOR EACH ROW EXECUTE FUNCTION \"RI_FKey_setdefault_upd\"()",
        ],
    );
    asked(
        "SELECT pg_get_triggerdef(99999), pg_get_triggerdef(0), pg_get_triggerdef(NULL), \
         pg_typeof(pg_get_triggerdef(0))::text",
        &["NULL|NULL|NULL|text"],
    );
    asked("SELECT count(*) FROM information_schema.triggers", &["0"]);
    // Each named by its own OID.
    asked(
        "SELECT count(*) FILTER (WHERE position(g.oid::text IN pg_get_triggerdef(g.oid)) > 0), \
         count(*) FROM pg_trigger g",
        &["17|17"],
    );
}

#[test]
fn obj_description_reads_each_built_in_objects_description_as_postgresql_does() {
    let ddl = "CREATE SCHEMA s; CREATE TABLE s.t (id int PRIMARY KEY, v text);";
    let mut s = Snail::open(&written("descriptions", ddl)).unwrap();
    let mut asked = |q: &str, want: &[&str]| {
        let (b, _) = s.ask(q, None).unwrap_or_else(|e| panic!("{q}: {}", e.0));
        assert_eq!(joined(&s, &b), want, "{q}");
    };
    asked(
        "SELECT obj_description('lower(text)'::regprocedure, 'pg_proc'), \
         obj_description('pg_class'::regclass, 'pg_class'), \
         obj_description('int4'::regtype, 'pg_type'), \
         obj_description('public'::regnamespace, 'pg_namespace'), \
         obj_description('pg_catalog'::regnamespace, 'pg_namespace')",
        &["lowercase|NULL|-2 billion to 2 billion integer, 4-byte storage|standard public schema|system catalog schema"],
    );
    // Of sub-object 0 of any catalog alone; of pg_catalog's relation of exactly the name given.
    asked(
        "SELECT obj_description(1259), obj_description(870), obj_description(99999, 'pg_class'), \
         obj_description(1259, 'nope'), obj_description(NULL, 'pg_class'), \
         obj_description(1259, NULL), obj_description('s.t'::regclass, 'pg_class'), \
         obj_description('s'::regnamespace, 'pg_namespace')",
        &["NULL|lowercase|NULL|NULL|NULL|NULL|NULL|NULL"],
    );
    asked(
        "SELECT obj_description(870, 'pg_class'), obj_description(1259, 'pg_catalog.pg_class'), \
         obj_description(870, 'PG_PROC'), pg_typeof(obj_description(1259, 'pg_class'))::text",
        &["NULL|NULL|NULL|text"],
    );
    for (q, want) in [
        (
            "SELECT count(*) FILTER (WHERE obj_description(oid, 'pg_proc') IS NOT NULL), \
             count(*) FILTER (WHERE obj_description(oid) IS NOT NULL), count(*) \
             FROM pg_proc WHERE oid < 16384",
            "3397|3397|3413",
        ),
        (
            "SELECT count(*) FILTER (WHERE obj_description(oid, 'pg_type') IS NOT NULL), \
             count(*) FROM pg_type WHERE oid < 16384",
            "107|621",
        ),
        (
            "SELECT count(*) FILTER (WHERE obj_description(oid, 'pg_class') IS NOT NULL), \
             count(*) FROM pg_class WHERE oid < 16384",
            "0|415",
        ),
        (
            "SELECT count(*) FILTER (WHERE obj_description(oid, 'pg_operator') IS NOT NULL), \
             count(*) FROM pg_operator WHERE oid < 16384",
            "799|799",
        ),
        (
            "SELECT count(*) FILTER (WHERE obj_description(oid, 'pg_namespace') IS NOT NULL), \
             count(*) FROM pg_namespace WHERE oid < 16384",
            "3|4",
        ),
    ] {
        asked(q, &[want]);
    }
    // A file that drops public drops its description with it.
    let ddl = "DROP SCHEMA public; CREATE SCHEMA s; CREATE TABLE s.t (id int PRIMARY KEY, v text);";
    let mut s = Snail::open(&written("descriptions-no-public", ddl)).unwrap();
    for (q, want) in [
        (
            "SELECT obj_description(2200, 'pg_namespace'), obj_description(2200)",
            "NULL|NULL",
        ),
        (
            "SELECT count(*) FILTER (WHERE obj_description(oid, 'pg_namespace') IS NOT NULL), \
             count(*) FROM pg_namespace WHERE oid < 16384",
            "2|3",
        ),
    ] {
        let (b, _) = s.ask(q, None).unwrap_or_else(|e| panic!("{q}: {}", e.0));
        assert_eq!(joined(&s, &b), [want], "{q}");
    }
}

#[test]
fn information_schema_reads_what_a_write_takes_where_no_view_is_written_through() {
    let tables = "SELECT table_name, table_type, is_insertable_into FROM information_schema.tables \
         WHERE table_schema = 'public' ORDER BY 1";
    let columns = "SELECT table_name, column_name, is_updatable FROM information_schema.columns \
         WHERE table_schema = 'public' ORDER BY 1, 2";
    let views = "SELECT table_name, is_updatable, is_insertable_into, is_trigger_updatable \
         FROM information_schema.views WHERE table_schema = 'public'";
    let table = "CREATE TABLE t (id integer PRIMARY KEY, g integer);";
    let counted = format!("{table} CREATE VIEW w AS SELECT count(*) AS n FROM t;");
    for (name, ddl, want) in [
        (
            "no-view",
            table.to_string(),
            [vec!["t|BASE TABLE|YES"], vec!["t|g|YES", "t|id|YES"], vec![]],
        ),
        (
            "unwritable-view",
            counted,
            [
                vec!["t|BASE TABLE|YES", "w|VIEW|NO"],
                vec!["t|g|YES", "t|id|YES", "w|n|NO"],
                vec!["w|NO|NO|NO"],
            ],
        ),
    ] {
        let mut s = Snail::open(&written(name, &ddl)).unwrap();
        for (q, want) in [tables, columns, views].into_iter().zip(want) {
            let (b, _) = s.ask(q, None).unwrap_or_else(|e| panic!("{name}: {q}: {}", e.0));
            assert_eq!(joined(&s, &b), want, "{name}: {q}");
        }
    }
}

#[test]
fn information_schema_reads_the_options_of_foreign_objects_a_snail_holds_none_of() {
    let ddl = "CREATE TABLE t (id integer PRIMARY KEY, g integer); \
         CREATE VIEW w AS SELECT id, g FROM t;";
    let mut s = Snail::open(&written("options", ddl)).unwrap();
    for q in [
        "SELECT table_name, column_name, option_name, option_value \
         FROM information_schema.column_options",
        "SELECT option_name FROM information_schema.foreign_server_options",
        "SELECT option_name FROM information_schema.foreign_table_options",
        "SELECT option_name FROM information_schema.foreign_data_wrapper_options",
    ] {
        let (b, _) = s.ask(q, None).unwrap_or_else(|e| panic!("{q}: {}", e.0));
        assert_eq!(joined(&s, &b), Vec::<String>::new(), "{q}");
    }
    // A column the view passes through from one beneath it that only a server fills is refused
    // where a statement asks it.
    for q in [
        "SELECT foreign_server_catalog FROM information_schema.foreign_server_options",
        "SELECT table_catalog FROM information_schema.column_options",
    ] {
        let e = s.ask(q, None).err().unwrap_or_else(|| panic!("{q} was taken")).0;
        assert!(e.contains("current_database() is a server's own"), "{q}: {e}");
    }
}

#[test]
fn pg_index_holds_each_keys_operator_class_and_order_as_postgresql_does() {
    let ddl = "CREATE SCHEMA s; \
         CREATE TABLE s.t (id integer PRIMARY KEY, v text, w varchar(20), p point, j jsonb, \
         d date, n numeric); \
         CREATE INDEX t_v_pattern ON s.t (v text_pattern_ops); \
         CREATE INDEX t_desc ON s.t (d DESC, id); \
         CREATE INDEX t_desc_last ON s.t (d DESC NULLS LAST); \
         CREATE INDEX t_first ON s.t (n NULLS FIRST, v DESC NULLS FIRST); \
         CREATE INDEX t_w ON s.t (w); \
         CREATE INDEX t_hash ON s.t USING hash (v); \
         CREATE INDEX t_gist ON s.t USING gist (p); \
         CREATE INDEX t_gin ON s.t USING gin (j jsonb_path_ops); \
         CREATE INDEX t_gin_default ON s.t USING gin (j); \
         CREATE INDEX t_brin ON s.t USING brin (d); \
         CREATE INDEX t_include ON s.t (v) INCLUDE (n);";
    let mut s = Snail::open(&written("index-classes", ddl)).unwrap();
    let q = "SELECT c.relname, i.indclass::text, i.indoption::text, \
         (SELECT o.opcname FROM pg_opclass o WHERE o.oid = i.indclass[0]), \
         (SELECT o.opcname FROM pg_opclass o WHERE o.oid = i.indclass[1]) \
         FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid \
         JOIN pg_namespace ns ON ns.oid = c.relnamespace WHERE ns.nspname = 's' ORDER BY 1";
    let (b, _) = s.ask(q, None).unwrap_or_else(|e| panic!("{q}: {}", e.0));
    assert_eq!(
        joined(&s, &b),
        [
            "t_brin|10136|0|date_minmax_ops|NULL",
            "t_desc|3122 1978|3 0|date_ops|int4_ops",
            "t_desc_last|3122|1|date_ops|NULL",
            "t_first|3125 3126|2 3|numeric_ops|text_ops",
            "t_gin|10091|0|jsonb_path_ops|NULL",
            "t_gin_default|10090|0|jsonb_ops|NULL",
            "t_gist|10061|0|point_ops|NULL",
            "t_hash|10037|0|text_ops|NULL",
            "t_include|3126|0|text_ops|NULL",
            "t_pkey|1978|0|int4_ops|NULL",
            "t_v_pattern|4217|0|text_pattern_ops|NULL",
            "t_w|3126|0|text_ops|NULL",
        ]
    );
}

#[test]
fn pg_attribute_holds_each_columns_type_as_postgresql_keeps_it() {
    let ddl = "CREATE SCHEMA s; CREATE TYPE s.pair AS (x integer, y integer); \
         CREATE DOMAIN s.price AS numeric(10,2); CREATE DOMAIN s.code AS char(2); \
         CREATE DOMAIN s.scores AS integer[]; CREATE DOMAIN s.known AS s.pair; \
         CREATE TABLE s.t (id integer PRIMARY KEY, a int[][], b varchar(2)[], c int ARRAY[3], \
         d _int4, e numeric(6,1)[], p s.price, q s.code, r s.scores, k s.known, \
         n integer GENERATED ALWAYS AS (id * 2) STORED, pt point, v text, j jsonb, tags text[], \
         body tsvector); \
         CREATE INDEX t_pt ON s.t USING gist (pt); CREATE INDEX t_v ON s.t USING hash (v); \
         CREATE INDEX t_j ON s.t USING gin (j jsonb_path_ops); \
         CREATE INDEX t_tags ON s.t USING gin (tags); CREATE INDEX t_body ON s.t USING gist (body); \
         CREATE INDEX t_a ON s.t (a);";
    let mut s = Snail::open(&written("attributes", ddl)).unwrap();
    let q = "SELECT c.relname || '.' || a.attnum AS k, a.attname, a.atttypid::regtype::text, \
         a.atttypmod, a.attndims, a.attlen, a.attbyval, a.attalign::text, a.attstorage::text, \
         a.atthasdef FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = 's' AND c.relkind IN ('r', 'i') AND a.attnum > 0 \
         ORDER BY c.relname, a.attnum";
    let (b, _) = s.ask(q, None).unwrap_or_else(|e| panic!("{q}: {}", e.0));
    // A declared array's dimensions and its element's typmod, none of a domain's; a generation
    // held as a DEFAULT; each index key the type its class or method stores.
    assert_eq!(
        joined(&s, &b),
        [
            "t.1|id|integer|-1|0|4|t|i|p|f",
            "t.2|a|integer[]|-1|2|-1|f|i|x|f",
            "t.3|b|character varying[]|6|1|-1|f|i|x|f",
            "t.4|c|integer[]|-1|1|-1|f|i|x|f",
            "t.5|d|integer[]|-1|0|-1|f|i|x|f",
            "t.6|e|numeric[]|393221|1|-1|f|i|x|f",
            "t.7|p|s.price|-1|0|-1|f|i|m|f",
            "t.8|q|s.code|-1|0|-1|f|i|x|f",
            "t.9|r|s.scores|-1|0|-1|f|i|x|f",
            "t.10|k|s.known|-1|0|-1|f|d|x|f",
            "t.11|n|integer|-1|0|4|t|i|p|t",
            "t.12|pt|point|-1|0|16|f|d|p|f",
            "t.13|v|text|-1|0|-1|f|i|x|f",
            "t.14|j|jsonb|-1|0|-1|f|i|x|f",
            "t.15|tags|text[]|-1|1|-1|f|i|x|f",
            "t.16|body|tsvector|-1|0|-1|f|i|x|f",
            "t_a.1|a|integer[]|-1|2|-1|f|i|x|f",
            "t_body.1|body|gtsvector|-1|0|-1|f|i|p|f",
            "t_j.1|j|integer|-1|0|4|t|i|p|f",
            "t_pkey.1|id|integer|-1|0|4|t|i|p|f",
            "t_pt.1|pt|box|-1|0|32|f|d|p|f",
            "t_tags.1|tags|text|-1|1|-1|f|i|x|f",
            "t_v.1|v|integer|-1|0|4|t|i|p|f",
        ]
    );
}

#[test]
fn the_reg_types_name_the_objects_initdb_makes_as_postgresql_does() {
    let ddl = "CREATE SCHEMA s; CREATE TABLE s.t (id int PRIMARY KEY);";
    let mut s = Snail::open(&written("initdb-names", ddl)).unwrap();
    let mut asked = |q: &str, want: &[&str]| {
        let (b, _) = s.ask(q, None).unwrap_or_else(|e| panic!("{q}: {}", e.0));
        assert_eq!(joined(&s, &b), want, "{q}");
    };
    // 18.6: information_schema's qualified, pg_catalog's bare, by name and by OID.
    asked(
        "SELECT 'information_schema.tables'::regclass::oid, 13526::regclass, \
         'pg_roles'::regclass::oid, 12000::regclass",
        &["13526|information_schema.tables|12000|pg_roles"],
    );
    asked(
        "SELECT 'information_schema.cardinal_number'::regtype::oid, \
         'information_schema.cardinal_number[]'::regtype, 13323::regtype",
        &["13323|information_schema.cardinal_number[]|information_schema.cardinal_number"],
    );
    asked(
        "SELECT 'information_schema._pg_expandarray(anyarray)'::regprocedure::oid, \
         13310::regprocedure, 13310::regproc, \
         'information_schema._pg_char_max_length(oid, integer)'::regprocedure, \
         'plpgsql_validator'::regproc::oid > 12000",
        &["13310|information_schema._pg_expandarray(anyarray)|information_schema._pg_expandarray|information_schema._pg_char_max_length(oid,integer)|t"],
    );
    asked(
        "SELECT 'information_schema'::regnamespace::oid, 13309::regnamespace, \
         'information_schema._pg_expandarray'::regproc::oid",
        &["13309|information_schema|13310"],
    );
    // Not on the search path, so not found by its name alone.
    for q in ["SELECT '_pg_expandarray'::regproc", "SELECT 'tables'::regclass"] {
        let e = s.ask(q, None).err().unwrap_or_else(|| panic!("{q} was taken")).0;
        assert!(e.contains("does not exist"), "{q}: {e}");
    }
}

#[test]
fn information_schema_s_functions_answer_a_statement_s_own_calls_as_postgresql_does() {
    let ddl = "CREATE SCHEMA s; CREATE TABLE s.p (a integer, b integer, v varchar(14), \
         n numeric(10,2), t timestamptz(3), i interval day to second(2), PRIMARY KEY (b, a));";
    let mut s = Snail::open(&written("schema-functions", ddl)).unwrap();
    let mut asked = |q: &str, want: &[&str]| {
        let (b, _) = s.ask(q, None).unwrap_or_else(|e| panic!("{q}: {}", e.0));
        assert_eq!(joined(&s, &b), want, "{q}");
    };
    // 18.6: each inlined as the views inline it, the encoding its body reads the file's.
    asked(
        "SELECT information_schema._pg_char_max_length(1043, 14), \
         information_schema._pg_numeric_precision(23, -1), \
         information_schema._pg_char_octet_length(1043, 14), \
         information_schema._pg_numeric_precision_radix(23, -1), \
         information_schema._pg_numeric_scale(1700, 655366), \
         information_schema._pg_datetime_precision(1184, 3)",
        &["10|32|40|2|2|3"],
    );
    asked(
        "SELECT information_schema._pg_index_position('s.p_pkey'::regclass, 2::smallint), \
         information_schema._pg_index_position('s.p_pkey'::regclass, 1::smallint)",
        &["1|2"],
    );
    // In FROM, each element and its place, under the function's name or the alias given.
    asked(
        "SELECT x, n FROM information_schema._pg_expandarray(ARRAY[10, 20, 30])",
        &["10|1", "20|2", "30|3"],
    );
    asked(
        "SELECT e.n, e.x FROM information_schema._pg_expandarray(ARRAY['a', 'b']) AS e",
        &["1|a", "2|b"],
    );
    // Reading the relations before it, as a function in FROM does.
    asked(
        "SELECT c.conname || '.' || e.n, e.x FROM pg_constraint c, \
         information_schema._pg_expandarray(c.conkey) e \
         WHERE c.conrelid = 's.p'::regclass ORDER BY 1",
        &["p_a_not_null.1|1", "p_b_not_null.1|2", "p_pkey.1|2", "p_pkey.2|1"],
    );
    // Over a table's columns, each row argument a whole row.
    asked(
        "SELECT a.attname, information_schema._pg_truetypid(a.*, t.*)::regtype::text, \
         information_schema._pg_truetypmod(a.*, t.*), \
         information_schema._pg_char_max_length(information_schema._pg_truetypid(a.*, t.*), \
         information_schema._pg_truetypmod(a.*, t.*)), \
         information_schema._pg_interval_type(information_schema._pg_truetypid(a.*, t.*), \
         information_schema._pg_truetypmod(a.*, t.*)) \
         FROM pg_attribute a JOIN pg_type t ON t.oid = a.atttypid \
         WHERE a.attrelid = 's.p'::regclass AND a.attnum > 0 ORDER BY a.attnum",
        &[
            "a|integer|-1|NULL|NULL",
            "b|integer|-1|NULL|NULL",
            "v|character varying|18|14|NULL",
            "n|numeric|655366|NULL|NULL",
            "t|timestamp with time zone|3|NULL|NULL",
            "i|interval|470286338|NULL|DAY TO SECOND(2)",
        ],
    );
}
