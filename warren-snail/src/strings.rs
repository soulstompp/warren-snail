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

/// The bytes a value of a string type, a bit string, a numeric held as its text or a type held as
/// its text is keyed by, from the bytes it is held as: a bpchar's without its trailing blanks, a
/// numeric's, a jsonb's, an inet's, a tsvector's or a tsquery's its class's key, any other's as
/// they are.
pub fn key_bytes(ty: ColumnType, held: &[u8]) -> Cow<'_, [u8]> {
    match ty {
        ColumnType::Bpchar(_) => Cow::Borrowed(bpchar_key(held)),
        ColumnType::NumericText(_) => Cow::Owned(
            crate::numeric::class_of_text(std::str::from_utf8(held).unwrap_or("")).into_bytes(),
        ),
        ColumnType::Textual(t) => crate::textual::key_bytes(t, held),
        _ => Cow::Borrowed(held),
    }
}

/// The bytes a value of a column is keyed by, from the bytes it is held as: `key_bytes`'s, and
/// under a library collation, `collator`'s key of those.
pub fn column_key<'a>(
    ty: ColumnType,
    collator: Option<&crate::collations::Collator>,
    held: &'a [u8],
) -> Cow<'a, [u8]> {
    let key = key_bytes(ty, held);
    match collator {
        Some(c) => Cow::Owned(c.key(&key)),
        None => key,
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
            if let Some(t) = crate::textual::Textual::of_type_name(n) {
                return Some(ColumnType::Textual(t));
            }
            if let Some(t) = crate::textual::Internal::of_type_name(n) {
                return Some(ColumnType::Internal(t));
            }
            if let Some(k) = crate::geo::GeoKind::of_name(n) {
                return Some(ColumnType::Geometric(k));
            }
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

/// An aclitem's three leaves as arrow's Parquet reader gives them: its grantee, grantor and
/// privileges.
fn aclitem_fields() -> arrow::datatypes::Fields {
    use arrow::datatypes::{DataType as A, Field};
    vec![
        Field::new("grantee", A::UInt32, false),
        Field::new("grantor", A::UInt32, false),
        Field::new("privileges", A::UInt64, false),
    ]
    .into()
}

/// An answer's aclitem at row `r`, as the text the snail holds; none for a NULL.
pub fn aclitem_at(a: &dyn arrow::array::Array, r: usize) -> Option<String> {
    use arrow::array::{Array, StructArray, UInt32Array, UInt64Array};
    let s = a.as_any().downcast_ref::<StructArray>()?;
    if s.is_null(r) {
        return None;
    }
    let u32_at = |i: usize| {
        s.column(i)
            .as_any()
            .downcast_ref::<UInt32Array>()
            .map(|x| x.value(r))
    };
    let privileges = s.column(2).as_any().downcast_ref::<UInt64Array>()?.value(r);
    Some(crate::textual::aclitem_text(
        u32_at(0)?,
        u32_at(1)?,
        privileges,
    ))
}

/// A geometric value's float8 leaves as arrow's Parquet reader gives them: a field each.
fn geometric_fields(ty: ColumnType) -> arrow::datatypes::Fields {
    use arrow::datatypes::{DataType as A, Field};
    ty.leaves()
        .iter()
        .map(|l| Field::new(l.field.unwrap_or_default(), A::Float64, false))
        .collect()
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
        ColumnType::Internal(t) if t.is_bytes() => A::Binary,
        // A path, a polygon or a snapshot: the tree of its own.
        ty if ty.is_held_tree() => crate::composed::group_arrow(ty),
        ColumnType::Geometric(_) => A::Struct(geometric_fields(ty)),
        ColumnType::Textual(crate::textual::Textual::Aclitem) => A::Struct(aclitem_fields()),
        _ => A::Utf8,
    }
}

