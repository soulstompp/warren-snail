//! The functions of text search in a pipeline: those of tsvector and tsquery, and those reading
//! the configuration a call names, each answer PostgreSQL 19's.
#![cfg(feature = "standalone")]

mod weather;

use arrow::array::{Array, AsArray};
use warren_snail::catalog::Catalog;
use warren_snail::read::Snail;
use warren_snail::write::Rows;
use weather::{tmp, write};

/// A snail of one table, `s.t`, made empty from its DDL, then the statements run on it.
fn snail_of(name: &str, ddl: &str, statements: &[&str]) -> Snail {
    let path = tmp(&format!("textsearch-{name}.snail"));
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
fn a_text_is_searched_under_the_configuration_its_call_names() {
    let mut s = one_row("configured");
    assert_eq!(
        ask(
            &mut s,
            "to_tsvector('english', 'The Fat Rats')::text, \
             plainto_tsquery('english', 'fat rats')::text, \
             ts_rank('a:1 b:2'::tsvector, 'a'::tsquery)::text, \
             setweight('a:1 b:2'::tsvector, 'A')::text, length('a b c'::tsvector)::text"
        )
        .unwrap(),
        some(&[
            "'fat':2 'rat':3",
            "'fat' & 'rat'",
            "0.06079271",
            "'a':1A 'b':2A",
            "3"
        ])
    );
    // A call naming no configuration reads the file's, here the default.
    assert_eq!(
        ask(&mut s, "to_tsvector('fat cats')::text, get_current_ts_config()::text").unwrap(),
        some(&["'cats':2 'fat':1", "simple"])
    );
}

#[test]
fn the_sets_of_text_search_make_a_row_an_item() {
    let mut s = one_row("sets");
    for (set, want) in [
        ("unnest('a:1A b'::tsvector)", vec!["(a,{1},{A})", "(b,,)"]),
        (
            "ts_parse('default', 'foo 42')",
            vec!["(1,foo)", "(12,\" \")", "(22,42)"],
        ),
    ] {
        let (b, _) = s
            .ask(
                &format!(
                    "WITH t AS (SELECT id FROM s.t WHERE id = 1) SELECT ({set})::text AS x FROM t"
                ),
                None,
            )
            .unwrap();
        let a = b.column(0).as_string::<i32>();
        assert_eq!(
            (0..a.len()).map(|r| a.value(r)).collect::<Vec<_>>(),
            want,
            "{set}"
        );
    }
}
