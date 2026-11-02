//! The composed types over every element type held: a value held as a group of leaves (a timetz,
//! a bit varying), an interval held as its own 16 bytes and a bit string as a part of a composed
//! value, each keyed as its own type is and back from its leaves' pages; daterange made canonical
//! by a day; a composite of no fields; and the multiranges' operators and array slices, each
//! answered as PostgreSQL answers it.

use std::cmp::Ordering;
use std::sync::Arc;
use warren_snail::catalog::ColumnType;
use warren_snail::composed::{
    key, make_range, text_in, text_out, type_of, Composed, Compound, Field, Item, EMPTY, LB_INC,
    LB_INF, UB_INC, UB_INF,
};
use warren_snail::read::statement::Datum;
use warren_snail::values::Values;

mod weather;

/// Each value written to its leaves, each leaf's pages encoded and decoded, the values made again
/// from the leaves decoded; then each value through an answer's Arrow array and back.
fn round_trip(ty: ColumnType, values: &[Item]) {
    let held: Vec<Option<Arc<Compound>>> = values
        .iter()
        .map(|v| (!v.is_null()).then(|| Arc::new(Compound::new(ty, v.clone()))))
        .collect();
    let leaves = Values::Composed(held.clone()).into_leaves(ty);
    let mut back = Vec::new();
    for (leaf, values) in ty.leaves().iter().zip(&leaves) {
        let desc = warren_snail::encode::leaf_descriptor("v", leaf, true).unwrap();
        let e = warren_snail::encode::encode(&desc, leaf.ty, values, 1).unwrap();
        let pages: Vec<&[u8]> = e.pages.iter().map(|p| p.bytes.as_slice()).collect();
        let rows: usize = e.pages.iter().map(|p| p.rows).sum();
        back.push(
            warren_snail::encode::decode_leaf(&desc, leaf, e.dictionary.as_deref(), &pages, rows)
                .unwrap(),
        );
    }
    let Values::Composed(again) = Values::from_leaves(ty, back) else {
        panic!("a composed column made again")
    };
    let answer =
        warren_snail::composed::arrow_array(ty, held.iter().map(|c| c.as_deref())).unwrap();
    for (r, (a, b)) in held.iter().zip(&again).enumerate() {
        // Each value's own parts back, not only its key.
        assert_eq!(a.as_ref().map(|c| &c.item), b.as_ref().map(|c| &c.item));
        let c = warren_snail::composed::compound_at(ty, answer.as_ref(), r);
        assert_eq!(a.as_ref().map(|c| &c.item), c.as_ref().map(|c| &c.item));
    }
}

fn array_of(element: ColumnType) -> ColumnType {
    type_of(Composed::Array {
        element,
        domain: None,
    })
}

/// A one-dimensional array from 1 of the values given.
fn array(elements: Vec<Item>) -> Item {
    let n = elements.len() as i32;
    Item::Array {
        dims: if n == 0 { vec![] } else { vec![n] },
        lower: if n == 0 { vec![] } else { vec![1] },
        elements,
    }
}

fn read(ty: ColumnType, s: &str) -> Item {
    match warren_snail::composed::scalar_in(ty, s).unwrap() {
        Datum::Null => Item::Null,
        d => Item::Scalar(d),
    }
}

#[test]
fn p8_an_element_held_as_a_group_of_leaves_or_a_bit_string_comes_back_from_its_pages() {
    let timetz = ColumnType::Timetz(None);
    let interval = ColumnType::Interval {
        range: 0x7FFF,
        precision: None,
    };
    // Each array of the values spelled, as PostgreSQL 18.6 wrote it (28818, 2026-10-30).
    for (ty, spelled, written) in [
        (
            timetz,
            &["12:00:00+01", "24:00:00-15:59", "00:00:00+15:59"][..],
            "{12:00:00+01,24:00:00-15:59,00:00:00+15:59}",
        ),
        (
            interval,
            &["1 mon", "30 days", "-1 year -2 days 03:04:05.5", "infinity"][..],
            "{\"1 mon\",\"30 days\",\"-1 years -2 days +03:04:05.5\",infinity}",
        ),
        (
            ColumnType::Varbit(None),
            &["", "1", "10100", "000000001"][..],
            "{\"\",1,10100,000000001}",
        ),
        (
            ColumnType::Bit(5),
            &["00000", "10100", "11111"][..],
            "{00000,10100,11111}",
        ),
    ] {
        let parts: Vec<Item> = spelled.iter().map(|s| read(ty, s)).collect();
        assert_eq!(text_out(array_of(ty), &array(parts.clone())), written);
        assert_eq!(
            text_in(array_of(ty), written).unwrap(),
            array(parts.clone())
        );
        let mut values = vec![Item::Null, array(vec![]), array(vec![Item::Null])];
        values.push(array(parts.clone()));
        values.push(array(parts.iter().cloned().chain([Item::Null]).collect()));
        values.push(Item::Array {
            elements: parts[..2].to_vec(),
            dims: vec![1, 2],
            lower: vec![0, -1],
        });
        round_trip(array_of(ty), &values);
        // As a composite's field, beside a field of one leaf.
        let record = type_of(Composed::Record {
            name: "s.g".into(),
            fields: vec![Field::new("a", ty), Field::new("b", ColumnType::Int4)],
        });
        let mut records = vec![Item::Null, Item::Record(vec![Item::Null, Item::Null])];
        for p in &parts {
            records.push(Item::Record(vec![p.clone(), Item::Scalar(Datum::Int(1))]));
        }
        round_trip(record, &records);
    }
    // A bit string of another length is refused as an element of bit(5), as PostgreSQL's INSERT
    // refuses it.
    assert!(text_in(array_of(ColumnType::Bit(5)), "{101}").is_err());
}

