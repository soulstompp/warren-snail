//! A file's own objects and their OIDs: handed out from 16384 in the order the DDL makes them, kept
//! as the file is opened again and grows, and named by the reg* types as PostgreSQL names them; and
//! the system columns, tableoid the table's OID and ctid each row's address in the generation its
//! statement reads.

mod weather;

use arrow::array::{Array, StringArray};
use arrow::record_batch::RecordBatch;
use warren_snail::catalog::Catalog;
use warren_snail::oids::Object;
use warren_snail::read::{Snail, Snails};
use warren_snail::values::Values;
use warren_snail::write::Rows;
use weather::{span, tmp, write};

const DDL: &str = "CREATE TABLE t (id bigint PRIMARY KEY, n text); \
     CREATE SCHEMA s; CREATE TABLE s.u (id bigint PRIMARY KEY, t_id bigint REFERENCES t (id)); \
     CREATE TYPE mood AS ENUM ('sad', 'ok'); \
     CREATE TABLE \"Mixed\" (id integer PRIMARY KEY); \
     CREATE TABLE pg_class (id integer PRIMARY KEY); \
     CREATE INDEX t_n ON t (n);";

fn rows_of_t(n: i64) -> Rows {
    Rows {
        columns: vec![
            Values::Int((1..=n).map(Some).collect()),
            Values::Text((1..=n).map(|i| Some(format!("n{}", i % 7))).collect()),
        ],
    }
}

fn written(name: &str, n: i64) -> std::path::PathBuf {
    let path = tmp(&format!("{name}.snail"));
    let catalog = Catalog::parse(DDL).unwrap();
    write(&path, &catalog, &[("t".into(), rows_of_t(n))]).unwrap();
    path
}

/// Each column of an answer as text, NULL as none.
fn texts(b: &RecordBatch) -> Vec<Vec<Option<String>>> {
    b.columns()
        .iter()
        .map(|a| {
            let t = a
                .as_any()
                .downcast_ref::<StringArray>()
                .expect("a text column");
            (0..t.len())
                .map(|r| t.is_valid(r).then(|| t.value(r).to_string()))
                .collect()
        })
        .collect()
}

fn one_row(b: &RecordBatch) -> Vec<String> {
    texts(b)
        .into_iter()
        .map(|c| c[0].clone().unwrap())
        .collect()
}

#[test]
fn oids_are_handed_out_from_16384_in_the_ddls_order_and_kept() {
    let catalog = Catalog::parse(DDL).unwrap();
    let o = &catalog.oids;
    let of = |x: Object| o.of(&x).unwrap();
    // The first table's relation, its row type and the type's array, in that order.
    assert_eq!(of(Object::Class("public.t".into())), 16384);
    assert_eq!(of(Object::Type("public.t".into())), 16385);
    assert_eq!(of(Object::Array("public.t".into())), 16386);
    // A schema before the first object in it; each later statement's objects after.
    let s = of(Object::Namespace("s".into()));
    let u = of(Object::Class("s.u".into()));
    assert!(of(Object::Class("public.t_pkey".into())) < s && s < u);
    assert!(u < of(Object::Type("public.mood".into())));
    assert!(
        of(Object::Label("public.mood".into(), "sad".into()))
            < of(Object::Label("public.mood".into(), "ok".into()))
    );
    let index = of(Object::Class("public.t_n".into()));
    assert_eq!(index + 1, o.next());
    // Every object has its own.
    let all: Vec<u32> = o.each().map(|(oid, _)| oid).collect();
    assert_eq!(all, (16384..o.next()).collect::<Vec<_>>());
    // Opened again, the file's DDL hands out the same.
    let path = written("oids-kept", 10);
    assert_eq!(*Snail::open(&path).unwrap().catalog.oids, **o);
}

#[cfg(feature = "standalone")]
#[test]
fn a_table_made_later_takes_the_next_oids_and_every_other_keeps_its_own() {
    let o = Catalog::parse(DDL).unwrap().oids;
    let path = written("oids-grown", 10);
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute("CREATE TABLE s.extra (a bigint PRIMARY KEY, b text)")
        .unwrap();
    drop(w);
    let again = Snail::open(&path).unwrap();
    let extra = again
        .catalog
        .oids
        .of(&Object::Class("s.extra".into()))
        .unwrap();
    assert_eq!(extra, o.next());
    for (oid, x) in o.each() {
        assert_eq!(again.catalog.oids.of(x), Some(oid), "{x:?}");
    }
}

