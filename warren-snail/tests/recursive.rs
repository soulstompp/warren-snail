//! WITH RECURSIVE: a query reading itself, made round by round, UNION stopping on repeats and
//! UNION ALL running until a round adds nothing; walks up a parent column, a closure, a path carried
//! as an array and a carry a rollback undoes; recursive and non-recursive queries side by side, in
//! the order they read each other; each column typed by the non-recursive term; and what
//! PostgreSQL's parser refuses, refused in its words. Every answer is PostgreSQL 19's and 18's over
//! the same rows.

mod weather;

use arrow::array::{Array, RecordBatch};
use arrow::util::display::array_value_to_string;
use warren_snail::catalog::Catalog;
use warren_snail::values::Values;
use warren_snail::write::Rows;
use weather::{fresh, tmp, write};

const DDL: &str =
    "CREATE TABLE a.p (id integer NOT NULL, parent integer, name text, PRIMARY KEY (id));
     CREATE TABLE a.e (src integer NOT NULL, dst integer NOT NULL, PRIMARY KEY (src, dst));
     CREATE TABLE a.two (id integer NOT NULL, PRIMARY KEY (id));
     CREATE TABLE a.s (id integer NOT NULL, kind text NOT NULL, setting text, PRIMARY KEY (id));";

/// The tables' snail: a tree by `parent`, one row its own parent; edges holding a ring and a
/// chain; two rows; a session's entries, a setting set inside a transaction rolled back.
fn snail(name: &str) -> std::path::PathBuf {
    let path = tmp(&format!("recursive-{name}.snail"));
    let int = |v: &[Option<i64>]| Values::Int(v.to_vec());
    let text = |v: &[Option<&str>]| Values::Text(v.iter().map(|s| s.map(str::to_string)).collect());
    let p = Rows {
        columns: vec![
            int(&[Some(1), Some(2), Some(3), Some(4), Some(5), Some(6)]),
            int(&[None, Some(1), Some(1), Some(2), Some(4), Some(6)]),
            text(&[
                Some("root"),
                Some("a"),
                Some("b"),
                Some("c"),
                Some("d"),
                Some("self"),
            ]),
        ],
    };
    let e = Rows {
        columns: vec![
            int(&[Some(1), Some(2), Some(3), Some(4)]),
            int(&[Some(2), Some(3), Some(1), Some(5)]),
        ],
    };
    let two = Rows {
        columns: vec![int(&[Some(1), Some(2)])],
    };
    let s = Rows {
        columns: vec![
            int(&[Some(1), Some(2), Some(3), Some(4), Some(5)]),
            text(&[
                Some("set"),
                Some("begin"),
                Some("set"),
                Some("rollback"),
                Some("set"),
            ]),
            text(&[Some("x"), None, Some("y"), None, Some("z")]),
        ],
    };
    let catalog = Catalog::parse(DDL).unwrap();
    write(
        &path,
        &catalog,
        &[
            ("a.p".into(), p),
            ("a.e".into(), e),
            ("a.two".into(), two),
            ("a.s".into(), s),
        ],
    )
    .unwrap();
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

const WALK: &str = "WITH RECURSIVE up(id, parent) AS (SELECT id, parent FROM a.p WHERE id = {at} \
     {union} SELECT p.id, p.parent FROM a.p p JOIN up ON p.id = up.parent{also}) \
     SELECT id, parent FROM up";

fn walk(at: u32, union: &str, also: &str) -> String {
    WALK.replace("{at}", &at.to_string())
        .replace("{union}", union)
        .replace("{also}", also)
}

#[test]
fn a_walk_up_a_parent_column_round_by_round() {
    let path = snail("walk");
    // Each round's rows after the round before's, the first the non-recursive term's.
    same(&path, &walk(5, "UNION", ""), "5,4 4,2 2,1 1,-");
    same(&path, &walk(5, "UNION ALL", ""), "5,4 4,2 2,1 1,-");
    // A row its own parent: UNION stops on the repeat.
    same(&path, &walk(6, "UNION", ""), "6,6");
    same(&path, &walk(6, "UNION ALL", " AND p.id <> p.parent"), "6,6");
    same(&path, &walk(99, "UNION ALL", ""), "");
    // The step joined by a comma, and the last SELECT reading the base table beside the walk.
    same(
        &path,
        "WITH RECURSIVE up(id, parent) AS (SELECT id, parent FROM a.p WHERE id = 5 UNION \
         SELECT p.id, p.parent FROM up, a.p p WHERE p.id = up.parent) \
         SELECT p.name FROM up, a.p p WHERE p.id = up.id ORDER BY p.name",
        "a c d root",
    );
}

#[test]
fn a_closure_and_a_path_carried_as_a_stack() {
    let path = snail("closure");
    same(
        &path,
        "WITH RECURSIVE reach(src, dst) AS (SELECT src, dst FROM a.e UNION \
         SELECT w.src, e.dst FROM reach w JOIN a.e e ON e.src = w.dst) \
         SELECT src, dst FROM reach ORDER BY src, dst",
        "1,1 1,2 1,3 2,1 2,2 2,3 3,1 3,2 3,3 4,5",
    );
    same(
        &path,
        "WITH RECURSIVE reach(src, dst, path) AS (SELECT src, dst, ARRAY[src, dst] FROM a.e \
         UNION ALL SELECT w.src, e.dst, w.path || e.dst FROM reach w JOIN a.e e ON e.src = w.dst \
         WHERE NOT e.dst = ANY(w.path)) \
         SELECT src, dst, array_to_string(path, '/') FROM reach ORDER BY src, dst",
        "1,2,1/2 1,3,1/2/3 2,1,2/3/1 2,3,2/3 3,1,3/1 3,2,3/1/2 4,5,4/5",
    );
    // A carry a rollback undoes: the stack its transaction began on given back.
    same(
        &path,
        "WITH RECURSIVE carry(id, stack, saved) AS (SELECT id, ARRAY[setting], NULL::text[] \
         FROM a.s WHERE id = 1 UNION ALL SELECT s.id, CASE WHEN s.kind = 'set' THEN \
         c.stack || s.setting WHEN s.kind = 'rollback' THEN c.saved ELSE c.stack END, CASE WHEN \
         s.kind = 'begin' THEN c.stack WHEN s.kind = 'set' THEN c.saved END FROM carry c JOIN a.s s \
         ON s.id = c.id + 1) SELECT id, array_to_string(stack, '/') FROM carry ORDER BY id",
        "1,x 2,x 3,x/y 4,x 5,x/z",
    );
}

#[test]
fn union_stops_on_repeats_and_union_all_when_a_round_adds_nothing() {
    let path = snail("rounds");
    same(
        &path,
        "WITH RECURSIVE t(n) AS (SELECT 1 UNION SELECT n % 3 + 1 FROM t) SELECT n FROM t",
        "1 2 3",
    );
    same(
        &path,
        "WITH RECURSIVE t(n) AS (SELECT 0 UNION ALL SELECT t.n + 1 FROM t, a.two WHERE t.n < 3) \
         SELECT n, count(*) FROM t GROUP BY n ORDER BY n",
        "0,1 1,2 2,4 3,8",
    );
    // The non-recursive term's own repeats are made once.
    same(
        &path,
        "WITH RECURSIVE t(n) AS (SELECT 1 FROM a.two UNION SELECT n FROM t) SELECT n FROM t",
        "1",
    );
    // NULLs are not distinct.
    same(
        &path,
        "WITH RECURSIVE t(n, s) AS (SELECT NULL::integer, 'a' UNION SELECT n, s FROM t) \
         SELECT n, s FROM t",
        "-,a",
    );
    // A recursive term in parentheses is sorted and limited in each round, and met by its own
    // set operation.
    same(
        &path,
        "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL (SELECT e.dst FROM t JOIN a.e e ON \
         e.src = t.n WHERE e.dst > t.n ORDER BY e.dst LIMIT 1)) SELECT n FROM t",
        "1 2 3",
    );
    same(
        &path,
        "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL (SELECT n + 1 FROM t WHERE n < 6 EXCEPT \
         SELECT 3)) SELECT n FROM t",
        "1 2",
    );
}

#[test]
fn several_queries_made_in_the_order_they_read_each_other() {
    let path = snail("several");
    same(
        &path,
        "WITH RECURSIVE a(n) AS (SELECT n * 10 FROM b), b(n) AS (SELECT 1 UNION ALL \
         SELECT n + 1 FROM b WHERE n < 3) SELECT n FROM a ORDER BY n",
        "10 20 30",
    );
    same(
        &path,
        "WITH RECURSIVE kids(id, parent) AS (SELECT id, parent FROM a.p WHERE parent <> id), \
         down(id, depth) AS (SELECT 1, 0 UNION ALL SELECT k.id, d.depth + 1 FROM kids k JOIN \
         down d ON k.parent = d.id) SELECT id, depth FROM down ORDER BY id",
        "1,0 2,1 3,1 4,2 5,3",
    );
    let e = refused(
        &path,
        "WITH RECURSIVE a(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM b WHERE n < 3), \
         b(n) AS (SELECT n FROM a) SELECT n FROM a",
    );
    assert_eq!(e, "mutual recursion between WITH items is not implemented");
}

#[test]
fn columns_typed_by_the_non_recursive_term() {
    let path = snail("typed");
    same(
        &path,
        "WITH RECURSIVE t(n) AS (SELECT 4000000000::bigint UNION ALL SELECT n + 1 FROM t \
         WHERE n < 4000000002) SELECT n FROM t",
        "4000000000 4000000001 4000000002",
    );
    // The recursive term's values assigned to the column's type.
    same(
        &path,
        "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT '2' FROM t WHERE n < 2) \
         SELECT n + 1 FROM t",
        "2 3",
    );
    // An untyped literal is text.
    same(
        &path,
        "WITH RECURSIVE t(x) AS (SELECT 'a' UNION ALL SELECT x || 'b' FROM t WHERE length(x) < 3) \
         SELECT x FROM t",
        "a ab abb",
    );
    // `||` and upper are text, which a column of no length takes and text itself is.
    for q in [
        "WITH RECURSIVE t(x) AS (SELECT 'a'::varchar UNION ALL SELECT x || 'b' FROM t WHERE length(x) < 3) \
         SELECT x FROM t",
        "WITH RECURSIVE t(x) AS (SELECT 'a'::name UNION ALL SELECT x || 'b' FROM t WHERE length(x) < 3) \
         SELECT x FROM t",
        "WITH RECURSIVE t(x) AS (SELECT 'a'::text UNION ALL SELECT x || 'b' FROM t WHERE length(x) < 3) \
         SELECT x FROM t",
        "WITH RECURSIVE t(x) AS (SELECT 'a'::varchar(3) UNION ALL SELECT (x || 'b')::varchar(3) FROM t \
         WHERE length(x) < 3) SELECT x FROM t",
    ] {
        same(&path, q, "a ab abb");
    }
    same(
        &path,
        "WITH RECURSIVE t(x) AS (SELECT 'a'::text UNION ALL SELECT upper(x) || 'b' FROM t WHERE length(x) < 3) \
         SELECT x FROM t",
        "a Ab ABb",
    );
    same(
        &path,
        "WITH RECURSIVE t(x) AS (SELECT 'a' UNION ALL SELECT x || 1 FROM t WHERE length(x) < 3) \
         SELECT x FROM t",
        "a a1 a11",
    );
    for (q, words) in [
        (
            "WITH RECURSIVE t(x) AS (SELECT 'a'::varchar(3) UNION ALL SELECT x || 'b' FROM t \
             WHERE length(x) < 3) SELECT x FROM t",
            "recursive query \"t\" column 1 has type character varying(3) in non-recursive term \
             but type character varying overall",
        ),
        (
            "WITH RECURSIVE t(x) AS (SELECT 'a'::varchar(3) UNION ALL SELECT upper(x) FROM t \
             WHERE length(x) < 1) SELECT x FROM t",
            "recursive query \"t\" column 1 has type character varying(3) in non-recursive term \
             but type character varying overall",
        ),
        (
            "WITH RECURSIVE t(x) AS (SELECT 'a'::varchar(3) UNION ALL SELECT lower(x) FROM t \
             WHERE false) SELECT x FROM t",
            "recursive query \"t\" column 1 has type character varying(3) in non-recursive term \
             but type character varying overall",
        ),
        (
            "WITH RECURSIVE t(x) AS (SELECT 'a'::char(3) UNION ALL SELECT x || 'b' FROM t \
             WHERE length(x) < 3) SELECT x FROM t",
            "recursive query \"t\" column 1 has type character(3) in non-recursive term but \
             type bpchar overall",
        ),
        (
            "WITH RECURSIVE t(x) AS (SELECT 1 UNION ALL SELECT x || 'b' FROM t WHERE x < 3) \
             SELECT x FROM t",
            "UNION types integer and text cannot be matched",
        ),
        (
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1.5 FROM t WHERE n < 3) \
             SELECT n FROM t",
            "recursive query \"t\" column 1 has type integer in non-recursive term but type \
             numeric overall",
        ),
        (
            "WITH RECURSIVE t(n) AS (SELECT NULL UNION ALL SELECT 1 FROM t WHERE n IS NULL) \
             SELECT n FROM t",
            "recursive query \"t\" column 1 has type text in non-recursive term but type integer \
             overall",
        ),
        (
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1, 2 FROM t WHERE n < 3) \
             SELECT n FROM t",
            "each UNION query must have the same number of columns",
        ),
        (
            "WITH RECURSIVE t(n, m) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 3) \
             SELECT n FROM t",
            "WITH query \"t\" has 1 columns available but 2 columns specified",
        ),
        (
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT 'x' FROM t WHERE n > 3) \
             SELECT n FROM t",
            "invalid input syntax for type integer: \"x\"",
        ),
    ] {
        assert_eq!(refused(&path, q), words, "{q}");
    }
}

