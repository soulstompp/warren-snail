//! pg_catalog's tables as relations: the built-in rows of the file's release and the rows of the
//! file's own objects, in the columns that release defines; a column only a server fills refused
//! where a statement asks it, and a table only a server holds refused whole; a view's rows refused
//! by its name where the snail does not print its question, never left out.

mod weather;

use arrow::array::{Array, StringArray};
use arrow::record_batch::RecordBatch;
use warren_snail::catalog::Catalog;
use warren_snail::read::Snail;
use weather::tmp;

const DDL: &str = "CREATE SCHEMA s; CREATE TABLE t (id bigint PRIMARY KEY, n text NOT NULL DEFAULT 'x', \
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

#[cfg(feature = "standalone")]
const GROUPED: &str = "CREATE SCHEMA dn; CREATE TABLE dn.a (id integer PRIMARY KEY, g integer, k integer); \
     CREATE VIEW dn.vd AS SELECT DISTINCT ON (g) g, id FROM dn.a ORDER BY g, id; \
     CREATE VIEW dn.vr AS SELECT count(*) FROM dn.a GROUP BY ROLLUP (g); \
     CREATE VIEW dn.vgr AS SELECT g, GROUPING(g) AS gr FROM dn.a GROUP BY id, ROLLUP (g); \
     CREATE VIEW dn.ve AS SELECT count(*) FROM dn.a GROUP BY ();";

#[cfg(feature = "standalone")]
#[test]
fn a_distinct_on_or_rollup_view_keeps_its_catalog_rows() {
    let path = tmp("pgc-grouped.snail");
    let catalog = Catalog::parse(GROUPED).unwrap();
    warren_snail::write::write_released(&path, &catalog, &[], &[], weather::span(), 18).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute("INSERT INTO dn.a VALUES (1, 1, 10), (2, 1, 20), (3, 2, 30), (4, 2, 40), (5, NULL, 50)")
        .unwrap();
    drop(w);
    let mut snail = Snail::open(&path).unwrap();
    // Every answer 18.6's after the same DDL.
    assert_eq!(
        rows(
            &mut snail,
            "SELECT attname::text, atttypid::regtype::text, attnum::text, attcollation::text \
             FROM pg_attribute WHERE attrelid = 'dn.vd'::regclass AND attnum > 0 ORDER BY attnum"
        ),
        [s(&["g", "integer", "1", "0"]), s(&["id", "integer", "2", "0"])]
    );
    assert_eq!(
        rows(
            &mut snail,
            "SELECT c.relname::text, a.attname::text, a.atttypid::regtype::text, a.attnum::text \
             FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid \
             WHERE c.relname IN ('vr', 'vgr', 've') AND a.attnum > 0 ORDER BY 1, a.attnum"
        ),
        [
            s(&["ve", "count", "bigint", "1"]),
            s(&["vgr", "g", "integer", "1"]),
            s(&["vgr", "gr", "integer", "2"]),
            s(&["vr", "count", "bigint", "1"]),
        ]
    );
    let rule = |view: &str| {
        format!(
            "SELECT refobjid::regclass::text, refobjsubid::text, deptype::text FROM pg_depend \
             WHERE classid = 'pg_rewrite'::regclass AND objid = \
             (SELECT oid FROM pg_rewrite WHERE ev_class = '{view}'::regclass) ORDER BY 1, 2"
        )
    };
    let on = |rel: &str, sub: &str, kind: &str| s(&[rel, sub, kind]);
    assert_eq!(
        rows(&mut snail, &rule("dn.vd")),
        [on("dn.a", "1", "n"), on("dn.a", "2", "n"), on("dn.vd", "0", "i")]
    );
    assert_eq!(
        rows(&mut snail, &rule("dn.vgr")),
        [on("dn.a", "1", "n"), on("dn.a", "2", "n"), on("dn.vgr", "0", "i")]
    );
    assert_eq!(
        rows(
            &mut snail,
            "SELECT column_name::text, data_type::text FROM information_schema.columns \
             WHERE table_name = 'vd' ORDER BY ordinal_position"
        ),
        [s(&["g", "integer"]), s(&["id", "integer"])]
    );
    assert_eq!(
        rows(&mut snail, "SELECT g::text, id::text FROM dn.vd"),
        [s(&["1", "1"]), s(&["2", "3"]), vec![None, Some("5".into())]]
    );
}

