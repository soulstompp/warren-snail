//! The functions a caller hands a snail, called from SQL as any function is: scalar, aggregate and
//! set-returning, a row of several columns among them, arithmetic past 64 bits inside, an
//! aggregate's rows in the order its ORDER BY gives; and jsonb_populate_record and its kin, a
//! table's row type read as a composite. Every answer is PostgreSQL 18's over the same rows, or
//! what the function handed in computes.

mod weather;

use arrow::array::{Array, RecordBatch};
use arrow::util::display::array_value_to_string;
use warren_snail::catalog::{Catalog, ColumnType};
use warren_snail::read::handed::{self, Fold, Function};
use warren_snail::read::{Snail, Snails, Value};
use warren_snail::values::Values;
use warren_snail::write::Rows;
use warren_snail::{Error, Result};
use weather::{fresh, tmp, write};

const DDL: &str = "CREATE SCHEMA a; CREATE TYPE a.dim AS (w integer, h integer);
     CREATE TABLE a.p (id integer NOT NULL, g integer, r bigint, c bigint, k text, PRIMARY KEY (id));
     CREATE TABLE a.q (id integer NOT NULL, p_id integer, w bigint, PRIMARY KEY (id));
     CREATE TABLE a.inv (id integer NOT NULL, name text, qty integer, price numeric(10,2), \
         tags text[], meta jsonb, doc json, at timestamptz, PRIMARY KEY (id));
     CREATE TABLE a.box (id integer NOT NULL, size a.dim, sizes a.dim[], PRIMARY KEY (id));";

fn int(v: &[i64]) -> Values {
    Values::Int(v.iter().map(|x| Some(*x)).collect())
}

/// `a.p`: thirteen (row, column) pairs in three groups, each with a key; `a.q`: six weights.
fn tables(catalog: &Catalog) -> Vec<(String, Rows)> {
    let p = Rows {
        columns: vec![
            int(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13]),
            int(&[1, 1, 1, 1, 2, 2, 2, 3, 3, 3, 3, 3, 3]),
            int(&[1, 1, 2, 2, 1, 2, 3, 1, 1, 2, 2, 3, 3]),
            int(&[1, 2, 1, 2, 1, 2, 3, 1, 2, 2, 3, 1, 3]),
            Values::Text(
                "d b d a c a e b f a c e d"
                    .split(' ')
                    .map(|s| Some(s.to_string()))
                    .collect(),
            ),
        ],
    };
    let q = Rows {
        columns: vec![
            int(&[1, 2, 3, 4, 5, 6]),
            int(&[1, 1, 2, 4, 9, 13]),
            int(&[30, 10, 20, 10, 5, 10]),
        ],
    };
    let empty = |name: &str| Rows {
        columns: catalog
            .tables
            .iter()
            .find(|t| t.name == name)
            .unwrap()
            .columns
            .iter()
            .map(|c| Values::new(c.ty))
            .collect(),
    };
    vec![
        ("a.p".into(), p),
        ("a.q".into(), q),
        ("a.inv".into(), empty("a.inv")),
        ("a.box".into(), empty("a.box")),
    ]
}

fn snail_file(name: &str) -> std::path::PathBuf {
    let path = tmp(&format!("handed-{name}.snail"));
    let catalog = Catalog::parse(DDL).unwrap();
    write(&path, &catalog, &tables(&catalog)).unwrap();
    path
}

fn ty(sql: &str) -> ColumnType {
    handed::type_named(sql).unwrap()
}

fn integer(v: &Value) -> Option<i64> {
    match v {
        Value::Int(x) => Some(*x),
        _ => None,
    }
}

fn text(v: &Value) -> Option<String> {
    match v {
        Value::Text(s) => Some(s.clone()),
        _ => None,
    }
}

/// `det2(a, b, c, d)`, `a·d − b·c` over bigints, computed in 128 bits: each product may pass 64
/// bits where their difference does not.
fn det2() -> Function {
    Function::scalar(
        "det2",
        &[ty("bigint"), ty("bigint"), ty("bigint"), ty("bigint")],
        ty("bigint"),
        |a| {
            let [a, b, c, d] = [&a[0], &a[1], &a[2], &a[3]].map(integer);
            let (Some(a), Some(b), Some(c), Some(d)) = (a, b, c, d) else {
                return Ok(Value::Null);
            };
            let x = i128::from(a) * i128::from(d) - i128::from(b) * i128::from(c);
            i64::try_from(x)
                .map(Value::Int)
                .map_err(|_| Error::new("bigint out of range"))
        },
    )
}

