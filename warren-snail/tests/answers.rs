//! Answers that were neither PostgreSQL's nor a refusal by name, each now one or the other: a sum
//! typed as PostgreSQL types it, on the question path, over a join and in a pipeline; a locking
//! clause refused by name on every read path; each refusal naming the clause or the call it
//! refuses; a step's expressions read on one table; a parameter typed by its context; and the
//! isolation level the snail runs taken in each of its spellings. Every answer is PostgreSQL 19's
//! and 18's over the same rows.

mod weather;

use arrow::array::{Array, Decimal128Array, Int64Array, RecordBatch};
use arrow::datatypes::DataType;
use warren_snail::catalog::Catalog;
use warren_snail::values::Values;
use warren_snail::write::Rows;
use weather::{fresh, tmp, write};

const DDL: &str = "CREATE TABLE a.t (id integer NOT NULL, g integer, v integer, sm smallint, \
                   s text, PRIMARY KEY (id));
     CREATE TABLE a.u (id integer NOT NULL, t_id integer, w integer, PRIMARY KEY (id));
     CREATE TABLE a.big (id integer NOT NULL, n bigint, PRIMARY KEY (id));";

const MAX: i64 = i64::MAX;

/// The tables' snail: `a.t` five rows, `a.u` five, `a.big` two bigints at their greatest and a 5.
fn snail(name: &str) -> std::path::PathBuf {
    let path = tmp(&format!("answers-{name}.snail"));
    let catalog = Catalog::parse(DDL).unwrap();
    let int = |v: &[Option<i64>]| Values::Int(v.to_vec());
    let t = Rows {
        columns: vec![
            int(&[Some(1), Some(2), Some(3), Some(4), Some(5)]),
            int(&[Some(1), Some(1), Some(2), Some(2), Some(3)]),
            int(&[Some(10), Some(20), None, Some(40), Some(7)]),
            int(&[Some(32767), Some(32767), Some(5), Some(-3), None]),
            Values::Text(
                ["a", "b", "c", "d", ""]
                    .iter()
                    .map(|s| (!s.is_empty()).then(|| s.to_string()))
                    .collect(),
            ),
        ],
    };
    let u = Rows {
        columns: vec![
            int(&[Some(1), Some(2), Some(3), Some(4), Some(5)]),
            int(&[Some(1), Some(1), Some(2), Some(4), Some(9)]),
            int(&[Some(5), Some(8), Some(9), Some(3), Some(1)]),
        ],
    };
    let big = Rows {
        columns: vec![
            int(&[Some(1), Some(2), Some(3)]),
            int(&[Some(MAX), Some(MAX), Some(5)]),
        ],
    };
    write(
        &path,
        &catalog,
        &[("a.t".into(), t), ("a.u".into(), u), ("a.big".into(), big)],
    )
    .unwrap();
    path
}

fn ask(path: &std::path::Path, q: &str) -> RecordBatch {
    fresh(path)
        .ask(q, None)
        .unwrap_or_else(|e| panic!("{q}: {e}"))
        .0
}

/// Column `c` as bigints, its type Int64.
fn bigints(b: &RecordBatch, c: usize) -> Vec<Option<i64>> {
    assert_eq!(
        b.column(c).data_type(),
        &DataType::Int64,
        "{:?}",
        b.schema()
    );
    let a = b.column(c).as_any().downcast_ref::<Int64Array>().unwrap();
    a.iter().collect()
}

/// Column `c` as a numeric's decimals at scale 0, named a numeric where the question path writes it.
fn numerics(b: &RecordBatch, c: usize) -> Vec<Option<i128>> {
    assert_eq!(
        b.column(c).data_type(),
        &DataType::Decimal128(38, 0),
        "{:?}",
        b.schema()
    );
    let a = b
        .column(c)
        .as_any()
        .downcast_ref::<Decimal128Array>()
        .unwrap();
    a.iter().collect()
}

