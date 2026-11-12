//! The index methods other than the B-tree: each read from its DDL as PostgreSQL 19 reads it, refused
//! in its words where PostgreSQL refuses it, and held as a strand of its own kind. A hash strand is a
//! strand on its key's B-tree order, `=` one place of it; aclitem, cid and xid, which have a hash and
//! no B-tree, on their bytes. A partial strand holds the rows its predicate admits, routed there at the
//! write, and serves a question whose conditions imply the predicate.

mod weather;

use arrow::array::{Array, Int32Array};
use warren_snail::catalog::Catalog;
use warren_snail::read::{BlockRow, Snail};
use warren_snail::scalars::unsigned_key;
use warren_snail::values::Values;
use warren_snail::write::Rows;
use weather::{exact, fresh, tmp, write};

/// Each statement's refusal, or None where it is taken; the schema's CREATE SCHEMA left out, which
/// no test here is about.
fn outcomes(ddl: &str) -> Vec<Option<String>> {
    Catalog::read(ddl)
        .unwrap()
        .statements
        .into_iter()
        .filter(|s| s.kind != "CREATE SCHEMA")
        .map(|s| s.refused)
        .collect()
}

#[test]
fn an_index_of_each_method_is_refused_as_postgresql_19_refuses_it() {
    let table = "CREATE SCHEMA s; CREATE TABLE s.t (id int PRIMARY KEY, r int4range, a int[], v tsvector, x xid, \
                 m money, b bit(3), c cid, g aclitem, p point, tx text, j jsonb, n inet);";
    // Each refusal as PostgreSQL 19 RC1 gave it (28819, 2026-10-31).
    let refused = [
        (
            "CREATE UNIQUE INDEX e ON s.t USING hash (x)",
            "access method \"hash\" does not support unique indexes",
        ),
        (
            "CREATE INDEX e ON s.t USING hash (x) INCLUDE (id)",
            "access method \"hash\" does not support included columns",
        ),
        (
            "CREATE INDEX e ON s.t USING hash (x, id)",
            "access method \"hash\" does not support multicolumn indexes",
        ),
        (
            "CREATE INDEX e ON s.t USING hash (m)",
            "data type money has no default operator class for access method \"hash\"",
        ),
        (
            "CREATE INDEX e ON s.t USING hash (b)",
            "data type bit has no default operator class for access method \"hash\"",
        ),
        (
            "CREATE INDEX e ON s.t USING hash (x DESC)",
            "access method \"hash\" does not support ASC/DESC options",
        ),
        (
            "CREATE INDEX e ON s.t USING hash (x ASC)",
            "access method \"hash\" does not support ASC/DESC options",
        ),
        (
            "CREATE INDEX e ON s.t USING hash (x NULLS FIRST)",
            "access method \"hash\" does not support NULLS FIRST/LAST options",
        ),
        (
            "CREATE INDEX e ON s.t USING hash (x int4_ops)",
            "operator class \"int4_ops\" does not accept data type xid",
        ),
        (
            "CREATE INDEX e ON s.t USING hash (x foo_ops)",
            "operator class \"foo_ops\" does not exist for access method \"hash\"",
        ),
        (
            "CREATE INDEX e ON s.t USING hash (x) WITH (fillfactor = 9)",
            "value 9 out of bounds for option \"fillfactor\"",
        ),
        (
            "CREATE INDEX e ON s.t USING hash (x) WITH (fillfactor = 'x')",
            "invalid value for integer option \"fillfactor\": x",
        ),
        (
            "CREATE INDEX e ON s.t USING hash (x) WITH (deduplicate_items = on)",
            "unrecognized parameter \"deduplicate_items\"",
        ),
        (
            "CREATE INDEX e ON s.t USING hash (x) WITH (fillfactor = 50, fillfactor = 60)",
            "parameter \"fillfactor\" specified more than once",
        ),
        (
            "CREATE INDEX e ON s.t USING hash (x xid_ops (siglen = 3))",
            "operator class xid_ops has no options",
        ),
        (
            "CREATE INDEX e ON s.t USING foo (x)",
            "access method \"foo\" does not exist",
        ),
        (
            "CREATE INDEX e ON s.t USING gin (v) WITH (fastupdate = maybe)",
            "invalid value for boolean option \"fastupdate\": maybe",
        ),
        (
            "CREATE INDEX e ON s.t USING gist (v tsvector_ops (siglen = 0))",
            "value 0 out of bounds for option \"siglen\"",
        ),
        (
            "CREATE INDEX e ON s.t USING brin (tx text_bloom_ops (false_positive_rate = 0.5))",
            "value 0.5 out of bounds for option \"false_positive_rate\"",
        ),
        (
            "CREATE INDEX e ON s.t USING gist (p) WITH (buffering = maybe)",
            "invalid value for enum option \"buffering\": maybe",
        ),
        (
            "CREATE INDEX e ON s.t USING spgist (tx, id)",
            "access method \"spgist\" does not support multicolumn indexes",
        ),
        (
            "CREATE INDEX e ON s.t USING gin (v) INCLUDE (id)",
            "access method \"gin\" does not support included columns",
        ),
        (
            "CREATE INDEX e ON s.t USING brin (j)",
            "data type jsonb has no default operator class for access method \"brin\"",
        ),
        (
            "CREATE INDEX e ON s.t USING brin (tx text_minmax_ops (values_per_range = 8))",
            "operator class text_minmax_ops has no options",
        ),
        (
            "CREATE INDEX e ON s.t USING gin (v tsvector_ops (siglen = 8))",
            "operator class tsvector_ops has no options",
        ),
    ];
    for (statement, why) in refused {
        let got = outcomes(&format!("{table}\n{statement};"));
        let e = got[1]
            .as_deref()
            .unwrap_or_else(|| panic!("{statement} was taken"));
        assert!(e.contains(why), "{statement}: {e}");
    }
    let got = outcomes(&format!(
        "{table}\nCREATE INDEX ok1 ON s.t USING hash (c);\nCREATE INDEX ok2 ON s.t USING hash (g);\n\
         CREATE INDEX ok3 ON s.t USING hash (tx text_pattern_ops) WITH (fillfactor = '50');\n\
         ALTER TABLE s.t CLUSTER ON ok1;"
    ));
    assert_eq!(got[1..4], [None, None, None], "{got:?}");
    assert!(
        got[4].as_deref().is_some_and(|e| e.contains(
            "cannot cluster on index \"ok1\" because access method does not support clustering"
        )),
        "{got:?}"
    );
}