/// The product of bigints, in 128 bits, as a numeric.
struct Product(Option<i128>);

impl Fold for Product {
    fn add(&mut self, args: &[Value]) -> Result<()> {
        if let Some(x) = integer(&args[0]) {
            let p = self.0.unwrap_or(1).checked_mul(i128::from(x));
            self.0 = Some(p.ok_or_else(|| Error::new("numeric out of range"))?);
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<Value> {
        Ok(self.0.map_or(Value::Null, |p| Value::Num(p, 0)))
    }
}

/// The sum of numerics of no fraction, as they reach it: an integer is no numeric.
struct Sum(i128);

impl Fold for Sum {
    fn add(&mut self, args: &[Value]) -> Result<()> {
        if let Value::Num(m, 0) = &args[0] {
            self.0 += m;
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<Value> {
        Ok(Value::Num(self.0, 0))
    }
}

/// Each key in the order it came, `-` for NULL.
struct Seq(Vec<String>);

impl Fold for Seq {
    fn add(&mut self, args: &[Value]) -> Result<()> {
        self.0.push(text(&args[0]).unwrap_or_else(|| "-".into()));
        Ok(())
    }

    fn finish(&mut self) -> Result<Value> {
        Ok(Value::Text(self.0.join(",")))
    }
}

/// The rank of the 0/1 matrix a group's (row, column) pairs give, by fraction-free elimination in
/// 128 bits; a row of its rank, rows and columns, and whether no step overflowed.
struct Rank {
    ty: ColumnType,
    pairs: Vec<(i64, i64)>,
}

impl Fold for Rank {
    fn add(&mut self, args: &[Value]) -> Result<()> {
        if let (Some(r), Some(c)) = (integer(&args[0]), integer(&args[1])) {
            self.pairs.push((r, c));
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<Value> {
        let mut rows: Vec<i64> = self.pairs.iter().map(|p| p.0).collect();
        let mut cols: Vec<i64> = self.pairs.iter().map(|p| p.1).collect();
        rows.sort();
        rows.dedup();
        cols.sort();
        cols.dedup();
        let mut m = vec![vec![0i128; cols.len()]; rows.len()];
        for (r, c) in &self.pairs {
            let i = rows.binary_search(r).unwrap();
            let j = cols.binary_search(c).unwrap();
            m[i][j] = 1;
        }
        let (mut rank, mut prev, mut exact) = (0usize, 1i128, true);
        for col in 0..cols.len() {
            let Some(p) = (rank..rows.len()).find(|&i| m[i][col] != 0) else {
                continue;
            };
            m.swap(rank, p);
            for i in rank + 1..rows.len() {
                for j in col + 1..cols.len() {
                    let x = m[i][j]
                        .checked_mul(m[rank][col])
                        .zip(m[i][col].checked_mul(m[rank][j]))
                        .and_then(|(a, b)| a.checked_sub(b));
                    match x {
                        Some(x) => m[i][j] = x / prev,
                        None => exact = false,
                    }
                }
                m[i][col] = 0;
            }
            prev = m[rank][col];
            rank += 1;
        }
        Value::row(
            self.ty,
            vec![
                Value::Int(rank as i64),
                Value::Int(rows.len() as i64),
                Value::Int(cols.len() as i64),
                Value::Bool(exact),
            ],
        )
    }
}

/// The pairs in the order they came, as an array of rows.
struct Pairs {
    ty: ColumnType,
    row: ColumnType,
    pairs: Vec<Value>,
}

impl Fold for Pairs {
    fn add(&mut self, args: &[Value]) -> Result<()> {
        self.pairs.push(Value::row(self.row, args.to_vec())?);
        Ok(())
    }

    fn finish(&mut self) -> Result<Value> {
        Value::array(self.ty, std::mem::take(&mut self.pairs))
    }
}

/// Every function the tests hand in.
fn functions() -> Vec<Function> {
    let rank = handed::row_type(
        "a.rank",
        &[
            ("rank", ty("integer")),
            ("rows", ty("integer")),
            ("columns", ty("integer")),
            ("exact", ty("boolean")),
        ],
    );
    let pair = handed::row_type("a.pair", &[("r", ty("bigint")), ("c", ty("bigint"))]);
    let pairs = handed::array_type(pair);
    let square = handed::row_type("a.square", &[("i", ty("integer")), ("sq", ty("bigint"))]);
    let split = handed::row_type("a.split", &[("head", ty("text")), ("tail", ty("text"))]);
    vec![
        det2(),
        Function::aggregate("prod", &[ty("bigint")], ty("numeric"), || {
            Box::new(Product(None))
        }),
        Function::aggregate("seq", &[ty("text")], ty("text"), || Box::new(Seq(Vec::new()))),
        Function::aggregate("rank_of", &[ty("bigint"), ty("bigint")], rank, move || {
            Box::new(Rank {
                ty: rank,
                pairs: Vec::new(),
            })
        }),
        Function::aggregate("pairs", &[ty("bigint"), ty("bigint")], pairs, move || {
            Box::new(Pairs {
                ty: pairs,
                row: pair,
                pairs: Vec::new(),
            })
        }),
        Function::set_returning("digits", &[ty("bigint")], ty("integer"), |a| {
            Ok(integer(&a[0]).map_or(Vec::new(), |n| {
                n.unsigned_abs()
                    .to_string()
                    .bytes()
                    .map(|b| Value::Int(i64::from(b - b'0')))
                    .collect()
            }))
        }),
        Function::set_returning("squares", &[ty("integer")], square, move |a| {
            (1..=integer(&a[0]).unwrap_or(0))
                .map(|i| Value::row(square, vec![Value::Int(i), Value::Int(i * i)]))
                .collect()
        }),
        Function::scalar("split_key", &[ty("text")], split, move |a| {
            let Some(s) = text(&a[0]) else {
                return Ok(Value::Null);
            };
            let (head, tail) = s.split_at(s.chars().next().map_or(0, char::len_utf8));
            Value::row(
                split,
                vec![Value::Text(head.into()), Value::Text(tail.into())],
            )
        }),
        Function::scalar("pick", &[ty("integer")], ty("text"), |_| {
            Ok(Value::Text("integer".into()))
        }),
        Function::scalar("pick", &[ty("text")], ty("text"), |_| {
            Ok(Value::Text("text".into()))
        }),
        Function::scalar("pick", &[ty("numeric")], ty("text"), |_| {
            Ok(Value::Text("numeric".into()))
        }),
        Function::scalar("a.twice", &[ty("integer")], ty("integer"), |a| {
            Ok(integer(&a[0]).map_or(Value::Null, |x| Value::Int(2 * x)))
        }),
        Function::aggregate("nsum", &[ty("numeric")], ty("numeric"), || Box::new(Sum(0))),
        Function::scalar("half", &[ty("numeric")], ty("numeric"), |a| {
            Ok(match &a[0] {
                Value::Num(m, s) => Value::Num(m * 5, s + 1),
                _ => Value::Null,
            })
        }),
        Function::scalar("wrong", &[], ty("integer"), |_| {
            Ok(Value::Text("four".into()))
        }),
    ]
}

/// The snail, opened fresh, every function handed in.
fn handed_snail(path: &std::path::Path) -> Snail {
    let mut s = fresh(path);
    for f in functions() {
        s.hand(f).unwrap();
    }
    s
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

/// Rows written as text, a field of `-` NULL, fields split at `|`.
fn want(rows: &[&str]) -> Vec<Row> {
    rows.iter()
        .map(|r| {
            r.split('|')
                .map(|v| (v != "-").then(|| v.to_string()))
                .collect()
        })
        .collect()
}

fn same(s: &mut Snail, q: &str, expected: &[&str]) {
    let (b, _) = s.ask(q, None).unwrap_or_else(|e| panic!("{q}: {e}"));
    assert_eq!(rows(&b), want(expected), "{q}");
}

fn refused(s: &mut Snail, q: &str) -> String {
    match s.ask(q, None) {
        Ok((b, _)) => panic!("{q}: answered {:?}", rows(&b)),
        Err(e) => e.to_string(),
    }
}

/// PostgreSQL's words for a call no form of a function takes, with their HINT.
fn no_function(call: &str) -> String {
    format!(
        "function {call} does not exist\nHINT:  No function matches the given name and argument \
         types. You might need to add explicit type casts."
    )
}

#[test]
fn a_scalar_function_handed_in_computes_past_64_bits_and_is_called_where_any_function_is() {
    let path = snail_file("scalar");
    let mut s = handed_snail(&path);
    // 3037000500² passes 2^63 − 1; the difference of the two products does not.
    same(
        &mut s,
        "SELECT det2(3037000500, 3037000499, 3037000499, 3037000500)",
        &["6074000999"],
    );
    same(
        &mut s,
        "SELECT id, det2(3037000500 * r, 3037000499 * r, 3037000499 * r, 3037000500 * r) FROM a.p \
         WHERE id IN (2, 3) ORDER BY id",
        &["2|6074000999", "3|24296003996"],
    );
    let e = refused(&mut s, "SELECT det2(3037000500, 0, 0, 3037000500)");
    assert_eq!(e, "bigint out of range");
    // In WHERE, and over a join.
    same(
        &mut s,
        "SELECT id FROM a.p WHERE det2(r, c, c, r) = 0 ORDER BY id",
        &["1", "4", "5", "6", "7", "8", "10", "13"],
    );
    same(
        &mut s,
        "SELECT p.id, det2(p.r, q.w, 1, q.w) FROM a.p p JOIN a.q q ON q.p_id = p.id WHERE q.w = 10 ORDER BY p.id",
        &["1|0", "4|10", "13|20"],
    );
    // An integer argument reaches a numeric parameter as a numeric.
    same(&mut s, "SELECT half(g) FROM a.p WHERE id = 5", &["1.0"]);
    // And an aggregate's, each row's.
    same(&mut s, "SELECT nsum(g) FROM a.p", &["28"]);
}

#[test]
fn a_call_takes_the_form_whose_parameters_its_arguments_meet_as_postgresql_chooses_one() {
    let path = snail_file("forms");
    let mut s = handed_snail(&path);
    same(
        &mut s,
        "SELECT pick(1), pick('x'), pick(g), pick(k) FROM a.p WHERE id = 1",
        &["integer|text|integer|text"],
    );
    assert_eq!(
        refused(&mut s, "SELECT pick(true) FROM a.p"),
        no_function("pick(boolean)")
    );
    // A bigint is taken by the numeric form, the one of the three it is cast to with no cast
    // written; an integer by its own form, which takes it exactly.
    same(&mut s, "SELECT pick(r), pick(id) FROM a.p WHERE id = 1", &["numeric|integer"]);
    // A function handed under a schema, called by its whole name or its last part.
    same(
        &mut s,
        "SELECT a.twice(g), twice(id) FROM a.p WHERE id = 7",
        &["4|14"],
    );
    assert_eq!(
        refused(&mut s, "SELECT det2(1, 2, 3) FROM a.p"),
        no_function("det2(integer, integer, integer)")
    );
    assert_eq!(
        refused(&mut s, "SELECT half(true) FROM a.p"),
        no_function("half(boolean)")
    );
    let e = refused(&mut s, "SELECT wrong() FROM a.p");
    assert!(e.starts_with("wrong() returned a value that is no integer"), "{e}");
    assert_eq!(
        refused(&mut s, "SELECT det2(r, r, r, r) OVER () FROM a.p"),
        "OVER specified, but det2 is not a window function nor an aggregate function"
    );
    assert_eq!(
        refused(&mut s, "SELECT seq(k) OVER (ORDER BY id) FROM a.p"),
        "seq is an aggregate handed in: one over a window is not read"
    );
}

#[test]
fn a_function_is_refused_as_it_is_handed_where_its_name_or_form_is_taken() {
    let path = snail_file("refused");
    let mut s = handed_snail(&path);
    let abs = Function::scalar("abs", &[ty("bigint")], ty("bigint"), |a| Ok(a[0].clone()));
    let e = s.hand(abs).unwrap_err().to_string();
    assert!(e.contains("abs is PostgreSQL's own"), "{e}");
    let again = Function::scalar("pick", &[ty("integer")], ty("text"), |a| Ok(a[0].clone()));
    assert_eq!(
        s.hand(again).unwrap_err().to_string(),
        "function pick already exists with same argument types"
    );
    let kind = Function::scalar("seq", &[ty("integer")], ty("text"), |a| Ok(a[0].clone()));
    let e = s.hand(kind).unwrap_err().to_string();
    assert!(e.contains("is handed in already as an aggregate"), "{e}");
    // Unhanded, the snail knows no function of the name.
    let e = refused(&mut fresh(&path), "SELECT det2(r, c, r, c) FROM a.p");
    assert!(e.contains("det2"), "{e}");
}

#[test]
fn an_aggregate_s_rows_reach_it_in_the_order_its_order_by_gives_ties_in_the_table_s() {
    let path = snail_file("order");
    let mut s = handed_snail(&path);
    // With no ORDER BY, in the table's order.
    same(&mut s, "SELECT seq(k) FROM a.p", &["d,b,d,a,c,a,e,b,f,a,c,e,d"]);
    // Its ORDER BY exactly, rows that tie in the table's order: PostgreSQL's
    // string_agg(k, ',' ORDER BY g DESC, id).
    same(
        &mut s,
        "SELECT seq(k ORDER BY g DESC) FROM a.p",
        &["b,f,a,c,e,d,c,a,e,d,b,d,a"],
    );
    same(
        &mut s,
        "SELECT g, seq(k ORDER BY k) FROM a.p GROUP BY g ORDER BY g",
        &["1|a,b,d,d", "2|a,c,e", "3|a,b,c,d,e,f"],
    );
    same(
        &mut s,
        "SELECT seq(k) FILTER (WHERE g = 1), seq(DISTINCT k) FROM a.p",
        &["d,b,d,a|a,b,c,d,e,f"],
    );
    // Each value once, NULL among them, last, as a DISTINCT aggregate's input is sorted.
    same(
        &mut s,
        "SELECT seq(DISTINCT CASE WHEN g = 2 THEN NULL ELSE k END) FROM a.p",
        &["a,b,c,d,e,f,-"],
    );
    // Over a join, its ORDER BY decides.
    same(
        &mut s,
        "SELECT seq(p.k || q.w ORDER BY q.w, p.id) FROM a.p p JOIN a.q q ON q.p_id = p.id",
        &["f5,d10,a10,d10,b20,d30"],
    );
    // A NULL reaches it.
    same(
        &mut s,
        "SELECT seq(CASE WHEN g = 2 THEN NULL ELSE k END ORDER BY id) FROM a.p WHERE id <= 6",
        &["d,b,d,a,-,-"],
    );
}

#[test]
fn an_aggregate_handed_in_gives_a_row_of_several_columns_and_a_value_past_64_bits() {
    let path = snail_file("aggregate");
    let mut s = handed_snail(&path);
    same(
        &mut s,
        "WITH x AS (SELECT g, rank_of(r, c) AS m FROM a.p GROUP BY g) \
         SELECT g, (m).rank, (m).rows, (m).columns, (m).exact FROM x ORDER BY g",
        &["1|1|2|2|true", "2|3|3|3|true", "3|3|3|3|true"],
    );
    same(
        &mut s,
        "SELECT (rank_of(r, c)).rank FROM a.p WHERE g <> 2",
        &["3"],
    );
    // 6 · 2^96, past 64 bits: PostgreSQL's (1 * 2 * 3)::numeric * 4294967296 ^ 3.
    same(
        &mut s,
        "SELECT prod(r * 4294967296) FROM a.p WHERE g = 2",
        &["475368975085586025561263702016"],
    );
    let e = refused(&mut s, "SELECT (rank_of(r, c)).nope FROM a.p");
    assert_eq!(e, "column \"nope\" not found in data type a.rank");
}

#[test]
fn a_set_returning_function_handed_in_makes_rows_in_the_select_list_and_in_from() {
    let path = snail_file("sets");
    let mut s = handed_snail(&path);
    same(
        &mut s,
        "SELECT id, digits(r * 1000 + c) FROM a.p WHERE id <= 2 ORDER BY id",
        &["1|1", "1|0", "1|0", "1|1", "2|1", "2|0", "2|0", "2|2"],
    );
    same(
        &mut s,
        "SELECT p.id, x.i, x.sq FROM a.p p, LATERAL squares(p.r::integer) x WHERE p.g = 2 ORDER BY p.id, x.i",
        &["5|1|1", "6|1|1", "6|2|4", "7|1|1", "7|2|4", "7|3|9"],
    );
    same(&mut s, "SELECT * FROM squares(2)", &["1|1", "2|4"]);
    // A scalar function giving a row, in FROM and by its field.
    same(
        &mut s,
        "SELECT p.id, x.head, x.tail FROM a.p p, LATERAL split_key(p.k || 'xy') x WHERE p.id <= 2 ORDER BY p.id",
        &["1|d|xy", "2|b|xy"],
    );
    same(
        &mut s,
        "SELECT id, (split_key(k || 'z')).tail FROM a.p WHERE id = 1",
        &["1|z"],
    );
    // An aggregate giving a set: an array of rows, in the order its ORDER BY gives, unnested.
    same(
        &mut s,
        "SELECT b.r, b.c FROM (SELECT pairs(r, c ORDER BY r DESC, c) AS a FROM a.p WHERE g = 1) s, \
         unnest(s.a) b",
        &["2|1", "2|2", "1|1", "1|2"],
    );
}

/// `a.p` and `a.q` on two snails of their own.
fn split(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let catalog = Catalog::parse(DDL).unwrap();
    let all = tables(&catalog);
    let one = tmp(&format!("handed-{name}-p.snail"));
    let two = tmp(&format!("handed-{name}-q.snail"));
    let p_ddl = Catalog::parse(
        "CREATE SCHEMA a; CREATE TABLE a.p (id integer NOT NULL, g integer, r bigint, c bigint, k text, PRIMARY KEY (id));",
    )
    .unwrap();
    let q_ddl = Catalog::parse(
        "CREATE SCHEMA a; CREATE TABLE a.q (id integer NOT NULL, p_id integer, w bigint, PRIMARY KEY (id));",
    )
    .unwrap();
    write(&one, &p_ddl, &all[0..1]).unwrap();
    write(&two, &q_ddl, &all[1..2]).unwrap();
    (one, two)
}

#[test]
fn several_snails_call_the_functions_handed_to_them() {
    let (one, two) = split("across");
    let mut s = Snails::open(&[&one, &two]).unwrap();
    for f in functions() {
        s.hand(f).unwrap();
    }
    let (b, _) = s
        .ask("SELECT seq(p.k || q.w ORDER BY q.w, p.id) FROM a.p p JOIN a.q q ON q.p_id = p.id")
        .unwrap();
    assert_eq!(rows(&b), want(&["f5,d10,a10,d10,b20,d30"]));
    let (b, _) = s
        .ask("SELECT det2(w, 1, 1, w) FROM a.q WHERE id = 1")
        .unwrap();
    assert_eq!(rows(&b), want(&["899"]));
}

#[test]
#[cfg(feature = "standalone")]
fn a_statement_calls_the_functions_handed_to_its_writer() {
    let path = snail_file("writes");
    let mut w = Snail::open_writing(&path).unwrap();
    for f in functions() {
        w.hand(f).unwrap();
    }
    w.execute("INSERT INTO a.q SELECT 100 + id, id, det2(r, c, c, r) FROM a.p WHERE id IN (2, 3)")
        .unwrap();
    same(
        &mut w,
        "SELECT id, w FROM a.q WHERE id > 100 ORDER BY id",
        &["102|-3", "103|3"],
    );
    drop(w);
    // Through several snails written together, on the snail holding the table.
    let (one, two) = split("writes-across");
    let mut s = Snails::open_writing(&[&one, &two]).unwrap();
    for f in functions() {
        s.hand(f).unwrap();
    }
    s.execute("INSERT INTO a.q SELECT 200, 1, det2(w, 1, 1, w) FROM a.q WHERE id = 1")
        .unwrap();
    let (b, _) = s.ask("SELECT w FROM a.q WHERE id = 200").unwrap();
    assert_eq!(rows(&b), want(&["899"]));
}

#[test]
fn jsonb_populate_record_makes_a_row_of_a_table_s_row_type_as_postgresql_does() {
    let path = snail_file("populate");
    let mut s = fresh(&path);
    let all = "i.id, i.name, i.qty, i.price::text, i.tags::text, i.meta::text, i.doc::text, i.at::text";
    same(
        &mut s,
        &format!(
            "SELECT {all} FROM jsonb_populate_record(NULL::a.inv, '{{\"id\": 7, \"name\": \"bolt\", \
             \"qty\": \"12\", \"price\": 1.5, \"tags\": [\"a\", \"b\"], \"meta\": {{\"x\": [1, 2]}}, \
             \"doc\": {{\"y\":  \"z\"}}, \"at\": \"2024-01-02 03:04:05+00\", \"extra\": 1}}') i"
        ),
        &["7|bolt|12|1.50|{a,b}|{\"x\": [1, 2]}|{\"y\": \"z\"}|2024-01-02 03:04:05+00"],
    );
    // A base row's fields where the object lacks a key; the base row whole for an empty object.
    same(
        &mut s,
        &format!(
            "SELECT {all} FROM jsonb_populate_record(ROW(1, 'nut', 5, 2.25, NULL, NULL, NULL, NULL)::a.inv, \
             '{{\"qty\": 9, \"name\": null}}') i"
        ),
        &["1|-|9|2.25|-|-|-|-"],
    );
    same(
        &mut s,
        "SELECT i.id, i.name FROM jsonb_populate_record(ROW(1, 'nut', 5, 2.25, NULL, NULL, NULL, NULL)::a.inv, '{}') i",
        &["1|nut"],
    );
    same(
        &mut s,
        "SELECT i.id FROM jsonb_populate_record(NULL::a.inv, NULL) i",
        &["-"],
    );
    same(
        &mut s,
        "SELECT i.id, i.name, i.qty, i.tags::text FROM jsonb_populate_recordset(NULL::a.inv, \
         '[{\"id\":1,\"qty\":2},{\"id\":2,\"name\":\"x\",\"tags\":\"{p,q}\"}]') i",
        &["1|-|2|-", "2|x|-|{p,q}"],
    );
    // json as written: a key given twice its last, a string into json written again as a string.
    same(
        &mut s,
        &format!(
            "SELECT {all} FROM json_populate_record(NULL::a.inv, '{{\"id\": 3, \"doc\": \"a\\u0041\", \
             \"meta\": \"s\", \"name\": \"x\", \"name\": \"y\"}}') i"
        ),
        &["3|y|-|-|-|\"s\"|\"aA\"|-"],
    );
    same(
        &mut s,
        "SELECT i.meta::text, i.doc::text FROM json_populate_record(NULL::a.inv, \
         '{\"id\": 3, \"doc\": {\"k\" :  [1,2]}, \"meta\": {\"k\" :  [1,2]}}') i",
        &["{\"k\": [1, 2]}|{\"k\" :  [1,2]}"],
    );
    // A composite field from an object or from its text; an array of composites.
    same(
        &mut s,
        "SELECT b.id, b.size::text, b.sizes::text FROM jsonb_populate_record(NULL::a.box, \
         '{\"id\":1,\"size\":{\"w\":2,\"h\":3},\"sizes\":[{\"w\":1,\"h\":1},{\"w\":5}]}') b",
        &["1|(2,3)|{\"(1,1)\",\"(5,)\"}"],
    );
    same(
        &mut s,
        "SELECT b.id, b.size::text FROM jsonb_populate_record(NULL::a.box, '{\"id\":2,\"size\":\"(4,5)\"}') b",
        &["2|(4,5)"],
    );
    // In the select list, by its fields; valid or not.
    same(
        &mut s,
        "SELECT (jsonb_populate_record(NULL::a.inv, '{\"qty\": 4}')).qty, \
         jsonb_populate_record_valid(NULL::a.inv, '{\"qty\": \"x\"}'), \
         jsonb_populate_record_valid(NULL::a.inv, '{\"qty\": \"4\"}')",
        &["4|false|true"],
    );
    // Over a table's rows, each its own document.
    same(
        &mut s,
        "SELECT p.id, (jsonb_populate_record(NULL::a.inv, ('{\"id\": ' || p.r::text || ', \"name\": \"' || p.k || '\"}')::jsonb)).name \
         FROM a.p p WHERE p.id <= 2 ORDER BY p.id",
        &["1|d", "2|b"],
    );
}

#[test]
fn a_table_s_row_type_is_read_as_a_composite() {
    let path = snail_file("rowtype");
    let mut s = fresh(&path);
    same(
        &mut s,
        "SELECT (ROW(3, 'w', 1, 0.5, NULL, NULL, NULL, NULL)::a.inv).name",
        &["w"],
    );
    assert_eq!(
        refused(&mut s, "SELECT (ROW(3, 'w', 1, 0.5, NULL, NULL, NULL, NULL)::a.inv).nope"),
        "column \"nope\" not found in data type a.inv"
    );
    assert_eq!(
        refused(&mut s, "SELECT (ROW(1, 'x')::a.inv).name"),
        "cannot cast type record to a.inv"
    );
}

#[test]
fn jsonb_populate_record_is_refused_in_postgresql_s_words() {
    let path = snail_file("populate-refused");
    let mut s = fresh(&path);
    for (q, e) in [
        ("SELECT i.id FROM jsonb_populate_record(NULL::a.inv, '5') i", "cannot call populate_composite on a scalar"),
        ("SELECT i.id FROM jsonb_populate_record(NULL::a.inv, '[1]') i", "cannot call populate_composite on an array"),
        ("SELECT jsonb_populate_record(1, '{}')", "first argument of jsonb_populate_record must be a row type"),
        ("SELECT jsonb_populate_record(NULL, '{}')", "could not determine polymorphic type because input has type unknown"),
        ("SELECT i.id FROM jsonb_populate_record(NULL::a.inv, '{\"qty\": \"x\"}') i", "invalid input syntax for type integer: \"x\""),
        ("SELECT i.id FROM jsonb_populate_recordset(NULL::a.inv, '{\"a\":1}') i", "cannot call jsonb_populate_recordset on a non-array"),
        ("SELECT i.id FROM jsonb_populate_recordset(NULL::a.inv, '[1]') i", "argument of jsonb_populate_recordset must be an array of objects"),
        ("SELECT i.id FROM json_populate_recordset(NULL::a.inv, '{\"a\":1}') i", "cannot call json_populate_recordset on an object"),
        ("SELECT b.id FROM jsonb_populate_record(NULL::a.box, '{\"size\": [1]}') b", "cannot call populate_composite on an array"),
        ("SELECT i.id FROM jsonb_populate_record(NULL::a.inv, '{\"tags\": {\"a\":1}}') i", "expected JSON array"),
    ] {
        assert_eq!(refused(&mut s, q), e, "{q}");
    }
}

#[test]
fn a_built_in_type_s_name_casts_to_the_built_in_type_beside_a_table_of_that_name() {
    // Tables named as oid, money and inet are: a cast names the built-in type, as PostgreSQL finds
    // pg_catalog's first, and a table's row type only where no built-in type has its name; money
    // answers its count of cents.
    let path = tmp("handed-built-in-names.snail");
    let catalog = Catalog::parse(
        "CREATE SCHEMA s; CREATE TABLE s.oid (id integer NOT NULL, PRIMARY KEY (id));
         CREATE TABLE s.money (id integer NOT NULL, PRIMARY KEY (id));
         CREATE TABLE s.inet (id integer NOT NULL, PRIMARY KEY (id));",
    )
    .unwrap();
    let rows = |t: &str| {
        (
            t.to_string(),
            Rows {
                columns: vec![int(&[7])],
            },
        )
    };
    write(&path, &catalog, &[rows("s.oid"), rows("s.money"), rows("s.inet")]).unwrap();
    let mut snail = fresh(&path);
    for (q, want) in [
        ("WITH t AS (SELECT id FROM s.oid) SELECT CAST(id AS oid) AS x FROM t", "7"),
        ("WITH t AS (SELECT id FROM s.money) SELECT CAST(7.5 AS money) AS x FROM t", "750"),
        ("WITH t AS (SELECT id FROM s.inet) SELECT CAST('10.0.0.7' AS inet) AS x FROM t", "10.0.0.7"),
    ] {
        let (b, _) = snail.ask(q, None).unwrap();
        assert_eq!(array_value_to_string(b.column(0), 0).unwrap(), want, "{q}");
    }
}
