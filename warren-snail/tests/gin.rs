//! The element strands: a GIN index, and a GiST index on a tsvector or a tsquery, held as a strand of
//! the relation 3NF would make of the column, a row per element and row, cut on the element's order;
//! the rows found by their keys on the table's order.

mod weather;

use arrow::array::{Array, Int32Array, Int64Array};
use warren_snail::catalog::{Catalog, ColumnType};
use warren_snail::read::statement::Datum;
use warren_snail::read::{BlockRow, Round, Snail};
use warren_snail::values::{push_datum, Values};
use warren_snail::write::Rows;
use weather::{exact, fresh, tmp, write};

/// Each statement's refusal, or None where it is taken.
fn outcomes(ddl: &str) -> Vec<Option<String>> {
    Catalog::read(ddl)
        .unwrap()
        .statements
        .into_iter()
        .map(|s| s.refused)
        .collect()
}

fn composed(
    ty: ColumnType,
    text: Option<&str>,
) -> Option<std::sync::Arc<warren_snail::composed::Compound>> {
    text.map(|t| {
        std::sync::Arc::new(warren_snail::composed::Compound::new(
            ty,
            warren_snail::composed::text_in(ty, t).unwrap(),
        ))
    })
}

/// Row `i`'s elements, none where its array is NULL: `i % 7`, one of few; `i % 997`, one of many;
/// `2000 + i / 256`, whose rows lie together; 9000, in every array; every 53rd holding a NULL, every 59th its first twice,
/// every 51st none.
fn elements_of(i: i64) -> Option<Vec<Option<i64>>> {
    if i % 50 == 0 {
        return None;
    }
    if i % 51 == 0 {
        return Some(Vec::new());
    }
    let mut v = vec![
        Some(i % 7),
        Some(i % 997 + 100),
        Some(2000 + i / 256),
        Some(9000),
    ];
    if i % 53 == 0 {
        v.push(None);
    }
    if i % 59 == 0 {
        v.push(v[0]);
    }
    Some(v)
}

fn array_text(v: &[Option<i64>]) -> String {
    let parts: Vec<String> = v
        .iter()
        .map(|e| e.map_or("NULL".to_string(), |x| x.to_string()))
        .collect();
    format!("{{{}}}", parts.join(","))
}

const ARRAYS: &str = "CREATE TABLE s.t (id int NOT NULL, a int[], pad bigint, PRIMARY KEY (id)); \
                      CREATE INDEX t_a ON s.t USING gin (a);";

fn arrays(name: &str, n: i64) -> std::path::PathBuf {
    let catalog = Catalog::parse(ARRAYS).unwrap();
    let id = Values::Int((0..n).map(Some).collect());
    let a = Values::Composed(
        (0..n)
            .map(|i| {
                let text = elements_of(i).map(|v| array_text(&v));
                composed(catalog.tables[0].columns[1].ty, text.as_deref())
            })
            .collect(),
    );
    let pad = Values::Int((0..n).map(Some).collect());
    let path = tmp(&format!("{name}.snail"));
    write(
        &path,
        &catalog,
        &[(
            "s.t".into(),
            Rows {
                columns: vec![id, a, pad],
            },
        )],
    )
    .unwrap();
    path
}

fn ids_of(b: &arrow::array::RecordBatch) -> Vec<i32> {
    let a = b.column(0).as_any().downcast_ref::<Int32Array>().unwrap();
    let mut v: Vec<i32> = (0..a.len()).map(|i| a.value(i)).collect();
    v.sort_unstable();
    v
}

fn ids(s: &mut Snail, sql: &str, order: Option<&str>) -> (Vec<i32>, Round) {
    let (b, round) = s.ask(sql, order).unwrap_or_else(|e| panic!("{sql}: {e}"));
    exact(sql, &round);
    (ids_of(&b), round)
}

fn count(s: &mut Snail, sql: &str, order: Option<&str>) -> (i64, Round) {
    let (b, round) = s.ask(sql, order).unwrap_or_else(|e| panic!("{sql}: {e}"));
    exact(sql, &round);
    let a = b.column(0).as_any().downcast_ref::<Int64Array>().unwrap();
    (a.value(0), round)
}

fn strand_rows(rows: &[BlockRow], order: &str, column: &str) -> u64 {
    rows.iter()
        .filter(|r| r.order == order && r.column == column)
        .map(|r| r.rows)
        .sum()
}

/// What each array operator keeps of the rows `elements_of` makes.
fn kept(n: i64, f: impl Fn(&[Option<i64>]) -> bool) -> Vec<i32> {
    (0..n)
        .filter(|&i| elements_of(i).is_some_and(|v| f(&v)))
        .map(|i| i as i32)
        .collect()
}

#[test]
fn an_array_s_operators_answer_alike_along_its_element_strand_and_the_table_s_order() {
    let n = 20480;
    let mut s = fresh(&arrays("gin-arrays", n));
    let has = |v: &[Option<i64>], x: i64| v.contains(&Some(x));
    let cases: Vec<(String, Vec<i32>)> = vec![
        ("a @> '{3}'".into(), kept(n, |v| has(v, 3))),
        (
            "a @> '{3,500}'".into(),
            kept(n, |v| has(v, 3) && has(v, 500)),
        ),
        ("'{500}' <@ a".into(), kept(n, |v| has(v, 500))),
        ("a @> '{}'".into(), kept(n, |_| true)),
        ("a @> '{3,NULL}'".into(), Vec::new()),
        ("a @> '{500,500}'".into(), kept(n, |v| has(v, 500))),
        ("a @> ARRAY[500]".into(), kept(n, |v| has(v, 500))),
        (
            "a && '{500,501}'".into(),
            kept(n, |v| has(v, 500) || has(v, 501)),
        ),
        ("a && '{500,NULL}'".into(), kept(n, |v| has(v, 500))),
        ("a && '{}'".into(), Vec::new()),
        (
            "a <@ '{0,1,2,3,4,5,6,100,2000,9000}'".into(),
            kept(n, |v| {
                v.iter().all(|e| {
                    e.is_some_and(|x| (0..=6).contains(&x) || x == 100 || x == 2000 || x == 9000)
                })
            }),
        ),
        ("a <@ '{}'".into(), kept(n, |v| v.is_empty())),
        (
            "'{3,NULL}' @> a".into(),
            kept(n, |v| v.is_empty() || v.iter().all(|e| *e == Some(3))),
        ),
        ("a = '{}'".into(), kept(n, |v| v.is_empty())),
        (
            "a = '{1,101,2000,9000}'".into(),
            kept(n, |v| *v == [Some(1), Some(101), Some(2000), Some(9000)]),
        ),
        (
            "a && '{500}' AND id < 3000".into(),
            kept(n, |v| has(v, 500))
                .into_iter()
                .filter(|&i| i < 3000)
                .collect(),
        ),
    ];
    for (w, want) in &cases {
        let q = format!("SELECT id FROM s.t WHERE {w}");
        assert_eq!(
            &ids(&mut s, &q, Some("t_pkey")).0,
            want,
            "{q} along the table's order"
        );
        assert_eq!(
            &ids(&mut s, &q, Some("t_a")).0,
            want,
            "{q} along the strand"
        );
        assert_eq!(&ids(&mut s, &q, None).0, want, "{q} by its pages");
    }
}

#[test]
fn an_element_s_rows_are_its_count_each_row_once_as_the_extraction_makes_them_unique() {
    let n = 20480;
    let s = fresh(&arrays("gin-counts", n));
    let by_element: u64 = (0..n)
        .filter_map(elements_of)
        .map(|v| {
            // NULLs make one, and an array of none holds that place.
            let mut u: Vec<Option<i64>> = v.clone();
            u.sort_unstable();
            u.dedup();
            if u.is_empty() {
                1
            } else {
                u.len() as u64
            }
        })
        .sum();
    let rows = s.blocks().unwrap();
    assert_eq!(strand_rows(&rows, "t_a", "id"), by_element);
    assert!(rows
        .iter()
        .any(|r| r.order == "t_a" && r.table.starts_with("pg_strand.")));
}

