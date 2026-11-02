//! The plane a GiST or SP-GiST strand on a point, a box, a polygon or a circle is cut on: a shape by
//! its bounding box's centre, x's and y's sortable bits interleaved, one a rung, x first, in 128 bits;
//! each block keeping its bounding box; and whether a block of a bounding box may hold a row an
//! operator's condition keeps.

use crate::catalog::{ColumnType, Method, Order};
use crate::directory::Span;
use crate::geo::{
    box_contain_box, box_ov, fp_ge, fp_gt, fp_le, fp_lt, Geo, GeoBox, GeoKind, Point, EPSILON,
};
use crate::{Error, Result};

/// Whether a column's values lie on the plane where a GiST or an SP-GiST strand names it.
pub(crate) fn on_plane(ty: ColumnType) -> bool {
    matches!(
        ty,
        ColumnType::Geometric(GeoKind::Point | GeoKind::Box | GeoKind::Polygon | GeoKind::Circle)
    )
}

/// A value of a column on the plane, from the bytes a row holds it as.
pub(crate) fn of_bytes(ty: ColumnType, b: &[u8]) -> Option<Geo> {
    match ty {
        ColumnType::Geometric(k) => crate::geo::geo_of_bytes(k, b).ok(),
        _ => None,
    }
}

/// A shape's bounding box, low x, low y, high x, high y: a circle's, its center give or take its
/// radius, as PostgreSQL's GiST keeps it.
pub(crate) fn bounding(g: &Geo) -> [f64; 4] {
    let corners = |b: &GeoBox| [b.low.x, b.low.y, b.high.x, b.high.y];
    match g {
        Geo::Point(p) => [p.x, p.y, p.x, p.y],
        Geo::Box(b) => corners(b),
        Geo::Polygon(p) => corners(&p.bound),
        Geo::Circle(c) => [
            c.center.x - c.radius,
            c.center.y - c.radius,
            c.center.x + c.radius,
            c.center.y + c.radius,
        ],
        Geo::Line(_) | Geo::Lseg(_) | Geo::Path(_) => {
            unreachable!("a line, a segment or a path lies on no strand of the plane")
        }
    }
}

/// The middle of a side of a bounding box, past overflow; NaN where the side runs from one infinity
/// to the other or has no end.
fn middle(low: f64, high: f64) -> f64 {
    if low == high {
        low
    } else {
        low / 2.0 + high / 2.0
    }
}

/// `v`'s bits, each lying on the even places of 128.
fn spread(v: u64) -> u128 {
    let mut x = v as u128;
    x = (x | x << 32) & 0x0000_0000_ffff_ffff_0000_0000_ffff_ffff;
    x = (x | x << 16) & 0x0000_ffff_0000_ffff_0000_ffff_0000_ffff;
    x = (x | x << 8) & 0x00ff_00ff_00ff_00ff_00ff_00ff_00ff_00ff;
    x = (x | x << 4) & 0x0f0f_0f0f_0f0f_0f0f_0f0f_0f0f_0f0f_0f0f;
    x = (x | x << 2) & 0x3333_3333_3333_3333_3333_3333_3333_3333;
    (x | x << 1) & 0x5555_5555_5555_5555_5555_5555_5555_5555
}

/// A shape's key on the plane: its bounding box's centre, x's and y's bits as their classes sort,
/// one a rung, x first, biased as a level keyed in 128 bits is.
pub(crate) fn key(g: &Geo) -> i128 {
    let [lx, ly, hx, hy] = bounding(g);
    let bits = |v: f64| (crate::scalars::float8_key(v.to_bits()) as u64) ^ (1 << 63);
    let z = spread(bits(middle(lx, hx))) << 1 | spread(bits(middle(ly, hy)));
    (z ^ (1 << 127)) as i128
}

/// The key on the plane of a value a row holds as bytes.
pub(crate) fn key_of_bytes(ty: ColumnType, b: &[u8]) -> Option<i128> {
    of_bytes(ty, b).map(|g| key(&g))
}

