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
            "SELECT t.id, d.n FROM a.t t, LATERAL (SELECT count(*) AS n FROM a.u u WHERE u.t_id = t.id) d"
                .into(),
            "LATERAL (SELECT count(*) AS n FROM a.u u WHERE u.t_id = t.id) d is not read: a LATERAL \
             subquery reads the rows before it"
                .into(),
        ),
        // A function over a join: once "… is no function the DDL declares".
        (
            format!("SELECT a.nothing(t.v) AS c {join}"),
            "a.nothing(t.v) is not read".into(),
        ),
    ] {
        assert_eq!(refused(&path, &q), words, "{q}");
    }
    // Priced alone, as the wrapper prices a scan, a question names each clause and FROM item it
    // does not read by itself: once "DISTINCT and HAVING", "WITH, LIMIT and FETCH", "a question
    // reads tables" and "name each column a question returns".
    let s = fresh(&path);
    for (q, words) in [
        ("SELECT DISTINCT t.g FROM a.t t", "DISTINCT is not read"),
        (
            "SELECT t.g, count(*) AS c FROM a.t t GROUP BY t.g HAVING count(*) > 1",
            "HAVING is not read",
        ),
        (
            "SELECT t.id FROM a.t t ORDER BY t.id LIMIT 2",
            "LIMIT is not read",
        ),
        (
            "SELECT t.id FROM a.t t ORDER BY t.id OFFSET 1",
            "OFFSET is not read",
        ),
        (
            "SELECT d.id FROM (SELECT t.id FROM a.t t) d",
            "(SELECT t.id FROM a.t t) d is not read: a question reads tables",
        ),
        (
            "SELECT * FROM a.t t",
            "* is not read: name each column a question returns",
        ),
    ] {
        let e = s.price(q, None).err().map(|e| e.to_string());
        assert_eq!(e.as_deref(), Some(words), "{q}");
    }
    // SELECT ALL is no DISTINCT: answered over a join as without it.
    let b = ask(&path, &format!("SELECT ALL t.id {join} ORDER BY t.id"));
    assert_eq!(
        weather::ints64(b.column(0).as_ref()),
        [Some(1), Some(1), Some(2), Some(4)]
    );
}

/// The ids a question with its parameters answers, in its order.
fn ids_with(path: &std::path::Path, q: &str, params: &[&str]) -> Vec<Option<i64>> {
    let params: Vec<String> = params.iter().map(|p| p.to_string()).collect();
    let (b, _) = fresh(path)
        .ask_with(q, None, &params)
        .unwrap_or_else(|e| panic!("{q}: {e}"));
    weather::ints64(b.column(0).as_ref())
}

#[test]
fn a_parameter_takes_the_type_its_context_gives_it() {
    let path = snail("params");
    let step = "WITH x AS (SELECT t.id, t.v, t.s FROM a.t t)";
    for (q, params, want) in [
        // Beside an integer, read as one, on either side: once "Int(10) and Text(\"10\") are not
        // compared".
        (format!("{step} SELECT x.id FROM x WHERE x.v = $1"), &["10"][..], vec![Some(1)]),
        (
            format!("{step} SELECT x.id FROM x WHERE $1 < x.v ORDER BY x.id"),
            &["10"],
            vec![Some(2), Some(4)],
        ),
        (
            format!("{step} SELECT x.id FROM x WHERE x.v BETWEEN $1 AND $2 ORDER BY x.id"),
            &["8", "30"],
            vec![Some(1), Some(2)],
        ),
        (
            format!("{step} SELECT x.id FROM x WHERE x.v IN ($1, $2) ORDER BY x.id"),
            &["10", "40"],
            vec![Some(1), Some(4)],
        ),
        (
            format!("{step} SELECT x.v + $1 AS w FROM x WHERE x.id = 2"),
            &["5"],
            vec![Some(25)],
        ),
        (
            "WITH x AS (SELECT b.id, b.n FROM a.big b) SELECT x.id FROM x WHERE x.n = $1 ORDER BY x.id"
                .to_string(),
            &["9223372036854775807"],
            vec![Some(1), Some(2)],
        ),
        // Cast, and beside text, as before.
        (format!("{step} SELECT x.id FROM x WHERE x.v = $1::integer"), &["20"], vec![Some(2)]),
        (format!("{step} SELECT x.id FROM x WHERE x.s = $1"), &["c"], vec![Some(3)]),
    ] {
        assert_eq!(ids_with(&path, &q, params), want, "{q}");
    }
    // Alone in the select list it is text, as PostgreSQL resolves an unknown there.
    let params = ["10".to_string()];
    let (b, _) = fresh(&path)
        .ask_with(
            &format!("{step} SELECT $1 AS p FROM x WHERE x.id = 1"),
            None,
            &params,
        )
        .unwrap();
    assert_eq!(
        weather::texts(b.column(0).as_ref()),
        [Some("10".to_string())]
    );
}

