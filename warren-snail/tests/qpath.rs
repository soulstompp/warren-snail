//! What the question path reads as PostgreSQL's analyser and planner read it: BETWEEN, NOT BETWEEN
//! and BETWEEN SYMMETRIC as their comparisons, on the question path and in a pipeline, the read
//! narrowed by them as by the comparisons written out; and SQL's spellings of SUBSTRING, each the
//! call PostgreSQL's grammar makes of it, answered as PostgreSQL 19 answers it.

mod weather;

use arrow::array::{Array, RecordBatch, StringArray};
use warren_snail::catalog::Catalog;
use warren_snail::read::{Round, Snail};
use warren_snail::values::Values;
use warren_snail::write::Rows;
use weather::{exact, fresh, ints64, one, tmp, write};

/// The rows of `s.t`: an id, a number on 64 places, NULL on every 97th row, and a text key of 500.
const ROWS: i64 = 131_072;

fn n_of(id: i64) -> Option<i64> {
    (id % 97 != 5).then_some(id % 64)
}

fn s_of(id: i64) -> String {
    format!("k{:03}-{}", id % 500, id % 7)
}

/// The table's snail: its key, and an index on each of `n` and `s`.
fn snail(name: &str) -> std::path::PathBuf {
    let path = tmp(&format!("{name}.snail"));
    let catalog = Catalog::parse(
        "CREATE TABLE s.t (id integer NOT NULL, n integer, s text, PRIMARY KEY (id));
         CREATE INDEX t_n ON s.t (n);
         CREATE INDEX t_s ON s.t (s);",
    )
    .unwrap();
    let ids: Vec<i64> = (0..ROWS).collect();
    let columns = vec![
        Values::Int(ids.iter().map(|&i| Some(i)).collect()),
        Values::Int(ids.iter().map(|&i| n_of(i)).collect()),
        Values::Text(ids.iter().map(|&i| Some(s_of(i))).collect()),
    ];
    write(&path, &catalog, &[("s.t".into(), Rows { columns })]).unwrap();
    path
}

fn ids(b: &RecordBatch) -> Vec<i64> {
    if b.num_rows() == 0 {
        return Vec::new();
    }
    let mut out: Vec<i64> = ints64(b.column(0).as_ref()).into_iter().flatten().collect();
    out.sort_unstable();
    out
}

/// The ids whose row passes `keep`.
fn kept(keep: impl Fn(i64) -> bool) -> Vec<i64> {
    (0..ROWS).filter(|&i| keep(i)).collect()
}

/// A question's ids and its round, on a snail holding no pages, along the order named.
fn asked(path: &std::path::Path, q: &str, order: Option<&str>) -> (Vec<i64>, Round) {
    let (b, round) = fresh(path).ask(q, order).unwrap();
    exact(q, &round);
    (ids(&b), round)
}

/// Two rounds that read the same: the order, its directory pages and its column pages.
fn same_read(a: &Round, b: &Round) {
    assert_eq!(
        (
            &a.order,
            a.directory_pages,
            a.column_pages_read,
            a.blocks_read
        ),
        (
            &b.order,
            b.directory_pages,
            b.column_pages_read,
            b.blocks_read
        ),
        "{a:?}\n{b:?}"
    );
}

#[test]
fn between_is_read_as_its_two_comparisons_and_narrows_the_read_as_they_do() {
    let path = snail("qpath_between");
    let in_range = |i: i64| n_of(i).is_some_and(|n| (3..=4).contains(&n));
    for order in [None, Some("t_n"), Some("t_pkey")] {
        let (a, ra) = asked(&path, "SELECT id FROM s.t WHERE n BETWEEN 3 AND 4", order);
        let (b, rb) = asked(&path, "SELECT id FROM s.t WHERE n >= 3 AND n <= 4", order);
        assert_eq!(a, kept(in_range));
        assert_eq!(a, b);
        same_read(&ra, &rb);
    }
    // Along the index on n, the range narrows the walk: two of 64 places.
    let (_, r) = asked(
        &path,
        "SELECT id FROM s.t WHERE n BETWEEN 3 AND 4",
        Some("t_n"),
    );
    let (_, whole) = asked(&path, "SELECT id FROM s.t WHERE n IS NOT NULL", Some("t_n"));
    assert!(r.column_pages_read * 8 < whole.column_pages_read, "{r:?}");
    // The bounds the other way round keep nothing; a NULL bound, nothing.
    for q in [
        "SELECT id FROM s.t WHERE n BETWEEN 4 AND 3",
        "SELECT id FROM s.t WHERE n BETWEEN NULL AND 4",
        "SELECT id FROM s.t WHERE n BETWEEN 3 AND NULL",
    ] {
        assert_eq!(asked(&path, q, None).0, Vec::<i64>::new(), "{q}");
    }
    // A number between two columns, and a text between two values.
    let (a, _) = asked(&path, "SELECT id FROM s.t WHERE 3 BETWEEN n AND id", None);
    assert_eq!(a, kept(|i| i >= 3 && n_of(i).is_some_and(|n| n <= 3)));
    let (a, ra) = asked(
        &path,
        "SELECT id FROM s.t WHERE s BETWEEN 'k100' AND 'k102-3'",
        Some("t_s"),
    );
    let (b, rb) = asked(
        &path,
        "SELECT id FROM s.t WHERE s >= 'k100' AND s <= 'k102-3'",
        Some("t_s"),
    );
    assert_eq!(
        a,
        kept(|i| {
            let s = s_of(i);
            s.as_str() >= "k100" && s.as_str() <= "k102-3"
        })
    );
    assert_eq!(a, b);
    same_read(&ra, &rb);
}

