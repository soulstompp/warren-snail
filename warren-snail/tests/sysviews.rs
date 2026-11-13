//! PostgreSQL's own views of pg_catalog and information_schema read over a snail's catalog: each
//! the definition of the file's release, a column only a server could fill refused where a
//! statement asks it, and a view whose rows only a server holds refused whole.

mod weather;

use arrow::array::{Array, StringArray};
use warren_snail::catalog::Catalog;
use warren_snail::read::Snail;
use weather::tmp;

const DDL: &str = "CREATE SCHEMA s; CREATE TABLE t (id bigint PRIMARY KEY, n text NOT NULL DEFAULT 'x', \
     v varchar(10), c numeric(8,2) CHECK (c > 0)); \
     CREATE TABLE s.u (id bigint PRIMARY KEY, t_id bigint REFERENCES t (id), k integer UNIQUE); \
     CREATE TABLE w (id serial PRIMARY KEY, n text); \
     CREATE VIEW tv AS SELECT id, n FROM t;";

fn written(name: &str, release: u8) -> std::path::PathBuf {
    let path = tmp(&format!("{name}.snail"));
    let catalog = Catalog::parse(DDL).unwrap();
    warren_snail::write::write_released(&path, &catalog, &[], &[], weather::span(), release)
        .unwrap();
    path
}

/// An answer's rows, the question casting every column to text.
fn rows(s: &mut Snail, q: &str) -> Vec<Vec<Option<String>>> {
    let (b, _) = s.ask(q, None).unwrap_or_else(|e| panic!("{q}: {}", e.0));
    let cols: Vec<&StringArray> = b
        .columns()
        .iter()
        .map(|a| {
            a.as_any()
                .downcast_ref::<StringArray>()
                .expect("a text column")
        })
        .collect();
    (0..b.num_rows())
        .map(|r| {
            cols.iter()
                .map(|c| c.is_valid(r).then(|| c.value(r).to_string()))
                .collect()
        })
        .collect()
}

fn s(v: &[&str]) -> Vec<Option<String>> {
    v.iter().map(|x| Some(x.to_string())).collect()
}

fn refused(s: &mut Snail, q: &str) -> String {
    match s.ask(q, None) {
        Ok(_) => panic!("{q}: answered"),
        Err(e) => e.0,
    }
}

#[test]
fn pg_tables_lists_the_files_tables_as_postgresql_does() {
    let mut snail = Snail::open(&written("sv-tables", 18)).unwrap();
    assert_eq!(
        rows(
            &mut snail,
            "SELECT schemaname::text, tablename::text, hasindexes::text, hastriggers::text \
             FROM pg_tables WHERE schemaname IN ('public', 's') ORDER BY tablename"
        ),
        [
            s(&["public", "t", "true", "true"]),
            s(&["s", "u", "true", "true"]),
            s(&["public", "w", "true", "false"]),
        ]
    );
    // An owner is a server's role.
    let e = refused(&mut snail, "SELECT tableowner FROM pg_tables");
    assert!(e.contains("pg_get_userbyid"), "{e}");
}

