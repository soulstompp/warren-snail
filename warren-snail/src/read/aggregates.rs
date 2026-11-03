//! The aggregates past those the types carry, each folded as PostgreSQL's transition and final
//! functions fold it: the spreads of numbers (variance and standard deviation) exactly over
//! integers and numerics and as float8_accum folds floats; the regressions as float8_regr_accum
//! folds pairs; the bitwise aggregates; any_value; the ordered-set aggregates over the rows they
//! sort (mode, percentile_cont, percentile_disc) and the hypothetical-set aggregates (rank,
//! dense_rank, percent_rank, cume_dist).

use super::pipeline::Value;
use crate::catalog::{Casing, ColumnType};
use crate::numeric::Numeric;
use crate::{Error, Result};
use std::cmp::Ordering;

fn overflow() -> Error {
    Error::new("value out of range: overflow")
}

/// What a regression's final function returns from float8_regr_accum's values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Regr {
    Sxx,
    Syy,
    Sxy,
    AvgX,
    AvgY,
    R2,
    Slope,
    Intercept,
    CovarPop,
    CovarSamp,
    Corr,
}

/// How an ordered-set or hypothetical-set aggregate reads its sorted rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Sorted {
    Mode,
    PercentileCont,
    PercentileDisc,
    Rank,
    DenseRank,
    PercentRank,
    CumeDist,
}

/// How an ORDER BY among an aggregate's arguments or its WITHIN GROUP sorts one column:
/// descending, its NULLs first, and the collation it compares strings under.
#[derive(Clone, Copy, Debug)]
pub(super) struct Direction {
    pub desc: bool,
    pub nulls_first: bool,
    pub under: Option<Casing>,
    /// Whether it compares strings by their bytes, whatever their collation.
    pub bytes: bool,
}

/// One aggregate past those the types carry, as it folds.
#[derive(Clone, Debug)]
pub(super) enum Agg {
    /// var_pop, var_samp, stddev_pop and stddev_samp of integers or numerics: the rows, their sum
    /// and their sum of squares, exactly, and whether a NaN or an infinity came.
    Spread {
        n: i64,
        sum: Numeric,
        squares: Numeric,
        odd: bool,
        variance: bool,
        sample: bool,
    },
    /// The same of floats, as float8_accum and float4_accum fold them: N, Sx and Sxx.
    FloatSpread {
        t: [f64; 3],
        variance: bool,
        sample: bool,
    },
    /// covar_pop, covar_samp, corr and the regr_ functions: float8_regr_accum's eight values.
    Regression { t: [f64; 8], of: Regr },
    /// regr_count: the rows where neither argument is NULL.
    RegrCount(i64),
    /// bit_and, bit_or and bit_xor: the operation, and what it has made so far.
    Bitwise { op: char, acc: Option<Value> },
    /// any_value: the first value not NULL.
    AnyValue(Option<Value>),
    /// string_agg: the values not NULL so far, each after the delimiter given beside it.
    Concat(Option<Value>),
    /// array_agg: every value, NULLs among them, and the array type it makes where the step
    /// resolved it; `arrays` where it takes arrays, each a slice of one more dimension.
    Collect {
        ty: Option<ColumnType>,
        arrays: bool,
        values: Vec<Value>,
    },
    /// An aggregate of json or jsonb.
    Json(Box<super::json::Fold>),
    /// range_agg: the type of the ranges or multiranges folded, and every range of theirs so far.
    RangeUnion(Option<(ColumnType, Vec<crate::composed::Item>)>),
    /// range_intersect_agg: the range or multirange every one folded so far holds.
    RangeIntersect(Option<Value>),
    /// An ordered-set or hypothetical-set aggregate: the rows it sorts, how it sorts them, and its
    /// direct arguments, read from the first row they are given on.
    Sorted {
        of: Sorted,
        rows: Vec<Vec<Value>>,
        by: Vec<Direction>,
        direct: Option<Vec<Value>>,
    },
    /// An aggregate a caller handed the snail: its form, and each row given it so far, its arguments
    /// in the types its signature names, in the order they came.
    Handed {
        form: std::rc::Rc<super::handed::Function>,
        rows: Vec<Vec<Value>>,
    },
}

