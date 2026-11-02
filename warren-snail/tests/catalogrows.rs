//! The catalog's vectors and row types: `int2vector` and `oidvector`, arrays counted from 0 in
//! PostgreSQL's own text and order, and each composite in pg_catalog, held with the fields of the
//! release the file records. Each expected text is PostgreSQL's, read on 18.6 and 19 RC1
//! (`ledgers/catalogrows.md`).

use std::cmp::Ordering;
use std::sync::Arc;
use warren_snail::catalog::{Catalog, ColumnType};
use warren_snail::composed::{
    array_order_key, built_in, key, text_in, text_out, type_of, Composed, Compound, Item,
};
use warren_snail::lists::{all, Kind};
use warren_snail::read::statement::Datum;
use warren_snail::rowtypes::{is_row_type, row_type_at};
use warren_snail::values::Values;

mod weather;

fn int2vector() -> ColumnType {
    built_in("int2vector").unwrap()
}

fn oidvector() -> ColumnType {
    built_in("oidvector").unwrap()
}

fn array_of(element: ColumnType) -> ColumnType {
    type_of(Composed::Array {
        element,
        domain: None,
    })
}

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
        assert_eq!(a.as_ref().map(|c| &c.item), b.as_ref().map(|c| &c.item));
        let c = warren_snail::composed::compound_at(ty, answer.as_ref(), r);
        assert_eq!(a.as_ref().map(|c| &c.item), c.as_ref().map(|c| &c.item));
    }
}

#[test]
fn a_vector_s_text_is_read_and_written_as_postgresql_s() {
    // Each spelling and what PostgreSQL prints.
    for (ty, read, printed) in [
        (int2vector(), "1 2 3", "1 2 3"),
        (int2vector(), " -7  8 ", "-7 8"),
        (int2vector(), "+1", "1"),
        (int2vector(), "", ""),
        (int2vector(), "   ", ""),
        (int2vector(), "1\t ", ""),
        (oidvector(), "4 5", "4 5"),
        (oidvector(), "-1", "4294967295"),
        (oidvector(), "010 0x10", "8 16"),
        (oidvector(), "1\t2", "1 2"),
        (oidvector(), "", ""),
    ] {
        match text_in(ty, read) {
            Ok(item) if !printed.is_empty() || read.trim().is_empty() => {
                assert_eq!(text_out(ty, &item), printed, "{read:?}");
                let Item::Array { dims, lower, .. } = &item else {
                    panic!("a vector is an array")
                };
                assert_eq!(lower, &[0], "{read:?}");
                assert_eq!(dims.len(), 1, "{read:?}");
            }
            // int2vectorin takes only a blank or nothing after a number.
            Err(e) => assert_eq!(read, "1\t ", "{e}"),
            Ok(item) => panic!("{read:?} read as {}", text_out(ty, &item)),
        }
    }
    for (ty, read, words) in [
        (
            int2vector(),
            "1,2",
            "invalid input syntax for type smallint: \"1,2\"",
        ),
        (
            int2vector(),
            "32768",
            "value \"32768\" is out of range for type smallint",
        ),
        (
            int2vector(),
            "0x10",
            "invalid input syntax for type smallint: \"0x10\"",
        ),
        (
            int2vector(),
            "1_000",
            "invalid input syntax for type smallint: \"1_000\"",
        ),
        (
            int2vector(),
            "1\t2",
            "invalid input syntax for type smallint: \"1\t2\"",
        ),
        (oidvector(), "1,2", "invalid input syntax for type oid: \",2\""),
        (
            oidvector(),
            "4294967296",
            "value \"4294967296\" is out of range for type oid",
        ),
        (oidvector(), "a", "invalid input syntax for type oid: \"a\""),
        (oidvector(), "1a", "invalid input syntax for type oid: \"a\""),
    ] {
        let e = text_in(ty, read).unwrap_err().to_string();
        assert_eq!(e, words, "{read:?}");
    }
    // As an array's element it is quoted where its blanks ask.
    let a = array_of(int2vector());
    let item = text_in(a, "{\"1 2\",3,NULL}").unwrap();
    assert_eq!(text_out(a, &item), "{\"1 2\",3,NULL}");
}

