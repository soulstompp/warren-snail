//! Set operations: UNION, INTERSECT and EXCEPT, each with ALL, over one table and over two, nested
//! with INTERSECT binding tighter and grouped by parentheses, sorted and limited over the whole,
//! inside WITH and as a derived table; each column typed as select_common_type types its arms', a
//! mismatch refused in PostgreSQL's words; rows met by their values, NULLs as not distinct and
//! arrays by their elements. Every answer is PostgreSQL 19's and 18's over the same rows.

mod weather;

use arrow::array::{Array, RecordBatch};
use arrow::util::display::array_value_to_string;
use warren_snail::catalog::Catalog;
use warren_snail::values::Values;
use warren_snail::write::Rows;
use weather::{fresh, tmp, write};

const DDL: &str = "CREATE SCHEMA a; CREATE TABLE a.t (id integer NOT NULL, g integer, s text, PRIMARY KEY (id));
     CREATE TABLE a.u (id integer NOT NULL, g bigint, s text, PRIMARY KEY (id));";

/// The tables' snail: `a.t` seven rows and `a.u` five, rows repeating and NULLs in both columns.
fn snail(name: &str) -> std::path::PathBuf {
    let path = tmp(&format!("sets-{name}.snail"));
    let int = |v: &[Option<i64>]| Values::Int(v.to_vec());
    let text = |v: &[Option<&str>]| Values::Text(v.iter().map(|s| s.map(str::to_string)).collect());
    let t = Rows {
        columns: vec![
            int(&[
                Some(1),
                Some(2),
                Some(3),
                Some(4),
                Some(5),
                Some(6),
                Some(7),
            ]),
            int(&[Some(1), Some(1), Some(2), Some(2), None, None, None]),
            text(&[Some("x"), Some("x"), Some("y"), None, Some("z"), None, None]),
        ],
    };
    let u = Rows {
        columns: vec![
            int(&[Some(1), Some(2), Some(3), Some(4), Some(5)]),
            int(&[Some(1), Some(2), None, None, Some(1)]),
            text(&[Some("x"), Some("q"), Some("z"), None, Some("x")]),
        ],
    };
    let catalog = Catalog::parse(DDL).unwrap();
    write(&path, &catalog, &[("a.t".into(), t), ("a.u".into(), u)]).unwrap();
    path
}

type Row = Vec<Option<String>>;

/// An answer's rows, each value as text, NULL as none.
fn rows(b: &RecordBatch) -> Vec<Row> {
    (0..b.num_rows())
        .map(|r| {
            b.columns()
                .iter()
                .map(|c| (!c.is_null(r)).then(|| array_value_to_string(c, r).unwrap()))
                .collect()
        })
        .collect()
}

/// Rows written as text, each row's fields between spaces, a field of `-` NULL.
fn want(rows: &str) -> Vec<Row> {
    rows.split_whitespace()
        .map(|r| {
            r.split(',')
                .map(|v| (v != "-").then(|| v.to_string()))
                .collect()
        })
        .collect()
}

/// A question's answer, in its order, its rows those written; every table read its predicted
/// pages, less those held.
fn same(path: &std::path::Path, q: &str, expected: &str) {
    let (b, round) = fresh(path)
        .ask(q, None)
        .unwrap_or_else(|e| panic!("{q}: {e}"));
    assert_eq!(rows(&b), want(expected), "{q}");
    assert_eq!(
        round.column_pages_read,
        round.column_pages_predicted - round.column_pages_held,
        "{q}: {round:?}"
    );
}

fn refused(path: &std::path::Path, q: &str) -> String {
    match fresh(path).ask(q, None) {
        Ok((b, _)) => panic!("{q}: answered {:?}", rows(&b)),
        Err(e) => e.to_string(),
    }
}

#[test]
fn every_operator_over_one_table_and_over_two() {
    let path = snail("operators");
    for (op, one, two) in [
        ("UNION", "1,x 2,y 2,- -,z -,-", "1,x 2,q 2,y 2,- -,z -,-"),
        (
            "UNION ALL",
            "1,x 1,x 2,y 2,- 2,- -,z -,z -,- -,- -,- -,-",
            "1,x 1,x 1,x 1,x 2,q 2,y 2,- -,z -,z -,- -,- -,-",
        ),
        ("INTERSECT", "2,- -,z -,-", "1,x -,z -,-"),
        ("INTERSECT ALL", "2,- -,z -,- -,-", "1,x 1,x -,z -,-"),
        ("EXCEPT", "1,x 2,y", "2,y 2,-"),
        ("EXCEPT ALL", "1,x 1,x 2,y", "2,y 2,- -,-"),
    ] {
        same(
            &path,
            &format!("SELECT g, s FROM a.t {op} SELECT g, s FROM a.t WHERE id > 3 ORDER BY 1, 2"),
            one,
        );
        same(
            &path,
            &format!("SELECT g, s FROM a.t {op} SELECT g, s FROM a.u ORDER BY 1, 2"),
            two,
        );
    }
}

