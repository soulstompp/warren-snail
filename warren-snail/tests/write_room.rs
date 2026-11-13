//! A write within its room: each order's rows past the room written once and read back once, its
//! blocks written as they close, and its file the file a write of a room past every row makes, page
//! for page but the root's two.

mod weather;

use std::path::Path;
use std::sync::Arc;
use warren_snail::catalog::Catalog;
use warren_snail::composed::{scalar_in, text_in, Compound};
use warren_snail::values::{push_datum, Values};
use warren_snail::write::{write_within, Rows, Written};
use weather::*;

/// A room past every row a test writes.
const WHOLE: u64 = 1 << 40;

/// A snail's files, the first past its root's two pages.
fn past_the_root(path: &Path) -> Vec<Vec<u8>> {
    let mut out = vec![std::fs::read(path).unwrap()[8192..].to_vec()];
    for n in 1.. {
        let mut p = path.as_os_str().to_owned();
        p.push(format!(".{n}"));
        match std::fs::read(&p) {
            Ok(b) => out.push(b),
            Err(_) => break,
        }
    }
    out
}

/// Writes `tables` within `room` and within a room past every row: the two files the same past
/// their roots, and what each says of its orders.
fn alike(name: &str, catalog: &Catalog, tables: &[(String, Rows)], room: u64) -> Vec<Written> {
    let whole = tmp(&format!("room-{name}-whole.snail"));
    let within = tmp(&format!("room-{name}-within.snail"));
    let a = write_within(&whole, catalog, tables, &[], span(), WHOLE).unwrap();
    let b = write_within(&within, catalog, tables, &[], span(), room).unwrap();
    assert_eq!(format!("{a:?}"), format!("{b:?}"));
    assert!(
        past_the_root(&whole) == past_the_root(&within),
        "{name}: the files part"
    );
    a
}

/// A table's rows from each row's values spelled as PostgreSQL's input reads them.
fn spelled(catalog: &Catalog, table: &str, rows: &[Vec<Option<String>>]) -> Rows {
    let t = catalog.table(table).unwrap();
    Rows {
        columns: t
            .own_columns()
            .iter()
            .enumerate()
            .map(|(c, col)| {
                let mut v = Values::new(col.ty);
                for r in rows {
                    match &r[c] {
                        None => v.push_null(),
                        Some(s) => push_datum(&mut v, &scalar_in(col.ty, s).unwrap()),
                    }
                }
                v
            })
            .collect(),
    }
}

#[test]
fn the_sample_past_its_room_writes_the_file_it_writes_whole() {
    let s = sample(20_000);
    let catalog = Catalog::parse(DDL).unwrap();
    let written = alike("sample", &catalog, &tables(&s), 1 << 16);
    let month = written.iter().find(|w| w.order == "r_month").unwrap();
    assert!(month.blocks > 8, "{month:?}");
}

#[test]
fn a_composed_level_past_its_room_comes_back_as_it_went() {
    let ddl = "CREATE SCHEMA s; CREATE TABLE s.t (id integer NOT NULL, v integer[], \
               PRIMARY KEY (v, id));";
    let catalog = Catalog::parse(ddl).unwrap();
    let ty = catalog.table("s.t").unwrap().columns[1].ty;
    let n = 8192i64;
    let v = Values::Composed(
        (0..n)
            .map(|i| {
                let k = (i * 7919) % n;
                let last = match k % 13 {
                    0 => "NULL".to_string(),
                    x => x.to_string(),
                };
                Some(Arc::new(Compound::new(
                    ty,
                    text_in(ty, &format!("{{{},{last}}}", k << 18)).unwrap(),
                )))
            })
            .collect(),
    );
    let rows = Rows {
        columns: vec![Values::Int((0..n).map(Some).collect()), v],
    };
    alike("composed", &catalog, &[("s.t".into(), rows)], 1 << 14);
}

#[test]
fn a_partial_strand_and_a_strand_past_their_room_write_as_whole() {
    let ddl = "CREATE SCHEMA s; CREATE TABLE s.t (a integer NOT NULL, b integer, c text, \
               PRIMARY KEY (a)); CREATE INDEX t_b ON s.t (b) WHERE b > 300; \
               CREATE INDEX t_cb ON s.t (c, b);";
    let catalog = Catalog::parse(ddl).unwrap();
    let rows: Vec<Vec<Option<String>>> = (0..20_000i64)
        .map(|i| {
            let a = (i * 7919) % 20_000;
            vec![
                Some(a.to_string()),
                (a % 11 != 0).then(|| (a % 997).to_string()),
                (a % 5 != 0).then(|| format!("{}", "x".repeat((a % 23) as usize))),
            ]
        })
        .collect();
    let written = alike(
        "partial",
        &catalog,
        &[("s.t".into(), spelled(&catalog, "s.t", &rows))],
        1 << 15,
    );
    assert!(written.iter().all(|w| w.blocks > 1), "{written:?}");
}

