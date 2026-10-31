//! numeric, as PostgreSQL holds and computes it: NaN, an infinity, or a number of any length with its
//! display scale, the digits it prints after the point. Read as `numeric_in` reads it, written as
//! `numeric_out` writes it, compared as `cmp_numerics` compares, and computed at the scales
//! `numeric.c` gives each operation, every rounding half away from zero.

use crate::{Error, Result};
use std::cmp::Ordering;
use std::fmt;

mod funcs;

/// The most digits a numeric holds after the point.
pub const DSCALE_MAX: u32 = 16383;
/// The most digits a numeric holds before the point.
pub const INTEGER_DIGITS_MAX: i64 = 131072;
/// The greatest precision a typmod names.
pub const PRECISION_MAX: u32 = 1000;
/// The least and greatest scale a typmod names.
pub const SCALE_MIN: i32 = -1000;
pub const SCALE_MAX: i32 = 1000;
/// The significant digits a quotient keeps at least, and the most digits after its point.
const MIN_SIG_DIGITS: i64 = 16;
const MAX_DISPLAY_SCALE: i64 = 1000;

const BASE: u64 = 1_000_000_000;
const LIMB_DIGITS: u32 = 9;

/// A natural number in limbs of nine decimal digits, the least first, with no zero limb on top.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) struct Nat(Vec<u32>);

impl Nat {
    pub(crate) fn zero() -> Nat {
        Nat(Vec::new())
    }

    fn trimmed(mut self) -> Nat {
        while self.0.last() == Some(&0) {
            self.0.pop();
        }
        self
    }

    pub(crate) fn from_u128(mut v: u128) -> Nat {
        let mut l = Vec::new();
        while v > 0 {
            l.push((v % BASE as u128) as u32);
            v /= BASE as u128;
        }
        Nat(l)
    }

    pub(crate) fn to_u128(&self) -> Option<u128> {
        let mut v: u128 = 0;
        for &x in self.0.iter().rev() {
            v = v.checked_mul(BASE as u128)?.checked_add(x as u128)?;
        }
        Some(v)
    }

    pub(crate) fn is_zero(&self) -> bool {
        self.0.is_empty()
    }

    /// The number from its decimal digits.
    fn from_digits(d: &[u8]) -> Nat {
        let d = &d[d.iter().position(|&c| c != b'0').unwrap_or(d.len())..];
        let mut l = Vec::with_capacity(d.len() / 9 + 1);
        let mut end = d.len();
        while end > 0 {
            let start = end.saturating_sub(LIMB_DIGITS as usize);
            l.push(
                d[start..end]
                    .iter()
                    .fold(0u32, |a, &c| a * 10 + (c - b'0') as u32),
            );
            end = start;
        }
        Nat(l).trimmed()
    }

    /// Its decimal digits, none for zero.
    fn digits(&self) -> String {
        let mut s = String::new();
        if let Some((top, rest)) = self.0.split_last() {
            s.push_str(&top.to_string());
            for x in rest.iter().rev() {
                s.push_str(&format!("{x:09}"));
            }
        }
        s
    }

    /// How many decimal digits it has, 0 for zero.
    fn digit_count(&self) -> u32 {
        match self.0.last() {
            None => 0,
            Some(&top) => {
                (self.0.len() as u32 - 1) * LIMB_DIGITS + (top.checked_ilog10().unwrap_or(0) + 1)
            }
        }
    }

    /// How many of its last decimal digits are zeros.
    fn trailing_zeros(&self) -> u32 {
        let mut n = 0;
        for &x in &self.0 {
            if x == 0 {
                n += LIMB_DIGITS;
            } else {
                let mut x = x;
                while x % 10 == 0 {
                    x /= 10;
                    n += 1;
                }
                return n;
            }
        }
        n
    }

    fn cmp(&self, o: &Nat) -> Ordering {
        self.0
            .len()
            .cmp(&o.0.len())
            .then_with(|| self.0.iter().rev().cmp(o.0.iter().rev()))
    }

    fn add(&self, o: &Nat) -> Nat {
        let (a, b) = if self.0.len() >= o.0.len() {
            (self, o)
        } else {
            (o, self)
        };
        let mut out = Vec::with_capacity(a.0.len() + 1);
        let mut carry = 0u64;
        for i in 0..a.0.len() {
            let t = a.0[i] as u64 + b.0.get(i).copied().unwrap_or(0) as u64 + carry;
            out.push((t % BASE) as u32);
            carry = t / BASE;
        }
        if carry > 0 {
            out.push(carry as u32);
        }
        Nat(out)
    }

    /// `self - o`, where `o` is not greater.
    fn sub(&self, o: &Nat) -> Nat {
        let mut out = Vec::with_capacity(self.0.len());
        let mut borrow = 0i64;
        for i in 0..self.0.len() {
            let mut t = self.0[i] as i64 - o.0.get(i).copied().unwrap_or(0) as i64 - borrow;
            borrow = (t < 0) as i64;
            if t < 0 {
                t += BASE as i64;
            }
            out.push(t as u32);
        }
        Nat(out).trimmed()
    }

    fn mul(&self, o: &Nat) -> Nat {
        if self.is_zero() || o.is_zero() {
            return Nat::zero();
        }
        let mut out = vec![0u64; self.0.len() + o.0.len()];
        for (i, &a) in self.0.iter().enumerate() {
            let mut carry = 0u64;
            for (j, &b) in o.0.iter().enumerate() {
                let t = out[i + j] + a as u64 * b as u64 + carry;
                out[i + j] = t % BASE;
                carry = t / BASE;
            }
            out[i + o.0.len()] += carry;
        }
        Nat(out.into_iter().map(|x| x as u32).collect()).trimmed()
    }

    fn mul_small(&self, m: u32) -> Nat {
        let mut out = Vec::with_capacity(self.0.len() + 1);
        let mut carry = 0u64;
        for &x in &self.0 {
            let t = x as u64 * m as u64 + carry;
            out.push((t % BASE) as u32);
            carry = t / BASE;
        }
        if carry > 0 {
            out.push(carry as u32);
        }
        Nat(out).trimmed()
    }

    fn divmod_small(&self, d: u32) -> (Nat, u32) {
        let mut out = vec![0u32; self.0.len()];
        let mut r = 0u64;
        for i in (0..self.0.len()).rev() {
            let t = r * BASE + self.0[i] as u64;
            out[i] = (t / d as u64) as u32;
            r = t % d as u64;
        }
        (Nat(out).trimmed(), r as u32)
    }

    /// The quotient and the remainder, by long division in limbs (Knuth's algorithm D).
    fn divmod(&self, d: &Nat) -> (Nat, Nat) {
        assert!(!d.is_zero(), "a division by zero reached the digits");
        if self.cmp(d) == Ordering::Less {
            return (Nat::zero(), self.clone());
        }
        if d.0.len() == 1 {
            let (q, r) = self.divmod_small(d.0[0]);
            return (q, Nat::from_u128(r as u128));
        }
        let f = (BASE / (*d.0.last().unwrap() as u64 + 1)) as u32;
        let v = d.mul_small(f).0;
        let mut u = self.mul_small(f).0;
        let n = v.len();
        if u.len() == self.0.len() {
            u.push(0);
        }
        while u.len() < self.0.len() + 1 {
            u.push(0);
        }
        let m = u.len() - n - 1;
        let mut q = vec![0u32; m + 1];
        let (vt, vs) = (v[n - 1] as u64, v[n - 2] as u64);
        for j in (0..=m).rev() {
            let top = u[j + n] as u64 * BASE + u[j + n - 1] as u64;
            let mut qhat = top / vt;
            let mut rhat = top % vt;
            while qhat >= BASE || qhat * vs > rhat * BASE + u[j + n - 2] as u64 {
                qhat -= 1;
                rhat += vt;
                if rhat >= BASE {
                    break;
                }
            }
            let mut borrow = 0i64;
            let mut carry = 0u64;
            for i in 0..n {
                let p = qhat * v[i] as u64 + carry;
                carry = p / BASE;
                let t = u[i + j] as i64 - (p % BASE) as i64 - borrow;
                borrow = (t < 0) as i64;
                u[i + j] = (if t < 0 { t + BASE as i64 } else { t }) as u32;
            }
            let t = u[j + n] as i64 - carry as i64 - borrow;
            if t < 0 {
                u[j + n] = (t + BASE as i64) as u32;
                qhat -= 1;
                let mut c = 0u64;
                for i in 0..n {
                    let s = u[i + j] as u64 + v[i] as u64 + c;
                    u[i + j] = (s % BASE) as u32;
                    c = s / BASE;
                }
                u[j + n] = ((u[j + n] as u64 + c) % BASE) as u32;
            } else {
                u[j + n] = t as u32;
            }
            q[j] = qhat as u32;
        }
        u.truncate(n);
        let (r, _) = Nat(u).trimmed().divmod_small(f);
        (Nat(q).trimmed(), r)
    }

