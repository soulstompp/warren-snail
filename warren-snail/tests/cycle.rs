//! Every family of types cut on its own cycle, at a size that cuts: an open value (text, bytea, a
//! lexeme) by the bits of its bytes, a self-enclosing one (an instant, an interval, a number, an
//! address) on its own rungs, an enclosing one (an array, a range, a record, jsonb) on its
//! structure, each part on its own rungs; each block's unit from its own furthest-out place; the
//! interval as one leaf of 16 bytes; and a column keyed by its bytes ruled out by its entries'
//! bounds, cut to 8 bytes. Each table holds 16,384 rows spread evenly on their own places, and each
//! round reads the pages it predicted.

mod weather;

use std::path::PathBuf;
use warren_snail::catalog::Catalog;
use warren_snail::read::{BlockRow, Snail};
use warren_snail::values::{push_datum, Values};
use warren_snail::write::Rows;
use weather::{exact, tmp, write};

const ROWS: i64 = 1 << 14;

/// A snail of one table `s.t`, each column's values read from their text as PostgreSQL reads them,
/// a NULL where none is given.
fn written(name: &str, ddl: &str, columns: Vec<Vec<Option<String>>>) -> PathBuf {
    let path = tmp(&format!("cycle-{name}.snail"));
    let catalog = Catalog::parse(ddl).unwrap();
    let table = catalog.table("s.t").unwrap();
    let columns = table
        .columns
        .iter()
        .zip(columns)
        .map(|(c, texts)| {
            let mut v = Values::new(c.ty);
            for t in texts {
                match t {
                    Some(t) => push_datum(
                        &mut v,
                        &warren_snail::composed::scalar_in(c.ty, &t).unwrap(),
                    ),
                    None => v.push_null(),
                }
            }
            v
        })
        .collect();
    write(&path, &catalog, &[("s.t".into(), Rows { columns })]).unwrap();
    path
}

/// Each row's text, from its number.
fn each(f: impl Fn(i64) -> String) -> Vec<Option<String>> {
    (0..ROWS).map(|i| Some(f(i))).collect()
}

/// The rows of each block of an order, in its order.
fn blocks_of(rows: &[BlockRow], order: &str) -> Vec<u64> {
    let mut out: Vec<(u64, u64)> = Vec::new();
    for r in rows.iter().filter(|r| r.order == order) {
        if out.last().map(|&(b, _)| b) != Some(r.block) {
            out.push((r.block, r.rows));
        }
    }
    out.into_iter().map(|(_, n)| n).collect()
}

/// The unit of an order's first leaf in each of its blocks.
fn units_of(rows: &[BlockRow], order: &str) -> Vec<u64> {
    let mut out: Vec<(u64, u64)> = Vec::new();
    for r in rows.iter().filter(|r| r.order == order) {
        if out.last().map(|&(b, _)| b) != Some(r.block) {
            out.push((r.block, r.k));
        }
    }
    out.into_iter().map(|(_, k)| k).collect()
}

/// A question asked of the snail at `path`, its round reading the pages it predicted: its blocks
/// read.
fn read(path: &PathBuf, q: &str) -> u64 {
    let mut snail = Snail::open(path).unwrap();
    let (_, round) = snail.ask(q, None).unwrap();
    exact(q, &round);
    round.blocks_read
}

/// The instant `seconds` after 2018 began, as text.
fn at(seconds: i64) -> String {
    chrono::DateTime::from_timestamp(1_514_764_800 + seconds, 0)
        .unwrap()
        .format("%Y-%m-%d %H:%M:%S+00")
        .to_string()
}

/// Four characters of 0x40 to 0x4f whose low nibbles hold `i`'s 14 bits.
fn nibbles(i: i64) -> String {
    [(i >> 12), (i >> 8) & 15, (i >> 4) & 15, i & 15]
        .iter()
        .map(|&c| char::from(0x40 + c as u8))
        .collect()
}