#[test]
fn a_sum_is_a_bigint_of_smaller_integers_and_a_numeric_of_bigints() {
    let path = snail("sums");
    let twice = 2 * i128::from(MAX);
    // The question path, plain, grouped and over a join: PostgreSQL 19's
    // 18446744073709551619, where the sum wrapped to 3 as a bigint.
    let b = ask(&path, "SELECT sum(b.n) AS s FROM a.big b");
    assert_eq!(numerics(&b, 0), [Some(twice + 5)]);
    assert_eq!(
        b.schema()
            .field(0)
            .metadata()
            .get("pg_type")
            .map(String::as_str),
        Some("numeric")
    );
    let b = ask(
        &path,
        "SELECT b.id % 2 AS k, sum(b.n) AS s FROM a.big b GROUP BY 1 ORDER BY 2 DESC",
    );
    assert_eq!(
        numerics(&b, 1),
        [Some(i128::from(MAX) + 5), Some(i128::from(MAX))]
    );
    let b = ask(
        &path,
        "SELECT t.g, sum(b.n) AS s FROM a.t t JOIN a.big b ON b.id = t.id GROUP BY t.g ORDER BY t.g",
    );
    assert_eq!(numerics(&b, 1), [Some(twice), Some(5)]);
    let b = ask(&path, "SELECT sum(b.n) AS s FROM a.big b WHERE b.id > 3");
    assert_eq!(numerics(&b, 0), [None]);
    // A smallint's and an integer's are bigints.
    let b = ask(
        &path,
        "SELECT t.g, sum(t.v) AS a, sum(t.sm) AS b FROM a.t t GROUP BY t.g ORDER BY t.g",
    );
    assert_eq!(bigints(&b, 1), [Some(30), Some(40), Some(7)]);
    assert_eq!(bigints(&b, 2), [Some(65534), Some(2), None]);
    // A pipeline's: grouped, over each value once, of a function, and windowed whole and running,
    // where an integer's came out as numerics and a running bigint's was refused as out of range.
    let b = ask(
        &path,
        "WITH x AS (SELECT t.g, t.v, t.sm FROM a.t t) \
         SELECT x.g, sum(x.v) AS a, sum(DISTINCT x.sm) AS b FROM x GROUP BY x.g ORDER BY x.g",
    );
    assert_eq!(bigints(&b, 1), [Some(30), Some(40), Some(7)]);
    assert_eq!(bigints(&b, 2), [Some(32767), Some(2), None]);
    let b = ask(&path, "SELECT sum(abs(t.sm)) AS s FROM a.t t");
    assert_eq!(bigints(&b, 0), [Some(65542)]);
    let b = ask(
        &path,
        "WITH x AS (SELECT b.n FROM a.big b) SELECT sum(x.n) AS s FROM x",
    );
    assert_eq!(numerics(&b, 0), [Some(twice + 5)]);
    let b = ask(
        &path,
        "SELECT t.id, sum(t.v) OVER (ORDER BY t.id) AS a, \
         sum(t.sm) OVER (ORDER BY t.id ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) AS b \
         FROM a.t t ORDER BY t.id",
    );
    assert_eq!(
        bigints(&b, 1),
        [Some(10), Some(30), Some(30), Some(70), Some(77)]
    );
    assert_eq!(bigints(&b, 2), [Some(65536); 5]);
    let b = ask(
        &path,
        "SELECT b.id, sum(b.n) OVER (ORDER BY b.id) AS s FROM a.big b ORDER BY b.id",
    );
    assert_eq!(
        numerics(&b, 1),
        [Some(i128::from(MAX)), Some(twice), Some(twice + 5)]
    );
}

/// The words a question is refused in.
fn refused(path: &std::path::Path, q: &str) -> String {
    match fresh(path).ask(q, None) {
        Ok(_) => panic!("{q}: answered"),
        Err(e) => e.to_string(),
    }
}

const NO_LOCK: &str = "is not read: a question takes no lock on the rows it reads";