    pub(crate) fn pow10(k: u32) -> Nat {
        Nat::from_u128(1).mul_pow10(k)
    }

    fn mul_pow10(&self, k: u32) -> Nat {
        if self.is_zero() {
            return Nat::zero();
        }
        let mut l = vec![0u32; (k / LIMB_DIGITS) as usize];
        l.extend_from_slice(&self.0);
        Nat(l).mul_small(10u32.pow(k % LIMB_DIGITS))
    }

    /// The quotient and remainder by `10^k`.
    fn divmod_pow10(&self, k: u32) -> (Nat, Nat) {
        let shift = ((k / LIMB_DIGITS) as usize).min(self.0.len());
        let (low, high) = self.0.split_at(shift);
        let (q, r) = Nat(high.to_vec()).divmod_small(10u32.pow(k % LIMB_DIGITS));
        let mut rem = low.to_vec();
        if (k / LIMB_DIGITS) as usize <= self.0.len() {
            rem.push(r);
        }
        (q, Nat(rem).trimmed())
    }
}

/// A numeric's value.
#[derive(Clone, Debug)]
enum Repr {
    NaN,
    Infinity {
        negative: bool,
    },
    /// `coef / 10^dscale`, negated where `negative`; zero is never negative.
    Finite {
        negative: bool,
        coef: Nat,
        dscale: u32,
    },
}

/// A numeric: NaN, an infinity, or a number with its display scale.
#[derive(Clone, Debug)]
pub struct Numeric(Repr);

fn overflow() -> Error {
    Error::new("value overflows numeric format")
}

fn division_by_zero() -> Error {
    Error::new("division by zero")
}

fn invalid(s: &str) -> Error {
    Error::new(format!("invalid input syntax for type numeric: \"{s}\""))
}

/// Whether C's `isspace` takes the byte.
fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r')
}

impl Numeric {
    pub const NAN: Numeric = Numeric(Repr::NaN);

    pub(crate) fn infinity(negative: bool) -> Numeric {
        Numeric(Repr::Infinity { negative })
    }

    pub(crate) fn zero() -> Numeric {
        Numeric(Repr::Finite {
            negative: false,
            coef: Nat::zero(),
            dscale: 0,
        })
    }

    fn finite(negative: bool, coef: Nat, dscale: u32) -> Numeric {
        let negative = negative && !coef.is_zero();
        Numeric(Repr::Finite {
            negative,
            coef,
            dscale,
        })
    }

    /// The number `units / 10^scale`.
    pub(crate) fn of_units(units: i128, scale: u32) -> Numeric {
        Numeric::finite(units < 0, Nat::from_u128(units.unsigned_abs()), scale)
    }

    /// An integer.
    pub(crate) fn of_int(v: i64) -> Numeric {
        Numeric::of_units(v as i128, 0)
    }

    pub fn is_nan(&self) -> bool {
        matches!(self.0, Repr::NaN)
    }

    /// Whether it is +Infinity or -Infinity.
    pub fn is_infinite(&self) -> bool {
        matches!(self.0, Repr::Infinity { .. })
    }

    pub(crate) fn is_finite(&self) -> bool {
        matches!(self.0, Repr::Finite { .. })
    }

    /// Its sign: -1, 0 or 1; NaN's 0.
    pub(crate) fn sign(&self) -> i32 {
        match &self.0 {
            Repr::NaN => 0,
            Repr::Infinity { negative } => {
                if *negative {
                    -1
                } else {
                    1
                }
            }
            Repr::Finite { negative, coef, .. } => {
                if coef.is_zero() {
                    0
                } else if *negative {
                    -1
                } else {
                    1
                }
            }
        }
    }

    /// The digits it prints after the point; none for NaN and the infinities.
    pub fn dscale(&self) -> Option<u32> {
        match &self.0 {
            Repr::Finite { dscale, .. } => Some(*dscale),
            _ => None,
        }
    }

    /// Its digits as one integer at its own scale, and that scale, where the integer fits 128 bits.
    pub fn units(&self) -> Option<(i128, u32)> {
        match &self.0 {
            Repr::Finite {
                negative,
                coef,
                dscale,
            } => {
                let u = i128::try_from(coef.to_u128()?).ok()?;
                Some((if *negative { -u } else { u }, *dscale))
            }
            _ => None,
        }
    }

    /// Its digits as one integer at `scale` places, where it holds no digit past them and fits 128
    /// bits.
    pub(crate) fn units_at(&self, scale: u32) -> Option<i128> {
        let Repr::Finite {
            negative,
            coef,
            dscale,
        } = &self.0
        else {
            return None;
        };
        let c = if scale >= *dscale {
            coef.mul_pow10(scale - dscale)
        } else {
            let (q, r) = coef.divmod_pow10(dscale - scale);
            if !r.is_zero() {
                return None;
            }
            q
        };
        let u = i128::try_from(c.to_u128()?).ok()?;
        Some(if *negative { -u } else { u })
    }

    /// The power of ten of its first digit; none for zero and the specials.
    fn top(&self) -> Option<i64> {
        match &self.0 {
            Repr::Finite { coef, dscale, .. } if !coef.is_zero() => {
                Some(coef.digit_count() as i64 - 1 - *dscale as i64)
            }
            _ => None,
        }
    }

    /// The value as `make_result` stores it, refused past what a numeric holds.
    fn checked(self) -> Result<Numeric> {
        if let Repr::Finite { dscale, .. } = &self.0 {
            if *dscale > DSCALE_MAX || self.top().is_some_and(|t| t >= INTEGER_DIGITS_MAX) {
                return Err(overflow());
            }
        }
        Ok(self)
    }

    /// A numeric as `numeric_in` reads it, under no typmod.
    pub fn parse(s: &str) -> Result<Numeric> {
        let b = s.as_bytes();
        let mut i = 0;
        while i < b.len() && is_space(b[i]) {
            i += 1;
        }
        let start = i;
        let mut negative = false;
        match b.get(i) {
            Some(b'+') => i += 1,
            Some(b'-') => {
                negative = true;
                i += 1
            }
            _ => {}
        }
        let rest_spaces = |from: usize| b[from..].iter().all(|&c| is_space(c));
        let first = b.get(i).copied();
        if !first.is_some_and(|c| c.is_ascii_digit() || c == b'.') {
            let at = |from: usize, word: &str| {
                b.len() >= from + word.len()
                    && b[from..from + word.len()].eq_ignore_ascii_case(word.as_bytes())
            };
            let (n, end) = if at(start, "NaN") {
                (Numeric::NAN, start + 3)
            } else if at(i, "Infinity") {
                (Numeric::infinity(negative), i + 8)
            } else if at(i, "inf") {
                (Numeric::infinity(negative), i + 3)
            } else {
                return Err(invalid(s));
            };
            if !rest_spaces(end) {
                return Err(invalid(s));
            }
            return Ok(n);
        }
        let base = match (b.get(i), b.get(i + 1)) {
            (Some(b'0'), Some(b'x' | b'X')) => 16,
            (Some(b'0'), Some(b'o' | b'O')) => 8,
            (Some(b'0'), Some(b'b' | b'B')) => 2,
            _ => 10,
        };
        if base == 10 {
            let (d, end) = decimal(s, i)?;
            if !rest_spaces(end) {
                return Err(invalid(s));
            }
            d.numeric(negative)
        } else {
            let (n, end) = non_decimal(s, i + 2, negative, base)?;
            if !rest_spaces(end) {
                return Err(invalid(s));
            }
            Ok(n)
        }
    }

