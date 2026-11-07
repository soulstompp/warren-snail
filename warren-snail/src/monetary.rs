//! Money's locales: the `lc_monetary` a file's money was written under, and the conventions
//! PostgreSQL 19 reads from it through `PGLC_localeconv` (`pg_locale.c`, `pg_localeconv_r.c`), by
//! which `cash_in` and `cash_out` read and write money (`cash.c`) and to_char writes its currency
//! symbol and its locale's signs (`formatting.c`). lc_numeric is C, as the snail answers under.
//!
//! C and POSIX are read by every build. Any other locale is read through glibc's `newlocale` and
//! `nl_langinfo_l`, as PostgreSQL reads it on glibc, only with the feature `collations`; without
//! it such a locale is refused by name.

use crate::{Error, Result};
use std::sync::Mutex;

/// The conventions a locale gives money and numbers, as PostgreSQL's `struct lconv` holds them:
/// the decimal point and the thousands separator from lc_numeric, the rest from lc_monetary. Each
/// one-character field holds the byte the locale gives, glibc's `\377` where it gives none.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Conv {
    pub decimal_point: String,
    pub thousands_sep: String,
    pub currency_symbol: String,
    pub mon_decimal_point: String,
    pub mon_thousands_sep: String,
    pub mon_grouping: Vec<u8>,
    pub positive_sign: String,
    pub negative_sign: String,
    pub frac_digits: i8,
    pub p_cs_precedes: i8,
    pub p_sep_by_space: i8,
    pub n_cs_precedes: i8,
    pub n_sep_by_space: i8,
    pub p_sign_posn: i8,
    pub n_sign_posn: i8,
}

/// The C locale's conventions, as glibc gives them.
fn c_conv() -> Conv {
    Conv {
        decimal_point: ".".into(),
        thousands_sep: String::new(),
        currency_symbol: String::new(),
        mon_decimal_point: String::new(),
        mon_thousands_sep: String::new(),
        mon_grouping: vec![0xff],
        positive_sign: String::new(),
        negative_sign: String::new(),
        frac_digits: -1,
        p_cs_precedes: -1,
        p_sep_by_space: -1,
        n_cs_precedes: -1,
        n_sep_by_space: -1,
        p_sign_posn: -1,
        n_sign_posn: -1,
    }
}

impl Conv {
    /// A locale's conventions, given field by field over C's: for the tests that read money under
    /// a locale this machine may not have.
    #[cfg(test)]
    pub(crate) fn of(fields: &[(&str, &str)]) -> Conv {
        let mut c = c_conv();
        for (name, v) in fields {
            let ch = || v.parse::<i8>().unwrap_or(-1);
            match *name {
                "decimal_point" => c.decimal_point = v.to_string(),
                "thousands_sep" => c.thousands_sep = v.to_string(),
                "currency_symbol" => c.currency_symbol = v.to_string(),
                "mon_decimal_point" => c.mon_decimal_point = v.to_string(),
                "mon_thousands_sep" => c.mon_thousands_sep = v.to_string(),
                "mon_grouping" => c.mon_grouping = vec![ch() as u8],
                "positive_sign" => c.positive_sign = v.to_string(),
                "negative_sign" => c.negative_sign = v.to_string(),
                "frac_digits" => c.frac_digits = ch(),
                "p_cs_precedes" => c.p_cs_precedes = ch(),
                "p_sep_by_space" => c.p_sep_by_space = ch(),
                "n_cs_precedes" => c.n_cs_precedes = ch(),
                "n_sep_by_space" => c.n_sep_by_space = ch(),
                "p_sign_posn" => c.p_sign_posn = ch(),
                "n_sign_posn" => c.n_sign_posn = ch(),
                other => panic!("{other} is no field of a struct lconv"),
            }
        }
        c
    }

    /// The fraction digits money holds: the locale's, or 2 where it gives none from 0 to 10.
    pub fn points(&self) -> u32 {
        match self.frac_digits {
            d @ 0..=10 => d as u32,
            _ => 2,
        }
    }

    /// Ten to the fraction digits: what one unit of money counts.
    pub fn scale(&self) -> i64 {
        10i64.pow(self.points())
    }

