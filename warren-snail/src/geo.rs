//! The geometric types: point, line, lseg, box, path, polygon and circle, each read and written as
//! PostgreSQL's input and output functions read and write it, with every operator and every cast
//! PostgreSQL holds among them, each computed as PostgreSQL's geo_ops.c computes it: its steps in
//! its order, float8_pl, float8_mi, float8_mul and float8_div with their refusals, and its EPSILON
//! comparisons. Every value keeps its own float bits, `-0` and NaN among them. Where one step
//! takes two that can each be refused, the second is taken first, as PostgreSQL's build takes
//! them.

use crate::scalars::{float_parse, float_text};
use crate::{Error, Result};
use std::f64::consts::PI;

/// The tolerance of PostgreSQL's fuzzy comparisons, geo_decls.h's `EPSILON`.
pub(crate) const EPSILON: f64 = 1.0E-06;

/// What `lseg_crossing` answers where the segment holds the point.
const POINT_ON_POLYGON: i32 = i32::MAX;

/// The bytes before a path's points, as `offsetof(PATH, p)` counts them.
const PATH_HEADER: u64 = 16;

/// The bytes before a polygon's points, as `offsetof(POLYGON, p)` counts them.
const POLYGON_HEADER: u64 = 40;

/// A point, as geo_decls.h's `Point` holds one: x and y.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

/// A line, as geo_decls.h's `LINE` holds one: A, B and C of Ax + By + C = 0.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct Line {
    pub a: f64,
    pub b: f64,
    pub c: f64,
}

/// A line segment, as geo_decls.h's `LSEG` holds one: its two end points in order.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct Lseg {
    pub p: [Point; 2],
}

/// A box, as geo_decls.h's `BOX` holds one: its high corner, then its low.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct GeoBox {
    pub high: Point,
    pub low: Point,
}

/// A path, as geo_decls.h's `PATH` holds one: closed or open, and its points.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Path {
    pub closed: bool,
    pub points: Vec<Point>,
}

/// A polygon, as geo_decls.h's `POLYGON` holds one: its points and the bounding box kept with them.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Polygon {
    pub points: Vec<Point>,
    pub bound: GeoBox,
}

/// A circle, as geo_decls.h's `CIRCLE` holds one: its center and its radius.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct Circle {
    pub center: Point,
    pub radius: f64,
}

/// A value of one of the seven geometric types.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Geo {
    Point(Point),
    Line(Line),
    Lseg(Lseg),
    Box(GeoBox),
    Path(Path),
    Polygon(Polygon),
    Circle(Circle),
}

/// The seven geometric types.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum GeoKind {
    Point,
    Line,
    Lseg,
    Box,
    Path,
    Polygon,
    Circle,
}

/// What a geometric operator gives: a geometric value, a boolean, a double precision, an integer
/// (a path's or a polygon's count of points), or NULL.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum GeoValue {
    Geo(Geo),
    Bool(bool),
    Float(f64),
    Int(i32),
    Null,
}

impl GeoKind {
    /// Every geometric type, in the order geo_decls.h declares them.
    pub const ALL: [GeoKind; 7] = [
        GeoKind::Point,
        GeoKind::Lseg,
        GeoKind::Path,
        GeoKind::Line,
        GeoKind::Box,
        GeoKind::Polygon,
        GeoKind::Circle,
    ];

    /// The type's name as PostgreSQL spells it.
    pub fn name(self) -> &'static str {
        match self {
            GeoKind::Point => "point",
            GeoKind::Line => "line",
            GeoKind::Lseg => "lseg",
            GeoKind::Box => "box",
            GeoKind::Path => "path",
            GeoKind::Polygon => "polygon",
            GeoKind::Circle => "circle",
        }
    }

    /// The type named `name`, as PostgreSQL spells it.
    pub fn of_name(name: &str) -> Option<GeoKind> {
        GeoKind::ALL.into_iter().find(|k| k.name() == name)
    }

    /// How many float8 leaves a value of the type holds where it holds a fixed number: a point
    /// 2, a line 3, an lseg 4, a box 4, a circle 3; none for a path or a polygon.
    pub fn fixed_leaves(self) -> Option<usize> {
        match self {
            GeoKind::Point => Some(2),
            GeoKind::Line | GeoKind::Circle => Some(3),
            GeoKind::Lseg | GeoKind::Box => Some(4),
            GeoKind::Path | GeoKind::Polygon => None,
        }
    }
}

impl Geo {
    /// The value's type.
    pub fn kind(&self) -> GeoKind {
        match self {
            Geo::Point(_) => GeoKind::Point,
            Geo::Line(_) => GeoKind::Line,
            Geo::Lseg(_) => GeoKind::Lseg,
            Geo::Box(_) => GeoKind::Box,
            Geo::Path(_) => GeoKind::Path,
            Geo::Polygon(_) => GeoKind::Polygon,
            Geo::Circle(_) => GeoKind::Circle,
        }
    }
}

impl Polygon {
    /// A polygon of `points`, with its bounding box as `make_bound_box` makes it (poly_in,
    /// path_poly, circle_poly). `points` holds at least one.
    pub fn new(points: Vec<Point>) -> Polygon {
        let bound = make_bound_box(&points);
        Polygon { points, bound }
    }
}

fn overflow() -> Error {
    Error::new("value out of range: overflow")
}

fn underflow() -> Error {
    Error::new("value out of range: underflow")
}

/// `float8_pl`: refused where finite values overflow.
fn f8_pl(a: f64, b: f64) -> Result<f64> {
    let r = a + b;
    if r.is_infinite() && !a.is_infinite() && !b.is_infinite() {
        return Err(overflow());
    }
    Ok(r)
}

/// `float8_mi`: refused where finite values overflow.
fn f8_mi(a: f64, b: f64) -> Result<f64> {
    let r = a - b;
    if r.is_infinite() && !a.is_infinite() && !b.is_infinite() {
        return Err(overflow());
    }
    Ok(r)
}

/// `float8_mul`: refused where finite values overflow, or values not zero give zero.
fn f8_mul(a: f64, b: f64) -> Result<f64> {
    let r = a * b;
    if r.is_infinite() && !a.is_infinite() && !b.is_infinite() {
        return Err(overflow());
    }
    if r == 0.0 && a != 0.0 && b != 0.0 {
        return Err(underflow());
    }
    Ok(r)
}

/// `float8_div`: refused by a zero divisor unless the dividend is NaN, where a finite dividend
/// overflows, or where a dividend not zero gives zero over a finite divisor.
fn f8_div(a: f64, b: f64) -> Result<f64> {
    if b == 0.0 && !a.is_nan() {
        return Err(Error::new("division by zero"));
    }
    let r = a / b;
    if r.is_infinite() && !a.is_infinite() {
        return Err(overflow());
    }
    if r == 0.0 && a != 0.0 && !b.is_infinite() {
        return Err(underflow());
    }
    Ok(r)
}

/// `float8_eq`: every NaN equal, and unequal to any other value.
fn f8_eq(a: f64, b: f64) -> bool {
    if a.is_nan() {
        b.is_nan()
    } else {
        !b.is_nan() && a == b
    }
}

/// `float8_lt`: NaN above every other value.
fn f8_lt(a: f64, b: f64) -> bool {
    !a.is_nan() && (b.is_nan() || a < b)
}

/// `float8_gt`: NaN above every other value.
fn f8_gt(a: f64, b: f64) -> bool {
    !b.is_nan() && (a.is_nan() || a > b)
}

/// `float8_min`.
fn f8_min(a: f64, b: f64) -> f64 {
    if f8_lt(a, b) {
        a
    } else {
        b
    }
}

/// `float8_max`.
fn f8_max(a: f64, b: f64) -> f64 {
    if f8_gt(a, b) {
        a
    } else {
        b
    }
}

/// `FPzero`.
fn fp_zero(a: f64) -> bool {
    a.abs() <= EPSILON
}

/// `FPeq`.
fn fp_eq(a: f64, b: f64) -> bool {
    a == b || (a - b).abs() <= EPSILON
}

/// `FPne`.
fn fp_ne(a: f64, b: f64) -> bool {
    a != b && (a - b).abs() > EPSILON
}

/// `FPlt`.
pub(crate) fn fp_lt(a: f64, b: f64) -> bool {
    a + EPSILON < b
}

/// `FPle`.
pub(crate) fn fp_le(a: f64, b: f64) -> bool {
    a <= b + EPSILON
}

/// `FPgt`.
pub(crate) fn fp_gt(a: f64, b: f64) -> bool {
    a > b + EPSILON
}

/// `FPge`.
pub(crate) fn fp_ge(a: f64, b: f64) -> bool {
    a + EPSILON >= b
}

thread_local! {
    /// Whether a hypotenuse is computed as releases up to 18 compute it.
    static OWN_HYPOT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// What `f` comes to as a release computes it: up to 18 geo_ops.c's `pg_hypot`, from 19 the C library's.
pub(crate) fn at_release<T>(release: u8, f: impl FnOnce() -> T) -> T {
    let before = OWN_HYPOT.replace(release <= 18);
    let out = f();
    OWN_HYPOT.set(before);
    out
}

/// `HYPOT`, as the release the value is read under computes it: `hypot`, or `pg_hypot`, which rounds
/// otherwise and refuses what passes a double.
fn hypot(x: f64, y: f64) -> Result<f64> {
    if !OWN_HYPOT.get() {
        return Ok(x.hypot(y));
    }
    if x.is_infinite() || y.is_infinite() {
        return Ok(f64::INFINITY);
    }
    if x.is_nan() || y.is_nan() {
        return Ok(f64::NAN);
    }
    let (x, y) = (x.abs().max(y.abs()), x.abs().min(y.abs()));
    if y == 0.0 {
        return Ok(x);
    }
    let yx = y / x;
    let result = x * (1.0 + yx * yx).sqrt();
    if result.is_infinite() {
        return Err(overflow());
    }
    if result == 0.0 {
        return Err(underflow());
    }
    Ok(result)
}

/// White space as C's `isspace` takes it under the C locale.
fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r')
}

/// The byte at `i`, as a C string reads it: 0 at its end.
fn at(b: &[u8], i: usize) -> u8 {
    b.get(i).copied().unwrap_or(0)
}

/// Where white space starting at `i` ends.
fn skip_space(b: &[u8], mut i: usize) -> usize {
    while is_space(at(b, i)) {
        i += 1;
    }
    i
}

/// Whether the `(` at `i` is the last in the string, as `strrchr(s, '(') == s` asks.
fn last_paren(b: &[u8], i: usize) -> bool {
    b[i + 1..]
        .iter()
        .take_while(|&&c| c != 0)
        .all(|&c| c != b'(')
}

/// The points `pair_count` finds by counting commas: half their count, rounded up, where it is
/// odd; else -1.
fn pair_count(b: &[u8]) -> i64 {
    let n = b
        .iter()
        .take_while(|&&c| c != 0)
        .filter(|&&c| c == b',')
        .count();
    if !n.is_multiple_of(2) {
        n.div_ceil(2) as i64
    } else {
        -1
    }
}

/// Whether `npts` points after a header of `header` bytes pass the 32-bit size PostgreSQL
/// allocates them in, as path_in and poly_in check it.
fn too_many(npts: u64, header: u64) -> bool {
    npts * 16 + header > i32::MAX as u64
}

fn too_many_error() -> Error {
    Error::new("too many points requested")
}

/// Where glibc's `strtod` stops reading a number starting at `i`: a decimal or hex number, an
/// infinity or a NaN; none where it reads nothing.
fn number_end(b: &[u8], mut i: usize) -> Option<usize> {
    if matches!(at(b, i), b'+' | b'-') {
        i += 1;
    }
    let word = |i: usize, w: &str| {
        b.len() >= i + w.len() && b[i..i + w.len()].eq_ignore_ascii_case(w.as_bytes())
    };
    if word(i, "infinity") {
        return Some(i + 8);
    }
    if word(i, "inf") {
        return Some(i + 3);
    }
    if word(i, "nan") {
        let mut end = i + 3;
        if at(b, end) == b'(' {
            let mut k = end + 1;
            while at(b, k).is_ascii_alphanumeric() || at(b, k) == b'_' {
                k += 1;
            }
            if at(b, k) == b')' {
                end = k + 1;
            }
        }
        return Some(end);
    }
    let hex = at(b, i) == b'0'
        && matches!(at(b, i + 1), b'x' | b'X')
        && (at(b, i + 2).is_ascii_hexdigit()
            || (at(b, i + 2) == b'.' && at(b, i + 3).is_ascii_hexdigit()));
    let (digit, marker): (fn(&u8) -> bool, u8) = if hex {
        i += 2;
        (u8::is_ascii_hexdigit, b'p')
    } else {
        (u8::is_ascii_digit, b'e')
    };
    let mut seen = false;
    while digit(&at(b, i)) {
        i += 1;
        seen = true;
    }
    if at(b, i) == b'.' {
        let mut k = i + 1;
        while digit(&at(b, k)) {
            k += 1;
            seen = true;
        }
        if seen {
            i = k;
        }
    }
    if !seen {
        return None;
    }
    if at(b, i).to_ascii_lowercase() == marker {
        let mut k = i + 1;
        if matches!(at(b, k), b'+' | b'-') {
            k += 1;
        }
        if at(b, k).is_ascii_digit() {
            while at(b, k).is_ascii_digit() {
                k += 1;
            }
            i = k;
        }
    }
    Some(i)
}

/// One geometric input string being read, and the type it is read as.
struct Reader<'a> {
    s: &'a str,
    b: &'a [u8],
    kind: &'static str,
}

impl Reader<'_> {
    fn wrong(&self) -> Error {
        Error::new(format!(
            "invalid input syntax for type {}: \"{}\"",
            self.kind, self.s
        ))
    }

    /// A number as `single_decode` reads it through `float8in_internal`: white space, what
    /// `strtod` reads, white space. The number and where the reading stopped.
    fn single(&self, i: usize) -> Result<(f64, usize)> {
        let start = skip_space(self.b, i);
        if at(self.b, start) == 0 {
            return Err(self.wrong());
        }
        let end = number_end(self.b, start).ok_or_else(|| self.wrong())?;
        let bits = float_parse(&self.s[start..end], true).map_err(|e| {
            if e.0.contains("out of range") {
                e
            } else {
                self.wrong()
            }
        })?;
        Ok((f64::from_bits(bits), skip_space(self.b, end)))
    }

    /// A point as `pair_decode` reads one: `x,y`, or `(x,y)` and white space; where `to_end`, the
    /// string ends there.
    fn pair(&self, i: usize, to_end: bool) -> Result<(Point, usize)> {
        let mut i = skip_space(self.b, i);
        let delim = at(self.b, i) == b'(';
        if delim {
            i += 1;
        }
        let (x, i) = self.single(i)?;
        if at(self.b, i) != b',' {
            return Err(self.wrong());
        }
        let (y, mut i) = self.single(i + 1)?;
        if delim {
            if at(self.b, i) != b')' {
                return Err(self.wrong());
            }
            i = skip_space(self.b, i + 1);
        }
        if to_end && at(self.b, i) != 0 {
            return Err(self.wrong());
        }
        Ok((Point { x, y }, i))
    }

    /// `npts` points as `path_decode` reads them: `[` (where `open_ok`) or `(` around them, or
    /// neither, each a pair and a comma after it; where `to_end`, the string ends there. The
    /// points, whether they opened with `[`, and where the reading stopped.
    fn path(
        &self,
        i: usize,
        open_ok: bool,
        npts: usize,
        to_end: bool,
    ) -> Result<(Vec<Point>, bool, usize)> {
        let b = self.b;
        let mut i = skip_space(b, i);
        let mut depth = 0;
        let open = at(b, i) == b'[';
        if open {
            if !open_ok {
                return Err(self.wrong());
            }
            depth += 1;
            i += 1;
        } else if at(b, i) == b'(' {
            let cp = skip_space(b, i + 1);
            if at(b, cp) == b'(' || last_paren(b, i) {
                depth += 1;
                i = cp;
            }
        }
        let mut points = Vec::with_capacity(npts);
        for _ in 0..npts {
            let (p, j) = self.pair(i, false)?;
            points.push(p);
            i = j;
            if at(b, i) == b',' {
                i += 1;
            }
        }
        while depth > 0 {
            let c = at(b, i);
            if c == b')' || (c == b']' && open && depth == 1) {
                depth -= 1;
                i = skip_space(b, i + 1);
            } else {
                return Err(self.wrong());
            }
        }
        if to_end && at(b, i) != 0 {
            return Err(self.wrong());
        }
        Ok((points, open, i))
    }
}

/// A box as `box_in` reads one: two corners, `[` refused, reordered so the first is the high.
fn box_in(r: &Reader) -> Result<GeoBox> {
    let (p, _, _) = r.path(0, false, 2, true)?;
    let (mut high, mut low) = (p[0], p[1]);
    if f8_lt(high.x, low.x) {
        std::mem::swap(&mut high.x, &mut low.x);
    }
    if f8_lt(high.y, low.y) {
        std::mem::swap(&mut high.y, &mut low.y);
    }
    Ok(GeoBox { high, low })
}

/// A line as `line_in` reads one: `{A,B,C}`, A and B not both zero, or two distinct points
/// written as an lseg is.
fn line_in(r: &Reader) -> Result<Line> {
    let b = r.b;
    let s = skip_space(b, 0);
    if at(b, s) == b'{' {
        let (a, i) = r.single(s + 1)?;
        if at(b, i) != b',' {
            return Err(r.wrong());
        }
        let (lb, i) = r.single(i + 1)?;
        if at(b, i) != b',' {
            return Err(r.wrong());
        }
        let (c, i) = r.single(i + 1)?;
        if at(b, i) != b'}' || at(b, skip_space(b, i + 1)) != 0 {
            return Err(r.wrong());
        }
        if fp_zero(a) && fp_zero(lb) {
            return Err(Error::new(
                "invalid line specification: A and B cannot both be zero",
            ));
        }
        return Ok(Line { a, b: lb, c });
    }
    let (p, _, _) = r.path(s, true, 2, true)?;
    if point_eq_point(&p[0], &p[1]) {
        return Err(Error::new(
            "invalid line specification: must be two distinct points",
        ));
    }
    line_construct(&p[0], point_sl(&p[0], &p[1])?)
}

/// A path as `path_in` reads one: its points counted by its commas, closed unless it opens with
/// `[`.
fn path_in(r: &Reader) -> Result<Path> {
    let b = r.b;
    let npts = pair_count(b);
    if npts <= 0 {
        return Err(r.wrong());
    }
    let mut s = skip_space(b, 0);
    let mut depth = 0;
    if at(b, s) == b'(' && last_paren(b, s) {
        s += 1;
        depth += 1;
    }
    if too_many(npts as u64, PATH_HEADER) {
        return Err(too_many_error());
    }
    let (points, open, mut s) = r.path(s, true, npts as usize, false)?;
    if depth >= 1 {
        if at(b, s) != b')' {
            return Err(r.wrong());
        }
        s = skip_space(b, s + 1);
    }
    if at(b, s) != 0 {
        return Err(r.wrong());
    }
    Ok(Path {
        closed: !open,
        points,
    })
}

/// A polygon as `poly_in` reads one: its points counted by its commas, `[` refused, and its
/// bounding box made.
fn poly_in(r: &Reader) -> Result<Polygon> {
    let npts = pair_count(r.b);
    if npts <= 0 {
        return Err(r.wrong());
    }
    if too_many(npts as u64, POLYGON_HEADER) {
        return Err(too_many_error());
    }
    let (points, _, _) = r.path(0, false, npts as usize, true)?;
    Ok(Polygon::new(points))
}

/// A circle as `circle_in` reads one: `<(x,y),r>`, `((x,y),r)`, `(x,y),r` or `x,y,r`, the radius
/// not below zero (NaN taken).
fn circle_in(r: &Reader) -> Result<Circle> {
    let b = r.b;
    let mut s = skip_space(b, 0);
    let mut depth = 0;
    if at(b, s) == b'<' {
        depth += 1;
        s += 1;
    } else if at(b, s) == b'(' {
        let cp = skip_space(b, s + 1);
        if at(b, cp) == b'(' {
            depth += 1;
            s = cp;
        }
    }
    let (center, mut s) = r.pair(s, false)?;
    if at(b, s) == b',' {
        s += 1;
    }
    let (radius, mut s) = r.single(s)?;
    if radius < 0.0 {
        return Err(r.wrong());
    }
    while depth > 0 {
        let c = at(b, s);
        if c == b')' || (c == b'>' && depth == 1) {
            depth -= 1;
            s = skip_space(b, s + 1);
        } else {
            return Err(r.wrong());
        }
    }
    if at(b, s) != 0 {
        return Err(r.wrong());
    }
    Ok(Circle { center, radius })
}

/// A value of type `kind` as its input function reads `s`: point_in, line_in, lseg_in, box_in,
/// path_in, poly_in or circle_in, refused with PostgreSQL's own message.
pub fn geo_in(kind: GeoKind, s: &str) -> Result<Geo> {
    let r = Reader {
        s,
        b: s.as_bytes(),
        kind: kind.name(),
    };
    Ok(match kind {
        GeoKind::Point => Geo::Point(r.pair(0, true)?.0),
        GeoKind::Line => Geo::Line(line_in(&r)?),
        GeoKind::Lseg => {
            let (p, _, _) = r.path(0, true, 2, true)?;
            Geo::Lseg(Lseg { p: [p[0], p[1]] })
        }
        GeoKind::Box => Geo::Box(box_in(&r)?),
        GeoKind::Path => Geo::Path(path_in(&r)?),
        GeoKind::Polygon => Geo::Polygon(poly_in(&r)?),
        GeoKind::Circle => Geo::Circle(circle_in(&r)?),
    })
}

/// A float8 as `float8out_internal` writes it with `extra_float_digits` at 1.
fn f8_text(v: f64) -> String {
    float_text(v.to_bits(), true)
}

/// A point as `pair_encode` writes it inside its parentheses.
fn point_text(p: &Point) -> String {
    format!("({},{})", f8_text(p.x), f8_text(p.y))
}

/// Points as `path_encode` writes them, commas between.
fn points_text(ps: &[Point]) -> String {
    ps.iter().map(point_text).collect::<Vec<_>>().join(",")
}

/// A value as its output function writes it: point_out `(x,y)`, line_out `{A,B,C}`, lseg_out
/// `[(x1,y1),(x2,y2)]`, box_out `(x1,y1),(x2,y2)`, path_out `[…]` open or `(…)` closed, poly_out
/// `(…)` and circle_out `<(x,y),r>`.
pub fn geo_out(g: &Geo) -> String {
    match g {
        Geo::Point(p) => point_text(p),
        Geo::Line(l) => format!("{{{},{},{}}}", f8_text(l.a), f8_text(l.b), f8_text(l.c)),
        Geo::Lseg(l) => format!("[{}]", points_text(&l.p)),
        Geo::Box(b) => points_text(&[b.high, b.low]),
        Geo::Path(p) if p.closed => format!("({})", points_text(&p.points)),
        Geo::Path(p) => format!("[{}]", points_text(&p.points)),
        Geo::Polygon(p) => format!("({})", points_text(&p.points)),
        Geo::Circle(c) => format!("<{},{}>", point_text(&c.center), f8_text(c.radius)),
    }
}

/// A value's float8 leaves in the order its type lays them out: a point x, y; a line A, B, C; an
/// lseg x1, y1, x2, y2; a box the high corner's x, y, then the low's; a circle the center's x, y,
/// then the radius; a path or a polygon each point's x, y in order (a path's closed flag apart,
/// `geo_closed`; a polygon's bounding box not among them).
pub fn geo_leaves(g: &Geo) -> Vec<f64> {
    let flat = |ps: &[Point]| ps.iter().flat_map(|p| [p.x, p.y]).collect();
    match g {
        Geo::Point(p) => vec![p.x, p.y],
        Geo::Line(l) => vec![l.a, l.b, l.c],
        Geo::Lseg(l) => flat(&l.p),
        Geo::Box(b) => flat(&[b.high, b.low]),
        Geo::Path(p) => flat(&p.points),
        Geo::Polygon(p) => flat(&p.points),
        Geo::Circle(c) => vec![c.center.x, c.center.y, c.radius],
    }
}