#[test]
fn not_between_keeps_the_values_outside_its_bounds_and_skips_the_blocks_inside() {
    let path = snail("qpath_not_between");
    let outside = |i: i64| n_of(i).is_some_and(|n| !(10..=50).contains(&n));
    let (a, r) = asked(
        &path,
        "SELECT id FROM s.t WHERE n NOT BETWEEN 10 AND 50",
        Some("t_n"),
    );
    assert_eq!(a, kept(outside));
    // Every block whose values all lie from 10 to 50 is left unread.
    assert!(r.entries_none > 0, "{r:?}");
    let (_, whole) = asked(&path, "SELECT id FROM s.t WHERE n IS NOT NULL", Some("t_n"));
    assert!(r.column_pages_read < whole.column_pages_read, "{r:?}");
    // Counted where the counts close: no NULL among the rows kept.
    let (b, _) = fresh(&path)
        .ask(
            "SELECT count(*) FROM s.t WHERE n NOT BETWEEN 10 AND 50",
            Some("t_n"),
        )
        .unwrap();
    assert_eq!(one(&b), kept(outside).len() as i64);
    // A NULL bound leaves the other comparison: n > 50, or n < 10.
    let (a, _) = asked(
        &path,
        "SELECT id FROM s.t WHERE n NOT BETWEEN NULL AND 50",
        None,
    );
    assert_eq!(a, kept(|i| n_of(i).is_some_and(|n| n > 50)));
    let (a, _) = asked(
        &path,
        "SELECT id FROM s.t WHERE n NOT BETWEEN 10 AND NULL",
        None,
    );
    assert_eq!(a, kept(|i| n_of(i).is_some_and(|n| n < 10)));
    // Bounds the other way round keep every value.
    let (a, _) = asked(
        &path,
        "SELECT id FROM s.t WHERE n NOT BETWEEN 50 AND 10",
        None,
    );
    assert_eq!(a, kept(|i| n_of(i).is_some()));
    // On text, the values before the first bound and after the second.
    let (a, _) = asked(
        &path,
        "SELECT id FROM s.t WHERE s NOT BETWEEN 'k001' AND 'k498-6'",
        Some("t_s"),
    );
    assert_eq!(
        a,
        kept(|i| {
            let s = s_of(i);
            s.as_str() < "k001" || s.as_str() > "k498-6"
        })
    );
    // Two columns of one table are not read, as `n > id` is not.
    assert!(fresh(&path)
        .ask("SELECT id FROM s.t WHERE n NOT BETWEEN 1 AND id", None)
        .is_err());
}

#[test]
fn between_symmetric_takes_its_bounds_in_either_order() {
    let path = snail("qpath_symmetric");
    for (q, plain) in [
        (
            "SELECT id FROM s.t WHERE n BETWEEN SYMMETRIC 4 AND 3",
            "SELECT id FROM s.t WHERE n BETWEEN 3 AND 4",
        ),
        (
            "SELECT id FROM s.t WHERE n BETWEEN SYMMETRIC (3) AND 4",
            "SELECT id FROM s.t WHERE n BETWEEN 3 AND 4",
        ),
        (
            "SELECT id FROM s.t WHERE n NOT BETWEEN SYMMETRIC 50 AND 10",
            "SELECT id FROM s.t WHERE n NOT BETWEEN 10 AND 50",
        ),
        (
            "SELECT id FROM s.t WHERE n BETWEEN ASYMMETRIC 3 AND 4",
            "SELECT id FROM s.t WHERE n BETWEEN 3 AND 4",
        ),
    ] {
        let (a, ra) = asked(&path, q, Some("t_n"));
        let (b, rb) = asked(&path, plain, Some("t_n"));
        assert_eq!(a, b, "{q}");
        assert!(!a.is_empty(), "{q}");
        same_read(&ra, &rb);
    }
    // A bound computed is read by a pipeline.
    let (a, _) = asked(
        &path,
        "SELECT id FROM s.t WHERE n between symmetric 2 + 2 AND 3",
        None,
    );
    assert_eq!(
        a,
        asked(&path, "SELECT id FROM s.t WHERE n BETWEEN 3 AND 4", None).0
    );
    // A NULL bound keeps nothing, NOT or not.
    for q in [
        "SELECT id FROM s.t WHERE n BETWEEN SYMMETRIC NULL AND 4",
        "SELECT id FROM s.t WHERE n NOT BETWEEN SYMMETRIC 10 AND NULL",
    ] {
        assert_eq!(asked(&path, q, None).0, Vec::<i64>::new(), "{q}");
    }
    // ASYMMETRIC keeps nothing where the bounds are the other way round.
    let (a, _) = asked(
        &path,
        "SELECT id FROM s.t WHERE n BETWEEN ASYMMETRIC 4 AND 3",
        None,
    );
    assert!(a.is_empty());
    // As PostgreSQL's grammar has it, a bound is required.
    let e = fresh(&path)
        .ask("SELECT id FROM s.t WHERE n BETWEEN SYMMETRIC AND 3", None)
        .unwrap_err();
    assert!(
        e.to_string().contains("syntax error at or near \"AND\""),
        "{e}"
    );
}

