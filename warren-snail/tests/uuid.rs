//! The uuid: held as its 16 bytes and keyed in the 128 bits they make, cut by those bits, compared,
//! grouped, ordered and joined as PostgreSQL does, read and written as PostgreSQL reads and writes
//! it; and the uuids gen_random_uuid() and uuidv7() hand out.

mod weather;

use arrow::array::{Array, FixedSizeBinaryArray};
use warren_snail::catalog::Catalog;
use warren_snail::read::{BlockRow, Snail};
use warren_snail::scalars::{uuid_bytes, uuid_key, uuid_text};
use warren_snail::values::Values;
use warren_snail::write::Rows;
use weather::{exact, ints64, tmp, write};

/// The uuid whose first 12 bits are `i` and whose last byte is `i`'s low byte.
fn id(i: i64) -> i128 {
    let mut b = [0u8; 16];
    b[0] = (i >> 4) as u8;
    b[1] = ((i & 15) << 4) as u8;
    b[15] = i as u8;
    uuid_key(b)
}

fn blocks_of(rows: &[BlockRow], order: &str, column: &str) -> Vec<u64> {
    rows.iter()
        .filter(|r| r.order == order && r.column == column)
        .map(|r| r.rows)
        .collect()
}

#[test]
fn a_uuid_is_cut_by_its_bits_on_the_sixteen_bytes_the_file_writes() {
    // 4,096 uuids spread evenly on their first 12 bits: a page holds 256 of 16 bytes, so the cut
    // stops at sixteen parts, where a bigint's 512 a page stops at eight.
    let path = tmp("uuid-cut.snail");
    let catalog = Catalog::parse("CREATE TABLE s.k (id uuid NOT NULL, PRIMARY KEY (id));").unwrap();
    let rows = Rows {
        columns: vec![Values::Wide((0..4096).map(|i| Some(id(i))).collect())],
    };
    write(&path, &catalog, &[("s.k".into(), rows)]).unwrap();
    let blocks = Snail::open(&path).unwrap().blocks().unwrap();
    assert_eq!(blocks_of(&blocks, "k_pkey", "id"), [256; 16]);
}

const DDL: &str =
    "CREATE TABLE s.u (id uuid NOT NULL, n integer NOT NULL, b uuid, PRIMARY KEY (id)); \
     CREATE INDEX u_b ON s.u (b, id); \
     CREATE INDEX u_d ON s.u (id DESC) INCLUDE (n); \
     CREATE TABLE s.v (k integer NOT NULL, u uuid NOT NULL, PRIMARY KEY (k));";

/// 4,096 rows of s.u, `b` NULL on every fourth and else one of six uuids; and s.v, two rows for each
/// of the first 100 of s.u.
fn written(name: &str) -> std::path::PathBuf {
    let path = tmp(&format!("{name}.snail"));
    let catalog = Catalog::parse(DDL).unwrap();
    let u = Rows {
        columns: vec![
            Values::Wide((0..4096).map(|i| Some(id(i))).collect()),
            Values::Int((0..4096).map(Some).collect()),
            Values::Wide((0..4096).map(|i| (i % 4 != 0).then(|| id(i % 8))).collect()),
        ],
    };
    let v = Rows {
        columns: vec![
            Values::Int((0..200).map(Some).collect()),
            Values::Wide((0..200).map(|k| Some(id(k / 2))).collect()),
        ],
    };
    write(&path, &catalog, &[("s.u".into(), u), ("s.v".into(), v)]).unwrap();
    path
}

fn uuids(a: &dyn Array) -> Vec<Option<i128>> {
    let a = a.as_any().downcast_ref::<FixedSizeBinaryArray>().unwrap();
    (0..a.len())
        .map(|i| (!a.is_null(i)).then(|| uuid_key(a.value(i).try_into().unwrap())))
        .collect()
}

fn ns(b: &arrow::array::RecordBatch) -> Vec<i64> {
    ints64(b.column(0).as_ref()).into_iter().flatten().collect()
}

