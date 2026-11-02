//! GiST and SP-GiST on the plane: a point, a box, a polygon or a circle, cut on x's and y's sortable
//! bits interleaved, one a rung, x first, a shape by its bounding box's centre, each block keeping its
//! bounding box, a block whose box a condition's value misses never read.

mod weather;

use arrow::array::{Array, Int64Array};
use parquet::data_type::ByteArray;
use warren_snail::catalog::Catalog;
use warren_snail::geo::{geo_bytes, geo_in, GeoKind};
use warren_snail::read::{BlockRow, Snail};
use warren_snail::values::Values;
use warren_snail::write::Rows;
use weather::{exact, fresh, tmp, write};

/// A column of shapes, each read from its text; `None` a NULL.
fn shapes(kind: GeoKind, texts: impl Iterator<Item = Option<String>>) -> Values {
    Values::Utf8(
        texts
            .map(|t| t.map(|t| ByteArray::from(geo_bytes(&geo_in(kind, &t).unwrap()))))
            .collect(),
    )
}

fn blocks_of(rows: &[BlockRow], order: &str, column: &str) -> Vec<u64> {
    rows.iter()
        .filter(|r| r.order == order && r.column == column)
        .map(|r| r.block)
        .collect()
}

/// The ids a question returns along an order, sorted; its round read exactly what it predicted.
fn ids(s: &mut Snail, sql: &str, order: &str) -> Vec<i64> {
    let (b, round) = s
        .ask(sql, Some(order))
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    exact(sql, &round);
    let a = b.column(0).as_any().downcast_ref::<Int64Array>().unwrap();
    let mut v: Vec<i64> = (0..a.len()).map(|i| a.value(i)).collect();
    v.sort();
    v
}

/// 2^20 points on a 1,024 by 1,024 grid, each coordinate from 1,024 on, so that a double's bits run
/// as an integer's over the grid's 10.
fn grid(name: &str) -> std::path::PathBuf {
    let catalog = Catalog::parse(
        "CREATE TABLE s.t (id bigint NOT NULL, p point, PRIMARY KEY (id)); \
         CREATE INDEX t_p ON s.t USING gist (p);",
    )
    .unwrap();
    let n = 1024i64 * 1024;
    let id = Values::Int((0..n).map(Some).collect());
    let p = shapes(
        GeoKind::Point,
        (0..n).map(|i| {
            // Row i's cell lies far from row i + 1's on the table's key.
            let j = (i * 7919) % n;
            Some(format!("({},{})", 1024 + j / 1024, 1024 + j % 1024))
        }),
    );
    let path = tmp(&format!("{name}.snail"));
    write(
        &path,
        &catalog,
        &[(
            "s.t".into(),
            Rows {
                columns: vec![id, p],
            },
        )],
    )
    .unwrap();
    path
}

#[test]
fn a_grid_of_points_is_cut_one_bit_a_rung_and_a_box_reads_the_blocks_the_arithmetic_gives() {
    let mut s = fresh(&grid("plane-grid"));
    // snail.nu plane: one bit a rung 2,048 blocks of 16 by 32 cells.
    let rows = s.blocks().unwrap();
    assert_eq!(blocks_of(&rows, "t_p", "p.x").len(), 2048);
    assert_eq!(blocks_of(&rows, "t_p", "p.y").len(), 2048);
    // snail.nu plane: a 100 by 100 box, cells 10 to 109 of each side, reads 28 blocks, 14,336 rows.
    let q = "SELECT id FROM s.t WHERE p <@ box '((1034,1034),(1133,1133))'";
    let (b, along) = s.ask(q, Some("t_p")).unwrap();
    exact(q, &along);
    assert_eq!(b.num_rows(), 10_000);
    assert_eq!(along.blocks_read, 28, "{along:?}");
    // Three leaves a block, id and p's two, each 512 values and its page's own bytes: two disk pages.
    assert_eq!(along.column_pages_read, 6 * 28, "{along:?}");
    assert_eq!(along.rows_walked, 14_336, "{along:?}");
    // x's bit first: a box 100 cells wide and 16 tall reads 7 blocks of 16 by 32, where 16 by 32 the
    // other way about would read 8.
    let wide = "SELECT id FROM s.t WHERE p <@ box '((1034,1034),(1133,1049))'";
    let (b, across) = s.ask(wide, Some("t_p")).unwrap();
    exact(wide, &across);
    assert_eq!((b.num_rows(), across.blocks_read), (1_600, 7), "{across:?}");
    // The counts name no place of a level whose key is no value of its column.
    assert_eq!(s.level_counts("s.t", "p", 1_000).unwrap(), None);
    // Asked with no order named, the strand's fewer pages are chosen.
    let (c, chosen) = s.ask(q, None).unwrap();
    assert_eq!(c.num_rows(), 10_000);
    assert_eq!(chosen.order, "t_p", "{chosen:?}");
    let (d, whole) = s.ask(q, Some("t_pkey")).unwrap();
    exact(q, &whole);
    assert_eq!(d.num_rows(), 10_000);
    assert_eq!(whole.blocks_read, 2048, "{whole:?}");
}

/// Each statement's refusal, or None where it is taken.
fn outcomes(ddl: &str) -> Vec<Option<String>> {
    Catalog::read(ddl)
        .unwrap()
        .statements
        .into_iter()
        .map(|s| s.refused)
        .collect()
}

/// A table's DDL and a statement on it: an EXCLUDE's as a clause of the table, as the DDL reads one.
fn spelled(table: &str, statement: &str) -> String {
    match statement.strip_prefix("ALTER TABLE s.t ADD ") {
        Some(clause) => format!("{},\n{clause});", table.trim_end_matches(");")),
        None => format!("{table} {statement};"),
    }
}

#[test]
fn an_index_on_the_plane_is_read_or_refused_as_postgresql_reads_it() {
    let table =
        "CREATE TABLE s.t (id int PRIMARY KEY, p point, b box, c circle, g polygon, l lseg, \
                 pa path, li line);";
    // Each refusal as PostgreSQL 18.6 and 19 RC1 gave it (28818, 28819, 2026-11-02).
    for (statement, why) in [
        (
            "CREATE UNIQUE INDEX e ON s.t USING gist (p)",
            "access method \"gist\" does not support unique indexes",
        ),
        (
            "CREATE INDEX e ON s.t USING gist (p DESC)",
            "access method \"gist\" does not support ASC/DESC options",
        ),
        (
            "CREATE INDEX e ON s.t USING gist (p NULLS FIRST)",
            "access method \"gist\" does not support NULLS FIRST/LAST options",
        ),
        (
            "CREATE INDEX e ON s.t USING gist (p box_ops)",
            "operator class \"box_ops\" does not accept data type point",
        ),
        (
            "CREATE INDEX e ON s.t USING gist (p quad_point_ops)",
            "operator class \"quad_point_ops\" does not exist for access method \"gist\"",
        ),
        (
            "CREATE INDEX e ON s.t USING spgist (p point_ops)",
            "operator class \"point_ops\" does not exist for access method \"spgist\"",
        ),
        (
            "CREATE INDEX e ON s.t USING gist (l)",
            "data type lseg has no default operator class for access method \"gist\"",
        ),
        (
            "CREATE INDEX e ON s.t USING gist (pa)",
            "data type path has no default operator class for access method \"gist\"",
        ),
        (
            "CREATE INDEX e ON s.t USING gist (li)",
            "data type line has no default operator class for access method \"gist\"",
        ),
        (
            "CREATE INDEX e ON s.t USING spgist (c)",
            "data type circle has no default operator class for access method \"spgist\"",
        ),
        (
            "CREATE INDEX e ON s.t USING spgist (p, id)",
            "access method \"spgist\" does not support multicolumn indexes",
        ),
        (
            "CREATE INDEX e ON s.t USING gist (p) WITH (buffering = maybe)",
            "invalid value for enum option \"buffering\": maybe",
        ),
        (
            "CREATE INDEX e ON s.t USING gist (p) WITH (fillfactor = 9)",
            "value 9 out of bounds for option \"fillfactor\"",
        ),
        (
            "CREATE INDEX e ON s.t USING spgist (p) WITH (buffering = auto)",
            "unrecognized parameter \"buffering\"",
        ),
        (
            "CREATE INDEX e ON s.t USING gist (p point_ops (x = 1))",
            "operator class point_ops has no options",
        ),
        (
            "CREATE INDEX e ON s.t USING hash (p)",
            "data type point has no default operator class for access method \"hash\"",
        ),
        (
            "ALTER TABLE s.t ADD EXCLUDE USING gist (p WITH =)",
            "operator does not exist: point = point",
        ),
        (
            "ALTER TABLE s.t ADD EXCLUDE USING gist (g WITH =)",
            "operator does not exist: polygon = polygon",
        ),
        (
            "ALTER TABLE s.t ADD EXCLUDE USING gist (p WITH <->)",
            "operator <->(point,point) is not a member of operator family \"point_ops\"",
        ),
        (
            "ALTER TABLE s.t ADD EXCLUDE USING gist (p WITH <>)",
            "operator <>(point,point) is not a member of operator family \"point_ops\"",
        ),
        (
            "ALTER TABLE s.t ADD EXCLUDE USING gist (p WITH <<)",
            "operator <<(point,point) is not commutative",
        ),
        (
            "ALTER TABLE s.t ADD EXCLUDE USING gist (p WITH <^)",
            "operator <^(point,point) is not commutative",
        ),
        (
            "ALTER TABLE s.t ADD EXCLUDE USING gist (b WITH =)",
            "operator =(box,box) is not a member of operator family \"box_ops\"",
        ),
        (
            "ALTER TABLE s.t ADD EXCLUDE USING gist (b WITH <@)",
            "operator <@(box,box) is not commutative",
        ),
        (
            "ALTER TABLE s.t ADD EXCLUDE USING gist (b WITH &<)",
            "operator &<(box,box) is not commutative",
        ),
        (
            "ALTER TABLE s.t ADD EXCLUDE USING gist (c WITH <>)",
            "operator <>(circle,circle) is not a member of operator family \"circle_ops\"",
        ),
        (
            "ALTER TABLE s.t ADD EXCLUDE USING gist (g WITH <->)",
            "operator <->(polygon,polygon) is not a member of operator family \"poly_ops\"",
        ),
    ] {
        let o = outcomes(&spelled(table, statement));
        let got = o
            .iter()
            .flatten()
            .next()
            .cloned()
            .unwrap_or_else(|| panic!("{statement} taken"));
        assert!(got.contains(why), "{statement}: {got}");
    }
    for statement in [
        "CREATE INDEX e ON s.t USING gist (p)",
        "CREATE INDEX e ON s.t USING gist (p point_ops)",
        "CREATE INDEX e ON s.t USING gist (b)",
        "CREATE INDEX e ON s.t USING gist (b box_ops)",
        "CREATE INDEX e ON s.t USING gist (c)",
        "CREATE INDEX e ON s.t USING gist (c circle_ops)",
        "CREATE INDEX e ON s.t USING gist (g)",
        "CREATE INDEX e ON s.t USING gist (g poly_ops)",
        "CREATE INDEX e ON s.t USING spgist (p)",
        "CREATE INDEX e ON s.t USING spgist (p quad_point_ops)",
        "CREATE INDEX e ON s.t USING spgist (p kd_point_ops)",
        "CREATE INDEX e ON s.t USING spgist (b)",
        "CREATE INDEX e ON s.t USING spgist (b box_ops)",
        "CREATE INDEX e ON s.t USING spgist (g)",
        "CREATE INDEX e ON s.t USING spgist (g poly_ops)",
        "CREATE INDEX e ON s.t USING gist (p, b, c, g)",
        "CREATE INDEX e ON s.t USING gist (p) INCLUDE (id)",
        "CREATE INDEX e ON s.t USING spgist (p) INCLUDE (id)",
        "CREATE INDEX e ON s.t USING gist (p) WITH (buffering = auto, fillfactor = 50)",
        "CREATE INDEX e ON s.t USING spgist (p kd_point_ops) WITH (fillfactor = 50)",
        "CREATE INDEX e ON s.t USING gist (p) WHERE id > 0",
        "ALTER TABLE s.t ADD EXCLUDE USING gist (p WITH ~=)",
        "ALTER TABLE s.t ADD EXCLUDE USING gist (b WITH &&)",
        "ALTER TABLE s.t ADD EXCLUDE USING gist (b WITH ~=)",
        "ALTER TABLE s.t ADD EXCLUDE USING spgist (b WITH &&)",
        "ALTER TABLE s.t ADD EXCLUDE USING gist (c WITH &&)",
        "ALTER TABLE s.t ADD EXCLUDE USING gist (g WITH &&)",
        "ALTER TABLE s.t ADD EXCLUDE USING spgist (g WITH &&)",
        "ALTER TABLE s.t ADD EXCLUDE USING spgist (p WITH ~=)",
    ] {
        let o = outcomes(&spelled(table, statement));
        assert!(o.iter().all(Option::is_none), "{statement}: {o:?}");
    }
    let e = Catalog::parse(&format!("{table} CREATE INDEX e ON s.t USING btree (p);"))
        .err()
        .unwrap()
        .to_string();
    assert!(
        e.contains("data type point has no default operator class for access method \"btree\""),
        "{e}"
    );
    // Clustered on: GiST, and never SP-GiST.
    let o = outcomes(&format!(
        "{table} CREATE INDEX c ON s.t USING gist (p); ALTER TABLE s.t CLUSTER ON c;"
    ));
    assert_eq!(o[2], None);
    let o = outcomes(&format!(
        "{table} CREATE INDEX c ON s.t USING spgist (p); ALTER TABLE s.t CLUSTER ON c;"
    ));
    assert!(
        o[2].as_deref().unwrap().contains(
            "cannot cluster on index \"c\" because access method does not support clustering"
        ),
        "{o:?}"
    );
}

