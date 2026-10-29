//! The fixed scalars: each one's key, its text as PostgreSQL writes it, and its input as
//! PostgreSQL reads it.
//!
//! A uuid is keyed in 128 bits: its bytes read big-endian, the top bit flipped, so that its keys
//! sort as its bytes do.

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

/// The machine's clock in nanoseconds since 1970 UTC, at least a step past the last a v7 took, so
/// the v7s one writer hands out rise.
fn ns_ascending() -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    let mut prev = PREVIOUS_NS.load(Ordering::Relaxed);
    loop {
        let ns = now.max(prev + SUBMS_MINIMAL_STEP_NS);
        match PREVIOUS_NS.compare_exchange_weak(prev, ns, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return ns,
            Err(p) => prev = p,
        }
    }
}

/// A version 7 uuid, as `uuidv7()` hands one out: the milliseconds since 1970 UTC in 48 bits, the
/// millisecond's fraction in the next 12, the rest random but its version's and its variant's.
pub fn uuid_v7() -> Result<i128> {
    let ns = ns_ascending();
    let (ms, sub) = (ns / NS_PER_MS, ns % NS_PER_MS);
    let mut bytes = [0u8; 16];
    bytes[..6].copy_from_slice(&(ms as u64).to_be_bytes()[2..]);
    let fraction = (sub * (1 << 12) / NS_PER_MS) as u16;
    bytes[6..8].copy_from_slice(&fraction.to_be_bytes());
    random(&mut bytes[8..])?;
    Ok(versioned(bytes, 7))
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }
}