/// Whether an answer names the type of a column of `ty` in its field's metadata: a bpchar's, a
/// name's, a bit string's, a type's held as its text or only a server makes, which their Arrow
/// types do not say.
pub fn is_named(ty: ColumnType) -> bool {
    matches!(
        ty,
        ColumnType::Bpchar(_)
            | ColumnType::Name
            | ColumnType::Textual(_)
            | ColumnType::Internal(_)
            | ColumnType::Geometric(_)
    ) || ty.is_bits()
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
        // A path, a polygon or a snapshot, on the tree of its own.
        ty if ty.is_held_tree() => {
            let values: Vec<Option<crate::read::statement::Datum>> = held
                .map(|b| {
                    b.map(|b| match ty {
                        ColumnType::Geometric(_) => {
                            crate::read::statement::Datum::Bytes(b.to_vec())
                        }
                        _ => crate::read::statement::Datum::Text(
                            String::from_utf8_lossy(b).into_owned(),
                        ),
                    })
                })
                .collect();
            crate::composed::group_array(ty, &values.iter().map(Option::as_ref).collect::<Vec<_>>())
                .expect("a value held as a tree takes its leaves")
        }
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
        ColumnType::Textual(crate::textual::Textual::Aclitem) => {
            let parts: Vec<Option<(u32, u32, u64)>> = held
                .map(|b| {
                    b.and_then(|b| crate::textual::aclitem_parse(std::str::from_utf8(b).ok()?).ok())
                })
                .collect();
            Arc::new(
                StructArray::try_new(
                    aclitem_fields(),
                    vec![
                        Arc::new(
                            parts
                                .iter()
                                .map(|p| Some(p.map_or(0, |p| p.0)))
                                .collect::<arrow::array::UInt32Array>(),
                        ),
                        Arc::new(
                            parts
                                .iter()
                                .map(|p| Some(p.map_or(0, |p| p.1)))
                                .collect::<arrow::array::UInt32Array>(),
                        ),
                        Arc::new(
                            parts
                                .iter()
                                .map(|p| Some(p.map_or(0, |p| p.2)))
                                .collect::<arrow::array::UInt64Array>(),
                        ),
                    ],
                    Some(parts.iter().map(Option::is_some).collect()),
                )
                .expect("an aclitem takes its three leaves"),
            )
        }
        ColumnType::Geometric(_) => {
            let rows: Vec<Option<&[u8]>> = held.collect();
            let n = ty.leaf_count();
            let leaves: Vec<arrow::array::ArrayRef> = (0..n)
                .map(|l| {
                    Arc::new(
                        rows.iter()
                            .map(|b| {
                                Some(f64::from_bits(u64::from_be_bytes(b.map_or([0; 8], |b| {
                                    b[8 * l..8 * l + 8].try_into().unwrap()
                                }))))
                            })
                            .collect::<arrow::array::Float64Array>(),
                    ) as arrow::array::ArrayRef
                })
                .collect();
            Arc::new(
                StructArray::try_new(
                    geometric_fields(ty),
                    leaves,
                    Some(rows.iter().map(Option::is_some).collect()),
                )
                .expect("a geometric value takes its leaves"),
            )
        }
        ColumnType::Bytes | ColumnType::Internal(_) if ty.held_as_bytes() => {
            Arc::new(held.collect::<BinaryArray>())
        }
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

/// PostgreSQL 19's own Unicode case tables (`cases.txt`, written by inner-warren's `cases.nu` from
/// its source): each code point's simple lower and upper case, its simple title case and folding,
/// the full mappings, the code points Unicode calls Cased and Case_Ignorable, and those it calls
/// Alphabetic and a decimal digit.
struct Cases {
    simple: std::collections::HashMap<u32, (u32, u32)>,
    /// A code point's simple title case and folding.
    titled: std::collections::HashMap<u32, (u32, u32)>,
    /// A code point's full lower and upper case, and whether it is mapped so only as a final sigma.
    special: std::collections::HashMap<u32, (bool, Vec<u32>, Vec<u32>)>,
    /// A code point's full title case and folding, where `special` maps it.
    special_titled: std::collections::HashMap<u32, (Vec<u32>, Vec<u32>)>,
    cased: Vec<(u32, u32)>,
    ignorable: Vec<(u32, u32)>,
    alphabetic: Vec<(u32, u32)>,
    digit: Vec<(u32, u32)>,
}

fn cases() -> &'static Cases {
    static CASES: std::sync::OnceLock<Cases> = std::sync::OnceLock::new();
    CASES.get_or_init(|| {
        let hex = |s: &str| u32::from_str_radix(s, 16).expect("a code point of cases.txt");
        let cps =
            |s: &str| -> Vec<u32> { s.split(',').filter(|x| !x.is_empty()).map(hex).collect() };
        let mut c = Cases {
            simple: Default::default(),
            titled: Default::default(),
            special: Default::default(),
            special_titled: Default::default(),
            cased: Vec::new(),
            ignorable: Vec::new(),
            alphabetic: Vec::new(),
            digit: Vec::new(),
        };
        let mut section = "";
        for line in include_str!("cases.txt").lines() {
            if let Some(name) = line.strip_prefix("= ") {
                section = name;
                continue;
            }
            let f: Vec<&str> = line.split(' ').collect();
            match section {
                "map" => {
                    c.simple.insert(hex(f[0]), (hex(f[1]), hex(f[2])));
                    c.titled.insert(hex(f[0]), (hex(f[3]), hex(f[4])));
                }
                "special" => {
                    let lower = cps(f.get(2).copied().unwrap_or_default());
                    let upper = cps(f.get(3).copied().unwrap_or_default());
                    c.special.insert(hex(f[0]), (f[1] == "1", lower, upper));
                    let title = cps(f.get(4).copied().unwrap_or_default());
                    let fold = cps(f.get(5).copied().unwrap_or_default());
                    c.special_titled.insert(hex(f[0]), (title, fold));
                }
                "cased" => c.cased.push((hex(f[0]), hex(f[1]))),
                "ignorable" => c.ignorable.push((hex(f[0]), hex(f[1]))),
                "alphabetic" => c.alphabetic.push((hex(f[0]), hex(f[1]))),
                _ => c.digit.push((hex(f[0]), hex(f[1]))),
            }
        }
        c.ignorable.sort();
        c.alphabetic.sort();
        c.digit.sort();
        c
    })
}

