//! A question over several relations, read as a pipeline: a step reading a join of tables in one
//! round, relations of any kind a FROM names met by their ON, USING or NATURAL, a derived table,
//! VALUES, a function in FROM and a view each read as a step, a star read as the columns a FROM
//! gives; over a join the clauses the question path does not read; HAVING; and COALESCE, LEAST,
//! GREATEST and NULLIF typed as PostgreSQL's parser types them. Every answer is PostgreSQL 19's and
//! 18's over the same rows.

mod weather;

use arrow::array::{Array, RecordBatch};
use arrow::util::display::array_value_to_string;
use warren_snail::catalog::Catalog;
use warren_snail::read::Round;
use warren_snail::values::Values;
use warren_snail::write::Rows;
use weather::{fresh, tmp, write};

const DDL: &str =
    "CREATE TABLE a.t (id integer NOT NULL, g integer, v integer, s text, PRIMARY KEY (id));
     CREATE TABLE a.u (id integer NOT NULL, t_id integer, w integer, PRIMARY KEY (id));
     CREATE TABLE a.k (lo integer NOT NULL, hi integer NOT NULL, label text, PRIMARY KEY (lo));
     CREATE VIEW a.tu AS SELECT t.id, t.g, u.w FROM a.t t JOIN a.u u ON u.t_id = t.id;
     CREATE VIEW a.heavy (id, weight) AS SELECT tu.id, tu.w FROM a.tu WHERE tu.w > 6;";

/// The tables' rows: `a.t` five, `a.u` six, `a.k` three spans.
fn tables() -> (Rows, Rows, Rows) {
    let int = |v: &[Option<i64>]| Values::Int(v.to_vec());
    let text = |v: &[Option<&str>]| Values::Text(v.iter().map(|s| s.map(str::to_string)).collect());
    let t = Rows {
        columns: vec![
            int(&[Some(1), Some(2), Some(3), Some(4), Some(5)]),
            int(&[Some(1), Some(1), Some(2), Some(2), Some(3)]),
            int(&[Some(10), None, Some(30), Some(40), None]),
            text(&[Some("x"), Some("y"), None, Some("x"), Some("z")]),
        ],
    };
    let u = Rows {
        columns: vec![
            int(&[Some(1), Some(2), Some(3), Some(4), Some(5), Some(6)]),
            int(&[Some(1), Some(1), Some(2), Some(4), Some(9), None]),
            int(&[Some(5), Some(7), Some(9), None, Some(11), Some(2)]),
        ],
    };
    let k = Rows {
        columns: vec![
            int(&[Some(0), Some(6), Some(10)]),
            int(&[Some(6), Some(10), Some(100)]),
            text(&[Some("low"), Some("mid"), Some("high")]),
        ],
    };
    (t, u, k)
}

