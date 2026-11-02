//! A table met with rows made before it is kept to their keys on every type the snail cuts on, in
//! each of the four places: a join with a step, a comma met by WHERE, a subquery's table and an
//! INTERSECT's second arm. Each table holds 16,384 rows, one key a row, cut into blocks; two rows of
//! it are found by another column, and the table met with them reads the blocks their keys lie
//! in, at most one more each where a separator only bounds them. Every answer is worked out from
//! the two rows found.

mod weather;

use warren_snail::catalog::Catalog;
use warren_snail::read::{Round, Snail};
use warren_snail::values::{push_datum, Values};
use warren_snail::write::Rows;
use weather::{tmp, write};

const ROWS: i64 = 1 << 14;

/// The two rows found: their `x`s sum to 14,001.
const FOUND: &str = "t.x BETWEEN 7000 AND 7001";

/// The table with its key in `ty`, each row's key from `key`, its `x` the row's number.
fn table(name: &str, types: &str, ty: &str, key: impl Fn(i64) -> String) -> std::path::PathBuf {
    let path = tmp(&format!("keyed-{name}.snail"));
    let ddl =
        format!("{types} CREATE TABLE s.t (v {ty} NOT NULL, x bigint NOT NULL, PRIMARY KEY (v));");
    let catalog = Catalog::parse(&ddl).unwrap();
    let t = catalog.table("s.t").unwrap();
    let mut v = Values::new(t.columns[0].ty);
    let mut x = Values::new(t.columns[1].ty);
    for i in 0..ROWS {
        push_datum(
            &mut v,
            &warren_snail::composed::scalar_in(t.columns[0].ty, &key(i))
                .unwrap_or_else(|e| panic!("{name}: {e}")),
        );
        push_datum(
            &mut x,
            &warren_snail::composed::scalar_in(t.columns[1].ty, &i.to_string()).unwrap(),
        );
    }
    write(
        &path,
        &catalog,
        &[(
            "s.t".into(),
            Rows {
                columns: vec![v, x],
            },
        )],
    )
    .unwrap();
    path
}

fn one(b: &arrow::array::RecordBatch, name: &str) -> i64 {
    match &warren_snail::read::pipeline_rows(b).unwrap()[..] {
        [r] => match &r[..] {
            [warren_snail::read::Value::Int(v)] => *v,
            [warren_snail::read::Value::Num(v, 0)] => *v as i64,
            other => panic!("{name}: one integer: {other:?}"),
        },
        other => panic!("one row: {other:?}"),
    }
}

/// The table's blocks.
fn blocks(path: &std::path::Path) -> u64 {
    Snail::open(path)
        .unwrap()
        .blocks()
        .unwrap()
        .iter()
        .map(|b| b.block)
        .max()
        .unwrap()
        + 1
}

/// Each read priced before it is read, and the table met with the rows found kept to their two keys.
fn kept(name: &str, q: &str, round: &Round, all: u64) {
    for r in &round.reads {
        assert_eq!(
            r.column_pages_read,
            r.column_pages_predicted - r.column_pages_held,
            "{name}: {q}: {r:?}"
        );
    }
    assert_eq!(round.reads.len(), 2, "{name}: {q}: {round:?}");
    let k = &round.reads[1];
    assert!(
        k.blocks_read <= 4 && (all < 8 || k.blocks_read * 2 <= all),
        "{name}: {q}: {} blocks of {all}: {k:?}",
        k.blocks_read
    );
}

/// The four places on one family of types.
fn four(name: &str, types: &str, ty: &str, key: impl Fn(i64) -> String) {
    let path = table(name, types, ty, key);
    let all = blocks(&path);
    // Days no year of which fills a page lie in one block.
    assert!(all >= 4 || ty == "date", "{name}: {all} blocks");
    let found = format!("SELECT t.v FROM s.t t WHERE {FOUND}");
    for (q, want) in [
        (
            format!("SELECT sum(u.x) FROM ({found}) k JOIN s.t u ON u.v = k.v"),
            14_001,
        ),
        (
            format!("SELECT sum(u.x) FROM ({found}) k, s.t u WHERE u.v = k.v"),
            14_001,
        ),
        (
            format!(
                "SELECT count(*) FROM s.t t WHERE {FOUND} \
                 AND EXISTS (SELECT 1 FROM s.t u WHERE u.v = t.v)"
            ),
            2,
        ),
        (
            format!("SELECT count(*) FROM ({found} INTERSECT SELECT u.v FROM s.t u) x"),
            2,
        ),
    ] {
        let mut snail = Snail::open(&path).unwrap();
        let (b, round) = snail
            .ask(&q, None)
            .unwrap_or_else(|e| panic!("{name}: {q}: {e}"));
        assert_eq!(one(&b, name), want, "{name}: {q}");
        kept(name, &q, &round, all);
    }
}

