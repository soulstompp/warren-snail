//! Subqueries: scalar, EXISTS, IN, ANY and ALL, uncorrelated and correlated, in the select list,
//! WHERE, HAVING and ORDER BY, and LATERAL in FROM. One that reads nothing of the query around it is
//! a step run once; one that reads it is met with each row by the =s that tie them, its tables read
//! from the file once, or read a row at a time where nothing ties them. Every answer is PostgreSQL
//! 19's and 18's over the same rows, NOT IN beside a NULL and ALL over no rows among them, and every
//! refusal is in PostgreSQL's words.

mod weather;

use arrow::array::{Array, RecordBatch};
use arrow::util::display::array_value_to_string;
use warren_snail::catalog::Catalog;
use warren_snail::read::Round;
use warren_snail::values::Values;
use warren_snail::write::Rows;
use weather::{fresh, tmp, write};

const T: &str = "CREATE TABLE a.t (id integer NOT NULL, v integer, g integer, PRIMARY KEY (id));";
const U: &str =
    "CREATE TABLE a.u (id integer NOT NULL, t_id integer, w integer, s text, PRIMARY KEY (id));";

/// The tables' rows: `a.t` four, one `v` NULL; `a.u` four, one `w` NULL and one `s` NULL, one
/// naming no row of `a.t`.
fn tables() -> (Rows, Rows) {
    let int = |v: &[Option<i64>]| Values::Int(v.to_vec());
    let text = |v: &[Option<&str>]| Values::Text(v.iter().map(|s| s.map(str::to_string)).collect());
    let t = Rows {
        columns: vec![
            int(&[Some(1), Some(2), Some(3), Some(4)]),
            int(&[Some(10), None, Some(30), Some(5)]),
            int(&[Some(1), Some(1), Some(2), Some(2)]),
        ],
    };
    let u = Rows {
        columns: vec![
            int(&[Some(1), Some(2), Some(3), Some(4)]),
            int(&[Some(1), Some(1), Some(3), Some(9)]),
            int(&[Some(5), None, Some(7), Some(40)]),
            text(&[Some("x"), Some("y"), None, Some("x")]),
        ],
    };
    (t, u)
}