    /// The value held to a typmod as `apply_typmod` and `apply_typmod_special` hold it: rounded to
    /// its scale, refused past its digits; NaN under any, an infinity under none.
    pub(crate) fn typmod(self, precision: u32, scale: i32) -> Result<Numeric> {
        match &self.0 {
            Repr::NaN => Ok(self),
            Repr::Infinity { .. } => Err(Error::new(format!(
                "numeric field overflow: A field with precision {precision}, scale {scale} \
                 cannot hold an infinite value."
            ))),
            Repr::Finite { .. } => {
                let r = self.round(scale);
                let max = precision as i64 - scale as i64;
                if r.top().is_some_and(|t| t + 1 > max) {
                    return Err(Error::new(format!(
                        "numeric field overflow: A field with precision {precision}, scale \
                         {scale} must round to an absolute value less than {}.",
                        if max == 0 {
                            "1".to_string()
                        } else {
                            format!("10^{max}")
                        }
                    )));
                }
                Ok(r)
            }
        }
    }

    /// Rounded half away from zero at `scale` places, which may be before the point, its display
    /// scale then `scale`, and never below 0.
    pub(crate) fn round(&self, scale: i32) -> Numeric {
        let Repr::Finite {
            negative,
            coef,
            dscale,
        } = &self.0
        else {
            return self.clone();
        };
        let d = *dscale as i64;
        let s = scale as i64;
        if s >= d {
            return Numeric::finite(*negative, coef.mul_pow10((s - d) as u32), s as u32);
        }
        let cut = (d - s) as u64;
        if cut > coef.digit_count() as u64 {
            // Every digit goes, and the first one dropped is a zero.
            return Numeric::finite(false, Nat::zero(), s.max(0) as u32);
        }
        let (mut q, r) = coef.divmod_pow10(cut as u32);
        if r.mul_small(2).cmp(&Nat::pow10(cut as u32)) != Ordering::Less {
            q = q.add(&Nat::from_u128(1));
        }
        if s < 0 {
            Numeric::finite(*negative, q.mul_pow10((-s) as u32), 0)
        } else {
            Numeric::finite(*negative, q, s as u32)
        }
    }

    /// Its trailing zeros past the point taken off, as `trim_scale` takes them.
    pub(crate) fn trimmed(&self) -> Numeric {
        match &self.0 {
            Repr::Finite {
                negative,
                coef,
                dscale,
            } => {
                let cut = coef.trailing_zeros().min(*dscale);
                let cut = if coef.is_zero() { *dscale } else { cut };
                Numeric::finite(*negative, coef.divmod_pow10(cut).0, dscale - cut)
            }
            _ => self.clone(),
        }
    }

    /// The key of its class: a string whose bytes sort as PostgreSQL orders numerics, equal for two
    /// values exactly where PostgreSQL's `=` holds. Its kind first (`0` -Infinity, `1` a negative
    /// number, `2` zero, `3` a positive one, `4` +Infinity, `5` NaN); then for a number the power of
    /// ten of its first digit, biased into six decimal digits, and its digits to its last nonzero
    /// one, each a decimal digit; a negative number's both taken from 9 and its digits ended by `:`.
    pub fn class_key(&self) -> String {
        match &self.0 {
            Repr::Infinity { negative: true } => "0".into(),
            Repr::Infinity { negative: false } => "4".into(),
            Repr::NaN => "5".into(),
            Repr::Finite { coef, .. } if coef.is_zero() => "2".into(),
            Repr::Finite { negative, coef, .. } => {
                let tz = coef.trailing_zeros();
                let digits = coef.digits();
                let sig = &digits[..digits.len() - tz as usize];
                let place = self.top().unwrap() + 500_000;
                let mut k = String::with_capacity(sig.len() + 8);
                if *negative {
                    k.push('1');
                    k.push_str(&format!("{:06}", 999_999 - place));
                    k.extend(sig.bytes().map(|c| (b'9' - (c - b'0')) as char));
                    k.push(':');
                } else {
                    k.push('3');
                    k.push_str(&format!("{place:06}"));
                    k.push_str(sig);
                }
                k
            }
        }
    }

    /// Its magnitude, `coef` at `scale` places, for two finite values at a common scale.
    fn aligned(coef: &Nat, dscale: u32, scale: u32) -> Nat {
        coef.mul_pow10(scale - dscale)
    }

    fn neg_inner(&self) -> Numeric {
        match &self.0 {
            Repr::NaN => Numeric::NAN,
            Repr::Infinity { negative } => Numeric::infinity(!negative),
            Repr::Finite {
                negative,
                coef,
                dscale,
            } => Numeric::finite(!negative, coef.clone(), *dscale),
        }
    }

    pub(crate) fn abs(&self) -> Numeric {
        match &self.0 {
            Repr::Infinity { .. } => Numeric::infinity(false),
            Repr::Finite { coef, dscale, .. } => Numeric::finite(false, coef.clone(), *dscale),
            Repr::NaN => Numeric::NAN,
        }
    }

    /// `self + o`, as `numeric_add` computes it: its scale the larger.
    pub(crate) fn add(&self, o: &Numeric) -> Result<Numeric> {
        match (&self.0, &o.0) {
            (Repr::NaN, _) | (_, Repr::NaN) => Ok(Numeric::NAN),
            (Repr::Infinity { negative: a }, Repr::Infinity { negative: b }) => Ok(if a == b {
                Numeric::infinity(*a)
            } else {
                Numeric::NAN
            }),
            (Repr::Infinity { negative }, _) | (_, Repr::Infinity { negative }) => {
                Ok(Numeric::infinity(*negative))
            }
            (
                Repr::Finite {
                    negative: na,
                    coef: a,
                    dscale: da,
                },
                Repr::Finite {
                    negative: nb,
                    coef: b,
                    dscale: db,
                },
            ) => {
                let d = (*da).max(*db);
                let (a, b) = (Numeric::aligned(a, *da, d), Numeric::aligned(b, *db, d));
                let n = if na == nb {
                    Numeric::finite(*na, a.add(&b), d)
                } else {
                    match a.cmp(&b) {
                        Ordering::Less => Numeric::finite(*nb, b.sub(&a), d),
                        _ => Numeric::finite(*na, a.sub(&b), d),
                    }
                };
                n.checked()
            }
        }
    }

    /// `self - o`, as `numeric_sub` computes it.
    pub(crate) fn sub(&self, o: &Numeric) -> Result<Numeric> {
        self.add(&o.neg_inner())
    }

    /// `self * o`, as `numeric_mul` computes it: exact at the sum of the scales, rounded to the most
    /// a numeric holds past them.
    pub(crate) fn mul(&self, o: &Numeric) -> Result<Numeric> {
        let inf = |negative: bool, other: &Numeric| match other.sign() {
            0 => Numeric::NAN,
            s => Numeric::infinity(negative != (s < 0)),
        };
        match (&self.0, &o.0) {
            (Repr::NaN, _) | (_, Repr::NaN) => Ok(Numeric::NAN),
            (Repr::Infinity { negative }, _) => Ok(inf(*negative, o)),
            (_, Repr::Infinity { negative }) => Ok(inf(*negative, self)),
            (
                Repr::Finite {
                    negative: na,
                    coef: a,
                    dscale: da,
                },
                Repr::Finite {
                    negative: nb,
                    coef: b,
                    dscale: db,
                },
            ) => {
                if let (Some(ta), Some(tb)) = (self.top(), o.top()) {
                    if ta + tb >= INTEGER_DIGITS_MAX {
                        return Err(overflow());
                    }
                }
                let n = Numeric::finite(na != nb, a.mul(b), da + db);
                let n = if da + db > DSCALE_MAX {
                    n.round(DSCALE_MAX as i32)
                } else {
                    n
                };
                n.checked()
            }
        }
    }

    /// The base-10000 weight of its first nonzero digit, and that digit: what `select_div_scale`
    /// reads; 0 and 0 for zero.
    fn nbase_lead(&self) -> (i64, u64) {
        let Some(top) = self.top() else {
            return (0, 0);
        };
        let Repr::Finite { coef, .. } = &self.0 else {
            return (0, 0);
        };
        let weight = top.div_euclid(4);
        // The digits from the first down to the place 10^(4·weight).
        let digits = coef.digits();
        let take = (top - 4 * weight + 1) as usize;
        let lead = digits
            .bytes()
            .chain(std::iter::repeat(b'0'))
            .take(take)
            .fold(0u64, |a, c| a * 10 + (c - b'0') as u64);
        (weight, lead)
    }