#[test]
fn intersect_binds_tighter_and_parentheses_group() {
    let path = snail("nesting");
    same(
        &path,
        "SELECT g FROM a.u UNION SELECT g FROM a.t INTERSECT SELECT g FROM a.t WHERE id > 4 ORDER BY 1",
        "1 2 -",
    );
    same(
        &path,
        "(SELECT g FROM a.u UNION SELECT g FROM a.t) INTERSECT SELECT g FROM a.t WHERE id > 2 \
         ORDER BY 1",
        "2 -",
    );
    // EXCEPT is taken left to right, unless parentheses say otherwise.
    same(
        &path,
        "SELECT g FROM a.t EXCEPT SELECT g FROM a.u EXCEPT SELECT g FROM a.t WHERE id = 3 ORDER BY 1",
        "",
    );
    same(
        &path,
        "SELECT g FROM a.t EXCEPT (SELECT g FROM a.u EXCEPT SELECT g FROM a.t WHERE id = 1) \
         ORDER BY 1",
        "1",
    );
}

#[test]
fn columns_typed_as_select_common_type_types_them() {
    let path = snail("typed");
    // An integer beside a bigint is a bigint, and beside a numeric a numeric, as the arm met after
    // names them.
    let e = refused(
        &path,
        "(SELECT g FROM a.t UNION SELECT g FROM a.u) EXCEPT SELECT s FROM a.t",
    );
    assert_eq!(e, "EXCEPT types bigint and text cannot be matched");
    let e = refused(
        &path,
        "(SELECT g FROM a.t UNION SELECT 2.5) INTERSECT SELECT s FROM a.u",
    );
    assert_eq!(e, "INTERSECT types numeric and text cannot be matched");
    same(
        &path,
        "SELECT x FROM (SELECT g AS x FROM a.t WHERE id < 3 UNION SELECT 2.5) d ORDER BY 1",
        "1 2.5",
    );
    // Quoted literals are text where nothing types them, and read in the type an arm gives.
    same(&path, "SELECT 'b' AS x UNION SELECT 'a' ORDER BY 1", "a b");
    same(
        &path,
        "SELECT g AS x FROM a.t WHERE id < 4 UNION SELECT '7' UNION SELECT NULL ORDER BY 1",
        "1 2 7 -",
    );
}

#[test]
fn a_mismatch_is_refused_in_postgresql_s_words() {
    let path = snail("refused");
    for (q, words) in [
        (
            "SELECT g, s FROM a.t UNION SELECT g FROM a.u",
            "each UNION query must have the same number of columns",
        ),
        (
            "SELECT g FROM a.t INTERSECT SELECT s FROM a.u",
            "INTERSECT types integer and text cannot be matched",
        ),
        (
            "SELECT g FROM a.t EXCEPT SELECT 'x'",
            "invalid input syntax for type integer: \"x\"",
        ),
        // Two literals are text before a third arm meets them.
        (
            "SELECT 'a' UNION SELECT 'b' UNION SELECT 1",
            "UNION types text and integer cannot be matched",
        ),
        // A VALUES list's literal is text before the set operation meets it.
        (
            "SELECT g FROM a.t UNION VALUES ('2')",
            "UNION types integer and text cannot be matched",
        ),
        (
            "SELECT g FROM a.t UNION SELECT g FROM a.u ORDER BY g + 1",
            "invalid UNION/INTERSECT/EXCEPT ORDER BY clause",
        ),
    ] {
        let e = refused(&path, q);
        assert!(e.contains(words), "{q}: {e}");
    }
    // A literal no integer reads, and a type with no equality, are refused before any row is read;
    // UNION ALL asks no equality.
    let e = refused(&path, "SELECT g FROM a.t WHERE id > 99 UNION SELECT 'x'");
    assert!(e.contains("invalid input syntax for type integer"), "{e}");
    let e = refused(
        &path,
        "SELECT '{}'::json AS x WHERE false UNION SELECT '[]'::json WHERE false",
    );
    assert_eq!(e, "could not identify an equality operator for type json");
    same(
        &path,
        "SELECT '{}'::json AS x WHERE false UNION ALL SELECT '[]'::json",
        "[]",
    );
}