#[test]
fn a_uuid_is_compared_grouped_ordered_and_joined_as_postgresql_does() {
    let path = written("uuid-ask");
    let mut snail = Snail::open(&path).unwrap();
    let mut ask = |q: &str, order: Option<&str>| {
        let (b, round) = snail.ask(q, order).unwrap();
        exact(q, &round);
        (b, round)
    };
    let lit = |i: i64| format!("'{}'", uuid_text(id(i)));
    // An equality is one place of the key, read in one block.
    let (b, round) = ask(
        &format!("SELECT u.n FROM s.u u WHERE u.id = {}", lit(100)),
        Some("u_pkey"),
    );
    assert_eq!((ns(&b), round.blocks_read), (vec![100], 1));
    // Every spelling PostgreSQL reads is the same uuid.
    let braced = format!(
        "'{{{}}}'",
        uuid_text(id(100)).to_uppercase().replace('-', "")
    );
    let (b, _) = ask(
        &format!("SELECT u.n FROM s.u u WHERE u.id = {braced}"),
        None,
    );
    assert_eq!(ns(&b), [100]);
    let (b, _) = ask(
        &format!("SELECT u.n FROM s.u u WHERE u.id < {} ORDER BY u.n", lit(5)),
        None,
    );
    assert_eq!(ns(&b), [0, 1, 2, 3, 4]);
    let (b, _) = ask(
        &format!(
            "SELECT u.n FROM s.u u WHERE u.id > {} AND {} >= u.id ORDER BY u.n",
            lit(4090),
            lit(4093)
        ),
        None,
    );
    assert_eq!(ns(&b), [4091, 4092, 4093]);
    // `<>` is counted from the counts in every block but the one that holds its value.
    for order in ["u_pkey", "u_d"] {
        let (b, round) = ask(
            &format!("SELECT count(*) FROM s.u u WHERE u.id <> {}", lit(7)),
            Some(order),
        );
        assert_eq!(ns(&b), [4095], "{order}");
        assert_eq!(round.blocks_read, 1, "{order}");
    }
    let (b, _) = ask(
        &format!("SELECT count(*) FROM s.u u WHERE u.b <> {}", lit(1)),
        None,
    );
    assert_eq!(ns(&b), [4096 - 1024 - 512]);
    // NULLs on their own cycle along u_b, counted.
    let (b, round) = ask("SELECT count(*) FROM s.u u WHERE u.b IS NULL", Some("u_b"));
    assert_eq!((ns(&b), round.column_pages_read), (vec![1024], 0));
    // Grouped, a uuid's groups come in its order, NULL last.
    let (b, _) = ask(
        "SELECT u.b, count(*) FROM s.u u GROUP BY u.b ORDER BY u.b",
        Some("u_b"),
    );
    let mut want: Vec<Option<i128>> = [1, 2, 3, 5, 6, 7].iter().map(|&i| Some(id(i))).collect();
    want.push(None);
    assert_eq!(uuids(b.column(0).as_ref()), want);
    assert_eq!(
        ints64(b.column(1).as_ref()),
        [512, 512, 512, 512, 512, 512, 1024].map(Some)
    );
    // Ordered descending along the index that holds it so, and along the key.
    for order in ["u_pkey", "u_d"] {
        let (b, _) = ask(
            &format!(
                "SELECT u.n, u.id FROM s.u u WHERE u.id >= {} ORDER BY u.id DESC",
                lit(4093)
            ),
            Some(order),
        );
        assert_eq!(ns(&b), [4095, 4094, 4093], "{order}");
    }
    // A join on a uuid: each of the first 100 rows of s.u meets two of s.v.
    let (b, _) = ask(
        "SELECT u.n, count(*) FROM s.u u JOIN s.v v ON v.u = u.id GROUP BY u.n ORDER BY u.n",
        None,
    );
    assert_eq!(ns(&b), (0..100).collect::<Vec<_>>());
    assert!(ints64(b.column(1).as_ref()).iter().all(|&c| c == Some(2)));
    // A uuid answered as its 16 bytes.
    let (b, _) = ask("SELECT u.id FROM s.u u WHERE u.n = 100", Some("u_pkey"));
    assert_eq!(uuids(b.column(0).as_ref()), [Some(id(100))]);
    // A strict bound past either end of the uuids keeps nothing; one at an end keeps that end.
    let (lo, hi) = (
        "00000000-0000-0000-0000-000000000000",
        "ffffffff-ffff-ffff-ffff-ffffffffffff",
    );
    for (w, want) in [
        (format!("u.id < '{lo}'"), 0),
        (format!("u.id <= '{lo}'"), 1),
        (format!("u.b < '{lo}'"), 0),
        (format!("u.b <= '{lo}'"), 0),
        (format!("u.b > '{hi}'"), 0),
        (format!("u.b >= '{hi}'"), 0),
        (format!("u.b > '{lo}'"), 3072),
        (format!("u.b < '{hi}'"), 3072),
    ] {
        let (b, _) = ask(&format!("SELECT count(*) FROM s.u u WHERE {w}"), None);
        assert_eq!(ns(&b), [want], "{w}");
    }
    for refused in [
        "SELECT u.n FROM s.u u WHERE u.id = 'x'",
        "SELECT u.n FROM s.u u WHERE u.id = '0011223344556677889aabbccddeeff'",
        "SELECT u.n FROM s.u u WHERE u.id IS TRUE",
        "SELECT u.n FROM s.u u WHERE u.b IS NOT UNKNOWN",
        "SELECT u.n FROM s.u u WHERE u.id = 5",
        "SELECT sum(u.id) FROM s.u u",
    ] {
        assert!(snail.ask(refused, None).is_err(), "{refused}");
    }
}

