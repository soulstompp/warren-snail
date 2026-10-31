//! The functions a pipeline answers by name, each family as the build takes it: the numbers first,
//! over the integers, numeric and the floats, each form as PostgreSQL 19 computes it; a float's
//! through the C library's functions PostgreSQL calls, under its checks.

use super::pipeline::Value;
use super::typed::NUMERIC;
use crate::catalog::ColumnType;
use crate::numeric::Numeric;
use crate::{Error, Result};

extern "C" {
    fn sin(x: f64) -> f64;
    fn cos(x: f64) -> f64;
    fn tan(x: f64) -> f64;
    fn asin(x: f64) -> f64;
    fn acos(x: f64) -> f64;
    fn atan(x: f64) -> f64;
    fn atan2(y: f64, x: f64) -> f64;
    fn sinh(x: f64) -> f64;
    fn cosh(x: f64) -> f64;
    fn tanh(x: f64) -> f64;
    fn asinh(x: f64) -> f64;
    fn acosh(x: f64) -> f64;
    fn atanh(x: f64) -> f64;
    fn exp(x: f64) -> f64;
    fn log(x: f64) -> f64;
    fn log10(x: f64) -> f64;
    fn cbrt(x: f64) -> f64;
    fn pow(x: f64, y: f64) -> f64;
    fn erf(x: f64) -> f64;
    fn erfc(x: f64) -> f64;
    fn tgamma(x: f64) -> f64;
    fn lgamma(x: f64) -> f64;
}

type F1 = unsafe extern "C" fn(f64) -> f64;
type F2 = unsafe extern "C" fn(f64, f64) -> f64;

/// The C library's functions of a double that PostgreSQL calls, as the machine's libm defines them:
/// looked up in it by name, since the toolchain links copies of its own under some of those names;
/// where the machine has no libm.so.6, the copies linked.
struct Libm {
    sin: F1,
    cos: F1,
    tan: F1,
    asin: F1,
    acos: F1,
    atan: F1,
    atan2: F2,
    sinh: F1,
    cosh: F1,
    tanh: F1,
    asinh: F1,
    acosh: F1,
    atanh: F1,
    exp: F1,
    log: F1,
    log10: F1,
    cbrt: F1,
    pow: F2,
    erf: F1,
    erfc: F1,
    tgamma: F1,
    lgamma: F1,
}