#[test]
fn a_case_and_a_parameter_on_one_table_are_read_as_a_step_reads_them() {
    let path = snail("handed");
    // Once "CASE WHEN … END is not a level the snail reads".
    let b = ask(
        &path,
        "SELECT t.id, CASE WHEN t.v > 15 THEN 1 ELSE 0 END AS c FROM a.t t ORDER BY t.id",
    );
    assert_eq!(
        weather::ints64(b.column(1).as_ref()),
        [Some(0), Some(1), Some(0), Some(1), Some(0)]
    );
    let b = ask(
        &path,
        "SELECT t.id FROM a.t t WHERE CASE WHEN t.v > 15 THEN true ELSE false END ORDER BY t.id",
    );
    assert_eq!(weather::ints64(b.column(0).as_ref()), [Some(2), Some(4)]);
    let b = ask(
        &path,
        "SELECT CASE WHEN t.g = 1 THEN 1 ELSE 2 END AS k, count(*) AS n FROM a.t t GROUP BY 1 ORDER BY 1",
    );
    assert_eq!(weather::ints64(b.column(1).as_ref()), [Some(2), Some(3)]);
    let b = ask(
        &path,
        "SELECT sum(CASE WHEN t.v > 15 THEN 1 ELSE 0 END) AS c FROM a.t t",
    );
    assert_eq!(bigints(&b, 0), [Some(2)]);
    // Once "parameters are read only in a pipeline".
    for (q, params, want) in [
        (
            "SELECT t.id FROM a.t t WHERE t.v > $1 ORDER BY t.id",
            &["15"][..],
            vec![Some(2), Some(4)],
        ),
        (
            "SELECT t.id FROM a.t t WHERE t.s = $1",
            &["c"],
            vec![Some(3)],
        ),
        (
            "SELECT t.id FROM a.t t WHERE t.id BETWEEN $1 AND $2 ORDER BY t.id",
            &["2", "4"],
            vec![Some(2), Some(3), Some(4)],
        ),
    ] {
        assert_eq!(ids_with(&path, q, params), want, "{q}");
    }
    // Over a join too, once "parameters are read only in a question on one table".
    assert_eq!(
        ids_with(
            &path,
            "SELECT t.id FROM a.t t JOIN a.u u ON u.t_id = t.id WHERE u.w > $1 ORDER BY t.id",
            &["4"]
        ),
        [Some(1), Some(1), Some(2)]
    );
}

