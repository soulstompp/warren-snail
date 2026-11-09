//! The operator classes PostgreSQL's catalog gives a key through its own type or a cast it holds as
//! bytes: B-tree `cidr_ops` on an inet and a cidr, the order `inet_ops` gives; text's classes on a
//! pg_node_tree and bytea's on the planner's statistics types, by each method; and the collation
//! those types' columns take. Each expected fact is PostgreSQL's, read on 18.6 and 19 RC1
//! (`ledgers/ddlfix.md`).

mod weather;

use arrow::array::{ArrayRef, BinaryArray, Int32Array, RecordBatch};
use std::sync::Arc;
use warren_snail::catalog::Catalog;
use warren_snail::read::Snail;
use warren_snail::values::Values;
use warren_snail::write::Rows;
use weather::{exact, texts, tmp, write};

fn fact(c: &Catalog, kind: &str, object: &str, name: &str) -> Option<String> {
    c.facts()
        .into_iter()
        .find(|f| f.kind == kind && f.object == object && f.fact == name)
        .map(|f| f.value)
}

fn index_fact(column: &str, ty: &str, using: &str) -> Result<String, String> {
    let ddl = format!(
        "CREATE SCHEMA s; CREATE TABLE s.t (id integer PRIMARY KEY, {column} {ty});\nCREATE INDEX i ON s.t {using};"
    );
    Catalog::parse(&ddl)
        .map(|c| fact(&c, "index", "s.t.i", "opclasses").unwrap())
        .map_err(|e| e.to_string())
}

#[test]
fn cidr_ops_is_read_on_an_inet_and_a_cidr() {
    assert_eq!(index_fact("c", "cidr", "USING btree (c cidr_ops)").unwrap(), "cidr_ops");
    assert_eq!(index_fact("c", "inet", "USING btree (c cidr_ops)").unwrap(), "cidr_ops");
    assert_eq!(index_fact("c", "cidr", "(c cidr_ops DESC NULLS LAST)").unwrap(), "cidr_ops");
    assert_eq!(index_fact("c", "cidr", "USING btree (c)").unwrap(), "inet_ops");
    assert_eq!(index_fact("c", "cidr", "USING hash (c cidr_ops)").unwrap(), "cidr_ops");
    let e = index_fact("c", "text", "USING btree (c cidr_ops)").unwrap_err();
    assert!(e.contains("cidr_ops is not read on c"), "{e}");
}

#[test]
fn an_exclusion_on_cidr_ops_is_of_the_family_of_inet_ops() {
    let e = Catalog::parse(
        "CREATE SCHEMA s; CREATE TABLE s.t (id integer PRIMARY KEY, c cidr, EXCLUDE USING gist (c inet_ops WITH <>));\n\
         CREATE TABLE s.u (id integer PRIMARY KEY, c cidr, EXCLUDE USING btree (c cidr_ops WITH =));",
    );
    assert!(e.is_ok(), "{:?}", e.err());
    let e = Catalog::parse(
        "CREATE SCHEMA s; CREATE TABLE s.u (id integer PRIMARY KEY, c cidr, EXCLUDE USING btree (c cidr_ops WITH <>));",
    )
    .err()
    .unwrap()
    .to_string();
    assert!(e.contains("is not a member of operator family \"network_ops\""), "{e}");
}