#[test]
fn a_rare_element_reads_few_pages_and_a_common_one_the_table_s_order() {
    let n = 40960;
    let mut s = fresh(&arrays("gin-pages", n));
    let q = "SELECT id, pad FROM s.t WHERE a @> '{2005}'";
    let (rare, along) = ids(&mut s, q, None);
    assert!(along.order.starts_with("t_a+"), "{along:?}");
    let (same, whole) = ids(&mut s, q, Some("t_pkey"));
    assert_eq!(rare, same);
    assert!(
        along.column_pages_read * 3 < whole.column_pages_read,
        "{along:?} {whole:?}"
    );
    // Rows of few keys read few blocks of the table's order.
    assert!(along.blocks_read < whole.blocks_read, "{along:?} {whole:?}");
    // A value no row passes reads no page.
    let (none, round) = ids(
        &mut s,
        "SELECT id FROM s.t WHERE a @> '{3,NULL}'",
        Some("t_a"),
    );
    assert!(none.is_empty());
    assert_eq!(round.column_pages_read, 0, "{round:?}");
    // Two elements, one in every row and one in few: the first is not worth its pages.
    let (_, both) = ids(&mut s, "SELECT id FROM s.t WHERE a @> '{9000,500}'", None);
    let (_, one) = ids(&mut s, "SELECT id FROM s.t WHERE a @> '{500}'", None);
    assert_eq!(
        both.column_pages_read, one.column_pages_read,
        "{both:?} {one:?}"
    );
    // Named, the strand is read even where the table's order reads fewer.
    let (_, named) = ids(
        &mut s,
        "SELECT id FROM s.t WHERE a @> '{9000}'",
        Some("t_a"),
    );
    assert!(named.order.starts_with("t_a+"), "{named:?}");
    // Every array holds it: what the strand would cost, should its keys lie in every block, is more
    // than it would save, so the table's order is read and the strand left unread.
    let q = "SELECT id FROM s.t WHERE a @> '{9000}'";
    let (_, common) = ids(&mut s, q, None);
    assert_eq!(common.order, "t_pkey", "{common:?}");
}

#[test]
fn a_count_of_one_element_is_answered_from_its_counts() {
    let n = 40960;
    let mut s = fresh(&arrays("gin-count", n));
    let want = |x: i64| kept(n, |v| v.contains(&Some(x))).len() as i64;
    let (c, round) = count(&mut s, "SELECT count(*) FROM s.t WHERE a @> '{3}'", None);
    assert_eq!(c, want(3));
    assert_eq!(round.order, "t_a", "{round:?}");
    assert!(round.column_pages_read < 40, "{round:?}");
    let (c, round) = count(&mut s, "SELECT count(*) FROM s.t WHERE a && '{500}'", None);
    assert_eq!(c, want(500));
    assert_eq!(round.order, "t_a", "{round:?}");
    // Beside another condition its count closes nothing.
    let (c, round) = count(
        &mut s,
        "SELECT count(*) FROM s.t WHERE a @> '{500}' AND id < 3000",
        None,
    );
    assert_eq!(
        c,
        kept(n, |v| v.contains(&Some(500)))
            .into_iter()
            .filter(|&i| i < 3000)
            .count() as i64
    );
    assert!(round.order != "t_a", "{round:?}");
    let (c, _) = count(
        &mut s,
        "SELECT count(*) FROM s.t WHERE a @> '{3,500}'",
        None,
    );
    assert_eq!(
        c,
        kept(n, |v| v.contains(&Some(3)) && v.contains(&Some(500))).len() as i64
    );
}

#[test]
fn a_gin_index_is_read_or_refused_as_postgresql_reads_it() {
    let table = "CREATE TABLE s.t (id int PRIMARY KEY, a int[], tx text[], j jsonb, v tsvector, \
                 q tsquery, js json[], pt point[], n int, c text, r int4range, x xid[], u uuid[]);";
    // Each as PostgreSQL 18.6 and 19 RC1 gave it (2026-11-02).
    let refused = [
        (
            "CREATE INDEX e ON s.t USING gin (js)",
            "could not identify a comparison function for type json",
        ),
        (
            "CREATE INDEX e ON s.t USING gin (pt)",
            "could not identify a comparison function for type point",
        ),
        (
            "CREATE INDEX e ON s.t USING gin (n)",
            "data type integer has no default operator class for access method \"gin\"",
        ),
        (
            "CREATE INDEX e ON s.t USING gin (q)",
            "data type tsquery has no default operator class for access method \"gin\"",
        ),
        (
            "CREATE INDEX e ON s.t USING gin (a) WITH (gin_pending_list_limit = 63)",
            "value 63 out of bounds for option \"gin_pending_list_limit\"",
        ),
        (
            "CREATE INDEX e ON s.t USING gin (a) WITH (fastupdate = maybe)",
            "invalid value for boolean option \"fastupdate\": maybe",
        ),
        (
            "CREATE INDEX e ON s.t USING gin (a DESC)",
            "access method \"gin\" does not support ASC/DESC options",
        ),
        (
            "CREATE INDEX e ON s.t USING gin (a NULLS FIRST)",
            "access method \"gin\" does not support NULLS FIRST/LAST options",
        ),
        (
            "CREATE UNIQUE INDEX e ON s.t USING gin (a)",
            "access method \"gin\" does not support unique indexes",
        ),
        (
            "CREATE INDEX e ON s.t USING gin (a) INCLUDE (n)",
            "access method \"gin\" does not support included columns",
        ),
    ];
    for (statement, why) in refused {
        let got = outcomes(&format!("{table}\n{statement};"));
        let e = got[1]
            .as_deref()
            .unwrap_or_else(|| panic!("{statement} was taken"));
        assert!(e.contains(why), "{statement}: {e}");
    }
    let taken = [
        "CREATE INDEX e ON s.t USING gin (a)",
        "CREATE INDEX e ON s.t USING gin (a, tx)",
        "CREATE INDEX e ON s.t USING gin (a array_ops, j jsonb_path_ops, v tsvector_ops)",
        "CREATE INDEX e ON s.t USING gin (a) WITH (fastupdate = off, gin_pending_list_limit = 64)",
        "CREATE INDEX e ON s.t USING gin (a) WHERE n > 0",
        "CREATE INDEX e ON s.t USING gin (j)",
        "CREATE INDEX e ON s.t USING gin (j jsonb_path_ops)",
        "CREATE INDEX e ON s.t USING gin (v)",
        "CREATE INDEX e ON s.t USING gin (u)",
        "CREATE INDEX e ON s.t USING gist (v)",
        "CREATE INDEX e ON s.t USING gist (q)",
        "CREATE INDEX e ON s.t USING gist (v tsvector_ops (siglen = 8))",
        "CREATE INDEX e ON s.t USING gist (v, q)",
        "CREATE INDEX e ON s.t USING gist (v) INCLUDE (n)",
        "CREATE INDEX e ON s.t USING gist (q) WHERE n > 0",
    ];
    for statement in taken {
        let got = outcomes(&format!("{table}\n{statement};"));
        assert_eq!(got[1], None, "{statement}");
    }
    let got = outcomes(&format!(
        "{table}\nCREATE INDEX e ON s.t USING gist (v, r);"
    ));
    let e = got[1].as_deref().unwrap();
    assert!(
        e.contains("beside a key of another kind is not held"),
        "{e}"
    );
    // A table refused takes its strands with it.
    let r = Catalog::read(
        "CREATE TABLE s.p (id int PRIMARY KEY, g geometry); \
         CREATE TABLE s.c (id int PRIMARY KEY, a int[], p int REFERENCES s.p); \
         CREATE INDEX e ON s.c USING gin (a);",
    )
    .unwrap();
    assert!(r.catalog.tables.is_empty(), "{:?}", r.catalog.tables.len());
    // Several keys are a strand each, of one index.
    let c = Catalog::parse(&format!(
        "{table}\nCREATE INDEX e ON s.t USING gin (a, tx) WITH (fastupdate = off);"
    ))
    .unwrap();
    let t = c.table("s.t").unwrap();
    assert_eq!(t.element_keys.len(), 2);
    assert_eq!(t.orders.len(), 1);
    let strand = c.table(&t.element_keys[1].strand).unwrap();
    assert_eq!(strand.columns[0].ty, ColumnType::Text(None));
    assert_eq!(strand.columns[1].name, "id");
    assert_eq!(strand.elements_of.as_deref(), Some("s.t"));
    let facts = c.facts();
    let told = |fact: &str| -> String {
        facts
            .iter()
            .find(|f| f.object == "s.t.e" && f.fact == fact)
            .map(|f| f.value.clone())
            .unwrap()
    };
    assert_eq!(told("method"), "gin");
    assert_eq!(told("keys"), "a,tx");
    assert_eq!(told("opclasses"), "array_ops,array_ops");
    assert_eq!(told("options"), "fastupdate=off");
    assert!(!facts.iter().any(|f| f.object.starts_with("pg_strand")));
    // A strand's table lies where its statement stands: a table declared after keeps every place.
    let c = Catalog::parse(&format!(
        "{table}\nCREATE TABLE s.u (id int PRIMARY KEY, a int[]);\n\
         CREATE INDEX e ON s.t USING gin (a);\n\
         CREATE TABLE s.w (id int PRIMARY KEY, a int[]);\n\
         CREATE INDEX f ON s.u USING gin (a);"
    ))
    .unwrap();
    let names: Vec<&str> = c
        .tables
        .iter()
        .filter(|t| t.declared())
        .map(|t| t.name.as_str())
        .collect();
    assert_eq!(names, ["s.t", "s.u", "s.w"]);
    assert_eq!(c.tables[0].name, "s.t");
    assert_eq!(c.tables[1].name, "s.u");
    assert!(c.tables[2].elements_of.is_some());
    assert_eq!(c.tables[3].name, "s.w");
    assert!(c.tables[4].elements_of.is_some());
}