fn vectors(element: i64) -> Vec<Item> {
    let mut out = vec![Vec::new()];
    for x in 0..3 {
        out.push(vec![x * element]);
        for y in 0..3 {
            out.push(vec![x * element, y * element]);
        }
    }
    out.into_iter()
        .map(|v| Item::Array {
            dims: vec![v.len() as i32],
            lower: vec![0],
            elements: v.into_iter().map(|x| Item::Scalar(Datum::Int(x))).collect(),
        })
        .collect()
}

fn elements(i: &Item) -> Vec<i64> {
    let Item::Array { elements, .. } = i else {
        panic!("a vector is an array")
    };
    elements
        .iter()
        .map(|e| match e {
            Item::Scalar(Datum::Int(x)) => *x,
            other => panic!("{other:?} in a vector"),
        })
        .collect()
}

#[test]
fn an_oidvector_orders_by_its_length_first_and_an_int2vector_as_an_array() {
    let oids: Vec<Item> = vectors(1 << 20)
        .into_iter()
        .map(|i| match i {
            Item::Array {
                elements,
                dims,
                lower,
            } => Item::Array {
                elements: elements
                    .into_iter()
                    .map(|e| match e {
                        Item::Scalar(Datum::Int(x)) => {
                            Item::Scalar(Datum::Int(warren_snail::scalars::unsigned_key(x as u64, 32)))
                        }
                        e => e,
                    })
                    .collect(),
                dims,
                lower,
            },
            other => other,
        })
        .collect();
    let by_oid = |a: &Item, b: &Item| -> Ordering {
        let (x, y) = (elements(a), elements(b));
        x.len().cmp(&y.len()).then(x.cmp(&y))
    };
    for a in &oids {
        for b in &oids {
            assert_eq!(key(oidvector(), a).cmp(&key(oidvector(), b)), by_oid(a, b));
            // min and max of it are array_smaller's and array_larger's.
            let (x, y) = (
                Compound::new(oidvector(), a.clone()),
                Compound::new(oidvector(), b.clone()),
            );
            let array = elements(a).cmp(&elements(b));
            assert_eq!(
                array_order_key(oidvector(), &x).cmp(&array_order_key(oidvector(), &y)),
                array
            );
        }
    }
    let small = vectors(1);
    for a in &small {
        for b in &small {
            assert_eq!(
                key(int2vector(), a).cmp(&key(int2vector(), b)),
                elements(a).cmp(&elements(b)),
            );
        }
    }
    // PostgreSQL: '1 2'::oidvector > '3'::oidvector, and '1 2'::int2vector < '3'::int2vector.
    let (a, b) = (
        text_in(oidvector(), "1 2").unwrap(),
        text_in(oidvector(), "3").unwrap(),
    );
    assert!(key(oidvector(), &a) > key(oidvector(), &b));
    let (a, b) = (
        text_in(int2vector(), "1 2").unwrap(),
        text_in(int2vector(), "3").unwrap(),
    );
    assert!(key(int2vector(), &a) < key(int2vector(), &b));
}

#[test]
fn a_vector_comes_back_from_its_one_leaf_counted_from_0() {
    assert_eq!(int2vector().leaves().len(), 1);
    assert_eq!(oidvector().leaves().len(), 1);
    for (ty, texts) in [
        (int2vector(), ["1 2 3", "", "-32768 32767"]),
        (oidvector(), ["4 5", "", "4294967295 0"]),
    ] {
        let mut values: Vec<Item> = texts.iter().map(|t| text_in(ty, t).unwrap()).collect();
        values.push(Item::Null);
        round_trip(ty, &values);
        let a = array_of(ty);
        let arrays = [
            text_in(a, &format!("{{\"{}\",NULL}}", texts[0])).unwrap(),
            text_in(a, "{}").unwrap(),
            Item::Null,
        ];
        round_trip(a, &arrays);
    }
}