#[test]
fn p8_a_composite_of_one_leaf_and_of_none_comes_back_from_its_pages() {
    let none = type_of(Composed::Record {
        name: "s.none".into(),
        fields: vec![],
    });
    round_trip(none, &[Item::Null, Item::Record(vec![]), Item::Null]);
    round_trip(
        array_of(none),
        &[
            Item::Null,
            array(vec![Item::Record(vec![]), Item::Null]),
            array(vec![]),
        ],
    );
    assert_eq!(text_out(none, &Item::Record(vec![])), "()");
    assert_eq!(text_in(none, "()").unwrap(), Item::Record(vec![]));
    assert!(text_in(none, "( )").is_err());
    let one = type_of(Composed::Record {
        name: "s.one".into(),
        fields: vec![Field::new("a", ColumnType::Int8)],
    });
    round_trip(
        one,
        &[
            Item::Null,
            Item::Record(vec![Item::Null]),
            Item::Record(vec![Item::Scalar(Datum::Int(7))]),
        ],
    );
}

/// A snail of one table, its first column the ids 1 on and each other column a composed type's,
/// written from the text of each value, NULL where none.
fn snail_of(name: &str, ddl: &str, columns: &[&[Option<&str>]]) -> warren_snail::read::Snail {
    let path = weather::tmp(&format!("{name}.snail"));
    let c = warren_snail::catalog::Catalog::parse(ddl).unwrap();
    let t = &c.tables[0];
    let rows = columns[0].len() as i64;
    let mut values = vec![Values::Int((1..=rows).map(Some).collect())];
    for (i, spelled) in columns.iter().enumerate() {
        let ty = t.columns[i + 1].ty;
        values.push(Values::Composed(
            spelled
                .iter()
                .map(|s| s.map(|s| Arc::new(Compound::new(ty, text_in(ty, s).unwrap()))))
                .collect(),
        ));
    }
    weather::write(
        &path,
        &c,
        &[(
            t.name.clone(),
            warren_snail::write::Rows { columns: values },
        )],
    )
    .unwrap();
    warren_snail::read::Snail::open(&path).unwrap()
}

/// An answer's column of a composed type, each value's text; none for a NULL.
fn texts_of(b: &arrow::array::RecordBatch, c: usize) -> Vec<Option<String>> {
    let ty = warren_snail::composed::field_composed(b.schema().field(c)).unwrap();
    (0..b.num_rows())
        .map(|r| {
            warren_snail::composed::compound_at(ty, b.column(c).as_ref(), r)
                .map(|v| text_out(ty, &v.item))
        })
        .collect()
}

#[test]
fn a_composite_of_one_leaf_counts_for_the_cut_by_its_leaf_s_values() {
    // 8,192 rows, each a composite whose one field is NULL: its leaf holds no value, so the bigint
    // key decides the cut, 512 rows a page, where the field's integer would decide at 1,024.
    let ddl = "CREATE TYPE s.one AS (a integer); \
        CREATE TABLE s.t (id bigint NOT NULL, o s.one NOT NULL, PRIMARY KEY (id));";
    let path = weather::tmp("elements-one-leaf-cut.snail");
    let c = warren_snail::catalog::Catalog::parse(ddl).unwrap();
    let ty = c.tables[0].columns[1].ty;
    let o = Values::Composed(
        (0..8192)
            .map(|_| Some(Arc::new(Compound::new(ty, Item::Record(vec![Item::Null])))))
            .collect(),
    );
    weather::write(
        &path,
        &c,
        &[(
            "s.t".into(),
            warren_snail::write::Rows {
                columns: vec![Values::Int((0..8192).map(Some).collect()), o],
            },
        )],
    )
    .unwrap();
    let rows: Vec<u64> = warren_snail::read::Snail::open(&path)
        .unwrap()
        .blocks()
        .unwrap()
        .iter()
        .filter(|r| r.order == "t_pkey" && r.column == "o.a")
        .map(|r| r.rows)
        .collect();
    assert_eq!(rows, [512; 16]);
}

