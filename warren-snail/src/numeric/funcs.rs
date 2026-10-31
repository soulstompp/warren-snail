//! numeric's functions as `numeric.c` computes them: on a finite number held in base-10000
//! digits, as PostgreSQL holds one while it computes (`NumericVar`), each step at the scale
//! `numeric.c` gives it.
#![allow(clippy::approx_constant)]

use super::{division_by_zero, overflow, Nat, Numeric, Repr};
use crate::{Error, Result};
use std::cmp::Ordering;

const NBASE: i32 = 10000;
const HALF_NBASE: i32 = 5000;
const DEC_DIGITS: i32 = 4;
const MUL_GUARD_DIGITS: i32 = 2;
const DIV_GUARD_DIGITS: i32 = 4;
const NBASE_SQR: i64 = 100_000_000;
const ROUND_POWERS: [i32; 4] = [0, 1000, 100, 10];
/// The greatest weight a stored numeric holds, and the greatest display scale.
const WEIGHT_MAX: i32 = i16::MAX as i32;
const DSCALE_MAX: i32 = 0x3FFF;
const MIN_SIG_DIGITS: i32 = 16;
const MAX_DISPLAY_SCALE: i32 = 1000;
const MIN_DISPLAY_SCALE: i32 = 0;
const MAX_RESULT_SCALE: i32 = 2000;

fn negative_root() -> Error {
    Error::new("cannot take square root of a negative number")
}

fn log_of_zero() -> Error {
    Error::new("cannot take logarithm of zero")
}

fn log_of_negative() -> Error {
    Error::new("cannot take logarithm of a negative number")
}

fn complex_power() -> Error {
    Error::new("a negative number raised to a non-integer power yields a complex result")
}

fn zero_to_negative() -> Error {
    Error::new("zero raised to a negative power is undefined")
}

/// A finite number as `numeric.c` computes on it: its sign, the weight of its first base-10000
/// digit, its display scale and its digits, the first carrying the weight.
#[derive(Clone, Debug)]
struct Var {
    negative: bool,
    weight: i32,
    dscale: i32,
    digits: Vec<i32>,
}

impl Var {
    fn zero(dscale: i32) -> Var {
        Var {
            negative: false,
            weight: 0,
            dscale,
            digits: Vec::new(),
        }
    }

    fn of(negative: bool, weight: i32, dscale: i32, digits: &[i32]) -> Var {
        Var {
            negative,
            weight,
            dscale,
            digits: digits.to_vec(),
        }
    }

    fn one() -> Var {
        Var::of(false, 0, 0, &[1])
    }

    fn two() -> Var {
        Var::of(false, 0, 0, &[2])
    }

    fn zero_point_nine() -> Var {
        Var::of(false, -1, 1, &[9000])
    }

    fn one_point_one() -> Var {
        Var::of(false, 0, 1, &[1, 1000])
    }

    fn ndigits(&self) -> i32 {
        self.digits.len() as i32
    }

    fn digit(&self, i: i32) -> i32 {
        if i >= 0 && i < self.ndigits() {
            self.digits[i as usize]
        } else {
            0
        }
    }

    /// An integer as `int64_to_numericvar` makes it.
    fn of_int(v: i64) -> Var {
        let mut u = v.unsigned_abs();
        let mut d = Vec::new();
        while u > 0 {
            d.push((u % NBASE as u64) as i32);
            u /= NBASE as u64;
        }
        d.reverse();
        Var {
            negative: v < 0,
            weight: d.len() as i32 - 1,
            dscale: 0,
            digits: d,
        }
        .zero_weight()
    }

    /// A natural number's decimal digits as a nonnegative integer.
    fn of_decimal(s: &str) -> Var {
        let s = s.trim_start_matches('0');
        if s.is_empty() {
            return Var::zero(0);
        }
        let pad = (4 - s.len() % 4) % 4;
        let padded = format!("{}{s}", "0".repeat(pad));
        let digits: Vec<i32> = padded
            .as_bytes()
            .chunks(4)
            .map(|c| c.iter().fold(0, |a, &b| a * 10 + (b - b'0') as i32))
            .collect();
        Var {
            negative: false,
            weight: digits.len() as i32 - 1,
            dscale: 0,
            digits,
        }
    }

    fn zero_weight(mut self) -> Var {
        if self.digits.is_empty() {
            self.weight = 0;
        }
        self
    }

    /// A finite numeric as `init_var_from_num` reads it.
    fn of_numeric(n: &Numeric) -> Var {
        let Repr::Finite {
            negative,
            coef,
            dscale,
        } = &n.0
        else {
            unreachable!("a numeric's digits are read only where it is finite")
        };
        let d = coef.digits();
        if d.is_empty() {
            return Var::zero(*dscale as i32);
        }
        let d = d.as_bytes();
        let low = -(*dscale as i64);
        let top = low + d.len() as i64 - 1;
        let digit_at = |p: i64| -> i32 {
            if p < low || p > top {
                0
            } else {
                (d[(top - p) as usize] - b'0') as i32
            }
        };
        let (wt, wb) = (top.div_euclid(4), low.div_euclid(4));
        let digits = (wb..=wt)
            .rev()
            .map(|w| (0..4).rev().fold(0, |a, k| a * 10 + digit_at(4 * w + k)))
            .collect();
        let mut v = Var {
            negative: *negative,
            weight: wt as i32,
            dscale: *dscale as i32,
            digits,
        };
        v.strip();
        v
    }

    /// The numeric `make_result` stores, refused where its weight or display scale passes what a
    /// numeric holds, written to its display scale as `get_str_from_var` writes it.
    fn numeric(mut self) -> Result<Numeric> {
        self.strip();
        if (!self.digits.is_empty() && (self.weight > WEIGHT_MAX || self.weight < i16::MIN as i32))
            || self.dscale < 0
            || self.dscale > DSCALE_MAX
        {
            return Err(overflow());
        }
        let mut s = String::new();
        if self.weight >= 0 {
            for d in 0..=self.weight {
                s.push_str(&format!("{:04}", self.digit(d)));
            }
        }
        let mut frac = String::new();
        let mut d = self.weight + 1;
        while (frac.len() as i32) < self.dscale {
            frac.push_str(&format!("{:04}", self.digit(d)));
            d += 1;
        }
        frac.truncate(self.dscale as usize);
        s.push_str(&frac);
        Ok(Numeric::finite(
            self.negative,
            Nat::from_digits(s.as_bytes()),
            self.dscale as u32,
        ))
    }

    /// Its text as `get_str_from_var` writes it, to its display scale.
    fn text(&self) -> String {
        let mut s = String::new();
        if self.negative {
            s.push('-');
        }
        let mut d;
        if self.weight < 0 {
            d = self.weight + 1;
            s.push('0');
        } else {
            for i in 0..=self.weight {
                if i == 0 {
                    s.push_str(&self.digit(i).to_string());
                } else {
                    s.push_str(&format!("{:04}", self.digit(i)));
                }
            }
            d = self.weight + 1;
        }
        if self.dscale > 0 {
            s.push('.');
            let mut frac = String::new();
            let mut i = 0;
            while i < self.dscale {
                frac.push_str(&format!("{:04}", self.digit(d)));
                d += 1;
                i += DEC_DIGITS;
            }
            frac.truncate(self.dscale as usize);
            s.push_str(&frac);
        }
        s
    }

    /// The double its text reads as, an infinity past a double's range
    /// (`numericvar_to_double_no_overflow`).
    fn to_double(&self) -> f64 {
        self.text().parse().unwrap_or(f64::NAN)
    }