#[cfg(feature = "standalone")]
#[test]
fn a_view_the_snail_does_not_print_is_refused_by_name_never_left_out() {
    let path = tmp("pgc-unprinted.snail");
    let catalog = Catalog::parse(
        "CREATE SCHEMA dn; CREATE TABLE dn.a (id integer PRIMARY KEY, g integer, k integer); \
         CREATE VIEW dn.vw AS SELECT id, sum(k) OVER (ORDER BY id RANGE BETWEEN 1 PRECEDING AND \
         CURRENT ROW) AS s FROM dn.a;",
    )
    .unwrap();
    warren_snail::write::write_released(&path, &catalog, &[], &[], weather::span(), 18).unwrap();
    // 18.6 answers `id`, `g`, `k` and 5, and refuses the DROP 2BP01 for the view; the snail, which
    // does not yet print a RANGE frame with an offset, refuses each by the view's name.
    let why = "the view dn.vw is not read: a RANGE frame with an offset is not printed as PostgreSQL prints it";
    let mut snail = Snail::open(&path).unwrap();
    for q in [
        "SELECT attname::text FROM pg_attribute WHERE attrelid = 'dn.a'::regclass AND attnum > 0 ORDER BY attnum",
        "SELECT count(*)::text FROM pg_depend WHERE refobjid = 'dn.a'::regclass",
    ] {
        assert_eq!(snail.ask(q, None).unwrap_err().0, why, "{q}");
    }
    drop(snail);
    let mut w = Snail::open_writing(&path).unwrap();
    assert_eq!(w.execute("DROP TABLE dn.a").unwrap_err().0, why);
    drop(w);
    let mut snail = Snail::open(&path).unwrap();
    assert_eq!(snail.ask("SELECT id FROM dn.a", None).unwrap().0.num_rows(), 0);
}

// 18.6 over the same DDL: a range is aligned as a double where its subtype is, else as an integer,
// its multirange and both arrays alike, and so is a column of each.
#[test]
fn a_file_s_range_is_aligned_as_its_subtype() {
    let ddl = "CREATE SCHEMA s; CREATE TYPE s.tr AS RANGE (subtype = text); \
         CREATE TYPE s.ir AS RANGE (subtype = int4); CREATE TYPE s.br AS RANGE (subtype = int8); \
         CREATE TYPE s.fr AS RANGE (subtype = float8); CREATE TYPE s.nr AS RANGE (subtype = numeric); \
         CREATE TYPE s.dr AS RANGE (subtype = date); \
         CREATE TABLE s.t (id integer PRIMARY KEY, a s.tr, b s.ir, c s.br, d s.fr, e s.nr, f s.dr, \
         h s.tr_multirange, i s.ir[], j s.br_multirange);";
    let path = tmp("ranges-aligned.snail");
    let catalog = Catalog::parse(ddl).unwrap();
    warren_snail::write::write_released(&path, &catalog, &[], &[], weather::span(), 18).unwrap();
    let mut snail = Snail::open(&path).unwrap();
    let pairs = |rows: Vec<Vec<Option<String>>>| -> Vec<String> {
        rows.into_iter()
            .map(|r| r.into_iter().map(Option::unwrap_or_default).collect::<Vec<_>>().join("|"))
            .collect()
    };
    let types = rows(
        &mut snail,
        "SELECT typname::text, typalign::text FROM pg_type \
         WHERE typnamespace = 's'::regnamespace ORDER BY typname",
    );
    assert_eq!(
        pairs(types),
        [
            "_br|d", "_br_multirange|d", "_dr|i", "_dr_multirange|i", "_fr|d", "_fr_multirange|d",
            "_ir|i", "_ir_multirange|i", "_nr|i", "_nr_multirange|i", "_t|d", "_tr|i",
            "_tr_multirange|i", "br|d", "br_multirange|d", "dr|i", "dr_multirange|i", "fr|d",
            "fr_multirange|d", "ir|i", "ir_multirange|i", "nr|i", "nr_multirange|i", "t|d", "tr|i",
            "tr_multirange|i",
        ]
    );
    let columns = rows(
        &mut snail,
        "SELECT attname::text, attalign::text FROM pg_attribute \
         WHERE attrelid = 's.t'::regclass AND attnum > 0 ORDER BY attnum",
    );
    assert_eq!(
        pairs(columns),
        ["id|i", "a|i", "b|i", "c|d", "d|d", "e|i", "f|i", "h|i", "i|i", "j|d"]
    );
}