/// A table's ctids along its order, as text: each block's rows a run under one block number, never
/// another block's, their places from 1; the runs' lengths.
fn runs_of(ctids: &[Option<String>]) -> Vec<u64> {
    let parsed: Vec<(u64, u64)> = ctids
        .iter()
        .map(|c| {
            let c = c.as_deref().unwrap();
            let (b, p) = c
                .trim_matches(|x| x == '(' || x == ')')
                .split_once(',')
                .unwrap();
            (b.parse().unwrap(), p.parse().unwrap())
        })
        .collect();
    let mut runs: Vec<(u64, u64)> = Vec::new();
    for (b, p) in parsed {
        match runs.last_mut() {
            Some((block, n)) if *block == b => {
                *n += 1;
                assert_eq!(p, *n, "a block's places run from 1 without a gap");
            }
            _ => {
                assert!(runs.iter().all(|(x, _)| *x != b), "block {b} met twice");
                assert_eq!(p, 1, "a block's first row is its place 1");
                runs.push((b, 1));
            }
        }
    }
    runs.into_iter().map(|(_, n)| n).collect()
}

fn ctids_in_order(snail: &mut Snail) -> Vec<Option<String>> {
    let (b, _) = snail
        .ask(
            "WITH q AS (SELECT ctid::text AS c, id FROM t) SELECT c FROM q ORDER BY id",
            None,
        )
        .unwrap();
    texts(&b).remove(0)
}

fn block_rows(snail: &Snail) -> Vec<u64> {
    snail
        .blocks()
        .unwrap()
        .iter()
        .filter(|r| r.order == "t_pkey" && r.column == "id")
        .map(|r| r.rows)
        .collect()
}

#[test]
fn a_renamed_label_keeps_its_oid_and_a_dropped_constraint_gives_its_up() {
    let ddl = "CREATE TYPE mood AS ENUM ('sad', 'ok'); \
               CREATE DOMAIN posint AS integer CONSTRAINT pos CHECK (VALUE > 0); \
               ALTER TYPE mood RENAME VALUE 'ok' TO 'fine'; \
               ALTER DOMAIN posint RENAME CONSTRAINT pos TO positive; \
               ALTER DOMAIN posint ADD CONSTRAINT small CHECK (VALUE < 100); \
               ALTER DOMAIN posint DROP CONSTRAINT positive;";
    let first = Catalog::parse("CREATE TYPE mood AS ENUM ('sad', 'ok'); CREATE DOMAIN posint AS integer CONSTRAINT pos CHECK (VALUE > 0);").unwrap();
    let c = Catalog::parse(ddl).unwrap();
    let label =
        |cat: &Catalog, l: &str| cat.oids.of(&Object::Label("public.mood".into(), l.into()));
    assert_eq!(label(&c, "fine"), label(&first, "ok"));
    assert_eq!(label(&c, "ok"), None);
    let constraint = |cat: &Catalog, n: &str| {
        cat.oids
            .of(&Object::Constraint("public.posint".into(), n.into()))
    };
    let pos = constraint(&first, "pos").unwrap();
    assert_eq!(constraint(&c, "positive"), None);
    assert_eq!(c.oids.object(pos), None);
    let small = constraint(&c, "small").unwrap();
    assert_eq!(small, first.oids.next());
    assert_eq!(c.oids.next(), small + 1);
}

#[test]
fn a_reg_value_of_the_files_own_object_reads_and_prints_its_name_as_postgresql_does() {
    let path = written("oids-names", 10);
    let mut snail = Snail::open(&path).unwrap();
    let (b, _) = snail
        .ask(
            "SELECT 't'::regclass::text AS a, 't'::regclass::oid::text AS b, \
             's.u'::regclass::text AS c, 'mood'::regtype::text AS d, 'mood[]'::regtype::text AS e, \
             '\"Mixed\"'::regclass::text AS f, 'public.pg_class'::regclass::text AS g, \
             'pg_class'::regclass::oid::text AS h, 's'::regnamespace::text AS i, \
             't_n'::regclass::text AS j, 's.u_pkey'::regclass::text AS k, \
             '16384'::regclass::text AS l, '99999'::regclass::text AS m, \
             't'::regtype::text AS n",
            None,
        )
        .unwrap();
    assert_eq!(
        one_row(&b),
        [
            "t",
            "16384",
            "s.u",
            "mood",
            "mood[]",
            "\"Mixed\"",
            "public.pg_class",
            "1259",
            "s",
            "t_n",
            "s.u_pkey",
            "t",
            "99999",
            "t"
        ]
    );
    let e = snail
        .ask("SELECT 'nothing'::regclass::text", None)
        .unwrap_err();
    assert!(
        e.0.starts_with("relation \"nothing\" does not exist"),
        "{}",
        e.0
    );
}