    /// The decimal point money is read and written with: the locale's where it is one byte, else
    /// a point.
    fn dsymbol(&self) -> u8 {
        match self.mon_decimal_point.as_bytes() {
            [d] => *d,
            _ => b'.',
        }
    }

    /// The separator between groups of digits: the locale's, else whichever of a comma and a
    /// point is not the decimal point.
    fn ssymbol(&self) -> &[u8] {
        if self.mon_thousands_sep.is_empty() {
            if self.dsymbol() != b',' {
                b","
            } else {
                b"."
            }
        } else {
            self.mon_thousands_sep.as_bytes()
        }
    }

    fn csymbol(&self) -> &[u8] {
        if self.currency_symbol.is_empty() {
            b"$"
        } else {
            self.currency_symbol.as_bytes()
        }
    }
}

/// An `lc_monetary`, by the number `open` gives it, the same on every thread: C's is 0.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Monetary(u16);

impl std::fmt::Debug for Monetary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name())
    }
}

/// Each locale opened, its name as given and its conventions, C's first.
static OPENED: Mutex<Vec<(String, &'static Conv)>> = Mutex::new(Vec::new());

fn opened<T>(f: impl FnOnce(&mut Vec<(String, &'static Conv)>) -> T) -> T {
    let mut all = OPENED.lock().unwrap();
    if all.is_empty() {
        all.push(("C".into(), Box::leak(Box::new(c_conv()))));
    }
    f(&mut all)
}

impl Monetary {
    pub const C: Monetary = Monetary(0);

    /// The locale named, as PostgreSQL takes it for lc_monetary: C and POSIX by every build, any
    /// other through glibc with the feature collations.
    pub fn open(locale: &str) -> Result<Monetary> {
        if matches!(locale, "C" | "POSIX") {
            return Ok(Monetary::C);
        }
        if let Some(at) = opened(|all| all.iter().position(|(n, _)| n == locale)) {
            return Ok(Monetary(at as u16));
        }
        let conv: &'static Conv = Box::leak(Box::new(library::conv(locale)?));
        Ok(opened(|all| {
            match all.iter().position(|(n, _)| n == locale) {
                Some(at) => Monetary(at as u16),
                None => {
                    all.push((locale.to_string(), conv));
                    Monetary(all.len() as u16 - 1)
                }
            }
        }))
    }

    /// The locale's name as it was opened; C for C and POSIX.
    pub fn name(self) -> String {
        opened(|all| all[self.0 as usize].0.clone())
    }

    pub fn conv(self) -> &'static Conv {
        opened(|all| all[self.0 as usize].1)
    }

    pub fn is_c(self) -> bool {
        self == Monetary::C
    }

    /// The same locale, where two names may name it: C and POSIX one.
    pub fn names_same(a: &str, b: &str) -> bool {
        let c = |s: &str| matches!(s, "C" | "POSIX");
        a == b || (c(a) && c(b))
    }
}

#[cfg(not(feature = "collations"))]
mod library {
    use super::*;

    pub fn conv(locale: &str) -> Result<Conv> {
        Err(Error::new(format!(
            "money under lc_monetary {locale} is read through glibc, and this warren-snail is \
             built without the feature collations: it reads money under C and POSIX only"
        )))
    }
}

#[cfg(feature = "collations")]
mod library {
    use super::*;
    use std::ffi::CString;

    /// glibc's items of LC_MONETARY, `_NL_ITEM(LC_MONETARY, n)`, in `langinfo.h`'s order.
    const MONETARY: libc::nl_item = 4 << 16;
    const CURRENCY_SYMBOL: libc::nl_item = MONETARY | 1;
    const MON_DECIMAL_POINT: libc::nl_item = MONETARY | 2;
    const MON_THOUSANDS_SEP: libc::nl_item = MONETARY | 3;
    const MON_GROUPING: libc::nl_item = MONETARY | 4;
    const POSITIVE_SIGN: libc::nl_item = MONETARY | 5;
    const NEGATIVE_SIGN: libc::nl_item = MONETARY | 6;
    const FRAC_DIGITS: libc::nl_item = MONETARY | 8;
    const P_CS_PRECEDES: libc::nl_item = MONETARY | 9;
    const P_SEP_BY_SPACE: libc::nl_item = MONETARY | 10;
    const N_CS_PRECEDES: libc::nl_item = MONETARY | 11;
    const N_SEP_BY_SPACE: libc::nl_item = MONETARY | 12;
    const P_SIGN_POSN: libc::nl_item = MONETARY | 13;
    const N_SIGN_POSN: libc::nl_item = MONETARY | 14;

    /// The conventions of `locale` as `pg_localeconv_r` reads them on glibc: a locale of every
    /// category, each field from `nl_langinfo_l`; lc_numeric's from C. The strings are the locale's
    /// codeset's, UTF-8 or ASCII, and so the database's own.
    pub fn conv(locale: &str) -> Result<Conv> {
        let invalid = || {
            Error::new(format!(
                "invalid value for parameter \"lc_monetary\": \"{locale}\""
            ))
        };
        let name = CString::new(locale).map_err(|_| invalid())?;
        let items = [
            CURRENCY_SYMBOL,
            MON_DECIMAL_POINT,
            MON_THOUSANDS_SEP,
            MON_GROUPING,
            POSITIVE_SIGN,
            NEGATIVE_SIGN,
            FRAC_DIGITS,
            P_CS_PRECEDES,
            P_SEP_BY_SPACE,
            N_CS_PRECEDES,
            N_SEP_BY_SPACE,
            P_SIGN_POSN,
            N_SIGN_POSN,
            libc::CODESET,
        ];
        let got = crate::sys::monetary::langinfo(&name, &items).ok_or_else(invalid)?;
        let item = |i: libc::nl_item| got[items.iter().position(|&x| x == i).unwrap()].as_slice();
        let text = |i| String::from_utf8_lossy(item(i)).into_owned();
        let byte = |i| item(i).first().map_or(0, |&b| b as i8);
        let codeset = item(libc::CODESET).to_vec();
        let conv = Conv {
            currency_symbol: text(CURRENCY_SYMBOL),
            mon_decimal_point: text(MON_DECIMAL_POINT),
            mon_thousands_sep: text(MON_THOUSANDS_SEP),
            mon_grouping: item(MON_GROUPING).to_vec(),
            positive_sign: text(POSITIVE_SIGN),
            negative_sign: text(NEGATIVE_SIGN),
            frac_digits: byte(FRAC_DIGITS),
            p_cs_precedes: byte(P_CS_PRECEDES),
            p_sep_by_space: byte(P_SEP_BY_SPACE),
            n_cs_precedes: byte(N_CS_PRECEDES),
            n_sep_by_space: byte(N_SEP_BY_SPACE),
            p_sign_posn: byte(P_SIGN_POSN),
            n_sign_posn: byte(N_SIGN_POSN),
            ..c_conv()
        };
        if codeset != b"UTF-8" && codeset != b"ANSI_X3.4-1968" {
            return Err(Error::new(format!(
                "lc_monetary {locale} writes money in {}: the snail reads a locale of UTF-8 or \
                 ASCII",
                String::from_utf8_lossy(&codeset)
            )));
        }
        Ok(conv)
    }
}

/// C's `isspace` under the C locale.
fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r')
}

/// money as PostgreSQL's `cash_in` reads it under `conv`: white space, the currency symbol, a sign
/// or an opening parenthesis, white space and the symbol again; digits with separators among them,
/// the decimal point and the locale's fraction digits, the next digit rounding the last half up;
/// then white space, closing parentheses, signs and the symbol. As its count of the smallest unit.
pub fn cash_in(s: &str, conv: &Conv) -> Result<i64> {
    let wrong = || Error::new(format!("invalid input syntax for type money: \"{s}\""));
    let range = || Error::new(format!("value \"{s}\" is out of range for type money"));
    let fpoint = conv.points();
    let dsymbol = conv.dsymbol();
    let ssymbol = conv.ssymbol();
    let csymbol = conv.csymbol();
    let psymbol: &[u8] = if conv.positive_sign.is_empty() {
        b"+"
    } else {
        conv.positive_sign.as_bytes()
    };
    let nsymbol: &[u8] = if conv.negative_sign.is_empty() {
        b"-"
    } else {
        conv.negative_sign.as_bytes()
    };
    let b = s.as_bytes();
    let at_is = |at: usize, sym: &[u8]| b[at.min(b.len())..].starts_with(sym);
    let mut at = 0;
    let lead = |at: &mut usize| {
        while *at < b.len() && is_space(b[*at]) {
            *at += 1;
        }
        if at_is(*at, csymbol) {
            *at += csymbol.len();
        }
        while *at < b.len() && is_space(b[*at]) {
            *at += 1;
        }
    };
    lead(&mut at);
    let mut negative = false;
    if at_is(at, nsymbol) {
        negative = true;
        at += nsymbol.len();
    } else if b.get(at) == Some(&b'(') {
        negative = true;
        at += 1;
    } else if at_is(at, psymbol) {
        at += psymbol.len();
    }
    lead(&mut at);
    // Built in the negative, as PostgreSQL builds it, so the most negative value is reached.
    let (mut value, mut dec, mut dot) = (0i64, 0, false);
    while at < b.len() {
        let c = b[at];
        if c.is_ascii_digit() && (!dot || dec < fpoint) {
            value = value
                .checked_mul(10)
                .and_then(|v| v.checked_sub(i64::from(c - b'0')))
                .ok_or_else(range)?;
            if dot {
                dec += 1;
            }
        } else if c == dsymbol && !dot {
            dot = true;
        } else if at_is(at, ssymbol) {
            at += ssymbol.len() - 1;
        } else {
            break;
        }
        at += 1;
    }
    if b.get(at).is_some_and(|&c| (b'5'..=b'9').contains(&c)) {
        value = value.checked_sub(1).ok_or_else(range)?;
    }
    for _ in dec..fpoint {
        value = value.checked_mul(10).ok_or_else(range)?;
    }
    while b.get(at).is_some_and(u8::is_ascii_digit) {
        at += 1;
    }
    while at < b.len() {
        if is_space(b[at]) || b[at] == b')' {
            at += 1;
        } else if at_is(at, nsymbol) {
            negative = true;
            at += nsymbol.len();
        } else if at_is(at, psymbol) {
            at += psymbol.len();
        } else if at_is(at, csymbol) {
            at += csymbol.len();
        } else {
            return Err(wrong());
        }
    }
    if negative {
        Ok(value)
    } else {
        value.checked_neg().ok_or_else(range)
    }
}

/// money as PostgreSQL's `cash_out` writes it under `conv`: its digits grouped by the locale's
/// first group, the decimal point and the fraction digits, then the currency symbol and the sign
/// placed as the locale's sign position, symbol order and spacing say.
pub fn cash_out(v: i64, conv: &Conv) -> String {
    let points = conv.points() as i32;
    let mon_group = match conv.mon_grouping.first().map(|&g| g as i8) {
        Some(g @ 1..=6) => i32::from(g),
        _ => 3,
    };
    let dsymbol = conv.dsymbol();
    let ssymbol = conv.ssymbol();
    let csymbol = String::from_utf8_lossy(conv.csymbol()).into_owned();
    let (signsymbol, sign_posn, cs_precedes, sep_by_space) = if v < 0 {
        (
            if conv.negative_sign.is_empty() {
                "-"
            } else {
                conv.negative_sign.as_str()
            },
            conv.n_sign_posn,
            conv.n_cs_precedes,
            conv.n_sep_by_space,
        )
    } else {
        (
            conv.positive_sign.as_str(),
            conv.p_sign_posn,
            conv.p_cs_precedes,
            conv.p_sep_by_space,
        )
    };
    // The digits, the decimal point and the separators, right to left.
    let mut u = v.unsigned_abs();
    let mut digit_pos = points;
    let mut rev: Vec<u8> = Vec::new();
    loop {
        if points != 0 && digit_pos == 0 {
            rev.push(dsymbol);
        } else if digit_pos < 0 && digit_pos % mon_group == 0 {
            rev.extend(ssymbol.iter().rev());
        }
        rev.push(b'0' + (u % 10) as u8);
        u /= 10;
        digit_pos -= 1;
        if u == 0 && digit_pos < 0 {
            break;
        }
    }
    rev.reverse();
    let amount = String::from_utf8_lossy(&rev).into_owned();
    let sp = |n: i8| if sep_by_space == n { " " } else { "" };
    let c = cs_precedes != 0;
    let (s, cs, a) = (signsymbol, csymbol.as_str(), amount.as_str());
    match sign_posn {
        0 if c => format!("({cs}{}{a})", sp(1)),
        0 => format!("({a}{}{cs})", sp(1)),
        2 if c => format!("{cs}{}{a}{}{s}", sp(1), sp(2)),
        2 => format!("{a}{}{cs}{}{s}", sp(1), sp(2)),
        3 if c => format!("{s}{}{cs}{}{a}", sp(2), sp(1)),
        3 => format!("{a}{}{s}{}{cs}", sp(1), sp(2)),
        4 if c => format!("{cs}{}{s}{}{a}", sp(2), sp(1)),
        4 => format!("{a}{}{cs}{}{s}", sp(1), sp(2)),
        _ if c => format!("{s}{}{cs}{}{a}", sp(2), sp(1)),
        _ => format!("{s}{}{a}{}{cs}", sp(2), sp(1)),
    }
}

/// An integer as money, `int4_cash` and `int8_cash`: its count of the smallest unit.
pub fn of_integer(v: i64, conv: &Conv) -> Result<i64> {
    v.checked_mul(conv.scale())
        .ok_or_else(|| Error::new("bigint out of range"))
}

/// A numeric as money, `numeric_cash`: times ten to the fraction digits, rounded to the nearest
/// count as numeric_int8 rounds.
pub fn of_numeric(n: &crate::numeric::Numeric, conv: &Conv) -> Result<i64> {
    n.mul(&crate::numeric::Numeric::of_int(conv.scale()))?
        .to_integer(i64::MIN, i64::MAX, "bigint")
}

/// money as a numeric, `cash_numeric`: its count over ten to the fraction digits, at that scale.
pub fn to_numeric(v: i64, conv: &Conv) -> crate::numeric::Numeric {
    crate::numeric::Numeric::of_units(i128::from(v), conv.points())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conv(fields: &[(&str, &str)]) -> Conv {
        Conv::of(fields)
    }

    /// en_US's conventions as glibc 2.39 gives them, and the fields `others` changes.
    fn us(others: &[(&str, &str)]) -> Conv {
        let mut all = vec![
            ("currency_symbol", "$"),
            ("mon_decimal_point", "."),
            ("mon_thousands_sep", ","),
            ("mon_grouping", "3"),
            ("negative_sign", "-"),
            ("frac_digits", "2"),
            ("p_cs_precedes", "1"),
            ("p_sep_by_space", "0"),
            ("n_cs_precedes", "1"),
            ("n_sep_by_space", "0"),
            ("p_sign_posn", "1"),
            ("n_sign_posn", "1"),
        ];
        all.extend_from_slice(others);
        conv(&all)
    }

    #[test]
    fn c_reads_and_writes_money_as_dollars_and_cents() {
        let c = Monetary::C.conv();
        assert_eq!(cash_out(123456789, c), "$1,234,567.89");
        assert_eq!(cash_out(-150, c), "-$1.50");
        assert_eq!(cash_out(0, c), "$0.00");
        assert_eq!(cash_out(i64::MIN, c), "-$92,233,720,368,547,758.08");
        assert_eq!(cash_in(" ( $1,234.565 ) ", c).unwrap(), -123457);
        assert_eq!(cash_in("-92233720368547758.08", c).unwrap(), i64::MIN);
        assert!(cash_in("92233720368547758.08", c).is_err());
        assert!(cash_in("1.00x", c).is_err());
        assert_eq!(Monetary::open("POSIX").unwrap(), Monetary::C);
    }

    #[test]
    fn the_sign_and_the_symbol_take_each_place_the_locale_names() {
        // Each sign position, the symbol before and after, each spacing: cash_out's own switch.
        let want = [
            ("0", "1", "0", "($1.50)", "($1.50)"),
            ("0", "1", "1", "($ 1.50)", "($ 1.50)"),
            ("0", "0", "1", "(1.50 $)", "(1.50 $)"),
            ("1", "1", "0", "-$1.50", "+$1.50"),
            ("1", "1", "2", "- $1.50", "+ $1.50"),
            ("1", "0", "1", "-1.50 $", "+1.50 $"),
            ("2", "1", "1", "$ 1.50-", "$ 1.50+"),
            ("2", "0", "2", "1.50$ -", "1.50$ +"),
            ("3", "1", "2", "- $1.50", "+ $1.50"),
            ("3", "0", "1", "1.50 -$", "1.50 +$"),
            ("4", "1", "2", "$ -1.50", "$ +1.50"),
            ("4", "0", "2", "1.50$ -", "1.50$ +"),
            ("9", "1", "1", "-$ 1.50", "+$ 1.50"),
        ];
        for (posn, precedes, space, negative, positive) in want {
            let c = us(&[
                ("positive_sign", "+"),
                ("n_sign_posn", posn),
                ("p_sign_posn", posn),
                ("n_cs_precedes", precedes),
                ("p_cs_precedes", precedes),
                ("n_sep_by_space", space),
                ("p_sep_by_space", space),
            ]);
            assert_eq!(cash_out(-150, &c), negative, "{posn} {precedes} {space}");
            assert_eq!(cash_out(150, &c), positive, "{posn} {precedes} {space}");
            assert_eq!(cash_in(negative, &c).unwrap(), -150, "{negative}");
        }
    }

    #[test]
    fn the_fraction_digits_and_the_separators_are_the_locale_s() {
        let dk = us(&[
            ("currency_symbol", "kr."),
            ("mon_decimal_point", ","),
            ("mon_thousands_sep", "."),
            ("p_sep_by_space", "2"),
            ("n_sep_by_space", "2"),
            ("p_sign_posn", "4"),
            ("n_sign_posn", "4"),
        ]);
        assert_eq!(cash_out(123456789100, &dk), "kr. 1.234.567.891,00");
        assert_eq!(cash_out(-1500, &dk), "kr. -15,00");
        assert_eq!(cash_in("1234567.891", &dk).unwrap(), 123456789100);
        assert_eq!(cash_in("kr. -1,50", &dk).unwrap(), -150);
        assert_eq!(cash_in("-1,5", &dk).unwrap(), -150);
        assert!(cash_in("$1.00", &dk).is_err());
        // No fraction digits: whole units, no decimal point; three: a thousandth.
        let yen = us(&[("frac_digits", "0"), ("currency_symbol", "¥")]);
        assert_eq!(cash_out(-1234567, &yen), "-¥1,234,567");
        assert_eq!(cash_in("¥1,234.5", &yen).unwrap(), 1235);
        assert_eq!(to_numeric(1235, &yen).to_string(), "1235");
        let n = |s: &str| crate::numeric::Numeric::parse(s).unwrap();
        assert_eq!(of_numeric(&n("1234.5"), &yen).unwrap(), 1235);
        let dinar = us(&[("frac_digits", "3"), ("mon_grouping", "4")]);
        assert_eq!(cash_out(12345678, &dinar), "$1,2345.678");
        assert_eq!(cash_in("1.2345", &dinar).unwrap(), 1235);
        assert_eq!(of_integer(5, &dinar).unwrap(), 5000);
        assert_eq!(of_numeric(&n("-1.2345"), &dinar).unwrap(), -1235);
        assert_eq!(to_numeric(-1235, &dinar).to_string(), "-1.235");
        // A decimal point of two bytes is a point; separators of two are taken whole.
        let wide = us(&[
            ("mon_decimal_point", "::"),
            ("mon_thousands_sep", "\u{202f}"),
        ]);
        assert_eq!(cash_out(123456789, &wide), "$1\u{202f}234\u{202f}567.89");
        assert_eq!(cash_in("$1\u{202f}234.5", &wide).unwrap(), 123450);
    }
}