fn snail(name: &str) -> std::path::PathBuf {
    let path = tmp(&format!("subqueries-{name}.snail"));
    let catalog = Catalog::parse(&format!("CREATE SCHEMA a;\n{T}\n{U}")).unwrap();
    let (t, u) = tables();
    write(&path, &catalog, &[("a.t".into(), t), ("a.u".into(), u)]).unwrap();
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

/// Rows written as text, a field of `-` NULL.
fn want(rows: &[&str]) -> Vec<Row> {
    rows.iter()
        .map(|r| {
            r.split(',')
                .map(|v| (v != "-").then(|| v.to_string()))
                .collect()
        })
        .collect()
}

/// A question's answer, its rows those written; every table's read its predicted pages.
fn same(path: &std::path::Path, q: &str, expected: &[&str]) -> Round {
    let (b, round) = fresh(path)
        .ask(q, None)
        .unwrap_or_else(|e| panic!("{q}: {e}"));
    assert_eq!(rows(&b), want(expected), "{q}");
    assert_eq!(
        round.column_pages_read,
        round.column_pages_predicted - round.column_pages_held,
        "{q}: {round:?}"
    );
    round
}

fn refused(path: &std::path::Path, q: &str) -> String {
    match fresh(path).ask(q, None) {
        Ok((b, _)) => panic!("{q}: answered {:?}", rows(&b)),
        Err(e) => e.to_string(),
    }
}

#[test]
fn a_subquery_reading_nothing_around_it_is_a_step_run_once() {
    let path = snail("once");
    let round = same(
        &path,
        "SELECT t.id, (SELECT max(u.w) FROM a.u u) AS m FROM a.t t ORDER BY t.id",
        &["1,40", "2,40", "3,40", "4,40"],
    );
    // The subquery's table read once, after the table around it.
    assert_eq!(round.order, "t_pkey+u_pkey");
    // A recursion inside a subquery, as PostgreSQL 18.6 answers it over these rows.
    same(
        &path,
        "SELECT t.id FROM a.t t WHERE t.id IN \
         (WITH RECURSIVE r (n) AS (SELECT 1) SELECT r.n FROM r)",
        &["1"],
    );
    same(
        &path,
        "SELECT t.id FROM a.t t WHERE t.id IN (SELECT u.t_id FROM a.u u) ORDER BY t.id",
        &["1", "3"],
    );
    same(
        &path,
        "SELECT t.id FROM a.t t WHERE t.v > (SELECT avg(u.w) FROM a.u u WHERE u.s = 'x') \
         ORDER BY 1",
        &["3"],
    );
    same(
        &path,
        "SELECT t.id FROM a.t t WHERE t.v = ANY (SELECT u.w * 2 FROM a.u u) ORDER BY 1",
        &["1"],
    );
    same(
        &path,
        "SELECT t.id FROM a.t t WHERE t.id = (SELECT min(u.t_id) FROM a.u u) ORDER BY 1",
        &["1"],
    );
    // A WITH query of the question around it, and one of its own.
    same(
        &path,
        "WITH c AS (SELECT u.t_id FROM a.u u WHERE u.w > 4) \
         SELECT t.id FROM a.t t WHERE t.id IN (SELECT c.t_id FROM c) ORDER BY 1",
        &["1", "3"],
    );
    same(
        &path,
        "SELECT t.id FROM a.t t WHERE t.id IN \
         (WITH c AS (SELECT u.t_id FROM a.u u) SELECT c.t_id FROM c) ORDER BY 1",
        &["1", "3"],
    );
}

#[test]
fn not_in_and_all_answer_as_postgresql_does_beside_a_null_and_over_no_rows() {
    let path = snail("nulls");
    same(
        &path,
        "SELECT t.id FROM a.t t WHERE t.id NOT IN (SELECT u.t_id FROM a.u u) ORDER BY t.id",
        &["2", "4"],
    );
    // A NULL among the subquery's values leaves NOT IN true of no row.
    same(
        &path,
        "SELECT t.id FROM a.t t WHERE t.v NOT IN (SELECT u.w FROM a.u u) ORDER BY t.id",
        &[],
    );
    // A NULL beside it leaves NOT IN NULL where the subquery returns rows.
    same(
        &path,
        "SELECT t.id FROM a.t t WHERE t.v NOT IN (SELECT u.w FROM a.u u WHERE u.w IS NOT NULL) \
         ORDER BY t.id",
        &["1", "3"],
    );
    same(
        &path,
        "SELECT t.id FROM a.t t WHERE t.v <> ALL (SELECT u.w FROM a.u u WHERE u.w IS NOT NULL) \
         ORDER BY 1",
        &["1", "3"],
    );
    // ALL over no rows is true, IN over none false, a NULL beside either.
    same(
        &path,
        "SELECT t.id, t.v > ALL (SELECT u.w FROM a.u u WHERE false) AS a, \
         t.v IN (SELECT u.w FROM a.u u WHERE false) AS b FROM a.t t ORDER BY t.id",
        &[
            "1,true,false",
            "2,true,false",
            "3,true,false",
            "4,true,false",
        ],
    );
    same(
        &path,
        "SELECT t.id, t.v > ALL (SELECT u.w FROM a.u u WHERE u.t_id = t.id) AS a, \
         t.v < ANY (SELECT u.w FROM a.u u) AS b FROM a.t t ORDER BY t.id",
        &["1,-,true", "2,true,-", "3,true,true", "4,true,true"],
    );
}

#[test]
fn a_correlated_subquery_meets_each_row_by_the_equalities_that_tie_them() {
    let path = snail("tied");
    let round = same(
        &path,
        "SELECT t.id FROM a.t t WHERE EXISTS (SELECT 1 FROM a.u u WHERE u.t_id = t.id) \
         ORDER BY t.id",
        &["1", "3"],
    );
    // Each table read from the file once, whatever rows ask the subquery.
    assert_eq!(round.order, "t_pkey+u_pkey");
    same(
        &path,
        "SELECT t.id FROM a.t t WHERE NOT EXISTS (SELECT 1 FROM a.u u WHERE u.t_id = t.id) \
         ORDER BY t.id",
        &["2", "4"],
    );
    // An aggregate over no rows of its own: count 0, max NULL, the column around it as it is.
    same(
        &path,
        "SELECT t.id, (SELECT count(*) FROM a.u u WHERE u.t_id = t.id) AS n, \
         (SELECT max(u.w) FROM a.u u WHERE u.t_id = t.id) AS m, \
         (SELECT coalesce(max(u.w), t.v) FROM a.u u WHERE u.t_id = t.id) AS c \
         FROM a.t t ORDER BY t.id",
        &["1,2,5,5", "2,0,-,-", "3,1,7,7", "4,0,-,5"],
    );
    same(
        &path,
        "SELECT t.id, (SELECT u.s FROM a.u u WHERE u.t_id = t.id ORDER BY u.id DESC LIMIT 1) AS s \
         FROM a.t t ORDER BY 1",
        &["1,y", "2,-", "3,-", "4,-"],
    );
    same(
        &path,
        "SELECT t.id, EXISTS (SELECT 1 FROM a.u u WHERE u.t_id = t.id) AS has, \
         CASE WHEN EXISTS (SELECT FROM a.u u WHERE u.t_id = t.id AND u.w IS NULL) THEN 1 ELSE 0 \
         END AS null_w FROM a.t t ORDER BY 1",
        &["1,true,1", "2,false,0", "3,true,0", "4,false,0"],
    );
    same(
        &path,
        "SELECT t.id FROM a.t t ORDER BY (SELECT count(*) FROM a.u u WHERE u.t_id = t.id) DESC, t.id",
        &["1", "3", "2", "4"],
    );
}

#[test]
fn a_correlated_subquery_with_nothing_tying_it_is_read_a_row_at_a_time() {
    let path = snail("untied");
    same(
        &path,
        "SELECT t.id FROM a.t t WHERE EXISTS (SELECT 1 FROM a.u u WHERE u.w < t.v) ORDER BY t.id",
        &["1", "3"],
    );
    same(
        &path,
        "SELECT t.id FROM a.t t WHERE NOT EXISTS \
         (SELECT 1 FROM a.u u WHERE u.t_id = t.id AND u.w > t.v) AND t.v IS NOT NULL ORDER BY 1",
        &["1", "3", "4"],
    );
    // A subquery inside one, reading the query two out.
    same(
        &path,
        "SELECT t.id FROM a.t t WHERE t.id IN (SELECT u.t_id FROM a.u u WHERE u.s IN \
         (SELECT x.s FROM a.u x WHERE x.id > t.id)) ORDER BY 1",
        &["1"],
    );
    // A derived table of a subquery reading the query around it.
    same(
        &path,
        "SELECT t.id FROM a.t t WHERE 1 < (SELECT count(*) FROM \
         (SELECT u.id FROM a.u u WHERE u.t_id = t.id) d) ORDER BY 1",
        &["1"],
    );
    // A FROM of a table beside a VALUES list made for each row, its table read from the file once.
    let round = same(
        &path,
        "SELECT t.id FROM a.t t WHERE EXISTS \
         (SELECT 1 FROM a.u u, (VALUES (1)) v (x) WHERE u.t_id = t.id AND v.x = 1) ORDER BY 1",
        &["1", "3"],
    );
    assert_eq!(round.order, "t_pkey+u_pkey");
}

#[test]
fn a_bare_name_in_a_subquery_is_its_own_before_the_query_around_it() {
    let path = snail("names");
    same(
        &path,
        "SELECT t.id, (SELECT count(*) FROM a.u u WHERE id > 2) AS n FROM a.t t ORDER BY 1",
        &["1,2", "2,2", "3,2", "4,2"],
    );
}

#[test]
fn a_condition_reading_no_column_is_read_once_before_any_row() {
    let path = snail("once-before");
    // PostgreSQL's one-time filter: refused though no row of the FROM reaches it.
    assert_eq!(
        refused(
            &path,
            "SELECT t.id FROM a.t t WHERE t.id < 0 AND (SELECT u.w FROM a.u u) > 0"
        ),
        "more than one row returned by a subquery used as an expression"
    );
    // Where it does not hold, the FROM is not read.
    let round = same(
        &path,
        "SELECT t.id FROM a.t t WHERE (SELECT count(*) FROM a.u u) > 9",
        &[],
    );
    assert_eq!(round.order, "u_pkey");
}

#[test]
fn rows_compared_with_a_subquery_field_by_field() {
    let path = snail("rows");
    same(
        &path,
        "SELECT t.id FROM a.t t WHERE (t.id, t.v) IN (SELECT u.t_id, u.w + 5 FROM a.u u) ORDER BY 1",
        &["1"],
    );
    same(
        &path,
        "SELECT t.id FROM a.t t WHERE (t.id, t.v) NOT IN (SELECT u.t_id, u.w FROM a.u u) ORDER BY 1",
        &["2", "3", "4"],
    );
}

#[test]
fn a_subquery_read_on_each_group_reads_only_what_is_one_value_for_it() {
    let path = snail("groups");
    same(
        &path,
        "SELECT t.g, (SELECT count(*) FROM a.u u WHERE u.w > t.g) AS n FROM a.t t GROUP BY t.g \
         ORDER BY 1",
        &["1,3", "2,3"],
    );
    // The primary key grouped decides every column of its table.
    same(
        &path,
        "SELECT t.v, (SELECT count(*) FROM a.u u WHERE u.w > t.v) AS n FROM a.t t GROUP BY t.id \
         ORDER BY 1",
        &["5,2", "10,1", "30,1", "-,0"],
    );
    same(
        &path,
        "SELECT t.g FROM a.t t GROUP BY t.g HAVING max(t.v) > ALL \
         (SELECT u.w FROM a.u u WHERE u.t_id = t.g) ORDER BY 1",
        &["2"],
    );
    same(
        &path,
        "SELECT t.g, count(*) AS n FROM a.t t GROUP BY t.g \
         HAVING count(*) > (SELECT count(*) FROM a.u u WHERE u.w > 6) ORDER BY 1",
        &[],
    );
    assert_eq!(
        refused(
            &path,
            "SELECT t.g, (SELECT count(*) FROM a.u u WHERE u.w > t.v) AS n FROM a.t t \
             GROUP BY t.g"
        ),
        "subquery uses ungrouped column \"t.v\" from outer query"
    );
    // Inside an aggregate's argument it is read on each row.
    let (b, _) = fresh(&path)
        .ask(
            "SELECT sum((SELECT count(*) FROM a.u u WHERE u.t_id = t.id)) AS s FROM a.t t",
            None,
        )
        .unwrap();
    assert_eq!(
        rows(&b)[0][0].as_deref().map(|s| s.trim_end_matches(".0")),
        Some("3")
    );
}

#[test]
fn a_lateral_subquery_reads_the_rows_before_it() {
    let path = snail("lateral");
    let round = same(
        &path,
        "SELECT t.id, s.w FROM a.t t, \
         LATERAL (SELECT u.w FROM a.u u WHERE u.t_id = t.id ORDER BY u.w LIMIT 1) s ORDER BY 1",
        &["1,5", "3,7"],
    );
    assert_eq!(round.order, "t_pkey+u_pkey");
    same(
        &path,
        "SELECT t.id, s.w FROM a.t t LEFT JOIN LATERAL \
         (SELECT u.w FROM a.u u WHERE u.t_id = t.id ORDER BY u.w LIMIT 1) s ON true ORDER BY 1",
        &["1,5", "2,-", "3,7", "4,-"],
    );
    same(
        &path,
        "SELECT t.id, x.n FROM a.t t JOIN LATERAL \
         (SELECT count(*) AS n FROM a.u u WHERE u.t_id = t.id) x ON true ORDER BY 1",
        &["1,2", "2,0", "3,1", "4,0"],
    );
    same(
        &path,
        "SELECT t.id, x.w FROM a.t t CROSS JOIN LATERAL \
         (SELECT u.w FROM a.u u WHERE u.t_id = t.id) x ORDER BY 1, 2",
        &["1,5", "1,-", "3,7"],
    );
}

/// An aggregate of the query around a subquery is that query's, one value for its one group,
/// which the subquery returns for each of its own rows, as 18.6 answers.
#[test]
fn an_aggregate_of_the_query_around_a_subquery_is_one_value_of_that_query_s() {
    let path = snail("around");
    same(
        &path,
        "SELECT (SELECT count(t.v) FROM a.u u WHERE u.id = 1) AS x FROM a.t t",
        &["3"],
    );
    same(
        &path,
        "SELECT (SELECT count(t.v) FROM a.u u LIMIT 1) AS x FROM a.t t",
        &["3"],
    );
}

#[test]
fn a_subquery_s_refusals_are_postgresql_s() {
    let path = snail("refused");
    for (q, why) in [
        (
            "SELECT (SELECT u.w FROM a.u u) AS x FROM a.t t",
            "more than one row returned by a subquery used as an expression",
        ),
        (
            "SELECT (SELECT u.w, u.id FROM a.u u) AS x FROM a.t t",
            "subquery must return only one column",
        ),
        (
            "SELECT t.id FROM a.t t WHERE t.id IN (SELECT u.t_id, u.w FROM a.u u)",
            "subquery has too many columns",
        ),
        (
            "SELECT t.id FROM a.t t WHERE (t.id, t.v) = ANY (SELECT u.t_id FROM a.u u)",
            "subquery has too few columns",
        ),
        (
            "SELECT t.id FROM a.t t WHERE t.id IN (SELECT u.t_id FROM a.u u FOR UPDATE)",
            "FOR UPDATE is not read: a question takes no lock on the rows it reads",
        ),
        // The aggregate is the query's around it, one value; the subquery a row of it for each of
        // a.u's four.
        (
            "SELECT (SELECT count(t.v) FROM a.u u) AS x FROM a.t t",
            "more than one row returned by a subquery used as an expression",
        ),
    ] {
        assert_eq!(refused(&path, q), why, "{q}");
    }
}

#[test]
fn several_snails_subqueries_read_each_table_from_its_own() {
    let main = tmp("subqueries-several-t.snail");
    let other = tmp("subqueries-several-u.snail");
    let (t, u) = tables();
    write(&main, &Catalog::parse(&format!("CREATE SCHEMA a; {T}")).unwrap(), &[("a.t".into(), t)]).unwrap();
    write(&other, &Catalog::parse(&format!("CREATE SCHEMA a; {U}")).unwrap(), &[("a.u".into(), u)]).unwrap();
    let mut s = warren_snail::read::Snails::open(&[&main, &other]).unwrap();
    for (q, expected) in [
        (
            "SELECT t.id FROM a.t t WHERE EXISTS (SELECT 1 FROM a.u u WHERE u.t_id = t.id) \
             ORDER BY t.id",
            &["1", "3"][..],
        ),
        (
            "SELECT t.id, (SELECT max(u.w) FROM a.u u WHERE u.t_id = t.id) AS m FROM a.t t \
             ORDER BY 1",
            &["1,5", "2,-", "3,7", "4,-"],
        ),
    ] {
        let (b, round) = s.ask(q).unwrap_or_else(|e| panic!("{q}: {e}"));
        assert_eq!(rows(&b), want(expected), "{q}");
        assert_eq!(round.snails.len(), 2, "{q}");
        for part in &round.snails {
            assert_eq!(
                part.column_pages_read,
                part.column_pages_predicted - part.column_pages_held,
                "{q}: {part:?}"
            );
        }
    }
}