/// Refuses a value PostgreSQL's strand of `order` does not take: a circle whose bounding box overflows,
/// which GiST's compress function computes as `float8_pl` and `float8_mi` do; any box or polygon under an
/// EXCLUDE on SP-GiST, whose scan `geo_spgist.c` refuses.
pub(crate) fn taken(order: &Order, g: &Geo) -> Result<()> {
    if order.method == Method::Spgist
        && order.exclusion.is_some()
        && matches!(g, Geo::Box(_) | Geo::Polygon(_))
    {
        return Err(Error::new("unrecognized scankey subtype: 0"));
    }
    if let (Method::Gist, Geo::Circle(c)) = (order.method, g) {
        let over =
            |v: f64, r: f64, sum: f64| sum.is_infinite() && !v.is_infinite() && !r.is_infinite();
        let [lx, ly, hx, hy] = bounding(g);
        if over(c.center.x, c.radius, hx)
            || over(c.center.x, c.radius, lx)
            || over(c.center.y, c.radius, hy)
            || over(c.center.y, c.radius, ly)
        {
            return Err(Error::new("value out of range: overflow"));
        }
    }
    Ok(())
}

/// A shape's bounding box as the bytes a span keeps: low corner, high corner, each coordinate's bits
/// big-endian; none where a coordinate of it is NaN, which no box holds.
pub(crate) fn bounds(g: &Geo) -> Option<(Vec<u8>, Vec<u8>)> {
    let b = bounding(g);
    if b.iter().any(|v| v.is_nan()) {
        return None;
    }
    let bytes = |x: f64, y: f64| {
        let mut out = x.to_bits().to_be_bytes().to_vec();
        out.extend_from_slice(&y.to_bits().to_be_bytes());
        out
    };
    Some((bytes(b[0], b[1]), bytes(b[2], b[3])))
}

/// The two coordinates a corner's bytes hold.
fn corner(b: &[u8]) -> [f64; 2] {
    let at = |i: usize| f64::from_bits(u64::from_be_bytes(b[i..i + 8].try_into().unwrap()));
    [at(0), at(8)]
}

/// A span's bounding box widened to hold a corner each way, the coordinates compared as numbers.
pub(crate) fn widen_corners(lo: &mut Vec<u8>, hi: &mut Vec<u8>, l: &[u8], h: &[u8]) {
    let (a, b) = (corner(l), corner(lo));
    let (c, d) = (corner(h), corner(hi));
    let mut out = a[0].min(b[0]).to_bits().to_be_bytes().to_vec();
    out.extend_from_slice(&a[1].min(b[1]).to_bits().to_be_bytes());
    *lo = out;
    let mut out = c[0].max(d[0]).to_bits().to_be_bytes().to_vec();
    out.extend_from_slice(&c[1].max(d[1]).to_bits().to_be_bytes());
    *hi = out;
}

/// The operators the plane's operator classes take, by the column's type and the value's: each
/// operator of `pg_amop` for `point_ops`, `quad_point_ops`, `kd_point_ops`, `box_ops`, `circle_ops`,
/// `poly_ops` and BRIN's `box_inclusion_ops` but `<->`.
pub(crate) fn takes(column: GeoKind, op: &str, value: GeoKind) -> bool {
    use GeoKind::{Box, Circle, Point, Polygon};
    match (column, value) {
        (Point, Point) => ["<<", ">>", "~=", "<<|", "|>>", "<^", ">^"].contains(&op),
        (Point, Box | Polygon | Circle) => op == "<@",
        (Box, Point) => op == "@>",
        (Box, Box) | (Polygon, Polygon) | (Circle, Circle) => [
            "<<", "&<", "&&", "&>", ">>", "~=", "@>", "<@", "&<|", "<<|", "|>>", "|&>",
        ]
        .contains(&op),
        _ => false,
    }
}

/// The operator a value on the left reads as, the column on its left; none where it has no such
/// form.
pub(crate) fn commuted(op: &str) -> Option<&'static str> {
    Some(match op {
        "@>" => "<@",
        "<@" => "@>",
        "<<" => ">>",
        ">>" => "<<",
        "<<|" => "|>>",
        "|>>" => "<<|",
        "<^" => ">^",
        ">^" => "<^",
        "&&" => "&&",
        "~=" => "~=",
        _ => return None,
    })
}