/// Three characters of 0x40 to 0x7f whose low bits are `i`'s 14, so that the text sorts and
/// spreads on its bits as `i` does.
fn chars(i: i64) -> String {
    [(i >> 8), (i >> 2) & 63, i & 3]
        .iter()
        .map(|&c| char::from(0x40 + c as u8))
        .collect()
}

#[test]
fn an_integer_is_cut_by_its_bits() {
    // 4 bytes a row: a page fills at 1,024 rows, so 16,384 spread evenly make 16 blocks.
    let path = written(
        "int",
        "CREATE TABLE s.t (a integer NOT NULL, PRIMARY KEY (a));",
        vec![each(|i| i.to_string())],
    );
    let blocks = Snail::open(&path).unwrap().blocks().unwrap();
    assert_eq!(blocks_of(&blocks, "t_pkey"), [1024; 16]);
    assert_eq!(read(&path, "SELECT t.a FROM s.t t WHERE t.a = 5000"), 1);
}

#[test]
fn a_float_is_cut_by_the_bits_of_its_class() {
    // 16,384 doubles one apart in their last bits, 8 bytes a row: a page fills at 512 rows.
    let path = written(
        "float",
        "CREATE TABLE s.t (a double precision NOT NULL, PRIMARY KEY (a));",
        vec![each(|i| {
            f64::from_bits(1f64.to_bits() + i as u64).to_string()
        })],
    );
    let blocks = Snail::open(&path).unwrap().blocks().unwrap();
    assert_eq!(blocks_of(&blocks, "t_pkey"), [512; 32]);
}

#[test]
fn a_numeric_is_cut_by_the_bits_of_its_smallest_unit() {
    // numeric(10, 2) as the count of its hundredths, 8 bytes a row; its NaN leaf never decides.
    let path = written(
        "numeric",
        "CREATE TABLE s.t (a numeric(10, 2) NOT NULL, PRIMARY KEY (a));",
        vec![each(|i| format!("{}.{:02}", i / 100, i % 100))],
    );
    let blocks = Snail::open(&path).unwrap().blocks().unwrap();
    assert_eq!(blocks_of(&blocks, "t_pkey"), [512; 32]);
}

#[test]
fn an_instant_is_cut_on_the_calendar_and_its_thin_month_joins_the_one_before() {
    // An hour apart from 2018 on, 8 bytes a row, a page of 512: each month fills one, each day
    // none, so every month is a block; 2019's November, 328 hours, joins October.
    let path = written(
        "instant",
        "CREATE TABLE s.t (a timestamptz NOT NULL, PRIMARY KEY (a));",
        vec![each(|i| at(3600 * i))],
    );
    let blocks = Snail::open(&path).unwrap().blocks().unwrap();
    let days = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut want: Vec<u64> = days.iter().map(|d| d * 24).collect();
    want.extend(days[..9].iter().map(|d| d * 24));
    want.push(31 * 24 + 328);
    assert_eq!(blocks_of(&blocks, "t_pkey"), want);
    assert_eq!(
        read(
            &path,
            "SELECT t.a FROM s.t t WHERE t.a = '2018-06-15 12:00:00+00'"
        ),
        1
    );
}

#[test]
fn an_interval_is_one_leaf_of_16_bytes_cut_on_30_day_months() {
    // An hour apart: a 30-day month holds 720, a page of 16 bytes 256, so each month is a block, the
    // last of 544; as three leaves its 4-byte months would have filled a page only at 1,024 rows,
    // and no month would have split.
    let path = written(
        "interval",
        "CREATE TABLE s.t (a interval NOT NULL, PRIMARY KEY (a));",
        vec![each(|i| format!("{i} hours"))],
    );
    let blocks = Snail::open(&path).unwrap().blocks().unwrap();
    let mut want = vec![720; 22];
    want.push(544);
    assert_eq!(blocks_of(&blocks, "t_pkey"), want);
    assert!(blocks.iter().all(|r| r.column == "a"));
    assert_eq!(
        read(&path, "SELECT t.a FROM s.t t WHERE t.a = '100 hours'"),
        1
    );
}