const ONE_LEAF: &str = "CREATE TYPE s.none AS (); CREATE TYPE s.one AS (a bigint); \
    CREATE TABLE s.t (id integer NOT NULL, n s.none, o s.one, PRIMARY KEY (id));";

#[test]
fn a_composite_of_one_leaf_or_none_is_read_whole_on_the_question_path() {
    let mut snail = snail_of(
        "elements-one-leaf",
        ONE_LEAF,
        &[
            &[Some("()"), None, Some("()")],
            &[Some("(7)"), Some("()"), None],
        ],
    );
    let (b, _) = snail
        .ask("SELECT id, n, o FROM s.t ORDER BY id", None)
        .unwrap();
    assert_eq!(
        texts_of(&b, 1),
        [Some("()".into()), None, Some("()".into())]
    );
    assert_eq!(
        texts_of(&b, 2),
        [Some("(7)".into()), Some("()".into()), None]
    );
    // A composite of no fields is NULL and NOT NULL both, as PostgreSQL 18.6 has it (28818,
    // 2026-10-30); and grouped apart from a NULL.
    let step = "WITH q AS (SELECT id, n FROM s.t)";
    let (b, _) = snail
        .ask(
            &format!("{step} SELECT id, n IS NULL AS a, n IS NOT NULL AS b FROM q ORDER BY id"),
            None,
        )
        .unwrap();
    assert_eq!(answer_text(&b), "true,true|true,false|true,true");
    let (b, _) = snail
        .ask(
            &format!("{step} SELECT n, count(*) AS c FROM q GROUP BY n ORDER BY c"),
            None,
        )
        .unwrap();
    assert_eq!(answer_text(&b), "1|2");
}

#[test]
#[cfg(feature = "standalone")]
fn a_composite_of_one_leaf_or_none_is_written_by_a_statement_and_read_whole() {
    let path = weather::tmp("elements-one-leaf-written.snail");
    let c = warren_snail::catalog::Catalog::parse(ONE_LEAF).unwrap();
    weather::write(&path, &c, &[]).unwrap();
    let mut w = warren_snail::read::Snail::open_writing(&path).unwrap();
    for sql in [
        "INSERT INTO s.t (id, n, o) VALUES (1, '()', '(7)')",
        "INSERT INTO s.t (id, n, o) VALUES (2, NULL, '()')",
        "INSERT INTO s.t (id, n, o) VALUES (3, ROW(), NULL)",
    ] {
        w.execute(sql).unwrap();
    }
    drop(w);
    let (b, _) = warren_snail::read::Snail::open(&path)
        .unwrap()
        .ask("SELECT id, n, o FROM s.t ORDER BY id", None)
        .unwrap();
    assert_eq!(
        texts_of(&b, 1),
        [Some("()".into()), None, Some("()".into())]
    );
    assert_eq!(
        texts_of(&b, 2),
        [Some("(7)".into()), Some("()".into()), None]
    );
}

/// A daterange's bounds as `daterange_canonical` writes them: a finite lower bound inclusive, a
/// finite upper bound exclusive, each moved a day where it was not; an infinite date or no bound
/// as it is.
fn canonical_date(flags: u8, lo: Option<i64>, hi: Option<i64>) -> (u8, Option<i64>, Option<i64>) {
    let finite = |d: i64| d != i64::from(i32::MIN) && d != i64::from(i32::MAX);
    let mut f = flags;
    let lo = match lo {
        Some(d) if finite(d) && f & LB_INC == 0 => {
            f |= LB_INC;
            Some(d + 1)
        }
        l => l,
    };
    let hi = match hi {
        Some(d) if finite(d) && f & UB_INC != 0 => {
            f &= !UB_INC;
            Some(d + 1)
        }
        h => h,
    };
    (f, lo, hi)
}

