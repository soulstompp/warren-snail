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
        "CREATE SCHEMA s; CREATE TABLE s.t (id integer NOT NULL, v jsonb, PRIMARY KEY (id)); \
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
        "CREATE SCHEMA s; CREATE TABLE s.t (id integer NOT NULL, v inet, PRIMARY KEY (id)); \
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
        "CREATE SCHEMA s; CREATE TABLE s.t (id integer NOT NULL, v cidr, PRIMARY KEY (id));",
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
        "CREATE SCHEMA s; CREATE TABLE s.t (id integer NOT NULL, v tsquery, PRIMARY KEY (id));",
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
        Catalog::parse("CREATE SCHEMA s; CREATE TABLE s.t (id integer NOT NULL, v point, PRIMARY KEY (id));")
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
        "CREATE SCHEMA s; CREATE TABLE s.t (id integer NOT NULL, n pg_node_tree, g gtsvector, PRIMARY KEY (id));";
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

/// Bytes from their hex digits.
fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

#[test]
fn the_planner_s_statistics_are_answered_as_their_bytes_their_output_refused_by_name() {
    use arrow::array::{Array, ArrayRef, BinaryArray, Int32Array};
    use std::sync::Arc;
    let ddl = "CREATE SCHEMA s; CREATE TABLE s.t (id integer NOT NULL, nd pg_ndistinct, dp pg_dependencies, \
               mc pg_mcv_list, PRIMARY KEY (id));";
    let catalog = Catalog::parse(ddl).unwrap();
    // The statistics PostgreSQL 19 RC1 kept on two columns (28819, 2026-10-31), and some bytes for
    // the list, whose output is bytea's.
    let nd = unhex("a4bf52a3010000000100000000000000000024400200000001000200");
    let dp =
        unhex("2c9a54b40100000002000000000000000000f03f020001000200000000000000f03f020002000100");
    let mc = unhex("c251a6e1010000000a000000");
    let binary = |v: &[u8]| Arc::new(BinaryArray::from(vec![Some(v), None])) as ArrayRef;
    let batch = RecordBatch::try_from_iter([
        ("id", Arc::new(Int32Array::from(vec![1, 2])) as ArrayRef),
        ("nd", binary(&nd)),
        ("dp", binary(&dp)),
        ("mc", binary(&mc)),
    ])
    .unwrap();
    let rows = Rows::from_batches(&catalog.tables[0], &[batch]).unwrap();
    let path = tmp("statistics.snail");
    write(&path, &catalog, &[("s.t".into(), rows)]).unwrap();
    let mut s = Snail::open(&path).unwrap();
    let step = "WITH q AS (SELECT id, nd, dp, mc FROM s.t)";
    let (b, _) = s
        .ask(&format!("{step} SELECT nd, dp FROM q WHERE id = 1"), None)
        .unwrap();
    let bytes = |c: usize| {
        let a = b.column(c).as_any().downcast_ref::<BinaryArray>().unwrap();
        a.value(0).to_vec()
    };
    assert_eq!((bytes(0), bytes(1)), (nd.clone(), dp.clone()));
    let (b, _) = s
        .ask(
            &format!("{step} SELECT mc::text AS x FROM q WHERE id = 1"),
            None,
        )
        .unwrap();
    assert_eq!(
        texts(b.column(0).as_ref()),
        [Some("\\xc251a6e1010000000a000000".to_string())]
    );
    for (e, ty) in [
        ("nd::text", "pg_ndistinct"),
        ("dp::varchar", "pg_dependencies"),
        ("nd::name", "pg_ndistinct"),
        ("concat(nd)", "pg_ndistinct"),
        ("format('%s', dp)", "pg_dependencies"),
        ("to_json(nd)", "pg_ndistinct"),
        ("json_agg(dp)", "pg_dependencies"),
        ("nd = 'x'", "pg_ndistinct"),
        ("dp LIKE 'x%'", "pg_dependencies"),
    ] {
        let e = s
            .ask(&format!("{step} SELECT {e} AS x FROM q"), None)
            .unwrap_err()
            .to_string();
        assert!(
            e.starts_with(&format!("the output of {ty} is not written")),
            "{e}"
        );
    }
    for e in ["nd::bytea", "count(nd)", "dp IS NULL", "encode(dp, 'hex')"] {
        assert!(
            s.ask(&format!("{step} SELECT {e} AS x FROM q"), None)
                .is_ok(),
            "{e}"
        );
    }
    // Compared and joined as PostgreSQL 19 RC1 resolves them (28819, 2026-11-01): none of their
    // own, each cast to text through its output beside a string, to bytea beside bytea.
    for (e, refused) in [
        (
            "nd = nd",
            "operator is not unique: pg_ndistinct = pg_ndistinct",
        ),
        (
            "nd = dp",
            "operator is not unique: pg_ndistinct = pg_dependencies",
        ),
        (
            "nd || 'x'",
            "operator is not unique: pg_ndistinct || unknown",
        ),
        (
            "mc || 'x'::text",
            "operator is not unique: pg_mcv_list || text",
        ),
        (
            "mc IS DISTINCT FROM mc",
            "operator is not unique: pg_mcv_list = pg_mcv_list",
        ),
        ("nd = 1", "operator does not exist: pg_ndistinct = integer"),
    ] {
        let got = s
            .ask(&format!("{step} SELECT {e} AS x FROM q"), None)
            .unwrap_err()
            .to_string();
        assert_eq!(got, weather::hinted(refused), "{e}");
    }
    let one = |s: &mut Snail, e: &str| -> Option<String> {
        let (b, _) = s
            .ask(
                &format!("{step} SELECT ({e})::text AS x FROM q WHERE id = 1"),
                None,
            )
            .unwrap();
        texts(b.column(0).as_ref()).remove(0)
    };
    for (e, want) in [
        ("mc = 'x'", "false"),
        ("mc = '\\xc251a6e1010000000a000000'", "true"),
        ("mc = '\\xC251A6E1010000000A000000'", "false"),
        ("mc = '\\xC251A6E1010000000A000000'::bytea", "true"),
        ("nd = '\\x01'::bytea", "false"),
        ("mc < 'x'", "true"),
        ("mc ~ 'x'", "true"),
        ("mc IN ('x', 'y')", "false"),
        ("mc IS DISTINCT FROM 'x'", "true"),
        ("mc || '\\x01'::bytea", "\\xc251a6e1010000000a00000001"),
    ] {
        assert_eq!(one(&mut s, e).as_deref(), Some(want), "{e}");
    }
    // Asked of the table itself, the pipeline reads it.
    let (b, _) = s.ask("SELECT id FROM s.t WHERE mc <> 'x'", None).unwrap();
    assert_eq!(ints64(b.column(0).as_ref()), [Some(1)]);
    // Nor is one written into a string as its output, as an INSERT … SELECT assigns it.
    #[cfg(feature = "standalone")]
    {
        let mut w = Snail::open_writing(&path).unwrap();
        w.execute("CREATE TABLE s.x (id integer PRIMARY KEY, t text)")
            .unwrap();
        let e = w
            .execute("INSERT INTO s.x SELECT id, nd FROM s.t")
            .unwrap_err()
            .to_string();
        assert!(
            e.starts_with("the output of pg_ndistinct is not written"),
            "{e}"
        );
        w.execute("INSERT INTO s.x SELECT id, mc FROM s.t").unwrap();
    }
}