#[test]
fn a_text_is_cut_a_bit_a_rung_where_a_byte_a_rung_kept_one_block() {
    // Three characters and their length, 7 bytes a row: a page fills at 586 rows, so the halves of
    // 1,024 split and those of 512 do not. A byte a rung, each first character's 256 rows fill none.
    let path = written(
        "text",
        "CREATE TABLE s.t (a text NOT NULL, PRIMARY KEY (a));",
        vec![each(chars)],
    );
    let blocks = Snail::open(&path).unwrap().blocks().unwrap();
    assert_eq!(blocks_of(&blocks, "t_pkey"), [1024; 16]);
    assert_eq!(
        read(
            &path,
            &format!("SELECT t.a FROM s.t t WHERE t.a = '{}'", chars(9000))
        ),
        1
    );
}

#[test]
fn a_bytea_is_cut_a_bit_a_rung() {
    // Two bytes and their length, 6 bytes a row: a page fills at 683 rows.
    let path = written(
        "bytea",
        "CREATE TABLE s.t (a bytea NOT NULL, PRIMARY KEY (a));",
        vec![each(|i| format!("\\x{:04x}", i))],
    );
    let blocks = Snail::open(&path).unwrap().blocks().unwrap();
    assert_eq!(blocks_of(&blocks, "t_pkey"), [1024; 16]);
}

#[test]
fn a_uuid_is_cut_by_its_128_bits() {
    // 16 bytes a row: a page fills at 256 rows.
    let path = written(
        "uuid",
        "CREATE TABLE s.t (a uuid NOT NULL, PRIMARY KEY (a));",
        vec![each(|i| {
            format!("{:04x}0000-0000-0000-0000-000000000000", i << 2)
        })],
    );
    let blocks = Snail::open(&path).unwrap().blocks().unwrap();
    assert_eq!(blocks_of(&blocks, "t_pkey"), [256; 64]);
}

#[test]
fn an_address_is_cut_by_the_bits_of_its_key() {
    // Addresses whose last two bytes hold 14 bits, their text 15 characters and its length: a page
    // fills at 216 rows, so blocks of 256.
    let path = written(
        "inet",
        "CREATE TABLE s.t (a inet NOT NULL, PRIMARY KEY (a));",
        vec![each(|i| {
            format!("200.200.{}.{}", 128 + (i >> 7), 128 + (i & 127))
        })],
    );
    let blocks = Snail::open(&path).unwrap().blocks().unwrap();
    assert_eq!(blocks_of(&blocks, "t_pkey"), [256; 64]);
}

#[test]
fn a_point_carried_decides_as_its_two_8_byte_leaves() {
    let path = written(
        "point",
        "CREATE TABLE s.t (k bigint NOT NULL, p point NOT NULL, PRIMARY KEY (k));",
        vec![each(|i| i.to_string()), each(|i| format!("({i}, {})", -i))],
    );
    let blocks = Snail::open(&path).unwrap().blocks().unwrap();
    assert_eq!(blocks_of(&blocks, "t_pkey"), [512; 32]);
}