#[test]
fn information_schema_reads_the_files_tables_constraints_and_schemas() {
    let mut snail = Snail::open(&written("sv-information", 18)).unwrap();
    assert_eq!(
        rows(
            &mut snail,
            "SELECT table_name::text, table_type::text, is_insertable_into::text \
             FROM information_schema.tables WHERE table_schema IN ('public', 's') ORDER BY 1"
        ),
        [
            s(&["t", "BASE TABLE", "YES"]),
            s(&["tv", "VIEW", "YES"]),
            s(&["u", "BASE TABLE", "YES"]),
            s(&["w", "BASE TABLE", "YES"]),
        ]
    );
    assert_eq!(
        rows(
            &mut snail,
            "SELECT constraint_name::text, constraint_type::text \
             FROM information_schema.table_constraints WHERE table_name = 't' ORDER BY 1"
        ),
        [
            s(&["t_c_check", "CHECK"]),
            s(&["t_id_not_null", "CHECK"]),
            s(&["t_n_not_null", "CHECK"]),
            s(&["t_pkey", "PRIMARY KEY"]),
        ]
    );
    // public is owned by pg_database_owner from 15 on, and listed all the same.
    assert_eq!(
        rows(
            &mut snail,
            "SELECT schema_name::text FROM information_schema.schemata ORDER BY 1"
        ),
        [
            s(&["information_schema"]),
            s(&["pg_catalog"]),
            s(&["pg_toast"]),
            s(&["public"]),
            s(&["s"]),
        ]
    );
    assert_eq!(
        rows(
            &mut snail,
            "SELECT sequence_name::text, data_type::text, maximum_value::text \
             FROM information_schema.sequences"
        ),
        [s(&["w_id_seq", "integer", "2147483647"])]
    );
    // information_schema's own domains, through the functions it reads them by; the file's
    // encoding and collation as a database's.
    assert_eq!(
        rows(
            &mut snail,
            "SELECT domain_name::text, data_type::text, character_maximum_length::text, \
             numeric_precision::text, datetime_precision::text \
             FROM information_schema.domains ORDER BY 1"
        ),
        [
            vec![
                Some("cardinal_number".into()),
                Some("integer".into()),
                None,
                Some("32".into()),
                None
            ],
            vec![
                Some("character_data".into()),
                Some("character varying".into()),
                None,
                None,
                None
            ],
            vec![
                Some("sql_identifier".into()),
                Some("name".into()),
                None,
                None,
                None
            ],
            vec![
                Some("time_stamp".into()),
                Some("timestamp with time zone".into()),
                None,
                None,
                Some("2".into())
            ],
            vec![
                Some("yes_or_no".into()),
                Some("character varying".into()),
                Some("3".into()),
                None,
                None
            ],
        ]
    );
    assert_eq!(
        rows(
            &mut snail,
            "SELECT domain_name::text, character_octet_length::text \
             FROM information_schema.domains WHERE character_octet_length IS NOT NULL ORDER BY 1"
        ),
        [
            s(&["character_data", "1073741824"]),
            s(&["yes_or_no", "12"])
        ]
    );
    assert_eq!(
        rows(
            &mut snail,
            "SELECT character_set_name::text, character_repertoire::text, form_of_use::text \
             FROM information_schema.character_sets"
        ),
        [s(&["UTF8", "UCS", "UTF8"])]
    );
    // The database's name is a server's.
    let e = refused(
        &mut snail,
        "SELECT table_catalog FROM information_schema.tables",
    );
    assert!(e.contains("current_database"), "{e}");
}

#[test]
fn a_view_whose_rows_only_a_server_holds_is_refused_whole() {
    let mut snail = Snail::open(&written("sv-servers", 18)).unwrap();
    for (q, holds) in [
        ("SELECT count(*) FROM pg_roles", "roles"),
        (
            "SELECT count(*) FROM information_schema.enabled_roles",
            "roles",
        ),
        (
            "SELECT count(*) FROM information_schema.table_privileges",
            "privileges",
        ),
        ("SELECT count(*) FROM pg_file_settings", "settings"),
        ("SELECT count(*) FROM pg_locks", "locks"),
        ("SELECT count(*) FROM pg_stat_activity", "statistics"),
    ] {
        let e = refused(&mut snail, q);
        assert!(e.contains(holds) && e.contains("server"), "{q}: {e}");
    }
    // A view the file's release does not define does not exist.
    let e = refused(
        &mut snail,
        "SELECT count(*) FROM pg_dsm_registry_allocations",
    );
    assert!(e.contains("does not exist"), "{e}");
}

#[test]
fn what_postgresqls_views_are_written_in_is_read() {
    let mut snail = Snail::open(&written("sv-spellings", 18)).unwrap();
    assert_eq!(
        rows(
            &mut snail,
            "SELECT (1 = ANY (ARRAY[1, 2]))::text, (3 = ALL (ARRAY[3, 3]))::text, \
             (CASE 2 WHEN 1 THEN 'one' WHEN 2 THEN 'two' END)::text, \
             (CASE 'f' WHEN 'f'::\"char\" THEN 'full' END)::text, \
             (CASE 'ab' WHEN 'a'::\"char\" THEN 'x' ELSE 'y' END)::text, \
             ('a'::\"char\" = 'ab'::text)::text, ('a'::\"char\" = 'ab')::text"
        ),
        [s(&["true", "true", "two", "full", "y", "false", "true"])]
    );
    // A name and an OID, the name cut at a character so that both fit in 63 bytes.
    assert_eq!(
        rows(
            &mut snail,
            "SELECT nameconcatoid('abc', 16384)::text, \
             nameconcatoid(repeat('é', 40)::name, 1)::text"
        ),
        [s(&["abc_16384", &format!("{}_1", "é".repeat(30))])]
    );
    assert_eq!(
        rows(
            &mut snail,
            "SELECT c.conname::text, c.conkey[1]::text FROM pg_constraint c \
             WHERE c.conname = 'u_k_key'"
        ),
        [s(&["u_k_key", "3"])]
    );
    // A volatile operand is read once in PostgreSQL, and is not written into each WHEN.
    let e = refused(&mut snail, "SELECT CASE random() WHEN 1 THEN 1 END");
    assert!(e.contains("not read"), "{e}");
}

/// A row of text and NULLs, `-` a NULL.
fn n(v: &[&str]) -> Vec<Option<String>> {
    v.iter().map(|x| (*x != "-").then(|| x.to_string())).collect()
}

