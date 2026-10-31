//! The index methods other than the B-tree: each read from its DDL as PostgreSQL 19 reads it, refused
//! in its words where PostgreSQL refuses it, and held as a strand of its own kind. A hash strand is a
//! strand on its key's B-tree order, `=` one place of it; aclitem, cid and xid, which have a hash and
//! no B-tree, on their bytes.

mod weather;

use arrow::array::{Array, Int32Array};
use warren_snail::catalog::Catalog;
use warren_snail::read::{BlockRow, Snail};
use warren_snail::scalars::unsigned_key;
use warren_snail::values::Values;
use warren_snail::write::Rows;
use weather::{exact, fresh, tmp, write};

/// Each statement's refusal, or None where it is taken.
fn outcomes(ddl: &str) -> Vec<Option<String>> {
    Catalog::read(ddl)
        .unwrap()
        .statements
        .into_iter()
        .map(|s| s.refused)
        .collect()
}

#[test]
fn an_index_of_each_method_is_refused_as_postgresql_19_refuses_it() {
    let table = "CREATE TABLE s.t (id int PRIMARY KEY, r int4range, a int[], v tsvector, x xid, \
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
        "CREATE TABLE s.t (id int PRIMARY KEY, tx text, x xid, g aclitem, n cidr);\n\
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
        "CREATE TABLE s.t (id int NOT NULL, k int NOT NULL, x xid NOT NULL, PRIMARY KEY (id)); \
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
        "CREATE TABLE s.t (id int PRIMARY KEY, c cid, g aclitem, x xid); \
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
            "CREATE TABLE s.t (a integer NOT NULL, b integer, PRIMARY KEY (a)); \
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
}