impl Agg {
    /// The aggregate a name makes, where it is one of these: a spread of numbers, taken as one of
    /// floats where its first value is a float.
    pub(super) fn of(name: &str) -> Option<Agg> {
        let spread = |variance: bool, sample: bool| Agg::Spread {
            n: 0,
            sum: Numeric::zero(),
            squares: Numeric::zero(),
            odd: false,
            variance,
            sample,
        };
        let regression = |of: Regr| Agg::Regression { t: [0.0; 8], of };
        Some(match name {
            "var_pop" => spread(true, false),
            "var_samp" | "variance" => spread(true, true),
            "stddev_pop" => spread(false, false),
            "stddev_samp" | "stddev" => spread(false, true),
            "regr_sxx" => regression(Regr::Sxx),
            "regr_syy" => regression(Regr::Syy),
            "regr_sxy" => regression(Regr::Sxy),
            "regr_avgx" => regression(Regr::AvgX),
            "regr_avgy" => regression(Regr::AvgY),
            "regr_r2" => regression(Regr::R2),
            "regr_slope" => regression(Regr::Slope),
            "regr_intercept" => regression(Regr::Intercept),
            "covar_pop" => regression(Regr::CovarPop),
            "covar_samp" => regression(Regr::CovarSamp),
            "corr" => regression(Regr::Corr),
            "regr_count" => Agg::RegrCount(0),
            "bit_and" => Agg::Bitwise { op: '&', acc: None },
            "bit_or" => Agg::Bitwise { op: '|', acc: None },
            "bit_xor" => Agg::Bitwise { op: '#', acc: None },
            "any_value" => Agg::AnyValue(None),
            "string_agg" => Agg::Concat(None),
            "range_agg" => Agg::RangeUnion(None),
            n if super::json::AGGREGATES.contains(&n) => {
                Agg::Json(Box::new(super::json::Fold::of(n)?))
            }
            "range_intersect_agg" => Agg::RangeIntersect(None),
            _ => return None,
        })
    }

    /// An ordered-set or hypothetical-set aggregate of the name, sorting by `by`.
    pub(super) fn sorted(name: &str, by: Vec<Direction>) -> Option<Agg> {
        let of = match name {
            "mode" => Sorted::Mode,
            "percentile_cont" => Sorted::PercentileCont,
            "percentile_disc" => Sorted::PercentileDisc,
            "rank" => Sorted::Rank,
            "dense_rank" => Sorted::DenseRank,
            "percent_rank" => Sorted::PercentRank,
            "cume_dist" => Sorted::CumeDist,
            _ => return None,
        };
        Some(Agg::Sorted {
            of,
            rows: Vec::new(),
            by,
            direct: None,
        })
    }

    /// Whether it keeps the rows it is given until its group ends.
    pub(super) fn keeps_rows(&self) -> bool {
        matches!(
            self,
            Agg::Collect { .. }
                | Agg::Json(_)
                | Agg::Handed { .. }
                | Agg::Sorted { .. }
                | Agg::RangeUnion(_)
        )
    }

    /// Whether it folds a NULL in as a value, as array_agg does.
    pub(super) fn keeps_nulls(&self) -> bool {
        matches!(self, Agg::Collect { .. } | Agg::Json(_)) || matches!(self, Agg::Handed { .. })
    }

    /// Its direct arguments, the first time they are given.
    pub(super) fn direct(&mut self, values: Vec<Value>) {
        if let Agg::Sorted { direct, .. } = self {
            direct.get_or_insert(values);
        }
    }