/// A code point's simple upper or lower case under PostgreSQL's own tables
/// (`unicode_uppercase_simple`, `unicode_lowercase_simple`).
pub(crate) fn simple_case(cp: u32, upper: bool) -> u32 {
    cases()
        .simple
        .get(&cp)
        .map_or(cp, |&(lower, up)| if upper { up } else { lower })
}

fn in_ranges(ranges: &[(u32, u32)], cp: u32) -> bool {
    ranges.iter().any(|&(lo, hi)| lo <= cp && cp <= hi)
}

/// Whether the character at `at`, a sigma, ends a word as Unicode's Final_Sigma says: a cased
/// character before it and none after, each passing over the case-ignorable ones.
fn final_sigma(chars: &[char], at: usize) -> bool {
    let c = cases();
    let first_cased = |mut it: Box<dyn Iterator<Item = &char> + '_>| {
        it.find(|&&x| !in_ranges(&c.ignorable, x as u32))
            .is_some_and(|&x| in_ranges(&c.cased, x as u32))
    };
    first_cased(Box::new(chars[..at].iter().rev()))
        && !first_cased(Box::new(chars[at + 1..].iter()))
}

/// Refused where the machine has no glibc C.UTF-8, the locale libc's C.utf8 and the database's
/// default map case under.
pub(crate) fn c_utf8() -> Result<()> {
    crate::sys::ctype::found("libc's C.utf8 maps case under")
}

