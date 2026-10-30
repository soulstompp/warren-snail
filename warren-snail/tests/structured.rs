//! The structured types and the rest: json and jsonb, inet and cidr, tsvector and tsquery held as a
//! text PostgreSQL's input reads back, each that PostgreSQL orders keyed by its class; the geometric
//! types and aclitem held as groups of leaves; the types only a server makes refused from a literal.

mod weather;

use arrow::array::RecordBatch;
use warren_snail::catalog::Catalog;
use warren_snail::read::Snail;
use warren_snail::values::Values;
use warren_snail::write::Rows;
use weather::{ints64, texts, tmp, write};

/// A snail of one table, `s.t`, its rows given column by column.
fn snail_of(name: &str, ddl: &str, columns: Vec<Values>) -> std::path::PathBuf {
    let path = tmp(&format!("{name}.snail"));
    let catalog = Catalog::parse(ddl).unwrap();
    write(&path, &catalog, &[("s.t".into(), Rows { columns })]).unwrap();
    path
}

fn ids(b: &RecordBatch) -> Vec<i64> {
    ints64(b.column(0).as_ref()).into_iter().flatten().collect()
}

/// Ids 1 on, each value written in its type's input spelling, read by it.
fn rows(values: &[&str], read: fn(&str) -> String) -> Vec<Values> {
    vec![
        Values::Int((1..=values.len() as i64).map(Some).collect()),
        Values::Text(values.iter().map(|v| Some(read(v))).collect()),
    ]
}

fn jsonb(s: &str) -> String {
    warren_snail::jsonb::jsonb_in(s).unwrap()
}

fn inet(s: &str) -> String {
    warren_snail::inet::inet_in(s, false).unwrap()
}

#[test]
fn a_jsonb_level_holds_its_values_in_jsonb_cmp_s_order_equal_classes_on_one_place() {
    // PostgreSQL's ORDER BY over these (the fact ledger, jsonb): [] null "a" 1 false true [1] [[]]
    // {} {"a": 1}; 1.0 equal to 1.
    let values = [
        r#"{"a":1}"#,
        "[[]]",
        "true",
        "1",
        "[]",
        "{}",
        "false",
        r#""a""#,
        "null",
        "[1]",
        "1.0",
    ];
    let path = snail_of(
        "jsonb-level",
        "CREATE TABLE s.t (id integer NOT NULL, v jsonb, PRIMARY KEY (id)); \
         CREATE INDEX t_v ON s.t (v, id);",
        rows(&values, jsonb),
    );
    let mut snail = Snail::open(&path).unwrap();
    let (b, _) = snail
        .ask("SELECT id, v FROM s.t ORDER BY v, id", Some("t_v"))
        .unwrap();
    assert_eq!(ids(&b), [5, 9, 8, 4, 11, 7, 3, 10, 2, 6, 1]);
    let (b, _) = snail
        .ask(
            "SELECT id FROM s.t WHERE v = '1.00' ORDER BY id",
            Some("t_v"),
        )
        .unwrap();
    assert_eq!(ids(&b), [4, 11]);
    // Each value comes out as its own text, 1.0 and 1 apart.
    let (b, _) = snail
        .ask("SELECT id, v FROM s.t WHERE id = 11", None)
        .unwrap();
    assert_eq!(texts(b.column(1).as_ref()), [Some("1.0".to_string())]);
}

#[test]
fn an_inet_level_orders_as_network_cmp_and_a_cidr_reads_a_literal_as_inet() {
    // PostgreSQL's ORDER BY over these (the fact ledger, inet).
    let values = [
        "10.0.0.0/8",
        "10.0.0.0",
        "9.255.255.255",
        "10.0.0.0/7",
        "10.0.0.1/8",
        "11.0.0.0/8",
        "::",
        "0.0.0.0/0",
    ];
    let path = snail_of(
        "inet-level",
        "CREATE TABLE s.t (id integer NOT NULL, v inet, PRIMARY KEY (id)); \
         CREATE INDEX t_v ON s.t (v, id);",
        rows(&values, inet),
    );
    let mut snail = Snail::open(&path).unwrap();
    let (b, _) = snail
        .ask("SELECT id, v FROM s.t ORDER BY v, id", Some("t_v"))
        .unwrap();
    assert_eq!(ids(&b), [8, 3, 4, 1, 5, 2, 6, 7]);
    // Beside a cidr a literal is read as an inet, so host bits right of its mask are taken.
    let cidrs = ["10.0.0.0/8", "10.0.0.1/32"];
    let path = snail_of(
        "cidr-literal",
        "CREATE TABLE s.t (id integer NOT NULL, v cidr, PRIMARY KEY (id));",
        rows(&cidrs, |s| warren_snail::inet::inet_in(s, true).unwrap()),
    );
    let (b, _) = Snail::open(&path)
        .unwrap()
        .ask(
            "SELECT id FROM s.t WHERE v < '10.0.0.1/8' ORDER BY id",
            None,
        )
        .unwrap();
    assert_eq!(ids(&b), [1]);
}