/// `n` rows of a table of one shape, `f` each row's text, with an index of each method named.
fn table_of(
    name: &str,
    ty: &str,
    kind: GeoKind,
    methods: &[&str],
    n: i64,
    f: impl Fn(i64) -> Option<String>,
) -> std::path::PathBuf {
    let indexes: String = methods
        .iter()
        .map(|m| format!("CREATE INDEX t_{m} ON s.t USING {m} (v); "))
        .collect();
    let catalog = Catalog::parse(&format!(
        "CREATE TABLE s.t (id bigint NOT NULL, v {ty}, PRIMARY KEY (id)); {indexes}"
    ))
    .unwrap();
    let id = Values::Int((0..n).map(Some).collect());
    let v = shapes(kind, (0..n).map(f));
    let path = tmp(&format!("{name}.snail"));
    write(
        &path,
        &catalog,
        &[(
            "s.t".into(),
            Rows {
                columns: vec![id, v],
            },
        )],
    )
    .unwrap();
    path
}

/// A cell of a 128 by 128 grid, row i's lying far from row i + 1's on the table's key.
fn cell(i: i64) -> (i64, i64) {
    let j = (i * 7919) % 16384;
    (1024 + 8 * (j / 128), 1024 + 8 * (j % 128))
}

/// Every operator of a column's operator classes, against each value given, along every order: the
/// same rows as the table's key finds them, each read its predicted pages.
fn operators_agree(s: &mut Snail, orders: &[&str], ops: &[&str], values: &[&str]) {
    for op in ops {
        for value in values {
            let q = format!("SELECT id FROM s.t WHERE v {op} {value}");
            let along_key = ids(s, &q, "t_pkey");
            for order in orders {
                assert_eq!(ids(s, &q, order), along_key, "{q} along {order}");
            }
        }
    }
}

const SHAPE_OPS: [&str; 12] = [
    "<<", "&<", "&&", "&>", ">>", "~=", "@>", "<@", "&<|", "<<|", "|>>", "|&>",
];

#[test]
fn every_point_operator_holds_along_each_strand_of_a_point_and_rules_blocks_out() {
    let path = table_of(
        "plane-points",
        "point",
        GeoKind::Point,
        &["gist", "spgist"],
        16384,
        |i| {
            // Every hundredth row NULL, and a NaN and each infinity among them.
            if i % 100 == 0 {
                return None;
            }
            Some(match i {
                1 => "(NaN,1100)".into(),
                2 => "(1100,NaN)".into(),
                3 => "(Infinity,1100)".into(),
                4 => "(-Infinity,1100)".into(),
                5 => "(1100,Infinity)".into(),
                _ => format!("({},{})", cell(i).0, cell(i).1),
            })
        },
    );
    let mut s = fresh(&path);
    let orders = ["t_gist", "t_spgist"];
    operators_agree(
        &mut s,
        &orders,
        &["<<", ">>", "~=", "<<|", "|>>", "<^", ">^"],
        &[
            "'(1040,1040)'",
            "point '(2000,1500)'",
            "'(NaN,1100)'",
            "'(Infinity,1100)'",
            "'(-Infinity,1100)'",
            "point(1100, 1200)",
        ],
    );
    operators_agree(
        &mut s,
        &orders,
        &["<@"],
        &[
            "box '((1040,1040),(1100,1100))'",
            "'((1040,1040),(1100,1100))'::box",
            "box '((-Infinity,1100),(Infinity,1100))'",
            "box '((3000,3000),(4000,4000))'",
            "polygon '((1040,1040),(1100,1040),(1100,1100))'",
            "circle '<(1100,1100),40>'",
            "circle '<(1100,1100),0>'",
        ],
    );
    // A value a statement draws is no value to rule a block out by.
    let drawn =
        "SELECT id FROM s.t WHERE v <@ box(point(random() * 0 + 1040, 1040), point(1100, 1100))";
    assert!(s.ask(drawn, Some("t_gist")).is_err());
    assert_eq!(
        ids(
            &mut s,
            "SELECT id FROM s.t WHERE v <@ box(point(1040, 1040), point(1100, 1100))",
            "t_gist"
        ),
        ids(
            &mut s,
            "SELECT id FROM s.t WHERE v <@ box '((1040,1040),(1100,1100))'",
            "t_pkey"
        ),
    );
    // The value on the left, through the operator's other form.
    for (q, r) in [
        (
            "SELECT id FROM s.t WHERE box '((1040,1040),(1100,1100))' @> v",
            "SELECT id FROM s.t WHERE v <@ box '((1040,1040),(1100,1100))'",
        ),
        (
            "SELECT id FROM s.t WHERE '(1040,1040)' >> v",
            "SELECT id FROM s.t WHERE v << '(1040,1040)'",
        ),
        (
            "SELECT id FROM s.t WHERE '(2000,2000)' >> v",
            "SELECT id FROM s.t WHERE v << '(2000,2000)'",
        ),
        (
            "SELECT id FROM s.t WHERE '(1040,1040)' |>> v",
            "SELECT id FROM s.t WHERE v <<| '(1040,1040)'",
        ),
        (
            "SELECT id FROM s.t WHERE '(1040,1040)' >^ v",
            "SELECT id FROM s.t WHERE v <^ '(1040,1040)'",
        ),
    ] {
        for order in orders {
            assert_eq!(ids(&mut s, q, order), ids(&mut s, r, "t_pkey"), "{q}");
        }
    }
    // A narrow box reads few of the blocks; a NaN's and a box past every point read those holding one.
    let blocks = blocks_of(&s.blocks().unwrap(), "t_gist", "v.x").len() as u64;
    assert!(blocks >= 16, "{blocks}");
    let q = "SELECT id FROM s.t WHERE v <@ box '((1040,1040),(1064,1064))'";
    let (_, narrow) = s.ask(q, Some("t_gist")).unwrap();
    assert!(narrow.blocks_read * 8 <= blocks, "{narrow:?}");
    let (_, none) = s
        .ask(
            "SELECT id FROM s.t WHERE v <@ box '((3000,3000),(4000,4000))'",
            Some("t_gist"),
        )
        .unwrap();
    assert!(none.blocks_read * 8 <= blocks, "{none:?}");
    // No value past every block's box reads any block, the block holding a NaN excepted.
    let (_, past) = s
        .ask(
            "SELECT id FROM s.t WHERE v ~= '(5000,5000)'",
            Some("t_gist"),
        )
        .unwrap();
    assert!(past.blocks_read <= 3, "{past:?}");
}

#[test]
fn every_box_operator_holds_along_each_strand_of_a_box_and_a_box_s_bounds_rule_blocks_out() {
    let path = table_of(
        "plane-boxes",
        "box",
        GeoKind::Box,
        &["gist", "spgist"],
        16384,
        |i| {
            if i % 100 == 0 {
                return None;
            }
            let (x, y) = cell(i);
            Some(match i {
                1 => "((NaN,1100),(1104,1104))".into(),
                2 => "((-Infinity,1100),(1100,1104))".into(),
                3 => "((1100,1100),(1100,1100))".into(),
                _ => format!("(({x},{y}),({},{}))", x + 4, y + 4),
            })
        },
    );
    let mut s = fresh(&path);
    operators_agree(
        &mut s,
        &["t_gist", "t_spgist"],
        &SHAPE_OPS,
        &[
            "'((1040,1040),(1100,1100))'",
            "box '((1100,1100),(1100,1100))'",
            "box '((1024,1024),(1028,1028))'",
            "box '((1100,1100),(1104,1104))'",
            "'((NaN,1100),(1104,1104))'",
            "box '((-Infinity,-Infinity),(Infinity,Infinity))'",
        ],
    );
    let blocks = blocks_of(&s.blocks().unwrap(), "t_gist", "v.high_x").len() as u64;
    assert!(blocks >= 16, "{blocks}");
    let (_, over) = s
        .ask(
            "SELECT id FROM s.t WHERE v && box '((1100,1100),(1110,1110))'",
            Some("t_gist"),
        )
        .unwrap();
    assert!(over.blocks_read * 3 <= blocks, "{over:?}");
    let (_, inside) = s
        .ask(
            "SELECT id FROM s.t WHERE v @> box '((1101,1101),(1102,1102))'",
            Some("t_gist"),
        )
        .unwrap();
    assert!(inside.blocks_read * 3 <= blocks, "{inside:?}");
    let (_, left) = s
        .ask(
            "SELECT id FROM s.t WHERE v << box '((1030,1100),(1040,1110))'",
            Some("t_gist"),
        )
        .unwrap();
    assert!(left.blocks_read * 3 <= blocks, "{left:?}");
}

#[test]
fn every_polygon_and_circle_operator_holds_along_its_strands_each_by_its_bounding_box() {
    let path = table_of(
        "plane-polygons",
        "polygon",
        GeoKind::Polygon,
        &["gist", "spgist"],
        16384,
        |i| {
            if i % 100 == 0 {
                return None;
            }
            let (x, y) = cell(i);
            Some(match i {
                1 => "((NaN,1100),(1104,1104),(1100,1104))".into(),
                2 => "((Infinity,1100),(1104,1104),(1100,1104))".into(),
                3 => "((1100,1100))".into(),
                _ => format!("(({x},{y}),({},{y}),({},{}))", x + 4, x + 4, y + 4),
            })
        },
    );
    let mut s = fresh(&path);
    operators_agree(
        &mut s,
        &["t_gist", "t_spgist"],
        &SHAPE_OPS,
        &[
            "'((1040,1040),(1100,1040),(1100,1100),(1040,1100))'",
            "polygon '((1100,1100))'",
            "polygon '((1024,1024),(1028,1024),(1028,1028))'",
            "'((NaN,1100),(1104,1104),(1100,1104))'",
        ],
    );
    let blocks = blocks_of(&s.blocks().unwrap(), "t_gist", "v.bound.high_x").len() as u64;
    assert!(blocks >= 16, "{blocks}");
    let (_, over) = s
        .ask(
            "SELECT id FROM s.t WHERE v && polygon '((1100,1100),(1110,1100),(1110,1110))'",
            Some("t_gist"),
        )
        .unwrap();
    assert!(over.blocks_read * 3 <= blocks, "{over:?}");

    let path = table_of(
        "plane-circles",
        "circle",
        GeoKind::Circle,
        &["gist"],
        16384,
        |i| {
            if i % 100 == 0 {
                return None;
            }
            let (x, y) = cell(i);
            Some(match i {
                1 => "<(NaN,1100),4>".into(),
                2 => "<(1100,1100),NaN>".into(),
                3 => "<(1100,1100),Infinity>".into(),
                4 => "<(Infinity,1100),Infinity>".into(),
                5 => "<(1100,1100),0>".into(),
                _ => format!("<({x},{y}),{}>", 1 + i % 4),
            })
        },
    );
    let mut s = fresh(&path);
    operators_agree(
        &mut s,
        &["t_gist"],
        &SHAPE_OPS,
        &[
            "'<(1100,1100),8>'",
            "circle '<(1100,1100),0>'",
            "circle '<(1100,1100),1000>'",
            "circle '<(3000,3000),1>'",
            "'<(NaN,1100),4>'",
        ],
    );
    let blocks = blocks_of(&s.blocks().unwrap(), "t_gist", "v.x").len() as u64;
    assert!(blocks >= 16, "{blocks}");
    let (_, over) = s
        .ask(
            "SELECT id FROM s.t WHERE v && circle '<(1200,1200),8>'",
            Some("t_gist"),
        )
        .unwrap();
    // The block holding the infinite radius, and those the circle lies over.
    assert!(over.blocks_read * 3 <= blocks, "{over:?}");
}