fn nibbles(i: i64) -> String {
    (0..4)
        .rev()
        .map(|d| char::from(b'a' + ((i >> (d * 4)) & 15) as u8))
        .collect()
}

#[test]
fn an_integer_and_a_float_are_kept_to_their_keys() {
    four("int4", "", "integer", |i| i.to_string());
    four("int2", "", "smallint", |i| (i - 8192).to_string());
    four("int8", "", "bigint", |i| (i * 1_000_000).to_string());
    four("float8", "", "double precision", |i| format!("{}.5", i));
    four("float4", "", "real", |i| format!("{}.5", i));
}

#[test]
fn a_numeric_is_kept_to_its_keys_at_each_precision() {
    four("num18", "", "numeric(10, 2)", |i| {
        format!("{}.{:02}", i / 100, i % 100)
    });
    four("num38", "", "numeric(30, 2)", |i| {
        format!("{}.{:02}", i / 100, i % 100)
    });
    four("numtext", "", "numeric", |i| {
        format!("{}.{:02}", i / 100, i % 100)
    });
}

#[test]
fn a_string_and_bytes_are_kept_to_their_keys() {
    four("text", "", "text", nibbles);
    four("varchar", "", "varchar(8)", nibbles);
    four("bpchar", "", "char(4)", nibbles);
    four("name", "", "name", nibbles);
    four("bytea", "", "bytea", |i| format!("\\x{:04x}", i));
    four("bit", "", "bit(32)", |i| format!("{:032b}", i));
    four("varbit", "", "bit varying", |i| {
        format!("{:b}", i + (1 << 14))
    });
}

#[test]
fn a_moment_and_a_span_are_kept_to_their_keys() {
    let at = |i: i64| {
        chrono::DateTime::from_timestamp(1_514_764_800 + i * 3_600, 0)
            .unwrap()
            .format("%Y-%m-%d %H:%M:%S")
            .to_string()
    };
    four("timestamptz", "", "timestamptz", |i| {
        format!("{}+00", at(i))
    });
    four("timestamp", "", "timestamp", at);
    four("date", "", "date", |i| {
        chrono::DateTime::from_timestamp(1_514_764_800 + i * 86_400, 0)
            .unwrap()
            .format("%Y-%m-%d")
            .to_string()
    });
    four("time", "", "time", |i| {
        format!("{:02}:{:02}:{:02}", i / 3600, i / 60 % 60, i % 60)
    });
    four("timetz", "", "timetz", |i| {
        format!("{:02}:{:02}:{:02}+00", i / 3600, i / 60 % 60, i % 60)
    });
    four("interval", "", "interval", |i| format!("{i} seconds"));
}

#[test]
fn the_scalars_are_kept_to_their_keys() {
    four("uuid", "", "uuid", |i| {
        format!("00000000-0000-0000-0000-{:012x}", i)
    });
    four("oid", "", "oid", |i| (i * 7).to_string());
    four("money", "", "money", |i| {
        format!("{}.{:02}", i / 100, i % 100)
    });
    four("macaddr", "", "macaddr", |i| {
        format!("00:00:00:00:{:02x}:{:02x}", i >> 8, i & 255)
    });
    four("pg_lsn", "", "pg_lsn", |i| format!("0/{:X}", i));
    // An enum's labels: 16,384 of them, keyed by their rank.
    let labels = (0..ROWS)
        .map(|i| format!("'{}'", nibbles(i)))
        .collect::<Vec<_>>()
        .join(", ");
    four(
        "enum",
        &format!("CREATE TYPE s.e AS ENUM ({labels});"),
        "s.e",
        nibbles,
    );
}

#[test]
fn a_composed_value_and_the_types_kept_as_text_are_kept_to_their_keys() {
    four("int4_array", "", "integer[]", |i| {
        format!("{{{i},{}}}", i + 1)
    });
    four("text_array", "", "text[]", |i| {
        format!("{{{}}}", nibbles(i))
    });
    four("int4range", "", "int4range", |i| format!("[{i},{})", i + 1));
    four("int4multirange", "", "int4multirange", |i| {
        format!("{{[{i},{})}}", i + 1)
    });
    four(
        "record",
        "CREATE TYPE s.c AS (a integer, b text);",
        "s.c",
        |i| format!("({i},x)"),
    );
    four("inet", "", "inet", |i| {
        format!("200.200.{}.{}", 128 + (i >> 7), 128 + (i & 127))
    });
    four("jsonb", "", "jsonb", |i| format!("\"{}\"", nibbles(i)));
    four("tsvector", "", "tsvector", nibbles);
}
