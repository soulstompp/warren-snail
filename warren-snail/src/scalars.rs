//! The fixed scalars: each one's key, its text as PostgreSQL writes it, and its input as
//! PostgreSQL reads it.
//!
//! A uuid is keyed in 128 bits: its bytes read big-endian, the top bit flipped, so that its keys
//! sort as its bytes do. An unsigned number (an oid, an xid, a pg_lsn, a `"char"`) is keyed in the
//! signed range of its width, and money as its count of cents.

use crate::catalog::ColumnType;
use crate::{Error, Result};
use std::sync::atomic::{AtomicI64, Ordering};

const TOP: u128 = 1 << 127;

/// A uuid's key, from its bytes.
pub fn uuid_key(bytes: [u8; 16]) -> i128 {
    (u128::from_be_bytes(bytes) ^ TOP) as i128
}

/// A uuid's bytes, from its key.
pub fn uuid_bytes(key: i128) -> [u8; 16] {
    (key as u128 ^ TOP).to_be_bytes()
}

/// A uuid as PostgreSQL's `uuid_out` writes it: lower-case hex digits in groups of 8, 4, 4, 4 and
/// 12, joined by hyphens.
pub fn uuid_text(key: i128) -> String {
    let mut out = String::with_capacity(36);
    for (i, b) in uuid_bytes(key).iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// A uuid as PostgreSQL's `uuid_in` reads it: 32 hex digits in either case, a hyphen allowed after
/// any group of four but the last, the whole in braces or not.
pub fn uuid_parse(s: &str) -> Result<i128> {
    let wrong = || Error::new(format!("invalid input syntax for type uuid: \"{s}\""));
    let b = s.as_bytes();
    let (mut at, braces) = if b.first() == Some(&b'{') {
        (1, true)
    } else {
        (0, false)
    };
    let mut bytes = [0u8; 16];
    for (i, out) in bytes.iter_mut().enumerate() {
        let digit = |c: Option<&u8>| c.and_then(|c| (*c as char).to_digit(16));
        let (Some(h), Some(l)) = (digit(b.get(at)), digit(b.get(at + 1))) else {
            return Err(wrong());
        };
        *out = (h * 16 + l) as u8;
        at += 2;
        if b.get(at) == Some(&b'-') && i % 2 == 1 && i < 15 {
            at += 1;
        }
    }
    if braces {
        if b.get(at) != Some(&b'}') {
            return Err(wrong());
        }
        at += 1;
    }
    if at != b.len() {
        return Err(wrong());
    }
    Ok(uuid_key(bytes))
}

/// Fills `buf` with random bytes from the machine's own source, as PostgreSQL's
/// `pg_strong_random` does.
fn random(buf: &mut [u8]) -> Result<()> {
    use std::io::Read;
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(buf))
        .map_err(|e| Error::new(format!("could not generate random values: {e}")))
}

/// Sets a uuid's version and its variant, RFC 9562's.
fn versioned(mut bytes: [u8; 16], version: u8) -> i128 {
    bytes[6] = (bytes[6] & 0x0f) | (version << 4);
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid_key(bytes)
}

/// A version 4 uuid, every bit random but its version's and its variant's, as
/// `gen_random_uuid()` and `uuidv4()` hand one out.
pub fn uuid_v4() -> Result<i128> {
    let mut bytes = [0u8; 16];
    random(&mut bytes)?;
    Ok(versioned(bytes, 4))
}

const NS_PER_MS: i64 = 1_000_000;
/// The nanoseconds a v7's twelve bits of a millisecond tell apart, and one more: the least step
/// between two v7s one writer hands out.
const SUBMS_MINIMAL_STEP_NS: i64 = NS_PER_MS / (1 << 12) + 1;

/// The last instant a v7 was handed out at, in nanoseconds since 1970 UTC.
static PREVIOUS_NS: AtomicI64 = AtomicI64::new(0);