#[test]
fn pg_settings_holds_every_setting_a_server_s_own_values_refused() {
    let mut snail = Snail::open(&written("sv-settings", 18)).unwrap();
    // Every setting 18.6 shows, in its order; seed is none of them.
    assert_eq!(
        rows(
            &mut snail,
            "SELECT count(*)::text, count(*) FILTER (WHERE context = 'user')::text, \
             count(DISTINCT category)::text FROM pg_settings"
        ),
        [s(&["399", "151", "46"])]
    );
    assert_eq!(
        rows(&mut snail, "SELECT count(*)::text FROM pg_settings WHERE name = 'seed'"),
        [s(&["0"])]
    );
    assert_eq!(
        rows(&mut snail, "SELECT name FROM pg_settings LIMIT 3 OFFSET 20"),
        [
            s(&["autovacuum_vacuum_insert_scale_factor"]),
            s(&["autovacuum_vacuum_insert_threshold"]),
            s(&["autovacuum_vacuum_max_threshold"]),
        ]
    );
    // What the release says of each, a server's own among them.
    assert_eq!(
        rows(
            &mut snail,
            "SELECT name, unit, category, short_desc, extra_desc, context, vartype, min_val, \
             max_val, enumvals::text, boot_val FROM pg_settings WHERE name IN ('shared_buffers', \
             'work_mem', 'search_path', 'transaction_isolation', 'config_file') ORDER BY name"
        ),
        [
            n(&["config_file", "-", "File Locations", "Sets the server's main configuration file.",
                "-", "postmaster", "string", "-", "-", "-", "-"]),
            n(&["search_path", "-", "Client Connection Defaults / Statement Behavior",
                "Sets the schema search order for names that are not schema-qualified.", "-",
                "user", "string", "-", "-", "-", "\"$user\", public"]),
            n(&["shared_buffers", "8kB", "Resource Usage / Memory",
                "Sets the number of shared memory buffers used by the server.", "-", "postmaster",
                "integer", "16", "1073741823", "-", "16384"]),
            n(&["transaction_isolation", "-", "Client Connection Defaults / Statement Behavior",
                "Sets the current transaction's isolation level.", "-", "user", "enum", "-", "-",
                "{serializable,\"repeatable read\",\"read committed\",\"read uncommitted\"}",
                "read committed"]),
            n(&["work_mem", "kB", "Resource Usage / Memory",
                "Sets the maximum memory to be used for query workspaces.",
                "This much memory can be used by each internal sort operation and hash table \
                 before switching to temporary disk files.", "user", "integer", "64",
                "2147483647", "-", "4096"]),
        ]
    );
    // A session's own at its value in its base unit, where it comes from and its value at the
    // session's start.
    let values = "SELECT name, setting, unit, source, reset_val, boot_val, sourcefile, \
         sourceline::text, pending_restart::text FROM pg_settings WHERE name IN ('work_mem', \
         'statement_timeout', 'random_page_cost', 'enable_seqscan', 'IntervalStyle', \
         'extra_float_digits', 'server_encoding', 'transaction_isolation', \
         'default_transaction_isolation', 'vacuum_cost_delay', 'session_replication_role', \
         'client_encoding', 'integer_datetimes', 'max_identifier_length', 'xmloption', \
         'timezone_abbreviations') \
         ORDER BY name";
    assert_eq!(
        rows(&mut snail, values),
        [
            n(&["IntervalStyle", "postgres", "-", "default", "postgres", "postgres", "-", "-", "false"]),
            n(&["client_encoding", "UTF8", "-", "default", "UTF8", "SQL_ASCII", "-", "-", "false"]),
            n(&["default_transaction_isolation", "read committed", "-", "default",
                "read committed", "read committed", "-", "-", "false"]),
            n(&["enable_seqscan", "on", "-", "default", "on", "on", "-", "-", "false"]),
            n(&["extra_float_digits", "1", "-", "default", "1", "1", "-", "-", "false"]),
            n(&["integer_datetimes", "on", "-", "default", "on", "on", "-", "-", "false"]),
            n(&["max_identifier_length", "63", "-", "default", "63", "63", "-", "-", "false"]),
            n(&["random_page_cost", "4", "-", "default", "4", "4", "-", "-", "false"]),
            n(&["server_encoding", "UTF8", "-", "default", "UTF8", "SQL_ASCII", "-", "-", "false"]),
            n(&["session_replication_role", "origin", "-", "default", "origin", "origin", "-", "-",
                "false"]),
            n(&["statement_timeout", "0", "ms", "default", "0", "0", "-", "-", "false"]),
            // No boot value: the server puts PostgreSQL's Default set in place as it starts.
            n(&["timezone_abbreviations", "Default", "-", "default", "Default", "-", "-", "-",
                "false"]),
            n(&["transaction_isolation", "read committed", "-", "override", "read committed",
                "read committed", "-", "-", "false"]),
            n(&["vacuum_cost_delay", "0", "ms", "default", "0", "0", "-", "-", "false"]),
            n(&["work_mem", "4096", "kB", "default", "4096", "4096", "-", "-", "false"]),
            n(&["xmloption", "content", "-", "default", "content", "content", "-", "-", "false"]),
        ]
    );
    // A server's own row stands, and each of its values is refused by its name where it is read;
    // the search path's for its own reason.
    assert_eq!(
        rows(
            &mut snail,
            "SELECT name, boot_val FROM pg_settings WHERE name IN ('shared_buffers', 'search_path') \
             ORDER BY name"
        ),
        [s(&["search_path", "\"$user\", public"]), s(&["shared_buffers", "16384"])]
    );
    assert_eq!(
        refused(
            &mut snail,
            "SELECT setting FROM pg_settings WHERE name = 'shared_buffers'"
        ),
        "pg_settings.setting of shared_buffers is refused: shared_buffers is the server's own \
         setting, and a snail has no server"
    );
    assert_eq!(
        refused(&mut snail, "SELECT * FROM pg_settings"),
        "pg_settings.setting of allow_alter_system is refused: allow_alter_system is the \
         server's own setting, and a snail has no server"
    );
    assert_eq!(
        refused(
            &mut snail,
            "SELECT s.reset_val FROM pg_settings s WHERE s.name = 'search_path'"
        ),
        "pg_settings.reset_val of search_path is refused: a snail names each table by its \
         schema, and has no search path"
    );
    for q in [
        "SELECT (SELECT setting FROM pg_settings WHERE name = 'shared_buffers')",
        "SELECT count(s.setting)::text FROM pg_settings s",
        "SELECT name FROM pg_settings ORDER BY setting",
    ] {
        let e = refused(&mut snail, q);
        assert!(e.contains("is the server's own setting"), "{q}: {e}");
    }
    assert_eq!(
        rows(
            &mut snail,
            "SELECT (SELECT setting FROM pg_settings WHERE name = 'work_mem'), \
             (SELECT p.name FROM pg_settings p WHERE p.name = 'port')"
        ),
        [s(&["4096", "port"])]
    );
    for column in ["source", "sourcefile", "sourceline::text", "pending_restart::text"] {
        let e = refused(
            &mut snail,
            &format!("SELECT {column} FROM pg_settings WHERE name = 'port'"),
        );
        assert!(e.contains("of port is refused: port is the server's own setting"), "{e}");
    }
    // 19 RC1's settings for a file of 19.
    let mut nineteen = Snail::open(&written("sv-settings-19", 19)).unwrap();
    assert_eq!(
        rows(&mut nineteen, "SELECT count(*)::text FROM pg_settings"),
        [s(&["419"])]
    );
}