#[test]
fn rows_numbered_as_they_come_and_an_element_strand_write_as_whole() {
    let ddl = "CREATE SCHEMA s; CREATE TABLE s.a (p point, j json); \
               CREATE TABLE s.g (id integer NOT NULL, xs integer[], PRIMARY KEY (id)); \
               CREATE INDEX g_xs ON s.g USING gin (xs);";
    let catalog = Catalog::parse(ddl).unwrap();
    let n = 10_000i64;
    let a: Vec<Vec<Option<String>>> = (0..n)
        .map(|i| {
            vec![
                Some(format!("({},{})", (i * 31) % 101, i % 7)),
                Some(format!("{{\"i\": {i}}}")),
            ]
        })
        .collect();
    let ty = catalog.table("s.g").unwrap().columns[1].ty;
    let xs = Values::Composed(
        (0..n)
            .map(|i| {
                Some(Arc::new(Compound::new(
                    ty,
                    text_in(ty, &format!("{{{},{}}}", i % 50, (i * 7) % 1000)).unwrap(),
                )))
            })
            .collect(),
    );
    let tables = [
        ("s.a".to_string(), spelled(&catalog, "s.a", &a)),
        (
            "s.g".to_string(),
            Rows {
                columns: vec![Values::Int(((0..n).rev()).map(Some).collect()), xs],
            },
        ),
    ];
    alike("numbered", &catalog, &tables, 1 << 15);
    // Read along the numbers, the rows come as they were given.
    let path = tmp("room-numbered-within.snail");
    write_within(&path, &catalog, &tables, &[], span(), 1 << 15).unwrap();
    let mut snail = warren_snail::read::Snail::open(&path).unwrap();
    let (batch, _) = snail.ask("SELECT j FROM s.a", None).unwrap();
    let got: Vec<String> = warren_snail::read::answer_text(&batch)
        .unwrap()
        .into_iter()
        .map(|r| r[0].clone().unwrap())
        .collect();
    let given: Vec<String> = a.iter().map(|r| r[1].clone().unwrap()).collect();
    assert!(got == given, "the rows came out of their order");
}

#[test]
fn a_key_two_rows_share_is_refused_past_the_room_as_within_it() {
    let ddl = "CREATE SCHEMA s; CREATE TABLE s.t (a integer NOT NULL, b text, PRIMARY KEY (a));";
    let catalog = Catalog::parse(ddl).unwrap();
    let rows: Vec<Vec<Option<String>>> = (0..12_000i64)
        .map(|i| {
            let a = if i == 9_000 {
                4_321
            } else {
                (i * 7919) % 12_000
            };
            vec![Some(a.to_string()), Some(format!("row {i}"))]
        })
        .collect();
    let tables = [("s.t".to_string(), spelled(&catalog, "s.t", &rows))];
    let refusal = |room: u64| {
        let path = tmp(&format!("room-shared-{room}.snail"));
        let e = write_within(&path, &catalog, &tables, &[], span(), room).unwrap_err();
        assert!(!path.exists(), "a write refused leaves no file");
        e.to_string()
    };
    let whole = refusal(WHOLE);
    assert_eq!(
        whole,
        "duplicate key value violates unique constraint \"t_pkey\": Key (a)=(4321) already exists."
    );
    assert_eq!(refusal(1 << 14), whole);
}

#[test]
fn the_first_row_a_check_refuses_is_refused_as_postgresql_s_copy_refuses_it() {
    let ddl = "CREATE SCHEMA s; CREATE TABLE s.t (a integer NOT NULL, b text NOT NULL, \
               CHECK (a <> 9100), PRIMARY KEY (a));";
    let catalog = Catalog::parse(ddl).unwrap();
    let rows: Vec<Vec<Option<String>>> = (0..10_000i64)
        .map(|i| {
            vec![
                Some(i.to_string()),
                (i != 8_500).then(|| format!("row {i}")),
            ]
        })
        .collect();
    let tables = [("s.t".to_string(), spelled(&catalog, "s.t", &rows))];
    for room in [WHOLE, 1 << 14] {
        let path = tmp(&format!("room-checked-{room}.snail"));
        let e = write_within(&path, &catalog, &tables, &[], span(), room).unwrap_err();
        assert_eq!(
            e.to_string(),
            "null value in column \"b\" of relation \"t\" violates not-null constraint: Failing \
             row contains (8500, null)."
        );
    }
}

