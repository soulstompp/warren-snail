//! The values a statement draws, as PostgreSQL 18.6 draws them for a session: `random()`, `random`
//! of two integers, two bigints or two numerics, `random_normal` and `setseed`, each from the
//! session's one state (`pg_prng.c`'s xoroshiro128**), seeded from the machine's own random source
//! at its first draw unless `setseed` seeded it first. The state is the session's, lent to each
//! statement while it runs, and neither a rollback nor a savepoint puts it back.

use super::pipeline::Value;
use crate::catalog::ColumnType;
use crate::numeric::Numeric;
use crate::{Error, Result};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// The two words xoroshiro128** moves on each draw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct State {
    s0: u64,
    s1: u64,
}

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut v = *state;
    v = (v ^ (v >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    v = (v ^ (v >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    v ^ (v >> 31)
}

impl State {
    /// Seeded from one word, as `pg_prng_seed` seeds it.
    fn seeded(mut seed: u64) -> State {
        let s0 = splitmix64(&mut seed);
        let s1 = splitmix64(&mut seed);
        State { s0, s1 }.checked()
    }

    /// Seeded from a double in [-1, 1], as `pg_prng_fseed` seeds it.
    fn of_double(f: f64) -> State {
        let seed = (((1u64 << 52) - 1) as f64 * f) as i64;
        State::seeded(seed as u64)
    }

    /// Seeded from the machine's own random source, its two words as they come; from the clock
    /// where the source cannot be read.
    fn strong() -> State {
        use std::io::Read;
        let mut b = [0u8; 16];
        let read = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b));
        match read {
            Ok(()) => State {
                s0: u64::from_le_bytes(b[..8].try_into().expect("eight bytes")),
                s1: u64::from_le_bytes(b[8..].try_into().expect("eight bytes")),
            }
            .checked(),
            Err(_) => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_micros() as u64);
                State::seeded(now ^ (u64::from(std::process::id()) << 32))
            }
        }
    }

    /// Never both words zero, which would draw only zeros.
    fn checked(self) -> State {
        match self.s0 == 0 && self.s1 == 0 {
            true => State {
                s0: 0x5851_F42D_4C95_7F2D,
                s1: 0x1405_7B7E_F767_814F,
            },
            false => self,
        }
    }

    fn next(&mut self) -> u64 {
        let s0 = self.s0;
        let sx = self.s1 ^ s0;
        let v = s0.wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        self.s0 = s0.rotate_left(24) ^ sx ^ (sx << 16);
        self.s1 = sx.rotate_left(37);
        v
    }

    /// A word from `lo` to `hi`, drawn again while it passes `hi`; `lo` with no draw where `hi`
    /// does not pass it.
    fn u64_range(&mut self, lo: u64, hi: u64) -> u64 {
        if hi <= lo {
            return lo;
        }
        let range = hi - lo;
        let shift = range.leading_zeros();
        loop {
            let v = self.next() >> shift;
            if v <= range {
                return lo + v;
            }
        }
    }

    /// An integer from `lo` to `hi`; `lo` with no draw where `hi` does not pass it.
    fn i64_range(&mut self, lo: i64, hi: i64) -> i64 {
        if hi <= lo {
            return lo;
        }
        let u = (lo as u64).wrapping_add(self.u64_range(0, (hi as u64).wrapping_sub(lo as u64)));
        u as i64
    }

    /// A double from 0 up to 1, of 52 bits.
    fn double(&mut self) -> f64 {
        (self.next() >> 12) as f64 / (1u64 << 52) as f64
    }

    /// A double of the standard normal distribution, by the Box-Muller transform.
    fn double_normal(&mut self) -> f64 {
        let u1 = 1.0 - self.double();
        let u2 = 1.0 - self.double();
        (-2.0 * u1.ln()).sqrt() * crate::sys::libm::libm().sin.call(2.0 * std::f64::consts::PI * u2)
    }
}

/// What a session draws from: no state until its first draw or `setseed`.
#[derive(Clone, Default)]
pub(super) struct Drawing(Rc<Cell<Option<State>>>);

impl Drawing {
    /// Where it stands, to be put back where a statement runs again.
    pub(super) fn stands(&self) -> Option<State> {
        self.0.get()
    }