#[test]
fn a_hash_index_is_read_with_its_operator_class_and_storage_as_postgresql_holds_them() {
    let reading = Catalog::read(
        "CREATE SCHEMA s; CREATE TABLE s.t (id int PRIMARY KEY, tx text, x xid, g aclitem, n cidr);\n\
         CREATE INDEX t_tx ON s.t USING hash (tx text_pattern_ops) WITH (fillfactor = '50');\n\
         CREATE INDEX t_x ON s.t USING hash (x);\n\
         CREATE INDEX t_g ON s.t USING hash (g);\n\
         CREATE INDEX t_n ON s.t USING hash (n cidr_ops);",
    )
    .unwrap();
    let t = &reading.catalog.tables[0];
    let o = |n: &str| t.order(n).unwrap();
    assert_eq!(o("t_tx").method.name(), "hash");
    assert_eq!(o("t_tx").opclasses, ["text_pattern_ops"]);
    assert_eq!(
        o("t_tx").options,
        [("fillfactor".to_string(), "50".to_string())]
    );
    assert_eq!(o("t_x").opclasses, ["xid_ops"]);
    assert_eq!(o("t_g").opclasses, ["aclitem_ops"]);
    assert_eq!(o("t_n").opclasses, ["cidr_ops"]);
    assert_eq!(o("t_pkey").method.name(), "btree");
}

fn blocks_of(rows: &[BlockRow], order: &str, column: &str) -> Vec<u64> {
    rows.iter()
        .filter(|r| r.order == order && r.column == column)
        .map(|r| r.rows)
        .collect()
}

fn ids(s: &mut Snail, sql: &str, order: &str) -> Vec<i32> {
    let (b, round) = s.ask(sql, Some(order)).unwrap();
    exact(sql, &round);
    let a = b.column(0).as_any().downcast_ref::<Int32Array>().unwrap();
    let mut v: Vec<i32> = (0..a.len()).map(|i| a.value(i)).collect();
    v.sort_unstable();
    v
}

#[test]
fn a_hash_strand_is_the_b_tree_strand_on_its_key() {
    // 4,096 rows, k and x spread over their 32 bits: the hash strand on k is cut as the B-tree
    // strand on k, and `=` along either reads the one block holding the value.
    let catalog = Catalog::parse(
        "CREATE SCHEMA s; CREATE TABLE s.t (id int NOT NULL, k int NOT NULL, x xid NOT NULL, PRIMARY KEY (id)); \
         CREATE INDEX t_hk ON s.t USING hash (k); \
         CREATE INDEX t_bk ON s.t (k); \
         CREATE INDEX t_hx ON s.t USING hash (x);",
    )
    .unwrap();
    let n = 4096i64;
    let spread = |i: i64| (i * 7919) % n;
    let id = Values::Int((0..n).map(Some).collect());
    let k = Values::Int(
        (0..n)
            .map(|i| Some((spread(i) << 20) - (1i64 << 31)))
            .collect(),
    );
    let x = Values::Int(
        (0..n)
            .map(|i| Some(unsigned_key((spread(i) as u64) << 20, 32)))
            .collect(),
    );
    let path = tmp("hash-strand.snail");
    write(
        &path,
        &catalog,
        &[(
            "s.t".into(),
            Rows {
                columns: vec![id, k, x],
            },
        )],
    )
    .unwrap();
    let mut s = fresh(&path);
    let blocks = s.blocks().unwrap();
    assert_eq!(
        blocks_of(&blocks, "t_hk", "k"),
        blocks_of(&blocks, "t_bk", "k")
    );
    assert!(blocks_of(&blocks, "t_hx", "x").len() > 1);
    let want = (0..n).find(|&i| spread(i) == 2001).unwrap() as i32;
    let k5 = (2001i64 << 20) - (1i64 << 31);
    let q = format!("SELECT id FROM s.t WHERE k = {k5}");
    for order in ["t_hk", "t_bk", "t_pkey"] {
        assert_eq!(ids(&mut s, &q, order), [want], "{order}");
    }
    let (_, along_hash) = s.ask(&q, Some("t_hk")).unwrap();
    let (_, along_key) = s.ask(&q, Some("t_pkey")).unwrap();
    assert_eq!(along_hash.blocks_read, 1);
    assert!(along_key.blocks_read > 1, "{along_key:?}");
    let q = format!("SELECT id FROM s.t WHERE x = '{}'::xid", 2001u64 << 20);
    assert_eq!(ids(&mut s, &q, "t_hx"), [want]);
    assert_eq!(ids(&mut s, &q, "t_pkey"), [want]);
    let (_, along_hash) = s.ask(&q, Some("t_hx")).unwrap();
    assert_eq!(along_hash.blocks_read, 1);
}