#[test]
fn a_set_operation_s_order_by_is_refused_in_postgresql_s_words() {
    // As 18.6 refused each on 28818 (2026-11-11): every item's columns read among the result's
    // alone, before an item that is no column of the result is refused.
    let path = snail("order-by-refused");
    let expression = "invalid UNION/INTERSECT/EXCEPT ORDER BY clause\nDETAIL:  Only result column \
         names can be used, not expressions or functions.\nHINT:  Add the expression/function to \
         every SELECT, or move the UNION into a FROM clause.";
    for (by, code, words) in [
        ("min(id)", "42703", "column \"id\" does not exist"),
        ("id", "42703", "column \"id\" does not exist"),
        ("t.g", "42P01", "missing FROM-clause entry for table \"t\""),
        ("min(g)", "0A000", expression),
        ("g + 1", "0A000", expression),
        ("g + 1, id", "42703", "column \"id\" does not exist"),
    ] {
        let q = format!("SELECT g FROM a.t UNION SELECT g FROM a.u ORDER BY {by}");
        let e = fresh(&path).ask(&q, None).unwrap_err();
        assert_eq!((e.sqlstate(), e.to_string().as_str()), (Some(code), words), "{q}");
    }
    same(
        &path,
        "SELECT g FROM a.t UNION SELECT g FROM a.u ORDER BY g DESC",
        "- 2 1",
    );
}

#[test]
fn sorted_and_limited_over_the_whole_inside_with_and_derived() {
    let path = snail("whole");
    same(
        &path,
        "SELECT g, s FROM a.t UNION SELECT g, s FROM a.u ORDER BY g DESC NULLS LAST, s LIMIT 3 \
         OFFSET 1",
        "2,y 2,- 1,x",
    );
    same(
        &path,
        "WITH x (n, label) AS (SELECT g, s FROM a.t INTERSECT SELECT g, s FROM a.u) \
         SELECT x.n, x.label, u.id FROM x JOIN a.u u ON u.g = x.n ORDER BY 3",
        "1,x,1 1,x,5",
    );
    same(
        &path,
        "SELECT d.g, count(*) AS n FROM (SELECT g FROM a.t EXCEPT ALL SELECT g FROM a.u) d \
         GROUP BY d.g ORDER BY 1",
        "2,1 -,1",
    );
    // Arms reading a WITH query, one sorted and limited in parentheses.
    same(
        &path,
        "WITH w AS (SELECT g, s FROM a.t WHERE g IS NOT NULL) \
         (SELECT g FROM w ORDER BY g DESC LIMIT 1) UNION ALL SELECT g FROM w WHERE s = 'x' \
         ORDER BY 1",
        "1 1 2",
    );
}

