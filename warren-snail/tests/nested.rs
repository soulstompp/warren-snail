//! A WITH inside a step: a WITH query's own WITH, a subquery's in FROM, a set operation's arm's, a
//! recursive query's, a view's, each read as steps of the question's own. Each name reads the
//! nearest WITH that names it, as PostgreSQL's parser reads it: without RECURSIVE a query's name is
//! seen after it, under RECURSIVE everywhere in its WITH, a nearer WITH hiding a farther one or a
//! table. What PostgreSQL's parser refuses is refused in its words, each query by the name the
//! statement wrote. Every answer is PostgreSQL 18.6's over the same rows.

mod weather;

use arrow::array::{Array, RecordBatch};
use arrow::util::display::array_value_to_string;
use warren_snail::catalog::Catalog;
use warren_snail::values::Values;
use warren_snail::write::Rows;
use weather::{fresh, tmp, write};

const DDL: &str = "CREATE TABLE r.t (id integer NOT NULL, g integer, v text, PRIMARY KEY (id));
     CREATE TABLE r.u (id integer NOT NULL, t_id integer, amount integer, PRIMARY KEY (id));
     CREATE TABLE r.p (id integer NOT NULL, parent integer, PRIMARY KEY (id));
     CREATE TABLE items (id integer NOT NULL, name text, PRIMARY KEY (id));
     CREATE VIEW r.vw AS WITH c AS (SELECT id, g FROM r.t WHERE g > 1) SELECT id, g * 10 AS g10 FROM c;";

/// Rows grouped by g, one with g NULL; amounts on them, one on no row and one on a row missing; a
/// tree by its parent; a table in the default schema; and a view whose question has a WITH.
fn snail(name: &str) -> std::path::PathBuf {
    let path = tmp(&format!("nested-{name}.snail"));
    let int = |v: &[Option<i64>]| Values::Int(v.to_vec());
    let text = |v: &[Option<&str>]| Values::Text(v.iter().map(|s| s.map(str::to_string)).collect());
    let t = Rows {
        columns: vec![
            int(&[Some(1), Some(2), Some(3), Some(4), Some(5), Some(6)]),
            int(&[Some(1), Some(1), Some(2), Some(2), Some(3), None]),
            text(&[Some("a"), Some("b"), Some("c"), None, Some("e"), Some("f")]),
        ],
    };
    let u = Rows {
        columns: vec![
            int(&[
                Some(1),
                Some(2),
                Some(3),
                Some(4),
                Some(5),
                Some(6),
                Some(7),
                Some(8),
            ]),
            int(&[
                Some(1),
                Some(1),
                Some(2),
                Some(3),
                Some(3),
                Some(5),
                None,
                Some(9),
            ]),
            int(&[
                Some(10),
                Some(-3),
                Some(7),
                Some(0),
                Some(2),
                Some(100),
                Some(1),
                Some(4),
            ]),
        ],
    };
    let p = Rows {
        columns: vec![
            int(&[Some(1), Some(2), Some(3), Some(4), Some(5)]),
            int(&[None, Some(1), Some(1), Some(2), Some(4)]),
        ],
    };
    let items = Rows {
        columns: vec![int(&[Some(1), Some(2)]), text(&[Some("one"), Some("two")])],
    };
    let catalog = Catalog::parse(DDL).unwrap();
    write(
        &path,
        &catalog,
        &[
            ("r.t".into(), t),
            ("r.u".into(), u),
            ("r.p".into(), p),
            ("items".into(), items),
        ],
    )
    .unwrap();
    path
}