/// The instant a v7 takes, the clock reading `now`, where the last took `prev`: at least a step past
/// it, so the v7s one writer hands out rise.
fn ns_after(now: i64, prev: i64) -> i64 {
    now.max(prev + SUBMS_MINIMAL_STEP_NS)
}

/// The machine's clock in nanoseconds since 1970 UTC, as the next v7 takes it.
fn ns_ascending() -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    let mut prev = PREVIOUS_NS.load(Ordering::Relaxed);
    loop {
        let ns = ns_after(now, prev);
        match PREVIOUS_NS.compare_exchange_weak(prev, ns, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return ns,
            Err(p) => prev = p,
        }
    }
}

/// The version 7 uuid of instant `ns`, its last 8 bytes `rest` but its variant's.
fn uuid_v7_at(ns: i64, rest: [u8; 8]) -> i128 {
    let (ms, sub) = (ns / NS_PER_MS, ns % NS_PER_MS);
    let mut bytes = [0u8; 16];
    bytes[..6].copy_from_slice(&(ms as u64).to_be_bytes()[2..]);
    let fraction = (sub * (1 << 12) / NS_PER_MS) as u16;
    bytes[6..8].copy_from_slice(&fraction.to_be_bytes());
    bytes[8..].copy_from_slice(&rest);
    versioned(bytes, 7)
}

/// A version 7 uuid, as `uuidv7()` hands one out: the milliseconds since 1970 UTC in 48 bits, the
/// millisecond's fraction in the next 12, the rest random but its version's and its variant's.
pub fn uuid_v7() -> Result<i128> {
    let ns = ns_ascending();
    let mut rest = [0u8; 8];
    random(&mut rest)?;
    Ok(uuid_v7_at(ns, rest))
}

/// The key of an unsigned number of `bits` bits: held in the signed range of its width, so its keys
/// sort as the numbers do.
pub fn unsigned_key(v: u64, bits: u32) -> i64 {
    (i128::from(v) - (1i128 << (bits - 1))) as i64
}

/// An unsigned number of `bits` bits, from its key.
pub fn unsigned_of(key: i64, bits: u32) -> u64 {
    (i128::from(key) + (1i128 << (bits - 1))) as u64
}

/// The value the file writes for a key of type `ty`: an unsigned number's own bits, any other key
/// as it is.
pub fn raw_of_key(ty: ColumnType, key: i64) -> i64 {
    match ty.unsigned_bits() {
        0 => key,
        bits => unsigned_of(key, bits) as i64,
    }
}

/// A key of type `ty`, from the value the file writes.
pub fn key_of_raw(ty: ColumnType, raw: i64) -> i64 {
    match ty.unsigned_bits() {
        0 => raw,
        64 => unsigned_key(raw as u64, 64),
        bits => unsigned_key(raw as u64 & ((1u64 << bits) - 1), bits),
    }
}

/// White space as C's `isspace` takes it under the C locale.
fn is_c_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r')
}

/// A number read as glibc's `strtoul` reads it with base 0, as C23 has it: white space, a sign, then
/// `0x` for hex, `0b` for binary, `0` for octal, or decimal. The number, whether it overflowed, and
/// where the reading stopped; none where no digit was read.
fn strtoul(s: &[u8]) -> Option<(u64, bool, usize)> {
    let mut at = 0;
    while at < s.len() && is_c_space(s[at]) {
        at += 1;
    }
    let negative = match s.get(at) {
        Some(b'-') => {
            at += 1;
            true
        }
        Some(b'+') => {
            at += 1;
            false
        }
        _ => false,
    };
    let digit = |b: u8, base: u32| (b as char).to_digit(base);
    let mut base = 10;
    if s.get(at) == Some(&b'0') {
        match s.get(at + 1) {
            Some(b'x' | b'X') if s.get(at + 2).is_some_and(|&b| digit(b, 16).is_some()) => {
                base = 16;
                at += 2;
            }
            Some(b'b' | b'B') if s.get(at + 2).is_some_and(|&b| digit(b, 2).is_some()) => {
                base = 2;
                at += 2;
            }
            _ => base = 8,
        }
    }
    let start = at;
    let (mut v, mut over) = (0u64, false);
    while let Some(d) = s.get(at).and_then(|&b| digit(b, base)) {
        match v
            .checked_mul(base as u64)
            .and_then(|x| x.checked_add(d as u64))
        {
            Some(x) => v = x,
            None => over = true,
        }
        at += 1;
    }
    if at == start {
        return None;
    }
    if over {
        return Some((u64::MAX, true, at));
    }
    Some((if negative { v.wrapping_neg() } else { v }, false, at))
}