fn libm() -> &'static Libm {
    static LIBM: std::sync::OnceLock<Libm> = std::sync::OnceLock::new();
    LIBM.get_or_init(|| {
        // SAFETY: dlopen reads the NUL-terminated name; the library stays open for the program.
        let h = unsafe { libc::dlopen(c"libm.so.6".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        let found = |name: &std::ffi::CStr| -> *mut libc::c_void {
            if h.is_null() {
                return std::ptr::null_mut();
            }
            // SAFETY: the handle is open and the name NUL-terminated.
            unsafe { libc::dlsym(h, name.as_ptr()) }
        };
        let f1 = |name: &std::ffi::CStr, linked: F1| -> F1 {
            let p = found(name);
            // SAFETY: libm's function of that name takes a double and returns one.
            if p.is_null() {
                linked
            } else {
                unsafe { std::mem::transmute::<*mut libc::c_void, F1>(p) }
            }
        };
        let f2 = |name: &std::ffi::CStr, linked: F2| -> F2 {
            let p = found(name);
            // SAFETY: libm's function of that name takes two doubles and returns one.
            if p.is_null() {
                linked
            } else {
                unsafe { std::mem::transmute::<*mut libc::c_void, F2>(p) }
            }
        };
        Libm {
            sin: f1(c"sin", sin),
            cos: f1(c"cos", cos),
            tan: f1(c"tan", tan),
            asin: f1(c"asin", asin),
            acos: f1(c"acos", acos),
            atan: f1(c"atan", atan),
            atan2: f2(c"atan2", atan2),
            sinh: f1(c"sinh", sinh),
            cosh: f1(c"cosh", cosh),
            tanh: f1(c"tanh", tanh),
            asinh: f1(c"asinh", asinh),
            acosh: f1(c"acosh", acosh),
            atanh: f1(c"atanh", atanh),
            exp: f1(c"exp", exp),
            log: f1(c"log", log),
            log10: f1(c"log10", log10),
            cbrt: f1(c"cbrt", cbrt),
            pow: f2(c"pow", pow),
            erf: f1(c"erf", erf),
            erfc: f1(c"erfc", erfc),
            tgamma: f1(c"tgamma", tgamma),
            lgamma: f1(c"lgamma", lgamma),
        }
    })
}

/// A function of the C library called with errno cleared: its result, and errno after it.
fn errno_of(f: impl FnOnce() -> f64) -> (f64, i32) {
    // SAFETY: errno is the calling thread's own; the functions read only their arguments.
    unsafe {
        *libc::__errno_location() = 0;
        let r = f();
        (r, *libc::__errno_location())
    }
}

/// A function of the C library over one double, which reads only its argument.
fn c1(f: unsafe extern "C" fn(f64) -> f64, x: f64) -> f64 {
    // SAFETY: the function reads only its argument.
    unsafe { f(x) }
}

fn overflow() -> Error {
    Error::new("value out of range: overflow")
}

fn underflow() -> Error {
    Error::new("value out of range: underflow")
}

fn out_of_range() -> Error {
    Error::new("input is out of range")
}

/// The names of the functions of the numbers the snail answers.
pub(super) fn is_number_function(name: &str) -> bool {
    matches!(
        name,
        "abs"
            | "cbrt"
            | "ceil"
            | "ceiling"
            | "floor"
            | "sign"
            | "round"
            | "dround"
            | "trunc"
            | "dtrunc"
            | "sqrt"
            | "exp"
            | "dexp"
            | "ln"
            | "dlog1"
            | "log"
            | "log10"
            | "dlog10"
            | "pow"
            | "power"
            | "degrees"
            | "radians"
            | "pi"
            | "sin"
            | "cos"
            | "tan"
            | "cot"
            | "asin"
            | "acos"
            | "atan"
            | "atan2"
            | "sind"
            | "cosd"
            | "tand"
            | "cotd"
            | "asind"
            | "acosd"
            | "atand"
            | "atan2d"
            | "sinh"
            | "cosh"
            | "tanh"
            | "asinh"
            | "acosh"
            | "atanh"
            | "erf"
            | "erfc"
            | "gamma"
            | "lgamma"
            | "width_bucket"
            | "mod"
            | "gcd"
            | "lcm"
            | "div"
            | "factorial"
            | "scale"
            | "min_scale"
            | "trim_scale"
            | "numeric_sqrt"
            | "numeric_exp"
            | "numeric_ln"
            | "numeric_log"
            | "numeric_div_trunc"
            | "to_hex"
            | "to_oct"
            | "to_bin"
            | "num_nulls"
            | "num_nonnulls"
    )
}

/// An argument as a double precision, as PostgreSQL casts it to one: an integer rounded to it, a
/// numeric through its text, a real exactly, a quoted literal as float8in reads it.
fn double(v: &Value) -> Result<f64> {
    match super::pipeline::float_cast(v.clone(), true)? {
        Value::Double(d) => Ok(d),
        other => Err(Error::new(format!("{other:?} is not a double precision"))),
    }
}

/// An argument as a numeric: an integer or a numeric itself, a quoted literal as numeric_in reads
/// it.
fn numeric(v: &Value) -> Result<Numeric> {
    match v {
        Value::Text(t) => Numeric::parse(t),
        v => v
            .to_numeric()
            .ok_or_else(|| Error::new(format!("{v:?} is not a numeric"))),
    }
}

/// An argument as an integer, a quoted literal as the integer's input reads it.
fn integer(v: &Value) -> Result<i64> {
    match v {
        Value::Int(x) => Ok(*x),
        Value::Text(t) => crate::composed::int_in(t, 64),
        v => Err(Error::new(format!("{v:?} is not an integer"))),
    }
}

/// An integer held to the range of its type, as PostgreSQL's integer functions refuse what passes
/// it.
pub(super) fn held_to(ty: Option<ColumnType>, v: i128) -> Result<Value> {
    let (lo, hi, name) = match ty {
        Some(ColumnType::Int2) => (i128::from(i16::MIN), i128::from(i16::MAX), "smallint"),
        Some(ColumnType::Int4) => (i128::from(i32::MIN), i128::from(i32::MAX), "integer"),
        _ => (i128::from(i64::MIN), i128::from(i64::MAX), "bigint"),
    };
    if v < lo || v > hi {
        return Err(Error::new(format!("{name} out of range")));
    }
    Ok(Value::Int(v as i64))
}

/// The type a call resolves to from its arguments' values, where the step did not resolve it.
fn resolved_by_values(name: &str, args: &[Value]) -> Option<ColumnType> {
    let types: Vec<Option<ColumnType>> = args
        .iter()
        .map(|v| match v {
            Value::Int(x) if i32::try_from(*x).is_ok() => Some(ColumnType::Int4),
            Value::Int(_) => Some(ColumnType::Int8),
            Value::Num(..) | Value::Numeric(_) => Some(NUMERIC),
            Value::Real(_) => Some(ColumnType::Float4),
            Value::Double(_) => Some(ColumnType::Float8),
            _ => None,
        })
        .collect();
    super::typed::numbers(name, &types).ok().flatten().flatten()
}

/// The value of a function of the numbers over its arguments' values: `ret` the type the call
/// resolves to and `types` its arguments', where the step resolved them. None where the snail
/// answers no function of the numbers by that name.
pub(super) fn numbers(
    name: &str,
    args: &[Value],
    types: &[Option<ColumnType>],
    ret: Option<ColumnType>,
) -> Result<Option<Value>> {
    if !is_number_function(name) {
        return Ok(None);
    }
    match name {
        "num_nulls" => {
            return Ok(Some(Value::Int(
                args.iter().filter(|v| v.is_null()).count() as i64,
            )))
        }
        "num_nonnulls" => {
            return Ok(Some(Value::Int(
                args.iter().filter(|v| !v.is_null()).count() as i64,
            )))
        }
        _ => {}
    }
    if args.iter().any(|v| v.is_null()) {
        return Ok(Some(Value::Null));
    }
    let numeric_args = || {
        args.iter()
            .zip(types.iter().chain(std::iter::repeat(&None)))
            .any(|(v, t)| {
                matches!(v, Value::Num(..) | Value::Numeric(_)) || t.is_some_and(|t| t.is_numeric())
            })
            && !args
                .iter()
                .any(|v| matches!(v, Value::Real(_) | Value::Double(_)))
    };
    match name {
        "width_bucket" if !numeric_args() => {
            let a: Vec<f64> = args[..3].iter().map(double).collect::<Result<_>>()?;
            let count = i32::try_from(integer(&args[3])?)
                .map_err(|_| Error::new("integer out of range"))?;
            return Ok(Some(Value::Int(i64::from(width_bucket_float8(
                a[0], a[1], a[2], count,
            )?))));
        }
        "width_bucket" | "scale" | "min_scale" | "factorial" => return numerics(name, args),
        _ => {}
    }
    let ret = match ret.or_else(|| resolved_by_values(name, args)) {
        Some(t) => t,
        None => return Ok(None),
    };
    Ok(Some(match ret {
        ColumnType::Float8 => Value::Double(doubles(name, args)?),
        ColumnType::Float4 => match args {
            [Value::Real(f)] if name == "abs" => Value::Real(f.abs()),
            _ => return Ok(None),
        },
        t if t.is_integer() => integers(name, args, t)?,
        t if t.is_numeric() => return numerics(name, args),
        ColumnType::Text(_) => {
            let v = integer(&args[0])?;
            let wide = match types.first() {
                Some(Some(ColumnType::Int8)) => true,
                Some(Some(ColumnType::Int2 | ColumnType::Int4)) => false,
                _ => i32::try_from(v).is_err(),
            };
            let u = if wide {
                v as u64
            } else {
                u64::from(v as i32 as u32)
            };
            Value::Text(match name {
                "to_hex" => format!("{u:x}"),
                "to_oct" => format!("{u:o}"),
                _ => format!("{u:b}"),
            })
        }
        _ => return Ok(None),
    }))
}

/// A function of the integers: abs, mod, gcd and lcm, each held to the range of the type it
/// returns.
fn integers(name: &str, args: &[Value], ret: ColumnType) -> Result<Value> {
    let a: Vec<i64> = args.iter().map(integer).collect::<Result<_>>()?;
    let v: i128 = match (name, a.as_slice()) {
        ("abs", [x]) => i128::from(*x).abs(),
        ("mod", [_, 0]) => return Err(Error::new("division by zero")),
        ("mod", [_, -1]) => 0,
        ("mod", [x, y]) => i128::from(*x) % i128::from(*y),
        ("gcd", [x, y]) => gcd(i128::from(*x), i128::from(*y)),
        ("lcm", [0, _] | [_, 0]) => 0,
        ("lcm", [x, y]) => {
            let (x, y) = (i128::from(*x), i128::from(*y));
            (x / gcd(x, y) * y).abs()
        }
        _ => return Err(Error::new(format!("{name} of integers is not read"))),
    };
    held_to(Some(ret), v)
}

fn gcd(mut a: i128, mut b: i128) -> i128 {
    a = a.abs();
    b = b.abs();
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// A function of numeric over its arguments, as numeric.c computes it.
fn numerics(name: &str, args: &[Value]) -> Result<Option<Value>> {
    let int = |v: &Value| -> Result<i32> {
        i32::try_from(integer(v)?).map_err(|_| Error::new("integer out of range"))
    };
    match (name, args) {
        ("factorial", [n]) => return Ok(Some(Value::of_numeric(Numeric::factorial(integer(n)?)?))),
        ("round" | "trunc", [x, s]) => {
            let (x, s) = (numeric(x)?, int(s)?);
            return Ok(Some(Value::of_numeric(if name == "round" {
                x.round_to(s)?
            } else {
                x.trunc(s)
            })));
        }
        ("width_bucket", [x, lo, hi, count]) => {
            return Ok(Some(Value::Int(i64::from(Numeric::width_bucket(
                &numeric(x)?,
                &numeric(lo)?,
                &numeric(hi)?,
                int(count)?,
            )?))))
        }
        _ => {}
    }
    let a: Vec<Numeric> = args.iter().map(numeric).collect::<Result<_>>()?;
    Ok(Some(match (name, a.as_slice()) {
        ("scale" | "min_scale", [x]) => {
            let s = if name == "scale" {
                x.scale_of()
            } else {
                x.min_scale()
            };
            return Ok(Some(s.map_or(Value::Null, |s| Value::Int(i64::from(s)))));
        }
        ("abs", [x]) => Value::of_numeric(x.abs()),
        ("ceil" | "ceiling", [x]) => Value::of_numeric(x.ceil()?),
        ("floor", [x]) => Value::of_numeric(x.floor()?),
        ("sign", [x]) => Value::of_numeric(x.sign_of()),
        ("round", [x]) => Value::of_numeric(x.round_to(0)?),
        ("trunc", [x]) => Value::of_numeric(x.trunc(0)),
        ("trim_scale", [x]) => Value::of_numeric(x.trim_scale()),
        ("sqrt" | "numeric_sqrt", [x]) => Value::of_numeric(x.sqrt()?),
        ("exp" | "numeric_exp", [x]) => Value::of_numeric(x.exp()?),
        ("ln" | "numeric_ln", [x]) => Value::of_numeric(x.ln()?),
        ("log" | "log10", [x]) => Value::of_numeric(x.log10()?),
        ("log" | "numeric_log", [b, x]) => Value::of_numeric(Numeric::log(b, x)?),
        ("pow" | "power", [x, y]) => Value::of_numeric(x.power(y)?),
        ("div" | "numeric_div_trunc", [x, y]) => Value::of_numeric(x.div_trunc(y)?),
        ("mod", [x, y]) => Value::of_numeric(x.rem(y)?),
        ("gcd", [x, y]) => Value::of_numeric(x.gcd(y)?),
        ("lcm", [x, y]) => Value::of_numeric(x.lcm(y)?),
        _ => return Ok(None),
    }))
}

/// `float8_mul` and `float8_div` of PostgreSQL: the product or quotient refused where finite
/// operands make an infinity, or numbers not zero make zero.
fn checked_mul(x: f64, y: f64) -> Result<f64> {
    let r = x * y;
    if r.is_infinite() && !x.is_infinite() && !y.is_infinite() {
        return Err(overflow());
    }
    if r == 0.0 && x != 0.0 && y != 0.0 {
        return Err(underflow());
    }
    Ok(r)
}

fn checked_div(x: f64, y: f64) -> Result<f64> {
    if y == 0.0 && !y.is_nan() {
        return Err(Error::new("division by zero"));
    }
    let r = x / y;
    if r.is_infinite() && !x.is_infinite() {
        return Err(overflow());
    }
    if r == 0.0 && x != 0.0 && !y.is_infinite() {
        return Err(underflow());
    }
    Ok(r)
}

/// float.h's `RADIANS_PER_DEGREE`, as the double it is read as.
const RADIANS_PER_DEGREE: f64 = 0.017_453_292_519_943_295;

/// The constants float.c's degree functions divide by, computed as it computes them.
struct Degrees {
    sin_30: f64,
    one_minus_cos_60: f64,
    asin_0_5: f64,
    acos_0_5: f64,
    atan_1_0: f64,
    tan_45: f64,
    cot_45: f64,
}

fn degrees() -> &'static Degrees {
    static DEGREES: std::sync::OnceLock<Degrees> = std::sync::OnceLock::new();
    DEGREES.get_or_init(|| {
        let mut d = Degrees {
            sin_30: c1(libm().sin, std::hint::black_box(30.0) * RADIANS_PER_DEGREE),
            one_minus_cos_60: 1.0 - c1(libm().cos, std::hint::black_box(60.0) * RADIANS_PER_DEGREE),
            asin_0_5: c1(libm().asin, std::hint::black_box(0.5)),
            acos_0_5: c1(libm().acos, std::hint::black_box(0.5)),
            atan_1_0: c1(libm().atan, std::hint::black_box(1.0)),
            tan_45: 0.0,
            cot_45: 0.0,
        };
        let forty_five = std::hint::black_box(45.0);
        d.tan_45 = sind_q1(&d, forty_five) / cosd_q1(&d, forty_five);
        d.cot_45 = cosd_q1(&d, forty_five) / sind_q1(&d, forty_five);
        d
    })
}

fn asind_q1(d: &Degrees, x: f64) -> f64 {
    if x <= 0.5 {
        (c1(libm().asin, x) / d.asin_0_5) * 30.0
    } else {
        90.0 - (c1(libm().acos, x) / d.acos_0_5) * 60.0
    }
}

fn acosd_q1(d: &Degrees, x: f64) -> f64 {
    if x <= 0.5 {
        90.0 - (c1(libm().asin, x) / d.asin_0_5) * 30.0
    } else {
        (c1(libm().acos, x) / d.acos_0_5) * 60.0
    }
}

fn sind_0_to_30(d: &Degrees, x: f64) -> f64 {
    (c1(libm().sin, x * RADIANS_PER_DEGREE) / d.sin_30) / 2.0
}

fn cosd_0_to_60(d: &Degrees, x: f64) -> f64 {
    1.0 - ((1.0 - c1(libm().cos, x * RADIANS_PER_DEGREE)) / d.one_minus_cos_60) / 2.0
}

fn sind_q1(d: &Degrees, x: f64) -> f64 {
    if x <= 30.0 {
        sind_0_to_30(d, x)
    } else {
        cosd_0_to_60(d, 90.0 - x)
    }
}

fn cosd_q1(d: &Degrees, x: f64) -> f64 {
    if x <= 60.0 {
        cosd_0_to_60(d, x)
    } else {
        sind_0_to_30(d, 90.0 - x)
    }
}

/// An angle in degrees brought into the first quadrant, as dsind, dcosd, dtand and dcotd bring it:
/// the angle, and the sign each of the four gives it.
fn quadrant(x: f64) -> Result<(f64, [f64; 4])> {
    if x.is_infinite() {
        return Err(out_of_range());
    }
    // The signs of sin, cos, tan and cot, in that order.
    let mut sign = [1.0f64; 4];
    let mut x = x % 360.0;
    if x < 0.0 {
        x = -x;
        for s in [0, 2, 3] {
            sign[s] = -sign[s];
        }
    }
    if x > 180.0 {
        x = 360.0 - x;
        for s in [0, 2, 3] {
            sign[s] = -sign[s];
        }
    }
    if x > 90.0 {
        x = 180.0 - x;
        for s in [1, 2, 3] {
            sign[s] = -sign[s];
        }
    }
    Ok((x, sign))
}

fn finite_result(r: f64) -> Result<f64> {
    if r.is_infinite() {
        return Err(overflow());
    }
    Ok(r)
}

/// A function of double precision, as float.c computes it.
fn doubles(name: &str, args: &[Value]) -> Result<f64> {
    let a: Vec<f64> = args.iter().map(double).collect::<Result<_>>()?;
    let x = a.first().copied().unwrap_or(0.0);
    let nan = f64::NAN;
    Ok(match (name, a.as_slice()) {
        ("pi", []) => std::f64::consts::PI,
        ("abs", [_]) => x.abs(),
        ("ceil" | "ceiling", [_]) => x.ceil(),
        ("floor", [_]) => x.floor(),
        ("round" | "dround", [_]) => x.round_ties_even(),
        ("sign", [_]) => {
            if x > 0.0 {
                1.0
            } else if x < 0.0 {
                -1.0
            } else {
                0.0
            }
        }
        ("trunc" | "dtrunc", [_]) => {
            if x >= 0.0 {
                x.floor()
            } else {
                -(-x).floor()
            }
        }
        ("sqrt", [_]) => {
            if x < 0.0 {
                return Err(Error::new("cannot take square root of a negative number"));
            }
            let r = x.sqrt();
            if r.is_infinite() && !x.is_infinite() {
                return Err(overflow());
            }
            if r == 0.0 && x != 0.0 {
                return Err(underflow());
            }
            r
        }
        ("cbrt", [_]) => {
            let r = c1(libm().cbrt, x);
            if r.is_infinite() && !x.is_infinite() {
                return Err(overflow());
            }
            if r == 0.0 && x != 0.0 {
                return Err(underflow());
            }
            r
        }
        ("pow" | "power", [x, y]) => dpow(*x, *y)?,
        ("exp" | "dexp", [_]) => {
            if x.is_nan() {
                x
            } else if x.is_infinite() {
                if x > 0.0 {
                    x
                } else {
                    0.0
                }
            } else {
                // SAFETY: exp reads only its argument.
                let (r, errno) = errno_of(|| unsafe { (libm().exp)(x) });
                if errno == libc::ERANGE {
                    return Err(if r != 0.0 { overflow() } else { underflow() });
                }
                if r.is_infinite() {
                    return Err(overflow());
                }
                if r == 0.0 {
                    return Err(underflow());
                }
                r
            }
        }
        ("ln" | "dlog1" | "log" | "log10" | "dlog10", [_]) => {
            if x == 0.0 {
                return Err(Error::new("cannot take logarithm of zero"));
            }
            if x < 0.0 {
                return Err(Error::new("cannot take logarithm of a negative number"));
            }
            let r = if matches!(name, "ln" | "dlog1") {
                c1(libm().log, x)
            } else {
                c1(libm().log10, x)
            };
            if r.is_infinite() && !x.is_infinite() {
                return Err(overflow());
            }
            if r == 0.0 && x != 1.0 {
                return Err(underflow());
            }
            r
        }
        ("acos" | "asin", [_]) => {
            if x.is_nan() {
                return Ok(nan);
            }
            if !(-1.0..=1.0).contains(&x) {
                return Err(out_of_range());
            }
            finite_result(if name == "acos" {
                c1(libm().acos, x)
            } else {
                c1(libm().asin, x)
            })?
        }
        ("atan", [_]) => {
            if x.is_nan() {
                return Ok(nan);
            }
            finite_result(c1(libm().atan, x))?
        }
        ("atan2", [y, x]) => {
            if x.is_nan() || y.is_nan() {
                return Ok(nan);
            }
            // SAFETY: atan2 reads only its arguments.
            finite_result(unsafe { (libm().atan2)(*y, *x) })?
        }
        ("cos" | "sin" | "tan" | "cot", [_]) => {
            if x.is_nan() {
                return Ok(nan);
            }
            let f = match name {
                "cos" => libm().cos,
                "sin" => libm().sin,
                _ => libm().tan,
            };
            // SAFETY: the function reads only its argument.
            let (r, errno) = errno_of(|| unsafe { f(x) });
            if errno != 0 || x.is_infinite() {
                return Err(out_of_range());
            }
            match name {
                "cot" => 1.0 / r,
                "tan" => r,
                _ => finite_result(r)?,
            }
        }
        ("acosd" | "asind", [_]) => {
            if x.is_nan() {
                return Ok(nan);
            }
            let d = degrees();
            if !(-1.0..=1.0).contains(&x) {
                return Err(out_of_range());
            }
            finite_result(match (name, x >= 0.0) {
                ("acosd", true) => acosd_q1(d, x),
                ("acosd", false) => 90.0 + asind_q1(d, -x),
                (_, true) => asind_q1(d, x),
                (_, false) => -asind_q1(d, -x),
            })?
        }
        ("atand", [_]) => {
            if x.is_nan() {
                return Ok(nan);
            }
            let d = degrees();
            finite_result((c1(libm().atan, x) / d.atan_1_0) * 45.0)?
        }
        ("atan2d", [y, x]) => {
            if x.is_nan() || y.is_nan() {
                return Ok(nan);
            }
            let d = degrees();
            // SAFETY: atan2 reads only its arguments.
            finite_result((unsafe { (libm().atan2)(*y, *x) } / d.atan_1_0) * 45.0)?
        }
        ("sind" | "cosd" | "tand" | "cotd", [_]) => {
            if x.is_nan() {
                return Ok(nan);
            }
            let (x, sign) = quadrant(x)?;
            let d = degrees();
            match name {
                "sind" => finite_result(sign[0] * sind_q1(d, x))?,
                "cosd" => finite_result(sign[1] * cosd_q1(d, x))?,
                "tand" => {
                    let r = sign[2] * ((sind_q1(d, x) / cosd_q1(d, x)) / d.tan_45);
                    if r == 0.0 {
                        0.0
                    } else {
                        r
                    }
                }
                _ => {
                    let r = sign[3] * ((cosd_q1(d, x) / sind_q1(d, x)) / d.cot_45);
                    if r == 0.0 {
                        0.0
                    } else {
                        r
                    }
                }
            }
        }
        ("degrees", [_]) => checked_div(x, RADIANS_PER_DEGREE)?,
        ("radians", [_]) => checked_mul(x, RADIANS_PER_DEGREE)?,
        ("sinh", [_]) => {
            // SAFETY: sinh reads only its argument.
            let (r, errno) = errno_of(|| unsafe { (libm().sinh)(x) });
            if errno == libc::ERANGE {
                if x < 0.0 {
                    f64::NEG_INFINITY
                } else {
                    f64::INFINITY
                }
            } else {
                r
            }
        }
        ("cosh", [_]) => {
            // SAFETY: cosh reads only its argument.
            let (r, errno) = errno_of(|| unsafe { (libm().cosh)(x) });
            let r = if errno == libc::ERANGE {
                f64::INFINITY
            } else {
                r
            };
            if r == 0.0 {
                return Err(underflow());
            }
            r
        }
        ("tanh", [_]) => finite_result(c1(libm().tanh, x))?,
        ("asinh", [_]) => c1(libm().asinh, x),
        ("acosh", [_]) => {
            if x < 1.0 {
                return Err(out_of_range());
            }
            c1(libm().acosh, x)
        }
        ("atanh", [_]) => {
            if x < -1.0 || x > 1.0 {
                return Err(out_of_range());
            }
            if x == -1.0 {
                f64::NEG_INFINITY
            } else if x == 1.0 {
                f64::INFINITY
            } else {
                c1(libm().atanh, x)
            }
        }
        ("erf", [_]) => finite_result(c1(libm().erf, x))?,
        ("erfc", [_]) => finite_result(c1(libm().erfc, x))?,
        ("gamma", [_]) => {
            if x.is_nan() {
                x
            } else if x.is_infinite() {
                if x < 0.0 {
                    return Err(overflow());
                }
                x
            } else {
                // SAFETY: tgamma reads only its argument.
                let (r, errno) = errno_of(|| unsafe { (libm().tgamma)(x) });
                if errno != 0 || r.is_infinite() || r.is_nan() {
                    return Err(if r != 0.0 { overflow() } else { underflow() });
                }
                if r == 0.0 {
                    return Err(underflow());
                }
                r
            }
        }
        ("lgamma", [_]) => {
            // SAFETY: lgamma reads only its argument.
            let (r, errno) = errno_of(|| unsafe { (libm().lgamma)(x) });
            if errno == libc::ERANGE || (r.is_infinite() && !x.is_infinite()) {
                return Err(overflow());
            }
            r
        }
        _ => {
            return Err(Error::new(format!(
                "{name} of double precision is not read"
            )))
        }
    })
}

/// `dpow`: x raised to y, as float.c gives it over NaN, the infinities and the C library's errors.
fn dpow(x: f64, y: f64) -> Result<f64> {
    if x.is_nan() {
        return Ok(if y.is_nan() || y != 0.0 {
            f64::NAN
        } else {
            1.0
        });
    }
    if y.is_nan() {
        return Ok(if x != 1.0 { f64::NAN } else { 1.0 });
    }
    if x == 0.0 && y < 0.0 {
        return Err(Error::new("zero raised to a negative power is undefined"));
    }
    if x < 0.0 && y.floor() != y {
        return Err(Error::new(
            "a negative number raised to a non-integer power yields a complex result",
        ));
    }
    if y.is_infinite() {
        let ax = x.abs();
        return Ok(if ax == 1.0 {
            1.0
        } else if y > 0.0 {
            if ax > 1.0 {
                y
            } else {
                0.0
            }
        } else if ax > 1.0 {
            0.0
        } else {
            -y
        });
    }
    if x.is_infinite() {
        return Ok(if y == 0.0 {
            1.0
        } else if x > 0.0 {
            if y > 0.0 {
                x
            } else {
                0.0
            }
        } else {
            let half = y / 2.0;
            let odd = half.floor() != half;
            if y > 0.0 {
                if odd {
                    x
                } else {
                    -x
                }
            } else if odd {
                -0.0
            } else {
                0.0
            }
        });
    }
    // SAFETY: pow reads only its arguments.
    let (r, errno) = errno_of(|| unsafe { (libm().pow)(x, y) });
    if errno == libc::EDOM || r.is_nan() {
        if x == 0.0 {
            return Ok(0.0);
        }
        let ax = x.abs();
        if ax == 1.0 {
            return Ok(1.0);
        }
        return Err(if (y >= 0.0 && ax > 1.0) || (y < 0.0 && ax < 1.0) {
            overflow()
        } else {
            underflow()
        });
    }
    if errno == libc::ERANGE {
        return Err(if r != 0.0 { overflow() } else { underflow() });
    }
    if r.is_infinite() {
        return Err(overflow());
    }
    if r == 0.0 && x != 0.0 {
        return Err(underflow());
    }
    Ok(r)
}

/// `width_bucket` of double precision, as float.c computes the bucket.
pub(super) fn width_bucket_float8(operand: f64, b1: f64, b2: f64, count: i32) -> Result<i32> {
    if count <= 0 {
        return Err(Error::new("count must be greater than zero"));
    }
    if b1.is_nan() || b2.is_nan() {
        return Err(Error::new("lower and upper bounds cannot be NaN"));
    }
    if b1.is_infinite() || b2.is_infinite() {
        return Err(Error::new("lower and upper bounds must be finite"));
    }
    let c = f64::from(count);
    let one_more = || {
        count
            .checked_add(1)
            .ok_or_else(|| Error::new("integer out of range"))
    };
    if b1 < b2 {
        if operand.is_nan() || operand >= b2 {
            return one_more();
        }
        if operand < b1 {
            return Ok(0);
        }
        let q = if !(b2 - b1).is_infinite() {
            (operand - b1) / (b2 - b1)
        } else {
            (operand / 2.0 - b1 / 2.0) / (b2 / 2.0 - b1 / 2.0)
        };
        let r = ((c * q) as i32).min(count - 1);
        Ok(r + 1)
    } else if b1 > b2 {
        if operand.is_nan() || operand > b1 {
            return Ok(0);
        }
        if operand <= b2 {
            return one_more();
        }
        let q = if !(b1 - b2).is_infinite() {
            (b1 - operand) / (b1 - b2)
        } else {
            (b1 / 2.0 - operand / 2.0) / (b1 / 2.0 - b2 / 2.0)
        };
        let r = ((c * q) as i32).min(count - 1);
        Ok(r + 1)
    } else {
        Err(Error::new("lower bound cannot equal upper bound"))
    }
}

/// The interval of every field, the type the time functions return an interval in.
pub(super) fn full_interval() -> ColumnType {
    ColumnType::Interval {
        range: crate::times::INTERVAL_FULL_RANGE,
        precision: None,
    }
}

/// The names of the functions of time the snail answers.
pub(super) fn is_time_function(name: &str) -> bool {
    matches!(
        name,
        "now"
            | "transaction_timestamp"
            | "statement_timestamp"
            | "clock_timestamp"
            | "timeofday"
            | "current_timestamp"
            | "current_date"
            | "localtimestamp"
            | "current_time"
            | "localtime"
            | "date_part"
            | "date_trunc"
            | "age"
            | "make_date"
            | "make_time"
            | "make_timestamp"
            | "make_timestamptz"
            | "make_interval"
            | "date_bin"
            | "to_timestamp"
            | "isfinite"
            | "justify_days"
            | "justify_hours"
            | "justify_interval"
            | "date_add"
            | "date_subtract"
            | "timestamp"
            | "timestamptz"
    )
}

/// An argument as an integer of 32 bits.
fn int32(v: &Value) -> Result<i32> {
    i32::try_from(integer(v)?).map_err(|_| Error::new("integer out of range"))
}

/// An argument as an instant, a timestamptz's where `tz`: a timestamp or a timestamptz as itself,
/// a date at its midnight, a quoted literal as the type's input reads it.
fn instant(v: &Value, tz: bool, now: i64) -> Result<i64> {
    match v {
        Value::Scalar(t, k)
            if matches!(
                super::clock::base(*t),
                ColumnType::Timestamp(_) | ColumnType::Timestamptz
            ) =>
        {
            Ok(*k)
        }
        Value::Scalar(ColumnType::Date, d) if tz => crate::times::date_timestamptz(*d as i32),
        Value::Scalar(ColumnType::Date, d) => crate::times::date_timestamp(*d as i32),
        Value::Text(s) if tz => crate::times::timestamptz_in(s, None, now),
        Value::Text(s) => crate::times::timestamp_in(s, None, now),
        v => Err(Error::new(format!(
            "{} is no instant",
            super::pipeline::kind_name(v)
        ))),
    }
}

/// An argument as an interval, a quoted literal as interval's input reads it.
fn interval_arg(v: &Value) -> Result<crate::times::Interval> {
    match v {
        Value::Interval(i) => Ok(*i),
        Value::Text(s) => crate::times::interval_in(s, crate::times::INTERVAL_FULL_RANGE, None),
        v => Err(Error::new(format!(
            "{} is no interval",
            super::pipeline::kind_name(v)
        ))),
    }
}

fn timestamp(t: i64) -> Value {
    Value::Scalar(ColumnType::Timestamp(None), t)
}

fn timestamptz(t: i64) -> Value {
    Value::Scalar(ColumnType::Timestamptz, t)
}

/// The machine's clock as PostgreSQL's timeofday writes it, under UTC.
fn timeofday(micros: i64) -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let (year, month, day, hour, min, sec, usec) = crate::times::civil(micros);
    let days = micros.div_euclid(crate::times::MICROS_PER_DAY);
    format!(
        "{} {} {day:02} {hour:02}:{min:02}:{sec:02}.{usec:06} {year} UTC",
        DAYS[days.rem_euclid(7) as usize],
        MONTHS[(month - 1) as usize]
    )
}