#[test]
fn a_snail_of_an_old_format_refuses_an_element_strand_by_name() {
    for (ddl, what) in [
        ("USING gin (a)", "the gin index t_x"),
        ("USING gist (v)", "the gist index t_x"),
    ] {
        let path = tmp("gin-eight.snail");
        let e = warren_snail::write::write_format_eight(
            &path,
            &Catalog::parse(&format!(
                "CREATE TABLE s.t (id integer NOT NULL, a int[], v tsvector, PRIMARY KEY (id)); \
                 CREATE INDEX t_x ON s.t {ddl};"
            ))
            .unwrap(),
            &[],
            weather::span(),
        )
        .err()
        .unwrap()
        .to_string();
        assert!(
            e.contains(&format!("s.t: {what}, which a snail of format 8")) && e.contains("`out`"),
            "{e}"
        );
    }
}

#[test]
fn a_table_is_not_clustered_on_an_element_strand() {
    let table = "CREATE TABLE s.t (id int PRIMARY KEY, a int[], v tsvector);";
    let got = outcomes(&format!(
        "{table}\nCREATE INDEX e ON s.t USING gin (a);\nALTER TABLE s.t CLUSTER ON e;"
    ));
    assert!(
        got[2]
            .as_deref()
            .unwrap()
            .contains("cannot cluster on index \"e\" because access method"),
        "{got:?}"
    );
    let got = outcomes(&format!(
        "{table}\nCREATE INDEX e ON s.t USING gist (v);\nALTER TABLE s.t CLUSTER ON e;"
    ));
    assert!(
        got[2]
            .as_deref()
            .unwrap()
            .contains("cannot be clustered on e"),
        "{got:?}"
    );
}

/// A snail of `n` rows the function makes, each a text a column, its table `s.t` made from `ddl`.
fn snail_of(name: &str, ddl: &str, n: i64, row: impl Fn(i64) -> Vec<Option<String>>) -> Snail {
    let catalog = Catalog::parse(ddl).unwrap();
    let t = &catalog.tables[0];
    let mut columns: Vec<Values> = t.columns.iter().map(|c| Values::new(c.ty)).collect();
    for i in 0..n {
        let r = row(i);
        for ((v, c), x) in columns.iter_mut().zip(&t.columns).zip(&r) {
            match (v, c.ty, x) {
                (Values::Int(a), _, x) => a.push(x.as_ref().map(|s| s.parse().unwrap())),
                (Values::Composed(a), ty, x) => a.push(composed(ty, x.as_deref())),
                (v, ColumnType::Textual(t), Some(x)) => push_datum(
                    v,
                    &Datum::Text(warren_snail::textual::text_in(t, x).unwrap()),
                ),
                (v, _, Some(x)) => push_datum(v, &Datum::Text(x.clone())),
                (v, _, None) => v.push_null(),
            }
        }
    }
    let path = tmp(&format!("{name}.snail"));
    write(&path, &catalog, &[("s.t".into(), Rows { columns })]).unwrap();
    fresh(&path)
}

/// Each condition answered alike along each order named and by its pages; each round's pages as
/// predicted.
fn alike(s: &mut Snail, orders: &[&str], wheres: &[&str]) {
    for w in wheres {
        let q = format!("SELECT id FROM s.t WHERE {w}");
        let (want, _) = ids(s, &q, Some("t_pkey"));
        for o in orders {
            assert_eq!(ids(s, &q, Some(o)).0, want, "{q} along {o}");
        }
        assert_eq!(ids(s, &q, None).0, want, "{q} by its pages");
    }
}

const JSONB: &str = "CREATE TABLE s.t (id int NOT NULL, j jsonb, PRIMARY KEY (id)); \
                     CREATE INDEX t_o ON s.t USING gin (j); \
                     CREATE INDEX t_p ON s.t USING gin (j jsonb_path_ops);";

fn jsonb_row(i: i64) -> Vec<Option<String>> {
    let j = match i % 11 {
        0 => None,
        1 => Some("{}".to_string()),
        2 => Some("[]".to_string()),
        3 => Some(format!("\"k{}\"", i % 5)),
        4 => Some(format!("{}", i % 9)),
        5 => Some(format!("[\"k{}\", {}, null, true]", i % 5, i % 9)),
        6 => Some(format!("{{\"a\": {{\"k{}\": [1, 2.50]}}}}", i % 5)),
        _ => Some(format!(
            "{{\"k{}\": {}, \"n{}\": \"v{}\", \"z\": [{{\"y\": false}}]}}",
            i % 5,
            i % 9,
            i % 397,
            i % 3
        )),
    };
    vec![Some(i.to_string()), j]
}

