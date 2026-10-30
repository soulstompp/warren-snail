//! The strings and the bit strings: bpchar, name, bytea, bit and bit varying, each read and written
//! as PostgreSQL's input and output functions read and write it, and each keyed as its comparison
//! orders it. A bit string is held as its key: its bits as a part of varying length, then its
//! length.

use crate::catalog::ColumnType;
use crate::{Error, Result};
use std::borrow::Cow;

/// The most bytes a name holds: PostgreSQL's NAMEDATALEN less its ending zero.
pub const NAME_BYTES: usize = 63;

/// The most bits a bit string holds, as PostgreSQL bounds one.
pub const BITS_MAX: u64 = 2_147_483_640;

/// bytea written as `byteaout` writes it under `bytea_output` hex: `\x` and two digits a byte.
pub fn bytea_out(b: &[u8]) -> String {
    let mut out = String::with_capacity(2 + 2 * b.len());
    out.push_str("\\x");
    for x in b {
        out.push_str(&format!("{x:02x}"));
    }
    out
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// The character starting at byte `at` of `s`, as PostgreSQL quotes one in a refusal.
fn char_at(s: &str, at: usize) -> &str {
    let end = (at + 1..=s.len())
        .find(|&e| s.is_char_boundary(e))
        .unwrap_or(s.len());
    &s[at..end]
}

/// bytea read as `byteain` reads it: after `\x`, two hex digits a byte, white space between them
/// passed over; otherwise each byte as it is, `\\` a backslash and `\` with three octal digits, the
/// first 0 to 3, the byte they make.
pub fn bytea_in(s: &str) -> Result<Vec<u8>> {
    let b = s.as_bytes();
    if let Some(h) = s.strip_prefix("\\x") {
        let hb = h.as_bytes();
        let mut out = Vec::with_capacity(hb.len() / 2);
        let mut i = 0;
        while i < hb.len() {
            if matches!(hb[i], b' ' | b'\n' | b'\t' | b'\r') {
                i += 1;
                continue;
            }
            let bad = |at: usize| {
                Error::new(format!("invalid hexadecimal digit: \"{}\"", char_at(h, at)))
            };
            let hi = hex_digit(hb[i]).ok_or_else(|| bad(i))?;
            i += 1;
            if i >= hb.len() {
                return Err(Error::new("invalid hexadecimal data: odd number of digits"));
            }
            let lo = hex_digit(hb[i]).ok_or_else(|| bad(i))?;
            i += 1;
            out.push(hi << 4 | lo);
        }
        return Ok(out);
    }
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' {
            out.push(b[i]);
            i += 1;
        } else if b.len() >= i + 4
            && (b'0'..=b'3').contains(&b[i + 1])
            && (b'0'..=b'7').contains(&b[i + 2])
            && (b'0'..=b'7').contains(&b[i + 3])
        {
            out.push((b[i + 1] - b'0') << 6 | (b[i + 2] - b'0') << 3 | (b[i + 3] - b'0'));
            i += 4;
        } else if b.get(i + 1) == Some(&b'\\') {
            out.push(b'\\');
            i += 2;
        } else {
            return Err(Error::new("invalid input syntax for type bytea"));
        }
    }
    Ok(out)
}

/// The bytes a bpchar is compared by: its own, its trailing blanks taken off.
pub fn bpchar_key(s: &[u8]) -> &[u8] {
    let n = s.iter().rposition(|&b| b != b' ').map_or(0, |i| i + 1);
    &s[..n]
}

/// A bpchar as the type of `n` characters holds it, as `bpchar_input` reads it, and an implicit
/// cast to it takes it: padded with blanks to `n`, its excess taken off where it is only blanks,
/// else refused; and where `explicit`, as an explicit cast takes it, its excess taken off whatever
/// it is. Without `n`, as it is.
pub fn bpchar_in(s: &str, n: Option<u32>, explicit: bool) -> Result<String> {
    let Some(n) = n else {
        return Ok(s.to_string());
    };
    let n = n as usize;
    let chars = s.chars().count();
    if chars > n {
        let cut = s.char_indices().nth(n).map_or(s.len(), |(i, _)| i);
        if !explicit && s.as_bytes()[cut..].iter().any(|&b| b != b' ') {
            return Err(Error::new(format!(
                "value too long for type character({n})"
            )));
        }
        return Ok(s[..cut].to_string());
    }
    let mut out = String::with_capacity(s.len() + n - chars);
    out.push_str(s);
    out.extend(std::iter::repeat_n(' ', n - chars));
    Ok(out)
}

