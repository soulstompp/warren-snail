//! The strings and the bit strings: bpchar, name, bytea and bit strings, each held, keyed and cut as
//! PostgreSQL compares it, read and written as PostgreSQL reads and writes it; text compared by its
//! bytes under each collation of byte order.

mod weather;

use arrow::array::{Array, BinaryArray, RecordBatch, StringArray};
use parquet::data_type::ByteArray;
use warren_snail::catalog::Catalog;
use warren_snail::read::{BlockRow, Snail};
use warren_snail::strings::{bits_key_at, bits_parse, bits_text, field_string};
use warren_snail::values::Values;
use warren_snail::write::Rows;
use weather::{exact, ints64, one, parquet_of, tmp, write};

/// A snail of one table, `s.t`, its rows given column by column.
fn snail_of(name: &str, ddl: &str, columns: Vec<Values>) -> std::path::PathBuf {
    let path = tmp(&format!("{name}.snail"));
    let catalog = Catalog::parse(ddl).unwrap();
    write(&path, &catalog, &[("s.t".into(), Rows { columns })]).unwrap();
    path
}

fn ids(b: &RecordBatch) -> Vec<i64> {
    ints64(b.column(0).as_ref()).into_iter().flatten().collect()
}

fn texts(b: &RecordBatch, c: usize) -> Vec<Option<String>> {
    let a = b.column(c).as_any().downcast_ref::<StringArray>().unwrap();
    a.iter().map(|v| v.map(str::to_string)).collect()
}

fn bytes(b: &RecordBatch, c: usize) -> Vec<Option<Vec<u8>>> {
    let a = b.column(c).as_any().downcast_ref::<BinaryArray>().unwrap();
    a.iter().map(|v| v.map(<[u8]>::to_vec)).collect()
}

/// A bit string column of an answer, each value as bit_out writes it.
fn bit_texts(b: &RecordBatch, c: usize) -> Vec<Option<String>> {
    let field = b.schema().field(c).clone();
    let ty = field_string(&field).expect("a bit string's field names its type");
    let a = b.column(c);
    (0..a.len())
        .map(|r| bits_key_at(ty, a.as_ref(), r).map(|k| bits_text(&k)))
        .collect()
}

fn blocks_of<'a>(rows: &'a [BlockRow], order: &str, column: &str) -> Vec<&'a BlockRow> {
    rows.iter()
        .filter(|r| r.order == order && r.column == column)
        .collect()
}

/// The bytea of `snail-tests/types/strings.sql`, by id from 1, NULL second.
fn byteas() -> Vec<Option<Vec<u8>>> {
    vec![
        Some(vec![0, 0xff, 0x41]),
        None,
        Some(vec![]),
        Some(vec![0]),
        Some(vec![0, 0]),
        Some(vec![1]),
        Some(vec![0xff]),
        Some(vec![0xff, 0xff]),
        Some(vec![0x0a, 0x0b]),
        Some(b"abc\\".to_vec()),
        Some(vec![0x41, 0]),
        Some(vec![b'a', 0, b'b']),
        Some(vec![]),
    ]
}

fn bytea_snail(name: &str) -> std::path::PathBuf {
    snail_of(
        name,
        "CREATE TABLE s.t (id integer NOT NULL, v bytea, PRIMARY KEY (id)); \
         CREATE INDEX t_v ON s.t (v, id);",
        vec![
            Values::Int((1..=13).map(Some).collect()),
            Values::Utf8(
                byteas()
                    .into_iter()
                    .map(|b| b.map(ByteArray::from))
                    .collect(),
            ),
        ],
    )
}