/// `range_cmp` over bounds of plain integers: an empty range first, then by the lower bound (none
/// before every value, an inclusive before an exclusive at one value), then the upper (an exclusive
/// before an inclusive, none after every value).
fn range_cmp(a: &(u8, Option<i64>, Option<i64>), b: &(u8, Option<i64>, Option<i64>)) -> Ordering {
    let empty = |r: &(u8, Option<i64>, Option<i64>)| r.0 & EMPTY != 0;
    match (empty(a), empty(b)) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Less,
        (false, true) => return Ordering::Greater,
        _ => {}
    }
    let lower = |r: &(u8, Option<i64>, Option<i64>)| (r.1.is_some(), r.1, r.0 & LB_INC == 0);
    let upper = |r: &(u8, Option<i64>, Option<i64>)| (r.2.is_none(), r.2, r.0 & UB_INC != 0);
    lower(a).cmp(&lower(b)).then(upper(a).cmp(&upper(b)))
}

#[test]
fn p4_a_daterange_is_made_canonical_by_a_day_and_its_key_sorts_as_range_cmp() {
    let range = warren_snail::composed::built_in("daterange").unwrap();
    let date = |d: i64| Item::Scalar(Datum::Int(d));
    let ends = [
        None,
        Some(i64::from(i32::MIN)),
        Some(-1),
        Some(0),
        Some(1),
        Some(2),
        Some(i64::from(i32::MAX)),
    ];
    let mut made = Vec::new();
    for lo in ends {
        for hi in ends {
            for flags in [0, LB_INC, UB_INC, LB_INC | UB_INC] {
                let r = make_range(ColumnType::Date, true, flags, lo.map(date), hi.map(date));
                // PostgreSQL's range_serialize first: a lower bound above its upper refused, equal
                // bounds not both inclusive empty; then the canonical form, serialized again.
                let (mut f, l, h) = (flags, lo, hi);
                if l.is_none() {
                    f = (f | LB_INF) & !LB_INC;
                }
                if h.is_none() {
                    f = (f | UB_INF) & !UB_INC;
                }
                let serial = |f: u8, l: Option<i64>, h: Option<i64>| match (l, h) {
                    (Some(x), Some(y)) if x > y => None,
                    (Some(x), Some(y)) if x == y && f & (LB_INC | UB_INC) != LB_INC | UB_INC => {
                        Some((EMPTY, None, None))
                    }
                    _ => Some((f, l, h)),
                };
                let want = serial(f, l, h).and_then(|first| {
                    if first.0 & EMPTY != 0 {
                        Some(first)
                    } else {
                        let (f, l, h) = canonical_date(first.0, first.1, first.2);
                        serial(f, l, h)
                    }
                });
                let got = r.ok().map(|i| match i {
                    Item::Range {
                        flags,
                        lower,
                        upper,
                    } => {
                        let v = |b: Option<Box<Item>>| match b.as_deref() {
                            Some(Item::Scalar(Datum::Int(d))) => Some(*d),
                            _ => None,
                        };
                        (flags, v(lower), v(upper))
                    }
                    _ => unreachable!(),
                });
                assert_eq!(got, want, "{lo:?} {hi:?} {flags:#x}");
                if let Some(w) = want {
                    made.push(w);
                }
            }
        }
    }
    let item = |r: &(u8, Option<i64>, Option<i64>)| Item::Range {
        flags: r.0,
        lower: r.1.map(|d| Box::new(date(d))),
        upper: r.2.map(|d| Box::new(date(d))),
    };
    for a in &made {
        for b in &made {
            assert_eq!(
                key(range, &item(a)).cmp(&key(range, &item(b))),
                range_cmp(a, b),
                "{} against {}",
                text_out(range, &item(a)),
                text_out(range, &item(b))
            );
        }
    }
    // A finite bound a day past the last date is refused, as PostgreSQL refuses it.
    assert!(text_in(range, "[2024-01-01,5874897-12-31]").is_err());
    assert_eq!(
        text_out(range, &text_in(range, "(2023-12-31,2024-02-01)").unwrap()),
        "[2024-01-01,2024-02-01)"
    );
}