#[test]
fn a_jsonb_s_operators_answer_alike_along_its_strands() {
    let mut s = snail_of("gin-jsonb", JSONB, 8192, jsonb_row);
    alike(
        &mut s,
        &["t_o"],
        &[
            "j ? 'k1'",
            "j ? 'z'",
            "j ? 'y'",
            "j ? 'absent'",
            "j ?| '{k1,k2}'",
            "j ?| '{}'",
            "j ?| ARRAY['n7', NULL]",
            "j ?| '{k1,k2}'::text[]",
            "j ?& '{k1,z}'",
            "j ?& '{}'",
        ],
    );
    alike(
        &mut s,
        &["t_o", "t_p"],
        &[
            "j @> '{\"k1\": 1}'",
            "j @> '{\"n7\": \"v1\"}'",
            "j @> '{}'",
            "j @> '[]'",
            "j @> '\"k1\"'",
            "j @> '1'",
            "j @> '[1]'",
            "j @> '{\"a\": {\"k1\": [2.5]}}'",
            "j @> '{\"a\": {\"k1\": [1, 2.500]}}'",
            "j @> '{\"z\": [{\"y\": false}]}'",
            "'{\"n7\": \"v1\"}' <@ j",
            "j @> 'null'",
            "j @> '[null, true]'",
        ],
    );
    // A rare key reads few pages along the strand, and every row's column along the table's order.
    let q = "SELECT id FROM s.t WHERE j ? 'n7'";
    let (_, along) = ids(&mut s, q, Some("t_o"));
    let (_, whole) = ids(&mut s, q, Some("t_pkey"));
    assert!(
        along.column_pages_read * 2 < whole.column_pages_read,
        "{along:?} {whole:?}"
    );
    // A key that nests is no count: `?` asks the top alone, so the strand finds what it rechecks.
    let (c, round) = count(
        &mut s,
        "SELECT count(*) FROM s.t WHERE j ? 'y'",
        Some("t_o"),
    );
    assert_eq!(c, 0);
    assert!(round.order.contains("t_o+"), "{round:?}");
}

const TEXTS: &str = "CREATE TABLE s.t (id int NOT NULL, v tsvector, q tsquery, PRIMARY KEY (id)); \
                     CREATE INDEX t_g ON s.t USING gin (v); \
                     CREATE INDEX t_s ON s.t USING gist (v tsvector_ops (siglen = 8)); \
                     CREATE INDEX t_q ON s.t USING gist (q);";

fn text_row(i: i64) -> Vec<Option<String>> {
    let v = match i % 13 {
        0 => None,
        1 => Some(String::new()),
        _ => Some(format!(
            "'w{}':1A 'word{}':2 'x{}':3B",
            i % 7,
            i % 397,
            i % 3
        )),
    };
    let q = match i % 17 {
        0 => None,
        1 => Some(String::new()),
        2 => Some(format!("'w{}' & !'x{}'", i % 7, i % 3)),
        _ => Some(format!("'word{}' | 'w{}':*", i % 397, i % 7)),
    };
    vec![Some(i.to_string()), v, q]
}

#[test]
fn a_tsvector_s_match_and_a_tsquery_s_containment_answer_alike_along_their_strands() {
    let mut s = snail_of("gin-texts", TEXTS, 8192, text_row);
    alike(
        &mut s,
        &["t_g", "t_s"],
        &[
            "v @@ 'w1'",
            "v @@ 'word7'",
            "v @@ 'word7 & w1'",
            "v @@ 'word7 | word8'",
            "v @@ 'word7 & !w0'",
            "v @@ '!word7'",
            "v @@ '!word7 | w1'",
            "v @@ 'word1:*'",
            "v @@ 'word1:* & x1'",
            "v @@ 'w1:A'",
            "v @@ 'w1:B'",
            "v @@ 'w1 <-> word1'",
            "v @@ 'absent'",
            "'word7'::tsquery @@ v",
            "v @@ 'word7'::tsquery",
            "v @@ to_tsquery('simple', 'word7')",
            "v @@ ''",
        ],
    );
    alike(
        &mut s,
        &["t_q"],
        &[
            "q @> 'word7'",
            "q @> 'w1 & word7'",
            "q @> ''",
            "q <@ 'word7 | w1'",
            "q <@ ''",
            "'word7'::tsquery <@ q",
            "'word7 | w1'::tsquery @> q",
        ],
    );
    // One lexeme's rows are its count, no weight and no prefix.
    let q = "SELECT count(*) FROM s.t WHERE v @@ 'w1'";
    let (c, round) = count(&mut s, q, None);
    assert_eq!(round.order.split('+').count(), 1, "{round:?}");
    assert_eq!(c, count(&mut s, q, Some("t_pkey")).0);
    // A weight or a prefix is held to each row: their counts are no element's.
    for w in ["w1:B", "word1:*", "w1 & word7"] {
        let q = format!("SELECT count(*) FROM s.t WHERE v @@ '{w}'");
        let (by_pages, _) = count(&mut s, &q, None);
        assert_eq!(by_pages, count(&mut s, &q, Some("t_pkey")).0, "{q}");
    }
    // A condition that asks nothing of a strand reads the table's order, named or not.
    let (_, round) = ids(
        &mut s,
        "SELECT id FROM s.t WHERE v @@ '!word7'",
        Some("t_g"),
    );
    assert_eq!(round.order, "t_pkey", "{round:?}");
    // A question with no condition on the strand's column is not along it.
    let e = s
        .ask("SELECT id FROM s.t", Some("t_g"))
        .unwrap_err()
        .to_string();
    assert!(e.contains("t_g does not hold every column"), "{e}");
}

#[test]
fn several_keys_of_a_gin_index_are_a_strand_each_read_where_a_condition_asks_it() {
    let ddl = "CREATE TABLE s.t (id int NOT NULL, a int[], b text[], PRIMARY KEY (id)); \
               CREATE INDEX t_ab ON s.t USING gin (a, b);";
    let mut s = snail_of("gin-several", ddl, 8192, |i| {
        vec![
            Some(i.to_string()),
            (i % 10 != 0).then(|| format!("{{{},{}}}", i % 7, i % 397)),
            (i % 11 != 0).then(|| format!("{{t{},\"t {}\"}}", i % 5, i % 397)),
        ]
    });
    alike(
        &mut s,
        &["t_ab"],
        &[
            "a @> '{3}'",
            "b @> '{t1}'",
            "a @> '{3,100}'",
            "a @> '{100}' AND b @> '{\"t 100\"}'",
            "a && '{100,101}' AND b && '{t0,t1}'",
        ],
    );
    // Two rare elements on two strands: each read, their keys met.
    let q = "SELECT id FROM s.t WHERE a @> '{200}' AND b @> '{\"t 201\"}'";
    let (none, round) = ids(&mut s, q, None);
    assert!(none.is_empty());
    assert_eq!(
        round.blocks_read,
        round.reads.iter().map(|r| r.blocks_read).sum::<u64>()
    );
    assert_eq!(
        round.reads.last().unwrap().column_pages_read,
        0,
        "{round:?}"
    );
}

#[test]
fn a_partial_gin_index_holds_the_elements_of_the_rows_its_predicate_admits() {
    let ddl = "CREATE TABLE s.t (id int NOT NULL, a int[], n int, PRIMARY KEY (id)); \
               CREATE INDEX t_a ON s.t USING gin (a) WHERE n > 5;";
    let mut s = snail_of("gin-partial", ddl, 4096, |i| {
        vec![
            Some(i.to_string()),
            Some(format!("{{{}}}", i % 397)),
            Some((i % 10).to_string()),
        ]
    });
    let rows = s.blocks().unwrap();
    assert_eq!(
        strand_rows(&rows, "t_a", "id"),
        (0..4096).filter(|i| i % 10 > 5).count() as u64
    );
    let q = "SELECT id FROM s.t WHERE a @> '{7}' AND n > 6";
    let (_, along) = ids(&mut s, q, Some("t_a"));
    assert!(along.order.starts_with("t_a+"), "{along:?}");
    // Not implied, it does not serve.
    let q = "SELECT id FROM s.t WHERE a @> '{7}'";
    let (ans, along) = ids(&mut s, q, Some("t_a"));
    assert_eq!(along.order, "t_pkey", "{along:?}");
    assert_eq!(ans.len(), (0..4096).filter(|i| i % 397 == 7).count());
}

#[cfg(feature = "standalone")]
mod written {
    use super::*;
    use warren_snail::read::Done;

    const DDL: &str = "CREATE TABLE s.t (id int NOT NULL, a int[], pad text, PRIMARY KEY (id)); \
                       CREATE INDEX t_a ON s.t USING gin (a);";

