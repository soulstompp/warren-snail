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
    // Two columns of one table the question path does not read: a pipeline does.
    let (a, _) = asked(
        &path,
        "SELECT id FROM s.t WHERE n NOT BETWEEN 1 AND id",
        None,
    );
    assert_eq!(a, kept(|i| n_of(i).is_some_and(|n| n < 1 || n > i)));
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

/// The rows of `s.p`: an id, a text key of 2,000 places, and a nullable text under the collation C,
/// NULL on every 13th row, under an index DESC.
const PREFIXED: i64 = 65_536;

fn k_of(id: i64) -> String {
    format!("p{:04}-{}", id % 2000, id % 5)
}

fn u_of(id: i64) -> Option<String> {
    (id % 13 != 0).then(|| format!("{}é", k_of(id * 7 + 3)))
}

/// The table's snail: its key, an index on `k`, and on `u` DESC.
fn prefixed(name: &str) -> std::path::PathBuf {
    let path = tmp(&format!("{name}.snail"));
    let catalog = Catalog::parse(
        "CREATE TABLE s.p (id integer NOT NULL, k text NOT NULL, u text COLLATE \"C\", \
         PRIMARY KEY (id));
         CREATE INDEX p_k ON s.p (k);
         CREATE INDEX p_u ON s.p (u DESC);",
    )
    .unwrap();
    let ids: Vec<i64> = (0..PREFIXED).collect();
    let columns = vec![
        Values::Int(ids.iter().map(|&i| Some(i)).collect()),
        Values::Text(ids.iter().map(|&i| Some(k_of(i))).collect()),
        Values::Text(ids.iter().map(|&i| u_of(i)).collect()),
    ];
    write(&path, &catalog, &[("s.p".into(), Rows { columns })]).unwrap();
    path
}

/// The ids of `s.p` whose row passes `keep`.
fn prefixed_kept(keep: impl Fn(i64) -> bool) -> Vec<i64> {
    (0..PREFIXED).filter(|&i| keep(i)).collect()
}

#[test]
fn a_range_on_a_text_level_narrows_the_walk_each_end_kept_or_left_out_as_written() {
    let path = prefixed("qpath_text_range");
    let k = |i: i64| k_of(i);
    for (q, keep) in [
        (
            "SELECT id FROM s.p WHERE k >= 'p0100' AND k < 'p0102'",
            Box::new(|i: i64| k(i).as_str() >= "p0100" && k(i).as_str() < "p0102")
                as Box<dyn Fn(i64) -> bool>,
        ),
        (
            "SELECT id FROM s.p WHERE k > 'p0100-4' AND k <= 'p0101-2'",
            Box::new(|i: i64| k(i).as_str() > "p0100-4" && k(i).as_str() <= "p0101-2"),
        ),
        (
            "SELECT id FROM s.p WHERE k < 'p0001-1'",
            Box::new(|i: i64| k(i).as_str() < "p0001-1"),
        ),
        (
            "SELECT id FROM s.p WHERE k >= 'p1999-3'",
            Box::new(|i: i64| k(i).as_str() >= "p1999-3"),
        ),
    ] {
        let (a, r) = asked(&path, q, Some("p_k"));
        assert_eq!(a, prefixed_kept(&keep), "{q}");
        // A few blocks of the order's, not all of them.
        let (_, whole) = asked(&path, "SELECT id FROM s.p WHERE k IS NOT NULL", Some("p_k"));
        assert!(r.blocks_read * 8 < whole.blocks_read, "{q}: {r:?}");
        assert!(r.entries_none > 0, "{q}: {r:?}");
        // Along the table's key the same rows.
        assert_eq!(asked(&path, q, Some("p_pkey")).0, a, "{q}");
    }
    // Counted from the counts where a block lies inside the range: no column page read for it.
    let (b, r) = fresh(&path)
        .ask(
            "SELECT count(*) FROM s.p WHERE k >= 'p0100' AND k < 'p0400'",
            Some("p_k"),
        )
        .unwrap();
    assert_eq!(
        one(&b),
        prefixed_kept(|i| k(i).as_str() >= "p0100" && k(i).as_str() < "p0400").len() as i64
    );
    assert!(r.entries_must > 0, "{r:?}");
    // On an index DESC whose NULLs come first: none of them is in a range, open or not.
    for (q, keep) in [
        (
            "SELECT id FROM s.p WHERE u < 'p0300'",
            Box::new(|i: i64| u_of(i).is_some_and(|u| u.as_str() < "p0300"))
                as Box<dyn Fn(i64) -> bool>,
        ),
        (
            "SELECT id FROM s.p WHERE u > 'p1700'",
            Box::new(|i: i64| u_of(i).is_some_and(|u| u.as_str() > "p1700")),
        ),
        (
            "SELECT id FROM s.p WHERE u BETWEEN 'p0500' AND 'p0501'",
            Box::new(|i: i64| {
                u_of(i).is_some_and(|u| ("p0500".."p0501").contains(&u.as_str()) || u == "p0501")
            }),
        ),
    ] {
        let (a, r) = asked(&path, q, Some("p_u"));
        assert_eq!(a, prefixed_kept(&keep), "{q}");
        let (_, whole) = asked(&path, "SELECT id FROM s.p WHERE u IS NOT NULL", Some("p_u"));
        assert!(r.blocks_read < whole.blocks_read, "{q}: {r:?}");
    }
}