    /// The scale `numeric_div` gives `self / o` (`select_div_scale`).
    fn div_scale(&self, o: &Numeric) -> u32 {
        let (w1, f1) = self.nbase_lead();
        let (w2, f2) = o.nbase_lead();
        let mut qweight = w1 - w2;
        if f1 <= f2 {
            qweight -= 1;
        }
        let rscale = (MIN_SIG_DIGITS - qweight * 4)
            .max(self.dscale().unwrap_or(0) as i64)
            .max(o.dscale().unwrap_or(0) as i64)
            .clamp(0, MAX_DISPLAY_SCALE);
        rscale as u32
    }

    /// `self / o` at `scale` places, rounded half away from zero, or cut toward zero.
    fn quotient(&self, o: &Numeric, scale: u32, round: bool) -> Result<Numeric> {
        let (
            Repr::Finite {
                negative: na,
                coef: a,
                dscale: da,
            },
            Repr::Finite {
                negative: nb,
                coef: b,
                dscale: db,
            },
        ) = (&self.0, &o.0)
        else {
            unreachable!("a quotient of two numbers")
        };
        if b.is_zero() {
            return Err(division_by_zero());
        }
        if let (Some(ta), Some(tb)) = (self.top(), o.top()) {
            if ta - tb > INTEGER_DIGITS_MAX {
                return Err(overflow());
            }
        }
        let t = *db as i64 + scale as i64 - *da as i64;
        let (num, den) = if t >= 0 {
            (a.mul_pow10(t as u32), b.clone())
        } else {
            (a.clone(), b.mul_pow10((-t) as u32))
        };
        let (mut q, r) = num.divmod(&den);
        if round && r.mul_small(2).cmp(&den) != Ordering::Less {
            q = q.add(&Nat::from_u128(1));
        }
        Numeric::finite(na != nb, q, scale).checked()
    }

    /// `self / o`, as `numeric_div` computes it, at `select_div_scale`'s scale.
    pub(crate) fn div(&self, o: &Numeric) -> Result<Numeric> {
        match (&self.0, &o.0) {
            (Repr::NaN, _) | (_, Repr::NaN) => Ok(Numeric::NAN),
            (Repr::Infinity { .. }, Repr::Infinity { .. }) => Ok(Numeric::NAN),
            (Repr::Infinity { negative }, _) => match o.sign() {
                0 => Err(division_by_zero()),
                s => Ok(Numeric::infinity(*negative != (s < 0))),
            },
            (_, Repr::Infinity { .. }) => Ok(Numeric::zero()),
            _ => self.quotient(o, self.div_scale(o), true),
        }
    }

    /// `self % o`, as `numeric_mod` computes it: the sign the dividend's, the scale the larger.
    pub(crate) fn rem(&self, o: &Numeric) -> Result<Numeric> {
        match (&self.0, &o.0) {
            (Repr::NaN, _) | (_, Repr::NaN) => Ok(Numeric::NAN),
            (Repr::Infinity { .. }, _) => match o.sign() {
                0 => Err(division_by_zero()),
                _ => Ok(Numeric::NAN),
            },
            (_, Repr::Infinity { .. }) => Ok(self.clone()),
            (
                Repr::Finite {
                    negative: na,
                    coef: a,
                    dscale: da,
                },
                Repr::Finite {
                    coef: b,
                    dscale: db,
                    ..
                },
            ) => {
                if b.is_zero() {
                    return Err(division_by_zero());
                }
                let d = (*da).max(*db);
                let (_, r) = Numeric::aligned(a, *da, d).divmod(&Numeric::aligned(b, *db, d));
                Numeric::finite(*na, r, d).checked()
            }
        }
    }

    /// Negated, as unary minus.
    pub(crate) fn neg(&self) -> Numeric {
        self.neg_inner()
    }

    /// The integer nearest it, half away from zero, held to `[lo, hi]`; refused with the words of
    /// the cast to `name`.
    pub(crate) fn to_integer(&self, lo: i64, hi: i64, name: &str) -> Result<i64> {
        match &self.0 {
            Repr::NaN => Err(Error::new(format!("cannot convert NaN to {name}"))),
            Repr::Infinity { .. } => Err(Error::new(format!("cannot convert infinity to {name}"))),
            Repr::Finite { .. } => {
                let out = || Error::new(format!("{name} out of range"));
                let r = self.round(0);
                let v = r.units_at(0).ok_or_else(out)?;
                if v < lo as i128 || v > hi as i128 {
                    return Err(out());
                }
                Ok(v as i64)
            }
        }
    }

    /// The double nearest it, as `numeric_float8` reads its text; a real's where not `wide`.
    pub(crate) fn to_float(&self, wide: bool) -> Result<f64> {
        Ok(match &self.0 {
            Repr::NaN => f64::NAN,
            Repr::Infinity { negative } => {
                if *negative {
                    f64::NEG_INFINITY
                } else {
                    f64::INFINITY
                }
            }
            Repr::Finite { .. } => {
                let bits = crate::scalars::float_parse(&self.to_string(), wide)?;
                if wide {
                    f64::from_bits(bits)
                } else {
                    f64::from(f32::from_bits(bits as u32))
                }
            }
        })
    }

    /// A float as `float8_numeric` and `float4_numeric` take it: NaN and the infinities as they are,
    /// any other through its 15 significant digits, or a real's 6.
    pub(crate) fn of_float(f: f64, wide: bool) -> Numeric {
        if f.is_nan() {
            Numeric::NAN
        } else if f.is_infinite() {
            Numeric::infinity(f < 0.0)
        } else {
            Numeric::parse(&crate::scalars::float_numeric_text(f, wide))
                .expect("a float's digits read as a numeric")
        }
    }
}

/// A number written in base 10 as its digits were read: all of them, how many lie before the
/// point and after it, and the exponent written after them.
struct Decimal {
    digits: Vec<u8>,
    before: i64,
    after: i64,
    exponent: i64,
}

impl Decimal {
    /// The number the digits make, as `make_result` stores it, refused past what a numeric holds.
    fn numeric(self, negative: bool) -> Result<Numeric> {
        let dscale = (self.after - self.exponent).max(0);
        if dscale > DSCALE_MAX as i64 {
            return Err(overflow());
        }
        let Some(lead) = self.digits.iter().position(|&c| c != b'0') else {
            return Ok(Numeric::finite(false, Nat::zero(), dscale as u32));
        };
        // The power of ten of the first nonzero digit, found before the digits are made.
        if self.before - 1 - lead as i64 + self.exponent >= INTEGER_DIGITS_MAX {
            return Err(overflow());
        }
        let shift = self.exponent - self.after + dscale;
        Ok(Numeric::finite(
            negative,
            Nat::from_digits(&self.digits).mul_pow10(shift as u32),
            dscale as u32,
        ))
    }
}

/// The decimal digits of a number written in base 10, from `i` on, as `set_var_from_str` reads
/// them: digits with one point among them, an underscore only between digits, then an exponent.
fn decimal(s: &str, mut i: usize) -> Result<(Decimal, usize)> {
    let b = s.as_bytes();
    let mut digits: Vec<u8> = Vec::new();
    let mut have_dp = false;
    let mut before: i64 = 0;
    let mut after: i64 = 0;
    if b.get(i) == Some(&b'.') {
        have_dp = true;
        i += 1;
    }
    if !b.get(i).is_some_and(u8::is_ascii_digit) {
        return Err(invalid(s));
    }
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_digit() {
            digits.push(c);
            if have_dp {
                after += 1;
            } else {
                before += 1;
            }
            i += 1;
        } else if c == b'.' {
            if have_dp {
                return Err(invalid(s));
            }
            have_dp = true;
            i += 1;
            if b.get(i) == Some(&b'_') {
                return Err(invalid(s));
            }
        } else if c == b'_' {
            i += 1;
            if !b.get(i).is_some_and(u8::is_ascii_digit) {
                return Err(invalid(s));
            }
        } else {
            break;
        }
    }
    let mut exponent: i64 = 0;
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        let mut neg = false;
        match b.get(i) {
            Some(b'+') => i += 1,
            Some(b'-') => {
                neg = true;
                i += 1
            }
            _ => {}
        }
        if !b.get(i).is_some_and(u8::is_ascii_digit) {
            return Err(invalid(s));
        }
        while i < b.len() {
            let c = b[i];
            if c.is_ascii_digit() {
                exponent = exponent * 10 + (c - b'0') as i64;
                if exponent > i32::MAX as i64 / 2 {
                    return Err(overflow());
                }
                i += 1;
            } else if c == b'_' {
                i += 1;
                if !b.get(i).is_some_and(u8::is_ascii_digit) {
                    return Err(invalid(s));
                }
            } else {
                break;
            }
        }
        if neg {
            exponent = -exponent;
        }
    }
    Ok((
        Decimal {
            digits,
            before,
            after,
            exponent,
        },
        i,
    ))
}