#[test]
fn a_strand_on_cidr_ops_holds_the_order_inet_ops_gives() {
    let ddl = "CREATE SCHEMA s; CREATE TABLE s.t (id integer PRIMARY KEY, c cidr, a inet);\n\
        CREATE INDEX by_c ON s.t USING btree (c cidr_ops);\n\
        CREATE INDEX by_a ON s.t USING btree (a cidr_ops);";
    let catalog = Catalog::parse(ddl).unwrap();
    let path = tmp("classes-cidr.snail");
    let held = |x: &[&str]| Values::Text(x.iter().map(|s| Some(s.to_string())).collect());
    write(
        &path,
        &catalog,
        &[(
            "s.t".into(),
            Rows {
                columns: vec![
                    Values::Int(vec![Some(1), Some(2), Some(3), Some(4)]),
                    held(&["::/0", "192.168.0.0/24", "10.0.0.0/16", "10.0.0.0/8"]),
                    held(&["::1", "192.168.0.1", "10.0.0.5/16", "10.0.0.0/8"]),
                ],
            },
        )],
    )
    .unwrap();
    let mut s = Snail::open(&path).unwrap();
    let (b, round) = s.ask("SELECT c FROM s.t ORDER BY c", Some("by_c")).unwrap();
    exact("cidr along its strand", &round);
    assert_eq!(
        texts(b.column(0).as_ref()),
        ["10.0.0.0/8", "10.0.0.0/16", "192.168.0.0/24", "::/0"].map(|x| Some(x.to_string()))
    );
    let (b, round) = s.ask("SELECT a FROM s.t ORDER BY a", Some("by_a")).unwrap();
    exact("inet along its strand", &round);
    assert_eq!(
        texts(b.column(0).as_ref()),
        ["10.0.0.0/8", "10.0.0.5/16", "192.168.0.1", "::1"].map(|x| Some(x.to_string()))
    );
}

#[test]
fn a_node_tree_takes_text_s_classes_by_each_method() {
    let nt = "pg_node_tree";
    for (using, class) in [
        ("USING btree (n)", "text_ops"),
        ("USING hash (n)", "text_ops"),
        ("USING spgist (n)", "text_ops"),
        ("USING brin (n)", "text_minmax_ops"),
        ("USING btree (n text_pattern_ops)", "text_pattern_ops"),
        ("USING btree (n varchar_ops)", "varchar_ops"),
        ("USING btree (n varchar_pattern_ops)", "varchar_pattern_ops"),
        ("USING hash (n text_pattern_ops)", "text_pattern_ops"),
        ("USING hash (n varchar_ops)", "varchar_ops"),
        ("USING hash (n varchar_pattern_ops)", "varchar_pattern_ops"),
        ("USING spgist (n text_ops)", "text_ops"),
        ("USING brin (n text_bloom_ops)", "text_bloom_ops"),
    ] {
        assert_eq!(index_fact("n", nt, using).as_deref(), Ok(class), "{using}");
    }
    for using in ["USING gist (n)", "USING gin (n)"] {
        let e = index_fact("n", nt, using).unwrap_err();
        assert!(
            e.contains("data type pg_node_tree has no default operator class for access method"),
            "{e}"
        );
    }
}

#[test]
fn the_planner_s_statistics_take_bytea_s_classes() {
    for ty in ["pg_ndistinct", "pg_dependencies", "pg_mcv_list"] {
        for (using, class) in [
            ("USING btree (n)", "bytea_ops"),
            ("USING hash (n)", "bytea_ops"),
            ("USING brin (n)", "bytea_minmax_ops"),
            ("USING brin (n bytea_bloom_ops)", "bytea_bloom_ops"),
            ("USING btree (n bytea_ops)", "bytea_ops"),
        ] {
            assert_eq!(index_fact("n", ty, using).as_deref(), Ok(class), "{ty} {using}");
        }
        let e = index_fact("n", ty, "USING spgist (n)").unwrap_err();
        assert!(e.contains("has no default operator class"), "{e}");
    }
    for ty in ["pg_brin_bloom_summary", "pg_brin_minmax_multi_summary"] {
        for using in ["USING btree (n)", "USING hash (n)", "USING brin (n)"] {
            let e = index_fact("n", ty, using).unwrap_err();
            assert!(e.contains(&format!("data type {ty} has no default operator class")), "{e}");
        }
    }
}