#[test]
fn a_pipeline_reads_a_uuid_as_postgresql_does() {
    let path = written("uuid-pipeline");
    let mut snail = Snail::open(&path).unwrap();
    let step = "WITH x AS (SELECT u.id, u.n, u.b FROM s.u u)";
    let text = uuid_text(id(100));
    let (b, _) = snail
        .ask(
            &format!(
                "{step} SELECT x.n FROM x WHERE x.id = '{}' OR '{}' = x.id ORDER BY x.n",
                text.to_uppercase(),
                uuid_text(id(3))
            ),
            None,
        )
        .unwrap();
    assert_eq!(ns(&b), [3, 100]);
    let (b, _) = snail
        .ask(
            &format!(
                "{step} SELECT x.n FROM x WHERE x.id <> '{text}' AND x.n > 98 AND x.n < 102 \
                 ORDER BY x.n"
            ),
            None,
        )
        .unwrap();
    assert_eq!(ns(&b), [99, 101]);
    let (b, _) = snail
        .ask(
            &format!(
                "{step} SELECT x.id::text AS t, CAST('{}' AS uuid) AS c, x.id::bytea AS y FROM x \
                 WHERE x.n = 100",
                text.replace('-', "")
            ),
            None,
        )
        .unwrap();
    assert_eq!(weather::texts(b.column(0).as_ref()), [Some(text.clone())]);
    assert_eq!(uuids(b.column(1).as_ref()), [Some(id(100))]);
    let y = b
        .column(2)
        .as_any()
        .downcast_ref::<arrow::array::BinaryArray>()
        .unwrap()
        .value(0)
        .to_vec();
    assert_eq!(y, uuid_bytes(id(100)));
    let (b, _) = snail
        .ask(
            &format!("{step} SELECT x.b, count(*) AS c FROM x GROUP BY x.b ORDER BY x.b"),
            None,
        )
        .unwrap();
    assert_eq!(b.num_rows(), 7);
    for refused in [
        format!("{step} SELECT x.n FROM x WHERE x.id = 'not a uuid'"),
        format!("{step} SELECT min(x.id) FROM x"),
        format!("{step} SELECT CAST('\\x01'::bytea AS uuid) FROM x"),
    ] {
        assert!(snail.ask(&refused, None).is_err(), "{refused}");
    }
}

#[test]
#[cfg(feature = "standalone")]
fn a_uuid_goes_out_as_parquet_s_uuid_and_comes_back() {
    let path = written("uuid-out");
    let dir = tmp("uuid-out-parquet");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    let outs = w.out(&dir).unwrap();
    let u = outs.iter().find(|t| t.table == "s.u").unwrap();
    let (batches, _) = weather::parquet_of(&u.file);
    let ids: Vec<Option<i128>> = batches
        .iter()
        .flat_map(|b| uuids(b.column_by_name("id").unwrap().as_ref()))
        .collect();
    assert_eq!(ids, (0..4096).map(|i| Some(id(i))).collect::<Vec<_>>());
    let meta = parquet::file::reader::FileReader::metadata(
        &parquet::file::reader::SerializedFileReader::new(std::fs::File::open(&u.file).unwrap())
            .unwrap(),
    )
    .clone();
    let id_column = meta.file_metadata().schema_descr().column(0);
    assert_eq!(
        id_column.logical_type_ref(),
        Some(&parquet::basic::LogicalType::Uuid)
    );
    // Each row group's least and greatest are its uuids' bytes, as Parquet orders them.
    let first = meta.row_group(0).column(0).statistics().unwrap();
    assert_eq!(
        (
            first.min_bytes_opt().unwrap(),
            first.max_bytes_opt().unwrap()
        ),
        (&uuid_bytes(id(0))[..], &uuid_bytes(id(1023))[..])
    );
    let catalog = Catalog::parse(DDL).unwrap();
    let table = catalog.table("s.u").unwrap();
    let rows = Rows::from_batches(table, &batches).unwrap();
    let back = tmp("uuid-back.snail");
    write(&back, &catalog, &[("s.u".into(), rows)]).unwrap();
    let mut again = Snail::open(&back).unwrap();
    let (b, _) = again
        .ask(
            &format!(
                "SELECT u.n FROM s.u u WHERE u.id = '{}'",
                uuid_text(id(4000))
            ),
            None,
        )
        .unwrap();
    assert_eq!(ns(&b), [4000]);
}