type Row = Vec<Option<String>>;

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
fn a_steps_own_with_is_read_as_steps_before_it() {
    let path = snail("steps");
    // A WITH query's own WITH, and one inside that.
    same(
        &path,
        "WITH a AS (WITH b AS (SELECT id, g FROM r.t WHERE g IS NOT NULL) \
         SELECT g, count(*) AS n FROM b GROUP BY g) SELECT g, n FROM a ORDER BY g",
        "1,2 2,2 3,1",
    );
    same(
        &path,
        "WITH a AS (WITH b AS (WITH c AS (SELECT id, g FROM r.t) \
         SELECT g, count(*) AS n FROM c GROUP BY g) SELECT max(n) AS most FROM b) SELECT most FROM a",
        "2",
    );
    // Its queries reading each other in order, and a query before the step.
    same(
        &path,
        "WITH a AS (WITH b AS (SELECT id, g FROM r.t), c AS (SELECT g, count(*) AS n FROM b GROUP BY g), \
         d AS (SELECT sum(n) AS total FROM c) SELECT total FROM d) SELECT total FROM a",
        "6",
    );
    same(
        &path,
        "WITH a AS (SELECT id, g FROM r.t WHERE id <= 4), b AS (WITH c AS (SELECT g, max(id) AS top \
         FROM a GROUP BY g) SELECT g, top FROM c) SELECT g, top FROM b ORDER BY g",
        "1,2 2,4",
    );
    // A subquery in FROM, at the top and inside a step, as sql-composer composes a fold.
    same(
        &path,
        "SELECT d.g, d.total FROM (WITH b AS (SELECT t_id, amount FROM r.u WHERE t_id IS NOT NULL) \
         SELECT t_id AS g, sum(amount) AS total FROM b GROUP BY t_id) d ORDER BY d.g",
        "1,7 2,7 3,2 5,100 9,4",
    );
    same(
        &path,
        "WITH placed AS (SELECT id, g, row_number() OVER (ORDER BY id) AS place FROM r.t), \
         summed AS (SELECT p.g, p.n, p.limit_n FROM (WITH bounds AS (SELECT 2 AS limit_n), \
         counted AS (SELECT g, count(*) AS n FROM placed GROUP BY g) SELECT c.g, c.n, b.limit_n \
         FROM counted c CROSS JOIN bounds b WHERE c.n <= b.limit_n) p) \
         SELECT g, n, limit_n FROM summed ORDER BY g NULLS FIRST",
        "-,1,2 1,2,2 2,2,2 3,1,2",
    );
    // A set operation's arm, a step that is a set operation, and one that is a VALUES list.
    same(
        &path,
        "SELECT id FROM r.t WHERE id <= 2 UNION ALL \
         (WITH b AS (SELECT id FROM r.t WHERE id >= 5) SELECT id FROM b) ORDER BY id",
        "1 2 5 6",
    );
    same(
        &path,
        "WITH a AS (WITH b AS (SELECT id FROM r.t) SELECT id FROM b WHERE id < 3 UNION \
         SELECT id FROM b WHERE id > 4) SELECT id FROM a ORDER BY id",
        "1 2 5 6",
    );
    same(
        &path,
        "WITH a AS (WITH b AS (SELECT 2 AS n) VALUES (1), (3)) SELECT column1 FROM a ORDER BY column1",
        "1 3",
    );
    // Read twice, its columns renamed, by a qualified star, sorted and limited.
    same(
        &path,
        "WITH a AS (WITH b AS (SELECT id, g FROM r.t WHERE g IS NOT NULL) SELECT x.id AS l, y.id AS r \
         FROM b x JOIN b y ON x.g = y.g AND x.id < y.id) SELECT l, r FROM a ORDER BY l, r",
        "1,2 3,4",
    );
    same(
        &path,
        "WITH a AS (WITH b(x, y) AS (SELECT id, g FROM r.t) SELECT b.x, b.y FROM b WHERE b.y = 2) \
         SELECT x, y FROM a ORDER BY x",
        "3,2 4,2",
    );
    same(
        &path,
        "WITH a AS (WITH b AS (SELECT id, v FROM r.t WHERE id < 3) SELECT b.*, c.* FROM b \
         CROSS JOIN (SELECT 9 AS nine) c) SELECT * FROM a ORDER BY id",
        "1,a,9 2,b,9",
    );
    same(
        &path,
        "WITH a AS (WITH b AS (SELECT id FROM r.t ORDER BY id DESC LIMIT 3) SELECT id FROM b \
         UNION ALL SELECT id FROM b WHERE id > 5 ORDER BY id LIMIT 3) SELECT id FROM a ORDER BY id",
        "4 5 6",
    );
}