    /// Folds in one row's arguments, each compared under the collation `under` gives it.
    pub(super) fn add(&mut self, args: &[Value], under: &[Option<Casing>]) -> Result<()> {
        match self {
            Agg::Handed { form, rows } => {
                rows.push(super::handed::fold_row(form, args)?);
                return Ok(());
            }
            Agg::Sorted { of, rows, by, .. } => {
                for (d, u) in by.iter_mut().zip(under) {
                    d.under = u.filter(|_| !d.bytes);
                }
                // The ordered-set aggregates pass over NULL; a hypothetical set sorts them too.
                let hypothetical = !matches!(
                    of,
                    Sorted::Mode | Sorted::PercentileCont | Sorted::PercentileDisc
                );
                if hypothetical || !args.iter().any(|v| v.is_null()) {
                    rows.push(args.to_vec());
                }
                return Ok(());
            }
            Agg::RegrCount(n) => {
                if !args.iter().any(|v| v.is_null()) {
                    *n += 1;
                }
                return Ok(());
            }
            Agg::Collect { values, .. } => {
                values.push(args[0].clone());
                return Ok(());
            }
            Agg::Json(fold) => return fold.add(args),
            Agg::Concat(acc) => {
                let [v, d] = args else {
                    return Err(Error::new("string_agg takes a value and a delimiter"));
                };
                if v.is_null() {
                    return Ok(());
                }
                *acc = Some(match (acc.take(), v, d) {
                    (None, v, _) => v.clone(),
                    (Some(Value::Bytes(mut a)), Value::Bytes(b), d) => {
                        if let Value::Bytes(d) = d {
                            a.extend_from_slice(d);
                        }
                        a.extend_from_slice(b);
                        Value::Bytes(a)
                    }
                    (Some(a), v, d) => {
                        let mut s = a.text().unwrap_or_default();
                        if let Some(d) = d.text() {
                            s.push_str(&d);
                        }
                        s.push_str(&v.text().unwrap_or_default());
                        Value::Text(s)
                    }
                });
                return Ok(());
            }
            _ => {}
        }
        if args.iter().any(|v| v.is_null()) {
            return Ok(());
        }
        if let (
            Agg::Spread {
                n: 0,
                variance,
                sample,
                ..
            },
            Value::Real(_) | Value::Double(_),
        ) = (&*self, &args[0])
        {
            *self = Agg::FloatSpread {
                t: [0.0; 3],
                variance: *variance,
                sample: *sample,
            };
        }
        match self {
            Agg::Spread {
                n,
                sum,
                squares,
                odd,
                ..
            } => {
                let x = args[0]
                    .to_numeric()
                    .ok_or_else(|| Error::new(format!("{:?} is no number", args[0])))?;
                *n += 1;
                if !x.is_finite() {
                    *odd = true;
                    return Ok(());
                }
                *sum = sum.add(&x)?;
                *squares = squares.add(&x.mul(&x)?)?;
            }
            Agg::FloatSpread { t, .. } => float8_accum(t, double(&args[0])?)?,
            Agg::Regression { t, .. } => {
                float8_regr_accum(t, double(&args[0])?, double(&args[1])?)?
            }
            Agg::Bitwise { op, acc } => {
                let v = args[0].clone();
                *acc = Some(match acc.take() {
                    None => v,
                    Some(a) => bitwise(*op, &a, &v)?,
                });
            }
            Agg::AnyValue(first) => {
                first.get_or_insert_with(|| args[0].clone());
            }
            Agg::RangeUnion(acc) => {
                let Value::Composed(ty, c) = &args[0] else {
                    return Err(Error::new(format!(
                        "function range_agg({}) does not exist",
                        super::pipeline::kind_name(&args[0])
                    )));
                };
                let ranges = &mut acc.get_or_insert_with(|| (*ty, Vec::new())).1;
                match &c.item {
                    crate::composed::Item::Multirange(rs) => ranges.extend(rs.iter().cloned()),
                    r => ranges.push(r.clone()),
                }
            }
            Agg::RangeIntersect(acc) => {
                *acc = Some(match acc.take() {
                    None => args[0].clone(),
                    Some(a) => range_intersect(&a, &args[0])?,
                });
            }
            Agg::RegrCount(_)
            | Agg::Sorted { .. }
            | Agg::Concat(_)
            | Agg::Collect { .. }
            | Agg::Json(_)
            | Agg::Handed { .. } => {
                unreachable!()
            }
        }
        Ok(())
    }