#[test]
fn every_row_type_of_every_release_is_held_as_its_catalog_s_or_refused_for_its_pseudo_type() {
    let refused = [
        ("pg_attribute", "column \"attmissingval\" has pseudo-type anyarray"),
        ("pg_statistic", "column \"stavalues1\" has pseudo-type anyarray"),
        (
            "pg_statistic_ext_data",
            "column \"stavalues1\" has pseudo-type anyarray",
        ),
        ("pg_stats", "column \"most_common_vals\" has pseudo-type anyarray"),
        (
            "pg_stats_ext_exprs",
            "column \"most_common_vals\" has pseudo-type anyarray",
        ),
    ];
    // The census's count of each release's composites in pg_catalog.
    let counts = [134, 139, 140, 142, 144, 150];
    for (release, count) in (14u8..=19).zip(counts) {
        let listed: Vec<&str> = all(Kind::Type)
            .iter()
            .filter(|t| t.in_release(release) && t.family == "composite types")
            .map(|t| t.name)
            .collect();
        assert_eq!(listed.len(), count, "{release}");
        for name in listed {
            assert!(is_row_type(name), "{name}");
            let r = row_type_at(name, release);
            match refused.iter().find(|(n, _)| *n == name) {
                Some((_, words)) => {
                    assert_eq!(r.unwrap_err().to_string(), *words, "{name}");
                }
                None => {
                    let ty = r.unwrap_or_else(|e| panic!("{name} of {release}: {e}"));
                    let ColumnType::Composed(t) = ty else {
                        panic!("{name} is a composite")
                    };
                    let Composed::Record { name: n, fields } = t.def() else {
                        panic!("{name} is a composite")
                    };
                    assert_eq!(n, name);
                    assert!(!fields.is_empty(), "{name}");
                    assert_eq!(ty.pg_name(), name);
                    // Its array too, and a DDL naming each under the release.
                    let ddl = format!("CREATE TABLE s.t (k integer PRIMARY KEY, c {name}, d {name}[]);");
                    let mut catalog = Catalog::parse(&ddl).unwrap();
                    catalog.with_release(release).unwrap();
                    assert_eq!(catalog.tables[0].columns[1].ty, ty, "{name} of {release}");
                }
            }
        }
    }
    let probed = warren_snail::lists::types(19);
    for (name, words) in refused {
        for n in [name.to_string(), format!("{name}[]")] {
            let p = probed.iter().find(|p| p.listed.name == n).unwrap();
            let why = p.refused.as_deref().unwrap_or_default();
            assert!(why.contains(words), "{n}: {why}");
        }
    }
    assert_eq!(
        probed
            .iter()
            .filter(|p| p.listed.family == "composite types" && p.refused.is_none())
            .count(),
        145
    );
}

#[test]
fn a_row_type_a_release_does_not_define_is_refused_under_it() {
    let mut catalog = Catalog::parse("CREATE TABLE s.t (k integer PRIMARY KEY, c pg_aios);").unwrap();
    let e = catalog.with_release(17).unwrap_err().to_string();
    assert!(
        e.contains("pg_aios is not in PostgreSQL 17's catalog"),
        "{e}"
    );
    catalog.with_release(18).unwrap();
    // One only an older release defines is read under it.
    let name = all(Kind::Type)
        .iter()
        .find(|t| t.family == "composite types" && !t.in_release(19))
        .map(|t| t.name);
    if let Some(name) = name {
        let mut catalog =
            Catalog::parse(&format!("CREATE TABLE s.t (k integer PRIMARY KEY, c {name});")).unwrap();
        assert!(catalog.with_release(19).is_err(), "{name}");
    }
}

#[test]
fn a_type_of_the_file_s_release_reaches_every_part_that_names_a_catalog() {
    // The name of OID 3813 is another function's by release, through an array and a row type.
    let ddl = "CREATE TABLE s.t (k integer PRIMARY KEY, a regproc[], c pg_am, d pg_am[]);";
    for (release, shown) in [
        (14u8, "pg_is_in_backup"),
        (15, "3813"),
        (17, "xmltext"),
    ] {
        let mut catalog = Catalog::parse(ddl).unwrap();
        catalog.with_release(release).unwrap();
        let ty = catalog.tables[0].columns[1].ty;
        let item = text_in(ty, "{3813}").unwrap();
        assert_eq!(text_out(ty, &item), format!("{{{shown}}}"), "{release}");
        let ty = catalog.tables[0].columns[2].ty;
        let item = text_in(ty, "(1,a,3813,i)").unwrap();
        assert_eq!(text_out(ty, &item), format!("(1,a,{shown},i)"), "{release}");
        let ty = catalog.tables[0].columns[3].ty;
        let item = text_in(ty, "{\"(1,a,3813,i)\"}").unwrap();
        assert_eq!(
            text_out(ty, &item),
            format!("{{\"(1,a,{shown},i)\"}}"),
            "{release}"
        );
    }
}