/// A text in upper case, or in lower case where `!upper`, as PostgreSQL's upper and lower map it
/// under a collation that maps case as `casing`: ASCII's letters alone; PostgreSQL 19's own Unicode
/// tables, a character to a character, or under `Full` to its full mapping where it has one, a
/// sigma that ends a word lowered to a final sigma; glibc's C.UTF-8, a character to a character; a
/// library collation's own mapping.
pub fn case(s: &str, upper: bool, casing: crate::catalog::Casing) -> Result<String> {
    use crate::catalog::Casing;
    Ok(match casing {
        Casing::Ascii => {
            if upper {
                s.to_ascii_uppercase()
            } else {
                s.to_ascii_lowercase()
            }
        }
        Casing::Libc => {
            c_utf8()?;
            s.chars()
                .map(|ch| {
                    let m = if upper {
                        crate::sys::ctype::upper(ch as u32)
                    } else {
                        crate::sys::ctype::lower(ch as u32)
                    };
                    char::from_u32(m).unwrap_or(ch)
                })
                .collect()
        }
        Casing::Library(id) => crate::collations::get(id).case(s, upper)?,
        Casing::Simple | Casing::Full => {
            let c = cases();
            let chars: Vec<char> = s.chars().collect();
            let mut out = String::with_capacity(s.len());
            for (at, &ch) in chars.iter().enumerate() {
                let cp = ch as u32;
                if casing == Casing::Full {
                    if let Some((sigma, lower, up)) = c.special.get(&cp) {
                        if !sigma || final_sigma(&chars, at) {
                            let to = if upper { up } else { lower };
                            out.extend(to.iter().filter_map(|&x| char::from_u32(x)));
                            continue;
                        }
                    }
                }
                let to = c
                    .simple
                    .get(&cp)
                    .map_or(cp, |&(lo, up)| if upper { up } else { lo });
                out.push(char::from_u32(to).unwrap_or(ch));
            }
            out
        }
    })
}

/// One piece of a LIKE pattern.
#[derive(Clone, Copy, PartialEq)]
enum Piece<T> {
    /// `%`: any run, the empty one among them.
    Any,
    /// `_`: any one.
    One,
    Lit(T),
    /// An escape that ends the pattern, refused where the text is matched up to it.
    End,
}

/// Whether `text` matches the LIKE `pattern`, each a sequence of characters or of bytes, as
/// PostgreSQL's LIKE reads one: `%` any run, `_` any one, and `escape` before any one that one
/// itself; a pattern that ends with its escape refused where the match reaches it with text left.
pub fn like<T: Copy + PartialEq>(
    text: &[T],
    pattern: &[T],
    percent: T,
    under: T,
    escape: Option<T>,
) -> Result<bool> {
    let mut pieces = Vec::with_capacity(pattern.len());
    let mut i = 0;
    while i < pattern.len() {
        let c = pattern[i];
        if Some(c) == escape {
            pieces.push(pattern.get(i + 1).map_or(Piece::End, |&n| Piece::Lit(n)));
            i += 2;
            continue;
        }
        pieces.push(if c == percent {
            Piece::Any
        } else if c == under {
            Piece::One
        } else {
            Piece::Lit(c)
        });
        i += 1;
    }
    // The text and the pattern walked together; at a mismatch, back to the last `%`, which takes
    // one more of the text.
    let (mut t, mut p) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        match pieces.get(p) {
            Some(Piece::Any) => {
                star = Some((p, t));
                p += 1;
            }
            Some(Piece::One) => {
                t += 1;
                p += 1;
            }
            Some(Piece::Lit(c)) if *c == text[t] => {
                t += 1;
                p += 1;
            }
            Some(Piece::End) => {
                return Err(Error::new(
                    "LIKE pattern must not end with escape character",
                ))
            }
            _ => match star {
                Some((sp, st)) => {
                    star = Some((sp, st + 1));
                    p = sp + 1;
                    t = st + 1;
                }
                None => return Ok(false),
            },
        }
    }
    Ok(pieces[p..].iter().all(|x| *x == Piece::Any))
}

/// Whether a text matches a LIKE pattern under a nondeterministic collation, as PostgreSQL 19's
/// `MatchText` matches it there: each run of the pattern between its wildcards compared with a run
/// of the text by the collation, each length of run tried in turn.
pub fn like_nondeterministic(
    text: &[char],
    pattern: &[char],
    escape: Option<char>,
    collator: &crate::collations::Collator,
) -> Result<bool> {
    Ok(match_run(text, pattern, escape, collator)? == Matched::Yes)
}

/// What a LIKE's match of a text's end makes of it: matched, not, or never from a later start.
#[derive(Clone, Copy, PartialEq)]
enum Matched {
    Yes,
    No,
    Never,
}