    /// A snail of `s.t`, made from `ddl`, holding the rows of ids 0 to n, each array `{i % 7, i % 97 + 100}`.
    fn made(name: &str, ddl: &str, n: i64) -> std::path::PathBuf {
        let catalog = Catalog::parse(ddl).unwrap();
        let t = &catalog.tables[0];
        let id = Values::Int((0..n).map(Some).collect());
        let a = Values::Composed(
            (0..n)
                .map(|i| {
                    let text = format!("{{{},{}}}", i % 7, i % 97 + 100);
                    composed(t.columns[1].ty, Some(&text))
                })
                .collect(),
        );
        let mut pad = Values::new(t.columns[2].ty);
        for i in 0..n {
            push_datum(&mut pad, &Datum::Text(format!("p{i}")));
        }
        let path = tmp(&format!("{name}.snail"));
        write(
            &path,
            &catalog,
            &[(
                "s.t".into(),
                Rows {
                    columns: vec![id, a, pad],
                },
            )],
        )
        .unwrap();
        path
    }

    fn run(w: &mut Snail, sql: &str) -> Done {
        w.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"))
    }

    /// The rows of the strand, against each row's elements made one of each by the table's order.
    fn whole(s: &mut Snail) {
        let (b, _) = s
            .ask(
                "WITH r AS (SELECT a FROM s.t) SELECT a::text AS x FROM r",
                None,
            )
            .unwrap();
        let a = b
            .column(0)
            .as_any()
            .downcast_ref::<arrow::array::StringArray>()
            .unwrap();
        let mut held = 0;
        for i in (0..a.len()).filter(|&i| !a.is_null(i)) {
            let inner = a.value(i).trim_matches(|c| c == '{' || c == '}');
            let mut parts: Vec<&str> = inner.split(',').collect();
            parts.sort_unstable();
            parts.dedup();
            held += parts.len() as u64;
        }
        let rows = s.blocks().unwrap();
        assert_eq!(strand_rows(&rows, "t_a", "id"), held, "the strand's rows");
        s.census().unwrap();
    }

    /// Each of a few questions alike along the strand and the table's order.
    fn answers(s: &mut Snail, wheres: &[&str]) {
        for w in wheres {
            let q = format!("SELECT id FROM s.t WHERE {w}");
            let (want, _) = ids(s, &q, Some("t_pkey"));
            assert_eq!(ids(s, &q, Some("t_a")).0, want, "{q}");
        }
    }

    const ASKED: [&str; 6] = [
        "a @> '{3}'",
        "a @> '{150}'",
        "a && '{150,151,7777}'",
        "a <@ '{1,2,3,150}'",
        "a = '{}'",
        "a @> '{7777}'",
    ];

    #[test]
    fn a_statement_routes_a_row_s_elements_into_the_strand_and_out() {
        let path = made("gin-writes", DDL, 4096);
        let mut s = Snail::open_writing(&path).unwrap();
        whole(&mut s);
        run(
            &mut s,
            "INSERT INTO s.t (id, a, pad) VALUES (5000, '{7777,7777,NULL}', 'x')",
        );
        whole(&mut s);
        assert_eq!(
            ids(
                &mut s,
                "SELECT id FROM s.t WHERE a @> '{7777}'",
                Some("t_a")
            )
            .0,
            [5000]
        );
        // No array, and an empty one, as PostgreSQL holds each.
        run(
            &mut s,
            "INSERT INTO s.t (id, a, pad) VALUES (5001, NULL, 'x'), (5002, '{}', 'x')",
        );
        whole(&mut s);
        assert_eq!(
            ids(&mut s, "SELECT id FROM s.t WHERE a = '{}'", Some("t_a")).0,
            [5002]
        );
        assert_eq!(
            ids(&mut s, "SELECT id FROM s.t WHERE a <@ '{}'", Some("t_a")).0,
            [5002]
        );
        // A column the strand does not hold writes nothing into it.
        let d = run(&mut s, "UPDATE s.t SET pad = 'y' WHERE id = 100");
        assert_eq!(d.orders, 1, "{d:?}");
        // Elements changed: only those gone and those come.
        let d = run(&mut s, "UPDATE s.t SET a = '{2,7777}' WHERE id = 100");
        assert_eq!(d.orders, 2, "{d:?}");
        whole(&mut s);
        assert_eq!(
            ids(
                &mut s,
                "SELECT id FROM s.t WHERE a @> '{7777}'",
                Some("t_a")
            )
            .0,
            [100, 5000]
        );
        answers(&mut s, &ASKED);
        // A row's key changed moves every element of it.
        run(&mut s, "UPDATE s.t SET id = 6000 WHERE id = 100");
        whole(&mut s);
        assert_eq!(
            ids(
                &mut s,
                "SELECT id FROM s.t WHERE a @> '{7777}'",
                Some("t_a")
            )
            .0,
            [5000, 6000]
        );
        run(&mut s, "DELETE FROM s.t WHERE id = 5000");
        whole(&mut s);
        // A delete found along the strand.
        run(&mut s, "DELETE FROM s.t WHERE a @> '{150}'");
        whole(&mut s);
        assert!(
            ids(&mut s, "SELECT id FROM s.t WHERE a @> '{150}'", Some("t_a"))
                .0
                .is_empty()
        );
        answers(&mut s, &ASKED);
        // An update found along it and written back into it.
        run(&mut s, "UPDATE s.t SET a = '{151}' WHERE a @> '{152}'");
        whole(&mut s);
        answers(&mut s, &ASKED);
        run(&mut s, "BEGIN");
        run(&mut s, "UPDATE s.t SET a = '{8888}' WHERE id = 1");
        assert_eq!(
            ids(
                &mut s,
                "SELECT id FROM s.t WHERE a @> '{8888}'",
                Some("t_a")
            )
            .0,
            [1]
        );
        run(&mut s, "ROLLBACK");
        assert!(ids(
            &mut s,
            "SELECT id FROM s.t WHERE a @> '{8888}'",
            Some("t_a")
        )
        .0
        .is_empty());
        whole(&mut s);
    }

    #[test]
    fn a_read_of_the_strand_prunes_its_dead_rows() {
        let path = made("gin-prune", DDL, 8192);
        let mut s = Snail::open_writing(&path).unwrap();
        let (_, first) = ids(&mut s, "SELECT id FROM s.t WHERE a @> '{150}'", Some("t_a"));
        assert_eq!(first.blocks_pruned, 0, "{first:?}");
        run(&mut s, "UPDATE s.t SET a = '{9000}' WHERE a @> '{1}'");
        let (_, after) = ids(&mut s, "SELECT id FROM s.t WHERE a @> '{2}'", Some("t_a"));
        let (_, again) = ids(&mut s, "SELECT id FROM s.t WHERE a @> '{1}'", Some("t_a"));
        assert!(
            after.blocks_pruned + again.blocks_pruned > 0,
            "{after:?} {again:?}"
        );
        whole(&mut s);
    }