/// The value of a function of time over its arguments' values, `now` the statement's clock: `ret`
/// the type the call resolves to, where the step resolved it. None where the snail answers no
/// function of time by that name and those arguments.
pub(super) fn time(
    name: &str,
    args: &[Value],
    ret: Option<ColumnType>,
    now: i64,
) -> Result<Option<Value>> {
    use crate::times as t;
    if !is_time_function(name) {
        return Ok(None);
    }
    let day = t::MICROS_PER_DAY;
    match (name, args) {
        ("now" | "transaction_timestamp" | "statement_timestamp" | "current_timestamp", []) => {
            return Ok(Some(timestamptz(now)))
        }
        ("clock_timestamp", []) => return Ok(Some(timestamptz(crate::scalars::wall_clock()))),
        ("timeofday", []) => return Ok(Some(Value::Text(timeofday(crate::scalars::wall_clock())))),
        ("current_date", []) => {
            return Ok(Some(Value::Scalar(ColumnType::Date, now.div_euclid(day))))
        }
        ("localtimestamp", []) => return Ok(Some(timestamp(now))),
        ("localtime", []) => {
            return Ok(Some(Value::Scalar(
                ColumnType::Time(None),
                now.rem_euclid(day),
            )))
        }
        ("current_time", []) => {
            return Ok(Some(Value::Scalar(
                ColumnType::Timetz(None),
                crate::scalars::timetz_key(now.rem_euclid(day), 0),
            )))
        }
        _ => {}
    }
    if args.iter().any(|v| v.is_null()) {
        return Ok(Some(Value::Null));
    }
    let tz = |v: &Value| matches!(v, Value::Scalar(t, _) if super::clock::base(*t) == ColumnType::Timestamptz);
    if let ("age", [Value::Scalar(ColumnType::Xid, _)]) = (name, args) {
        return Err(crate::functions::refusal("age", "sessions"));
    }
    Ok(Some(match (name, args) {
        ("date_part", [unit, x]) => {
            let unit = text_arg(unit)?;
            let f = match x {
                Value::Scalar(ty, k) => match super::clock::base(*ty) {
                    ColumnType::Date => t::date_part_date(&unit, *k as i32)?,
                    ColumnType::Timestamp(_) => t::date_part_timestamp(&unit, *k)?,
                    ColumnType::Timestamptz => t::date_part_timestamptz(&unit, *k)?,
                    ColumnType::Time(_) => t::date_part_time(&unit, *k)?,
                    ColumnType::Timetz(_) => {
                        let (time, zone) = crate::scalars::timetz_parts(*k);
                        t::date_part_timetz(&unit, time, zone)?
                    }
                    _ => return Ok(None),
                },
                Value::Interval(v) => t::date_part_interval(&unit, *v)?,
                _ => return Ok(None),
            };
            f.map_or(Value::Null, Value::Double)
        }
        ("date_trunc", [unit, x]) => {
            let unit = text_arg(unit)?;
            match x {
                Value::Interval(v) => Value::Interval(t::interval_trunc(&unit, *v)?),
                x if tz(x) || matches!(x, Value::Scalar(ColumnType::Date, _)) => {
                    timestamptz(t::timestamptz_trunc(&unit, instant(x, true, now)?)?)
                }
                x => timestamp(t::timestamp_trunc(&unit, instant(x, false, now)?)?),
            }
        }
        ("date_trunc", [unit, x, zone]) => timestamptz(t::timestamptz_trunc_zone(
            &text_arg(unit)?,
            instant(x, true, now)?,
            &text_arg(zone)?,
        )?),
        ("age", [a, b]) => {
            let wide = tz(a) || tz(b) || ret.is_none();
            Value::Interval(t::timestamp_age(
                instant(a, wide, now)?,
                instant(b, wide, now)?,
            )?)
        }
        ("age", [x]) => {
            let midnight = now.div_euclid(day) * day;
            Value::Interval(t::timestamp_age(midnight, instant(x, tz(x), now)?)?)
        }
        ("make_date", [y, m, d]) => {
            let d = t::make_date(int32(y)?, int32(m)?, int32(d)?)?;
            Value::Scalar(ColumnType::Date, i64::from(d))
        }
        ("make_time", [h, m, s]) => Value::Scalar(
            ColumnType::Time(None),
            t::make_time(int32(h)?, int32(m)?, double(s)?)?,
        ),
        ("make_timestamp", [y, mo, d, h, mi, s]) => timestamp(t::make_timestamp(
            int32(y)?,
            int32(mo)?,
            int32(d)?,
            int32(h)?,
            int32(mi)?,
            double(s)?,
        )?),
        ("make_timestamptz", [y, mo, d, h, mi, s, rest @ ..]) if rest.len() <= 1 => {
            let zone = rest.first().map(text_arg).transpose()?;
            timestamptz(t::make_timestamptz(
                int32(y)?,
                int32(mo)?,
                int32(d)?,
                int32(h)?,
                int32(mi)?,
                double(s)?,
                zone.as_deref(),
            )?)
        }
        ("make_interval", a) if a.len() <= 7 => {
            let int = |i: usize| a.get(i).map_or(Ok(0), int32);
            Value::Interval(t::make_interval(
                int(0)?,
                int(1)?,
                int(2)?,
                int(3)?,
                int(4)?,
                int(5)?,
                a.get(6).map_or(Ok(0.0), double)?,
            )?)
        }
        ("date_bin", [stride, source, origin]) => {
            let wide = match ret {
                Some(ColumnType::Timestamp(_)) => false,
                Some(_) => true,
                None => tz(source) || tz(origin),
            };
            let r = t::timestamp_bin(
                interval_arg(stride)?,
                instant(source, wide, now)?,
                instant(origin, wide, now)?,
            )?;
            if wide {
                timestamptz(r)
            } else {
                timestamp(r)
            }
        }
        ("to_timestamp", [f]) if !matches!(f, Value::Text(_)) => {
            timestamptz(t::float8_timestamptz(double(f)?)?)
        }
        ("isfinite", [x]) => Value::Bool(match x {
            Value::Interval(v) => v.is_finite(),
            Value::Scalar(ColumnType::Date, d) => {
                *d != i64::from(i32::MIN) && *d != i64::from(i32::MAX)
            }
            Value::Scalar(_, k) => *k != i64::MIN && *k != i64::MAX,
            _ => return Ok(None),
        }),
        ("justify_days", [v]) => Value::Interval(t::interval_justify_days(interval_arg(v)?)?),
        ("justify_hours", [v]) => Value::Interval(t::interval_justify_hours(interval_arg(v)?)?),
        ("justify_interval", [v]) => {
            Value::Interval(t::interval_justify_interval(interval_arg(v)?)?)
        }
        ("date_add", [x, v]) => timestamptz(t::timestamptz_pl_interval(
            instant(x, true, now)?,
            interval_arg(v)?,
        )?),
        ("date_subtract", [x, v]) => timestamptz(t::timestamptz_mi_interval(
            instant(x, true, now)?,
            interval_arg(v)?,
        )?),
        ("timestamp", [Value::Scalar(ColumnType::Date, d), Value::Scalar(ty, k)])
            if super::clock::base(*ty) == ColumnType::Time(None) =>
        {
            timestamp(t::datetime_pl(*d as i32, *k)?)
        }
        ("timestamptz", [Value::Scalar(ColumnType::Date, d), Value::Scalar(ty, k)]) => {
            match super::clock::base(*ty) {
                ColumnType::Time(_) => timestamptz(t::datetime_pl(*d as i32, *k)?),
                ColumnType::Timetz(_) => {
                    let (time, zone) = crate::scalars::timetz_parts(*k);
                    timestamptz(t::datetimetz_pl(*d as i32, time, zone)?)
                }
                _ => return Ok(None),
            }
        }
        _ => return Ok(None),
    }))
}