    /// Its answer over the rows folded.
    pub(super) fn value(&self) -> Result<Value> {
        Ok(match self {
            Agg::Handed { form, rows } => super::handed::folded(form, rows)?,
            Agg::Spread {
                n,
                sum,
                squares,
                odd,
                variance,
                sample,
            } => {
                if *n == 0 || (*sample && *n <= 1) {
                    Value::Null
                } else if *odd {
                    Value::Numeric(Numeric::NAN)
                } else {
                    Value::of_numeric(Numeric::spread(*n, sum, squares, *variance, *sample)?)
                }
            }
            Agg::FloatSpread {
                t,
                variance,
                sample,
            } => {
                let (n, sxx) = (t[0], t[2]);
                if n == 0.0 || (*sample && n <= 1.0) {
                    Value::Null
                } else {
                    let v = if *sample { sxx / (n - 1.0) } else { sxx / n };
                    Value::Double(if *variance { v } else { v.sqrt() })
                }
            }
            Agg::Regression { t, of } => regression(t, *of).map_or(Value::Null, Value::Double),
            Agg::RegrCount(n) => Value::Int(*n),
            Agg::Bitwise { acc, .. } | Agg::AnyValue(acc) | Agg::Concat(acc) => {
                acc.clone().unwrap_or(Value::Null)
            }
            Agg::Sorted {
                of,
                rows,
                by,
                direct,
            } => sorted_value(*of, rows, by, direct.as_deref().unwrap_or(&[]))?,
            Agg::Collect { ty, arrays, values } => collected(*ty, *arrays, values)?,
            Agg::Json(fold) => fold.value()?,
            Agg::RangeUnion(None) | Agg::RangeIntersect(None) => Value::Null,
            Agg::RangeIntersect(Some(v)) => v.clone(),
            Agg::RangeUnion(Some((ty, ranges))) => {
                let (multi, range) = match super::typed::multirange_range(*ty) {
                    Some(range) => (*ty, range),
                    None => (
                        crate::composed::multirange_of(*ty).ok_or_else(|| {
                            Error::new(format!("{} has no multirange", ty.pg_name()))
                        })?,
                        *ty,
                    ),
                };
                super::pipeline::composed(
                    multi,
                    crate::composed::Item::Multirange(crate::composed::canonical_ranges(
                        range,
                        ranges.clone(),
                    )),
                )
            }
        })
    }
}

/// A number as a double precision, as PostgreSQL casts one to the float8 these aggregates take.
fn double(v: &Value) -> Result<f64> {
    match super::pipeline::float_cast(v.clone(), true)? {
        Value::Double(d) => Ok(d),
        other => Err(Error::new(format!("{other:?} is no double precision"))),
    }
}

/// float8_accum: one value into N, Sx and Sxx.
fn float8_accum(t: &mut [f64; 3], x: f64) -> Result<()> {
    let (n0, sx0) = (t[0], t[1]);
    t[0] = n0 + 1.0;
    t[1] = sx0 + x;
    if n0 > 0.0 {
        let tmp = x * t[0] - t[1];
        t[2] += tmp * tmp / (t[0] * n0);
        if t[1].is_infinite() || t[2].is_infinite() {
            if !sx0.is_infinite() && !x.is_infinite() {
                return Err(overflow());
            }
            t[2] = f64::NAN;
        }
    } else if x.is_nan() || x.is_infinite() {
        t[2] = f64::NAN;
    }
    Ok(())
}