/// A name as `namein` reads it: cut to 63 bytes on a character's boundary.
pub fn name_in(s: &str) -> String {
    if s.len() <= NAME_BYTES {
        return s.to_string();
    }
    let mut end = NAME_BYTES;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// A text as a string type's text output takes it, cast to text: a bpchar's trailing blanks taken
/// off, any other as it is.
pub fn text_of(ty: ColumnType, s: &str) -> &str {
    match ty {
        ColumnType::Bpchar(_) => std::str::from_utf8(bpchar_key(s.as_bytes())).unwrap_or(s),
        _ => s,
    }
}

/// The key of a bit string of `len` bits: its bits, the last byte padded with zeros, as a part of
/// varying length, then its length in four bytes, big-endian.
pub fn bits_key(len: u32, bits: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bits.len() + 6);
    crate::places::varying_part(&mut out, bits);
    out.extend_from_slice(&len.to_be_bytes());
    out
}

/// A bit string's length and bits, from its key.
pub fn bits_of_key(key: &[u8]) -> (u32, Vec<u8>) {
    let (bits, rest) = crate::places::split_varying(key);
    let len = rest
        .get(..4)
        .map_or(0, |r| u32::from_be_bytes(r.try_into().unwrap()));
    (len, bits)
}

/// Bit `r` of a bit string, from its key: 0 or 1, or -1 past its last.
pub fn bit_of_key(key: &[u8], r: usize) -> i64 {
    // The byte holding bit `r`, read as the key's varying part is walked, then its length.
    let (mut i, mut at, mut byte) = (0, 0, 0u8);
    while i < key.len() {
        let b = key[i];
        if b == 0 && key.get(i + 1) != Some(&0xff) {
            break;
        }
        if at == r / 8 {
            byte = b;
        }
        at += 1;
        i += if b == 0 { 2 } else { 1 };
    }
    let len = key
        .get(i + 2..i + 6)
        .map_or(0, |l| u32::from_be_bytes(l.try_into().unwrap()));
    if r >= len as usize {
        return -1;
    }
    ((byte >> (7 - r % 8)) & 1) as i64
}

/// A bit string's bits written out, a `0` or `1` each, as `bit_out` and `varbit_out` write them.
pub fn bits_out(len: u32, bits: &[u8]) -> String {
    (0..len as usize)
        .map(|r| {
            if (bits[r / 8] >> (7 - r % 8)) & 1 == 1 {
                '1'
            } else {
                '0'
            }
        })
        .collect()
}

/// A bit string's key written out as `bit_out` writes it.
pub fn bits_text(key: &[u8]) -> String {
    let (len, bits) = bits_of_key(key);
    bits_out(len, &bits)
}