/// `extract(field FROM x)`, as numeric.
pub(super) fn extract(unit: &str, x: &Value) -> Result<Option<Value>> {
    use crate::times as t;
    let n = match x {
        Value::Null => return Ok(Some(Value::Null)),
        Value::Scalar(ty, k) => match super::clock::base(*ty) {
            ColumnType::Date => t::extract_date(unit, *k as i32)?,
            ColumnType::Timestamp(_) => t::extract_timestamp(unit, *k)?,
            ColumnType::Timestamptz => t::extract_timestamptz(unit, *k)?,
            ColumnType::Time(_) => t::extract_time(unit, *k)?,
            ColumnType::Timetz(_) => {
                let (time, zone) = crate::scalars::timetz_parts(*k);
                t::extract_timetz(unit, time, zone)?
            }
            _ => return Ok(None),
        },
        Value::Interval(v) => t::extract_interval(unit, *v)?,
        _ => return Ok(None),
    };
    Ok(Some(n.map_or(Value::Null, Value::of_numeric)))
}

/// `(s1, e1) OVERLAPS (s2, e2)` over values of time, an end given as an interval taken from its
/// start.
pub(super) fn overlaps(v: [Value; 4], now: i64) -> Result<Value> {
    use crate::times as t;
    let [s1, e1, s2, e2] = v;
    let kind = [&s1, &e1, &s2, &e2]
        .into_iter()
        .find_map(super::clock::time_of)
        .filter(|k| !matches!(k, ColumnType::Interval { .. }));
    let key = |v: &Value| -> Result<Option<i128>> {
        Ok(match v {
            Value::Null => None,
            Value::Scalar(ColumnType::Date, d) if kind == Some(ColumnType::Date) => {
                Some(i128::from(*d))
            }
            // A timetz's key orders it as timetz_cmp does, a time's is its microseconds.
            Value::Scalar(ty, k)
                if matches!(
                    super::clock::base(*ty),
                    ColumnType::Timetz(_) | ColumnType::Time(_)
                ) =>
            {
                Some(i128::from(*k))
            }
            v => Some(i128::from(instant(
                v,
                kind == Some(ColumnType::Timestamptz),
                now,
            )?)),
        })
    };
    let end = |s: &Value, e: &Value| -> Result<Option<i128>> {
        match e {
            Value::Interval(i) => {
                if s.is_null() {
                    return Ok(None);
                }
                let r = match kind {
                    Some(ColumnType::Time(_)) => {
                        let Value::Scalar(_, k) = s else {
                            return Ok(None);
                        };
                        i128::from(t::time_pl_interval(*k, *i)?)
                    }
                    Some(ColumnType::Timestamptz) => {
                        i128::from(t::timestamptz_pl_interval(instant(s, true, now)?, *i)?)
                    }
                    _ => i128::from(t::timestamp_pl_interval(instant(s, false, now)?, *i)?),
                };
                Ok(Some(r))
            }
            e => key(e),
        }
    };
    let r = crate::times::overlaps(key(&s1)?, end(&s1, &e1)?, key(&s2)?, end(&s2, &e2)?);
    Ok(r.map_or(Value::Null, Value::Bool))
}

