//! The functions of json and jsonb in a pipeline: the builders over each kind of value, the
//! functions of documents and of paths, their sets and aggregates, each answer PostgreSQL 19's.
#![cfg(feature = "standalone")]

mod weather;

use arrow::array::{Array, AsArray};
use warren_snail::catalog::Catalog;
use warren_snail::read::Snail;
use warren_snail::write::Rows;
use weather::{tmp, write};

/// A snail of one table, `s.t`, made empty from its DDL, then the statements run on it.
fn snail_of(name: &str, ddl: &str, statements: &[&str]) -> Snail {
    let path = tmp(&format!("json-{name}.snail"));
    let catalog = Catalog::parse(ddl).unwrap();
    let empty = Rows {
        columns: catalog.tables[0]
            .columns
            .iter()
            .map(|c| warren_snail::values::Values::new(c.ty))
            .collect(),
    };
    write(&path, &catalog, &[("s.t".into(), empty)]).unwrap();
    let mut snail = Snail::open_writing(&path).unwrap();
    for s in statements {
        snail.execute(s).unwrap();
    }
    snail
}

/// The answer to a select list over no table row, a value of text a column.
fn ask(snail: &mut Snail, select: &str) -> warren_snail::Result<Vec<Option<String>>> {
    let q = format!("WITH t AS (SELECT id FROM s.t WHERE id = 1) SELECT {select} FROM t");
    let (b, _) = snail.ask(&q, None)?;
    Ok((0..b.num_columns())
        .map(|c| {
            let a = b.column(c).as_string::<i32>();
            (!a.is_null(0)).then(|| a.value(0).to_string())
        })
        .collect())
}

fn one_row(name: &str) -> Snail {
    snail_of(
        name,
        "CREATE TABLE s.t (id integer PRIMARY KEY, x integer, k text);",
        &["INSERT INTO s.t VALUES (1, 1, 'a'), (2, NULL, 'b'), (3, 3, 'a')"],
    )
}

fn some(xs: &[&str]) -> Vec<Option<String>> {
    xs.iter()
        .map(|s| (!s.is_empty()).then(|| s.to_string()))
        .collect()
}

#[test]
fn the_builders_write_each_kind_of_value_as_postgresql_does() {
    let mut s = one_row("builders");
    assert_eq!(
        ask(
            &mut s,
            "to_json('2024-02-29 12:34:56.789'::timestamp)::text, \
             to_jsonb('2024-02-29 12:34:56+00'::timestamptz)::text, \
             json_build_object('a', 1.50, 'b', 'x', 'c', NULL, 'd', true, 'e', '{1,NULL}'::int[])::text"
        )
        .unwrap(),
        some(&[
            "\"2024-02-29T12:34:56.789\"",
            "\"2024-02-29T12:34:56+00:00\"",
            "{\"a\" : 1.50, \"b\" : \"x\", \"c\" : null, \"d\" : true, \"e\" : [1,null]}",
        ])
    );
}

#[test]
fn the_functions_of_documents_take_their_defaults_and_their_nulls() {
    let mut s = one_row("documents");
    assert_eq!(
        ask(
            &mut s,
            "jsonb_set('{\"a\":1}', '{b}', '2')::text, jsonb_set('{\"a\":1}', '{b}', '2', false)::text, \
             jsonb_set_lax('{\"a\":1}', '{a}', NULL)::text, jsonb_set(NULL, '{b}', '2')::text"
        )
        .unwrap(),
        some(&["{\"a\": 1, \"b\": 2}", "{\"a\": 1}", "{\"a\": null}", ""])
    );
}

#[test]
fn a_path_is_run_with_its_variables_and_its_operators() {
    let mut s = one_row("paths");
    assert_eq!(
        ask(
            &mut s,
            "jsonb_path_query_array('{\"a\":[1,2,3]}', '$.a[*] ? (@ > $v)', '{\"v\":1}')::text, \
             jsonb_path_exists('[1]', 'strict $.a', '{}', true)::text, \
             ('{\"a\":1}'::jsonb @? '$.a ? (@ > 0)')::text, ('[1]'::jsonb @@ '$[0] == 1')::text"
        )
        .unwrap(),
        some(&["[2, 3]", "", "true", "true"])
    );
}

#[test]
fn the_aggregates_and_sets_of_json() {
    let mut s = one_row("aggregates");
    let (b, _) = s
        .ask(
            "WITH t AS (SELECT id, x, k FROM s.t) SELECT json_agg(x ORDER BY id)::text AS a, \
             jsonb_object_agg(k, x ORDER BY id)::text AS b FROM t",
            None,
        )
        .unwrap();
    let col = |c: usize| b.column(c).as_string::<i32>().value(0).to_string();
    assert_eq!(
        (col(0), col(1)),
        ("[1, null, 3]".into(), "{\"a\": 3, \"b\": null}".into())
    );
    let (b, _) = s
        .ask(
            "WITH t AS (SELECT id FROM s.t) SELECT id, (jsonb_each(CASE WHEN id = 1 THEN \
             '{\"a\": 1, \"b\": [2]}'::jsonb ELSE '{}'::jsonb END))::text AS e FROM t ORDER BY id",
            None,
        )
        .unwrap();
    // An empty object makes no row.
    let e = b.column(1).as_string::<i32>();
    assert_eq!(
        (0..e.len()).map(|r| e.value(r)).collect::<Vec<_>>(),
        ["(a,1)", "(b,[2])"]
    );
}

#[test]
fn like_regex_runs_on_postgresql_s_engine_under_its_flags() {
    let mut s = one_row("like-regex");
    assert_eq!(
        ask(
            &mut s,
            "jsonb_path_exists('\"a\\nb\"', '$ ? (@ like_regex \"a.b\")')::text, \
             jsonb_path_exists('\"a\\nb\"', '$ ? (@ like_regex \"a.b\" flag \"s\")')::text, \
             jsonb_path_query_array('[\"abc\", \"Abd\", 1, \"x\"]', \
             '$[*] ? (@ like_regex \"^a\" flag \"i\")')::text"
        )
        .unwrap(),
        some(&["false", "true", "[\"abc\", \"Abd\"]"])
    );
}