#[test]
fn what_postgresql_refuses_in_a_recursion_is_refused_in_its_words() {
    let path = snail("refused");
    let term = |t: &str| format!("WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL {t}) SELECT n FROM t");
    for (q, words) in [
        (
            term("SELECT max(n) + 1 FROM t WHERE n < 3 GROUP BY n"),
            "aggregate functions are not allowed in a recursive query's recursive term",
        ),
        (
            term("SELECT n + 1 FROM t WHERE n < 3 ORDER BY n"),
            "ORDER BY in a recursive query is not implemented",
        ),
        (
            term("SELECT n + 1 FROM t WHERE n < 3 LIMIT 2"),
            "LIMIT in a recursive query is not implemented",
        ),
        (
            term("SELECT n + 1 FROM t WHERE n < 3 OFFSET 1"),
            "OFFSET in a recursive query is not implemented",
        ),
        (
            term("SELECT x.n + 1 FROM t x, t y WHERE x.n < 3"),
            "recursive reference to query \"t\" must not appear more than once",
        ),
        (
            term("SELECT p.id FROM a.p p LEFT JOIN t ON t.n = p.parent"),
            "recursive reference to query \"t\" must not appear within an outer join",
        ),
        (
            term("SELECT p.id FROM t RIGHT JOIN a.p p ON t.n = p.parent"),
            "recursive reference to query \"t\" must not appear within an outer join",
        ),
        (
            term("SELECT p.id FROM t FULL JOIN a.p p ON t.n = p.parent"),
            "recursive reference to query \"t\" must not appear within an outer join",
        ),
        (
            term("SELECT id FROM a.p WHERE parent IN (SELECT n FROM t)"),
            "recursive reference to query \"t\" must not appear within a subquery",
        ),
        (
            term("(SELECT 5 EXCEPT SELECT n FROM t)"),
            "recursive reference to query \"t\" must not appear within EXCEPT",
        ),
        (
            "WITH RECURSIVE t(m) AS (SELECT 1::money UNION SELECT m FROM t) SELECT m FROM t".into(),
            "could not implement recursive UNION\nDETAIL:  All column datatypes must be hashable.",
        ),
        (
            "WITH RECURSIVE t(n) AS (SELECT n FROM t UNION ALL SELECT 1) SELECT n FROM t".into(),
            "recursive reference to query \"t\" must not appear within its non-recursive term",
        ),
        (
            "WITH RECURSIVE t(n) AS (SELECT 1 INTERSECT SELECT n FROM t) SELECT n FROM t".into(),
            "recursive query \"t\" does not have the form non-recursive-term UNION [ALL] \
             recursive-term",
        ),
    ] {
        assert_eq!(refused(&path, &q), words, "{q}");
    }
}

#[test]
fn search_cycle_and_an_endless_recursion_refused_by_name() {
    let path = snail("unread");
    let e = refused(
        &path,
        "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 3) \
         SEARCH DEPTH FIRST BY n SET ordering SELECT n FROM t",
    );
    assert!(e.contains("SEARCH clause is not read"), "{e}");
    let e = refused(
        &path,
        "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 3) \
         CYCLE n SET looped USING path SELECT n FROM t",
    );
    assert!(e.contains("CYCLE clause is not read"), "{e}");
    // A derived table named search is no clause.
    same(
        &path,
        "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 2) \
         SELECT search.n FROM (SELECT n FROM t) search ORDER BY n",
        "1 2",
    );
    let e = refused(
        &path,
        "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT t.n + 1 FROM t, a.two) SELECT n FROM t",
    );
    assert!(e.contains("made more than 1048576 rows"), "{e}");
}