/// An unsigned number as PostgreSQL's `uint32in_subr` or `uint64in_subr` reads one for type `name`:
/// `strtoul`'s reading, white space after it; a 32-bit one taken where it fits after the number is
/// extended either way, so `-1` is the largest.
fn unsigned_in(s: &str, bits: u32, name: &str) -> Result<u64> {
    let wrong = || Error::new(format!("invalid input syntax for type {name}: \"{s}\""));
    let range = || Error::new(format!("value \"{s}\" is out of range for type {name}"));
    let (v, over, end) = strtoul(s.as_bytes()).ok_or_else(wrong)?;
    if over {
        return Err(range());
    }
    if !s.as_bytes()[end..].iter().all(|&b| is_c_space(b)) {
        return Err(wrong());
    }
    if bits == 32 {
        let r = v as u32;
        if v != u64::from(r) && v != (r as i32) as i64 as u64 {
            return Err(range());
        }
        return Ok(u64::from(r));
    }
    Ok(v)
}

/// A `"char"` as PostgreSQL's `charin` reads it: a backslash and three octal digits, or else the
/// first byte, none for the empty string.
fn char_in(s: &str) -> u8 {
    let b = s.as_bytes();
    let octal = |c: u8| (b'0'..=b'7').contains(&c);
    if b.len() == 4 && b[0] == b'\\' && b[1..].iter().all(|&c| octal(c)) {
        return ((b[1] - b'0') << 6).wrapping_add((b[2] - b'0') << 3) + (b[3] - b'0');
    }
    b.first().copied().unwrap_or(0)
}

/// A `"char"` as PostgreSQL's `charout` writes it: a byte with its high bit set as a backslash and
/// three octal digits, the empty string for 0, any other byte as itself.
fn char_out(c: u8) -> String {
    if c >= 0x80 {
        format!("\\{c:03o}")
    } else if c == 0 {
        String::new()
    } else {
        (c as char).to_string()
    }
}

/// A pg_lsn as PostgreSQL's `pg_lsn_in` reads it: one to eight hex digits, a slash, one to eight.
fn lsn_in(s: &str) -> Result<u64> {
    let wrong = || Error::new(format!("invalid input syntax for type pg_lsn: \"{s}\""));
    let (hi, lo) = s.split_once('/').ok_or_else(wrong)?;
    let part = |p: &str| {
        if (1..=8).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_hexdigit()) {
            u32::from_str_radix(p, 16).map_err(|_| wrong())
        } else {
            Err(wrong())
        }
    };
    Ok((u64::from(part(hi)?) << 32) | u64::from(part(lo)?))
}

/// A pg_lsn as PostgreSQL 19's `pg_lsn_out` writes it, `%X/%08X`.
fn lsn_out(v: u64) -> String {
    format!("{:X}/{:08X}", v >> 32, v as u32)
}