/// An integer written in base 2, 8 or 16 after its prefix, from `i` on, as
/// `set_var_from_non_decimal_integer_str` reads it.
fn non_decimal(s: &str, mut i: usize, negative: bool, base: u32) -> Result<(Numeric, usize)> {
    let b = s.as_bytes();
    let digit = |c: u8| (c as char).to_digit(base);
    let first = i;
    let mut v = Nat::zero();
    while i < b.len() {
        let c = b[i];
        if let Some(d) = digit(c) {
            v = v.mul_small(base).add(&Nat::from_u128(d as u128));
            i += 1;
        } else if c == b'_' {
            i += 1;
            if !b.get(i).is_some_and(|&c| digit(c).is_some()) {
                return Err(invalid(s));
            }
        } else {
            break;
        }
    }
    if i == first {
        return Err(invalid(s));
    }
    let n = Numeric::finite(negative, v, 0);
    if n.top().is_some_and(|t| t >= INTEGER_DIGITS_MAX) {
        return Err(overflow());
    }
    Ok((n, i))
}

impl fmt::Display for Numeric {
    /// As `numeric_out` writes it: every digit of its display scale after the point, never an
    /// exponent.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Repr::NaN => f.write_str("NaN"),
            Repr::Infinity { negative: true } => f.write_str("-Infinity"),
            Repr::Infinity { negative: false } => f.write_str("Infinity"),
            Repr::Finite {
                negative,
                coef,
                dscale,
            } => {
                let digits = coef.digits();
                let s = *dscale as usize;
                let padded = format!("{:0>width$}", digits, width = s + 1);
                let (whole, frac) = padded.split_at(padded.len() - s);
                if *negative {
                    f.write_str("-")?;
                }
                f.write_str(whole)?;
                if s > 0 {
                    f.write_str(".")?;
                    f.write_str(frac)?;
                }
                Ok(())
            }
        }
    }
}

impl PartialEq for Numeric {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Numeric {}

impl PartialOrd for Numeric {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Numeric {
    /// As `cmp_numerics` compares: every NaN equal and above +Infinity, -Infinity below every
    /// number, and numbers by their values, whatever their scales.
    fn cmp(&self, other: &Self) -> Ordering {
        let rank = |n: &Numeric| match &n.0 {
            Repr::Infinity { negative: true } => 0,
            Repr::Finite { .. } => 1,
            Repr::Infinity { negative: false } => 2,
            Repr::NaN => 3,
        };
        match (&self.0, &other.0) {
            (
                Repr::Finite {
                    coef: a,
                    dscale: da,
                    ..
                },
                Repr::Finite {
                    coef: b,
                    dscale: db,
                    ..
                },
            ) => {
                let (sa, sb) = (self.sign(), other.sign());
                if sa != sb {
                    return sa.cmp(&sb);
                }
                let d = (*da).max(*db);
                let m = match (self.top(), other.top()) {
                    (Some(x), Some(y)) if x != y => x.cmp(&y),
                    _ => Numeric::aligned(a, *da, d).cmp(&Numeric::aligned(b, *db, d)),
                };
                if sa < 0 {
                    m.reverse()
                } else {
                    m
                }
            }
            _ => rank(self).cmp(&rank(other)),
        }
    }
}

/// The integer that stands for NaN in a numeric(p, s) column: `10^p`, the least past its digits.
pub fn nan_units(precision: u8) -> i128 {
    10i128.pow(u32::from(precision))
}

/// The bytes the plain library writes a decimal of `precision` digits in as a fixed array: the
/// fewest whose signed range holds every number of that many digits.
pub fn decimal_bytes(precision: u8) -> usize {
    let top = 10u128.pow(precision as u32);
    (1..=16)
        .find(|&n| top <= (1u128 << (8 * n - 1)))
        .unwrap_or(16)
}

/// A number held to a numeric(p, s) column, as the integer of its smallest unit: rounded to its
/// scale and refused past its digits, NaN at the greatest integer its page form holds.
pub(crate) fn column_units(n: &Numeric, precision: u8, scale: u8) -> Result<i128> {
    if n.is_nan() {
        return Ok(nan_units(precision));
    }
    let held = n.clone().typmod(u32::from(precision), i32::from(scale))?;
    held.units_at(u32::from(scale))
        .ok_or_else(|| Error::new("numeric field overflow"))
}

/// Where a number lies among a numeric(p, s) column's keys: the key equal to it, where one is; the
/// greatest key below it, one step below the least where none is; and the least key above it, none
/// above NaN's.
pub(crate) fn column_bounds(
    n: &Numeric,
    precision: u8,
    scale: u8,
) -> (Option<i128>, i128, Option<i128>) {
    let top = nan_units(precision);
    // The integer of a magnitude, where it lies within the column's digits.
    let within = |v: &Nat| {
        v.to_u128()
            .and_then(|u| i128::try_from(u).ok())
            .filter(|&u| u < top)
    };
    // A number past the column's digits: below its least, or between its greatest and NaN.
    let past = |negative: bool| {
        if negative {
            (-top, -top + 1)
        } else {
            (top - 1, top)
        }
    };
    match &n.0 {
        Repr::NaN => (Some(top), top - 1, None),
        Repr::Infinity { negative: false } => (None, top - 1, Some(top)),
        Repr::Infinity { negative: true } => (None, -top, Some(-top + 1)),
        Repr::Finite {
            negative,
            coef,
            dscale,
        } => {
            let s = u32::from(scale);
            let (floor, ceil) = if s >= *dscale {
                match within(&coef.mul_pow10(s - dscale)) {
                    Some(v) if *negative => (-v, -v),
                    Some(v) => (v, v),
                    None => past(*negative),
                }
            } else {
                let (q, r) = coef.divmod_pow10(dscale - s);
                match (within(&q), r.is_zero(), negative) {
                    (None, _, _) => past(*negative),
                    (Some(q), true, false) => (q, q),
                    (Some(q), true, true) => (-q, -q),
                    (Some(q), false, false) => (q, q + 1),
                    (Some(q), false, true) => (-q - 1, -q),
                }
            };
            let at = (floor == ceil).then_some(floor);
            (at, ceil - 1, Some(floor + 1))
        }
    }
}

/// The number a numeric(p, s) column's integer holds.
pub(crate) fn column_numeric(units: i128, precision: u8, scale: u8) -> Numeric {
    if units == nan_units(precision) {
        Numeric::NAN
    } else {
        Numeric::of_units(units, u32::from(scale))
    }
}

/// An answer's field of a numeric written as each value's text: Utf8, its metadata `pg_type`
/// naming the type.
pub fn text_field(name: &str, nullable: bool) -> arrow::datatypes::Field {
    arrow::datatypes::Field::new(name, arrow::datatypes::DataType::Utf8, nullable).with_metadata(
        [(crate::scalars::PG_TYPE.to_string(), "numeric".to_string())]
            .into_iter()
            .collect(),
    )
}

/// An answer's field of a numeric of type `ty`, as arrow's Parquet reader gives its page form:
/// Decimal128 of its precision and scale up to 38 digits, NaN at the greatest integer that form
/// holds; its text past them. Its metadata `pg_type` names the type.
pub fn numeric_field(
    name: &str,
    ty: crate::catalog::ColumnType,
    nullable: bool,
) -> arrow::datatypes::Field {
    use arrow::datatypes::DataType;
    let data_type = match ty {
        crate::catalog::ColumnType::Numeric { precision, scale } => {
            DataType::Decimal128(precision, scale as i8)
        }
        _ => DataType::Utf8,
    };
    arrow::datatypes::Field::new(name, data_type, nullable).with_metadata(
        [(crate::scalars::PG_TYPE.to_string(), ty.pg_name())]
            .into_iter()
            .collect(),
    )
}

/// Whether an answer's field holds a numeric, by its metadata.
pub fn is_numeric_field(f: &arrow::datatypes::Field) -> bool {
    f.metadata()
        .get(crate::scalars::PG_TYPE)
        .is_some_and(|t| t == "numeric" || t.starts_with("numeric("))
}

/// The key of the class of a numeric's text, as the file holds it.
pub(crate) fn class_of_text(s: &str) -> String {
    Numeric::parse(s).map_or_else(|_| s.to_string(), |n| n.class_key())
}

/// A running sum, and the rows it counts, as `do_numeric_accum` folds them: NaN and the infinities
/// counted apart, the numbers summed exactly at the largest scale among them.
#[derive(Clone, Debug, Default)]
pub(crate) struct Sum {
    rows: u64,
    nan: u64,
    pinf: u64,
    ninf: u64,
    /// The largest display scale among the numbers.
    scale: u32,
    /// The sum of the numbers at `scale`, while it fits 128 bits.
    small: i128,
    /// The sum of the numbers, once `small` could not hold it.
    big: Option<Numeric>,
    /// Whether the sum passed what a numeric holds.
    overflowed: bool,
}

impl Sum {
    /// Adds a number given as its units at `scale`.
    pub(crate) fn add_units(&mut self, units: i128, scale: u32) {
        self.rows += 1;
        if self.big.is_none() {
            let to = self.scale.max(scale);
            let up =
                |v: i128, from: u32| 10i128.checked_pow(to - from).and_then(|p| v.checked_mul(p));
            if let (Some(a), Some(b)) = (up(self.small, self.scale), up(units, scale)) {
                if let Some(t) = a.checked_add(b) {
                    self.small = t;
                    self.scale = to;
                    return;
                }
            }
        }
        self.add_big(&Numeric::of_units(units, scale));
    }