#[test]
fn a_column_keeps_a_length_only_where_every_arm_gives_it() {
    let path = tmp("sets-lengths.snail");
    let catalog = Catalog::parse(
        "CREATE SCHEMA b; CREATE TABLE b.l (id integer NOT NULL, agegr1 varchar(5), \
         v7 varchar(7), c3 char(3), c5 char(5), PRIMARY KEY (id));",
    )
    .unwrap();
    let text = |s: &str| Values::Text(vec![Some(s.into())]);
    let l = Rows {
        columns: vec![
            Values::Int(vec![Some(1)]),
            text("a"),
            text("abcdefg"),
            text("abc"),
            text("abcde"),
        ],
    };
    write(&path, &catalog, &[("b.l".into(), l)]).unwrap();
    // An answer's first column, each value as text, and the type its metadata names, if any.
    let ask = |q: &str| {
        let (b, _) = fresh(&path)
            .ask(q, None)
            .unwrap_or_else(|e| panic!("{q}: {e}"));
        let ty = b.schema().field(0).metadata().get("pg_type").cloned();
        let values: Vec<String> = rows(&b)
            .into_iter()
            .map(|r| r[0].clone().unwrap_or_else(|| "-".into()))
            .collect();
        (values.join(" "), ty.unwrap_or_default())
    };
    // A literal, or a CASE of literals, beside a varchar(5), either first, at the top and in a FROM:
    // a varchar of no length, the CASE's text where it comes first.
    for (q, want) in [
        ("SELECT 'African Descent' AS c UNION ALL SELECT agegr1 FROM b.l", "African Descent a"),
        ("SELECT agegr1 FROM b.l UNION ALL SELECT 'African Descent'", "a African Descent"),
        (
            "SELECT c || '|' || pg_typeof(c) FROM (SELECT 'African Descent' AS c UNION ALL \
             SELECT agegr1 FROM b.l) x ORDER BY 1",
            "African Descent|character varying a|character varying",
        ),
        (
            "SELECT c || '|' || pg_typeof(c) FROM (SELECT agegr1 AS c FROM b.l UNION ALL \
             SELECT 'African Descent') x ORDER BY 1",
            "African Descent|character varying a|character varying",
        ),
        (
            "SELECT c || '|' || pg_typeof(c) FROM (SELECT CASE WHEN id > 0 THEN 'African Descent' \
             ELSE 'x' END AS c FROM b.l UNION ALL SELECT agegr1 FROM b.l) x ORDER BY 1",
            "African Descent|text a|text",
        ),
        (
            "SELECT c || '|' || pg_typeof(c) FROM (SELECT agegr1 AS c FROM b.l UNION ALL \
             SELECT CASE WHEN id > 0 THEN 'African Descent' ELSE 'x' END FROM b.l) x ORDER BY 1",
            "African Descent|character varying a|character varying",
        ),
        (
            "SELECT c FROM (SELECT v7 AS c FROM b.l UNION ALL SELECT agegr1 FROM b.l) x ORDER BY 1",
            "a abcdefg",
        ),
        (
            "SELECT column1 FROM (VALUES ('abc'::varchar(5)), ('African Descent')) v ORDER BY 1",
            "African Descent abc",
        ),
    ] {
        assert_eq!(ask(q).0, want, "{q}");
    }
    // A char(3) keeps its length only beside a char(3): beside a literal it pads nothing and cuts
    // nothing.
    for (q, want, ty) in [
        ("SELECT c3 FROM b.l UNION ALL SELECT c3 FROM b.l", "abc abc", "character(3)"),
        ("SELECT c3 FROM b.l UNION ALL SELECT c5 FROM b.l", "abc abcde", "bpchar"),
        ("SELECT c3 FROM b.l UNION ALL SELECT 'abcdefgh'", "abc abcdefgh", "bpchar"),
        ("SELECT c3 FROM b.l UNION ALL SELECT NULL", "abc -", "bpchar"),
        (
            "SELECT * FROM (VALUES ('ab'::char(3)), ('cd'::char(3))) v",
            "ab  cd ",
            "character(3)",
        ),
    ] {
        assert_eq!(ask(q), (want.to_string(), ty.to_string()), "{q}");
    }
    for (q, want) in [
        (
            "SELECT octet_length(c) FROM (SELECT c3 AS c FROM b.l UNION ALL SELECT 'ab') x",
            "3 2",
        ),
        (
            "SELECT octet_length(c) FROM (SELECT 'ab' AS c UNION ALL SELECT c3 FROM b.l) x",
            "2 3",
        ),
        (
            "SELECT octet_length(column1) FROM (VALUES ('ab'::char(3)), ('abcdefgh')) v",
            "3 8",
        ),
    ] {
        assert_eq!(ask(q).0, want, "{q}");
    }
}

#[test]
fn rows_of_arrays_are_met_by_their_elements() {
    let path = snail("arrays");
    let q = "SELECT d.x::text FROM (SELECT x FROM (VALUES ('{a,b}'::text[]), ('{a,b}'), \
             ('{a,NULL}'), ('{}'), ('{NULL}'), (NULL), (NULL)) v(x) EXCEPT ALL SELECT x FROM \
             (VALUES ('{a,b}'::text[]), ('{a,NULL}'), ('{b,a}'), (NULL)) w(x)) d ORDER BY 1";
    let (b, _) = fresh(&path)
        .ask(q, None)
        .unwrap_or_else(|e| panic!("{q}: {e}"));
    let text = |s: &str| Some(s.to_string());
    assert_eq!(
        rows(&b),
        vec![
            vec![text("{NULL}")],
            vec![text("{a,b}")],
            vec![text("{}")],
            vec![None]
        ],
        "{q}"
    );
}

#[test]
fn arms_read_tables_of_several_snails() {
    let one = snail("several-one");
    let two = tmp("sets-several-two.snail");
    let catalog = Catalog::parse(
        "CREATE SCHEMA b; CREATE TABLE b.v (id integer NOT NULL, g integer, s text, PRIMARY KEY (id));",
    )
    .unwrap();
    let v = Rows {
        columns: vec![
            Values::Int(vec![Some(1), Some(2)]),
            Values::Int(vec![Some(2), None]),
            Values::Text(vec![Some("y".into()), None]),
        ],
    };
    write(&two, &catalog, &[("b.v".into(), v)]).unwrap();
    let mut s = warren_snail::read::Snails::open(&[&one, &two]).unwrap();
    let q = "SELECT g, s FROM a.t EXCEPT ALL SELECT g, s FROM b.v ORDER BY 1, 2";
    let (b, round) = s.ask(q).unwrap_or_else(|e| panic!("{q}: {e}"));
    assert_eq!(rows(&b), want("1,x 1,x 2,- -,z -,-"), "{q}");
    assert_eq!(round.snails.len(), 2, "{q}");
    for part in &round.snails {
        assert_eq!(
            part.column_pages_read,
            part.column_pages_predicted - part.column_pages_held,
            "{q}: {part:?}"
        );
    }
}
