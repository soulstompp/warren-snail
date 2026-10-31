//! Arrays in a pipeline: ARRAY[...] and the casts of arrays, the set-returning functions of a
//! step's select list, array_agg and the functions of arrays, each answer PostgreSQL 19's.
#![cfg(feature = "standalone")]

mod weather;

use arrow::array::{Array, AsArray, RecordBatch};
use warren_snail::catalog::Catalog;
use warren_snail::read::Snail;
use warren_snail::write::Rows;
use weather::{tmp, write};

/// A snail of one table, `s.t`, made empty from its DDL, then the statements run on it.
fn snail_of(name: &str, ddl: &str, statements: &[&str]) -> Snail {
    let path = tmp(&format!("arrays-{name}.snail"));
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

fn arrays(name: &str) -> Snail {
    snail_of(
        name,
        "CREATE TABLE s.t (id integer PRIMARY KEY, a integer[], t text[], n numeric);",
        &[
            "INSERT INTO s.t VALUES (1, '{1,2,3}', '{a,b}', 1.5), (2, '{}', '{x,NULL}', NULL), \
           (3, NULL, '{y}', 2), (4, '[2:3]={7,8}', NULL, 3)",
        ],
    )
}

/// An answer's columns of text, a row at a time.
fn rows(b: &RecordBatch) -> Vec<Vec<Option<String>>> {
    let cols: Vec<_> = (0..b.num_columns())
        .map(|c| {
            let a = arrow::compute::cast(b.column(c), &arrow::datatypes::DataType::Utf8).unwrap();
            let a = a.as_string::<i32>().clone();
            (0..a.len())
                .map(|r| (!a.is_null(r)).then(|| a.value(r).to_string()))
                .collect::<Vec<_>>()
        })
        .collect();
    (0..b.num_rows())
        .map(|r| cols.iter().map(|c| c[r].clone()).collect())
        .collect()
}

/// Each row's answer to the select list, in id's order (a column of its own, left out), written as
/// PostgreSQL's psql writes them unaligned: `|` between the values, a NULL empty.
fn ask(snail: &mut Snail, select: &str) -> warren_snail::Result<Vec<String>> {
    ask_where(snail, select, "true")
}

/// The same, of the rows a condition admits.
fn ask_where(snail: &mut Snail, select: &str, cond: &str) -> warren_snail::Result<Vec<String>> {
    let q = format!(
        "WITH t AS (SELECT id, a, t, n FROM s.t) \
         SELECT id AS o, {select} FROM t WHERE {cond} ORDER BY o"
    );
    let (b, _) = snail.ask(&q, None)?;
    Ok(rows(&b)
        .into_iter()
        .map(|r| {
            r.into_iter()
                .skip(1)
                .map(Option::unwrap_or_default)
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect())
}

#[test]
fn an_array_constructor_takes_its_elements_common_type() {
    let mut s = arrays("constructor");
    assert_eq!(
        ask(
            &mut s,
            "(ARRAY[id, 2, 3])::text, (ARRAY[n, id])::text, (ARRAY['a', t[1]])::text, \
             ARRAY[[1,2],[3,4]]::text, (ARRAY[a, a])::text"
        )
        .unwrap(),
        [
            "{1,2,3}|{1.5,1}|{a,a}|{{1,2},{3,4}}|{{1,2,3},{1,2,3}}",
            "{2,2,3}|{NULL,2}|{a,x}|{{1,2},{3,4}}|{}",
            "{3,2,3}|{2,3}|{a,y}|{{1,2},{3,4}}|{}",
            "{4,2,3}|{3,4}|{a,NULL}|{{1,2},{3,4}}|[1:2][2:3]={{7,8},{7,8}}",
        ]
    );
    // A cast hands its type to the elements; an array of one type cast to another, element by
    // element.
    assert_eq!(
        ask_where(
            &mut s,
            "(ARRAY['1','2']::int[])::text, (ARRAY[]::int[])::text, (ARRAY[1.5, 2]::int[])::text, \
             (ARRAY[a]::text[])::text, ('{1.5,2.5}'::numeric[]::int[])::text, \
             (ARRAY[NULL::int[], NULL])::text, (ARRAY[id, 1.5])::text",
            "id = 1"
        )
        .unwrap(),
        ["{1,2}|{}|{2,2}|{{1,2,3}}|{2,3}|{}|{1,1.5}"]
    );
    assert_eq!(
        ask_where(&mut s, "(a::text[])::text", "id = 4").unwrap(),
        ["[2:3]={7,8}"]
    );
    for (select, why) in [
        ("(ARRAY[ARRAY[1,2], NULL])::text", "matching dimensions"),
        (
            "(ARRAY[id, 'x'])::text",
            "invalid input syntax for type integer",
        ),
    ] {
        let e = ask(&mut s, select).unwrap_err().to_string();
        assert!(e.contains(why), "{select}: {e}");
    }
}

#[test]
fn a_set_returning_call_makes_as_many_rows_as_its_longest_set() {
    let mut s = arrays("sets");
    assert_eq!(
        ask(&mut s, "id, unnest(a), unnest(t)").unwrap(),
        ["1|1|a", "1|2|b", "1|3|", "2||x", "2||", "3||y", "4|7|", "4|8|"]
    );
    assert_eq!(
        ask_where(
            &mut s,
            "unnest('{4,5}'::int[]) + 1, unnest(ARRAY[1,2,3])",
            "id = 1"
        )
        .unwrap(),
        ["5|1", "6|2", "|3"]
    );
    for (select, why) in [
        ("CASE WHEN id > 1 THEN unnest(a) END", "not allowed in CASE"),
        ("coalesce(unnest(a), 0)", "not allowed in COALESCE"),
        ("sum(unnest(a))", "aggregate function calls cannot contain"),
        ("unnest(unnest(ARRAY[a]))", "among another's arguments"),
    ] {
        let e = ask(&mut s, select).unwrap_err().to_string();
        assert!(e.contains(why), "{select}: {e}");
    }
    let e = ask_where(&mut s, "id", "unnest(a) > 1")
        .unwrap_err()
        .to_string();
    assert!(e.contains("not allowed in WHERE"), "{e}");
    // Over groups, the set is of the group's aggregate.
    let (b, _) = s
        .ask(
            "WITH t AS (SELECT id, a, t, n FROM s.t) \
             SELECT id, unnest(array_agg(n))::text AS x FROM t GROUP BY id ORDER BY id",
            None,
        )
        .unwrap();
    assert_eq!(
        rows(&b)
            .into_iter()
            .map(|r| r[1].clone())
            .collect::<Vec<_>>(),
        [Some("1.5".into()), None, Some("2".into()), Some("3".into())]
    );
}

#[test]
fn array_agg_keeps_its_nulls_and_the_percentiles_take_arrays() {
    let mut s = arrays("aggregates");
    let (b, _) = s
        .ask(
            "WITH t AS (SELECT id, a, t, n FROM s.t) \
             SELECT (array_agg(id ORDER BY id DESC))::text AS x, (array_agg(n))::text AS y, \
             (array_agg(DISTINCT n))::text AS z, \
             (percentile_cont(ARRAY[0.25, 0.5, NULL]) WITHIN GROUP (ORDER BY id))::text AS p, \
             (percentile_disc('{{0.1},{0.9}}'::float8[]) WITHIN GROUP (ORDER BY id))::text AS q \
             FROM t",
            None,
        )
        .unwrap();
    assert_eq!(
        rows(&b)[0],
        [
            "{4,3,2,1}",
            "{1.5,NULL,2,3}",
            "{1.5,2,3,NULL}",
            "{1.75,2.5,NULL}",
            "{{1},{4}}"
        ]
        .map(|s| Some(s.to_string()))
    );
}

#[test]
fn the_functions_of_arrays_take_their_arguments_types_and_defaults() {
    let mut s = arrays("functions");
    assert_eq!(
        ask_where(
            &mut s,
            "array_fill(NULL::int, ARRAY[2])::text, array_sort(ARRAY[3,NULL,1], true)::text, \
             string_to_array('a,b,,c', ',', '')::text, ('{1,2}'::int[] || NULL::int)::text, \
             (ARRAY[('{1.5}'::numeric[])[1], 2])::text, quote_nullable(NULL::text)",
            "id = 1"
        )
        .unwrap(),
        ["{NULL,NULL}|{NULL,3,1}|{a,b,NULL,c}|{1,2,NULL}|{1.5,2}|NULL"]
    );
}

#[test]
fn the_regular_expressions_sets_make_a_row_a_match() {
    let mut s = arrays("regexps");
    assert_eq!(
        ask_where(
            &mut s,
            "(regexp_matches('a1b22', '([a-z])([0-9]+)', 'g'))::text",
            "id = 1"
        )
        .unwrap(),
        ["{a,1}", "{b,22}"]
    );
    assert_eq!(
        ask_where(
            &mut s,
            "regexp_split_to_table('one  two', '\\s+')",
            "id = 1"
        )
        .unwrap(),
        ["one", "two"]
    );
}