#[test]
fn p4_an_array_of_timetz_interval_or_bit_strings_sorts_as_its_elements_compare() {
    let timetz = ColumnType::Timetz(None);
    let interval = ColumnType::Interval {
        range: 0x7FFF,
        precision: None,
    };
    // timetz_cmp: the time on the UTC clock, then the zone; interval_cmp: the total of 30-day
    // months and 24-hour days; bit_cmp: the bits, a string that ends first sorting first.
    let utc = |s: &str| -> (i64, i32) {
        let Item::Scalar(Datum::Int(k)) = read(timetz, s) else {
            unreachable!()
        };
        let (t, z) = warren_snail::scalars::timetz_parts(k);
        (t + i64::from(z) * 1_000_000, z)
    };
    let total = |s: &str| -> i128 {
        let Item::Scalar(Datum::Interval(v)) = read(interval, s) else {
            unreachable!()
        };
        i128::from(v.months) * 30 * 86_400_000_000
            + i128::from(v.days) * 86_400_000_000
            + i128::from(v.micros)
    };
    let bits = |s: &str| s.to_string();
    for (ty, spelled, cmp) in [
        (
            timetz,
            vec![
                "12:00:00+01",
                "11:00:00+00",
                "11:00:00-01",
                "00:00:00+15:59",
                "24:00:00-15:59",
            ],
            Box::new(move |a: &str, b: &str| utc(a).cmp(&utc(b)))
                as Box<dyn Fn(&str, &str) -> Ordering>,
        ),
        (
            interval,
            vec![
                "1 mon",
                "30 days",
                "720:00:00",
                "-1 day",
                "infinity",
                "-infinity",
                "0",
            ],
            Box::new(move |a: &str, b: &str| total(a).cmp(&total(b))),
        ),
        (
            ColumnType::Varbit(None),
            vec!["", "0", "1", "00", "01", "10", "000", "0001", "1000"],
            Box::new(move |a: &str, b: &str| bits(a).cmp(&bits(b))),
        ),
    ] {
        let ty_a = array_of(ty);
        for a in &spelled {
            for b in &spelled {
                for (x, y) in [
                    (vec![*a], vec![*b]),
                    (vec![*a, *b], vec![*b]),
                    (vec![*a], vec![*a, *b]),
                ] {
                    let want = x
                        .iter()
                        .zip(&y)
                        .map(|(p, q)| cmp(p, q))
                        .find(|o| *o != Ordering::Equal)
                        .unwrap_or(x.len().cmp(&y.len()));
                    let item = |v: &[&str]| array(v.iter().map(|s| read(ty, s)).collect());
                    assert_eq!(
                        key(ty_a, &item(&x)).cmp(&key(ty_a, &item(&y))),
                        want,
                        "{x:?} against {y:?}"
                    );
                }
            }
        }
    }
}

/// An answer's rows, each its values' text joined by commas, the rows by bars; NULL empty.
fn answer_text(b: &arrow::array::RecordBatch) -> String {
    (0..b.num_rows())
        .map(|r| {
            (1..b.num_columns())
                .map(|c| {
                    let a = b.column(c).as_ref();
                    if a.is_null(r) {
                        return String::new();
                    }
                    match warren_snail::composed::field_composed(b.schema().field(c)) {
                        Some(ty) => warren_snail::composed::compound_at(ty, a, r)
                            .map(|v| text_out(ty, &v.item))
                            .unwrap_or_default(),
                        None => arrow::util::display::array_value_to_string(a, r).unwrap(),
                    }
                })
                .collect::<Vec<_>>()
                .join(",")
        })
        .collect::<Vec<_>>()
        .join("|")
}

const QUESTIONS: &str = "CREATE TABLE s.q (id integer NOT NULL, m int4multirange, r int4range, \
    a integer[], PRIMARY KEY (id));";

const ROWS: [&[Option<&str>]; 3] = [
    &[
        Some("{[1,3),[5,8)}"),
        Some("{}"),
        None,
        Some("{(,2),[4,5)}"),
        Some("{[0,10)}"),
        Some("{[20,30)}"),
        Some("{[10,20),[40,41)}"),
    ],
    &[
        Some("[2,6)"),
        Some("empty"),
        None,
        Some("[3,4)"),
        Some("(,)"),
        Some("[30,40)"),
        Some("[0,1)"),
    ],
    &[
        Some("{1,2,3,4}"),
        Some("{}"),
        None,
        Some("[0:2]={7,NULL,9}"),
        Some("{{1,2},{3,4}}"),
        Some("{5,6,7}"),
        Some("{{1},{2},{3}}"),
    ],
];