/// A bit string read as `bit_in` and `varbit_in` read one: `B` or `b` and binary digits, `X` or `x`
/// and hex digits, or binary digits alone; for bit(`n`) exactly `n` of them, for bit varying(`n`)
/// at most `n`, any without one. Its length and its bits.
pub fn bits_in(s: &str, n: Option<u32>, varying: bool) -> Result<(u32, Vec<u8>)> {
    let (binary, digits) = match s.as_bytes().first() {
        Some(b'b' | b'B') => (true, &s[1..]),
        Some(b'x' | b'X') => (false, &s[1..]),
        _ => (true, s),
    };
    let len = if binary {
        digits.len() as u64
    } else {
        if digits.len() as u64 > BITS_MAX / 4 {
            return Err(Error::new(format!(
                "bit string length exceeds the maximum allowed ({BITS_MAX})"
            )));
        }
        digits.len() as u64 * 4
    };
    match n {
        Some(n) if !varying && len != u64::from(n) => {
            return Err(Error::new(format!(
                "bit string length {len} does not match type bit({n})"
            )))
        }
        Some(n) if varying && len > u64::from(n) => {
            return Err(Error::new(format!(
                "bit string too long for type bit varying({n})"
            )))
        }
        _ => {}
    }
    let mut bits = vec![0u8; (len as usize).div_ceil(8)];
    for (i, (at, c)) in digits.char_indices().enumerate() {
        if binary {
            match c {
                '1' => bits[i / 8] |= 0x80 >> (i % 8),
                '0' => {}
                _ => {
                    return Err(Error::new(format!(
                        "\"{}\" is not a valid binary digit",
                        char_at(digits, at)
                    )))
                }
            }
        } else {
            let x = u8::try_from(c).ok().and_then(hex_digit).ok_or_else(|| {
                Error::new(format!(
                    "\"{}\" is not a valid hexadecimal digit",
                    char_at(digits, at)
                ))
            })?;
            bits[i / 2] |= if i % 2 == 0 { x << 4 } else { x };
        }
    }
    Ok((len as u32, bits))
}

/// A bit string's key, read from its text as the type's input reads it.
pub fn bits_parse(s: &str, n: Option<u32>, varying: bool) -> Result<Vec<u8>> {
    let (len, bits) = bits_in(s, n, varying)?;
    Ok(bits_key(len, &bits))
}

/// A bit string cast to bit(`n`), or to bit varying(`n`) where `varying`, as `bit` and `varbit`
/// take it: of another length refused, unless `explicit`, where it is cut or padded with zeros to
/// bit(`n`)'s, or cut to bit varying(`n`)'s. Without `n`, as it is.
pub fn bits_cast(key: &[u8], n: Option<u32>, varying: bool, explicit: bool) -> Result<Vec<u8>> {
    let Some(n) = n else {
        return Ok(key.to_vec());
    };
    let (len, bits) = bits_of_key(key);
    let fits = if varying { len <= n } else { len == n };
    if fits {
        return Ok(key.to_vec());
    }
    if !explicit {
        return Err(Error::new(if varying {
            format!("bit string too long for type bit varying({n})")
        } else {
            format!("bit string length {len} does not match type bit({n})")
        }));
    }
    let mut out = vec![0u8; (n as usize).div_ceil(8)];
    let keep = out.len().min(bits.len());
    out[..keep].copy_from_slice(&bits[..keep]);
    if n % 8 != 0 {
        let last = out.len() - 1;
        out[last] &= 0xffu8 << (8 - n % 8);
    }
    Ok(bits_key(n, &out))
}

/// The bytes a value of a string type or a bit string is keyed by, from the bytes it is held as:
/// a bpchar's without its trailing blanks, any other's as they are.
pub fn key_bytes(ty: ColumnType, held: &[u8]) -> Cow<'_, [u8]> {
    match ty {
        ColumnType::Bpchar(_) => Cow::Borrowed(bpchar_key(held)),
        _ => Cow::Borrowed(held),
    }
}

/// A string type or a bit string named by PostgreSQL, as an answer's field metadata names it.
pub fn type_named(named: &str) -> Option<ColumnType> {
    let length = |s: &str| -> Option<u32> { s.strip_suffix(')')?.parse().ok() };
    Some(match named {
        "bpchar" => ColumnType::Bpchar(None),
        "name" => ColumnType::Name,
        "bit varying" => ColumnType::Varbit(None),
        n => {
            if let Some(m) = n.strip_prefix("character(") {
                ColumnType::Bpchar(Some(length(m)?))
            } else if let Some(m) = n.strip_prefix("bit varying(") {
                ColumnType::Varbit(Some(length(m)?))
            } else {
                ColumnType::Bit(length(n.strip_prefix("bit(")?)?)
            }
        }
    })
}

/// A bit string's two leaves as arrow's Parquet reader gives a bit varying's: its length and its
/// bits.
fn bits_fields() -> arrow::datatypes::Fields {
    use arrow::datatypes::{DataType as A, Field};
    vec![
        Field::new("length", A::Int32, false),
        Field::new("bits", A::Binary, false),
    ]
    .into()
}