/// The unit an EXTRACT names, as its text.
pub(super) fn field_unit(field: &sqlparser::ast::DateTimeField) -> String {
    field.to_string().trim_matches('\'').to_lowercase()
}

/// The parameters of make_interval, in their order: each given by its place or its name, each
/// other 0.
const INTERVAL_PARAMETERS: [&str; 7] =
    ["years", "months", "weeks", "days", "hours", "mins", "secs"];

/// make_interval's seven arguments from a call that gives them by place or by name.
pub(super) fn named_interval(
    f: &sqlparser::ast::Function,
    mut ev: impl FnMut(&sqlparser::ast::Expr) -> Result<Value>,
) -> Result<Vec<Value>> {
    use sqlparser::ast::{FunctionArg, FunctionArgExpr, FunctionArguments};
    let mut out: Vec<Value> = (0..6)
        .map(|_| Value::Int(0))
        .chain([Value::Double(0.0)])
        .collect();
    let list = match &f.args {
        FunctionArguments::None => return Ok(out),
        FunctionArguments::List(l) => &l.args,
        _ => return Err(Error::new(format!("{f}: its arguments are not read"))),
    };
    let mut at = 0;
    for a in list {
        let (place, x) = match a {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(x)) => {
                at += 1;
                (at - 1, x)
            }
            FunctionArg::Named {
                name,
                arg: FunctionArgExpr::Expr(x),
                ..
            }
            | FunctionArg::ExprNamed {
                name: sqlparser::ast::Expr::Identifier(name),
                arg: FunctionArgExpr::Expr(x),
                ..
            } => {
                let n = crate::catalog::ident(name);
                let place = INTERVAL_PARAMETERS
                    .iter()
                    .position(|p| *p == n)
                    .ok_or_else(|| {
                        Error::new(format!("function make_interval({n} => …) does not exist"))
                    })?;
                (place, x)
            }
            _ => return Err(Error::new(format!("{f}: its arguments are not read"))),
        };
        if place >= out.len() {
            return Err(Error::new(format!(
                "{f}: make_interval takes seven arguments"
            )));
        }
        out[place] = ev(x)?;
    }
    Ok(out)
}

/// The names of the functions of strings the snail answers.
pub(super) fn is_string_function(name: &str) -> bool {
    matches!(
        name,
        "length"
            | "char_length"
            | "character_length"
            | "textlen"
            | "octet_length"
            | "bit_length"
            | "ascii"
            | "chr"
            | "initcap"
            | "left"
            | "right"
            | "lpad"
            | "rpad"
            | "ltrim"
            | "rtrim"
            | "btrim"
            | "substr"
            | "strpos"
            | "repeat"
            | "replace"
            | "reverse"
            | "split_part"
            | "translate"
            | "concat"
            | "concat_ws"
            | "quote_ident"
            | "quote_literal"
            | "quote_nullable"
            | "format"
            | "unistr"
            | "casefold"
            | "like"
            | "notlike"
            | "starts_with"
            | "md5"
            | "sha224"
            | "sha256"
            | "sha384"
            | "sha512"
            | "crc32"
            | "crc32c"
            | "encode"
            | "decode"
            | "get_byte"
            | "set_byte"
            | "get_bit"
            | "set_bit"
            | "bit_count"
            | "convert"
            | "convert_from"
            | "convert_to"
            | "to_ascii"
            | "pg_encoding_to_char"
            | "pg_char_to_encoding"
            | "pg_encoding_max_length"
    )
}

/// An argument's text: a text's, a name's, a bpchar's without its trailing blanks, as each casts to
/// text; a quoted literal's.
fn text_arg(v: &Value) -> Result<String> {
    match v {
        Value::Text(t) => Ok(t.clone()),
        Value::Str(ty, s) => Ok(crate::strings::text_of(*ty, s).to_string()),
        v => Err(Error::new(format!(
            "{} is not text",
            super::pipeline::kind_name(v)
        ))),
    }
}

/// A value as its type's output function writes it, which concat and format read: a boolean's
/// `t` or `f`.
fn output_text(v: &Value) -> Option<String> {
    match v {
        Value::Bool(b) => Some(if *b { "t" } else { "f" }.to_string()),
        v => v.text(),
    }
}

/// The keywords of PostgreSQL 19's grammar that are not unreserved, which quote_ident quotes.
fn reserved() -> &'static std::collections::HashSet<&'static str> {
    static RESERVED: std::sync::OnceLock<std::collections::HashSet<&'static str>> =
        std::sync::OnceLock::new();
    RESERVED.get_or_init(|| include_str!("../keywords.txt").lines().collect())
}

/// A name as PostgreSQL's quote_identifier writes it: bare where it is lower-case letters, digits
/// and underscores, begun by a letter or underscore, and no keyword but an unreserved one; else in
/// double quotes, each double quote doubled.
pub(super) fn quote_ident(s: &str) -> String {
    let safe = s
        .bytes()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c == b'_')
        && s.bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
        && !reserved().contains(s);
    if safe {
        s.to_string()
    } else {
        format!("\"{}\"", s.replace('"', "\"\""))
    }
}

/// A text as PostgreSQL's quote_literal writes it: in single quotes, each quote and backslash
/// doubled, and an E before where it holds a backslash.
pub(super) fn quote_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 3);
    if s.contains('\\') {
        out.push('E');
    }
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' || ch == '\\' {
            out.push(ch);
        }
        out.push(ch);
    }
    out.push('\'');
    out
}

/// `text_substring`: the characters from `start`, counted from 1, `length` of them where given.
fn substring<T: Clone>(chars: &[T], start: i64, length: Option<i64>) -> Result<Vec<T>> {
    let s1 = start.max(1);
    let end = match length {
        None => chars.len() as i64 + 1,
        Some(l) if l < 0 => return Err(Error::new("negative substring length not allowed")),
        Some(l) => match i32::try_from(start)
            .ok()
            .and_then(|s| s.checked_add(i32::try_from(l).ok()?))
        {
            Some(e) => i64::from(e),
            None => chars.len() as i64 + 1,
        },
    };
    if end <= 1 || s1 > chars.len() as i64 {
        return Ok(Vec::new());
    }
    let to = (end - 1).min(chars.len() as i64);
    Ok(chars[(s1 - 1) as usize..to as usize].to_vec())
}