#[test]
#[cfg(feature = "standalone")]
fn a_uuid_is_written_from_its_spellings_and_its_defaults_hand_out_a_v4_and_rising_v7s() {
    let path = tmp("uuid-writes.snail");
    let catalog = Catalog::parse(
        "CREATE TABLE s.r (id uuid DEFAULT gen_random_uuid() NOT NULL, n integer NOT NULL, \
         PRIMARY KEY (id)); \
         CREATE TABLE s.s (id uuid DEFAULT uuidv7() NOT NULL, n integer NOT NULL, PRIMARY KEY (id)); \
         CREATE TABLE s.t (id uuid NOT NULL, label text, PRIMARY KEY (id));",
    )
    .unwrap();
    write(&path, &catalog, &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    for n in 0..300 {
        w.execute(&format!("INSERT INTO s.r (n) VALUES ({n})"))
            .unwrap();
        w.execute(&format!("INSERT INTO s.s (id, n) VALUES (DEFAULT, {n})"))
            .unwrap();
    }
    let version = |k: i128| uuid_bytes(k)[6] >> 4;
    let (b, _) = w.ask("SELECT r.id FROM s.r r", None).unwrap();
    let v4 = uuids(b.column(0).as_ref());
    assert_eq!(v4.len(), 300);
    assert!(v4.iter().all(|k| version(k.unwrap()) == 4));
    // A writer's v7s rise: the order of their keys is the order they were handed out.
    let (b, _) = w
        .ask("SELECT s.n, s.id FROM s.s s ORDER BY s.id", None)
        .unwrap();
    assert_eq!(ns(&b), (0..300).collect::<Vec<_>>());
    let (b, _) = w.ask("SELECT s.id FROM s.s s", None).unwrap();
    assert!(uuids(b.column(0).as_ref())
        .iter()
        .all(|k| version(k.unwrap()) == 7));
    // Every spelling uuid_in reads, and a uuid written into text as uuid_out writes it.
    w.execute(
        "INSERT INTO s.t (id, label) VALUES ('{00112233-4455-6677-8899-AABBCCDDEEFF}', 'a'), \
         ('0011-2233-4455-6677-8899-aabb-ccdd-eef0', 'b')",
    )
    .unwrap();
    assert!(w
        .execute("INSERT INTO s.t (id) VALUES ('00112233-4455-6677-8899-aabbccddeeff')")
        .unwrap_err()
        .to_string()
        .contains("duplicate key value violates unique constraint"));
    assert!(w
        .execute("INSERT INTO s.t (id) VALUES ('{00112233-4455-6677-8899-aabbccddeeff')")
        .unwrap_err()
        .to_string()
        .contains("invalid input syntax for type uuid"));
    w.execute("INSERT INTO s.t (id, label) SELECT r.id, r.id FROM s.r r WHERE r.n = 7")
        .unwrap();
    let (b, _) = w
        .ask("SELECT t.label, t.id FROM s.t t ORDER BY t.id", None)
        .unwrap();
    let labels = weather::texts(b.column(0).as_ref());
    assert_eq!(labels.len(), 3);
    assert!(labels.contains(&Some("a".into())) && labels.contains(&Some("b".into())));
    let (b, _) = w.ask("SELECT r.id FROM s.r r WHERE r.n = 7", None).unwrap();
    let seven = uuids(b.column(0).as_ref())[0].unwrap();
    assert!(labels.contains(&Some(uuid_text(seven))));
    w.execute(&format!(
        "UPDATE s.t SET label = 'c' WHERE id = '{}'",
        uuid_text(seven)
    ))
    .unwrap();
    w.execute("DELETE FROM s.t WHERE id = '00112233445566778899aabbccddeeff'")
        .unwrap();
    let (b, _) = w
        .ask("SELECT t.label FROM s.t t ORDER BY t.label", None)
        .unwrap();
    assert_eq!(
        weather::texts(b.column(0).as_ref()),
        [Some("b".into()), Some("c".into())]
    );
    // A uuid is assigned to text, and text never to a uuid.
    assert!(w
        .execute("INSERT INTO s.r (id, n) SELECT t.label, 1 FROM s.t t")
        .unwrap_err()
        .to_string()
        .contains("is of type uuid but expression is of type text"));
}

#[test]
#[cfg(feature = "standalone")]
fn a_snail_of_format_eight_refuses_a_uuid_by_name() {
    let catalog = Catalog::parse("CREATE TABLE s.k (id uuid NOT NULL, PRIMARY KEY (id));").unwrap();
    let path = tmp("uuid-eight.snail");
    let e = warren_snail::write::write_format_eight(&path, &catalog, &[], weather::span())
        .err()
        .unwrap()
        .to_string();
    assert!(
        e.contains("s.k.id is uuid, which a snail of format 8 cannot hold") && e.contains("`out`"),
        "{e}"
    );
}