#[test]
#[cfg(feature = "standalone")]
fn a_document_s_arrows_on_one_table_are_read_as_a_step_reads_them() {
    let path = tmp("answers-arrows.snail");
    let catalog =
        Catalog::parse("CREATE TABLE a.j (id integer NOT NULL, doc jsonb, PRIMARY KEY (id));")
            .unwrap();
    let empty = Rows {
        columns: catalog.tables[0]
            .columns
            .iter()
            .map(|c| Values::new(c.ty))
            .collect(),
    };
    write(&path, &catalog, &[("a.j".into(), empty)]).unwrap();
    let mut w = warren_snail::read::Snail::open_writing(&path).unwrap();
    w.execute(
        "INSERT INTO a.j (id, doc) VALUES (1, '{\"Plan\": {\"Node Type\": \"Seq Scan\", \"Rows\": 5}}'), \
         (2, '{\"Plan\": {\"Node Type\": \"Hash Join\"}}'), (3, '{\"x\": 1}'), (4, NULL)",
    )
    .unwrap();
    // Once "j.doc -> 'Plan' ->> 'Node Type' is not a level the snail reads".
    let (b, _) = w
        .ask(
            "SELECT j.id, j.doc -> 'Plan' ->> 'Node Type' AS n FROM a.j j ORDER BY j.id",
            None,
        )
        .unwrap();
    assert_eq!(
        weather::texts(b.column(1).as_ref()),
        [
            Some("Seq Scan".into()),
            Some("Hash Join".into()),
            None,
            None
        ]
    );
    let (b, _) = w
        .ask("SELECT j.id FROM a.j j WHERE j.doc ->> 'x' = '1'", None)
        .unwrap();
    assert_eq!(weather::ints64(b.column(0).as_ref()), [Some(3)]);
    let (b, _) = w
        .ask(
            "SELECT j.id, j.doc #>> '{Plan,Rows}' AS r FROM a.j j ORDER BY j.id",
            None,
        )
        .unwrap();
    assert_eq!(
        weather::texts(b.column(1).as_ref()),
        [Some("5".into()), None, None, None]
    );
    // A parameter beside a document is read as one, as a quoted literal is.
    let params = [r#"{"x": 1}"#.to_string()];
    let (b, _) = w
        .ask_with(
            "WITH x AS (SELECT j.id, j.doc FROM a.j j) SELECT x.id FROM x WHERE x.doc @> $1",
            None,
            &params,
        )
        .unwrap();
    assert_eq!(weather::ints64(b.column(0).as_ref()), [Some(3)]);
}

#[test]
#[cfg(feature = "standalone")]
fn the_level_the_snail_runs_is_taken_in_each_of_its_spellings() {
    let path = snail("levels");
    let mut w = warren_snail::read::Snail::open_writing(&path).unwrap();
    let mut n = 100;
    let mut insert = |w: &mut warren_snail::read::Snail| {
        n += 1;
        w.execute(&format!("INSERT INTO a.u (id, t_id, w) VALUES ({n}, 1, 1)"))
    };
    let count = |w: &mut warren_snail::read::Snail| {
        weather::one(&w.ask("SELECT count(*) FROM a.u u", None).unwrap().0)
    };
    // Once each "… is not a statement the snail writes".
    for begin in [
        "BEGIN ISOLATION LEVEL READ COMMITTED",
        "START TRANSACTION ISOLATION LEVEL READ COMMITTED",
        "BEGIN TRANSACTION ISOLATION LEVEL READ UNCOMMITTED, READ WRITE",
        "BEGIN ISOLATION LEVEL SERIALIZABLE ISOLATION LEVEL READ COMMITTED",
    ] {
        assert_eq!(w.execute(begin).unwrap().statement, "BEGIN", "{begin}");
        insert(&mut w).unwrap();
        w.execute("COMMIT").unwrap();
    }
    assert_eq!(count(&mut w), 9);
    // SET TRANSACTION: outside a transaction it does nothing; inside, the transaction's level,
    // taken again after a read, and another refused after one, the transaction failed with it.
    assert_eq!(
        w.execute("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
            .unwrap()
            .statement,
        "SET"
    );
    w.execute("BEGIN").unwrap();
    w.execute("SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED")
        .unwrap();
    w.execute("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .unwrap();
    insert(&mut w).unwrap();
    w.execute("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .unwrap();
    let e = w
        .execute("SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED")
        .unwrap_err()
        .to_string();
    assert_eq!(
        e,
        "SET TRANSACTION ISOLATION LEVEL must be called before any query"
    );
    assert!(insert(&mut w).is_err());
    w.execute("ROLLBACK").unwrap();
    assert_eq!(count(&mut w), 9);
    // A read is a query too; READ UNCOMMITTED is a level of its own.
    w.execute("BEGIN ISOLATION LEVEL READ UNCOMMITTED").unwrap();
    assert_eq!(count(&mut w), 9);
    assert!(w
        .execute("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .is_err());
    w.execute("ROLLBACK").unwrap();
    // SET SESSION CHARACTERISTICS: the level of the transactions after, undone by a ROLLBACK.
    w.execute("BEGIN").unwrap();
    w.execute("SET SESSION CHARACTERISTICS AS TRANSACTION ISOLATION LEVEL READ UNCOMMITTED")
        .unwrap();
    w.execute("ROLLBACK").unwrap();
    w.execute("BEGIN").unwrap();
    insert(&mut w).unwrap();
    w.execute("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .unwrap();
    w.execute("COMMIT").unwrap();
    w.execute("SET SESSION CHARACTERISTICS AS TRANSACTION ISOLATION LEVEL READ UNCOMMITTED")
        .unwrap();
    w.execute("BEGIN").unwrap();
    insert(&mut w).unwrap();
    w.execute("SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED")
        .unwrap();
    assert!(w
        .execute("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .is_err());
    w.execute("ROLLBACK").unwrap();
    assert_eq!(count(&mut w), 10);
    // The levels the snail does not run, by name, the last a statement names deciding; READ ONLY.
    for (s, why) in [
        (
            "BEGIN ISOLATION LEVEL REPEATABLE READ",
            "ISOLATION LEVEL REPEATABLE READ",
        ),
        (
            "START TRANSACTION ISOLATION LEVEL SERIALIZABLE",
            "ISOLATION LEVEL SERIALIZABLE",
        ),
        (
            "BEGIN ISOLATION LEVEL READ COMMITTED, ISOLATION LEVEL SERIALIZABLE",
            "ISOLATION LEVEL SERIALIZABLE",
        ),
        (
            "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ",
            "ISOLATION LEVEL REPEATABLE READ",
        ),
        (
            "SET SESSION CHARACTERISTICS AS TRANSACTION ISOLATION LEVEL SERIALIZABLE",
            "ISOLATION LEVEL SERIALIZABLE",
        ),
    ] {
        assert_eq!(
            w.execute(s).unwrap_err().to_string(),
            format!("{why} is not read: the snail's transactions are READ COMMITTED"),
            "{s}"
        );
    }
    assert_eq!(
        w.execute("BEGIN READ ONLY").unwrap_err().to_string(),
        "READ ONLY is not read: a transaction of the snail's reads and writes"
    );
    assert_eq!(
        w.execute("BEGIN ISOLATION LEVEL SNAPSHOT")
            .unwrap_err()
            .to_string(),
        "syntax error at or near \"SNAPSHOT\""
    );
}