#[test]
fn bytea_is_a_level_ordered_by_its_bytes_an_ended_value_first() {
    let path = bytea_snail("bytea-level");
    let mut snail = Snail::open(&path).unwrap();
    let (b, round) = snail
        .ask(
            "SELECT t.id, t.v FROM s.t t ORDER BY t.v, t.id",
            Some("t_v"),
        )
        .unwrap();
    exact("bytea's order", &round);
    let mut want: Vec<(Option<Vec<u8>>, i64)> = byteas().into_iter().zip(1..).collect();
    want.sort_by(|a, b| match (&a.0, &b.0) {
        (None, None) => a.1.cmp(&b.1),
        (None, _) => std::cmp::Ordering::Greater,
        (_, None) => std::cmp::Ordering::Less,
        (x, y) => x.cmp(y).then(a.1.cmp(&b.1)),
    });
    assert_eq!(ids(&b), want.iter().map(|w| w.1).collect::<Vec<_>>());
    assert_eq!(
        bytes(&b, 1),
        want.into_iter().map(|w| w.0).collect::<Vec<_>>()
    );
    // A condition on bytea reads its literal as byteain does, in either form.
    for (q, want) in [
        ("SELECT t.id FROM s.t t WHERE t.v = '\\x0000'", vec![5]),
        ("SELECT t.id FROM s.t t WHERE t.v = '\\000\\000'", vec![5]),
        ("SELECT t.id FROM s.t t WHERE t.v = 'abc\\\\'", vec![10]),
        (
            "SELECT t.id FROM s.t t WHERE t.v < '\\x00' ORDER BY t.id",
            vec![3, 13],
        ),
        (
            "SELECT t.id FROM s.t t WHERE t.v > '\\xff' ORDER BY t.id",
            vec![8],
        ),
        (
            "SELECT t.id FROM s.t t WHERE t.v >= '\\x41' AND t.v < '\\x62' ORDER BY t.id",
            vec![10, 11, 12],
        ),
        (
            "SELECT t.id FROM s.t t WHERE t.v <> '\\x' ORDER BY t.id",
            vec![1, 4, 5, 6, 7, 8, 9, 10, 11, 12],
        ),
    ] {
        for order in ["t_pkey", "t_v"] {
            let (b, round) = snail.ask(q, Some(order)).unwrap();
            exact(q, &round);
            assert_eq!(ids(&b), want, "{q} along {order}");
        }
    }
    let e = snail
        .ask("SELECT t.id FROM s.t t WHERE t.v = '\\x0'", None)
        .unwrap_err()
        .to_string();
    assert!(e.contains("odd number of digits"), "{e}");
}

#[test]
fn a_bytea_level_is_cut_by_its_bytes_and_fixed_by_an_equality() {
    // 4,096 values of 64 bytes, spread on the first over 64 places: 68 bytes each written plain, so
    // a place's 64 rows fill a page, and a place's next byte splits it into rows that fill none.
    let n = 4096;
    let v: Vec<Option<ByteArray>> = (0..n)
        .map(|i: i64| {
            let mut b = vec![0u8; 64];
            b[0] = (i >> 6) as u8;
            b[1] = (i & 63) as u8;
            Some(ByteArray::from(b))
        })
        .collect();
    let path = snail_of(
        "bytea-cut",
        "CREATE TABLE s.t (v bytea NOT NULL, PRIMARY KEY (v));",
        vec![Values::Utf8(v)],
    );
    let mut snail = Snail::open(&path).unwrap();
    let blocks = snail.blocks().unwrap();
    let rows: Vec<u64> = blocks_of(&blocks, "t_pkey", "v")
        .iter()
        .map(|r| r.rows)
        .collect();
    assert_eq!(rows, [64; 64]);
    let q = format!(
        "SELECT count(*) FROM s.t t WHERE t.v = '\\x2007{}'",
        "00".repeat(62)
    );
    let (b, round) = snail.ask(&q, None).unwrap();
    exact("bytea's equality", &round);
    assert_eq!((one(&b), round.blocks_read), (1, 1));
}

#[test]
fn a_bytea_level_goes_out_and_comes_back_by_its_pages() {
    let path = bytea_snail("bytea-out");
    let dir = tmp("bytea-out-parquet");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let outs = Snail::open(&path).unwrap().out(&dir).unwrap();
    let (batches, _) = parquet_of(&outs[0].file);
    let catalog = Catalog::parse(
        "CREATE TABLE s.t (id integer NOT NULL, v bytea, PRIMARY KEY (id)); \
         CREATE INDEX t_v ON s.t (v, id);",
    )
    .unwrap();
    let table = catalog.table("s.t").unwrap();
    let rows = Rows::from_batches(table, &batches).unwrap();
    let back = tmp("bytea-out-back.snail");
    write(&back, &catalog, &[("s.t".into(), rows)]).unwrap();
    let (b, _) = Snail::open(&back)
        .unwrap()
        .ask("SELECT t.id, t.v FROM s.t t ORDER BY t.id", None)
        .unwrap();
    assert_eq!(bytes(&b, 1), byteas());
}