#[test]
fn tableoid_is_the_tables_oid_and_ctid_each_rows_block_and_place() {
    // 100,000 rows: about 200 blocks, under a level of the directory above them.
    let path = written("oids-ctid", 100_000);
    let mut snail = Snail::open(&path).unwrap();
    let (b, _) = snail
        .ask(
            "WITH q AS (SELECT ctid::text AS c, tableoid::regclass::text AS o, id FROM t) \
             SELECT c, o FROM q ORDER BY id",
            None,
        )
        .unwrap();
    let cols = texts(&b);
    assert!(cols[1].iter().all(|o| o.as_deref() == Some("t")));
    // Along the order each block's rows in turn, one block number a block, its rows from 1; and
    // the same again on a second read of the generation.
    let blocks = block_rows(&snail);
    assert!(blocks.len() > 50, "{blocks:?}");
    assert_eq!(runs_of(&cols[0]), blocks);
    assert_eq!(ctids_in_order(&mut snail), cols[0]);
    // The question path reads them too, in its select list.
    let (b, _) = snail
        .ask("SELECT tableoid, id FROM t WHERE id <= 2", None)
        .unwrap();
    assert_eq!(b.num_rows(), 2);
    // A self-join on ctid meets each row with itself.
    let (b, _) = snail
        .ask(
            "WITH q AS (SELECT count(*)::text AS n FROM t a JOIN t b ON a.ctid = b.ctid) \
             SELECT n FROM q",
            None,
        )
        .unwrap();
    assert_eq!(one_row(&b), ["100000"]);
    // A star reads no system column, alone or qualified.
    let (b, _) = snail
        .ask(
            "WITH q AS (SELECT t.*, ctid FROM t WHERE id <= 1) SELECT * FROM q",
            None,
        )
        .unwrap();
    assert_eq!(b.num_columns(), 3);
}

#[test]
fn a_ctid_from_outside_the_statement_and_the_heaps_own_columns_are_refused_by_name() {
    let path = written("oids-refused", 10);
    let mut snail = Snail::open(&path).unwrap();
    let e = snail
        .ask(
            "WITH q AS (SELECT id FROM t WHERE ctid = '(0,1)') SELECT * FROM q",
            None,
        )
        .unwrap_err();
    assert!(e.0.contains("from outside the statement"), "{}", e.0);
    for c in ["xmin", "xmax", "cmin", "cmax"] {
        let e = snail.ask(&format!("SELECT {c} FROM t"), None).unwrap_err();
        assert!(e.0.contains("heap tuple"), "{c}: {}", e.0);
    }
    for c in ["ctid", "tableoid", "xmin"] {
        let e = Catalog::parse(&format!("CREATE TABLE x ({c} integer PRIMARY KEY);")).unwrap_err();
        assert!(
            e.0.contains("conflicts with a system column name"),
            "{c}: {}",
            e.0
        );
    }
}

#[test]
fn a_file_of_format_8_gives_each_row_its_ctid_as_format_9_does() {
    // A block's first page is in its page map in every format.
    let path = tmp("oids-format-8.snail");
    let catalog = Catalog::parse("CREATE TABLE t (id bigint PRIMARY KEY, n text);").unwrap();
    warren_snail::write::write_format_eight(
        &path,
        &catalog,
        &[("t".into(), rows_of_t(20_000))],
        span(),
    )
    .unwrap();
    let mut snail = Snail::open(&path).unwrap();
    let ctids = ctids_in_order(&mut snail);
    assert_eq!(runs_of(&ctids), block_rows(&snail));
}

#[test]
fn several_snails_name_none_of_their_objects_by_oid() {
    let a = written("oids-several-a", 10);
    let b = tmp("oids-several-b.snail");
    let other = Catalog::parse("CREATE TABLE w (id bigint PRIMARY KEY);").unwrap();
    write(&b, &other, &[]).unwrap();
    let mut both = Snails::open(&[a.as_path(), b.as_path()]).unwrap();
    let e = both.ask("SELECT 't'::regclass::text").unwrap_err();
    assert!(e.0.contains("several snails"), "{}", e.0);
    let (r, _) = both.ask("SELECT 'pg_class'::regclass::oid::text").unwrap();
    assert_eq!(one_row(&r), ["1259"]);
}

