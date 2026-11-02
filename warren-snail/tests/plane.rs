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

#[test]
fn a_distance_is_answered_as_a_row_at_a_time_and_a_condition_beside_it_keeps_its_strand() {
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
         EXCLUDE USING spgist (g WITH &&), \
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
        // Both on a point of one block of the strand, each past the other's box.
        run(
            &mut a,
            "INSERT INTO s.t (id, p) VALUES (9000, '(1024.5,1024.5)')",
        );
        run(
            &mut b,
            "INSERT INTO s.t (id, p) VALUES (9001, '(1024.25,1024.25)')",
        );
        // At SERIALIZABLE a read marks what the other writes, so only the snapshot's level reads here.
        if level == "REPEATABLE READ" {
            let q = "SELECT id FROM s.t WHERE p ~= '(1024.25,1024.25)'";
            assert_eq!(ids(&mut a, q, "t_p"), Vec::<i64>::new());
            assert_eq!(ids(&mut b, q, "t_p"), [9001]);
        }
        run(&mut a, "COMMIT");
        run(&mut b, "COMMIT");
        for (q, want) in [
            ("SELECT id FROM s.t WHERE p ~= '(1024.5,1024.5)'", 9000),
            ("SELECT id FROM s.t WHERE p ~= '(1024.25,1024.25)'", 9001),
        ] {
            assert_eq!(ids(&mut a, q, "t_p"), [want], "{level}: {q}");
            assert_eq!(ids(&mut a, q, "t_ps"), [want], "{level}: {q}");
        }
    }
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
        "INSERT INTO s.t (id, p) VALUES (9000, '(9100,9100)')",
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