#[cfg(feature = "standalone")]
mod written {
    use super::*;
    use warren_snail::read::Snail;
    use warren_snail::write::Rows;
    use weather::{ints64, texts, tmp};

    fn snail_at(name: &str, ddl: &str, release: u8, tables: &[(String, Rows)]) -> std::path::PathBuf {
        let path = tmp(name);
        let catalog = Catalog::parse(ddl).unwrap();
        warren_snail::write::write_released(&path, &catalog, tables, &[], weather::span(), release).unwrap();
        path
    }

    fn asked(path: &std::path::Path, q: &str) -> Vec<Option<String>> {
        let (b, _) = Snail::open(path).unwrap().ask(q, None).unwrap();
        texts(b.column(0).as_ref())
    }

    fn refused(path: &std::path::Path, q: &str) -> String {
        Snail::open(path).unwrap().ask(q, None).unwrap_err().to_string()
    }

    #[test]
    fn a_vector_column_is_subscripted_from_0_and_cast_as_postgresql_casts_it() {
        let path = snail_at(
            "vector-subscripts.snail",
            "CREATE TABLE s.t (id integer PRIMARY KEY, a int2vector, o oidvector);",
            19,
            &[],
        );
        let mut w = Snail::open_writing(&path).unwrap();
        w.execute("INSERT INTO s.t VALUES (1, '1 2 3', '4 5'), (2, '', ''), (3, NULL, NULL)")
            .unwrap();
        drop(w);
        let step = "WITH t AS (SELECT id, a, o FROM s.t)";
        let at = |e: &str| format!("{step} SELECT ({e})::text AS x FROM t ORDER BY id");
        for (e, want) in [
            ("a", [Some("1 2 3"), Some(""), None]),
            ("a[0]", [Some("1"), None, None]),
            ("o[1]", [Some("5"), None, None]),
            ("a[-1]", [None, None, None]),
            ("a[0:1]", [Some("{1,2}"), Some("{}"), None]),
            ("a::smallint[]", [Some("[0:2]={1,2,3}"), Some("{}"), None]),
            ("o::oid[]", [Some("[0:1]={4,5}"), Some("{}"), None]),
            ("a::integer[]", [Some("[0:2]={1,2,3}"), Some("{}"), None]),
            ("o::bigint[]", [Some("[0:1]={4,5}"), Some("{}"), None]),
            ("a::text", [Some("1 2 3"), Some(""), None]),
            ("o::varchar(2)", [Some("4 "), Some(""), None]),
            ("array_lower(a, 1)", [Some("0"), Some("0"), None]),
            ("array_upper(a, 1)", [Some("2"), Some("-1"), None]),
            ("array_dims(a)", [Some("[0:2]"), Some("[0:-1]"), None]),
            ("array_ndims(a)", [Some("1"), Some("1"), None]),
            ("a || a", [Some("[0:5]={1,2,3,1,2,3}"), Some("{}"), None]),
            ("a || 9::smallint", [Some("[0:3]={1,2,3,9}"), Some("[0:0]={9}"), Some("{9}")]),
            ("a || 9", [Some("[0:3]={1,2,3,9}"), Some("{9}"), Some("{9}")]),
            ("o || 9::oid", [Some("[0:2]={4,5,9}"), Some("[0:0]={9}"), Some("{9}")]),
            ("array_remove(a, 2::smallint)", [Some("[0:1]={1,3}"), Some("{}"), None]),
            ("array_reverse(a)", [Some("3 2 1"), Some(""), None]),
            ("o::oid[]::text", [Some("[0:1]={4,5}"), Some("{}"), None]),
        ] {
            let got = asked(&path, &at(e));
            let want: Vec<Option<String>> = want.iter().map(|w| w.map(str::to_string)).collect();
            assert_eq!(got, want, "{e}");
        }
        for (e, words) in [
            ("a::oidvector", "cannot cast type int2vector to oidvector"),
            ("o::int2vector", "cannot cast type oidvector to int2vector"),
            ("a::smallint[]::int2vector", "cannot cast type smallint[] to int2vector"),
            ("o::oid[]::oidvector", "cannot cast type oid[] to oidvector"),
            ("o::smallint[]", "cannot cast type oidvector to smallint[]"),
            ("a::smallint", "cannot cast type int2vector to smallint"),
            ("o::oid", "cannot cast type oidvector to oid"),
            ("a = o", "operator does not exist: int2vector = oidvector"),
            ("a = '{1,2}'::smallint[]", "operator does not exist: int2vector = smallint[]"),
            ("array_agg(a)", "array is not a valid int2vector"),
        ] {
            let q = format!("{step} SELECT ({e})::text AS x FROM t WHERE id = 1 GROUP BY id, a, o");
            let q = if e.starts_with("array_agg") {
                format!("{step} SELECT ({e})::text AS x FROM t WHERE id = 1")
            } else {
                q
            };
            assert!(refused(&path, &q).contains(words), "{e}: {}", refused(&path, &q));
        }
    }