    /// The nearest integer, half away from zero, where it fits 64 bits (`numericvar_to_int64`).
    fn to_int64(&self) -> Option<i64> {
        let mut r = self.clone();
        r.round(0);
        r.strip();
        if r.digits.is_empty() {
            return Some(0);
        }
        let mut v: i128 = 0;
        for i in 0..=r.weight {
            v = v
                .checked_mul(NBASE as i128)?
                .checked_add(r.digit(i) as i128)?;
            if v > i64::MAX as i128 + 1 {
                return None;
            }
        }
        let v = if r.negative { -v } else { v };
        i64::try_from(v).ok()
    }

    /// `strip_var`: no zero digit first or last; zero positive, its weight 0.
    fn strip(&mut self) {
        let lead = self.digits.iter().take_while(|&&d| d == 0).count();
        if lead > 0 {
            self.digits.drain(..lead);
            self.weight -= lead as i32;
        }
        while self.digits.last() == Some(&0) {
            self.digits.pop();
        }
        if self.digits.is_empty() {
            self.negative = false;
            self.weight = 0;
        }
    }

    /// `round_var`: rounded half away from zero at `rscale` places, which may be before the point.
    fn round(&mut self, rscale: i32) {
        self.dscale = rscale;
        let mut di = (self.weight + 1) * DEC_DIGITS + rscale;
        if di < 0 {
            self.digits.clear();
            self.weight = 0;
            self.negative = false;
            return;
        }
        let mut ndigits = (di + DEC_DIGITS - 1) / DEC_DIGITS;
        di %= DEC_DIGITS;
        let n = self.ndigits();
        if !(ndigits < n || (ndigits == n && di > 0)) {
            return;
        }
        let mut carry;
        if di == 0 {
            carry = (self.digits[ndigits as usize] >= HALF_NBASE) as i32;
            self.digits.truncate(ndigits as usize);
        } else {
            self.digits.truncate(ndigits as usize);
            ndigits -= 1;
            let mut pow10 = ROUND_POWERS[di as usize];
            let extra = self.digits[ndigits as usize] % pow10;
            self.digits[ndigits as usize] -= extra;
            carry = 0;
            if extra >= pow10 / 2 {
                pow10 += self.digits[ndigits as usize];
                if pow10 >= NBASE {
                    pow10 -= NBASE;
                    carry = 1;
                }
                self.digits[ndigits as usize] = pow10;
            }
        }
        while carry != 0 {
            ndigits -= 1;
            if ndigits < 0 {
                self.digits.insert(0, carry);
                self.weight += 1;
                break;
            }
            let c = carry + self.digits[ndigits as usize];
            if c >= NBASE {
                self.digits[ndigits as usize] = c - NBASE;
                carry = 1;
            } else {
                self.digits[ndigits as usize] = c;
                carry = 0;
            }
        }
    }

    /// `trunc_var`: cut toward zero at `rscale` places, which may be before the point.
    fn trunc(&mut self, rscale: i32) {
        self.dscale = rscale;
        let mut di = (self.weight + 1) * DEC_DIGITS + rscale;
        if di <= 0 {
            self.digits.clear();
            self.weight = 0;
            self.negative = false;
            return;
        }
        let mut ndigits = (di + DEC_DIGITS - 1) / DEC_DIGITS;
        if ndigits <= self.ndigits() {
            self.digits.truncate(ndigits as usize);
            di %= DEC_DIGITS;
            if di > 0 {
                ndigits -= 1;
                let pow10 = ROUND_POWERS[di as usize];
                let extra = self.digits[ndigits as usize] % pow10;
                self.digits[ndigits as usize] -= extra;
            }
        }
    }
}

/// `cmp_abs`.
fn cmp_abs(a: &Var, b: &Var) -> Ordering {
    let (mut i1, mut i2) = (0i32, 0i32);
    let (mut w1, mut w2) = (a.weight, b.weight);
    let (n1, n2) = (a.ndigits(), b.ndigits());
    while w1 > w2 && i1 < n1 {
        if a.digit(i1) != 0 {
            return Ordering::Greater;
        }
        i1 += 1;
        w1 -= 1;
    }
    while w2 > w1 && i2 < n2 {
        if b.digit(i2) != 0 {
            return Ordering::Less;
        }
        i2 += 1;
        w2 -= 1;
    }
    if w1 == w2 {
        while i1 < n1 && i2 < n2 {
            match a.digit(i1).cmp(&b.digit(i2)) {
                Ordering::Equal => {}
                o => return o,
            }
            i1 += 1;
            i2 += 1;
        }
    }
    while i1 < n1 {
        if a.digit(i1) != 0 {
            return Ordering::Greater;
        }
        i1 += 1;
    }
    while i2 < n2 {
        if b.digit(i2) != 0 {
            return Ordering::Less;
        }
        i2 += 1;
    }
    Ordering::Equal
}

