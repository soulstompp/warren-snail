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
    fresh(&snail_at(name, ddl, n, row))
}

fn snail_at(
    name: &str,
    ddl: &str,
    n: i64,
    row: impl Fn(i64) -> Vec<Option<String>>,
) -> std::path::PathBuf {
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
    path
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
    // Under OR and NOT, each row is held to it.
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

/// The column pages each read of a round read, the strands' first, each by the order it went along.
fn reads_of(round: &Round) -> Vec<(String, u64)> {
    round
        .reads
        .iter()
        .map(|r| (r.order.clone(), r.column_pages_read))
        .collect()
}

const PATHS: &str = "CREATE TABLE s.t (id int NOT NULL, j jsonb, pad bigint, PRIMARY KEY (id)); \
                     CREATE INDEX t_o ON s.t USING gin (j); \
                     CREATE INDEX t_p ON s.t USING gin (j jsonb_path_ops);";

/// Row `i`'s jsonb: an object of a few of many values, one whose key holds an array, one nested, and
/// every 256th a scalar or an array at the top.
fn path_doc(i: i64) -> Option<String> {
    if i % 256 == 3 {
        return Some(format!("\"k{}\"", i % 5));
    }
    if i % 256 == 7 {
        return Some(format!("[\"k{}\", \"m{}\"]", i % 5, i % 9));
    }
    if i % 256 == 11 {
        return None;
    }
    Some(match i % 3 {
        0 => format!(
            "{{\"a\": {}, \"b\": {}, \"z\": [{{\"y\": \"s{}\"}}]}}",
            i % 7,
            i % 997,
            i % 13
        ),
        1 => format!(
            "{{\"a\": [\"x{}\", {}.0], \"s\": [\"x{}\"]}}",
            i % 11,
            i % 5,
            i % 11
        ),
        _ => format!(
            "{{\"c\": {{\"d\": \"v{}\", \"e\": null, \"f\": true}}}}",
            i % 13
        ),
    })
}

fn path_row(i: i64) -> Vec<Option<String>> {
    vec![Some(i.to_string()), path_doc(i), Some(i.to_string())]
}

/// The rows of `n` holding what `f` asks of `i`, the three shapes above each its own.
fn path_kept(n: i64, f: impl Fn(i64) -> bool) -> Vec<i32> {
    (0..n)
        .filter(|&i| ![3, 7, 11].contains(&(i % 256)) && f(i))
        .map(|i| i as i32)
        .collect()
}

#[test]
fn a_path_s_equalities_read_its_strand_under_either_operator_class() {
    let n = 20480;
    let mut s = snail_of("gin-paths", PATHS, n, path_row);
    let b = |i: i64, v: i64| i % 3 == 0 && i % 997 == v;
    let cases: Vec<(&str, Vec<i32>)> = vec![
        ("j @@ '$.b == 100'", path_kept(n, |i| b(i, 100))),
        ("j @@ '100 == $.b'", path_kept(n, |i| b(i, 100))),
        ("j @@ '$.b == 100.0'", path_kept(n, |i| b(i, 100))),
        ("j @@ '$.b == 100'::jsonpath", path_kept(n, |i| b(i, 100))),
        ("j @? '$ ? (@.b == 100)'", path_kept(n, |i| b(i, 100))),
        ("j @? '$.b ? (@ == 100)'", path_kept(n, |i| b(i, 100))),
        (
            "j @@ 'exists($.b ? (@ == 100))'",
            path_kept(n, |i| b(i, 100)),
        ),
        (
            "j @@ '$.b == 100 || $.b == 101'",
            path_kept(n, |i| b(i, 100) || b(i, 101)),
        ),
        (
            "j @@ '$.b == 100 && $.a == 2'",
            path_kept(n, |i| b(i, 100) && i % 7 == 2),
        ),
        (
            "j @@ '$.b == 100 && $.a != 2'",
            path_kept(n, |i| b(i, 100) && i % 7 != 2),
        ),
        (
            "j @@ '$.b == 100 && $.a > 2'",
            path_kept(n, |i| b(i, 100) && i % 7 > 2),
        ),
        (
            "j @@ '!(!($.b == 100) || !($.a == 2))'",
            path_kept(n, |i| b(i, 100) && i % 7 == 2),
        ),
        (
            "j @@ '!(!($.b == 100) && !($.b == 101))'",
            path_kept(n, |i| b(i, 100) || b(i, 101)),
        ),
        (
            "j @@ '$.c.d == \"v5\"'",
            path_kept(n, |i| i % 3 == 2 && i % 13 == 5),
        ),
        (
            "j @@ 'strict $.c.d == \"v5\"'",
            path_kept(n, |i| i % 3 == 2 && i % 13 == 5),
        ),
        ("j @@ '$.c.e == null'", path_kept(n, |i| i % 3 == 2)),
        ("j @@ '$.c.f == true'", path_kept(n, |i| i % 3 == 2)),
        ("j @@ '$.c.f == false'", Vec::new()),
        (
            "j @@ '$.a[*] == \"x5\"'",
            path_kept(n, |i| i % 3 == 1 && i % 11 == 5),
        ),
        (
            "j @@ 'strict $.s[*] == \"x5\"'",
            path_kept(n, |i| i % 3 == 1 && i % 11 == 5),
        ),
        (
            "j @@ 'strict $.s[0] == \"x5\"'",
            path_kept(n, |i| i % 3 == 1 && i % 11 == 5),
        ),
        (
            "j @@ '$.z[*].y == \"s5\"'",
            path_kept(n, |i| i % 3 == 0 && i % 13 == 5),
        ),
        (
            "j @@ '$.z.y == \"s5\"'",
            path_kept(n, |i| i % 3 == 0 && i % 13 == 5),
        ),
        (
            "j @? '$.z[*] ? (@.y == \"s5\").y'",
            path_kept(n, |i| i % 3 == 0 && i % 13 == 5),
        ),
    ];
    for (w, want) in &cases {
        let q = format!("SELECT id FROM s.t WHERE {w}");
        assert_eq!(
            &ids(&mut s, &q, Some("t_pkey")).0,
            want,
            "{q} along the table's order"
        );
        for strand in ["t_o", "t_p"] {
            let (got, round) = ids(&mut s, &q, Some(strand));
            assert_eq!(&got, want, "{q} along {strand}");
            assert!(
                round.order.starts_with(strand),
                "{q} along {strand}: {round:?}"
            );
        }
        assert_eq!(&ids(&mut s, &q, None).0, want, "{q} by its pages");
    }
    // Under `jsonb_ops` alone: an accessor `jsonb_path_ops` cannot follow.
    let cases: Vec<(&str, Vec<i32>)> = vec![
        (
            "j @@ '$.c.* == \"v5\"'",
            path_kept(n, |i| i % 3 == 2 && i % 13 == 5),
        ),
        (
            "j @@ '$.c.** == \"v5\"'",
            path_kept(n, |i| i % 3 == 2 && i % 13 == 5),
        ),
        (
            "j @@ 'strict $.**.d == \"v5\"'",
            path_kept(n, |i| i % 3 == 2 && i % 13 == 5),
        ),
    ];
    for (w, want) in &cases {
        let q = format!("SELECT id FROM s.t WHERE {w}");
        assert_eq!(
            &ids(&mut s, &q, Some("t_pkey")).0,
            want,
            "{q} along the table's order"
        );
        let (got, round) = ids(&mut s, &q, Some("t_o"));
        assert_eq!(&got, want, "{q} along t_o");
        assert!(round.order.starts_with("t_o"), "{q}: {round:?}");
        let (got, round) = ids(&mut s, &q, Some("t_p"));
        assert_eq!(&got, want, "{q} along t_p");
        assert_eq!(round.order, "t_pkey", "{q}: {round:?}");
    }
    // Each read's pages: the strand reading fewest, the table's order kept to the keys it found.
    for (w, want) in [
        ("j @@ '$.b == 100'", [("t_p", 3), ("t_pkey", 49)]),
        (
            "j @@ '$.b == 100 || $.b == 101'",
            [("t_p", 3), ("t_pkey", 98)],
        ),
        ("j @@ '$.c.* == \"v5\"'", [("t_o", 27), ("t_pkey", 160)]),
        ("j @@ '$.c.e == null'", [("t_p", 20), ("t_pkey", 160)]),
        ("j @? '$ ? (@.b == 100)'", [("t_p", 3), ("t_pkey", 49)]),
    ] {
        let q = format!("SELECT id FROM s.t WHERE {w}");
        let (_, round) = ids(&mut s, &q, None);
        let want: Vec<(String, u64)> = want.iter().map(|(o, n)| (o.to_string(), *n)).collect();
        assert_eq!(reads_of(&round), want, "{q}");
    }
    let (_, whole) = ids(&mut s, "SELECT id FROM s.t WHERE j @? '$'", None);
    assert_eq!(
        (whole.order.as_str(), whole.column_pages_read),
        ("t_pkey", 160)
    );
}

#[test]
fn a_path_nothing_can_be_asked_from_reads_the_table_s_order() {
    let n = 20480;
    let mut s = snail_of("gin-paths-none", PATHS, n, path_row);
    // A jsonb has no `@@` with its path on its left, nor `@?` either way round.
    for w in [
        "'$.b == 100' @@ j",
        "'$.b == 100' @? j",
        "j @? 1",
        "j @@ '$.b ++'",
    ] {
        let q = format!("SELECT id FROM s.t WHERE {w}");
        assert!(s.ask(&q, None).is_err(), "{q}");
    }
    let (_, whole) = ids(&mut s, "SELECT id FROM s.t WHERE j @? '$'", Some("t_pkey"));
    let cases = [
        "j @? '$.a'",
        "j @? '$.b ? (@ > 100)'",
        "j @? '$.b == 100'",
        "j @@ '$.b != 100'",
        "j @@ '$.b > 100'",
        "j @@ '!($.b == 100)'",
        "j @@ '!($.b == 100 && $.a == 2)'",
        "j @@ '!($.b == 100 || $.a == 2)'",
        "j @@ '$.b == 100 || $.a > 2'",
        "j @@ '$.b == $.a'",
        "j @@ '$.a.type() == \"number\"'",
        "j @@ '1 == 1'",
        "j @@ '$.b'",
        "j @@ '($.b == 100) is unknown'",
        "j @@ '$.c.d like_regex \"^v5\"'",
        "j @@ '$.c.d starts with \"v5\"'",
    ];
    for w in cases {
        let q = format!("SELECT id FROM s.t WHERE {w}");
        let (want, along) = ids(&mut s, &q, Some("t_pkey"));
        assert_eq!(
            along.column_pages_read, whole.column_pages_read,
            "{q}: {along:?}"
        );
        let (got, round) = ids(&mut s, &q, None);
        assert_eq!(got, want, "{q}");
        assert_eq!(round.order, "t_pkey", "{q}: {round:?}");
        assert_eq!(
            round.column_pages_read, whole.column_pages_read,
            "{q}: {round:?}"
        );
        for strand in ["t_o", "t_p"] {
            let (got, round) = ids(&mut s, &q, Some(strand));
            assert_eq!(got, want, "{q} along {strand}");
            assert_eq!(round.order, "t_pkey", "{q} along {strand}: {round:?}");
        }
    }
}

/// A row the path passes on which each strand must not miss.
#[test]
fn what_must_hold_of_a_path_keeps_the_rows_postgresql_s_index_misses() {
    let n = 20480;
    let mut s = snail_of("gin-paths-misses", PATHS, n, path_row);
    let raw = |i: i64, k: i64| i % 256 == 3 && i % 5 == k;
    let in_array = |i: i64, k: i64| i % 256 == 7 && i % 5 == k;
    let a_is = |i: i64, v: i64| ![3, 7, 11].contains(&(i % 256)) && i % 3 == 0 && i % 7 == v;
    let cases: Vec<(&str, Vec<i32>)> = vec![
        (
            "j @@ 'strict $ == \"k1\"'",
            (0..n).filter(|&i| raw(i, 1)).map(|i| i as i32).collect(),
        ),
        (
            "j @? 'strict $ ? (@ == \"k1\")'",
            (0..n).filter(|&i| raw(i, 1)).map(|i| i as i32).collect(),
        ),
        (
            "j @@ '$ == \"k1\"'",
            (0..n)
                .filter(|&i| raw(i, 1) || in_array(i, 1))
                .map(|i| i as i32)
                .collect(),
        ),
        (
            "j @@ 'strict $[*] == \"k1\"'",
            (0..n)
                .filter(|&i| in_array(i, 1))
                .map(|i| i as i32)
                .collect(),
        ),
        (
            "j @@ '$.a == (-2).abs()'",
            (0..n)
                .filter(|&i| a_is(i, 2))
                .map(|i| i as i32)
                .chain((0..n).filter_map(|i| {
                    (![3, 7, 11].contains(&(i % 256)) && i % 3 == 1 && i % 5 == 2)
                        .then_some(i as i32)
                }))
                .collect::<std::collections::BTreeSet<i32>>()
                .into_iter()
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
        assert_eq!(&ids(&mut s, &q, Some("t_o")).0, want, "{q} along t_o");
        assert_eq!(&ids(&mut s, &q, Some("t_p")).0, want, "{q} along t_p");
        assert_eq!(&ids(&mut s, &q, None).0, want, "{q} by its pages");
    }
    // A NOT over an AND asks neither side of what it negates.
    let q = "SELECT id FROM s.t WHERE j @@ '!(!($.b == 100) && $.a == 2)'";
    let (want, _) = ids(&mut s, q, Some("t_pkey"));
    let expect: Vec<i32> = (0..n)
        .filter(|&i| {
            let doc = ![3, 7, 11].contains(&(i % 256));
            let (b, a) = (
                doc && i % 3 == 0 && i % 997 == 100,
                doc && i % 3 == 0 && i % 7 == 2,
            );
            // Its array beside a number compares a string with one: unknown, and no answer.
            let unknown = doc && i % 3 == 1;
            i % 256 != 11 && !unknown && (b || !a)
        })
        .map(|i| i as i32)
        .collect();
    assert_eq!(want, expect);
    let (got, round) = ids(&mut s, q, None);
    assert_eq!(got, want);
    assert_eq!(round.order, "t_pkey", "{round:?}");
}

#[test]
fn what_a_path_asks_is_what_each_operator_class_holds() {
    use warren_snail::jsonb::{gin_elements, gin_key, gin_path_elements};
    use warren_snail::jsonpath::{gin_asks, jsonpath_in, Asks};
    let held = |p: &str| jsonpath_in(p).unwrap();
    let ops = |p: &str, exists: bool| gin_asks(&held(p), exists, false).unwrap();
    let path = |p: &str, exists: bool| gin_asks(&held(p), exists, true).unwrap();
    let one = |doc: &str| gin_elements(doc).unwrap().remove(0);
    let last = |doc: &str| gin_elements(doc).unwrap().pop().unwrap();
    let on_path = |doc: &str| gin_path_elements(doc).unwrap().remove(0);
    // A string in lax mode may stand in an array, a key to `jsonb_ops`, or under a key.
    let string = |s: &str| {
        Asks::Any(vec![
            Asks::At(gin_key(s)),
            Asks::At(last(&format!("{{\"k\": \"{s}\"}}"))),
        ])
    };
    assert_eq!(
        ops("$.a == \"x\"", false),
        Some(Asks::All(vec![Asks::At(gin_key("a")), string("x")]))
    );
    assert_eq!(
        ops("$.a.b == 1", false),
        Some(Asks::All(vec![
            Asks::At(gin_key("b")),
            Asks::At(gin_key("a")),
            Asks::At(last("{\"a\": 1}"))
        ]))
    );
    assert_eq!(
        ops("strict $.a == \"x\"", false),
        Some(Asks::All(vec![
            Asks::At(gin_key("a")),
            Asks::At(last("{\"a\": \"x\"}"))
        ]))
    );
    assert_eq!(
        ops("strict $.a[*] == \"x\"", false),
        Some(Asks::All(vec![
            Asks::At(gin_key("a")),
            Asks::At(gin_key("x"))
        ]))
    );
    assert_eq!(ops("strict $.** == \"x\"", false), Some(string("x")));
    assert_eq!(
        ops("strict $.* == \"x\"", false),
        Some(Asks::At(last("{\"a\": \"x\"}")))
    );
    // A jsonb that is one string holds it as a key.
    assert_eq!(
        ops("strict $ == \"x\"", false),
        Some(Asks::At(one("\"x\"")))
    );
    assert_eq!(one("\"x\""), gin_key("x"));
    // The keys on the way down, each scalar with them, an array adding nothing.
    assert_eq!(
        path("$.a[*].b == 1", false),
        Some(Asks::At(on_path("{\"a\": {\"b\": 1}}")))
    );
    assert_eq!(
        path("$.a == \"x\"", false),
        Some(Asks::At(on_path("{\"a\": \"x\"}")))
    );
    assert_eq!(path("$ == true", false), Some(Asks::At(on_path("true"))));
    assert_eq!(path("$.* == 1", false), None);
    assert_eq!(path("$.** == 1", false), None);
    // A filter asks what its condition asks, under the path so far.
    assert_eq!(
        path("$.a ? (@.b == 1).c == 2", false),
        Some(Asks::All(vec![
            Asks::At(on_path("{\"a\": {\"b\": 1}}")),
            Asks::At(on_path("{\"a\": {\"c\": 2}}"))
        ]))
    );
    assert_eq!(path("$.a.type() ? (@ == 1)", false), None);
    assert_eq!(
        path("$.a ? (@.b == 1).type() == 1", false),
        Some(Asks::At(on_path("{\"a\": {\"b\": 1}}")))
    );
    // `@?` asks what a path's filters ask, and an existence nothing; `@@` asks of a predicate.
    assert_eq!(ops("$.a", true), None);
    assert_eq!(path("$.a", true), None);
    assert_eq!(ops("$.a", false), None);
    assert_eq!(ops("$.a == 1", true), None);
    assert_eq!(
        path("exists($.a ? (@ == 1))", false),
        Some(Asks::At(on_path("{\"a\": 1}")))
    );
    // A numeral's scale is no matter; what is applied to a scalar makes it no scalar.
    assert_eq!(
        path("$.a == 1.50", false),
        Some(Asks::At(on_path("{\"a\": 1.5}")))
    );
    assert_eq!(ops("$.a == (-1).abs()", false), None);
    assert_eq!(ops("(-1).abs() == $.a", false), None);
    assert_eq!(ops("($.a == 1).boolean()", false), None);
    // An AND keeps what it can ask of one side; an OR needs both.
    assert_eq!(
        path("$.a == 1 && $.b > 1", false),
        Some(Asks::At(on_path("{\"a\": 1}")))
    );
    assert_eq!(path("$.a == 1 || $.b > 1", false), None);
    assert_eq!(
        path("$.a == 1 || $.b == 2", false),
        Some(Asks::Any(vec![
            Asks::At(on_path("{\"a\": 1}")),
            Asks::At(on_path("{\"b\": 2}"))
        ]))
    );
    // A NOT turns them over, and asks nothing of what it negates.
    assert_eq!(path("!($.a == 1)", false), None);
    assert_eq!(path("!(exists($.a ? (@ == 1)))", false), None);
    assert_eq!(
        path("!(!($.a == 1))", false),
        Some(Asks::At(on_path("{\"a\": 1}")))
    );
    assert_eq!(
        path("!(!($.a == 1) || !($.b == 2))", false),
        Some(Asks::All(vec![
            Asks::At(on_path("{\"a\": 1}")),
            Asks::At(on_path("{\"b\": 2}"))
        ]))
    );
    assert_eq!(
        path("!(!($.a == 1) && !($.b == 2))", false),
        Some(Asks::Any(vec![
            Asks::At(on_path("{\"a\": 1}")),
            Asks::At(on_path("{\"b\": 2}"))
        ]))
    );
    assert_eq!(path("!(!($.a == 1) && $.b == 2)", false), None);
    assert_eq!(
        path("!(!($.a == 1) || $.b == 2)", false),
        Some(Asks::At(on_path("{\"a\": 1}")))
    );
    assert_eq!(
        path("$.b == 1 && !($.a == 1)", false),
        Some(Asks::At(on_path("{\"b\": 1}")))
    );
    // `$` starts over.
    assert_eq!(
        path("$.a ? ($.b == 1)", true),
        Some(Asks::At(on_path("{\"b\": 1}")))
    );
}

const OVER: &str =
    "CREATE TABLE s.t (id int NOT NULL, a int[], v tsvector, n int, PRIMARY KEY (id)); \
                    CREATE INDEX t_a ON s.t USING gin (a); \
                    CREATE INDEX t_v ON s.t USING gin (v);";
const OVER_ROWS: i64 = 20480;

/// Row `i`'s array of a common element, a rare one and one whose rows lie together, a NULL every
/// 50th; its tsvector the same, every 51st; `n` of ten, a NULL every 11th.
fn over_row(i: i64) -> Vec<Option<String>> {
    vec![
        Some(i.to_string()),
        (i % 50 != 0).then(|| format!("{{{},{},{}}}", i % 7, i % 997 + 100, 2000 + i / 256)),
        (i % 51 != 0).then(|| format!("'w{}':1 'word{}':2 'z{}':3", i % 7, i % 997, i / 256)),
        (i % 11 != 0).then(|| (i % 10).to_string()),
    ]
}

fn over_kept(f: impl Fn(i64) -> bool) -> Vec<i32> {
    (0..OVER_ROWS).filter(|&i| f(i)).map(|i| i as i32).collect()
}

/// Whether row `i`'s array, its tsvector, hold the element or the word: none where the column is NULL.
fn rare_a(i: i64, x: i64) -> Option<bool> {
    (i % 50 != 0).then_some(i % 997 + 100 == x)
}
fn rare_v(i: i64, x: i64) -> Option<bool> {
    (i % 51 != 0).then_some(i % 997 == x)
}
fn or(a: Option<bool>, b: Option<bool>) -> bool {
    a == Some(true) || b == Some(true)
}

#[test]
fn an_or_over_a_strand_s_operators_reads_it_where_its_pages_are_fewer() {
    let mut s = snail_of("gin-or", OVER, OVER_ROWS, over_row);
    let cases: Vec<(&str, Vec<i32>)> = vec![
        (
            "a @> '{150}' OR a @> '{151}'",
            over_kept(|i| or(rare_a(i, 150), rare_a(i, 151))),
        ),
        (
            "a && '{150}' OR a @> '{151}'",
            over_kept(|i| or(rare_a(i, 150), rare_a(i, 151))),
        ),
        (
            "(a @> '{150}' OR a @> '{151}') OR a @> '{152}'",
            over_kept(|i| or(rare_a(i, 150), rare_a(i, 151)) || rare_a(i, 152) == Some(true)),
        ),
        (
            "a @> '{150,NULL}' OR a @> '{151}'",
            over_kept(|i| rare_a(i, 151) == Some(true)),
        ),
        (
            "a @> '{150}' OR a @> '{150}'",
            over_kept(|i| rare_a(i, 150) == Some(true)),
        ),
        (
            "(a @> '{150}' OR a @> '{151}') AND n > 3",
            over_kept(|i| or(rare_a(i, 150), rare_a(i, 151)) && i % 11 != 0 && i % 10 > 3),
        ),
    ];
    for (w, want) in &cases {
        let q = format!("SELECT id FROM s.t WHERE {w}");
        assert_eq!(
            &ids(&mut s, &q, Some("t_pkey")).0,
            want,
            "{q} along the table's order"
        );
        let (got, round) = ids(&mut s, &q, Some("t_a"));
        assert_eq!(&got, want, "{q} along t_a");
        assert!(round.order.starts_with("t_a+"), "{q}: {round:?}");
        let (got, round) = ids(&mut s, &q, None);
        assert_eq!(&got, want, "{q} by its pages");
        assert!(round.order.starts_with("t_a+"), "{q}: {round:?}");
    }
    // Across two strands, each read for its own, the rows met by their keys.
    let cases: Vec<(&str, Vec<i32>)> = vec![
        (
            "a @> '{150}' OR v @@ 'word60'",
            over_kept(|i| or(rare_a(i, 150), rare_v(i, 60))),
        ),
        (
            "v @@ 'word60' OR a @> '{150}'",
            over_kept(|i| or(rare_a(i, 150), rare_v(i, 60))),
        ),
        (
            "(a @> '{150}' AND v @@ 'word50') OR v @@ 'word60'",
            over_kept(|i| {
                rare_a(i, 150) == Some(true) && rare_v(i, 50) == Some(true)
                    || rare_v(i, 60) == Some(true)
            }),
        ),
        (
            "(a @> '{150}' OR v @@ 'word60') AND (a @> '{3}' OR v @@ 'w3')",
            over_kept(|i| {
                or(rare_a(i, 150), rare_v(i, 60)) && i % 7 == 3 && (i % 50 != 0 || i % 51 != 0)
            }),
        ),
        (
            "(a @> '{150}' OR v @@ 'word60') AND v @@ 'w3'",
            over_kept(|i| or(rare_a(i, 150), rare_v(i, 60)) && i % 7 == 3 && i % 51 != 0),
        ),
    ];
    for (w, want) in &cases {
        let q = format!("SELECT id FROM s.t WHERE {w}");
        assert_eq!(
            &ids(&mut s, &q, Some("t_pkey")).0,
            want,
            "{q} along the table's order"
        );
        let (got, round) = ids(&mut s, &q, None);
        assert_eq!(&got, want, "{q} by its pages");
        assert!(
            round.order.contains("t_pkey") && round.reads.len() >= 2,
            "{q}: {round:?}"
        );
    }
    // Named one strand of the two, the table's order is read, the other arm's rows being past it.
    let q = "SELECT id FROM s.t WHERE a @> '{150}' OR v @@ 'word60'";
    let (got, round) = ids(&mut s, q, Some("t_a"));
    assert_eq!(got, over_kept(|i| or(rare_a(i, 150), rare_v(i, 60))));
    assert_eq!(round.order, "t_pkey", "{round:?}");
    // Each read's pages, an element whose rows lie together, one spread over the table, none held.
    let (_, whole) = ids(
        &mut s,
        "SELECT id FROM s.t WHERE a IS NOT NULL",
        Some("t_pkey"),
    );
    assert_eq!(whole.column_pages_read, 140);
    for (w, want) in [
        ("a @> '{2005}'", vec![("t_a", 3), ("t_pkey", 5)]),
        (
            "a @> '{2005}' OR a @> '{2006}'",
            vec![("t_a", 3), ("t_pkey", 5)],
        ),
        (
            "a @> '{2005}' OR a @> '{2070}'",
            vec![("t_a", 6), ("t_pkey", 10)],
        ),
        (
            "a @> '{2005}' OR v @@ 'z70'",
            vec![("t_a", 3), ("t_v", 3), ("t_pkey", 16)],
        ),
        (
            "(a @> '{2005}' AND v @@ 'w3') OR v @@ 'z70'",
            vec![("t_a", 3), ("t_v", 11), ("t_pkey", 16)],
        ),
        (
            "(a @> '{2005}' OR a @> '{2006}') AND n > 3",
            vec![("t_a", 3), ("t_pkey", 6)],
        ),
        (
            "(a @> '{2005}' OR v @@ 'z70') AND v @@ 'w3'",
            vec![("t_a", 3), ("t_v", 11), ("t_pkey", 16)],
        ),
        // Common elements read at most the table and their strand, as one element's do.
        (
            "a @> '{3}' OR a @> '{4}'",
            vec![("t_a", 16), ("t_pkey", 140)],
        ),
        // No row holds them: nothing past the directory.
        (
            "a @> '{99999}' OR v @@ 'absent'",
            vec![("t_a", 0), ("t_v", 0), ("t_pkey", 0)],
        ),
    ] {
        let q = format!("SELECT id FROM s.t WHERE {w}");
        let (_, round) = ids(&mut s, &q, None);
        let want: Vec<(String, u64)> = want.iter().map(|(o, n)| (o.to_string(), *n)).collect();
        assert_eq!(reads_of(&round), want, "{q}");
    }
}

#[test]
fn an_or_that_could_pass_a_row_holding_nothing_reads_the_table_s_order() {
    let mut s = snail_of("gin-or-whole", OVER, OVER_ROWS, over_row);
    let cases: Vec<(&str, Vec<i32>)> = vec![
        (
            "a @> '{150}' OR n = 3",
            over_kept(|i| rare_a(i, 150) == Some(true) || (i % 11 != 0 && i % 10 == 3)),
        ),
        (
            "a @> '{150}' OR a IS NULL",
            over_kept(|i| rare_a(i, 150) != Some(false)),
        ),
        (
            "a IS NULL OR v @@ 'word60'",
            over_kept(|i| i % 50 == 0 || rare_v(i, 60) == Some(true)),
        ),
        (
            "a @> '{150}' OR n IS NULL",
            over_kept(|i| rare_a(i, 150) == Some(true) || i % 11 == 0),
        ),
        (
            "a @> '{150}' OR v @@ '!word60'",
            over_kept(|i| rare_a(i, 150) == Some(true) || rare_v(i, 60) == Some(false)),
        ),
        (
            "a @> '{150}' OR NOT v @@ 'word60'",
            over_kept(|i| rare_a(i, 150) == Some(true) || rare_v(i, 60) == Some(false)),
        ),
        ("a @> '{150}' OR a @> '{}'", over_kept(|i| i % 50 != 0)),
    ];
    for (w, want) in &cases {
        let q = format!("SELECT id FROM s.t WHERE {w}");
        let (got, along) = ids(&mut s, &q, Some("t_pkey"));
        assert_eq!(&got, want, "{q} along the table's order");
        let (got, round) = ids(&mut s, &q, None);
        assert_eq!(&got, want, "{q} by its pages");
        assert_eq!(round.order, "t_pkey", "{q}: {round:?}");
        assert_eq!(round.column_pages_read, along.column_pages_read, "{q}");
    }
}

#[test]
fn a_tsquery_s_own_operators_read_the_strand_as_an_or_of_its_words_does() {
    let mut s = snail_of("gin-tsquery-ops", OVER, OVER_ROWS, over_row);
    let cases: Vec<(&str, Vec<i32>)> = vec![
        (
            "v @@ 'word60 | word61'",
            over_kept(|i| or(rare_v(i, 60), rare_v(i, 61))),
        ),
        (
            "v @@ 'word60 & w4'",
            over_kept(|i| rare_v(i, 60) == Some(true) && i % 7 == 4),
        ),
        (
            "v @@ 'word60 & !w4'",
            over_kept(|i| rare_v(i, 60) == Some(true) && i % 7 != 4),
        ),
        (
            "v @@ 'w4 <-> word60'",
            over_kept(|i| rare_v(i, 60) == Some(true) && i % 7 == 4),
        ),
        (
            "v @@ '(word60 | word61) & !(w4 | w5)'",
            over_kept(|i| or(rare_v(i, 60), rare_v(i, 61)) && i % 7 != 4 && i % 7 != 5),
        ),
    ];
    for (w, want) in &cases {
        let q = format!("SELECT id FROM s.t WHERE {w}");
        assert_eq!(
            &ids(&mut s, &q, Some("t_pkey")).0,
            want,
            "{q} along the table's order"
        );
        let (got, round) = ids(&mut s, &q, Some("t_v"));
        assert_eq!(&got, want, "{q} along t_v");
        assert!(round.order.starts_with("t_v+"), "{q}: {round:?}");
        let (got, round) = ids(&mut s, &q, None);
        assert_eq!(&got, want, "{q} by its pages");
        assert!(round.order.starts_with("t_v+"), "{q}: {round:?}");
    }
    for (w, want) in [
        ("v @@ 'z5 | z6'", [("t_v", 7), ("t_pkey", 6)]),
        ("v @@ 'z5 & w4'", [("t_v", 11), ("t_pkey", 4)]),
        ("v @@ 'z5 & !w4'", [("t_v", 3), ("t_pkey", 4)]),
        ("v @@ 'w4 <-> word60'", [("t_v", 11), ("t_pkey", 6)]),
    ] {
        let q = format!("SELECT id FROM s.t WHERE {w}");
        let (_, round) = ids(&mut s, &q, None);
        let want: Vec<(String, u64)> = want.iter().map(|(o, n)| (o.to_string(), *n)).collect();
        assert_eq!(reads_of(&round), want, "{q}");
    }
    let (_, one) = ids(
        &mut s,
        "SELECT id FROM s.t WHERE v @@ 'word60 | word61'",
        None,
    );
    let (_, two) = ids(
        &mut s,
        "SELECT id FROM s.t WHERE v @@ 'word60' OR v @@ 'word61'",
        None,
    );
    assert_eq!(reads_of(&one), reads_of(&two));
    // What asks nothing of any word reads the table's order, the strand named or not.
    for w in [
        "v @@ '!word60'",
        "v @@ 'word60 | !word61'",
        "v @@ '!(word60 & word61)'",
    ] {
        let q = format!("SELECT id FROM s.t WHERE {w}");
        let (want, _) = ids(&mut s, &q, Some("t_pkey"));
        for along in [Some("t_v"), None] {
            let (got, round) = ids(&mut s, &q, along);
            assert_eq!(got, want, "{q}");
            assert_eq!(round.order, "t_pkey", "{q}: {round:?}");
        }
    }
}

#[test]
fn a_not_on_its_own_reads_the_table_s_order_and_beside_a_condition_is_held_to_each_row() {
    let mut s = snail_of("gin-not", OVER, OVER_ROWS, over_row);
    let (_, whole_a) = ids(
        &mut s,
        "SELECT id FROM s.t WHERE a IS NOT NULL",
        Some("t_pkey"),
    );
    assert_eq!(whole_a.column_pages_read, 140);
    let cases: Vec<(&str, Vec<i32>)> = vec![
        (
            "NOT a @> '{150}'",
            over_kept(|i| rare_a(i, 150) == Some(false)),
        ),
        (
            "NOT (a @> '{150}' OR a @> '{151}')",
            over_kept(|i| i % 50 != 0 && !or(rare_a(i, 150), rare_a(i, 151))),
        ),
        (
            "NOT (a @> '{150}' AND a @> '{3}')",
            over_kept(|i| i % 50 != 0 && !(rare_a(i, 150) == Some(true) && i % 7 == 3)),
        ),
    ];
    for (w, want) in &cases {
        let q = format!("SELECT id FROM s.t WHERE {w}");
        for along in [Some("t_pkey"), Some("t_a"), None] {
            let (got, round) = ids(&mut s, &q, along);
            assert_eq!(&got, want, "{q} along {along:?}");
            assert_eq!(round.order, "t_pkey", "{q} along {along:?}: {round:?}");
            assert!(
                round.reads.iter().all(|r| !r.order.starts_with("t_a")),
                "{round:?}"
            );
            assert_eq!(
                round.column_pages_read, whole_a.column_pages_read,
                "{q}: {round:?}"
            );
        }
    }
    // Beside a condition a strand answers, the strand is read and the NOT held to its rows.
    let q = "SELECT id FROM s.t WHERE a @> '{2005}' AND NOT a @> '{3}'";
    let (got, round) = ids(&mut s, q, None);
    assert_eq!(
        got,
        over_kept(|i| i % 50 != 0 && i / 256 == 5 && i % 7 != 3)
    );
    assert_eq!(
        reads_of(&round),
        [("t_a".to_string(), 3), ("t_pkey".to_string(), 5)]
    );
    // Beside a condition on the table's key, its range is read.
    let q = "SELECT id FROM s.t WHERE id < 1000 AND NOT a @> '{3}'";
    let (got, round) = ids(&mut s, q, None);
    assert_eq!(got, over_kept(|i| i < 1000 && i % 50 != 0 && i % 7 != 3));
    assert_eq!(round.order, "t_pkey", "{round:?}");
    assert!(
        round.column_pages_read * 5 < whole_a.column_pages_read,
        "{round:?}"
    );
    // A NULL is no answer to a NOT, nor to an OR of nothing but NULLs and falses.
    let q = "SELECT id FROM s.t WHERE NOT v @@ 'word60' OR NOT a @> '{150}'";
    let (got, _) = ids(&mut s, q, None);
    assert_eq!(
        got,
        over_kept(|i| rare_v(i, 60) == Some(false) || rare_a(i, 150) == Some(false))
    );
}

#[test]
fn an_or_is_priced_and_scanned_along_its_strands() {
    let mut s = snail_of("gin-or-scan", OVER, OVER_ROWS, over_row);
    let q = "SELECT id FROM s.t WHERE a @> '{150}' OR v @@ 'word60'";
    let (want, round) = ids(&mut s, q, None);
    let price = s.price(q, None).unwrap();
    assert_eq!(price.order, "t_a+t_v+t_pkey", "{price:?}");
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
    assert!(r.order.contains("t_a") && r.order.contains("t_v"), "{r:?}");
    exact(q, &r);
    // A count is no element's count: its arms may hold one row twice.
    let q = "SELECT count(*) FROM s.t WHERE a @> '{150}' OR a @> '{151}'";
    let (c, round) = count(&mut s, q, None);
    assert_eq!(
        c as usize,
        over_kept(|i| or(rare_a(i, 150), rare_a(i, 151))).len()
    );
    assert!(round.order.starts_with("t_a+"), "{round:?}");
    // A condition by a NOT reads its column though no answer asks it; another column besides.
    let q = "SELECT count(*) FROM s.t WHERE NOT a @> '{150}' OR n = 3";
    let (c, _) = count(&mut s, q, None);
    assert_eq!(
        c as usize,
        over_kept(|i| rare_a(i, 150) == Some(false) || (i % 11 != 0 && i % 10 == 3)).len()
    );
    // Its price reads the table's order where the strands would not pay.
    let q = "SELECT id FROM s.t WHERE a @> '{150}' OR n = 3";
    assert_eq!(s.price(q, None).unwrap().order, "t_pkey");
}

#[test]
fn a_join_reads_a_side_s_or_along_its_strands() {
    let ddl = format!("{OVER} CREATE TABLE s.u (id int NOT NULL, x bigint, PRIMARY KEY (id));");
    let catalog = Catalog::parse(&ddl).unwrap();
    let n = 8192i64;
    let t = catalog.table("s.t").unwrap();
    let mut columns: Vec<Values> = t.columns.iter().map(|c| Values::new(c.ty)).collect();
    for i in 0..n {
        for ((v, c), x) in columns.iter_mut().zip(&t.columns).zip(&over_row(i)) {
            match (v, c.ty, x) {
                (Values::Int(a), _, x) => a.push(x.as_ref().map(|s| s.parse().unwrap())),
                (Values::Composed(a), ty, x) => a.push(composed(ty, x.as_deref())),
                (v, ColumnType::Textual(t), Some(x)) => push_datum(
                    v,
                    &Datum::Text(warren_snail::textual::text_in(t, x).unwrap()),
                ),
                (v, _, _) => v.push_null(),
            }
        }
    }
    let path = tmp("gin-or-join.snail");
    write(
        &path,
        &catalog,
        &[
            ("s.t".into(), Rows { columns }),
            (
                "s.u".into(),
                Rows {
                    columns: vec![
                        Values::Int((0..n).map(Some).collect()),
                        Values::Int((0..n).map(|i| Some(i * 2)).collect()),
                    ],
                },
            ),
        ],
    )
    .unwrap();
    let mut s = fresh(&path);
    let q = "SELECT x.id, u.x FROM s.t x JOIN s.u u ON x.id = u.id \
             WHERE x.a @> '{150}' OR x.v @@ 'word60'";
    let (b, round) = s.ask(q, None).unwrap();
    exact(q, &round);
    let want = (0..n)
        .filter(|&i| or(rare_a(i, 150), rare_v(i, 60)))
        .count();
    assert_eq!(b.num_rows(), want);
    assert!(
        round.order.contains("t_a") && round.order.contains("t_v"),
        "{round:?}"
    );
    // Written whole, one side's own name on each part.
    let q = "SELECT s.t.id FROM s.t JOIN s.u ON s.t.id = s.u.id \
             WHERE (s.t.a @> '{150}' OR s.t.n = 3) AND s.t.v @@ 'w3'";
    let (b, round) = s.ask(q, None).unwrap();
    exact(q, &round);
    let want = (0..n)
        .filter(|&i| {
            (rare_a(i, 150) == Some(true) || (i % 11 != 0 && i % 10 == 3))
                && i % 51 != 0
                && i % 7 == 3
        })
        .count();
    assert_eq!(b.num_rows(), want);
}

const THREE_ATS: &str =
    "CREATE TABLE s.t (id int NOT NULL, v tsvector, q tsquery, t text, j jsonb, n int, \
                         PRIMARY KEY (id)); \
                         CREATE INDEX t_v ON s.t USING gin (v); \
                         CREATE INDEX t_q ON s.t USING gist (q);";

fn three_ats_row(i: i64) -> Vec<Option<String>> {
    vec![
        Some(i.to_string()),
        (i % 13 != 0).then(|| format!("'w{}':1 'word{}':2", i % 7, i % 397)),
        Some(format!("'word{}'", i % 397)),
        Some(format!("word{}", i % 397)),
        Some(format!("{{\"a\": {}}}", i % 7)),
        Some((i % 10).to_string()),
    ]
}

#[test]
fn three_ats_read_as_the_match_they_are_where_they_can_be_told_apart() {
    let n = 8192;
    let mut s = snail_of("gin-three-ats", THREE_ATS, n, three_ats_row);
    let word = |i: i64, w: i64| i % 13 != 0 && i % 397 == w;
    for w in [
        "v @@@ 'word60'",
        "'word60' @@@ v",
        "v @@@ 'word60'::tsquery",
        "'word60'::tsquery @@@ v",
        "v @@@ to_tsquery('simple', 'word60')",
        "v @@@ 'word60' AND n >= 0",
    ] {
        let q = format!("SELECT id FROM s.t WHERE {w}");
        let want: Vec<i32> = (0..n).filter(|&i| word(i, 60)).map(|i| i as i32).collect();
        assert_eq!(
            ids(&mut s, &q, Some("t_pkey")).0,
            want,
            "{q} along the table's order"
        );
        assert_eq!(ids(&mut s, &q, Some("t_v")).0, want, "{q} along t_v");
        let (got, round) = ids(&mut s, &q, None);
        assert_eq!(got, want, "{q} by its pages");
        assert!(round.order.starts_with("t_v"), "{q}: {round:?}");
    }
    // As `@@`, a word for word: the same reads, its count closed.
    let (_, three) = ids(&mut s, "SELECT id FROM s.t WHERE v @@@ 'word60'", None);
    let (_, two) = ids(&mut s, "SELECT id FROM s.t WHERE v @@ 'word60'", None);
    assert_eq!(reads_of(&three), reads_of(&two));
    let (c, round) = count(
        &mut s,
        "SELECT count(*) FROM s.t WHERE v @@@ 'word60'",
        None,
    );
    assert_eq!(c as usize, (0..n).filter(|&i| word(i, 60)).count());
    assert_eq!(round.order, "t_v", "{round:?}");
    // Under an OR and a NOT, each row is held to it.
    let (got, round) = ids(
        &mut s,
        "SELECT id FROM s.t WHERE v @@@ 'word60' OR v @@@ 'word61'",
        None,
    );
    let want: Vec<i32> = (0..n)
        .filter(|&i| word(i, 60) || word(i, 61))
        .map(|i| i as i32)
        .collect();
    assert_eq!(got, want);
    assert!(round.order.starts_with("t_v+"), "{round:?}");
    let (got, _) = ids(&mut s, "SELECT id FROM s.t WHERE NOT v @@@ 'word60'", None);
    assert_eq!(
        got.len(),
        (0..n).filter(|&i| i % 13 != 0 && i % 397 != 60).count()
    );
    // Two columns, and a tsquery on its left: a row at a time.
    let rows = |s: &mut Snail, q: &str| s.ask(q, None).unwrap().0.num_rows();
    let q = "SELECT id FROM s.t WHERE v @@@ q";
    assert_eq!(rows(&mut s, q), (0..n).filter(|&i| i % 13 != 0).count());
    let q = "SELECT id FROM s.t WHERE q @@@ v";
    assert_eq!(rows(&mut s, q), (0..n).filter(|&i| i % 13 != 0).count());
    let q = "SELECT id FROM s.t WHERE q @@@ 'word60'";
    assert_eq!(rows(&mut s, q), (0..n).filter(|&i| i % 397 == 60).count());
    let (b, _) = s
        .ask(
            "WITH h AS (SELECT id, v, q FROM s.t) SELECT id, v @@@ q AS m FROM h WHERE id < 3",
            None,
        )
        .unwrap();
    assert_eq!(b.num_rows(), 3);
    // On no text search type, no such operator.
    for w in [
        "t @@@ 'word60'",
        "'word60' @@@ t",
        "j @@@ '$.a == 1'",
        "n @@@ 'x'",
        "'a' @@@ 'b'",
    ] {
        let q = format!("SELECT id FROM s.t WHERE {w}");
        assert!(s.ask(&q, None).is_err(), "{q}");
    }
    // `@@` and a prefix `@` written apart are two operators, each refused as PostgreSQL refuses it.
    for w in ["v @@ @ 'word60'", "v @@  @ 1", "v @@\n@ 'word60'"] {
        let q = format!("SELECT id FROM s.t WHERE {w}");
        let e = s
            .ask(&q, None)
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(e.contains("operator"), "{q}: {e}");
    }
    // A string or a comment holding them is no operator.
    let (got, _) = ids(
        &mut s,
        "SELECT id FROM s.t WHERE t = '@@ @ ' /* @@ @ */",
        None,
    );
    assert!(got.is_empty());
}

const BESIDE: &str =
    "CREATE TABLE s.t (id int NOT NULL, v tsvector, r int4range, n int, PRIMARY KEY (id)); \
                      CREATE INDEX t_vr ON s.t USING gist (v, r);";

fn beside_row(i: i64) -> Vec<Option<String>> {
    vec![
        Some(i.to_string()),
        (i % 13 != 0).then(|| format!("'w{}':1 'word{}':2", i % 7, i % 997)),
        (i % 17 != 0).then(|| format!("[{i},{})", i + 3)),
        Some((i % 10).to_string()),
    ]
}

#[test]
fn a_gist_index_beside_a_key_of_another_kind_holds_a_strand_of_each() {
    let table =
        "CREATE TABLE s.t (id int NOT NULL, v tsvector, q tsquery, r int4range, m int4multirange, \
                 i inet, n int, PRIMARY KEY (id));";
    let c = Catalog::parse(&format!(
        "{table}\nCREATE INDEX e ON s.t USING gist (v, r);"
    ))
    .unwrap();
    let t = c.table("s.t").unwrap();
    // The range's keys an order on the table; the tsvector a strand of its own.
    let order = t.orders.iter().find(|o| o.name == "e").unwrap();
    assert_eq!(order.levels.len(), 1);
    assert_eq!(order.keys, ["r"]);
    assert_eq!(order.opclasses, ["range_ops"]);
    assert_eq!(t.element_keys.len(), 1);
    assert_eq!(t.element_keys[0].key, 0);
    assert_eq!(t.element_keys[0].opclass, "tsvector_ops");
    assert!(c.table(&t.element_keys[0].strand).is_some());
    // One index to the catalog, its keys in their places.
    let facts = c.facts();
    let told = |object: &str, fact: &str| -> Vec<String> {
        facts
            .iter()
            .filter(|f| f.object == object && f.fact == fact)
            .map(|f| f.value.clone())
            .collect()
    };
    assert_eq!(told("s.t.e", "keys"), ["v,r"]);
    assert_eq!(told("s.t.e", "opclasses"), ["tsvector_ops,range_ops"]);
    assert_eq!(told("s.t.e", "method"), ["gist"]);
    assert_eq!(told("s.t.e", "desc"), ["false,false"]);
    assert_eq!(told("s.t.e", "nulls_first"), ["false,false"]);
    let c = Catalog::parse(&format!(
        "{table}\nCREATE INDEX e ON s.t USING gist (r, v, m, q) WHERE n > 0;"
    ))
    .unwrap();
    let t = c.table("s.t").unwrap();
    let order = t.orders.iter().find(|o| o.name == "e").unwrap();
    assert_eq!(order.keys, ["r", "m"]);
    assert!(order.predicate.is_some());
    assert_eq!(
        t.element_keys.iter().map(|k| k.key).collect::<Vec<_>>(),
        [1, 3]
    );
    assert!(t.element_keys.iter().all(|k| k.predicate.is_some()));
    let facts = c.facts();
    let told = |fact: &str| -> String {
        facts
            .iter()
            .find(|f| f.object == "s.t.e" && f.fact == fact)
            .map(|f| f.value.clone())
            .unwrap()
    };
    assert_eq!(told("keys"), "r,v,m,q");
    assert_eq!(
        told("opclasses"),
        "range_ops,tsvector_ops,multirange_ops,tsquery_ops"
    );
    // Refused as ever: a key no GiST holds, a key PostgreSQL refuses.
    let got = outcomes(&format!(
        "{table}\nCREATE INDEX e ON s.t USING gist (v, n);"
    ));
    assert!(
        got[1]
            .as_deref()
            .unwrap()
            .contains("no default operator class"),
        "{got:?}"
    );
    // Clustered on it, the table has no order one row a place.
    let got = outcomes(&format!(
        "{table}\nCREATE INDEX e ON s.t USING gist (v, r);\nALTER TABLE s.t CLUSTER ON e;"
    ));
    assert!(got[2].as_deref().unwrap().contains("element"), "{got:?}");
    // A format with no strands refuses it by name.
    let path = tmp("gin-beside-eight.snail");
    let e = warren_snail::write::write_format_eight(
        &path,
        &Catalog::parse(&format!(
            "{table}\nCREATE INDEX e ON s.t USING gist (v, r);"
        ))
        .unwrap(),
        &[],
        weather::span(),
    )
    .err()
    .unwrap()
    .to_string();
    assert!(
        e.contains("the gist index e, which a snail of format 8"),
        "{e}"
    );
}

#[test]
fn a_gist_index_beside_another_kind_reads_each_key_where_it_serves() {
    let n = 20480;
    let mut s = snail_of("gin-beside", BESIDE, n, beside_row);
    let word = |i: i64, w: i64| i % 13 != 0 && i % 997 == w;
    let span = |i: i64, lo: i64, hi: i64| i % 17 != 0 && i < hi && i + 3 > lo;
    let kept = |f: &dyn Fn(i64) -> bool| -> Vec<i32> {
        (0..n).filter(|&i| f(i)).map(|i| i as i32).collect()
    };
    // On the range alone: its order, by its bounds.
    let q = "SELECT id FROM s.t WHERE r && '[5000,5040)'";
    let want = kept(&|i| span(i, 5000, 5040));
    assert_eq!(ids(&mut s, q, Some("t_pkey")).0, want);
    let (got, along) = ids(&mut s, q, Some("t_vr"));
    assert_eq!(got, want);
    assert_eq!(along.order, "t_vr", "{along:?}");
    let (got, round) = ids(&mut s, q, None);
    assert_eq!(got, want);
    assert_eq!(round.order, "t_vr", "{round:?}");
    assert_eq!(
        (along.column_pages_read, along.blocks_read),
        (11, 1),
        "{along:?}"
    );
    // On the tsvector alone: its strand, the rows by their keys.
    let q = "SELECT id FROM s.t WHERE v @@ 'word60'";
    let want = kept(&|i| word(i, 60));
    assert_eq!(ids(&mut s, q, Some("t_pkey")).0, want);
    let (got, along) = ids(&mut s, q, Some("t_vr"));
    assert_eq!(got, want);
    assert!(along.order.starts_with("t_vr+"), "{along:?}");
    let (got, round) = ids(&mut s, q, None);
    assert_eq!(got, want);
    assert!(round.order.starts_with("t_vr+"), "{round:?}");
    assert_eq!(
        reads_of(&round),
        [("t_vr".to_string(), 3), ("t_pkey".to_string(), 38)]
    );
    // On both: one read, or the other kept to its keys, each row held to both.
    let q = "SELECT id FROM s.t WHERE v @@ 'word60' AND r && '[0,10000)'";
    let want = kept(&|i| word(i, 60) && span(i, 0, 10000));
    assert_eq!(ids(&mut s, q, Some("t_pkey")).0, want);
    assert_eq!(ids(&mut s, q, Some("t_vr")).0, want);
    let (got, round) = ids(&mut s, q, None);
    assert_eq!(got, want);
    assert_eq!(
        reads_of(&round),
        [
            ("t_vr".to_string(), 55),
            ("t_vr".to_string(), 3),
            ("t_pkey".to_string(), 45)
        ]
    );
    let q = "SELECT id FROM s.t WHERE v @@ 'word60' AND r && '[5000,5040)'";
    let (got, round) = ids(&mut s, q, None);
    assert_eq!(got, kept(&|i| word(i, 60) && span(i, 5000, 5040)));
    assert_eq!(
        reads_of(&round),
        [
            ("t_vr".to_string(), 11),
            ("t_vr".to_string(), 3),
            ("t_pkey".to_string(), 0)
        ]
    );
    // The tsvector's condition asks nothing: the table's order holds it, named or not.
    let q = "SELECT id FROM s.t WHERE v @@ '!word60'";
    let want = kept(&|i| i % 13 != 0 && i % 997 != 60);
    assert_eq!(ids(&mut s, q, Some("t_pkey")).0, want);
    let (got, round) = ids(&mut s, q, Some("t_vr"));
    assert_eq!(got, want);
    assert_eq!(round.order, "t_pkey", "{round:?}");
    assert_eq!(ids(&mut s, q, None).0, want);
    // A count on one word closes on its counts; a price names the strand or the order it reads.
    let (c, round) = count(&mut s, "SELECT count(*) FROM s.t WHERE v @@ 'word60'", None);
    assert_eq!(c as usize, kept(&|i| word(i, 60)).len());
    assert_eq!(round.order, "t_vr", "{round:?}");
    let price = s
        .price("SELECT id FROM s.t WHERE v @@ 'word60'", None)
        .unwrap();
    assert!(price.order.starts_with("t_vr+"), "{price:?}");
    let price = s
        .price("SELECT id FROM s.t WHERE r && '[5000,5040)'", None)
        .unwrap();
    assert_eq!(price.order, "t_vr", "{price:?}");
    // A count beside what its order cannot read: no element's counts close it.
    let (c, round) = count(
        &mut s,
        "SELECT count(*) FROM s.t WHERE r && '[5000,5040)' AND n = 3",
        None,
    );
    assert_eq!(
        c as usize,
        kept(&|i| span(i, 5000, 5040) && i % 10 == 3).len()
    );
    assert!(round.order.contains("t_vr"), "{round:?}");
    // Each order and strand holds its rows: the range's non-NULL and NULL rows, a word a row.
    let rows = s.blocks().unwrap();
    assert_eq!(
        strand_rows(&rows, "t_vr", "id"),
        n as u64 + (0..n).filter(|i| i % 13 != 0).count() as u64 * 2
    );
}

#[cfg(feature = "standalone")]
mod beside_writers {
    use super::*;

    #[test]
    fn a_statement_writes_each_key_of_an_index_beside_another_kind() {
        let n = 4096;
        let path = snail_at("gin-beside-writes", BESIDE, n, beside_row);
        let mut s = Snail::open_writing(&path).unwrap();
        let along = |s: &mut Snail, w: &str| {
            let q = format!("SELECT id FROM s.t WHERE {w}");
            let (want, _) = ids(s, &q, Some("t_pkey"));
            assert_eq!(ids(s, &q, Some("t_vr")).0, want, "{q} along t_vr");
            assert_eq!(ids(s, &q, None).0, want, "{q} by its pages");
            s.census().unwrap();
            want
        };
        for st in [
            "INSERT INTO s.t (id, v, r, n) VALUES (9000, 'word7777', '[9000,9003)', 1)",
            "UPDATE s.t SET r = '[9100,9103)' WHERE id = 9000",
            "UPDATE s.t SET v = 'word7778' WHERE id = 9000",
            "UPDATE s.t SET v = 'word7779', r = '[9200,9203)' WHERE id = 100",
            "UPDATE s.t SET id = 9500 WHERE id = 200",
        ] {
            s.execute(st).unwrap_or_else(|e| panic!("{st}: {e}"));
        }
        assert_eq!(along(&mut s, "v @@ 'word7777'"), Vec::<i32>::new());
        assert_eq!(along(&mut s, "v @@ 'word7778'"), [9000]);
        assert_eq!(along(&mut s, "v @@ 'word7779'"), [100]);
        assert_eq!(along(&mut s, "r && '[9100,9101)'"), [9000]);
        assert_eq!(along(&mut s, "r && '[9000,9001)'"), Vec::<i32>::new());
        assert_eq!(along(&mut s, "r && '[9200,9201)'"), [100]);
        assert_eq!(along(&mut s, "r && '[200,201)'"), [198, 199, 9500]);
        s.execute("DELETE FROM s.t WHERE v @@ 'word7778' OR r && '[9200,9201)'")
            .unwrap();
        assert_eq!(along(&mut s, "v @@ 'word7778'"), Vec::<i32>::new());
        assert_eq!(along(&mut s, "r && '[9200,9201)'"), Vec::<i32>::new());
    }

    #[test]
    fn a_statement_finds_its_rows_by_an_or_along_the_strands() {
        let n = 8192;
        let path = snail_at("gin-or-writes", OVER, n, over_row);
        let mut s = Snail::open_writing(&path).unwrap();
        let in_a = |x: i64| (0..n).filter(move |&i| i % 50 != 0 && i % 997 + 100 == x);
        let moved = in_a(150).chain(in_a(151)).count() as u64;
        let d = s
            .execute("UPDATE s.t SET a = '{7777}' WHERE a @> '{150}' OR a @> '{151}'")
            .unwrap();
        assert_eq!(d.rows, moved);
        let (got, round) = ids(&mut s, "SELECT id FROM s.t WHERE a @> '{7777}'", None);
        assert_eq!(got.len() as u64, moved);
        assert!(round.order.starts_with("t_a"), "{round:?}");
        let gone = in_a(152).count() as u64;
        let d = s
            .execute("DELETE FROM s.t WHERE a @> '{152}' OR v @@ 'nothing'")
            .unwrap();
        assert_eq!(d.rows, gone);
        let (got, _) = ids(
            &mut s,
            "SELECT id FROM s.t WHERE a @> '{152}'",
            Some("t_pkey"),
        );
        assert!(got.is_empty());
        // A NOT beside the strand's own, in a transaction that sees its own rows.
        s.execute("BEGIN ISOLATION LEVEL REPEATABLE READ").unwrap();
        s.execute("INSERT INTO s.t (id, a, v, n) VALUES (9000, '{7777,3}', 'w3', 1), (9001, '{7777}', 'w4', 1)")
            .unwrap();
        let (got, _) = ids(
            &mut s,
            "SELECT id FROM s.t WHERE a @> '{7777}' AND NOT a @> '{3}' AND id >= 9000",
            None,
        );
        assert_eq!(got, [9001]);
        let (got, _) = ids(
            &mut s,
            "SELECT id FROM s.t WHERE (a @> '{7777}' OR v @@ 'w4') AND id >= 9000",
            None,
        );
        assert_eq!(got, [9000, 9001]);
        s.execute("COMMIT").unwrap();
        s.census().unwrap();
        // `@@` and a prefix `@` written apart are refused in a statement too, `@@@` not.
        assert!(s.execute("DELETE FROM s.t WHERE v @@ @ 'w4'").is_err());
        // Writers beside `@@@`.
        let d = s
            .execute("DELETE FROM s.t WHERE v @@@ 'w4' AND id >= 9000")
            .unwrap();
        assert_eq!(d.rows, 1);
    }
}

const ARMS: &str = "CREATE TABLE s.t (id int NOT NULL, a int[], v tsvector, r int4range, m int, pad bigint, PRIMARY KEY (id)); \
                    CREATE INDEX t_a ON s.t USING gin (a); \
                    CREATE INDEX t_vr ON s.t USING gist (v, r); \
                    CREATE INDEX t_m ON s.t (m);";

fn arms_row(i: i64) -> Vec<Option<String>> {
    vec![
        Some(i.to_string()),
        Some(format!("{{{},{}}}", i % 7, 2000 + i / 256)),
        Some(format!("'w{}':1 'z{}':2", i % 7, i / 256)),
        Some(format!("[{i},{})", i + 3)),
        Some((i % 997).to_string()),
        Some(i.to_string()),
    ]
}

/// The rows of an element and of a condition an order answers: `2000 + i / 256` lying together, `id`, `m`
/// of 997 and `r` each the table's own order or a strand's.
#[test]
fn an_or_reads_an_arm_an_order_answers_as_postgresql_s_bitmap_or_would() {
    let n = 20480;
    let mut s = snail_of("gin-arms", ARMS, n, arms_row);
    let kept = |f: &dyn Fn(i64) -> bool| -> Vec<i32> {
        (0..n).filter(|&i| f(i)).map(|i| i as i32).collect()
    };
    let z = |i: i64, k: i64| i / 256 == k;
    let r = |i: i64, lo: i64, hi: i64| i < hi && i + 3 > lo;
    let cases: Vec<(&str, Vec<i32>, Vec<(&str, u64)>)> = vec![
        (
            "a @> '{2005}' OR id < 50",
            kept(&|i| z(i, 5) || i < 50),
            vec![("t_pkey", 1), ("t_a", 2), ("t_pkey", 9)],
        ),
        (
            "a @> '{2005}' OR m = 5",
            kept(&|i| z(i, 5) || i % 997 == 5),
            vec![("t_m", 2), ("t_a", 2), ("t_pkey", 124)],
        ),
        (
            "a @> '{2005}' OR r && '[15000,15040)'",
            kept(&|i| z(i, 5) || r(i, 15000, 15040)),
            vec![("t_vr", 8), ("t_a", 2), ("t_pkey", 16)],
        ),
        (
            "v @@ 'z5' OR r && '[15000,15040)'",
            kept(&|i| z(i, 5) || r(i, 15000, 15040)),
            vec![("t_vr", 8), ("t_vr", 3), ("t_pkey", 12)],
        ),
        (
            "v @@ 'z5' AND r && '[1500,1510)'",
            kept(&|i| z(i, 5) && r(i, 1500, 1510)),
            vec![("t_vr", 8), ("t_vr", 3), ("t_pkey", 6)],
        ),
        (
            "(a @> '{2005}' OR v @@ 'z70') AND r && '[1500,1510)'",
            kept(&|i| (z(i, 5) || z(i, 70)) && r(i, 1500, 1510)),
            vec![("t_vr", 8), ("t_a", 2), ("t_vr", 3), ("t_pkey", 10)],
        ),
    ];
    for (w, want, reads) in &cases {
        let q = format!("SELECT id FROM s.t WHERE {w}");
        assert_eq!(
            &ids(&mut s, &q, Some("t_pkey")).0,
            want,
            "{q} along the table's order"
        );
        let (got, round) = ids(&mut s, &q, None);
        assert_eq!(&got, want, "{q} by its pages");
        let reads: Vec<(String, u64)> = reads.iter().map(|(o, n)| (o.to_string(), *n)).collect();
        assert_eq!(reads_of(&round), reads, "{q}");
    }
    // Named: the index with both keys reads them both; one of another column is no reader of this.
    let q = "SELECT id FROM s.t WHERE v @@ 'z5' OR r && '[15000,15040)'";
    let (got, round) = ids(&mut s, q, Some("t_vr"));
    assert_eq!(got, kept(&|i| z(i, 5) || r(i, 15000, 15040)));
    assert!(round.order.starts_with("t_vr+"), "{round:?}");
    assert!(s.ask(q, Some("t_a")).is_err());
    // Named the strand of one arm, an arm of an order's asks what is not it: the table's order.
    let q = "SELECT id FROM s.t WHERE a @> '{2005}' OR id < 50";
    let (got, round) = ids(&mut s, q, Some("t_a"));
    assert_eq!(got, kept(&|i| z(i, 5) || i < 50));
    assert_eq!(round.order, "t_pkey", "{round:?}");
    // An arm no order narrows asks nothing: the table's order is read, each row held to the OR.
    let q = "SELECT id FROM s.t WHERE a @> '{2005}' OR m > 5";
    let (got, round) = ids(&mut s, q, None);
    assert_eq!(got, kept(&|i| z(i, 5) || i % 997 > 5));
    assert_eq!(round.order, "t_pkey", "{round:?}");
    // No row can pass the arm: the other's rows alone.
    let q = "SELECT id FROM s.t WHERE a @> '{2005}' OR m = NULL";
    let (got, round) = ids(&mut s, q, None);
    assert_eq!(got, kept(&|i| z(i, 5)));
    assert_eq!(
        reads_of(&round),
        [("t_a".to_string(), 2), ("t_pkey".to_string(), 6)],
        "{q}"
    );
    let price = s
        .price("SELECT id FROM s.t WHERE a @> '{2005}' OR id < 50", None)
        .unwrap();
    assert_eq!(price.order, "t_a+t_pkey", "{price:?}");
}

#[test]
fn an_or_is_held_to_each_row_of_a_block_its_first_column_holds_nothing_of() {
    let n = 8192;
    // A block's worth and more with no array: the OR's first column all NULL there.
    let mut s = snail_of("gin-or-nulls", OVER, n, |i| {
        let mut r = over_row(i);
        if i < 4096 {
            r[1] = None;
        }
        r
    });
    let q = "SELECT id FROM s.t WHERE a @> '{150}' OR n = 3";
    let want: Vec<i32> = (0..n)
        .filter(|&i| {
            (i >= 4096 && i % 50 != 0 && i % 997 + 100 == 150) || (i % 11 != 0 && i % 10 == 3)
        })
        .map(|i| i as i32)
        .collect();
    let (got, _) = ids(&mut s, q, None);
    assert_eq!(got, want);
    let (got, _) = ids(&mut s, q, Some("t_pkey"));
    assert_eq!(got, want);
    // Named a strand on a column only beside the first, what it cannot answer reads the table's order.
    let q = "SELECT id FROM s.t WHERE a IS NULL OR v @@ '!word60'";
    let (got, round) = ids(&mut s, q, Some("t_v"));
    assert_eq!(round.order, "t_pkey", "{round:?}");
    let (want, _) = ids(&mut s, q, Some("t_pkey"));
    assert_eq!(got, want);
    assert_eq!(
        want.len(),
        (0..n)
            .filter(|&i| i < 4096 || i % 50 == 0 || rare_v(i, 60) == Some(false))
            .count()
    );
}

#[test]
fn an_or_on_a_table_an_outer_join_leaves_null_keeps_the_rows_it_leaves() {
    let ddl = format!("{OVER} CREATE TABLE s.u (id int NOT NULL, x bigint, PRIMARY KEY (id));");
    let catalog = Catalog::parse(&ddl).unwrap();
    let n = 2048i64;
    let t = catalog.table("s.t").unwrap();
    // `s.t` holds the even ids of `s.u`, so a LEFT JOIN from `s.u` leaves the odd ones NULL.
    let mut columns: Vec<Values> = t.columns.iter().map(|c| Values::new(c.ty)).collect();
    for i in (0..n).step_by(2) {
        for ((v, c), x) in columns.iter_mut().zip(&t.columns).zip(&over_row(i)) {
            match (v, c.ty, x) {
                (Values::Int(a), _, x) => a.push(x.as_ref().map(|s| s.parse().unwrap())),
                (Values::Composed(a), ty, x) => a.push(composed(ty, x.as_deref())),
                (v, ColumnType::Textual(t), Some(x)) => push_datum(
                    v,
                    &Datum::Text(warren_snail::textual::text_in(t, x).unwrap()),
                ),
                (v, _, _) => v.push_null(),
            }
        }
    }
    let path = tmp("gin-or-outer.snail");
    write(
        &path,
        &catalog,
        &[
            ("s.t".into(), Rows { columns }),
            (
                "s.u".into(),
                Rows {
                    columns: vec![
                        Values::Int((0..n).map(Some).collect()),
                        Values::Int((0..n).map(Some).collect()),
                    ],
                },
            ),
        ],
    )
    .unwrap();
    let mut s = fresh(&path);
    let q = "SELECT u.id FROM s.u u LEFT JOIN s.t x ON u.id = x.id \
             WHERE x.a @> '{150}' OR x.id IS NULL";
    // Refused, or the rows PostgreSQL gives: never those of an inner join.
    if let Ok((b, _)) = s.ask(q, None) {
        let want = (0..n)
            .filter(|&i| i % 2 == 1 || (i % 50 != 0 && i % 997 + 100 == 150))
            .count();
        assert_eq!(b.num_rows(), want);
    }
}

#[test]
fn a_condition_on_the_table_s_own_order_is_read_with_it_not_before_it() {
    let ddl = "CREATE TABLE s.t (id int NOT NULL, a int[], r int4range, pad bigint); \
               CREATE INDEX t_r ON s.t USING gist (r); \
               ALTER TABLE s.t CLUSTER ON t_r; \
               CREATE INDEX t_a ON s.t USING gin (a);";
    let n = 8192;
    let mut s = snail_of("gin-on-order", ddl, n, |i| {
        vec![
            Some(i.to_string()),
            Some(format!("{{{},{}}}", i % 7, 2000 + i / 256)),
            Some(format!("[{i},{})", i + 3)),
            Some(i.to_string()),
        ]
    });
    let q = "SELECT id FROM s.t WHERE a @> '{2005}' AND r && '[1200,1500)'";
    let (got, round) = ids(&mut s, q, None);
    let want: Vec<i32> = (0..n)
        .filter(|&i| i / 256 == 5 && i < 1500 && i + 3 > 1200)
        .map(|i| i as i32)
        .collect();
    assert_eq!(got.len(), want.len());
    assert!(
        round.reads.iter().filter(|r| r.order == "t_r").count() <= 1,
        "{round:?}"
    );
}

#[test]
fn an_or_reads_a_partial_strand_where_what_stands_beside_its_arm_implies_its_predicate() {
    let ddl = "CREATE TABLE s.t (id int NOT NULL, a int[], p int[], n int, PRIMARY KEY (id)); \
               CREATE INDEX t_a ON s.t USING gin (a); \
               CREATE INDEX t_p ON s.t USING gin (p) WHERE n > 5;";
    let n = 20480;
    let mut s = snail_of("gin-or-partial", ddl, n, |i| {
        let z = 2000 + i / 256;
        vec![
            Some(i.to_string()),
            Some(format!("{{{z}}}")),
            Some(format!("{{{z}}}")),
            Some((i % 10).to_string()),
        ]
    });
    let z = |i: i64, k: i64| i / 256 == k;
    let q = "SELECT id FROM s.t WHERE (p @> '{2005}' AND n > 6) OR a @> '{2070}'";
    let (got, round) = ids(&mut s, q, None);
    let want: Vec<i32> = (0..n)
        .filter(|&i| (z(i, 5) && i % 10 > 6) || z(i, 70))
        .map(|i| i as i32)
        .collect();
    assert_eq!(got, want);
    assert!(
        round.order.contains("t_p") && round.order.contains("t_a"),
        "{round:?}"
    );
    // Beside nothing that implies it, the strand is not asked, and every row may hold the arm.
    let q = "SELECT id FROM s.t WHERE p @> '{2005}' OR a @> '{2070}'";
    let (got, round) = ids(&mut s, q, None);
    let want: Vec<i32> = (0..n)
        .filter(|&i| z(i, 5) || z(i, 70))
        .map(|i| i as i32)
        .collect();
    assert_eq!(got, want);
    assert_eq!(round.order, "t_pkey", "{round:?}");
    let q = "SELECT id FROM s.t WHERE (p @> '{2005}' AND n > 4) OR a @> '{2070}'";
    let (got, round) = ids(&mut s, q, None);
    let want: Vec<i32> = (0..n)
        .filter(|&i| (z(i, 5) && i % 10 > 4) || z(i, 70))
        .map(|i| i as i32)
        .collect();
    assert_eq!(got, want);
    assert_eq!(round.order, "t_pkey", "{round:?}");
}

#[test]
fn an_or_and_a_not_over_a_strand_s_spans_are_read_with_no_element_key_on_the_table() {
    let ddl = "CREATE TABLE s.t (id int NOT NULL, r int4range, n int, PRIMARY KEY (id)); \
               CREATE INDEX t_r ON s.t USING gist (r);";
    let n = 8192;
    let mut s = snail_of("gin-spans", ddl, n, |i| {
        vec![
            Some(i.to_string()),
            (i % 13 != 0).then(|| format!("[{i},{})", i + 3)),
            Some((i % 10).to_string()),
        ]
    });
    let r = |i: i64, lo: i64, hi: i64| i % 13 != 0 && i < hi && i + 3 > lo;
    let kept = |f: &dyn Fn(i64) -> bool| -> Vec<i32> {
        (0..n).filter(|&i| f(i)).map(|i| i as i32).collect()
    };
    let (got, _) = ids(
        &mut s,
        "SELECT id FROM s.t WHERE r && '[100,110)' OR r && '[5000,5010)'",
        None,
    );
    assert_eq!(got, kept(&|i| r(i, 100, 110) || r(i, 5000, 5010)));
    let (got, _) = ids(
        &mut s,
        "SELECT id FROM s.t WHERE r && '[100,110)' OR id = 5",
        None,
    );
    assert_eq!(got, kept(&|i| r(i, 100, 110) || i == 5));
    let (got, _) = ids(
        &mut s,
        "SELECT id FROM s.t WHERE NOT r && '[100,8000)' AND n = 3",
        None,
    );
    assert_eq!(
        got,
        kept(&|i| i % 13 != 0 && !r(i, 100, 8000) && i % 10 == 3)
    );
}