    #[test]
    fn an_order_and_a_strand_on_a_vector_cut_and_read_in_its_order() {
        let ddl = "CREATE TABLE s.t (id integer PRIMARY KEY, o oidvector, a int2vector);
             CREATE INDEX t_o ON s.t USING btree (o oidvector_ops, id);
             CREATE INDEX t_h ON s.t USING hash (o oidvector_ops);
             CREATE INDEX t_a ON s.t (a, id);";
        let path = snail_at("vector-strands.snail", ddl, 19, &[]);
        let mut w = Snail::open_writing(&path).unwrap();
        let mut values = Vec::new();
        for i in 0..4096 {
            values.push(format!(
                "({i}, '{} {}', '{}')",
                i % 7,
                i % 11,
                i % 13
            ));
            if i % 5 == 0 {
                values.push(format!("({}, '{}', '{} {}')", i + 100_000, i % 7, i % 3, i % 5));
            }
        }
        w.execute(&format!("INSERT INTO s.t VALUES {}", values.join(", ")))
            .unwrap();
        drop(w);
        let mut s = Snail::open_writing(&path).unwrap();
        // Along the strand, a shorter vector first, whatever its elements.
        let (b, round) = s
            .ask("SELECT id, o FROM s.t WHERE o = '3 4' ORDER BY id", Some("t_o"))
            .unwrap();
        assert!(b.num_rows() > 0);
        assert_eq!(round.order, "t_o");
        let expected: Vec<Option<i64>> = (0..4096i64)
            .filter(|i| i % 7 == 3 && i % 11 == 4)
            .map(Some)
            .collect();
        assert_eq!(ints64(b.column(0).as_ref()), expected);
        let (b, _) = s
            .ask("SELECT id FROM s.t WHERE o < '0 0' ORDER BY id", Some("t_o"))
            .unwrap();
        // Every vector of one element is less.
        let expected: Vec<Option<i64>> = (0..4096i64)
            .filter(|i| i % 5 == 0)
            .map(|i| Some(i + 100_000))
            .collect();
        assert_eq!(ints64(b.column(0).as_ref()), expected);
        let (b, _) = s
            .ask("SELECT id FROM s.t WHERE o = '6' ORDER BY id", Some("t_h"))
            .unwrap();
        assert!(b.num_rows() > 0);
        // An int2vector along its strand is an array's order.
        let (b, _) = s
            .ask("SELECT id FROM s.t WHERE a < '1' ORDER BY id", Some("t_a"))
            .unwrap();
        let mut expected: Vec<i64> = (0..4096i64).filter(|i| i % 13 == 0).collect();
        expected.extend((0..4096i64).filter(|i| i % 5 == 0 && i % 3 == 0).map(|i| i + 100_000));
        expected.sort();
        assert_eq!(
            ints64(b.column(0).as_ref()),
            expected.into_iter().map(Some).collect::<Vec<_>>()
        );
        // min and max are the array's order on either vector.
        let (b, _) = s
            .ask("WITH t AS (SELECT o FROM s.t) SELECT min(o)::text AS x, max(o)::text AS y FROM t", None)
            .unwrap();
        assert_eq!(texts(b.column(0).as_ref()), [Some("0".to_string())]);
        assert_eq!(texts(b.column(1).as_ref()), [Some("6 10".to_string())]);
        // Written out and back.
        let dir = tmp("vector-out");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        s.out(&dir).unwrap();
        let back = tmp("vector-back.snail");
        warren_snail::write::write_parquet(
            &back,
            &Catalog::parse(ddl).unwrap(),
            &[("s.t".to_string(), dir.join("s.t.parquet"))],
            weather::span(),
            None,
            None,
        )
        .unwrap();
        let q = "WITH t AS (SELECT id, o, a FROM s.t) SELECT id, o::text AS x, a::text AS y FROM t ORDER BY id";
        let (x, _) = s.ask(q, None).unwrap();
        let (y, _) = Snail::open(&back).unwrap().ask(q, None).unwrap();
        assert_eq!(texts(x.column(1).as_ref()), texts(y.column(1).as_ref()));
        assert_eq!(texts(x.column(2).as_ref()), texts(y.column(2).as_ref()));
    }