/// A path's closed flag; none for any other type.
pub fn geo_closed(g: &Geo) -> Option<bool> {
    match g {
        Geo::Path(p) => Some(p.closed),
        _ => None,
    }
}

/// The value of type `kind` made back from its leaves as `geo_leaves` lays them out, and for a
/// path its closed flag: a polygon's bounding box made as poly_in makes it. Refused where the
/// leaves are not as many as the type holds, or a path or a polygon has no point.
pub fn geo_from_leaves(kind: GeoKind, leaves: &[f64], closed: bool) -> Result<Geo> {
    let wrong = || {
        Error::new(format!(
            "{} leaves cannot make a value of type {}",
            leaves.len(),
            kind.name()
        ))
    };
    if let Some(n) = kind.fixed_leaves() {
        if leaves.len() != n {
            return Err(wrong());
        }
    } else if leaves.is_empty() || !leaves.len().is_multiple_of(2) {
        return Err(wrong());
    }
    let pt = |i: usize| Point {
        x: leaves[i],
        y: leaves[i + 1],
    };
    let points = || (0..leaves.len()).step_by(2).map(pt).collect::<Vec<_>>();
    Ok(match kind {
        GeoKind::Point => Geo::Point(pt(0)),
        GeoKind::Line => Geo::Line(Line {
            a: leaves[0],
            b: leaves[1],
            c: leaves[2],
        }),
        GeoKind::Lseg => Geo::Lseg(Lseg { p: [pt(0), pt(2)] }),
        GeoKind::Box => Geo::Box(GeoBox {
            high: pt(0),
            low: pt(2),
        }),
        GeoKind::Path => Geo::Path(Path {
            closed,
            points: points(),
        }),
        GeoKind::Polygon => Geo::Polygon(Polygon::new(points())),
        GeoKind::Circle => Geo::Circle(Circle {
            center: pt(0),
            radius: leaves[2],
        }),
    })
}

/// `point_eq_point`: within EPSILON on each axis, or with a NaN, equal exactly.
fn point_eq_point(p1: &Point, p2: &Point) -> bool {
    if p1.x.is_nan() || p1.y.is_nan() || p2.x.is_nan() || p2.y.is_nan() {
        return f8_eq(p1.x, p2.x) && f8_eq(p1.y, p2.y);
    }
    fp_eq(p1.x, p2.x) && fp_eq(p1.y, p2.y)
}

/// `point_dt`.
fn point_dt(p1: &Point, p2: &Point) -> Result<f64> {
    let x = f8_mi(p1.x, p2.x)?;
    let y = f8_mi(p1.y, p2.y)?;
    hypot(x, y)
}

/// `point_sl`: Infinity where the x are equal, 0 where the y are.
fn point_sl(p1: &Point, p2: &Point) -> Result<f64> {
    if fp_eq(p1.x, p2.x) {
        return Ok(f64::INFINITY);
    }
    if fp_eq(p1.y, p2.y) {
        return Ok(0.0);
    }
    f8_div(f8_mi(p1.y, p2.y)?, f8_mi(p1.x, p2.x)?)
}

/// `point_invsl`: 0 where the x are equal, Infinity where the y are.
fn point_invsl(p1: &Point, p2: &Point) -> Result<f64> {
    if fp_eq(p1.x, p2.x) {
        return Ok(0.0);
    }
    if fp_eq(p1.y, p2.y) {
        return Ok(f64::INFINITY);
    }
    f8_div(f8_mi(p1.x, p2.x)?, f8_mi(p2.y, p1.y)?)
}

/// `point_add_point`.
fn point_add(p1: &Point, p2: &Point) -> Result<Point> {
    let x = f8_pl(p1.x, p2.x)?;
    let y = f8_pl(p1.y, p2.y)?;
    Ok(Point { x, y })
}

/// `point_sub_point`.
fn point_sub(p1: &Point, p2: &Point) -> Result<Point> {
    let x = f8_mi(p1.x, p2.x)?;
    let y = f8_mi(p1.y, p2.y)?;
    Ok(Point { x, y })
}

/// `point_mul_point`.
fn point_mul(p1: &Point, p2: &Point) -> Result<Point> {
    let yx = f8_mul(p1.y, p2.x)?;
    let y = f8_pl(f8_mul(p1.x, p2.y)?, yx)?;
    let yy = f8_mul(p1.y, p2.y)?;
    let x = f8_mi(f8_mul(p1.x, p2.x)?, yy)?;
    Ok(Point { x, y })
}

/// `point_div_point`.
fn point_div(p1: &Point, p2: &Point) -> Result<Point> {
    let yy = f8_mul(p2.y, p2.y)?;
    let div = f8_pl(f8_mul(p2.x, p2.x)?, yy)?;
    let xy = f8_mul(p1.x, p2.y)?;
    let y = f8_div(f8_mi(f8_mul(p1.y, p2.x)?, xy)?, div)?;
    let yy = f8_mul(p1.y, p2.y)?;
    let x = f8_div(f8_pl(f8_mul(p1.x, p2.x)?, yy)?, div)?;
    Ok(Point { x, y })
}

/// `line_construct`: the line through `pt` of slope `m`, `x = C` where `m` is infinite, `y = C`
/// where it is zero.
fn line_construct(pt: &Point, m: f64) -> Result<Line> {
    if m.is_infinite() {
        return Ok(Line {
            a: -1.0,
            b: 0.0,
            c: pt.x,
        });
    }
    if m == 0.0 {
        return Ok(Line {
            a: 0.0,
            b: -1.0,
            c: pt.y,
        });
    }
    let mut c = f8_mi(pt.y, f8_mul(m, pt.x)?)?;
    if c == 0.0 {
        c = 0.0;
    }
    Ok(Line { a: m, b: -1.0, c })
}

/// `line_sl`.
fn line_sl(l: &Line) -> Result<f64> {
    if fp_zero(l.a) {
        return Ok(0.0);
    }
    if fp_zero(l.b) {
        return Ok(f64::INFINITY);
    }
    f8_div(l.a, -l.b)
}

/// `line_invsl`.
fn line_invsl(l: &Line) -> Result<f64> {
    if fp_zero(l.a) {
        return Ok(f64::INFINITY);
    }
    if fp_zero(l.b) {
        return Ok(0.0);
    }
    f8_div(l.b, l.a)
}

/// `line_interpt_line`: where two lines meet; none where they are parallel or the same.
fn line_interpt_line(l1: &Line, l2: &Line) -> Result<Option<Point>> {
    let (mut x, mut y);
    if !fp_zero(l1.b) {
        if fp_eq(l2.a, f8_mul(l1.a, f8_div(l2.b, l1.b)?)?) {
            return Ok(None);
        }
        let ab = f8_mul(l2.a, l1.b)?;
        let below = f8_mi(f8_mul(l1.a, l2.b)?, ab)?;
        let bc = f8_mul(l2.b, l1.c)?;
        x = f8_div(f8_mi(f8_mul(l1.b, l2.c)?, bc)?, below)?;
        y = f8_div(-f8_pl(f8_mul(l1.a, x)?, l1.c)?, l1.b)?;
    } else if !fp_zero(l2.b) {
        if fp_eq(l1.a, f8_mul(l2.a, f8_div(l1.b, l2.b)?)?) {
            return Ok(None);
        }
        let ab = f8_mul(l1.a, l2.b)?;
        let below = f8_mi(f8_mul(l2.a, l1.b)?, ab)?;
        let bc = f8_mul(l1.b, l2.c)?;
        x = f8_div(f8_mi(f8_mul(l2.b, l1.c)?, bc)?, below)?;
        y = f8_div(-f8_pl(f8_mul(l2.a, x)?, l2.c)?, l2.b)?;
    } else {
        return Ok(None);
    }
    if x == 0.0 {
        x = 0.0;
    }
    if y == 0.0 {
        y = 0.0;
    }
    Ok(Some(Point { x, y }))
}

/// `line_contain_point`.
fn line_contain_point(l: &Line, p: &Point) -> Result<bool> {
    let by = f8_mul(l.b, p.y)?;
    Ok(fp_zero(f8_pl(f8_pl(f8_mul(l.a, p.x)?, by)?, l.c)?))
}

/// `line_closept_point`: the distance from `p` to the foot of its perpendicular on `l`, and the
/// foot; NaN and `p` where the perpendicular does not meet the line.
fn line_closept_point(l: &Line, p: &Point) -> Result<(f64, Point)> {
    let tmp = line_construct(p, line_invsl(l)?)?;
    match line_interpt_line(&tmp, l)? {
        None => Ok((f64::NAN, *p)),
        Some(c) => Ok((point_dt(&c, p)?, c)),
    }
}

/// `line_perp`.
fn line_perp(l1: &Line, l2: &Line) -> Result<bool> {
    if fp_zero(l1.a) {
        return Ok(fp_zero(l2.b));
    }
    if fp_zero(l2.a) {
        return Ok(fp_zero(l1.b));
    }
    if fp_zero(l1.b) {
        return Ok(fp_zero(l2.a));
    }
    if fp_zero(l2.b) {
        return Ok(fp_zero(l1.a));
    }
    let bb = f8_mul(l1.b, l2.b)?;
    Ok(fp_eq(f8_div(f8_mul(l1.a, l2.a)?, bb)?, -1.0))
}

/// `line_eq`: exactly equal where any is NaN, else proportional.
fn line_eq(l1: &Line, l2: &Line) -> Result<bool> {
    if [l1.a, l1.b, l1.c, l2.a, l2.b, l2.c]
        .iter()
        .any(|v| v.is_nan())
    {
        return Ok(f8_eq(l1.a, l2.a) && f8_eq(l1.b, l2.b) && f8_eq(l1.c, l2.c));
    }
    let ratio = if !fp_zero(l2.a) {
        f8_div(l1.a, l2.a)?
    } else if !fp_zero(l2.b) {
        f8_div(l1.b, l2.b)?
    } else if !fp_zero(l2.c) {
        f8_div(l1.c, l2.c)?
    } else {
        1.0
    };
    Ok(fp_eq(l1.a, f8_mul(ratio, l2.a)?)
        && fp_eq(l1.b, f8_mul(ratio, l2.b)?)
        && fp_eq(l1.c, f8_mul(ratio, l2.c)?))
}

/// `line_distance`: 0 where the lines meet.
fn line_distance(l1: &Line, l2: &Line) -> Result<f64> {
    if line_interpt_line(l1, l2)?.is_some() {
        return Ok(0.0);
    }
    let usable = |v: f64| !fp_zero(v) && !v.is_nan();
    let ratio = if usable(l1.a) && usable(l2.a) {
        f8_div(l1.a, l2.a)?
    } else if usable(l1.b) && usable(l2.b) {
        f8_div(l1.b, l2.b)?
    } else {
        1.0
    };
    f8_div(f8_mi(l1.c, f8_mul(ratio, l2.c)?)?.abs(), hypot(l1.a, l1.b)?)
}

fn lseg(p0: Point, p1: Point) -> Lseg {
    Lseg { p: [p0, p1] }
}

/// `lseg_sl`.
fn lseg_sl(l: &Lseg) -> Result<f64> {
    point_sl(&l.p[0], &l.p[1])
}

/// `lseg_invsl`.
fn lseg_invsl(l: &Lseg) -> Result<f64> {
    point_invsl(&l.p[0], &l.p[1])
}

/// `lseg_length`.
fn lseg_length(l: &Lseg) -> Result<f64> {
    point_dt(&l.p[0], &l.p[1])
}

/// `lseg_center`.
fn lseg_center(l: &Lseg) -> Result<Point> {
    let x = f8_div(f8_pl(l.p[0].x, l.p[1].x)?, 2.0)?;
    let y = f8_div(f8_pl(l.p[0].y, l.p[1].y)?, 2.0)?;
    Ok(Point { x, y })
}

/// `lseg_contain_point`: the distances to the two ends add up to the length, within EPSILON.
fn lseg_contain_point(l: &Lseg, p: &Point) -> Result<bool> {
    let a = point_dt(p, &l.p[0])?;
    let b = point_dt(p, &l.p[1])?;
    Ok(fp_eq(a + b, point_dt(&l.p[0], &l.p[1])?))
}

/// `lseg_interpt_line`: where the segment meets the line, an end point where it meets there.
fn lseg_interpt_line(l: &Lseg, line: &Line) -> Result<Option<Point>> {
    let tmp = line_construct(&l.p[0], lseg_sl(l)?)?;
    let Some(interpt) = line_interpt_line(&tmp, line)? else {
        return Ok(None);
    };
    if !lseg_contain_point(l, &interpt)? {
        return Ok(None);
    }
    Ok(Some(if point_eq_point(&l.p[0], &interpt) {
        l.p[0]
    } else if point_eq_point(&l.p[1], &interpt) {
        l.p[1]
    } else {
        interpt
    }))
}

/// `lseg_interpt_lseg`: where two segments meet.
fn lseg_interpt_lseg(l1: &Lseg, l2: &Lseg) -> Result<Option<Point>> {
    let tmp = line_construct(&l2.p[0], lseg_sl(l2)?)?;
    let Some(interpt) = lseg_interpt_line(l1, &tmp)? else {
        return Ok(None);
    };
    if !lseg_contain_point(l2, &interpt)? {
        return Ok(None);
    }
    Ok(Some(interpt))
}

/// `lseg_closept_line`: 0 and the meeting point where they meet, else the nearer end's distance
/// to the line and that end, the second where neither is nearer.
fn lseg_closept_line(l: &Lseg, line: &Line) -> Result<(f64, Point)> {
    if let Some(p) = lseg_interpt_line(l, line)? {
        return Ok((0.0, p));
    }
    let d1 = line_closept_point(line, &l.p[0])?.0;
    let d2 = line_closept_point(line, &l.p[1])?.0;
    if d1 < d2 {
        Ok((d1, l.p[0]))
    } else {
        Ok((d2, l.p[1]))
    }
}

/// `lseg_closept_point`: the distance from `p` to the segment's point nearest it, and that point.
fn lseg_closept_point(l: &Lseg, p: &Point) -> Result<(f64, Point)> {
    let tmp = line_construct(p, point_invsl(&l.p[0], &l.p[1])?)?;
    let (_, closept) = lseg_closept_line(l, &tmp)?;
    Ok((point_dt(&closept, p)?, closept))
}

/// `lseg_closept_lseg`: the distance between two segments, and the point on `on` nearest `to`.
fn lseg_closept_lseg(on: &Lseg, to: &Lseg) -> Result<(f64, Point)> {
    if let Some(p) = lseg_interpt_lseg(on, to)? {
        return Ok((0.0, p));
    }
    let (mut dist, mut result) = lseg_closept_point(on, &to.p[0])?;
    let (d, point) = lseg_closept_point(on, &to.p[1])?;
    if f8_lt(d, dist) {
        dist = d;
        result = point;
    }
    for end in on.p {
        let d = lseg_closept_point(to, &end)?.0;
        if f8_lt(d, dist) {
            dist = d;
            result = end;
        }
    }
    Ok((dist, result))
}

/// `box_construct`: the box of two corners, the larger of each axis high.
fn box_construct(p1: &Point, p2: &Point) -> GeoBox {
    let (hx, lx) = if f8_gt(p1.x, p2.x) {
        (p1.x, p2.x)
    } else {
        (p2.x, p1.x)
    };
    let (hy, ly) = if f8_gt(p1.y, p2.y) {
        (p1.y, p2.y)
    } else {
        (p2.y, p1.y)
    };
    GeoBox {
        high: Point { x: hx, y: hy },
        low: Point { x: lx, y: ly },
    }
}

/// `box_ov`.
pub(crate) fn box_ov(b1: &GeoBox, b2: &GeoBox) -> bool {
    fp_le(b1.low.x, b2.high.x)
        && fp_le(b2.low.x, b1.high.x)
        && fp_le(b1.low.y, b2.high.y)
        && fp_le(b2.low.y, b1.high.y)
}

/// `box_contain_box`: `inner` in `outer` or on its border.
pub(crate) fn box_contain_box(outer: &GeoBox, inner: &GeoBox) -> bool {
    fp_ge(outer.high.x, inner.high.x)
        && fp_le(outer.low.x, inner.low.x)
        && fp_ge(outer.high.y, inner.high.y)
        && fp_le(outer.low.y, inner.low.y)
}

/// The greatest of a leaf that may hold a NaN, which sorts last: it stands for every value.
fn past_nan(greatest: f64) -> f64 {
    if greatest.is_nan() {
        f64::INFINITY
    } else {
        greatest
    }
}

/// Whether a box among those whose high x, high y, low x and low y lie within the least and greatest
/// given may pass `box op q`, by the comparisons PostgreSQL's operator makes.
pub(crate) fn box_may_hold(op: &str, leaves: [(f64, f64); 4], q: &GeoBox) -> bool {
    let [hx, hy, lx, ly] = leaves;
    let (hx_top, hy_top, lx_top, ly_top) = (
        past_nan(hx.1),
        past_nan(hy.1),
        past_nan(lx.1),
        past_nan(ly.1),
    );
    match op {
        "&&" => {
            fp_le(lx.0, q.high.x)
                && fp_le(q.low.x, hx_top)
                && fp_le(ly.0, q.high.y)
                && fp_le(q.low.y, hy_top)
        }
        "<<" => fp_lt(hx.0, q.low.x),
        "&<" => fp_le(hx.0, q.high.x),
        ">>" => fp_gt(lx_top, q.high.x),
        "&>" => fp_ge(lx_top, q.low.x),
        "<<|" => fp_lt(hy.0, q.low.y),
        "&<|" => fp_le(hy.0, q.high.y),
        "|>>" => fp_gt(ly_top, q.high.y),
        "|&>" => fp_ge(ly_top, q.low.y),
        "@>" => {
            fp_ge(hx_top, q.high.x)
                && fp_le(lx.0, q.low.x)
                && fp_ge(hy_top, q.high.y)
                && fp_le(ly.0, q.low.y)
        }
        "<@" => {
            fp_ge(q.high.x, hx.0)
                && fp_le(q.low.x, lx_top)
                && fp_ge(q.high.y, hy.0)
                && fp_le(q.low.y, ly_top)
        }
        "~=" => {
            let near = |(least, greatest): (f64, f64), v: f64| {
                v.is_nan()
                    || greatest.is_nan()
                    || (least - 2.0 * EPSILON <= v && v <= greatest + 2.0 * EPSILON)
            };
            near(hx, q.high.x) && near(hy, q.high.y) && near(lx, q.low.x) && near(ly, q.low.y)
        }
        _ => true,
    }
}

/// Whether a box among those whose leaves lie so may hold the point, as `box_may_hold` asks.
pub(crate) fn box_may_hold_point(leaves: [(f64, f64); 4], p: &Point) -> bool {
    let [hx, hy, lx, ly] = leaves;
    past_nan(hx.1) >= p.x && lx.0 <= p.x && past_nan(hy.1) >= p.y && ly.0 <= p.y
}

/// `box_ar`.
fn box_ar(b: &GeoBox) -> Result<f64> {
    f8_mul(f8_mi(b.high.x, b.low.x)?, f8_mi(b.high.y, b.low.y)?)
}

/// `box_cn`.
fn box_cn(b: &GeoBox) -> Result<Point> {
    let x = f8_div(f8_pl(b.high.x, b.low.x)?, 2.0)?;
    let y = f8_div(f8_pl(b.high.y, b.low.y)?, 2.0)?;
    Ok(Point { x, y })
}

/// `box_contain_point`: in the box or on its border, compared exactly.
fn box_contain_point(b: &GeoBox, p: &Point) -> bool {
    b.high.x >= p.x && b.low.x <= p.x && b.high.y >= p.y && b.low.y <= p.y
}

/// `box_intersect`: none where the boxes do not overlap.
fn box_intersect(b1: &GeoBox, b2: &GeoBox) -> Option<GeoBox> {
    if !box_ov(b1, b2) {
        return None;
    }
    Some(GeoBox {
        high: Point {
            x: f8_min(b1.high.x, b2.high.x),
            y: f8_min(b1.high.y, b2.high.y),
        },
        low: Point {
            x: f8_max(b1.low.x, b2.low.x),
            y: f8_max(b1.low.y, b2.low.y),
        },
    })
}

/// The box's four sides as box_closept_point and box_interpt_lseg walk them: from the low corner
/// and from the high to (low x, high y), then to (high x, low y).
fn box_sides(b: &GeoBox) -> [Lseg; 4] {
    let p = Point {
        x: b.low.x,
        y: b.high.y,
    };
    let q = Point {
        x: b.high.x,
        y: b.low.y,
    };
    [
        lseg(b.low, p),
        lseg(b.high, p),
        lseg(b.low, q),
        lseg(b.high, q),
    ]
}

/// `box_closept_point`: 0 and `p` where the box holds it, else the distance to the nearest side
/// and the point on it.
fn box_closept_point(b: &GeoBox, p: &Point) -> Result<(f64, Point)> {
    if box_contain_point(b, p) {
        return Ok((0.0, *p));
    }
    let sides = box_sides(b);
    let (mut dist, mut result) = lseg_closept_point(&sides[0], p)?;
    for side in &sides[1..] {
        let (d, closept) = lseg_closept_point(side, p)?;
        if f8_lt(d, dist) {
            dist = d;
            result = closept;
        }
    }
    Ok((dist, result))
}

/// `box_interpt_lseg`: whether the segment meets the box, inside it counting; where `want`, also
/// the segment's point nearest the box's center, found once their bounding boxes overlap.
fn box_interpt_lseg(b: &GeoBox, l: &Lseg, want: bool) -> Result<(bool, Option<Point>)> {
    let lbox = GeoBox {
        low: Point {
            x: f8_min(l.p[0].x, l.p[1].x),
            y: f8_min(l.p[0].y, l.p[1].y),
        },
        high: Point {
            x: f8_max(l.p[0].x, l.p[1].x),
            y: f8_max(l.p[0].y, l.p[1].y),
        },
    };
    if !box_ov(&lbox, b) {
        return Ok((false, None));
    }
    let result = if want {
        Some(lseg_closept_point(l, &box_cn(b)?)?.1)
    } else {
        None
    };
    if box_contain_point(b, &l.p[0]) || box_contain_point(b, &l.p[1]) {
        return Ok((true, result));
    }
    for side in box_sides(b) {
        if lseg_interpt_lseg(&side, l)?.is_some() {
            return Ok((true, result));
        }
    }
    Ok((false, result))
}

/// `box_closept_lseg`: 0 where the segment meets the box, else the distance to the nearest side;
/// where `want`, the point on or in the box nearest the segment.
fn box_closept_lseg(b: &GeoBox, l: &Lseg, want: bool) -> Result<(f64, Option<Point>)> {
    let (hit, result) = box_interpt_lseg(b, l, want)?;
    if hit {
        return Ok((0.0, result));
    }
    let sides = box_sides(b);
    let (mut dist, mut result) = lseg_closept_lseg(&sides[0], l)?;
    for side in &sides[1..] {
        let (d, closept) = lseg_closept_lseg(side, l)?;
        if f8_lt(d, dist) {
            dist = d;
            result = closept;
        }
    }
    Ok((dist, Some(result)))
}

/// `box_mul`: each corner multiplied, the box made again from them.
fn box_mul(b: &GeoBox, p: &Point) -> Result<GeoBox> {
    let high = point_mul(&b.high, p)?;
    let low = point_mul(&b.low, p)?;
    Ok(box_construct(&high, &low))
}

/// `box_div`: each corner divided, the box made again from them.
fn box_div(b: &GeoBox, p: &Point) -> Result<GeoBox> {
    let high = point_div(&b.high, p)?;
    let low = point_div(&b.low, p)?;
    Ok(box_construct(&high, &low))
}

/// `box_circle`.
fn box_circle(b: &GeoBox) -> Result<Circle> {
    let center = box_cn(b)?;
    let radius = point_dt(&center, &b.high)?;
    Ok(Circle { center, radius })
}