/// Whether an EXCLUDE's element may name an operator on the plane, as PostgreSQL's `ComputeIndexAttrs`
/// asks: one the type has, commutative as `pg_operator` says, and of its operator class's family.
pub(crate) fn exclusion_operator(opclass: &str, ty: ColumnType, op: &str) -> Result<()> {
    let ColumnType::Geometric(kind) = ty else {
        return Ok(());
    };
    let t = kind.name();
    let sample = crate::geo::geo_in(
        kind,
        match kind {
            GeoKind::Point => "(0,0)",
            GeoKind::Box => "((0,0),(1,1))",
            GeoKind::Polygon => "((0,0),(1,1),(1,0))",
            _ => "<(0,0),1>",
        },
    )?;
    if crate::geo::geo_binary(op, &sample, &sample).is_none() {
        return Err(Error::new(format!("operator does not exist: {t} {op} {t}")));
    }
    let (commutative, members): (&[&str], &[&str]) = match kind {
        GeoKind::Point => (&["*", "+", "<->", "<>", "?-", "?|", "~="], &["~="]),
        GeoKind::Box => (&["&&", "<->", "=", "~="], &["&&", "~="]),
        GeoKind::Circle => (&["&&", "<->", "<>", "=", "~="], &["&&", "~="]),
        _ => (&["&&", "<->", "~="], &["&&", "~="]),
    };
    if !commutative.contains(&op) {
        return Err(Error::new(format!(
            "operator {op}({t},{t}) is not commutative"
        )));
    }
    if !members.contains(&op) {
        return Err(Error::new(format!(
            "operator {op}({t},{t}) is not a member of operator family \"{opclass}\""
        )));
    }
    Ok(())
}

/// How far past a comparison's own fuzz a rule keeps a block where the operator's values pass through
/// rounded sums and distances: twice the fuzz, and eight of the double's steps at the largest value
/// it adds.
fn margin(values: &[f64]) -> f64 {
    2.0 * EPSILON + 8.0 * f64::EPSILON * values.iter().map(|v| v.abs()).sum::<f64>()
}

/// Whether a block whose column's values lie within `span` may hold a row `column op value` keeps,
/// the column of type `column`: each rule is the operator's own test on the block's most favourable
/// corner. A block holding a shape with a NaN coordinate is never ruled out.
pub(crate) fn may_hold(column: GeoKind, op: &str, span: &Span, value: &Geo) -> bool {
    if span.empty {
        return true;
    }
    if !span.present {
        return false;
    }
    let ([lx, ly], [hx, hy]) = (corner(&span.lo), corner(&span.hi));
    let block = corners(lx, ly, hx, hy);
    match (column, value) {
        (GeoKind::Point, Geo::Point(q)) => match op {
            "<<" => fp_lt(lx, q.x),
            ">>" => fp_gt(hx, q.x),
            "<<|" | "<^" => fp_lt(ly, q.y),
            "|>>" | ">^" => fp_gt(hy, q.y),
            "~=" => within(&block, q.x, q.y, q.x, q.y, 2.0 * EPSILON),
            _ => true,
        },
        // Compared exactly, by `box_contain_point`.
        (GeoKind::Point, Geo::Box(q)) => {
            lx <= q.high.x && hx >= q.low.x && ly <= q.high.y && hy >= q.low.y
        }
        // Against the polygon's bounding box, as `point_inside` compares, where every vertex is finite.
        (GeoKind::Point, Geo::Polygon(q)) => {
            q.points
                .iter()
                .any(|p| !p.x.is_finite() || !p.y.is_finite())
                || !(fp_lt(q.bound.high.x - lx, 0.0)
                    || fp_gt(q.bound.low.x - hx, 0.0)
                    || fp_lt(q.bound.high.y - ly, 0.0)
                    || fp_gt(q.bound.low.y - hy, 0.0))
        }
        // Against the center, a coordinate at a time.
        (GeoKind::Point, Geo::Circle(c)) => {
            !(lx - c.center.x > c.radius
                || c.center.x - hx > c.radius
                || ly - c.center.y > c.radius
                || c.center.y - hy > c.radius)
        }
        (GeoKind::Box, Geo::Box(q)) => boxes(op, &block, q),
        // Compared exactly, by `box_contain_point`.
        (GeoKind::Box, Geo::Point(q)) => hx >= q.x && lx <= q.x && hy >= q.y && ly <= q.y,
        (GeoKind::Polygon, Geo::Polygon(q)) => polygons(op, &block, &q.bound),
        (GeoKind::Circle, Geo::Circle(_)) => circles(op, &block, value),
        _ => true,
    }
}