#[cfg(feature = "standalone")]
#[test]
fn pg_settings_shows_what_the_session_set() {
    let mut snail = Snail::open_writing(&written("sv-settings-set", 18)).unwrap();
    for set in [
        "SET work_mem = '64MB'",
        "SET random_page_cost = 2.5",
        "SET vacuum_cost_delay = '1500us'",
        "SET default_transaction_isolation = 'repeatable read'",
        "SET statement_timeout = '5min'",
        "SET timezone_abbreviations = 'Default'",
        "BEGIN",
        "SET LOCAL enable_seqscan = off",
    ] {
        snail.execute(set).unwrap_or_else(|e| panic!("{set}: {e}"));
    }
    let set = "SELECT name, setting, unit, source, reset_val FROM pg_settings WHERE name IN \
         ('work_mem', 'statement_timeout', 'random_page_cost', 'enable_seqscan', \
         'vacuum_cost_delay', 'transaction_isolation', 'default_transaction_isolation') \
         ORDER BY name";
    assert_eq!(
        rows(&mut snail, set),
        [
            n(&["default_transaction_isolation", "repeatable read", "-", "session",
                "read committed"]),
            n(&["enable_seqscan", "off", "-", "session", "on"]),
            n(&["random_page_cost", "2.5", "-", "session", "4"]),
            n(&["statement_timeout", "300000", "ms", "session", "0"]),
            n(&["transaction_isolation", "repeatable read", "-", "override", "read committed"]),
            n(&["vacuum_cost_delay", "1.5", "ms", "session", "0"]),
            n(&["work_mem", "65536", "kB", "session", "4096"]),
        ]
    );
    for sql in ["COMMIT", "RESET work_mem"] {
        snail.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    assert_eq!(
        rows(
            &mut snail,
            "SELECT name, setting, source, reset_val FROM pg_settings \
             WHERE name IN ('work_mem', 'enable_seqscan') ORDER BY name"
        ),
        [
            s(&["enable_seqscan", "on", "default", "on"]),
            s(&["work_mem", "4096", "default", "4096"]),
        ]
    );
}
