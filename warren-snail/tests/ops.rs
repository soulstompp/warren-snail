//! The operators of the numbers, the strings and the documents by their own names, each read in a
//! plain question, a WHERE and a WITH step alike: typed and bound as PostgreSQL 18.6's parser types
//! and binds them, overflowing and refused in its words, and where a read narrows, its pages exact
//! and fewer than the table's. Every answer here is PostgreSQL's over the same rows.

mod weather;

use arrow::array::{Array, RecordBatch};
use arrow::util::display::array_value_to_string;
use std::path::{Path, PathBuf};
use warren_snail::catalog::Catalog;
use warren_snail::read::Round;
use warren_snail::values::{push_datum, Values};
use warren_snail::write::Rows;
use weather::{exact, fresh, tmp, write};

const DDL: &str =
    "CREATE TABLE s.t (id integer PRIMARY KEY, i2 smallint, i2b smallint, i4 integer, \
     i4b integer, i8 bigint, f4 real, f8 double precision, n numeric, t text, tb text, c char(4), \
     nm name, b bytea, bb bytea, j json, jb jsonb, k text[]);";

/// A row of `s.t`, each column's text as its input reads it.
type Row = [Option<&'static str>; 18];

/// The rows: every column NULL; each integer's lowest; its highest; and values of each type.
const ROWS: [Row; 5] = [
    [
        Some("1"),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    ],
    [
        Some("2"),
        Some("-32768"),
        Some("1"),
        Some("-2147483648"),
        Some("1"),
        Some("-9223372036854775808"),
        Some("-1.5"),
        Some("-1.5"),
        Some("-1.5"),
        Some("abc"),
        Some("ab"),
        Some("abc"),
        Some("abc"),
        Some("abc"),
        Some("ab%"),
        Some("{\"a\": {\"b\": 1}}"),
        Some("{\"a\": {\"b\": 1}, \"c\": 2}"),
        Some("{a,b}"),
    ],
    [
        Some("3"),
        Some("32767"),
        Some("15"),
        Some("2147483647"),
        Some("31"),
        Some("9223372036854775807"),
        Some("4"),
        Some("16"),
        Some("16"),
        Some("b"),
        Some("b%"),
        Some("b "),
        Some("b"),
        Some("b"),
        Some("_"),
        Some("[1, 2]"),
        Some("[1, 2]"),
        Some("{1}"),
    ],
    [
        Some("4"),
        Some("6"),
        Some("3"),
        Some("6"),
        Some("3"),
        Some("6"),
        Some("NaN"),
        Some("-Infinity"),
        Some("NaN"),
        Some("A%"),
        Some("a\\%"),
        Some("A%"),
        Some("A%"),
        Some("\\xc3a9"),
        Some("\\xc325"),
        Some("{\"c\": 1}"),
        Some("{\"c\": 1}"),
        Some("{c}"),
    ],
    [
        Some("5"),
        Some("-7"),
        Some("-1"),
        Some("-7"),
        Some("-1"),
        Some("-7"),
        Some("0"),
        Some("27"),
        Some("0"),
        Some(""),
        Some(""),
        Some(""),
        Some(""),
        Some(""),
        Some(""),
        Some("null"),
        Some("null"),
        Some("{a,NULL}"),
    ],
];

fn snail(name: &str) -> PathBuf {
    let path = tmp(&format!("ops-{name}.snail"));
    let catalog = Catalog::parse(DDL).unwrap();
    let t = catalog.table("s.t").unwrap();
    let columns = t
        .columns
        .iter()
        .enumerate()
        .map(|(c, col)| {
            let mut v = Values::new(col.ty);
            for row in &ROWS {
                match row[c] {
                    Some(text) => push_datum(
                        &mut v,
                        &warren_snail::composed::scalar_in(col.ty, text).unwrap(),
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

/// An answer's rows, each value as text, a NULL as `-`.
fn rows(b: &RecordBatch) -> Vec<String> {
    (0..b.num_rows())
        .map(|r| {
            (0..b.num_columns())
                .map(|c| match b.column(c).is_null(r) {
                    true => "-".to_string(),
                    false => array_value_to_string(b.column(c), r).unwrap(),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

/// An expression over the rows of `s.t` as PostgreSQL's text writes it, read each way: as a plain
/// question, and in a WITH step, the same; and in a WHERE, the rows where it is not NULL.
fn each_way(path: &Path, expr: &str) -> Vec<String> {
    let mut snail = fresh(path);
    let mut ask = |q: String| {
        let (b, _) = snail.ask(&q, None).unwrap_or_else(|e| panic!("{q}: {e}"));
        rows(&b)
    };
    let plain = ask(format!("SELECT ({expr})::text AS x FROM s.t ORDER BY id"));
    let stepped = ask(format!(
        "WITH w AS (SELECT * FROM s.t) SELECT ({expr})::text AS x FROM w ORDER BY id"
    ));
    assert_eq!(plain, stepped, "{expr}");
    let kept = ask(format!(
        "SELECT id FROM s.t WHERE ({expr}) IS NOT NULL ORDER BY id"
    ));
    let want: Vec<String> = plain
        .iter()
        .enumerate()
        .filter(|(_, x)| *x != "-")
        .map(|(i, _)| (i + 1).to_string())
        .collect();
    assert_eq!(kept, want, "{expr}");
    plain
}

/// An expression's answer over row `id` alone, or PostgreSQL's words for why not, each way.
fn at(path: &Path, id: i64, expr: &str) -> Result<String, String> {
    let mut snail = fresh(path);
    let mut ask = |q: String| {
        snail
            .ask(&q, None)
            .map(|(b, _)| rows(&b).join(","))
            .map_err(|e| e.to_string())
    };
    let plain = ask(format!(
        "SELECT ({expr})::text AS x FROM s.t WHERE id = {id}"
    ));
    let stepped = ask(format!(
        "WITH w AS (SELECT * FROM s.t) SELECT ({expr})::text AS x FROM w WHERE id = {id}"
    ));
    assert_eq!(plain, stepped, "{expr}");
    plain
}

fn strs(xs: &[&str]) -> Vec<String> {
    xs.iter().map(|s| s.to_string()).collect()
}

#[test]
fn a_prefix_operator_answers_as_its_function_in_each_type() {
    let path = snail("prefix");
    let abs = |c: &str| each_way(&path, &format!("@ {c}"));
    assert_eq!(each_way(&path, "@ i2b"), strs(&["-", "1", "15", "3", "1"]));
    assert_eq!(abs("i4b"), strs(&["-", "1", "31", "3", "1"]));
    assert_eq!(abs("f4"), strs(&["-", "1.5", "4", "NaN", "0"]));
    assert_eq!(abs("f8"), strs(&["-", "1.5", "16", "Infinity", "27"]));
    assert_eq!(abs("n"), strs(&["-", "1.5", "16", "NaN", "0"]));
    let plus = |c: &str| each_way(&path, &format!("+ {c}"));
    assert_eq!(plus("i2"), strs(&["-", "-32768", "32767", "6", "-7"]));
    assert_eq!(
        plus("i4"),
        strs(&["-", "-2147483648", "2147483647", "6", "-7"])
    );
    assert_eq!(
        plus("i8"),
        strs(&[
            "-",
            "-9223372036854775808",
            "9223372036854775807",
            "6",
            "-7"
        ])
    );
    assert_eq!(plus("f4"), strs(&["-", "-1.5", "4", "NaN", "0"]));
    assert_eq!(plus("f8"), strs(&["-", "-1.5", "16", "-Infinity", "27"]));
    assert_eq!(plus("n"), strs(&["-", "-1.5", "16", "NaN", "0"]));
    // Of double precision alone: a numeric's square root is no numeric's here.
    assert_eq!(at(&path, 3, "|/ n").unwrap(), "4");
    assert_eq!(at(&path, 3, "|/ f4").unwrap(), "2");
    assert_eq!(at(&path, 3, "|/ i2b + 1").unwrap(), "4");
    assert_eq!(at(&path, 5, "||/ f8").unwrap(), "3.0000000000000004");
    assert_eq!(at(&path, 4, "||/ f8").unwrap(), "-Infinity");
    assert_eq!(at(&path, 1, "|/ n").unwrap(), "-");
    assert_eq!(at(&path, 1, "||/ i4").unwrap(), "-");
    // `sqrt(16.0)` is a numeric of its scale; `|/` a double precision.
    assert_eq!(at(&path, 3, "sqrt(16.0)").unwrap(), "4.000000000000000");
    assert_eq!(at(&path, 3, "|/ 16.0").unwrap(), "4");
}

#[test]
fn an_integer_s_absolute_value_overflows_in_its_type_s_words() {
    let path = snail("abs_overflow");
    assert_eq!(at(&path, 2, "@ i2").unwrap_err(), "smallint out of range");
    assert_eq!(at(&path, 2, "@ i4").unwrap_err(), "integer out of range");
    assert_eq!(at(&path, 2, "@ i8").unwrap_err(), "bigint out of range");
    assert_eq!(at(&path, 3, "@ i2").unwrap(), "32767");
    assert_eq!(at(&path, 3, "@ i4").unwrap(), "2147483647");
    assert_eq!(at(&path, 3, "@ i8").unwrap(), "9223372036854775807");
    assert_eq!(at(&path, 5, "@ i2").unwrap(), "7");
    // A literal's own width: an integer's lowest, written with its minus.
    assert_eq!(
        at(&path, 1, "@ -2147483648").unwrap_err(),
        "integer out of range"
    );
    assert_eq!(at(&path, 1, "@ -2147483649").unwrap(), "2147483649");
}

#[test]
fn a_square_root_refuses_a_negative_and_a_power_its_domain_in_postgresql_s_words() {
    let path = snail("domains");
    assert_eq!(
        at(&path, 2, "|/ f8").unwrap_err(),
        "cannot take square root of a negative number"
    );
    assert_eq!(at(&path, 2, "||/ i2").map(|_| ()), Ok(()));
    assert_eq!(
        at(&path, 5, "n ^ -1").unwrap_err(),
        "zero raised to a negative power is undefined"
    );
    assert_eq!(
        at(&path, 5, "f4 ^ -1").unwrap_err(),
        "zero raised to a negative power is undefined"
    );
    assert_eq!(
        at(&path, 2, "n ^ 0.5").unwrap_err(),
        "a negative number raised to a non-integer power yields a complex result"
    );
    assert_eq!(
        at(&path, 2, "f8 ^ 0.5").unwrap_err(),
        "a negative number raised to a non-integer power yields a complex result"
    );
    assert_eq!(
        at(&path, 3, "f8 ^ 400").unwrap_err(),
        "value out of range: overflow"
    );
    assert_eq!(
        at(&path, 3, "f8 ^ -400").unwrap_err(),
        "value out of range: underflow"
    );
}

#[test]
fn a_power_takes_the_form_postgresql_resolves_double_precision_unless_a_numeric_stands_alone() {
    let path = snail("power");
    for (expr, want) in [
        ("2 ^ 3", "8"),
        ("2.0 ^ 3", "8.0000000000000000"),
        ("2 ^ 3.0", "8.0000000000000000"),
        ("2::real ^ 3", "8"),
        ("2::real ^ 3.0", "8"),
        ("2.0 ^ 3::real", "8"),
        ("2::int2 ^ 3::int8", "8"),
        ("5 ^ '2'", "25"),
        ("'5' ^ 2", "25"),
        ("'5' ^ '2'", "25"),
        ("5.0 ^ '2'", "25.000000000000000"),
        ("'NaN'::float8 ^ 0", "1"),
        ("1 ^ 'NaN'::numeric", "1"),
        ("2 ^ 3 ^ 2", "64"),
        ("2 * 3 ^ 2", "18"),
    ] {
        assert_eq!(at(&path, 1, expr).as_deref(), Ok(want), "{expr}");
    }
    assert_eq!(at(&path, 3, "f8 ^ i2b").unwrap(), "1.152921504606847e+18");
    assert_eq!(at(&path, 1, "f8 ^ 2").unwrap(), "-");
    assert_eq!(at(&path, 1, "2 ^ f8").unwrap(), "-");
    assert_eq!(at(&path, 1, "NULL ^ 2").unwrap(), "-");
}

#[test]
fn the_integers_bit_operators_answer_in_the_wider_width_either_way_round() {
    let path = snail("bits");
    for (op, f) in [
        ("&", (|a, b| a & b) as fn(i64, i64) -> i64),
        ("|", |a, b| a | b),
        ("#", |a, b| a ^ b),
    ] {
        for (a, b) in [
            ("i2", "i2b"),
            ("i2", "i4b"),
            ("i4", "i2b"),
            ("i4", "i4b"),
            ("i8", "i2b"),
            ("i8", "i4b"),
            ("i8", "i4"),
            ("i8", "i2"),
            ("i4", "i2"),
        ] {
            let there = each_way(&path, &format!("{a} {op} {b}"));
            let back = each_way(&path, &format!("{b} {op} {a}"));
            assert_eq!(there, back, "{a} {op} {b}");
            let value = |c: &str, r: usize| -> i64 {
                let at = ["i2", "i2b", "i4", "i4b", "i8"]
                    .iter()
                    .position(|x| *x == c)
                    .unwrap();
                ROWS[r][at + 1].unwrap().parse().unwrap()
            };
            let want: Vec<String> = (0..ROWS.len())
                .map(|r| match r {
                    0 => "-".to_string(),
                    r => f(value(a, r), value(b, r)).to_string(),
                })
                .collect();
            assert_eq!(there, want, "{a} {op} {b}");
        }
    }
    assert_eq!(at(&path, 1, "1 & NULL").unwrap(), "-");
    assert_eq!(at(&path, 1, "NULL # 1").unwrap(), "-");
    assert_eq!(at(&path, 1, "NULL::int8 | 1").unwrap(), "-");
    // A quoted literal takes the other side's type, read by its input.
    assert_eq!(at(&path, 1, "5 & '3'").unwrap(), "1");
    assert_eq!(at(&path, 1, "'3' & 5").unwrap(), "1");
    assert_eq!(at(&path, 1, "5::int8 # '3'").unwrap(), "6");
    assert_eq!(
        at(&path, 1, "5 | '99999999999'").unwrap_err(),
        "value \"99999999999\" is out of range for type integer"
    );
    assert_eq!(
        at(&path, 1, "5::int2 & '99999'").unwrap_err(),
        "value \"99999\" is out of range for type smallint"
    );
    assert_eq!(at(&path, 1, "5::int8 & '99999999999'").unwrap(), "5");
    // Its width is the wider's: a shift of it tells.
    assert_eq!(at(&path, 3, "(i2 & i4) << 1").unwrap(), "65534");
    assert_eq!(at(&path, 3, "(i2 & i2b) << 15").unwrap(), "-32768");
    assert_eq!(at(&path, 3, "(i2 | i8) >> 62").unwrap(), "1");
}

#[test]
fn a_shift_takes_its_count_as_the_machine_shifts_each_width() {
    let path = snail("shifts");
    for (expr, want) in [
        ("1::int2 << 15", "-32768"),
        ("1::int2 << 16", "0"),
        ("1::int2 << 32", "1"),
        ("1::int2 << -1", "0"),
        ("(-1)::int2 >> 1", "-1"),
        ("(-1)::int2 >> 40", "-1"),
        ("16384::int2 << 1", "-32768"),
        ("(-32768)::int2 >> 15", "-1"),
        ("1 << 31", "-2147483648"),
        ("1 << 32", "1"),
        ("1 << 33", "2"),
        ("1 << -1", "-2147483648"),
        ("-8 >> 1", "-4"),
        ("-8 >> 33", "-4"),
        ("1 >> 32", "1"),
        ("1::int8 << 63", "-9223372036854775808"),
        ("1::int8 << 64", "1"),
        ("1::int8 << -1", "-9223372036854775808"),
        ("(-8)::int8 >> 1", "-4"),
        ("5000000000 << 1", "10000000000"),
        ("5000000000 >> 1", "2500000000"),
        ("1 << 1::int2", "2"),
        ("1::int2 >> 1::int2", "0"),
        ("5 << '3'", "40"),
        ("'5' << 3", "40"),
        ("'5' >> 1", "2"),
        ("5::int2 << '3'", "40"),
    ] {
        assert_eq!(at(&path, 1, expr).as_deref(), Ok(want), "{expr}");
    }
    assert_eq!(at(&path, 3, "i2 << i2b").unwrap(), "-32768");
    assert_eq!(at(&path, 3, "i4 << i4b").unwrap(), "-2147483648");
    assert_eq!(at(&path, 3, "i8 >> i4b").unwrap(), "4294967295");
    assert_eq!(at(&path, 3, "i4 >> i2b").unwrap(), "65535");
    assert_eq!(at(&path, 1, "i4 << 1").unwrap(), "-");
    assert_eq!(at(&path, 1, "1 << i4b").unwrap(), "-");
    assert_eq!(at(&path, 1, "1 << NULL").unwrap(), "-");
    assert_eq!(
        at(&path, 1, "5 << '99999999999'").unwrap_err(),
        "value \"99999999999\" is out of range for type integer"
    );
}

#[test]
fn an_integer_s_complement_keeps_its_type_a_literal_of_no_type_is_not_unique() {
    let path = snail("not");
    assert_eq!(
        each_way(&path, "~ i2"),
        strs(&["-", "32767", "-32768", "-7", "6"])
    );
    assert_eq!(
        each_way(&path, "~ i4"),
        strs(&["-", "2147483647", "-2147483648", "-7", "6"])
    );
    assert_eq!(
        each_way(&path, "~ i8"),
        strs(&[
            "-",
            "9223372036854775807",
            "-9223372036854775808",
            "-7",
            "6"
        ])
    );
    assert_eq!(
        at(&path, 1, "~ '5'").unwrap_err(),
        "operator is not unique: ~ unknown"
    );
    assert_eq!(
        at(&path, 1, "~ NULL").unwrap_err(),
        "operator is not unique: ~ unknown"
    );
    assert_eq!(at(&path, 1, "~ b'101'").unwrap(), "010");
    assert_eq!(
        at(&path, 1, "~ '1.2.3.4'::inet").unwrap(),
        "254.253.252.251/32"
    );
}

#[test]
fn an_operator_a_type_has_no_form_of_is_refused_before_any_row_in_postgresql_s_words() {
    let path = snail("refused");
    let mut snail = fresh(&path);
    for (expr, words) in [
        ("1 & 1.5", "operator does not exist: integer & numeric"),
        ("1.5 & 1", "operator does not exist: numeric & integer"),
        (
            "f8 & 1",
            "operator does not exist: double precision & integer",
        ),
        ("1 | f4", "operator does not exist: integer | real"),
        ("n # 1", "operator does not exist: numeric # integer"),
        ("t & 1", "operator does not exist: text & integer"),
        ("'5' & '3'", "operator is not unique: unknown & unknown"),
        ("'5' # '3'", "operator is not unique: unknown # unknown"),
        ("i8 << i8", "operator does not exist: bigint << bigint"),
        ("1 << 1::int8", "operator does not exist: integer << bigint"),
        ("1 << 1.5", "operator does not exist: integer << numeric"),
        ("1.5 << 1", "operator does not exist: numeric << integer"),
        ("'5' << '3'", "operator is not unique: unknown << unknown"),
        ("~ 1.5", "operator does not exist: ~ numeric"),
        ("~ f8", "operator does not exist: ~ double precision"),
        ("~ t", "operator does not exist: ~ text"),
        ("@ t", "operator does not exist: @ text"),
        ("@ true", "operator does not exist: @ boolean"),
        ("+ t", "operator does not exist: + text"),
        ("|/ t", "operator does not exist: |/ text"),
        ("||/ true", "operator does not exist: ||/ boolean"),
        ("t ^ 2", "operator does not exist: text ^ integer"),
        ("2 ^ true", "operator does not exist: integer ^ boolean"),
        ("1 ^@ 'a'", "operator does not exist: integer ^@ unknown"),
        ("t ^@ 1", "operator does not exist: text ^@ integer"),
        ("b ^@ 'a'", "operator does not exist: bytea ^@ unknown"),
        ("1 ~<~ 2", "operator does not exist: integer ~<~ integer"),
        ("b ~>=~ 'a'", "operator does not exist: bytea ~>=~ unknown"),
        ("t ~<=~ 1", "operator does not exist: text ~<=~ integer"),
        ("1 ~~ 'a'", "operator does not exist: integer ~~ unknown"),
        (
            "1 !~~* 'a'",
            "operator does not exist: integer !~~* unknown",
        ),
        ("b ~~* 'a'", "operator does not exist: bytea ~~* unknown"),
        ("j ?| k", "operator does not exist: json ?| text[]"),
        ("j - k", "operator does not exist: json - text[]"),
        ("j #- k", "operator does not exist: json #- text[]"),
        ("t #> k", "operator does not exist: text #> text[]"),
        ("jb ?& t", "operator does not exist: jsonb ?& text"),
    ] {
        for q in [
            format!("SELECT {expr} AS x FROM s.t WHERE id < 0"),
            format!("WITH w AS (SELECT * FROM s.t) SELECT {expr} AS x FROM w WHERE id < 0"),
            format!("SELECT id FROM s.t WHERE id < 0 AND ({expr}) IS NULL"),
        ] {
            let e = snail.ask(&q, None).unwrap_err().to_string();
            assert_eq!(e, words, "{q}");
        }
    }
}

#[test]
fn the_grammar_binds_as_postgresql_s_a_prefix_operator_over_a_sum_a_sign_within_a_power() {
    let path = snail("bound");
    for (expr, want) in [
        ("@ -5 + 3", "2"),
        ("(@ -5) + 3", "8"),
        ("@ (-5) + 3", "2"),
        ("|/ 16 + 9", "5"),
        ("||/ 8 + 19", "3.0000000000000004"),
        ("~ 1 + 1", "-3"),
        ("1 + @ -2 + -3", "6"),
        ("@ -5 * 2", "10"),
        ("@ -5", "5"),
        ("- -5", "5"),
        ("~ -1", "0"),
        ("|/ +4", "2"),
        ("@ @ -5", "5"),
        ("~ 1 & 3", "2"),
        ("~ 1 * 3", "-4"),
        ("1 & ~ 1 + 1", "1"),
        ("-2 ^ 2", "4"),
        ("- 2.0 ^ 2", "4.0000000000000000"),
        ("-(2 ^ 2)", "-4"),
        ("(-2) ^ 2", "4"),
        ("+ 2 ^ 2", "4"),
        ("2 ^ -2", "0.25"),
        ("1 << 2 + 1", "8"),
        ("1 | 2 & 3", "3"),
        ("1 | 2 # 3", "0"),
        ("5 # 1 << 2", "16"),
        ("2 & 3 = 2", "true"),
        ("2 = 3 & 2", "true"),
        ("'a' ~~ 'a' || 'b'", "trueb"),
        ("'a' ~ 'a' || 'b'", "trueb"),
        ("true = 'a' ~~ 'a'", "true"),
        ("true = 'a' ~ 'a'", "true"),
        ("'a' ~~ 'a' = true", "true"),
        ("'a' !~~ 'a' IS NULL", "false"),
        ("NOT 'a' ~~ 'b'", "true"),
    ] {
        assert_eq!(at(&path, 1, expr).as_deref(), Ok(want), "{expr}");
    }
}

#[test]
fn an_operator_of_like_reads_as_like_on_each_string_and_on_bytea() {
    let path = snail("like");
    // LIKE by its operator, and by its words.
    for (op, words) in [
        ("~~", "LIKE"),
        ("!~~", "NOT LIKE"),
        ("~~*", "ILIKE"),
        ("!~~*", "NOT ILIKE"),
    ] {
        for (x, p) in [
            ("t", "tb"),
            ("t", "'a%'"),
            ("c", "'b%'"),
            ("c", "'b'"),
            ("c", "'b '"),
            ("nm", "'A%'"),
            ("nm", "tb"),
            ("t", "NULL"),
        ] {
            let by_op = each_way(&path, &format!("{x} {op} {p}"));
            let by_words = each_way(&path, &format!("{x} {words} {p}"));
            assert_eq!(by_op, by_words, "{x} {op} {p}");
        }
    }
    assert_eq!(
        each_way(&path, "t ~~ tb"),
        strs(&["-", "false", "true", "false", "true"])
    );
    assert_eq!(
        each_way(&path, "t !~~ tb"),
        strs(&["-", "true", "false", "true", "false"])
    );
    assert_eq!(
        each_way(&path, "t ~~* 'A%'"),
        strs(&["-", "true", "false", "true", "false"])
    );
    assert_eq!(
        each_way(&path, "t !~~* 'A%'"),
        strs(&["-", "false", "true", "false", "true"])
    );
    assert_eq!(
        each_way(&path, "c ~~ 'b'"),
        strs(&["-", "false", "false", "false", "false"])
    );
    assert_eq!(
        each_way(&path, "c ~~ 'b   '"),
        strs(&["-", "false", "true", "false", "false"])
    );
    assert_eq!(
        each_way(&path, "nm ~~ 'b'"),
        strs(&["-", "false", "true", "false", "false"])
    );
    assert_eq!(
        each_way(&path, "b ~~ bb"),
        strs(&["-", "true", "true", "true", "true"])
    );
    assert_eq!(
        each_way(&path, "b !~~ 'a%'"),
        strs(&["-", "false", "true", "true", "true"])
    );
    assert_eq!(
        at(&path, 1, "'ab' ~~ 'a\\'").unwrap_err(),
        "LIKE pattern must not end with escape character"
    );
}

#[test]
fn a_prefix_operator_of_text_answers_as_starts_with_a_bpchar_without_its_blanks() {
    let path = snail("starts");
    assert_eq!(
        each_way(&path, "t ^@ 'a'"),
        strs(&["-", "true", "false", "false", "false"])
    );
    assert_eq!(
        each_way(&path, "t ^@ tb"),
        strs(&["-", "true", "false", "false", "true"])
    );
    assert_eq!(
        each_way(&path, "t ^@ ''"),
        strs(&["-", "true", "true", "true", "true"])
    );
    assert_eq!(
        each_way(&path, "t ^@ NULL"),
        strs(&["-", "-", "-", "-", "-"])
    );
    assert_eq!(
        each_way(&path, "NULL ^@ t"),
        strs(&["-", "-", "-", "-", "-"])
    );
    assert_eq!(
        each_way(&path, "c ^@ 'b'"),
        strs(&["-", "false", "true", "false", "false"])
    );
    assert_eq!(
        each_way(&path, "c ^@ 'b '"),
        strs(&["-", "false", "false", "false", "false"])
    );
    assert_eq!(
        each_way(&path, "nm ^@ 'A'"),
        strs(&["-", "false", "false", "true", "false"])
    );
    assert_eq!(
        at(&path, 1, "'a'::varchar ^@ 'a'::varchar").unwrap(),
        "true"
    );
    assert_eq!(at(&path, 1, "'a'::text ^@ 'a'::name").unwrap(), "true");
}

#[test]
fn a_pattern_comparison_reads_bytes_a_string_before_what_it_begins_a_bpchar_without_its_blanks() {
    let path = snail("comparisons");
    for (expr, want) in [
        ("'a' ~<~ 'b'", "true"),
        ("'a' ~<~ 'a'", "false"),
        ("'a' ~<=~ 'a'", "true"),
        ("'a' ~>=~ 'a'", "true"),
        ("'a' ~>~ 'a'", "false"),
        ("'a' ~<~ 'a '", "true"),
        ("'a' ~>~ 'A'", "true"),
        ("'é' ~>~ 'z'", "true"),
        ("'a '::char(3) ~<~ 'a'::char(3)", "false"),
        ("'a '::char(3) ~<=~ 'a'::char(3)", "true"),
        ("'a'::char(3) ~>=~ 'a  '::char(3)", "true"),
        ("'a'::char(3) ~<~ 'a  x'::char(5)", "true"),
        ("'a'::char(3) ~<~ 'a '", "false"),
        ("'a'::char(3) ~<~ 'a '::text", "true"),
        ("'a '::text ~>~ 'a'::char(3)", "true"),
        ("'a'::name ~<~ 'b'", "true"),
        ("'a'::name ~<~ 'b'::name", "true"),
        ("NULL ~<~ 'a'", "-"),
        ("'a' ~<~ NULL", "-"),
        ("NULL ~<~ NULL", "-"),
    ] {
        assert_eq!(at(&path, 1, expr).as_deref(), Ok(want), "{expr}");
    }
    for (a, op, b) in [
        ("t", "~<~", "tb"),
        ("t", "~<=~", "tb"),
        ("c", "~<~", "'b'"),
        ("c", "~>=~", "'b '"),
        ("c", "~>=~", "'b '::text"),
        ("nm", "~>~", "'A'"),
    ] {
        let flipped = match op {
            "~<~" => "~>~",
            "~<=~" => "~>=~",
            "~>=~" => "~<=~",
            _ => "~<~",
        };
        assert_eq!(
            each_way(&path, &format!("{a} {op} {b}")),
            each_way(&path, &format!("{b} {flipped} {a}")),
            "{a} {op} {b}"
        );
    }
    assert_eq!(
        each_way(&path, "t ~<~ tb"),
        strs(&["-", "false", "true", "true", "false"])
    );
    assert_eq!(
        each_way(&path, "c ~>=~ 'b '"),
        strs(&["-", "false", "true", "false", "false"])
    );
    assert_eq!(
        each_way(&path, "c ~>~ 'b '::text"),
        strs(&["-", "false", "false", "false", "false"])
    );
}

#[test]
fn a_document_s_operator_takes_a_text_array_column_on_its_right_as_a_literal_and_an_array_are() {
    let path = snail("json");
    let path_of = strs(&["-", "1", "2", "1", "-"]);
    assert_eq!(each_way(&path, "jb #> k"), path_of);
    assert_eq!(each_way(&path, "j #> k"), path_of);
    assert_eq!(each_way(&path, "jb #>> k"), path_of);
    assert_eq!(each_way(&path, "j #>> k"), path_of);
    for (id, want) in [
        (2, "{\"a\": {}, \"c\": 2}"),
        (3, "[1]"),
        (4, "{}"),
        (1, "-"),
    ] {
        assert_eq!(at(&path, id, "jb #- k").as_deref(), Ok(want), "{id}");
    }
    assert_eq!(
        at(&path, 5, "jb #- k").unwrap_err(),
        "cannot delete path in scalar"
    );
    for (id, want) in [(2, "{\"c\": 2}"), (3, "[1, 2]"), (4, "{}"), (1, "-")] {
        assert_eq!(at(&path, id, "jb - k").as_deref(), Ok(want), "{id}");
    }
    assert_eq!(
        at(&path, 5, "jb - k").unwrap_err(),
        "cannot delete from scalar"
    );
    assert_eq!(
        each_way(&path, "jb ?| k"),
        strs(&["-", "true", "false", "true", "false"])
    );
    assert_eq!(
        each_way(&path, "jb ?& k"),
        strs(&["-", "false", "false", "true", "false"])
    );
    // The same, by a literal and by ARRAY[…] on the right.
    for (expr, lit) in [
        ("jb #> '{a,b}'", "jb #> ARRAY['a','b']"),
        ("jb ?| '{a,x}'", "jb ?| ARRAY['a','x']"),
        ("jb ?& '{a,c}'", "jb ?& ARRAY['a','c']"),
        ("j #>> '{a,b}'", "j #>> ARRAY['a','b']"),
    ] {
        assert_eq!(each_way(&path, expr), each_way(&path, lit), "{expr}");
    }
    assert_eq!(
        at(&path, 2, "jb #- '{a,b}'"),
        at(&path, 2, "jb #- ARRAY['a','b']")
    );
    assert_eq!(
        at(&path, 2, "jb - '{a,c}'::text[]"),
        at(&path, 2, "jb - ARRAY['a','c']")
    );
    assert_eq!(
        at(&path, 3, "jb #- '{a,b}'").unwrap_err(),
        "path element at position 1 is not an integer: \"a\""
    );
    assert_eq!(
        at(&path, 1, "'{\"a\":1}'::jsonb - '{a}'").unwrap(),
        "{\"a\": 1}"
    );
    // A NULL among the keys: no answer of a path, none of its keys; refused where a path is deleted.
    assert_eq!(at(&path, 5, "jb #> k").unwrap(), "-");
    assert_eq!(at(&path, 5, "jb ?| k").unwrap(), "false");
    assert_eq!(
        at(&path, 2, "jb #- ARRAY['a', NULL]").unwrap_err(),
        "path element at position 2 is null"
    );
    // Of two dimensions: read flat, but where a path or keys are deleted.
    assert_eq!(at(&path, 2, "jb ?| ARRAY[['a'], ['x']]").unwrap(), "true");
    assert_eq!(at(&path, 2, "jb #> ARRAY[['a'], ['b']]").unwrap(), "1");
    assert_eq!(
        at(&path, 2, "jb #- ARRAY[['a'], ['b']]").unwrap_err(),
        "wrong number of array subscripts"
    );
    assert_eq!(
        at(&path, 2, "jb - ARRAY[['a'], ['b']]").unwrap_err(),
        "wrong number of array subscripts"
    );
}

#[test]
fn a_where_of_text_array_literal_is_read_as_one_along_any_table() {
    let path = snail("where_literal");
    let mut snail = fresh(&path);
    let mut ask = |q: &str| rows(&snail.ask(q, None).unwrap_or_else(|e| panic!("{q}: {e}")).0);
    assert_eq!(
        ask("SELECT id FROM s.t WHERE jb ?| '{a}' ORDER BY id"),
        strs(&["2"])
    );
    assert_eq!(
        ask("SELECT id FROM s.t WHERE jb ?| '{a,c}' ORDER BY id"),
        strs(&["2", "4"])
    );
    assert_eq!(
        ask("SELECT id FROM s.t WHERE jb ?& '{a,c}' ORDER BY id"),
        strs(&["2"])
    );
    assert_eq!(
        ask("SELECT id FROM s.t WHERE jb ?| k ORDER BY id"),
        strs(&["2", "4"])
    );
    assert_eq!(
        ask("SELECT id FROM s.t WHERE NOT jb ?| k ORDER BY id"),
        strs(&["3", "5"])
    );
}

#[test]
fn an_operator_reads_in_every_clause_of_a_question_and_a_join() {
    let path = snail("clauses");
    let mut snail = fresh(&path);
    let mut ask = |q: &str| rows(&snail.ask(q, None).unwrap_or_else(|e| panic!("{q}: {e}")).0);
    assert_eq!(
        ask("SELECT id & 1 AS odd, count(*) AS n FROM s.t GROUP BY id & 1 ORDER BY id & 1"),
        strs(&["0|2", "1|3"])
    );
    assert_eq!(
        ask("SELECT id FROM s.t ORDER BY id # 1 DESC, id"),
        strs(&["4", "5", "2", "3", "1"])
    );
    assert_eq!(
        ask("SELECT id & 1 AS odd FROM s.t GROUP BY id & 1 HAVING count(*) > 2"),
        strs(&["1"])
    );
    assert_eq!(
        ask("SELECT count(*) FILTER (WHERE id & 1 = 1) AS n FROM s.t"),
        strs(&["3"])
    );
    assert_eq!(
        ask("SELECT sum(id << 1) AS n, sum(@ i2b) AS m FROM s.t"),
        strs(&["30|20"])
    );
    assert_eq!(
        ask("SELECT a.id FROM s.t a JOIN s.t b ON a.id & 1 = b.id AND b.id = 1 ORDER BY a.id"),
        strs(&["1", "3", "5"])
    );
    assert_eq!(
        ask("SELECT a.id, b.id FROM s.t a JOIN s.t b ON a.id = b.id WHERE (a.id & b.id) = 4"),
        strs(&["4|4"])
    );
    assert_eq!(
        ask("SELECT a.id & b.id AS x FROM s.t a JOIN s.t b ON a.id = b.id WHERE a.id < 3 ORDER BY 1"),
        strs(&["1", "2"])
    );
    assert_eq!(
        ask("SELECT id FROM s.t WHERE id IN (SELECT id FROM s.t WHERE t ^@ 'a') ORDER BY id"),
        strs(&["2"])
    );
    assert_eq!(
        ask("SELECT id FROM s.t WHERE t ^@ 'a' UNION ALL SELECT id FROM s.t WHERE c ~<~ 'A%' ORDER BY 1"),
        strs(&["2", "5"])
    );
    assert_eq!(
        ask("SELECT CASE WHEN id & 1 = 1 THEN 'odd' ELSE 'even' END AS x FROM s.t WHERE id = 2"),
        strs(&["even"])
    );
    assert_eq!(
        ask("SELECT i4 & 3 AS x FROM s.t WHERE id = $1 LIMIT 1"
            .replace("$1", "4")
            .as_str()),
        strs(&["2"])
    );
    let (b, _) = snail
        .ask(
            "SELECT id & 1, @ id, + id, ~ id, t ^@ 'a', t ~<~ 'a', id ^ 2 FROM s.t WHERE id = 1",
            None,
        )
        .unwrap();
    for f in b.schema().fields() {
        assert_eq!(f.name(), "?column?");
    }
}

/// A snail of `s.k`, 30,000 rows, whose text and bpchar columns are each read along an index of
/// its pattern's own operator class.
fn keyed(name: &str) -> PathBuf {
    let path = tmp(&format!("ops-{name}.snail"));
    let catalog = Catalog::parse(
        "CREATE TABLE s.k (id bigint PRIMARY KEY, s text, c char(8), m name, d bytea, pad bigint);
         CREATE INDEX k_s ON s.k (s text_pattern_ops);
         CREATE INDEX k_c ON s.k (c bpchar_pattern_ops);
         CREATE INDEX k_m ON s.k (m);
         CREATE INDEX k_d ON s.k (d);",
    )
    .unwrap();
    let t = catalog.table("s.k").unwrap();
    let columns = t
        .columns
        .iter()
        .enumerate()
        .map(|(c, col)| {
            let mut v = Values::new(col.ty);
            for i in 0..30_000i64 {
                let text = match c {
                    0 | 5 => i.to_string(),
                    1 => format!("p{:04}", i * 7 % 3_000),
                    2 => format!("q{:04}", i * 11 % 3_000),
                    3 => format!("r{:04}", i * 13 % 3_000),
                    _ => format!("\\x71{:06x}", i * 5 % 4_096),
                };
                push_datum(
                    &mut v,
                    &warren_snail::composed::scalar_in(col.ty, &text).unwrap(),
                );
            }
            v
        })
        .collect();
    write(&path, &catalog, &[("s.k".into(), Rows { columns })]).unwrap();
    path
}

/// A question's ids, sorted, and its round, its pages exact, along the order named.
fn asked(path: &Path, q: &str, order: Option<&str>) -> (Vec<i64>, Round) {
    let (b, round) = fresh(path)
        .ask(q, order)
        .unwrap_or_else(|e| panic!("{q}: {e}"));
    exact(q, &round);
    let mut ids: Vec<i64> = rows(&b).iter().map(|r| r.parse().unwrap()).collect();
    ids.sort_unstable();
    (ids, round)
}

fn kept_ids(keep: impl Fn(i64) -> bool) -> Vec<i64> {
    (0..30_000).filter(|&i| keep(i)).collect()
}

/// The pages of a read of the whole of an order: the same rows by a pattern narrowing nothing.
fn whole(path: &Path, column: &str, order: &str) -> Round {
    asked(
        path,
        &format!("SELECT id FROM s.k WHERE {column} ~~* '%'"),
        Some(order),
    )
    .1
}

#[test]
fn a_fixed_prefix_narrows_the_walk_by_each_operator_that_has_one_its_pages_exact() {
    let path = keyed("narrow_prefix");
    let s = |i: i64| format!("p{:04}", i * 7 % 3_000);
    let want = kept_ids(|i| s(i).starts_with("p012"));
    let all = whole(&path, "s", "k_s");
    let range = asked(
        &path,
        "SELECT id FROM s.k WHERE s >= 'p012' AND s < 'p013'",
        Some("k_s"),
    );
    for q in [
        "SELECT id FROM s.k WHERE s ^@ 'p012'",
        "SELECT id FROM s.k WHERE (s ^@ 'p012')",
        "SELECT id FROM s.k WHERE s ~~ 'p012%'",
    ] {
        let (a, r) = asked(&path, q, Some("k_s"));
        assert_eq!(a, want, "{q}");
        assert_eq!(
            (r.blocks_read, r.directory_pages, r.column_pages_read),
            (
                range.1.blocks_read,
                range.1.directory_pages,
                range.1.column_pages_read
            ),
            "{q}: {r:?}"
        );
        assert!(
            r.column_pages_read * 8 < all.column_pages_read,
            "{q}: {r:?}"
        );
    }
    // Left to itself it takes the order the prefix narrows.
    let (a, r) = asked(&path, "SELECT id FROM s.k WHERE s ^@ 'p012'", None);
    assert_eq!(a, want);
    assert_eq!(r.order, "k_s");
    // On a bpchar, its keys without their blanks; on a name; a bytea's by LIKE alone.
    let c = |i: i64| format!("q{:04}", i * 11 % 3_000);
    let (a, r) = asked(&path, "SELECT id FROM s.k WHERE c ^@ 'q012'", Some("k_c"));
    assert_eq!(a, kept_ids(|i| c(i).starts_with("q012")));
    assert!(
        r.column_pages_read * 8 < whole(&path, "c", "k_c").column_pages_read,
        "{r:?}"
    );
    let (a, r) = asked(&path, "SELECT id FROM s.k WHERE c ~~ 'q012%'", Some("k_c"));
    assert_eq!(a, kept_ids(|i| c(i).starts_with("q012")));
    assert!(
        r.column_pages_read * 8 < whole(&path, "c", "k_c").column_pages_read,
        "{r:?}"
    );
    // `c ^@ 'q012 '`: the blank no key's text begins with narrows to no row.
    let (a, _) = asked(&path, "SELECT id FROM s.k WHERE c ^@ 'q012 '", Some("k_c"));
    assert!(a.is_empty());
    let m = |i: i64| format!("r{:04}", i * 13 % 3_000);
    let (a, r) = asked(&path, "SELECT id FROM s.k WHERE m ^@ 'r012'", Some("k_m"));
    assert_eq!(a, kept_ids(|i| m(i).starts_with("r012")));
    assert!(
        r.column_pages_read * 8 < whole(&path, "m", "k_m").column_pages_read,
        "{r:?}"
    );
    let (a, r) = asked(&path, "SELECT id FROM s.k WHERE m ~~ 'r012%'", Some("k_m"));
    assert_eq!(a, kept_ids(|i| m(i).starts_with("r012")));
    assert!(
        r.column_pages_read * 8 < whole(&path, "m", "k_m").column_pages_read,
        "{r:?}"
    );
    let (a, r) = asked(
        &path,
        "SELECT id FROM s.k WHERE d ~~ 'q\\000\\000%'",
        Some("k_d"),
    );
    assert_eq!(a, kept_ids(|i| i * 5 % 4_096 < 256));
    let (all_b, wb) = asked(&path, "SELECT id FROM s.k WHERE d LIKE '%'", Some("k_d"));
    assert_eq!(all_b.len(), 30_000);
    assert!(r.column_pages_read * 4 < wb.column_pages_read, "{r:?}");
    // No prefix, or a negation: the order read whole, the same rows.
    let (a, r) = asked(&path, "SELECT id FROM s.k WHERE s ^@ ''", Some("k_s"));
    assert_eq!(a.len(), 30_000);
    assert_eq!(r.column_pages_read, all.column_pages_read);
    let (a, r) = asked(&path, "SELECT id FROM s.k WHERE s !~~ 'p012%'", Some("k_s"));
    assert_eq!(a, kept_ids(|i| !s(i).starts_with("p012")));
    assert_eq!(r.column_pages_read, all.column_pages_read);
    // A NULL pattern keeps no row, and reads none.
    let (a, r) = asked(&path, "SELECT id FROM s.k WHERE s ^@ NULL", None);
    assert!(a.is_empty());
    assert_eq!(r.column_pages_read, 0);
}

#[test]
fn a_pattern_comparison_narrows_the_walk_along_a_strand_of_its_operator_class_its_pages_exact() {
    let path = keyed("narrow_compare");
    let s = |i: i64| format!("p{:04}", i * 7 % 3_000);
    let all = whole(&path, "s", "k_s");
    for (q, written, keep) in [
        (
            "SELECT id FROM s.k WHERE s ~<~ 'p0003'",
            "SELECT id FROM s.k WHERE s < 'p0003'",
            (|x: &str| x < "p0003") as fn(&str) -> bool,
        ),
        (
            "SELECT id FROM s.k WHERE s ~<=~ 'p0003'",
            "SELECT id FROM s.k WHERE s <= 'p0003'",
            |x| x <= "p0003",
        ),
        (
            "SELECT id FROM s.k WHERE s ~>~ 'p2997'",
            "SELECT id FROM s.k WHERE s > 'p2997'",
            |x| x > "p2997",
        ),
        (
            "SELECT id FROM s.k WHERE s ~>=~ 'p2997'",
            "SELECT id FROM s.k WHERE s >= 'p2997'",
            |x| x >= "p2997",
        ),
        (
            "SELECT id FROM s.k WHERE 'p0003' ~>~ s",
            "SELECT id FROM s.k WHERE s < 'p0003'",
            |x| x < "p0003",
        ),
        (
            "SELECT id FROM s.k WHERE 'p2997' ~<=~ s",
            "SELECT id FROM s.k WHERE s >= 'p2997'",
            |x| x >= "p2997",
        ),
        (
            "SELECT id FROM s.k WHERE s ~>=~ 'p0100' AND s ~<~ 'p0120'",
            "SELECT id FROM s.k WHERE s >= 'p0100' AND s < 'p0120'",
            |x| ("p0100".."p0120").contains(&x),
        ),
    ] {
        let (a, r) = asked(&path, q, Some("k_s"));
        assert_eq!(a, kept_ids(|i| keep(&s(i))), "{q}");
        let w = asked(&path, written, Some("k_s")).1;
        assert_eq!(
            (r.blocks_read, r.directory_pages, r.column_pages_read),
            (w.blocks_read, w.directory_pages, w.column_pages_read),
            "{q}: {r:?}"
        );
        assert!(
            r.column_pages_read * 8 < all.column_pages_read,
            "{q}: {r:?}"
        );
        let (a, r) = asked(&path, q, None);
        assert_eq!(a, kept_ids(|i| keep(&s(i))), "{q}");
        assert_eq!(r.order, "k_s", "{q}");
    }
    // On a bpchar along its own class, a literal without its blanks.
    let c = |i: i64| format!("q{:04}", i * 11 % 3_000);
    let call = whole(&path, "c", "k_c");
    for q in [
        "SELECT id FROM s.k WHERE c ~<~ 'q0003'",
        "SELECT id FROM s.k WHERE c ~<~ 'q0003   '",
        "SELECT id FROM s.k WHERE 'q0003 ' ~>~ c",
    ] {
        let (a, r) = asked(&path, q, Some("k_c"));
        assert_eq!(a, kept_ids(|i| c(i).as_str() < "q0003"), "{q}");
        assert!(
            r.column_pages_read * 8 < call.column_pages_read,
            "{q}: {r:?}"
        );
    }
    // A text literal keeps its blanks; a cast is read a row at a time, along the table's own order.
    let (a, _) = asked(
        &path,
        "SELECT id FROM s.k WHERE c ~<=~ 'q0003 '::text",
        None,
    );
    assert_eq!(a, kept_ids(|i| c(i).as_str() <= "q0003 "));
    // Beside a name, of no literal's truncation.
    let m = |i: i64| format!("r{:04}", i * 13 % 3_000);
    let (a, _) = asked(&path, "SELECT id FROM s.k WHERE m ~<~ 'r0003'", None);
    assert_eq!(a, kept_ids(|i| m(i).as_str() < "r0003"));
    // A NULL keeps no row, and reads none.
    let (a, r) = asked(&path, "SELECT id FROM s.k WHERE s ~<~ NULL", None);
    assert!(a.is_empty());
    assert_eq!(r.column_pages_read, 0);
}

#[test]
#[cfg(feature = "standalone")]
fn an_operator_reads_in_a_write_s_conditions_and_values_a_check_and_a_generated_column() {
    use warren_snail::read::Snail;
    let path = tmp("ops-writes.snail");
    let catalog = Catalog::parse(
        "CREATE TABLE s.w (id integer PRIMARY KEY, n integer, t text, d integer DEFAULT 1 << 3, \
         g integer GENERATED ALWAYS AS (n & 1) STORED, CHECK (@ n - 1 >= 0));",
    )
    .unwrap();
    let empty = Rows {
        columns: catalog.tables[0]
            .columns
            .iter()
            .map(|c| Values::new(c.ty))
            .collect(),
    };
    write(&path, &catalog, &[("s.w".into(), empty)]).unwrap();
    let mut snail = Snail::open_writing(&path).unwrap();
    snail
        .execute("INSERT INTO s.w (id, n, t) VALUES (1, 1, 'ab'), (2, 2, 'ba'), (3, 7, 'ac')")
        .unwrap();
    // `@ n - 1` is `@ (n - 1)`: never below 0, where `(@ n) - 1` would refuse this row.
    snail
        .execute("INSERT INTO s.w (id, n, t) VALUES (4, 0, 'x')")
        .unwrap();
    snail
        .execute("UPDATE s.w SET n = n # 8 WHERE t ^@ 'a' AND n & 1 = 1")
        .unwrap();
    snail.execute("DELETE FROM s.w WHERE t ~~ 'b%'").unwrap();
    let (b, _) = snail
        .ask("SELECT id, n, d, g FROM s.w ORDER BY id", None)
        .unwrap();
    assert_eq!(rows(&b), strs(&["1|9|8|1", "3|15|8|1", "4|0|8|0"]));
    let (b, _) = snail
        .ask(
            "WITH w AS (SELECT @ -n + 1 AS x, id FROM s.w) SELECT x FROM w ORDER BY id",
            None,
        )
        .unwrap();
    assert_eq!(rows(&b), strs(&["8", "14", "1"]));
}

/// A snail of `s.d`, written recording `config` where it names one, its table's rows the words.
fn searched(name: &str, config: Option<&str>) -> PathBuf {
    let path = tmp(&format!("ops-{name}.snail"));
    let catalog = Catalog::parse(
        "CREATE TABLE s.d (id integer PRIMARY KEY, t text, tb text, c char(12), v varchar(12), \
         nm name, q tsquery);",
    )
    .unwrap();
    let rows: [[Option<&str>; 7]; 4] = [
        [Some("1"), None, None, None, None, None, None],
        [
            Some("2"),
            Some("The Fat Rats"),
            Some("fat rats"),
            Some("Fat Rats"),
            Some("Fat Rats"),
            Some("Fat Rats"),
            Some("fat & rat"),
        ],
        [
            Some("3"),
            Some("the"),
            Some("the"),
            Some("the"),
            Some("the"),
            Some("the"),
            Some("the"),
        ],
        [
            Some("4"),
            Some("stars"),
            Some("star"),
            Some("star"),
            Some("star"),
            Some("star"),
            Some("star"),
        ],
    ];
    let t = catalog.table("s.d").unwrap();
    let columns = t
        .columns
        .iter()
        .enumerate()
        .map(|(c, col)| {
            let mut v = Values::new(col.ty);
            for row in &rows {
                match row[c] {
                    Some(text) => push_datum(
                        &mut v,
                        &warren_snail::composed::scalar_in(col.ty, text).unwrap(),
                    ),
                    None => v.push_null(),
                }
            }
            v
        })
        .collect();
    let mut carried = warren_snail::write::Carried::default();
    carried.text_search = config.unwrap_or_default().to_string();
    warren_snail::write::write_carried(
        &path,
        &catalog,
        &[("s.d".into(), Rows { columns })],
        &[],
        weather::span(),
        19,
        &carried,
    )
    .unwrap();
    path
}

/// What each way of asking gives of an expression over `s.d`, by id, as text.
fn over_words(path: &Path, expr: &str) -> Vec<String> {
    let mut snail = fresh(path);
    let mut ask = |q: String| {
        let (b, _) = snail.ask(&q, None).unwrap_or_else(|e| panic!("{q}: {e}"));
        rows(&b)
    };
    let plain = ask(format!("SELECT ({expr})::text AS x FROM s.d ORDER BY id"));
    let stepped = ask(format!(
        "WITH w AS (SELECT * FROM s.d) SELECT ({expr})::text AS x FROM w ORDER BY id"
    ));
    assert_eq!(plain, stepped, "{expr}");
    plain
}

#[test]
fn a_file_records_its_configuration_and_a_call_naming_none_reads_it() {
    let simple = searched("config_default", None);
    let english = searched("config_english", Some("pg_catalog.english"));
    assert_eq!(fresh(&simple).text_search(), "pg_catalog.simple");
    assert_eq!(fresh(&english).text_search(), "pg_catalog.english");
    // Named by its short name, it is recorded by its whole.
    let short = searched("config_short", Some("english"));
    assert_eq!(fresh(&short).text_search(), "pg_catalog.english");
    for (expr, under_simple, under_english) in [
        (
            "to_tsvector(t)",
            ["-", "'fat':2 'rats':3 'the':1", "'the':1", "'stars':1"],
            ["-", "'fat':2 'rat':3", "", "'star':1"],
        ),
        (
            "to_tsvector(v)",
            ["-", "'fat':1 'rats':2", "'the':1", "'star':1"],
            ["-", "'fat':1 'rat':2", "", "'star':1"],
        ),
        (
            "plainto_tsquery(tb)",
            ["-", "'fat' & 'rats'", "'the'", "'star'"],
            ["-", "'fat' & 'rat'", "", "'star'"],
        ),
        (
            "phraseto_tsquery(tb)",
            ["-", "'fat' <-> 'rats'", "'the'", "'star'"],
            ["-", "'fat' <-> 'rat'", "", "'star'"],
        ),
        (
            "websearch_to_tsquery(tb)",
            ["-", "'fat' & 'rats'", "'the'", "'star'"],
            ["-", "'fat' & 'rat'", "", "'star'"],
        ),
        (
            "to_tsquery(q::text)",
            ["-", "'fat' & 'rat'", "'the'", "'star'"],
            ["-", "'fat' & 'rat'", "", "'star'"],
        ),
        (
            "ts_headline(t, q)",
            ["-", "The <b>Fat</b> Rats", "<b>the</b>", "stars"],
            ["-", "The <b>Fat</b> <b>Rats</b>", "the", "<b>stars</b>"],
        ),
        (
            "ts_headline(t, q, 'StartSel=<, StopSel=>')",
            ["-", "The <Fat> Rats", "<the>", "stars"],
            ["-", "The <Fat> <Rats>", "the", "<stars>"],
        ),
        (
            "get_current_ts_config()",
            ["simple", "simple", "simple", "simple"],
            ["english", "english", "english", "english"],
        ),
        (
            "current_setting('default_text_search_config')",
            [
                "pg_catalog.simple",
                "pg_catalog.simple",
                "pg_catalog.simple",
                "pg_catalog.simple",
            ],
            [
                "pg_catalog.english",
                "pg_catalog.english",
                "pg_catalog.english",
                "pg_catalog.english",
            ],
        ),
        (
            "to_tsvector(get_current_ts_config(), 'cats')",
            ["'cats':1", "'cats':1", "'cats':1", "'cats':1"],
            ["'cat':1", "'cat':1", "'cat':1", "'cat':1"],
        ),
        // A call naming one reads it, under either.
        (
            "to_tsvector('english', 'cats')",
            ["'cat':1", "'cat':1", "'cat':1", "'cat':1"],
            ["'cat':1", "'cat':1", "'cat':1", "'cat':1"],
        ),
        (
            "to_tsvector('simple', 'cats')",
            ["'cats':1", "'cats':1", "'cats':1", "'cats':1"],
            ["'cats':1", "'cats':1", "'cats':1", "'cats':1"],
        ),
    ] {
        assert_eq!(over_words(&simple, expr), strs(&under_simple), "{expr}");
        assert_eq!(over_words(&english, expr), strs(&under_english), "{expr}");
    }
    let debugged = |path: &Path| {
        let (b, _) = fresh(path)
            .ask(
                "WITH w AS (SELECT t FROM s.d WHERE id = 4) SELECT (ts_debug(t))::text AS x FROM w",
                None,
            )
            .unwrap();
        rows(&b)
    };
    assert_eq!(
        debugged(&simple),
        strs(&["(asciiword,\"Word, all ASCII\",stars,{simple},simple,{stars})"])
    );
    assert_eq!(
        debugged(&english),
        strs(&["(asciiword,\"Word, all ASCII\",stars,{english_stem},english_stem,{star})"])
    );
}

#[test]
fn text_matches_as_a_vector_of_it_under_the_file_s_configuration_each_way_of_asking() {
    let simple = searched("match_default", None);
    let english = searched("match_english", Some("pg_catalog.english"));
    for (expr, under_simple, under_english) in [
        (
            "t @@ tb",
            ["-", "true", "true", "false"],
            ["-", "true", "false", "true"],
        ),
        (
            "t @@ q",
            ["-", "false", "true", "false"],
            ["-", "true", "false", "true"],
        ),
        (
            "v @@ q",
            ["-", "false", "true", "true"],
            ["-", "true", "false", "true"],
        ),
        (
            "c @@ tb",
            ["-", "true", "true", "true"],
            ["-", "true", "false", "true"],
        ),
        (
            "nm @@ tb",
            ["-", "true", "true", "true"],
            ["-", "true", "false", "true"],
        ),
        (
            "t @@ 'rats'",
            ["-", "true", "false", "false"],
            ["-", "true", "false", "false"],
        ),
        // A text on the right is a plain query, its `&` no operator.
        (
            "t @@ 'fat & rats'",
            ["-", "true", "false", "false"],
            ["-", "true", "false", "false"],
        ),
        ("t @@ NULL", ["-", "-", "-", "-"], ["-", "-", "-", "-"]),
        ("NULL @@ t", ["-", "-", "-", "-"], ["-", "-", "-", "-"]),
        // A literal on the left beside a query is a text.
        (
            "'cats' @@ 'cat'::tsquery",
            ["false", "false", "false", "false"],
            ["true", "true", "true", "true"],
        ),
        (
            "'cats' @@ 'cats'",
            ["true", "true", "true", "true"],
            ["true", "true", "true", "true"],
        ),
    ] {
        assert_eq!(over_words(&simple, expr), strs(&under_simple), "{expr}");
        assert_eq!(over_words(&english, expr), strs(&under_english), "{expr}");
        // In a WHERE, the rows where it holds.
        let held = |path: &Path, want: &[&str]| {
            let (b, _) = fresh(path)
                .ask(
                    &format!("SELECT id FROM s.d WHERE {expr} ORDER BY id"),
                    None,
                )
                .unwrap_or_else(|e| panic!("{expr}: {e}"));
            let kept: Vec<String> = want
                .iter()
                .enumerate()
                .filter(|(_, x)| **x == "true")
                .map(|(i, _)| (i + 1).to_string())
                .collect();
            assert_eq!(rows(&b), kept, "{expr}");
        };
        held(&simple, &under_simple);
        held(&english, &under_english);
    }
    let mut snail = fresh(&simple);
    for (expr, words) in [
        ("t @@ id", "operator does not exist: text @@ integer"),
        ("id @@ q", "operator does not exist: integer @@ tsquery"),
        ("q @@ t", "operator does not exist: tsquery @@ text"),
        ("1 @@ 'a'", "operator does not exist: integer @@ unknown"),
        (
            "to_tsvector(id)",
            "function to_tsvector(integer) does not exist",
        ),
        (
            "to_tsquery(id)",
            "function to_tsquery(integer) does not exist",
        ),
        (
            "ts_headline(id, q)",
            "function ts_headline(integer, tsquery) does not exist",
        ),
    ] {
        let q = format!("SELECT {expr} AS x FROM s.d WHERE id < 0");
        assert_eq!(snail.ask(&q, None).unwrap_err().to_string(), words, "{q}");
    }
}

#[test]
fn a_configuration_goes_out_and_comes_in_with_the_tables_as_lc_monetary_does() {
    use warren_snail::read::{Snail, TEXT_SEARCH_KEY};
    use warren_snail::write::{named, settled_text_search, Named};
    let path = searched("carried", Some("pg_catalog.english"));
    let dir = tmp("ops-carried-out");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = fresh(&path).out(&dir).unwrap()[0].file.clone();
    let of = named("s.d", std::slice::from_ref(&file)).unwrap();
    assert_eq!(of.text_search.as_deref(), Some("pg_catalog.english"));
    // Written back it reads under what its files name, a flag naming the same taken, another not.
    let catalog = Catalog::parse(
        "CREATE TABLE s.d (id integer PRIMARY KEY, t text, tb text, c char(12), v varchar(12), \
         nm name, q tsquery);",
    )
    .unwrap();
    let back = tmp("ops-carried-back.snail");
    let given = [("s.d".to_string(), file)];
    warren_snail::write::write_parquet(&back, &catalog, &given, weather::span(), None, None)
        .unwrap();
    assert_eq!(
        Snail::open(&back).unwrap().text_search(),
        "pg_catalog.english"
    );
    assert_eq!(
        over_words(&back, "to_tsvector('cats')"),
        strs(&["'cat':1", "'cat':1", "'cat':1", "'cat':1"])
    );
    let again = tmp("ops-carried-again.snail");
    let e = warren_snail::write::write_parquet_in(
        &again,
        &catalog,
        &given,
        weather::span(),
        None,
        None,
        Some("simple"),
        None,
    )
    .unwrap_err()
    .to_string();
    assert!(
        e.contains("--text-search-config pg_catalog.simple")
            && e.contains(TEXT_SEARCH_KEY)
            && e.contains("write takes the configuration its files name"),
        "{e}"
    );
    let n = |c: Option<&str>| {
        let mut n = Named::default();
        n.text_search = c.map(str::to_string);
        n
    };
    let s = |x: Option<&str>, f: Option<&str>| {
        settled_text_search(&[("s.a".to_string(), n(x))], f).map_err(|e| e.to_string())
    };
    assert_eq!(s(None, None).unwrap(), "pg_catalog.simple");
    assert_eq!(s(None, Some("english")).unwrap(), "pg_catalog.english");
    assert_eq!(
        s(Some("pg_catalog.english"), None).unwrap(),
        "pg_catalog.english"
    );
    assert_eq!(
        s(Some("pg_catalog.english"), Some("pg_catalog.english")).unwrap(),
        "pg_catalog.english"
    );
    assert_eq!(
        s(None, Some("pg_catalog.french")).unwrap_err(),
        "text search configuration \"pg_catalog.french\" is not read"
    );
    let e = settled_text_search(
        &[
            ("s.a".to_string(), n(Some("pg_catalog.simple"))),
            ("s.b".to_string(), n(Some("pg_catalog.english"))),
        ],
        None,
    )
    .unwrap_err()
    .to_string();
    assert!(e.contains("the snail answers under one"), "{e}");
}

#[test]
#[cfg(feature = "standalone")]
fn a_set_of_the_file_s_configuration_does_nothing_and_of_another_is_refused_by_name() {
    use warren_snail::read::Snail;
    let path = searched("set", Some("pg_catalog.english"));
    let mut snail = Snail::open_writing(&path).unwrap();
    for same in [
        "SET default_text_search_config = 'english'",
        "SET default_text_search_config TO 'pg_catalog.english'",
        "SET default_text_search_config TO DEFAULT",
        "SET LOCAL default_text_search_config = 'pg_catalog.english'",
    ] {
        snail
            .execute(same)
            .unwrap_or_else(|e| panic!("{same}: {e}"));
    }
    for other in [
        "SET default_text_search_config = 'simple'",
        "SET default_text_search_config = 'pg_catalog.french'",
    ] {
        let e = snail.execute(other).unwrap_err().to_string();
        assert!(
            e.contains(
                "is refused: the snail answers under default_text_search_config pg_catalog.english"
            ),
            "{other}: {e}"
        );
    }
    let (b, _) = snail
        .ask(
            "WITH w AS (SELECT id FROM s.d WHERE id = 1) SELECT \
             set_config('default_text_search_config', 'english', false) AS x FROM w",
            None,
        )
        .unwrap();
    assert_eq!(rows(&b), strs(&["pg_catalog.english"]));
    let e = snail
        .ask(
            "WITH w AS (SELECT id FROM s.d WHERE id = 1) SELECT \
             set_config('default_text_search_config', 'simple', false) AS x FROM w",
            None,
        )
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("the snail answers under default_text_search_config pg_catalog.english only"),
        "{e}"
    );
    // A write reads under it too.
    snail.execute("DELETE FROM s.d WHERE t @@ 'stars'").unwrap();
    let (b, _) = snail.ask("SELECT id FROM s.d ORDER BY id", None).unwrap();
    assert_eq!(rows(&b), strs(&["1", "2", "3"]));
}

#[test]
fn a_call_naming_none_across_snails_of_two_configurations_is_refused_by_name() {
    use warren_snail::read::Snails;
    let a = searched("across_a", None);
    let dir = tmp("ops-across-b.snail");
    let catalog = Catalog::parse("CREATE TABLE r.b (id integer PRIMARY KEY, t text);").unwrap();
    let mut carried = warren_snail::write::Carried::default();
    carried.text_search = "english".to_string();
    warren_snail::write::write_carried(
        &dir,
        &catalog,
        &[(
            "r.b".into(),
            Rows {
                columns: vec![
                    Values::new(catalog.tables[0].columns[0].ty),
                    Values::new(catalog.tables[0].columns[1].ty),
                ],
            },
        )],
        &[],
        weather::span(),
        19,
        &carried,
    )
    .unwrap();
    let mut both = Snails::open(&[&a, &dir]).unwrap();
    let e = both
        .ask("SELECT to_tsvector(a.t) AS x FROM s.d a LEFT JOIN r.b b ON a.id = b.id")
        .unwrap_err()
        .to_string();
    assert!(e.contains("across snails recording different ones"), "{e}");
    // Naming one reads under neither.
    let (b, _) = both
        .ask(
            "SELECT to_tsvector('simple', a.t)::text AS x FROM s.d a LEFT JOIN r.b b ON a.id = b.id \
             WHERE a.id = 4",
        )
        .unwrap();
    assert_eq!(rows(&b), strs(&["'stars':1"]));
}

#[test]
fn a_recut_keeps_its_file_s_configuration() {
    let path = searched("recut", Some("pg_catalog.english"));
    let to = tmp("ops-recut-to.snail");
    let _ = std::fs::remove_file(&to);
    fresh(&path).recut(&to).unwrap();
    assert_eq!(fresh(&to).text_search(), "pg_catalog.english");
}

#[test]
fn a_view_binds_its_operators_as_a_statement_does() {
    let path = tmp("ops-view.snail");
    let catalog = Catalog::parse(
        "CREATE TABLE s.t (id integer PRIMARY KEY, n integer, t text);
         CREATE VIEW s.v AS SELECT id, @ n + 1 AS a, t ~~ 'a%' AS b, t ~<~ 'b' AS c FROM s.t;",
    )
    .unwrap();
    let columns = vec![
        Values::Int(vec![Some(1), Some(2)]),
        Values::Int(vec![Some(-5), Some(5)]),
        Values::Text(vec![Some("ab".into()), Some("c".into())]),
    ];
    write(&path, &catalog, &[("s.t".into(), Rows { columns })]).unwrap();
    let mut snail = fresh(&path);
    let (b, _) = snail
        .ask("SELECT a, b, c FROM s.v ORDER BY id", None)
        .unwrap();
    assert_eq!(rows(&b), strs(&["4|true|true", "6|false|false"]));
}

#[test]
fn a_guard_on_the_left_keeps_a_row_from_what_would_refuse_it() {
    let path = snail("guard");
    let mut snail = fresh(&path);
    let mut ask = |q: &str| {
        snail
            .ask(q, None)
            .map(|(b, _)| rows(&b))
            .map_err(|e| e.to_string())
    };
    assert_eq!(
        ask("SELECT id FROM s.t WHERE id = 3 AND @ i2 > 0").unwrap(),
        strs(&["3"])
    );
    assert_eq!(
        ask("SELECT id FROM s.t WHERE f8 >= 0 AND |/ f8 > 4 ORDER BY id").unwrap(),
        strs(&["5"])
    );
    assert_eq!(
        ask("SELECT id FROM s.t WHERE id = 2 OR |/ f8 > 4 ORDER BY id"),
        Err("cannot take square root of a negative number".to_string())
    );
    assert_eq!(
        ask("SELECT id FROM s.t WHERE f8 < 0 OR |/ f8 > 4 ORDER BY id").unwrap(),
        strs(&["2", "4", "5"])
    );
    assert_eq!(
        ask("SELECT id FROM s.t WHERE i2 = 6 AND @ i2 > 0 ORDER BY id").unwrap(),
        strs(&["4"])
    );
}