#[test]
fn a_tsquery_holds_a_text_that_reads_back_into_the_same_value_and_prints_as_tsqueryout() {
    let held = warren_snail::tsearch::tsquery_in("a & (b & c)").unwrap();
    assert_eq!(held, "'a' & ( 'b' & 'c' )");
    assert_eq!(warren_snail::tsearch::tsquery_out(&held), "'a' & 'b' & 'c'");
    let values = ["a & (b & c)", "a & b & c"];
    let path = snail_of(
        "tsquery-held",
        "CREATE TABLE s.t (id integer NOT NULL, v tsquery, PRIMARY KEY (id));",
        rows(&values, |s| warren_snail::tsearch::tsquery_in(s).unwrap()),
    );
    let mut snail = Snail::open(&path).unwrap();
    // As in PostgreSQL, the two print alike and are not equal.
    let (b, _) = snail
        .ask(
            "WITH t AS (SELECT id, v FROM s.t) SELECT id, v::text AS x FROM t \
             WHERE v = 'a & b & c' ORDER BY id",
            None,
        )
        .unwrap();
    assert_eq!(ids(&b), [2]);
    let (b, _) = snail
        .ask(
            "WITH t AS (SELECT id, v FROM s.t) SELECT id, v::text AS x FROM t ORDER BY id",
            None,
        )
        .unwrap();
    assert_eq!(
        texts(b.column(1).as_ref()),
        [
            Some("'a' & 'b' & 'c'".to_string()),
            Some("'a' & 'b' & 'c'".to_string())
        ]
    );
}

#[cfg(feature = "standalone")]
#[test]
fn a_geometric_value_is_its_float8_leaves_each_bit_kept_out_and_back() {
    use arrow::array::{Array, Float64Array, StructArray};
    let path = tmp("point-bits.snail");
    let catalog =
        Catalog::parse("CREATE TABLE s.t (id integer NOT NULL, v point, PRIMARY KEY (id));")
            .unwrap();
    let empty = Rows {
        columns: vec![Values::Int(vec![]), Values::Utf8(vec![])],
    };
    write(&path, &catalog, &[("s.t".into(), empty)]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute("INSERT INTO s.t (id, v) VALUES (1, '(-0,NaN)'), (2, '(1.5,-2)'), (3, NULL)")
        .unwrap();
    let blocks = Snail::open(&path).unwrap().blocks().unwrap();
    let mut leaves: Vec<&str> = blocks
        .iter()
        .filter(|r| r.order == "t_pkey" && r.column.starts_with("v."))
        .map(|r| r.column.as_str())
        .collect();
    leaves.dedup();
    assert_eq!(leaves, ["v.x", "v.y"]);
    let dir = tmp("point-bits-out");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let outs = Snail::open(&path).unwrap().out(&dir).unwrap();
    let back = tmp("point-bits-back.snail");
    let file = std::fs::File::open(&outs[0].file).unwrap();
    let batches: Vec<RecordBatch> =
        parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file)
            .unwrap()
            .build()
            .unwrap()
            .map(Result::unwrap)
            .collect();
    let rows = Rows::from_batches(&catalog.tables[0], &batches).unwrap();
    write(&back, &catalog, &[("s.t".into(), rows)]).unwrap();
    let (b, _) = Snail::open(&back)
        .unwrap()
        .ask("SELECT id, v FROM s.t ORDER BY id", None)
        .unwrap();
    let s = b.column(1).as_any().downcast_ref::<StructArray>().unwrap();
    let x = s.column(0).as_any().downcast_ref::<Float64Array>().unwrap();
    let y = s.column(1).as_any().downcast_ref::<Float64Array>().unwrap();
    assert_eq!(x.value(0).to_bits(), (-0.0f64).to_bits());
    assert!(y.value(0).is_nan());
    assert_eq!((x.value(1), y.value(1)), (1.5, -2.0));
    assert!(s.is_null(2));
}