#[test]
fn a_brin_on_a_node_tree_rules_out_no_row_it_holds() {
    let tree = |n: i64| format!("({{QUERY :commandType {n}}})");
    let n = 4096i64;
    for class in ["text_minmax_ops", "text_bloom_ops"] {
        let ddl = format!(
            "CREATE SCHEMA s; CREATE TABLE s.t (id integer PRIMARY KEY, n pg_node_tree);\n\
             CREATE INDEX b ON s.t USING brin (n {class});"
        );
        let catalog = Catalog::parse(&ddl).unwrap();
        let path = tmp(&format!("classes-tree-{class}.snail"));
        write(
            &path,
            &catalog,
            &[(
                "s.t".into(),
                Rows {
                    columns: vec![
                        Values::Int((0..n).map(Some).collect()),
                        Values::Text((0..n).map(|i| Some(tree(i))).collect()),
                    ],
                },
            )],
        )
        .unwrap();
        let mut s = Snail::open(&path).unwrap();
        for k in [0, 1, n / 2, n - 1] {
            let q = format!("SELECT id FROM s.t WHERE n = '{}'", tree(k));
            let (b, round) = s.ask(&q, None).unwrap_or_else(|e| panic!("{q}: {e}"));
            exact(&q, &round);
            assert_eq!(b.num_rows(), 1, "{class}: {q}");
        }
    }
}

#[test]
fn a_brin_on_the_planner_s_statistics_rules_out_no_row_it_holds() {
    let n = 4096i64;
    let bytes = |i: i64| i.to_be_bytes().to_vec();
    for class in ["bytea_minmax_ops", "bytea_bloom_ops"] {
        let ddl = format!(
            "CREATE SCHEMA s; CREATE TABLE s.t (id integer PRIMARY KEY, n pg_ndistinct);\n\
             CREATE INDEX b ON s.t USING brin (n {class});"
        );
        let catalog = Catalog::parse(&ddl).unwrap();
        let path = tmp(&format!("classes-stats-{class}.snail"));
        let held: Vec<Vec<u8>> = (0..n).map(bytes).collect();
        let batch = RecordBatch::try_from_iter([
            ("id", Arc::new(Int32Array::from_iter_values(0..n as i32)) as ArrayRef),
            (
                "n",
                Arc::new(BinaryArray::from_iter_values(held.iter().map(Vec::as_slice))) as ArrayRef,
            ),
        ])
        .unwrap();
        let rows = Rows::from_batches(&catalog.tables[0], &[batch]).unwrap();
        write(&path, &catalog, &[("s.t".into(), rows)]).unwrap();
        let mut s = Snail::open(&path).unwrap();
        let hex: String = bytes(n / 2).iter().map(|b| format!("{b:02x}")).collect();
        let q = format!("SELECT id FROM s.t WHERE n = '\\x{hex}'::bytea");
        let (b, _) = s.ask(&q, None).unwrap_or_else(|e| panic!("{q}: {e}"));
        assert_eq!(b.num_rows(), 1, "{class}: {q}");
    }
}

#[test]
fn the_server_s_own_types_take_the_collation_postgresql_gives_them() {
    let c = Catalog::parse(
        "CREATE SCHEMA s; CREATE TABLE s.t (id integer PRIMARY KEY, a pg_node_tree, b pg_ndistinct, c pg_dependencies, \
         d pg_mcv_list, e pg_brin_bloom_summary, f pg_brin_minmax_multi_summary, g gtsvector, \
         h pg_node_tree COLLATE \"C\");",
    )
    .unwrap();
    for column in ["a", "b", "c", "d", "e", "f"] {
        assert_eq!(
            fact(&c, "column", &format!("s.t.{column}"), "collation").as_deref(),
            Some("default"),
            "{column}"
        );
    }
    assert_eq!(fact(&c, "column", "s.t.g", "collation"), None);
    assert_eq!(fact(&c, "column", "s.t.id", "collation"), None);
    assert_eq!(fact(&c, "column", "s.t.h", "collation").as_deref(), Some("C"));
}