/// The Arrow type an answer holds a value of a string type or a bit string as: what arrow's
/// Parquet reader gives its page form.
pub fn string_arrow(ty: ColumnType) -> arrow::datatypes::DataType {
    use arrow::datatypes::DataType as A;
    match ty {
        ColumnType::Bit(n) => A::FixedSizeBinary((n as i32 + 7) / 8),
        ColumnType::Varbit(_) => A::Struct(bits_fields()),
        ColumnType::Bytes => A::Binary,
        _ => A::Utf8,
    }
}

/// Whether an answer names the type of a column of `ty` in its field's metadata: a bpchar's, a
/// name's, a bit string's, which their Arrow types do not say.
pub fn is_named(ty: ColumnType) -> bool {
    matches!(ty, ColumnType::Bpchar(_) | ColumnType::Name) || ty.is_bits()
}

/// An answer's field for a column of a string type or a bit string, named for its PostgreSQL type
/// in its metadata where its Arrow type does not say it.
pub fn string_field(name: &str, ty: ColumnType, nullable: bool) -> arrow::datatypes::Field {
    let f = arrow::datatypes::Field::new(name, string_arrow(ty), nullable);
    if is_named(ty) {
        f.with_metadata([(crate::scalars::PG_TYPE.to_string(), ty.pg_name())].into())
    } else {
        f
    }
}

/// The string type or bit string an answer's field holds, by its metadata.
pub fn field_string(f: &arrow::datatypes::Field) -> Option<ColumnType> {
    let ty = type_named(f.metadata().get(crate::scalars::PG_TYPE)?)?;
    (string_arrow(ty) == *f.data_type()).then_some(ty)
}

/// A column of a string type or a bit string as an answer holds it, from the bytes each value is
/// held as: text as text, bytea as bytes, a bit string as its bits.
pub fn string_array<'a>(
    ty: ColumnType,
    held: impl Iterator<Item = Option<&'a [u8]>>,
) -> arrow::array::ArrayRef {
    use arrow::array::{BinaryArray, FixedSizeBinaryArray, Int32Array, StringArray, StructArray};
    use std::sync::Arc;
    match ty {
        ColumnType::Bit(n) => Arc::new(
            FixedSizeBinaryArray::try_from_sparse_iter_with_size(
                held.map(|k| k.map(|k| bits_of_key(k).1)),
                (n as i32 + 7) / 8,
            )
            .expect("a bit string takes its bits"),
        ),
        ColumnType::Varbit(_) => {
            let parts: Vec<Option<(u32, Vec<u8>)>> = held.map(|k| k.map(bits_of_key)).collect();
            let lengths: Int32Array = parts
                .iter()
                .map(|p| Some(p.as_ref().map_or(0, |p| p.0 as i32)))
                .collect();
            let bits: BinaryArray = parts
                .iter()
                .map(|p| Some(p.as_ref().map_or(&[][..], |p| p.1.as_slice())))
                .collect();
            Arc::new(
                StructArray::try_new(
                    bits_fields(),
                    vec![Arc::new(lengths), Arc::new(bits)],
                    Some(parts.iter().map(Option::is_some).collect()),
                )
                .expect("a bit varying takes its two leaves"),
            )
        }
        ColumnType::Bytes => Arc::new(held.collect::<BinaryArray>()),
        _ => Arc::new(
            held.map(|b| b.map(|b| std::str::from_utf8(b).unwrap_or_default()))
                .collect::<StringArray>(),
        ),
    }
}