/// The last row of each table lies at the far corner of the plane, each value half of PostgreSQL's
/// EPSILON past it: no block's box holds the value, and the operator takes the row.
#[test]
fn a_shape_within_postgresql_s_fuzz_of_a_value_is_found_along_every_order() {
    let n = 4096;
    let tables: [(&str, GeoKind, &[&str], fn(i64) -> String, &[&str]); 3] = [
        (
            "point",
            GeoKind::Point,
            &["gist", "spgist"],
            |i| format!("({i},{i})"),
            &["v ~= point '(4095.0000005,4095.0000005)'"],
        ),
        (
            "box",
            GeoKind::Box,
            &["gist", "spgist"],
            |i| format!("(({i},{i}),({},{}))", i + 1, i + 1),
            &["v ~= box '((4095.0000005,4095),(4096.0000005,4096))'"],
        ),
        (
            "circle",
            GeoKind::Circle,
            &["gist"],
            |i| format!("<({i},{i}),1>"),
            &[
                "v ~= circle '<(4095.0000005,4095),1>'",
                "v @> circle '<(4095.0000005,4095),1>'",
                "v <@ circle '<(4095.0000005,4095),1>'",
                "v && circle '<(4097.0000005,4095),1>'",
            ],
        ),
    ];
    for (ty, kind, methods, row, conditions) in tables {
        let path = table_of(&format!("plane-fuzz-{ty}"), ty, kind, methods, n, |i| {
            Some(row(i))
        });
        let mut s = fresh(&path);
        for c in conditions {
            let q = format!("SELECT id FROM s.t WHERE {c}");
            assert_eq!(ids(&mut s, &q, "t_pkey"), vec![n - 1], "{q}");
            for m in methods {
                assert_eq!(ids(&mut s, &q, &format!("t_{m}")), vec![n - 1], "{q} {m}");
            }
        }
    }
}

#[test]
fn a_distance_beside_a_condition_finds_its_nearest_rows_along_its_strand() {
    let path = table_of(
        "plane-distance",
        "point",
        GeoKind::Point,
        &["gist"],
        4096,
        |i| (i % 100 != 0).then(|| format!("({},{})", 1024 + (i * 7919) % 64, 1024 + i % 64)),
    );
    let mut s = fresh(&path);
    let q = "SELECT id FROM s.t WHERE v IS NOT NULL ORDER BY v <-> point '(1030,1030)', id LIMIT 5";
    let (b, _) = s.ask(q, None).unwrap();
    assert_eq!(b.num_rows(), 5);
    // The nearest five, worked out apart from the operator.
    let mut all: Vec<(i64, i64)> = (0..4096i64)
        .filter(|i| i % 100 != 0)
        .map(|i| {
            let (x, y) = (1024 + (i * 7919) % 64 - 1030, 1024 + i % 64 - 1030);
            (x * x + y * y, i)
        })
        .collect();
    all.sort();
    let a = b.column(0).as_any().downcast_ref::<Int64Array>().unwrap();
    let got: Vec<i64> = (0..5).map(|i| a.value(i)).collect();
    assert_eq!(got, all[..5].iter().map(|x| x.1).collect::<Vec<_>>());
}

#[test]
fn write_and_the_old_formats_treat_a_shape_as_postgresql_and_their_refusals_name_the_way_across() {
    // A circle whose bounding box overflows, refused where it enters the strand.
    let catalog = Catalog::parse(
        "CREATE TABLE s.t (id bigint NOT NULL, v circle, PRIMARY KEY (id)); \
         CREATE INDEX t_v ON s.t USING gist (v);",
    )
    .unwrap();
    let e = write(
        &tmp("plane-overflow.snail"),
        &catalog,
        &[(
            "s.t".into(),
            Rows {
                columns: vec![
                    Values::Int(vec![Some(1)]),
                    shapes(
                        GeoKind::Circle,
                        std::iter::once(Some("<(1e308,0),1e308>".into())),
                    ),
                ],
            },
        )],
    )
    .err()
    .unwrap()
    .to_string();
    assert!(e.contains("value out of range: overflow"), "{e}");
    for (ty, method) in [("point", "gist"), ("polygon", "spgist")] {
        let e = warren_snail::write::write_format_eight(
            &tmp("plane-eight.snail"),
            &Catalog::parse(&format!(
                "CREATE TABLE s.t (a integer NOT NULL, v {ty}, PRIMARY KEY (a)); \
                 CREATE INDEX t_v ON s.t USING {method} (v);",
            ))
            .unwrap(),
            &[],
            weather::span(),
        )
        .err()
        .unwrap()
        .to_string();
        assert!(
            e.contains(&format!(
                "s.t: the {method} index t_v, which a snail of format 8"
            )) && e.contains("`out`"),
            "{e}"
        );
    }
}

#[test]
fn a_table_clustered_on_a_gist_index_on_the_plane_is_cut_on_it_and_read_as_any_order() {
    let catalog = Catalog::parse(
        "CREATE TABLE s.t (id bigint NOT NULL, v point, PRIMARY KEY (id)); \
         CREATE INDEX t_v ON s.t USING gist (v); \
         ALTER TABLE s.t CLUSTER ON t_v;",
    )
    .unwrap();
    let table = catalog.table("s.t").unwrap();
    assert_eq!(table.orders[0].name, "t_v");
    assert!(table.orders[0].table_order);
    let n = 16384i64;
    let id = Values::Int((0..n).map(Some).collect());
    let v = shapes(
        GeoKind::Point,
        (0..n).map(|i| (i % 100 != 0).then(|| format!("({},{})", cell(i).0, cell(i).1))),
    );
    let path = tmp("plane-clustered.snail");
    write(
        &path,
        &catalog,
        &[(
            "s.t".into(),
            Rows {
                columns: vec![id, v],
            },
        )],
    )
    .unwrap();
    let mut s = fresh(&path);
    // The first rows of the table's own order are a block's corner of the plane.
    let rows = s.blocks().unwrap();
    let blocks = blocks_of(&rows, "t_v", "v.x").len() as u64;
    assert!(blocks >= 16, "{blocks}");
    for q in [
        "SELECT id FROM s.t WHERE v <@ box '((1040,1040),(1100,1100))'",
        "SELECT id FROM s.t WHERE v <^ '(1040,1040)'",
        "SELECT id FROM s.t WHERE v IS NULL",
    ] {
        assert_eq!(ids(&mut s, q, "t_v"), ids(&mut s, q, "t_pkey"), "{q}");
    }
    let q = "SELECT id FROM s.t WHERE v <@ box '((1040,1040),(1064,1064))'";
    let (b, round) = s.ask(q, None).unwrap();
    exact(q, &round);
    assert_eq!(round.order, "t_v", "{round:?}");
    assert!(
        b.num_rows() > 0 && round.blocks_read * 8 <= blocks,
        "{round:?}"
    );
    assert_eq!(
        s.ask("SELECT count(*) FROM s.t", None)
            .unwrap()
            .0
            .num_rows(),
        1
    );
}

#[cfg(feature = "standalone")]
fn run(w: &mut Snail, sql: &str) {
    w.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
}

#[cfg(feature = "standalone")]
fn refused(w: &mut Snail, sql: &str, why: &str) {
    match w.execute(sql) {
        Ok(d) => panic!("{sql}: taken, {d:?}"),
        Err(e) => assert!(e.to_string().contains(why), "{sql}: {e}"),
    }
}

/// A table of a point, a box, a polygon and a circle, each with its strands, 4,096 rows.
#[cfg(feature = "standalone")]
fn shapes_snail(name: &str) -> std::path::PathBuf {
    let catalog = Catalog::parse(
        "CREATE TABLE s.t (id bigint NOT NULL, p point, b box, g polygon, c circle, PRIMARY KEY (id)); \
         CREATE INDEX t_p ON s.t USING gist (p); \
         CREATE INDEX t_ps ON s.t USING spgist (p kd_point_ops); \
         CREATE INDEX t_b ON s.t USING gist (b); \
         CREATE INDEX t_bs ON s.t USING spgist (b); \
         CREATE INDEX t_g ON s.t USING gist (g); \
         CREATE INDEX t_gs ON s.t USING spgist (g); \
         CREATE INDEX t_c ON s.t USING gist (c);",
    )
    .unwrap();
    let n = 4096i64;
    let at = |i: i64| {
        (
            1024 + 8 * (((i * 7919) % n) / 64),
            1024 + 8 * (((i * 7919) % n) % 64),
        )
    };
    let columns = vec![
        Values::Int((0..n).map(Some).collect()),
        shapes(
            GeoKind::Point,
            (0..n).map(|i| Some(format!("({},{})", at(i).0, at(i).1))),
        ),
        shapes(
            GeoKind::Box,
            (0..n).map(|i| {
                Some(format!(
                    "(({},{}),({},{}))",
                    at(i).0,
                    at(i).1,
                    at(i).0 + 4,
                    at(i).1 + 4
                ))
            }),
        ),
        shapes(
            GeoKind::Polygon,
            (0..n).map(|i| {
                Some(format!(
                    "(({},{}),({},{}),({},{}))",
                    at(i).0,
                    at(i).1,
                    at(i).0 + 4,
                    at(i).1,
                    at(i).0 + 4,
                    at(i).1 + 4
                ))
            }),
        ),
        shapes(
            GeoKind::Circle,
            (0..n).map(|i| Some(format!("<({},{}),2>", at(i).0, at(i).1))),
        ),
    ];
    let path = tmp(&format!("{name}.snail"));
    write(&path, &catalog, &[("s.t".into(), Rows { columns })]).unwrap();
    path
}

/// Each question, along the table's key and each strand of its column: the same ids.
#[cfg(feature = "standalone")]
fn all_alike(s: &mut Snail, questions: &[(&str, &[&str], &[i64])]) {
    for (q, orders, want) in questions {
        let along_key = ids(s, q, "t_pkey");
        assert_eq!(along_key, *want, "{q}");
        for order in *orders {
            assert_eq!(ids(s, q, order), along_key, "{q} along {order}");
        }
    }
}