#[test]
fn each_name_reads_the_nearest_with_that_names_it() {
    let path = snail("names");
    // A step's own query named as a query before the step, as the step itself, as a table.
    same(
        &path,
        "WITH a AS (SELECT 1 AS v), b AS (WITH a AS (SELECT 2 AS v) SELECT v FROM a) \
         SELECT a.v, b.v FROM a, b",
        "1,2",
    );
    same(
        &path,
        "WITH folded AS (WITH folded AS (SELECT g, sum(id) AS s FROM r.t WHERE g IS NOT NULL GROUP BY g) \
         SELECT f.g, f.s * 10 AS s10 FROM folded f) SELECT g, s10 FROM folded ORDER BY g",
        "1,30 2,70 3,50",
    );
    same(
        &path,
        "WITH s AS (WITH items AS (SELECT 7 AS id, 'seven' AS name) SELECT id, name FROM items) \
         SELECT id, name FROM s UNION ALL SELECT id, name FROM items ORDER BY id",
        "1,one 2,two 7,seven",
    );
    // Without RECURSIVE a query does not see itself, nor one after it: its text reads the outer.
    same(
        &path,
        "WITH a AS (SELECT 1 AS v), b AS (WITH a AS (SELECT v + 1 AS v FROM a) SELECT v FROM a) \
         SELECT v FROM b",
        "2",
    );
    same(
        &path,
        "WITH a AS (WITH b AS (SELECT 1 AS v) SELECT (WITH c AS (SELECT v FROM b), \
         b AS (SELECT 2 AS v) SELECT c.v * 10 + b.v FROM c, b) AS x) SELECT x FROM a",
        "12",
    );
    // Under RECURSIVE every name of its WITH is seen everywhere in it.
    same(
        &path,
        "WITH a AS (WITH b AS (SELECT 1 AS v) SELECT (WITH RECURSIVE c AS (SELECT v FROM b), \
         b AS (SELECT 2 AS v) SELECT c.v FROM c) AS x) SELECT x FROM a",
        "2",
    );
    // A subquery's own WITH hides the step's from the subquery alone.
    same(
        &path,
        "WITH a AS (WITH b AS (SELECT 1 AS v) SELECT (WITH b AS (SELECT 2 AS v) SELECT v FROM b) AS i, \
         b.v AS o FROM b) SELECT i, o FROM a",
        "2,1",
    );
    // A step's own recursive query named as the question's query before it.
    same(
        &path,
        "WITH t AS (SELECT 3 AS m), a AS (WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL \
         SELECT n + 1 FROM t WHERE n < 4) SELECT n FROM t) SELECT a.n, t.m FROM a CROSS JOIN t ORDER BY a.n",
        "1,3 2,3 3,3 4,3",
    );
}

#[test]
fn under_recursive_a_name_an_inner_with_takes_is_no_reference() {
    let path = snail("captured");
    // The two do not read each other, so neither is a recursion.
    same(
        &path,
        "WITH RECURSIVE a AS (SELECT v FROM b), b AS (WITH a AS (SELECT 1 AS v) SELECT v FROM a) \
         SELECT v FROM a",
        "1",
    );
    same(
        &path,
        "WITH RECURSIVE b AS (WITH b AS (SELECT 1 AS v) SELECT v FROM b) SELECT v FROM b",
        "1",
    );
    // Taken by a WITH inside a subquery of the query.
    same(
        &path,
        "WITH RECURSIVE a AS (SELECT v FROM (WITH a AS (SELECT 1 AS v) SELECT v FROM a) d) \
         SELECT v FROM a",
        "1",
    );
}

#[test]
fn a_recursive_querys_own_with_is_read_by_both_its_terms() {
    let path = snail("recursive");
    same(
        &path,
        "WITH RECURSIVE t(n) AS (WITH s AS (SELECT 4 AS m) SELECT 1 FROM s UNION ALL \
         SELECT t.n + 1 FROM t, s WHERE t.n < s.m) SELECT n FROM t ORDER BY n",
        "1 2 3 4",
    );
    // Written before the parentheses around its terms.
    same(
        &path,
        "WITH RECURSIVE t(n) AS (WITH s AS (SELECT 3 AS m) \
         (SELECT 1 UNION ALL SELECT t.n + 1 FROM t, s WHERE t.n < s.m)) SELECT n FROM t ORDER BY n",
        "1 2 3",
    );
    // Written inside the parentheses.
    same(
        &path,
        "WITH RECURSIVE t(n) AS ((WITH s AS (SELECT 2 AS m) \
         SELECT 1 UNION ALL SELECT t.n + 1 FROM t, s WHERE t.n < s.m)) SELECT n FROM t ORDER BY n",
        "1 2",
    );
    // A step's own recursion, walking up the tree.
    same(
        &path,
        "WITH a AS (WITH RECURSIVE up(id, parent) AS (SELECT id, parent FROM r.p WHERE id = 5 \
         UNION ALL SELECT p.id, p.parent FROM r.p p JOIN up ON p.id = up.parent) \
         SELECT count(*) AS depth, min(id) AS top FROM up) SELECT depth, top FROM a",
        "4,1",
    );
}

#[test]
fn a_views_question_with_a_with_of_its_own_is_read() {
    let path = snail("view");
    same(
        &path,
        "SELECT id, g10 FROM r.vw ORDER BY id",
        "3,20 4,20 5,30",
    );
    // The question's query of the same name is not the view's.
    same(
        &path,
        "WITH c AS (SELECT 100 AS id) SELECT v.id, v.g10, c.id FROM r.vw v CROSS JOIN c ORDER BY v.id",
        "3,20,100 4,20,100 5,30,100",
    );
}