/// money as PostgreSQL's `cash_in` reads it under the C locale: white space, a `$`, a sign or an
/// opening parenthesis, digits with commas among them, a point and two digits, the next digit
/// rounding the last half up; then a closing parenthesis, a sign or a `$`.
fn money_in(s: &str) -> Result<i64> {
    let wrong = || Error::new(format!("invalid input syntax for type money: \"{s}\""));
    let range = || Error::new(format!("value \"{s}\" is out of range for type money"));
    let b = s.as_bytes();
    let mut at = 0;
    let skip = |at: &mut usize| {
        while *at < b.len() && is_c_space(b[*at]) {
            *at += 1;
        }
    };
    let symbol = |at: &mut usize| {
        if b.get(*at) == Some(&b'$') {
            *at += 1;
        }
    };
    skip(&mut at);
    symbol(&mut at);
    skip(&mut at);
    let mut negative = false;
    match b.get(at) {
        Some(b'-') | Some(b'(') => {
            negative = true;
            at += 1;
        }
        Some(b'+') => at += 1,
        _ => {}
    }
    skip(&mut at);
    symbol(&mut at);
    skip(&mut at);
    // Built in the negative, as PostgreSQL builds it, so the most negative value is reached.
    let (mut value, mut dec, mut dot) = (0i64, 0, false);
    while at < b.len() {
        let c = b[at];
        if c.is_ascii_digit() && (!dot || dec < 2) {
            value = value
                .checked_mul(10)
                .and_then(|v| v.checked_sub(i64::from(c - b'0')))
                .ok_or_else(range)?;
            if dot {
                dec += 1;
            }
        } else if c == b'.' && !dot {
            dot = true;
        } else if c != b',' {
            break;
        }
        at += 1;
    }
    if b.get(at).is_some_and(|&c| (b'5'..=b'9').contains(&c)) {
        value = value.checked_sub(1).ok_or_else(range)?;
    }
    for _ in dec..2 {
        value = value.checked_mul(10).ok_or_else(range)?;
    }
    while b.get(at).is_some_and(u8::is_ascii_digit) {
        at += 1;
    }
    while at < b.len() {
        match b[at] {
            c if is_c_space(c) || c == b')' || c == b'+' || c == b'$' => {}
            b'-' => negative = true,
            _ => return Err(wrong()),
        }
        at += 1;
    }
    if negative {
        Ok(value)
    } else {
        value.checked_neg().ok_or_else(range)
    }
}