#[test]
fn a_bytea_level_is_refused_by_name_in_formats_seven_and_eight() {
    let catalog = Catalog::parse("CREATE TABLE s.t (v bytea NOT NULL, PRIMARY KEY (v));").unwrap();
    let rows = || Rows {
        columns: vec![Values::Utf8(vec![Some(ByteArray::from(vec![1u8]))])],
    };
    for (name, eight) in [("bytea-seven.snail", false), ("bytea-eight.snail", true)] {
        let path = tmp(name);
        let tables = [("s.t".to_string(), rows())];
        let made = if eight {
            warren_snail::write::write_format_eight(&path, &catalog, &tables, weather::span())
        } else {
            warren_snail::write::write_format_seven(&path, &catalog, &tables, weather::span())
        };
        let e = made.unwrap_err().to_string();
        assert!(e.contains("bytea level"), "{e}");
    }
}

/// bpchar, unlimited and of three characters, each holding values equal but for their trailing
/// blanks.
const BPCHAR_DDL: &str =
    "CREATE TABLE s.t (id integer NOT NULL, v bpchar, c char(3), PRIMARY KEY (id)); \
     CREATE INDEX t_v ON s.t (v, id); \
     CREATE INDEX t_c ON s.t (c, id);";

/// The rows of `BPCHAR_DDL`, as the statement `BPCHAR_INSERT` writes them.
fn bpchar_rows() -> Vec<Values> {
    let t = |v: [Option<&str>; 8]| Values::Text(v.iter().map(|s| s.map(str::to_string)).collect());
    vec![
        Values::Int((1..=8).map(Some).collect()),
        t([
            Some("a"),
            Some("a  "),
            Some("a\t"),
            Some(""),
            Some(" "),
            Some("a!"),
            None,
            Some("b "),
        ]),
        t([
            Some("a  "),
            Some("a  "),
            Some("a\t "),
            Some("   "),
            Some("   "),
            Some("a! "),
            None,
            Some("abc"),
        ]),
    ]
}

const BPCHAR_INSERT: &str = "INSERT INTO s.t (id, v, c) VALUES (1, 'a', 'a'), (2, 'a  ', 'a  '), \
     (3, 'a\t', 'a\t'), (4, '', ''), (5, ' ', ' '), (6, 'a!', 'a!'), (7, NULL, NULL), \
     (8, 'b ', 'abc  ')";

fn bpchar_snail(name: &str) -> std::path::PathBuf {
    snail_of(name, BPCHAR_DDL, bpchar_rows())
}