#[test]
fn a_range_is_cut_on_its_structure_its_bounds_on_their_own_rungs() {
    // An int4range's three 4-byte leaves: a page fills at 1,024 rows, cut on the lower bound's bits.
    let ints = written(
        "int4range",
        "CREATE TABLE s.t (r int4range NOT NULL, PRIMARY KEY (r));",
        vec![each(|i| format!("[{i},{})", i + 1))],
    );
    let blocks = Snail::open(&ints).unwrap().blocks().unwrap();
    assert_eq!(blocks_of(&blocks, "t_pkey"), [1024; 16]);
    assert_eq!(
        read(&ints, "SELECT t.r FROM s.t t WHERE t.r = '[77,78)'"),
        1
    );
    // A tstzrange's lower bound on the calendar, half an hour apart through 2018: its flags fill a
    // page at 1,024 rows, each month 1,344 to 1,488 and each day 48, so every month is a block, and
    // December's 352 join November.
    let instants = written(
        "tstzrange",
        "CREATE TABLE s.t (r tstzrange NOT NULL, PRIMARY KEY (r));",
        vec![(0..ROWS)
            .map(|i| Some(format!("[{},{})", at(1800 * i), at(1800 * (i + 1)))))
            .collect()],
    );
    let blocks = Snail::open(&instants).unwrap().blocks().unwrap();
    let days = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30 + 7];
    let mut want: Vec<u64> = days.iter().map(|d| d * 48).collect();
    *want.last_mut().unwrap() += 16;
    assert_eq!(blocks_of(&blocks, "t_pkey"), want);
}

#[test]
fn a_multirange_is_cut_on_its_ranges_in_turn() {
    let path = written(
        "multirange",
        "CREATE TABLE s.t (m int4multirange NOT NULL, PRIMARY KEY (m));",
        vec![each(|i| format!("{{[{i},{})}}", i + 1))],
    );
    let blocks = Snail::open(&path).unwrap().blocks().unwrap();
    assert_eq!(blocks_of(&blocks, "t_pkey"), [1024; 16]);
}

#[test]
fn an_array_is_cut_on_its_elements_and_a_long_one_takes_a_unit_only_in_its_own_block() {
    // Three elements of 4 bytes a row: a page fills at 342 rows, so blocks of 512 on the first
    // element's bits. One row of 10,000 elements, 40,000 bytes, takes its own block's unit to 12
    // disk pages; every other block keeps a unit of one.
    let path = written(
        "array",
        "CREATE TABLE s.t (a integer[] NOT NULL, PRIMARY KEY (a));",
        vec![each(|i| {
            let rest = if i == 8000 { 9999 } else { 2 };
            format!("{{{i}{}}}", ",0".repeat(rest))
        })],
    );
    let blocks = Snail::open(&path).unwrap().blocks().unwrap();
    assert_eq!(blocks_of(&blocks, "t_pkey"), [512; 32]);
    let units = units_of(&blocks, "t_pkey");
    let mut want = vec![1; 32];
    want[8000 / 512] = 12;
    assert_eq!(units, want);
    // A point read of another block reads its unit of one page.
    let other = read(&path, "SELECT t.a FROM s.t t WHERE t.a = '{100,0,0}'");
    assert_eq!(other, 1);
    // Text elements by the bits of their bytes: four characters and their length, a page of 512.
    let texts = written(
        "text-array",
        "CREATE TABLE s.t (a text[] NOT NULL, PRIMARY KEY (a));",
        vec![each(|i| format!("{{{}}}", nibbles(i)))],
    );
    let blocks = Snail::open(&texts).unwrap().blocks().unwrap();
    assert_eq!(blocks_of(&blocks, "t_pkey"), [512; 32]);
}

#[test]
fn a_jsonb_is_cut_on_the_bits_of_its_key() {
    // A string of four characters, its text six and its length: 10 bytes a row, a page of 410 rows.
    let path = written(
        "jsonb",
        "CREATE TABLE s.t (j jsonb NOT NULL, PRIMARY KEY (j));",
        vec![each(|i| format!("\"{}\"", nibbles(i)))],
    );
    let blocks = Snail::open(&path).unwrap().blocks().unwrap();
    assert_eq!(blocks_of(&blocks, "t_pkey"), [512; 32]);
}

