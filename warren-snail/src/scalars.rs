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
    uuid_v7_of(ns / NS_PER_MS, ns % NS_PER_MS, rest)
}

/// The version 7 uuid of millisecond `ms` since 1970 UTC and `sub` nanoseconds into it, its last 8
/// bytes `rest` but its variant's, as `generate_uuidv7` makes it.
fn uuid_v7_of(ms: i64, sub: i64, rest: [u8; 8]) -> i128 {
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

/// The last instant a version 7 uuid holds, in microseconds since 1970 UTC: the last of its 48
/// bits of milliseconds.
const V7_LAST_US: i64 = ((1 << 48) - 1) * 1000;

/// A version 7 uuid as `uuidv7(interval)` hands one out: the machine's clock to the microsecond,
/// moved by `shift` as a timestamptz in microseconds since 1970 UTC, its nanoseconds kept; refused
/// where the instant falls before 1970 or past the 48 bits of milliseconds.
pub fn uuid_v7_shifted(shift: impl FnOnce(i64) -> Result<i64>) -> Result<i128> {
    let ns = ns_ascending();
    let us = shift(ns / 1000)?;
    if !(0..=V7_LAST_US).contains(&us) {
        return Err(Error::new("timestamp out of range for UUID version 7"));
    }
    let mut rest = [0u8; 8];
    random(&mut rest)?;
    Ok(uuid_v7_of(us / 1000, (us % 1000) * 1000 + ns % 1000, rest))
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
    raw_of_bits(key, ty.unsigned_bits())
}

/// A key of type `ty`, from the value the file writes.
pub fn key_of_raw(ty: ColumnType, raw: i64) -> i64 {
    key_of_bits(raw, ty.unsigned_bits())
}

/// The value the file writes for the key of an unsigned number of `bits` bits, its own bits; for
/// `bits` 0, any other key, as it is.
pub(crate) fn raw_of_bits(key: i64, bits: u32) -> i64 {
    match bits {
        0 => key,
        bits => unsigned_of(key, bits) as i64,
    }
}

/// The key of an unsigned number of `bits` bits from the value the file writes; for `bits` 0, any
/// other value, as it is.
pub(crate) fn key_of_bits(raw: i64, bits: u32) -> i64 {
    match bits {
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
pub(crate) fn unsigned_in(s: &str, bits: u32, name: &str) -> Result<u64> {
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

/// A number read as glibc's `strtoul` reads it in base 10: white space, a sign, then digits. The
/// number, whether it overflowed, and where the reading stopped: at the start where no digit was
/// read.
fn strtoul10(s: &[u8]) -> (u64, bool, usize) {
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
    let start = at;
    let (mut v, mut over) = (0u64, false);
    while let Some(d) = s.get(at).filter(|b| b.is_ascii_digit()) {
        match v
            .checked_mul(10)
            .and_then(|x| x.checked_add(u64::from(d - b'0')))
        {
            Some(x) => v = x,
            None => over = true,
        }
        at += 1;
    }
    if at == start {
        return (0, false, 0);
    }
    if over {
        return (u64::MAX, true, at);
    }
    (if negative { v.wrapping_neg() } else { v }, false, at)
}

/// The key of a tid: its block number and its offset, the block's bits above the offset's.
pub fn tid_key(block: u32, offset: u16) -> i64 {
    unsigned_key((u64::from(block) << 16) | u64::from(offset), 48)
}

/// A tid's block number and offset, from its key.
pub fn tid_parts(key: i64) -> (u32, u16) {
    let v = unsigned_of(key, 48);
    ((v >> 16) as u32, v as u16)
}

/// A tid as PostgreSQL's `tidin` reads it: the first `(`, or a comma, begins the block number and
/// the next comma the offset, the scan stopping at a `)`; each read as `strtoul` reads it in base
/// 10, the block ending at the comma and taken where it fits after it is extended either way, the
/// offset ending at the `)` and at most 65535.
fn tid_in(s: &str) -> Result<i64> {
    let wrong = || Error::new(format!("invalid input syntax for type tid: \"{s}\""));
    let b = s.as_bytes();
    let mut coord = Vec::with_capacity(2);
    for (i, &c) in b.iter().enumerate() {
        if coord.len() == 2 || c == b')' {
            break;
        }
        if c == b',' || (c == b'(' && coord.is_empty()) {
            coord.push(i + 1);
        }
    }
    let [block_at, offset_at] = coord[..] else {
        return Err(wrong());
    };
    let (v, over, end) = strtoul10(&b[block_at..]);
    if over || b.get(block_at + end) != Some(&b',') {
        return Err(wrong());
    }
    let block = v as u32;
    if v != u64::from(block) && v != (block as i32) as i64 as u64 {
        return Err(wrong());
    }
    let (v, over, end) = strtoul10(&b[offset_at..]);
    if over || b.get(offset_at + end) != Some(&b')') || v > u64::from(u16::MAX) {
        return Err(wrong());
    }
    Ok(tid_key(block, v as u16))
}

/// A tid as PostgreSQL's `tidout` writes it: `(block,offset)`.
fn tid_out(key: i64) -> String {
    let (block, offset) = tid_parts(key);
    format!("({block},{offset})")
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

/// One `%x` or `%2x` of glibc's `sscanf`: white space, a sign, `0x` where no width bounds it, then
/// hex digits, at most `width` characters after the white space. The number as an int, and where
/// the reading stopped; none where no digit was read.
fn scan_hex(b: &[u8], mut at: usize, width: Option<usize>) -> Option<(i64, usize)> {
    while at < b.len() && is_c_space(b[at]) {
        at += 1;
    }
    let limit = width.map_or(b.len(), |w| (at + w).min(b.len()));
    let negative = match b.get(at) {
        Some(b'-') if at < limit => {
            at += 1;
            true
        }
        Some(b'+') if at < limit => {
            at += 1;
            false
        }
        _ => false,
    };
    if width.is_none()
        && b.get(at) == Some(&b'0')
        && matches!(b.get(at + 1), Some(b'x' | b'X'))
        && b.get(at + 2).is_some_and(u8::is_ascii_hexdigit)
    {
        at += 2;
    }
    let start = at;
    let mut v: u32 = 0;
    while at < limit && b[at].is_ascii_hexdigit() {
        v = v
            .wrapping_mul(16)
            .wrapping_add((b[at] as char).to_digit(16).unwrap());
        at += 1;
    }
    (at > start).then(|| {
        let v = if negative { v.wrapping_neg() } else { v };
        (i64::from(v as i32), at)
    })
}

/// A macaddr as PostgreSQL's `macaddr_in` reads it: six numbers in hex, by the first of its seven
/// `sscanf` patterns that takes the whole, each number from 0 to 255.
fn mac_in(s: &str) -> Result<u64> {
    let wrong = || Error::new(format!("invalid input syntax for type macaddr: \"{s}\""));
    let b = s.as_bytes();
    // Each pattern: the separator after each of the six numbers, and the width of each.
    let patterns: [(&[u8; 5], Option<usize>); 7] = [
        (b":::::", None),
        (b"-----", None),
        (b"\0\0:\0\0", Some(2)),
        (b"\0\0-\0\0", Some(2)),
        (b"\0.\0.\0", Some(2)),
        (b"\0-\0-\0", Some(2)),
        (b"\0\0\0\0\0", Some(2)),
    ];
    for (seps, width) in patterns {
        let mut at = 0;
        let mut out = [0i64; 6];
        let mut taken = true;
        for (n, slot) in out.iter_mut().enumerate() {
            match scan_hex(b, at, width) {
                Some((v, next)) => {
                    *slot = v;
                    at = next;
                }
                None => {
                    taken = false;
                    break;
                }
            }
            if n < 5 && seps[n] != 0 {
                if b.get(at) != Some(&seps[n]) {
                    taken = false;
                    break;
                }
                at += 1;
            }
        }
        if !taken || !b[at..].iter().all(|&c| is_c_space(c)) {
            continue;
        }
        if out.iter().any(|v| !(0..=255).contains(v)) {
            return Err(Error::new(format!(
                "invalid octet value in \"macaddr\" value: \"{s}\""
            )));
        }
        return Ok(out.iter().fold(0u64, |acc, &v| (acc << 8) | v as u64));
    }
    Err(wrong())
}

/// A macaddr8 as PostgreSQL's `macaddr8_in` reads it: white space, then six or eight bytes of two
/// hex digits each, one separator throughout or none, white space after; six bytes take FF:FE in
/// their middle. A single character left after the last byte is passed over, as PostgreSQL passes
/// it over.
fn mac8_in(s: &str) -> Result<u64> {
    let wrong = || Error::new(format!("invalid input syntax for type macaddr8: \"{s}\""));
    let b = s.as_bytes();
    let mut at = 0;
    while at < b.len() && is_c_space(b[at]) {
        at += 1;
    }
    let mut bytes = [0u8; 8];
    let (mut count, mut spacer) = (0usize, 0u8);
    while at + 1 < b.len() {
        if count == 8 {
            return Err(wrong());
        }
        let pair = std::str::from_utf8(&b[at..at + 2]).map_err(|_| wrong())?;
        if !pair.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err(wrong());
        }
        bytes[count] = u8::from_str_radix(pair, 16).map_err(|_| wrong())?;
        count += 1;
        at += 2;
        if matches!(b.get(at), Some(b':' | b'-' | b'.')) {
            if spacer == 0 {
                spacer = b[at];
            } else if spacer != b[at] {
                return Err(wrong());
            }
            at += 1;
        }
        if (count == 6 || count == 8) && b.get(at).is_some_and(|&c| is_c_space(c)) {
            if b[at..].iter().all(|&c| is_c_space(c)) {
                at = b.len();
            } else {
                return Err(wrong());
            }
        }
    }
    let bytes = match count {
        8 => bytes,
        6 => [
            bytes[0], bytes[1], bytes[2], 0xff, 0xfe, bytes[3], bytes[4], bytes[5],
        ],
        _ => return Err(wrong()),
    };
    Ok(u64::from_be_bytes(bytes))
}

/// A MAC address as PostgreSQL writes it: its bytes in lower-case hex, joined by colons.
fn mac_out(v: u64, n: usize) -> String {
    v.to_be_bytes()[8 - n..]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// The bytes the file writes for a key of a type it writes as a fixed array of bytes.
pub fn fixed_of_key(ty: ColumnType, key: i64) -> Vec<u8> {
    let n = ty.fixed_bytes().unwrap_or(8);
    unsigned_of(key, ty.unsigned_bits()).to_be_bytes()[8 - n..].to_vec()
}

/// The key of a value of type `ty` from the fixed array of bytes the file writes.
pub fn key_of_fixed(ty: ColumnType, bytes: &[u8]) -> i64 {
    let v = bytes.iter().fold(0u64, |acc, &b| (acc << 8) | u64::from(b));
    unsigned_key(v, ty.unsigned_bits())
}

/// A macaddr as a macaddr8, FF:FE in its middle, or a macaddr8 as a macaddr where its middle is
/// FF:FE, as PostgreSQL's casts between them take it.
pub fn mac_cast(from: ColumnType, to: ColumnType, key: i64) -> Result<i64> {
    if from == to {
        return Ok(key);
    }
    let b = fixed_of_key(from, key);
    match to {
        ColumnType::Macaddr8 => Ok(key_of_fixed(
            to,
            &[b[0], b[1], b[2], 0xff, 0xfe, b[3], b[4], b[5]],
        )),
        _ if b[3] == 0xff && b[4] == 0xfe => {
            Ok(key_of_fixed(to, &[b[0], b[1], b[2], b[5], b[6], b[7]]))
        }
        _ => Err(Error::new(
            "macaddr8 data out of range to convert to macaddr",
        )),
    }
}

/// The key of a real's bits: its class as PostgreSQL's comparison sees it, `-0` with `0` and every
/// NaN one place above infinity, held in the signed range of 32 bits.
pub fn float4_key(bits: u32) -> i64 {
    if f32::from_bits(bits).is_nan() {
        return 0x7f80_0001;
    }
    let s = if bits == 0x8000_0000 { 0 } else { bits as i32 };
    i64::from(if s >= 0 { s } else { s ^ 0x7fff_ffff })
}

/// The key of a double precision's bits, as `float4_key` keys a real's.
pub fn float8_key(bits: u64) -> i64 {
    if f64::from_bits(bits).is_nan() {
        return 0x7ff0_0000_0000_0001;
    }
    let s = if bits == 0x8000_0000_0000_0000 {
        0
    } else {
        bits as i64
    };
    if s >= 0 {
        s
    } else {
        s ^ 0x7fff_ffff_ffff_ffff
    }
}

/// The longest prefix of `s` glibc's `strtod` reads, past any white space: a decimal or hex number,
/// an infinity or a NaN. Where the number starts, and where the prefix ends; none where it reads
/// nothing.
fn strtod_prefix(s: &[u8]) -> Option<(usize, usize)> {
    let mut at = 0;
    while at < s.len() && is_c_space(s[at]) {
        at += 1;
    }
    let start = at;
    if matches!(s.get(at), Some(b'+' | b'-')) {
        at += 1;
    }
    let word = |at: usize, w: &str| {
        s.len() >= at + w.len() && s[at..at + w.len()].eq_ignore_ascii_case(w.as_bytes())
    };
    if word(at, "infinity") {
        return Some((start, at + 8));
    }
    if word(at, "inf") {
        return Some((start, at + 3));
    }
    if word(at, "nan") {
        let mut end = at + 3;
        if s.get(end) == Some(&b'(') {
            let mut k = end + 1;
            while s
                .get(k)
                .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_')
            {
                k += 1;
            }
            if s.get(k) == Some(&b')') {
                end = k + 1;
            }
        }
        return Some((start, end));
    }
    let hex = s.get(at) == Some(&b'0')
        && matches!(s.get(at + 1), Some(b'x' | b'X'))
        && (s.get(at + 2).is_some_and(u8::is_ascii_hexdigit)
            || (s.get(at + 2) == Some(&b'.') && s.get(at + 3).is_some_and(u8::is_ascii_hexdigit)));
    let (digit, marker): (fn(&u8) -> bool, u8) = if hex {
        at += 2;
        (u8::is_ascii_hexdigit, b'p')
    } else {
        (u8::is_ascii_digit, b'e')
    };
    let mut seen = false;
    while s.get(at).is_some_and(digit) {
        at += 1;
        seen = true;
    }
    if s.get(at) == Some(&b'.') {
        let mut k = at + 1;
        while s.get(k).is_some_and(digit) {
            k += 1;
            seen = true;
        }
        if seen {
            at = k;
        }
    }
    if !seen {
        return None;
    }
    if s.get(at).is_some_and(|c| c.to_ascii_lowercase() == marker) {
        let mut k = at + 1;
        if matches!(s.get(k), Some(b'+' | b'-')) {
            k += 1;
        }
        if s.get(k).is_some_and(u8::is_ascii_digit) {
            while s.get(k).is_some_and(u8::is_ascii_digit) {
                k += 1;
            }
            at = k;
        }
    }
    Some((start, at))
}

/// A hex number C reads, `0x` its digits, a point among them, `p` and a power of two, as a
/// double; none where its digits pass the 60 bits held exactly.
fn hex_float(t: &str) -> Option<f64> {
    let (negative, t) = match t.as_bytes()[0] {
        b'-' => (true, &t[1..]),
        b'+' => (false, &t[1..]),
        _ => (false, t),
    };
    let t = &t[2..];
    let (mantissa, power) = match t.find(['p', 'P']) {
        Some(i) => (&t[..i], t[i + 1..].parse::<i32>().ok()?),
        None => (t, 0),
    };
    let (whole, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits = format!("{whole}{frac}");
    let digits = digits.trim_start_matches('0');
    if digits.len() > 15 {
        return None;
    }
    let m = if digits.is_empty() {
        0
    } else {
        u64::from_str_radix(digits, 16).ok()?
    };
    let v = m as f64 * 2f64.powi(power - 4 * frac.len() as i32);
    Some(if negative { -v } else { v })
}

/// A real or a double precision as PostgreSQL's `float4in` or `float8in` reads one: white space,
/// what `strtod` reads, white space; refused where its digits are not zero and it reads as zero or
/// as an infinity. Its bits, a real's in the low 32.
pub fn float_parse(s: &str, wide: bool) -> Result<u64> {
    let name = if wide { "double precision" } else { "real" };
    let wrong = || Error::new(format!("invalid input syntax for type {name}: \"{s}\""));
    let b = s.as_bytes();
    let (start, end) = strtod_prefix(b).ok_or_else(wrong)?;
    if !b[end..].iter().all(|&c| is_c_space(c)) {
        return Err(wrong());
    }
    let t = &s[start..end];
    let body = t.trim_start_matches(['+', '-']);
    let negative = t.starts_with('-');
    let lower = body.to_ascii_lowercase();
    let range = || Error::new(format!("\"{t}\" is out of range for type {name}"));
    // Its digits before any exponent, all of them zero or not.
    let mantissa = match lower.strip_prefix("0x") {
        Some(h) => h.split('p').next().unwrap_or(""),
        None => lower.split('e').next().unwrap_or(""),
    };
    let zero_digits = !mantissa.bytes().any(|c| c.is_ascii_hexdigit() && c != b'0');
    let special = lower.starts_with("inf") || lower.starts_with("nan");
    let sign = |v: f64| if negative { -v } else { v };
    let v: f64 = if lower.starts_with("nan") {
        sign(f64::NAN)
    } else if lower.starts_with("inf") {
        sign(f64::INFINITY)
    } else if lower.starts_with("0x") {
        hex_float(t).ok_or_else(wrong)?
    } else if wide {
        t.parse::<f64>().map_err(|_| wrong())?
    } else {
        let f: f32 = t.parse().map_err(|_| wrong())?;
        if !special && (f.is_infinite() || (f == 0.0 && !zero_digits)) {
            return Err(range());
        }
        return Ok(u64::from(f.to_bits()));
    };
    if wide {
        if !special && (v.is_infinite() || (v == 0.0 && !zero_digits)) {
            return Err(range());
        }
        Ok(v.to_bits())
    } else {
        let f = v as f32;
        if !special && (f.is_infinite() || (f == 0.0 && !zero_digits)) {
            return Err(range());
        }
        // A NaN keeps its sign, as strtof gives it.
        let f = if v.is_nan() {
            f32::from_bits(f32::NAN.to_bits() | (u32::from(negative) << 31))
        } else {
            f
        };
        Ok(u64::from(f.to_bits()))
    }
}

/// Whether the decimal `digits` × 10^(`exp` - its length + 1) reads back as the float `bits` and lies
/// strictly inside the values that do: read a hair above and a hair below, it still does.
fn strictly_inside(digits: &str, exp: i32, bits: u64, wide: bool) -> bool {
    let at = exp - digits.len() as i32 + 1;
    let reads = |m: &str, e: i32| {
        let t = format!("{m}e{e}");
        if wide {
            t.parse::<f64>().map(f64::to_bits).ok() == Some(bits)
        } else {
            t.parse::<f32>().map(|f| u64::from(f.to_bits())).ok() == Some(bits)
        }
    };
    let m: u64 = digits.parse().unwrap_or(0);
    reads(digits, at)
        && reads(&format!("{digits}{}1", "0".repeat(20)), at - 21)
        && (m == 0 || reads(&format!("{}{}", m - 1, "9".repeat(21)), at - 21))
}

/// A float's shortest digits as PostgreSQL's Ryu gives them, which never writes the exact midpoint
/// between two floats: the shortest digits strictly inside the values that read back as it, the
/// nearest of their length. Its sign, its digits and the exponent of its first.
fn pg_shortest(e: String, bits: u64, wide: bool) -> (bool, String, i32) {
    let (negative, d, exp) = shortest(e);
    let magnitude = if wide {
        bits & !(1 << 63)
    } else {
        bits & 0x7fff_ffff
    };
    let v = if wide {
        f64::from_bits(magnitude)
    } else {
        f64::from(f32::from_bits(magnitude as u32))
    };
    if magnitude == 0 {
        return (negative, d, exp);
    }
    if strictly_inside(&d, exp, magnitude, wide) {
        let (d, exp) = half_even(d, exp, magnitude, wide, v);
        return (negative, d, exp);
    }
    for n in d.len() + 1..=17 {
        let (_, d, exp) = shortest(format!("{:.*e}", n - 1, v));
        if strictly_inside(&d, exp, magnitude, wide) {
            return (negative, d, exp);
        }
    }
    (negative, d, exp)
}

/// The digits of `v` exactly halfway between two decimals of `d`'s length, where both read back as
/// it, rounded half to even as PostgreSQL's Ryu rounds them; any other `d` as it is.
fn half_even(d: String, exp: i32, magnitude: u64, wide: bool, v: f64) -> (String, i32) {
    let n = d.len();
    let exact = format!("{:.*e}", n + 59, v);
    let (m, x) = exact.split_once('e').expect("an exponent form");
    let digits: String = m.chars().filter(char::is_ascii_digit).collect();
    let x: i32 = x.parse().expect("an exponent");
    let tie = digits.as_bytes()[n] == b'5' && digits[n + 1..].bytes().all(|b| b == b'0');
    if !tie {
        return (d, exp);
    }
    let low = digits[..n].to_string();
    let mut high: Vec<u8> = low.clone().into_bytes();
    let mut i = n;
    let mut x_high = x;
    loop {
        if i == 0 {
            high.insert(0, b'1');
            high.pop();
            x_high += 1;
            break;
        }
        i -= 1;
        if high[i] == b'9' {
            high[i] = b'0';
        } else {
            high[i] += 1;
            break;
        }
    }
    let high = String::from_utf8(high).expect("digits");
    let trim = |s: &str| {
        let t = s.trim_end_matches('0');
        if t.is_empty() {
            "0".to_string()
        } else {
            t.to_string()
        }
    };
    let even = |s: &str| (s.as_bytes()[n - 1] - b'0').is_multiple_of(2);
    let (lo_in, hi_in) = (
        strictly_inside(&trim(&low), x, magnitude, wide),
        strictly_inside(&trim(&high), x_high, magnitude, wide),
    );
    match (lo_in, hi_in) {
        (true, true) if even(&low) => (trim(&low), x),
        (true, true) => (trim(&high), x_high),
        (true, false) => (trim(&low), x),
        (false, true) => (trim(&high), x_high),
        (false, false) => (d, exp),
    }
}

/// A value's shortest digits and the exponent of its first, from Rust's exponent form.
fn shortest(e: String) -> (bool, String, i32) {
    let (m, x) = e.split_once('e').expect("an exponent form");
    let negative = m.starts_with('-');
    let digits: String = m.chars().filter(char::is_ascii_digit).collect();
    let digits = digits.trim_end_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };
    (
        negative,
        digits.to_string(),
        x.parse().expect("an exponent"),
    )
}

/// A real or a double precision as PostgreSQL's `float4out` or `float8out` writes one with
/// `extra_float_digits` at 1: NaN, Infinity and -Infinity by name; else its shortest digits, laid
/// out in full where the first's exponent is from -4 up to 6 for a real or 15 for a double, or as
/// a digit, a point, the rest and an exponent of at least two digits.
pub fn float_text(bits: u64, wide: bool) -> String {
    let (nan, inf, neg_inf, e, limit) = if wide {
        let v = f64::from_bits(bits);
        (
            v.is_nan(),
            v == f64::INFINITY,
            v == f64::NEG_INFINITY,
            format!("{v:e}"),
            15,
        )
    } else {
        let v = f32::from_bits(bits as u32);
        (
            v.is_nan(),
            v == f32::INFINITY,
            v == f32::NEG_INFINITY,
            format!("{v:e}"),
            6,
        )
    };
    if nan {
        return "NaN".into();
    }
    if inf {
        return "Infinity".into();
    }
    if neg_inf {
        return "-Infinity".into();
    }
    let (negative, d, exp) = pg_shortest(e, bits, wide);
    let sign = if negative { "-" } else { "" };
    if (-4..limit).contains(&exp) {
        let body = if exp >= 0 {
            let whole = exp as usize + 1;
            if d.len() <= whole {
                format!("{d}{}", "0".repeat(whole - d.len()))
            } else {
                format!("{}.{}", &d[..whole], &d[whole..])
            }
        } else {
            format!("0.{}{d}", "0".repeat((-exp - 1) as usize))
        };
        return format!("{sign}{body}");
    }
    let rest = if d.len() > 1 {
        format!(".{}", &d[1..])
    } else {
        String::new()
    };
    format!(
        "{sign}{}{rest}e{}{:02}",
        &d[..1],
        if exp < 0 { '-' } else { '+' },
        exp.abs()
    )
}

/// A double precision as PostgreSQL's `float8_numeric` or `float4_numeric` gives it to numeric:
/// the text of `%.*g` with 15 or 6 significant digits.
pub fn float_numeric_text(v: f64, wide: bool) -> String {
    let digits = if wide { 15 } else { 6 };
    let e = format!("{:.*e}", digits - 1, v);
    let (m, x) = e.split_once('e').unwrap();
    let exp: i32 = x.parse().unwrap();
    let negative = m.starts_with('-');
    let d: String = m.chars().filter(char::is_ascii_digit).collect();
    let d = d.trim_end_matches('0');
    let d = if d.is_empty() { "0" } else { d };
    let sign = if negative { "-" } else { "" };
    if exp < -4 || exp >= digits as i32 {
        let rest = if d.len() > 1 {
            format!(".{}", &d[1..])
        } else {
            String::new()
        };
        return format!(
            "{sign}{}{rest}e{}{:02}",
            &d[..1],
            if exp < 0 { '-' } else { '+' },
            exp.abs()
        );
    }
    if exp >= 0 {
        let whole = exp as usize + 1;
        if d.len() <= whole {
            format!("{sign}{d}{}", "0".repeat(whole - d.len()))
        } else {
            format!("{sign}{}.{}", &d[..whole], &d[whole..])
        }
    } else {
        format!("{sign}0.{}{d}", "0".repeat((-exp - 1) as usize))
    }
}

/// The keys of the reals either side of a double precision `c`, where no real is `c` itself: the
/// greatest below it and the least above it. None where a real is `c`.
pub fn float4_between(c: f64) -> Option<(i64, i64)> {
    if c.is_nan() {
        return None;
    }
    let f = c as f32;
    if f as f64 == c {
        return None;
    }
    let up = |f: f32| {
        f32::from_bits(if f >= 0.0 {
            if f == 0.0 {
                1
            } else {
                f.to_bits() + 1
            }
        } else {
            f.to_bits() - 1
        })
    };
    let down = |f: f32| -up(-f);
    let (below, above) = if (f as f64) < c {
        (f, up(f))
    } else {
        (down(f), f)
    };
    Some((float4_key(below.to_bits()), float4_key(above.to_bits())))
}

/// The bits a timetz's zone takes below its time in its key, the zone counted from the least.
const ZONE_BITS: u32 = 17;

/// A timetz's key: its time on the UTC clock, then its zone in seconds west of UTC, so that it
/// sorts as PostgreSQL compares a timetz.
pub fn timetz_key(time: i64, zone: i32) -> i64 {
    let zone = i64::from(zone);
    ((time + zone * 1_000_000) << ZONE_BITS) | (zone + (1 << (ZONE_BITS - 1)))
}

/// A timetz's time and zone, from its key.
pub fn timetz_parts(key: i64) -> (i64, i32) {
    let (utc, zone) = timetz_utc(key);
    let zone = zone - (1 << (ZONE_BITS - 1));
    (utc - zone * 1_000_000, zone as i32)
}

/// A timetz's time on the UTC clock and its zone counted from the least, from its key.
pub fn timetz_utc(key: i64) -> (i64, i64) {
    (key >> ZONE_BITS, key & ((1 << ZONE_BITS) - 1))
}

thread_local! {
    /// The clocks of the round under way on this thread, in microseconds since 1970 UTC: where its
    /// transaction began, and where its statement began.
    static ROUND: std::cell::Cell<Option<(i64, i64)>> = const { std::cell::Cell::new(None) };
}

/// The clock now() and a literal's `now`, `today`, `tomorrow` and `yesterday` read: where the
/// transaction of the round under way began, as PostgreSQL's transaction timestamp; the machine's
/// where no round is under way.
pub(crate) fn transaction_clock() -> i64 {
    ROUND.with(|c| c.get()).map_or_else(wall_clock, |(t, _)| t)
}

/// The clock statement_timestamp() reads: where the statement of the round under way began.
pub(crate) fn statement_clock() -> i64 {
    ROUND.with(|c| c.get()).map_or_else(wall_clock, |(_, s)| s)
}

/// A round's clocks, read by all the round evaluates while it is held; the clocks before it come
/// back where it is dropped.
pub(crate) struct Clocked(Option<(i64, i64)>);

/// Holds the clocks of a round, its transaction's and its statement's.
pub(crate) fn clocked(transaction: i64, statement: i64) -> Clocked {
    Clocked(ROUND.with(|c| c.replace(Some((transaction, statement)))))
}

impl Drop for Clocked {
    fn drop(&mut self) {
        ROUND.with(|c| c.set(self.0));
    }
}

/// The machine's clock now, in microseconds since 1970 UTC: what clock_timestamp() reads.
pub fn wall_clock() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

/// A key of a type keyed as a number as PostgreSQL's output function writes the value.
pub fn scalar_text(ty: ColumnType, key: i64) -> String {
    match ty {
        ColumnType::Oid
        | ColumnType::Xid
        | ColumnType::Cid
        | ColumnType::Xid8
        | ColumnType::Oid8 => unsigned_of(key, ty.unsigned_bits()).to_string(),
        ColumnType::Reg(kind, release) => {
            crate::regs::reg_text(kind, unsigned_of(key, 32) as u32, release)
        }
        ColumnType::PgLsn => lsn_out(unsigned_of(key, 64)),
        ColumnType::Char => char_out(unsigned_of(key, 8) as u8),
        ColumnType::Money(m) => crate::monetary::cash_out(key, m.conv()),
        ColumnType::Macaddr => mac_out(unsigned_of(key, 48), 6),
        ColumnType::Macaddr8 => mac_out(unsigned_of(key, 64), 8),
        ColumnType::Tid => tid_out(key),
        ColumnType::Date => crate::times::date_out(key as i32),
        ColumnType::Time(_) => crate::times::time_out(key),
        ColumnType::Timestamp(_) => crate::times::timestamp_out(key),
        ColumnType::Timestamptz | ColumnType::TimestamptzRounded(_) => {
            crate::times::timestamptz_out(key)
        }
        ColumnType::Timetz(_) => {
            let (t, z) = timetz_parts(key);
            crate::times::timetz_out(t, z)
        }
        _ => key.to_string(),
    }
}

/// A value of a type keyed as a number, read from its text as PostgreSQL's input function reads
/// it, as its key.
pub fn scalar_parse(ty: ColumnType, s: &str) -> Result<i64> {
    Ok(match ty {
        ColumnType::Oid
        | ColumnType::Xid
        | ColumnType::Cid
        | ColumnType::Xid8
        | ColumnType::Oid8 => {
            let bits = ty.unsigned_bits();
            unsigned_key(unsigned_in(s, bits, &ty.pg_name())?, bits)
        }
        ColumnType::Reg(kind, release) => {
            unsigned_key(u64::from(crate::regs::reg_parse(kind, s, release)?), 32)
        }
        ColumnType::PgLsn => unsigned_key(lsn_in(s)?, 64),
        ColumnType::Char => unsigned_key(u64::from(char_in(s)), 8),
        ColumnType::Money(m) => crate::monetary::cash_in(s, m.conv())?,
        ColumnType::Macaddr => unsigned_key(mac_in(s)?, 48),
        ColumnType::Macaddr8 => unsigned_key(mac8_in(s)?, 64),
        ColumnType::Tid => tid_in(s)?,
        ColumnType::Date => i64::from(crate::times::date_in(s, transaction_clock())?),
        ColumnType::Time(p) => crate::times::time_in(s, p.map(u32::from), transaction_clock())?,
        ColumnType::Timestamp(p) => {
            crate::times::timestamp_in(s, p.map(u32::from), transaction_clock())?
        }
        ColumnType::Timestamptz => crate::times::timestamptz_in(s, None, transaction_clock())?,
        ColumnType::TimestamptzRounded(p) => {
            crate::times::timestamptz_in(s, Some(u32::from(p)), transaction_clock())?
        }
        ColumnType::Timetz(p) => {
            let (t, z) = crate::times::timetz_in(s, p.map(u32::from), transaction_clock())?;
            timetz_key(t, z)
        }
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

/// The key of an answer's field metadata that names the PostgreSQL release whose catalog names a
/// reg* column's OIDs.
pub const PG_RELEASE: &str = "pg_release";

/// The key of an answer's field metadata that names the lc_monetary a money column reads and
/// prints under, where it is not C.
pub const PG_MONETARY: &str = "lc_monetary";

/// A tid's two leaves as arrow's Parquet reader gives them: its block number and its offset.
fn tid_fields() -> arrow::datatypes::Fields {
    use arrow::datatypes::{DataType as A, Field};
    vec![
        Field::new("block", A::UInt32, false),
        Field::new("offset", A::UInt16, false),
    ]
    .into()
}
/// A timetz's two leaves as arrow's Parquet reader gives them: its time and its zone.
fn timetz_fields() -> arrow::datatypes::Fields {
    use arrow::datatypes::{DataType as A, Field, TimeUnit};
    vec![
        Field::new("time", A::Time64(TimeUnit::Microsecond), false),
        Field::new("zone", A::Int32, false),
    ]
    .into()
}

/// An interval's three fields, as an answer holds them.
fn interval_fields() -> arrow::datatypes::Fields {
    use arrow::datatypes::{DataType as A, Field};
    vec![
        Field::new("months", A::Int32, false),
        Field::new("days", A::Int32, false),
        Field::new("micros", A::Int64, false),
    ]
    .into()
}

/// A column of intervals as an answer holds it: a struct of its three fields.
pub fn interval_array(
    values: impl Iterator<Item = Option<crate::times::Interval>>,
) -> arrow::array::ArrayRef {
    use arrow::array::{Int32Array, Int64Array, StructArray};
    use std::sync::Arc;
    let values: Vec<Option<crate::times::Interval>> = values.collect();
    let at = |f: fn(&crate::times::Interval) -> i64| -> Vec<i64> {
        values.iter().map(|v| v.as_ref().map_or(0, f)).collect()
    };
    let months: Int32Array = at(|v| v.months.into())
        .into_iter()
        .map(|x| x as i32)
        .collect();
    let days: Int32Array = at(|v| v.days.into())
        .into_iter()
        .map(|x| x as i32)
        .collect();
    let micros: Int64Array = at(|v| v.micros).into_iter().collect();
    Arc::new(
        StructArray::try_new(
            interval_fields(),
            vec![Arc::new(months), Arc::new(days), Arc::new(micros)],
            Some(values.iter().map(Option::is_some).collect()),
        )
        .expect("an interval takes its three fields"),
    )
}

/// Row `r`'s interval in a column an answer holds; none for a NULL.
pub fn interval_at(a: &dyn arrow::array::Array, r: usize) -> Option<crate::times::Interval> {
    use arrow::array::{Int32Array, Int64Array, StructArray};
    if a.is_null(r) {
        return None;
    }
    let s = a.as_any().downcast_ref::<StructArray>()?;
    let i32_of = |n: &str| {
        s.column_by_name(n)?
            .as_any()
            .downcast_ref::<Int32Array>()
            .map(|x| x.value(r))
    };
    Some(crate::times::Interval {
        months: i32_of("months")?,
        days: i32_of("days")?,
        micros: s
            .column_by_name("micros")?
            .as_any()
            .downcast_ref::<Int64Array>()?
            .value(r),
    })
}

/// The Arrow type an answer holds a scalar of type `ty` as: what arrow's Parquet reader gives its
/// page form.
pub fn scalar_arrow(ty: ColumnType) -> arrow::datatypes::DataType {
    use arrow::datatypes::{DataType as A, TimeUnit};
    match ty {
        ColumnType::Tid => return A::Struct(tid_fields()),
        ColumnType::Timetz(_) => return A::Struct(timetz_fields()),
        ColumnType::Interval { .. } => return A::Struct(interval_fields()),
        ColumnType::Date => return A::Date32,
        ColumnType::Time(_) => return A::Time64(TimeUnit::Microsecond),
        ColumnType::Timestamp(_) => return A::Timestamp(TimeUnit::Microsecond, None),
        ColumnType::Timestamptz | ColumnType::TimestamptzRounded(_) => {
            return A::Timestamp(TimeUnit::Microsecond, Some("UTC".into()))
        }
        _ => {}
    }
    if let Some(n) = ty.fixed_bytes() {
        return A::FixedSizeBinary(n as i32);
    }
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
    use arrow::array::{FixedSizeBinaryArray, Int64Array, UInt32Array, UInt64Array, UInt8Array};
    use std::sync::Arc;
    match ty {
        ColumnType::Date => {
            return Arc::new(
                keys.map(|k| k.map(|k| k as i32))
                    .collect::<arrow::array::Date32Array>(),
            )
        }
        ColumnType::Time(_) => {
            return Arc::new(keys.collect::<arrow::array::Time64MicrosecondArray>())
        }
        ColumnType::Timestamp(_) => {
            return Arc::new(keys.collect::<arrow::array::TimestampMicrosecondArray>())
        }
        ColumnType::Timestamptz | ColumnType::TimestamptzRounded(_) => {
            return Arc::new(
                keys.collect::<arrow::array::TimestampMicrosecondArray>()
                    .with_timezone("UTC"),
            )
        }
        ColumnType::Timetz(_) => {
            let keys: Vec<Option<i64>> = keys.collect();
            let parts = |k: &Option<i64>| k.map_or((0, 0), timetz_parts);
            let times: arrow::array::Time64MicrosecondArray =
                keys.iter().map(|k| Some(parts(k).0)).collect();
            let zones: arrow::array::Int32Array = keys.iter().map(|k| Some(parts(k).1)).collect();
            return Arc::new(
                arrow::array::StructArray::try_new(
                    timetz_fields(),
                    vec![Arc::new(times), Arc::new(zones)],
                    Some(keys.iter().map(Option::is_some).collect()),
                )
                .expect("a timetz takes its two leaves"),
            );
        }
        _ => {}
    }
    if ty == ColumnType::Tid {
        let keys: Vec<Option<i64>> = keys.collect();
        let parts = |k: &Option<i64>| k.map_or((0, 0), tid_parts);
        let blocks: UInt32Array = keys.iter().map(|k| Some(parts(k).0)).collect();
        let offsets: arrow::array::UInt16Array = keys.iter().map(|k| Some(parts(k).1)).collect();
        return Arc::new(
            arrow::array::StructArray::try_new(
                tid_fields(),
                vec![Arc::new(blocks), Arc::new(offsets)],
                Some(keys.iter().map(Option::is_some).collect()),
            )
            .expect("a tid takes its two leaves"),
        );
    }
    if let Some(n) = ty.fixed_bytes() {
        return Arc::new(
            FixedSizeBinaryArray::try_from_sparse_iter_with_size(
                keys.map(|k| k.map(|k| fixed_of_key(ty, k))),
                n as i32,
            )
            .expect("a MAC address takes its bytes"),
        );
    }
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
    use arrow::array::{FixedSizeBinaryArray, Int64Array, UInt32Array, UInt64Array, UInt8Array};
    if a.is_null(r) {
        return None;
    }
    let any = a.as_any();
    match ty {
        ColumnType::Date => {
            return Some(i64::from(
                any.downcast_ref::<arrow::array::Date32Array>()?.value(r),
            ))
        }
        ColumnType::Time(_) => {
            return Some(
                any.downcast_ref::<arrow::array::Time64MicrosecondArray>()?
                    .value(r),
            )
        }
        ColumnType::Timestamp(_) | ColumnType::Timestamptz | ColumnType::TimestamptzRounded(_) => {
            return Some(
                any.downcast_ref::<arrow::array::TimestampMicrosecondArray>()?
                    .value(r),
            )
        }
        ColumnType::Timetz(_) => {
            let s = any.downcast_ref::<arrow::array::StructArray>()?;
            let time = s
                .column_by_name("time")?
                .as_any()
                .downcast_ref::<arrow::array::Time64MicrosecondArray>()?;
            let zone = s
                .column_by_name("zone")?
                .as_any()
                .downcast_ref::<arrow::array::Int32Array>()?;
            return Some(timetz_key(time.value(r), zone.value(r)));
        }
        _ => {}
    }
    if ty == ColumnType::Tid {
        let s = any.downcast_ref::<arrow::array::StructArray>()?;
        let block = s
            .column_by_name("block")?
            .as_any()
            .downcast_ref::<UInt32Array>()?;
        let offset = s
            .column_by_name("offset")?
            .as_any()
            .downcast_ref::<arrow::array::UInt16Array>()?;
        return Some(tid_key(block.value(r), offset.value(r)));
    }
    if ty.fixed_bytes().is_some() {
        return Some(key_of_fixed(
            ty,
            any.downcast_ref::<FixedSizeBinaryArray>()?.value(r),
        ));
    }
    Some(match ty.unsigned_bits() {
        8 => unsigned_key(u64::from(any.downcast_ref::<UInt8Array>()?.value(r)), 8),
        32 => unsigned_key(u64::from(any.downcast_ref::<UInt32Array>()?.value(r)), 32),
        64 => unsigned_key(any.downcast_ref::<UInt64Array>()?.value(r), 64),
        _ => any.downcast_ref::<Int64Array>()?.value(r),
    })
}

/// An answer's field, named for a scalar's PostgreSQL type in its metadata.
pub fn scalar_field(name: &str, ty: ColumnType, nullable: bool) -> arrow::datatypes::Field {
    // An interval's answer is named by its type alone, whatever fields its column keeps.
    let ty = match ty {
        ColumnType::Interval { .. } => ColumnType::Interval {
            range: crate::times::INTERVAL_FULL_RANGE,
            precision: None,
        },
        ty => ty,
    };
    let mut named = vec![(PG_TYPE.to_string(), ty.pg_name())];
    if let ColumnType::Reg(_, release) = ty {
        named.push((PG_RELEASE.to_string(), release.to_string()));
    }
    if let ColumnType::Money(m) = ty {
        if !m.is_c() {
            named.push((PG_MONETARY.to_string(), m.name()));
        }
    }
    arrow::datatypes::Field::new(name, scalar_arrow(ty), nullable)
        .with_metadata(named.into_iter().collect())
}

/// The scalar type an answer's field holds, by its metadata.
pub fn field_scalar(f: &arrow::datatypes::Field) -> Option<ColumnType> {
    let named = f.metadata().get(PG_TYPE)?;
    if let Some(kind) = crate::regs::RegKind::of_type_name(named) {
        let release = f
            .metadata()
            .get(PG_RELEASE)
            .and_then(|r| r.parse().ok())
            .unwrap_or(crate::regs::LATEST);
        let ty = ColumnType::Reg(kind, release);
        return (scalar_arrow(ty) == *f.data_type()).then_some(ty);
    }
    if named == "money" {
        let m = match f.metadata().get(PG_MONETARY) {
            Some(l) => crate::monetary::Monetary::open(l).ok()?,
            None => crate::monetary::Monetary::C,
        };
        let ty = ColumnType::Money(m);
        return (scalar_arrow(ty) == *f.data_type()).then_some(ty);
    }
    [
        ColumnType::Oid,
        ColumnType::Xid,
        ColumnType::Cid,
        ColumnType::Xid8,
        ColumnType::PgLsn,
        ColumnType::Char,
        ColumnType::Macaddr,
        ColumnType::Macaddr8,
        ColumnType::Oid8,
        ColumnType::Tid,
        ColumnType::Date,
    ]
    .into_iter()
    .chain(
        [
            None,
            Some(0),
            Some(1),
            Some(2),
            Some(3),
            Some(4),
            Some(5),
            Some(6),
        ]
        .into_iter()
        .flat_map(|p| {
            [
                ColumnType::Time(p),
                ColumnType::Timetz(p),
                ColumnType::Timestamp(p),
            ]
        }),
    )
    .chain(std::iter::once(ColumnType::Interval {
        range: crate::times::INTERVAL_FULL_RANGE,
        precision: None,
    }))
    .chain(std::iter::once(ColumnType::Timestamptz))
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
            | ColumnType::Money(_)
            | ColumnType::Macaddr
            | ColumnType::Macaddr8
            | ColumnType::Reg(..)
            | ColumnType::Oid8
            | ColumnType::Tid
            | ColumnType::Date
            | ColumnType::Time(_)
            | ColumnType::Timetz(_)
            | ColumnType::Timestamp(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    const MONEY: ColumnType = ColumnType::Money(crate::monetary::Monetary::C);

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
    fn every_real_s_key_sorts_as_postgresql_orders_reals() {
        // Every bit pattern, walked from -Infinity up through the negatives, -0, 0 and the positives
        // to Infinity: each key rises as the machine's own < does, -0 and 0 share one, and every NaN
        // lies on one key above Infinity.
        let walk = (0x8000_0001u32..=0xff80_0000)
            .rev()
            .chain([0x8000_0000, 0])
            .chain(1..=0x7f80_0000);
        let (mut last, mut last_key, mut walked) = (f32::NEG_INFINITY, i64::MIN, 0u64);
        for b in walk {
            let (f, k) = (f32::from_bits(b), float4_key(b));
            assert!(
                (f > last && k > last_key) || (f == last && k == last_key) || walked == 0,
                "{b:#x}"
            );
            (last, last_key, walked) = (f, k, walked + 1);
        }
        assert_eq!(walked, (1u64 << 32) - 2 * ((1 << 23) - 1));
        let nan = float4_key(0x7fc0_0000);
        assert!(nan > float4_key(0x7f80_0000));
        let nans = (0x7f80_0001u32..=0x7fff_ffff).chain(0xff80_0001..=0xffff_ffff);
        assert!(nans.into_iter().all(|b| float4_key(b) == nan));
        // Every key lies in the signed range of 32 bits, as the cut takes it.
        assert!((i64::from(i32::MIN)..=i64::from(i32::MAX)).contains(&nan));
        assert_eq!(float4_key(0xff80_0000), i64::from(i32::MIN) + 0x007f_ffff);
    }

    #[test]
    fn a_double_s_key_sorts_as_postgresql_orders_doubles_at_its_boundaries() {
        let ordered = [
            f64::NEG_INFINITY,
            f64::MIN,
            -1.0,
            -f64::MIN_POSITIVE,
            -5e-324,
            0.0,
            5e-324,
            f64::MIN_POSITIVE,
            1.0,
            f64::MAX,
            f64::INFINITY,
        ];
        for w in ordered.windows(2) {
            assert!(
                float8_key(w[0].to_bits()) < float8_key(w[1].to_bits()),
                "{w:?}"
            );
        }
        assert_eq!(
            float8_key((-0.0f64).to_bits()),
            float8_key(0.0f64.to_bits())
        );
        let nan = float8_key(f64::NAN.to_bits());
        assert!(nan > float8_key(f64::INFINITY.to_bits()));
        for b in [
            0x7ff0_0000_0000_0001u64,
            0xfff8_0000_0000_0000,
            0x7fff_ffff_ffff_ffff,
            0xffff_ffff_ffff_ffff,
        ] {
            assert_eq!(float8_key(b), nan);
        }
    }

    #[test]
    fn a_float_is_read_and_written_as_postgresql_reads_and_writes_it() {
        let real = |s: &str| float_parse(s, false).map(|b| float_text(b, false));
        let double = |s: &str| float_parse(s, true).map(|b| float_text(b, true));
        for (s, want) in [
            ("0.1", "0.1"),
            ("3.4028235e+38", "3.4028235e+38"),
            ("-0", "-0"),
            ("NaN", "NaN"),
            ("-NaN", "NaN"),
            ("-INF", "-Infinity"),
            ("  1.5  ", "1.5"),
            ("1e-45", "1e-45"),
            ("1.5e-45", "1e-45"),
            ("16777217", "1.6777216e+07"),
            ("123456", "123456"),
            ("1234567", "1.234567e+06"),
            ("0.0001", "0.0001"),
            ("0.00001", "1e-05"),
            ("495232.625", "495232.62"),
        ] {
            assert_eq!(real(s).unwrap(), want, "{s}");
        }
        for (s, want) in [
            ("0.30000000000000004", "0.30000000000000004"),
            ("123456789012345678", "1.2345678901234568e+17"),
            ("123456789012345", "123456789012345"),
            ("1e15", "1e+15"),
            ("1e-7", "1e-07"),
            ("4.9e-324", "5e-324"),
            ("0x10", "16"),
            ("inf", "Infinity"),
            ("nan(1)", "NaN"),
            ("1e-320", "1e-320"),
            // The double nearest 1e23 lies exactly between it and the next, and PostgreSQL never
            // writes such a midpoint.
            ("1e23", "9.999999999999999e+22"),
            // Exactly halfway between two decimals of its shortest length, rounded half to even.
            ("214988023257504.63", "214988023257504.62"),
        ] {
            assert_eq!(double(s).unwrap(), want, "{s}");
        }
        for s in ["", "1.5x", "1e400", "1e-400", "infinit", "1e", "--1", " "] {
            assert!(double(s).is_err(), "{s}");
        }
        assert!(real("1e39").is_err() && real("1e-46").is_err());
        // -NaN keeps its sign bit, as strtod gives it.
        assert_eq!(float_parse("-NaN", true).unwrap(), 0xfff8_0000_0000_0000);
        // No real is 0.1 as a double, so it lies between two.
        let (below, above) = float4_between(0.1).unwrap();
        assert_eq!(above, below + 1);
        assert_eq!(float4_between(1.5), None);
    }

    #[test]
    fn a_mac_address_is_read_and_written_as_postgresql_reads_and_writes_it() {
        let mac = |s: &str| {
            scalar_parse(ColumnType::Macaddr, s).map(|k| scalar_text(ColumnType::Macaddr, k))
        };
        for s in [
            "08:00:2b:01:02:03",
            "08002b:010203",
            "08-00-2B-01-02-03",
            "0800.2b01.0203",
            "08002b010203",
            "0800-2b01-0203",
            " 8:0:2b:1:2:3 ",
            "0x8:0:2b:1:2:3",
        ] {
            assert_eq!(mac(s).unwrap(), "08:00:2b:01:02:03", "{s}");
        }
        for s in [
            "08:00:2b:01:02",
            "100:00:2b:01:02:03",
            "08:00:2b:01:02:03x",
            "-1:0:0:0:0:0",
            "",
        ] {
            assert!(mac(s).is_err(), "{s}");
        }
        let mac8 = |s: &str| {
            scalar_parse(ColumnType::Macaddr8, s).map(|k| scalar_text(ColumnType::Macaddr8, k))
        };
        for (s, want) in [
            ("08002b0102030405", "08:00:2b:01:02:03:04:05"),
            ("08-00-2b-01-02-03", "08:00:2b:ff:fe:01:02:03"),
            ("0800.2bff.fe01.0203", "08:00:2b:ff:fe:01:02:03"),
            (" 08:00:2b:01:02:03:04:05 ", "08:00:2b:01:02:03:04:05"),
            ("08:00:2b:01:02:03:04:05x", "08:00:2b:01:02:03:04:05"),
        ] {
            assert_eq!(mac8(s).unwrap(), want, "{s}");
        }
        for s in [
            "08:00:2b:01:02",
            "08:00-2b:01:02:03",
            "08:00:2b:01:02:03:04:05:06",
            "08:00:2b:01:02:03 x",
        ] {
            assert!(mac8(s).is_err(), "{s}");
        }
        let six = scalar_parse(ColumnType::Macaddr, "08:00:2b:01:02:03").unwrap();
        let eight = mac_cast(ColumnType::Macaddr, ColumnType::Macaddr8, six).unwrap();
        assert_eq!(
            scalar_text(ColumnType::Macaddr8, eight),
            "08:00:2b:ff:fe:01:02:03"
        );
        assert_eq!(
            mac_cast(ColumnType::Macaddr8, ColumnType::Macaddr, eight).unwrap(),
            six
        );
        let other = scalar_parse(ColumnType::Macaddr8, "08:00:2b:01:02:03:04:05").unwrap();
        assert!(mac_cast(ColumnType::Macaddr8, ColumnType::Macaddr, other).is_err());
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
        let money = |s: &str| scalar_parse(MONEY, s).map(|k| scalar_text(MONEY, k));
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
    fn a_tid_reads_and_writes_as_tidin_and_tidout_do() {
        // Each spelling as PostgreSQL 18.6 read it (`'…'::tid::text`, 2026-10-30).
        let tid =
            |s: &str| scalar_parse(ColumnType::Tid, s).map(|k| scalar_text(ColumnType::Tid, k));
        for (s, want) in [
            ("(1,2)", "(1,2)"),
            ("(-1,0)", "(4294967295,0)"),
            ("( 1, 2)", "(1,2)"),
            ("(\t1,\n2)", "(1,2)"),
            ("(+1,+2)", "(1,2)"),
            ("(-0,-0)", "(0,0)"),
            ("(-2147483648,0)", "(2147483648,0)"),
            ("(18446744073709551615,0)", "(4294967295,0)"),
            ("(4294967295,65535)", "(4294967295,65535)"),
            ("(010,1)", "(10,1)"),
            ("x(1,2)", "(1,2)"),
            ("(1,2)junk", "(1,2)"),
            ("(1,2) ", "(1,2)"),
            ("(,2)", "(0,2)"),
            ("(1,)", "(1,0)"),
            (",1,2)", "(1,2)"),
        ] {
            assert_eq!(tid(s).unwrap(), want, "{s}");
        }
        for s in [
            "( 1 , 2 )",
            "(4294967296,0)",
            "(-2147483649,0)",
            "(-4294967295,0)",
            "(18446744073709551616,0)",
            "(0,65536)",
            "(0,-1)",
            "(0,99999999999999999999)",
            "(0x10,1)",
            "1,2)",
            "(1,2",
            "((1,2)",
            "(1,2,3)",
            "(1;2)",
            "( ,2)",
            "(-,2)",
            "",
        ] {
            assert!(tid(s).is_err(), "{s}");
        }
        // Its key rises with the block, then the offset, both unsigned.
        let keys: Vec<i64> = [
            (0, 0),
            (0, 1),
            (0, 65535),
            (1, 0),
            (2147483648, 1),
            (4294967295, 65535),
        ]
        .iter()
        .map(|&(b, o)| tid_key(b, o))
        .collect();
        assert!(keys.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(tid_parts(tid_key(4294967295, 65535)), (4294967295, 65535));
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