#[test]
fn an_equality_on_a_text_level_fixes_it_and_the_next_level_narrows_after_it() {
    let path = tmp("qpath_text_equality.snail");
    let catalog = Catalog::parse(
        "CREATE TABLE s.g (id integer NOT NULL, g text NOT NULL, k text NOT NULL, PRIMARY KEY (id));
         CREATE INDEX g_gk ON s.g (g, k);",
    )
    .unwrap();
    let ids: Vec<i64> = (0..PREFIXED).collect();
    let columns = vec![
        Values::Int(ids.iter().map(|&i| Some(i)).collect()),
        Values::Text(ids.iter().map(|&i| Some(format!("g{}", i % 4))).collect()),
        Values::Text(ids.iter().map(|&i| Some(k_of(i))).collect()),
    ];
    write(&path, &catalog, &[("s.g".into(), Rows { columns })]).unwrap();
    // A text level after it keeps no summary, so only the walk narrows by it.
    let (a, r) = asked(
        &path,
        "SELECT id FROM s.g WHERE g = 'g1' AND k < 'p0100'",
        Some("g_gk"),
    );
    assert_eq!(
        a,
        (0..PREFIXED)
            .filter(|&i| i % 4 == 1 && k_of(i).as_str() < "p0100")
            .collect::<Vec<i64>>()
    );
    let (_, whole) = asked(&path, "SELECT id FROM s.g WHERE g = 'g1'", Some("g_gk"));
    assert!(r.blocks_read < whole.blocks_read, "{r:?} {whole:?}");
}