fn match_run(
    mut t: &[char],
    mut p: &[char],
    escape: Option<char>,
    collator: &crate::collations::Collator,
) -> Result<Matched> {
    let ended = || Error::new("LIKE pattern must not end with escape character");
    let utf8 = |s: &[char]| s.iter().collect::<String>();
    if p == ['%'] {
        return Ok(Matched::Yes);
    }
    while !t.is_empty() && !p.is_empty() {
        if p[0] == '%' {
            p = &p[1..];
            while let Some(&c) = p.first() {
                if c == '%' {
                    p = &p[1..];
                } else if c == '_' {
                    if t.is_empty() {
                        return Ok(Matched::Never);
                    }
                    t = &t[1..];
                    p = &p[1..];
                } else {
                    break;
                }
            }
            if p.is_empty() {
                return Ok(Matched::Yes);
            }
            if Some(p[0]) == escape && p.len() < 2 {
                return Err(ended());
            }
            while !t.is_empty() {
                let m = match_run(t, p, escape, collator)?;
                if m != Matched::No {
                    return Ok(m);
                }
                t = &t[1..];
            }
            return Ok(Matched::Never);
        }
        if p[0] == '_' {
            t = &t[1..];
            p = &p[1..];
            continue;
        }
        // The pattern's run up to its next wildcard, its escapes taken off.
        let mut run = Vec::new();
        let mut i = 0;
        while i < p.len() {
            if Some(p[i]) == escape {
                i += 1;
                if i == p.len() {
                    return Err(ended());
                }
            } else if p[i] == '_' || p[i] == '%' {
                break;
            }
            run.push(p[i]);
            i += 1;
        }
        let run = utf8(&run);
        let rest = &p[i..];
        if rest.is_empty() {
            return Ok(if collator.equal(run.as_bytes(), utf8(t).as_bytes()) {
                Matched::Yes
            } else {
                Matched::No
            });
        }
        for k in 0..=t.len() {
            if collator.equal(run.as_bytes(), utf8(&t[..k]).as_bytes())
                && match_run(&t[k..], rest, escape, collator)? == Matched::Yes
            {
                return Ok(Matched::Yes);
            }
        }
        return Ok(Matched::No);
    }
    if !t.is_empty() {
        return Ok(Matched::No);
    }
    while p.first() == Some(&'%') {
        p = &p[1..];
    }
    Ok(if p.is_empty() {
        Matched::Yes
    } else {
        Matched::Never
    })
}

/// A LIKE's ESCAPE: its one character, or none where it is empty.
pub fn like_escape(e: &str) -> Result<Option<char>> {
    let mut chars = e.chars();
    match (chars.next(), chars.next()) {
        (None, _) => Ok(None),
        (Some(c), None) => Ok(Some(c)),
        _ => Err(Error::new(
            "invalid escape string: Escape string must be empty or one character.",
        )),
    }
}

/// A bit string's bits, each a 0 or a 1, from its key.
fn bits_of(key: &[u8]) -> Vec<bool> {
    let (len, b) = bits_of_key(key);
    (0..len as usize)
        .map(|r| (b[r / 8] >> (7 - r % 8)) & 1 == 1)
        .collect()
}

/// A bit string's key, from its bits.
fn key_of_bits(bits: &[bool]) -> Vec<u8> {
    let mut b = vec![0u8; bits.len().div_ceil(8)];
    for (r, &x) in bits.iter().enumerate() {
        if x {
            b[r / 8] |= 0x80 >> (r % 8);
        }
    }
    bits_key(bits.len() as u32, &b)
}

/// Two bit strings of one length, bit by bit, as `bitand`, `bitor` and `bitxor` take them: `name`
/// the operation PostgreSQL's refusal names where their lengths differ.
pub fn bits_each(a: &[u8], b: &[u8], name: &str, f: fn(bool, bool) -> bool) -> Result<Vec<u8>> {
    let (x, y) = (bits_of(a), bits_of(b));
    if x.len() != y.len() {
        return Err(Error::new(format!(
            "cannot {name} bit strings of different sizes"
        )));
    }
    Ok(key_of_bits(
        &x.iter().zip(&y).map(|(&p, &q)| f(p, q)).collect::<Vec<_>>(),
    ))
}

/// A bit string with every bit turned, as `bitnot` takes it.
pub fn bits_not(a: &[u8]) -> Vec<u8> {
    key_of_bits(&bits_of(a).iter().map(|b| !b).collect::<Vec<_>>())
}