/// money as PostgreSQL's `cash_out` writes it under the C locale: a `$`, commas between thousands,
/// a point and two digits, a minus sign before it all where it is negative.
fn money_out(v: i64) -> String {
    let u = v.unsigned_abs();
    let whole = (u / 100).to_string();
    let mut grouped = String::new();
    for (i, c) in whole.chars().enumerate() {
        if i > 0 && (whole.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(c);
    }
    format!("{}${grouped}.{:02}", if v < 0 { "-" } else { "" }, u % 100)
}

/// A key of a type keyed as a number as PostgreSQL's output function writes the value.
pub fn scalar_text(ty: ColumnType, key: i64) -> String {
    match ty {
        ColumnType::Oid | ColumnType::Xid | ColumnType::Cid | ColumnType::Xid8 => {
            unsigned_of(key, ty.unsigned_bits()).to_string()
        }
        ColumnType::PgLsn => lsn_out(unsigned_of(key, 64)),
        ColumnType::Char => char_out(unsigned_of(key, 8) as u8),
        ColumnType::Money => money_out(key),
        _ => key.to_string(),
    }
}

/// A value of a type keyed as a number, read from its text as PostgreSQL's input function reads
/// it, as its key.
pub fn scalar_parse(ty: ColumnType, s: &str) -> Result<i64> {
    Ok(match ty {
        ColumnType::Oid | ColumnType::Xid | ColumnType::Cid | ColumnType::Xid8 => {
            let bits = ty.unsigned_bits();
            unsigned_key(unsigned_in(s, bits, &ty.pg_name())?, bits)
        }
        ColumnType::PgLsn => unsigned_key(lsn_in(s)?, 64),
        ColumnType::Char => unsigned_key(u64::from(char_in(s)), 8),
        ColumnType::Money => money_in(s)?,
        other => {
            return Err(Error::new(format!(
                "{} is not read as a scalar",
                other.pg_name()
            )))
        }
    })
}

/// The key of an answer's field metadata that names the PostgreSQL type of a column whose Arrow
/// type does not say it.
pub const PG_TYPE: &str = "pg_type";

/// The Arrow type an answer holds a scalar of type `ty` as: what arrow's Parquet reader gives its
/// page form.
pub fn scalar_arrow(ty: ColumnType) -> arrow::datatypes::DataType {
    use arrow::datatypes::DataType as A;
    match ty.unsigned_bits() {
        8 => A::UInt8,
        32 => A::UInt32,
        64 => A::UInt64,
        _ => A::Int64,
    }
}

/// A column of scalars of type `ty`, from their keys, as an answer holds it.
pub fn scalar_array(
    ty: ColumnType,
    keys: impl Iterator<Item = Option<i64>>,
) -> arrow::array::ArrayRef {
    use arrow::array::{Int64Array, UInt32Array, UInt64Array, UInt8Array};
    use std::sync::Arc;
    match ty.unsigned_bits() {
        8 => Arc::new(
            keys.map(|k| k.map(|k| unsigned_of(k, 8) as u8))
                .collect::<UInt8Array>(),
        ),
        32 => Arc::new(
            keys.map(|k| k.map(|k| unsigned_of(k, 32) as u32))
                .collect::<UInt32Array>(),
        ),
        64 => Arc::new(
            keys.map(|k| k.map(|k| unsigned_of(k, 64)))
                .collect::<UInt64Array>(),
        ),
        _ => Arc::new(keys.collect::<Int64Array>()),
    }
}

/// Row `r`'s key in a column of scalars of type `ty` an answer holds; none for a NULL.
pub fn scalar_key_at(ty: ColumnType, a: &dyn arrow::array::Array, r: usize) -> Option<i64> {
    use arrow::array::{Int64Array, UInt32Array, UInt64Array, UInt8Array};
    if a.is_null(r) {
        return None;
    }
    let any = a.as_any();
    Some(match ty.unsigned_bits() {
        8 => unsigned_key(u64::from(any.downcast_ref::<UInt8Array>()?.value(r)), 8),
        32 => unsigned_key(u64::from(any.downcast_ref::<UInt32Array>()?.value(r)), 32),
        64 => unsigned_key(any.downcast_ref::<UInt64Array>()?.value(r), 64),
        _ => any.downcast_ref::<Int64Array>()?.value(r),
    })
}

/// An answer's field, named for a scalar's PostgreSQL type in its metadata.
pub fn scalar_field(name: &str, ty: ColumnType, nullable: bool) -> arrow::datatypes::Field {
    arrow::datatypes::Field::new(name, scalar_arrow(ty), nullable)
        .with_metadata([(PG_TYPE.to_string(), ty.pg_name())].into_iter().collect())
}

/// The scalar type an answer's field holds, by its metadata.
pub fn field_scalar(f: &arrow::datatypes::Field) -> Option<ColumnType> {
    let named = f.metadata().get(PG_TYPE)?;
    [
        ColumnType::Oid,
        ColumnType::Xid,
        ColumnType::Cid,
        ColumnType::Xid8,
        ColumnType::PgLsn,
        ColumnType::Char,
        ColumnType::Money,
    ]
    .into_iter()
    .find(|t| t.pg_name() == *named && scalar_arrow(*t) == *f.data_type())
}

/// Where a `"char"`'s text lies among the others' by its bytes: the order of PostgreSQL's min and max of
/// one, which it reads through text. The empty text of 0 first; a backslash's, then the bytes
/// written in octal, each a backslash and three digits, then the bytes past a backslash.
pub fn char_text_rank(b: u8) -> i64 {
    let b = i64::from(b);
    match b {
        0..=0x5c => b,
        0x80.. => 0x5d + (b - 0x80),
        _ => b + 0x80,
    }
}

/// The byte of a `"char"` whose text has rank `r`.
pub fn char_of_text_rank(r: i64) -> u8 {
    (match r {
        0..=0x5c => r,
        0x5d..=0xdc => r - 0x5d + 0x80,
        _ => r - 0x80,
    }) as u8
}

/// Whether a type is one `scalar_text` and `scalar_parse` read and write.
pub fn is_scalar(ty: ColumnType) -> bool {
    matches!(
        ty,
        ColumnType::Oid
            | ColumnType::Xid
            | ColumnType::Cid
            | ColumnType::Xid8
            | ColumnType::PgLsn
            | ColumnType::Char
            | ColumnType::Money
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unsigned_number_s_key_sorts_as_the_number_and_gives_it_back() {
        for bits in [8, 32, 64] {
            let top = if bits == 64 {
                u64::MAX
            } else {
                (1u64 << bits) - 1
            };
            let all = [0, 1, top / 2, top / 2 + 1, top - 1, top];
            for w in all.windows(2) {
                assert!(
                    unsigned_key(w[0], bits) < unsigned_key(w[1], bits),
                    "{bits}"
                );
            }
            for v in all {
                let k = unsigned_key(v, bits);
                assert_eq!(unsigned_of(k, bits), v);
                assert!(
                    i128::from(k) >= -(1i128 << (bits - 1)) && i128::from(k) < 1i128 << (bits - 1)
                );
            }
        }
    }

    #[test]
    fn a_char_s_text_rank_sorts_as_its_text_and_gives_its_byte_back() {
        let mut by_text: Vec<u8> = (0..=255).collect();
        by_text.sort_by_key(|&b| char_out(b).into_bytes());
        let mut by_rank: Vec<u8> = (0..=255).collect();
        by_rank.sort_by_key(|&b| char_text_rank(b));
        assert_eq!(by_rank, by_text);
        for b in 0..=255u8 {
            assert_eq!(char_of_text_rank(char_text_rank(b)), b);
        }
    }

    #[test]
    fn oid_xid8_char_pg_lsn_and_money_read_and_write_as_postgresql_does() {
        let oid =
            |s: &str| scalar_parse(ColumnType::Oid, s).map(|k| scalar_text(ColumnType::Oid, k));
        for (s, want) in [
            ("0b101", "5"),
            ("010", "8"),
            ("0x10", "16"),
            ("0X1f", "31"),
            (" +5 ", "5"),
            ("-1", "4294967295"),
            ("-2147483648", "2147483648"),
            ("4294967295", "4294967295"),
            ("\t7\n", "7"),
            ("00", "0"),
            ("-0", "0"),
        ] {
            assert_eq!(oid(s).unwrap(), want, "{s}");
        }
        for s in [
            "08",
            "-2147483649",
            "4294967296",
            "",
            " ",
            "0x",
            "5x",
            "+-1",
            "0o17",
        ] {
            assert!(oid(s).is_err(), "{s}");
        }
        let xid8 =
            |s: &str| scalar_parse(ColumnType::Xid8, s).map(|k| scalar_text(ColumnType::Xid8, k));
        assert_eq!(xid8("-1").unwrap(), "18446744073709551615");
        assert!(xid8("18446744073709551616").is_err());
        let ch =
            |s: &str| scalar_text(ColumnType::Char, scalar_parse(ColumnType::Char, s).unwrap());
        for (s, want) in [
            ("a", "a"),
            ("abc", "a"),
            ("\\377", "\\377"),
            ("", ""),
            ("\\101", "A"),
            ("é", "\\303"),
        ] {
            assert_eq!(ch(s), want, "{s}");
        }
        let lsn =
            |s: &str| scalar_parse(ColumnType::PgLsn, s).map(|k| scalar_text(ColumnType::PgLsn, k));
        assert_eq!(lsn("16/b374d848").unwrap(), "16/B374D848");
        assert_eq!(lsn("0/10").unwrap(), "0/00000010");
        for s in ["", "0/", "/0", "123456789/0", " 0/0", "0/0 ", "g/0"] {
            assert!(lsn(s).is_err(), "{s}");
        }
        let money =
            |s: &str| scalar_parse(ColumnType::Money, s).map(|k| scalar_text(ColumnType::Money, k));
        for (s, want) in [
            ("-1234567.891", "-$1,234,567.89"),
            ("0", "$0.00"),
            ("-0.01", "-$0.01"),
            ("92233720368547758.07", "$92,233,720,368,547,758.07"),
            ("-92233720368547758.08", "-$92,233,720,368,547,758.08"),
            ("1.005", "$1.01"),
            (" ( $ 1.5 ) ", "-$1.50"),
            ("1.5-", "-$1.50"),
            ("$1,000.00", "$1,000.00"),
        ] {
            assert_eq!(money(s).unwrap(), want, "{s}");
        }
        for s in ["92233720368547758.08", "x", "1.5x"] {
            assert!(money(s).is_err(), "{s}");
        }
    }

    #[test]
    fn a_uuid_s_key_sorts_as_its_bytes_and_gives_them_back() {
        let mut all: Vec<[u8; 16]> = Vec::new();
        for a in [0u8, 1, 0x7f, 0x80, 0xff] {
            for z in [0u8, 1, 0x7f, 0x80, 0xff] {
                let mut b = [0u8; 16];
                b[0] = a;
                b[15] = z;
                all.push(b);
            }
        }
        let mut by_key = all.clone();
        by_key.sort_by_key(|b| uuid_key(*b));
        all.sort();
        assert_eq!(by_key, all);
        for b in all {
            assert_eq!(uuid_bytes(uuid_key(b)), b);
        }
    }

    #[test]
    fn a_uuid_is_read_and_written_as_postgresql_reads_and_writes_it() {
        let canonical = "00112233-4455-6677-8899-aabbccddeeff";
        for s in [
            canonical,
            "{00112233-4455-6677-8899-AABBCCDDEEFF}",
            "00112233445566778899aabbccddeeff",
            "0011-2233-4455-6677-8899-aabb-ccdd-eeff",
            "{00112233445566778899aabbccddeeff}",
        ] {
            assert_eq!(uuid_text(uuid_parse(s).unwrap()), canonical, "{s}");
        }
        for s in [
            "",
            "0011223344556677889aabbccddeeff",
            "00112233445566778899aabbccddeeff0",
            "-00112233445566778899aabbccddeeff",
            "00112233445566778899aabbccddeeff-",
            "0-0112233445566778899aabbccddeeff",
            "00-112233445566778899aabbccddeeff",
            "001122-33445566778899aabbccddeeff",
            "00112233--4455-6677-8899-aabbccddeeff",
            "{00112233445566778899aabbccddeeff",
            "00112233445566778899aabbccddeeff}",
            " 00112233445566778899aabbccddeeff",
            "g0112233445566778899aabbccddeeff",
        ] {
            assert!(uuid_parse(s).is_err(), "{s}");
        }
    }

    #[test]
    fn a_v4_and_a_v7_carry_their_versions_and_v7s_rise() {
        let version = |k: i128| uuid_bytes(k)[6] >> 4;
        let variant = |k: i128| uuid_bytes(k)[8] >> 6;
        let v4 = uuid_v4().unwrap();
        assert_eq!((version(v4), variant(v4)), (4, 2));
        let mut last = uuid_v7().unwrap();
        for _ in 0..10_000 {
            let next = uuid_v7().unwrap();
            assert_eq!((version(next), variant(next)), (7, 2));
            assert!(next > last);
            last = next;
        }
        // A clock that stands still, or steps back, still gives v7s that rise, whatever their
        // random bytes: each takes a step past the last, and its fraction rises with it.
        let (mut prev, mut last) = (0i64, i128::MIN);
        for (i, now) in [
            5_000_000i64,
            5_000_000,
            5_000_000,
            4_000_000,
            5_000_100,
            9_000_000,
        ]
        .into_iter()
        .enumerate()
        {
            let ns = ns_after(now, prev);
            assert!(ns > prev, "{i}");
            let v7 = uuid_v7_at(ns, [0xff - i as u8 * 40; 8]);
            assert!(v7 > last, "{i}");
            (prev, last) = (ns, v7);
        }
    }
}