#[test]
fn a_subquery_reads_its_steps_own_with_and_its_own_once_for_each_row() {
    let path = snail("subqueries");
    same(
        &path,
        "WITH a AS (WITH b AS (SELECT g, count(*) AS n FROM r.t GROUP BY g) \
         SELECT t.id, (SELECT n FROM b WHERE b.g = t.g) AS same_g FROM r.t t) \
         SELECT id, same_g FROM a ORDER BY id",
        "1,2 2,2 3,2 4,2 5,1 6,-",
    );
    same(
        &path,
        "SELECT t.id, (WITH c AS (WITH d AS (SELECT t.id * 3 AS x) SELECT x + 1 AS y FROM d) \
         SELECT y FROM c) AS y FROM r.t t ORDER BY t.id",
        "1,4 2,7 3,10 4,13 5,16 6,19",
    );
    same(
        &path,
        "WITH a AS (SELECT t.id, (WITH c AS (SELECT u.amount FROM r.u u WHERE u.t_id = t.id) \
         SELECT sum(amount) FROM c) AS total FROM r.t t) SELECT id, total FROM a ORDER BY id",
        "1,7 2,7 3,2 4,- 5,100 6,-",
    );
    same(
        &path,
        "SELECT t.id, l.x FROM r.t t, LATERAL (WITH c AS (SELECT t.id * 2 AS x) SELECT x FROM c) l \
         ORDER BY t.id",
        "1,2 2,4 3,6 4,8 5,10 6,12",
    );
}

#[test]
fn what_postgresql_refuses_of_a_with_inside_a_step_is_refused_in_its_words() {
    let path = snail("refused");
    let step = |w: &str| format!("WITH a AS ({w}) SELECT * FROM a");
    for (q, words) in [
        (
            step("WITH b AS (SELECT n FROM c), c AS (SELECT 1 AS n) SELECT n FROM b"),
            "relation \"c\" does not exist",
        ),
        (
            step("WITH b AS (SELECT 1 AS n), b AS (SELECT 2 AS n) SELECT n FROM b"),
            "WITH query name \"b\" specified more than once",
        ),
        (
            step("WITH d AS (DELETE FROM r.u WHERE id = 1 RETURNING id) SELECT id FROM d"),
            "WITH clause containing a data-modifying statement must be at the top level",
        ),
        (
            "WITH a AS (SELECT 1 AS x) (WITH b AS (SELECT 2 AS y) SELECT x, y FROM a, b)".into(),
            "multiple WITH clauses not allowed",
        ),
        (
            step("WITH b(x, y, z) AS (SELECT 1, 2) SELECT * FROM b"),
            "WITH query \"b\" has 2 columns available but 3 columns specified",
        ),
        (
            "WITH RECURSIVE t(n) AS (WITH s AS (SELECT n FROM t) SELECT 1 UNION ALL \
             SELECT n + 1 FROM t WHERE n < 3) SELECT n FROM t"
                .into(),
            "recursive reference to query \"t\" must not appear within a subquery",
        ),
        (
            "WITH RECURSIVE t(n) AS (WITH d AS (DELETE FROM r.u WHERE id = 1 RETURNING id) \
             SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 3) SELECT n FROM t"
                .into(),
            "WITH clause containing a data-modifying statement must be at the top level",
        ),
        // A step's own recursion refused by the name the statement wrote it by.
        (
            step("WITH RECURSIVE t(n) AS (SELECT n FROM t UNION ALL SELECT 1) SELECT n FROM t"),
            "recursive reference to query \"t\" must not appear within its non-recursive term",
        ),
        (
            step("WITH RECURSIVE t(n) AS (SELECT 1 INTERSECT SELECT n + 1 FROM t) SELECT n FROM t"),
            "recursive query \"t\" does not have the form non-recursive-term UNION [ALL] recursive-term",
        ),
        (
            step(
                "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT x.n + 1 FROM t x, t y \
                 WHERE x.n < 3) SELECT n FROM t",
            ),
            "recursive reference to query \"t\" must not appear more than once",
        ),
        (
            step(
                "WITH RECURSIVE t(n) AS (SELECT 1::integer UNION ALL SELECT (n + 1)::bigint FROM t \
                 WHERE n < 3) SELECT n FROM t",
            ),
            "recursive query \"t\" column 1 has type integer in non-recursive term but type bigint \
             overall",
        ),
        (
            "SELECT * FROM (WITH b AS (SELECT 1 AS n) SELECT * FROM b TABLESAMPLE SYSTEM (50)) d"
                .into(),
            "TABLESAMPLE clause can only be applied to tables and materialized views",
        ),
        (
            step("WITH b AS (SELECT 1 AS n) SELECT b.zz FROM b"),
            "column b.zz does not exist",
        ),
    ] {
        let got = refused(&path, &q);
        assert!(got.contains(words), "{q}: {got}");
        assert!(!got.contains('\0') && !got.contains('\u{4}'), "{q}: {got:?}");
    }
}