    pub(super) fn put_back(&self, at: Option<State>) {
        self.0.set(at);
    }

    /// Seeded as setseed seeds it, by SET seed.
    pub(super) fn seed(&self, f: f64) {
        self.0.set(Some(State::of_double(f)));
    }
}

/// The state lent to the statement under way seeded as setseed seeds it, by set_config('seed', …).
pub(super) fn seed_lent(f: f64) {
    let s = State::of_double(f);
    drawn(|at| *at = s);
}

thread_local! {
    /// The state of the session whose statement is under way on this thread.
    static LENT: RefCell<Option<Drawing>> = const { RefCell::new(None) };
    /// What a statement draws from where no session lent it one.
    static OWN: Drawing = Drawing::default();
}

/// A session's state lent to its statement until dropped.
pub(super) struct Lending(Option<Option<Drawing>>);

pub(super) fn lending(d: &Drawing) -> Lending {
    Lending(Some(LENT.with(|l| l.replace(Some(d.clone())))))
}

impl Drop for Lending {
    fn drop(&mut self) {
        if let Some(before) = self.0.take() {
            LENT.with(|l| *l.borrow_mut() = before);
        }
    }
}

/// A draw on the state lent, seeded first where nothing has seeded it.
fn drawn<T>(f: impl FnOnce(&mut State) -> T) -> T {
    let d = LENT
        .with(|l| l.borrow().clone())
        .unwrap_or_else(|| OWN.with(Drawing::clone));
    let mut s = d.0.get().unwrap_or_else(State::strong);
    let v = f(&mut s);
    d.0.set(Some(s));
    v
}

/// The names of the functions that draw.
pub(super) fn is_drawing(name: &str) -> bool {
    matches!(name, "random" | "random_normal" | "setseed")
}

/// A value of void, which `setseed` returns: no value, written as nothing.
pub(super) fn void() -> Value {
    Value::Typed(VOID, String::new())
}

pub(super) const VOID: ColumnType = ColumnType::Internal(crate::textual::Internal::Void);

/// Whether a type named is void.
pub(super) fn is_void(dt: &sqlparser::ast::DataType) -> bool {
    match dt {
        sqlparser::ast::DataType::Custom(name, modifiers) if modifiers.is_empty() => {
            let parts: Vec<String> = name
                .0
                .iter()
                .map(|p| match p {
                    sqlparser::ast::ObjectNamePart::Identifier(i) => crate::catalog::ident(i),
                    other => other.to_string(),
                })
                .collect();
            matches!(parts.as_slice(), [v] | [_, v] if v == "void")
                && (parts.len() == 1 || parts[0] == "pg_catalog")
        }
        _ => false,
    }
}

/// A value cast to void, as void's input reads any text: NULL stays NULL.
pub(super) fn to_void(v: Value) -> Result<Value> {
    Ok(match v {
        Value::Null => Value::Null,
        Value::Text(_) | Value::Str(..) => void(),
        v @ Value::Typed(VOID, _) => v,
        other => {
            return Err(Error::new(format!(
                "cannot cast type {} to void",
                super::pipeline::kind_name(&other)
            )))
        }
    })
}