#[cfg(feature = "standalone")]
#[test]
fn files_out_of_a_snail_come_back_past_the_room_by_their_pages_as_whole() {
    let s = sample(20_000);
    let catalog = Catalog::parse(DDL).unwrap();
    let path = snail("room-out", &s);
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("room-out");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let outs = warren_snail::read::Snail::open_writing(&path)
        .unwrap()
        .out(&dir)
        .unwrap();
    let given: Vec<(String, std::path::PathBuf)> = outs
        .iter()
        .map(|t| (t.table.clone(), t.file.clone()))
        .collect();
    let write = |name: &str, room: u64| {
        let to = tmp(name);
        let w = warren_snail::write::write_parquet_in(
            &to,
            &catalog,
            &given,
            span(),
            None,
            None,
            None,
            room,
        )
        .unwrap();
        (to, w)
    };
    let (whole, a) = write("room-out-whole.snail", WHOLE);
    let (within, b) = write("room-out-within.snail", 1 << 16);
    assert_eq!(format!("{a:?}"), format!("{b:?}"));
    assert!(past_the_root(&whole) == past_the_root(&within));
    let readings = a.iter().find(|w| w.order == "r_station").unwrap();
    assert!(
        readings
            .columns
            .iter()
            .all(|c| c.moved_blocks == readings.blocks),
        "{readings:?}"
    );
}

#[cfg(feature = "standalone")]
#[test]
fn a_reference_from_files_is_held_to_its_parent_as_from_rows_held() {
    use arrow::array::{Int32Array, RecordBatch};
    let ddl = "CREATE SCHEMA s; CREATE TABLE s.p (k integer NOT NULL, PRIMARY KEY (k)); \
               CREATE TABLE s.c (id integer NOT NULL, k integer REFERENCES s.p (k), \
               PRIMARY KEY (id));";
    let catalog = Catalog::parse(ddl).unwrap();
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("room-references");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = |name: &str, columns: Vec<(&str, Vec<Option<i32>>)>| {
        let batch = RecordBatch::try_from_iter(
            columns
                .into_iter()
                .map(|(n, v)| (n, Arc::new(Int32Array::from(v)) as arrow::array::ArrayRef)),
        )
        .unwrap();
        let path = dir.join(format!("{name}.parquet"));
        let mut w = parquet::arrow::ArrowWriter::try_new(
            std::fs::File::create(&path).unwrap(),
            batch.schema(),
            None,
        )
        .unwrap();
        w.write(&batch).unwrap();
        w.close().unwrap();
        path
    };
    let parents: Vec<Option<i32>> = (0..3000).map(|k| Some(k * 2)).collect();
    let children = |missing: bool| -> Vec<Option<i32>> {
        (0..5000)
            .map(|i| match i {
                4_000 if missing => Some(7),
                i if i % 9 == 0 => None,
                i => Some((i % 3000) * 2),
            })
            .collect()
    };
    for missing in [false, true] {
        let p = file("p", vec![("k", parents.clone())]);
        let c = file(
            "c",
            vec![
                ("id", (0..5000).map(Some).collect()),
                ("k", children(missing)),
            ],
        );
        let to = tmp("room-references.snail");
        let from_files = warren_snail::write::write_parquet_in(
            &to,
            &catalog,
            &[("s.p".into(), p), ("s.c".into(), c)],
            span(),
            None,
            None,
            None,
            1 << 14,
        );
        let held =
            |v: Vec<Option<i32>>| Values::Int(v.into_iter().map(|x| x.map(i64::from)).collect());
        let rows = [
            (
                "s.p".to_string(),
                Rows {
                    columns: vec![held(parents.clone())],
                },
            ),
            (
                "s.c".to_string(),
                Rows {
                    columns: vec![
                        Values::Int((0..5000).map(Some).collect()),
                        held(children(missing)),
                    ],
                },
            ),
        ];
        let from_rows = write_within(
            &tmp("room-references-held.snail"),
            &catalog,
            &rows,
            &[],
            span(),
            1 << 14,
        );
        match (from_files, from_rows) {
            (Ok(_), Ok(_)) => assert!(!missing),
            (Err(a), Err(b)) => {
                assert!(missing);
                assert_eq!(a.to_string(), b.to_string());
                assert!(a.to_string().contains("Key (k)=(7) is not present"), "{a}");
            }
            (a, b) => panic!("{:?} against {:?}", a.err(), b.err()),
        }
    }
}
