//! The functions of ranges and multiranges in a pipeline: their constructors, bounds and tests,
//! the sets and aggregates of ranges, each answer PostgreSQL 19's.
#![cfg(feature = "standalone")]

mod weather;

use arrow::array::{Array, AsArray, RecordBatch};
use warren_snail::catalog::Catalog;
use warren_snail::read::Snail;
use warren_snail::write::Rows;
use weather::{tmp, write};

/// A snail of one table, `s.t`, made empty from its DDL, then the statements run on it.
fn snail_of(name: &str, ddl: &str, statements: &[&str]) -> Snail {
    let path = tmp(&format!("ranges-{name}.snail"));
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

fn ranges(name: &str) -> Snail {
    snail_of(
        name,
        "CREATE SCHEMA s; CREATE TABLE s.t (id integer PRIMARY KEY, r int4range, m int4multirange, n numrange, \
         d daterange, ts tsrange, a integer, b integer);",
        &["INSERT INTO s.t VALUES \
           (1, '[1,5)', '{[1,3),[5,8)}', '[1.5,2.5]', '[2024-01-01,2024-02-01)', \
            '[2024-01-01 00:00,2024-01-02 12:00)', 1, 4), \
           (2, 'empty', '{}', 'empty', 'empty', 'empty', 3, 3), \
           (3, NULL, NULL, NULL, NULL, NULL, NULL, 2), \
           (4, '(,10)', '{(,0),[3,4),[9,)}', '(,)', '[2024-01-01,)', '(,2024-01-01)', 5, 1), \
           (5, '[3,4)', '{[2,10)}', '(0,1)', '[2023-12-31,2024-01-01]', \
            '[2024-01-01,2024-01-01]', -1, 7)"],
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
        "WITH t AS (SELECT id, r, m, n, d, ts, a, b FROM s.t) \
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
fn a_range_s_bounds_and_tests_answer_as_postgresql_s() {
    let mut s = ranges("bounds");
    assert_eq!(
        ask(
            &mut s,
            "id, lower(r)::text, upper(m)::text, isempty(r)::text, lower_inc(m)::text, \
             upper_inf(r)::text, lower(n)::text, upper(d)::text, lower(ts)::text"
        )
        .unwrap(),
        [
            "1|1|8|false|true|false|1.5|2024-02-01|2024-01-01 00:00:00",
            "2|||true|false|false|||",
            "3||||||||",
            "4|||false|false|false|||",
            "5|3|10|false|true|false|0|2024-01-02|2024-01-01 00:00:00",
        ]
    );
}

#[test]
fn a_range_is_made_as_its_constructor_makes_it() {
    let mut s = ranges("constructors");
    assert_eq!(
        ask_where(
            &mut s,
            "id, int4range(a, b)::text, int4range(a, b, '[]')::text, numrange(a, b, '()')::text, \
             multirange(r)::text, range_merge(m)::text",
            "id < 4"
        )
        .unwrap(),
        [
            "1|[1,4)|[1,5)|(1,4)|{[1,5)}|[1,8)",
            "2|empty|[3,4)|empty|{}|empty",
            "3|(,2)|(,3)|(,2)||",
        ]
    );
    let e = ask(&mut s, "int4range(a, b)::text")
        .unwrap_err()
        .to_string();
    assert!(e.contains("lower bound must be less than or equal"), "{e}");
    let e = ask(&mut s, "int4range(1, 2, 'x')::text")
        .unwrap_err()
        .to_string();
    assert!(e.contains("invalid range bound flags"), "{e}");
    // A multirange of one NULL range is NULL; of a NULL among others, refused.
    assert_eq!(
        ask(
            &mut s,
            "int4multirange()::text, int4multirange(NULL::int4range)::text"
        )
        .unwrap()[0],
        "{}|"
    );
    let e = ask(&mut s, "int4multirange(r, int4range(a, a + 1))::text")
        .unwrap_err()
        .to_string();
    assert!(e.contains("cannot contain null members"), "{e}");
}

#[test]
fn the_subtype_s_difference_and_canonical_form() {
    let mut s = ranges("subdiff");
    assert_eq!(
        ask(
            &mut s,
            "int4range_subdiff(a, b)::text, daterange_subdiff(upper(d), lower(d))::text, \
             tsrange_subdiff(upper(ts), lower(ts))::text, \
             numrange_subdiff(upper(n), lower(n))::text, int4range_canonical(r)::text"
        )
        .unwrap(),
        [
            "-3|31|129600|1|[1,5)",
            "0||||empty",
            "||||",
            "4||||(,10)",
            "-8|2|0|1|[3,4)",
        ]
    );
}

#[test]
fn the_sets_of_ranges_make_a_row_a_range() {
    let mut s = ranges("sets");
    assert_eq!(
        ask(&mut s, "id, unnest(m)::text").unwrap(),
        ["1|[1,3)", "1|[5,8)", "4|(,0)", "4|[3,4)", "4|[9,)", "5|[2,10)"]
    );
    // A split leaves two ranges, the multirange beside it NULL on the second row; an empty one none.
    assert_eq!(
        ask(
            &mut s,
            "id, range_minus_multi(r, int4range(2, 3))::text, \
             multirange_minus_multi(m, '{[0,2)}')::text"
        )
        .unwrap(),
        [
            "1|[1,2)|{[2,3),[5,8)}",
            "1|[3,5)|",
            "4|(,2)|{(,0),[3,4),[9,)}",
            "4|[3,10)|",
            "5|[3,4)|{[2,10)}",
        ]
    );
}

#[test]
fn the_aggregates_of_ranges_union_and_intersect() {
    let mut s = ranges("aggregates");
    let (b, _) = s
        .ask(
            "WITH t AS (SELECT id, r, m FROM s.t) SELECT range_agg(r)::text AS a, \
             range_agg(m)::text AS b, range_intersect_agg(r)::text AS c, \
             range_intersect_agg(m)::text AS d FROM t",
            None,
        )
        .unwrap();
    assert_eq!(
        rows(&b)[0],
        ["{(,10)}", "{(,0),[1,)}", "empty", "{}"].map(|s| Some(s.to_string()))
    );
}