    #[test]
    fn at_one_snapshot_writers_of_one_strand_block_both_commit() {
        for level in ["REPEATABLE READ", "SERIALIZABLE"] {
            let path = made(&format!("gin-beside-{}", &level[..3]), DDL, 2048);
            let mut a = Snail::open_writing(&path).unwrap();
            let mut b = Snail::open_writing(&path).unwrap();
            run(&mut a, &format!("BEGIN ISOLATION LEVEL {level}"));
            run(&mut b, &format!("BEGIN ISOLATION LEVEL {level}"));
            // Each its own row's elements, the same ones: one block of the strand.
            run(
                &mut a,
                "INSERT INTO s.t (id, a, pad) VALUES (3000, '{5000,5001}', 'a')",
            );
            run(
                &mut b,
                "INSERT INTO s.t (id, a, pad) VALUES (3001, '{5000,5002}', 'b')",
            );
            // Each reads its own along the strand, and not the other's; at SERIALIZABLE, whichever
            // commits second would be refused for having read what the other wrote.
            if level == "REPEATABLE READ" {
                assert_eq!(
                    ids(
                        &mut a,
                        "SELECT id FROM s.t WHERE a @> '{5000}'",
                        Some("t_a")
                    )
                    .0,
                    [3000]
                );
                assert_eq!(
                    ids(
                        &mut b,
                        "SELECT id FROM s.t WHERE a @> '{5000}'",
                        Some("t_a")
                    )
                    .0,
                    [3001]
                );
            }
            run(&mut a, "COMMIT");
            run(&mut b, "COMMIT");
            let mut s = Snail::open_writing(&path).unwrap();
            assert_eq!(
                ids(
                    &mut s,
                    "SELECT id FROM s.t WHERE a @> '{5000}'",
                    Some("t_a")
                )
                .0,
                [3000, 3001]
            );
            assert_eq!(
                ids(
                    &mut s,
                    "SELECT id FROM s.t WHERE a @> '{5002}'",
                    Some("t_a")
                )
                .0,
                [3001]
            );
            whole(&mut s);
            // A row taken by one and updated by another: the second is refused as PostgreSQL's is.
            run(&mut a, &format!("BEGIN ISOLATION LEVEL {level}"));
            run(&mut b, &format!("BEGIN ISOLATION LEVEL {level}"));
            run(&mut a, "UPDATE s.t SET a = '{5003}' WHERE id = 3000");
            let _ = ids(
                &mut b,
                "SELECT id FROM s.t WHERE a @> '{5000}'",
                Some("t_a"),
            );
            run(&mut a, "COMMIT");
            let e = b
                .execute("UPDATE s.t SET a = '{5004}' WHERE id = 3000")
                .unwrap_err()
                .to_string();
            assert!(e.contains("could not serialize access"), "{e}");
            run(&mut b, "ROLLBACK");
            whole(&mut s);
            assert_eq!(
                ids(
                    &mut s,
                    "SELECT id FROM s.t WHERE a @> '{5003}'",
                    Some("t_a")
                )
                .0,
                [3000]
            );
        }
    }

    #[test]
    fn at_serializable_a_read_of_the_strand_meets_a_writer_of_its_element() {
        let path = made("gin-serial", DDL, 2048);
        let mut a = Snail::open_writing(&path).unwrap();
        let mut b = Snail::open_writing(&path).unwrap();
        run(&mut a, "BEGIN ISOLATION LEVEL SERIALIZABLE");
        run(&mut b, "BEGIN ISOLATION LEVEL SERIALIZABLE");
        // Each reads what the other writes, none yet: a write skew through the strand alone.
        assert!(ids(
            &mut a,
            "SELECT id FROM s.t WHERE a @> '{6001}'",
            Some("t_a")
        )
        .0
        .is_empty());
        assert!(ids(
            &mut b,
            "SELECT id FROM s.t WHERE a @> '{6000}'",
            Some("t_a")
        )
        .0
        .is_empty());
        run(
            &mut a,
            "INSERT INTO s.t (id, a, pad) VALUES (3000, '{6000}', 'a')",
        );
        run(
            &mut b,
            "INSERT INTO s.t (id, a, pad) VALUES (3001, '{6001}', 'b')",
        );
        run(&mut a, "COMMIT");
        let e = b.execute("COMMIT").unwrap_err().to_string();
        assert!(e.contains("could not serialize access"), "{e}");
    }

    #[test]
    fn at_read_committed_writers_of_other_blocks_go_beside_each_other() {
        let path = made("gin-committed", DDL, 2048);
        let mut a = Snail::open_writing(&path).unwrap();
        let mut b = Snail::open_writing(&path).unwrap();
        run(&mut a, "BEGIN");
        run(
            &mut a,
            "INSERT INTO s.t (id, a, pad) VALUES (3000, '{196}', 'a')",
        );
        // Its row in the table's first block, its element in the strand's first.
        run(&mut b, "UPDATE s.t SET a = '{0}' WHERE id = 1");
        assert_eq!(
            ids(
                &mut b,
                "SELECT id FROM s.t WHERE a @> '{196}' AND id >= 3000",
                Some("t_a")
            )
            .0
            .len(),
            0
        );
        run(&mut a, "COMMIT");
        assert_eq!(
            ids(
                &mut b,
                "SELECT id FROM s.t WHERE a @> '{196}' AND id >= 3000",
                Some("t_a")
            )
            .0,
            [3000]
        );
        whole(&mut b);
    }

    #[test]
    fn a_gin_index_made_on_a_table_holding_rows_is_refused_and_on_an_empty_one_taken() {
        let ddl = "CREATE TABLE s.t (id int NOT NULL, a int[], pad text, PRIMARY KEY (id));";
        let path = made("gin-declared", ddl, 16);
        let mut s = Snail::open_writing(&path).unwrap();
        let e = s
            .execute("CREATE INDEX t_a ON s.t USING gin (a)")
            .unwrap_err()
            .to_string();
        assert!(e.contains("s.t: it holds rows"), "{e}");
        run(
            &mut s,
            "CREATE TABLE s.u (id int NOT NULL, a int[], PRIMARY KEY (id))",
        );
        run(
            &mut s,
            "CREATE INDEX u_a ON s.u USING gin (a) WITH (fastupdate = off)",
        );
        run(
            &mut s,
            "INSERT INTO s.u (id, a) VALUES (1, '{1,2}'), (2, '{2,3}')",
        );
        // A table declared after keeps every order where it was.
        run(
            &mut s,
            "CREATE TABLE s.w (id int NOT NULL, a int[], PRIMARY KEY (id))",
        );
        run(&mut s, "INSERT INTO s.w (id, a) VALUES (1, '{1,2}')");
        run(&mut s, "INSERT INTO s.u (id, a) VALUES (3, '{2}')");
        let (b, round) = s
            .ask("SELECT id FROM s.u WHERE a @> '{2}'", Some("u_a"))
            .unwrap();
        assert_eq!(ids_of(&b), [1, 2, 3]);
        exact("along u_a", &round);
        s.census().unwrap();
    }

    #[test]
    fn a_table_goes_out_and_comes_back_with_its_strand_made_again() {
        let path = made("gin-out", DDL, 2048);
        let mut s = Snail::open_writing(&path).unwrap();
        let parts = tmp("gin-out-parts");
        let _ = std::fs::remove_dir_all(&parts);
        std::fs::create_dir_all(&parts).unwrap();
        let out = s.out(&parts).unwrap();
        assert_eq!(out.len(), 1);
        let catalog = Catalog::parse(DDL).unwrap();
        let again = tmp("gin-out-again.snail");
        warren_snail::write::write_parquet(
            &again,
            &catalog,
            &[("s.t".to_string(), parts.join("s.t.parquet"))],
            weather::span(),
            None,
            None,
        )
        .unwrap();
        let mut s = Snail::open_writing(&again).unwrap();
        whole(&mut s);
        answers(&mut s, &ASKED);
    }
}

#[test]
fn the_round_s_room_holds_the_keys_or_the_round_reads_the_table_s_order() {
    let n = 20480;
    let mut s = fresh(&arrays("gin-room", n));
    let q = "SELECT id FROM s.t WHERE a @> '{3}'";
    let (want, _) = ids(&mut s, q, Some("t_pkey"));
    let (_, roomy) = ids(&mut s, q, Some("t_a"));
    let read = roomy.reads[0].column_pages_read;
    s.size_rounds(warren_snail::room::Sizes::of(4096));
    let (b, round) = s.ask(q, Some("t_a")).unwrap();
    assert_eq!(ids_of(&b), want);
    // It stopped reading the strand where the keys passed the room, and read the table's order.
    assert!(round.order.contains("t_a"), "{round:?}");
    assert!(
        round.reads[0].column_pages_read < read,
        "{round:?} {roomy:?}"
    );
    assert_eq!(
        round.column_pages_read,
        round.reads.iter().map(|r| r.column_pages_read).sum::<u64>()
    );
}