/// A file whose table keeps a regclass, for the OIDs its writers' objects take.
#[cfg(feature = "standalone")]
fn keeping(name: &str) -> std::path::PathBuf {
    let path = tmp(&format!("{name}.snail"));
    let ddl = "CREATE SCHEMA s; CREATE TABLE s.keep (k integer NOT NULL, r regclass, PRIMARY KEY (k));";
    write(&path, &Catalog::parse(ddl).unwrap(), &[]).unwrap();
    path
}

#[cfg(feature = "standalone")]
fn run(w: &mut Snail, sql: &str) {
    w.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
}

#[cfg(feature = "standalone")]
#[test]
fn a_file_one_writer_made_records_no_statement() {
    // One writer, its DDL in a transaction block and out of one: each object's OIDs follow the
    // text, so the root keeps no counter and records no statement, as before them.
    let path = keeping("oids-one-writer");
    let mut w = Snail::open_writing(&path).unwrap();
    for sql in [
        "CREATE TABLE s.a (id integer PRIMARY KEY)",
        "BEGIN",
        "CREATE TABLE s.b (id integer PRIMARY KEY)",
        "CREATE SEQUENCE s.q",
        "COMMIT",
        "CREATE TABLE s.c (id integer PRIMARY KEY)",
    ] {
        run(&mut w, sql);
    }
    drop(w);
    let root = warren_snail::file::Reader::open(&path).unwrap().root().unwrap();
    assert_eq!((root.next_oid, root.oids.bytes), (0, 0));
    let s = Snail::open(&path).unwrap();
    assert!(s.catalog.oids.departures().is_empty());
    let all: Vec<u32> = s.catalog.oids.each().map(|(oid, _)| oid).collect();
    assert_eq!(all, (16384..s.catalog.oids.next()).collect::<Vec<_>>());
}

#[cfg(feature = "standalone")]
#[test]
fn recut_keeps_each_object_s_oid() {
    // A transaction's table committed after another's, so that the root records both: recut, each
    // keeps its OID, and the regclass the first stored still names it.
    let path = keeping("oids-recut");
    let (mut a, mut b) = (Snail::open_writing(&path).unwrap(), Snail::open_writing(&path).unwrap());
    run(&mut a, "BEGIN");
    run(&mut a, "CREATE TABLE s.o1 (id integer PRIMARY KEY)");
    run(&mut a, "INSERT INTO s.keep (k, r) VALUES (1, 's.o1')");
    run(&mut b, "CREATE TABLE s.o2 (id integer PRIMARY KEY)");
    run(&mut a, "COMMIT");
    drop(b);
    let to = tmp("oids-recut-to.snail");
    a.recut(&to).unwrap();
    let sql = "SELECT r::text, ('s.o1'::regclass::oid < 's.o2'::regclass::oid)::text FROM s.keep";
    let mut r = Snail::open(&to).unwrap();
    let (x, _) = r.ask(sql, None).unwrap();
    assert_eq!(one_row(&x), ["s.o1", "true"]);
    let from = Snail::open(&path).unwrap();
    assert_eq!(r.catalog.oids.departures(), from.catalog.oids.departures());
    assert_eq!(r.catalog.oids.departures().len(), 2);
}

#[cfg(feature = "standalone")]
#[test]
fn a_snail_of_format_8_gives_oids_in_its_text_s_order() {
    // Its root holds no counter (ledgers/w10-plan.md, reading 1): a transaction's own table takes
    // its OIDs after another's committed since, as the text gives them, and nothing is recorded.
    let path = tmp("oids-format-8-writers.snail");
    let catalog = Catalog::parse("CREATE TABLE t (id bigint PRIMARY KEY, n text);").unwrap();
    warren_snail::write::write_format_eight(&path, &catalog, &[], span()).unwrap();
    let (mut a, mut b) = (Snail::open_writing(&path).unwrap(), Snail::open_writing(&path).unwrap());
    run(&mut a, "BEGIN");
    run(&mut a, "CREATE TABLE o1 (id bigint PRIMARY KEY)");
    run(&mut b, "CREATE TABLE o2 (id bigint PRIMARY KEY)");
    run(&mut a, "COMMIT");
    drop((a, b));
    let mut r = Snail::open(&path).unwrap();
    let (x, _) = r
        .ask("SELECT ('o2'::regclass::oid < 'o1'::regclass::oid)::text", None)
        .unwrap();
    assert_eq!(one_row(&x), ["true"]);
    let root = warren_snail::file::Reader::open(&path).unwrap().root().unwrap();
    assert_eq!((root.next_oid, root.oids.bytes), (0, 0));
}