#[cfg(feature = "standalone")]
#[test]
fn aclitem_cid_and_xid_are_keyed_on_their_bytes_in_a_hash_strand() {
    let catalog = Catalog::parse(
        "CREATE SCHEMA s; CREATE TABLE s.t (id int PRIMARY KEY, c cid, g aclitem, x xid); \
         CREATE INDEX t_c ON s.t USING hash (c); \
         CREATE INDEX t_g ON s.t USING hash (g); \
         CREATE INDEX t_x ON s.t USING hash (x);",
    )
    .unwrap();
    let path = tmp("hash-bytes.snail");
    write(&path, &catalog, &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute(
        "INSERT INTO s.t (id, c, g, x) VALUES (1, '5', '10=r/10', '4294967295'), \
         (2, '7', '=arw/10', '1'), (3, NULL, NULL, NULL), (4, '5', '10=r/10', '2147483648'), \
         (5, '4294967295', '10=r*/10', '0')",
    )
    .unwrap();
    drop(w);
    let mut s = fresh(&path);
    for (q, order, want) in [
        ("SELECT id FROM s.t WHERE c = '5'", "t_c", vec![1, 4]),
        ("SELECT id FROM s.t WHERE c = '4294967295'", "t_c", vec![5]),
        ("SELECT id FROM s.t WHERE g = '10=r/10'", "t_g", vec![1, 4]),
        ("SELECT id FROM s.t WHERE g = '10=r*/10'", "t_g", vec![5]),
        ("SELECT id FROM s.t WHERE x = '4294967295'", "t_x", vec![1]),
        ("SELECT id FROM s.t WHERE x <> '1'", "t_x", vec![1, 4, 5]),
        ("SELECT id FROM s.t WHERE x IS NULL", "t_x", vec![3]),
    ] {
        assert_eq!(ids(&mut s, q, order), want, "{q} along {order}");
        assert_eq!(ids(&mut s, q, "t_pkey"), want, "{q} along t_pkey");
    }
}

#[test]
fn a_snail_of_format_eight_refuses_an_index_of_another_method_by_name() {
    let path = tmp("eight-hash.snail");
    let e = warren_snail::write::write_format_eight(
        &path,
        &Catalog::parse(
            "CREATE SCHEMA s; CREATE TABLE s.t (a integer NOT NULL, b integer, PRIMARY KEY (a)); \
             CREATE INDEX t_b ON s.t USING hash (b);",
        )
        .unwrap(),
        &[],
        weather::span(),
    )
    .err()
    .unwrap()
    .to_string();
    assert!(
        e.contains("s.t: the hash index t_b, which a snail of format 8") && e.contains("`out`"),
        "{e}"
    );
    let e = warren_snail::write::write_format_eight(
        &path,
        &Catalog::parse(
            "CREATE SCHEMA s; CREATE TABLE s.t (a integer NOT NULL, b integer, PRIMARY KEY (a)); \
             CREATE INDEX t_b ON s.t (b) WHERE b > 0;",
        )
        .unwrap(),
        &[],
        weather::span(),
    )
    .err()
    .unwrap()
    .to_string();
    assert!(
        e.contains("s.t: the partial index t_b, which a snail of format 8") && e.contains("`out`"),
        "{e}"
    );
}

#[test]
fn a_partial_index_s_predicate_is_read_as_a_question_s_where_or_refused_by_name() {
    let table = "CREATE SCHEMA s; CREATE TABLE s.t (id int PRIMARY KEY, a int, tags int[], gone timestamptz);";
    let got = outcomes(&format!(
        "{table}\nCREATE INDEX t_live ON s.t (a) WHERE gone IS NULL;\n\
         CREATE INDEX t_big ON s.t USING hash (a) WHERE a > 100 AND a <= 200;\n\
         CREATE INDEX t_tagged ON s.t (a) WHERE cardinality(tags) > 0;\n\
         ALTER TABLE s.t CLUSTER ON t_live;"
    ));
    assert_eq!(got[1..3], [None, None], "{got:?}");
    assert!(
        got[3]
            .as_deref()
            .is_some_and(|e| e.contains("t_tagged") && e.contains("predicate")),
        "{got:?}"
    );
    assert!(
        got[4]
            .as_deref()
            .is_some_and(|e| e.contains("cannot cluster on partial index \"t_live\"")),
        "{got:?}"
    );
}

/// A table of 4,096 rows, `a` its id's remainder by 512, `b` NULL on every fourth row, and a partial
/// strand on `a` holding the rows whose `b` is not NULL and whose `a` is under 256.
fn partial_snail(name: &str) -> std::path::PathBuf {
    let catalog = Catalog::parse(
        "CREATE SCHEMA s; CREATE TABLE s.t (id int NOT NULL, a int NOT NULL, b int, PRIMARY KEY (id)); \
         CREATE INDEX t_part ON s.t (a) WHERE b IS NOT NULL AND a < 256;",
    )
    .unwrap();
    let n = 4096i64;
    let id = Values::Int((0..n).map(Some).collect());
    let a = Values::Int((0..n).map(|i| Some(i % 512)).collect());
    let b = Values::Int((0..n).map(|i| (i % 4 != 0).then_some(i)).collect());
    let path = tmp(&format!("{name}.snail"));
    write(
        &path,
        &catalog,
        &[(
            "s.t".into(),
            Rows {
                columns: vec![id, a, b],
            },
        )],
    )
    .unwrap();
    path
}

/// The rows an order holds, by the directory.
fn rows_of_order(s: &Snail, order: &str) -> u64 {
    s.blocks()
        .unwrap()
        .iter()
        .filter(|r| r.order == order && r.column == "a")
        .map(|r| r.rows)
        .sum()
}

#[test]
fn a_partial_strand_holds_the_rows_its_predicate_admits_and_serves_what_implies_it() {
    let mut s = fresh(&partial_snail("partial"));
    // The rows whose b is not NULL and whose a is under 256: three of every four of half of them.
    assert_eq!(rows_of_order(&s, "t_part"), 1536);
    let implied = "SELECT id FROM s.t WHERE a = 7 AND b IS NOT NULL";
    let mut want = ids(&mut s, implied, "t_pkey");
    want.sort_unstable();
    assert_eq!(want.len(), 8);
    assert_eq!(ids(&mut s, implied, "t_part"), want);
    let (_, round) = s.ask(implied, None).unwrap();
    assert!(round.served.contains(&"t_part".to_string()), "{round:?}");
    // A comparison of b keeps no NULL, so it implies b IS NOT NULL.
    let implied = "SELECT id FROM s.t WHERE a >= 10 AND a < 12 AND b > 0";
    assert_eq!(
        ids(&mut s, implied, "t_part"),
        ids(&mut s, implied, "t_pkey")
    );
    for not_implied in [
        "SELECT id FROM s.t WHERE a = 7",
        "SELECT id FROM s.t WHERE a = 300 AND b IS NOT NULL",
        "SELECT id FROM s.t WHERE a < 300 AND b IS NOT NULL",
    ] {
        let (_, round) = s.ask(not_implied, None).unwrap();
        assert!(
            !round.served.contains(&"t_part".to_string()),
            "{not_implied}: {round:?}"
        );
        let e = s
            .ask(not_implied, Some("t_part"))
            .err()
            .unwrap()
            .to_string();
        assert!(
            e.contains("which the question does not imply"),
            "{not_implied}: {e}"
        );
    }
}

#[cfg(feature = "standalone")]
#[test]
fn a_statement_routes_a_row_into_a_partial_strand_and_out_as_its_predicate_says() {
    let path = partial_snail("partial-writes");
    let mut w = Snail::open_writing(&path).unwrap();
    // In: a new row admitted, and a row whose b was NULL given one; out: a row whose b is set NULL,
    // one whose a moves past 255, one deleted; and a row left out moved within what it leaves out.
    for sql in [
        "INSERT INTO s.t (id, a, b) VALUES (5000, 7, 1), (5001, 7, NULL)",
        "UPDATE s.t SET b = 1 WHERE id = 4",
        "UPDATE s.t SET b = NULL WHERE id = 1",
        "UPDATE s.t SET a = 300 WHERE id = 2",
        "DELETE FROM s.t WHERE id = 3",
        "UPDATE s.t SET a = 301 WHERE id = 256",
        "UPDATE s.t SET b = 2 WHERE id = 5",
    ] {
        w.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    drop(w);
    let mut s = fresh(&path);
    // 1,536 rows, one in and one more given b, three out.
    assert_eq!(rows_of_order(&s, "t_part"), 1535);
    for q in [
        "SELECT id FROM s.t WHERE a = 7 AND b IS NOT NULL",
        "SELECT id FROM s.t WHERE a < 8 AND b IS NOT NULL",
        "SELECT id FROM s.t WHERE a = 0 AND b IS NOT NULL",
        "SELECT id FROM s.t WHERE a = 4 AND b IS NOT NULL",
    ] {
        assert_eq!(ids(&mut s, q, "t_part"), ids(&mut s, q, "t_pkey"), "{q}");
    }
    let (b, _) = s
        .ask(
            "SELECT count(*) FROM s.t WHERE a < 256 AND b IS NOT NULL",
            Some("t_part"),
        )
        .unwrap();
    let n = b
        .column(0)
        .as_any()
        .downcast_ref::<arrow::array::Int64Array>()
        .unwrap()
        .value(0);
    assert_eq!(n, 1535);
}

#[test]
fn a_gist_or_sp_gist_index_is_held_on_a_range_a_multirange_an_inet_text_and_the_plane() {
    let table = "CREATE SCHEMA s; CREATE TABLE s.t (id int PRIMARY KEY, r int4range, m int4multirange, n inet, \
                 tx text, p point, v tsvector);";
    let got = outcomes(&format!(
        "{table}\nCREATE INDEX t_r ON s.t USING gist (r) INCLUDE (tx);\n\
         CREATE INDEX t_rm ON s.t USING gist (r, m);\n\
         CREATE INDEX t_n ON s.t USING gist (n inet_ops);\n\
         CREATE INDEX t_sr ON s.t USING spgist (r);\n\
         CREATE INDEX t_sn ON s.t USING spgist (n);\n\
         CREATE INDEX t_st ON s.t USING spgist (tx);\n\
         CREATE INDEX t_p ON s.t USING gist (p);\n\
         CREATE INDEX t_v ON s.t USING gist (v);\n\
         CREATE INDEX t_q ON s.t USING spgist (p kd_point_ops);"
    ));
    assert_eq!(
        got[1..10],
        [None, None, None, None, None, None, None, None, None],
        "{got:?}"
    );
}

/// A table of `n` rows, row i's range [10j, 10j + 5) for j = 7919i mod n, so that neighbours on the
/// table's key lie far apart on the range's order; with a GiST index on the range.
fn ranges(name: &str, n: i64) -> std::path::PathBuf {
    spans_of(name, n, "int4range")
}

/// As `ranges`, of type `ty`: a multirange holds its range and one past it.
fn spans_of(name: &str, n: i64, ty: &str) -> std::path::PathBuf {
    let catalog = Catalog::parse(&format!(
        "CREATE SCHEMA s; CREATE TABLE s.t (id int NOT NULL, r {ty}, PRIMARY KEY (id)); \
         CREATE INDEX t_r ON s.t USING gist (r);"
    ))
    .unwrap();
    let multi = ty.contains("multi");
    let ty = warren_snail::composed::built_in(ty).unwrap();
    let id = Values::Int((0..n).map(Some).collect());
    let r = Values::Composed(
        (0..n)
            .map(|i| {
                let j = (i * 7919) % n;
                // Every hundredth row empty, and every hundred-and-first NULL.
                let text = match (i % 100 == 0, multi) {
                    (true, false) => "empty".to_string(),
                    (true, true) => "{}".to_string(),
                    (false, false) => format!("[{},{})", j * 10, j * 10 + 5),
                    (false, true) => format!(
                        "{{[{},{}),[{},{})}}",
                        j * 10,
                        j * 10 + 2,
                        j * 10 + 4,
                        j * 10 + 5
                    ),
                };
                (i % 101 != 0).then(|| {
                    std::sync::Arc::new(warren_snail::composed::Compound::new(
                        ty,
                        warren_snail::composed::text_in(ty, &text).unwrap(),
                    ))
                })
            })
            .collect(),
    );
    let path = tmp(&format!("{name}.snail"));
    write(
        &path,
        &catalog,
        &[(
            "s.t".into(),
            Rows {
                columns: vec![id, r],
            },
        )],
    )
    .unwrap();
    path
}

#[test]
fn a_gist_strand_on_a_range_rules_out_a_block_whose_span_its_value_misses() {
    let mut s = fresh(&ranges("gist-range", 20480));
    let blocks = blocks_of(&s.blocks().unwrap(), "t_r", "r.lower").len();
    assert!(blocks > 2, "{blocks}");
    for q in [
        "SELECT id FROM s.t WHERE r && '[100000,100100)'",
        "SELECT id FROM s.t WHERE '[100000,100100)' && r",
        "SELECT id FROM s.t WHERE r @> '[100001,100003)'",
        "SELECT id FROM s.t WHERE r @> 100002",
        "SELECT id FROM s.t WHERE r @> 'empty'",
        "SELECT id FROM s.t WHERE r <@ '[100000,100100)'",
        "SELECT id FROM s.t WHERE '[100000,100100)' @> r",
        "SELECT id FROM s.t WHERE r <@ 'empty'",
        "SELECT id FROM s.t WHERE r << '[1000,1010)'",
        "SELECT id FROM s.t WHERE r >> '[204000,204010)'",
        "SELECT id FROM s.t WHERE r &< '[0,1000)'",
        "SELECT id FROM s.t WHERE r &> '[204000,204010)'",
        "SELECT id FROM s.t WHERE r -|- '[100005,100010)'",
        "SELECT id FROM s.t WHERE r && 'empty'",
        "SELECT id FROM s.t WHERE r && '(,)' AND id < 50",
    ] {
        let along_key = ids(&mut s, q, "t_pkey");
        assert_eq!(ids(&mut s, q, "t_r"), along_key, "{q}");
    }
    // A narrow value lies in the span of one block or two: the rest are never read.
    let q = "SELECT id FROM s.t WHERE r && '[100000,100100)'";
    let (_, along) = s.ask(q, Some("t_r")).unwrap();
    assert!(
        along.blocks_read <= 2 && along.entries_none as usize >= blocks - 2,
        "{along:?}"
    );
    let (_, whole) = s.ask(q, Some("t_pkey")).unwrap();
    assert!(whole.blocks_read > along.blocks_read, "{whole:?}");
    // An empty range lies in no span; only the blocks holding one are read for <@ 'empty'.
    let (_, empty) = s
        .ask("SELECT id FROM s.t WHERE r <@ 'empty'", Some("t_r"))
        .unwrap();
    assert!(empty.blocks_read < blocks as u64, "{empty:?}");
}

#[test]
fn a_gist_strand_s_condition_it_cannot_hold_is_refused_never_taken_as_false() {
    let mut s = fresh(&ranges("gist-refused", 4096));
    // Each refusal as PostgreSQL 19 RC1 gave it (28819, 2026-10-31) and 18.6 (28818, 2026-11-11),
    // along either order and before any row: no form of the operator takes a value of that type
    // beside an int4range.
    for (q, refused) in [
        (
            "SELECT id FROM s.t WHERE r -|- '[20005,20010)'::int8range",
            "int4range -|- int8range",
        ),
        (
            "SELECT id FROM s.t WHERE r && '[20000,20100)'::int8range",
            "int4range && int8range",
        ),
        (
            "SELECT id FROM s.t WHERE r @> 20002::bigint",
            "int4range @> bigint",
        ),
        (
            "SELECT id FROM s.t WHERE r @> 20002.5",
            "int4range @> numeric",
        ),
        (
            "SELECT id FROM s.t WHERE 20002::int2 <@ r",
            "smallint <@ int4range",
        ),
        (
            "SELECT id FROM s.t WHERE r @> 99999999::bigint",
            "int4range @> bigint",
        ),
    ] {
        for order in ["t_pkey", "t_r"] {
            let got = s.ask(q, Some(order)).unwrap_err().to_string();
            assert_eq!(
                got,
                format!(
                    "operator does not exist: {refused}\nHINT:  No operator matches the given name \
                     and argument types. You might need to add explicit type casts."
                ),
                "{q}"
            );
        }
    }
    // A value that fails as a row is held to it fails the question, as PostgreSQL's does.
    for order in ["t_pkey", "t_r"] {
        let got = s
            .ask(
                "SELECT id FROM s.t WHERE r && int4range(20010, 20005)",
                Some(order),
            )
            .unwrap_err();
        assert_eq!(
            got.to_string(),
            "range lower bound must be less than or equal to range upper bound"
        );
    }
    // A NULL fails every operator a strand answers, along the key where its rows are walked too.
    let q = "SELECT id FROM s.t WHERE r -|- '[20005,20010)'";
    let along_key = ids(&mut s, q, "t_pkey");
    assert_eq!(along_key.len(), 2, "{along_key:?}");
    assert_eq!(ids(&mut s, q, "t_r"), along_key);
}

#[test]
fn an_sp_gist_strand_on_an_inet_rules_out_a_block_whose_addresses_its_network_misses() {
    let catalog = Catalog::parse(
        "CREATE SCHEMA s; CREATE TABLE s.t (id int NOT NULL, n inet, PRIMARY KEY (id)); \
         CREATE INDEX t_n ON s.t USING spgist (n);",
    )
    .unwrap();
    let path = tmp("spgist-inet.snail");
    write(&path, &catalog, &[]).unwrap();
    #[cfg(feature = "standalone")]
    {
        let mut w = Snail::open_writing(&path).unwrap();
        let rows: Vec<String> = (0..4096)
            .map(|i| {
                let j = (i * 7919) % 4096;
                format!("({i}, '10.{}.{}.0/24')", j / 256, j % 256)
            })
            .collect();
        w.execute(&format!(
            "INSERT INTO s.t (id, n) VALUES {}",
            rows.join(", ")
        ))
        .unwrap();
        drop(w);
        let mut s = fresh(&path);
        for q in [
            "SELECT id FROM s.t WHERE n << '10.7.0.0/16'",
            "SELECT id FROM s.t WHERE n <<= '10.7.3.0/24'",
            "SELECT id FROM s.t WHERE n >> '10.7.3.4'",
            "SELECT id FROM s.t WHERE n >>= '10.7.3.0/24'",
            "SELECT id FROM s.t WHERE n && '10.15.0.0/16'",
            "SELECT id FROM s.t WHERE '10.7.0.0/16' >> n",
            "SELECT id FROM s.t WHERE n && '192.168.0.0/16'",
            // Inside the last network of all, past its first address.
            "SELECT id FROM s.t WHERE n >> '10.15.255.9'",
        ] {
            let along_key = ids(&mut s, q, "t_pkey");
            assert_eq!(ids(&mut s, q, "t_n"), along_key, "{q}");
        }
        let (_, along) = s
            .ask("SELECT id FROM s.t WHERE n << '10.7.0.0/16'", Some("t_n"))
            .unwrap();
        assert_eq!(along.blocks_read, 1, "{along:?}");
        let last = "SELECT id FROM s.t WHERE n >> '10.15.255.9'";
        assert_eq!(ids(&mut s, last, "t_n").len(), 1);
        let (_, none) = s
            .ask(
                "SELECT id FROM s.t WHERE n && '192.168.0.0/16'",
                Some("t_n"),
            )
            .unwrap();
        assert_eq!(none.blocks_read, 0, "{none:?}");
    }
}

#[cfg(feature = "standalone")]
#[test]
fn a_range_written_into_a_gist_strand_widens_its_block_s_span() {
    let path = ranges("gist-writes", 20480);
    let mut w = Snail::open_writing(&path).unwrap();
    // A range past every span, into the strand's last block; one moved past it; one deleted.
    for sql in [
        "INSERT INTO s.t (id, r) VALUES (30000, '[900000,900010)')",
        "UPDATE s.t SET r = '[950000,950001)' WHERE id = 1",
        "DELETE FROM s.t WHERE id = 2",
    ] {
        w.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    drop(w);
    let mut s = fresh(&path);
    for q in [
        "SELECT id FROM s.t WHERE r && '[900005,900006)'",
        "SELECT id FROM s.t WHERE r @> 950000",
        "SELECT id FROM s.t WHERE r >> '[800000,800001)'",
        "SELECT id FROM s.t WHERE r && '[79190,79195)'",
    ] {
        let along_key = ids(&mut s, q, "t_pkey");
        assert_eq!(ids(&mut s, q, "t_r"), along_key, "{q}");
    }
    assert_eq!(
        ids(
            &mut s,
            "SELECT id FROM s.t WHERE r >> '[800000,800001)'",
            "t_r"
        ),
        [1, 30000]
    );
}

#[test]
fn a_gist_strand_on_a_multirange_keeps_the_span_of_its_ranges() {
    let mut s = fresh(&spans_of("gist-multirange", 20480, "int4multirange"));
    let blocks = blocks_of(&s.blocks().unwrap(), "t_r", "r.list.element.lower").len();
    for q in [
        "SELECT id FROM s.t WHERE r && '{[100004,100005)}'",
        "SELECT id FROM s.t WHERE r && '[100003,100004)'::int4range",
        "SELECT id FROM s.t WHERE r @> 100004",
        "SELECT id FROM s.t WHERE r <@ '{[100000,100010)}'",
        "SELECT id FROM s.t WHERE r <@ '{}'",
        "SELECT id FROM s.t WHERE r >> '{[204000,204010)}'",
        "SELECT id FROM s.t WHERE r -|- '{[100005,100010)}'",
        // The last range of the last row of all.
        "SELECT id FROM s.t WHERE r && '{[204794,204795)}'",
    ] {
        let along_key = ids(&mut s, q, "t_pkey");
        assert_eq!(ids(&mut s, q, "t_r"), along_key, "{q}");
    }
    let (_, along) = s
        .ask(
            "SELECT id FROM s.t WHERE r && '{[100004,100005)}'",
            Some("t_r"),
        )
        .unwrap();
    assert!(blocks > 2, "{blocks}");
    assert!(along.blocks_read < blocks as u64, "{along:?}");
    assert_eq!(
        ids(
            &mut s,
            "SELECT id FROM s.t WHERE r && '{[100004,100005)}'",
            "t_r"
        )
        .len(),
        1
    );
    let last = "SELECT id FROM s.t WHERE r && '{[204794,204795)}'";
    assert_eq!(ids(&mut s, last, "t_r").len(), 1);
}

#[test]
fn a_condition_by_a_gist_operator_is_refused_in_a_partial_index_s_predicate() {
    let got = outcomes(
        "CREATE SCHEMA s; CREATE TABLE s.t (id int PRIMARY KEY, r int4range);\n\
         CREATE INDEX t_r ON s.t (id) WHERE r && '[1,2)';",
    );
    assert!(
        got[1]
            .as_deref()
            .is_some_and(|e| e.contains("is not read in a predicate")),
        "{got:?}"
    );
}

#[test]
fn an_exclude_constraint_is_read_as_postgresql_19_reads_it() {
    let table = "CREATE SCHEMA s; CREATE TABLE s.b (id int PRIMARY KEY, room int, during int4range, \
                 EXCLUDE USING gist (during WITH &&), \
                 CONSTRAINT b_code EXCLUDE USING btree (room WITH =) WHERE (id > 100));";
    let reading = Catalog::read(table).unwrap();
    assert_eq!(
        reading.statements[0].refused, None,
        "{:?}",
        reading.statements
    );
    let t = &reading.catalog.tables[0];
    let during = t.order("b_during_excl").unwrap();
    assert_eq!(during.method.name(), "gist");
    assert_eq!(during.exclusion.as_ref().unwrap().operators, ["&&"]);
    let code = t.order("b_code").unwrap();
    assert_eq!(code.predicate.as_deref(), Some("(id > 100)"));
    // Each refusal as PostgreSQL 19 RC1 gave it (28819, 2026-10-31).
    for (constraint, why) in [
        (
            "EXCLUDE USING gist (r WITH <<)",
            "operator <<(anyrange,anyrange) is not commutative",
        ),
        (
            "EXCLUDE USING gist (r WITH @>)",
            "operator @>(anyrange,anyrange) is not commutative",
        ),
        (
            "EXCLUDE (a WITH <>)",
            "operator <>(integer,integer) is not a member of operator family \"integer_ops\"",
        ),
        (
            "EXCLUDE USING gin (a WITH =)",
            "access method \"gin\" does not support exclusion constraints",
        ),
        (
            "EXCLUDE USING gist (a WITH =)",
            "data type integer has no default operator class for access method \"gist\"",
        ),
    ] {
        let got = outcomes(&format!(
            "CREATE SCHEMA s; CREATE TABLE s.c (id int PRIMARY KEY, a int, r int4range, {constraint});"
        ));
        let e = got[0]
            .as_deref()
            .unwrap_or_else(|| panic!("{constraint} was taken"));
        assert!(e.contains(why), "{constraint}: {e}");
    }
}

#[cfg(feature = "standalone")]
fn refused(w: &mut Snail, sql: &str, why: &str) {
    let e = w
        .execute(sql)
        .err()
        .unwrap_or_else(|| panic!("{sql} was taken"))
        .to_string();
    assert!(e.contains(why), "{sql}: {e}");
}

#[cfg(feature = "standalone")]
fn bookings(name: &str) -> std::path::PathBuf {
    let catalog = Catalog::parse(
        "CREATE SCHEMA s; CREATE TABLE s.b (id int PRIMARY KEY, room int, during int4range, \
         EXCLUDE USING gist (during WITH &&), \
         CONSTRAINT b_code EXCLUDE USING btree (room WITH =) WHERE (id > 100));",
    )
    .unwrap();
    let path = tmp(&format!("{name}.snail"));
    write(&path, &catalog, &[]).unwrap();
    path
}

#[cfg(feature = "standalone")]
#[test]
fn an_exclude_refuses_a_row_whose_values_conflict_with_another_s() {
    let mut w = Snail::open_writing(&bookings("exclude")).unwrap();
    // Each statement taken or refused as PostgreSQL 19 RC1 took or refused it (28819, 2026-10-31).
    let conflict = "conflicting key value violates exclusion constraint";
    for sql in [
        "INSERT INTO s.b (id, room, during) VALUES (1, 1, '[1,5)')",
        "INSERT INTO s.b (id, room, during) VALUES (2, 1, '[5,9)')",
        "INSERT INTO s.b (id, room, during) VALUES (4, 2, 'empty'), (5, 2, NULL)",
        "UPDATE s.b SET during = '[10,20)' WHERE id = 1",
        "INSERT INTO s.b (id, room, during) VALUES (6, 3, '[1,5)')",
        "INSERT INTO s.b (id, room, during) VALUES (101, 7, NULL), (2101, 8, NULL)",
        "DELETE FROM s.b WHERE id = 2",
        "INSERT INTO s.b (id, room, during) VALUES (7, 3, '[6,9)')",
    ] {
        w.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    for (sql, name) in [
        (
            "INSERT INTO s.b (id, room, during) VALUES (3, 2, '[4,6)')",
            "b_during_excl",
        ),
        (
            "UPDATE s.b SET during = '[3,4)' WHERE id = 7",
            "b_during_excl",
        ),
        (
            "INSERT INTO s.b (id, room, during) VALUES (102, 7, NULL)",
            "b_code",
        ),
        (
            "INSERT INTO s.b (id, room, during) VALUES (103, 9, NULL), (104, 9, NULL)",
            "b_code",
        ),
        (
            "INSERT INTO s.b (id, room, during) VALUES (8, 4, '[15,16)')",
            "b_during_excl",
        ),
    ] {
        refused(&mut w, sql, &format!("{conflict} \"{name}\""));
    }
    // A room repeated outside the predicate conflicts with nothing.
    w.execute("INSERT INTO s.b (id, room, during) VALUES (9, 7, NULL)")
        .unwrap();
    drop(w);
    let mut s = fresh(&bookings_path("exclude"));
    assert_eq!(
        ids(&mut s, "SELECT id FROM s.b WHERE id > 0", "b_pkey"),
        [1, 4, 5, 6, 7, 9, 101, 2101]
    );
}

#[cfg(feature = "standalone")]
fn bookings_path(name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}.snail"))
}

#[cfg(feature = "standalone")]
#[test]
fn a_writer_whose_value_conflicts_with_another_s_uncommitted_waits_and_is_refused_as_postgresql_refuses_it(
) {
    use std::time::Duration;
    let path = bookings("exclude-two");
    let mut one = Snail::open_writing(&path).unwrap();
    one.execute("INSERT INTO s.b (id, room, during) VALUES (1, 1, '[1,5)')")
        .unwrap();
    for (end, taken) in [("ROLLBACK", true), ("COMMIT", false)] {
        // The first writer holds a range in a transaction; a second writer's range overlapping it
        // waits for it, then is refused where it committed and taken where it rolled back.
        one.execute("BEGIN").unwrap();
        one.execute("INSERT INTO s.b (id, room, during) VALUES (10, 1, '[100,200)')")
            .unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let p = path.clone();
        std::thread::spawn(move || {
            let mut two = Snail::open_writing(&p).unwrap();
            let r = two
                .execute("INSERT INTO s.b (id, room, during) VALUES (20, 2, '[150,160)')")
                .map_err(|e| e.to_string());
            if r.is_ok() {
                two.execute("DELETE FROM s.b WHERE id = 20").unwrap();
            }
            tx.send(r).unwrap();
        });
        std::thread::sleep(Duration::from_millis(400));
        assert!(rx.try_recv().is_err(), "the second writer did not wait");
        one.execute(end).unwrap();
        let r = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(r.is_ok(), taken, "{end}: {r:?}");
        if !taken {
            assert!(r
                .unwrap_err()
                .contains("conflicting key value violates exclusion constraint \"b_during_excl\""),);
        }
    }
}

#[test]
fn write_and_a_snail_of_format_eight_refuse_an_exclude_by_name() {
    let catalog = Catalog::parse(
        "CREATE SCHEMA s; CREATE TABLE s.t (a integer NOT NULL, b integer, PRIMARY KEY (a), \
         CONSTRAINT t_b EXCLUDE (b WITH =));",
    )
    .unwrap();
    let rows = Rows {
        columns: vec![Values::Int(vec![Some(1)]), Values::Int(vec![Some(2)])],
    };
    let e = write(
        &tmp("exclude-write.snail"),
        &catalog,
        &[("s.t".into(), rows)],
    )
    .err()
    .unwrap()
    .to_string();
    assert!(
        e.contains("write does not hold its rows to the EXCLUDE t_b: give them by INSERT"),
        "{e}"
    );
    let e = warren_snail::write::write_format_eight(
        &tmp("eight-exclude.snail"),
        &catalog,
        &[],
        weather::span(),
    )
    .err()
    .unwrap()
    .to_string();
    assert!(
        e.contains("s.t: the EXCLUDE t_b, which a snail of format 8"),
        "{e}"
    );
}

#[cfg(feature = "standalone")]
#[test]
fn writers_of_one_exclude_s_strand_take_turns_whatever_blocks_they_write() {
    use std::time::Duration;
    let catalog =
        Catalog::parse("CREATE SCHEMA s; CREATE TABLE s.w (id int PRIMARY KEY, room int, EXCLUDE (room WITH =));")
            .unwrap();
    let path = tmp("exclude-turns.snail");
    write(&path, &catalog, &[]).unwrap();
    let mut one = Snail::open_writing(&path).unwrap();
    let rows: Vec<String> = (0..2048).map(|i| format!("({i}, {})", i * 2)).collect();
    one.execute(&format!(
        "INSERT INTO s.w (id, room) VALUES {}",
        rows.join(", ")
    ))
    .unwrap();
    let blocks = blocks_of(&fresh(&path).blocks().unwrap(), "w_room_excl", "room");
    assert!(blocks.len() >= 2, "{blocks:?}");
    // The first writer holds a room in the strand's first block and a key in the table's last; the
    // second writes a room in the strand's last block and a key in the table's first, blocks the
    // first holds none of, and waits for the first all the same.
    one.execute("BEGIN").unwrap();
    one.execute("INSERT INTO s.w (id, room) VALUES (5001, 1)")
        .unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let p = path.clone();
    std::thread::spawn(move || {
        let mut two = Snail::open_writing(&p).unwrap();
        tx.send(
            two.execute("INSERT INTO s.w (id, room) VALUES (-1, 4095)")
                .map_err(|e| e.to_string()),
        )
        .unwrap();
    });
    std::thread::sleep(Duration::from_millis(400));
    assert!(rx.try_recv().is_err(), "the second writer did not wait");
    one.execute("COMMIT").unwrap();
    rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
}