#[test]
fn a_pattern_s_fixed_prefix_narrows_the_read_along_an_order_on_its_column_and_changes_no_answer() {
    let path = prefixed("qpath_prefix");
    let k = |i: i64| k_of(i);
    // ILIKE's prefix stops at its first letter, which has two cases: no prefix, the order read
    // whole, the same rows.
    let (unnarrowed, whole) = asked(
        &path,
        "SELECT id FROM s.p WHERE k ILIKE 'p012%'",
        Some("p_k"),
    );
    assert_eq!(unnarrowed, prefixed_kept(|i| k(i).starts_with("p012")));
    assert_eq!(whole.entries_none, 0, "{whole:?}");
    let range = asked(
        &path,
        "SELECT id FROM s.p WHERE k >= 'p012' AND k < 'p013'",
        Some("p_k"),
    );
    for q in [
        "SELECT id FROM s.p WHERE k LIKE 'p012%'",
        "SELECT id FROM s.p WHERE k LIKE 'p012_-%'",
        "SELECT id FROM s.p WHERE k LIKE 'p012%' ESCAPE '#'",
        "SELECT id FROM s.p WHERE k ~ '^p012'",
        "SELECT id FROM s.p WHERE k ~ '^p012.*'",
        "SELECT id FROM s.p WHERE k ~ '^p012[0-9]'",
        "SELECT id FROM s.p WHERE k SIMILAR TO 'p012%'",
        "SELECT id FROM s.p WHERE k SIMILAR TO 'p012#%%' ESCAPE '#' OR k SIMILAR TO 'p012%'",
    ] {
        // A disjunction is the pipeline's, which reads the table whole.
        let order = if q.contains(" OR ") {
            None
        } else {
            Some("p_k")
        };
        let (a, r) = asked(&path, q, order);
        assert_eq!(a, unnarrowed, "{q}");
        if order.is_some() {
            // The blocks the written range reads, and far fewer column pages than the whole.
            assert_eq!(
                (r.blocks_read, r.directory_pages),
                (range.1.blocks_read, range.1.directory_pages),
                "{q}: {r:?}"
            );
            assert!(
                r.column_pages_read * 8 < whole.column_pages_read,
                "{q}: {r:?}"
            );
        }
    }
    // A pattern of one string narrows to its one key.
    for q in [
        "SELECT id FROM s.p WHERE k LIKE 'p0123-3'",
        "SELECT id FROM s.p WHERE k ~ '^p0123-3$'",
        "SELECT id FROM s.p WHERE k SIMILAR TO 'p0123-3'",
        "SELECT id FROM s.p WHERE k ILIKE '0123-3'",
    ] {
        let (a, r) = asked(&path, q, Some("p_k"));
        let want = if q.contains("ILIKE") {
            Vec::new()
        } else {
            prefixed_kept(|i| k(i) == "p0123-3")
        };
        assert_eq!(a, want, "{q}");
        assert!(r.blocks_read <= 1, "{q}: {r:?}");
    }
    // The pattern is held on every row the range keeps: what the prefix keeps beside it is not
    // in the answer.
    let (a, _) = asked(
        &path,
        "SELECT id FROM s.p WHERE k LIKE 'p012%-3'",
        Some("p_k"),
    );
    assert_eq!(
        a,
        prefixed_kept(|i| k(i).starts_with("p012") && k(i).ends_with("-3"))
    );
    let (a, _) = asked(
        &path,
        "SELECT id FROM s.p WHERE k ~ '^p01(2|3)4'",
        Some("p_k"),
    );
    assert_eq!(
        a,
        prefixed_kept(|i| k(i).starts_with("p0124") || k(i).starts_with("p0134"))
    );
    // A negation narrows nothing; NULL keeps no row.
    let (a, r) = asked(
        &path,
        "SELECT id FROM s.p WHERE k NOT LIKE 'p012%'",
        Some("p_k"),
    );
    assert_eq!(a, prefixed_kept(|i| !k(i).starts_with("p012")));
    assert_eq!(r.entries_none, 0, "{r:?}");
    let (a, _) = asked(&path, "SELECT id FROM s.p WHERE k !~ '^p'", Some("p_k"));
    assert!(a.is_empty());
    let (a, r) = asked(&path, "SELECT id FROM s.p WHERE k LIKE NULL", None);
    assert!(a.is_empty());
    assert_eq!(r.column_pages_read, 0);
    // A pattern on a nullable text under C, along its index DESC: no NULL among the rows.
    let (a, r) = asked(&path, "SELECT id FROM s.p WHERE u LIKE 'p03%'", Some("p_u"));
    assert_eq!(
        a,
        prefixed_kept(|i| u_of(i).is_some_and(|u| u.starts_with("p03")))
    );
    let (_, whole) = asked(&path, "SELECT id FROM s.p WHERE u IS NOT NULL", Some("p_u"));
    assert!(r.blocks_read * 4 < whole.blocks_read, "{r:?}");
    // Counted.
    let (b, _) = fresh(&path)
        .ask("SELECT count(*) FROM s.p WHERE k LIKE 'p01%'", None)
        .unwrap();
    assert_eq!(
        one(&b),
        prefixed_kept(|i| k(i).starts_with("p01")).len() as i64
    );
    // A regular expression PostgreSQL's planner refuses as it reads its prefix is refused over
    // no rows too, in its words.
    let e = fresh(&path)
        .ask("SELECT id FROM s.p WHERE id < 0 AND k ~ '('", None)
        .unwrap_err();
    assert!(
        e.to_string()
            .contains("invalid regular expression: parentheses () not balanced"),
        "{e}"
    );
    // A row on which the pattern fails fails the question, as in PostgreSQL; a pattern ending in
    // its escape narrows to the string before it, which no row holds, as PostgreSQL's plan along
    // an index does.
    let (a, _) = asked(&path, "SELECT id FROM s.p WHERE k LIKE 'p\\'", Some("p_k"));
    assert!(a.is_empty());
    let e = fresh(&path)
        .ask("SELECT id FROM s.p WHERE k LIKE 'p0%\\'", Some("p_k"))
        .unwrap_err();
    assert!(
        e.to_string()
            .contains("LIKE pattern must not end with escape character"),
        "{e}"
    );
    // Held under the column's collation: under C, é is no letter.
    let (a, _) = asked(
        &path,
        "SELECT id FROM s.p WHERE u ~ '^p0[0-9]{3}-[0-9][[:alpha:]]'",
        Some("p_u"),
    );
    assert!(a.is_empty());
    let (a, _) = asked(
        &path,
        "SELECT id FROM s.p WHERE u COLLATE \"C.utf8\" ~ '^p0[0-9]{3}-[0-9][[:alpha:]]'",
        None,
    );
    assert_eq!(
        a,
        prefixed_kept(|i| u_of(i).is_some_and(|u| u.starts_with("p0")))
    );
    // A pattern the question path does not read, a column, is a pipeline's.
    let (b, _) = fresh(&path)
        .ask("SELECT count(*) FROM s.p WHERE k LIKE k", None)
        .unwrap();
    assert_eq!(one(&b), PREFIXED);
}