/// A bit string shifted `n` places to the left, to the right where `n` is negative, as
/// `bitshiftleft` takes it: of the same length, zeros coming in.
pub fn bits_shift(a: &[u8], n: i64) -> Vec<u8> {
    let x = bits_of(a);
    let len = x.len() as i64;
    let out: Vec<bool> = (0..len)
        .map(|r| {
            let from = r + n;
            (0..len).contains(&from) && x[from as usize]
        })
        .collect();
    key_of_bits(&out)
}

/// Two bit strings one after the other, as `bitcat` takes them.
pub fn bits_cat(a: &[u8], b: &[u8]) -> Vec<u8> {
    let mut x = bits_of(a);
    x.extend(bits_of(b));
    key_of_bits(&x)
}

/// A text with each word's first letter in upper case and the rest in lower, as PostgreSQL's
/// initcap makes it under a collation that maps case as `casing`: a word a run of letters and
/// digits, read after each character is mapped; ASCII's letters alone, glibc's C.UTF-8 a
/// character to a character, a library collation as its library titles text, or PostgreSQL's own
/// Unicode tables as its builtin provider titles.
pub fn title(s: &str, casing: crate::catalog::Casing) -> Result<String> {
    use crate::catalog::Casing;
    let mut was_alnum = false;
    match casing {
        Casing::Ascii => Ok(s
            .chars()
            .map(|ch| {
                let m = if was_alnum {
                    ch.to_ascii_lowercase()
                } else {
                    ch.to_ascii_uppercase()
                };
                was_alnum = m.is_ascii_alphanumeric();
                m
            })
            .collect()),
        Casing::Libc => {
            c_utf8()?;
            Ok(s.chars()
                .map(|ch| {
                    let m = if was_alnum {
                        crate::sys::ctype::lower(ch as u32)
                    } else {
                        crate::sys::ctype::upper(ch as u32)
                    };
                    was_alnum = crate::sys::ctype::is(crate::sys::ctype::Class::Alnum, m);
                    char::from_u32(m).unwrap_or(ch)
                })
                .collect())
        }
        Casing::Library(id) => crate::collations::get(id).title(s),
        Casing::Simple | Casing::Full => Ok(unicode_title_fold(s, true, casing == Casing::Full)),
    }
}

/// Whether a code point lies in one of the sorted ranges.
fn in_sorted(ranges: &[(u32, u32)], cp: u32) -> bool {
    let at = ranges.partition_point(|&(_, hi)| hi < cp);
    ranges.get(at).is_some_and(|&(lo, _)| lo <= cp)
}

/// A text titled, as initcap titles it under PostgreSQL's own Unicode tables, or else folded, as
/// casefold folds it there; in full under pg_unicode_fast, a character to a character under
/// pg_c_utf8. A word begins wherever a character is a letter or digit and the one before it is not,
/// or the other way round, read on the text as given: Alphabetic or a decimal digit, the digits
/// ASCII's alone under pg_c_utf8. A word's first character takes its title case (its upper case
/// under pg_c_utf8), every other its lower.
fn unicode_title_fold(s: &str, title: bool, full: bool) -> String {
    let c = cases();
    let chars: Vec<char> = s.chars().collect();
    let alnum = |cp: u32| {
        in_sorted(&c.alphabetic, cp)
            || if full {
                in_sorted(&c.digit, cp)
            } else {
                (0x30..=0x39).contains(&cp)
            }
    };
    let mut out = String::with_capacity(s.len());
    let mut prev: Option<bool> = None;
    for (at, &ch) in chars.iter().enumerate() {
        let cp = ch as u32;
        // 0 lower, 1 upper, 2 title, 3 fold.
        let kind = if title {
            let a = alnum(cp);
            let starts = prev != Some(a);
            prev = Some(a);
            match (starts, full) {
                (false, _) => 0,
                (true, true) => 2,
                (true, false) => 1,
            }
        } else {
            3
        };
        if full && cp >= 0x80 {
            if let (Some((sigma, lower, upper)), Some((titled, folded))) =
                (c.special.get(&cp), c.special_titled.get(&cp))
            {
                if !sigma || final_sigma(&chars, at) {
                    let to = match kind {
                        0 => lower,
                        1 => upper,
                        2 => titled,
                        _ => folded,
                    };
                    out.extend(to.iter().filter_map(|&x| char::from_u32(x)));
                    continue;
                }
            }
        }
        let to = match kind {
            0 | 1 => c
                .simple
                .get(&cp)
                .map_or(cp, |&(lo, up)| if kind == 1 { up } else { lo }),
            _ => c
                .titled
                .get(&cp)
                .map_or(cp, |&(t, f)| if kind == 2 { t } else { f }),
        };
        out.push(char::from_u32(to).unwrap_or(ch));
    }
    out
}