    #[test]
    fn a_vector_is_assigned_to_an_array_and_a_string_and_nothing_to_a_vector_but_itself() {
        let ddl = "CREATE TABLE s.t (id integer PRIMARY KEY, a int2vector, o oidvector);
             CREATE TABLE s.u (id integer PRIMARY KEY, a int2vector, o oidvector, s smallint[], i oid[], x text, b integer[], y text[]);";
        let path = snail_at("vector-assign.snail", ddl, 19, &[]);
        let mut w = Snail::open_writing(&path).unwrap();
        w.execute("INSERT INTO s.t VALUES (1, '1 2', '4 5')").unwrap();
        for (i, (to, from)) in [
            ("a", "a"),
            ("s", "a"),
            ("i", "o"),
            ("x", "a"),
            ("x", "o"),
            ("b", "a"),
            ("y", "a"),
            ("i", "a"),
        ]
        .iter()
        .enumerate()
        {
            w.execute(&format!(
                "INSERT INTO s.u (id, {to}) SELECT {i}, {from} FROM s.t"
            ))
            .unwrap_or_else(|e| panic!("{to} from {from}: {e}"));
        }
        for (to, from, words) in [
            ("a", "s", "column \"a\" is of type int2vector but expression is of type smallint[]"),
            ("o", "i", "column \"o\" is of type oidvector but expression is of type oid[]"),
            ("a", "x", "column \"a\" is of type int2vector but expression is of type text"),
        ] {
            let e = w
                .execute(&format!("INSERT INTO s.u (id, {to}) SELECT 100, {from} FROM s.u WHERE {from} IS NOT NULL"))
                .unwrap_err()
                .to_string();
            assert!(e.contains(words), "{e}");
        }
        for (to, from, words) in [
            ("a", "o", "column \"a\" is of type int2vector but expression is of type oidvector"),
            ("o", "a", "column \"o\" is of type oidvector but expression is of type int2vector"),
        ] {
            let e = w
                .execute(&format!("INSERT INTO s.u (id, {to}) SELECT 100, {from} FROM s.t"))
                .unwrap_err()
                .to_string();
            assert!(e.contains(words), "{e}");
        }
        drop(w);
        let q = "WITH t AS (SELECT id, a, o, s, i, x, b, y FROM s.u) SELECT coalesce(a::text, s::text, i::text, x, b::text, y::text) AS x FROM t ORDER BY id";
        let want = [
            "1 2",
            "[0:1]={1,2}",
            "[0:1]={4,5}",
            "1 2",
            "4 5",
            "[0:1]={1,2}",
            "[0:1]={1,2}",
            "[0:1]={1,2}",
        ];
        assert_eq!(
            asked(&path, q),
            want.iter().map(|w| Some(w.to_string())).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_row_type_whose_fields_differ_is_read_as_each_file_s_release_says() {
        // pg_database holds datlastsysoid on 14 alone, and datlocprovider from 15.
        let ddl = "CREATE TABLE s.t (id integer PRIMARY KEY, c pg_database);";
        let old = snail_at("row-14.snail", ddl, 14, &[]);
        let new = snail_at("row-19.snail", ddl, 19, &[]);
        let fields = |n: usize| {
            let ty = row_type_at("pg_database", n as u8).unwrap();
            let ColumnType::Composed(t) = ty else { panic!() };
            let Composed::Record { fields, .. } = t.def() else {
                panic!()
            };
            fields.len()
        };
        let (f14, f19) = (fields(14), fields(19));
        assert_ne!(f14, f19);
        let literal = |n: usize| {
            let mut parts = vec!["1", "db"];
            parts.resize(n, "");
            format!("({})", parts.join(","))
        };
        let insert = |path: &std::path::Path, n: usize| {
            Snail::open_writing(path)
                .unwrap()
                .execute(&format!("INSERT INTO s.t VALUES (1, '{}')", literal(n)))
                .map(|_| ())
        };
        assert!(insert(&old, f19).unwrap_err().to_string().contains("malformed record literal"));
        assert!(insert(&new, f14).unwrap_err().to_string().contains("malformed record literal"));
        insert(&old, f14).unwrap();
        insert(&new, f19).unwrap();
        let step = "WITH t AS (SELECT id, c FROM s.t)";
        let lastsys = format!("{step} SELECT (c).datlastsysoid::text AS x FROM t");
        assert_eq!(asked(&old, &lastsys), [None]);
        assert!(refused(&new, &lastsys).contains("column \"datlastsysoid\" not found in data type pg_database"));
        let provider = format!("{step} SELECT (c).datlocprovider::text AS x FROM t");
        assert!(refused(&old, &provider).contains("column \"datlocprovider\" not found in data type pg_database"));
        assert_eq!(asked(&new, &provider), [None]);
        let name = format!("{step} SELECT (c).datname::text AS x FROM t");
        assert_eq!(asked(&old, &name), [Some("db".to_string())]);
        assert_eq!(asked(&new, &name), [Some("db".to_string())]);
        // A cast names the file's row type too.
        let cast = format!("SELECT '{}'::pg_database::text AS x", literal(f14));
        assert!(refused(&new, &cast).contains("malformed record literal"));
        assert_eq!(asked(&old, &cast).len(), 1);
        // Out, then back: under its own release, and refused under another.
        let dir = tmp("row-14-out");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Snail::open_writing(&old).unwrap().out(&dir).unwrap();
        let back = tmp("row-14-back.snail");
        warren_snail::write::write_parquet(
            &back,
            &Catalog::parse(ddl).unwrap(),
            &[("s.t".to_string(), dir.join("s.t.parquet"))],
            weather::span(),
            None,
            None,
        )
        .unwrap();
        assert_eq!(Snail::open(&back).unwrap().release(), 14);
        assert_eq!(asked(&back, &name), [Some("db".to_string())]);
    }

    #[test]
    fn a_row_type_holds_what_a_literal_cannot_write_and_a_node_tree_only_by_a_copy() {
        let ddl = "CREATE TABLE s.t (id integer PRIMARY KEY, c pg_attrdef, d pg_rewrite);";
        let ty = row_type_at("pg_attrdef", 19).unwrap();
        let tree = "({QUERY :commandType 1})";
        let item = Item::Record(vec![
            Item::Scalar(Datum::Int(warren_snail::scalars::unsigned_key(1, 32))),
            Item::Scalar(Datum::Int(warren_snail::scalars::unsigned_key(2, 32))),
            Item::Scalar(Datum::Int(3)),
            Item::Scalar(Datum::Text(tree.into())),
        ]);
        let tables = [(
            "s.t".to_string(),
            Rows {
                columns: vec![
                    Values::Int(vec![Some(1), Some(2)]),
                    Values::Composed(vec![Some(Arc::new(Compound::new(ty, item))), None]),
                    Values::Composed(vec![None, None]),
                ],
            },
        )];
        let path = snail_at("row-tree.snail", ddl, 19, &tables);
        let step = "WITH t AS (SELECT id, c FROM s.t)";
        assert_eq!(
            asked(&path, &format!("{step} SELECT (c).adbin::text AS x FROM t ORDER BY id")),
            [Some(tree.to_string()), None]
        );
        assert_eq!(
            asked(&path, &format!("{step} SELECT c::text AS x FROM t ORDER BY id")),
            [Some(format!("(1,2,3,\"{tree}\")")), None]
        );
        let mut w = Snail::open_writing(&path).unwrap();
        let e = w
            .execute("INSERT INTO s.t (id, c) VALUES (3, '(1,2,3,x)')")
            .unwrap_err()
            .to_string();
        assert!(e.contains("cannot accept a value of type pg_node_tree"), "{e}");
        // NULL is taken, and a copy keeps its tree.
        w.execute("INSERT INTO s.t (id, c) VALUES (4, '(1,2,3,)')").unwrap();
        w.execute("INSERT INTO s.t (id, c) SELECT id + 10, c FROM s.t WHERE id = 1")
            .unwrap();
        drop(w);
        assert_eq!(
            asked(&path, &format!("{step} SELECT (c).adbin::text AS x FROM t WHERE id = 11")),
            [Some(tree.to_string())]
        );
        // Out, then back, the tree among the values.
        let dir = tmp("row-tree-out");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Snail::open_writing(&path).unwrap().out(&dir).unwrap();
        let back = tmp("row-tree-back.snail");
        warren_snail::write::write_parquet(
            &back,
            &Catalog::parse(ddl).unwrap(),
            &[("s.t".to_string(), dir.join("s.t.parquet"))],
            weather::span(),
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            asked(&back, &format!("{step} SELECT (c).adbin::text AS x FROM t WHERE id = 11")),
            [Some(tree.to_string())]
        );
    }

    #[test]
    fn a_row_type_is_compared_as_far_as_its_fields_are_ordered() {
        let ddl = "CREATE TABLE s.t (id integer PRIMARY KEY, n pg_namespace);";
        let path = snail_at("row-reach.snail", ddl, 19, &[]);
        let mut w = Snail::open_writing(&path).unwrap();
        w.execute("INSERT INTO s.t VALUES (1, '(11,a,10,)'), (2, '(99,b,10,)'), (3, '(11,a,10,)')")
            .unwrap();
        drop(w);
        let step = "WITH t AS (SELECT id, n FROM s.t)";
        // They part before aclitem[].
        assert_eq!(
            asked(&path, &format!("{step} SELECT (a.n < b.n)::text AS x FROM t a JOIN t b ON a.id = 1 AND b.id = 2")),
            [Some("true".to_string())]
        );
        let (b, _) = Snail::open(&path)
            .unwrap()
            .ask("SELECT id FROM s.t WHERE n < '(50,a,10,)'::pg_namespace ORDER BY id", None)
            .unwrap();
        assert_eq!(ints64(b.column(0).as_ref()), [Some(1), Some(3)]);
        // Or they reach it.
        for q in [
            format!("{step} SELECT (a.n < b.n)::text AS x FROM t a JOIN t b ON a.id = 1 AND b.id = 3"),
            "SELECT id FROM s.t WHERE n <= '(11,a,10,)'::pg_namespace".to_string(),
            "SELECT min(n) FROM s.t".to_string(),
        ] {
            let e = refused(&path, &q);
            assert!(
                e.contains("could not identify a comparison function for type aclitem[]"),
                "{q}: {e}"
            );
        }
        // = and GROUP BY read aclitem's equality; ORDER BY and a DISTINCT aggregate ask an order.
        let (b, _) = Snail::open(&path)
            .unwrap()
            .ask("SELECT id FROM s.t WHERE n = '(11,a,10,)'::pg_namespace ORDER BY id", None)
            .unwrap();
        assert_eq!(ints64(b.column(0).as_ref()), [Some(1), Some(3)]);
        let (b, _) = Snail::open(&path)
            .unwrap()
            .ask("SELECT count(*) AS x FROM s.t GROUP BY n ORDER BY x", None)
            .unwrap();
        assert_eq!(ints64(b.column(0).as_ref()), [Some(1), Some(2)]);
        for q in [
            "SELECT id FROM s.t ORDER BY n",
            "SELECT count(DISTINCT n) FROM s.t",
        ] {
            let e = refused(&path, q);
            assert!(
                e.contains("could not identify an ordering operator for type pg_namespace"),
                "{q}: {e}"
            );
        }
    }

    #[test]
    fn a_snail_of_an_old_format_refuses_a_vector_and_a_row_type_by_name_with_the_way_across() {
        for format in [7u32, 8] {
            let path = tmp(&format!("old-{format}.snail"));
            let plain = Catalog::parse("CREATE TABLE s.p (id integer PRIMARY KEY);").unwrap();
            if format == 7 {
                warren_snail::write::write_format_seven(&path, &plain, &[], weather::span()).unwrap();
            } else {
                warren_snail::write::write_format_eight(&path, &plain, &[], weather::span()).unwrap();
            }
            for (ty, name) in [("int2vector", "int2vector"), ("oidvector", "oidvector"), ("pg_class", "pg_class")] {
                let mut w = Snail::open_writing(&path).unwrap();
                let e = w
                    .execute(&format!("CREATE TABLE s.t (id integer PRIMARY KEY, c {ty})"))
                    .unwrap_err()
                    .to_string();
                assert!(e.contains(name), "{e}");
                assert!(e.contains("`out`") && e.contains("`write`"), "{e}");
            }
        }
    }
}