/// Row `r` of an answer's column of a bit string, as the key it is held as; none for a NULL.
pub fn bits_key_at(ty: ColumnType, a: &dyn arrow::array::Array, r: usize) -> Option<Vec<u8>> {
    use arrow::array::{BinaryArray, FixedSizeBinaryArray, Int32Array, StructArray};
    if a.is_null(r) {
        return None;
    }
    let any = a.as_any();
    match ty {
        ColumnType::Bit(n) => Some(bits_key(
            n,
            any.downcast_ref::<FixedSizeBinaryArray>()?.value(r),
        )),
        ColumnType::Varbit(_) => {
            let s = any.downcast_ref::<StructArray>()?;
            let len = s
                .column_by_name("length")?
                .as_any()
                .downcast_ref::<Int32Array>()?;
            let bits = s
                .column_by_name("bits")?
                .as_any()
                .downcast_ref::<BinaryArray>()?;
            Some(bits_key(len.value(r) as u32, bits.value(r)))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytea_is_read_as_byteain_reads_it() {
        for (s, want) in [
            ("\\x00ff41", vec![0, 0xff, 0x41]),
            ("\\x", vec![]),
            ("\\x0A0b", vec![0x0a, 0x0b]),
            ("\\x 0a\t0b\n", vec![0x0a, 0x0b]),
            ("abc\\\\", b"abc\\".to_vec()),
            ("\\101\\000", vec![0x41, 0]),
            ("a\\000b", vec![b'a', 0, b'b']),
            ("", vec![]),
            ("\\377", vec![0xff]),
        ] {
            assert_eq!(bytea_in(s).unwrap(), want, "{s:?}");
        }
        for s in ["\\x0", "\\x0g", "\\x0 a", "\\", "\\400", "\\8", "a\\x"] {
            assert!(bytea_in(s).is_err(), "{s:?}");
        }
        assert_eq!(bytea_out(&[0, 0xff, 0x41]), "\\x00ff41");
    }

    #[test]
    fn a_bpchar_is_padded_cut_and_keyed_without_its_trailing_blanks() {
        assert_eq!(bpchar_in("a", Some(3), false).unwrap(), "a  ");
        assert_eq!(bpchar_in("abc  ", Some(3), false).unwrap(), "abc");
        assert!(bpchar_in("abcd", Some(3), false).is_err());
        assert_eq!(bpchar_in("abcd", Some(3), true).unwrap(), "abc");
        assert_eq!(bpchar_in("é", Some(2), false).unwrap(), "é ");
        assert_eq!(bpchar_in("a  ", None, false).unwrap(), "a  ");
        assert_eq!(bpchar_key(b"a \t "), b"a \t");
        assert_eq!(bpchar_key(b"   "), b"");
    }

    #[test]
    fn a_name_is_cut_to_63_bytes_on_a_character() {
        assert_eq!(name_in(&"x".repeat(70)).len(), 63);
        assert_eq!(name_in(&"é".repeat(40)), "é".repeat(31));
        assert_eq!(name_in(""), "");
    }

    #[test]
    fn a_bit_string_is_read_and_written_as_bit_in_and_bit_out_do() {
        assert_eq!(bits_in("101", None, true).unwrap(), (3, vec![0xa0]));
        assert_eq!(bits_in("B10101", Some(5), false).unwrap(), (5, vec![0xa8]));
        assert_eq!(bits_in("X1F", None, true).unwrap(), (8, vec![0x1f]));
        assert_eq!(bits_in("x1f2", None, true).unwrap(), (12, vec![0x1f, 0x20]));
        assert_eq!(bits_in("", None, true).unwrap(), (0, vec![]));
        assert!(bits_in("102", None, true).is_err());
        assert!(bits_in("10", Some(3), false).is_err());
        assert!(bits_in("1010", Some(3), true).is_err());
        assert_eq!(bits_out(5, &[0xa8]), "10101");
        let k = bits_parse("10100", None, true).unwrap();
        assert_eq!(bits_text(&k), "10100");
        assert_eq!(
            bits_text(&bits_cast(&k, Some(3), false, true).unwrap()),
            "101"
        );
        assert_eq!(
            bits_text(&bits_cast(&k, Some(8), false, true).unwrap()),
            "10100000"
        );
        assert!(bits_cast(&k, Some(3), false, false).is_err());
        assert_eq!(
            bits_text(&bits_cast(&k, Some(3), true, true).unwrap()),
            "101"
        );
        assert!(bits_cast(&k, Some(3), true, false).is_err());
    }

    /// Every string of up to `n` characters over `alphabet`.
    fn words(alphabet: &[u8], n: usize) -> Vec<Vec<u8>> {
        let mut out = vec![Vec::new()];
        let mut last = vec![Vec::new()];
        for _ in 0..n {
            last = last
                .iter()
                .flat_map(|w| {
                    alphabet.iter().map(move |&c| {
                        let mut w = w.clone();
                        w.push(c);
                        w
                    })
                })
                .collect();
            out.extend(last.iter().cloned());
        }
        out
    }

    /// bpcharcmp under a collation of byte order, as varchar.c and varlena.c write it: each side's
    /// length less its trailing blanks (`bcTruelen`), then `memcmp` over the shorter, then the
    /// shorter first.
    fn bpcharcmp(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
        let truelen = |s: &[u8]| {
            let mut i = s.len();
            while i > 0 && s[i - 1] == b' ' {
                i -= 1;
            }
            i
        };
        let (la, lb) = (truelen(a), truelen(b));
        let n = la.min(lb);
        match a[..n].cmp(&b[..n]) {
            std::cmp::Ordering::Equal => la.cmp(&lb),
            other => other,
        }
    }

    #[test]
    fn a_bpchar_s_key_sorts_on_its_class_as_bpcharcmp_does_over_every_short_string() {
        let all = words(b" \ta!", 5);
        for a in &all {
            for b in &all {
                assert_eq!(
                    bpchar_key(a).cmp(bpchar_key(b)),
                    bpcharcmp(a, b),
                    "{a:?} {b:?}"
                );
            }
        }
    }

    /// bit_cmp, as varbit.c writes it: `memcmp` over the bytes both hold, then the shorter in bits
    /// first.
    fn bit_cmp(a: (u32, &[u8]), b: (u32, &[u8])) -> std::cmp::Ordering {
        let n = a.1.len().min(b.1.len());
        match a.1[..n].cmp(&b.1[..n]) {
            std::cmp::Ordering::Equal => a.0.cmp(&b.0),
            other => other,
        }
    }

    #[test]
    fn a_bit_string_s_key_sorts_as_bit_cmp_does_and_each_bit_a_place_of_three_over_every_short_one()
    {
        // Every bit string of up to 12 bits, as its bits, a 0 or 1 each.
        let all: Vec<Vec<u8>> = words(&[0, 1], 12);
        let held = |w: &[u8]| -> (u32, Vec<u8>) {
            let mut bits = vec![0u8; w.len().div_ceil(8)];
            for (i, &b) in w.iter().enumerate() {
                bits[i / 8] |= b << (7 - i % 8);
            }
            (w.len() as u32, bits)
        };
        // Each bit a place of three, an ended one first: the bits compared one by one, the shorter
        // first where one ends.
        let mut by_places = all.clone();
        by_places.sort();
        let keys: Vec<Vec<u8>> = by_places
            .iter()
            .map(|w| {
                let (len, bits) = held(w);
                bits_key(len, &bits)
            })
            .collect();
        assert!(keys.windows(2).all(|k| k[0] < k[1]));
        for (w, k) in by_places.iter().zip(&keys) {
            let (len, bits) = held(w);
            assert_eq!(bits_of_key(k), (len, bits.clone()));
            for r in 0..13 {
                let want = w.get(r).map_or(-1, |&b| b as i64);
                assert_eq!(bit_of_key(k, r), want, "{w:?} {r}");
            }
        }
        // bit_cmp agrees with the places on every neighbouring pair, and on a seeded spread of others.
        for p in by_places.windows(2) {
            let (a, b) = (held(&p[0]), held(&p[1]));
            assert_eq!(bit_cmp((a.0, &a.1), (b.0, &b.1)), std::cmp::Ordering::Less);
        }
        let mut x: u64 = 0x2545_f491_4f6c_dd1d;
        for _ in 0..200_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let (i, j) = (
                (x % all.len() as u64) as usize,
                ((x >> 32) % all.len() as u64) as usize,
            );
            let (a, b) = (held(&all[i]), held(&all[j]));
            assert_eq!(
                bits_key(a.0, &a.1).cmp(&bits_key(b.0, &b.1)),
                bit_cmp((a.0, &a.1), (b.0, &b.1)),
                "{:?} {:?}",
                all[i],
                all[j]
            );
        }
    }
}