#[cfg(feature = "standalone")]
#[test]
fn a_bpchar_s_name_s_and_a_bytea_s_prefix_narrow_as_their_keys_are_held() {
    let rows: Vec<String> = (0..20_000)
        .map(|i| {
            format!(
                "({i}, 'q{:03}{}', 'q{:03}-{}', '\\x71{:06x}')",
                i % 500,
                if i % 3 == 0 { "" } else { " x" },
                i % 500,
                i % 4,
                i % 4096
            )
        })
        .collect();
    let insert = format!("INSERT INTO s.v VALUES {}", rows.join(", "));
    let mut s = snail_of(
        "qpath_prefix_kinds",
        "CREATE TABLE s.v (id integer NOT NULL, c char(6), nm name, b bytea, PRIMARY KEY (id));
         CREATE INDEX v_c ON s.v (c);
         CREATE INDEX v_nm ON s.v (nm);
         CREATE INDEX v_b ON s.v (b);",
        &[&insert],
    );
    let mut ask = |q: &str, order: &str| {
        let (b, r) = s.ask(q, Some(order)).unwrap();
        exact(q, &r);
        (ids(&b), r)
    };
    // A bpchar's padded text matches `^q012 ` where its value is 'q012', held without its blanks.
    let (a, r) = ask("SELECT id FROM s.v WHERE c ~ '^q012 '", "v_c");
    assert_eq!(
        a,
        (0..20_000).filter(|i| i % 500 == 12).collect::<Vec<i64>>()
    );
    let (_, whole) = ask("SELECT id FROM s.v WHERE c IS NOT NULL", "v_c");
    assert!(r.blocks_read < whole.blocks_read, "{r:?}");
    let (a, _) = ask("SELECT id FROM s.v WHERE c LIKE 'q01%'", "v_c");
    assert_eq!(
        a,
        (0..20_000)
            .filter(|i| (10..20).contains(&(i % 500)))
            .collect::<Vec<i64>>()
    );
    // A name, under C.
    let (a, r) = ask("SELECT id FROM s.v WHERE nm LIKE 'q012-%'", "v_nm");
    assert_eq!(
        a,
        (0..20_000).filter(|i| i % 500 == 12).collect::<Vec<i64>>()
    );
    let (_, whole) = ask("SELECT id FROM s.v WHERE nm IS NOT NULL", "v_nm");
    assert!(r.blocks_read < whole.blocks_read, "{r:?}");
    // A bytea's pattern is bytea: its bytes are the prefix.
    let (a, r) = ask("SELECT id FROM s.v WHERE b LIKE 'q\\000\\000%'", "v_b");
    assert_eq!(
        a,
        (0..20_000).filter(|i| i % 4096 < 256).collect::<Vec<i64>>()
    );
    let (_, whole) = ask("SELECT id FROM s.v WHERE b IS NOT NULL", "v_b");
    assert!(r.blocks_read < whole.blocks_read, "{r:?}");
}