    /// Adds a number to the sum held whole.
    fn add_big(&mut self, n: &Numeric) {
        let total = self
            .big
            .take()
            .unwrap_or_else(|| Numeric::of_units(self.small, self.scale));
        self.scale = self.scale.max(n.dscale().unwrap_or(0));
        match total.add(n) {
            Ok(t) => self.big = Some(t),
            Err(_) => {
                self.overflowed = true;
                self.big = Some(total);
            }
        }
    }

    pub(crate) fn add(&mut self, n: &Numeric) {
        match &n.0 {
            Repr::NaN => self.nan += 1,
            Repr::Infinity { negative: true } => self.ninf += 1,
            Repr::Infinity { negative: false } => self.pinf += 1,
            Repr::Finite { .. } => match n.units() {
                Some((u, s)) => self.add_units(u, s),
                None => {
                    self.rows += 1;
                    self.add_big(n);
                }
            },
        }
    }

    /// Adds a number `times` over, as that many rows.
    pub(crate) fn add_times(&mut self, n: &Numeric, times: u64) {
        if times == 0 {
            return;
        }
        match (&n.0, n.units()) {
            (Repr::Finite { .. }, Some((u, s))) => match u.checked_mul(i128::from(times)) {
                Some(t) => {
                    self.add_units(t, s);
                    self.rows += times - 1;
                }
                None => (0..times).for_each(|_| self.add(n)),
            },
            (Repr::Finite { .. }, None) => (0..times).for_each(|_| self.add(n)),
            (Repr::NaN, _) => self.nan += times,
            (Repr::Infinity { negative: true }, _) => self.ninf += times,
            (Repr::Infinity { negative: false }, _) => self.pinf += times,
        }
    }

    pub(crate) fn merge(&mut self, o: &Sum) {
        self.nan += o.nan;
        self.pinf += o.pinf;
        self.ninf += o.ninf;
        self.overflowed |= o.overflowed;
        if o.rows == 0 {
            return;
        }
        let rows = self.rows;
        match &o.big {
            None => self.add_units(o.small, o.scale),
            Some(b) => self.add(b),
        }
        self.rows = rows + o.rows;
    }

    /// The number of rows summed, NaN and the infinities among them.
    pub(crate) fn count(&self) -> u64 {
        self.rows + self.nan + self.pinf + self.ninf
    }

    /// The sum, as `numeric_sum` gives it: none over no row; NaN where one was, or both infinities.
    pub(crate) fn total(&self) -> Option<Result<Numeric>> {
        if self.count() == 0 {
            return None;
        }
        Some(Ok(if self.nan > 0 || (self.pinf > 0 && self.ninf > 0) {
            Numeric::NAN
        } else if self.pinf > 0 {
            Numeric::infinity(false)
        } else if self.ninf > 0 {
            Numeric::infinity(true)
        } else if self.overflowed {
            return Some(Err(overflow()));
        } else {
            match &self.big {
                None => Numeric::of_units(self.small, self.scale),
                Some(b) => b.round(self.scale as i32),
            }
        }))
    }