/// float8_regr_accum: one pair, y then x, into N, Sx, Sxx, Sy, Syy, Sxy and the values common to
/// every row so far.
fn float8_regr_accum(t: &mut [f64; 8], y: f64, x: f64) -> Result<()> {
    let old = *t;
    let n = old[0] + 1.0;
    let (sx, sy) = (old[1] + x, old[3] + y);
    let (mut sxx, mut syy, mut sxy) = (old[2], old[4], old[5]);
    let (mut common_x, mut common_y) = (old[6], old[7]);
    if old[0] > 0.0 {
        if x != common_x || x.is_nan() {
            common_x = f64::NAN;
        }
        if y != common_y || y.is_nan() {
            common_y = f64::NAN;
        }
        let tmp_x = x * n - sx;
        let tmp_y = y * n - sy;
        let scale = 1.0 / (n * old[0]);
        if common_x.is_nan() {
            sxx += tmp_x * tmp_x * scale;
        }
        if common_y.is_nan() {
            syy += tmp_y * tmp_y * scale;
        }
        if common_x.is_nan() && common_y.is_nan() {
            sxy += tmp_x * tmp_y * scale;
        } else if x.is_nan() || x.is_infinite() || y.is_nan() || y.is_infinite() {
            sxy = f64::NAN;
        }
        if sx.is_infinite()
            || sxx.is_infinite()
            || sy.is_infinite()
            || syy.is_infinite()
            || sxy.is_infinite()
        {
            if ((sx.is_infinite() || sxx.is_infinite())
                && !old[1].is_infinite()
                && !x.is_infinite())
                || ((sy.is_infinite() || syy.is_infinite())
                    && !old[3].is_infinite()
                    && !y.is_infinite())
                || (sxy.is_infinite()
                    && !old[1].is_infinite()
                    && !x.is_infinite()
                    && !old[3].is_infinite()
                    && !y.is_infinite())
            {
                return Err(overflow());
            }
            if sxx.is_infinite() {
                sxx = f64::NAN;
            }
            if syy.is_infinite() {
                syy = f64::NAN;
            }
            if sxy.is_infinite() {
                sxy = f64::NAN;
            }
        }
    } else {
        if x.is_nan() || x.is_infinite() {
            sxx = f64::NAN;
            sxy = f64::NAN;
        }
        if y.is_nan() || y.is_infinite() {
            syy = f64::NAN;
            sxy = f64::NAN;
        }
        common_x = x;
        common_y = y;
    }
    *t = [n, sx, sxx, sy, syy, sxy, common_x, common_y];
    Ok(())
}

/// A regression's final function over float8_regr_accum's values; none where PostgreSQL's is NULL.
fn regression(t: &[f64; 8], of: Regr) -> Option<f64> {
    let [n, sx, sxx, sy, syy, sxy, common_x, common_y] = *t;
    if n < 1.0 {
        return None;
    }
    Some(match of {
        Regr::Sxx => sxx,
        Regr::Syy => syy,
        Regr::Sxy => sxy,
        Regr::AvgX => {
            if !common_x.is_nan() {
                common_x
            } else {
                sx / n
            }
        }
        Regr::AvgY => {
            if !common_y.is_nan() {
                common_y
            } else {
                sy / n
            }
        }
        Regr::CovarPop => sxy / n,
        Regr::CovarSamp => {
            if n < 2.0 {
                return None;
            }
            sxy / (n - 1.0)
        }
        Regr::Corr => {
            if sxx == 0.0 || syy == 0.0 {
                return None;
            }
            let product = sxx * syy;
            let root = if product == 0.0 || product.is_infinite() {
                sxx.sqrt() * syy.sqrt()
            } else {
                product.sqrt()
            };
            (sxy / root).clamp(-1.0, 1.0)
        }
        Regr::R2 => {
            if sxx == 0.0 {
                return None;
            }
            if syy == 0.0 {
                return Some(1.0);
            }
            let numerator = sxy * sxy;
            let denominator = sxx * syy;
            let r = if numerator == 0.0
                || numerator.is_infinite()
                || denominator == 0.0
                || denominator.is_infinite()
            {
                let root = sxy / (sxx.sqrt() * syy.sqrt());
                root * root
            } else {
                numerator / denominator
            };
            if r > 1.0 {
                1.0
            } else {
                r
            }
        }
        Regr::Slope => {
            if sxx == 0.0 {
                return None;
            }
            sxy / sxx
        }
        Regr::Intercept => {
            if sxx == 0.0 {
                return None;
            }
            let mut dy = sx * sxy / sxx;
            if (dy == 0.0 || dy.is_infinite())
                && !(sx.is_infinite()
                    || sxy.is_infinite()
                    || sxx.is_infinite()
                    || sx.is_nan()
                    || sxy.is_nan()
                    || sxx.is_nan())
            {
                let (m_sx, n_sx) = frexp(sx);
                let (m_sxy, n_sxy) = frexp(sxy);
                let (m_sxx, n_sxx) = frexp(sxx);
                dy = ldexp(m_sx * m_sxy / m_sxx, n_sx + n_sxy - n_sxx);
            }
            (sy - dy) / n
        }
    })
}