#[test]
#[cfg(feature = "standalone")]
fn a_bpchar_is_read_padded_to_its_length_its_excess_blanks_taken_off() {
    let path = tmp("bpchar-insert.snail");
    write(&path, &Catalog::parse(BPCHAR_DDL).unwrap(), &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute(BPCHAR_INSERT).unwrap();
    let (b, _) = Snail::open(&path)
        .unwrap()
        .ask("SELECT t.id, t.v, t.c FROM s.t t ORDER BY t.id", None)
        .unwrap();
    let want = bpchar_rows();
    for c in [1, 2] {
        let held: Vec<Option<String>> = (0..8)
            .map(|i| want[c].text(i).map(str::to_string))
            .collect();
        assert_eq!(texts(&b, c), held, "column {c}");
    }
    // A value too long for char(3) is refused, unless its excess is blanks.
    let e = w
        .execute("INSERT INTO s.t (id, v, c) VALUES (9, 'x', 'abcd')")
        .unwrap_err()
        .to_string();
    assert!(e.contains("value too long for type character(3)"), "{e}");
}

#[test]
fn a_bpchar_is_compared_without_its_trailing_blanks_and_keeps_its_own() {
    let path = bpchar_snail("bpchar-compare");
    let mut snail = Snail::open(&path).unwrap();
    // Every value back as written: an unlimited bpchar's blanks kept, a char(3) padded to three
    // and its excess blanks taken off.
    let (b, _) = snail
        .ask("SELECT t.id, t.v, t.c FROM s.t t ORDER BY t.id", None)
        .unwrap();
    let some = |s: &str| Some(s.to_string());
    assert_eq!(
        texts(&b, 1),
        [
            some("a"),
            some("a  "),
            some("a\t"),
            some(""),
            some(" "),
            some("a!"),
            None,
            some("b ")
        ]
    );
    assert_eq!(
        texts(&b, 2),
        [
            some("a  "),
            some("a  "),
            some("a\t "),
            some("   "),
            some("   "),
            some("a! "),
            None,
            some("abc")
        ]
    );
    // A trailing blank sorts below a tab: the blanks are not compared.
    for (by, order) in [("t.v", "t_v"), ("t.c", "t_c"), ("t.v", "t_pkey")] {
        let q = format!("SELECT t.id, {by} FROM s.t t ORDER BY {by}, t.id");
        let (b, round) = snail.ask(&q, Some(order)).unwrap();
        exact(&q, &round);
        assert_eq!(ids(&b), [4, 5, 1, 2, 3, 6, 8, 7], "{q} along {order}");
    }
    for (q, want) in [
        (
            "SELECT t.id FROM s.t t WHERE t.v = 'a' ORDER BY t.id",
            vec![1, 2],
        ),
        (
            "SELECT t.id FROM s.t t WHERE t.c = 'a    ' ORDER BY t.id",
            vec![1, 2],
        ),
        (
            "SELECT t.id FROM s.t t WHERE t.v = '' ORDER BY t.id",
            vec![4, 5],
        ),
        (
            "SELECT t.id FROM s.t t WHERE t.v < 'a\t' ORDER BY t.id",
            vec![1, 2, 4, 5],
        ),
        (
            "SELECT t.id FROM s.t t WHERE t.v > 'a ' ORDER BY t.id",
            vec![3, 6, 8],
        ),
    ] {
        let on = if q.contains("t.c") { "t_c" } else { "t_v" };
        for order in ["t_pkey", on] {
            let (b, round) = snail.ask(q, Some(order)).unwrap();
            exact(q, &round);
            assert_eq!(ids(&b), want, "{q} along {order}");
        }
    }
    // Equal values are one group, whichever of them it prints.
    let (b, _) = snail
        .ask(
            "SELECT t.v, count(*) FROM s.t t GROUP BY t.v ORDER BY t.v",
            None,
        )
        .unwrap();
    let groups: Vec<(Option<String>, i64)> = texts(&b, 0)
        .into_iter()
        .map(|v| v.map(|s| s.trim_end_matches(' ').to_string()))
        .zip(ints64(b.column(1).as_ref()).into_iter().flatten())
        .collect();
    assert_eq!(
        groups,
        [
            (some(""), 2),
            (some("a"), 2),
            (some("a\t"), 1),
            (some("a!"), 1),
            (some("b"), 1),
            (None, 1)
        ]
    );
    let (b, _) = snail
        .ask("SELECT min(t.v) AS lo, max(t.c) AS hi FROM s.t t", None)
        .unwrap();
    assert_eq!(texts(&b, 0)[0].as_deref().map(|s| s.trim_end()), Some(""));
    assert_eq!(texts(&b, 1)[0].as_deref(), Some("abc"));
}

#[test]
fn a_bpchar_becomes_text_without_its_trailing_blanks_in_a_pipeline() {
    let path = bpchar_snail("bpchar-pipeline");
    let mut snail = Snail::open(&path).unwrap();
    let (b, _) = snail
        .ask(
            "WITH t AS (SELECT id, v, c FROM s.t) SELECT id, CAST(c AS text) AS x, \
             CAST(v AS char(1)) AS y, c::name AS z FROM t ORDER BY id",
            None,
        )
        .unwrap();
    let some = |s: &str| Some(s.to_string());
    assert_eq!(
        texts(&b, 1),
        [
            some("a"),
            some("a"),
            some("a\t"),
            some(""),
            some(""),
            some("a!"),
            None,
            some("abc")
        ]
    );
    assert_eq!(
        texts(&b, 2),
        [
            some("a"),
            some("a"),
            some("a"),
            some(" "),
            some(" "),
            some("a"),
            None,
            some("b")
        ]
    );
    assert_eq!(texts(&b, 3)[0].as_deref(), Some("a"));
    let (b, _) = snail
        .ask(
            "WITH t AS (SELECT id, v FROM s.t) SELECT id FROM t WHERE v = 'a     ' ORDER BY id",
            None,
        )
        .unwrap();
    assert_eq!(ids(&b), [1, 2]);
}

#[test]
#[cfg(feature = "standalone")]
fn a_name_is_cut_to_63_bytes_and_compared_by_its_bytes() {
    let path = tmp("name.snail");
    let catalog = Catalog::parse(
        "CREATE TABLE s.t (id integer NOT NULL, v name, PRIMARY KEY (id)); \
         CREATE INDEX t_v ON s.t (v, id);",
    )
    .unwrap();
    write(&path, &catalog, &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute(&format!(
        "INSERT INTO s.t (id, v) VALUES (1, '{}'), (2, '{}'), (3, 'B'), (4, 'a')",
        "x".repeat(70),
        "é".repeat(40)
    ))
    .unwrap();
    let mut snail = Snail::open(&path).unwrap();
    let (b, _) = snail
        .ask("SELECT t.id, t.v FROM s.t t ORDER BY t.v", Some("t_v"))
        .unwrap();
    assert_eq!(ids(&b), [3, 4, 1, 2]);
    let v = texts(&b, 1);
    assert_eq!(v[2].as_deref(), Some("x".repeat(63).as_str()));
    assert_eq!(v[3].as_deref(), Some("é".repeat(31).as_str()));
    let (b, _) = snail
        .ask(
            &format!("SELECT t.id FROM s.t t WHERE t.v = '{}'", "x".repeat(64)),
            None,
        )
        .unwrap();
    assert_eq!(ids(&b), [1]);
}

/// The bit strings of a table of bit(5) and bit varying.
const BITS_DDL: &str =
    "CREATE TABLE s.t (id integer NOT NULL, b bit(5), v varbit, PRIMARY KEY (id)); \
     CREATE INDEX t_b ON s.t (b, id); \
     CREATE INDEX t_v ON s.t (v, id);";

/// The bit strings each row of `BITS_DDL` holds, as their input reads them.
const BITS: [(Option<&str>, Option<&str>); 10] = [
    (Some("10100"), Some("1")),
    (None, None),
    (Some("B11111"), Some("")),
    (Some("00000"), Some("10")),
    (Some("10100"), Some("01")),
    (Some("00001"), Some("0")),
    (Some("B01010"), Some("00")),
    (Some("10101"), Some("100000000")),
    (Some("01111"), Some("11")),
    (Some("10000"), Some("X1F")),
];

fn bits_snail(name: &str) -> std::path::PathBuf {
    let keys = |bit: bool| {
        Values::Utf8(
            BITS.iter()
                .map(|r| {
                    let s = if bit { r.0 } else { r.1 };
                    s.map(|s| ByteArray::from(bits_parse(s, bit.then_some(5), !bit).unwrap()))
                })
                .collect(),
        )
    };
    snail_of(
        name,
        BITS_DDL,
        vec![
            Values::Int((1..=10).map(Some).collect()),
            keys(true),
            keys(false),
        ],
    )
}

#[test]
#[cfg(feature = "standalone")]
fn a_bit_string_is_read_as_bit_in_reads_it_and_held_to_its_length() {
    let path = tmp("bits-insert.snail");
    write(&path, &Catalog::parse(BITS_DDL).unwrap(), &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute(
        "INSERT INTO s.t (id, b, v) VALUES (1, '10100', '1'), (2, NULL, NULL), \
         (3, B'11111', ''), (4, '00000', '10'), (5, '10100', '01'), (6, '00001', '0'), \
         (7, 'B01010', '00'), (8, '10101', '100000000'), (9, '01111', '11'), \
         (10, '10000', X'1F')",
    )
    .unwrap();
    let ask = |p: &std::path::Path| {
        let (b, _) = Snail::open(p)
            .unwrap()
            .ask("SELECT t.id, t.b, t.v FROM s.t t ORDER BY t.id", None)
            .unwrap();
        (bit_texts(&b, 1), bit_texts(&b, 2))
    };
    assert_eq!(ask(&path), ask(&bits_snail("bits-insert-rows")));
    for (v, why) in [
        ("'1010'", "bit string length 4 does not match type bit(5)"),
        ("'10102'", "\"2\" is not a valid binary digit"),
    ] {
        let e = w
            .execute(&format!(
                "INSERT INTO s.t (id, b, v) VALUES (11, {v}, NULL)"
            ))
            .unwrap_err()
            .to_string();
        assert!(e.contains(why), "{v}: {e}");
    }
}

#[test]
fn a_bit_string_is_ordered_as_bit_cmp_orders_it_each_bit_a_place_of_three() {
    let path = bits_snail("bits-order");
    let mut snail = Snail::open(&path).unwrap();
    for order in ["t_v", "t_pkey"] {
        let (b, round) = snail
            .ask(
                "SELECT t.id, t.v FROM s.t t ORDER BY t.v, t.id",
                Some(order),
            )
            .unwrap();
        exact("varbit's order", &round);
        assert_eq!(ids(&b), [3, 6, 7, 10, 5, 1, 4, 8, 9, 2], "along {order}");
        assert_eq!(bit_texts(&b, 1)[7].as_deref(), Some("100000000"));
    }
    for order in ["t_b", "t_pkey"] {
        let (b, _) = snail
            .ask(
                "SELECT t.id, t.b FROM s.t t ORDER BY t.b DESC, t.id",
                Some(order),
            )
            .unwrap();
        assert_eq!(ids(&b), [2, 3, 8, 1, 5, 10, 9, 7, 6, 4], "along {order}");
    }
    // A literal of another length is compared by bit_cmp: B'101' lies below 10100, a prefix of it.
    for (q, want) in [
        (
            "SELECT t.id FROM s.t t WHERE t.b = B'10100' ORDER BY t.id",
            vec![1, 5],
        ),
        (
            "SELECT t.id FROM s.t t WHERE t.b < B'101' ORDER BY t.id",
            vec![4, 6, 7, 9, 10],
        ),
        (
            "SELECT t.id FROM s.t t WHERE t.b >= '10101' ORDER BY t.id",
            vec![3, 8],
        ),
        (
            "SELECT t.id FROM s.t t WHERE t.v = X'1F' ORDER BY t.id",
            vec![10],
        ),
        (
            "SELECT t.id FROM s.t t WHERE t.v < '1' ORDER BY t.id",
            vec![3, 5, 6, 7, 10],
        ),
    ] {
        let on = if q.contains("t.b") { "t_b" } else { "t_v" };
        for order in ["t_pkey", on] {
            let (b, round) = snail.ask(q, Some(order)).unwrap();
            exact(q, &round);
            assert_eq!(ids(&b), want, "{q} along {order}");
        }
    }
    let e = snail
        .ask("SELECT min(t.b) FROM s.t t", None)
        .unwrap_err()
        .to_string();
    assert!(e.contains("function min(bit) does not exist"), "{e}");
}

#[test]
fn a_bit_string_goes_out_as_its_page_form_and_comes_back_by_its_pages() {
    use parquet::file::reader::{FileReader, SerializedFileReader};
    let path = bits_snail("bits-out");
    let dir = tmp("bits-out-parquet");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let outs = Snail::open(&path).unwrap().out(&dir).unwrap();
    let file = &outs[0].file;
    let reader = SerializedFileReader::new(std::fs::File::open(file).unwrap()).unwrap();
    let schema = reader.metadata().file_metadata().schema_descr();
    let leaves: Vec<(String, String)> = (0..schema.num_columns())
        .map(|i| {
            let c = schema.column(i);
            (c.path().string(), format!("{:?}", c.physical_type()))
        })
        .collect();
    assert_eq!(
        leaves,
        [
            ("id".into(), "INT32".into()),
            ("b".into(), "FIXED_LEN_BYTE_ARRAY".into()),
            ("v.length".into(), "INT32".into()),
            ("v.bits".into(), "BYTE_ARRAY".into())
        ]
    );
    let (batches, groups) = parquet_of(file);
    let catalog = Catalog::parse(BITS_DDL).unwrap();
    let table = catalog.table("s.t").unwrap();
    let rows = Rows::from_batches(table, &batches).unwrap();
    let given = warren_snail::write::source(file, table).unwrap();
    let back = tmp("bits-out-back.snail");
    let written = warren_snail::write::write_spanned(
        &back,
        &catalog,
        &[("s.t".into(), rows)],
        &[("s.t".into(), given)],
        weather::span(),
    )
    .unwrap();
    let pkey = written.iter().find(|w| w.order == "t_pkey").unwrap();
    assert!(
        pkey.columns.iter().all(|c| c.moved_blocks == groups),
        "{pkey:?}"
    );
    let ask = |p: &std::path::Path| {
        let (b, _) = Snail::open(p)
            .unwrap()
            .ask("SELECT t.id, t.b, t.v FROM s.t t ORDER BY t.id", None)
            .unwrap();
        (bit_texts(&b, 1), bit_texts(&b, 2))
    };
    let (b, v) = ask(&back);
    assert_eq!(ask(&path), (b.clone(), v.clone()));
    assert_eq!(b[2].as_deref(), Some("11111"));
    assert_eq!(v[9].as_deref(), Some("00011111"));
}

#[test]
fn a_bit_string_is_cut_by_its_bits_and_a_short_one_never_decides() {
    let cut = |name: &str, ddl: &str, keys: Vec<Vec<u8>>| -> Vec<u64> {
        let path = snail_of(
            name,
            ddl,
            vec![Values::Utf8(
                keys.into_iter().map(|k| Some(ByteArray::from(k))).collect(),
            )],
        );
        blocks_of(
            &Snail::open(&path).unwrap().blocks().unwrap(),
            "t_pkey",
            "v",
        )
        .iter()
        .map(|r| r.rows)
        .collect()
    };
    let spread = |n: u32| -> Vec<Vec<u8>> {
        (0..4096u64)
            .map(|i| {
                let bits: String = (0..n)
                    .map(|r| {
                        if r < 12 && (i >> (11 - r)) & 1 == 1 {
                            '1'
                        } else {
                            '0'
                        }
                    })
                    .collect();
                bits_parse(&bits, Some(n), false).unwrap()
            })
            .collect()
    };
    // bit(64), 8 bytes written: 512 a page, cut on its first bits as a bigint is.
    assert_eq!(
        cut(
            "bits-cut-64",
            "CREATE TABLE s.t (v bit(64) NOT NULL, PRIMARY KEY (v));",
            spread(64)
        ),
        [512; 8]
    );
    // Beside a bigint, which a page holds 512 of: bit(16), 2 bytes written, never decides, and the
    // cut on its bits goes on to the bigint's 512; bit(32), 4 bytes, decides, and stops at 1,024.
    let beside = |n: u32| -> Vec<u64> {
        let path = snail_of(
            &format!("bits-cut-beside-{n}"),
            &format!("CREATE TABLE s.t (v bit({n}) NOT NULL, w bigint NOT NULL, PRIMARY KEY (v));"),
            vec![
                Values::Utf8(
                    spread(n)
                        .into_iter()
                        .map(|k| Some(ByteArray::from(k)))
                        .collect(),
                ),
                Values::Int((0..4096).map(Some).collect()),
            ],
        );
        blocks_of(
            &Snail::open(&path).unwrap().blocks().unwrap(),
            "t_pkey",
            "v",
        )
        .iter()
        .map(|r| r.rows)
        .collect()
    };
    assert_eq!(beside(16), [512; 8]);
    assert_eq!(beside(32), [1024; 4]);
}

#[test]
fn the_collations_of_byte_order_are_held_and_any_other_refused_by_name() {
    for c in [
        "\"C\"",
        "\"POSIX\"",
        "ucs_basic",
        "pg_c_utf8",
        "pg_unicode_fast",
        "\"C.utf8\"",
        "pg_catalog.\"C\"",
        "\"default\"",
    ] {
        let ddl = format!(
            "CREATE TABLE s.t (id integer NOT NULL, v text COLLATE {c}, n name COLLATE {c}, \
             b char(2) COLLATE {c}, PRIMARY KEY (id));"
        );
        Catalog::parse(&ddl).unwrap_or_else(|e| panic!("{c}: {e}"));
    }
    for c in ["\"en_US.utf8\"", "\"und-x-icu\"", "unicode", "c"] {
        let ddl = format!(
            "CREATE TABLE s.t (id integer NOT NULL, v text COLLATE {c}, PRIMARY KEY (id));"
        );
        let e = Catalog::parse(&ddl).unwrap_err().to_string();
        assert!(e.contains("only a collation of byte order"), "{c}: {e}");
    }
    let c = Catalog::parse(
        "CREATE COLLATION s.fast (provider = builtin, locale = 'PG_UNICODE_FAST'); \
         CREATE COLLATION s.lib (provider = libc, locale = 'C.utf8'); \
         CREATE TABLE s.t (id integer NOT NULL, v text COLLATE s.fast, w text COLLATE s.lib, \
         PRIMARY KEY (id));",
    )
    .unwrap();
    assert_eq!(c.collations.len(), 2);
    let e = Catalog::parse("CREATE COLLATION s.us (provider = libc, locale = 'en_US.utf8');")
        .unwrap_err()
        .to_string();
    assert!(e.contains("only a collation of byte order"), "{e}");
}

#[test]
fn case_follows_the_collation_a_column_or_a_collate_names() {
    let path = snail_of(
        "case-collations",
        "CREATE TABLE s.t (id integer NOT NULL, d text, c text COLLATE \"C\", f text COLLATE \
         pg_unicode_fast, n name, PRIMARY KEY (id));",
        vec![
            Values::Int(vec![Some(1)]),
            Values::Text(vec![Some("straße é".into())]),
            Values::Text(vec![Some("straße é".into())]),
            Values::Text(vec![Some("straße é".into())]),
            Values::Text(vec![Some("straße é".into())]),
        ],
    );
    let mut snail = Snail::open(&path).unwrap();
    let (b, _) = snail
        .ask(
            "WITH t AS (SELECT id, d, c, f, n FROM s.t) SELECT upper(d) AS d, upper(c) AS c, \
             upper(f) AS f, upper(n) AS n, upper(d COLLATE pg_c_utf8) AS s, \
             upper(d COLLATE \"POSIX\") AS p FROM t",
            None,
        )
        .unwrap();
    let row: Vec<Option<String>> = (0..6).map(|c| texts(&b, c)[0].clone()).collect();
    let some = |s: &str| Some(s.to_string());
    // The default glibc's C.UTF-8, C ASCII's letters alone, pg_unicode_fast in full, a name under
    // its own C, pg_c_utf8 a character to a character.
    assert_eq!(
        row,
        [
            some("STRAßE É"),
            some("STRAßE é"),
            some("STRASSE É"),
            some("STRAßE é"),
            some("STRAßE É"),
            some("STRAßE é")
        ]
    );
    // A column a step renames keeps its collation.
    let (b, _) = snail
        .ask(
            "WITH t AS (SELECT id, f AS g FROM s.t) SELECT lower(upper(g)) AS x FROM t",
            None,
        )
        .unwrap();
    assert_eq!(texts(&b, 0)[0].as_deref(), Some("strasse é"));
    let e = snail
        .ask(
            "WITH t AS (SELECT id, d FROM s.t) SELECT upper(d COLLATE \"en_US.utf8\") AS x FROM t",
            None,
        )
        .unwrap_err()
        .to_string();
    assert!(e.contains("only a collation of byte order"), "{e}");
}