/// `format`: the template's conversions %s, %I and %L, each with its argument's place and width.
fn format(template: &str, args: &[Value]) -> Result<String> {
    let unterminated = || Error::new("unterminated format() type specifier");
    let chars: Vec<char> = template.chars().collect();
    let mut out = String::new();
    let mut next = 1usize;
    let mut i = 0;
    let digits = |i: &mut usize| -> Result<Option<i32>> {
        let mut found = false;
        let mut v: i32 = 0;
        while *i < chars.len() && chars[*i].is_ascii_digit() {
            v = v
                .checked_mul(10)
                .and_then(|v| v.checked_add(chars[*i] as i32 - '0' as i32))
                .ok_or_else(|| Error::new("number is out of range"))?;
            *i += 1;
            found = true;
            if *i >= chars.len() {
                return Err(unterminated());
            }
        }
        Ok(found.then_some(v))
    };
    let arg = |n: usize| -> Result<&Value> {
        n.checked_sub(1)
            .and_then(|n| args.get(n))
            .ok_or_else(|| Error::new("too few arguments for format()"))
    };
    while i < chars.len() {
        if chars[i] != '%' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        i += 1;
        if i >= chars.len() {
            return Err(unterminated());
        }
        if chars[i] == '%' {
            out.push('%');
            i += 1;
            continue;
        }
        let (mut argpos, mut widthpos, mut minus, mut width) = (None, None, false, 0i32);
        if let Some(n) = digits(&mut i)? {
            if chars[i] != '$' {
                width = n;
            } else {
                if n == 0 {
                    return Err(Error::new(
                        "format specifies argument 0, but arguments are numbered from 1",
                    ));
                }
                argpos = Some(n as usize);
                i += 1;
                if i >= chars.len() {
                    return Err(unterminated());
                }
            }
        }
        if argpos.is_some() || width == 0 {
            while chars[i] == '-' {
                minus = true;
                i += 1;
                if i >= chars.len() {
                    return Err(unterminated());
                }
            }
            if chars[i] == '*' {
                i += 1;
                if i >= chars.len() {
                    return Err(unterminated());
                }
                match digits(&mut i)? {
                    Some(n) => {
                        if chars[i] != '$' {
                            return Err(Error::new(
                                "width argument position must be ended by \"$\"",
                            ));
                        }
                        if n == 0 {
                            return Err(Error::new(
                                "format specifies argument 0, but arguments are numbered from 1",
                            ));
                        }
                        widthpos = Some(n as usize);
                        i += 1;
                        if i >= chars.len() {
                            return Err(unterminated());
                        }
                    }
                    None => widthpos = Some(0),
                }
            } else if let Some(n) = digits(&mut i)? {
                width = n;
            }
        }
        let conversion = chars[i];
        if !matches!(conversion, 's' | 'I' | 'L') {
            return Err(Error::new(format!(
                "unrecognized format() type specifier \"{conversion}\""
            )));
        }
        i += 1;
        if let Some(w) = widthpos {
            if w > 0 {
                next = w;
            }
            let v = arg(next)?;
            next += 1;
            width = match v {
                v if v.is_null() => 0,
                Value::Int(x) => {
                    i32::try_from(*x).map_err(|_| Error::new("integer out of range"))?
                }
                v => crate::composed::int_in(&output_text(v).unwrap_or_default(), 32)? as i32,
            };
        }
        let at = argpos.unwrap_or(next);
        let v = arg(at)?;
        next = at + 1;
        let s = match (conversion, v.is_null()) {
            ('s', true) => String::new(),
            ('L', true) => "NULL".to_string(),
            ('I', true) => {
                return Err(Error::new(
                    "null values cannot be formatted as an SQL identifier",
                ))
            }
            ('I', false) => quote_ident(&output_text(v).unwrap_or_default()),
            ('L', false) => quote_literal(&output_text(v).unwrap_or_default()),
            _ => output_text(v).unwrap_or_default(),
        };
        let (left, w) = if width < 0 {
            if width == i32::MIN {
                return Err(Error::new("number is out of range"));
            }
            (true, (-width) as usize)
        } else {
            (minus, width as usize)
        };
        let len = s.chars().count();
        if w > len && !left {
            out.push_str(&" ".repeat(w - len));
        }
        out.push_str(&s);
        if w > len && left {
            out.push_str(&" ".repeat(w - len));
        }
    }
    Ok(out)
}

/// `unistr`: the text with each of its Unicode escapes decoded.
fn unistr(s: &str) -> Result<String> {
    let b = s.as_bytes();
    let mut out = String::new();
    let mut first: Option<u32> = None;
    let invalid_pair = || Error::new("invalid Unicode surrogate pair");
    let hex = |at: usize, n: usize| -> Option<u32> {
        let h = b.get(at..at + n)?;
        h.iter()
            .all(u8::is_ascii_hexdigit)
            .then(|| u32::from_str_radix(std::str::from_utf8(h).unwrap(), 16).unwrap())
    };
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' {
            if first.is_some() {
                return Err(invalid_pair());
            }
            let ch = s[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        let (code, step) = if b.get(i + 1) == Some(&b'\\') {
            if first.is_some() {
                return Err(invalid_pair());
            }
            out.push('\\');
            i += 2;
            continue;
        } else if let Some(c) = hex(i + 1, 4) {
            (c, 5)
        } else if b.get(i + 1) == Some(&b'u') && hex(i + 2, 4).is_some() {
            (hex(i + 2, 4).unwrap(), 6)
        } else if b.get(i + 1) == Some(&b'+') && hex(i + 2, 6).is_some() {
            (hex(i + 2, 6).unwrap(), 8)
        } else if b.get(i + 1) == Some(&b'U') && hex(i + 2, 8).is_some() {
            (hex(i + 2, 8).unwrap(), 10)
        } else {
            return Err(Error::new("invalid Unicode escape"));
        };
        if code == 0 || code > 0x10FFFF {
            return Err(Error::new(format!(
                "invalid Unicode code point: {code:04X}"
            )));
        }
        let second = (0xDC00..=0xDFFF).contains(&code);
        let code = match first.take() {
            Some(f) if second => 0x10000 + ((f - 0xD800) << 10) + (code - 0xDC00),
            Some(_) => return Err(invalid_pair()),
            None if second => return Err(invalid_pair()),
            None => code,
        };
        if (0xD800..=0xDBFF).contains(&code) {
            first = Some(code);
        } else {
            out.push(char::from_u32(code).ok_or_else(invalid_pair)?);
        }
        i += step;
    }
    if first.is_some() {
        return Err(invalid_pair());
    }
    Ok(out)
}

