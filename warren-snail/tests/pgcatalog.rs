//! pg_catalog's tables as relations: the built-in rows of the file's release and the rows of the
//! file's own objects, in the columns that release defines; a column only a server fills refused
//! where a statement asks it, and a table only a server holds refused whole.

mod weather;

use arrow::array::{Array, StringArray};
use arrow::record_batch::RecordBatch;
use warren_snail::catalog::Catalog;
use warren_snail::read::Snail;
use weather::tmp;

const DDL: &str = "CREATE TABLE t (id bigint PRIMARY KEY, n text NOT NULL DEFAULT 'x', \
     v varchar(10), c numeric(8,2) CHECK (c > 0)); \
     CREATE TABLE s.u (id bigint PRIMARY KEY, t_id bigint REFERENCES t (id), k integer UNIQUE); \
     CREATE TYPE mood AS ENUM ('sad', 'ok', 'glad'); \
     CREATE TABLE w (id serial PRIMARY KEY, m mood); \
     CREATE TABLE pg_class (id integer PRIMARY KEY); \
     CREATE INDEX t_n ON t (n);";

fn written(name: &str, release: u8) -> std::path::PathBuf {
    let path = tmp(&format!("{name}.snail"));
    let catalog = Catalog::parse(DDL).unwrap();
    warren_snail::write::write_released(&path, &catalog, &[], &[], weather::span(), release)
        .unwrap();
    path
}

/// An answer's rows, each cell as text, the question casting every column to text.
fn rows(s: &mut Snail, q: &str) -> Vec<Vec<Option<String>>> {
    let (b, _) = s.ask(q, None).unwrap_or_else(|e| panic!("{q}: {}", e.0));
    text_rows(&b)
}

fn text_rows(b: &RecordBatch) -> Vec<Vec<Option<String>>> {
    let cols: Vec<&StringArray> = b
        .columns()
        .iter()
        .map(|a| a.as_any().downcast_ref::<StringArray>().expect("a text column"))
        .collect();
    (0..b.num_rows())
        .map(|r| cols.iter().map(|c| c.is_valid(r).then(|| c.value(r).to_string())).collect())
        .collect()
}

fn s(v: &[&str]) -> Vec<Option<String>> {
    v.iter().map(|x| Some(x.to_string())).collect()
}

#[test]
fn the_files_relations_and_their_columns_are_rows_of_pg_class_and_pg_attribute() {
    let mut snail = Snail::open(&written("pgc-classes", 19)).unwrap();
    assert_eq!(
        rows(
            &mut snail,
            "SELECT relname::text, relkind::text, relnatts::text FROM pg_class \
             WHERE relnamespace IN ('public'::regnamespace, 's'::regnamespace) ORDER BY relname"
        ),
        [
            s(&["pg_class", "r", "1"]),
            s(&["pg_class_pkey", "i", "1"]),
            s(&["t", "r", "4"]),
            s(&["t_n", "i", "1"]),
            s(&["t_pkey", "i", "1"]),
            s(&["u", "r", "3"]),
            s(&["u_k_key", "i", "1"]),
            s(&["u_pkey", "i", "1"]),
            s(&["w", "r", "2"]),
            s(&["w_id_seq", "S", "3"]),
            s(&["w_pkey", "i", "1"]),
        ]
    );
    assert_eq!(
        rows(
            &mut snail,
            "SELECT attname::text, atttypid::regtype::text, atttypmod::text, attnotnull::text, \
             atthasdef::text FROM pg_attribute WHERE attrelid = 't'::regclass AND attnum > 0 \
             ORDER BY attnum"
        ),
        [
            s(&["id", "bigint", "-1", "true", "false"]),
            s(&["n", "text", "-1", "true", "true"]),
            s(&["v", "character varying", "14", "false", "false"]),
            s(&["c", "numeric", "524294", "false", "false"]),
        ]
    );
    // An enum's column is of the enum, its labels in their order.
    assert_eq!(
        rows(
            &mut snail,
            "SELECT atttypid::regtype::text FROM pg_attribute WHERE attrelid = 'w'::regclass \
             AND attname = 'm'"
        ),
        [s(&["mood"])]
    );
    assert_eq!(
        rows(
            &mut snail,
            "SELECT enumlabel::text FROM pg_enum WHERE enumtypid = 'mood'::regtype \
             ORDER BY enumsortorder"
        ),
        [s(&["sad"]), s(&["ok"]), s(&["glad"])]
    );
}