/// C's frexp: a finite number as its fraction in [0.5, 1) and its power of two.
fn frexp(x: f64) -> (f64, i32) {
    if x == 0.0 || !x.is_finite() {
        return (x, 0);
    }
    let bits = x.to_bits();
    let exp = ((bits >> 52) & 0x7ff) as i32;
    if exp == 0 {
        // A subnormal: scaled into the normal range first.
        let (m, e) = frexp(x * f64::powi(2.0, 54));
        return (m, e - 54);
    }
    let m = f64::from_bits((bits & !(0x7ff << 52)) | (1022 << 52));
    (m, exp - 1022)
}

/// C's ldexp: x times 2 to the power e.
fn ldexp(x: f64, e: i32) -> f64 {
    let mut x = x;
    let mut e = e;
    while e > 1000 {
        x *= f64::powi(2.0, 1000);
        e -= 1000;
    }
    while e < -1000 {
        x *= f64::powi(2.0, -1000);
        e += 1000;
    }
    x * f64::powi(2.0, e)
}

/// One step of bit_and, bit_or or bit_xor: integers bit by bit, or bit strings of one length.
fn bitwise(op: char, a: &Value, b: &Value) -> Result<Value> {
    Ok(match (a, b) {
        (Value::Int(x), Value::Int(y)) => Value::Int(match op {
            '&' => x & y,
            '|' => x | y,
            _ => x ^ y,
        }),
        (Value::Bits(_, x), Value::Bits(_, y)) => {
            let (name, f): (&str, fn(bool, bool) -> bool) = match op {
                '&' => ("AND", |p, q| p && q),
                '|' => ("OR", |p, q| p || q),
                _ => ("XOR", |p, q| p != q),
            };
            let k = crate::strings::bits_each(x, y, name, f)?;
            Value::Bits(ColumnType::Bit(crate::strings::bits_of_key(&k).0), k)
        }
        _ => {
            return Err(Error::new(format!(
                "bitwise {op} of {} is not read",
                super::pipeline::kind_name(a)
            )))
        }
    })
}