/// `box_poly`: the four corners from the low, the bounding box by `box_construct`.
fn box_poly(b: &GeoBox) -> Polygon {
    Polygon {
        points: vec![
            b.low,
            Point {
                x: b.low.x,
                y: b.high.y,
            },
            b.high,
            Point {
                x: b.high.x,
                y: b.low.y,
            },
        ],
        bound: box_construct(&b.high, &b.low),
    }
}

/// `inter_lb`: whether the line meets any side of the box.
fn inter_lb(line: &Line, b: &GeoBox) -> Result<bool> {
    let ll = b.low;
    let lh = Point {
        x: b.low.x,
        y: b.high.y,
    };
    let hl = Point {
        x: b.high.x,
        y: b.low.y,
    };
    for side in [
        lseg(ll, lh),
        lseg(b.high, lh),
        lseg(b.high, hl),
        lseg(ll, hl),
    ] {
        if lseg_interpt_line(&side, line)?.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The segments `(prev, i)` of a path in the order its loops walk them: from the second point on,
/// a closed path's closing segment first.
fn segments(points: &[Point], closed: bool) -> impl Iterator<Item = Lseg> + '_ {
    (0..points.len()).filter_map(move |i| {
        let prev = if i > 0 {
            i - 1
        } else if closed {
            points.len() - 1
        } else {
            return None;
        };
        Some(lseg(points[prev], points[i]))
    })
}

/// `make_bound_box`.
fn make_bound_box(points: &[Point]) -> GeoBox {
    let (mut x1, mut y1) = (points[0].x, points[0].y);
    let (mut x2, mut y2) = (x1, y1);
    for p in &points[1..] {
        if f8_lt(p.x, x1) {
            x1 = p.x;
        }
        if f8_gt(p.x, x2) {
            x2 = p.x;
        }
        if f8_lt(p.y, y1) {
            y1 = p.y;
        }
        if f8_gt(p.y, y2) {
            y2 = p.y;
        }
    }
    GeoBox {
        high: Point { x: x2, y: y2 },
        low: Point { x: x1, y: y1 },
    }
}

/// `lseg_crossing`: how the segment from (prev_x, prev_y) to (x, y) crosses the positive x axis,
/// or POINT_ON_POLYGON where it holds the origin.
fn lseg_crossing(x: f64, y: f64, prev_x: f64, prev_y: f64) -> Result<i32> {
    if fp_zero(y) {
        if fp_zero(x) {
            return Ok(POINT_ON_POLYGON);
        }
        if fp_gt(x, 0.0) {
            if fp_zero(prev_y) {
                return Ok(if fp_gt(prev_x, 0.0) {
                    0
                } else {
                    POINT_ON_POLYGON
                });
            }
            return Ok(if fp_lt(prev_y, 0.0) { 1 } else { -1 });
        }
        if fp_zero(prev_y) {
            return Ok(if fp_lt(prev_x, 0.0) {
                0
            } else {
                POINT_ON_POLYGON
            });
        }
        return Ok(0);
    }
    let y_sign = if fp_gt(y, 0.0) { 1 } else { -1 };
    if fp_zero(prev_y) {
        return Ok(if fp_lt(prev_x, 0.0) { 0 } else { y_sign });
    }
    if (y_sign < 0 && fp_lt(prev_y, 0.0)) || (y_sign > 0 && fp_gt(prev_y, 0.0)) {
        return Ok(0);
    }
    if fp_ge(x, 0.0) && fp_gt(prev_x, 0.0) {
        return Ok(2 * y_sign);
    }
    if fp_lt(x, 0.0) && fp_le(prev_x, 0.0) {
        return Ok(0);
    }
    let yx = f8_mul(f8_mi(y, prev_y)?, x)?;
    let z = f8_mi(f8_mul(f8_mi(x, prev_x)?, y)?, yx)?;
    if fp_zero(z) {
        return Ok(POINT_ON_POLYGON);
    }
    if (y_sign < 0 && fp_lt(z, 0.0)) || (y_sign > 0 && fp_gt(z, 0.0)) {
        return Ok(0);
    }
    Ok(2 * y_sign)
}

/// `point_inside`: 0 outside the ring of `points`, 1 inside, 2 on it.
fn point_inside(p: &Point, points: &[Point]) -> Result<i32> {
    let x0 = f8_mi(points[0].x, p.x)?;
    let y0 = f8_mi(points[0].y, p.y)?;
    let (mut prev_x, mut prev_y) = (x0, y0);
    let mut total = 0i32;
    for q in &points[1..] {
        let x = f8_mi(q.x, p.x)?;
        let y = f8_mi(q.y, p.y)?;
        let cross = lseg_crossing(x, y, prev_x, prev_y)?;
        if cross == POINT_ON_POLYGON {
            return Ok(2);
        }
        total += cross;
        (prev_x, prev_y) = (x, y);
    }
    let cross = lseg_crossing(x0, y0, prev_x, prev_y)?;
    if cross == POINT_ON_POLYGON {
        return Ok(2);
    }
    total += cross;
    Ok(if total != 0 { 1 } else { 0 })
}

/// `plist_same`: the same points from some starting point, forward or backward.
fn plist_same(p1: &[Point], p2: &[Point]) -> bool {
    let n = p1.len();
    for i in 0..n {
        if !point_eq_point(&p2[i], &p1[0]) {
            continue;
        }
        if (1..n).all(|ii| point_eq_point(&p2[(i + ii) % n], &p1[ii])) {
            return true;
        }
        if (1..n).all(|ii| point_eq_point(&p2[(i + n - ii) % n], &p1[ii])) {
            return true;
        }
    }
    false
}

/// `on_ppath`: on an open path's segments, within EPSILON; in or on a closed path's ring.
fn on_ppath(p: &Point, path: &Path) -> Result<bool> {
    if path.closed {
        return Ok(point_inside(p, &path.points)? != 0);
    }
    let pts = &path.points;
    let mut a = point_dt(p, &pts[0])?;
    for w in pts.windows(2) {
        let b = point_dt(p, &w[1])?;
        if fp_eq(f8_pl(a, b)?, point_dt(&w[0], &w[1])?) {
            return Ok(true);
        }
        a = b;
    }
    Ok(false)
}

/// `path_inter`: whether any segment of one meets any of the other, once their bounding boxes
/// overlap.
fn path_inter(p1: &Path, p2: &Path) -> Result<bool> {
    let bound = |ps: &[Point]| {
        let mut b = GeoBox {
            high: ps[0],
            low: ps[0],
        };
        for p in &ps[1..] {
            b.high.x = f8_max(p.x, b.high.x);
            b.high.y = f8_max(p.y, b.high.y);
            b.low.x = f8_min(p.x, b.low.x);
            b.low.y = f8_min(p.y, b.low.y);
        }
        b
    };
    if !box_ov(&bound(&p1.points), &bound(&p2.points)) {
        return Ok(false);
    }
    for s1 in segments(&p1.points, p1.closed) {
        for s2 in segments(&p2.points, p2.closed) {
            if lseg_interpt_lseg(&s1, &s2)?.is_some() {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// The least distance between the segments of two rings or paths, as path_distance and
/// poly_distance take it; none where either has no segment.
fn segments_distance(
    p1: &[Point],
    closed1: bool,
    p2: &[Point],
    closed2: bool,
) -> Result<Option<f64>> {
    let mut min: Option<f64> = None;
    for s1 in segments(p1, closed1) {
        for s2 in segments(p2, closed2) {
            let d = lseg_closept_lseg(&s1, &s2)?.0;
            if min.is_none_or(|m| f8_lt(d, m)) {
                min = Some(d);
            }
        }
    }
    Ok(min)
}

/// `path_length`.
fn path_length(path: &Path) -> Result<f64> {
    let mut result = 0.0;
    for s in segments(&path.points, path.closed) {
        result = f8_pl(result, point_dt(&s.p[0], &s.p[1])?)?;
    }
    Ok(result)
}

/// `dist_ppath_internal`: the least distance to a segment; 0 where the path has none.
fn dist_ppath(p: &Point, path: &Path) -> Result<f64> {
    let mut result: Option<f64> = None;
    for s in segments(&path.points, path.closed) {
        let d = lseg_closept_point(&s, p)?.0;
        if result.is_none_or(|r| f8_lt(d, r)) {
            result = Some(d);
        }
    }
    Ok(result.unwrap_or(0.0))
}

/// `path_add`: two open paths joined; NULL where either is closed.
fn path_add(p1: &Path, p2: &Path) -> Result<Option<Path>> {
    if p1.closed || p2.closed {
        return Ok(None);
    }
    if too_many((p1.points.len() + p2.points.len()) as u64, PATH_HEADER) {
        return Err(too_many_error());
    }
    let mut points = p1.points.clone();
    points.extend_from_slice(&p2.points);
    Ok(Some(Path {
        closed: p1.closed,
        points,
    }))
}

/// A path with `f` applied to each of its points.
fn path_map(path: &Path, f: impl Fn(&Point) -> Result<Point>) -> Result<Path> {
    Ok(Path {
        closed: path.closed,
        points: path.points.iter().map(f).collect::<Result<_>>()?,
    })
}

/// `path_poly`: refused for an open path.
fn path_poly(path: &Path) -> Result<Polygon> {
    if !path.closed {
        return Err(Error::new("open path cannot be converted to polygon"));
    }
    Ok(Polygon::new(path.points.clone()))
}

/// `dist_ppoly_internal`: 0 in or on the polygon, else the least distance to a side, the closing
/// side first.
fn dist_ppoly(p: &Point, poly: &Polygon) -> Result<f64> {
    let pts = &poly.points;
    if point_inside(p, pts)? != 0 {
        return Ok(0.0);
    }
    let mut result = lseg_closept_point(&lseg(pts[0], pts[pts.len() - 1]), p)?.0;
    for w in pts.windows(2) {
        let d = lseg_closept_point(&lseg(w[0], w[1]), p)?.0;
        if f8_lt(d, result) {
            result = d;
        }
    }
    Ok(result)
}

/// `dist_cpoly_internal`: the center's distance less the radius, 0 below 0.
fn dist_cpoly(c: &Circle, poly: &Polygon) -> Result<f64> {
    Ok(not_below_zero(f8_mi(
        dist_ppoly(&c.center, poly)?,
        c.radius,
    )?))
}

/// `poly_overlap_internal`: the bounding boxes overlap, and a side of each meets, or one's first
/// point is in the other.
fn poly_overlap(a: &Polygon, b: &Polygon) -> Result<bool> {
    if !box_ov(&a.bound, &b.bound) {
        return Ok(false);
    }
    for sa in segments(&a.points, true) {
        for sb in segments(&b.points, true) {
            if lseg_interpt_lseg(&sa, &sb)?.is_some() {
                return Ok(true);
            }
        }
    }
    Ok(point_inside(&a.points[0], &b.points)? != 0 || point_inside(&b.points[0], &a.points)? != 0)
}

/// `touched_lseg_inside_poly`: for `a` on side `s` and `b` not, whether the segment is in the
/// polygon past `s`.
fn touched_lseg_inside_poly(
    a: &Point,
    b: &Point,
    s: &Lseg,
    poly: &Polygon,
    start: usize,
) -> Result<bool> {
    let t = lseg(*a, *b);
    if point_eq_point(a, &s.p[0]) {
        if lseg_contain_point(&t, &s.p[1])? {
            return lseg_inside_poly(b, &s.p[1], poly, start);
        }
    } else if point_eq_point(a, &s.p[1]) {
        if lseg_contain_point(&t, &s.p[0])? {
            return lseg_inside_poly(b, &s.p[0], poly, start);
        }
    } else if lseg_contain_point(&t, &s.p[0])? {
        return lseg_inside_poly(b, &s.p[0], poly, start);
    } else if lseg_contain_point(&t, &s.p[1])? {
        return lseg_inside_poly(b, &s.p[1], poly, start);
    }
    Ok(true)
}

/// `lseg_inside_poly`: whether the segment from `a` to `b` is in the polygon, its sides checked
/// from `start`.
fn lseg_inside_poly(a: &Point, b: &Point, poly: &Polygon, start: usize) -> Result<bool> {
    let pts = &poly.points;
    let t = lseg(*a, *b);
    let mut s0 = pts[if start == 0 { pts.len() - 1 } else { start - 1 }];
    let (mut res, mut intersection) = (true, false);
    let mut i = start;
    while i < pts.len() && res {
        let s = lseg(s0, pts[i]);
        if lseg_contain_point(&s, &t.p[0])? {
            if lseg_contain_point(&s, &t.p[1])? {
                return Ok(true);
            }
            res = touched_lseg_inside_poly(&t.p[0], &t.p[1], &s, poly, i + 1)?;
        } else if lseg_contain_point(&s, &t.p[1])? {
            res = touched_lseg_inside_poly(&t.p[1], &t.p[0], &s, poly, i + 1)?;
        } else if let Some(interpt) = lseg_interpt_lseg(&t, &s)? {
            intersection = true;
            res = lseg_inside_poly(&t.p[0], &interpt, poly, i + 1)?;
            if res {
                res = lseg_inside_poly(&t.p[1], &interpt, poly, i + 1)?;
            }
        }
        s0 = pts[i];
        i += 1;
    }
    if res && !intersection {
        let p = Point {
            x: f8_div(f8_pl(t.p[0].x, t.p[1].x)?, 2.0)?,
            y: f8_div(f8_pl(t.p[0].y, t.p[1].y)?, 2.0)?,
        };
        res = point_inside(&p, pts)? != 0;
    }
    Ok(res)
}

/// `poly_contain_poly`: the inner bounding box in the outer, and every side of the inner in the
/// outer polygon.
fn poly_contain(outer: &Polygon, inner: &Polygon) -> Result<bool> {
    if !box_contain_box(&outer.bound, &inner.bound) {
        return Ok(false);
    }
    for s in segments(&inner.points, true) {
        if !lseg_inside_poly(&s.p[0], &s.p[1], outer, 0)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// `poly_distance`: 0 where the polygons overlap, else the least distance between sides.
fn poly_distance(a: &Polygon, b: &Polygon) -> Result<Option<f64>> {
    if poly_overlap(a, b)? {
        return Ok(Some(0.0));
    }
    segments_distance(&a.points, true, &b.points, true)
}

/// `poly_to_circle`: the mean of the points, and the mean of their distances to it.
fn poly_to_circle(poly: &Polygon) -> Result<Circle> {
    let n = poly.points.len() as f64;
    let mut center = Point { x: 0.0, y: 0.0 };
    for p in &poly.points {
        center = point_add(&center, p)?;
    }
    center.x = f8_div(center.x, n)?;
    center.y = f8_div(center.y, n)?;
    let mut radius = 0.0;
    for p in &poly.points {
        radius = f8_pl(radius, point_dt(p, &center)?)?;
    }
    radius = f8_div(radius, n)?;
    Ok(Circle { center, radius })
}

/// `circle_ar`.
fn circle_ar(c: &Circle) -> Result<f64> {
    f8_mul(f8_mul(c.radius, c.radius)?, PI)
}

/// `circle_same`: radii within EPSILON or both NaN, and the centers the same.
fn circle_same(c1: &Circle, c2: &Circle) -> bool {
    ((c1.radius.is_nan() && c2.radius.is_nan()) || fp_eq(c1.radius, c2.radius))
        && point_eq_point(&c1.center, &c2.center)
}

/// `circle_box`.
fn circle_box(c: &Circle) -> Result<GeoBox> {
    let delta = f8_div(c.radius, 2f64.sqrt())?;
    let hx = f8_pl(c.center.x, delta)?;
    let lx = f8_mi(c.center.x, delta)?;
    let hy = f8_pl(c.center.y, delta)?;
    let ly = f8_mi(c.center.y, delta)?;
    Ok(GeoBox {
        high: Point { x: hx, y: hy },
        low: Point { x: lx, y: ly },
    })
}

/// `circle_poly_internal`: `npts` points around the circle, from angle 0; refused for a radius of
/// zero.
fn circle_poly(npts: i32, c: &Circle) -> Result<Polygon> {
    if fp_zero(c.radius) {
        return Err(Error::new(
            "cannot convert circle with radius zero to polygon",
        ));
    }
    if npts < 2 {
        return Err(Error::new("must request at least 2 points"));
    }
    let anglestep = f8_div(2.0 * PI, f64::from(npts))?;
    let mut points = Vec::with_capacity(npts as usize);
    for i in 0..npts {
        let angle = f8_mul(anglestep, f64::from(i))?;
        let x = f8_mi(c.center.x, f8_mul(c.radius, angle.cos())?)?;
        let y = f8_pl(c.center.y, f8_mul(c.radius, angle.sin())?)?;
        points.push(Point { x, y });
    }
    Ok(Polygon::new(points))
}

/// A distance as circle_distance, dist_pc, dist_cpoint and dist_cpoly give it: 0 where it falls
/// below 0.
fn not_below_zero(v: f64) -> f64 {
    if v < 0.0 {
        0.0
    } else {
        v
    }
}

fn point_value(p: Point) -> GeoValue {
    GeoValue::Geo(Geo::Point(p))
}

fn point_or_null(p: Option<Point>) -> GeoValue {
    p.map_or(GeoValue::Null, point_value)
}

/// A closest point found with its distance, NULL where the distance is NaN (close_pl, close_ps,
/// close_lseg, close_pb, close_ls, close_sb).
fn closest(found: (f64, Point)) -> GeoValue {
    if found.0.is_nan() {
        GeoValue::Null
    } else {
        point_value(found.1)
    }
}

/// The prefix operator `op` on `a` as PostgreSQL's pg_operator holds it on 19, computed by its
/// function: `#` (path_npoints, poly_npoints), `?-` (lseg_horizontal, line_horizontal), `?|`
/// (lseg_vertical, line_vertical), `@-@` (lseg_length, path_length) and `@@` (lseg_center,
/// box_center, poly_center, circle_center). None where PostgreSQL has no such operator.
pub fn geo_unary(op: &str, a: &Geo) -> Option<Result<GeoValue>> {
    use Geo as G;
    use GeoValue as V;
    let v = match (op, a) {
        ("#", G::Path(p)) => Ok(V::Int(p.points.len() as i32)),
        ("#", G::Polygon(p)) => Ok(V::Int(p.points.len() as i32)),
        ("?-", G::Lseg(l)) => Ok(V::Bool(fp_eq(l.p[0].y, l.p[1].y))),
        ("?-", G::Line(l)) => Ok(V::Bool(fp_zero(l.a))),
        ("?|", G::Lseg(l)) => Ok(V::Bool(fp_eq(l.p[0].x, l.p[1].x))),
        ("?|", G::Line(l)) => Ok(V::Bool(fp_zero(l.b))),
        ("@-@", G::Lseg(l)) => lseg_length(l).map(V::Float),
        ("@-@", G::Path(p)) => path_length(p).map(V::Float),
        ("@@", G::Lseg(l)) => lseg_center(l).map(point_value),
        ("@@", G::Box(b)) => box_cn(b).map(point_value),
        ("@@", G::Polygon(p)) => poly_to_circle(p).map(|c| point_value(c.center)),
        ("@@", G::Circle(c)) => Ok(point_value(c.center)),
        _ => return None,
    };
    Some(v)
}

/// The operator `op` on `a` and `b` as PostgreSQL's pg_operator holds it on 19 for those two
/// types, computed by its function (point_add, box_overlap, dist_ps, close_lseg, poly_contain,
/// circle_same, …): its geometric result, NULL where the function gives NULL (`#` of parallel
/// lines or of boxes that do not overlap, `+` of a closed path), or its refusal. None where
/// PostgreSQL has no operator `op` for these types.
pub fn geo_binary(op: &str, a: &Geo, b: &Geo) -> Option<Result<GeoValue>> {
    use Geo as G;
    use GeoValue as V;
    let boolean = |r: Result<bool>| r.map(V::Bool);
    let float = |r: Result<f64>| r.map(V::Float);
    let v = match (op, a, b) {
        // Points.
        ("+", G::Point(p), G::Point(q)) => point_add(p, q).map(point_value),
        ("-", G::Point(p), G::Point(q)) => point_sub(p, q).map(point_value),
        ("*", G::Point(p), G::Point(q)) => point_mul(p, q).map(point_value),
        ("/", G::Point(p), G::Point(q)) => point_div(p, q).map(point_value),
        ("<<", G::Point(p), G::Point(q)) => Ok(V::Bool(fp_lt(p.x, q.x))),
        (">>", G::Point(p), G::Point(q)) => Ok(V::Bool(fp_gt(p.x, q.x))),
        ("|>>" | ">^", G::Point(p), G::Point(q)) => Ok(V::Bool(fp_gt(p.y, q.y))),
        ("<<|" | "<^", G::Point(p), G::Point(q)) => Ok(V::Bool(fp_lt(p.y, q.y))),
        ("?|", G::Point(p), G::Point(q)) => Ok(V::Bool(fp_eq(p.x, q.x))),
        ("?-", G::Point(p), G::Point(q)) => Ok(V::Bool(fp_eq(p.y, q.y))),
        ("~=", G::Point(p), G::Point(q)) => Ok(V::Bool(point_eq_point(p, q))),
        ("<>", G::Point(p), G::Point(q)) => Ok(V::Bool(!point_eq_point(p, q))),
        ("<->", G::Point(p), G::Point(q)) => float(point_dt(p, q)),

        // Lines.
        ("?#", G::Line(l), G::Line(m)) => boolean(line_interpt_line(l, m).map(|p| p.is_some())),
        ("?||", G::Line(l), G::Line(m)) => boolean(line_interpt_line(l, m).map(|p| p.is_none())),
        ("?-|", G::Line(l), G::Line(m)) => boolean(line_perp(l, m)),
        ("=", G::Line(l), G::Line(m)) => boolean(line_eq(l, m)),
        ("<->", G::Line(l), G::Line(m)) => float(line_distance(l, m)),
        ("#", G::Line(l), G::Line(m)) => line_interpt_line(l, m).map(point_or_null),

        // Line segments.
        ("?#", G::Lseg(l), G::Lseg(m)) => boolean(lseg_interpt_lseg(l, m).map(|p| p.is_some())),
        ("?||", G::Lseg(l), G::Lseg(m)) => run(|| Ok(V::Bool(fp_eq(lseg_sl(l)?, lseg_sl(m)?)))),
        ("?-|", G::Lseg(l), G::Lseg(m)) => run(|| Ok(V::Bool(fp_eq(lseg_sl(l)?, lseg_invsl(m)?)))),
        ("=", G::Lseg(l), G::Lseg(m)) => Ok(V::Bool(
            point_eq_point(&l.p[0], &m.p[0]) && point_eq_point(&l.p[1], &m.p[1]),
        )),
        ("<>", G::Lseg(l), G::Lseg(m)) => Ok(V::Bool(
            !point_eq_point(&l.p[0], &m.p[0]) || !point_eq_point(&l.p[1], &m.p[1]),
        )),
        ("<" | "<=" | ">" | ">=", G::Lseg(l), G::Lseg(m)) => run(|| {
            let (x, y) = (lseg_length(l)?, lseg_length(m)?);
            Ok(V::Bool(fp_order(op, x, y)))
        }),
        ("<->", G::Lseg(l), G::Lseg(m)) => float(lseg_closept_lseg(l, m).map(|r| r.0)),
        ("#", G::Lseg(l), G::Lseg(m)) => lseg_interpt_lseg(l, m).map(point_or_null),

        // Boxes.
        ("~=", G::Box(x), G::Box(y)) => Ok(V::Bool(
            point_eq_point(&x.high, &y.high) && point_eq_point(&x.low, &y.low),
        )),
        ("&&" | "?#", G::Box(x), G::Box(y)) => Ok(V::Bool(box_ov(x, y))),
        ("<<", G::Box(x), G::Box(y)) => Ok(V::Bool(fp_lt(x.high.x, y.low.x))),
        ("&<", G::Box(x), G::Box(y)) => Ok(V::Bool(fp_le(x.high.x, y.high.x))),
        (">>", G::Box(x), G::Box(y)) => Ok(V::Bool(fp_gt(x.low.x, y.high.x))),
        ("&>", G::Box(x), G::Box(y)) => Ok(V::Bool(fp_ge(x.low.x, y.low.x))),
        ("<<|", G::Box(x), G::Box(y)) => Ok(V::Bool(fp_lt(x.high.y, y.low.y))),
        ("&<|", G::Box(x), G::Box(y)) => Ok(V::Bool(fp_le(x.high.y, y.high.y))),
        ("|>>", G::Box(x), G::Box(y)) => Ok(V::Bool(fp_gt(x.low.y, y.high.y))),
        ("|&>", G::Box(x), G::Box(y)) => Ok(V::Bool(fp_ge(x.low.y, y.low.y))),
        ("<@", G::Box(x), G::Box(y)) => Ok(V::Bool(box_contain_box(y, x))),
        ("@>", G::Box(x), G::Box(y)) => Ok(V::Bool(box_contain_box(x, y))),
        ("<^", G::Box(x), G::Box(y)) => Ok(V::Bool(fp_le(x.high.y, y.low.y))),
        (">^", G::Box(x), G::Box(y)) => Ok(V::Bool(fp_ge(x.low.y, y.high.y))),
        ("<" | "<=" | "=" | ">" | ">=", G::Box(x), G::Box(y)) => run(|| {
            let t = box_ar(y)?;
            let s = box_ar(x)?;
            Ok(V::Bool(fp_order(op, s, t)))
        }),
        ("<->", G::Box(x), G::Box(y)) => float(run(|| point_dt(&box_cn(x)?, &box_cn(y)?))),
        ("#", G::Box(x), G::Box(y)) => {
            Ok(box_intersect(x, y).map_or(V::Null, |r| V::Geo(G::Box(r))))
        }
        ("+", G::Box(x), G::Point(p)) => run(|| {
            let high = point_add(&x.high, p)?;
            let low = point_add(&x.low, p)?;
            Ok(V::Geo(G::Box(GeoBox { high, low })))
        }),
        ("-", G::Box(x), G::Point(p)) => run(|| {
            let high = point_sub(&x.high, p)?;
            let low = point_sub(&x.low, p)?;
            Ok(V::Geo(G::Box(GeoBox { high, low })))
        }),
        ("*", G::Box(x), G::Point(p)) => box_mul(x, p).map(|r| V::Geo(G::Box(r))),
        ("/", G::Box(x), G::Point(p)) => box_div(x, p).map(|r| V::Geo(G::Box(r))),

        // Paths.
        ("<" | "<=" | "=" | ">" | ">=", G::Path(p), G::Path(q)) => {
            let (m, n) = (p.points.len(), q.points.len());
            Ok(V::Bool(match op {
                "<" => m < n,
                "<=" => m <= n,
                "=" => m == n,
                ">" => m > n,
                _ => m >= n,
            }))
        }
        ("?#", G::Path(p), G::Path(q)) => boolean(path_inter(p, q)),
        ("<->", G::Path(p), G::Path(q)) => {
            segments_distance(&p.points, p.closed, &q.points, q.closed)
                .map(|d| d.map_or(V::Null, V::Float))
        }
        ("+", G::Path(p), G::Path(q)) => {
            path_add(p, q).map(|r| r.map_or(V::Null, |r| V::Geo(G::Path(r))))
        }
        ("+", G::Path(path), G::Point(p)) => {
            path_map(path, |q| point_add(q, p)).map(|r| V::Geo(G::Path(r)))
        }
        ("-", G::Path(path), G::Point(p)) => {
            path_map(path, |q| point_sub(q, p)).map(|r| V::Geo(G::Path(r)))
        }
        ("*", G::Path(path), G::Point(p)) => {
            path_map(path, |q| point_mul(q, p)).map(|r| V::Geo(G::Path(r)))
        }
        ("/", G::Path(path), G::Point(p)) => {
            path_map(path, |q| point_div(q, p)).map(|r| V::Geo(G::Path(r)))
        }

        // Polygons.
        ("<<", G::Polygon(p), G::Polygon(q)) => Ok(V::Bool(p.bound.high.x < q.bound.low.x)),
        ("&<", G::Polygon(p), G::Polygon(q)) => Ok(V::Bool(p.bound.high.x <= q.bound.high.x)),
        (">>", G::Polygon(p), G::Polygon(q)) => Ok(V::Bool(p.bound.low.x > q.bound.high.x)),
        ("&>", G::Polygon(p), G::Polygon(q)) => Ok(V::Bool(p.bound.low.x >= q.bound.low.x)),
        ("<<|", G::Polygon(p), G::Polygon(q)) => Ok(V::Bool(p.bound.high.y < q.bound.low.y)),
        ("&<|", G::Polygon(p), G::Polygon(q)) => Ok(V::Bool(p.bound.high.y <= q.bound.high.y)),
        ("|>>", G::Polygon(p), G::Polygon(q)) => Ok(V::Bool(p.bound.low.y > q.bound.high.y)),
        ("|&>", G::Polygon(p), G::Polygon(q)) => Ok(V::Bool(p.bound.low.y >= q.bound.low.y)),
        ("~=", G::Polygon(p), G::Polygon(q)) => Ok(V::Bool(
            p.points.len() == q.points.len() && plist_same(&p.points, &q.points),
        )),
        ("&&", G::Polygon(p), G::Polygon(q)) => boolean(poly_overlap(p, q)),
        ("@>", G::Polygon(p), G::Polygon(q)) => boolean(poly_contain(p, q)),
        ("<@", G::Polygon(p), G::Polygon(q)) => boolean(poly_contain(q, p)),
        ("<->", G::Polygon(p), G::Polygon(q)) => {
            poly_distance(p, q).map(|d| d.map_or(V::Null, V::Float))
        }

        // Circles.
        ("~=", G::Circle(c), G::Circle(d)) => Ok(V::Bool(circle_same(c, d))),
        ("&&", G::Circle(c), G::Circle(d)) => run(|| {
            let dist = point_dt(&c.center, &d.center)?;
            Ok(V::Bool(fp_le(dist, f8_pl(c.radius, d.radius)?)))
        }),
        ("&<", G::Circle(c), G::Circle(d)) => run(|| {
            Ok(V::Bool(fp_le(
                f8_pl(c.center.x, c.radius)?,
                f8_pl(d.center.x, d.radius)?,
            )))
        }),
        ("<<", G::Circle(c), G::Circle(d)) => run(|| {
            Ok(V::Bool(fp_lt(
                f8_pl(c.center.x, c.radius)?,
                f8_mi(d.center.x, d.radius)?,
            )))
        }),
        (">>", G::Circle(c), G::Circle(d)) => run(|| {
            Ok(V::Bool(fp_gt(
                f8_mi(c.center.x, c.radius)?,
                f8_pl(d.center.x, d.radius)?,
            )))
        }),
        ("&>", G::Circle(c), G::Circle(d)) => run(|| {
            Ok(V::Bool(fp_ge(
                f8_mi(c.center.x, c.radius)?,
                f8_mi(d.center.x, d.radius)?,
            )))
        }),
        ("<@", G::Circle(c), G::Circle(d)) => run(|| {
            let dist = point_dt(&c.center, &d.center)?;
            Ok(V::Bool(fp_le(dist, f8_mi(d.radius, c.radius)?)))
        }),
        ("@>", G::Circle(c), G::Circle(d)) => run(|| {
            let dist = point_dt(&c.center, &d.center)?;
            Ok(V::Bool(fp_le(dist, f8_mi(c.radius, d.radius)?)))
        }),
        ("<<|", G::Circle(c), G::Circle(d)) => run(|| {
            Ok(V::Bool(fp_lt(
                f8_pl(c.center.y, c.radius)?,
                f8_mi(d.center.y, d.radius)?,
            )))
        }),
        ("|>>", G::Circle(c), G::Circle(d)) => run(|| {
            Ok(V::Bool(fp_gt(
                f8_mi(c.center.y, c.radius)?,
                f8_pl(d.center.y, d.radius)?,
            )))
        }),
        ("&<|", G::Circle(c), G::Circle(d)) => run(|| {
            Ok(V::Bool(fp_le(
                f8_pl(c.center.y, c.radius)?,
                f8_pl(d.center.y, d.radius)?,
            )))
        }),
        ("|&>", G::Circle(c), G::Circle(d)) => run(|| {
            Ok(V::Bool(fp_ge(
                f8_mi(c.center.y, c.radius)?,
                f8_mi(d.center.y, d.radius)?,
            )))
        }),
        ("=" | "<>" | "<" | ">" | "<=" | ">=", G::Circle(c), G::Circle(d)) => run(|| {
            let t = circle_ar(d)?;
            let s = circle_ar(c)?;
            Ok(V::Bool(fp_order(op, s, t)))
        }),
        ("<->", G::Circle(c), G::Circle(d)) => run(|| {
            let dist = point_dt(&c.center, &d.center)?;
            Ok(V::Float(not_below_zero(f8_mi(
                dist,
                f8_pl(c.radius, d.radius)?,
            )?)))
        }),
        ("+", G::Circle(c), G::Point(p)) => run(|| {
            let center = point_add(&c.center, p)?;
            Ok(V::Geo(G::Circle(Circle {
                center,
                radius: c.radius,
            })))
        }),
        ("-", G::Circle(c), G::Point(p)) => run(|| {
            let center = point_sub(&c.center, p)?;
            Ok(V::Geo(G::Circle(Circle {
                center,
                radius: c.radius,
            })))
        }),
        ("*", G::Circle(c), G::Point(p)) => run(|| {
            let center = point_mul(&c.center, p)?;
            let radius = f8_mul(c.radius, hypot(p.x, p.y)?)?;
            Ok(V::Geo(G::Circle(Circle { center, radius })))
        }),
        ("/", G::Circle(c), G::Point(p)) => run(|| {
            let center = point_div(&c.center, p)?;
            let radius = f8_div(c.radius, hypot(p.x, p.y)?)?;
            Ok(V::Geo(G::Circle(Circle { center, radius })))
        }),

        // Across types: distances.
        ("<->", G::Point(p), G::Lseg(l)) | ("<->", G::Lseg(l), G::Point(p)) => {
            float(lseg_closept_point(l, p).map(|r| r.0))
        }
        ("<->", G::Point(p), G::Path(path)) | ("<->", G::Path(path), G::Point(p)) => {
            float(dist_ppath(p, path))
        }
        ("<->", G::Point(p), G::Box(x)) | ("<->", G::Box(x), G::Point(p)) => {
            float(box_closept_point(x, p).map(|r| r.0))
        }
        ("<->", G::Point(p), G::Polygon(poly)) | ("<->", G::Polygon(poly), G::Point(p)) => {
            float(dist_ppoly(p, poly))
        }
        ("<->", G::Point(p), G::Line(l)) | ("<->", G::Line(l), G::Point(p)) => {
            float(line_closept_point(l, p).map(|r| r.0))
        }
        ("<->", G::Point(p), G::Circle(c)) | ("<->", G::Circle(c), G::Point(p)) => run(|| {
            let dist = point_dt(p, &c.center)?;
            Ok(V::Float(not_below_zero(f8_mi(dist, c.radius)?)))
        }),
        ("<->", G::Lseg(l), G::Box(x)) | ("<->", G::Box(x), G::Lseg(l)) => {
            float(box_closept_lseg(x, l, false).map(|r| r.0))
        }
        ("<->", G::Lseg(l), G::Line(line)) | ("<->", G::Line(line), G::Lseg(l)) => {
            float(lseg_closept_line(l, line).map(|r| r.0))
        }
        ("<->", G::Polygon(poly), G::Circle(c)) | ("<->", G::Circle(c), G::Polygon(poly)) => {
            float(dist_cpoly(c, poly))
        }

        // Across types: closest points.
        ("##", G::Point(p), G::Lseg(l)) => lseg_closept_point(l, p).map(closest),
        ("##", G::Point(p), G::Box(x)) => box_closept_point(x, p).map(closest),
        ("##", G::Point(p), G::Line(l)) => line_closept_point(l, p).map(closest),
        ("##", G::Lseg(l), G::Lseg(m)) => run(|| {
            if lseg_sl(l)? == lseg_sl(m)? {
                return Ok(V::Null);
            }
            lseg_closept_lseg(m, l).map(closest)
        }),
        ("##", G::Lseg(l), G::Box(x)) => box_closept_lseg(x, l, true).map(|(d, p)| match p {
            Some(p) => closest((d, p)),
            None => V::Null,
        }),
        ("##", G::Line(line), G::Lseg(l)) => run(|| {
            if lseg_sl(l)? == line_sl(line)? {
                return Ok(V::Null);
            }
            lseg_closept_line(l, line).map(closest)
        }),

        // Across types: containment and intersection.
        ("<@", G::Point(p), G::Lseg(l)) => boolean(lseg_contain_point(l, p)),
        ("<@", G::Point(p), G::Path(path)) | ("@>", G::Path(path), G::Point(p)) => {
            boolean(on_ppath(p, path))
        }
        ("<@", G::Point(p), G::Box(x)) | ("@>", G::Box(x), G::Point(p)) => {
            Ok(V::Bool(box_contain_point(x, p)))
        }
        ("<@", G::Point(p), G::Polygon(poly)) | ("@>", G::Polygon(poly), G::Point(p)) => {
            boolean(point_inside(p, &poly.points).map(|r| r != 0))
        }
        ("<@", G::Point(p), G::Line(l)) => boolean(line_contain_point(l, p)),
        ("<@", G::Point(p), G::Circle(c)) | ("@>", G::Circle(c), G::Point(p)) => {
            boolean(point_dt(&c.center, p).map(|d| d <= c.radius))
        }
        ("<@", G::Lseg(l), G::Box(x)) => Ok(V::Bool(
            box_contain_point(x, &l.p[0]) && box_contain_point(x, &l.p[1]),
        )),
        ("<@", G::Lseg(l), G::Line(line)) => boolean(run(|| {
            Ok(line_contain_point(line, &l.p[0])? && line_contain_point(line, &l.p[1])?)
        })),
        ("?#", G::Lseg(l), G::Box(x)) => boolean(box_interpt_lseg(x, l, false).map(|r| r.0)),
        ("?#", G::Lseg(l), G::Line(line)) => {
            boolean(lseg_interpt_line(l, line).map(|p| p.is_some()))
        }
        ("?#", G::Line(line), G::Box(x)) => boolean(inter_lb(line, x)),

        _ => return None,
    };
    Some(v)
}

/// The steps `f` runs, as one result.
fn run<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    f()
}

/// The fuzzy comparison an area or a length operator `op` names: FPlt, FPle, FPeq, FPne, FPgt or
/// FPge.
fn fp_order(op: &str, a: f64, b: f64) -> bool {
    match op {
        "<" => fp_lt(a, b),
        "<=" => fp_le(a, b),
        "=" => fp_eq(a, b),
        "<>" => fp_ne(a, b),
        ">" => fp_gt(a, b),
        _ => fp_ge(a, b),
    }
}

/// `a` cast to type `to` as PostgreSQL's pg_cast holds the cast on 19, computed by its function:
/// point to box (point_box), lseg to point (lseg_center), path to polygon (path_poly, refused for
/// an open path), box to point (box_center), lseg (box_diagonal), polygon (box_poly) or circle
/// (box_circle), polygon to point (poly_center), path (poly_path), box (poly_box) or circle
/// (poly_circle), circle to point (circle_center), box (circle_box) or polygon (circle_to_poly,
/// 12 points). A value cast to its own type is itself. None where PostgreSQL has no such cast.
pub fn geo_cast(a: &Geo, to: GeoKind) -> Option<Result<Geo>> {
    use Geo as G;
    use GeoKind as K;
    if a.kind() == to {
        return Some(Ok(a.clone()));
    }
    let v = match (a, to) {
        (G::Point(p), K::Box) => Ok(G::Box(GeoBox { high: *p, low: *p })),
        (G::Lseg(l), K::Point) => lseg_center(l).map(G::Point),
        (G::Path(p), K::Polygon) => path_poly(p).map(G::Polygon),
        (G::Box(b), K::Point) => box_cn(b).map(G::Point),
        (G::Box(b), K::Lseg) => Ok(G::Lseg(lseg(b.high, b.low))),
        (G::Box(b), K::Polygon) => Ok(G::Polygon(box_poly(b))),
        (G::Box(b), K::Circle) => box_circle(b).map(G::Circle),
        (G::Polygon(p), K::Point) => poly_to_circle(p).map(|c| G::Point(c.center)),
        (G::Polygon(p), K::Path) => Ok(G::Path(Path {
            closed: true,
            points: p.points.clone(),
        })),
        (G::Polygon(p), K::Box) => Ok(G::Box(p.bound)),
        (G::Polygon(p), K::Circle) => poly_to_circle(p).map(G::Circle),
        (G::Circle(c), K::Point) => Ok(G::Point(c.center)),
        (G::Circle(c), K::Box) => circle_box(c).map(G::Box),
        (G::Circle(c), K::Polygon) => circle_poly(12, c).map(G::Polygon),
        _ => return None,
    };
    Some(v)
}

/// An argument of a geometric function: a geometric value, a double precision or an integer.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub enum GeoArg<'a> {
    Geo(&'a Geo),
    Float(f64),
    Int(i32),
}

/// The bytes `palloc` refuses past, `MaxAllocSize`.
const MAX_ALLOC: i64 = 0x3fff_ffff;

/// The function `name` on `args` as PostgreSQL's pg_proc holds it on 19 for those types, computed by
/// its function: box_area, circle_area, path_area (NULL for an open path), box_center,
/// circle_center, box_diagonal, circle_diameter, box_height, box_width, circle_radius,
/// path_isclosed, path_isopen, path_close, path_open, point_slope, points_box, cr_circle,
/// line_construct_pp, lseg_construct, construct_point and circle_poly; and those an operator
/// shares (ishorizontal, isvertical, isparallel, isperp, length, npoints) by the operator's. None
/// where PostgreSQL has no such function of these types.
pub fn geo_function(name: &str, args: &[GeoArg]) -> Option<Result<GeoValue>> {
    use Geo as G;
    use GeoArg as A;
    use GeoValue as V;
    let v = match (name, args) {
        ("area", [A::Geo(G::Box(b))]) => box_ar(b).map(V::Float),
        ("area", [A::Geo(G::Circle(c))]) => circle_ar(c).map(V::Float),
        ("area", [A::Geo(G::Path(p))]) => run(|| {
            if !p.closed {
                return Ok(V::Null);
            }
            let n = p.points.len();
            let mut area = 0.0;
            for i in 0..n {
                let (a, b) = (&p.points[i], &p.points[(i + 1) % n]);
                area = f8_pl(area, f8_mul(a.x, b.y)?)?;
                area = f8_mi(area, f8_mul(a.y, b.x)?)?;
            }
            f8_div(area.abs(), 2.0).map(V::Float)
        }),
        ("center", [A::Geo(G::Box(b))]) => box_cn(b).map(point_value),
        ("center", [A::Geo(G::Circle(c))]) => Ok(point_value(c.center)),
        ("diagonal", [A::Geo(G::Box(b))]) => Ok(V::Geo(G::Lseg(lseg(b.high, b.low)))),
        ("diameter", [A::Geo(G::Circle(c))]) => f8_mul(c.radius, 2.0).map(V::Float),
        ("height", [A::Geo(G::Box(b))]) => f8_mi(b.high.y, b.low.y).map(V::Float),
        ("width", [A::Geo(G::Box(b))]) => f8_mi(b.high.x, b.low.x).map(V::Float),
        ("radius", [A::Geo(G::Circle(c))]) => Ok(V::Float(c.radius)),
        ("isclosed", [A::Geo(G::Path(p))]) => Ok(V::Bool(p.closed)),
        ("isopen", [A::Geo(G::Path(p))]) => Ok(V::Bool(!p.closed)),
        ("pclose" | "popen", [A::Geo(G::Path(p))]) => Ok(V::Geo(G::Path(Path {
            closed: name == "pclose",
            points: p.points.clone(),
        }))),
        ("ishorizontal", [A::Geo(a)]) => return geo_unary("?-", a),
        ("isvertical", [A::Geo(a)]) => return geo_unary("?|", a),
        ("length", [A::Geo(a @ (G::Lseg(_) | G::Path(_)))]) => return geo_unary("@-@", a),
        ("npoints", [A::Geo(a)]) => return geo_unary("#", a),
        ("ishorizontal", [A::Geo(a @ G::Point(_)), A::Geo(b @ G::Point(_))]) => {
            return geo_binary("?-", a, b)
        }
        ("isvertical", [A::Geo(a @ G::Point(_)), A::Geo(b @ G::Point(_))]) => {
            return geo_binary("?|", a, b)
        }
        ("isparallel", [A::Geo(a), A::Geo(b)]) if a.kind() == b.kind() => {
            return geo_binary("?||", a, b)
        }
        ("isperp", [A::Geo(a), A::Geo(b)]) if a.kind() == b.kind() => {
            return geo_binary("?-|", a, b)
        }
        ("slope", [A::Geo(G::Point(p)), A::Geo(G::Point(q))]) => point_sl(p, q).map(V::Float),
        ("box", [A::Geo(G::Point(p)), A::Geo(G::Point(q))]) => {
            Ok(V::Geo(G::Box(box_construct(p, q))))
        }
        ("lseg", [A::Geo(G::Point(p)), A::Geo(G::Point(q))]) => Ok(V::Geo(G::Lseg(lseg(*p, *q)))),
        ("line", [A::Geo(G::Point(p)), A::Geo(G::Point(q))]) => run(|| {
            if point_eq_point(p, q) {
                return Err(Error::new(
                    "invalid line specification: must be two distinct points",
                ));
            }
            Ok(V::Geo(G::Line(line_construct(p, point_sl(p, q)?)?)))
        }),
        ("circle", [A::Geo(G::Point(p)), A::Float(r)]) => Ok(V::Geo(G::Circle(Circle {
            center: *p,
            radius: *r,
        }))),
        ("point", [A::Float(x), A::Float(y)]) => Ok(point_value(Point { x: *x, y: *y })),
        ("polygon", [A::Int(n), A::Geo(G::Circle(c))]) => run(|| {
            if fp_zero(c.radius) || *n < 2 {
                return circle_poly(*n, c).map(|p| V::Geo(G::Polygon(p)));
            }
            // The polygon's size as circle_poly_internal reckons it in an int.
            let base = (16 * i64::from(*n)) as i32;
            let size = (40 + i64::from(base)) as i32;
            if base / *n != 16 || size <= base {
                return Err(Error::new("too many points requested"));
            }
            if i64::from(size) > MAX_ALLOC {
                return Err(Error::new(format!(
                    "invalid memory alloc request size {size}"
                )));
            }
            circle_poly(*n, c).map(|p| V::Geo(G::Polygon(p)))
        }),
        _ => return None,
    };
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_distance_rounds_as_the_release_it_is_read_under_rounds_it() {
        let at = |x, y| point_dt(&Point { x, y }, &Point { x: 0.0, y: 0.0 });
        let of_18 = |x, y| at_release(18, || at(x, y));
        // As 18.6 and 19 RC1 each answer it (28818, 28819, 2026-11-02).
        assert_eq!(of_18(1e150, 1e150).unwrap(), 1.4142135623730951e150);
        assert_eq!(at(1e150, 1e150).unwrap(), 1.414213562373095e150);
        assert_eq!(at_release(19, || at(1e150, 1e150)).unwrap(), 1.414213562373095e150);
        assert_eq!(of_18(3.0, 4.0).unwrap(), 5.0);
        assert_eq!(of_18(f64::INFINITY, f64::NAN).unwrap(), f64::INFINITY);
        assert!(of_18(f64::NAN, 1.0).unwrap().is_nan());
        assert_eq!(of_18(1e308, 1e308).unwrap(), 1.4142135623730951e308);
        assert!(of_18(1.7e308, 1.7e308).unwrap_err().to_string().contains("overflow"));
        assert!(at(1.7e308, 1.7e308).unwrap().is_infinite());
        // Past the call, as it was.
        assert_eq!(at(1e150, 1e150).unwrap(), 1.414213562373095e150);
    }

    /// Boxes of the coordinates given, a coordinate each from the list, high above low unless a NaN.
    fn box_of(v: [f64; 4]) -> GeoBox {
        GeoBox {
            high: Point {
                x: v[0].max(v[2]),
                y: v[1].max(v[3]),
            },
            low: Point {
                x: v[0].min(v[2]),
                y: v[1].min(v[3]),
            },
        }
    }

    #[test]
    fn a_block_s_leaves_never_rule_out_a_box_an_operator_keeps() {
        // PostgreSQL's fuzzy comparisons are held at their edge: a walk of coordinates either side of
        // EPSILON, and past the ends, on blocks of two boxes and every operator.
        let near = [
            -1.0,
            -EPSILON,
            -EPSILON / 2.0,
            0.0,
            EPSILON / 2.0,
            EPSILON,
            1.0,
        ];
        let far = [f64::NEG_INFINITY, f64::INFINITY, 1e300, f64::NAN];
        let all: Vec<f64> = near.iter().chain(&far).copied().collect();
        // A seeded walk of the boxes: every quadruple would ask too many pairs.
        let mut at = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            at ^= at << 13;
            at ^= at >> 7;
            at ^= at << 17;
            all[(at % all.len() as u64) as usize]
        };
        let keep = |raw: [f64; 4]| -> GeoBox {
            // A NaN stays where it is written, as box_in leaves it.
            if raw.iter().any(|x| x.is_nan()) {
                GeoBox {
                    high: Point {
                        x: raw[0],
                        y: raw[1],
                    },
                    low: Point {
                        x: raw[2],
                        y: raw[3],
                    },
                }
            } else {
                box_of(raw)
            }
        };
        let mut kept = 0u64;
        for _ in 0..200_000 {
            let mut draw = || keep([next(), next(), next(), next()]);
            let (a, b, q) = (draw(), draw(), draw());
            let of = |f: fn(&GeoBox) -> f64| {
                let key = |x: f64| crate::scalars::float8_key(x.to_bits());
                let (x, y) = (f(&a), f(&b));
                let least = if key(x) <= key(y) { x } else { y };
                let greatest = if key(x) >= key(y) { x } else { y };
                (least, greatest)
            };
            let leaves = [
                of(|b| b.high.x),
                of(|b| b.high.y),
                of(|b| b.low.x),
                of(|b| b.low.y),
            ];
            for op in [
                "<<", "&<", "&&", "&>", ">>", "~=", "@>", "<@", "&<|", "<<|", "|>>", "|&>",
            ] {
                for row in [&a, &b] {
                    let held = geo_binary(op, &Geo::Box(row.clone()), &Geo::Box(q.clone()));
                    if matches!(held, Some(Ok(GeoValue::Bool(true)))) {
                        kept += 1;
                        assert!(
                            box_may_hold(op, leaves, &q),
                            "{op}: {row:?} against {q:?} ruled out by {leaves:?}"
                        );
                    }
                }
            }
            let p = Point {
                x: q.high.x,
                y: q.low.y,
            };
            for row in [&a, &b] {
                if box_contain_point(row, &p) {
                    assert!(box_may_hold_point(leaves, &p), "{row:?} {p:?}");
                }
            }
        }
        // A walk that kept nothing would hold nothing.
        assert!(kept > 100_000, "{kept}");
    }

    /// What PostgreSQL 19 answered, one question a line, tab separated: `in`, a type and a text
    /// read as it; `un`, a prefix operator, a type and a text; `cast`, a type, a text and the type
    /// cast to; `bin`, an operator and two typed texts. Last on each line, the answer as psql
    /// prints it with `extra_float_digits` at 1, or `ERROR: ` and the message. Texts write a
    /// backslash as `\\` and a control character as `\x` and two hex digits.
    const ANSWERS: &str = r#"
in	point	(1,2)	(1,2)
in	point	(-Infinity,-1.7976931348623157e+308)	(-Infinity,-1.7976931348623157e+308)
in	point	(NaN,Infinity)	(NaN,Infinity)
in	point	(0,0)	(0,0)
in	point	(1.0000001,2)	(1.0000001,2)
in	point	(1e2,-0)	(100,-0)
in	point	(0.1,0.2)	(0.1,0.2)
in	point	(10000000.5,0)	(10000000.5,0)
in	point	1,2	(1,2)
in	point	( 1 , 2 )	(1,2)
in	line	{1,-1,0}	{1,-1,0}
in	line	{2,-2,0}	{2,-2,0}
in	line	{0,1,0}	{0,1,0}
in	line	{1,0,-5}	{1,0,-5}
in	line	[(0,0),(1,1)]	{1,-1,0}
in	lseg	[(0,0),(1,1)]	[(0,0),(1,1)]
in	lseg	[(1,1),(0,0)]	[(1,1),(0,0)]
in	lseg	[(-1,-1),(1e10,1e-10)]	[(-1,-1),(10000000000,1e-10)]
in	lseg	(0,0),(1,1)	[(0,0),(1,1)]
in	box	(1,1),(0,0)	(1,1),(0,0)
in	box	(Infinity,Infinity),(-Infinity,-Infinity)	(Infinity,Infinity),(-Infinity,-Infinity)
in	box	(2,0.5),(0,0)	(2,0.5),(0,0)
in	box	(0,0),(0,0)	(0,0),(0,0)
in	box	((1,0),(0,1))	(1,1),(0,0)
in	path	[(0,0),(1,1)]	[(0,0),(1,1)]
in	path	((0,0),(1,1))	((0,0),(1,1))
in	path	((5,5),(9,9))	((5,5),(9,9))
in	path	[(0,0)]	[(0,0)]
in	path	(0,0),(1,1)	((0,0),(1,1))
in	polygon	((0,0),(1,1),(1,0))	((0,0),(1,1),(1,0))
in	polygon	((1,1),(1,0),(0,0))	((1,1),(1,0),(0,0))
in	polygon	((0,0))	((0,0))
in	polygon	(0,0),(1,1),(1,0)	((0,0),(1,1),(1,0))
in	circle	<(0,0),1>	<(0,0),1>
in	circle	<(5,5),1>	<(5,5),1>
in	circle	<(0,0),0>	<(0,0),0>
in	circle	<(1.5,-2.5),1e300>	<(1.5,-2.5),1e+300>
in	circle	0,0,1	<(0,0),1>
in	circle	((0,0),1)	<(0,0),1>
bin	*	point	(1e300,1e-200)	point	(1e300,1e-200)	ERROR: value out of range: underflow
bin	/	point	(1,1)	point	(4.9e-324,1e300)	ERROR: value out of range: overflow
bin	/	point	(1,1)	point	(1e300,4.9e-324)	ERROR: value out of range: underflow
bin	<	box	(1e-200,1e-200),(0,0)	box	(1e300,1e300),(-1e300,-1e300)	ERROR: value out of range: overflow
bin	<	box	(1e300,1e300),(-1e300,-1e300)	box	(1e-200,1e-200),(0,0)	ERROR: value out of range: underflow
bin	=	circle	<(0,0),1e-200>	circle	<(0,0),1e300>	ERROR: value out of range: overflow
bin	=	circle	<(0,0),1e300>	circle	<(0,0),1e-200>	ERROR: value out of range: underflow
bin	<@	point	(1e300,1e-200)	line	{1e300,1e-200,0}	ERROR: value out of range: underflow
bin	<->	path	[(0,0)]	point	(3,4)	0
bin	<->	path	[(0,0)]	path	[(1,1)]	NULL
cast	circle	<(1,2),3>	polygon	((-2,2),(-1.598076211353316,3.5),(-0.5000000000000004,4.598076211353316),(0.9999999999999998,5),(2.499999999999999,4.598076211353316),(3.5980762113533156,3.500000000000001),(4,2.0000000000000004),(3.5980762113533165,0.5000000000000009),(2.5000000000000013,-0.5980762113533151),(1.0000000000000004,-1),(-0.499999999999998,-0.5980762113533169),(-1.5980762113533151,0.49999999999999867))
cast	circle	<(1e300,2),1e300>	polygon	((0,2),(1.3397459621556137e+299,4.9999999999999995e+299),(4.9999999999999995e+299,8.660254037844387e+299),(1e+300,1e+300),(1.4999999999999998e+300,8.660254037844387e+299),(1.8660254037844387e+300,5.000000000000003e+299),(2e+300,1.2246467991473533e+284),(1.8660254037844387e+300,-4.999999999999997e+299),(1.5000000000000004e+300,-8.660254037844384e+299),(1.0000000000000002e+300,-1e+300),(5.000000000000007e+299,-8.660254037844391e+299),(1.3397459621556167e+299,-5.000000000000005e+299))
cast	circle	<(1,2),1e-7>	polygon	ERROR: cannot convert circle with radius zero to polygon
un	@@	polygon	((0,0),(1e308,0),(1e308,1))	ERROR: value out of range: overflow
un	#	path	[(0,0),(1,1),(2,2)]	3
cast	path	[(0,0),(1,1)]	polygon	ERROR: open path cannot be converted to polygon
in	line	[(0,1e308),(1,-1e308)]	ERROR: value out of range: overflow
in	line	{1e-7,1e-7,1}	ERROR: invalid line specification: A and B cannot both be zero
in	line	[(1,2),(1.0000001,2)]	ERROR: invalid line specification: must be two distinct points
in	point	(0,0)	(0,0)
in	path	(0,0)	((0,0))
in	polygon	(0,0)	((0,0))
in	point	(-0,-0)	(-0,-0)
in	path	(-0,-0)	((-0,-0))
in	polygon	(-0,-0)	((-0,-0))
in	lseg	[(1,2),(3,4)]	[(1,2),(3,4)]
in	path	[(1,2),(3,4)]	[(1,2),(3,4)]
in	line	[(1,2),(3,4)]	{1,-1,1}
in	lseg	[(0,0),(0,0)]	[(0,0),(0,0)]
in	path	[(0,0),(0,0)]	[(0,0),(0,0)]
in	line	[(0,0),(0,0)]	ERROR: invalid line specification: must be two distinct points
in	lseg	((1,2),(3,4))	[(1,2),(3,4)]
in	path	((1,2),(3,4))	((1,2),(3,4))
in	line	((1,2),(3,4))	{1,-1,1}
in	box	((1,2),(3,4))	(3,4),(1,2)
in	polygon	((1,2),(3,4))	((1,2),(3,4))
in	path	[(0,0)]	[(0,0)]
in	path	((0,0))	((0,0))
in	polygon	((0,0))	((0,0))
in	circle	<(1,2),3>	<(1,2),3>
in	line	{0,0,1}	ERROR: invalid line specification: A and B cannot both be zero
in	lseg	 (0,0), (6,6)	[(0,0),(6,6)]
in	path	 (0,0), (6,6)	((0,0),(6,6))
in	line	 (0,0), (6,6)	{1,-1,0}
in	box	 (0,0), (6,6)	(6,6),(0,0)
in	polygon	 (0,0), (6,6)	((0,0),(6,6))
in	circle	 ( 100 , 200 ) , 10 	<(100,200),10>
in	lseg	( 11,12,13,14) 	[(11,12),(13,14)]
in	path	( 11,12,13,14) 	((11,12),(13,14))
in	line	( 11,12,13,14) 	{1,-1,1}
in	box	( 11,12,13,14) 	(13,14),(11,12)
in	polygon	( 11,12,13,14) 	((11,12),(13,14))
in	circle	 ( ( 1 , 2 ) , 3 ) 	<(1,2),3>
in	lseg	 ( ( 1 , 2 ) , ( 3 , 4 ) ) 	[(1,2),(3,4)]
in	path	 ( ( 1 , 2 ) , ( 3 , 4 ) ) 	((1,2),(3,4))
in	line	 ( ( 1 , 2 ) , ( 3 , 4 ) ) 	{1,-1,1}
in	box	 ( ( 1 , 2 ) , ( 3 , 4 ) ) 	(3,4),(1,2)
in	polygon	 ( ( 1 , 2 ) , ( 3 , 4 ) ) 	((1,2),(3,4))
in	circle	 1 , 3 , 5 	<(1,3),5>
in	point	 ( Nan , NaN ) 	(NaN,NaN)
in	path	 ( Nan , NaN ) 	((NaN,NaN))
in	polygon	 ( Nan , NaN ) 	((NaN,NaN))
in	path	((1,2))	((1,2))
in	polygon	((1,2))	((1,2))
in	point	 ( 1 , 2 ) 	(1,2)
in	path	 ( 1 , 2 ) 	((1,2))
in	polygon	 ( 1 , 2 ) 	((1,2))
in	point	\x09(1,2)\x0a	(1,2)
in	path	\x09(1,2)\x0a	((1,2))
in	polygon	\x09(1,2)\x0a	((1,2))
in	point	(1e400,2)	ERROR: "1e400" is out of range for type double precision
in	lseg	(1e400,2)	ERROR: "1e400" is out of range for type double precision
in	path	(1e400,2)	ERROR: "1e400" is out of range for type double precision
in	line	(1e400,2)	ERROR: "1e400" is out of range for type double precision
in	box	(1e400,2)	ERROR: "1e400" is out of range for type double precision
in	polygon	(1e400,2)	ERROR: "1e400" is out of range for type double precision
in	circle	(1e400,2)	ERROR: "1e400" is out of range for type double precision
in	point	(1e-400,2)	ERROR: "1e-400" is out of range for type double precision
in	lseg	(1e-400,2)	ERROR: "1e-400" is out of range for type double precision
in	path	(1e-400,2)	ERROR: "1e-400" is out of range for type double precision
in	line	(1e-400,2)	ERROR: "1e-400" is out of range for type double precision
in	box	(1e-400,2)	ERROR: "1e-400" is out of range for type double precision
in	polygon	(1e-400,2)	ERROR: "1e-400" is out of range for type double precision
in	circle	(1e-400,2)	ERROR: "1e-400" is out of range for type double precision
in	point	(0e-400,2)	(0,2)
in	path	(0e-400,2)	((0,2))
in	polygon	(0e-400,2)	((0,2))
in	point	(3e-324,1)	(5e-324,1)
in	path	(3e-324,1)	((5e-324,1))
in	polygon	(3e-324,1)	((5e-324,1))
in	point	(-1e400,2)	ERROR: "-1e400" is out of range for type double precision
in	lseg	(-1e400,2)	ERROR: "-1e400" is out of range for type double precision
in	path	(-1e400,2)	ERROR: "-1e400" is out of range for type double precision
in	line	(-1e400,2)	ERROR: "-1e400" is out of range for type double precision
in	box	(-1e400,2)	ERROR: "-1e400" is out of range for type double precision
in	polygon	(-1e400,2)	ERROR: "-1e400" is out of range for type double precision
in	circle	(-1e400,2)	ERROR: "-1e400" is out of range for type double precision
in	point	(.5,2.)	(0.5,2)
in	path	(.5,2.)	((0.5,2))
in	polygon	(.5,2.)	((0.5,2))
in	point	(0x10,0x1p3)	(16,8)
in	path	(0x10,0x1p3)	((16,8))
in	polygon	(0x10,0x1p3)	((16,8))
in	point	(inf,-inf)	(Infinity,-Infinity)
in	path	(inf,-inf)	((Infinity,-Infinity))
in	polygon	(inf,-inf)	((Infinity,-Infinity))
in	point	(infinity,-INFINITY)	(Infinity,-Infinity)
in	path	(infinity,-INFINITY)	((Infinity,-Infinity))
in	polygon	(infinity,-INFINITY)	((Infinity,-Infinity))
in	point	(+inf,+nan)	(Infinity,NaN)
in	path	(+inf,+nan)	((Infinity,NaN))
in	polygon	(+inf,+nan)	((Infinity,NaN))
in	point	(nan(123),1)	(NaN,1)
in	path	(nan(123),1)	((NaN,1))
in	polygon	(nan(123),1)	((NaN,1))
in	point	(-nan,1)	(NaN,1)
in	path	(-nan,1)	((NaN,1))
in	polygon	(-nan,1)	((NaN,1))
in	lseg	(1,2)(3,4)	[(1,2),(3,4)]
in	line	(1,2)(3,4)	{1,-1,1}
in	box	(1,2)(3,4)	(3,4),(1,2)
in	lseg	1,2,3,4	[(1,2),(3,4)]
in	path	1,2,3,4	((1,2),(3,4))
in	line	1,2,3,4	{1,-1,1}
in	box	1,2,3,4	(3,4),(1,2)
in	polygon	1,2,3,4	((1,2),(3,4))
in	lseg	(1,2),(3,4)	[(1,2),(3,4)]
in	path	(1,2),(3,4)	((1,2),(3,4))
in	line	(1,2),(3,4)	{1,-1,1}
in	box	(1,2),(3,4)	(3,4),(1,2)
in	polygon	(1,2),(3,4)	((1,2),(3,4))
in	lseg	[(1,2),(3,4))	[(1,2),(3,4)]
in	path	[(1,2),(3,4))	[(1,2),(3,4)]
in	line	[(1,2),(3,4))	{1,-1,1}
in	lseg	[1,2,3,4]	[(1,2),(3,4)]
in	path	[1,2,3,4]	[(1,2),(3,4)]
in	line	[1,2,3,4]	{1,-1,1}
in	lseg	(1,2,3,4)	[(1,2),(3,4)]
in	path	(1,2,3,4)	((1,2),(3,4))
in	line	(1,2,3,4)	{1,-1,1}
in	box	(1,2,3,4)	(3,4),(1,2)
in	polygon	(1,2,3,4)	((1,2),(3,4))
in	lseg	( (1,2) , (3,4) )	[(1,2),(3,4)]
in	path	( (1,2) , (3,4) )	((1,2),(3,4))
in	line	( (1,2) , (3,4) )	{1,-1,1}
in	box	( (1,2) , (3,4) )	(3,4),(1,2)
in	polygon	( (1,2) , (3,4) )	((1,2),(3,4))
in	circle	<1,2,3>	<(1,2),3>
in	circle	<(1,2),-0>	<(1,2),-0>
in	circle	((1,2),3)	<(1,2),3>
in	circle	(1,2),3	<(1,2),3>
in	circle	1,2,3	<(1,2),3>
in	circle	((1,2)3)	<(1,2),3>
in	circle	<(1,2),3)	<(1,2),3>
in	circle	((1,2),3>	<(1,2),3>
in	line	{1,2,3}	{1,2,3}
in	line	{1e-7,1e-7,1}	ERROR: invalid line specification: A and B cannot both be zero
in	line	{1e-6,0,1}	ERROR: invalid line specification: A and B cannot both be zero
in	line	 {1,2,3}	{1,2,3}
in	line	{-0,1,-0}	{-0,1,-0}
in	line	{NaN,0,0}	{NaN,0,0}
in	lseg	[(0,0),(1e-7,0)]	[(0,0),(1e-07,0)]
in	path	[(0,0),(1e-7,0)]	[(0,0),(1e-07,0)]
in	line	[(0,0),(1e-7,0)]	ERROR: invalid line specification: must be two distinct points
in	lseg	[(0,0),(1,1)]	[(0,0),(1,1)]
in	path	[(0,0),(1,1)]	[(0,0),(1,1)]
in	line	[(0,0),(1,1)]	{1,-1,0}
in	lseg	[(0,1e308),(1,-1e308)]	[(0,1e+308),(1,-1e+308)]
in	path	[(0,1e308),(1,-1e308)]	[(0,1e+308),(1,-1e+308)]
in	line	[(0,1e308),(1,-1e308)]	ERROR: value out of range: overflow
in	lseg	[(1e-300,1),(2e-300,2)]	[(1e-300,1),(2e-300,2)]
in	path	[(1e-300,1),(2e-300,2)]	[(1e-300,1),(2e-300,2)]
in	line	[(1e-300,1),(2e-300,2)]	{-1,0,1e-300}
in	lseg	[(1,1e300),(2,-1e300)]	[(1,1e+300),(2,-1e+300)]
in	path	[(1,1e300),(2,-1e300)]	[(1,1e+300),(2,-1e+300)]
in	line	[(1,1e300),(2,-1e300)]	{-2e+300,-1,3e+300}
in	path	(0,0),(1,1),(2,2)	((0,0),(1,1),(2,2))
in	polygon	(0,0),(1,1),(2,2)	((0,0),(1,1),(2,2))
in	path	((0,0),(1,1),(2,2))	((0,0),(1,1),(2,2))
in	polygon	((0,0),(1,1),(2,2))	((0,0),(1,1),(2,2))
in	path	[(0,0),(1,1),(2,2)]	[(0,0),(1,1),(2,2)]
in	lseg	(0,0),(1,1),	[(0,0),(1,1)]
in	line	(0,0),(1,1),	{1,-1,0}
in	box	(0,0),(1,1),	(1,1),(0,0)
in	lseg	( ( 0 , 0 ) , ( 1 , 1 ) )	[(0,0),(1,1)]
in	path	( ( 0 , 0 ) , ( 1 , 1 ) )	((0,0),(1,1))
in	line	( ( 0 , 0 ) , ( 1 , 1 ) )	{1,-1,0}
in	box	( ( 0 , 0 ) , ( 1 , 1 ) )	(1,1),(0,0)
in	polygon	( ( 0 , 0 ) , ( 1 , 1 ) )	((0,0),(1,1))
in	path	(0,0,1,1,2,2)	((0,0),(1,1),(2,2))
in	polygon	(0,0,1,1,2,2)	((0,0),(1,1),(2,2))
in	lseg	[0,0,1,1]	[(0,0),(1,1)]
in	path	[0,0,1,1]	[(0,0),(1,1)]
in	line	[0,0,1,1]	{1,-1,0}
in	point	(1,2)\x0b	(1,2)
in	path	(1,2)\x0b	((1,2))
in	polygon	(1,2)\x0b	((1,2))
in	point	\x0c(1,2)	(1,2)
in	path	\x0c(1,2)	((1,2))
in	polygon	\x0c(1,2)	((1,2))
in	point	1 , 2	(1,2)
in	path	1 , 2	((1,2))
in	polygon	1 , 2	((1,2))
in	lseg	((1,2),(3,4),)	[(1,2),(3,4)]
in	line	((1,2),(3,4),)	{1,-1,1}
in	box	((1,2),(3,4),)	(3,4),(1,2)
in	lseg	[(1,2),(3,4),]	[(1,2),(3,4)]
in	line	[(1,2),(3,4),]	{1,-1,1}
in	lseg	[(1,2)(3,4)]	[(1,2),(3,4)]
in	line	[(1,2)(3,4)]	{1,-1,1}
in	lseg	((1,2)(3,4))	[(1,2),(3,4)]
in	line	((1,2)(3,4))	{1,-1,1}
in	box	((1,2)(3,4))	(3,4),(1,2)
in	point	(1e999999999999,1)	ERROR: "1e999999999999" is out of range for type double precision
in	lseg	(1e999999999999,1)	ERROR: "1e999999999999" is out of range for type double precision
in	path	(1e999999999999,1)	ERROR: "1e999999999999" is out of range for type double precision
in	line	(1e999999999999,1)	ERROR: "1e999999999999" is out of range for type double precision
in	box	(1e999999999999,1)	ERROR: "1e999999999999" is out of range for type double precision
in	polygon	(1e999999999999,1)	ERROR: "1e999999999999" is out of range for type double precision
in	circle	(1e999999999999,1)	ERROR: "1e999999999999" is out of range for type double precision
in	point	-0,-0	(-0,-0)
in	path	-0,-0	((-0,-0))
in	polygon	-0,-0	((-0,-0))
in	point	(1.7976931348623157e308,-1.7976931348623157e308)	(1.7976931348623157e+308,-1.7976931348623157e+308)
in	path	(1.7976931348623157e308,-1.7976931348623157e308)	((1.7976931348623157e+308,-1.7976931348623157e+308))
in	polygon	(1.7976931348623157e308,-1.7976931348623157e308)	((1.7976931348623157e+308,-1.7976931348623157e+308))
in	point	(1.8e308,1)	ERROR: "1.8e308" is out of range for type double precision
in	lseg	(1.8e308,1)	ERROR: "1.8e308" is out of range for type double precision
in	path	(1.8e308,1)	ERROR: "1.8e308" is out of range for type double precision
in	line	(1.8e308,1)	ERROR: "1.8e308" is out of range for type double precision
in	box	(1.8e308,1)	ERROR: "1.8e308" is out of range for type double precision
in	polygon	(1.8e308,1)	ERROR: "1.8e308" is out of range for type double precision
in	circle	(1.8e308,1)	ERROR: "1.8e308" is out of range for type double precision
in	point	(4.9e-324,1)	(5e-324,1)
in	path	(4.9e-324,1)	((5e-324,1))
in	polygon	(4.9e-324,1)	((5e-324,1))
in	point	(2.4e-324,1)	ERROR: "2.4e-324" is out of range for type double precision
in	lseg	(2.4e-324,1)	ERROR: "2.4e-324" is out of range for type double precision
in	path	(2.4e-324,1)	ERROR: "2.4e-324" is out of range for type double precision
in	line	(2.4e-324,1)	ERROR: "2.4e-324" is out of range for type double precision
in	box	(2.4e-324,1)	ERROR: "2.4e-324" is out of range for type double precision
in	polygon	(2.4e-324,1)	ERROR: "2.4e-324" is out of range for type double precision
in	circle	(2.4e-324,1)	ERROR: "2.4e-324" is out of range for type double precision
in	point	(2.5e-324,1)	(5e-324,1)
in	path	(2.5e-324,1)	((5e-324,1))
in	polygon	(2.5e-324,1)	((5e-324,1))
in	point	( 1.5e+3 , -2.5E-3 )	(1500,-0.0025)
in	path	( 1.5e+3 , -2.5E-3 )	((1500,-0.0025))
in	polygon	( 1.5e+3 , -2.5E-3 )	((1500,-0.0025))
in	point	(00001,0002.000)	(1,2)
in	path	(00001,0002.000)	((1,2))
in	polygon	(00001,0002.000)	((1,2))
in	path	(1,2),(3,4),(5,6),(7,8),(9,10)	((1,2),(3,4),(5,6),(7,8),(9,10))
in	polygon	(1,2),(3,4),(5,6),(7,8),(9,10)	((1,2),(3,4),(5,6),(7,8),(9,10))
in	path	(1,2,3,4,5,6)	((1,2),(3,4),(5,6))
in	polygon	(1,2,3,4,5,6)	((1,2),(3,4),(5,6))
in	circle	<(1,2),3) 	<(1,2),3>
in	lseg	(2,2),(0,0),	[(2,2),(0,0)]
in	line	(2,2),(0,0),	{1,-1,0}
in	box	(2,2),(0,0),	(2,2),(0,0)
in	circle	<(0,0),1e400>	ERROR: "1e400" is out of range for type double precision
in	circle	<(0,0),1e-400>	ERROR: "1e-400" is out of range for type double precision
in	circle	<(0,0),NaN>	<(0,0),NaN>
in	circle	<(0,0),-NaN>	<(0,0),NaN>
in	circle	<(NaN,-0),0>	<(NaN,-0),0>
in	point	(0.1,0.2)	(0.1,0.2)
in	path	(0.1,0.2)	((0.1,0.2))
in	polygon	(0.1,0.2)	((0.1,0.2))
in	point	(0.30000000000000004,1e-5)	(0.30000000000000004,1e-05)
in	path	(0.30000000000000004,1e-5)	((0.30000000000000004,1e-05))
in	polygon	(0.30000000000000004,1e-5)	((0.30000000000000004,1e-05))
in	point	(123456789012345678,1e16)	(1.2345678901234568e+17,1e+16)
in	path	(123456789012345678,1e16)	((1.2345678901234568e+17,1e+16))
in	polygon	(123456789012345678,1e16)	((1.2345678901234568e+17,1e+16))
in	point	(1e15,1e-5)	(1e+15,1e-05)
in	path	(1e15,1e-5)	((1e+15,1e-05))
in	polygon	(1e15,1e-5)	((1e+15,1e-05))
in	point	(0.0001,0.00001)	(0.0001,1e-05)
in	path	(0.0001,0.00001)	((0.0001,1e-05))
in	polygon	(0.0001,0.00001)	((0.0001,1e-05))
in	lseg	(0,0)	ERROR: invalid input syntax for type lseg: "(0,0)"
in	line	(0,0)	ERROR: invalid input syntax for type line: "(0,0)"
in	box	(0,0)	ERROR: invalid input syntax for type box: "(0,0)"
in	circle	(0,0)	ERROR: invalid input syntax for type circle: "(0,0)"
in	lseg	(-0,-0)	ERROR: invalid input syntax for type lseg: "(-0,-0)"
in	line	(-0,-0)	ERROR: invalid input syntax for type line: "(-0,-0)"
in	box	(-0,-0)	ERROR: invalid input syntax for type box: "(-0,-0)"
in	circle	(-0,-0)	ERROR: invalid input syntax for type circle: "(-0,-0)"
in	point	[(1,2),(3,4)]	ERROR: invalid input syntax for type point: "[(1,2),(3,4)]"
in	box	[(1,2),(3,4)]	ERROR: invalid input syntax for type box: "[(1,2),(3,4)]"
in	polygon	[(1,2),(3,4)]	ERROR: invalid input syntax for type polygon: "[(1,2),(3,4)]"
in	circle	[(1,2),(3,4)]	ERROR: invalid input syntax for type circle: "[(1,2),(3,4)]"
in	point	[(0,0),(0,0)]	ERROR: invalid input syntax for type point: "[(0,0),(0,0)]"
in	box	[(0,0),(0,0)]	ERROR: invalid input syntax for type box: "[(0,0),(0,0)]"
in	polygon	[(0,0),(0,0)]	ERROR: invalid input syntax for type polygon: "[(0,0),(0,0)]"
in	circle	[(0,0),(0,0)]	ERROR: invalid input syntax for type circle: "[(0,0),(0,0)]"
in	point	((1,2),(3,4))	ERROR: invalid input syntax for type point: "((1,2),(3,4))"
in	circle	((1,2),(3,4))	ERROR: invalid input syntax for type circle: "((1,2),(3,4))"
in	point	[(0,0)]	ERROR: invalid input syntax for type point: "[(0,0)]"
in	lseg	[(0,0)]	ERROR: invalid input syntax for type lseg: "[(0,0)]"
in	line	[(0,0)]	ERROR: invalid input syntax for type line: "[(0,0)]"
in	box	[(0,0)]	ERROR: invalid input syntax for type box: "[(0,0)]"
in	polygon	[(0,0)]	ERROR: invalid input syntax for type polygon: "[(0,0)]"
in	circle	[(0,0)]	ERROR: invalid input syntax for type circle: "[(0,0)]"
in	point	((0,0))	ERROR: invalid input syntax for type point: "((0,0))"
in	lseg	((0,0))	ERROR: invalid input syntax for type lseg: "((0,0))"
in	line	((0,0))	ERROR: invalid input syntax for type line: "((0,0))"
in	box	((0,0))	ERROR: invalid input syntax for type box: "((0,0))"
in	point	<(1,2),3>	ERROR: invalid input syntax for type point: "<(1,2),3>"
in	lseg	<(1,2),3>	ERROR: invalid input syntax for type lseg: "<(1,2),3>"
in	path	<(1,2),3>	ERROR: invalid input syntax for type path: "<(1,2),3>"
in	line	<(1,2),3>	ERROR: invalid input syntax for type line: "<(1,2),3>"
in	polygon	<(1,2),3>	ERROR: invalid input syntax for type polygon: "<(1,2),3>"
in	lseg	{0,0,1}	ERROR: invalid input syntax for type lseg: "{0,0,1}"
in	path	{0,0,1}	ERROR: invalid input syntax for type path: "{0,0,1}"
in	polygon	{0,0,1}	ERROR: invalid input syntax for type polygon: "{0,0,1}"
in	path	{0,0,1} x	ERROR: invalid input syntax for type path: "{0,0,1} x"
in	line	{0,0,1} x	ERROR: invalid input syntax for type line: "{0,0,1} x"
in	polygon	{0,0,1} x	ERROR: invalid input syntax for type polygon: "{0,0,1} x"
in	path	1,	ERROR: invalid input syntax for type path: "1,"
in	path	 ( 100 , 200 ) , 10 	ERROR: invalid input syntax for type path: " ( 100 , 200 ) , 10 "
in	path	 ( ( 1 , 2 ) , 3 ) 	ERROR: invalid input syntax for type path: " ( ( 1 , 2 ) , 3 ) "
cast	point	(0,0)	box	(0,0),(0,0)
cast	point	(1,2)	box	(1,2),(1,2)
cast	point	(-3,4)	box	(-3,4),(-3,4)
un	?-	lseg	[(1,2),(3,4)]	f
un	?-	lseg	[(0,-20),(30,-20)]	t
un	?|	lseg	[(1,2),(3,4)]	f
un	?|	lseg	[(-10,2),(-10,3)]	t
un	@-@	lseg	[(1,2),(3,4)]	2.8284271247461903
un	@-@	lseg	[(0,0),(6,6)]	8.48528137423857
un	@-@	lseg	[(10,-10),(-3,-4)]	14.317821063276353
un	@@	lseg	[(1,2),(3,4)]	(2,3)
un	@@	lseg	[(0,0),(6,6)]	(3,3)
un	@@	lseg	[(10,-10),(-3,-4)]	(3.5,-7)
un	@@	lseg	[(-1.6404668118148713,0),(1.0000005,4.9e-324)]	ERROR: value out of range: underflow
cast	lseg	[(1,2),(3,4)]	point	(2,3)
cast	lseg	[(0,0),(6,6)]	point	(3,3)
cast	lseg	[(10,-10),(-3,-4)]	point	(3.5,-7)
cast	lseg	[(-1.6404668118148713,0),(1.0000005,4.9e-324)]	point	ERROR: value out of range: underflow
un	#	path	[(1,2),(3,4)]	2
un	#	path	[(0,0),(3,0),(4,5),(1,6)]	4
un	#	path	[(0,0)]	1
un	@-@	path	[(1,2),(3,4)]	2.8284271247461903
un	@-@	path	((1,2),(3,4))	5.656854249492381
un	@-@	path	[(0,0),(3,0),(4,5),(1,6)]	11.261297173761164
cast	path	[(1,2),(3,4)]	polygon	ERROR: open path cannot be converted to polygon
cast	path	((1,2),(3,4))	polygon	((1,2),(3,4))
cast	path	((0,0))	polygon	((0,0))
cast	path	((0,0),(1,1),(1,0))	polygon	((0,0),(1,1),(1,0))
un	?-	line	{0,-1,5}	t
un	?-	line	{1,0,5}	f
un	?|	line	{0,-1,5}	f
un	?|	line	{1,0,5}	t
un	@@	box	(2,2),(0,0)	(1,1)
un	@@	box	(3,3),(1,1)	(2,2)
un	@@	box	(-2,2),(-8,-10)	(-5,-4)
un	@@	box	(-3,0),(-2.5,4.9e-324)	ERROR: value out of range: underflow
cast	box	(2,2),(0,0)	point	(1,1)
cast	box	(3,3),(1,1)	point	(2,2)
cast	box	(-2,2),(-8,-10)	point	(-5,-4)
cast	box	(-3,0),(-2.5,4.9e-324)	point	ERROR: value out of range: underflow
cast	box	(2,2),(0,0)	lseg	[(2,2),(0,0)]
cast	box	(3,3),(1,1)	lseg	[(3,3),(1,1)]
cast	box	(-2,2),(-8,-10)	lseg	[(-2,2),(-8,-10)]
cast	box	(2,2),(0,0)	polygon	((0,0),(0,2),(2,2),(2,0))
cast	box	(3,3),(1,1)	polygon	((1,1),(1,3),(3,3),(3,1))
cast	box	(-2,2),(-8,-10)	polygon	((-8,-10),(-8,2),(-2,2),(-2,-10))
cast	box	(2,2),(0,0)	circle	<(1,1),1.4142135623730951>
cast	box	(3,3),(1,1)	circle	<(2,2),1.4142135623730951>
cast	box	(-2,2),(-8,-10)	circle	<(-5,-4),6.708203932499369>
cast	box	(-3,0),(-2.5,4.9e-324)	circle	ERROR: value out of range: underflow
un	#	polygon	(2,0),(2,4),(0,0)	3
un	#	polygon	(1,2),(3,4),(5,6),(7,8)	4
un	#	polygon	(0,0)	1
un	@@	polygon	(2,0),(2,4),(0,0)	(1.3333333333333333,1.3333333333333333)
un	@@	polygon	(3,1),(3,3),(1,0)	(2.3333333333333335,1.3333333333333333)
un	@@	polygon	(1,2),(3,4),(5,6),(7,8)	(4,5)
cast	polygon	(2,0),(2,4),(0,0)	point	(1.3333333333333333,1.3333333333333333)
cast	polygon	(3,1),(3,3),(1,0)	point	(2.3333333333333335,1.3333333333333333)
cast	polygon	(1,2),(3,4),(5,6),(7,8)	point	(4,5)
cast	polygon	(2,0),(2,4),(0,0)	path	((2,0),(2,4),(0,0))
cast	polygon	(3,1),(3,3),(1,0)	path	((3,1),(3,3),(1,0))
cast	polygon	(1,2),(3,4),(5,6),(7,8)	path	((1,2),(3,4),(5,6),(7,8))
cast	polygon	(2,0),(2,4),(0,0)	box	(2,4),(0,0)
cast	polygon	(3,1),(3,3),(1,0)	box	(3,3),(1,0)
cast	polygon	(1,2),(3,4),(5,6),(7,8)	box	(7,8),(1,2)
cast	polygon	(2,0),(2,4),(0,0)	circle	<(1.3333333333333333,1.3333333333333333),2.0416890506363647>
cast	polygon	(3,1),(3,3),(1,0)	circle	<(2.3333333333333335,1.3333333333333333),1.4753430037918527>
cast	polygon	(1,2),(3,4),(5,6),(7,8)	circle	<(4,5),2.82842712474619>
un	@@	circle	<(5,1),3>	(5,1)
un	@@	circle	<(1,2),100>	(1,2)
un	@@	circle	<(1,3),5>	(1,3)
cast	circle	<(5,1),3>	point	(5,1)
cast	circle	<(1,2),100>	point	(1,2)
cast	circle	<(1,3),5>	point	(1,3)
cast	circle	<(5,1),3>	box	(7.121320343559642,3.1213203435596424),(2.8786796564403576,-1.1213203435596424)
cast	circle	<(1,2),100>	box	(71.71067811865474,72.71067811865474),(-69.71067811865474,-68.71067811865474)
cast	circle	<(1,3),5>	box	(4.535533905932738,6.535533905932738),(-2.5355339059327373,-0.5355339059327373)
cast	circle	<(5,1),3>	polygon	((2,1),(2.401923788646684,2.5),(3.4999999999999996,3.598076211353316),(5,4),(6.499999999999999,3.598076211353316),(7.598076211353316,2.500000000000001),(8,1.0000000000000004),(7.598076211353316,-0.4999999999999991),(6.500000000000002,-1.5980762113533151),(5.000000000000001,-2),(3.5000000000000018,-1.598076211353317),(2.401923788646685,-0.5000000000000013))
cast	circle	<(1,2),100>	polygon	((-99,2),(-85.60254037844388,51.99999999999999),(-49.000000000000014,88.60254037844386),(0.9999999999999939,102),(50.99999999999998,88.60254037844388),(87.60254037844385,52.000000000000036),(101,2.0000000000000124),(87.60254037844388,-47.99999999999997),(51.00000000000004,-84.60254037844383),(1.0000000000000184,-98),(-48.999999999999936,-84.6025403784439),(-85.60254037844383,-48.00000000000004))
cast	circle	<(1,3),5>	polygon	((-4,3),(-3.3301270189221936,5.5),(-1.5000000000000004,7.330127018922193),(0.9999999999999997,8),(3.499999999999999,7.330127018922194),(5.330127018922193,5.500000000000002),(6,3.0000000000000004),(5.3301270189221945,0.5000000000000013),(3.500000000000002,-1.3301270189221919),(1.0000000000000009,-2),(-1.4999999999999964,-1.3301270189221954),(-3.330127018922192,0.4999999999999978))
cast	circle	<(3,5),0>	polygon	ERROR: cannot convert circle with radius zero to polygon
bin	#	lseg	[(1,2),(3,4)]	lseg	[(1,2),(3,4)]	NULL
bin	#	lseg	[(1,2),(3,4)]	lseg	[(1,0),(1.0000001,5)]	(1,2)
bin	#	lseg	[(0,0),(6,6)]	lseg	[(0,0),(0,0)]	(0,0)
bin	#	lseg	[(0,0),(6,6)]	lseg	[(1,0),(1.0000001,5)]	(1,1)
bin	#	lseg	[(1e-300,Infinity),(-3,1)]	lseg	[(1e300,-2),(0,0.7444622094743369)]	ERROR: value out of range: underflow
bin	#	line	{0,-1,5}	line	{0,-1,5}	NULL
bin	#	line	{0,-1,5}	line	{1,0,5}	(-5,5)
bin	#	line	{0,-1,5}	line	{1,-1,0}	(5,5)
bin	#	line	{0,-1,5}	line	{-0.4,-1,-6}	(-27.5,5)
bin	#	line	{0,1e300,-1}	line	{-1e300,0.5,0}	ERROR: value out of range: overflow
bin	#	box	(2,2),(0,0)	box	(2,2),(0,0)	(2,2),(0,0)
bin	#	box	(2,2),(0,0)	box	(3,3),(1,1)	(2,2),(1,1)
bin	#	box	(2,2),(0,0)	box	(-2,2),(-8,-10)	NULL
bin	#	box	(2,2),(0,0)	box	(2,0.5),(0,0)	(2,0.5),(0,0)
bin	##	point	(0,0)	lseg	[(1,2),(3,4)]	(1,2)
bin	##	point	(0,0)	lseg	[(0,0),(6,6)]	(0,0)
bin	##	point	(0,0)	lseg	[(10,-10),(-3,-4)]	(-2.048780487804878,-4.439024390243902)
bin	##	point	(0,0)	lseg	[(NaN,1),(NaN,90)]	NULL
bin	##	point	(1e+300,Inf)	lseg	[(-1,-1),(1e10,1e-10)]	ERROR: value out of range: overflow
bin	##	point	(0,0)	line	{0,-1,5}	(0,5)
bin	##	point	(0,0)	line	{1,0,5}	(-5,0)
bin	##	point	(0,0)	line	{0,3,0}	(0,0)
bin	##	point	(0,0)	line	{3,NaN,5}	NULL
bin	##	point	(1e-300,-1e-300)	line	{1e-300,1,0}	ERROR: value out of range: underflow
bin	##	point	(0,0)	box	(2,2),(0,0)	(0,0)
bin	##	point	(0,0)	box	(3,3),(1,1)	(1,1)
bin	##	point	(0,0)	box	(-2,2),(-8,-10)	(-2,0)
bin	##	point	(0,0)	box	(NaN,NaN),(0,0)	NULL
bin	##	lseg	[(1,2),(3,4)]	lseg	[(1,2),(3,4)]	NULL
bin	##	lseg	[(1,2),(3,4)]	lseg	[(10,-10),(-3,-4)]	(-1.9853658536585364,-4.468292682926829)
bin	##	lseg	[(1,2),(3,4)]	lseg	[(-1e6,2e2),(3e5,-4e1)]	(3.002101672827777,15.384061150460402)
bin	##	lseg	[(1,2),(3,4)]	lseg	[(-10,2),(-10,3)]	(-10,2)
bin	##	lseg	[(0,-1),(4.9e-324,3)]	lseg	[(0.5,-3),(0,-2)]	ERROR: value out of range: underflow
bin	##	lseg	[(1,2),(3,4)]	box	(2,2),(0,0)	(1,2)
bin	##	lseg	[(1,2),(3,4)]	box	(3,3),(1,1)	(1.5,2.5)
bin	##	lseg	[(1,2),(3,4)]	box	(-2,2),(-8,-10)	(-2,2)
bin	##	lseg	[(NaN,1),(NaN,90)]	box	(2,2),(0,0)	NULL
bin	##	lseg	[(4.9e-324,-2.4668766867074376),(-3,-1)]	box	(-2.534120155423725,-3),(2,3)	ERROR: value out of range: underflow
bin	##	line	{0,-1,5}	lseg	[(1,2),(3,4)]	(3,4)
bin	##	line	{0,-1,5}	lseg	[(0,0),(6,6)]	(5,5)
bin	##	line	{0,-1,5}	lseg	[(10,-10),(-3,-4)]	(-3,-4)
bin	##	line	{0,-1,5}	lseg	[(0,-20),(30,-20)]	NULL
bin	##	line	{-1e300,-3,2}	lseg	[(-3.8307492646360464,-2.5),(-2.5,-1)]	ERROR: value out of range: underflow
bin	&&	box	(2,2),(0,0)	box	(2,2),(0,0)	t
bin	&&	box	(2,2),(0,0)	box	(-2,2),(-8,-10)	f
bin	&&	polygon	(2,0),(2,4),(0,0)	polygon	(2,0),(2,4),(0,0)	t
bin	&&	polygon	(2,0),(2,4),(0,0)	polygon	(0,1),(0,1)	f
bin	&&	polygon	(1,2),(3,4),(5,6),(7,8)	polygon	((-1e300,0),(0,1e300),(1e300,0))	ERROR: value out of range: overflow
bin	&&	circle	<(5,1),3>	circle	<(5,1),3>	t
bin	&&	circle	<(5,1),3>	circle	<(100,200),10>	f
bin	&<	box	(2,2),(0,0)	box	(2,2),(0,0)	t
bin	&<	box	(2,2),(0,0)	box	(-2,2),(-8,-10)	f
bin	&<	polygon	(2,0),(2,4),(0,0)	polygon	(2,0),(2,4),(0,0)	t
bin	&<	polygon	(2,0),(2,4),(0,0)	polygon	(0,0)	f
bin	&<	circle	<(5,1),3>	circle	<(5,1),3>	t
bin	&<	circle	<(5,1),3>	circle	<(1,3),5>	f
bin	&<|	box	(2,2),(0,0)	box	(2,2),(0,0)	t
bin	&<|	box	(2,2),(0,0)	box	(2,0.5),(0,0)	f
bin	&<|	polygon	(2,0),(2,4),(0,0)	polygon	(2,0),(2,4),(0,0)	t
bin	&<|	polygon	(2,0),(2,4),(0,0)	polygon	(3,1),(3,3),(1,0)	f
bin	&<|	circle	<(5,1),3>	circle	<(5,1),3>	t
bin	&<|	circle	<(5,1),3>	circle	<(3,5),NaN>	f
bin	&>	box	(2,2),(0,0)	box	(2,2),(0,0)	t
bin	&>	box	(2,2),(0,0)	box	(3,3),(1,1)	f
bin	&>	polygon	(2,0),(2,4),(0,0)	polygon	(2,0),(2,4),(0,0)	t
bin	&>	polygon	(2,0),(2,4),(0,0)	polygon	(3,1),(3,3),(1,0)	f
bin	&>	circle	<(5,1),3>	circle	<(5,1),3>	t
bin	&>	circle	<(5,1),3>	circle	<(100,200),10>	f
bin	*	point	(0,0)	point	(0,0)	(0,0)
bin	*	point	(0,0)	point	(-3,4)	(-0,0)
bin	*	point	(0,0)	point	(-5,-12)	(0,-0)
bin	*	point	(1e-300,-1e-300)	point	(1e-300,-1e-300)	ERROR: value out of range: underflow
bin	*	path	[(1,2),(3,4)]	point	(0,0)	[(0,0),(0,0)]
bin	*	path	[(1,2),(3,4)]	point	(1,2)	[(-3,4),(-5,10)]
bin	*	path	[(1,2),(3,4)]	point	(-3,4)	[(-11,-2),(-25,0)]
bin	*	path	[(-1e300,0),(1e300,0)]	point	(1e+300,Inf)	ERROR: value out of range: overflow
bin	*	box	(2,2),(0,0)	point	(0,0)	(0,0),(0,0)
bin	*	box	(2,2),(0,0)	point	(1,2)	(0,6),(-2,0)
bin	*	box	(2,2),(0,0)	point	(-3,4)	(-0,2),(-14,0)
bin	*	box	(1e300,1e300),(-1e300,-1e300)	point	(1e+300,Inf)	ERROR: value out of range: overflow
bin	*	circle	<(5,1),3>	point	(0,0)	<(0,0),0>
bin	*	circle	<(5,1),3>	point	(1,2)	<(3,11),6.708203932499369>
bin	*	circle	<(5,1),3>	point	(-3,4)	<(-19,17),15>
bin	*	circle	<(0,0),1e300>	point	(1e300,1e300)	ERROR: value out of range: overflow
bin	+	point	(0,0)	point	(0,0)	(0,0)
bin	+	point	(0,0)	point	(1,2)	(1,2)
bin	+	point	(0,0)	point	(-3,4)	(-3,4)
bin	+	path	[(1,2),(3,4)]	point	(0,0)	[(1,2),(3,4)]
bin	+	path	[(1,2),(3,4)]	point	(1,2)	[(2,4),(4,6)]
bin	+	path	[(1,2),(3,4)]	point	(-3,4)	[(-2,6),(0,8)]
bin	+	path	[(1,2),(3,4)]	path	[(1,2),(3,4)]	[(1,2),(3,4),(1,2),(3,4)]
bin	+	path	[(1,2),(3,4)]	path	((1,2),(3,4))	NULL
bin	+	path	[(1,2),(3,4)]	path	[(0,0),(3,0),(4,5),(1,6)]	[(1,2),(3,4),(0,0),(3,0),(4,5),(1,6)]
bin	+	path	[(1,2),(3,4)]	path	[(0,0)]	[(1,2),(3,4),(0,0)]
bin	+	box	(2,2),(0,0)	point	(0,0)	(2,2),(0,0)
bin	+	box	(2,2),(0,0)	point	(1,2)	(3,4),(1,2)
bin	+	box	(2,2),(0,0)	point	(-3,4)	(-1,6),(-3,4)
bin	+	circle	<(5,1),3>	point	(0,0)	<(5,1),3>
bin	+	circle	<(5,1),3>	point	(1,2)	<(6,3),3>
bin	+	circle	<(5,1),3>	point	(-3,4)	<(2,5),3>
bin	-	point	(0,0)	point	(0,0)	(0,0)
bin	-	point	(0,0)	point	(1,2)	(-1,-2)
bin	-	point	(0,0)	point	(-3,4)	(3,-4)
bin	-	path	[(1,2),(3,4)]	point	(0,0)	[(1,2),(3,4)]
bin	-	path	[(1,2),(3,4)]	point	(1,2)	[(0,0),(2,2)]
bin	-	path	[(1,2),(3,4)]	point	(-3,4)	[(4,-2),(6,0)]
bin	-	box	(2,2),(0,0)	point	(0,0)	(2,2),(0,0)
bin	-	box	(2,2),(0,0)	point	(1,2)	(1,0),(-1,-2)
bin	-	box	(2,2),(0,0)	point	(-3,4)	(5,-2),(3,-4)
bin	-	circle	<(5,1),3>	point	(0,0)	<(5,1),3>
bin	-	circle	<(5,1),3>	point	(1,2)	<(4,-1),3>
bin	-	circle	<(5,1),3>	point	(-3,4)	<(8,-3),3>
bin	/	point	(0,0)	point	(0,0)	ERROR: division by zero
bin	/	point	(0,0)	point	(1,2)	(0,0)
bin	/	point	(0,0)	point	(-3,4)	(0,-0)
bin	/	point	(0,0)	point	(-5,-12)	(-0,0)
bin	/	path	[(1,2),(3,4)]	point	(0,0)	ERROR: division by zero
bin	/	path	[(1,2),(3,4)]	point	(1,2)	[(1,0),(2.2,-0.4)]
bin	/	path	[(1,2),(3,4)]	point	(-3,4)	[(0.2,-0.4),(0.28,-0.96)]
bin	/	path	[(1,2),(3,4)]	point	(5.1,34.5)	[(0.06092447338562478,-0.019979280745893148),(0.12604212915001728,-0.06832420699521484)]
bin	/	box	(2,2),(0,0)	point	(0,0)	ERROR: division by zero
bin	/	box	(2,2),(0,0)	point	(1,2)	(1.2,0),(0,-0.4)
bin	/	box	(2,2),(0,0)	point	(-3,4)	(0.08,-0),(0,-0.56)
bin	/	box	(2,2),(0,0)	point	(5.1,34.5)	(0.06511765576439249,0),(0,-0.04834492624932169)
bin	/	circle	<(5,1),3>	point	(0,0)	ERROR: division by zero
bin	/	circle	<(5,1),3>	point	(1,2)	<(1.4,-1.8),1.3416407864998738>
bin	/	circle	<(5,1),3>	point	(-3,4)	<(-0.44,-0.92),0.6>
bin	/	circle	<(5,1),3>	point	(5.1,34.5)	<(0.04933155739726703,-0.13763504513837502),0.08602170429368425>
bin	<	lseg	[(1,2),(3,4)]	lseg	[(1,2),(3,4)]	f
bin	<	lseg	[(1,2),(3,4)]	lseg	[(0,0),(6,6)]	t
bin	<	path	[(1,2),(3,4)]	path	[(1,2),(3,4)]	f
bin	<	path	[(1,2),(3,4)]	path	[(0,0),(3,0),(4,5),(1,6)]	t
bin	<	box	(2,2),(0,0)	box	(2,2),(0,0)	f
bin	<	box	(2,2),(0,0)	box	(-2,2),(-8,-10)	t
bin	<	box	(2,2),(0,0)	box	(1e300,1e300),(-1e300,-1e300)	ERROR: value out of range: overflow
bin	<	circle	<(5,1),3>	circle	<(5,1),3>	f
bin	<	circle	<(5,1),3>	circle	<(1,2),100>	t
bin	<	circle	<(5,1),3>	circle	<(0,0),1e300>	ERROR: value out of range: overflow
bin	<->	point	(0,0)	point	(0,0)	0
bin	<->	point	(0,0)	point	(1,2)	2.23606797749979
bin	<->	point	(0,0)	point	(-3,4)	5
bin	<->	point	(0,0)	lseg	[(1,2),(3,4)]	2.23606797749979
bin	<->	point	(0,0)	lseg	[(0,0),(6,6)]	0
bin	<->	point	(0,0)	lseg	[(10,-10),(-3,-4)]	4.8890120703870465
bin	<->	point	(1e+300,Inf)	lseg	[(-1,-1),(1e10,1e-10)]	ERROR: value out of range: overflow
bin	<->	point	(0,0)	path	[(1,2),(3,4)]	2.23606797749979
bin	<->	point	(0,0)	path	[(0,0),(3,0),(4,5),(1,6)]	0
bin	<->	point	(0,0)	path	[(11,12),(13,14)]	16.278820596099706
bin	<->	point	(1e300,3)	path	((-7.464351073823465,0),(1e200,-1),(-3,Infinity),(1e-7,0),(1.5,-3),(-2.0532258254273428,0))	ERROR: value out of range: overflow
bin	<->	point	(0,0)	line	{0,-1,5}	5
bin	<->	point	(0,0)	line	{0,3,0}	0
bin	<->	point	(0,0)	line	{-0.4,-1,-6}	5.570860145311556
bin	<->	point	(1e-300,-1e-300)	line	{1e-300,1,0}	ERROR: value out of range: underflow
bin	<->	point	(0,0)	box	(2,2),(0,0)	0
bin	<->	point	(0,0)	box	(3,3),(1,1)	1.4142135623730951
bin	<->	point	(0,0)	box	(-2,2),(-8,-10)	2
bin	<->	point	(0,0)	polygon	(2,0),(2,4),(0,0)	0
bin	<->	point	(0,0)	polygon	(3,1),(3,3),(1,0)	1
bin	<->	point	(0,0)	polygon	(1,2),(3,4),(5,6),(7,8)	2.23606797749979
bin	<->	point	(1,2)	polygon	((-1e300,0),(0,1e300),(1e300,0))	ERROR: value out of range: overflow
bin	<->	point	(0,0)	circle	<(5,1),3>	2.0990195135927845
bin	<->	point	(0,0)	circle	<(1,2),100>	0
bin	<->	point	(0,0)	circle	<(100,200),10>	213.60679774997897
bin	<->	lseg	[(1,2),(3,4)]	point	(0,0)	2.23606797749979
bin	<->	lseg	[(1,2),(3,4)]	point	(1,2)	0
bin	<->	lseg	[(1,2),(3,4)]	point	(-3,4)	4.47213595499958
bin	<->	lseg	[(-1,-1),(1e10,1e-10)]	point	(1e+300,Inf)	ERROR: value out of range: overflow
bin	<->	lseg	[(1,2),(3,4)]	lseg	[(1,2),(3,4)]	0
bin	<->	lseg	[(1,2),(3,4)]	lseg	[(0,0),(6,6)]	0.7071067811865476
bin	<->	lseg	[(1,2),(3,4)]	lseg	[(10,-10),(-3,-4)]	7.1239890168496975
bin	<->	lseg	[(-0.3800093713913473,1e200),(2,0.5)]	lseg	[(-1,-1),(1e-200,3)]	ERROR: value out of range: underflow
bin	<->	lseg	[(1,2),(3,4)]	line	{0,-1,5}	1
bin	<->	lseg	[(1,2),(3,4)]	line	{1,0,5}	6
bin	<->	lseg	[(1,2),(3,4)]	line	{0,3,0}	2
bin	<->	lseg	[(7.2568339259686105,-2.193609324556096),(-2.478254821718311,-4.225425622342003)]	line	{-2,1e200,-2.5}	ERROR: value out of range: overflow
bin	<->	lseg	[(1,2),(3,4)]	box	(2,2),(0,0)	0
bin	<->	lseg	[(1,2),(3,4)]	box	(-2,2),(-8,-10)	3
bin	<->	lseg	[(1,2),(3,4)]	box	(3,3),(3,3)	0.7071067811865476
bin	<->	lseg	[(1e-200,-1e-6),(1e200,2)]	box	(0.5,-3),(0.5,1e-6)	ERROR: value out of range: underflow
bin	<->	path	[(1,2),(3,4)]	point	(0,0)	2.23606797749979
bin	<->	path	[(1,2),(3,4)]	point	(1,2)	0
bin	<->	path	[(1,2),(3,4)]	point	(-3,4)	4.47213595499958
bin	<->	path	((1e300,-7.149127284737102),(0,0),(-1,-2),(-3,0),(1,0.999999),(-2.8798012149610708,1e-7))	point	(-1e300,3)	ERROR: value out of range: overflow
bin	<->	path	[(1,2),(3,4)]	path	[(1,2),(3,4)]	0
bin	<->	path	[(1,2),(3,4)]	path	[(0,0),(3,0),(4,5),(1,6)]	0.784464540552736
bin	<->	path	[(1,2),(3,4)]	path	[(0,0)]	NULL
bin	<->	path	[(1,2),(3,4)]	path	((0,0))	2.23606797749979
bin	<->	path	[(6.784519199182196,-1.5),(1,0),(1e300,-0.5),(-3,1e200)]	path	((1e-7,-2.5),(2,8.930420444982069),(0,-3),(-7.572096899744167,-2.02809644334518))	ERROR: value out of range: underflow
bin	<->	line	{0,-1,5}	point	(0,0)	5
bin	<->	line	{0,-1,5}	point	(1,2)	3
bin	<->	line	{0,-1,5}	point	(-3,4)	1
bin	<->	line	{1e-300,1,0}	point	(1e-300,-1e-300)	ERROR: value out of range: underflow
bin	<->	line	{0,-1,5}	lseg	[(1,2),(3,4)]	1
bin	<->	line	{0,-1,5}	lseg	[(0,0),(6,6)]	0
bin	<->	line	{0,-1,5}	lseg	[(10,-10),(-3,-4)]	9
bin	<->	line	{1e300,1,2}	lseg	[(1.865595766507834,2),(3.2174263955884648,1e200)]	ERROR: value out of range: underflow
bin	<->	line	{0,-1,5}	line	{0,-1,5}	0
bin	<->	line	{0,-1,5}	line	{0,3,0}	5
bin	<->	line	{0,-1,5}	line	{0,-1,3}	2
bin	<->	box	(2,2),(0,0)	point	(0,0)	0
bin	<->	box	(2,2),(0,0)	point	(-3,4)	3.605551275463989
bin	<->	box	(2,2),(0,0)	point	(5.1,34.5)	32.64751139060985
bin	<->	box	(2,2),(0,0)	lseg	[(1,2),(3,4)]	0
bin	<->	box	(2,2),(0,0)	lseg	[(10,-10),(-3,-4)]	4.8890120703870465
bin	<->	box	(2,2),(0,0)	lseg	[(-1e6,2e2),(3e5,-4e1)]	13.3842459257596
bin	<->	box	(4.9e-324,2.429177999154728),(2,3)	lseg	[(-3,-1),(-2,3.5645306244271993)]	ERROR: value out of range: underflow
bin	<->	box	(2,2),(0,0)	box	(2,2),(0,0)	0
bin	<->	box	(2,2),(0,0)	box	(3,3),(1,1)	1.4142135623730951
bin	<->	box	(2,2),(0,0)	box	(-2,2),(-8,-10)	7.810249675906654
bin	<->	polygon	(2,0),(2,4),(0,0)	point	(0,0)	0
bin	<->	polygon	(2,0),(2,4),(0,0)	point	(-3,4)	4.47213595499958
bin	<->	polygon	(2,0),(2,4),(0,0)	point	(5.1,34.5)	30.657136200238927
bin	<->	polygon	((-1e300,0),(0,1e300),(1e300,0))	point	(1,2)	ERROR: value out of range: overflow
bin	<->	polygon	(2,0),(2,4),(0,0)	polygon	(2,0),(2,4),(0,0)	0
bin	<->	polygon	(2,0),(2,4),(0,0)	polygon	(0,1),(0,1)	0.4472135954999579
bin	<->	polygon	(3,1),(3,3),(1,0)	polygon	(1,2),(3,4),(5,6),(7,8)	0.7071067811865476
bin	<->	polygon	(1,2),(3,4),(5,6),(7,8)	polygon	((-1e300,0),(0,1e300),(1e300,0))	ERROR: value out of range: overflow
bin	<->	polygon	(2,0),(2,4),(0,0)	circle	<(5,1),3>	0
bin	<->	polygon	(2,0),(2,4),(0,0)	circle	<(100,200),10>	209.1346617949794
bin	<->	polygon	(2,0),(2,4),(0,0)	circle	<(3,5),0>	1.4142135623730951
bin	<->	polygon	((-1e300,0),(0,1e300),(1e300,0))	circle	<(5,1),3>	ERROR: value out of range: overflow
bin	<->	circle	<(5,1),3>	point	(0,0)	2.0990195135927845
bin	<->	circle	<(5,1),3>	point	(1,2)	1.1231056256176606
bin	<->	circle	<(5,1),3>	point	(-3,4)	5.54400374531753
bin	<->	circle	<(5,1),3>	polygon	(2,0),(2,4),(0,0)	0
bin	<->	circle	<(5,1),3>	polygon	(1,2),(3,4),(5,6),(7,8)	0.5355339059327378
bin	<->	circle	<(5,1),3>	polygon	(0,0)	2.0990195135927845
bin	<->	circle	<(5,1),3>	polygon	((-1e300,0),(0,1e300),(1e300,0))	ERROR: value out of range: overflow
bin	<->	circle	<(5,1),3>	circle	<(5,1),3>	0
bin	<->	circle	<(5,1),3>	circle	<(100,200),10>	207.5130381632796
bin	<->	circle	<(5,1),3>	circle	<(3,5),0>	1.4721359549995796
bin	<<	point	(0,0)	point	(0,0)	f
bin	<<	point	(0,0)	point	(1,2)	t
bin	<<	box	(2,2),(0,0)	box	(2,2),(0,0)	f
bin	<<	box	(2,2),(0,0)	box	(2.5,3.5),(2.5,2.5)	t
bin	<<	polygon	(2,0),(2,4),(0,0)	polygon	(2,0),(2,4),(0,0)	f
bin	<<	polygon	(0,0)	polygon	(3,1),(3,3),(1,0)	t
bin	<<	circle	<(5,1),3>	circle	<(5,1),3>	f
bin	<<	circle	<(5,1),3>	circle	<(100,200),10>	t
bin	<<|	point	(0,0)	point	(0,0)	f
bin	<<|	point	(0,0)	point	(1,2)	t
bin	<<|	box	(2,2),(0,0)	box	(2,2),(0,0)	f
bin	<<|	box	(2,2),(0,0)	box	(2.5,3.5),(2.5,2.5)	t
bin	<<|	polygon	(2,0),(2,4),(0,0)	polygon	(2,0),(2,4),(0,0)	f
bin	<<|	polygon	(0,0)	polygon	(1,2),(3,4),(5,6),(7,8)	t
bin	<<|	circle	<(5,1),3>	circle	<(5,1),3>	f
bin	<<|	circle	<(5,1),3>	circle	<(100,200),10>	t
bin	<=	lseg	[(1,2),(3,4)]	lseg	[(1,2),(3,4)]	t
bin	<=	lseg	[(1,2),(3,4)]	lseg	[(-10,2),(-10,3)]	f
bin	<=	path	[(1,2),(3,4)]	path	[(1,2),(3,4)]	t
bin	<=	path	[(1,2),(3,4)]	path	[(0,0)]	f
bin	<=	box	(2,2),(0,0)	box	(2,2),(0,0)	t
bin	<=	box	(2,2),(0,0)	box	(2.5,3.5),(2.5,2.5)	f
bin	<=	box	(2,2),(0,0)	box	(1e300,1e300),(-1e300,-1e300)	ERROR: value out of range: overflow
bin	<=	circle	<(5,1),3>	circle	<(5,1),3>	t
bin	<=	circle	<(5,1),3>	circle	<(3,5),0>	f
bin	<=	circle	<(5,1),3>	circle	<(0,0),1e300>	ERROR: value out of range: overflow
bin	<>	point	(0,0)	point	(0,0)	f
bin	<>	point	(0,0)	point	(1,2)	t
bin	<>	lseg	[(1,2),(3,4)]	lseg	[(1,2),(3,4)]	f
bin	<>	lseg	[(1,2),(3,4)]	lseg	[(0,0),(6,6)]	t
bin	<>	circle	<(5,1),3>	circle	<(5,1),3>	f
bin	<>	circle	<(5,1),3>	circle	<(1,2),100>	t
bin	<>	circle	<(5,1),3>	circle	<(0,0),1e300>	ERROR: value out of range: overflow
bin	<@	point	(0,0)	lseg	[(1,2),(3,4)]	f
bin	<@	point	(0,0)	lseg	[(0,0),(6,6)]	t
bin	<@	point	(0,0)	path	[(1,2),(3,4)]	f
bin	<@	point	(0,0)	path	[(0,0),(3,0),(4,5),(1,6)]	t
bin	<@	point	(0,0)	line	{0,-1,5}	f
bin	<@	point	(0,0)	line	{0,3,0}	t
bin	<@	point	(1e-300,-1e-300)	line	{1e-300,1,0}	ERROR: value out of range: underflow
bin	<@	point	(0,0)	box	(2,2),(0,0)	t
bin	<@	point	(0,0)	box	(3,3),(1,1)	f
bin	<@	point	(0,0)	polygon	(2,0),(2,4),(0,0)	t
bin	<@	point	(0,0)	polygon	(3,1),(3,3),(1,0)	f
bin	<@	point	(1,2)	polygon	((-1e300,0),(0,1e300),(1e300,0))	ERROR: value out of range: overflow
bin	<@	point	(0,0)	circle	<(5,1),3>	f
bin	<@	point	(0,0)	circle	<(1,2),100>	t
bin	<@	lseg	[(1,2),(3,4)]	line	{0,-1,5}	f
bin	<@	lseg	[(0,0),(6,6)]	line	{1,-1,0}	t
bin	<@	lseg	[(4.9e-324,4.339961072133127),(-1,2)]	line	{-1e-7,-2,-1.1599323332312892}	ERROR: value out of range: underflow
bin	<@	lseg	[(1,2),(3,4)]	box	(2,2),(0,0)	f
bin	<@	lseg	[(1,2),(3,4)]	box	(Infinity,Infinity),(-Infinity,-Infinity)	t
bin	<@	box	(2,2),(0,0)	box	(2,2),(0,0)	t
bin	<@	box	(2,2),(0,0)	box	(3,3),(1,1)	f
bin	<@	polygon	(2,0),(2,4),(0,0)	polygon	(2,0),(2,4),(0,0)	t
bin	<@	polygon	(2,0),(2,4),(0,0)	polygon	(3,1),(3,3),(1,0)	f
bin	<@	circle	<(5,1),3>	circle	<(5,1),3>	t
bin	<@	circle	<(5,1),3>	circle	<(1,3),5>	f
bin	<^	point	(0,0)	point	(0,0)	f
bin	<^	point	(0,0)	point	(1,2)	t
bin	<^	box	(2,2),(0,0)	box	(2,2),(0,0)	f
bin	<^	box	(2,2),(0,0)	box	(2.5,3.5),(2.5,2.5)	t
bin	=	lseg	[(1,2),(3,4)]	lseg	[(1,2),(3,4)]	t
bin	=	lseg	[(1,2),(3,4)]	lseg	[(0,0),(6,6)]	f
bin	=	path	[(1,2),(3,4)]	path	[(1,2),(3,4)]	t
bin	=	path	[(1,2),(3,4)]	path	[(0,0),(3,0),(4,5),(1,6)]	f
bin	=	line	{0,-1,5}	line	{0,-1,5}	t
bin	=	line	{0,-1,5}	line	{1,0,5}	f
bin	=	line	{4.9e-324,1.5,4.1976082484290185}	line	{3,-9.044869175647898,-1.3021076795398905}	ERROR: value out of range: underflow
bin	=	box	(2,2),(0,0)	box	(2,2),(0,0)	t
bin	=	box	(2,2),(0,0)	box	(-2,2),(-8,-10)	f
bin	=	box	(2,2),(0,0)	box	(1e300,1e300),(-1e300,-1e300)	ERROR: value out of range: overflow
bin	=	circle	<(5,1),3>	circle	<(5,1),3>	t
bin	=	circle	<(5,1),3>	circle	<(1,2),100>	f
bin	=	circle	<(5,1),3>	circle	<(0,0),1e300>	ERROR: value out of range: overflow
bin	>	lseg	[(1,2),(3,4)]	lseg	[(1,2),(3,4)]	f
bin	>	lseg	[(1,2),(3,4)]	lseg	[(-10,2),(-10,3)]	t
bin	>	path	[(1,2),(3,4)]	path	[(1,2),(3,4)]	f
bin	>	path	[(1,2),(3,4)]	path	[(0,0)]	t
bin	>	box	(2,2),(0,0)	box	(2,2),(0,0)	f
bin	>	box	(2,2),(0,0)	box	(2.5,3.5),(2.5,2.5)	t
bin	>	box	(2,2),(0,0)	box	(1e300,1e300),(-1e300,-1e300)	ERROR: value out of range: overflow
bin	>	circle	<(5,1),3>	circle	<(5,1),3>	f
bin	>	circle	<(5,1),3>	circle	<(3,5),0>	t
bin	>	circle	<(5,1),3>	circle	<(0,0),1e300>	ERROR: value out of range: overflow
bin	>=	lseg	[(1,2),(3,4)]	lseg	[(1,2),(3,4)]	t
bin	>=	lseg	[(1,2),(3,4)]	lseg	[(0,0),(6,6)]	f
bin	>=	path	[(1,2),(3,4)]	path	[(1,2),(3,4)]	t
bin	>=	path	[(1,2),(3,4)]	path	[(0,0),(3,0),(4,5),(1,6)]	f
bin	>=	box	(2,2),(0,0)	box	(2,2),(0,0)	t
bin	>=	box	(2,2),(0,0)	box	(-2,2),(-8,-10)	f
bin	>=	box	(2,2),(0,0)	box	(1e300,1e300),(-1e300,-1e300)	ERROR: value out of range: overflow
bin	>=	circle	<(5,1),3>	circle	<(5,1),3>	t
bin	>=	circle	<(5,1),3>	circle	<(1,2),100>	f
bin	>=	circle	<(5,1),3>	circle	<(0,0),1e300>	ERROR: value out of range: overflow
bin	>>	point	(0,0)	point	(0,0)	f
bin	>>	point	(0,0)	point	(-3,4)	t
bin	>>	box	(2,2),(0,0)	box	(2,2),(0,0)	f
bin	>>	box	(2,2),(0,0)	box	(-2,2),(-8,-10)	t
bin	>>	polygon	(2,0),(2,4),(0,0)	polygon	(2,0),(2,4),(0,0)	f
bin	>>	polygon	(3,1),(3,3),(1,0)	polygon	(0,0)	t
bin	>>	circle	<(5,1),3>	circle	<(5,1),3>	f
bin	>>	circle	<(5,1),3>	circle	<(0,0),1>	t
bin	>^	point	(0,0)	point	(0,0)	f
bin	>^	point	(0,0)	point	(-5,-12)	t
bin	>^	box	(2,2),(0,0)	box	(2,2),(0,0)	f
bin	>^	box	(2,2),(0,0)	box	(0,0),(-0,-0)	t
bin	?#	lseg	[(1,2),(3,4)]	lseg	[(1,2),(3,4)]	f
bin	?#	lseg	[(1,2),(3,4)]	lseg	[(1,0),(1.0000001,5)]	t
bin	?#	lseg	[(4.9e-324,1.0000001),(-3.6636405394159084,0)]	lseg	[(3,-1.5),(-1,1)]	ERROR: value out of range: underflow
bin	?#	lseg	[(1,2),(3,4)]	line	{0,-1,5}	f
bin	?#	lseg	[(1,2),(3,4)]	line	{0,-1,3}	t
bin	?#	lseg	[(2,-1e-6),(1,1e300)]	line	{-0.5079345250175429,-0,-1e300}	ERROR: value out of range: overflow
bin	?#	lseg	[(1,2),(3,4)]	box	(2,2),(0,0)	t
bin	?#	lseg	[(1,2),(3,4)]	box	(-2,2),(-8,-10)	f
bin	?#	path	[(1,2),(3,4)]	path	[(1,2),(3,4)]	f
bin	?#	path	[(1,2),(3,4)]	path	((0,0),(2,0),(2,2),(0,2))	t
bin	?#	path	[(-2,-0.5),(1e200,3)]	path	((-2,2),(-4.563561425278664,2),(1e-300,0),(1e-6,2.607137378456187),(-3,0),(2,3))	ERROR: value out of range: underflow
bin	?#	line	{0,-1,5}	line	{0,-1,5}	f
bin	?#	line	{0,-1,5}	line	{1,0,5}	t
bin	?#	line	{-4.639538410935065,-1e300,-0.3223374986750134}	line	[(-3,0),(1,-1e300)]	ERROR: value out of range: overflow
bin	?#	line	{0,-1,5}	box	(2,2),(0,0)	f
bin	?#	line	{0,-1,5}	box	(1e300,1e300),(-1e300,-1e300)	t
bin	?#	line	{-8.7952177246782,1e300,NaN}	box	(1e200,2.5),(-1,-2)	ERROR: value out of range: overflow
bin	?#	box	(2,2),(0,0)	box	(2,2),(0,0)	t
bin	?#	box	(2,2),(0,0)	box	(-2,2),(-8,-10)	f
bin	?-	point	(0,0)	point	(0,0)	t
bin	?-	point	(0,0)	point	(1,2)	f
bin	?-|	lseg	[(1,2),(3,4)]	lseg	[(1,2),(3,4)]	f
bin	?-|	lseg	[(1,2),(3,4)]	lseg	[(0,1),(1,0)]	t
bin	?-|	line	{0,-1,5}	line	{0,-1,5}	f
bin	?-|	line	{0,-1,5}	line	{1,0,5}	t
bin	?|	point	(0,0)	point	(0,0)	t
bin	?|	point	(0,0)	point	(1,2)	f
bin	?||	lseg	[(1,2),(3,4)]	lseg	[(1,2),(3,4)]	t
bin	?||	lseg	[(1,2),(3,4)]	lseg	[(10,-10),(-3,-4)]	f
bin	?||	line	{0,-1,5}	line	{0,-1,5}	t
bin	?||	line	{0,-1,5}	line	{1,0,5}	f
bin	?||	line	{4.9e-324,-2,2}	line	[(-2,-7.838809812086078),(-Infinity,-1)]	ERROR: value out of range: underflow
bin	@>	path	[(1,2),(3,4)]	point	(0,0)	f
bin	@>	path	[(1,2),(3,4)]	point	(1,2)	t
bin	@>	path	((-1.5,-9.387083322482892),(-1,0.5),(3,5.413800074487131),(-1e300,1e200),(1,0.6419566692782865),(2,1e-150))	point	(2,7.147030417106631)	ERROR: value out of range: overflow
bin	@>	box	(2,2),(0,0)	point	(0,0)	t
bin	@>	box	(2,2),(0,0)	point	(-3,4)	f
bin	@>	box	(2,2),(0,0)	box	(2,2),(0,0)	t
bin	@>	box	(2,2),(0,0)	box	(3,3),(1,1)	f
bin	@>	polygon	(2,0),(2,4),(0,0)	point	(0,0)	t
bin	@>	polygon	(2,0),(2,4),(0,0)	point	(-3,4)	f
bin	@>	polygon	((-1e300,0),(0,1e300),(1e300,0))	point	(1,2)	ERROR: value out of range: overflow
bin	@>	polygon	(2,0),(2,4),(0,0)	polygon	(2,0),(2,4),(0,0)	t
bin	@>	polygon	(2,0),(2,4),(0,0)	polygon	(3,1),(3,3),(1,0)	f
bin	@>	circle	<(5,1),3>	point	(0,0)	f
bin	@>	circle	<(5,1),3>	point	(3,0)	t
bin	@>	circle	<(5,1),3>	circle	<(5,1),3>	t
bin	@>	circle	<(5,1),3>	circle	<(1,2),100>	f
bin	|&>	box	(2,2),(0,0)	box	(2,2),(0,0)	t
bin	|&>	box	(2,2),(0,0)	box	(3,3),(1,1)	f
bin	|&>	polygon	(2,0),(2,4),(0,0)	polygon	(2,0),(2,4),(0,0)	t
bin	|&>	polygon	(2,0),(2,4),(0,0)	polygon	(1,2),(3,4),(5,6),(7,8)	f
bin	|&>	circle	<(5,1),3>	circle	<(5,1),3>	t
bin	|&>	circle	<(5,1),3>	circle	<(1,2),3>	f
bin	|>>	point	(0,0)	point	(0,0)	f
bin	|>>	point	(0,0)	point	(-5,-12)	t
bin	|>>	box	(2,2),(0,0)	box	(2,2),(0,0)	f
bin	|>>	box	(3,3),(1,1)	box	(2,0.5),(0,0)	t
bin	|>>	polygon	(2,0),(2,4),(0,0)	polygon	(2,0),(2,4),(0,0)	f
bin	|>>	polygon	(1,2),(3,4),(5,6),(7,8)	polygon	(0,0)	t
bin	|>>	circle	<(5,1),3>	circle	<(5,1),3>	f
bin	|>>	circle	<(5,1),3>	circle	<(1.5,-2.5),1e-7>	t
bin	~=	point	(0,0)	point	(0,0)	t
bin	~=	point	(0,0)	point	(1,2)	f
bin	~=	box	(2,2),(0,0)	box	(2,2),(0,0)	t
bin	~=	box	(2,2),(0,0)	box	(3,3),(1,1)	f
bin	~=	polygon	(2,0),(2,4),(0,0)	polygon	(2,0),(2,4),(0,0)	t
bin	~=	polygon	(2,0),(2,4),(0,0)	polygon	(3,1),(3,3),(1,0)	f
bin	~=	circle	<(5,1),3>	circle	<(5,1),3>	t
bin	~=	circle	<(5,1),3>	circle	<(1,2),100>	f
bin	##	point	(0,1)	box	(0,0),(-0,-0)	(-0,0)
bin	##	point	(0,3)	lseg	[(1e-6,0),(0,0)]	(0,0)
bin	##	line	{1,1,1e300}	lseg	[(0,0),(2,0)]	(2,0)
bin	##	lseg	[(1,2),(3,4)]	box	(0,0),(-0,-0)	(-0,0)
bin	<@	circle	<(2,3),1e-6>	circle	<(3,3),1>	t
bin	&<	circle	<(3,-1),1e-6>	circle	<(1,2),2>	t
bin	<^	box	(0,1e-6),(-3,-2.5)	box	(3,0),(0,1)	t
bin	?#	box	(3,3),(1,1e-6)	box	(3,0),(3,-1e-7)	t
bin	@>	polygon	((1,0),(3,2),(-1,0),(0,2),(0,3))	polygon	((0,1),(2,1))	f
bin	<@	polygon	((1,1),(3,1),(0,0),(1,1))	polygon	((3,3),(3,0),(0,-1),(3,2),(-1,-1))	f
in	line	[(0,-0),(1,1)]	{1,-1,0}
bin	~=	polygon	(1,2),(3,4),(5,6),(7,8)	polygon	(7,8),(5,6),(3,4),(1,2)	t
bin	&&	polygon	((1,3),(1,0))	polygon	((1,0))	t
bin	<->	polygon	((-1,3),(2,1),(1,3))	polygon	((1,2))	0
bin	<->	point	(2,3)	lseg	[(2,3),(3,0)]	0
bin	<->	lseg	[(1,2),(2,-1)]	point	(1,2)	0
bin	<>	point	(NaN,NaN)	point	(NaN,NaN)	f
bin	~=	point	(NaN,NaN)	point	(NaN,NaN)	t
bin	<->	line	{1,0,5}	line	{NaN,0,3}	2
bin	<->	line	{NaN,0,3}	line	{1,0,5}	NaN
in	box	(0,0),(NaN,1)	(NaN,1),(0,0)
bin	<->	path	[(NaN,0),(1,1),(2,2)]	path	[(5,5),(6,6)]	4.242640687119285
bin	<->	path	[(5,5),(6,6)]	path	[(1,1),(2,2),(NaN,0)]	4.242640687119285
bin	<->	path	[(NaN,NaN),(NaN,NaN),(1,1),(2,2)]	path	[(5,5),(6,6)]	4.242640687119285
"#;

    fn unescape(s: &str) -> String {
        let b = s.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'\\' && b[i + 1] == b'x' {
                out.push(u8::from_str_radix(&s[i + 2..i + 4], 16).unwrap());
                i += 4;
            } else if b[i] == b'\\' {
                out.push(b[i + 1]);
                i += 2;
            } else {
                out.push(b[i]);
                i += 1;
            }
        }
        String::from_utf8(out).unwrap()
    }

    fn escape(s: &str) -> String {
        let mut out = String::new();
        for c in s.chars() {
            match c {
                '\\' => out.push_str("\\\\"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\x{:02x}", c as u32)),
                c => out.push(c),
            }
        }
        out
    }

    /// An answer as psql prints it.
    fn answer(v: &Result<GeoValue>) -> String {
        match v {
            Err(e) => format!("ERROR: {}", escape(&e.0)),
            Ok(GeoValue::Null) => "NULL".into(),
            Ok(GeoValue::Bool(b)) => (if *b { "t" } else { "f" }).into(),
            Ok(GeoValue::Int(i)) => i.to_string(),
            Ok(GeoValue::Float(f)) => f8_text(*f),
            Ok(GeoValue::Geo(g)) => geo_out(g),
        }
    }

    fn kind(name: &str) -> GeoKind {
        GeoKind::of_name(name).unwrap()
    }

    fn read(name: &str, s: &str) -> Geo {
        geo_in(kind(name), s).unwrap()
    }

    /// One question of `ANSWERS` asked of the module.
    fn ask(f: &[&str]) -> Result<GeoValue> {
        match f[0] {
            "in" => geo_in(kind(f[1]), &unescape(f[2])).map(GeoValue::Geo),
            "un" => geo_unary(f[1], &read(f[2], &unescape(f[3]))).unwrap(),
            "cast" => geo_cast(&read(f[1], &unescape(f[2])), kind(f[3]))
                .unwrap()
                .map(GeoValue::Geo),
            _ => geo_binary(
                f[1],
                &read(f[2], &unescape(f[3])),
                &read(f[4], &unescape(f[5])),
            )
            .unwrap(),
        }
    }

    #[test]
    fn every_question_answers_as_postgresql_19_answered_it() {
        let mut wrong = Vec::new();
        let mut asked = 0;
        for line in ANSWERS.lines().filter(|l| !l.is_empty()) {
            let f: Vec<&str> = line.split('\t').collect();
            let (want, question) = f.split_last().unwrap();
            let got = answer(&ask(question));
            asked += 1;
            if got != *want {
                wrong.push(format!("{line}\n  got {got}"));
            }
        }
        assert!(asked > 800, "{asked}");
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    /// Every operator pg_operator holds on the geometric types on 19: its name, its left type
    /// (none for a prefix operator) and its right.
    const OPERATORS: &str = "
        # - path; # - polygon; # lseg lseg; # box box; # line line; ## point lseg; ## point box;
        ## point line; ## lseg lseg; ## lseg box; ## line lseg; && box box; && polygon polygon;
        && circle circle; &< box box; &< polygon polygon; &< circle circle; &<| box box;
        &<| polygon polygon; &<| circle circle; &> box box; &> polygon polygon; &> circle circle;
        * point point; * path point; * box point; * circle point; + point point; + path point;
        + path path; + box point; + circle point; - point point; - path point; - box point;
        - circle point; / point point; / path point; / box point; / circle point; < lseg lseg;
        < path path; < box box; < circle circle; <-> point point; <-> point lseg;
        <-> point path; <-> point box; <-> point polygon; <-> point line; <-> point circle;
        <-> lseg point; <-> lseg lseg; <-> lseg box; <-> lseg line; <-> path point;
        <-> path path; <-> box point; <-> box lseg; <-> box box; <-> polygon point;
        <-> polygon polygon; <-> polygon circle; <-> line point; <-> line lseg; <-> line line;
        <-> circle point; <-> circle polygon; <-> circle circle; << point point; << box box;
        << polygon polygon; << circle circle; <<| point point; <<| box box;
        <<| polygon polygon; <<| circle circle; <= lseg lseg; <= path path; <= box box;
        <= circle circle; <> point point; <> lseg lseg; <> circle circle; <@ point lseg;
        <@ point path; <@ point box; <@ point polygon; <@ point line; <@ point circle;
        <@ lseg box; <@ lseg line; <@ box box; <@ polygon polygon; <@ circle circle;
        <^ point point; <^ box box; = lseg lseg; = path path; = box box; = line line;
        = circle circle; > lseg lseg; > path path; > box box; > circle circle; >= lseg lseg;
        >= path path; >= box box; >= circle circle; >> point point; >> box box;
        >> polygon polygon; >> circle circle; >^ point point; >^ box box; ?# lseg lseg;
        ?# lseg box; ?# lseg line; ?# path path; ?# box box; ?# line box; ?# line line;
        ?- - lseg; ?- - line; ?- point point; ?-| lseg lseg; ?-| line line; ?| - lseg;
        ?| - line; ?| point point; ?|| lseg lseg; ?|| line line; @-@ - lseg; @-@ - path;
        @> path point; @> box point; @> box box; @> polygon point; @> polygon polygon;
        @> circle point; @> circle circle; @@ - lseg; @@ - box; @@ - polygon; @@ - circle;
        |&> box box; |&> polygon polygon; |&> circle circle; |>> point point; |>> box box;
        |>> polygon polygon; |>> circle circle; ~= point point; ~= box box;
        ~= polygon polygon; ~= circle circle";

    /// A value of each type.
    fn one(k: GeoKind) -> Geo {
        let s = match k {
            GeoKind::Point => "(1,2)",
            GeoKind::Line => "{1,-1,0}",
            GeoKind::Lseg => "[(0,0),(1,1)]",
            GeoKind::Box => "(1,1),(0,0)",
            GeoKind::Path => "[(0,0),(1,1)]",
            GeoKind::Polygon => "((0,0),(1,1),(1,0))",
            GeoKind::Circle => "<(0,0),1>",
        };
        geo_in(k, s).unwrap()
    }

    #[test]
    fn the_operators_are_the_157_pg_operator_holds_and_no_other() {
        let held: std::collections::BTreeSet<String> = OPERATORS
            .split(';')
            .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
            .collect();
        assert_eq!(held.len(), 157);
        let names: std::collections::BTreeSet<&str> = OPERATORS
            .split(';')
            .filter_map(|s| s.split_whitespace().next())
            .collect();
        assert_eq!(names.len(), 34);
        let mut found = std::collections::BTreeSet::new();
        for op in &names {
            for a in GeoKind::ALL {
                if geo_unary(op, &one(a)).is_some() {
                    found.insert(format!("{op} - {}", a.name()));
                }
                for b in GeoKind::ALL {
                    if geo_binary(op, &one(a), &one(b)).is_some() {
                        found.insert(format!("{op} {} {}", a.name(), b.name()));
                    }
                }
            }
        }
        assert_eq!(found, held);
    }

    #[test]
    fn the_casts_are_the_14_pg_cast_holds_and_no_other() {
        let held = [
            "point box",
            "lseg point",
            "path polygon",
            "box point",
            "box lseg",
            "box polygon",
            "box circle",
            "polygon point",
            "polygon path",
            "polygon box",
            "polygon circle",
            "circle point",
            "circle box",
            "circle polygon",
        ];
        let mut found = Vec::new();
        for a in GeoKind::ALL {
            for b in GeoKind::ALL {
                if a != b && geo_cast(&one(a), b).is_some() {
                    found.push(format!("{} {}", a.name(), b.name()));
                }
            }
        }
        found.sort();
        let mut held: Vec<String> = held.iter().map(|s| s.to_string()).collect();
        held.sort();
        assert_eq!(found, held);
        assert_eq!(
            geo_out(
                &geo_cast(&one(GeoKind::Line), GeoKind::Line)
                    .unwrap()
                    .unwrap()
            ),
            "{1,-1,0}"
        );
    }

    #[test]
    fn a_box_made_a_polygon_keeps_the_bounding_box_box_poly_gives_it() {
        // PostgreSQL 19: (box '(0,0),(-0,-0)')::polygon::box → (-0,-0),(0,0), while the same
        // points read as a polygon give polygon '((-0,-0),(-0,0),(0,0),(0,-0))'::box →
        // (-0,-0),(-0,-0).
        let poly = geo_cast(&read("box", "(0,0),(-0,-0)"), GeoKind::Polygon)
            .unwrap()
            .unwrap();
        assert_eq!(geo_out(&poly), "((-0,-0),(-0,0),(0,0),(0,-0))");
        let bound = geo_cast(&poly, GeoKind::Box).unwrap().unwrap();
        assert_eq!(geo_out(&bound), "(-0,-0),(0,0)");
        let again = geo_from_leaves(GeoKind::Polygon, &geo_leaves(&poly), false).unwrap();
        let bound = geo_cast(&again, GeoKind::Box).unwrap().unwrap();
        assert_eq!(geo_out(&bound), "(-0,-0),(-0,-0)");
    }

    #[test]
    fn a_value_s_leaves_make_it_again_bit_for_bit() {
        let values = [
            ("point", "(-0,NaN)", 2),
            ("line", "{1,-0,-Infinity}", 3),
            ("lseg", "[(1,2),(-0,4)]", 4),
            ("box", "(1,1),(-0,-0)", 4),
            ("path", "[(1,2),(3,4),(-0,NaN)]", 6),
            ("path", "((1,2))", 2),
            ("polygon", "((0,0),(1e300,-1e-300),(NaN,1))", 6),
            ("circle", "<(1,-0),NaN>", 3),
        ];
        for (k, s, n) in values {
            let g = read(k, s);
            let leaves = geo_leaves(&g);
            assert_eq!(leaves.len(), n, "{s}");
            assert_eq!(kind(k).fixed_leaves().unwrap_or(n), n);
            let back = geo_from_leaves(kind(k), &leaves, geo_closed(&g).unwrap_or(false)).unwrap();
            assert_eq!(geo_out(&back), geo_out(&g));
            let bits = |v: &[f64]| v.iter().map(|f| f.to_bits()).collect::<Vec<_>>();
            assert_eq!(bits(&geo_leaves(&back)), bits(&leaves), "{s}");
        }
        let poly = read("polygon", "((3,1),(0,4),(-2,-5))");
        let back = geo_from_leaves(GeoKind::Polygon, &geo_leaves(&poly), false).unwrap();
        assert_eq!(
            geo_out(&geo_cast(&back, GeoKind::Box).unwrap().unwrap()),
            "(3,4),(-2,-5)"
        );
        assert!(geo_from_leaves(GeoKind::Circle, &[1.0, 2.0], false).is_err());
        assert!(geo_from_leaves(GeoKind::Path, &[], true).is_err());
        assert!(geo_from_leaves(GeoKind::Polygon, &[1.0, 2.0, 3.0], false).is_err());
    }

    #[test]
    fn a_value_keeps_its_own_float_bits() {
        let p = read("point", "(-0,-NaN)");
        let GeoValue::Geo(Geo::Point(q)) = geo_binary("+", &p, &read("point", "(0,0)"))
            .unwrap()
            .unwrap()
        else {
            panic!();
        };
        assert_eq!(geo_out(&p), "(-0,NaN)");
        let Geo::Point(p) = p else { panic!() };
        assert_eq!(p.x.to_bits(), (-0.0f64).to_bits());
        assert!(p.y.is_nan() && p.y.is_sign_negative());
        assert_eq!(q.x.to_bits(), 0.0f64.to_bits());
    }
}

/// A value as a row holds it: its float8s' bits, 8 bytes each, big-endian, in the order
/// `geo_leaves` lays them out; a path's closed flag a byte before them, and a polygon's bounding
/// box, its high corner then its low, before its points.
pub fn geo_bytes(g: &Geo) -> Vec<u8> {
    let mut out = match g {
        Geo::Path(p) => vec![p.closed as u8],
        Geo::Polygon(p) => geo_leaves(&Geo::Box(p.bound))
            .iter()
            .flat_map(|f| f.to_bits().to_be_bytes())
            .collect(),
        _ => Vec::new(),
    };
    out.extend(geo_leaves(g).iter().flat_map(|f| f.to_bits().to_be_bytes()));
    out
}

/// A value of type `kind`, from the bytes a row holds it as: a polygon with the bounding box it
/// was made with, which one made again from its points can differ from where `-0` and `0` tie.
pub fn geo_of_bytes(kind: GeoKind, b: &[u8]) -> Result<Geo> {
    let floats = |b: &[u8]| -> Vec<f64> {
        b.chunks(8)
            .map(|c| f64::from_bits(u64::from_be_bytes(c.try_into().unwrap_or([0; 8]))))
            .collect()
    };
    match kind {
        GeoKind::Path if !b.is_empty() => geo_from_leaves(kind, &floats(&b[1..]), b[0] != 0),
        GeoKind::Polygon if b.len() >= 32 => {
            let Geo::Box(bound) = geo_from_leaves(GeoKind::Box, &floats(&b[..32]), false)? else {
                unreachable!()
            };
            let Geo::Polygon(p) = geo_from_leaves(kind, &floats(&b[32..]), false)? else {
                unreachable!()
            };
            Ok(Geo::Polygon(Polygon {
                points: p.points,
                bound,
            }))
        }
        _ => geo_from_leaves(kind, &floats(b), false),
    }
}
