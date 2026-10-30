//! The composed types: each one's key sorts as PostgreSQL's comparison function for it does, over
//! a small domain of its values walked whole, each held against a comparator written here from the
//! same function; and every value comes back from its leaves' pages.

use std::cmp::Ordering;
use std::sync::Arc;
use warren_snail::catalog::ColumnType;
use warren_snail::composed::{
    key, text_in, text_out, type_of, Composed, Compound, Field, Item, EMPTY, LB_INC, LB_INF,
    UB_INC, UB_INF,
};
use warren_snail::read::statement::Datum;
use warren_snail::values::Values;

fn int(v: i64) -> Item {
    Item::Scalar(Datum::Int(v))
}

/// Every key's order is the comparator's, over every pair.
fn sorts_as(ty: ColumnType, values: &[Item], cmp: impl Fn(&Item, &Item) -> Ordering) {
    let keys: Vec<Vec<u8>> = values.iter().map(|v| key(ty, v)).collect();
    for (i, a) in values.iter().enumerate() {
        for (j, b) in values.iter().enumerate() {
            assert_eq!(
                keys[i].cmp(&keys[j]),
                cmp(a, b),
                "{} against {}",
                text_out(ty, a),
                text_out(ty, b)
            );
        }
    }
}

/// Byte strings of length 0 to 3 over 0x00, 0x01 and 0xFF.
fn strings() -> Vec<Vec<u8>> {
    let mut out = vec![Vec::new()];
    let mut last = vec![Vec::new()];
    for _ in 0..3 {
        let mut next = Vec::new();
        for s in &last {
            for b in [0x00u8, 0x01, 0xFF] {
                let mut t = s.clone();
                t.push(b);
                next.push(t);
            }
        }
        out.extend(next.iter().cloned());
        last = next;
    }
    out
}

#[test]
fn p3_two_parts_of_varying_length_sort_as_a_tuple_an_ended_part_first() {
    let ty = type_of(Composed::Record {
        name: "p3".into(),
        fields: vec![
            Field::new("a", ColumnType::Bytes),
            Field::new("b", ColumnType::Bytes),
        ],
    });
    let s = strings();
    assert_eq!(s.len(), 40);
    let values: Vec<Item> = s
        .iter()
        .flat_map(|a| {
            s.iter().map(move |b| {
                Item::Record(vec![
                    Item::Scalar(Datum::Bytes(a.clone())),
                    Item::Scalar(Datum::Bytes(b.clone())),
                ])
            })
        })
        .collect();
    let bytes = |i: &Item, f: usize| match i {
        Item::Record(v) => match &v[f] {
            Item::Scalar(Datum::Bytes(b)) => b.clone(),
            _ => unreachable!(),
        },
        _ => unreachable!(),
    };
    sorts_as(ty, &values, |a, b| {
        bytes(a, 0)
            .cmp(&bytes(b, 0))
            .then(bytes(a, 1).cmp(&bytes(b, 1)))
    });
}

/// `array_cmp`: element by element, a NULL after any value and two NULLs equal; then fewer
/// elements first, fewer dimensions, each dimension's length, each lower bound.
fn array_cmp(a: &Item, b: &Item) -> Ordering {
    let (
        Item::Array {
            elements: ea,
            dims: da,
            lower: la,
        },
        Item::Array {
            elements: eb,
            dims: db,
            lower: lb,
        },
    ) = (a, b)
    else {
        unreachable!()
    };
    for (x, y) in ea.iter().zip(eb) {
        let o = match (x, y) {
            (Item::Null, Item::Null) => Ordering::Equal,
            (Item::Null, _) => Ordering::Greater,
            (_, Item::Null) => Ordering::Less,
            (Item::Scalar(Datum::Int(p)), Item::Scalar(Datum::Int(q))) => p.cmp(q),
            _ => unreachable!(),
        };
        if o != Ordering::Equal {
            return o;
        }
    }
    ea.len()
        .cmp(&eb.len())
        .then(da.len().cmp(&db.len()))
        .then(da.cmp(db))
        .then(la.cmp(lb))
}

/// Every array of integers whose elements are NULL, 0 or 1, of every shape up to 2 by 2, each
/// dimension's lower bound -1, 0 or 1; and the array of no elements.
fn arrays() -> Vec<Item> {
    let parts = [Item::Null, int(0), int(1)];
    let mut out = vec![Item::Array {
        elements: Vec::new(),
        dims: Vec::new(),
        lower: Vec::new(),
    }];
    let shapes: Vec<Vec<i32>> = vec![
        vec![1],
        vec![2],
        vec![1, 1],
        vec![1, 2],
        vec![2, 1],
        vec![2, 2],
    ];
    for dims in shapes {
        let n: usize = dims.iter().product::<i32>() as usize;
        let mut lowers: Vec<Vec<i32>> = vec![Vec::new()];
        for _ in &dims {
            lowers = lowers
                .into_iter()
                .flat_map(|l| {
                    [-1, 0, 1].into_iter().map(move |x| {
                        let mut l = l.clone();
                        l.push(x);
                        l
                    })
                })
                .collect();
        }
        for code in 0..3usize.pow(n as u32) {
            let elements: Vec<Item> = (0..n)
                .map(|i| parts[(code / 3usize.pow(i as u32)) % 3].clone())
                .collect();
            for lower in &lowers {
                out.push(Item::Array {
                    elements: elements.clone(),
                    dims: dims.clone(),
                    lower: lower.clone(),
                });
            }
        }
    }
    out
}