#[test]
fn a_locking_clause_is_refused_by_name_on_every_read_path() {
    let path = snail("locks");
    for (q, lock) in [
        (
            "SELECT t.id FROM a.t t WHERE t.id = 1 FOR UPDATE",
            "FOR UPDATE",
        ),
        ("SELECT t.id FROM a.t t FOR SHARE", "FOR SHARE"),
        (
            "SELECT t.id FROM a.t t FOR NO KEY UPDATE",
            "FOR NO KEY UPDATE",
        ),
        (
            "SELECT t.id FROM a.t t FOR KEY SHARE NOWAIT",
            "FOR KEY SHARE",
        ),
        (
            "SELECT t.id FROM a.t t FOR UPDATE NOWAIT",
            "FOR UPDATE NOWAIT",
        ),
        (
            "SELECT t.id FROM a.t t FOR SHARE SKIP LOCKED",
            "FOR SHARE SKIP LOCKED",
        ),
        ("SELECT t.id FROM a.t t FOR UPDATE FOR SHARE", "FOR UPDATE"),
        (
            "SELECT t.id, u.w FROM a.t t JOIN a.u u ON u.t_id = t.id FOR UPDATE OF t",
            "FOR UPDATE OF t",
        ),
        (
            "SELECT t.g, count(*) FROM a.t t JOIN a.u u ON u.t_id = t.id GROUP BY t.g FOR SHARE",
            "FOR SHARE",
        ),
        ("SELECT abs(t.v) FROM a.t t FOR UPDATE", "FOR UPDATE"),
        (
            "WITH x AS (SELECT t.id FROM a.t t) SELECT x.id FROM x FOR UPDATE",
            "FOR UPDATE",
        ),
        (
            "WITH x AS (SELECT t.id FROM a.t t FOR SHARE) SELECT x.id FROM x",
            "FOR SHARE",
        ),
        (
            "SELECT t.id, sum(t.v) OVER (ORDER BY t.id) FROM a.t t FOR KEY SHARE",
            "FOR KEY SHARE",
        ),
    ] {
        assert_eq!(refused(&path, q), format!("{lock} {NO_LOCK}"), "{q}");
    }
    // The words in a string or a name are no clause.
    let b = ask(
        &path,
        "SELECT t.id AS \"for key share\" FROM a.t t WHERE t.s = 'for no key update'",
    );
    assert_eq!(b.num_rows(), 0);
}

#[test]
#[cfg(feature = "standalone")]
fn an_insert_s_question_takes_no_lock_and_is_refused_naming_it() {
    let path = snail("locks-insert");
    let mut w = warren_snail::read::Snail::open_writing(&path).unwrap();
    for (s, lock) in [
        (
            "INSERT INTO a.u (id, t_id, w) SELECT t.id + 100, t.id, t.v FROM a.t t FOR UPDATE",
            "FOR UPDATE",
        ),
        (
            "INSERT INTO a.u (id, t_id, w) SELECT t.id + 100, t.id, t.v FROM a.t t FOR KEY SHARE",
            "FOR KEY SHARE",
        ),
    ] {
        let e = w.execute(s).unwrap_err().to_string();
        assert_eq!(e, format!("{lock} {NO_LOCK}"), "{s}");
    }
    // Without the clause the rows are written.
    w.execute(
        "INSERT INTO a.u (id, t_id, w) SELECT t.id + 100, t.id, t.v FROM a.t t WHERE t.id = 1",
    )
    .unwrap();
    let b = w.ask("SELECT count(*) FROM a.u u", None).unwrap().0;
    assert_eq!(weather::one(&b), 6);
}