#[test]
fn a_tsvector_is_cut_on_the_bits_of_its_lexemes() {
    // One lexeme of four characters, whose low nibbles hold 14 bits; its text quoted, 6 and its
    // length: 10 bytes a row, a page of 410 rows.
    let path = written(
        "tsvector",
        "CREATE TABLE s.t (v tsvector NOT NULL, PRIMARY KEY (v));",
        vec![each(nibbles)],
    );
    let blocks = Snail::open(&path).unwrap().blocks().unwrap();
    assert_eq!(blocks_of(&blocks, "t_pkey"), [512; 32]);
}

#[test]
fn an_enum_is_cut_by_its_rank_and_its_label_never_decides() {
    let path = written(
        "enum",
        "CREATE TYPE s.e AS ENUM ('north', 'east', 'south', 'west');
         CREATE TABLE s.t (e s.e NOT NULL, k integer NOT NULL, PRIMARY KEY (e, k));",
        vec![
            each(|i| ["north", "east", "south", "west"][(i % 4) as usize].into()),
            each(|i| (i / 4).to_string()),
        ],
    );
    let blocks = Snail::open(&path).unwrap().blocks().unwrap();
    assert_eq!(blocks_of(&blocks, "t_pkey"), [1024; 16]);
}

#[test]
fn a_record_is_cut_on_its_fields_in_turn() {
    let path = written(
        "record",
        "CREATE TYPE s.c AS (a integer, b text);
         CREATE TABLE s.t (c s.c NOT NULL, PRIMARY KEY (c));",
        vec![each(|i| format!("({i},x)"))],
    );
    let blocks = Snail::open(&path).unwrap().blocks().unwrap();
    assert_eq!(blocks_of(&blocks, "t_pkey"), [1024; 16]);
}

#[test]
fn money_is_cut_by_the_bits_of_its_count() {
    let path = written(
        "money",
        "CREATE TABLE s.t (m money NOT NULL, PRIMARY KEY (m));",
        vec![each(|i| format!("{}.{:02}", i / 100, i % 100))],
    );
    let blocks = Snail::open(&path).unwrap().blocks().unwrap();
    assert_eq!(blocks_of(&blocks, "t_pkey"), [512; 32]);
}

#[test]
fn a_bit_string_is_cut_by_its_bits() {
    // bit(32), 4 bytes a row, and a bit varying, its length beside its bits: pages of 1,024 rows.
    let bits = |i: i64| format!("{:032b}", i);
    let fixed = written(
        "bit",
        "CREATE TABLE s.t (b bit(32) NOT NULL, PRIMARY KEY (b));",
        vec![each(bits)],
    );
    let blocks = Snail::open(&fixed).unwrap().blocks().unwrap();
    assert_eq!(blocks_of(&blocks, "t_pkey"), [1024; 16]);
    let varying = written(
        "varbit",
        "CREATE TABLE s.t (b bit varying(32) NOT NULL, PRIMARY KEY (b));",
        vec![each(bits)],
    );
    let blocks = Snail::open(&varying).unwrap().blocks().unwrap();
    assert_eq!(blocks_of(&blocks, "t_pkey"), [1024; 16]);
}

#[test]
fn a_text_carried_rules_blocks_out_by_its_bounds_cut_to_8_bytes() {
    // The text rises with the key, so each block's least and greatest name a run of its own: an
    // equality reads the one block whose bounds hold it, and a value past every bound reads none.
    let path = written(
        "bounds",
        "CREATE TABLE s.t (k integer NOT NULL, t text NOT NULL, PRIMARY KEY (k));",
        vec![
            each(|i| i.to_string()),
            each(|i| format!("{}{}", chars(i), "z".repeat(12))),
        ],
    );
    let q = |v: &str| format!("SELECT t.k FROM s.t t WHERE t.t = '{v}'");
    let hit = format!("{}{}", chars(9000), "z".repeat(12));
    assert_eq!(read(&path, &q(&hit)), 1);
    assert_eq!(read(&path, &q("!")), 0);
    assert_eq!(read(&path, &q(&chars(9000))), 1);
}