/// The value of a function of strings over its arguments' values, `casing` how the collation its
/// first argument carries maps case. None where the snail answers no function of strings by that
/// name and those arguments.
pub(super) fn strings(
    name: &str,
    args: &[Value],
    casing: crate::catalog::Casing,
) -> Result<Option<Value>> {
    if !is_string_function(name) {
        return Ok(None);
    }
    let text = |s: String| Value::Text(s);
    // The functions not strict: concat, concat_ws and format pass over NULL, quote_nullable writes
    // it.
    substring_search(name, casing)?;
    match (name, args) {
        ("concat", a) => {
            return Ok(Some(text(
                a.iter()
                    .filter_map(output_text)
                    .collect::<Vec<_>>()
                    .concat(),
            )))
        }
        ("concat_ws", [sep, rest @ ..]) => {
            if sep.is_null() {
                return Ok(Some(Value::Null));
            }
            let sep = output_text(sep).unwrap_or_default();
            return Ok(Some(text(
                rest.iter()
                    .filter_map(output_text)
                    .collect::<Vec<_>>()
                    .join(&sep),
            )));
        }
        ("format", [t, rest @ ..]) => {
            if t.is_null() {
                return Ok(Some(Value::Null));
            }
            return Ok(Some(text(format(&text_arg(t)?, rest)?)));
        }
        ("quote_nullable", [v]) => {
            return Ok(Some(text(match output_text(v) {
                None => "NULL".to_string(),
                Some(s) => quote_literal(&s),
            })))
        }
        _ => {}
    }
    if args.iter().any(|v| v.is_null()) {
        return Ok(Some(Value::Null));
    }
    let int = |v: &Value| -> Result<i64> { integer(v) };
    let bytes_of = |v: &Value| -> Option<Vec<u8>> {
        match v {
            Value::Bytes(b) => Some(b.clone()),
            _ => None,
        }
    };
    let int32 = |v: &Value| -> Result<i32> {
        i32::try_from(int(v)?).map_err(|_| Error::new("integer out of range"))
    };
    match (name, args) {
        ("md5", [Value::Bytes(b)]) => return Ok(Some(text(crate::bytes::md5_hex(b)))),
        ("md5", [v]) => return Ok(Some(text(crate::bytes::md5_hex(text_arg(v)?.as_bytes())))),
        ("sha224" | "sha256" | "sha384" | "sha512", [Value::Bytes(b)]) => {
            return Ok(Some(Value::Bytes(match name {
                "sha224" => crate::bytes::sha224(b),
                "sha256" => crate::bytes::sha256(b),
                "sha384" => crate::bytes::sha384(b),
                _ => crate::bytes::sha512(b),
            })))
        }
        ("crc32", [Value::Bytes(b)]) => return Ok(Some(Value::Int(crate::bytes::crc32(b)))),
        ("crc32c", [Value::Bytes(b)]) => return Ok(Some(Value::Int(crate::bytes::crc32c(b)))),
        ("bit_count", [Value::Bytes(b)]) => {
            return Ok(Some(Value::Int(crate::bytes::bit_count(b))))
        }
        ("encode", [Value::Bytes(b), f]) => {
            return Ok(Some(text(crate::bytes::encode(b, &text_arg(f)?)?)))
        }
        ("decode", [s, f]) => {
            return Ok(Some(Value::Bytes(crate::bytes::decode(
                &text_arg(s)?,
                &text_arg(f)?,
            )?)))
        }
        ("get_byte", [Value::Bytes(b), n]) => {
            return Ok(Some(Value::Int(crate::bytes::get_byte(b, int(n)?)?)))
        }
        ("get_bit", [Value::Bytes(b), n]) => {
            return Ok(Some(Value::Int(crate::bytes::get_bit(b, int(n)?)?)))
        }
        ("set_byte", [Value::Bytes(b), n, v]) => {
            return Ok(Some(Value::Bytes(crate::bytes::set_byte(
                b,
                int(n)?,
                int(v)?,
            )?)))
        }
        ("set_bit", [Value::Bytes(b), n, v]) => {
            return Ok(Some(Value::Bytes(crate::bytes::set_bit(
                b,
                int(n)?,
                int(v)?,
            )?)))
        }
        ("convert", [Value::Bytes(b), from, to]) => {
            return Ok(Some(Value::Bytes(crate::bytes::convert(
                b,
                &text_arg(from)?,
                &text_arg(to)?,
            )?)))
        }
        ("convert_from", [Value::Bytes(b), e]) => {
            return Ok(Some(text(crate::bytes::convert_from(b, &text_arg(e)?)?)))
        }
        ("convert_to", [s, e]) => {
            return Ok(Some(Value::Bytes(crate::bytes::convert_to(
                &text_arg(s)?,
                &text_arg(e)?,
            )?)))
        }
        ("to_ascii", [s]) => return Ok(Some(text(crate::bytes::to_ascii(&text_arg(s)?, None)?))),
        ("to_ascii", [s, Value::Int(code)]) => {
            return Ok(Some(text(crate::bytes::to_ascii_code(
                &text_arg(s)?,
                i32::try_from(*code).map_err(|_| Error::new("integer out of range"))?,
            )?)))
        }
        ("to_ascii", [s, e]) => {
            return Ok(Some(text(crate::bytes::to_ascii(
                &text_arg(s)?,
                Some(&text_arg(e)?),
            )?)))
        }
        ("pg_encoding_to_char", [c]) => {
            return Ok(Some(Value::Str(
                ColumnType::Name,
                crate::bytes::pg_encoding_to_char(int32(c)?),
            )))
        }
        ("pg_char_to_encoding", [e]) => {
            return Ok(Some(Value::Int(i64::from(
                crate::bytes::pg_char_to_encoding(&text_arg(e)?),
            ))))
        }
        ("pg_encoding_max_length", [c]) => {
            return Ok(Some(
                crate::bytes::pg_encoding_max_length(int32(c)?)
                    .map_or(Value::Null, |m| Value::Int(i64::from(m))),
            ))
        }
        _ => {}
    }
    Ok(Some(match (name, args) {
        // bytea's forms: its bytes.
        ("length" | "octet_length", [Value::Bytes(b)]) => Value::Int(b.len() as i64),
        ("bit_length", [Value::Bytes(b)]) => Value::Int(b.len() as i64 * 8),
        ("substr", [Value::Bytes(b), from, rest @ ..]) if rest.len() <= 1 => Value::Bytes(
            substring(b, int(from)?, rest.first().map(int).transpose()?)?,
        ),
        ("reverse", [Value::Bytes(b)]) => Value::Bytes(b.iter().rev().copied().collect()),
        ("btrim" | "ltrim" | "rtrim", [Value::Bytes(b), set]) => {
            let set = bytes_of(set).ok_or_else(|| Error::new("btrim(bytea, …) takes bytea"))?;
            let mut lo = 0;
            let mut hi = b.len();
            if name != "rtrim" {
                while lo < hi && set.contains(&b[lo]) {
                    lo += 1;
                }
            }
            if name != "ltrim" {
                while hi > lo && set.contains(&b[hi - 1]) {
                    hi -= 1;
                }
            }
            Value::Bytes(b[lo..hi].to_vec())
        }
        // A bpchar's length passes over its trailing blanks; its octets count them.
        ("octet_length", [Value::Str(_, s)]) => Value::Int(s.len() as i64),
        ("length" | "char_length" | "character_length" | "textlen", [v]) => {
            Value::Int(text_arg(v)?.chars().count() as i64)
        }
        ("octet_length", [v]) => Value::Int(text_arg(v)?.len() as i64),
        ("bit_length", [v]) => Value::Int(text_arg(v)?.len() as i64 * 8),
        ("ascii", [v]) => Value::Int(text_arg(v)?.chars().next().map_or(0, |c| c as i64)),
        ("chr", [n]) => {
            let n = int(n)?;
            if n < 0 {
                return Err(Error::new("character number must be positive"));
            }
            if n == 0 {
                return Err(Error::new("null character not permitted"));
            }
            let n = u32::try_from(n).map_err(|_| Error::new("integer out of range"))?;
            if n > 0x10FFFF {
                return Err(Error::new(format!(
                    "requested character too large for encoding: {n}"
                )));
            }
            text(
                char::from_u32(n)
                    .ok_or_else(|| {
                        Error::new(format!("requested character not valid for encoding: {n}"))
                    })?
                    .to_string(),
            )
        }
        ("initcap", [v]) => text(crate::strings::title(&text_arg(v)?, casing)?),
        ("casefold", [v]) => text(crate::strings::fold(&text_arg(v)?, casing)?),
        ("left" | "right", [v, n]) => {
            let c: Vec<char> = text_arg(v)?.chars().collect();
            let n = int(n)?;
            let len = c.len() as i64;
            let keep = if n >= 0 { n.min(len) } else { (len + n).max(0) } as usize;
            text(if name == "left" {
                c[..keep].iter().collect()
            } else {
                c[c.len() - keep..].iter().collect()
            })
        }
        ("lpad" | "rpad", [v, n, rest @ ..]) if rest.len() <= 1 => {
            let s: Vec<char> = text_arg(v)?.chars().collect();
            let fill: Vec<char> = match rest.first() {
                Some(f) => text_arg(f)?.chars().collect(),
                None => vec![' '],
            };
            let mut len = int(n)?.clamp(0, i64::from(i32::MAX)) as usize;
            if fill.is_empty() {
                len = len.min(s.len());
            }
            if s.len() >= len {
                text(s[..len].iter().collect())
            } else {
                let pad: String = fill.iter().cycle().take(len - s.len()).collect();
                let s: String = s.iter().collect();
                text(if name == "lpad" { pad + &s } else { s + &pad })
            }
        }
        ("btrim" | "ltrim" | "rtrim", [v, rest @ ..]) if rest.len() <= 1 => {
            let s: Vec<char> = text_arg(v)?.chars().collect();
            let set: Vec<char> = match rest.first() {
                Some(f) => text_arg(f)?.chars().collect(),
                None => vec![' '],
            };
            let (mut lo, mut hi) = (0, s.len());
            if name != "rtrim" {
                while lo < hi && set.contains(&s[lo]) {
                    lo += 1;
                }
            }
            if name != "ltrim" {
                while hi > lo && set.contains(&s[hi - 1]) {
                    hi -= 1;
                }
            }
            text(s[lo..hi].iter().collect())
        }
        ("substr", [v, from, rest @ ..]) if rest.len() <= 1 => {
            let c: Vec<char> = text_arg(v)?.chars().collect();
            text(
                substring(&c, int(from)?, rest.first().map(int).transpose()?)?
                    .into_iter()
                    .collect(),
            )
        }
        ("strpos", [v, sub]) => {
            let (s, sub) = (text_arg(v)?, text_arg(sub)?);
            Value::Int(match s.find(&sub) {
                Some(at) => s[..at].chars().count() as i64 + 1,
                None => 0,
            })
        }
        ("starts_with", [v, prefix]) => Value::Bool(text_arg(v)?.starts_with(&text_arg(prefix)?)),
        ("repeat", [v, n]) => {
            let s = text_arg(v)?;
            let n = int(n)?.max(0);
            if (s.len() as i64)
                .checked_mul(n)
                .is_none_or(|t| t > i64::from(i32::MAX))
            {
                return Err(Error::new("requested length too large"));
            }
            text(s.repeat(n as usize))
        }
        ("replace", [v, from, to]) => {
            let (s, from, to) = (text_arg(v)?, text_arg(from)?, text_arg(to)?);
            text(if from.is_empty() {
                s
            } else {
                s.replace(&from, &to)
            })
        }
        ("reverse", [v]) => text(text_arg(v)?.chars().rev().collect()),
        ("split_part", [v, delim, n]) => {
            let (s, delim, n) = (text_arg(v)?, text_arg(delim)?, int(n)?);
            if n == 0 {
                return Err(Error::new("field position must not be zero"));
            }
            if s.is_empty() {
                text(String::new())
            } else if delim.is_empty() {
                text(if n == 1 || n == -1 { s } else { String::new() })
            } else {
                let parts: Vec<&str> = s.split(delim.as_str()).collect();
                let at = if n > 0 { n - 1 } else { parts.len() as i64 + n };
                text(
                    usize::try_from(at)
                        .ok()
                        .and_then(|at| parts.get(at))
                        .map_or(String::new(), |p| p.to_string()),
                )
            }
        }
        ("translate", [v, from, to]) => {
            let (from, to): (Vec<char>, Vec<char>) = (
                text_arg(from)?.chars().collect(),
                text_arg(to)?.chars().collect(),
            );
            text(
                text_arg(v)?
                    .chars()
                    .filter_map(|c| match from.iter().position(|&f| f == c) {
                        Some(p) => to.get(p).copied(),
                        None => Some(c),
                    })
                    .collect(),
            )
        }
        ("quote_ident", [v]) => text(quote_ident(&text_arg(v)?)),
        ("quote_literal", [v]) => text(quote_literal(&output_text(v).unwrap_or_default())),
        ("unistr", [v]) => text(unistr(&text_arg(v)?)?),
        ("like" | "notlike", [v, p]) => {
            let under = Some(casing).filter(|k| matches!(k, crate::catalog::Casing::Library(_)));
            let m = super::pipeline::like_values(v, p, under)?;
            Value::Bool(m == (name == "like"))
        }
        _ => return Ok(None),
    }))
}

/// Refuses a search for a substring under a nondeterministic collation: starts_with as PostgreSQL
/// refuses it, the searches PostgreSQL makes there substring by substring under the collation
/// (strpos, POSITION, replace, split_part) as not read yet.
pub(super) fn substring_search(name: &str, casing: crate::catalog::Casing) -> Result<()> {
    let crate::catalog::Casing::Library(id) = casing else {
        return Ok(());
    };
    if crate::collations::get(id).spec.deterministic {
        return Ok(());
    }
    match name {
        "starts_with" => Err(Error::new(
            "nondeterministic collations are not supported for substring searches",
        )),
        "strpos" | "position" | "replace" | "split_part" => Err(Error::new(format!(
            "{name} under a nondeterministic collation is not read yet"
        ))),
        _ => Ok(()),
    }
}

/// `POSITION(sub IN s)`: where the substring first begins, counted from 1, 0 where it does not
/// occur; in characters for text, in bytes for bytea.
pub(super) fn position(s: Value, sub: Value) -> Result<Value> {
    if s.is_null() || sub.is_null() {
        return Ok(Value::Null);
    }
    Ok(match (&s, &sub) {
        (Value::Bytes(s), Value::Bytes(sub)) => Value::Int(if sub.is_empty() {
            1
        } else {
            s.windows(sub.len())
                .position(|w| w == sub.as_slice())
                .map_or(0, |p| p as i64 + 1)
        }),
        _ => return Ok(strings("strpos", &[s, sub], crate::catalog::Casing::Libc)?.unwrap()),
    })
}

/// `OVERLAY(s PLACING t FROM start [FOR count])`: the characters, or bytes, of `s` from `start`,
/// `count` of them or as many as `t` holds, replaced by `t`.
pub(super) fn overlay(s: Value, t: Value, from: Value, count: Option<Value>) -> Result<Value> {
    if s.is_null() || t.is_null() || from.is_null() || count.as_ref().is_some_and(|c| c.is_null()) {
        return Ok(Value::Null);
    }
    let sp = integer(&from)?;
    let place = |chars_s: usize, chars_t: usize| -> Result<(i64, i64)> {
        let sl = match &count {
            Some(c) => integer(c)?,
            None => chars_t as i64,
        };
        let _ = chars_s;
        if sp <= 0 {
            return Err(Error::new("negative substring length not allowed"));
        }
        let end = i32::try_from(sp)
            .ok()
            .and_then(|a| a.checked_add(i32::try_from(sl).ok()?))
            .ok_or_else(|| Error::new("integer out of range"))?;
        Ok((sp, i64::from(end)))
    };
    Ok(match (&s, &t) {
        (Value::Bytes(a), Value::Bytes(b)) => {
            let (sp, end) = place(a.len(), b.len())?;
            let mut out = substring(a, 1, Some(sp - 1))?;
            out.extend_from_slice(b);
            out.extend(substring(a, end, None)?);
            Value::Bytes(out)
        }
        _ => {
            let (a, b): (Vec<char>, Vec<char>) = (
                text_arg(&s)?.chars().collect(),
                text_arg(&t)?.chars().collect(),
            );
            let (sp, end) = place(a.len(), b.len())?;
            let mut out = substring(&a, 1, Some(sp - 1))?;
            out.extend_from_slice(&b);
            out.extend(substring(&a, end, None)?);
            Value::Text(out.into_iter().collect())
        }
    })
}