#[test]
fn a_multirange_s_operators_answer_as_postgresql_s() {
    let mut snail = snail_of("elements-multiranges", QUESTIONS, &ROWS);
    let step = "WITH q AS (SELECT id, m, r FROM s.q)";
    // Each answer as PostgreSQL 18.6 gave it (28818, 2026-10-30), a row of the pairs for each
    // first id, each pair's values by commas and the pairs by bars, by the second id.
    for (q, want) in [
        (
            "SELECT a.id, (a.m @> b.m) AS c, (a.m && b.m) AS o, (a.m << b.m) AS l, (a.m >> b.m) AS g, \
             (a.m &< b.m) AS x, (a.m &> b.m) AS y, (a.m -|- b.m) AS j FROM q a CROSS JOIN q b ORDER BY a.id, b.id",
            [
                "true,true,false,false,true,true,false|true,false,false,false,false,false,false|,,,,,,|false,true,false,false,false,true,false|false,true,false,false,true,true,false|false,false,true,false,true,false,false|false,false,true,false,true,false,false",
                "false,false,false,false,false,false,false|true,false,false,false,false,false,false|,,,,,,|false,false,false,false,false,false,false|false,false,false,false,false,false,false|false,false,false,false,false,false,false|false,false,false,false,false,false,false",
                ",,,,,,|,,,,,,|,,,,,,|,,,,,,|,,,,,,|,,,,,,|,,,,,,",
                "false,true,false,false,true,false,false|true,false,false,false,false,false,false|,,,,,,|true,true,false,false,true,true,false|false,true,false,false,true,false,false|false,false,true,false,true,false,false|false,false,true,false,true,false,false",
                "true,true,false,false,false,false,false|true,false,false,false,false,false,false|,,,,,,|false,true,false,false,false,true,false|true,true,false,false,true,true,false|false,false,true,false,true,false,false|false,false,true,false,true,false,true",
                "false,false,false,true,false,true,false|true,false,false,false,false,false,false|,,,,,,|false,false,false,true,false,true,false|false,false,false,true,false,true,false|true,true,false,false,true,true,false|false,false,false,false,true,true,false",
                "false,false,false,true,false,true,false|true,false,false,false,false,false,false|,,,,,,|false,false,false,true,false,true,false|false,false,false,true,false,true,true|false,false,false,false,false,false,false|true,true,false,false,true,true,false",
            ]
            .join("|"),
        ),
        (
            "SELECT a.id, (a.m + b.m) AS u, (a.m * b.m) AS i, (a.m - b.m) AS d FROM q a CROSS JOIN q b \
             ORDER BY a.id, b.id",
            [
                "{[1,3),[5,8)},{[1,3),[5,8)},{}|{[1,3),[5,8)},{},{[1,3),[5,8)}|,,|{(,3),[4,8)},{[1,2)},{[2,3),[5,8)}|{[0,10)},{[1,3),[5,8)},{}|{[1,3),[5,8),[20,30)},{},{[1,3),[5,8)}|{[1,3),[5,8),[10,20),[40,41)},{},{[1,3),[5,8)}",
                "{[1,3),[5,8)},{},{}|{},{},{}|,,|{(,2),[4,5)},{},{}|{[0,10)},{},{}|{[20,30)},{},{}|{[10,20),[40,41)},{},{}",
                ",,|,,|,,|,,|,,|,,|,,",
                "{(,3),[4,8)},{[1,2)},{(,1),[4,5)}|{(,2),[4,5)},{},{(,2),[4,5)}|,,|{(,2),[4,5)},{(,2),[4,5)},{}|{(,10)},{[0,2),[4,5)},{(,0)}|{(,2),[4,5),[20,30)},{},{(,2),[4,5)}|{(,2),[4,5),[10,20),[40,41)},{},{(,2),[4,5)}",
                "{[0,10)},{[1,3),[5,8)},{[0,1),[3,5),[8,10)}|{[0,10)},{},{[0,10)}|,,|{(,10)},{[0,2),[4,5)},{[2,4),[5,10)}|{[0,10)},{[0,10)},{}|{[0,10),[20,30)},{},{[0,10)}|{[0,20),[40,41)},{},{[0,10)}",
                "{[1,3),[5,8),[20,30)},{},{[20,30)}|{[20,30)},{},{[20,30)}|,,|{(,2),[4,5),[20,30)},{},{[20,30)}|{[0,10),[20,30)},{},{[20,30)}|{[20,30)},{[20,30)},{}|{[10,30),[40,41)},{},{[20,30)}",
                "{[1,3),[5,8),[10,20),[40,41)},{},{[10,20),[40,41)}|{[10,20),[40,41)},{},{[10,20),[40,41)}|,,|{(,2),[4,5),[10,20),[40,41)},{},{[10,20),[40,41)}|{[0,20),[40,41)},{},{[10,20),[40,41)}|{[10,30),[40,41)},{},{[10,20),[40,41)}|{[10,20),[40,41)},{[10,20),[40,41)},{}",
            ]
            .join("|"),
        ),
        (
            "SELECT id, (m @> r) AS a, (r @> m) AS b, (m <@ r) AS c, (r <@ m) AS d, (m && r) AS e, \
             (r && m) AS f, (m -|- r) AS g, (r -|- m) AS h, (m << r) AS i, (r >> m) AS j, (m &< r) AS k, \
             (r &> m) AS l, (m @> 4) AS n, (4 <@ m) AS o FROM q ORDER BY id",
            [
                "false,false,false,false,true,true,false,false,false,false,false,true,false,false",
                "true,true,true,true,false,false,false,false,false,false,false,false,false,false",
                ",,,,,,,,,,,,,",
                "false,false,false,false,false,false,false,false,false,false,false,true,true,true",
                "false,true,true,false,true,true,false,false,false,false,true,false,true,true",
                "false,false,false,false,false,false,true,true,true,true,true,true,false,false",
                "false,false,false,false,false,false,false,false,false,false,false,false,false,false",
            ]
            .join("|"),
        ),
    ] {
        let (b, _) = snail.ask(&format!("{step} {q}"), None).unwrap();
        assert_eq!(answer_text(&b), want, "{q}");
    }
    for refused in [
        "SELECT id, (r + m) AS x FROM q",
        "SELECT id, (m * r) AS x FROM q",
        "SELECT id, (m || m) AS x FROM q",
    ] {
        assert!(
            snail.ask(&format!("{step} {refused}"), None).is_err(),
            "{refused}"
        );
    }
}