/// A box from its corners.
fn corners(lx: f64, ly: f64, hx: f64, hy: f64) -> GeoBox {
    GeoBox {
        high: Point { x: hx, y: hy },
        low: Point { x: lx, y: ly },
    }
}

/// Whether a box from (qlx, qly) to (qhx, qhy) lies inside `b`, give or take `by`, as a value equal
/// to one the block holds does.
fn within(b: &GeoBox, qlx: f64, qly: f64, qhx: f64, qhy: f64, by: f64) -> bool {
    b.low.x <= qlx + by && b.low.y <= qly + by && b.high.x + by >= qhx && b.high.y + by >= qhy
}

/// A rule for a box of the block's against a box: `box_ops`' operators, as `geo.rs` computes them.
fn boxes(op: &str, b: &GeoBox, q: &GeoBox) -> bool {
    match op {
        "<<" => fp_lt(b.low.x, q.low.x),
        "&<" => fp_le(b.low.x, q.high.x),
        ">>" => fp_gt(b.high.x, q.high.x),
        "&>" => fp_ge(b.high.x, q.low.x),
        "<<|" => fp_lt(b.low.y, q.low.y),
        "&<|" => fp_le(b.low.y, q.high.y),
        "|>>" => fp_gt(b.high.y, q.high.y),
        "|&>" => fp_ge(b.high.y, q.low.y),
        "&&" | "<@" => box_ov(b, q),
        "@>" => box_contain_box(b, q),
        "~=" => within(b, q.low.x, q.low.y, q.high.x, q.high.y, 2.0 * EPSILON),
        _ => true,
    }
}

/// A rule for a polygon of the block's against a polygon's bounding box: its sides compared exactly,
/// `&&`, `@>` and `<@` each asking its bounding boxes first.
fn polygons(op: &str, b: &GeoBox, q: &GeoBox) -> bool {
    match op {
        "<<" => b.low.x < q.low.x,
        "&<" => b.low.x <= q.high.x,
        ">>" => b.high.x > q.high.x,
        "&>" => b.high.x >= q.low.x,
        "<<|" => b.low.y < q.low.y,
        "&<|" => b.low.y <= q.high.y,
        "|>>" => b.high.y > q.high.y,
        "|&>" => b.high.y >= q.low.y,
        _ => boxes(op, b, q),
    }
}