#[cfg(feature = "standalone")]
#[test]
fn a_shape_written_moved_and_deleted_widens_its_block_s_box_on_every_strand() {
    let path = shapes_snail("plane-writes");
    let mut w = Snail::open_writing(&path).unwrap();
    for sql in [
        // Past every block's box, into the strands' last blocks.
        "INSERT INTO s.t VALUES (9000, '(9000,9000)', '((9000,9000),(9004,9004))', \
         '((9000,9000),(9004,9000),(9004,9004))', '<(9000,9000),2>')",
        "UPDATE s.t SET p = '(9500,9500)', b = '((9500,9500),(9504,9504))', \
         g = '((9500,9500),(9504,9500),(9504,9504))', c = '<(9500,9500),2>' WHERE id = 1",
        "INSERT INTO s.t VALUES (9001, NULL, NULL, NULL, NULL)",
        "DELETE FROM s.t WHERE id = 2",
        // A NaN and an infinity, each kept where every operator can find it.
        "INSERT INTO s.t VALUES (9002, '(NaN,1)', '((NaN,1),(2,3))', '((NaN,1),(2,3),(4,5))', '<(NaN,1),2>')",
        "INSERT INTO s.t VALUES (9003, '(Infinity,1)', '((Infinity,1),(2,3))', \
         '((Infinity,1),(2,3),(4,5))', '<(1,1),Infinity>')",
    ] {
        run(&mut w, sql);
    }
    refused(
        &mut w,
        "INSERT INTO s.t (id, c) VALUES (9004, '<(1e308,0),1e308>')",
        "value out of range: overflow",
    );
    refused(
        &mut w,
        "UPDATE s.t SET c = '<(1e308,0),1e308>' WHERE id = 3",
        "value out of range: overflow",
    );
    let check = |s: &mut Snail| {
        let (ps, bs, gs, cs): (&[&str], &[&str], &[&str], &[&str]) = (
            &["t_p", "t_ps"],
            &["t_b", "t_bs"],
            &["t_g", "t_gs"],
            &["t_c"],
        );
        all_alike(
            s,
            &[
                ("SELECT id FROM s.t WHERE p ~= '(9000,9000)'", ps, &[9000]),
                (
                    "SELECT id FROM s.t WHERE p >> '(8000,8000)'",
                    ps,
                    &[1, 9000, 9003],
                ),
                ("SELECT id FROM s.t WHERE p ~= '(NaN,1)'", ps, &[9002]),
                ("SELECT id FROM s.t WHERE p IS NULL", ps, &[9001]),
                (
                    "SELECT id FROM s.t WHERE b && box '((9500,9500),(9501,9501))'",
                    bs,
                    &[1],
                ),
                (
                    "SELECT id FROM s.t WHERE b ~= '((NaN,1),(2,3))'",
                    bs,
                    &[9002],
                ),
                (
                    "SELECT id FROM s.t WHERE g && polygon '((9500,9500),(9501,9500),(9501,9501))'",
                    gs,
                    &[1],
                ),
                (
                    "SELECT id FROM s.t WHERE g ~= '((NaN,1),(2,3),(4,5))'",
                    gs,
                    &[9002],
                ),
                (
                    "SELECT id FROM s.t WHERE c && circle '<(9500,9500),1>'",
                    cs,
                    &[1, 9003],
                ),
                (
                    "SELECT id FROM s.t WHERE c << circle '<(0,0),0.5>'",
                    cs,
                    &[],
                ),
            ],
        );
    };
    check(&mut w);
    // A read that finds its block's tail wasted prunes it, and the box stays what the block holds.
    drop(w);
    let mut s = fresh(&path);
    check(&mut s);
    let mut w = Snail::open_writing(&path).unwrap();
    for _ in 0..3 {
        for id in 3000..3040 {
            run(
                &mut w,
                &format!("UPDATE s.t SET p = '(1030,{id})' WHERE id = {id}"),
            );
        }
        check(&mut w);
    }
    let q = "SELECT id FROM s.t WHERE p ~= '(1030,3000)'";
    assert_eq!(ids(&mut w, q, "t_p"), [3000]);
    assert_eq!(ids(&mut w, q, "t_pkey"), [3000]);
}

#[cfg(feature = "standalone")]
#[test]
fn a_table_clustered_on_the_plane_takes_statements_as_any_order_does() {
    let catalog = Catalog::parse(
        "CREATE TABLE s.t (id bigint NOT NULL, v point, PRIMARY KEY (id)); \
         CREATE INDEX t_v ON s.t USING gist (v); \
         ALTER TABLE s.t CLUSTER ON t_v;",
    )
    .unwrap();
    let n = 4096i64;
    let id = Values::Int((0..n).map(Some).collect());
    // Two rows on each point, so that the table's own key tells no row from its neighbour.
    let v = shapes(
        GeoKind::Point,
        (0..n).map(|i| Some(format!("({},{})", 1024 + (i / 2) % 64, 1024 + (i / 2) / 64))),
    );
    let path = tmp("plane-clustered-writes.snail");
    write(
        &path,
        &catalog,
        &[(
            "s.t".into(),
            Rows {
                columns: vec![id, v],
            },
        )],
    )
    .unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    // A row past every point lands in the last block of the table's own order.
    let rows_of = |w: &Snail| -> Vec<u64> {
        w.blocks()
            .unwrap()
            .iter()
            .filter(|r| r.order == "t_v" && r.column == "v.x")
            .map(|r| r.rows)
            .collect()
    };
    let before = rows_of(&w);
    run(&mut w, "INSERT INTO s.t VALUES (8000, '(6000,6000)')");
    let mut grown = before.clone();
    *grown.last_mut().unwrap() += 1;
    assert_eq!(rows_of(&w), grown);
    run(&mut w, "DELETE FROM s.t WHERE id = 8000");
    run(&mut w, "UPDATE s.t SET v = '(5000,5000)' WHERE id = 0");
    run(&mut w, "DELETE FROM s.t WHERE id = 3");
    run(
        &mut w,
        "INSERT INTO s.t VALUES (9000, '(1024,1024)'), (9001, NULL)",
    );
    let q = "SELECT id FROM s.t WHERE v ~= '(1024,1024)'";
    assert_eq!(ids(&mut w, q, "t_v"), [1, 9000]);
    assert_eq!(ids(&mut w, q, "t_pkey"), [1, 9000]);
    let q = "SELECT id FROM s.t WHERE v ~= '(1025,1024)'";
    assert_eq!(ids(&mut w, q, "t_v"), [2]);
    let q = "SELECT id FROM s.t WHERE v ~= '(5000,5000)'";
    assert_eq!(ids(&mut w, q, "t_v"), [0]);
    let q = "SELECT id FROM s.t WHERE v IS NULL";
    assert_eq!(ids(&mut w, q, "t_v"), [9001]);
    let (b, _) = w.ask("SELECT count(*) FROM s.t", None).unwrap();
    assert_eq!(weather::one(&b), 4097);
    // Moved and deleted again, in a transaction, one of two rows on one point among them.
    run(&mut w, "BEGIN");
    run(&mut w, "DELETE FROM s.t WHERE id = 1");
    run(&mut w, "UPDATE s.t SET v = '(1024,1024)' WHERE id = 2");
    run(&mut w, "COMMIT");
    assert_eq!(
        ids(&mut w, "SELECT id FROM s.t WHERE v ~= '(1024,1024)'", "t_v"),
        [2, 9000]
    );
}

#[cfg(feature = "standalone")]
fn exclusions(name: &str) -> std::path::PathBuf {
    let path = tmp(&format!("{name}.snail"));
    let catalog = Catalog::parse(
        "CREATE TABLE s.t (id integer NOT NULL, p point, b box, g polygon, c circle, PRIMARY KEY (id), \
         EXCLUDE USING gist (p WITH ~=), \
         EXCLUDE USING gist (b WITH &&), \
         EXCLUDE USING gist (g WITH &&), \
         EXCLUDE USING gist (c WITH &&));",
    )
    .unwrap();
    write(&path, &catalog, &[]).unwrap();
    path
}