#[test]
fn a_refusal_names_the_clause_or_the_call_it_refuses() {
    let path = snail("words");
    let join = "FROM a.t t JOIN a.u u ON u.t_id = t.id";
    for (q, words) in [
        // A walk up a parent column, its last SELECT reading the walk alone: once "the question
        // reads no table".
        (
            "WITH RECURSIVE up AS (SELECT t.id, t.g FROM a.t t WHERE t.id = 4 \
             UNION SELECT q.id, q.g FROM a.t q JOIN up ON q.id = up.g) SELECT up.id FROM up"
                .to_string(),
            "WITH RECURSIVE is not read".to_string(),
        ),
        (
            "WITH x AS (SELECT t.id FROM a.t t UNION SELECT u.id FROM a.u u) SELECT x.id FROM x"
                .into(),
            "set operations are not read".into(),
        ),
        (
            "WITH x AS (SELECT t.id FROM a.t t) SELECT x.id FROM x UNION SELECT x.id FROM x".into(),
            "set operations are not read".into(),
        ),
        (
            "WITH x AS (VALUES (1)) SELECT x.column1 FROM x".into(),
            "VALUES (1) is not read: a step is a SELECT".into(),
        ),
        (
            "WITH x AS (SELECT d.id FROM (SELECT t.id FROM a.t t) d) SELECT x.id FROM x".into(),
            "(SELECT t.id FROM a.t t) d is not read: a step reads tables and steps by name".into(),
        ),
        // A function over a join: once "… is no function the DDL declares".
        (
            format!("SELECT t.g, string_agg(t.s, ',' ORDER BY u.w) AS s {join} GROUP BY t.g"),
            "string_agg(t.s, ',' ORDER BY u.w) is not read".into(),
        ),
        (
            format!("SELECT coalesce(t.v, -1) AS c {join}"),
            "coalesce(t.v, -1) is not read".into(),
        ),
        (
            format!("SELECT t.id {join} WHERE abs(u.w) > 3"),
            "abs(u.w) is not read".into(),
        ),
        // Each clause by its own name: once "DISTINCT and HAVING", "WITH, LIMIT and FETCH".
        (
            format!("SELECT DISTINCT t.g {join}"),
            "DISTINCT is not read over a join".into(),
        ),
        (
            format!("SELECT t.g, count(*) AS c {join} GROUP BY t.g HAVING count(*) > 1"),
            "HAVING is not read over a join".into(),
        ),
        (
            "SELECT t.g, count(*) AS c FROM a.t t GROUP BY t.g HAVING count(*) > 1".into(),
            "HAVING is not read".into(),
        ),
        (
            format!("SELECT t.id {join} ORDER BY t.id LIMIT 2"),
            "LIMIT is not read over a join".into(),
        ),
        (
            format!("SELECT t.id {join} ORDER BY t.id OFFSET 1"),
            "OFFSET is not read over a join".into(),
        ),
        // A FROM item, by itself: once "a question reads tables".
        (
            "SELECT d.id FROM (SELECT t.id FROM a.t t) d".into(),
            "(SELECT t.id FROM a.t t) d is not read: a question reads tables".into(),
        ),
        (
            "SELECT t.id FROM a.t t JOIN (VALUES (1), (2)) v (x) ON v.x = t.id".into(),
            "(VALUES (1), (2)) v (x) is not read: a question reads tables".into(),
        ),
        (
            "SELECT t.id, g.g FROM a.t t, LATERAL generate_series(1, t.v) g".into(),
            "LATERAL generate_series(1, t.v) g is not read: a question reads tables".into(),
        ),
        // A star, by itself: once "name each column a question returns".
        (
            "SELECT * FROM a.t t".into(),
            "* is not read: name each column a question returns".into(),
        ),
        (
            format!("SELECT t.*, u.w {join}"),
            "t.* is not read: name each column a question returns".into(),
        ),
    ] {
        assert_eq!(refused(&path, &q), words, "{q}");
    }
    // SELECT ALL is no DISTINCT: answered over a join as without it.
    let b = ask(&path, &format!("SELECT ALL t.id {join} ORDER BY t.id"));
    assert_eq!(
        weather::ints64(b.column(0).as_ref()),
        [Some(1), Some(1), Some(2), Some(4)]
    );
}