#[test]
fn a_pipeline_reads_between_as_postgresql_s_analyser_writes_it() {
    let path = snail("qpath_between_pipeline");
    let step = "WITH x AS (SELECT id, n, s FROM s.t)";
    for (q, keep) in [
        (
            format!("{step} SELECT id FROM x WHERE n BETWEEN 3 AND 4"),
            Box::new(|i: i64| n_of(i).is_some_and(|n| (3..=4).contains(&n)))
                as Box<dyn Fn(i64) -> bool>,
        ),
        (
            format!("{step} SELECT id FROM x WHERE n NOT BETWEEN 10 AND 50"),
            Box::new(|i: i64| n_of(i).is_some_and(|n| !(10..=50).contains(&n))),
        ),
        (
            format!("{step} SELECT id FROM x WHERE n BETWEEN SYMMETRIC 4 AND 3"),
            Box::new(|i: i64| n_of(i).is_some_and(|n| (3..=4).contains(&n))),
        ),
        (
            format!("{step} SELECT id FROM x WHERE n NOT BETWEEN SYMMETRIC 50 AND NULL"),
            Box::new(|_: i64| false),
        ),
        (
            format!("{step} SELECT id FROM x WHERE abs(n) NOT BETWEEN NULL AND 50"),
            Box::new(|i: i64| n_of(i).is_some_and(|n| n > 50)),
        ),
    ] {
        let (b, _) = fresh(&path).ask(&q, None).unwrap();
        assert_eq!(ids(&b), kept(keep), "{q}");
    }
    // In what a step returns: NULL where the value is.
    let (b, _) = fresh(&path)
        .ask(
            &format!(
                "{step} SELECT id, (n BETWEEN SYMMETRIC 5 AND 1)::text AS v FROM x WHERE id < 10 \
                 OR id = 5"
            ),
            None,
        )
        .unwrap();
    let v = b.column(1).as_any().downcast_ref::<StringArray>().unwrap();
    let got: Vec<Option<&str>> = (0..v.len())
        .map(|r| v.is_valid(r).then(|| v.value(r)))
        .collect();
    let want: Vec<Option<&str>> = (0..10)
        .map(|i| {
            n_of(i).map(|n| {
                if (1..=5).contains(&n) {
                    "true"
                } else {
                    "false"
                }
            })
        })
        .collect();
    assert_eq!(got, want);
}

/// A snail of one table made empty from its DDL, then the statements run on it.
#[cfg(feature = "standalone")]
fn snail_of(name: &str, ddl: &str, statements: &[&str]) -> Snail {
    let path = tmp(&format!("{name}.snail"));
    let catalog = Catalog::parse(ddl).unwrap();
    let table = catalog.tables[0].name.clone();
    let empty = Rows {
        columns: catalog.tables[0]
            .columns
            .iter()
            .map(|c| Values::new(c.ty))
            .collect(),
    };
    write(&path, &catalog, &[(table, empty)]).unwrap();
    let mut snail = Snail::open_writing(&path).unwrap();
    for s in statements {
        snail.execute(s).unwrap();
    }
    snail
}

/// One row of each string type, as `types.nu`'s SUBSTRING checks hold it.
#[cfg(feature = "standalone")]
fn strings_snail() -> Snail {
    snail_of(
        "qpath_substring",
        "CREATE TABLE s.u (id integer NOT NULL, s text, c char(6), nm name, b bytea, \
         bt bit varying(10), i integer, PRIMARY KEY (id));",
        &["INSERT INTO s.u VALUES (1, 'hello', 'ab', 'hello', '\\x616263', B'10110', 2)"],
    )
}

