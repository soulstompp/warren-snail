//! PostgreSQL's own views of pg_catalog and information_schema read over a snail's catalog: each
//! the definition of the file's release, a column only a server could fill refused where a
//! statement asks it, and a view whose rows only a server holds refused whole.

mod weather;

use arrow::array::{Array, StringArray};
use warren_snail::catalog::Catalog;
use warren_snail::read::Snail;
use weather::tmp;

const DDL: &str = "CREATE TABLE t (id bigint PRIMARY KEY, n text NOT NULL DEFAULT 'x', \
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
            s(&["tv", "VIEW", "NO"]),
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
        ("SELECT count(*) FROM pg_settings", "settings"),
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