/// The tables' snail, with two views.
fn snail(name: &str) -> std::path::PathBuf {
    let path = tmp(&format!("relations-{name}.snail"));
    let catalog = Catalog::parse(DDL).unwrap();
    let (t, u, k) = tables();
    write(
        &path,
        &catalog,
        &[("a.t".into(), t), ("a.u".into(), u), ("a.k".into(), k)],
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

/// A question's answer, in its order, its rows those written; every table's read its predicted
/// pages, less those held.
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
fn a_step_reads_a_join_of_tables_in_one_round() {
    let path = snail("round");
    let round = same(
        &path,
        "WITH x AS (SELECT t.id, t.g, u.w FROM a.t t JOIN a.u u ON u.t_id = t.id WHERE t.g < 3) \
         SELECT x.g, sum(x.w) AS s, count(*) AS n FROM x GROUP BY x.g ORDER BY 1",
        &["1,21,3", "2,-,1"],
    );
    // Both tables read in the step's one round, each along an order of its own.
    assert_eq!(round.order, "t_pkey+u_pkey");
    // A join on inequalities in a step, as C1 pairs it.
    same(
        &path,
        "WITH x AS (SELECT t.id, k.label FROM a.t t JOIN a.k k ON t.v >= k.lo AND t.v < k.hi) \
         SELECT x.id, x.label FROM x ORDER BY 1",
        &["1,high", "3,high", "4,high"],
    );
}

#[test]
fn relations_a_from_names_meet_by_their_on() {
    let path = snail("meet");
    let steps = "WITH a AS (SELECT t.id, t.g FROM a.t t WHERE t.v IS NOT NULL), \
                 b AS (SELECT u.t_id, u.w FROM a.u u)";
    same(
        &path,
        &format!("{steps} SELECT a.id, b.w FROM a JOIN b ON b.t_id = a.id ORDER BY 1, 2"),
        &["1,5", "1,7", "4,-"],
    );
    // The = written either way round.
    same(
        &path,
        &format!("{steps} SELECT a.id, b.w FROM a JOIN b ON a.id = b.t_id ORDER BY 1, 2"),
        &["1,5", "1,7", "4,-"],
    );
    same(
        &path,
        &format!("{steps} SELECT a.id, b.w FROM a LEFT JOIN b ON b.t_id = a.id ORDER BY 1, 2"),
        &["1,5", "1,7", "3,-", "4,-"],
    );
    same(
        &path,
        &format!("{steps} SELECT a.id, b.w FROM a RIGHT JOIN b ON b.t_id = a.id ORDER BY 2, 1"),
        &["-,2", "1,5", "1,7", "-,9", "-,11", "4,-"],
    );
    same(
        &path,
        &format!("{steps} SELECT a.id, b.w FROM a FULL JOIN b ON b.t_id = a.id ORDER BY 1, 2"),
        &["1,5", "1,7", "3,-", "4,-", "-,2", "-,9", "-,11"],
    );
    // A step beside a table, and a comma's product.
    same(
        &path,
        &format!("{steps} SELECT a.id, k.label FROM a JOIN a.k k ON a.id > k.lo ORDER BY 1, 2"),
        &["1,low", "3,low", "4,low"],
    );
    same(
        &path,
        "WITH a AS (SELECT t.id FROM a.t t WHERE t.id < 3), b AS (SELECT k.label FROM a.k k) \
         SELECT a.id, b.label FROM a, b ORDER BY 1, 2",
        &["1,high", "1,low", "1,mid", "2,high", "2,low", "2,mid"],
    );
    // A RIGHT and a FULL JOIN after the first join.
    same(
        &path,
        "SELECT t.id, u.w, k.label FROM a.t t JOIN a.u u ON u.t_id = t.id \
         RIGHT JOIN a.k k ON k.lo = t.v - 4 ORDER BY 3",
        &["-,-,high", "-,-,low", "1,5,mid", "1,7,mid"],
    );
    same(
        &path,
        "SELECT t.id, k.label FROM a.t t JOIN a.u u ON u.t_id = t.id \
         FULL JOIN a.k k ON k.lo = u.w + 1 ORDER BY 2, 1",
        &["2,high", "-,low", "1,mid", "1,-", "4,-"],
    );
    // Refused as PostgreSQL refuses: a FULL JOIN on nothing it meets the rows by; an ON naming a
    // relation of another FROM item.
    assert_eq!(
        refused(
            &path,
            &format!("{steps} SELECT a.id FROM a FULL JOIN b ON b.t_id < a.id")
        ),
        "FULL JOIN is only supported with merge-joinable or hash-joinable join conditions"
    );
    assert!(refused(
        &path,
        &format!("{steps} SELECT a.id FROM a.k k, a JOIN b ON b.w = k.lo")
    )
    .contains("lo"));
}

#[test]
fn using_and_natural_merge_the_columns_they_meet_on() {
    let path = snail("using");
    // The merged column first, then each side's others.
    same(
        &path,
        "SELECT * FROM a.t JOIN a.u USING (id) WHERE id < 3 ORDER BY 1",
        &["1,1,10,x,1,5", "2,1,-,y,1,7"],
    );
    same(
        &path,
        "SELECT * FROM a.t t LEFT JOIN a.u u USING (id) WHERE t.g = 3",
        &["5,3,-,z,9,11"],
    );
    // The merged column is the right side's where a RIGHT JOIN keeps it, the first not NULL where
    // a FULL JOIN does; each side's own still read by its name.
    same(
        &path,
        "SELECT id, t.id, u.id FROM a.t t RIGHT JOIN a.u u USING (id) ORDER BY 3",
        &["1,1,1", "2,2,2", "3,3,3", "4,4,4", "5,5,5", "6,-,6"],
    );
    same(
        &path,
        "SELECT id, t.id, u.id FROM a.t t FULL JOIN a.u u USING (id) WHERE id > 4 ORDER BY 1",
        &["5,5,5", "6,-,6"],
    );
    // NATURAL on no column in common is a product.
    same(&path, "SELECT count(*) FROM a.t NATURAL JOIN a.k", &["15"]);
    same(
        &path,
        "WITH x AS (SELECT t.id, t.g FROM a.t t), y AS (SELECT u.id, u.w FROM a.u u) \
         SELECT x.g, y.w FROM x NATURAL JOIN y ORDER BY 1, 2",
        &["1,5", "1,7", "2,9", "2,-", "3,11"],
    );
    assert_eq!(
        refused(&path, "SELECT * FROM a.t t JOIN a.u u USING (nothing)"),
        "column \"nothing\" specified in USING clause does not exist in left table"
    );
    assert_eq!(
        refused(
            &path,
            "WITH x AS (SELECT t.id FROM a.t t) SELECT id FROM x JOIN a.u u ON u.t_id = x.id"
        ),
        "column reference \"id\" is ambiguous"
    );
}

#[test]
fn a_derived_table_and_values_are_read_as_steps() {
    let path = snail("derived");
    same(
        &path,
        "SELECT d.n, d.gg FROM (SELECT t.g, count(*) AS n FROM a.t t GROUP BY t.g) d (gg) ORDER BY 2",
        &["2,1", "2,2", "1,3"],
    );
    // Each column of VALUES in the type its values meet in.
    same(
        &path,
        "SELECT v.x, v.y FROM (VALUES (1, 'a'), (2.5, NULL)) v (x, y) ORDER BY 1",
        &["1,a", "2.5,-"],
    );
    same(
        &path,
        "WITH v (lo, n) AS (VALUES (0, 'a'), (6, 'b')) SELECT k.label, v.n FROM v JOIN a.k k ON k.lo = v.lo ORDER BY 1",
        &["low,a", "mid,b"],
    );
    same(
        &path,
        "SELECT v.x, t.s FROM (VALUES (1), (3), (9)) v (x) LEFT JOIN a.t t ON t.id = v.x ORDER BY 1",
        &["1,x", "3,-", "9,-"],
    );
    assert_eq!(
        refused(&path, "SELECT * FROM (VALUES (1), (true)) v"),
        "VALUES types integer and boolean cannot be matched"
    );
}

#[test]
fn a_star_reads_the_columns_its_from_gives() {
    let path = snail("star");
    same(
        &path,
        "SELECT * FROM a.k k ORDER BY 1",
        &["0,6,low", "6,10,mid", "10,100,high"],
    );
    same(
        &path,
        "SELECT k.*, t.id FROM a.k k JOIN a.t t ON t.v > k.lo AND t.v <= k.hi ORDER BY 4",
        &["6,10,mid,1", "10,100,high,3", "10,100,high,4"],
    );
    assert_eq!(
        refused(&path, "SELECT q.* FROM a.k k"),
        "missing FROM-clause entry for table \"q\""
    );
}

#[test]
fn over_a_join_the_pipeline_reads_what_the_question_path_does_not() {
    let path = snail("clauses");
    let join = "FROM a.t t JOIN a.u u ON u.t_id = t.id";
    same(
        &path,
        &format!("SELECT DISTINCT t.g {join} ORDER BY 1"),
        &["1", "2"],
    );
    same(
        &path,
        &format!(
            "SELECT t.g, count(DISTINCT u.w) AS d, count(*) FILTER (WHERE u.w > 6) AS f, \
             string_agg(u.w::text, ';' ORDER BY u.w DESC) AS s {join} GROUP BY t.g ORDER BY 1"
        ),
        &["1,3,2,9;7;5", "2,0,0,-"],
    );
    same(
        &path,
        &format!("SELECT t.g, count(*) AS c {join} GROUP BY t.g HAVING count(*) > 1 ORDER BY 1"),
        &["1,3"],
    );
    same(
        &path,
        &format!("SELECT t.id, u.w {join} ORDER BY u.w DESC LIMIT 2 OFFSET 1"),
        &["2,9", "1,7"],
    );
    same(
        &path,
        &format!("SELECT t.id {join} ORDER BY u.w FETCH FIRST 2 ROWS ONLY"),
        &["1", "1"],
    );
    same(
        &path,
        &format!("SELECT t.s {join} ORDER BY u.w"),
        &["x", "x", "y", "x"],
    );
    // A column's values counted, a GROUP BY by a name the select list gives, and no table at all.
    same(
        &path,
        &format!("SELECT t.g AS k, count(u.w) AS n {join} GROUP BY k ORDER BY k"),
        &["1,3", "2,0"],
    );
    same(&path, "SELECT count(t.v) AS n FROM a.t t", &["3"]);
    same(&path, "SELECT 1 AS one, 'x' AS two", &["1,x"]);
    // A parameter over a join, typed by what it meets.
    let (b, _) = fresh(&path)
        .ask_with(
            &format!("SELECT t.id, u.w {join} WHERE u.w > $1 ORDER BY 2"),
            None,
            &["6".to_string()],
        )
        .unwrap();
    assert_eq!(rows(&b), want(&["1,7", "2,9"]));
}

#[test]
fn having_keeps_the_groups_it_holds_of() {
    let path = snail("having");
    same(
        &path,
        "SELECT t.g, count(*) AS c FROM a.t t GROUP BY t.g HAVING sum(t.v) > 20 ORDER BY 1",
        &["2,2"],
    );
    // Grouped by nothing, one group, kept or not.
    same(
        &path,
        "SELECT count(*) AS c FROM a.t t HAVING count(*) > 10",
        &[],
    );
    same(
        &path,
        "SELECT count(*) AS c FROM a.t t HAVING count(*) > 1",
        &["5"],
    );
    // HAVING alone groups: one row where it holds, of no aggregate.
    same(&path, "SELECT 'g' AS x FROM a.t t HAVING true", &["g"]);
    same(
        &path,
        "WITH x AS (SELECT t.g, count(*) AS c FROM a.t t GROUP BY t.g HAVING min(t.id) > 1) \
         SELECT x.g FROM x ORDER BY 1",
        &["2", "3"],
    );
    assert_eq!(
        refused(&path, "SELECT t.g FROM a.t t GROUP BY t.g HAVING t.v > 1"),
        "column \"t.v\" must appear in the GROUP BY clause or be used in an aggregate function"
    );
}

#[test]
fn coalesce_least_greatest_and_nullif_take_the_type_their_arguments_meet_in() {
    let path = snail("chosen");
    same(
        &path,
        "SELECT t.id, coalesce(t.v, 2.5) AS c, least(t.v, 20, NULL) AS l, \
         greatest(t.s, 'y') AS m, nullif(t.v, 10) AS n FROM a.t t ORDER BY 1",
        &[
            "1,10,10,y,-",
            "2,2.5,20,y,-",
            "3,30,20,y,30",
            "4,40,20,y,40",
            "5,2.5,20,z,-",
        ],
    );
    same(
        &path,
        "SELECT t.id, coalesce(u.w, t.v, 0) AS c FROM a.t t LEFT JOIN a.u u ON u.t_id = t.id ORDER BY 1, 2",
        &["1,5", "1,7", "2,9", "3,30", "4,40", "5,0"],
    );
    // Refused before any row, in PostgreSQL's words.
    for (q, words) in [
        (
            "SELECT coalesce(t.v, 'x') FROM a.t t WHERE false",
            "invalid input syntax for type integer: \"x\"",
        ),
        (
            "SELECT coalesce(t.v, t.s) FROM a.t t",
            "COALESCE types integer and text cannot be matched",
        ),
        (
            "SELECT nullif(t.v, t.s) FROM a.t t",
            "operator does not exist: integer = text",
        ),
    ] {
        assert_eq!(refused(&path, q), words, "{q}");
    }
}

#[test]
fn a_view_is_kept_in_the_catalog_and_read_as_a_step() {
    let path = snail("views");
    same(
        &path,
        "SELECT v.g, sum(v.w) AS s FROM a.tu v GROUP BY v.g ORDER BY 1",
        &["1,21", "2,-"],
    );
    // A view over a view, its columns named by its column list.
    same(
        &path,
        "SELECT h.id, h.weight, t.s FROM a.heavy h JOIN a.t t ON t.id = h.id ORDER BY 2",
        &["1,7,x", "2,9,y"],
    );
    let snail = fresh(&path);
    assert_eq!(snail.catalog.views.len(), 2);
    // Its question read as PostgreSQL reads it as the view is made.
    for (ddl, words) in [
        (
            "CREATE VIEW a.bad AS SELECT t.nothing FROM a.t t",
            "column t.nothing does not exist",
        ),
        (
            "CREATE VIEW a.tu AS SELECT t.id FROM a.t t",
            "relation \"a.tu\" already exists",
        ),
        (
            "CREATE VIEW a.two (x, y) AS SELECT t.id FROM a.t t",
            "CREATE VIEW specifies more column names than columns",
        ),
    ] {
        let e = Catalog::parse(&format!("{DDL}\n{ddl};"))
            .unwrap_err()
            .to_string();
        assert_eq!(e, words, "{ddl}");
    }
}

#[test]
fn a_snail_of_format_eight_refuses_a_view_by_name() {
    let path = tmp("relations-eight.snail");
    let e = warren_snail::write::write_format_eight(
        &path,
        &Catalog::parse(DDL).unwrap(),
        &[],
        weather::span(),
    )
    .err()
    .unwrap()
    .to_string();
    assert!(
        e.contains("the view a.tu, which a snail of format 8") && e.contains("`out`"),
        "{e}"
    );
}

#[test]
#[cfg(feature = "standalone")]
fn a_view_is_declared_through_a_writer_and_read_after() {
    let path = snail("declared");
    let mut w = warren_snail::read::Snail::open_writing(&path).unwrap();
    w.execute("CREATE VIEW a.light AS SELECT tu.id, tu.w FROM a.tu WHERE tu.w < 6")
        .unwrap();
    let e = w
        .execute("CREATE VIEW a.light AS SELECT t.id FROM a.t t")
        .unwrap_err()
        .to_string();
    assert_eq!(e, "relation \"a.light\" already exists");
    drop(w);
    same(&path, "SELECT l.id, l.w FROM a.light l", &["1,5"]);
    assert_eq!(fresh(&path).catalog.views.len(), 3);
}

#[test]
fn several_snails_relations_meet_as_one_snail_s_do() {
    // The spans in a snail of their own, beside the tables and the views.
    let main = tmp("relations-several-main.snail");
    let ddl = DDL.replace(
        "CREATE TABLE a.k (lo integer NOT NULL, hi integer NOT NULL, label text, PRIMARY KEY (lo));",
        "",
    );
    let (t, u, k) = tables();
    write(
        &main,
        &Catalog::parse(&ddl).unwrap(),
        &[("a.t".into(), t), ("a.u".into(), u)],
    )
    .unwrap();
    let spans = tmp("relations-several-spans.snail");
    let catalog = Catalog::parse(
        "CREATE TABLE b.k (lo integer NOT NULL, hi integer NOT NULL, label text, PRIMARY KEY (lo));",
    )
    .unwrap();
    write(&spans, &catalog, &[("b.k".into(), k)]).unwrap();
    let mut s = warren_snail::read::Snails::open(&[&main, &spans]).unwrap();
    for (q, expected) in [
        (
            "WITH x AS (SELECT t.id, t.v FROM a.t t) SELECT x.id, k.label FROM x \
             JOIN b.k k ON x.v >= k.lo AND x.v < k.hi ORDER BY 1",
            &["1,high", "3,high", "4,high"][..],
        ),
        (
            "SELECT DISTINCT k.label FROM a.tu v JOIN b.k k ON v.w >= k.lo AND v.w < k.hi ORDER BY 1",
            &["low", "mid"],
        ),
        (
            "SELECT k.label, count(t.id) AS n FROM b.k k LEFT JOIN a.t t ON t.g = k.lo + 1 \
             GROUP BY k.label ORDER BY 1",
            &["high,0", "low,2", "mid,0"],
        ),
    ] {
        let (b, round) = s.ask(q).unwrap_or_else(|e| panic!("{q}: {e}"));
        assert_eq!(rows(&b), want(expected), "{q}");
        // Each snail's part of the round read the pages it predicted.
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

#[test]
fn a_scalar_function_s_alias_reads_its_column_under_any_name_the_column_has() {
    let path = snail("alias");
    same(
        &path,
        "SELECT x FROM unnest(ARRAY[3, 1]) x (n)",
        &["3", "1"],
    );
    same(
        &path,
        "SELECT n FROM unnest(ARRAY[3, 1]) x (n)",
        &["3", "1"],
    );
    same(
        &path,
        "SELECT x.n FROM unnest(ARRAY[3, 1]) x (n)",
        &["3", "1"],
    );
    // A function that names its column itself.
    same(
        &path,
        "SELECT v FROM jsonb_array_elements_text('[3, 1]') v",
        &["3", "1"],
    );
    assert_eq!(
        refused(&path, "SELECT y FROM unnest(ARRAY[3, 1]) x (n)"),
        "column \"y\" does not exist"
    );
}

#[test]
fn a_function_in_from_makes_rows_for_each_row_before_it() {
    let path = snail("calls");
    // A bare reference to a function returning a scalar reads its one column.
    same(&path, "SELECT u FROM unnest(ARRAY[3, 1]) u", &["3", "1"]);
    same(
        &path,
        "SELECT t.id, x FROM a.t t, unnest(ARRAY[t.id, t.g]) x WHERE t.id < 3 ORDER BY 1, 2",
        &["1,1", "1,1", "2,1", "2,2"],
    );
    same(
        &path,
        "SELECT * FROM unnest(ARRAY['b', 'a']) WITH ORDINALITY AS u (v, n)",
        &["b,1", "a,2"],
    );
    // A row before it the function makes no row for, under its ON, is kept beside NULLs.
    same(
        &path,
        "SELECT t.id, x.n FROM a.t t LEFT JOIN LATERAL unnest(ARRAY[t.v]) x (n) ON x.n > 15 ORDER BY 1",
        &["1,-", "2,-", "3,30", "4,40", "5,-"],
    );
    // COALESCE reads its arguments in turn, and no further than the first not NULL.
    same(
        &path,
        "SELECT coalesce(t.id, t.v / 0) AS c FROM a.t t ORDER BY 1",
        &["1", "2", "3", "4", "5"],
    );
    // A join's rows keep no table's order, so a window over them reads them whole.
    same(
        &path,
        "WITH x AS (SELECT u.t_id FROM a.u u) SELECT t.id, rank() OVER (ORDER BY t.id) AS n \
         FROM a.t t JOIN x ON x.t_id = t.id ORDER BY 1, 2",
        &["1,1", "1,1", "2,3", "4,4"],
    );
}