#[test]
fn a_value_of_another_type_beside_a_range_is_refused_as_postgresql_resolves_the_operator() {
    let mut snail = snail_of("elements-resolution", QUESTIONS, &ROWS);
    let step = "WITH q AS (SELECT id, m, r FROM s.q)";
    // Each refusal as PostgreSQL 19 RC1 gave it (28819, 2026-10-31): an element of a type other
    // than the subtype, or a range of another range type, has no form of the operator, over a row
    // and over none.
    for (e, refused) in [
        ("m @> 4::bigint", "int4multirange @> bigint"),
        ("m @> 4.5", "int4multirange @> numeric"),
        ("4::int2 <@ m", "smallint <@ int4multirange"),
        ("m <@ 4", "int4multirange <@ integer"),
        ("m && '[1,2)'::int8range", "int4multirange && int8range"),
        ("r -|- '[1,2)'::int8range", "int4range -|- int8range"),
        ("r @> 4::bigint", "int4range @> bigint"),
        ("r = m", "int4range = int4multirange"),
        ("r + 4", "int4range + integer"),
    ] {
        for rows in ["id = 1", "id < 0"] {
            let q = format!("{step} SELECT id, ({e}) AS x FROM q WHERE {rows}");
            let got = snail.ask(&q, None).unwrap_err().to_string();
            assert_eq!(got, format!("operator does not exist: {refused}"), "{e}");
        }
    }
    for e in ["m @> 4", "m @> NULL", "r <@ m", "m @> '{[1,2)}'", "4 <@ r"] {
        let q = format!("{step} SELECT id, ({e}) AS x FROM q WHERE id = 1");
        assert!(snail.ask(&q, None).is_ok(), "{e}");
    }
}

#[test]
fn a_multirange_s_equal_bounds_merge_into_the_later_range_s() {
    let num = warren_snail::composed::built_in("nummultirange").unwrap();
    // As PostgreSQL 18.6 read each (28818, 2026-10-30).
    for (spelled, want) in [
        ("{[1,2],[1.0,3]}", "{[1.0,3]}"),
        ("{[1.0,2],[1,2]}", "{[1,2]}"),
        ("{[1,2.0],[1.5,2]}", "{[1,2]}"),
    ] {
        assert_eq!(
            text_out(num, &text_in(num, spelled).unwrap()),
            want,
            "{spelled}"
        );
    }
}

#[test]
fn an_array_slice_answers_as_postgresql_s() {
    let mut snail = snail_of("elements-slices", QUESTIONS, &ROWS);
    let (b, _) = snail
        .ask(
            "WITH q AS (SELECT id, a FROM s.q) SELECT id, a[2:3] AS b, a[:1] AS c, a[2:] AS d, \
             a[1:1][2] AS e, a[0:0] AS f, a[3:2] AS g, a[1][1:2] AS h FROM q ORDER BY id",
            None,
        )
        .unwrap();
    // As PostgreSQL 18.6 gave it (28818, 2026-10-30).
    assert_eq!(
        answer_text(&b),
        [
            "{2,3},{1},{2,3,4},{},{},{},{}",
            "{},{},{},{},{},{},{}",
            ",,,,,,",
            "{9},{7,NULL},{9},{},{7},{},{}",
            "{{3,4}},{{1,2}},{{3,4}},{{1,2}},{},{},{{1,2}}",
            "{6,7},{5},{6,7},{},{},{},{}",
            "{{2},{3}},{{1}},{{2},{3}},{{1}},{},{},{{1}}",
        ]
        .join("|")
    );
    for refused in [
        "SELECT id, a[3000000000] AS x FROM q",
        "SELECT id, a[1:2:1] AS x FROM q",
    ] {
        assert!(
            snail
                .ask(
                    &format!("WITH q AS (SELECT id, a FROM s.q) {refused}"),
                    None
                )
                .is_err(),
            "{refused}"
        );
    }
}