/// `cmp_var`.
fn cmp(a: &Var, b: &Var) -> Ordering {
    match (a.digits.is_empty(), b.digits.is_empty()) {
        (true, true) => return Ordering::Equal,
        (true, false) => {
            return if b.negative {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (false, true) => {
            return if a.negative {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        _ => {}
    }
    match (a.negative, b.negative) {
        (false, true) => Ordering::Greater,
        (true, false) => Ordering::Less,
        (false, false) => cmp_abs(a, b),
        (true, true) => cmp_abs(b, a),
    }
}

/// `add_abs`: |a| + |b|, positive.
fn add_abs(a: &Var, b: &Var) -> Var {
    let res_weight = a.weight.max(b.weight) + 1;
    let res_dscale = a.dscale.max(b.dscale);
    let r1 = a.ndigits() - a.weight - 1;
    let r2 = b.ndigits() - b.weight - 1;
    let res_rscale = r1.max(r2);
    let res_ndigits = (res_rscale + res_weight + 1).max(1);
    let mut out = vec![0i32; res_ndigits as usize];
    let mut i1 = res_rscale + a.weight + 1;
    let mut i2 = res_rscale + b.weight + 1;
    let mut carry = 0;
    for i in (0..res_ndigits).rev() {
        i1 -= 1;
        i2 -= 1;
        carry += a.digit(i1) + b.digit(i2);
        if carry >= NBASE {
            out[i as usize] = carry - NBASE;
            carry = 1;
        } else {
            out[i as usize] = carry;
            carry = 0;
        }
    }
    let mut v = Var {
        negative: false,
        weight: res_weight,
        dscale: res_dscale,
        digits: out,
    };
    v.strip();
    v
}

/// `sub_abs`: |a| - |b|, where |a| is not the less, positive.
fn sub_abs(a: &Var, b: &Var) -> Var {
    let res_weight = a.weight;
    let res_dscale = a.dscale.max(b.dscale);
    let r1 = a.ndigits() - a.weight - 1;
    let r2 = b.ndigits() - b.weight - 1;
    let res_rscale = r1.max(r2);
    let res_ndigits = (res_rscale + res_weight + 1).max(1);
    let mut out = vec![0i32; res_ndigits as usize];
    let mut i1 = res_rscale + a.weight + 1;
    let mut i2 = res_rscale + b.weight + 1;
    let mut borrow = 0;
    for i in (0..res_ndigits).rev() {
        i1 -= 1;
        i2 -= 1;
        borrow += a.digit(i1) - b.digit(i2);
        if borrow < 0 {
            out[i as usize] = borrow + NBASE;
            borrow = -1;
        } else {
            out[i as usize] = borrow;
            borrow = 0;
        }
    }
    let mut v = Var {
        negative: false,
        weight: res_weight,
        dscale: res_dscale,
        digits: out,
    };
    v.strip();
    v
}

/// `add_var`.
fn add(a: &Var, b: &Var) -> Var {
    if a.negative == b.negative {
        let mut r = add_abs(a, b);
        r.negative = a.negative && !r.digits.is_empty();
        return r;
    }
    signed_difference(a, b, a.negative)
}

/// `sub_var`.
fn sub(a: &Var, b: &Var) -> Var {
    if a.negative != b.negative {
        let mut r = add_abs(a, b);
        r.negative = a.negative && !r.digits.is_empty();
        return r;
    }
    signed_difference(a, b, a.negative)
}

/// |a| - |b| as a signed number, `a_negative` the sign |a| carries.
fn signed_difference(a: &Var, b: &Var, a_negative: bool) -> Var {
    match cmp_abs(a, b) {
        Ordering::Equal => Var::zero(a.dscale.max(b.dscale)),
        Ordering::Greater => {
            let mut r = sub_abs(a, b);
            r.negative = a_negative && !r.digits.is_empty();
            r
        }
        Ordering::Less => {
            let mut r = sub_abs(b, a);
            r.negative = !a_negative && !r.digits.is_empty();
            r
        }
    }
}

/// `mul_var`: the product rounded to `rscale` places.
fn mul(a: &Var, b: &Var, rscale: i32) -> Var {
    let (v1, v2) = if a.ndigits() > b.ndigits() {
        (b, a)
    } else {
        (a, b)
    };
    let (n1, n2) = (v1.ndigits(), v2.ndigits());
    if n1 == 0 {
        return Var::zero(rscale);
    }
    if n1 <= 6 && rscale == v1.dscale + v2.dscale {
        return mul_exact(v1, v2);
    }
    let negative = v1.negative != v2.negative;
    let mut p1 = (n1 + 1) / 2;
    let mut p2 = (n2 + 1) / 2;
    let res_ndigits = n1 + n2;
    let mut res_pairs = res_ndigits / 2 + 1;
    let pair_offset = res_pairs - p1 - p2 + 1;
    let res_weight = v1.weight + v2.weight + 1 + 2 * res_pairs - res_ndigits - (n1 & 1) - (n2 & 1);
    let maxdigits = res_weight + 1 + (rscale + DEC_DIGITS - 1) / DEC_DIGITS + MUL_GUARD_DIGITS;
    let maxpairs = maxdigits / 2 + 1;
    res_pairs = res_pairs.min(maxpairs);
    if res_pairs <= pair_offset {
        return Var::zero(rscale);
    }
    p1 = p1.min(res_pairs - pair_offset);
    p2 = p2.min(res_pairs - pair_offset);
    let pair = |v: &Var, n: i32, i: i32| -> u128 {
        if 2 * i + 1 < n {
            (v.digit(2 * i) * NBASE + v.digit(2 * i + 1)) as u128
        } else {
            (v.digit(2 * i) * NBASE) as u128
        }
    };
    let pairs2: Vec<u128> = (0..p2).map(|i| pair(v2, n2, i)).collect();
    let mut dig = vec![0u128; res_pairs as usize];
    for i1 in (0..p1).rev() {
        let d1 = pair(v1, n1, i1);
        if d1 == 0 {
            continue;
        }
        let limit = p2.min(res_pairs - i1 - pair_offset);
        for i2 in 0..limit {
            dig[(i1 + pair_offset + i2) as usize] += d1 * pairs2[i2 as usize];
        }
    }
    let mut out = vec![0i32; 2 * res_pairs as usize];
    let mut carry: u128 = 0;
    for i in (0..res_pairs as usize).rev() {
        let mut d = dig[i] + carry;
        carry = d / NBASE_SQR as u128;
        d -= carry * NBASE_SQR as u128;
        out[2 * i + 1] = (d % NBASE as u128) as i32;
        out[2 * i] = (d / NBASE as u128) as i32;
    }
    let mut r = Var {
        negative,
        weight: res_weight,
        dscale: rscale,
        digits: out,
    };
    r.round(rscale);
    r.strip();
    r
}

/// The exact product, its display scale the sum of the two (`mul_var_short`).
fn mul_exact(a: &Var, b: &Var) -> Var {
    let (n1, n2) = (a.digits.len(), b.digits.len());
    let mut acc = vec![0u64; n1 + n2];
    for (i1, &x) in a.digits.iter().enumerate() {
        for (i2, &y) in b.digits.iter().enumerate() {
            acc[i1 + i2 + 1] += x as u64 * y as u64;
        }
    }
    let mut carry = 0u64;
    let mut out = vec![0i32; n1 + n2];
    for i in (0..n1 + n2).rev() {
        let t = acc[i] + carry;
        out[i] = (t % NBASE as u64) as i32;
        carry = t / NBASE as u64;
    }
    let mut r = Var {
        negative: a.negative != b.negative,
        weight: a.weight + b.weight + 1,
        dscale: a.dscale + b.dscale,
        digits: out,
    };
    r.strip();
    r
}

/// `div_var`: the quotient at `rscale` places, rounded or cut toward zero; where not `exact`,
/// figured to guard digits and no further.
fn div(a: &Var, b: &Var, rscale: i32, round: bool, exact: bool) -> Result<Var> {
    let (n1, n2) = (a.ndigits(), b.ndigits());
    if n2 == 0 || b.digits[0] == 0 {
        return Err(division_by_zero());
    }
    if n2 <= 2 {
        let mut iv = b.digits[0];
        let mut iw = b.weight;
        if n2 == 2 {
            iv = iv * NBASE + b.digits[1];
            iw -= 1;
        }
        if b.negative {
            iv = -iv;
        }
        return div_int(a, iv as i64, iw, rscale, round);
    }
    if n2 <= 4 {
        let mut iv = b.digits[0] as i64;
        let mut iw = b.weight;
        for i in 1..n2 {
            iv = iv * NBASE as i64 + b.digit(i) as i64;
            iw -= 1;
        }
        if b.negative {
            iv = -iv;
        }
        return div_int(a, iv, iw, rscale, round);
    }
    if n1 == 0 {
        return Ok(Var::zero(rscale));
    }
    let exact = exact || n2 <= 2 * (DIV_GUARD_DIGITS + 2);
    let negative = a.negative != b.negative;
    let res_weight = a.weight - b.weight + 1;
    let mut res_ndigits = (res_weight + 1 + (rscale + DEC_DIGITS - 1) / DEC_DIGITS).max(1);
    if round {
        res_ndigits += 1;
    }
    if !exact {
        res_ndigits += DIV_GUARD_DIGITS;
    }
    let mut p1 = (n1 + 1) / 2;
    let mut p2 = (n2 + 1) / 2;
    let res_pairs = (res_ndigits + 1) / 2;
    let div_pairs;
    if exact {
        div_pairs = res_pairs + p2;
        p1 = p1.min(div_pairs);
    } else {
        div_pairs = res_pairs;
        p1 = p1.min(div_pairs);
        p2 = p2.min(div_pairs);
    }
    let pair = |v: &Var, n: i32, i: i32| -> i64 {
        if 2 * i + 1 < n {
            v.digit(2 * i) as i64 * NBASE as i64 + v.digit(2 * i + 1) as i64
        } else {
            v.digit(2 * i) as i64 * NBASE as i64
        }
    };
    let mut dividend = vec![0i64; div_pairs as usize + 1];
    for i in 0..p1 {
        dividend[i as usize] = pair(a, n1, i);
    }
    let divisor: Vec<i64> = (0..p2).map(|i| pair(b, n2, i)).collect();
    let mut fdivisor = divisor[0] as f64 * NBASE_SQR as f64;
    if p2 > 1 {
        fdivisor += divisor[1] as f64;
    }
    let fdivisorinverse = 1.0 / fdivisor;
    let limit = (i64::MAX - i64::MAX / NBASE_SQR - 1) / (NBASE_SQR - 1);
    let quotient_digit = |dividend: &[i64], qi: usize| -> i32 {
        let mut fdividend = dividend[qi] as f64 * NBASE_SQR as f64;
        fdividend += dividend[qi + 1] as f64;
        let fquotient = fdividend * fdivisorinverse;
        if fquotient >= 0.0 {
            fquotient as i32
        } else {
            (fquotient as i32) - 1
        }
    };
    let normalized = |newdig: i64| -> (i64, i64) {
        if newdig < 0 {
            let carry = -((-newdig - 1) / NBASE_SQR) - 1;
            (newdig - carry * NBASE_SQR, carry)
        } else if newdig >= NBASE_SQR {
            let carry = newdig / NBASE_SQR;
            (newdig - carry * NBASE_SQR, carry)
        } else {
            (newdig, 0)
        }
    };
    let mut maxdiv: i64 = 1;
    for qi in 0..res_pairs {
        let q = qi as usize;
        let mut qdigit = quotient_digit(&dividend, q);
        if qdigit != 0 {
            maxdiv += (qdigit as i64).abs();
            if maxdiv > limit {
                let mut carry = 0i64;
                let mut i = (qi + p2 - 2).min(div_pairs - 1);
                while i > qi {
                    let (d, c) = normalized(dividend[i as usize] + carry);
                    dividend[i as usize] = d;
                    carry = c;
                    i -= 1;
                }
                dividend[q] += carry;
                maxdiv = 1;
                qdigit = quotient_digit(&dividend, q);
                maxdiv += (qdigit as i64).abs();
            }
            if qdigit != 0 {
                let istop = p2.min(div_pairs - qi);
                for i in 0..istop as usize {
                    dividend[q + i] -= qdigit as i64 * divisor[i];
                }
            }
        }
        dividend[q + 1] = dividend[q + 1].wrapping_add(dividend[q].wrapping_mul(NBASE_SQR));
        dividend[q] = qdigit as i64;
    }
    if exact {
        let at = res_pairs as usize;
        let p2u = p2 as usize;
        let mut carry = 0i64;
        for i in (0..p2u - 1).rev() {
            let (d, c) = normalized(dividend[at + i] + carry);
            dividend[at + i + 1] = d;
            carry = c;
        }
        dividend[at] = carry;
        if dividend[at] < 0 {
            loop {
                let mut carry = 0i64;
                for i in (1..p2u).rev() {
                    let d = dividend[at + i] + divisor[i] + carry;
                    if d >= NBASE_SQR {
                        dividend[at + i] = d - NBASE_SQR;
                        carry = 1;
                    } else {
                        dividend[at + i] = d;
                        carry = 0;
                    }
                }
                dividend[at] += divisor[0] + carry;
                dividend[at - 1] -= 1;
                if dividend[at] >= 0 {
                    break;
                }
            }
        } else {
            loop {
                let mut less = false;
                for i in 0..p2u {
                    if dividend[at + i] < divisor[i] {
                        less = true;
                        break;
                    }
                    if dividend[at + i] > divisor[i] {
                        break;
                    }
                }
                if less {
                    break;
                }
                let mut carry = 0i64;
                for i in (1..p2u).rev() {
                    let d = dividend[at + i] - divisor[i] + carry;
                    if d < 0 {
                        dividend[at + i] = d + NBASE_SQR;
                        carry = -1;
                    } else {
                        dividend[at + i] = d;
                        carry = 0;
                    }
                }
                dividend[at] = dividend[at] - divisor[0] + carry;
                dividend[at - 1] += 1;
            }
        }
    }
    let mut out = vec![0i32; 2 * res_pairs as usize];
    let mut carry = 0i64;
    for i in (0..res_pairs as usize).rev() {
        let (d, c) = normalized(dividend[i] + carry);
        carry = c;
        out[2 * i + 1] = (d as u32 % NBASE as u32) as i32;
        out[2 * i] = (d as u32 / NBASE as u32) as i32;
    }
    let mut r = Var {
        negative,
        weight: res_weight,
        dscale: rscale,
        digits: out,
    };
    if round {
        r.round(rscale);
    } else {
        r.trunc(rscale);
    }
    r.strip();
    Ok(r)
}

/// `div_var_int` and `div_var_int64`: the quotient by `ival · 10000^ival_weight`, by short
/// division, at `rscale` places, rounded or cut toward zero.
fn div_int(v: &Var, ival: i64, ival_weight: i32, rscale: i32, round: bool) -> Result<Var> {
    if ival == 0 {
        return Err(division_by_zero());
    }
    if v.digits.is_empty() {
        return Ok(Var::zero(rscale));
    }
    let negative = if v.negative { ival > 0 } else { ival < 0 };
    let res_weight = v.weight - ival_weight;
    let mut res_ndigits = (res_weight + 1 + (rscale + DEC_DIGITS - 1) / DEC_DIGITS).max(1);
    if round {
        res_ndigits += 1;
    }
    let divisor = ival.unsigned_abs() as u128;
    let mut carry: u128 = 0;
    let mut out = Vec::with_capacity(res_ndigits as usize);
    for i in 0..res_ndigits {
        carry = carry * NBASE as u128 + v.digit(i) as u128;
        out.push((carry / divisor) as i32);
        carry %= divisor;
    }
    let mut r = Var {
        negative,
        weight: res_weight,
        dscale: rscale,
        digits: out,
    };
    if round {
        r.round(rscale);
    } else {
        r.trunc(rscale);
    }
    r.strip();
    Ok(r)
}

/// `mod_var`: a - trunc(a / b) · b.
fn modulo_var(a: &Var, b: &Var) -> Result<Var> {
    let q = div(a, b, 0, false, true)?;
    let p = mul(b, &q, b.dscale);
    Ok(sub(a, &p))
}

/// `gcd_var`.
fn gcd_var(a: &Var, b: &Var) -> Result<Var> {
    let res_dscale = a.dscale.max(b.dscale);
    let c = cmp_abs(a, b);
    let (a, b) = if c == Ordering::Less { (b, a) } else { (a, b) };
    if c == Ordering::Equal || b.digits.is_empty() {
        let mut r = a.clone();
        r.negative = false;
        r.dscale = res_dscale;
        return Ok(r);
    }
    let mut x = a.clone();
    let mut r = b.clone();
    loop {
        let m = modulo_var(&x, &r)?;
        if m.digits.is_empty() {
            break;
        }
        x = r;
        r = m;
    }
    r.negative = false;
    r.dscale = res_dscale;
    Ok(r)
}

/// The greatest natural number whose square is not past `n`.
fn isqrt(n: &Nat) -> Nat {
    if n.is_zero() {
        return Nat::zero();
    }
    let mut x = Nat::pow10(n.digit_count().div_ceil(2));
    loop {
        let (q, _) = n.divmod(&x);
        let (y, _) = x.add(&q).divmod_small(2);
        if y.cmp(&x) != Ordering::Less {
            return x;
        }
        x = y;
    }
}

/// `sqrt_var`: the square root rounded to `rscale` places, which may be before the point.
fn sqrt(arg: &Var, rscale: i32) -> Result<Var> {
    match cmp(arg, &Var::zero(0)) {
        Ordering::Equal => return Ok(Var::zero(rscale)),
        Ordering::Less => return Err(negative_root()),
        Ordering::Greater => {}
    }
    let res_weight = if arg.weight >= 0 {
        arg.weight / 2
    } else {
        -((-arg.weight - 1) / 2 + 1)
    };
    let res_ndigits = if rscale + 1 >= 0 {
        res_weight + 1 + (rscale + DEC_DIGITS) / DEC_DIGITS
    } else {
        res_weight + 1 - (-rscale - 1) / DEC_DIGITS
    }
    .max(1);
    let src_ndigits = (arg.weight + 1 + (res_ndigits - res_weight - 1) * 2).max(1);
    let mut dec = String::with_capacity(4 * src_ndigits as usize);
    for i in 0..src_ndigits {
        dec.push_str(&format!("{:04}", arg.digit(i)));
    }
    let s = isqrt(&Nat::from_digits(dec.as_bytes()));
    let mut r = Var::of_decimal(&s.digits());
    r.weight = res_weight;
    r.round(rscale);
    r.strip();
    Ok(r)
}

/// `exp_var`: e to the power of the argument, rounded to `rscale` places.
fn exp_var(arg: &Var, rscale: i32) -> Result<Var> {
    let mut x = arg.clone();
    let mut val = x.to_double();
    if val.abs() >= (MAX_RESULT_SCALE * 3) as f64 {
        if val > 0.0 {
            return Err(overflow());
        }
        return Ok(Var::zero(rscale));
    }
    let dweight = (val * 0.434294481903252) as i32;
    let mut ndiv2 = 0;
    if val.abs() > 0.01 {
        ndiv2 = 1;
        val /= 2.0;
        while val.abs() > 0.01 {
            ndiv2 += 1;
            val /= 2.0;
        }
        let local_rscale = x.dscale + ndiv2;
        x = div_int(&x, 1i64 << ndiv2, 0, local_rscale, true)?;
    }
    let sig_digits = (1 + dweight + rscale + (ndiv2 as f64 * 0.301029995663981) as i32).max(0) + 8;
    let local_rscale = sig_digits - 1;
    let mut result = add(&Var::one(), &x);
    let mut elem = mul(&x, &x, local_rscale);
    let mut ni = 2;
    elem = div_int(&elem, ni, 0, local_rscale, true)?;
    while !elem.digits.is_empty() {
        result = add(&result, &elem);
        elem = mul(&elem, &x, local_rscale);
        ni += 1;
        elem = div_int(&elem, ni, 0, local_rscale, true)?;
    }
    for _ in 0..ndiv2 {
        let local_rscale = (sig_digits - result.weight * 2 * DEC_DIGITS).max(MIN_DISPLAY_SCALE);
        result = mul(&result, &result, local_rscale);
    }
    result.round(rscale);
    Ok(result)
}

/// `estimate_ln_dweight`: the power of ten of the first digit of the natural logarithm, roughly.
fn estimate_ln_dweight(v: &Var) -> i32 {
    if v.negative {
        return 0;
    }
    if cmp(v, &Var::zero_point_nine()) != Ordering::Less
        && cmp(v, &Var::one_point_one()) != Ordering::Greater
    {
        let x = sub(v, &Var::one());
        if x.digits.is_empty() {
            0
        } else {
            x.weight * DEC_DIGITS + (x.digits[0] as f64).log10() as i32
        }
    } else if !v.digits.is_empty() {
        let mut digits = v.digits[0];
        let mut dweight = v.weight * DEC_DIGITS;
        if v.digits.len() > 1 {
            digits = digits * NBASE + v.digits[1];
            dweight -= DEC_DIGITS;
        }
        let ln = (digits as f64).ln() + dweight as f64 * 2.302585092994046;
        ln.abs().log10() as i32
    } else {
        0
    }
}

/// `ln_var`: the natural logarithm, rounded to `rscale` places.
fn ln_var(arg: &Var, rscale: i32) -> Result<Var> {
    match cmp(arg, &Var::zero(0)) {
        Ordering::Equal => return Err(log_of_zero()),
        Ordering::Less => return Err(log_of_negative()),
        Ordering::Greater => {}
    }
    let mut x = arg.clone();
    let mut fact = Var::two();
    let mut nsqrt = 0;
    while cmp(&x, &Var::zero_point_nine()) != Ordering::Greater {
        let local_rscale = rscale - x.weight * DEC_DIGITS / 2 + 8;
        x = sqrt(&x, local_rscale)?;
        fact = mul(&fact, &Var::two(), 0);
        nsqrt += 1;
    }
    while cmp(&x, &Var::one_point_one()) != Ordering::Less {
        let local_rscale = rscale - x.weight * DEC_DIGITS / 2 + 8;
        x = sqrt(&x, local_rscale)?;
        fact = mul(&fact, &Var::two(), 0);
        nsqrt += 1;
    }
    let local_rscale = rscale + ((nsqrt + 1) as f64 * 0.301029995663981) as i32 + 8;
    let mut result = sub(&x, &Var::one());
    let elem = add(&x, &Var::one());
    result = div(&result, &elem, local_rscale, true, false)?;
    let mut xx = result.clone();
    let x2 = mul(&result, &result, local_rscale);
    let mut ni = 1;
    loop {
        ni += 2;
        xx = mul(&xx, &x2, local_rscale);
        let elem = div_int(&xx, ni, 0, local_rscale, true)?;
        if elem.digits.is_empty() {
            break;
        }
        result = add(&result, &elem);
        if elem.weight < result.weight - local_rscale * 2 / DEC_DIGITS {
            break;
        }
    }
    Ok(mul(&result, &fact, rscale))
}

/// `log_var`: the logarithm of `num` in `base`, at the scale it chooses.
fn log_var(base: &Var, num: &Var) -> Result<Var> {
    let ln_base_dweight = estimate_ln_dweight(base);
    let ln_num_dweight = estimate_ln_dweight(num);
    let result_dweight = ln_num_dweight - ln_base_dweight;
    let rscale = (MIN_SIG_DIGITS - result_dweight)
        .max(base.dscale)
        .max(num.dscale)
        .max(MIN_DISPLAY_SCALE)
        .min(MAX_DISPLAY_SCALE);
    let ln_base_rscale = (rscale + result_dweight - ln_base_dweight + 8).max(MIN_DISPLAY_SCALE);
    let ln_num_rscale = (rscale + result_dweight - ln_num_dweight + 8).max(MIN_DISPLAY_SCALE);
    let ln_base = ln_var(base, ln_base_rscale)?;
    let ln_num = ln_var(num, ln_num_rscale)?;
    div(&ln_num, &ln_base, rscale, true, false)
}

/// `power_var`: `base` to the power of `exp`, at the scale it chooses.
fn power_var(base: &Var, exp: &Var) -> Result<Var> {
    if exp.digits.is_empty() || exp.ndigits() <= exp.weight + 1 {
        if let Some(v) = exp.to_int64() {
            if let Ok(v) = i32::try_from(v) {
                return power_var_int(base, v, exp.dscale);
            }
        }
    }
    if cmp(base, &Var::zero(0)) == Ordering::Equal {
        return Ok(Var::zero(MIN_SIG_DIGITS));
    }
    let mut abs_base = base.clone();
    let res_negative = if base.negative {
        if !exp.digits.is_empty() && exp.ndigits() > exp.weight + 1 {
            return Err(complex_power());
        }
        abs_base.negative = false;
        !exp.digits.is_empty()
            && exp.ndigits() == exp.weight + 1
            && exp.digits[exp.digits.len() - 1] & 1 == 1
    } else {
        false
    };
    let base = &abs_base;
    let ln_dweight = estimate_ln_dweight(base);
    let local_rscale = (8 - ln_dweight).max(MIN_DISPLAY_SCALE);
    let ln_base = ln_var(base, local_rscale)?;
    let ln_num = mul(&ln_base, exp, local_rscale);
    let mut val = ln_num.to_double();
    if val.abs() > MAX_RESULT_SCALE as f64 * 3.01 {
        if val > 0.0 {
            return Err(overflow());
        }
        return Ok(Var::zero(MAX_DISPLAY_SCALE));
    }
    val *= 0.434294481903252;
    let rscale = (MIN_SIG_DIGITS - val as i32)
        .max(base.dscale)
        .max(exp.dscale)
        .max(MIN_DISPLAY_SCALE)
        .min(MAX_DISPLAY_SCALE);
    let sig_digits = (rscale + val as i32).max(0);
    let local_rscale = (sig_digits - ln_dweight + 8).max(MIN_DISPLAY_SCALE);
    let ln_base = ln_var(base, local_rscale)?;
    let ln_num = mul(&ln_base, exp, local_rscale);
    let mut result = exp_var(&ln_num, rscale)?;
    if res_negative && !result.digits.is_empty() {
        result.negative = true;
    }
    Ok(result)
}

/// `power_var_int`: `base` to an integer power, at the scale it chooses.
fn power_var_int(base: &Var, exp: i32, exp_dscale: i32) -> Result<Var> {
    let f = if !base.digits.is_empty() {
        let mut f = base.digits[0] as f64;
        let mut p = base.weight * DEC_DIGITS;
        let mut i = 1;
        while i < base.ndigits() && i * DEC_DIGITS < 16 {
            f = f * NBASE as f64 + base.digit(i) as f64;
            p -= DEC_DIGITS;
            i += 1;
        }
        exp as f64 * (f.log10() + p as f64)
    } else {
        0.0
    };
    if f > ((WEIGHT_MAX + 1) * DEC_DIGITS) as f64 {
        return Err(overflow());
    }
    if f + 1.0 < -(MAX_DISPLAY_SCALE as f64) {
        return Ok(Var::zero(MAX_DISPLAY_SCALE));
    }
    let rscale = (MIN_SIG_DIGITS - f as i32)
        .max(base.dscale)
        .max(exp_dscale)
        .max(MIN_DISPLAY_SCALE)
        .min(MAX_DISPLAY_SCALE);
    match exp {
        0 => {
            let mut r = Var::one();
            r.dscale = rscale;
            return Ok(r);
        }
        1 => {
            let mut r = base.clone();
            r.round(rscale);
            return Ok(r);
        }
        -1 => return div(&Var::one(), base, rscale, true, true),
        2 => return Ok(mul(base, base, rscale)),
        _ => {}
    }
    if base.digits.is_empty() {
        if exp < 0 {
            return Err(division_by_zero());
        }
        return Ok(Var::zero(rscale));
    }
    let mut sig_digits = 1 + rscale + f as i32;
    sig_digits += (exp as f64).abs().ln() as i32 + 8;
    let mut neg = exp < 0;
    let mut mask = exp.unsigned_abs();
    let mut base_prod = base.clone();
    let mut result = if mask & 1 == 1 {
        base.clone()
    } else {
        Var::one()
    };
    loop {
        mask >>= 1;
        if mask == 0 {
            break;
        }
        let local_rscale = (sig_digits - 2 * base_prod.weight * DEC_DIGITS)
            .min(2 * base_prod.dscale)
            .max(MIN_DISPLAY_SCALE);
        base_prod = mul(&base_prod, &base_prod, local_rscale);
        if mask & 1 == 1 {
            let local_rscale = (sig_digits - (base_prod.weight + result.weight) * DEC_DIGITS)
                .min(base_prod.dscale + result.dscale)
                .max(MIN_DISPLAY_SCALE);
            result = mul(&base_prod, &result, local_rscale);
        }
        if base_prod.weight > WEIGHT_MAX || result.weight > WEIGHT_MAX {
            if !neg {
                return Err(overflow());
            }
            result = Var::zero(result.dscale);
            neg = false;
            break;
        }
    }
    if neg {
        div(&Var::one(), &result, rscale, true, false)
    } else {
        result.round(rscale);
        Ok(result)
    }
}

/// `get_min_scale`: the fewest places past the point that hold it whole.
fn min_scale_of(v: &Var) -> i32 {
    let mut last = v.ndigits() - 1;
    while last >= 0 && v.digits[last as usize] == 0 {
        last -= 1;
    }
    if last < 0 {
        return 0;
    }
    let mut ms = (last - v.weight) * DEC_DIGITS;
    if ms <= 0 {
        return 0;
    }
    let mut d = v.digits[last as usize];
    while d % 10 == 0 {
        ms -= 1;
        d /= 10;
    }
    ms
}

/// Whether a numeric holds no digit past its point, an infinity among them
/// (`numeric_is_integral`).
fn is_integral(n: &Numeric) -> bool {
    match &n.0 {
        Repr::NaN => false,
        Repr::Infinity { .. } => true,
        Repr::Finite { .. } => {
            let v = Var::of_numeric(n);
            v.digits.is_empty() || v.ndigits() <= v.weight + 1
        }
    }
}

impl Numeric {
    fn one() -> Numeric {
        Numeric::finite(false, Nat::from_u128(1), 0)
    }

    /// The least integer not below it (`numeric_ceil`).
    pub(crate) fn ceil(&self) -> Result<Numeric> {
        if !self.is_finite() {
            return Ok(self.clone());
        }
        let v = Var::of_numeric(self);
        let mut t = v.clone();
        t.trunc(0);
        if !v.negative && cmp(&v, &t) != Ordering::Equal {
            t = add(&t, &Var::one());
        }
        t.numeric()
    }

    /// The greatest integer not above it (`numeric_floor`).
    pub(crate) fn floor(&self) -> Result<Numeric> {
        if !self.is_finite() {
            return Ok(self.clone());
        }
        let v = Var::of_numeric(self);
        let mut t = v.clone();
        t.trunc(0);
        if v.negative && cmp(&v, &t) != Ordering::Equal {
            t = sub(&t, &Var::one());
        }
        t.numeric()
    }

    /// -1, 0 or 1 by its sign; NaN's NaN (`numeric_sign`).
    pub(crate) fn sign_of(&self) -> Numeric {
        match self.sign() {
            _ if self.is_nan() => Numeric::NAN,
            0 => Numeric::zero(),
            s => Numeric::finite(s < 0, Nat::from_u128(1), 0),
        }
    }

    /// Rounded half away from zero at `scale` places, which may be before the point
    /// (`numeric_round`).
    pub(crate) fn round_to(&self, scale: i32) -> Result<Numeric> {
        if !self.is_finite() {
            return Ok(self.clone());
        }
        let scale = scale
            .max(-(WEIGHT_MAX + 1) * DEC_DIGITS - 1)
            .min(DSCALE_MAX);
        let mut v = Var::of_numeric(self);
        v.round(scale);
        if scale < 0 {
            v.dscale = 0;
        }
        v.numeric()
    }

    /// Cut toward zero at `scale` places, which may be before the point (`numeric_trunc`).
    pub(crate) fn trunc(&self, scale: i32) -> Numeric {
        if !self.is_finite() {
            return self.clone();
        }
        let scale = scale.max(-(WEIGHT_MAX + 1) * DEC_DIGITS).min(DSCALE_MAX);
        let mut v = Var::of_numeric(self);
        v.trunc(scale);
        if scale < 0 {
            v.dscale = 0;
        }
        v.numeric().expect("a numeric cut short is a numeric")
    }

    /// The integer quotient, cut toward zero (`numeric_div_trunc`, `div`).
    pub(crate) fn div_trunc(&self, o: &Numeric) -> Result<Numeric> {
        match (&self.0, &o.0) {
            (Repr::NaN, _) | (_, Repr::NaN) => Ok(Numeric::NAN),
            (Repr::Infinity { negative }, _) => {
                if !o.is_finite() {
                    return Ok(Numeric::NAN);
                }
                match o.sign() {
                    0 => Err(division_by_zero()),
                    s => Ok(Numeric::infinity(*negative != (s < 0))),
                }
            }
            (_, Repr::Infinity { .. }) => Ok(Numeric::zero()),
            _ => div(&Var::of_numeric(self), &Var::of_numeric(o), 0, false, true)?.numeric(),
        }
    }

    /// The greatest common divisor, its scale the larger; NaN where either is not finite
    /// (`numeric_gcd`).
    pub(crate) fn gcd(&self, o: &Numeric) -> Result<Numeric> {
        if !self.is_finite() || !o.is_finite() {
            return Ok(Numeric::NAN);
        }
        gcd_var(&Var::of_numeric(self), &Var::of_numeric(o))?.numeric()
    }

    /// The least common multiple, its scale the larger; NaN where either is not finite
    /// (`numeric_lcm`).
    pub(crate) fn lcm(&self, o: &Numeric) -> Result<Numeric> {
        if !self.is_finite() || !o.is_finite() {
            return Ok(Numeric::NAN);
        }
        let (a, b) = (Var::of_numeric(self), Var::of_numeric(o));
        let mut r = if a.digits.is_empty() || b.digits.is_empty() {
            Var::zero(0)
        } else {
            let g = gcd_var(&a, &b)?;
            let q = div(&a, &g, 0, false, true)?;
            let mut r = mul(&b, &q, b.dscale);
            r.negative = false;
            r
        };
        r.dscale = a.dscale.max(b.dscale);
        r.numeric()
    }

    /// `n!` (`numeric_fac`).
    pub(crate) fn factorial(n: i64) -> Result<Numeric> {
        if n < 0 {
            return Err(Error::new("factorial of a negative number is undefined"));
        }
        if n <= 1 {
            return Ok(Numeric::one());
        }
        if n > 32177 {
            return Err(overflow());
        }
        let mut p = Nat::from_u128(n as u128);
        for k in (2..n).rev() {
            p = p.mul_small(k as u32);
        }
        Numeric::finite(false, p, 0).checked()
    }

    /// The square root, to at least 16 significant digits and no fewer places than its own
    /// (`numeric_sqrt`).
    pub(crate) fn sqrt(&self) -> Result<Numeric> {
        match &self.0 {
            Repr::Infinity { negative: true } => return Err(negative_root()),
            Repr::NaN | Repr::Infinity { .. } => return Ok(self.clone()),
            Repr::Finite { .. } => {}
        }
        let arg = Var::of_numeric(self);
        let sweight = arg.weight * DEC_DIGITS / 2 + 1;
        let rscale = (MIN_SIG_DIGITS - sweight)
            .max(arg.dscale)
            .max(MIN_DISPLAY_SCALE)
            .min(MAX_DISPLAY_SCALE);
        sqrt(&arg, rscale)?.numeric()
    }

    /// e to its power (`numeric_exp`).
    pub(crate) fn exp(&self) -> Result<Numeric> {
        match &self.0 {
            Repr::Infinity { negative: true } => return Ok(Numeric::zero()),
            Repr::NaN | Repr::Infinity { .. } => return Ok(self.clone()),
            Repr::Finite { .. } => {}
        }
        let arg = Var::of_numeric(self);
        let val = (arg.to_double() * 0.434294481903252)
            .max(-(MAX_RESULT_SCALE as f64))
            .min(MAX_RESULT_SCALE as f64);
        let rscale = (MIN_SIG_DIGITS - val as i32)
            .max(arg.dscale)
            .max(MIN_DISPLAY_SCALE)
            .min(MAX_DISPLAY_SCALE);
        exp_var(&arg, rscale)?.numeric()
    }

    /// The natural logarithm (`numeric_ln`).
    pub(crate) fn ln(&self) -> Result<Numeric> {
        match &self.0 {
            Repr::Infinity { negative: true } => return Err(log_of_negative()),
            Repr::NaN | Repr::Infinity { .. } => return Ok(self.clone()),
            Repr::Finite { .. } => {}
        }
        let arg = Var::of_numeric(self);
        let rscale = (MIN_SIG_DIGITS - estimate_ln_dweight(&arg))
            .max(arg.dscale)
            .max(MIN_DISPLAY_SCALE)
            .min(MAX_DISPLAY_SCALE);
        ln_var(&arg, rscale)?.numeric()
    }

    /// The logarithm of `x` in `base` (`numeric_log`, `log(b, x)`).
    pub(crate) fn log(base: &Numeric, x: &Numeric) -> Result<Numeric> {
        if !base.is_finite() || !x.is_finite() {
            if base.is_nan() || x.is_nan() {
                return Ok(Numeric::NAN);
            }
            let (s1, s2) = (base.sign(), x.sign());
            if s1 < 0 || s2 < 0 {
                return Err(log_of_negative());
            }
            if s1 == 0 || s2 == 0 {
                return Err(log_of_zero());
            }
            if base.is_infinite() {
                return Ok(if x.is_infinite() {
                    Numeric::NAN
                } else {
                    Numeric::zero()
                });
            }
            return Ok(Numeric::infinity(false));
        }
        log_var(&Var::of_numeric(base), &Var::of_numeric(x))?.numeric()
    }

    /// The logarithm in base 10 (`log(numeric)`, `log10(numeric)`: `log(10, x)`).
    pub(crate) fn log10(&self) -> Result<Numeric> {
        Numeric::log(&Numeric::of_int(10), self)
    }

    /// It to the power of `exponent` (`numeric_power`).
    pub(crate) fn power(&self, exponent: &Numeric) -> Result<Numeric> {
        let (x, y) = (self, exponent);
        if !x.is_finite() || !y.is_finite() {
            if x.is_nan() {
                if y.is_finite() && y.sign() == 0 {
                    return Ok(Numeric::one());
                }
                return Ok(Numeric::NAN);
            }
            if y.is_nan() {
                if x.is_finite() && cmp(&Var::of_numeric(x), &Var::one()) == Ordering::Equal {
                    return Ok(Numeric::one());
                }
                return Ok(Numeric::NAN);
            }
            let (s1, s2) = (x.sign(), y.sign());
            if s1 == 0 && s2 < 0 {
                return Err(zero_to_negative());
            }
            if s1 < 0 && !is_integral(y) {
                return Err(complex_power());
            }
            if x.is_finite() && cmp(&Var::of_numeric(x), &Var::one()) == Ordering::Equal {
                return Ok(Numeric::one());
            }
            if s2 == 0 {
                return Ok(Numeric::one());
            }
            if s1 == 0 && s2 > 0 {
                return Ok(Numeric::zero());
            }
            if y.is_infinite() {
                let abs_x_gt_one = if !x.is_finite() {
                    true
                } else {
                    let mut a = Var::of_numeric(x);
                    if a.negative && cmp_abs(&a, &Var::one()) == Ordering::Equal {
                        return Ok(Numeric::one());
                    }
                    a.negative = false;
                    cmp(&a, &Var::one()) == Ordering::Greater
                };
                return Ok(if abs_x_gt_one == (s2 > 0) {
                    Numeric::infinity(false)
                } else {
                    Numeric::zero()
                });
            }
            if let Repr::Infinity { negative: false } = x.0 {
                return Ok(if s2 > 0 {
                    Numeric::infinity(false)
                } else {
                    Numeric::zero()
                });
            }
            if s2 < 0 {
                return Ok(Numeric::zero());
            }
            let e = Var::of_numeric(y);
            let odd = !e.digits.is_empty()
                && e.ndigits() == e.weight + 1
                && e.digits[e.digits.len() - 1] & 1 == 1;
            return Ok(Numeric::infinity(odd));
        }
        if x.sign() == 0 && y.sign() < 0 {
            return Err(zero_to_negative());
        }
        power_var(&Var::of_numeric(x), &Var::of_numeric(y))?.numeric()
    }

    /// The digits it prints past its point; none for NaN and the infinities (`numeric_scale`).
    pub(crate) fn scale_of(&self) -> Option<i32> {
        self.dscale().map(|d| d as i32)
    }

    /// The fewest places past the point that hold it whole; none for NaN and the infinities
    /// (`numeric_min_scale`).
    pub(crate) fn min_scale(&self) -> Option<i32> {
        if !self.is_finite() {
            return None;
        }
        Some(min_scale_of(&Var::of_numeric(self)))
    }

    /// It at the fewest places that hold it whole (`numeric_trim_scale`).
    pub(crate) fn trim_scale(&self) -> Numeric {
        if !self.is_finite() {
            return self.clone();
        }
        let mut v = Var::of_numeric(self);
        v.dscale = min_scale_of(&v);
        v.numeric().expect("a numeric at fewer places is a numeric")
    }

    /// The bucket `operand` falls in, of `count` equal buckets from `low` to `high`, 0 before them
    /// and `count + 1` past them (`width_bucket_numeric`).
    pub(crate) fn width_bucket(
        operand: &Numeric,
        low: &Numeric,
        high: &Numeric,
        count: i32,
    ) -> Result<i32> {
        if count <= 0 {
            return Err(Error::new("count must be greater than zero"));
        }
        if low.is_nan() || high.is_nan() {
            return Err(Error::new("lower and upper bounds cannot be NaN"));
        }
        if low.is_infinite() || high.is_infinite() {
            return Err(Error::new("lower and upper bounds must be finite"));
        }
        let count_var = Var::of_int(count as i64);
        let past = || add(&count_var, &Var::one());
        let r = match low.cmp(high) {
            Ordering::Equal => return Err(Error::new("lower bound cannot equal upper bound")),
            Ordering::Less => {
                if operand < low {
                    Var::zero(0)
                } else if operand >= high {
                    past()
                } else {
                    bucket(operand, low, high, &count_var)?
                }
            }
            Ordering::Greater => {
                if operand > low {
                    Var::zero(0)
                } else if operand <= high {
                    past()
                } else {
                    bucket(operand, low, high, &count_var)?
                }
            }
        };
        r.to_int64()
            .and_then(|v| i32::try_from(v).ok())
            .ok_or_else(|| Error::new("integer out of range"))
    }
}

/// `compute_bucket`: ((operand - low) · count) / (high - low), cut toward zero, plus one.
fn bucket(operand: &Numeric, low: &Numeric, high: &Numeric, count: &Var) -> Result<Var> {
    let b1 = Var::of_numeric(low);
    let b2 = Var::of_numeric(high);
    let op = sub(&Var::of_numeric(operand), &b1);
    let span = sub(&b2, &b1);
    let op = mul(&op, count, op.dscale + count.dscale);
    let q = div(&op, &span, 0, false, true)?;
    Ok(add(&q, &Var::one()))
}

/// `select_div_scale`: the scale a quotient of the two is computed at.
fn select_div_scale(a: &Var, b: &Var) -> i32 {
    let lead = |v: &Var| {
        (0..v.ndigits())
            .map(|i| (i, v.digit(i)))
            .find(|&(_, d)| d != 0)
            .map_or((0, 0), |(i, d)| (v.weight - i, d))
    };
    let ((w1, d1), (w2, d2)) = (lead(a), lead(b));
    let mut qweight = w1 - w2;
    if d1 <= d2 {
        qweight -= 1;
    }
    (MIN_SIG_DIGITS - qweight * DEC_DIGITS)
        .max(a.dscale)
        .max(b.dscale)
        .clamp(MIN_DISPLAY_SCALE, MAX_DISPLAY_SCALE)
}

impl Numeric {
    /// The variance, or the standard deviation, of `n` numbers whose sum and sum of squares are
    /// given exactly, of the population or of a sample, as `numeric_stddev_internal` computes it.
    pub(crate) fn spread(
        n: i64,
        sum: &Numeric,
        squares: &Numeric,
        variance: bool,
        sample: bool,
    ) -> Result<Numeric> {
        let vn = Var::of_int(n);
        let sum_x = Var::of_numeric(sum);
        let rscale = sum_x.dscale * 2;
        let sum_x = mul(&sum_x, &sum_x, rscale);
        let sum_x2 = mul(&vn, &Var::of_numeric(squares), rscale);
        let numerator = sub(&sum_x2, &sum_x);
        if cmp(&numerator, &Var::zero(0)) != Ordering::Greater {
            return Ok(Numeric::zero());
        }
        let denominator = if sample {
            mul(&vn, &sub(&vn, &Var::one()), 0)
        } else {
            mul(&vn, &vn, 0)
        };
        let rscale = select_div_scale(&numerator, &denominator);
        let q = div(&numerator, &denominator, rscale, true, true)?;
        if variance {
            q.numeric()
        } else {
            sqrt(&q, rscale)?.numeric()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(s: &str) -> Numeric {
        Numeric::parse(s).unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    #[test]
    fn a_numeric_goes_into_base_ten_thousand_digits_and_back_whole() {
        for s in [
            "0",
            "0.000",
            "1",
            "-1",
            "12345.6789",
            "0.00001",
            "-99999999.99999999",
            "1e100",
            "1.5e-30",
            "10000",
            "123456789012345678901234567890.1234567",
        ] {
            let v = Var::of_numeric(&n(s));
            assert_eq!(
                v.clone().numeric().unwrap().to_string(),
                n(s).to_string(),
                "{s}"
            );
            assert_eq!(v.text(), n(s).to_string(), "{s}");
        }
    }

    #[test]
    fn the_roots_logarithms_and_powers_give_postgresql_s_digits() {
        // Each answer as PostgreSQL 19 RC1 gives it.
        assert_eq!(n("2").sqrt().unwrap().to_string(), "1.414213562373095");
        assert_eq!(n("1").exp().unwrap().to_string(), "2.7182818284590452");
        assert_eq!(n("10").ln().unwrap().to_string(), "2.3025850929940457");
        assert_eq!(
            n("2").power(&n("0.5")).unwrap().to_string(),
            "1.4142135623730950"
        );
        assert_eq!(n("100").log10().unwrap().to_string(), "2.0000000000000000");
        assert_eq!(
            Numeric::factorial(20).unwrap().to_string(),
            "2432902008176640000"
        );
    }

    #[test]
    fn the_rounding_functions_hold_their_scales() {
        assert_eq!(n("-1.5").ceil().unwrap().to_string(), "-1");
        assert_eq!(n("-1.5").floor().unwrap().to_string(), "-2");
        assert_eq!(n("-0.5").ceil().unwrap().to_string(), "0");
        assert_eq!(n("1234.5678").round_to(-2).unwrap().to_string(), "1200");
        assert_eq!(n("1234.5678").trunc(2).to_string(), "1234.56");
        assert_eq!(n("1.5").round_to(20000).unwrap().scale_of(), Some(16383));
        assert_eq!(n("1.2300").trim_scale().to_string(), "1.23");
        assert_eq!(n("1.2300").min_scale(), Some(2));
        assert_eq!(n("-7.5").sign_of().to_string(), "-1");
    }

    #[test]
    fn the_errors_are_postgresql_s() {
        let e = |r: Result<Numeric>| r.unwrap_err().to_string();
        assert_eq!(
            e(n("-1").sqrt()),
            "cannot take square root of a negative number"
        );
        assert_eq!(e(n("0").ln()), "cannot take logarithm of zero");
        assert_eq!(
            e(n("0").power(&n("-1"))),
            "zero raised to a negative power is undefined"
        );
        assert_eq!(
            e(n("-2").power(&n("0.5"))),
            "a negative number raised to a non-integer power yields a complex result"
        );
        assert_eq!(
            e(n("10").power(&n("1000000"))),
            "value overflows numeric format"
        );
        assert_eq!(e(n("1").div_trunc(&n("0"))), "division by zero");
    }

    #[test]
    fn an_exact_quotient_is_the_quotient_cut_at_its_places() {
        // Divisors of five digits and more go through the long division; each quotient cut at 0
        // places is the one the decimal digits give.
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for _ in 0..2000 {
            let a = (next() as u128) * (next() as u128) + 1;
            let b = (next() as u128 % 1_000_000_000_000_000_000) + 100_000_000_000_000_000;
            let q = div(
                &Var::of_decimal(&a.to_string()),
                &Var::of_decimal(&b.to_string()),
                0,
                false,
                true,
            )
            .unwrap();
            assert_eq!(q.text(), (a / b).to_string(), "{a} / {b}");
        }
    }
}