#[test]
fn p4_an_array_s_key_sorts_as_array_cmp() {
    let ty = type_of(Composed::Array {
        element: ColumnType::Int4,
        domain: None,
    });
    let values = arrays();
    sorts_as(ty, &values, array_cmp);
}

#[test]
fn p4_a_composite_s_key_sorts_as_record_cmp() {
    let ty = type_of(Composed::Record {
        name: "pair".into(),
        fields: vec![
            Field::new("a", ColumnType::Int4),
            Field::new("b", ColumnType::Text(None)),
        ],
    });
    let mut values = Vec::new();
    for a in [Item::Null, int(-1), int(0), int(1)] {
        for b in [None, Some(""), Some("a"), Some("ab"), Some("b")] {
            values.push(Item::Record(vec![
                a.clone(),
                b.map_or(Item::Null, |s| Item::Scalar(Datum::Text(s.into()))),
            ]));
        }
    }
    // A field NULL after any value, two NULLs equal; field by field.
    let field = |x: &Item, y: &Item| match (x, y) {
        (Item::Null, Item::Null) => Ordering::Equal,
        (Item::Null, _) => Ordering::Greater,
        (_, Item::Null) => Ordering::Less,
        (Item::Scalar(Datum::Int(p)), Item::Scalar(Datum::Int(q))) => p.cmp(q),
        (Item::Scalar(Datum::Text(p)), Item::Scalar(Datum::Text(q))) => {
            p.as_bytes().cmp(q.as_bytes())
        }
        _ => unreachable!(),
    };
    sorts_as(ty, &values, |a, b| match (a, b) {
        (Item::Record(x), Item::Record(y)) => field(&x[0], &y[0]).then(field(&x[1], &y[1])),
        _ => unreachable!(),
    });
}