/// Two rows compared along a WITHIN GROUP's ORDER BY, NULLs where each column puts them.
pub(super) fn compare_rows(a: &[Value], b: &[Value], by: &[Direction]) -> Result<Ordering> {
    for (i, d) in by.iter().enumerate() {
        let (x, y) = (&a[i], &b[i]);
        let o = match (x.is_null(), y.is_null()) {
            (true, true) => Ordering::Equal,
            (true, false) => {
                if d.nulls_first {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            }
            (false, true) => {
                if d.nulls_first {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
            (false, false) => {
                let o = super::pipeline::compare_values(x, y, d.under)?.unwrap_or(Ordering::Equal);
                if d.desc {
                    o.reverse()
                } else {
                    o
                }
            }
        };
        if o != Ordering::Equal {
            return Ok(o);
        }
    }
    Ok(Ordering::Equal)
}

/// The fraction a percentile is asked at, held to [0, 1].
fn fraction(direct: &[Value]) -> Result<Option<f64>> {
    let Some(v) = direct.first() else {
        return Ok(None);
    };
    if v.is_null() {
        return Ok(None);
    }
    let p = double(v)?;
    if !(0.0..=1.0).contains(&p) {
        return Err(Error::new(format!(
            "percentile value {} is not between 0 and 1",
            g_format(p)
        )));
    }
    Ok(Some(p))
}

/// A double as C's `%g` writes it, PostgreSQL's way of naming a percentile in its refusal.
fn g_format(p: f64) -> String {
    if p.is_nan() {
        return "nan".into();
    }
    if p.is_infinite() {
        return if p > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if p == 0.0 {
        return "0".into();
    }
    let exp = p.abs().log10().floor() as i32;
    let trim = |s: String| {
        if s.contains('.') {
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        } else {
            s
        }
    };
    if (-4..6).contains(&exp) {
        let decimals = (5 - exp).max(0) as usize;
        trim(format!("{p:.decimals$}"))
    } else {
        let s = format!("{p:.5e}");
        let (m, e) = s.split_once('e').unwrap();
        let e: i32 = e.parse().unwrap();
        format!(
            "{}e{}{:02}",
            trim(m.to_string()),
            if e < 0 { '-' } else { '+' },
            e.abs()
        )
    }
}

/// An ordered-set or hypothetical-set aggregate's answer over its rows.
fn sorted_value(
    of: Sorted,
    rows: &[Vec<Value>],
    by: &[Direction],
    direct: &[Value],
) -> Result<Value> {
    let mut sorted = rows.to_vec();
    let mut failed = None;
    sorted.sort_by(|a, b| {
        compare_rows(a, b, by).unwrap_or_else(|e| {
            failed.get_or_insert(e);
            Ordering::Equal
        })
    });
    if let Some(e) = failed {
        return Err(e);
    }
    let n = sorted.len();
    Ok(match of {
        Sorted::Mode => {
            let mut best: Option<(&Value, usize)> = None;
            let mut i = 0;
            while i < n {
                let mut j = i + 1;
                while j < n && compare_rows(&sorted[i], &sorted[j], by)? == Ordering::Equal {
                    j += 1;
                }
                if best.is_none_or(|(_, f)| j - i > f) {
                    best = Some((&sorted[i][0], j - i));
                }
                i = j;
            }
            best.map_or(Value::Null, |(v, _)| v.clone())
        }
        // Of an array of fractions, an array of the same shape, each fraction's value.
        Sorted::PercentileDisc | Sorted::PercentileCont
            if matches!(direct.first(), Some(Value::Composed(..))) =>
        {
            let Some(Value::Composed(ty, c)) = direct.first() else {
                unreachable!()
            };
            let crate::composed::Item::Array {
                elements,
                dims,
                lower,
            } = &c.item
            else {
                return Ok(Value::Null);
            };
            if n == 0 {
                return Ok(Value::Null);
            }
            let fraction_type = super::typed::element_of(*ty).unwrap_or(ColumnType::Float8);
            let values = elements
                .iter()
                .map(|p| {
                    let p = super::pipeline::part_value(fraction_type, p);
                    sorted_value(of, &sorted, by, &[p])
                })
                .collect::<Result<Vec<_>>>()?;
            let element = match (of, &sorted[0][0]) {
                (Sorted::PercentileCont, Value::Interval(_)) => super::calls::full_interval(),
                (Sorted::PercentileCont, _) => ColumnType::Float8,
                (_, v) => super::pipeline::type_of_value(v).unwrap_or(ColumnType::Text(None)),
            };
            let elements = values
                .into_iter()
                .map(|v| super::pipeline::part_of(element, v, false))
                .collect::<Result<Vec<_>>>()?;
            super::pipeline::composed(
                super::typed::array_of(element),
                crate::composed::Item::Array {
                    elements,
                    dims: dims.clone(),
                    lower: lower.clone(),
                },
            )
        }
        Sorted::PercentileDisc => {
            let Some(p) = fraction(direct)? else {
                return Ok(Value::Null);
            };
            if n == 0 {
                return Ok(Value::Null);
            }
            let row = (p * n as f64).ceil() as usize;
            sorted[row.max(1) - 1][0].clone()
        }
        Sorted::PercentileCont => {
            let Some(p) = fraction(direct)? else {
                return Ok(Value::Null);
            };
            if n == 0 {
                return Ok(Value::Null);
            }
            let at = p * (n - 1) as f64;
            let (first, second) = (at.floor() as usize, at.ceil() as usize);
            let lo = &sorted[first][0];
            if first == second {
                match lo {
                    Value::Interval(_) => lo.clone(),
                    v => Value::Double(double(v)?),
                }
            } else {
                let hi = &sorted[second][0];
                let proportion = at - first as f64;
                match (lo, hi) {
                    (Value::Interval(a), Value::Interval(b)) => {
                        let diff = crate::times::interval_mi(*b, *a)?;
                        let scaled = crate::times::interval_mul(diff, proportion)?;
                        Value::Interval(crate::times::interval_pl(scaled, *a)?)
                    }
                    (a, b) => {
                        let (a, b) = (double(a)?, double(b)?);
                        Value::Double(a + proportion * (b - a))
                    }
                }
            }
        }
        Sorted::Rank | Sorted::PercentRank | Sorted::CumeDist | Sorted::DenseRank => {
            if direct.len() != by.len() {
                return Err(Error::new(
                    "a hypothetical-set aggregate takes as many direct arguments as it orders by",
                ));
            }
            let before = |row: &Vec<Value>| -> Result<bool> {
                let o = compare_rows(row, direct, by)?;
                Ok(o == Ordering::Less || (of == Sorted::CumeDist && o == Ordering::Equal))
            };
            match of {
                Sorted::DenseRank => {
                    let mut groups = 0i64;
                    let mut last: Option<&Vec<Value>> = None;
                    for r in &sorted {
                        if !before(r)? {
                            break;
                        }
                        if last.is_none_or(|l| compare_rows(l, r, by).ok() != Some(Ordering::Equal))
                        {
                            groups += 1;
                        }
                        last = Some(r);
                    }
                    Value::Int(groups + 1)
                }
                _ => {
                    let mut rank = 1i64;
                    for r in &sorted {
                        if !before(r)? {
                            break;
                        }
                        rank += 1;
                    }
                    match of {
                        Sorted::Rank => Value::Int(rank),
                        Sorted::PercentRank => Value::Double(if n == 0 {
                            0.0
                        } else {
                            (rank - 1) as f64 / n as f64
                        }),
                        _ => Value::Double(rank as f64 / (n + 1) as f64),
                    }
                }
            }
        }
    })
}

/// array_agg's answer: NULL over no rows; else the values as one dimension, or, of arrays, each a
/// slice of one more dimension than theirs.
fn collected(ty: Option<ColumnType>, arrays: bool, values: &[Value]) -> Result<Value> {
    if values.is_empty() {
        return Ok(Value::Null);
    }
    let found = values.iter().find_map(super::pipeline::type_of_value);
    let arrays =
        arrays || ty.is_none() && found.is_some_and(|t| super::typed::element_of(t).is_some());
    let ty = match (ty, found) {
        (Some(t), _) => t,
        (None, Some(t)) if arrays => t,
        (None, Some(t)) => super::typed::array_of(t),
        (None, None) => super::typed::array_of(ColumnType::Text(None)),
    };
    let element = super::typed::element_of(ty)
        .ok_or_else(|| Error::new(format!("{} is no array type", ty.pg_name())))?;
    if arrays {
        let mut stacked = crate::arrays::ArrayAgg::new();
        for v in values {
            match v {
                Value::Composed(_, c) => stacked.push(&c.item)?,
                _ => stacked.push(&crate::composed::Item::Null)?,
            }
        }
        // array_agg's result is of its argument's type, and two dimensions make no vector.
        if crate::composed::is_vector(ty) {
            return Err(Error::new(format!(
                "array is not a valid {}",
                ty.pg_name()
            )));
        }
        return Ok(super::pipeline::composed(ty, stacked.finish()?));
    }
    let elements = values
        .iter()
        .map(|v| super::pipeline::part_of(element, v.clone(), false))
        .collect::<Result<Vec<_>>>()?;
    Ok(super::pipeline::composed(
        ty,
        crate::composed::Item::Array {
            dims: vec![elements.len() as i32],
            lower: vec![1],
            elements,
        },
    ))
}

/// The range two ranges both hold, or the multirange two multiranges both hold.
fn range_intersect(a: &Value, b: &Value) -> Result<Value> {
    use crate::composed::{Composed, Item, RangeAnswer};
    let (Value::Composed(ty, x), Value::Composed(_, y)) = (a, b) else {
        return Err(Error::new(format!(
            "function range_intersect_agg({}) does not exist",
            super::pipeline::kind_name(b)
        )));
    };
    let ColumnType::Composed(r) = ty else {
        unreachable!()
    };
    Ok(match (r.def(), &x.item, &y.item) {
        (Composed::Multirange { range, .. }, Item::Multirange(xs), Item::Multirange(ys)) => {
            let both = if xs.is_empty() || ys.is_empty() {
                Vec::new()
            } else {
                crate::composed::multirange_intersect(*range, xs, ys)?
            };
            super::pipeline::composed(*ty, Item::Multirange(both))
        }
        _ => match crate::composed::range_op(*ty, "*", &x.item, &y.item)? {
            RangeAnswer::Range(i) => super::pipeline::composed(*ty, i),
            RangeAnswer::Truth(_) => unreachable!(),
        },
    })
}