/// A call of one of the functions that draw, as PostgreSQL answers it, `ret` the type the step
/// resolved it to; none for any other. Each is strict: a NULL argument answers NULL and draws
/// nothing. A question read only for its columns draws nothing.
pub(super) fn call(name: &str, args: &[Value], ret: Option<ColumnType>) -> Result<Option<Value>> {
    if !is_drawing(name) {
        return Ok(None);
    }
    if args.iter().any(Value::is_null) {
        return Ok(Some(Value::Null));
    }
    let probing = super::sequences::probing_now();
    Ok(Some(match (name, args) {
        ("setseed", [x]) => {
            let f = double(x)?;
            if !(-1.0..=1.0).contains(&f) {
                return Err(Error::new(format!(
                    "setseed parameter {} is out of allowed range [-1,1]",
                    g_format(f)
                )));
            }
            if !probing {
                let s = State::of_double(f);
                drawn(|at| *at = s);
            }
            void()
        }
        ("random", []) if probing => Value::Double(0.0),
        ("random", []) => Value::Double(drawn(State::double)),
        ("random_normal", _) if args.len() <= 2 => {
            let mean = args.first().map(double).transpose()?.unwrap_or(0.0);
            let stddev = args.get(1).map(double).transpose()?.unwrap_or(1.0);
            let z = match probing {
                true => 0.0,
                false => drawn(State::double_normal),
            };
            Value::Double(stddev * z + mean)
        }
        ("random", [lo, hi]) => {
            let integers = match ret {
                Some(t) => t.is_integer(),
                None => matches!((lo, hi), (Value::Int(_), Value::Int(_))),
            };
            match integers {
                true => {
                    let (lo, hi) = (integer(lo)?, integer(hi)?);
                    if lo > hi {
                        return Err(Error::new(BOUNDS));
                    }
                    match probing {
                        true => Value::Int(lo),
                        false => Value::Int(drawn(|s| s.i64_range(lo, hi))),
                    }
                }
                false => {
                    let (lo, hi) = (numeric(lo)?, numeric(hi)?);
                    Value::of_numeric(random_numeric(&lo, &hi, probing)?)
                }
            }
        }
        _ => return Ok(None),
    }))
}

const BOUNDS: &str = "lower bound must be less than or equal to upper bound";

fn double(v: &Value) -> Result<f64> {
    match super::pipeline::float_cast(v.clone(), true)? {
        Value::Double(d) => Ok(d),
        other => Err(Error::new(format!("{other:?} is not a double precision"))),
    }
}

fn integer(v: &Value) -> Result<i64> {
    match v {
        Value::Int(x) => Ok(*x),
        Value::Text(t) => crate::composed::int_in(t, 64),
        v => Err(Error::new(format!("{v:?} is not an integer"))),
    }
}

fn numeric(v: &Value) -> Result<Numeric> {
    match v {
        Value::Text(t) => Numeric::parse(t),
        v => v
            .to_numeric()
            .ok_or_else(|| Error::new(format!("{v:?} is not a numeric"))),
    }
}

/// A double as PostgreSQL's `%g` writes it, `NaN` and `Infinity` for the special values.
fn g_format(f: f64) -> String {
    match f {
        f if f.is_nan() => "NaN".into(),
        f if f.is_infinite() && f > 0.0 => "Infinity".into(),
        f if f.is_infinite() => "-Infinity".into(),
        f => super::settings::g_format(f),
    }
}

/// The digits of PostgreSQL's numeric, of four decimal digits each.
const NBASE: u64 = 10_000;

/// A positive number's digits of four decimal digits each, as PostgreSQL holds them, none zero at
/// either end, and the power of 10000 of the first.
fn nbase_digits(n: &Numeric) -> (Vec<u64>, i64) {
    let text = n.to_string();
    let (whole, frac) = text.split_once('.').unwrap_or((&text, ""));
    let whole = whole.trim_start_matches('0');
    let lead = (4 - whole.len() % 4) % 4;
    let tail = (4 - frac.len() % 4) % 4;
    let all = format!("{}{whole}{frac}{}", "0".repeat(lead), "0".repeat(tail));
    let mut digits: Vec<u64> = all
        .as_bytes()
        .chunks(4)
        .map(|c| std::str::from_utf8(c).expect("digits").parse().expect("digits"))
        .collect();
    let mut weight = ((lead + whole.len()) / 4) as i64 - 1;
    while digits.first() == Some(&0) {
        digits.remove(0);
        weight -= 1;
    }
    while digits.last() == Some(&0) {
        digits.pop();
    }
    (digits, weight)
}

/// A number of digits of four decimal digits each, the first at the power `weight` of 10000, at
/// display scale `scale`.
fn of_nbase(digits: &[u64], weight: i64, scale: u32) -> Result<Numeric> {
    let mut whole = String::new();
    let mut frac = String::new();
    for (i, d) in digits.iter().enumerate() {
        match weight - (i as i64) >= 0 {
            true => whole.push_str(&format!("{d:04}")),
            false => frac.push_str(&format!("{d:04}")),
        }
    }
    if weight < -1 {
        frac.insert_str(0, &"0".repeat(4 * (-weight - 1) as usize));
    }
    let whole = match whole.trim_start_matches('0') {
        "" => "0",
        w => w,
    };
    Ok(Numeric::parse(&format!("{whole}.{frac}0"))?.round(scale as i32))
}