/// A text case folded, as PostgreSQL's casefold folds it under a collation that maps case as
/// `casing`: ASCII's letters and glibc's C.UTF-8 to lower case, a library collation as its library
/// folds, PostgreSQL's own Unicode tables as its builtin provider folds.
pub fn fold(s: &str, casing: crate::catalog::Casing) -> Result<String> {
    use crate::catalog::Casing;
    match casing {
        Casing::Ascii | Casing::Libc => case(s, false, casing),
        Casing::Library(id) => crate::collations::get(id).fold(s),
        Casing::Simple | Casing::Full => Ok(unicode_title_fold(s, false, casing == Casing::Full)),
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

    #[test]
    fn case_is_mapped_as_each_collation_maps_it() {
        use crate::catalog::Casing::*;
        let up = |s: &str, c| case(s, true, c).unwrap();
        let low = |s: &str, c| case(s, false, c).unwrap();
        assert_eq!(up("élan ÉLAN", Ascii), "éLAN ÉLAN");
        assert_eq!(low("élan ÉLAN", Ascii), "élan Élan");
        for c in [Simple, Full, Libc] {
            assert_eq!(up("élan", c), "ÉLAN", "{c:?}");
        }
        assert_eq!(up("straße", Simple), "STRAßE");
        assert_eq!(up("straße", Full), "STRASSE");
        assert_eq!(up("ŉ", Full), "ʼN");
        assert_eq!(low("ΣΑΣ ΟΔΟΣ", Simple), "σασ οδοσ");
        assert_eq!(low("ΣΑΣ ΟΔΟΣ", Full), "σας οδος");
        // A sigma no cased letter comes before is no final one; nor one a letter follows.
        assert_eq!(low("Σ", Full), "σ");
        assert_eq!(low("ΑΣ'Β", Full), "ασ'β");
        assert_eq!(low("ΑΣ.", Full), "ας.");
        assert_eq!(low("İstanbul", Simple), "istanbul");
        assert_eq!(low("İstanbul", Full), "i\u{307}stanbul");
        assert_eq!(low("İstanbul", Libc), "istanbul");
        assert_eq!(up("ǆemal", Libc), "ǄEMAL");
    }

    #[test]
    fn like_matches_as_postgresql_s_like_does() {
        let m = |t: &str, p: &str| {
            let (t, p): (Vec<char>, Vec<char>) = (t.chars().collect(), p.chars().collect());
            like(&t, &p, '%', '_', Some('\\'))
        };
        assert!(m("abc", "a%").unwrap());
        assert!(m("abc", "%c").unwrap());
        assert!(m("abc", "_b_").unwrap());
        assert!(!m("abc", "_b").unwrap());
        assert!(m("a%c", "a\\%c").unwrap());
        assert!(!m("abc", "a\\%c").unwrap());
        assert!(m("", "%").unwrap());
        assert!(m("", "").unwrap());
        assert!(!m("", "_").unwrap());
        assert!(m("aXbXc", "a%b%c").unwrap());
        assert!(m("é", "_").unwrap());
        // A pattern ending in its escape is refused only where the match reaches it with text left.
        assert!(!m("a", "a\\").unwrap());
        assert!(m("ab", "a\\").is_err());
        assert!(m("xab", "%a\\").is_err());
        assert!(like(b"a%", b"a!%", b'%', b'_', Some(b'!')).unwrap());
        assert!(like(b"a\\b", b"a\\b", b'%', b'_', None).unwrap());
        assert_eq!(like_escape("").unwrap(), None);
        assert!(like_escape("ab").is_err());
    }
}