/// The value of a function of formatting, to_char, to_number, to_date and to_timestamp of two
/// texts, as formatting.c gives it: `types` its arguments' types, where the step resolved them.
/// None where the snail answers no such function.
pub(super) fn formatting(
    name: &str,
    args: &[Value],
    types: &[Option<ColumnType>],
) -> Result<Option<Value>> {
    use crate::formatting as f;
    if !matches!(name, "to_char" | "to_number" | "to_date" | "to_timestamp") {
        return Ok(None);
    }
    let [x, fmt] = args else {
        return Ok(None);
    };
    if name == "to_timestamp" && !matches!(x, Value::Text(_) | Value::Str(..)) {
        return Ok(None);
    }
    if x.is_null() || fmt.is_null() {
        return Ok(Some(Value::Null));
    }
    let fmt = text_arg(fmt)?;
    let text = |s: String| Value::Text(s);
    Ok(Some(match (name, x) {
        ("to_number", s) => match f::to_number(&text_arg(s)?, &fmt)? {
            Some(n) => Value::of_numeric(n),
            None => Value::Null,
        },
        ("to_date", s) => Value::Scalar(
            ColumnType::Date,
            i64::from(f::to_date(&text_arg(s)?, &fmt)?),
        ),
        ("to_timestamp", s) => Value::Scalar(
            ColumnType::Timestamptz,
            f::to_timestamp_text(&text_arg(s)?, &fmt)?,
        ),
        (_, Value::Int(v)) => {
            let wide = match types.first() {
                Some(Some(ColumnType::Int8)) => true,
                Some(Some(ColumnType::Int2 | ColumnType::Int4)) => false,
                _ => i32::try_from(*v).is_err(),
            };
            text(if wide {
                f::to_char_int8(*v, &fmt)?
            } else {
                f::to_char_int4(*v as i32, &fmt)?
            })
        }
        (_, Value::Real(v)) => text(f::to_char_float4(*v, &fmt)?),
        (_, Value::Double(v)) => text(f::to_char_float8(*v, &fmt)?),
        (_, v @ (Value::Num(..) | Value::Numeric(_))) => {
            text(f::to_char_numeric(&v.to_numeric().unwrap(), &fmt)?)
        }
        (_, Value::Interval(v)) => f::to_char_interval(*v, &fmt)?.map_or(Value::Null, text),
        (_, Value::Scalar(ty, k)) => match super::clock::base(*ty) {
            ColumnType::Timestamp(_) => f::to_char_timestamp(*k, &fmt)?.map_or(Value::Null, text),
            ColumnType::Timestamptz => f::to_char_timestamptz(*k, &fmt)?.map_or(Value::Null, text),
            ColumnType::Date => {
                f::to_char_timestamptz(crate::times::date_timestamptz(*k as i32)?, &fmt)?
                    .map_or(Value::Null, text)
            }
            _ => return Ok(None),
        },
        _ => return Ok(None),
    }))
}

/// Whether a function is a regular expression's: the `regexp_` functions and SIMILAR TO's
/// translations.
pub(super) fn is_regexp_function(name: &str) -> bool {
    matches!(
        name,
        "regexp_count"
            | "regexp_instr"
            | "regexp_like"
            | "regexp_match"
            | "regexp_matches"
            | "regexp_replace"
            | "regexp_split_to_array"
            | "regexp_split_to_table"
            | "regexp_substr"
            | "similar_escape"
            | "similar_to_escape"
    )
}

/// Whether a regular expression's function returns a set of rows.
pub(super) fn returns_a_set(name: &str) -> bool {
    matches!(name, "regexp_matches" | "regexp_split_to_table")
}

/// Why a function returning a set is refused.
pub(super) fn set_refused(name: &str) -> Error {
    Error::new(format!(
        "{name} returns a set of rows, and the snail reads no function that returns a set yet"
    ))
}

/// `text[]`, what regexp_match and regexp_split_to_array return.
pub(super) fn text_array() -> ColumnType {
    ColumnType::Composed(crate::composed::TypeRef::of(
        crate::composed::Composed::Array {
            element: ColumnType::Text(None),
            domain: None,
        },
    ))
}

/// A one-dimensional `text[]` of the elements, from 1.
fn text_array_of(elements: Vec<Option<String>>) -> Value {
    use crate::composed::{Compound, Item};
    let ty = text_array();
    let n = elements.len() as i32;
    let elements: Vec<Item> = elements
        .into_iter()
        .map(|e| match e {
            Some(s) => Item::Scalar(super::statement::Datum::Text(s)),
            None => Item::Null,
        })
        .collect();
    let (dims, lower) = if n == 0 {
        (vec![], vec![])
    } else {
        (vec![n], vec![1])
    };
    let item = Item::Array {
        elements,
        dims,
        lower,
    };
    Value::Composed(ty, std::sync::Arc::new(Compound::new(ty, item)))
}

/// The classes and case a regular expression is compiled under, by the collation its text
/// carries.
pub(super) fn ctype_of(casing: crate::catalog::Casing) -> Result<crate::regex::Ctype> {
    use crate::catalog::Casing;
    use crate::regex::Ctype;
    let ctype = match casing {
        Casing::Ascii => Ctype::C,
        Casing::Libc => Ctype::Libc { default: true },
        Casing::Simple => Ctype::Builtin { full: false },
        Casing::Full => Ctype::Builtin { full: true },
    };
    if !ctype.available() {
        return Err(Error::new(
            "this machine's C library has no C.UTF-8, whose classes a regular expression reads",
        ));
    }
    Ok(ctype)
}

/// The value of a regular expression's function, as regexp.c gives it, under `casing`; None where
/// the name is none of them. Every one but similar_escape is strict.
pub(super) fn regexps(
    name: &str,
    args: &[Value],
    casing: crate::catalog::Casing,
) -> Result<Option<Value>> {
    use crate::regexp as r;
    if !is_regexp_function(name) {
        return Ok(None);
    }
    if returns_a_set(name) {
        return Err(set_refused(name));
    }
    let no_form = || {
        Error::new(format!(
            "function {name}({}) does not exist",
            args.iter()
                .map(super::pipeline::kind_name)
                .collect::<Vec<_>>()
                .join(", ")
        ))
    };
    if name == "similar_escape" {
        let [pat, esc] = args else {
            return Err(no_form());
        };
        let pat = (!pat.is_null()).then(|| text_arg(pat)).transpose()?;
        let esc = (!esc.is_null()).then(|| text_arg(esc)).transpose()?;
        return Ok(Some(
            r::similar_escape(pat.as_deref(), esc.as_deref())?.map_or(Value::Null, Value::Text),
        ));
    }
    if args.iter().any(Value::is_null) {
        return Ok(Some(Value::Null));
    }
    if name == "similar_to_escape" {
        return Ok(Some(Value::Text(match args {
            [pat] => r::similar_to_escape(&text_arg(pat)?, None)?,
            [pat, esc] => r::similar_to_escape(&text_arg(pat)?, Some(&text_arg(esc)?))?,
            _ => return Err(no_form()),
        })));
    }
    let ctype = ctype_of(casing)?;
    let (s, p) = match args {
        [s, p, ..] => (text_arg(s)?, text_arg(p)?),
        _ => return Err(no_form()),
    };
    let rest = &args[2..];
    let int = |i: usize| -> Result<Option<i32>> { rest.get(i).map(int32).transpose() };
    let txt = |i: usize| -> Result<Option<String>> { rest.get(i).map(text_arg).transpose() };
    let text = |x: Option<String>| x.map_or(Value::Null, Value::Text);
    Ok(Some(match (name, rest.len()) {
        ("regexp_count", 0..=2) => Value::Int(i64::from(r::regexp_count(
            &s,
            &p,
            int(0)?,
            txt(1)?.as_deref(),
            ctype,
        )?)),
        ("regexp_instr", 0..=5) => Value::Int(i64::from(r::regexp_instr(
            &s,
            &p,
            int(0)?,
            int(1)?,
            int(2)?,
            txt(3)?.as_deref(),
            int(4)?,
            ctype,
        )?)),
        ("regexp_like", 0..=1) => Value::Bool(r::regexp_like(&s, &p, txt(0)?.as_deref(), ctype)?),
        ("regexp_match", 0..=1) => match r::regexp_match(&s, &p, txt(0)?.as_deref(), ctype)? {
            Some(e) => text_array_of(e),
            None => Value::Null,
        },
        ("regexp_split_to_array", 0..=1) => text_array_of(
            r::regexp_split_to_array(&s, &p, txt(0)?.as_deref(), ctype)?
                .into_iter()
                .map(Some)
                .collect(),
        ),
        ("regexp_substr", 0..=4) => text(r::regexp_substr(
            &s,
            &p,
            int(0)?,
            int(1)?,
            txt(2)?.as_deref(),
            int(3)?,
            ctype,
        )?),
        ("regexp_replace", 1) => Value::Text(r::textregexreplace_noopt(
            &s,
            &p,
            &text_arg(&rest[0])?,
            ctype,
        )?),
        // The fourth argument an integer is the start; text, the flags.
        ("regexp_replace", 2) => match &rest[1] {
            Value::Int(_) => Value::Text(r::textregexreplace_extended(
                &s,
                &p,
                &text_arg(&rest[0])?,
                int32(&rest[1])?,
                None,
                None,
                ctype,
            )?),
            opt => Value::Text(r::textregexreplace(
                &s,
                &p,
                &text_arg(&rest[0])?,
                &text_arg(opt)?,
                ctype,
            )?),
        },
        ("regexp_replace", 3..=4) => Value::Text(r::textregexreplace_extended(
            &s,
            &p,
            &text_arg(&rest[0])?,
            int32(&rest[1])?,
            Some(int32(&rest[2])?),
            rest.get(3).map(text_arg).transpose()?.as_deref(),
            ctype,
        )?),
        _ => return Err(no_form()),
    }))
}

/// `s ~ p` and its kin, `~*` where `icase`, negated where `not`, under `casing`: a text, a name, or
/// a bpchar with its blanks (bpcharregexeq), against a pattern.
pub(super) fn regex_op(
    s: &Value,
    p: &Value,
    icase: bool,
    not: bool,
    casing: crate::catalog::Casing,
) -> Result<Value> {
    if s.is_null() || p.is_null() {
        return Ok(Value::Null);
    }
    let ctype = ctype_of(casing)?;
    let s = match s {
        Value::Str(ColumnType::Bpchar(_), own) => own.clone(),
        s => text_arg(s)?,
    };
    let m = crate::regexp::textregexeq(&s, &text_arg(p)?, icase, ctype)?;
    Ok(Value::Bool(m != not))
}

/// `substring(s from p)`, and `substring(s from p for esc)` (SQL's form, `similar_escape`'s
/// translation first), under `casing`.
pub(super) fn substring_by_pattern(
    s: &Value,
    p: &Value,
    esc: Option<&Value>,
    casing: crate::catalog::Casing,
) -> Result<Value> {
    if s.is_null() || p.is_null() || esc.is_some_and(Value::is_null) {
        return Ok(Value::Null);
    }
    let p = match esc {
        None => text_arg(p)?,
        Some(e) => match crate::regexp::similar_escape(Some(&text_arg(p)?), Some(&text_arg(e)?))? {
            Some(p) => p,
            None => return Ok(Value::Null),
        },
    };
    let ctype = ctype_of(casing)?;
    Ok(crate::regexp::textregexsubstr(&text_arg(s)?, &p, ctype)?.map_or(Value::Null, Value::Text))
}