#[test]
fn p8_an_array_of_a_geometric_type_or_an_aclitem_comes_back_from_its_pages() {
    use warren_snail::geo::GeoKind;
    // Each array as PostgreSQL 18.6 wrote it (28818, 2026-10-30), box's elements between `;`; an
    // aclitem's roles as their OIDs, as the snail prints every role.
    for (ty, written) in [
        (
            ColumnType::Geometric(GeoKind::Point),
            "{\"(1,2)\",\"(0,0)\"}",
        ),
        (
            ColumnType::Geometric(GeoKind::Box),
            "{(1,1),(0,0);(2,2),(1,1)}",
        ),
        (ColumnType::Geometric(GeoKind::Circle), "{\"<(0,0),1>\"}"),
        (ColumnType::Geometric(GeoKind::Lseg), "{\"[(0,0),(1,1)]\"}"),
        (ColumnType::Geometric(GeoKind::Line), "{\"{1,-1,0}\"}"),
        (
            ColumnType::Textual(warren_snail::textual::Textual::Aclitem),
            "{=r/6171,3373=rw*/3373}",
        ),
    ] {
        let a = array_of(ty);
        let item = text_in(a, written).unwrap();
        assert_eq!(text_out(a, &item), written);
        let Item::Array { elements, .. } = &item else {
            unreachable!()
        };
        let mut values = vec![Item::Null, array(vec![]), item.clone()];
        values.push(array(
            elements.iter().cloned().chain([Item::Null]).collect(),
        ));
        round_trip(a, &values);
    }
}

const UNEQUAL: &str = "CREATE TABLE s.u (id integer NOT NULL, j json[], p point[], x xid[], \
    PRIMARY KEY (id));";

#[test]
fn an_array_whose_element_has_no_equality_is_compared_by_none() {
    let mut snail = snail_of(
        "elements-unequal",
        UNEQUAL,
        &[
            &[Some("{\"{}\"}"), Some("{\"[]\"}")],
            &[Some("{\"(1,2)\"}"), Some("{}")],
            &[Some("{1}"), Some("{1}")],
        ],
    );
    let step = "WITH q AS (SELECT id, j, p, x FROM s.u)";
    // As PostgreSQL 18.6 answered or refused each (28818, 2026-10-30): json and point have no
    // equality, xid one but no order.
    for (q, refused) in [
        ("SELECT id, (j = j) AS a FROM q", true),
        ("SELECT id, (j IS DISTINCT FROM j) AS a FROM q", true),
        ("SELECT id, (j @> j) AS a FROM q", true),
        ("SELECT id, (p = p) AS a FROM q", true),
        ("SELECT id, (x = x) AS a FROM q", false),
        ("SELECT id, (x @> x) AS a FROM q", false),
        ("SELECT id, (x < x) AS a FROM q", true),
        ("SELECT j, count(*) AS n FROM q GROUP BY j", true),
        ("SELECT x, count(*) AS n FROM q GROUP BY x", false),
        ("SELECT min(j) AS a FROM q WHERE id = 1", false),
        ("SELECT min(j) AS a FROM q", true),
    ] {
        let r = snail.ask(&format!("{step} {q}"), None);
        assert_eq!(r.is_err(), refused, "{q}: {:?}", r.err());
    }
    for (q, refused) in [
        ("SELECT id FROM s.u WHERE j = '{\"{}\"}'", true),
        ("SELECT id FROM s.u WHERE j = NULL", false),
        ("SELECT id FROM s.u WHERE x = '{1}'", false),
        ("SELECT a.id FROM s.u a JOIN s.u b ON a.j = b.j", true),
        ("SELECT a.id FROM s.u a JOIN s.u b ON a.x = b.x", false),
    ] {
        let r = snail.ask(q, None);
        assert_eq!(r.is_err(), refused, "{q}: {:?}", r.err());
        if let Err(e) = r {
            assert!(
                e.to_string()
                    .contains("could not identify an equality operator for type json"),
                "{q}: {e}"
            );
        }
    }
}