/// A rule for a circle of the block's against a circle: the sides by its own sums, `&&`, `<@`, `@>`
/// and `~=` by a distance, through rounded sums, so give or take a margin.
fn circles(op: &str, b: &GeoBox, value: &Geo) -> bool {
    let [qlx, qly, qhx, qhy] = bounding(value);
    let q = corners(qlx, qly, qhx, qhy);
    let m = margin(&[
        b.low.x, b.low.y, b.high.x, b.high.y, q.low.x, q.low.y, q.high.x, q.high.y,
    ]);
    match op {
        "&&" | "<@" => {
            !(b.low.x - q.high.x > m
                || q.low.x - b.high.x > m
                || b.low.y - q.high.y > m
                || q.low.y - b.high.y > m)
        }
        "@>" | "~=" => {
            !(b.low.x - q.low.x > m
                || q.high.x - b.high.x > m
                || b.low.y - q.low.y > m
                || q.high.y - b.high.y > m)
        }
        _ => boxes(op, b, &q),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::geo_in;

    fn rungs(k: i128) -> Vec<u8> {
        let z = k.wrapping_sub(i128::MIN) as u128;
        (0..128).map(|r| ((z >> (127 - r)) & 1) as u8).collect()
    }

    fn point(s: &str) -> Geo {
        geo_in(GeoKind::Point, s).unwrap()
    }

    #[test]
    fn a_key_interleaves_x_s_and_y_s_sortable_bits_x_first() {
        let sortable = |v: f64| (crate::scalars::float8_key(v.to_bits()) as u64) ^ (1 << 63);
        for (x, y) in [
            (0.0, 0.0),
            (1.0, -1.0),
            (1024.5, 2047.25),
            (-3.5e10, f64::INFINITY),
            (f64::NEG_INFINITY, f64::NAN),
        ] {
            let p = Geo::Point(crate::geo::Point { x, y });
            let r = rungs(key(&p));
            for i in 0..64 {
                assert_eq!(
                    r[2 * i],
                    ((sortable(x) >> (63 - i)) & 1) as u8,
                    "{x} rung {i}"
                );
                assert_eq!(
                    r[2 * i + 1],
                    ((sortable(y) >> (63 - i)) & 1) as u8,
                    "{y} rung {i}"
                );
            }
        }
        // x's first differing bit decides before y's at the same place.
        assert!(key(&point("(1025,1024)")) > key(&point("(1024,1025)")));
    }

    #[test]
    fn a_coordinate_s_keys_rise_with_it_and_minus_zero_and_zero_share_one() {
        let at = |v: f64| key(&Geo::Point(crate::geo::Point { x: v, y: 0.0 }));
        let walk = [
            f64::NEG_INFINITY,
            -1e300,
            -1.5,
            -f64::MIN_POSITIVE,
            0.0,
            f64::MIN_POSITIVE,
            1.5,
            1e300,
            f64::INFINITY,
            f64::NAN,
        ];
        assert!(walk.windows(2).all(|w| at(w[0]) < at(w[1])), "{walk:?}");
        assert_eq!(at(-0.0), at(0.0));
    }

    #[test]
    fn a_shape_is_keyed_by_its_bounding_box_s_centre() {
        let p = key(&point("(3,5)"));
        for (kind, s) in [
            (GeoKind::Box, "((2,4),(4,6))"),
            (GeoKind::Polygon, "((2,4),(4,4),(4,6),(2,6))"),
            (GeoKind::Circle, "<(3,5),9>"),
        ] {
            assert_eq!(key(&geo_in(kind, s).unwrap()), p, "{s}");
        }
        // Past overflow, and from one infinity to the other.
        let wide = geo_in(GeoKind::Box, "((-1e308,-1e308),(1.5e308,1.5e308))").unwrap();
        assert_eq!(key(&wide), key(&point("(2.5e307,2.5e307)")));
        let all = geo_in(GeoKind::Box, "((-Infinity,0),(Infinity,0))").unwrap();
        assert_eq!(key(&all), key(&point("(NaN,0)")));
    }

    #[test]
    fn a_circle_whose_bounding_box_overflows_is_refused_by_gist_as_postgresql_refuses_it() {
        let catalog = crate::catalog::Catalog::parse(
            "CREATE TABLE s.t (id int PRIMARY KEY, c circle, p point); \
             CREATE INDEX t_c ON s.t USING gist (c); \
             CREATE INDEX t_p ON s.t USING gist (p);",
        )
        .unwrap();
        let t = catalog.table("s.t").unwrap();
        let (c, p) = (t.order("t_c").unwrap(), t.order("t_p").unwrap());
        let huge = geo_in(GeoKind::Circle, "<(1e308,0),1e308>").unwrap();
        assert!(taken(c, &huge).is_err());
        let fine = geo_in(GeoKind::Circle, "<(Infinity,0),1>").unwrap();
        assert!(taken(c, &fine).is_ok());
        assert!(taken(p, &point("(1e308,1e308)")).is_ok());
    }

    /// Every rule against the operator it stands for: a seeded walk of blocks of a few shapes each, and
    /// values near them. A block is never ruled out where a row of it passes.
    #[test]
    fn a_block_is_ruled_out_only_where_no_shape_it_holds_passes_the_operator() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let eps = EPSILON;
        let picks = [
            0.0,
            1.0,
            2.0,
            3.0,
            4.0,
            1.0 + eps,
            1.0 - eps,
            2.0 + 2.0 * eps,
            3.0 - 3.0 * eps,
            1e10,
            1e10 + 1e-5,
            1.0 + eps / 2.0,
            3.0 - eps / 2.0,
            -1.0,
            -2.5,
            eps / 2.0,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
        ];
        let kinds = [
            GeoKind::Point,
            GeoKind::Box,
            GeoKind::Polygon,
            GeoKind::Circle,
        ];
        let pick = |next: &mut dyn FnMut() -> u64| picks[(next() % picks.len() as u64) as usize];
        let shape = |kind: GeoKind, next: &mut dyn FnMut() -> u64| -> Geo {
            let s = match kind {
                GeoKind::Point => format!("({},{})", pick(next), pick(next)),
                GeoKind::Box => format!(
                    "(({},{}),({},{}))",
                    pick(next),
                    pick(next),
                    pick(next),
                    pick(next)
                ),
                GeoKind::Polygon => format!(
                    "(({},{}),({},{}),({},{}))",
                    pick(next),
                    pick(next),
                    pick(next),
                    pick(next),
                    pick(next),
                    pick(next)
                ),
                _ => format!("<({},{}),{}>", pick(next), pick(next), pick(next).abs()),
            };
            geo_in(kind, &s.replace("inf", "Infinity")).unwrap()
        };
        let mut checked = 0u64;
        let mut ruled = 0u64;
        for _ in 0..40_000 {
            let kind = kinds[(next() % 4) as usize];
            let rows: Vec<Geo> = (0..(1 + next() % 4))
                .map(|_| shape(kind, &mut next))
                .collect();
            let mut span = Span {
                plane: true,
                ..Span::default()
            };
            for r in &rows {
                match bounds(r) {
                    Some((l, h)) => span.widen(Some((&l, &h))),
                    None => span.widen(None),
                }
            }
            let values: &[GeoKind] = match kind {
                GeoKind::Point => &kinds,
                GeoKind::Box => &kinds[..2],
                k => std::slice::from_ref(match k {
                    GeoKind::Polygon => &kinds[2],
                    _ => &kinds[3],
                }),
            };
            let value_kind = values[(next() % values.len() as u64) as usize];
            let q = shape(value_kind, &mut next);
            for op in [
                "<<", ">>", "~=", "<<|", "|>>", "<^", ">^", "<@", "&<", "&&", "&>", "@>", "&<|",
                "|&>",
            ] {
                if !takes(kind, op, value_kind) {
                    continue;
                }
                let held = rows.iter().any(|r| {
                    matches!(
                        crate::geo::geo_binary(op, r, &q),
                        Some(Ok(crate::geo::GeoValue::Bool(true)))
                    )
                });
                let may = may_hold(kind, op, &span, &q);
                checked += 1;
                ruled += u64::from(!may);
                assert!(
                    may || !held,
                    "{op}: {:?} against {q:?} ruled out by {span:?}",
                    rows
                );
            }
        }
        // The walk ruled blocks out, so the rules are not vacuous.
        assert!(checked > 100_000 && ruled > 10_000, "{checked} {ruled}");
    }

    #[test]
    fn a_block_holding_a_nan_is_never_ruled_out_and_a_nan_value_keeps_only_its_own() {
        let mut span = Span {
            plane: true,
            ..Span::default()
        };
        let nan = geo_in(GeoKind::Point, "(NaN,1)").unwrap();
        assert!(bounds(&nan).is_none());
        span.widen(None);
        assert!(may_hold(GeoKind::Point, "~=", &span, &nan));
        assert!(may_hold(GeoKind::Point, "<<", &span, &point("(-1e300,0)")));
        let (l, h) = bounds(&point("(1,1)")).unwrap();
        let mut plain = Span {
            plane: true,
            ..Span::default()
        };
        plain.widen(Some((&l, &h)));
        assert!(!may_hold(GeoKind::Point, "~=", &plain, &nan));
        assert!(!may_hold(GeoKind::Point, "~=", &plain, &point("(2,1)")));
        assert!(may_hold(
            GeoKind::Point,
            "~=",
            &plain,
            &point("(1.00000000001,1)")
        ));
    }
}