#[test]
fn a_scan_begun_along_a_strand_hands_its_blocks_as_a_round_does() {
    let n = 20480;
    let mut s = fresh(&arrays("gin-scan", n));
    let q = "SELECT id FROM s.t WHERE a @> '{500}'";
    let price = s.price(q, None).unwrap();
    assert!(price.order.starts_with("t_a+"), "{price:?}");
    assert_eq!(price.table_rows, n as u64);
    let (want, round) = ids(&mut s, q, None);
    assert!(
        price.column_pages >= round.column_pages_read,
        "{price:?} {round:?}"
    );
    assert!(price.rows_low as usize <= want.len() && want.len() as u64 <= price.rows_high);
    let mut scan = s.scan(q, None).unwrap();
    let mut got = Vec::new();
    while let Some(b) = s.next_block(&mut scan).unwrap() {
        got.extend(ids_of(&b));
    }
    got.sort_unstable();
    assert_eq!(got, want);
    let r = scan.round();
    assert!(r.order.starts_with("t_a+"), "{r:?}");
    exact(q, &r);
    // Beside a condition on the table's key, the rows it can hold are the fewer.
    let narrow = s
        .price("SELECT id FROM s.t WHERE a @> '{9000}' AND id < 10", None)
        .unwrap();
    assert!(narrow.rows_low <= narrow.rows_high, "{narrow:?}");
    // A count its counts close, through the scan and the price.
    let q = "SELECT count(*) FROM s.t WHERE a @> '{500}'";
    let price = s.price(q, None).unwrap();
    assert_eq!(price.order, "t_a", "{price:?}");
    let mut scan = s.scan(q, None).unwrap();
    let b = s.next_block(&mut scan).unwrap().unwrap();
    let a = b.column(0).as_any().downcast_ref::<Int64Array>().unwrap();
    assert_eq!(a.value(0), want.len() as i64);
    assert_eq!(scan.round().order, "t_a");
}

#[test]
fn a_join_finds_a_table_s_rows_along_its_strand() {
    let ddl = "CREATE TABLE s.t (id int NOT NULL, a int[], PRIMARY KEY (id)); \
               CREATE INDEX t_a ON s.t USING gin (a); \
               CREATE TABLE s.u (id int NOT NULL, x bigint, PRIMARY KEY (id));";
    let catalog = Catalog::parse(ddl).unwrap();
    let n = 8192i64;
    let t = catalog.table("s.t").unwrap();
    let rows_t = Rows {
        columns: vec![
            Values::Int((0..n).map(Some).collect()),
            Values::Composed(
                (0..n)
                    .map(|i| composed(t.columns[1].ty, Some(&format!("{{{}}}", i % 397))))
                    .collect(),
            ),
        ],
    };
    let rows_u = Rows {
        columns: vec![
            Values::Int((0..n).map(Some).collect()),
            Values::Int((0..n).map(|i| Some(i * 2)).collect()),
        ],
    };
    let path = tmp("gin-join.snail");
    write(
        &path,
        &catalog,
        &[("s.t".into(), rows_t), ("s.u".into(), rows_u)],
    )
    .unwrap();
    let mut s = fresh(&path);
    let q = "SELECT t.id, u.x FROM s.t t JOIN s.u u ON t.id = u.id WHERE t.a @> '{9}'";
    let (b, round) = s.ask(q, None).unwrap();
    exact(q, &round);
    assert_eq!(b.num_rows(), (0..n).filter(|i| i % 397 == 9).count());
    assert!(round.order.contains("t_a"), "{round:?}");
}

#[test]
fn a_count_on_blocks_wholly_of_one_element_reads_no_column_page_of_them() {
    let n = 40960;
    let mut s = fresh(&arrays("gin-closes", n));
    let q = "SELECT count(*) FROM s.t WHERE a @> '{9000}'";
    let (c, round) = count(&mut s, q, None);
    assert_eq!(c, kept(n, |v| v.contains(&Some(9000))).len() as i64);
    assert_eq!(round.order, "t_a", "{round:?}");
    // The blocks wholly on it are closed by their counts: at most the two where it starts and ends.
    assert!(round.blocks_read <= 2, "{round:?}");
    assert!(round.entries_must > 0, "{round:?}");
    let (c, whole) = count(&mut s, q, Some("t_pkey"));
    assert_eq!(c, kept(n, |v| v.contains(&Some(9000))).len() as i64);
    assert!(
        round.column_pages_read * 10 < whole.column_pages_read,
        "{round:?} {whole:?}"
    );
}

#[test]
fn a_gist_on_a_tsquery_answers_as_its_scan_where_postgresql_s_index_misses_an_empty_value() {
    let mut s = snail_of("gin-misses", TEXTS, 8192, text_row);
    let empty = (0..8192).filter(|i| i % 17 == 1).count();
    let some = (0..8192).filter(|i| i % 17 != 0).count();
    assert_eq!(
        ids(&mut s, "SELECT id FROM s.t WHERE q <@ ''", Some("t_q"))
            .0
            .len(),
        empty
    );
    assert_eq!(
        ids(&mut s, "SELECT id FROM s.t WHERE q @> ''", Some("t_q"))
            .0
            .len(),
        some
    );
}

#[cfg(feature = "standalone")]
mod beside {
    use super::*;
    use std::time::Duration;
    use warren_snail::read::Snails;

    fn empty(name: &str, ddl: &str) -> std::path::PathBuf {
        let catalog = Catalog::parse(ddl).unwrap();
        let tables: Vec<(String, Rows)> = catalog
            .tables
            .iter()
            .filter(|t| t.declared())
            .map(|t| {
                (
                    t.name.clone(),
                    Rows {
                        columns: t.columns.iter().map(|c| Values::new(c.ty)).collect(),
                    },
                )
            })
            .collect();
        let path = tmp(&format!("{name}.snail"));
        write(&path, &catalog, &tables).unwrap();
        path
    }