    /// The mean, as `numeric_avg` gives it: the sum over the count, at `numeric_div`'s scale.
    pub(crate) fn avg(&self) -> Option<Result<Numeric>> {
        let total = self.total()?;
        Some(total.and_then(|t| {
            if t.is_finite() {
                t.div(&Numeric::of_units(self.rows as i128, 0))
            } else {
                Ok(t)
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(s: &str) -> Numeric {
        Numeric::parse(s).unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    #[test]
    fn a_numeric_is_read_and_written_as_numeric_in_and_numeric_out_do() {
        for (s, want) in [
            ("1.0", "1.0"),
            ("1.00", "1.00"),
            ("1e3", "1000"),
            ("1.5e3", "1500"),
            ("1e-20", "0.00000000000000000001"),
            ("123.4560", "123.4560"),
            ("1_000.5", "1000.5"),
            (" 1.50 ", "1.50"),
            (".5", "0.5"),
            ("5.", "5"),
            ("+inf", "Infinity"),
            ("infinity", "Infinity"),
            ("-Infinity", "-Infinity"),
            ("-inf", "-Infinity"),
            ("nan", "NaN"),
            (" NaN ", "NaN"),
            ("0x1F", "31"),
            ("-0x1f", "-31"),
            ("0o17", "15"),
            ("0b101", "5"),
            ("0_5", "5"),
            ("-0", "0"),
            ("-0.00", "0.00"),
            ("0e999999", "0"),
            ("1.5e-1", "0.15"),
            ("00012.300", "12.300"),
            ("1E+2", "100"),
        ] {
            assert_eq!(n(s).to_string(), want, "{s}");
        }
        for s in [
            "",
            " ",
            "+-5",
            "--5",
            "-+5",
            "+NaN",
            "-NaN",
            "NaNx",
            "infinit",
            "infinityx",
            ".",
            "1._5",
            "1_.5",
            "1__0",
            "_1",
            "1e",
            "1e_5",
            "0x",
            "0xg",
            "0b2",
            "1.2.3",
            "1 2",
            "x",
            "1e+",
        ] {
            assert!(Numeric::parse(s).is_err(), "{s:?}");
        }
        assert_eq!(n("1e131071").to_string().len(), 131072);
        assert_eq!(n("1e-16383").to_string().len(), 16385);
        for s in ["1e131072", "1e-16384", "0e-16384", "1e1073741824"] {
            assert_eq!(
                Numeric::parse(s).unwrap_err().to_string(),
                "value overflows numeric format",
                "{s}"
            );
        }
    }

    #[test]
    fn a_typmod_rounds_half_away_from_zero_and_refuses_past_its_digits() {
        let t = |s: &str, p: u32, sc: i32| n(s).typmod(p, sc).map(|x| x.to_string());
        assert_eq!(t("1.005", 5, 2).unwrap(), "1.01");
        assert_eq!(t("-1.005", 5, 2).unwrap(), "-1.01");
        assert_eq!(t("2.5", 2, 0).unwrap(), "3");
        assert_eq!(t("125", 3, -1).unwrap(), "130");
        assert_eq!(t("123", 3, -1).unwrap(), "120");
        assert_eq!(t("9994", 3, -1).unwrap(), "9990");
        assert_eq!(t("0.00123", 3, 5).unwrap(), "0.00123");
        assert_eq!(t("0.000005", 3, 5).unwrap(), "0.00001");
        assert_eq!(t("-0.000004", 3, 5).unwrap(), "0.00000");
        assert_eq!(t("NaN", 3, 1).unwrap(), "NaN");
        assert!(t("999.5", 3, 0).is_err());
        assert!(t("9995", 3, -1).is_err());
        assert!(t("0.01", 3, 5).is_err());
        assert!(t("Infinity", 10, 2).is_err());
        assert_eq!(t("0.00999", 3, 5).unwrap(), "0.00999");
    }

    /// The comparator of P4, written from `cmp_numerics`: the kind (NaN above +Infinity, -Infinity
    /// below every number), then for two numbers their signs, then their base-10000 weights, then
    /// their base-10000 digits from the first.
    fn reference_cmp(a: &Numeric, b: &Numeric) -> Ordering {
        fn digits(x: &Numeric) -> (i32, i64, Vec<u32>) {
            let sign = x.sign();
            if sign == 0 {
                return (0, 0, Vec::new());
            }
            let Repr::Finite { coef, dscale, .. } = &x.0 else {
                unreachable!()
            };
            // Pad to base-10000 digits: the point at a multiple of 4 places.
            let frac = (*dscale as usize).div_ceil(4) * 4;
            let mut s = format!("{:0>w$}", coef.digits(), w = *dscale as usize);
            s.push_str(&"0".repeat(frac - *dscale as usize));
            let whole = s.len() - frac;
            let pad = (4 - whole % 4) % 4;
            let s = format!("{}{s}", "0".repeat(pad));
            let mut d: Vec<u32> = s
                .as_bytes()
                .chunks(4)
                .map(|c| c.iter().fold(0u32, |a, &x| a * 10 + (x - b'0') as u32))
                .collect();
            let mut weight = ((whole + pad) / 4) as i64 - 1;
            while d.first() == Some(&0) {
                d.remove(0);
                weight -= 1;
            }
            while d.last() == Some(&0) {
                d.pop();
            }
            (sign, weight, d)
        }
        let kind = |x: &Numeric| match &x.0 {
            Repr::Infinity { negative: true } => 0,
            Repr::Finite { .. } => 1,
            Repr::Infinity { negative: false } => 2,
            Repr::NaN => 3,
        };
        if kind(a) != 1 || kind(b) != 1 {
            return kind(a).cmp(&kind(b));
        }
        let (sa, wa, da) = digits(a);
        let (sb, wb, db) = digits(b);
        if sa != sb {
            return sa.cmp(&sb);
        }
        if sa == 0 {
            return Ordering::Equal;
        }
        let abs = wa.cmp(&wb).then_with(|| da.cmp(&db));
        if sa < 0 {
            abs.reverse()
        } else {
            abs
        }
    }

    /// Every numeric of up to three digits, the point anywhere among them or beside them, either
    /// sign, and NaN and the infinities: the whole of that domain.
    fn small_domain() -> Vec<Numeric> {
        let mut out = vec![
            Numeric::NAN,
            Numeric::infinity(false),
            Numeric::infinity(true),
        ];
        for len in 1..=3u32 {
            for v in 0..10u32.pow(len) {
                let d = format!("{v:0>w$}", w = len as usize);
                for point in 0..=len as usize {
                    for sign in ["", "-"] {
                        let s = format!("{sign}{}.{}", &d[..point], &d[point..]);
                        out.push(n(&s));
                    }
                }
                for e in [-5, 4] {
                    out.push(n(&format!("{d}e{e}")));
                }
            }
        }
        out
    }

    #[test]
    fn every_small_numeric_s_class_key_sorts_as_postgresql_compares_numerics() {
        // Sorted by the comparator, every neighbour pair compared: both orders being total, the
        // keys then agree with it on every pair of the domain.
        let mut all = small_domain();
        all.sort_by(reference_cmp);
        let keys: Vec<String> = all.iter().map(Numeric::class_key).collect();
        for i in 1..all.len() {
            let (a, b) = (&all[i - 1], &all[i]);
            let want = reference_cmp(a, b);
            assert_eq!(
                keys[i - 1].as_bytes().cmp(keys[i].as_bytes()),
                want,
                "{a} {b}"
            );
            assert_eq!(a.cmp(b), want, "{a} {b}");
        }
        // Each key's bytes past the kind and the exponent are decimal digits, and every key is
        // ASCII.
        for k in &keys {
            assert!(k.is_ascii());
            if k.len() > 1 {
                assert!(
                    k[7..].bytes().all(|c| c.is_ascii_digit() || c == b':'),
                    "{k}"
                );
            }
        }
        assert_eq!(n("1.0").class_key(), n("1.00").class_key());
        assert_eq!(n("1000").class_key(), n("1e3").class_key());
        assert_eq!(n("-0").class_key(), n("0.00").class_key());
    }

    #[test]
    fn the_ends_of_a_numeric_keep_their_order() {
        let ends = [
            "-Infinity",
            "-9e131071",
            "-1e131071",
            "-1",
            "-1e-16383",
            "0",
            "1e-16383",
            "0.33333333333333333333",
            "1",
            "99999999999999999999999999999999999999.5",
            "1e131071",
            "9e131071",
            "Infinity",
            "NaN",
        ];
        for w in ends.windows(2) {
            assert!(n(w[0]) < n(w[1]), "{} {}", w[0], w[1]);
            assert!(
                n(w[0]).class_key() < n(w[1]).class_key(),
                "{} {}",
                w[0],
                w[1]
            );
        }
    }

    /// A decimal reference, written from `numeric.c`: two numbers as integers at their scales;
    /// a sum at the larger scale, a product at the sum of the scales, a quotient by
    /// `select_div_scale` rounded half away from zero, a remainder with the dividend's sign.
    fn reference(op: char, a: (i128, u32), b: (i128, u32)) -> Option<(i128, u32)> {
        let up = |(m, s): (i128, u32), to: u32| m * 10i128.pow(to - s);
        match op {
            '+' | '-' => {
                let d = a.1.max(b.1);
                let y = if op == '+' { up(b, d) } else { -up(b, d) };
                Some((up(a, d) + y, d))
            }
            '*' => Some((a.0 * b.0, a.1 + b.1)),
            '%' => {
                if b.0 == 0 {
                    return None;
                }
                let d = a.1.max(b.1);
                Some((up(a, d) % up(b, d), d))
            }
            '/' => {
                if b.0 == 0 {
                    return None;
                }
                // The base-10000 weight and first digit of each, from its decimal digits.
                let lead = |(m, s): (i128, u32)| -> (i64, i128) {
                    if m == 0 {
                        return (0, 0);
                    }
                    let digits = m.unsigned_abs().to_string();
                    let top = digits.len() as i64 - 1 - s as i64;
                    let w = top.div_euclid(4);
                    let take = (top - 4 * w + 1) as usize;
                    let lead: String = digits
                        .chars()
                        .chain(std::iter::repeat('0'))
                        .take(take)
                        .collect();
                    (w, lead.parse().unwrap())
                };
                let ((w1, f1), (w2, f2)) = (lead(a), lead(b));
                let q = w1 - w2 - (f1 <= f2) as i64;
                let r = (16 - q * 4).max(a.1 as i64).max(b.1 as i64).clamp(0, 1000) as u32;
                let t = b.1 as i64 + r as i64 - a.1 as i64;
                let (num, den) = if t >= 0 {
                    (a.0.checked_mul(10i128.checked_pow(t as u32)?)?, b.0)
                } else {
                    (a.0, b.0.checked_mul(10i128.checked_pow((-t) as u32)?)?)
                };
                let (q, rem) = (num / den, num % den);
                let q = if 2 * rem.abs() >= den.abs() {
                    q + if (num < 0) != (den < 0) { -1 } else { 1 }
                } else {
                    q
                };
                Some((q, r))
            }
            _ => None,
        }
    }

    /// A seeded walk of arithmetic against the reference: pairs of numbers of up to eight digits
    /// at scales 0 to 4, each operation's value and scale.
    #[test]
    fn arithmetic_is_numeric_c_s_over_a_seeded_walk() {
        let mut state: u64 = 0x006e_756d_6572_6963;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..20_000 {
            let mut pick = || {
                let digits = (next() % 9) as u32;
                let m = (next() % 10u64.pow(digits)) as i128;
                let m = if next() % 2 == 0 { -m } else { m };
                (m, (next() % 5) as u32)
            };
            let (a, b) = (pick(), pick());
            let (x, y) = (Numeric::of_units(a.0, a.1), Numeric::of_units(b.0, b.1));
            for op in ['+', '-', '*', '/', '%'] {
                let got = match op {
                    '+' => x.add(&y),
                    '-' => x.sub(&y),
                    '*' => x.mul(&y),
                    '/' => x.div(&y),
                    _ => x.rem(&y),
                };
                match reference(op, a, b) {
                    Some((m, s)) => {
                        let got = got.unwrap();
                        assert_eq!(
                            got.to_string(),
                            Numeric::of_units(m, s).to_string(),
                            "{x} {op} {y}"
                        );
                    }
                    None => assert!(got.is_err(), "{x} {op} {y}"),
                }
            }
        }
    }

    #[test]
    fn arithmetic_holds_postgresql_s_own_answers() {
        let op = |a: &str, o: char, b: &str| {
            let (a, b) = (n(a), n(b));
            match o {
                '+' => a.add(&b),
                '-' => a.sub(&b),
                '*' => a.mul(&b),
                '/' => a.div(&b),
                _ => a.rem(&b),
            }
            .map(|x| x.to_string())
            .unwrap_or_else(|e| e.to_string())
        };
        // Each read from PostgreSQL 19 RC1 on 28819, 2026-10-30.
        for (a, o, b, want) in [
            ("1", '/', "3", "0.33333333333333333333"),
            ("10", '/', "4", "2.5000000000000000"),
            ("100000", '/', "3", "33333.333333333333"),
            (
                "1e-20",
                '/',
                "3",
                "0.0000000000000000000033333333333333333333",
            ),
            ("2", '/', "3e10", "0.0000000000666666666666666667"),
            (
                "123456789012345678901234567890",
                '/',
                "7",
                "17636684144620811271604938270",
            ),
            ("7.5", '%', "2", "1.5"),
            ("-7.5", '%', "2", "-1.5"),
            ("7.5", '%', "-2.25", "0.75"),
            ("1e20", '%', "7", "2"),
            ("0.0", '%', "5", "0.0"),
            ("1.50", '+', "2.255", "3.755"),
            ("1.50", '*', "2.255", "3.38250"),
            ("1.5", '-', "1.5", "0.0"),
            ("-1.5", '*', "0", "0.0"),
            ("NaN", '+', "1", "NaN"),
            ("Infinity", '-', "Infinity", "NaN"),
            ("Infinity", '*', "0", "NaN"),
            ("5", '/', "Infinity", "0"),
            ("NaN", '%', "0", "NaN"),
            ("1", '/', "0", "division by zero"),
            ("inf", '%', "0", "division by zero"),
            ("inf", '%', "2", "NaN"),
            ("5", '%', "inf", "5"),
            ("4.00", '/', "3", "1.3333333333333333"),
        ] {
            assert_eq!(op(a, o, b), want, "{a} {o} {b}");
        }
        assert_eq!(n("2.5").round(0).to_string(), "3");
        assert_eq!(n("-2.5").round(0).to_string(), "-3");
        assert_eq!(n("1.2345").round(2).to_string(), "1.23");
        assert_eq!(n("1234.5").round(-2).to_string(), "1200");
    }

    #[test]
    fn a_sum_and_an_avg_are_numeric_sum_s_and_numeric_avg_s() {
        let mut s = Sum::default();
        for x in ["1.0", "1.00", "2"] {
            s.add(&n(x));
        }
        assert_eq!(s.total().unwrap().unwrap().to_string(), "4.00");
        assert_eq!(s.avg().unwrap().unwrap().to_string(), "1.3333333333333333");
        let mut s = Sum::default();
        for x in [1, 2, 2] {
            s.add_units(x, 0);
        }
        assert_eq!(s.avg().unwrap().unwrap().to_string(), "1.6666666666666667");
        let mut s = Sum::default();
        s.add(&n("Infinity"));
        s.add(&n("1"));
        assert_eq!(s.total().unwrap().unwrap().to_string(), "Infinity");
        s.add(&n("-inf"));
        assert_eq!(s.total().unwrap().unwrap().to_string(), "NaN");
        assert!(Sum::default().total().is_none());
        // Past 128 bits the sum is held exactly.
        let mut s = Sum::default();
        let big = "99999999999999999999999999999999999999.5";
        for _ in 0..4 {
            s.add(&n(big));
        }
        let mut t = Sum::default();
        t.merge(&s);
        t.merge(&s);
        assert_eq!(
            t.total().unwrap().unwrap().to_string(),
            "799999999999999999999999999999999999996.0"
        );
    }

    #[test]
    fn nan_lies_above_every_number_a_column_s_page_form_holds() {
        assert_eq!(decimal_bytes(19), 9);
        assert_eq!(decimal_bytes(21), 9);
        assert_eq!(decimal_bytes(22), 10);
        assert_eq!(decimal_bytes(38), 16);
        for p in 1..=38u8 {
            assert_eq!(nan_units(p), 10i128.pow(p as u32), "{p}");
            if p <= 18 {
                assert!(i64::try_from(nan_units(p)).is_ok(), "{p}");
            } else {
                // The fewest bytes whose signed range holds p digits, as the plain library sizes
                // them, hold NaN's integer too.
                let n = decimal_bytes(p);
                assert!(10u128.pow(p as u32) < 1u128 << (8 * n - 1), "{p}");
                assert!(n == 1 || 10u128.pow(p as u32) > 1u128 << (8 * (n - 1) - 1));
            }
            assert_eq!(column_numeric(nan_units(p), p, 0).to_string(), "NaN");
        }
    }

    /// Every key of a numeric(2, 1) column, NaN's among them, against every number of up to three
    /// digits at scales 0 to 3, past the column's both ways, and NaN and the infinities: the keys a
    /// comparison keeps are those whose numbers PostgreSQL's comparison keeps.
    #[test]
    fn a_comparison_keeps_the_keys_whose_numbers_it_keeps() {
        let (p, s) = (2u8, 1u8);
        let keys: Vec<i128> = (-99..=99).chain([nan_units(p)]).collect();
        let mut literals = vec![
            Numeric::NAN,
            Numeric::infinity(false),
            Numeric::infinity(true),
            n("1e40"),
            n("-1e40"),
        ];
        for m in -1100..=1100 {
            for sc in 0..=3 {
                literals.push(Numeric::of_units(m, sc));
            }
        }
        for lit in &literals {
            let (at, below, above) = column_bounds(lit, p, s);
            for &k in &keys {
                let v = column_numeric(k, p, s);
                let o = v.cmp(lit);
                assert_eq!(at == Some(k), o == Ordering::Equal, "{lit} = {v}");
                assert_eq!(k <= below, o == Ordering::Less, "{v} < {lit}");
                assert_eq!(
                    above.is_some_and(|a| k >= a),
                    o == Ordering::Greater,
                    "{v} > {lit}"
                );
            }
        }
    }

    #[test]
    fn long_division_gives_back_its_dividend() {
        let mut state: u64 = 0x5eed;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..2_000 {
            let digits = |k: u64| -> String {
                let len = 1 + (k % 60) as usize;
                (0..len)
                    .map(|i| (b'0' + ((k >> (i % 50)) % 10) as u8) as char)
                    .collect()
            };
            let a = Nat::from_digits(digits(next()).as_bytes());
            let b = Nat::from_digits(digits(next()).as_bytes());
            if b.is_zero() {
                continue;
            }
            let (q, r) = a.divmod(&b);
            assert_eq!(r.cmp(&b), Ordering::Less);
            assert_eq!(q.mul(&b).add(&r), a, "{} / {}", a.digits(), b.digits());
        }
    }
}