/// A numeric from `lo` to `hi`, at the display scale the greater of theirs, as `random_var` draws
/// it: below the range's length its first digits at once, the rest four at a time, then one at a
/// time, the last a multiple of the scale's power of ten, drawn again where it passes the length.
fn random_numeric(lo: &Numeric, hi: &Numeric, probing: bool) -> Result<Numeric> {
    for (n, which) in [(lo, "lower"), (hi, "upper")] {
        if n.is_nan() {
            return Err(Error::new(format!("{which} bound cannot be NaN")));
        }
        if n.is_infinite() {
            return Err(Error::new(format!("{which} bound cannot be infinity")));
        }
    }
    let scale = lo.dscale().unwrap_or(0).max(hi.dscale().unwrap_or(0));
    let length = hi.sub(lo)?;
    if length.sign() < 0 {
        return Err(Error::new(BOUNDS));
    }
    if length.sign() == 0 || probing {
        return Ok(lo.round(scale as i32));
    }
    let (rlen, weight) = nbase_digits(&length);
    let groups = (i64::from(scale) + 3) / 4;
    let ndigits = (weight + 1 + groups) as usize;
    let pow10 = 10u64.pow((groups * 4 - i64::from(scale)) as u32);
    let mut rlen64 = rlen[0];
    let mut rlen64_digits = 1;
    while rlen64_digits < ndigits && rlen64_digits < 4 {
        rlen64 *= NBASE;
        if rlen64_digits < rlen.len() {
            rlen64 += rlen[rlen64_digits];
        }
        rlen64_digits += 1;
    }
    let made = drawn(|s| -> Result<Numeric> {
        loop {
            let mut digits = vec![0u64; ndigits];
            let mut r = match rlen64_digits == ndigits && pow10 != 1 {
                true => s.u64_range(0, rlen64 / pow10) * pow10,
                false => s.u64_range(0, rlen64),
            };
            for i in (0..rlen64_digits).rev() {
                digits[i] = r % NBASE;
                r /= NBASE;
            }
            let whole = match pow10 != 1 {
                true => ndigits - 1,
                false => ndigits,
            };
            let mut i = rlen64_digits;
            while i + 3 < whole {
                let mut r = s.u64_range(0, NBASE.pow(4) - 1);
                for _ in 0..3 {
                    digits[i] = r % NBASE;
                    r /= NBASE;
                    i += 1;
                }
                digits[i] = r;
                i += 1;
            }
            while i < whole {
                digits[i] = s.u64_range(0, NBASE - 1);
                i += 1;
            }
            if i < ndigits {
                digits[i] = s.u64_range(0, NBASE / pow10 - 1) * pow10;
            }
            let n = of_nbase(&digits, weight, scale)?;
            if n <= length {
                return Ok(n);
            }
        }
    })?;
    made.add(lo).map(|n| n.round(scale as i32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_seed_draws_as_postgresql_draws_from_it() {
        let mut s = State::of_double(0.5);
        assert_eq!(s.double(), 0.9851677175347999);
        assert_eq!(s.double(), 0.825301858027981);
        let mut s = State::of_double(-1.0);
        assert_eq!(s.double(), 0.725656831544149);
    }

    #[test]
    fn a_numeric_s_digits_of_four_and_their_weight() {
        let n = |s: &str| Numeric::parse(s).unwrap();
        assert_eq!(nbase_digits(&n("123456.789")), (vec![12, 3456, 7890], 1));
        assert_eq!(nbase_digits(&n("0.001")), (vec![10], -1));
        assert_eq!(nbase_digits(&n("0.00000001")), (vec![1], -2));
        assert_eq!(nbase_digits(&n("10000")), (vec![1], 1));
        assert_eq!(of_nbase(&[12, 3456, 7890], 1, 3).unwrap().to_string(), "123456.789");
        assert_eq!(of_nbase(&[1], -2, 8).unwrap().to_string(), "0.00000001");
    }
}