#[cfg(feature = "standalone")]
#[test]
fn an_aclitem_is_three_leaves_its_roles_their_oids_and_names_no_role() {
    let path = tmp("aclitem.snail");
    let catalog =
        Catalog::parse("CREATE SCHEMA s; CREATE TABLE s.t (id integer NOT NULL, v aclitem, PRIMARY KEY (id));")
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
        "CREATE SCHEMA s; CREATE TABLE s.t (id integer NOT NULL, v json, PRIMARY KEY (id));",
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

/// Each row's values of a pipeline's answer as text, a column after the first each.
fn texts_of(snail: &mut Snail, q: &str) -> Vec<Vec<Option<String>>> {
    let (b, _) = snail.ask(q, None).unwrap();
    (1..b.num_columns())
        .map(|c| texts(b.column(c).as_ref()))
        .collect()
}

/// A table's snail written out to Parquet, then written back from its file by `write`.
#[cfg(feature = "standalone")]
fn out_and_back(path: &std::path::Path, catalog: &Catalog, name: &str) -> std::path::PathBuf {
    let dir = tmp(&format!("{name}-out"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // Opened to write, so a block holding a tail is pruned on its way out.
    let outs = Snail::open_writing(path).unwrap().out(&dir).unwrap();
    let file = std::fs::File::open(&outs[0].file).unwrap();
    let batches: Vec<RecordBatch> =
        parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file)
            .unwrap()
            .build()
            .unwrap()
            .map(Result::unwrap)
            .collect();
    let rows = Rows::from_batches(&catalog.tables[0], &batches).unwrap();
    let back = tmp(&format!("{name}-back.snail"));
    write(&back, catalog, &[("s.t".into(), rows)]).unwrap();
    back
}

#[cfg(feature = "standalone")]
#[test]
fn a_path_and_a_polygon_are_trees_of_their_own_the_polygon_s_bounding_box_kept_out_and_back() {
    let path = tmp("path-polygon.snail");
    let catalog = Catalog::parse(
        "CREATE SCHEMA s; CREATE TABLE s.t (id integer NOT NULL, p path, g polygon, PRIMARY KEY (id)); \
         CREATE TABLE s.u (id integer NOT NULL, p path, g polygon, PRIMARY KEY (id));",
    )
    .unwrap();
    let empty = Rows {
        columns: vec![
            Values::Int(vec![]),
            Values::Utf8(vec![]),
            Values::Utf8(vec![]),
        ],
    };
    write(&path, &catalog, &[("s.t".into(), empty)]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute(
        "INSERT INTO s.t (id, p, g) VALUES (1, '[(0,0),(-0,NaN)]', '((0,0),(1,1),(1,0))'), \
         (2, '((1,2))', CAST(CAST('(0,0),(-0,-0)' AS box) AS polygon)), (3, NULL, NULL)",
    )
    .unwrap();
    // Onto the tail of the block that holds the rows.
    w.execute("INSERT INTO s.t (id, p, g) VALUES (4, '((5,5),(9,9),(1,1))', '((0,0))')")
        .unwrap();
    drop(w);
    let blocks = Snail::open(&path).unwrap().blocks().unwrap();
    let mut leaves: Vec<&str> = blocks
        .iter()
        .filter(|r| r.order == "t_pkey" && r.column != "id")
        .map(|r| r.column.as_str())
        .collect();
    leaves.dedup();
    assert_eq!(
        leaves,
        [
            "p.closed",
            "p.points.list.element.x",
            "p.points.list.element.y",
            "g.bound.high_x",
            "g.bound.high_y",
            "g.bound.low_x",
            "g.bound.low_y",
            "g.points.list.element.x",
            "g.points.list.element.y"
        ]
    );
    // PostgreSQL 18.6's answers on 28818, 2026-10-30, to the same statements: a polygon made from a
    // box keeps the box, `(-0,-0),(0,0)`, where its points make `(-0,-0),(-0,-0)`.
    let q = "WITH t AS (SELECT id, p, g FROM s.t) \
             SELECT id, p::text AS x, CAST(g AS box)::text AS y, g::text AS z FROM t ORDER BY id";
    let want = |s: &str| Some(s.to_string());
    let expected = vec![
        vec![
            want("[(0,0),(-0,NaN)]"),
            want("((1,2))"),
            None,
            want("((5,5),(9,9),(1,1))"),
        ],
        vec![
            want("(1,1),(0,0)"),
            want("(-0,-0),(0,0)"),
            None,
            want("(0,0),(0,0)"),
        ],
        vec![
            want("((0,0),(1,1),(1,0))"),
            want("((-0,-0),(-0,0),(0,0),(0,-0))"),
            None,
            want("((0,0))"),
        ],
    ];
    assert_eq!(texts_of(&mut Snail::open(&path).unwrap(), q), expected);
    let back = out_and_back(&path, &catalog, "path-polygon");
    assert_eq!(texts_of(&mut Snail::open(&back).unwrap(), q), expected);
    // A path compares by its points' count.
    let (b, _) = Snail::open(&path)
        .unwrap()
        .ask("SELECT id FROM s.t WHERE p = '[(7,7)]' ORDER BY id", None)
        .unwrap();
    assert_eq!(ids(&b), [2]);
    // Taken from an answer by an INSERT … SELECT, each value as its leaves give it.
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute("INSERT INTO s.u (id, p, g) SELECT id, p, g FROM s.t WHERE id = 2")
        .unwrap();
    drop(w);
    let q = "WITH t AS (SELECT id, p, g FROM s.u) \
             SELECT id, p::text AS x, CAST(g AS box)::text AS y FROM t";
    assert_eq!(
        texts_of(&mut Snail::open(&path).unwrap(), q),
        [vec![want("((1,2))")], vec![want("(-0,-0),(0,0)")]]
    );
}

#[test]
fn a_snapshot_is_its_xmin_its_xmax_and_its_transactions_compared_by_nothing() {
    let ddl = "CREATE SCHEMA s; CREATE TABLE s.t (id integer NOT NULL, v pg_snapshot, w txid_snapshot, \
               PRIMARY KEY (id));";
    let held = |v: &[Option<&str>]| Values::Text(v.iter().map(|s| s.map(str::to_string)).collect());
    let path = snail_of(
        "snapshot",
        ddl,
        vec![
            Values::Int(vec![Some(1), Some(2), Some(3)]),
            held(&[
                Some("10:20:12,15"),
                Some("18446744073709551615:18446744073709551615:"),
                None,
            ]),
            held(&[Some("1:1:"), None, Some("5:9:6")]),
        ],
    );
    let mut snail = Snail::open(&path).unwrap();
    let blocks = snail.blocks().unwrap();
    let mut leaves: Vec<&str> = blocks
        .iter()
        .filter(|r| r.column.starts_with("v."))
        .map(|r| r.column.as_str())
        .collect();
    leaves.dedup();
    assert_eq!(leaves, ["v.xmin", "v.xmax", "v.xip.list.element"]);
    let q = "WITH t AS (SELECT id, v, w FROM s.t) \
             SELECT id, v::text AS x, w::text AS y FROM t ORDER BY id";
    assert_eq!(
        texts_of(&mut snail, q),
        [
            vec![
                Some("10:20:12,15".to_string()),
                Some("18446744073709551615:18446744073709551615:".to_string()),
                None
            ],
            vec![Some("1:1:".to_string()), None, Some("5:9:6".to_string())]
        ]
    );
    // PostgreSQL has no operator, order or equality for either, nor a cast between the two.
    for q in [
        "SELECT id FROM s.t WHERE v = '1:1:'",
        "WITH t AS (SELECT id, v FROM s.t) SELECT id FROM t WHERE v = '1:1:'",
        "WITH t AS (SELECT id, v FROM s.t) SELECT id FROM t WHERE v < '1:2:'",
        "SELECT v, count(*) FROM s.t GROUP BY v",
        "SELECT id FROM s.t ORDER BY w",
        "SELECT min(v) FROM s.t",
        "WITH t AS (SELECT id, v FROM s.t) SELECT id, CAST(v AS txid_snapshot) AS x FROM t",
        // Nor a join's family.
        "SELECT a.id FROM s.t AS a JOIN s.t AS b ON a.v = b.v",
    ] {
        assert!(snail.ask(q, None).is_err(), "{q}");
    }
    let (b, _) = snail
        .ask(
            "WITH t AS (SELECT id FROM s.t) \
             SELECT id, CAST(CAST('1:3:2,2' AS pg_snapshot) AS text) AS x FROM t WHERE id = 1",
            None,
        )
        .unwrap();
    assert_eq!(texts(b.column(1).as_ref()), [Some("1:3:2".to_string())]);
}

#[cfg(feature = "standalone")]
#[test]
fn an_array_of_a_type_with_no_equality_refuses_one_only_of_two_arrays_of_one_shape() {
    let path = tmp("carried-arrays.snail");
    let catalog = Catalog::parse(
        "CREATE SCHEMA s; CREATE TABLE s.t (id integer NOT NULL, p point[], b box[], a aclitem[], q path[], \
         PRIMARY KEY (id));",
    )
    .unwrap();
    let empty = Rows {
        columns: (0..5)
            .map(|c| Values::new(catalog.tables[0].columns[c].ty))
            .collect(),
    };
    write(&path, &catalog, &[("s.t".into(), empty)]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute(
        "INSERT INTO s.t (id, p, b, a, q) VALUES \
         (1, '{\"(1,2)\",\"(0,0)\"}', '{(1,1),(0,0);(2,2),(1,1)}', '{=r/10,10=rw*/10}', \
          '{\"[(0,0),(1,1)]\",\"((0,0))\"}'), \
         (2, '{}', '{}', '{}', '{}'), (3, NULL, NULL, NULL, NULL)",
    )
    .unwrap();
    drop(w);
    let mut snail = Snail::open(&path).unwrap();
    // PostgreSQL 18.6's text of the same values on 28818, 2026-10-30: box's array between `;`.
    let q = "WITH t AS (SELECT id, p, b, a, q FROM s.t) \
             SELECT id, p::text AS w, b::text AS x, a::text AS y, q::text AS z FROM t ORDER BY id";
    let want = |s: &str| Some(s.to_string());
    assert_eq!(
        texts_of(&mut snail, q),
        [
            vec![want("{\"(1,2)\",\"(0,0)\"}"), want("{}"), None],
            vec![want("{(1,1),(0,0);(2,2),(1,1)}"), want("{}"), None],
            vec![want("{=r/10,10=rw*/10}"), want("{}"), None],
            vec![want("{\"[(0,0),(1,1)]\",\"((0,0))\"}"), want("{}"), None],
        ]
    );
    // A pipeline answers no row with a column of no type.
    let found = |snail: &mut Snail, q: &str| {
        snail.ask(q, None).map(|(b, _)| {
            if b.num_rows() == 0 {
                Vec::new()
            } else {
                ids(&b)
            }
        })
    };
    // Of no other shape than each row's, = is answered; of one row's, refused, as array_eq looks
    // for its elements' equality only there.
    assert_eq!(
        found(
            &mut snail,
            "SELECT id FROM s.t WHERE p = '{\"(9,9)\"}' ORDER BY id"
        )
        .unwrap(),
        Vec::<i64>::new()
    );
    assert_eq!(
        found(
            &mut snail,
            "WITH t AS (SELECT id, p FROM s.t) \
             SELECT id FROM t WHERE p IS DISTINCT FROM '{\"(9,9)\"}' ORDER BY id"
        )
        .unwrap(),
        [1, 2, 3]
    );
    for q in [
        "SELECT id FROM s.t WHERE p = '{}'",
        "SELECT id FROM s.t WHERE p <> '{\"(9,9)\",\"(8,8)\"}'",
        "SELECT id FROM s.t WHERE q @> '{}'",
        "WITH t AS (SELECT id, p FROM s.t) SELECT id FROM t WHERE p IS DISTINCT FROM '{}'",
        "SELECT p, count(*) FROM s.t GROUP BY p",
        "WITH t AS (SELECT id, p FROM s.t) SELECT p, count(*) FROM t GROUP BY p",
        "SELECT id FROM s.t ORDER BY a",
        "SELECT id FROM s.t WHERE a < '{}'",
        "SELECT min(p) FROM s.t",
        "WITH t AS (SELECT id, a FROM s.t) SELECT id, a + '=r/10' AS x FROM t WHERE id = 1",
        "WITH t AS (SELECT id, a FROM s.t) SELECT id, a @> '=r/10' AS x FROM t",
    ] {
        assert!(snail.ask(q, None).is_err(), "{q}");
    }
    // aclitem's equality groups its arrays and finds them.
    assert_eq!(
        found(
            &mut snail,
            "SELECT id FROM s.t WHERE a = '{=r/10,10=rw*/10}' ORDER BY id"
        )
        .unwrap(),
        [1]
    );
    let (b, _) = snail
        .ask(
            "SELECT min(id) AS i, count(*) AS n FROM s.t GROUP BY a",
            None,
        )
        .unwrap();
    assert_eq!(b.num_rows(), 3);
    // aclcontains: the grantee and grantor, every privilege and grant option asked held.
    let q = "WITH t AS (SELECT id, a FROM s.t) SELECT id, a @> '10=w/10' AS x, \
             a @> '10=w*/10' AS y, a @> '10=d/10' AS z FROM t WHERE id = 1";
    let (b, _) = snail.ask(q, None).unwrap();
    let truth = |c: usize| {
        b.column(c)
            .as_any()
            .downcast_ref::<arrow::array::BooleanArray>()
            .unwrap()
            .value(0)
    };
    assert_eq!((truth(1), truth(2), truth(3)), (true, true, false));
    // min of one value is that value, as PostgreSQL compares none.
    let (b, _) = snail
        .ask("SELECT min(p)::text AS x FROM s.t WHERE id = 1", None)
        .unwrap();
    assert_eq!(
        texts(b.column(0).as_ref()),
        [Some("{\"(1,2)\",\"(0,0)\"}".to_string())]
    );
}