#[test]
fn the_files_constraints_indexes_and_sequences_are_their_catalogs_rows() {
    let mut snail = Snail::open(&written("pgc-keys", 19)).unwrap();
    assert_eq!(
        rows(
            &mut snail,
            "SELECT conname::text, contype::text FROM pg_constraint \
             WHERE conrelid IN ('t'::regclass, 's.u'::regclass) ORDER BY conname"
        ),
        [
            s(&["t_c_check", "c"]),
            s(&["t_id_not_null", "n"]),
            s(&["t_n_not_null", "n"]),
            s(&["t_pkey", "p"]),
            s(&["u_id_not_null", "n"]),
            s(&["u_k_key", "u"]),
            s(&["u_pkey", "p"]),
            s(&["u_t_id_fkey", "f"]),
        ]
    );
    assert_eq!(
        rows(
            &mut snail,
            "SELECT indexrelid::regclass::text, indisprimary::text, indkey::text FROM pg_index \
             WHERE indrelid = 't'::regclass ORDER BY 1"
        ),
        [s(&["t_n", "false", "2"]), s(&["t_pkey", "true", "1"])]
    );
    assert_eq!(
        rows(
            &mut snail,
            "SELECT seqtypid::regtype::text, seqstart::text, seqincrement::text FROM pg_sequence \
             WHERE seqrelid = 'w_id_seq'::regclass"
        ),
        [s(&["integer", "1", "1"])]
    );
}

#[test]
fn the_built_in_rows_are_the_files_releases() {
    // A file of 14 answers with 14's catalog, one of 19 with 19's: each its own count of types and
    // its own columns.
    let count = |release: u8| {
        let mut snail = Snail::open(&written(&format!("pgc-release-{release}"), release)).unwrap();
        rows(
            &mut snail,
            "SELECT count(*)::text FROM pg_type WHERE typnamespace = 'pg_catalog'::regnamespace",
        )[0][0]
            .clone()
            .unwrap()
            .parse::<u32>()
            .unwrap()
    };
    let (fourteen, nineteen) = (count(14), count(19));
    assert!(fourteen > 400 && nineteen > fourteen, "{fourteen} {nineteen}");
    let mut snail = Snail::open(&written("pgc-builtin", 19)).unwrap();
    assert_eq!(
        rows(
            &mut snail,
            "SELECT typname::text, typlen::text, typbyval::text FROM pg_type WHERE oid = 23"
        ),
        [s(&["int4", "4", "true"])]
    );
    assert_eq!(
        rows(&mut snail, "SELECT amname::text FROM pg_am WHERE oid = 403"),
        [s(&["btree"])]
    );
}

#[test]
fn pg_catalog_is_searched_before_the_files_own_names() {
    let mut snail = Snail::open(&written("pgc-shadow", 19)).unwrap();
    // pg_class alone is the catalog's, though the file holds a table of that name.
    let catalog = rows(&mut snail, "SELECT count(*)::text FROM pg_class");
    assert!(catalog[0][0].as_deref().unwrap().parse::<u32>().unwrap() > 100);
}

#[test]
fn a_column_or_a_table_only_a_server_fills_is_refused_by_name() {
    let mut snail = Snail::open(&written("pgc-refused", 19)).unwrap();
    for (q, word) in [
        ("SELECT relpages FROM pg_class", "relpages"),
        ("SELECT * FROM pg_namespace", "nspowner"),
        ("SELECT attmissingval FROM pg_attribute", "attmissingval"),
        ("SELECT rolname FROM pg_authid", "pg_authid"),
    ] {
        let e = snail.ask(q, None).unwrap_err();
        assert!(e.0.contains(word) && e.0.contains("server"), "{q}: {}", e.0);
    }
}

#[test]
fn several_snails_read_no_catalog_table() {
    let a = written("pgc-several-a", 19);
    let b = tmp("pgc-several-b.snail");
    let other = Catalog::parse("CREATE TABLE z (id bigint PRIMARY KEY);").unwrap();
    warren_snail::write::write_released(&b, &other, &[], &[], weather::span(), 19).unwrap();
    let mut both = warren_snail::read::Snails::open(&[a.as_path(), b.as_path()]).unwrap();
    let e = both.ask("SELECT relname FROM pg_class").unwrap_err();
    assert!(e.0.contains("one snail at a time"), "{}", e.0);
}