    #[test]
    fn writers_of_one_block_of_a_strand_take_turns_at_read_committed() {
        let ddl = "CREATE TABLE s.t (id int NOT NULL, a int[], PRIMARY KEY (id)); \
                   CREATE INDEX t_a ON s.t USING gin (a);";
        let path = empty("gin-turns", ddl);
        let mut a = Snail::open_writing(&path).unwrap();
        // Rows far apart on the table's order, one element between them, and a block of each order.
        let values: Vec<String> = (1..=8000).map(|i| format!("({i}, '{{7}}')")).collect();
        a.execute(&format!(
            "INSERT INTO s.t (id, a) VALUES {}",
            values.join(", ")
        ))
        .unwrap();
        a.execute("BEGIN").unwrap();
        a.execute("UPDATE s.t SET a = '{7,8}' WHERE id = 1")
            .unwrap();
        let again = path.clone();
        let other = std::thread::spawn(move || {
            let mut b = Snail::open_writing(&again).unwrap();
            b.execute("UPDATE s.t SET a = '{7,8}' WHERE id = 8000")
                .unwrap();
        });
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            !other.is_finished(),
            "a writer of the strand's block waits its turn"
        );
        a.execute("COMMIT").unwrap();
        other.join().unwrap();
        let (found, _) = ids(&mut a, "SELECT id FROM s.t WHERE a @> '{8}'", Some("t_a"));
        assert_eq!(found, [1, 8000]);
    }

    #[test]
    fn a_stored_generated_column_is_routed_as_any_column_is() {
        let ddl = "CREATE TABLE s.t (id int NOT NULL, c text, v tsvector GENERATED ALWAYS AS \
                   (to_tsvector('simple', c)) STORED, PRIMARY KEY (id)); \
                   CREATE INDEX t_v ON s.t USING gin (v);";
        let path = empty("gin-generated", ddl);
        let mut s = Snail::open_writing(&path).unwrap();
        s.execute("INSERT INTO s.t (id, c) VALUES (1, 'the red fox'), (2, 'a blue fox')")
            .unwrap();
        s.execute("UPDATE s.t SET c = 'a blue hen' WHERE id = 2")
            .unwrap();
        let q = |w: &str| format!("SELECT id FROM s.t WHERE v @@ '{w}'");
        assert_eq!(ids(&mut s, &q("fox"), Some("t_v")).0, [1]);
        assert_eq!(ids(&mut s, &q("hen"), Some("t_v")).0, [2]);
        assert!(ids(&mut s, &q("absent"), Some("t_v")).0.is_empty());
    }

    #[test]
    fn a_transaction_across_snails_writes_each_strand_and_a_round_reads_them_together() {
        let one = empty(
            "gin-across-a",
            "CREATE TABLE s.t (id int NOT NULL, a int[], PRIMARY KEY (id)); \
             CREATE INDEX t_a ON s.t USING gin (a);",
        );
        let two = empty(
            "gin-across-b",
            "CREATE TABLE s.u (id int NOT NULL, j jsonb, PRIMARY KEY (id)); \
             CREATE INDEX u_j ON s.u USING gin (j);",
        );
        let mut w = Snails::open_writing(&[&one, &two]).unwrap();
        w.execute("BEGIN").unwrap();
        let rows =
            |f: &dyn Fn(i64) -> String| (1..=8000).map(|i| f(i)).collect::<Vec<_>>().join(", ");
        w.execute(&format!(
            "INSERT INTO s.t (id, a) VALUES {}",
            rows(&|i| format!("({i}, '{{{}}}')", i % 397))
        ))
        .unwrap();
        w.execute(&format!(
            "INSERT INTO s.u (id, j) VALUES {}",
            rows(&|i| format!("({i}, '{{\"k{}\": 1}}')", i % 3))
        ))
        .unwrap();
        w.execute("COMMIT").unwrap();
        let q =
            "SELECT t.id FROM s.t t JOIN s.u u ON t.id = u.id WHERE t.a @> '{5}' AND u.j ? 'k1'";
        let (b, round) = w.ask(q).unwrap();
        let want: Vec<i32> = (1..=8000).filter(|i| i % 397 == 5 && i % 3 == 1).collect();
        assert_eq!(ids_of(&b), want);
        assert!(round.order.contains("t_a"), "{round:?}");
    }
}

#[test]
fn a_jsonb_s_operators_are_read_on_its_column_with_no_strand_to_ask() {
    let ddl = "CREATE TABLE s.t (id int NOT NULL, j jsonb, v tsvector, PRIMARY KEY (id));";
    let mut s = snail_of("gin-bare", ddl, 1024, |i| {
        let mut r = jsonb_row(i);
        r.push(Some(format!("'w{}'", i % 7)));
        r
    });
    let (ans, _) = ids(&mut s, "SELECT id FROM s.t WHERE j ? 'z'", None);
    assert_eq!(ans.len(), (0..1024).filter(|i| i % 11 > 6).count());
    let (ans, _) = ids(&mut s, "SELECT id FROM s.t WHERE j @> '{\"k1\": 1}'", None);
    assert_eq!(ans.is_empty(), false);
    let (ans, _) = ids(&mut s, "SELECT id FROM s.t WHERE v @@ 'w1'", None);
    assert_eq!(ans.len(), (0..1024).filter(|i| i % 7 == 1).count());
    // Under OR and NOT, the question path reading neither, each row is held to it.
    let (b, _) = s
        .ask("SELECT id FROM s.t WHERE v @@ 'w1' OR j ? 'z'", None)
        .unwrap();
    assert_eq!(
        b.num_rows(),
        (0..1024).filter(|i| i % 7 == 1 || i % 11 > 6).count()
    );
    let (b, _) = s
        .ask("SELECT id FROM s.t WHERE NOT v @@ 'w1'", None)
        .unwrap();
    assert_eq!(b.num_rows(), (0..1024).filter(|i| i % 7 != 1).count());
}

#[test]
fn a_join_handed_over_is_priced_with_the_strand_it_reads() {
    let ddl = "CREATE TABLE s.t (id int NOT NULL, a int[], PRIMARY KEY (id)); \
               CREATE INDEX t_a ON s.t USING gin (a); \
               CREATE TABLE s.u (id int NOT NULL, x bigint, PRIMARY KEY (id));";
    let catalog = Catalog::parse(ddl).unwrap();
    let n = 8192i64;
    let t = catalog.table("s.t").unwrap();
    let path = tmp("gin-priced.snail");
    write(
        &path,
        &catalog,
        &[
            (
                "s.t".into(),
                Rows {
                    columns: vec![
                        Values::Int((0..n).map(Some).collect()),
                        Values::Composed(
                            (0..n)
                                .map(|i| {
                                    composed(t.columns[1].ty, Some(&format!("{{{}}}", i % 397)))
                                })
                                .collect(),
                        ),
                    ],
                },
            ),
            (
                "s.u".into(),
                Rows {
                    columns: vec![
                        Values::Int((0..n).map(Some).collect()),
                        Values::Int((0..n).collect::<Vec<_>>().into_iter().map(Some).collect()),
                    ],
                },
            ),
        ],
    )
    .unwrap();
    let s = fresh(&path);
    let q = "SELECT t.id, u.x FROM s.t t JOIN s.u u ON t.id = u.id WHERE t.a @> '{9}'";
    let priced = warren_snail::read::price_together(&[&s], q).unwrap();
    let (_, round) = fresh(&path).ask(q, None).unwrap();
    let most: u64 = priced.tables.iter().map(|t| t.column_pages_high).sum();
    let fewest: u64 = priced.tables.iter().map(|t| t.column_pages_low).sum();
    assert!(
        fewest <= round.column_pages_read && round.column_pages_read <= most,
        "{priced:?} {round:?}"
    );
    let along = priced.tables.iter().find(|t| t.table == "s.t").unwrap();
    assert!(
        along.column_pages_high > along.column_pages_estimate,
        "{along:?}"
    );
}

#[test]
fn an_empty_array_lies_on_the_strand_where_no_array_holds_a_null() {
    let ddl = "CREATE TABLE s.t (id int NOT NULL, a int[], PRIMARY KEY (id)); \
               CREATE INDEX t_a ON s.t USING gin (a);";
    let n = 20480;
    let mut s = snail_of("gin-empty", ddl, n, |i| {
        let a = match i % 1000 {
            0 => None,
            1 => Some("{}".to_string()),
            _ => Some(format!("{{{}}}", i % 7)),
        };
        vec![Some(i.to_string()), a]
    });
    let want: Vec<i32> = (0..n as i32).filter(|i| i % 1000 == 1).collect();
    for w in ["a = '{}'", "a <@ '{}'", "'{}' = a", "'{}' @> a"] {
        let q = format!("SELECT id FROM s.t WHERE {w}");
        let (found, round) = ids(&mut s, &q, Some("t_a"));
        assert_eq!(found, want, "{q}");
        // The strand's place, and the blocks of its keys, at most one a key.
        assert!(round.blocks_read as usize <= 1 + want.len(), "{round:?}");
    }
    // A NULL column holds no place of the strand; `a @> '{}'` is every array, by the table's order.
    let rows = s.blocks().unwrap();
    assert_eq!(
        strand_rows(&rows, "t_a", "id"),
        (0..n).filter(|i| i % 1000 != 0).count() as u64
    );
    let (all, round) = ids(&mut s, "SELECT id FROM s.t WHERE a @> '{}'", Some("t_a"));
    assert_eq!(all.len(), (0..n).filter(|i| i % 1000 != 0).count());
    assert_eq!(round.order, "t_pkey", "{round:?}");
}