/// `range_cmp_bounds` on two bounds, each its value (none where unbounded), inclusive, and lower.
fn bound_cmp(a: (Option<i64>, bool, bool), b: (Option<i64>, bool, bool)) -> Ordering {
    let (av, ai, al) = a;
    let (bv, bi, bl) = b;
    match (av, bv) {
        (None, None) if al == bl => Ordering::Equal,
        (None, _) => {
            if al {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (_, None) => {
            if bl {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (Some(x), Some(y)) => match x.cmp(&y) {
            Ordering::Equal => match (ai, bi) {
                (false, false) if al == bl => Ordering::Equal,
                (false, _) => {
                    if al {
                        Ordering::Greater
                    } else {
                        Ordering::Less
                    }
                }
                (true, false) => {
                    if bl {
                        Ordering::Less
                    } else {
                        Ordering::Greater
                    }
                }
                (true, true) => Ordering::Equal,
            },
            o => o,
        },
    }
}

fn range_bounds(r: &Item) -> Option<((Option<i64>, bool, bool), (Option<i64>, bool, bool))> {
    let Item::Range {
        flags,
        lower,
        upper,
    } = r
    else {
        unreachable!()
    };
    if flags & EMPTY != 0 {
        return None;
    }
    let v = |b: &Option<Box<Item>>| match b.as_deref() {
        Some(Item::Scalar(Datum::Int(x))) => Some(*x),
        _ => None,
    };
    Some((
        (v(lower), flags & LB_INC != 0, true),
        (v(upper), flags & UB_INC != 0, false),
    ))
}

/// `range_cmp`: an empty range first; then the lower bounds, then the upper.
fn range_cmp(a: &Item, b: &Item) -> Ordering {
    match (range_bounds(a), range_bounds(b)) {
        (None, None) => Ordering::Equal,
        (None, _) => Ordering::Less,
        (_, None) => Ordering::Greater,
        (Some((al, au)), Some((bl, bu))) => bound_cmp(al, bl).then(bound_cmp(au, bu)),
    }
}

/// Every range over -1 to 2 by every flag, unbounded ends among them, made as `make_range` makes
/// it, refusals passed over.
fn ranges(subtype: ColumnType, discrete: bool) -> Vec<Item> {
    let mut out = Vec::new();
    let ends: Vec<Option<i64>> = vec![None, Some(-1), Some(0), Some(1), Some(2)];
    for lo in &ends {
        for hi in &ends {
            for flags in [0, LB_INC, UB_INC, LB_INC | UB_INC] {
                let made = warren_snail::composed::make_range(
                    subtype,
                    discrete,
                    flags,
                    lo.map(int),
                    hi.map(int),
                );
                if let Ok(r) = made {
                    out.push(r);
                }
            }
        }
    }
    out
}

#[test]
fn p4_a_range_s_key_sorts_as_range_cmp() {
    for (discrete, name) in [(true, "int8range"), (false, "intspan")] {
        let ty = type_of(Composed::Range {
            name: name.into(),
            subtype: ColumnType::Int8,
            discrete,
        });
        let mut values = ranges(ColumnType::Int8, discrete);
        values.push(Item::Range {
            flags: EMPTY,
            lower: None,
            upper: None,
        });
        assert!(values.len() > 40);
        sorts_as(ty, &values, range_cmp);
    }
}

#[test]
fn p4_a_multirange_s_key_sorts_as_multirange_cmp() {
    let range = type_of(Composed::Range {
        name: "intspan".into(),
        subtype: ColumnType::Int8,
        discrete: false,
    });
    let ty = type_of(Composed::Multirange {
        name: "intspans".into(),
        range,
    });
    let rs: Vec<Item> = ranges(ColumnType::Int8, false)
        .into_iter()
        .filter(|r| range_bounds(r).is_some())
        .collect();
    let mut values = vec![Item::Multirange(Vec::new())];
    for a in &rs {
        values.push(Item::Multirange(vec![a.clone()]));
        for b in &rs {
            values.push(Item::Multirange(vec![a.clone(), b.clone()]));
        }
    }
    // `multirange_cmp`: range by range, each by its lower then its upper bound; one that runs out
    // first, first.
    sorts_as(ty, &values, |a, b| {
        let (Item::Multirange(x), Item::Multirange(y)) = (a, b) else {
            unreachable!()
        };
        for (p, q) in x.iter().zip(y) {
            let o = range_cmp(p, q);
            if o != Ordering::Equal {
                return o;
            }
        }
        x.len().cmp(&y.len())
    });
}

#[test]
fn p4_an_enum_s_key_is_its_rank_after_every_add_value() {
    // Seeded runs of ADD VALUE BEFORE and AFTER, the ranks held against the list as the DDL leaves
    // it.
    let mut seed: u64 = 0x5eed_1300;
    let mut next = |n: usize| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % n as u64) as usize
    };
    for run in 0..20 {
        let mut ddl = String::from("CREATE TYPE e AS ENUM ('l0');");
        let mut list = vec!["l0".to_string()];
        for i in 1..12 {
            let label = format!("l{i}");
            let at = next(list.len());
            let before = next(2) == 0;
            let other = list[at].clone();
            ddl.push_str(&format!(
                " ALTER TYPE e ADD VALUE '{label}' {} '{other}';",
                if before { "BEFORE" } else { "AFTER" }
            ));
            list.insert(if before { at } else { at + 1 }, label);
        }
        ddl.push_str(" CREATE TABLE t (id integer PRIMARY KEY, v e);");
        let c = warren_snail::catalog::Catalog::parse(&ddl).unwrap();
        let ty = c.tables[0].columns[1].ty;
        for (rank, label) in list.iter().enumerate() {
            let d = warren_snail::composed::scalar_in(ty, label).unwrap();
            assert_eq!(d, Datum::Int(rank as i64), "run {run}: {label} in {list:?}");
            assert_eq!(text_out(ty, &Item::Scalar(d)), *label);
        }
    }
}

/// Each value written to its leaves, each leaf's pages encoded and decoded, and the values made
/// again from the leaves decoded.
fn round_trip(ty: ColumnType, values: &[Item]) {
    let held: Vec<Option<Arc<Compound>>> = values
        .iter()
        .map(|v| (!v.is_null()).then(|| Arc::new(Compound::new(ty, v.clone()))))
        .collect();
    let column = Values::Composed(held.clone());
    let leaves = column.clone().into_leaves(ty);
    let mut back = Vec::new();
    for (leaf, values) in ty.leaves().iter().zip(&leaves) {
        let desc = warren_snail::encode::leaf_descriptor("v", leaf, true).unwrap();
        for k in [1, 2] {
            let e = warren_snail::encode::encode(&desc, leaf.ty, values, k).unwrap();
            let pages: Vec<&[u8]> = e.pages.iter().map(|p| p.bytes.as_slice()).collect();
            let rows: usize = e.pages.iter().map(|p| p.rows).sum();
            assert_eq!(rows, held.len(), "{}", leaf.name("v"));
            let d = warren_snail::encode::decode_leaf(
                &desc,
                leaf,
                e.dictionary.as_deref(),
                &pages,
                rows,
            )
            .unwrap();
            let (Values::Nested(x), Values::Nested(y)) = (&d, values) else {
                panic!("a leaf of a composed column holds its levels")
            };
            assert_eq!(
                (&x.defs, &x.reps, &x.starts),
                (&y.defs, &y.reps, &y.starts),
                "{}",
                leaf.name("v")
            );
            for e in 0..x.values.len() {
                assert_eq!(
                    warren_snail::values::datum_of(&x.values, e, leaf.ty),
                    warren_snail::values::datum_of(&y.values, e, leaf.ty),
                    "{}",
                    leaf.name("v")
                );
            }
            if k == 1 {
                back.push(d);
            }
        }
    }
    let Values::Composed(again) = Values::from_leaves(ty, back) else {
        panic!("a composed column made again")
    };
    for (a, b) in held.iter().zip(&again) {
        // Each value's own parts back, not only its key.
        assert_eq!(a.as_ref().map(|c| &c.item), b.as_ref().map(|c| &c.item));
    }
}

#[test]
fn p8_every_array_up_to_two_by_two_comes_back_from_its_pages() {
    let ty = type_of(Composed::Array {
        element: ColumnType::Int4,
        domain: None,
    });
    let mut values = arrays();
    values.push(Item::Null);
    round_trip(ty, &values);
}

#[test]
fn p8_composites_with_null_fields_and_all_null_come_back_from_their_pages() {
    let inner = type_of(Composed::Record {
        name: "pair".into(),
        fields: vec![
            Field::new("a", ColumnType::Int4),
            Field::new("b", ColumnType::Text(None)),
        ],
    });
    let ty = type_of(Composed::Record {
        name: "outer".into(),
        fields: vec![
            Field::new("p", inner),
            Field::new(
                "xs",
                type_of(Composed::Array {
                    element: inner,
                    domain: None,
                }),
            ),
        ],
    });
    let pair = |a: Option<i64>, b: Option<&str>| {
        Item::Record(vec![
            a.map_or(Item::Null, int),
            b.map_or(Item::Null, |s| Item::Scalar(Datum::Text(s.into()))),
        ])
    };
    let mut values = vec![Item::Null, Item::Record(vec![Item::Null, Item::Null])];
    for p in [
        Item::Null,
        pair(None, None),
        pair(Some(1), None),
        pair(None, Some("")),
        pair(Some(2), Some("ab")),
    ] {
        for xs in [
            Item::Null,
            Item::Array {
                elements: Vec::new(),
                dims: Vec::new(),
                lower: Vec::new(),
            },
            Item::Array {
                elements: vec![Item::Null, pair(None, None), pair(Some(3), Some("x"))],
                dims: vec![3],
                lower: vec![1],
            },
        ] {
            values.push(Item::Record(vec![p.clone(), xs]));
        }
    }
    round_trip(ty, &values);
}

#[test]
fn p8_ranges_with_every_flag_and_multiranges_come_back_from_their_pages() {
    let range = type_of(Composed::Range {
        name: "intspan".into(),
        subtype: ColumnType::Int8,
        discrete: false,
    });
    let mut values = ranges(ColumnType::Int8, false);
    values.push(Item::Range {
        flags: EMPTY,
        lower: None,
        upper: None,
    });
    values.push(Item::Range {
        flags: LB_INF | UB_INF,
        lower: None,
        upper: None,
    });
    values.push(Item::Null);
    round_trip(range, &values);
    let ty = type_of(Composed::Multirange {
        name: "intspans".into(),
        range,
    });
    let multi: Vec<Item> = vec![
        Item::Null,
        Item::Multirange(Vec::new()),
        text_in(ty, "{[1,2)}").unwrap(),
        text_in(ty, "{(,1), [3,4], (5,)}").unwrap(),
    ];
    round_trip(ty, &multi);
}

#[test]
fn a_composed_value_s_text_is_postgresql_s() {
    let ints = type_of(Composed::Array {
        element: ColumnType::Int4,
        domain: None,
    });
    let texts = type_of(Composed::Array {
        element: ColumnType::Text(None),
        domain: None,
    });
    // Each as PostgreSQL 18.6 wrote it (28818, 2026-10-30), from the spelling beside it.
    for (ty, spelled, written) in [
        (ints, " { 1 , 2 } ", "{1,2}"),
        (ints, "[0:1]={7,NULL}", "[0:1]={7,NULL}"),
        (ints, "{{1,2},{3,4}}", "{{1,2},{3,4}}"),
        (ints, "{{}}", "{}"),
        (ints, "{0x10, -0b11}", "{16,-3}"),
        (
            texts,
            r#"{x,"NULL",NULL,"", "a b", a\,b}"#,
            r#"{x,"NULL",NULL,"","a b","a,b"}"#,
        ),
        (texts, r#"{"a\"b","a\\b"}"#, r#"{"a\"b","a\\b"}"#),
    ] {
        let item = text_in(ty, spelled).unwrap();
        assert_eq!(text_out(ty, &item), written, "{spelled}");
    }
    for refused in [
        "{1,2",
        "{{1,2},{3}}",
        "{1}x",
        "[1:2]={1}",
        "{1,{2}}",
        "[2:1]={}",
    ] {
        assert!(text_in(ints, refused).is_err(), "{refused}");
    }
    let range = type_of(Composed::Range {
        name: "int4range".into(),
        subtype: ColumnType::Int4,
        discrete: true,
    });
    let multi = type_of(Composed::Multirange {
        name: "int4multirange".into(),
        range,
    });
    let pair = type_of(Composed::Record {
        name: "pair".into(),
        fields: vec![
            Field::new("a", ColumnType::Int4),
            Field::new("b", ColumnType::Text(None)),
        ],
    });
    // Each as PostgreSQL 18.6 wrote it, as the ledger of time, ranges and composed types measured.
    for (ty, spelled, written) in [
        (multi, "{[1,3),[3,5)}", "{[1,5)}"),
        (multi, "{[1,3),[2,5)}", "{[1,5)}"),
        (multi, "{[5,6),[1,2)}", "{[1,2),[5,6)}"),
        (multi, "{empty,[1,2)}", "{[1,2)}"),
        (range, "[1,3]", "[1,4)"),
        (range, "(0,3]", "[1,4)"),
        (range, "(1,2)", "empty"),
        (pair, r#"(2,"x,""y""")"#, r#"(2,"x,""y""")"#),
        (pair, r#"(5,"\\")"#, r#"(5,"\\")"#),
    ] {
        let item = text_in(ty, spelled).unwrap();
        assert_eq!(text_out(ty, &item), written, "{spelled}");
    }
    // A statement's quoted string written again as it was read.
    let split =
        warren_snail::read::statement::split(r#"INSERT INTO s.t VALUES ('{"it''s"}'); SELECT 1"#)
            .unwrap();
    assert_eq!(split[0], r#"INSERT INTO s.t VALUES ('{"it''s"}')"#);
}

mod weather;

/// The rows of each block of an order, by one of its leaves.
fn block_rows(path: &std::path::Path, order: &str, leaf: &str) -> Vec<u64> {
    warren_snail::read::Snail::open(path)
        .unwrap()
        .blocks()
        .unwrap()
        .iter()
        .filter(|r| r.order == order && r.column == leaf)
        .map(|r| r.rows)
        .collect()
}

/// A snail of one table, `s.t`, its rows given column by column.
fn snail_of(name: &str, ddl: &str, columns: Vec<Values>) -> std::path::PathBuf {
    let path = weather::tmp(&format!("{name}.snail"));
    let catalog = warren_snail::catalog::Catalog::parse(ddl).unwrap();
    weather::write(
        &path,
        &catalog,
        &[("s.t".into(), warren_snail::write::Rows { columns })],
    )
    .unwrap();
    path
}

#[test]
fn r8_a_repeated_leaf_decides_the_cut_by_its_values_bytes_and_the_shape_never() {
    let ty = type_of(Composed::Array {
        element: ColumnType::Int4,
        domain: None,
    });
    let ddl = "CREATE TABLE s.t (id bigint NOT NULL, v integer[] NOT NULL, PRIMARY KEY (id));";
    let column = |spelled: &dyn Fn(i64) -> String| {
        Values::Composed(
            (0..8192)
                .map(|i| {
                    Some(Arc::new(Compound::new(
                        ty,
                        text_in(ty, &spelled(i)).unwrap(),
                    )))
                })
                .collect(),
        )
    };
    let ids = || Values::Int((0..8192).map(Some).collect());
    // One element a row, 4 bytes: 1,024 rows fill a page, where the bigint key fills one at 512.
    let one = snail_of("r8-one", ddl, vec![ids(), column(&|i| format!("{{{i}}}"))]);
    assert_eq!(
        block_rows(&one, "t_pkey", "v.elements.list.element"),
        [1024; 8]
    );
    // Two a row, 8 bytes: 512.
    let two = snail_of(
        "r8-two",
        ddl,
        vec![ids(), column(&|i| format!("{{{i},{i}}}"))],
    );
    assert_eq!(
        block_rows(&two, "t_pkey", "v.elements.list.element"),
        [512; 16]
    );
    // Two a row in two dimensions, the shape written in every row beside them: the shape never
    // decides, so the cut is the one-dimensional array's.
    let shaped = snail_of(
        "r8-shaped",
        ddl,
        vec![ids(), column(&|i| format!("{{{{{i}}},{{{i}}}}}"))],
    );
    assert_eq!(
        block_rows(&shaped, "t_pkey", "v.elements.list.element"),
        [512; 16]
    );
    assert_eq!(
        block_rows(&shaped, "t_pkey", "v.dims.list.element"),
        [512; 16]
    );
}

/// 4,096 arrays of one element spread over its 32 bits, the table ordered on them.
fn composed_level(name: &str) -> std::path::PathBuf {
    let ty = type_of(Composed::Array {
        element: ColumnType::Int4,
        domain: None,
    });
    let ddl = "CREATE TABLE s.t (id integer NOT NULL, v integer[] NOT NULL, PRIMARY KEY (v, id));";
    snail_of(
        name,
        ddl,
        vec![
            Values::Int((0..4096).map(Some).collect()),
            Values::Composed(
                (0..4096i64)
                    .map(|i| {
                        let v = ((i << 20) as i32).to_string();
                        Some(Arc::new(Compound::new(
                            ty,
                            text_in(ty, &format!("{{{v}}}")).unwrap(),
                        )))
                    })
                    .collect(),
            ),
        ],
    )
}

#[test]
fn a_composed_level_is_cut_on_its_key_and_read_along_it() {
    let path = composed_level("composed-level");
    let blocks = block_rows(&path, "t_pkey", "id");
    assert!(blocks.len() > 1, "{blocks:?}");
    assert_eq!(blocks.iter().sum::<u64>(), 4096);
    let mut snail = warren_snail::read::Snail::open(&path).unwrap();
    for (q, n) in [
        ("SELECT id FROM s.t WHERE v = '{0}'", 1),
        ("SELECT id FROM s.t WHERE v < '{0}'", 2048),
        ("SELECT id FROM s.t WHERE v >= '{1048576}'", 2047),
    ] {
        let (batch, round) = snail.ask(q, None).unwrap();
        assert_eq!(batch.num_rows(), n, "{q}");
        weather::exact(q, &round);
    }
    // Equality fixes the level: one block read.
    let (_, round) = snail
        .ask("SELECT id FROM s.t WHERE v = '{0}'", None)
        .unwrap();
    assert_eq!(round.blocks_read, 1);
}

/// The text of each value of an answer's composed column, read back as the pipeline reads it.
fn texts_of(b: &arrow::array::RecordBatch, c: usize) -> Vec<Option<String>> {
    let f = b.schema().field(c).clone();
    let ty = warren_snail::composed::field_composed(&f).expect("a composed field");
    (0..b.num_rows())
        .map(|r| {
            warren_snail::composed::compound_at(ty, b.column(c).as_ref(), r)
                .map(|v| text_out(ty, &v.item))
        })
        .collect()
}

const MIXED: &str = "CREATE TYPE s.pair AS (a integer, b text); \
    CREATE TABLE s.t (id integer NOT NULL, a integer[], p s.pair, r int4range, m int4multirange, PRIMARY KEY (id)); \
    CREATE INDEX t_a ON s.t (a, id);";

fn mixed(name: &str) -> std::path::PathBuf {
    let c = warren_snail::catalog::Catalog::parse(MIXED).unwrap();
    let t = &c.tables[0];
    let col = |i: usize, spelled: &[Option<&str>]| {
        let ty = t.columns[i].ty;
        Values::Composed(
            spelled
                .iter()
                .map(|s| s.map(|s| Arc::new(Compound::new(ty, text_in(ty, s).unwrap()))))
                .collect(),
        )
    };
    snail_of(
        name,
        MIXED,
        vec![
            Values::Int((1..=4).map(Some).collect()),
            col(
                1,
                &[Some("{1,2}"), None, Some("[0:1]={7,NULL}"), Some("{}")],
            ),
            col(2, &[Some("(1,\"a b\")"), Some("(,)"), None, Some("(2,)")]),
            col(3, &[Some("[1,5)"), Some("empty"), None, Some("(,3]")]),
            col(
                4,
                &[Some("{[1,3),[5,7)}"), Some("{}"), None, Some("{(,1)}")],
            ),
        ],
    )
}

#[test]
fn a_composed_table_goes_out_as_arrow_answers_it_and_comes_back_by_its_pages() {
    let path = mixed("composed-out");
    let mut snail = warren_snail::read::Snail::open(&path).unwrap();
    let (answer, _) = snail
        .ask("SELECT id, a, p, r, m FROM s.t ORDER BY id", None)
        .unwrap();
    let dir = weather::tmp("composed-out-parquet");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let outs = snail.out(&dir).unwrap();
    let (batches, _) = weather::parquet_of(&outs[0].file);
    // A repeated leaf's chunk counts its values, an entry of its levels each, apart from its rows.
    {
        use parquet::file::reader::{FileReader, SerializedFileReader};
        let reader =
            SerializedFileReader::new(std::fs::File::open(&outs[0].file).unwrap()).unwrap();
        let rg = reader.metadata().row_group(0);
        let at = (0..rg.num_columns())
            .find(|&i| rg.column(i).column_path().string() == "a.elements.list.element")
            .unwrap();
        assert_eq!((rg.num_rows(), rg.column(at).num_values()), (4, 6));
    }
    // The page form as arrow's Parquet reader gives it is the answer's Arrow type (R3).
    for c in 1..5 {
        assert_eq!(
            batches[0].schema().field(c).data_type(),
            answer.schema().field(c).data_type(),
            "{}",
            answer.schema().field(c).name()
        );
    }
    assert_eq!(
        texts_of(&answer, 1),
        [
            Some("{1,2}".into()),
            None,
            Some("[0:1]={7,NULL}".into()),
            Some("{}".into())
        ]
    );
    assert_eq!(
        texts_of(&answer, 2),
        [
            Some("(1,\"a b\")".into()),
            Some("(,)".into()),
            None,
            Some("(2,)".into())
        ]
    );
    assert_eq!(
        texts_of(&answer, 3),
        [
            Some("[1,5)".into()),
            Some("empty".into()),
            None,
            Some("(,4)".into())
        ]
    );
    assert_eq!(
        texts_of(&answer, 4),
        [
            Some("{[1,3),[5,7)}".into()),
            Some("{}".into()),
            None,
            Some("{(,1)}".into())
        ]
    );
    // Back in by its pages, the same answer.
    let catalog = warren_snail::catalog::Catalog::parse(MIXED).unwrap();
    let rows = warren_snail::write::Rows::from_batches(&catalog.tables[0], &batches).unwrap();
    let back = weather::tmp("composed-out-back.snail");
    weather::write(&back, &catalog, &[("s.t".into(), rows)]).unwrap();
    let (again, _) = warren_snail::read::Snail::open(&back)
        .unwrap()
        .ask("SELECT id, a, p, r, m FROM s.t ORDER BY id", None)
        .unwrap();
    for c in 1..5 {
        assert_eq!(texts_of(&again, c), texts_of(&answer, c));
    }
}

#[cfg(feature = "standalone")]
#[test]
fn a_composed_level_takes_a_row_into_the_block_its_key_falls_in() {
    let path = composed_level("composed-level-writes");
    let mut w = warren_snail::read::Snail::open_writing(&path).unwrap();
    for sql in [
        "INSERT INTO s.t (id, v) VALUES (5000, '{1}')",
        "INSERT INTO s.t (id, v) SELECT 5001, v FROM s.t WHERE id = 1",
        "DELETE FROM s.t WHERE v = '{0}'",
    ] {
        w.execute(sql).unwrap();
    }
    drop(w);
    let mut snail = warren_snail::read::Snail::open(&path).unwrap();
    for (q, n) in [
        ("SELECT id FROM s.t WHERE v = '{1}'", 1),
        ("SELECT id FROM s.t WHERE v = '{1048576}'", 2),
        ("SELECT id FROM s.t WHERE v = '{0}'", 0),
        ("SELECT id FROM s.t WHERE v < '{0}'", 2048),
        ("SELECT id FROM s.t WHERE v > '{0}'", 2049),
    ] {
        let (batch, round) = snail.ask(q, None).unwrap();
        assert_eq!(batch.num_rows(), n, "{q}");
        weather::exact(q, &round);
    }
}

#[cfg(feature = "standalone")]
#[test]
fn a_composed_level_takes_rows_into_its_tail_and_by_update_and_delete() {
    let path = mixed("composed-writes");
    let mut w = warren_snail::read::Snail::open_writing(&path).unwrap();
    for sql in [
        "INSERT INTO s.t (id, a, p) VALUES (5, '{1,3}', ROW(3, 'c'))",
        "INSERT INTO s.t (id, a, r) VALUES (6, ARRAY[0], int4range(2, 4, '[]'))",
        "UPDATE s.t SET a = '{9}' WHERE id = 1",
        "DELETE FROM s.t WHERE a = '{}'",
    ] {
        w.execute(sql).unwrap();
    }
    drop(w);
    let mut snail = warren_snail::read::Snail::open(&path).unwrap();
    let (b, _) = snail
        .ask("SELECT id, a FROM s.t ORDER BY a, id", Some("t_a"))
        .unwrap();
    let ids: Vec<i64> = weather::ints64(b.column(0).as_ref())
        .into_iter()
        .flatten()
        .collect();
    // {0} < {1,3} < [0:1]={7,NULL} < {9}, then the NULL.
    assert_eq!(ids, [6, 5, 3, 1, 2]);
    let (b, _) = snail
        .ask("SELECT id, r FROM s.t WHERE id = 6", None)
        .unwrap();
    assert_eq!(texts_of(&b, 1)[0].as_deref(), Some("[2,5)"));
    let (b, round) = snail
        .ask("SELECT id FROM s.t WHERE a = '{1,3}'", Some("t_a"))
        .unwrap();
    assert_eq!(b.num_rows(), 1);
    weather::exact("a = '{1,3}'", &round);
}

#[cfg(feature = "standalone")]
#[test]
fn a_domain_s_constraints_are_met_at_the_write_its_arrays_and_fields_too() {
    let ddl = "CREATE DOMAIN s.posint AS integer CHECK (VALUE > 0); \
        CREATE DOMAIN s.nn AS s.posint NOT NULL DEFAULT 5; \
        CREATE TYPE s.dp AS (a s.posint, b text); \
        CREATE TABLE s.t (id integer NOT NULL, x s.posint, y s.nn, z s.posint[], w s.dp, PRIMARY KEY (id));";
    let path = weather::tmp("composed-domains.snail");
    let c = warren_snail::catalog::Catalog::parse(ddl).unwrap();
    weather::write(&path, &c, &[]).unwrap();
    let mut w = warren_snail::read::Snail::open_writing(&path).unwrap();
    for (sql, refused) in [
        ("INSERT INTO s.t (id, x) VALUES (1, 3)", None),
        (
            "INSERT INTO s.t (id, x) VALUES (2, 0)",
            Some("value for domain s.posint violates check constraint"),
        ),
        (
            "INSERT INTO s.t (id, y) VALUES (3, NULL)",
            Some("domain s.nn does not allow null values"),
        ),
        ("INSERT INTO s.t (id, z) VALUES (4, '{1,NULL}')", None),
        (
            "INSERT INTO s.t (id, z) VALUES (5, '{1,-2}')",
            Some("value for domain s.posint"),
        ),
        (
            "INSERT INTO s.t (id, w) VALUES (6, '(0,a)')",
            Some("value for domain s.posint"),
        ),
        ("INSERT INTO s.t (id, w) VALUES (7, ROW(4, 'b'))", None),
        (
            "UPDATE s.t SET x = -1 WHERE id = 1",
            Some("value for domain s.posint"),
        ),
    ] {
        match (w.execute(sql), refused) {
            (Ok(_), None) => {}
            (Err(e), Some(why)) => assert!(e.to_string().contains(why), "{sql}: {e}"),
            (r, _) => panic!("{sql}: {r:?}"),
        }
    }
    drop(w);
    let (b, _) = warren_snail::read::Snail::open(&path)
        .unwrap()
        .ask("SELECT id, x, y FROM s.t ORDER BY id", None)
        .unwrap();
    // The domain's DEFAULT fills y where the row gives none.
    let ys: Vec<Option<i64>> = weather::ints64(b.column(2).as_ref());
    assert_eq!(ys, [Some(5), Some(5), Some(5)]);
}

#[test]
fn a_composed_value_s_operators_answer_as_postgresql_s() {
    let path = mixed("composed-operators");
    let mut snail = warren_snail::read::Snail::open(&path).unwrap();
    let step = "WITH q AS (SELECT id, a, p, r FROM s.t)";
    // Each answer as PostgreSQL 18.6 gave it (28818, 2026-10-30), the rows by id.
    for (q, want) in [
        (
            "SELECT id, (a || 3)::text AS x FROM q ORDER BY id",
            "{1,2,3}|{3}|[0:2]={7,NULL,3}|{3}",
        ),
        (
            "SELECT id, (0 || a)::text AS x FROM q ORDER BY id",
            "{0,1,2}|{0}|[0:2]={0,7,NULL}|{0}",
        ),
        (
            "SELECT id, (a || '{5,6}')::text AS x FROM q ORDER BY id",
            "{1,2,5,6}|{5,6}|[0:3]={7,NULL,5,6}|{5,6}",
        ),
        (
            "SELECT id, (a @> '{2}')::text AS x FROM q ORDER BY id",
            "true||false|false",
        ),
        (
            "SELECT id, (a <@ '{1,2,3}')::text AS x FROM q ORDER BY id",
            "true||false|true",
        ),
        (
            "SELECT id, (a && '{7,1}')::text AS x FROM q ORDER BY id",
            "true||true|false",
        ),
        (
            "SELECT id, (2 = ANY(a))::text AS x FROM q ORDER BY id",
            "true|||false",
        ),
        (
            "SELECT id, (2 <> ALL(a))::text AS x FROM q ORDER BY id",
            "false|||true",
        ),
        (
            "SELECT id, a[0]::text AS x, a[1]::text AS y FROM q ORDER BY id",
            ",1|,|7,|,",
        ),
        ("SELECT id, (p).a::text AS x FROM q ORDER BY id", "1|||2"),
        (
            "SELECT id, (r @> 3)::text AS x FROM q ORDER BY id",
            "true|false||true",
        ),
        (
            "SELECT id, (r && '[4,9)')::text AS x FROM q ORDER BY id",
            "true|false||false",
        ),
        (
            "SELECT id, (r << '[6,9)')::text AS x, (r >> '[-9,0)')::text AS y FROM q ORDER BY id",
            "true,true|false,false|,|true,false",
        ),
        (
            "SELECT id, (r &< '[0,4)')::text AS x, (r &> '[2,3)')::text AS y FROM q ORDER BY id",
            "false,false|false,false|,|true,false",
        ),
        (
            "SELECT id, (r -|- '[5,8)')::text AS x FROM q ORDER BY id",
            "true|false||false",
        ),
        (
            "SELECT id, (r + '[4,6)')::text AS x FROM q WHERE id = 1",
            "[1,6)",
        ),
        (
            "SELECT id, (r * '[3,9)')::text AS x FROM q ORDER BY id",
            "[3,5)|empty||[3,4)",
        ),
        (
            "SELECT id, (r - '[3,9)')::text AS x FROM q ORDER BY id",
            "[1,3)|empty||(,3)",
        ),
    ] {
        let (b, _) = snail.ask(&format!("{step} {q}"), None).unwrap();
        let rows: Vec<String> = (0..b.num_rows())
            .map(|r| {
                (1..b.num_columns())
                    .map(|c| {
                        let a = b.column(c);
                        if a.is_null(r) {
                            String::new()
                        } else {
                            arrow::util::display::array_value_to_string(a, r).unwrap()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .collect();
        assert_eq!(rows.join("|"), want, "{q}");
    }
    // A composite is NULL where every field is; NOT NULL where none is.
    let ids = |sql: &str, snail: &mut warren_snail::read::Snail| -> Vec<i64> {
        let (b, _) = snail.ask(sql, None).unwrap();
        weather::ints64(b.column(0).as_ref())
            .into_iter()
            .flatten()
            .collect()
    };
    assert_eq!(
        ids("SELECT id FROM s.t WHERE p IS NULL ORDER BY id", &mut snail),
        [2, 3]
    );
    assert_eq!(
        ids(
            "SELECT id FROM s.t WHERE p IS NOT NULL ORDER BY id",
            &mut snail
        ),
        [1]
    );
    // min and max of an array by its key, and its NULLs counted with no column page read.
    let (b, _) = snail
        .ask("SELECT min(a) AS x, max(a) AS y FROM s.t", None)
        .unwrap();
    assert_eq!(texts_of(&b, 0), [Some("{}".to_string())]);
    assert_eq!(texts_of(&b, 1), [Some("[0:1]={7,NULL}".to_string())]);
    let (b, round) = snail
        .ask("SELECT count(*) FROM s.t WHERE a IS NULL", None)
        .unwrap();
    assert_eq!(weather::ints64(b.column(0).as_ref()), [Some(1)]);
    assert_eq!(round.column_pages_read, 0);
    for refused in [
        "SELECT id FROM s.t WHERE p = '(1,x)'",
        "SELECT id, (r + '[20,30)')::text AS x FROM q WHERE id = 1",
        "SELECT min(r) AS x FROM q",
    ] {
        let sql = if refused.contains(" q ") {
            format!("{step} {refused}")
        } else {
            refused.to_string()
        };
        assert!(snail.ask(&sql, None).is_err(), "{refused}");
    }
}

#[test]
#[cfg(feature = "standalone")]
fn a_snail_of_format_eight_refuses_a_composed_column_by_name() {
    for (ddl, why) in [
        (
            "CREATE TABLE s.k (id integer NOT NULL, a integer[], PRIMARY KEY (id));",
            "s.k.a is integer[], a group of 3 leaves",
        ),
        (
            "CREATE TABLE s.k (id integer NOT NULL, r int4range, PRIMARY KEY (id));",
            "s.k.r is int4range, a group of 3 leaves",
        ),
        (
            "CREATE TYPE s.m AS ENUM ('a'); CREATE TABLE s.k (id integer NOT NULL, m s.m, PRIMARY KEY (id));",
            "which a snail of format 8",
        ),
    ] {
        let catalog = warren_snail::catalog::Catalog::parse(ddl).unwrap();
        let path = weather::tmp("composed-eight.snail");
        let e = warren_snail::write::write_format_eight(&path, &catalog, &[], weather::span())
            .err()
            .unwrap()
            .to_string();
        assert!(e.contains(why) && e.contains("`out`"), "{e}");
    }
}