#[cfg(feature = "standalone")]
#[test]
fn a_type_only_a_server_makes_is_refused_from_a_literal_and_held_null_only_where_none_is_copied() {
    let ddl =
        "CREATE TABLE s.t (id integer NOT NULL, n pg_node_tree, g gtsvector, PRIMARY KEY (id));";
    let catalog = Catalog::parse(ddl).unwrap();
    // A node tree copied in is held; a gtsvector copied in is refused, as no table holds one.
    let path = tmp("server-made.snail");
    let tree = "({QUERY :commandType 1})";
    write(
        &path,
        &catalog,
        &[(
            "s.t".into(),
            Rows {
                columns: vec![
                    Values::Int(vec![Some(1), Some(2)]),
                    Values::Text(vec![Some(tree.into()), None]),
                    Values::Utf8(vec![None, None]),
                ],
            },
        )],
    )
    .unwrap();
    let (b, _) = Snail::open(&path)
        .unwrap()
        .ask("SELECT id, n FROM s.t ORDER BY n, id", None)
        .unwrap();
    assert_eq!(texts(b.column(1).as_ref()), [Some(tree.to_string()), None]);
    let mut w = Snail::open_writing(&path).unwrap();
    let e = w
        .execute("INSERT INTO s.t (id, n) VALUES (3, 'x')")
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("cannot accept a value of type pg_node_tree"),
        "{e}"
    );
    let e = w
        .execute("INSERT INTO s.t (id, g) VALUES (3, 'x')")
        .unwrap_err()
        .to_string();
    assert!(e.contains("cannot accept a value of type gtsvector"), "{e}");
    // Nor does a file's column give one.
    let batch = RecordBatch::try_from_iter([
        (
            "id",
            std::sync::Arc::new(arrow::array::Int32Array::from(vec![4])) as arrow::array::ArrayRef,
        ),
        (
            "n",
            std::sync::Arc::new(arrow::array::StringArray::from(vec![None::<&str>])) as _,
        ),
        (
            "g",
            std::sync::Arc::new(arrow::array::BinaryArray::from(vec![&b"x"[..]])) as _,
        ),
    ])
    .unwrap();
    let e = match Rows::from_batches(&catalog.tables[0], &[batch]) {
        Err(e) => e.to_string(),
        Ok(_) => String::new(),
    };
    assert!(e.contains("cannot accept a value of type gtsvector"), "{e}");
}

#[cfg(feature = "standalone")]
#[test]
fn an_aclitem_is_three_leaves_its_roles_their_oids_and_names_no_role() {
    let path = tmp("aclitem.snail");
    let catalog =
        Catalog::parse("CREATE TABLE s.t (id integer NOT NULL, v aclitem, PRIMARY KEY (id));")
            .unwrap();
    let empty = Rows {
        columns: vec![Values::Int(vec![]), Values::Text(vec![])],
    };
    write(&path, &catalog, &[("s.t".into(), empty)]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute("INSERT INTO s.t (id, v) VALUES (1, '3373=r*w/6171'), (2, ' =arwdDxtm/ 10 ')")
        .unwrap();
    let e = w
        .execute("INSERT INTO s.t (id, v) VALUES (3, 'pg_monitor=r/10')")
        .unwrap_err()
        .to_string();
    assert!(e.contains("role \"pg_monitor\""), "{e}");
    let mut snail = Snail::open(&path).unwrap();
    let (b, _) = snail
        .ask(
            "WITH t AS (SELECT id, v FROM s.t) SELECT id, v::text AS x FROM t ORDER BY id",
            None,
        )
        .unwrap();
    assert_eq!(
        texts(b.column(1).as_ref()),
        [
            Some("3373=r*w/6171".to_string()),
            Some("=arwdDxtm/10".to_string())
        ]
    );
    let blocks = snail.blocks().unwrap();
    let mut leaves: Vec<&str> = blocks
        .iter()
        .filter(|r| r.column.starts_with("v."))
        .map(|r| r.column.as_str())
        .collect();
    leaves.dedup();
    assert_eq!(leaves, ["v.grantee", "v.grantor", "v.privileges"]);
}

#[test]
fn a_type_postgresql_neither_orders_nor_groups_is_refused_both_ways() {
    let path = snail_of(
        "json-refused",
        "CREATE TABLE s.t (id integer NOT NULL, v json, PRIMARY KEY (id));",
        rows(&[r#"{"a":1}"#, "[1, 2]"], str::to_string),
    );
    let mut snail = Snail::open(&path).unwrap();
    for q in [
        "SELECT v, count(*) FROM s.t GROUP BY v",
        "WITH t AS (SELECT id, v FROM s.t) SELECT v, count(*) FROM t GROUP BY v",
        "SELECT id FROM s.t WHERE v = '[1, 2]'",
        "WITH t AS (SELECT id, v FROM s.t) SELECT id FROM t WHERE v = '[1, 2]'",
        // Refused before any value is read, a NULL beside it as well.
        "WITH t AS (SELECT id, v FROM s.t) SELECT id FROM t WHERE v = NULL",
        "SELECT id FROM s.t ORDER BY v",
    ] {
        assert!(snail.ask(q, None).is_err(), "{q}");
    }
    // Its operators read it as written.
    let (b, _) = snail
        .ask(
            "WITH t AS (SELECT id, v FROM s.t) SELECT id, v -> 'a' AS x FROM t ORDER BY id",
            None,
        )
        .unwrap();
    assert_eq!(texts(b.column(1).as_ref()), [Some("1".to_string()), None]);
}