#[cfg(feature = "standalone")]
#[test]
fn an_exclude_on_the_plane_refuses_a_shape_that_conflicts_as_postgresql_refuses_it() {
    let path = exclusions("plane-exclude");
    let mut w = Snail::open_writing(&path).unwrap();
    run(
        &mut w,
        "INSERT INTO s.t VALUES (1, '(1,1)', '((0,0),(2,2))', '((0,0),(2,0),(2,2))', '<(0,0),1>')",
    );
    for (sql, name) in [
        ("INSERT INTO s.t (id, p) VALUES (2, '(1,1)')", "t_p_excl"),
        (
            "INSERT INTO s.t (id, b) VALUES (2, '((1,1),(3,3))')",
            "t_b_excl",
        ),
        (
            "INSERT INTO s.t (id, g) VALUES (2, '((1,1),(3,1),(3,3))')",
            "t_g_excl",
        ),
        (
            "INSERT INTO s.t (id, c) VALUES (2, '<(1,1),1>')",
            "t_c_excl",
        ),
    ] {
        refused(
            &mut w,
            sql,
            &format!("conflicting key value violates exclusion constraint \"{name}\""),
        );
    }
    // NULL conflicts with nothing; a value apart conflicts with nothing; a NaN only with a NaN, by `~=`.
    run(&mut w, "INSERT INTO s.t (id) VALUES (3), (4)");
    run(
        &mut w,
        "INSERT INTO s.t VALUES (5, '(5,5)', '((5,5),(6,6))', '((5,5),(6,5),(6,6))', '<(5,5),0.5>')",
    );
    run(&mut w, "INSERT INTO s.t (id, p) VALUES (6, '(NaN,1)')");
    refused(
        &mut w,
        "INSERT INTO s.t (id, p) VALUES (7, '(NaN,1)')",
        "exclusion constraint \"t_p_excl\"",
    );
    run(&mut w, "INSERT INTO s.t (id, p) VALUES (7, '(NaN,2)')");
    // An UPDATE moving a row off itself conflicts with no other, and onto another refused.
    run(
        &mut w,
        "UPDATE s.t SET b = '((5.5,5.5),(7,7))' WHERE id = 5",
    );
    refused(
        &mut w,
        "UPDATE s.t SET b = '((1,1),(5.5,5.5))' WHERE id = 5",
        "exclusion constraint \"t_b_excl\"",
    );
    run(&mut w, "DELETE FROM s.t WHERE id = 1");
    run(&mut w, "UPDATE s.t SET b = '((1,1),(5,5))' WHERE id = 5");
    // On SP-GiST, PostgreSQL takes the constraint and refuses every box or polygon it meets (geo_spgist.c).
    let catalog = Catalog::parse(
        "CREATE TABLE s.u (id integer NOT NULL, b box, g polygon, p point, PRIMARY KEY (id), \
         EXCLUDE USING spgist (b WITH &&), EXCLUDE USING spgist (g WITH ~=), \
         EXCLUDE USING spgist (p kd_point_ops WITH ~=));",
    )
    .unwrap();
    let path = tmp("plane-exclude-spgist.snail");
    write(&path, &catalog, &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    run(&mut w, "INSERT INTO s.u (id) VALUES (1)");
    for sql in [
        "INSERT INTO s.u (id, b) VALUES (2, '((0,0),(1,1))')",
        "INSERT INTO s.u (id, g) VALUES (2, '((0,0),(1,1),(1,0))')",
        "UPDATE s.u SET b = '((0,0),(1,1))' WHERE id = 1",
    ] {
        refused(&mut w, sql, "unrecognized scankey subtype: 0");
    }
    run(&mut w, "INSERT INTO s.u (id, p) VALUES (2, '(1,1)')");
    refused(
        &mut w,
        "INSERT INTO s.u (id, p) VALUES (3, '(1,1)')",
        "exclusion constraint \"u_p_excl\"",
    );
}

#[cfg(feature = "standalone")]
#[test]
fn a_second_writer_of_a_conflicting_shape_waits_and_is_refused_where_the_first_commits() {
    use std::time::Duration;
    let path = exclusions("plane-exclude-two");
    let mut one = Snail::open_writing(&path).unwrap();
    run(
        &mut one,
        "INSERT INTO s.t (id, b) VALUES (1, '((0,0),(1,1))')",
    );
    for (end, taken) in [("ROLLBACK", true), ("COMMIT", false)] {
        run(&mut one, "BEGIN");
        run(
            &mut one,
            "INSERT INTO s.t (id, b) VALUES (10, '((100,100),(200,200))')",
        );
        let (tx, rx) = std::sync::mpsc::channel();
        let p = path.clone();
        std::thread::spawn(move || {
            let mut two = Snail::open_writing(&p).unwrap();
            let r = two
                .execute("INSERT INTO s.t (id, b) VALUES (20, '((150,150),(160,160))')")
                .map_err(|e| e.to_string());
            if r.is_ok() {
                two.execute("DELETE FROM s.t WHERE id = 20").unwrap();
            }
            tx.send(r).unwrap();
        });
        std::thread::sleep(Duration::from_millis(400));
        assert!(rx.try_recv().is_err(), "the second writer did not wait");
        run(&mut one, end);
        let r = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(r.is_ok(), taken, "{end}: {r:?}");
    }
}

#[cfg(feature = "standalone")]
#[test]
fn at_one_snapshot_two_writers_of_one_block_of_the_plane_both_commit_and_the_block_keeps_their_box()
{
    for level in ["REPEATABLE READ", "SERIALIZABLE"] {
        let path = shapes_snail(&format!("plane-beside-{}", level.replace(' ', "-")));
        let mut a = Snail::open_writing(&path).unwrap();
        let mut b = Snail::open_writing(&path).unwrap();
        run(&mut a, &format!("BEGIN ISOLATION LEVEL {level}"));
        run(&mut b, &format!("BEGIN ISOLATION LEVEL {level}"));
        // Both past the box of the strand's first block, which holds them, each past the other's.
        run(
            &mut a,
            "INSERT INTO s.t (id, p) VALUES (9000, '(1023.5,1023.5)')",
        );
        run(
            &mut b,
            "INSERT INTO s.t (id, p) VALUES (9001, '(1023.25,1023.25)')",
        );
        // At SERIALIZABLE a read marks what the other writes, so only the snapshot's level reads here.
        if level == "REPEATABLE READ" {
            let q = "SELECT id FROM s.t WHERE p ~= '(1023.25,1023.25)'";
            assert_eq!(ids(&mut a, q, "t_p"), Vec::<i64>::new());
            assert_eq!(ids(&mut b, q, "t_p"), [9001]);
        }
        run(&mut a, "COMMIT");
        run(&mut b, "COMMIT");
        for (q, want) in [
            ("SELECT id FROM s.t WHERE p ~= '(1023.5,1023.5)'", 9000),
            ("SELECT id FROM s.t WHERE p ~= '(1023.25,1023.25)'", 9001),
        ] {
            assert_eq!(ids(&mut a, q, "t_p"), [want], "{level}: {q}");
            assert_eq!(ids(&mut a, q, "t_ps"), [want], "{level}: {q}");
        }
    }
}

#[cfg(feature = "standalone")]
#[test]
fn at_repeatable_read_two_writers_of_one_block_update_and_delete_their_own_rows_and_a_prune_keeps_the_box(
) {
    let path = shapes_snail("plane-beside-updates");
    let mut a = Snail::open_writing(&path).unwrap();
    let mut b = Snail::open_writing(&path).unwrap();
    run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    run(&mut b, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    // Rows 0 and 64 of the grid's first cells lie in one block of the strand.
    run(
        &mut a,
        "UPDATE s.t SET p = '(1024.5,1024.5)' WHERE p ~= '(1024,1024)'",
    );
    run(&mut b, "DELETE FROM s.t WHERE p ~= '(1024,1032)'");
    run(&mut a, "COMMIT");
    run(&mut b, "COMMIT");
    assert_eq!(
        ids(&mut a, "SELECT id FROM s.t WHERE p ~= '(1024,1032)'", "t_p"),
        Vec::<i64>::new()
    );
    assert_eq!(
        ids(
            &mut a,
            "SELECT id FROM s.t WHERE p ~= '(1024.5,1024.5)'",
            "t_p"
        ),
        [0]
    );
    // The same row changed since the snapshot is refused, in PostgreSQL's words.
    run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    assert_eq!(
        ids(
            &mut a,
            "SELECT id FROM s.t WHERE p ~= '(1024.5,1024.5)'",
            "t_p"
        ),
        [0]
    );
    run(&mut b, "UPDATE s.t SET p = '(1025.5,1025.5)' WHERE id = 0");
    refused(
        &mut a,
        "UPDATE s.t SET p = '(1026.5,1026.5)' WHERE p ~= '(1024.5,1024.5)'",
        "could not serialize access due to concurrent update",
    );
    run(&mut a, "ROLLBACK");
    // Many rows moved within the block's places waste its pages: a read along the strand prunes it, and
    // the box it keeps still holds what it holds.
    let mut pruned = 0;
    for round in 0..12 {
        for id in 0..64 {
            run(
                &mut a,
                &format!(
                    "UPDATE s.t SET p = '({},{})' WHERE id = {id}",
                    1024 + round,
                    1024 + id
                ),
            );
        }
        let q = format!("SELECT id FROM s.t WHERE p ~= '({},1024)'", 1024 + round);
        let (_, r) = a.ask(&q, Some("t_p")).unwrap();
        pruned += r.blocks_pruned;
        assert_eq!(ids(&mut a, &q, "t_p"), ids(&mut a, &q, "t_pkey"), "{q}");
    }
    assert!(pruned > 0, "no read along the strand pruned");
    let q = "SELECT id FROM s.t WHERE p <@ box '((1024,1024),(1035,1087))'";
    assert_eq!(ids(&mut a, q, "t_p"), ids(&mut a, q, "t_pkey"));
    // A block pruned since a snapshot takes the row its transaction wrote, in the box it keeps.
    run(&mut b, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    run(
        &mut b,
        "INSERT INTO s.t (id, p) VALUES (9100, '(1023.75,1023.75)')",
    );
    for id in 0..64 {
        run(
            &mut a,
            &format!(
                "UPDATE s.t SET p = '(1024.5,{})' WHERE id = {id}",
                1024 + id
            ),
        );
    }
    let (_, r) = a
        .ask(
            "SELECT id FROM s.t WHERE p <@ box '((1024,1024),(1035,1087))'",
            Some("t_p"),
        )
        .unwrap();
    assert!(r.blocks_pruned > 0, "{r:?}");
    run(&mut b, "COMMIT");
    let q = "SELECT id FROM s.t WHERE p ~= '(1023.75,1023.75)'";
    assert_eq!(ids(&mut a, q, "t_p"), [9100]);
    assert_eq!(ids(&mut a, q, "t_pkey"), [9100]);
}

#[cfg(feature = "standalone")]
#[test]
fn at_serializable_a_read_along_the_plane_meets_a_row_written_into_a_block_it_ruled_out() {
    let path = shapes_snail("plane-marks");
    let mut a = Snail::open_writing(&path).unwrap();
    let mut b = Snail::open_writing(&path).unwrap();
    run(&mut a, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    run(&mut b, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    // Each reads a box empty on the strand, every block ruled out, and writes into the other's box.
    let inside = |x: i64| {
        format!(
            "SELECT id FROM s.t WHERE p <@ box '(({x},{x}),({},{}))'",
            x + 1,
            x + 1
        )
    };
    assert_eq!(ids(&mut a, &inside(9000), "t_p"), Vec::<i64>::new());
    assert_eq!(ids(&mut b, &inside(9100), "t_p"), Vec::<i64>::new());
    run(
        &mut a,
        "INSERT INTO s.t (id, p) VALUES (-5000, '(9100,9100)')",
    );
    run(
        &mut b,
        "INSERT INTO s.t (id, p) VALUES (9001, '(9000,9000)')",
    );
    run(&mut a, "COMMIT");
    let e = b.execute("COMMIT").err().unwrap().to_string();
    assert!(
        e.contains("could not serialize access due to read/write dependencies among transactions"),
        "{e}"
    );
    // Apart, on the table's rows: two transactions that read nothing commit.
    let mut c = Snail::open_writing(&path).unwrap();
    run(&mut c, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    run(
        &mut c,
        "INSERT INTO s.t (id, p) VALUES (9002, '(9200,9200)')",
    );
    run(&mut c, "COMMIT");
}

#[test]
fn a_join_reads_its_side_along_the_plane_and_a_comparison_of_two_columns_is_a_row_s() {
    let catalog = Catalog::parse(
        "CREATE TABLE s.t (id bigint NOT NULL, v box, PRIMARY KEY (id)); \
         CREATE INDEX t_v ON s.t USING gist (v); \
         CREATE TABLE s.u (id bigint NOT NULL, t_id bigint NOT NULL, w box, PRIMARY KEY (id));",
    )
    .unwrap();
    let n = 16384i64;
    let boxes = |f: &dyn Fn(i64) -> i64| {
        shapes(
            GeoKind::Box,
            (0..n).map(|i| {
                let (x, y) = cell(f(i));
                Some(format!("(({x},{y}),({},{}))", x + 4, y + 4))
            }),
        )
    };
    let t = Rows {
        columns: vec![Values::Int((0..n).map(Some).collect()), boxes(&|i| i)],
    };
    let u = Rows {
        columns: vec![
            Values::Int((0..n).map(Some).collect()),
            Values::Int((0..n).map(|i| Some((i * 3) % n)).collect()),
            boxes(&|i| i + 1),
        ],
    };
    let path = tmp("plane-join.snail");
    write(&path, &catalog, &[("s.t".into(), t), ("s.u".into(), u)]).unwrap();
    let mut s = fresh(&path);
    let want = |pass: &dyn Fn(i64) -> bool| -> Vec<i64> {
        let mut v: Vec<i64> = (0..n).filter(|&i| pass(i)).collect();
        v.sort();
        v
    };
    let in_box = |i: i64| {
        let (x, y) = cell(i);
        x <= 1100 && x + 4 >= 1040 && y <= 1100 && y + 4 >= 1040
    };
    let q = "SELECT u.id FROM s.t JOIN s.u ON t.id = u.t_id WHERE t.v && box '((1040,1040),(1100,1100))'";
    let (b, _) = s.ask(q, None).unwrap_or_else(|e| panic!("{q}: {e}"));
    let a = b.column(0).as_any().downcast_ref::<Int64Array>().unwrap();
    let mut got: Vec<i64> = (0..a.len()).map(|i| a.value(i)).collect();
    got.sort();
    assert_eq!(got, want(&|i| in_box((i * 3) % n)));
    // A box's `=` is its area, which no key reads: a join on it is refused by name, as before the strand.
    let q = "SELECT count(*) FROM s.t a JOIN s.t b ON a.v = b.v WHERE a.id < 3 AND b.id < 3";
    let e = s.ask(q, None).err().unwrap().to_string();
    assert!(e.contains("a join on box is not read"), "{e}");
    // Two columns of one table, and a condition the operator's rule cannot read: a row's.
    let q = "SELECT id FROM s.u WHERE w && box '((1040,1040),(1100,1100))' OR id = 0";
    let (b, _) = s.ask(q, None).unwrap_or_else(|e| panic!("{q}: {e}"));
    assert!(b.num_rows() > 1);
    let q = "SELECT count(*) FROM s.t WHERE NOT (v && box '((1040,1040),(1100,1100))')";
    let (b, _) = s.ask(q, None).unwrap_or_else(|e| panic!("{q}: {e}"));
    assert_eq!(
        weather::one(&b),
        (0..n).filter(|&i| !in_box(i)).count() as i64
    );
    let q = "SELECT count(*) FROM s.t WHERE v && box '((1040,1040),(1100,1100))'";
    let (b, round) = s.ask(q, None).unwrap_or_else(|e| panic!("{q}: {e}"));
    exact(q, &round);
    assert_eq!(
        weather::one(&b),
        (0..n).filter(|&i| in_box(i)).count() as i64
    );
    assert_eq!(round.order, "t_v", "{round:?}");
}

#[test]
fn a_read_along_the_plane_and_a_distance_s_sort_keep_to_the_round_s_room() {
    let path = table_of(
        "plane-room",
        "point",
        GeoKind::Point,
        &["gist"],
        16384,
        |i| Some(format!("({},{})", cell(i).0, cell(i).1)),
    );
    let along = "SELECT id, v FROM s.t WHERE v <@ box '((1024,1024),(2047,2047))'";
    let nearest =
        "SELECT id, v <-> point '(1500,1500)' FROM s.t ORDER BY v <-> point '(1500,1500)', id";
    let mut s = fresh(&path);
    let wide = [
        s.ask(along, Some("t_gist")).unwrap(),
        s.ask(nearest, None).unwrap(),
    ];
    assert_eq!(wide[1].1.spill_pages_written, 0, "{:?}", wide[1].1);
    let room: u64 = 65_536;
    let dir = tmp("plane-room-spill");
    std::fs::create_dir_all(&dir).unwrap();
    let mut s = fresh(&path);
    s.size_rounds(warren_snail::room::Sizes::of(room).in_dir(&dir));
    let (b, round) = s.ask(along, Some("t_gist")).unwrap();
    exact(along, &round);
    assert_eq!(b.num_rows(), wide[0].0.num_rows());
    assert_eq!(round.column_pages_read, wide[0].1.column_pages_read);
    let (b, round) = s.ask(nearest, None).unwrap();
    let rows = |b: &arrow::array::RecordBatch| -> Vec<(i64, u64)> {
        let id = b.column(0).as_any().downcast_ref::<Int64Array>().unwrap();
        let d = b.column(1).as_any();
        let d = d.downcast_ref::<arrow::array::Float64Array>().unwrap();
        (0..b.num_rows())
            .map(|i| (id.value(i), d.value(i).to_bits()))
            .collect()
    };
    assert_eq!(rows(&b), rows(&wide[1].0));
    assert!(
        round.spill_pages_written > 0 && round.spill_pages_written == round.spill_pages_read,
        "{round:?}"
    );
    assert!(round.bytes_held < wide[1].1.bytes_held, "{round:?}");
}

#[test]
fn what_a_mount_asks_holds_on_the_plane_its_price_its_scan_its_sample_and_its_shapes_as_answers() {
    let path = table_of(
        "plane-mount",
        "point",
        GeoKind::Point,
        &["gist"],
        16384,
        |i| (i % 100 != 0).then(|| format!("({},{})", cell(i).0, cell(i).1)),
    );
    let mut s = fresh(&path);
    let q = "SELECT t.id FROM s.t t WHERE t.v <@ box '((1040,1040),(1100,1100))'";
    let p = s.price(q, None).unwrap();
    assert_eq!(p.order, "t_gist");
    let mut scan = s.scan(q, None).unwrap();
    let mut n = 0;
    while let Some(b) = s.next_block(&mut scan).unwrap() {
        n += b.num_rows() as u64;
    }
    let round = scan.round();
    assert!(p.rows_low <= n && n <= p.rows_high, "{n} {p:?}");
    assert_eq!(round.column_pages_read, p.column_pages, "{round:?} {p:?}");
    exact(q, &round);
    // A sample of a table whose own order is the plane's reads its rows' pages, whichever.
    let catalog = Catalog::parse(
        "CREATE TABLE s.t (id bigint NOT NULL, v point, PRIMARY KEY (id)); \
         CREATE INDEX t_v ON s.t USING gist (v); \
         ALTER TABLE s.t CLUSTER ON t_v;",
    )
    .unwrap();
    let clustered = tmp("plane-mount-clustered.snail");
    write(
        &clustered,
        &catalog,
        &[(
            "s.t".into(),
            Rows {
                columns: vec![
                    Values::Int((0..16384).map(Some).collect()),
                    shapes(
                        GeoKind::Point,
                        (0..16384).map(|i| Some(format!("({},{})", cell(i).0, cell(i).1))),
                    ),
                ],
            },
        )],
    )
    .unwrap();
    let c = fresh(&clustered);
    let mut draws = 0.0f64;
    let mut draw = move || {
        draws = (draws + 0.618_033_988_75) % 1.0;
        draws
    };
    let (mut scan, live) = c.sample("SELECT t.id FROM s.t t", 30, &mut draw).unwrap();
    assert_eq!(live, 16384);
    let mut got = 0;
    while let Some(b) = c.next_block(&mut scan).unwrap() {
        got += b.num_rows();
    }
    assert_eq!(got, 30);
    assert_eq!(
        scan.round().column_pages_read,
        scan.round().column_pages_predicted
    );
    // A shape handed back along the strand is the one the table's key hands back.
    let q = "SELECT id, v FROM s.t WHERE v <@ box '((1040,1040),(1100,1100))' ORDER BY id";
    let (along, _) = s.ask(q, Some("t_gist")).unwrap();
    let (key, _) = s.ask(q, Some("t_pkey")).unwrap();
    assert!(along.num_rows() > 0);
    assert_eq!(along, key);
}

/// A commit rebuilds the entries above the blocks it wrote: each keeps every block's box, a block's
/// highest y and lowest x not lying at one corner.
#[cfg(feature = "standalone")]
#[test]
fn a_commit_rebuilds_the_boxes_above_its_blocks_a_coordinate_at_a_time() {
    let catalog = Catalog::parse(
        "CREATE TABLE s.t (id bigint NOT NULL, v point, PRIMARY KEY (id)); \
         CREATE INDEX t_v ON s.t USING gist (v);",
    )
    .unwrap();
    let side = 256i64;
    let n = side * side;
    let at = |i: i64| (1024 + (i * 7919 % n) / side, 1024 + (i * 7919 % n) % side);
    let path = tmp("plane-above.snail");
    write(
        &path,
        &catalog,
        &[(
            "s.t".into(),
            Rows {
                columns: vec![
                    Values::Int((0..n).map(Some).collect()),
                    shapes(
                        GeoKind::Point,
                        (0..n).map(|i| Some(format!("({},{})", at(i).0, at(i).1))),
                    ),
                ],
            },
        )],
    )
    .unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    assert!(blocks_of(&w.blocks().unwrap(), "t_v", "v.x").len() >= 64);
    run(&mut w, "INSERT INTO s.t VALUES (900000, '(1024,1024)')");
    // Every eighth point of the grid, each found along the strand.
    let mut found = 0;
    for x in (1024..1280).step_by(8) {
        for y in (1024..1280).step_by(8) {
            let q = format!("SELECT count(*) FROM s.t WHERE v ~= '({x},{y})'");
            let (b, _) = w.ask(&q, Some("t_v")).unwrap();
            found += weather::one(&b);
        }
    }
    assert_eq!(found, 1025);
}

#[test]
fn a_strand_of_several_levels_keeps_the_box_of_each_and_reads_a_condition_on_any() {
    let catalog = Catalog::parse(
        "CREATE TABLE s.t (id bigint NOT NULL, p point, b box, r int4range, PRIMARY KEY (id)); \
         CREATE INDEX t_pb ON s.t USING gist (p, b) INCLUDE (r); \
         CREATE INDEX t_rp ON s.t USING gist (r, p);",
    )
    .unwrap();
    let n = 8192i64;
    let ranges = warren_snail::composed::built_in("int4range").unwrap();
    let columns = vec![
        Values::Int((0..n).map(Some).collect()),
        shapes(
            GeoKind::Point,
            (0..n).map(|i| (i % 50 != 0).then(|| format!("({},{})", cell(i).0, cell(i).1))),
        ),
        shapes(
            GeoKind::Box,
            (0..n).map(|i| {
                let (x, y) = cell(i * 3);
                Some(format!("(({x},{y}),({},{}))", x + 4, y + 4))
            }),
        ),
        Values::Composed(
            (0..n)
                .map(|i| {
                    Some(std::sync::Arc::new(warren_snail::composed::Compound::new(
                        ranges,
                        warren_snail::composed::text_in(ranges, &format!("[{i},{})", i + 4))
                            .unwrap(),
                    )))
                })
                .collect(),
        ),
    ];
    let path = tmp("plane-levels.snail");
    write(&path, &catalog, &[("s.t".into(), Rows { columns })]).unwrap();
    let mut s = fresh(&path);
    for (q, orders) in [
        (
            "p <@ box '((1040,1040),(1100,1100))'",
            &["t_pb", "t_rp"][..],
        ),
        ("b && box '((1040,1040),(1100,1100))'", &["t_pb"][..]),
        ("r && '[100,120)'", &["t_pb", "t_rp"][..]),
        (
            "r && '[100,120)' AND p <^ '(2000,2000)'",
            &["t_pb", "t_rp"][..],
        ),
        (
            "p <@ box '((1040,1040),(1100,1100))' AND b >> box '((1024,1024),(1030,1030))'",
            &["t_pb"][..],
        ),
        ("p IS NULL", &["t_pb", "t_rp"][..]),
    ] {
        let q = format!("SELECT id FROM s.t WHERE {q}");
        let along_key = ids(&mut s, &q, "t_pkey");
        assert!(!along_key.is_empty(), "{q}");
        for order in orders {
            assert_eq!(ids(&mut s, &q, order), along_key, "{q} along {order}");
        }
    }
    // A condition on the second level alone rules its blocks out by that level's box.
    let (_, narrow) = s
        .ask(
            "SELECT id FROM s.t WHERE b && box '((3000,3000),(4000,4000))'",
            Some("t_pb"),
        )
        .unwrap();
    assert_eq!(narrow.blocks_read, 0, "{narrow:?}");
}

#[test]
fn a_partial_strand_on_the_plane_holds_the_rows_it_admits_and_serves_what_implies_it() {
    let catalog = Catalog::parse(
        "CREATE TABLE s.t (id bigint NOT NULL, v point, PRIMARY KEY (id)); \
         CREATE INDEX t_v ON s.t USING gist (v) WHERE id >= 1000;",
    )
    .unwrap();
    let n = 8192i64;
    let v = shapes(
        GeoKind::Point,
        (0..n).map(|i| Some(format!("({},{})", cell(i).0, cell(i).1))),
    );
    let path = tmp("plane-partial.snail");
    write(
        &path,
        &catalog,
        &[(
            "s.t".into(),
            Rows {
                columns: vec![Values::Int((0..n).map(Some).collect()), v],
            },
        )],
    )
    .unwrap();
    let mut s = fresh(&path);
    let q = "SELECT id FROM s.t WHERE v <@ box '((1040,1040),(1100,1100))' AND id >= 2000";
    let along = ids(&mut s, q, "t_v");
    assert!(!along.is_empty());
    assert_eq!(along, ids(&mut s, q, "t_pkey"));
    // A question that does not imply the predicate is not read along its strand.
    let q = "SELECT id FROM s.t WHERE v <@ box '((1040,1040),(1100,1100))'";
    assert!(s.ask(q, Some("t_v")).is_err());
    let (_, chosen) = s.ask(q, None).unwrap();
    assert_eq!(chosen.order, "t_pkey", "{chosen:?}");
}

/// The ids of a question's answer, in its order.
fn in_order(b: &arrow::array::RecordBatch) -> Vec<i64> {
    if b.num_rows() == 0 {
        return Vec::new();
    }
    let a = b.column(0).as_any().downcast_ref::<Int64Array>().unwrap();
    (0..a.len()).map(|i| a.value(i)).collect()
}

/// The row of the grid at a cell, as `grid` lays them out.
fn grid_id(x: i64, y: i64) -> i64 {
    let n = 1024i64 * 1024;
    let j = (x - 1024) * 1024 + (y - 1024);
    (0..n).find(|&i| (i * 7919) % n == j).unwrap()
}

#[test]
fn a_distance_on_the_grid_reads_the_strand_s_nearest_blocks_as_their_pages_say() {
    let mut s = fresh(&grid("plane-near"));
    // 20 by 20 cells round the point: the box that holds its five nearest.
    let boxed = "SELECT id FROM s.t WHERE p <@ box '((1490,1490),(1509,1509))'";
    let (b, by_box) = s.ask(boxed, Some("t_p")).unwrap();
    exact(boxed, &by_box);
    assert_eq!(b.num_rows(), 400);
    assert_eq!(
        (by_box.column_pages_read, by_box.directory_pages - by_box.directory_pages_held),
        (24, 8),
        "{by_box:?}"
    );
    let mut s = fresh(&grid("plane-near-2"));
    let q = "SELECT id FROM s.t ORDER BY p <-> point '(1500,1500)' LIMIT 5";
    let (b, near) = s.ask(q, None).unwrap();
    exact(q, &near);
    assert_eq!(near.order, "t_p", "{near:?}");
    assert_eq!(
        (near.column_pages_read, near.directory_pages - near.directory_pages_held),
        (6, 8),
        "{near:?}"
    );
    assert_eq!(near.blocks_read, 1, "{near:?}");
    let mut got = in_order(&b);
    got.sort();
    let mut five: Vec<i64> = [(1500, 1500), (1499, 1500), (1501, 1500), (1500, 1499), (1500, 1501)]
        .iter()
        .map(|&(x, y)| grid_id(x, y))
        .collect();
    five.sort();
    assert_eq!(got, five);
    assert_eq!(in_order(&b)[0], grid_id(1500, 1500));
    // Along the table's own key: every block, and every page.
    let mut s = fresh(&grid("plane-near-3"));
    let (b, whole) = s.ask(q, Some("t_pkey")).unwrap();
    assert_eq!(whole.column_pages_read, 12_288, "{whole:?}");
    assert_eq!(whole.directory_pages, 197, "{whole:?}");
    let mut own = in_order(&b);
    own.sort();
    assert_eq!(own, five);
}

/// The ids of a question asked along an order, in its order, its reads exactly what they predicted.
fn along(s: &mut Snail, q: &str, order: Option<&str>) -> (Vec<i64>, warren_snail::read::Round) {
    let (b, round) = s.ask(q, order).unwrap_or_else(|e| panic!("{q}: {e}"));
    exact(q, &round);
    (in_order(&b), round)
}

/// A point's distance from a point, past where a NaN and the infinities come.
fn apart(x: f64, y: f64, qx: f64, qy: f64) -> (u8, f64) {
    let d = (x - qx).hypot(y - qy);
    if d.is_nan() {
        (1, 0.0)
    } else {
        (0, d)
    }
}

#[test]
fn a_nan_an_infinity_and_a_null_come_as_an_order_by_sorts_them_at_their_own_ends() {
    // A grid of 4,096 points, every hundredth NULL, and the first rows past every cell.
    let special = |i: i64| match i {
        1 => Some("(NaN,1100)".to_string()),
        2 => Some("(1100,NaN)".to_string()),
        3 => Some("(Infinity,1100)".to_string()),
        4 => Some("(-Infinity,-Infinity)".to_string()),
        5 => Some("(NaN,Infinity)".to_string()),
        i if i % 100 == 0 => None,
        i => Some(format!("({},{})", 1024 + (i * 7919) % 64, 1024 + i % 64)),
    };
    let path = table_of("plane-near-ends", "point", GeoKind::Point, &["gist"], 4096, special);
    let (qx, qy) = (1030.0, 1030.0);
    let values: Vec<(i64, Option<(f64, f64)>)> = (0..4096)
        .map(|i| {
            let xy = special(i).map(|t| {
                let (x, y) = t[1..t.len() - 1].split_once(',').unwrap();
                (x.parse::<f64>().unwrap(), y.parse::<f64>().unwrap())
            });
            (i, xy)
        })
        .collect();
    let mut sorted: Vec<(u8, (u8, f64), i64)> = values
        .iter()
        .map(|&(i, xy)| match xy {
            Some((x, y)) => (0, apart(x, y, qx, qy), i),
            None => (1, (0, 0.0), i),
        })
        .collect();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let ids = |n: usize| sorted[..n].iter().map(|r| r.2).collect::<Vec<i64>>();
    let finite = sorted
        .iter()
        .filter(|r| r.0 == 0 && r.1 .0 == 0 && r.1 .1.is_finite())
        .count();
    let nans = sorted.iter().filter(|r| r.0 == 0 && r.1 .0 == 1).count();
    let nulls = sorted.iter().filter(|r| r.0 == 1).count();
    assert_eq!((nans, nulls), (2, 41));
    for n in [1, 40, finite, finite + 1, finite + 3, finite + 3 + nans, 4096 - nulls + 1, 4096] {
        let q = format!("SELECT id FROM s.t ORDER BY v <-> point '(1030,1030)', id LIMIT {n}");
        let mut s = fresh(&path);
        let (got, round) = along(&mut s, &q, Some("t_gist"));
        assert_eq!(got, ids(n), "{q}");
        assert_eq!(round.order, "t_gist");
    }
    // Its nearest rows read no block of the NaN, the infinities or the NULLs alone.
    let mut s = fresh(&path);
    let q = "SELECT id FROM s.t ORDER BY v <-> point '(1030,1030)', id LIMIT 3";
    let (got, round) = along(&mut s, q, Some("t_gist"));
    assert_eq!(got, ids(3));
    let whole = s
        .blocks()
        .unwrap()
        .iter()
        .filter(|r| r.order == "t_gist" && r.column == "v.x")
        .count() as u64;
    assert!(round.blocks_read < whole, "{round:?}");
    // A point asked from beyond the finite is left to the table's own read.
    for value in ["(NaN,1)", "(Infinity,1)"] {
        let q = format!("SELECT id FROM s.t ORDER BY v <-> point '{value}', id LIMIT 3");
        let (_, round) = along(&mut fresh(&path), &q, None);
        assert_eq!(round.order, "t_pkey", "{q}");
    }
}

/// Rows of each type, on a coarse grid, a NaN, an infinity and a NULL among them.
fn shape_of(kind: GeoKind, i: i64) -> Option<String> {
    if i % 100 == 0 {
        return None;
    }
    let (x, y) = cell(i);
    Some(match (kind, i) {
        (GeoKind::Point, 1) => "(NaN,1100)".into(),
        (GeoKind::Point, 2) => "(Infinity,1100)".into(),
        (GeoKind::Point, _) => format!("({x},{y})"),
        (GeoKind::Box, 1) => "((NaN,1100),(1104,1104))".into(),
        (GeoKind::Box, 2) => "((-Infinity,-Infinity),(Infinity,Infinity))".into(),
        (GeoKind::Box, _) => format!("(({x},{y}),({},{}))", x + 4, y + 4),
        (GeoKind::Polygon, 1) => "((NaN,1100),(1104,1104),(1100,1104))".into(),
        (GeoKind::Polygon, 2) => "((Infinity,1100),(1104,1104),(1100,1104))".into(),
        (GeoKind::Polygon, _) => format!("(({x},{y}),({},{y}),({},{}))", x + 4, x + 4, y + 4),
        (GeoKind::Circle, 1) => "<(NaN,1100),4>".into(),
        (GeoKind::Circle, 2) => "<(1100,1100),Infinity>".into(),
        (_, _) => format!("<({x},{y}),{}>", 1 + i % 4),
    })
}

#[test]
fn every_type_and_class_with_a_distance_answers_along_its_strand_as_along_the_table_s_key() {
    for (kind, ty, methods) in [
        (GeoKind::Point, "point", &["gist", "spgist"][..]),
        (GeoKind::Box, "box", &["gist", "spgist"][..]),
        (GeoKind::Polygon, "polygon", &["gist", "spgist"][..]),
        (GeoKind::Circle, "circle", &["gist"][..]),
    ] {
        let path = table_of(&format!("plane-near-{ty}"), ty, kind, methods, 16384, |i| {
            shape_of(kind, i)
        });
        for (value, n) in [
            ("(1500,1500)", 1),
            ("(1500,1500)", 7),
            ("(1502.5,1097.25)", 100),
            ("(0,0)", 3),
            ("(3000,3000)", 16_000),
            ("(1500,1500)", 16_384),
        ] {
            let q = format!(
                "SELECT id FROM s.t ORDER BY v <-> point '{value}', id LIMIT {n}"
            );
            let (key, whole) = along(&mut fresh(&path), &q, Some("t_pkey"));
            assert_eq!(whole.order, "t_pkey");
            for m in methods {
                let name = format!("t_{m}");
                let (got, round) = along(&mut fresh(&path), &q, Some(&name));
                assert_eq!(got, key, "{q} along {name}");
                assert_eq!(round.order, name);
                assert!(round.column_pages_read <= whole.column_pages_read + 16, "{round:?}");
            }
            let (chosen, _) = along(&mut fresh(&path), &q, None);
            assert_eq!(chosen, key, "{q}");
        }
        // Written with the point on the left, by an alias and by a place.
        let by_key = "SELECT id, v <-> point '(1500,1500)' AS d FROM s.t ORDER BY d, id LIMIT 6";
        let (key, _) = along(&mut fresh(&path), by_key, Some("t_pkey"));
        let name = format!("t_{}", methods[0]);
        for q in [
            "SELECT id FROM s.t ORDER BY point '(1500,1500)' <-> v, id LIMIT 6",
            "SELECT id, v <-> point '(1500,1500)' AS d FROM s.t ORDER BY d, id LIMIT 6",
            "SELECT id, v <-> point '(1500,1500)' FROM s.t ORDER BY 2, id LIMIT 6",
            "SELECT t.id FROM s.t t ORDER BY (t.v <-> point(1500, 1500)), t.id LIMIT 6",
        ] {
            let (got, round) = along(&mut fresh(&path), q, None);
            assert_eq!(got, key, "{q}");
            assert_eq!(round.order, name, "{q}");
        }
    }
}

#[test]
fn a_tie_is_read_only_where_the_question_can_mean_it_and_with_ties_keeps_them_all() {
    // Two points at one distance from the point, straight off each side, in two blocks.
    let path = table_of("plane-near-ties", "point", GeoKind::Point, &["gist"], 16384, |i| {
        Some(format!("({},{})", cell(i).0, cell(i).1))
    });
    let centre = "(1532,1528)";
    let plain = format!("SELECT v <-> point '{centre}' FROM s.t ORDER BY v <-> point '{centre}' LIMIT 1");
    let later = format!("SELECT id FROM s.t ORDER BY v <-> point '{centre}', id LIMIT 1");
    let (_, first) = along_any(&mut fresh(&path), &plain);
    let (_, broken) = along(&mut fresh(&path), &later, None);
    assert_eq!(first.order, "t_gist");
    // The first row is its own answer where the distance is all it orders by; every row of its
    // distance is met before the id can choose.
    assert!(first.blocks_read < broken.blocks_read, "{first:?} {broken:?}");
    let (key, _) = along(&mut fresh(&path), &later, Some("t_pkey"));
    let (got, _) = along(&mut fresh(&path), &later, None);
    assert_eq!(got, key);
    let fetch = format!(
        "SELECT id FROM s.t ORDER BY v <-> point '{centre}' FETCH FIRST 1 ROW WITH TIES"
    );
    let sorted = |mut v: Vec<i64>| {
        v.sort();
        v
    };
    let (keyed, _) = along(&mut fresh(&path), &fetch, Some("t_pkey"));
    let (got, round) = along(&mut fresh(&path), &fetch, None);
    assert_eq!(round.order, "t_gist");
    assert!(keyed.len() > 1, "{keyed:?}");
    assert_eq!(sorted(got), sorted(keyed));
    // OFFSET counts, and LIMIT 0 reads no column page.
    let off = format!("SELECT id FROM s.t ORDER BY v <-> point '{centre}', id LIMIT 3 OFFSET 2");
    let (key, _) = along(&mut fresh(&path), &off, Some("t_pkey"));
    assert_eq!(along(&mut fresh(&path), &off, None).0, key);
    let none = format!("SELECT id FROM s.t ORDER BY v <-> point '{centre}' LIMIT 0");
    let (got, round) = along_any(&mut fresh(&path), &none);
    assert_eq!((got, round.column_pages_read), (0, 0));
}

/// The rows of a question's answer and its round, reads exactly what they predicted.
fn along_any(s: &mut Snail, q: &str) -> (usize, warren_snail::read::Round) {
    let (b, round) = s.ask(q, None).unwrap_or_else(|e| panic!("{q}: {e}"));
    exact(q, &round);
    (b.num_rows(), round)
}

#[test]
fn a_question_with_no_limit_or_as_many_rows_as_the_table_goes_by_pages_and_its_room() {
    let path = table_of("plane-near-all", "point", GeoKind::Point, &["gist"], 16384, |i| {
        Some(format!("({},{})", cell(i).0, cell(i).1))
    });
    let q = "SELECT id FROM s.t ORDER BY v <-> point '(1500,1500)', id";
    let (key, whole) = along(&mut fresh(&path), q, None);
    // A sort that fits costs no page, and the strand reads as many: the table's own.
    assert_eq!(whole.order, "t_pkey", "{whole:?}");
    let every = "SELECT id FROM s.t ORDER BY v <-> point '(1500,1500)', id LIMIT 16384";
    let (got, round) = along(&mut fresh(&path), every, None);
    assert_eq!((got, round.order.as_str()), (key.clone(), "t_pkey"));
    // Past its room a sort spills, and the strand reads nothing more and holds a ring of blocks.
    let dir = tmp("plane-near-all-spill");
    std::fs::create_dir_all(&dir).unwrap();
    let mut s = fresh(&path);
    s.size_rounds(warren_snail::room::Sizes::of(131_072).in_dir(&dir));
    let (got, round) = along(&mut s, q, None);
    assert_eq!(got, key);
    assert_eq!(round.order, "t_gist", "{round:?}");
    assert_eq!(round.column_pages_read, whole.column_pages_read, "{round:?}");
    assert_eq!(round.spill_pages_written, round.spill_pages_read, "{round:?}");
    let mut s = fresh(&path);
    s.size_rounds(warren_snail::room::Sizes::of(131_072).in_dir(&dir));
    let (got, by_key) = along(&mut s, q, Some("t_pkey"));
    assert_eq!(got, key);
    assert!(by_key.spill_pages_written > round.spill_pages_written, "{by_key:?} {round:?}");
    // At a room smaller than a block's rows what the walk holds is written, once, and read back once.
    let mut s = fresh(&path);
    s.size_rounds(warren_snail::room::Sizes::of(16_384).in_dir(&dir));
    let (got, tight) = along(&mut s, q, Some("t_gist"));
    assert_eq!(got, key);
    assert!(tight.spill_pages_written > 0, "{tight:?}");
    assert_eq!(tight.spill_pages_written, tight.spill_pages_read, "{tight:?}");
    // A condition the strand rules blocks out by goes along it, however few rows the box holds.
    let boxed = "SELECT id FROM s.t WHERE v <@ box '((1490,1490),(1540,1540))' \
                 ORDER BY v <-> point '(1500,1500)', id";
    let (key, whole) = along(&mut fresh(&path), boxed, Some("t_pkey"));
    let (got, round) = along(&mut fresh(&path), boxed, None);
    assert_eq!(got, key);
    assert_eq!(round.order, "t_gist");
    assert!(round.column_pages_read * 4 < whole.column_pages_read, "{round:?} {whole:?}");
}

#[test]
fn a_descending_distance_or_its_nulls_first_is_left_to_the_table_s_read() {
    let path = table_of("plane-near-desc", "point", GeoKind::Point, &["gist"], 4096, |i| {
        (i % 100 != 0).then(|| format!("({},{})", 1024 + (i * 7919) % 64, 1024 + i % 64))
    });
    for q in [
        "SELECT id FROM s.t ORDER BY v <-> point '(1030,1030)' DESC, id LIMIT 5",
        "SELECT id FROM s.t ORDER BY v <-> point '(1030,1030)' NULLS FIRST, id LIMIT 5",
        "SELECT id FROM s.t ORDER BY id, v <-> point '(1030,1030)' LIMIT 5",
        "SELECT id FROM s.t ORDER BY (v <-> point '(1030,1030)') + 1, id LIMIT 5",
        "SELECT id FROM s.t ORDER BY v <-> point(id, 0), id LIMIT 5",
    ] {
        let (_, round) = along_any(&mut fresh(&path), q);
        assert_eq!(round.order, "t_pkey", "{q}");
    }
    let (b, round) = fresh(&path)
        .ask(
            "SELECT id, count(*) FROM s.t GROUP BY id ORDER BY id LIMIT 1",
            None,
        )
        .unwrap();
    assert_eq!((b.num_rows() as u64, round.rows), (1, 1));
}

#[test]
fn a_column_off_the_strand_is_read_after_the_table_kept_to_the_keys_the_walk_found() {
    let catalog = Catalog::parse(
        "CREATE TABLE s.t (id bigint NOT NULL, v point, w bigint, z bigint, PRIMARY KEY (id)); \
         CREATE INDEX t_v ON s.t USING gist (v); \
         CREATE INDEX t_vw ON s.t USING gist (v) INCLUDE (w);",
    )
    .unwrap();
    let n = 16384;
    let path = tmp("plane-near-off.snail");
    write(
        &path,
        &catalog,
        &[(
            "s.t".into(),
            Rows {
                columns: vec![
                    Values::Int((0..n).map(Some).collect()),
                    shapes(
                        GeoKind::Point,
                        (0..n).map(|i| Some(format!("({},{})", cell(i).0, cell(i).1))),
                    ),
                    Values::Int((0..n).map(|i| Some(i * 2)).collect()),
                    Values::Int((0..n).map(|i| Some(i * 3)).collect()),
                ],
            },
        )],
    )
    .unwrap();
    let keyed = "SELECT id, z FROM s.t ORDER BY v <-> point '(1500,1500)', id LIMIT 5";
    let (key, whole) = along(&mut fresh(&path), keyed, Some("t_pkey"));
    let (got, round) = along(&mut fresh(&path), keyed, None);
    assert_eq!(got, key);
    assert_eq!(round.order, "t_v+t_pkey", "{round:?}");
    assert_eq!(round.reads.len(), 2, "{round:?}");
    // Its fifth row ties with seven more: twelve keys, each a block of the table's, four leaves' pages.
    assert_eq!(whole.column_pages_read, 320, "{whole:?}");
    assert_eq!(round.reads[0].column_pages_read, 6, "{round:?}");
    assert_eq!(round.reads[1].blocks_read, 12, "{round:?}");
    assert_eq!(round.reads[1].column_pages_read, 48, "{round:?}");
    assert_eq!(round.column_pages_read, 54, "{round:?}");
    // With the distance all it orders by, five keys are enough.
    let cut = "SELECT id, z FROM s.t ORDER BY v <-> point '(1500,1500)' LIMIT 5";
    let (b, round) = fresh(&path).ask(cut, None).unwrap();
    exact(cut, &round);
    assert_eq!(b.num_rows(), 5);
    assert_eq!(round.reads[1].blocks_read, 5, "{round:?}");
    assert_eq!(round.column_pages_read, 26, "{round:?}");
    // The answer's own columns are the table's.
    let (b, _) = fresh(&path).ask(keyed, None).unwrap();
    let z = b.column(1).as_any().downcast_ref::<Int64Array>().unwrap();
    for (i, id) in key.iter().enumerate() {
        assert_eq!(z.value(i), id * 3);
    }
    // A star reads every column so; INCLUDE holds what it names on the strand alone.
    let star = "SELECT * FROM s.t ORDER BY v <-> point '(1500,1500)', id LIMIT 5";
    let (b, round) = fresh(&path).ask(star, None).unwrap();
    exact(star, &round);
    assert_eq!(in_order(&b), key);
    assert_eq!(round.order, "t_v+t_pkey", "{round:?}");
    assert_eq!(b.num_columns(), 4);
    let included = "SELECT id, w FROM s.t ORDER BY v <-> point '(1500,1500)', id LIMIT 5";
    let (got, round) = along(&mut fresh(&path), included, None);
    assert_eq!(got, key);
    assert_eq!(round.order, "t_vw", "{round:?}");
    // A condition reading a column off every strand leaves it to the table's read; on one, it goes on.
    let off = "SELECT id FROM s.t WHERE z % 2 = 0 ORDER BY v <-> point '(1500,1500)', id LIMIT 5";
    let (_, round) = along(&mut fresh(&path), off, None);
    assert_eq!(round.order, "t_pkey", "{round:?}");
    let on = "SELECT id, z FROM s.t WHERE w % 4 = 0 ORDER BY v <-> point '(1500,1500)', id LIMIT 5";
    let (key, _) = along(&mut fresh(&path), on, Some("t_pkey"));
    let (got, round) = along(&mut fresh(&path), on, None);
    assert_eq!(got, key);
    assert_eq!(round.order, "t_vw+t_pkey", "{round:?}");
}

#[test]
fn a_condition_keeps_a_row_from_the_rows_asked_for_and_the_walk_goes_on_past_it() {
    let path = table_of("plane-near-cond", "point", GeoKind::Point, &["gist"], 16384, |i| {
        Some(format!("({},{})", cell(i).0, cell(i).1))
    });
    for q in [
        "SELECT id FROM s.t WHERE id % 2 = 0 ORDER BY v <-> point '(1500,1500)', id LIMIT 7",
        "SELECT id FROM s.t WHERE id % 1000 = 0 ORDER BY v <-> point '(1500,1500)', id LIMIT 7",
        "SELECT id FROM s.t WHERE v <@ box '((1024,1024),(1100,1100))' \
         ORDER BY v <-> point '(1500,1500)', id LIMIT 7",
        "SELECT id FROM s.t WHERE v IS NOT NULL ORDER BY v <-> point '(1500,1500)', id LIMIT 7",
        "SELECT id FROM s.t WHERE id = 5 ORDER BY v <-> point '(1500,1500)', id LIMIT 7",
        "SELECT id FROM s.t WHERE id < 0 ORDER BY v <-> point '(1500,1500)', id LIMIT 7",
    ] {
        let (key, _) = along(&mut fresh(&path), q, Some("t_pkey"));
        let (got, round) = along(&mut fresh(&path), q, Some("t_gist"));
        assert_eq!(got, key, "{q}");
        assert_eq!(round.order, "t_gist");
    }
    // Asked with a parameter for its point or its count, as a statement's binds reach it.
    let q = "SELECT id FROM s.t ORDER BY v <-> $1, id LIMIT $2";
    let (b, round) = fresh(&path)
        .ask_with(q, None, &["(1500,1500)".into(), "5".into()])
        .unwrap();
    let plain = "SELECT id FROM s.t ORDER BY v <-> point '(1500,1500)', id LIMIT 5";
    let (key, _) = along(&mut fresh(&path), plain, Some("t_pkey"));
    assert_eq!(in_order(&b), key);
    assert_eq!(round.order, "t_gist", "{round:?}");
}

#[cfg(feature = "standalone")]
#[test]
fn a_row_written_comes_where_its_distance_puts_it_and_a_page_held_is_read_from_no_file() {
    let path = table_of("plane-near-writes", "point", GeoKind::Point, &["gist"], 16384, |i| {
        Some(format!("({},{})", cell(i).0, cell(i).1))
    });
    let mut w = Snail::open_writing(&path).unwrap();
    let q = "SELECT id FROM s.t ORDER BY v <-> point '(1500.5,1500.5)', id LIMIT 3";
    // Nearer than any, a row in a block whose box it widens.
    w.execute("INSERT INTO s.t VALUES (100000, '(1500.5,1500.5)')").unwrap();
    w.execute("UPDATE s.t SET v = '(1500.5,1501.5)' WHERE id = 7").unwrap();
    let (key, _) = along(&mut fresh(&path), q, Some("t_pkey"));
    let (got, round) = along(&mut fresh(&path), q, Some("t_gist"));
    assert_eq!(got, key);
    assert_eq!(got[0], 100000);
    assert_eq!(got[1], 7);
    assert!(round.blocks_read < 32, "{round:?}");
    w.execute("DELETE FROM s.t WHERE id = 100000").unwrap();
    let (key, _) = along(&mut fresh(&path), q, Some("t_pkey"));
    let (got, _) = along(&mut fresh(&path), q, Some("t_gist"));
    assert_eq!(got, key);
    assert_eq!(got[0], 7);
    // A transaction reads its own rows; and what it wrote is gone once it rolls back.
    w.execute("BEGIN").unwrap();
    w.execute("INSERT INTO s.t VALUES (100001, '(1500.5,1500.5)')").unwrap();
    let (b, _) = w.ask(q, Some("t_gist")).unwrap();
    assert_eq!(in_order(&b)[0], 100001);
    w.execute("ROLLBACK").unwrap();
    let (b, _) = w.ask(q, Some("t_gist")).unwrap();
    assert_eq!(in_order(&b)[0], 7);
    drop(w);
    // Asked again on one open snail, its pages are found held.
    let mut s = Snail::open(&path).unwrap();
    let (first, round) = along(&mut s, q, Some("t_gist"));
    assert!(round.column_pages_read > 0, "{round:?}");
    let (again, round) = along(&mut s, q, Some("t_gist"));
    assert_eq!(again, first);
    assert_eq!(round.column_pages_read, 0, "{round:?}");
    assert_eq!(round.column_pages_held, round.column_pages_predicted, "{round:?}");
}