/// Under a collation a library orders, the keys are the library's: a prefix narrows nothing, as
/// PostgreSQL's planner reads no range from one there, and an exact pattern narrows to its one key,
/// under a nondeterministic collation the key its class shares.
#[cfg(feature = "collations")]
#[test]
fn under_a_library_s_collation_only_an_exact_pattern_narrows_the_read() {
    let path = tmp("qpath_prefix_collated.snail");
    let catalog = Catalog::parse(
        "CREATE COLLATION s.ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);
         CREATE TABLE s.c (id integer NOT NULL, k text COLLATE \"en-US-x-icu\", ci text COLLATE s.ci,
         PRIMARY KEY (id));
         CREATE INDEX c_k ON s.c (k);
         CREATE INDEX c_ci ON s.c (ci);",
    )
    .unwrap();
    let ids: Vec<i64> = (0..PREFIXED).collect();
    let ci = |i: i64| {
        let k = k_of(i);
        if (i / 2000) % 2 == 0 {
            k.to_uppercase()
        } else {
            k
        }
    };
    let columns = vec![
        Values::Int(ids.iter().map(|&i| Some(i)).collect()),
        Values::Text(ids.iter().map(|&i| Some(k_of(i))).collect()),
        Values::Text(ids.iter().map(|&i| Some(ci(i))).collect()),
    ];
    write(&path, &catalog, &[("s.c".into(), Rows { columns })]).unwrap();
    let kept = |keep: &dyn Fn(i64) -> bool| -> Vec<i64> {
        ids.iter().copied().filter(|&i| keep(i)).collect()
    };
    let (a, r) = asked(
        &path,
        "SELECT id FROM s.c WHERE k LIKE 'p012%'",
        Some("c_k"),
    );
    assert_eq!(a, kept(&|i| k_of(i).starts_with("p012")));
    assert_eq!(r.entries_none, 0, "{r:?}");
    let (a, r) = asked(
        &path,
        "SELECT id FROM s.c WHERE k LIKE 'p0123-3'",
        Some("c_k"),
    );
    assert_eq!(a, kept(&|i| k_of(i) == "p0123-3"));
    assert!(r.blocks_read <= 1, "{r:?}");
    // Under the nondeterministic collation, every member of the class.
    let (a, r) = asked(
        &path,
        "SELECT id FROM s.c WHERE ci LIKE 'p0123-3'",
        Some("c_ci"),
    );
    assert_eq!(a, kept(&|i| k_of(i) == "p0123-3"));
    assert!(r.blocks_read <= 1, "{r:?}");
    let (a, r) = asked(
        &path,
        "SELECT id FROM s.c WHERE ci LIKE 'p012%'",
        Some("c_ci"),
    );
    assert_eq!(a, kept(&|i| k_of(i).starts_with("p012")));
    assert_eq!(r.entries_none, 0, "{r:?}");
}