/// An expression over the one row, as text: PostgreSQL's answer, or its refusal's words.
#[cfg(feature = "standalone")]
fn substring(snail: &mut Snail, expr: &str) -> Result<Option<String>, String> {
    let q = format!(
        "WITH x AS (SELECT id, s, c, nm, b, bt, i FROM s.u) SELECT ({expr})::text AS v FROM x"
    );
    let (b, _) = snail.ask(&q, None).map_err(|e| e.to_string())?;
    let a = b.column(0).as_any().downcast_ref::<StringArray>().unwrap();
    Ok(a.is_valid(0).then(|| a.value(0).to_string()))
}

#[cfg(feature = "standalone")]
#[test]
fn each_spelling_of_substring_is_the_call_postgresql_s_grammar_makes_of_it() {
    let mut s = strings_snail();
    // Each beside PostgreSQL 19's answer to it, over the same row.
    for (expr, want) in [
        ("substring(s FROM 2 FOR 3)", Some("ell")),
        ("substring(s FOR 3 FROM 2)", Some("ell")),
        ("substring(s FOR i FROM i)", Some("el")),
        ("substring(s FROM 2)", Some("ello")),
        ("substring(s, 2)", Some("ello")),
        ("substring(s, 2, 3)", Some("ell")),
        ("substring(s FOR '3')", Some("hel")),
        ("substring(s FOR 2.5)", Some("hel")),
        ("substring(s FOR true)", Some("h")),
        ("substring(s FROM '2' FOR 3)", Some("ell")),
        ("substring(s FROM 2 FOR '3')", Some("ell")),
        ("substring(s FROM -1 FOR 3)", Some("h")),
        ("substring(s FROM 0 FOR 2)", Some("h")),
        ("substring(s FROM 'l+')", Some("ll")),
        ("substring(s, 'l+')", Some("ll")),
        ("substring(s FROM '2')", None),
        ("substring(s FROM '2' FOR '3')", None),
        ("substring(s FOR '#' FROM '%#\"l_#\"%')", Some("ll")),
        ("substring(s FROM '%#\"l_#\"%' FOR '#')", Some("ll")),
        ("substring(s SIMILAR '%#\"l_#\"%' ESCAPE '#')", Some("ll")),
        ("substring(s FROM NULL)", None),
        ("substring(s FOR NULL)", None),
        ("substring(c FROM 2)", Some("b")),
        ("substring(c FROM 'b.*')", Some("b")),
        ("substring(nm FROM 2 FOR 2)", Some("el")),
        ("substring(b FROM 2 FOR 1)", Some("\\x62")),
        ("substring(bt FROM 2 FOR 3)", Some("011")),
        ("substring(bt FROM 4)", Some("10")),
        ("substr(s, '2')", Some("ello")),
        ("substr(s, 2, 2)", Some("el")),
    ] {
        assert_eq!(
            substring(&mut s, expr),
            Ok(want.map(str::to_string)),
            "{expr}"
        );
    }
    // Each refused in PostgreSQL 19's words.
    for (expr, words) in [
        ("substring(s SIMILAR 'h%')", "syntax error at or near \")\""),
        (
            "substring(s SIMILAR 'h%' FOR 'x')",
            "syntax error at or near \"FOR\"",
        ),
        (
            "substring(s FROM 1.5)",
            "function pg_catalog.substring(text, numeric) does not exist",
        ),
        (
            "substring(i FROM 2)",
            "function pg_catalog.substring(integer, integer) does not exist",
        ),
        (
            "substring(s FROM s FOR i)",
            "function pg_catalog.substring(text, text, integer) does not exist",
        ),
        (
            "substring(s FROM 3000000000)",
            "function pg_catalog.substring(text, bigint) does not exist",
        ),
        (
            "substring(s FROM 'l' FOR i)",
            "invalid input syntax for type integer: \"l\"",
        ),
        (
            "substring(b FROM 'a')",
            "invalid input syntax for type integer: \"a\"",
        ),
        (
            "substring(s FOR s)",
            "invalid input syntax for type integer: \"hello\"",
        ),
        (
            "substring(s FROM 2 FOR -1)",
            "negative substring length not allowed",
        ),
        (
            "substring(s FOR '#\"l_#\"%' FROM '%')",
            "invalid escape string",
        ),
        ("substr(s, s)", "function substr(text, text) does not exist"),
        (
            "substr(bt, 2)",
            "function substr(bit varying, integer) does not exist",
        ),
    ] {
        let got = substring(&mut s, expr);
        assert!(
            got.as_ref().is_err_and(|e| e.contains(words)),
            "{expr}: {got:?}"
        );
    }
}
