//! The functions of byte strings, as PostgreSQL 19 computes them: md5 and the SHA-2 digests, the
//! two CRC-32s, encode and decode in each of their formats, a byte's and a bit's reading and
//! setting, and the conversions between the encodings a snail reads (UTF8, SQL_ASCII and LATIN1),
//! with PostgreSQL's table of every encoding's name, code and longest character.

use crate::{Error, Result};

// ---------------------------------------------------------------------------------------------
// The digests.

const MD5_S: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9,
    14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15,
    21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

const MD5_K: [u32; 64] = [
    0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
    0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
    0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
    0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
    0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
    0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
    0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
    0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
];

/// A message padded to whole blocks of `block` bytes, its length in bits written last in `width`
/// bytes, big-endian or little-endian.
fn padded(b: &[u8], block: usize, width: usize, big: bool) -> Vec<u8> {
    let mut m = b.to_vec();
    m.push(0x80);
    while m.len() % block != block - width {
        m.push(0);
    }
    let bits = (b.len() as u128) * 8;
    let len = if big {
        bits.to_be_bytes()
    } else {
        bits.to_le_bytes()
    };
    if big {
        m.extend_from_slice(&len[16 - width..]);
    } else {
        m.extend_from_slice(&len[..width]);
    }
    m
}

/// md5's digest, as 32 lowercase hexadecimal digits.
pub fn md5_hex(b: &[u8]) -> String {
    let mut h: [u32; 4] = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476];
    for chunk in padded(b, 64, 8, false).chunks(64) {
        let w: Vec<u32> = chunk
            .chunks(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        let (mut a, mut bb, mut c, mut d) = (h[0], h[1], h[2], h[3]);
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((bb & c) | (!bb & d), i),
                1 => ((d & bb) | (!d & c), (5 * i + 1) % 16),
                2 => (bb ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (bb | !d), (7 * i) % 16),
            };
            let f = f.wrapping_add(a).wrapping_add(MD5_K[i]).wrapping_add(w[g]);
            a = d;
            d = c;
            c = bb;
            bb = bb.wrapping_add(f.rotate_left(MD5_S[i]));
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(bb);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
    }
    h.iter()
        .flat_map(|x| x.to_le_bytes())
        .map(|x| format!("{x:02x}"))
        .collect()
}

const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// The SHA-256 rounds from an initial state, the digest's words.
fn sha256_words(b: &[u8], init: [u32; 8]) -> [u32; 8] {
    let mut h = init;
    for chunk in padded(b, 64, 8, true).chunks(64) {
        let mut w = [0u32; 64];
        for (i, c) in chunk.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes([c[0], c[1], c[2], c[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut v = h;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7]
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(SHA256_K[i])
                .wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v = [
                t1.wrapping_add(t2),
                v[0],
                v[1],
                v[2],
                v[3].wrapping_add(t1),
                v[4],
                v[5],
                v[6],
            ];
        }
        for i in 0..8 {
            h[i] = h[i].wrapping_add(v[i]);
        }
    }
    h
}

/// SHA-224's digest.
pub fn sha224(b: &[u8]) -> Vec<u8> {
    let h = sha256_words(
        b,
        [
            0xc1059ed8, 0x367cd507, 0x3070dd17, 0xf70e5939, 0xffc00b31, 0x68581511, 0x64f98fa7,
            0xbefa4fa4,
        ],
    );
    h[..7].iter().flat_map(|x| x.to_be_bytes()).collect()
}

/// SHA-256's digest.
pub fn sha256(b: &[u8]) -> Vec<u8> {
    let h = sha256_words(
        b,
        [
            0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
            0x5be0cd19,
        ],
    );
    h.iter().flat_map(|x| x.to_be_bytes()).collect()
}

const SHA512_K: [u64; 80] = [
    0x428a2f98d728ae22,
    0x7137449123ef65cd,
    0xb5c0fbcfec4d3b2f,
    0xe9b5dba58189dbbc,
    0x3956c25bf348b538,
    0x59f111f1b605d019,
    0x923f82a4af194f9b,
    0xab1c5ed5da6d8118,
    0xd807aa98a3030242,
    0x12835b0145706fbe,
    0x243185be4ee4b28c,
    0x550c7dc3d5ffb4e2,
    0x72be5d74f27b896f,
    0x80deb1fe3b1696b1,
    0x9bdc06a725c71235,
    0xc19bf174cf692694,
    0xe49b69c19ef14ad2,
    0xefbe4786384f25e3,
    0x0fc19dc68b8cd5b5,
    0x240ca1cc77ac9c65,
    0x2de92c6f592b0275,
    0x4a7484aa6ea6e483,
    0x5cb0a9dcbd41fbd4,
    0x76f988da831153b5,
    0x983e5152ee66dfab,
    0xa831c66d2db43210,
    0xb00327c898fb213f,
    0xbf597fc7beef0ee4,
    0xc6e00bf33da88fc2,
    0xd5a79147930aa725,
    0x06ca6351e003826f,
    0x142929670a0e6e70,
    0x27b70a8546d22ffc,
    0x2e1b21385c26c926,
    0x4d2c6dfc5ac42aed,
    0x53380d139d95b3df,
    0x650a73548baf63de,
    0x766a0abb3c77b2a8,
    0x81c2c92e47edaee6,
    0x92722c851482353b,
    0xa2bfe8a14cf10364,
    0xa81a664bbc423001,
    0xc24b8b70d0f89791,
    0xc76c51a30654be30,
    0xd192e819d6ef5218,
    0xd69906245565a910,
    0xf40e35855771202a,
    0x106aa07032bbd1b8,
    0x19a4c116b8d2d0c8,
    0x1e376c085141ab53,
    0x2748774cdf8eeb99,
    0x34b0bcb5e19b48a8,
    0x391c0cb3c5c95a63,
    0x4ed8aa4ae3418acb,
    0x5b9cca4f7763e373,
    0x682e6ff3d6b2b8a3,
    0x748f82ee5defb2fc,
    0x78a5636f43172f60,
    0x84c87814a1f0ab72,
    0x8cc702081a6439ec,
    0x90befffa23631e28,
    0xa4506cebde82bde9,
    0xbef9a3f7b2c67915,
    0xc67178f2e372532b,
    0xca273eceea26619c,
    0xd186b8c721c0c207,
    0xeada7dd6cde0eb1e,
    0xf57d4f7fee6ed178,
    0x06f067aa72176fba,
    0x0a637dc5a2c898a6,
    0x113f9804bef90dae,
    0x1b710b35131c471b,
    0x28db77f523047d84,
    0x32caab7b40c72493,
    0x3c9ebe0a15c9bebc,
    0x431d67c49c100d4c,
    0x4cc5d4becb3e42b6,
    0x597f299cfc657e2a,
    0x5fcb6fab3ad6faec,
    0x6c44198c4a475817,
];

/// The SHA-512 rounds from an initial state, the digest's words.
fn sha512_words(b: &[u8], init: [u64; 8]) -> [u64; 8] {
    let mut h = init;
    for chunk in padded(b, 128, 16, true).chunks(128) {
        let mut w = [0u64; 80];
        for (i, c) in chunk.chunks(8).enumerate() {
            w[i] = u64::from_be_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]);
        }
        for i in 16..80 {
            let s0 = w[i - 15].rotate_right(1) ^ w[i - 15].rotate_right(8) ^ (w[i - 15] >> 7);
            let s1 = w[i - 2].rotate_right(19) ^ w[i - 2].rotate_right(61) ^ (w[i - 2] >> 6);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut v = h;
        for i in 0..80 {
            let s1 = v[4].rotate_right(14) ^ v[4].rotate_right(18) ^ v[4].rotate_right(41);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7]
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(SHA512_K[i])
                .wrapping_add(w[i]);
            let s0 = v[0].rotate_right(28) ^ v[0].rotate_right(34) ^ v[0].rotate_right(39);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v = [
                t1.wrapping_add(t2),
                v[0],
                v[1],
                v[2],
                v[3].wrapping_add(t1),
                v[4],
                v[5],
                v[6],
            ];
        }
        for i in 0..8 {
            h[i] = h[i].wrapping_add(v[i]);
        }
    }
    h
}

/// SHA-384's digest.
pub fn sha384(b: &[u8]) -> Vec<u8> {
    let h = sha512_words(
        b,
        [
            0xcbbb9d5dc1059ed8,
            0x629a292a367cd507,
            0x9159015a3070dd17,
            0x152fecd8f70e5939,
            0x67332667ffc00b31,
            0x8eb44a8768581511,
            0xdb0c2e0d64f98fa7,
            0x47b5481dbefa4fa4,
        ],
    );
    h[..6].iter().flat_map(|x| x.to_be_bytes()).collect()
}

/// SHA-512's digest.
pub fn sha512(b: &[u8]) -> Vec<u8> {
    let h = sha512_words(
        b,
        [
            0x6a09e667f3bcc908,
            0xbb67ae8584caa73b,
            0x3c6ef372fe94f82b,
            0xa54ff53a5f1d36f1,
            0x510e527fade682d1,
            0x9b05688c2b3e6c1f,
            0x1f83d9abfb41bd6b,
            0x5be0cd19137e2179,
        ],
    );
    h.iter().flat_map(|x| x.to_be_bytes()).collect()
}

/// A reflected CRC-32 of the polynomial given: `crc` from all ones, its bits turned at the end.
fn crc_reflected(b: &[u8], poly: u32) -> i64 {
    let mut crc = 0xFFFF_FFFFu32;
    for &x in b {
        crc ^= u32::from(x);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ poly
            } else {
                crc >> 1
            };
        }
    }
    i64::from(crc ^ 0xFFFF_FFFF)
}

/// crc32: the CRC-32 of Ethernet.
pub fn crc32(b: &[u8]) -> i64 {
    crc_reflected(b, 0xEDB8_8320)
}

/// crc32c: the Castagnoli CRC-32.
pub fn crc32c(b: &[u8]) -> i64 {
    crc_reflected(b, 0x82F6_3B78)
}

// ---------------------------------------------------------------------------------------------
// encode and decode.

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const BASE64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
const BASE32HEX: &[u8; 32] = b"0123456789ABCDEFGHIJKLMNOPQRSTUV";

/// The formats encode and decode take, as PostgreSQL finds one: by its name in any case.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    Hex,
    Base64,
    Base64Url,
    Base32Hex,
    Escape,
}

fn format_of(name: &str) -> Result<Format> {
    Ok(match name.to_ascii_lowercase().as_str() {
        "hex" => Format::Hex,
        "base64" => Format::Base64,
        "base64url" => Format::Base64Url,
        "base32hex" => Format::Base32Hex,
        "escape" => Format::Escape,
        _ => return Err(Error::new(format!("unrecognized encoding: \"{name}\""))),
    })
}

/// A byte string written in a format: hex, base64 (a line break after each 76 characters),
/// base64url, base32hex or escape.
pub fn encode(b: &[u8], format: &str) -> Result<String> {
    let mut out = String::new();
    match format_of(format)? {
        Format::Hex => {
            for x in b {
                out.push_str(&format!("{x:02x}"));
            }
        }
        f @ (Format::Base64 | Format::Base64Url) => {
            let url = f == Format::Base64Url;
            let alphabet = if url { BASE64URL } else { BASE64 };
            let mut line = 0;
            let (chunks, rest) = b.as_chunks::<3>();
            for c in chunks {
                let buf = u32::from(c[0]) << 16 | u32::from(c[1]) << 8 | u32::from(c[2]);
                for shift in [18, 12, 6, 0] {
                    out.push(alphabet[((buf >> shift) & 0x3f) as usize] as char);
                }
                line += 4;
                if !url && line >= 76 {
                    out.push('\n');
                    line = 0;
                }
            }
            if !rest.is_empty() {
                let buf = u32::from(rest[0]) << 16 | rest.get(1).map_or(0, |&x| u32::from(x) << 8);
                out.push(alphabet[((buf >> 18) & 0x3f) as usize] as char);
                out.push(alphabet[((buf >> 12) & 0x3f) as usize] as char);
                if rest.len() == 2 {
                    out.push(alphabet[((buf >> 6) & 0x3f) as usize] as char);
                    if !url {
                        out.push('=');
                    }
                } else if !url {
                    out.push_str("==");
                }
            }
        }
        Format::Base32Hex => {
            let (mut buffer, mut bits) = (0u32, 0);
            for &x in b {
                buffer = (buffer << 8) | u32::from(x);
                bits += 8;
                while bits >= 5 {
                    bits -= 5;
                    out.push(BASE32HEX[((buffer >> bits) & 0x1f) as usize] as char);
                    buffer &= (1u32 << bits) - 1;
                }
            }
            if bits > 0 {
                out.push(BASE32HEX[((buffer << (5 - bits)) & 0x1f) as usize] as char);
            }
            while !out.len().is_multiple_of(8) {
                out.push('=');
            }
        }
        Format::Escape => {
            for &c in b {
                if c == 0 || c >= 0x80 {
                    out.push('\\');
                    out.push((b'0' + (c >> 6)) as char);
                    out.push((b'0' + ((c >> 3) & 7)) as char);
                    out.push((b'0' + (c & 7)) as char);
                } else if c == b'\\' {
                    out.push_str("\\\\");
                } else {
                    out.push(c as char);
                }
            }
        }
    }
    Ok(out)
}

/// The character of a text that begins at byte `at`, as PostgreSQL names one in a refusal.
fn char_at(s: &str, at: usize) -> &str {
    let end = s[at..]
        .char_indices()
        .nth(1)
        .map_or(s.len(), |(i, _)| at + i);
    &s[at..end]
}

fn hex_value(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

fn is_blank(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r')
}

/// A byte string read back from a format, refused as PostgreSQL refuses a text not written in it.
pub fn decode(s: &str, format: &str) -> Result<Vec<u8>> {
    let f = format_of(format)?;
    let b = s.as_bytes();
    let mut out = Vec::new();
    match f {
        Format::Hex => {
            let mut i = 0;
            while i < b.len() {
                if matches!(b[i], b' ' | b'\n' | b'\t' | b'\r') {
                    i += 1;
                    continue;
                }
                let hi = hex_value(b[i]).ok_or_else(|| {
                    Error::new(format!("invalid hexadecimal digit: \"{}\"", char_at(s, i)))
                })?;
                i += 1;
                if i >= b.len() {
                    return Err(Error::new("invalid hexadecimal data: odd number of digits"));
                }
                let lo = hex_value(b[i]).ok_or_else(|| {
                    Error::new(format!("invalid hexadecimal digit: \"{}\"", char_at(s, i)))
                })?;
                i += 1;
                out.push(hi << 4 | lo);
            }
        }
        Format::Base64 | Format::Base64Url => {
            let url = f == Format::Base64Url;
            let name = if url { "base64url" } else { "base64" };
            let (mut buf, mut pos, mut end) = (0u32, 0, 0);
            for (i, &raw) in b.iter().enumerate() {
                if is_blank(raw) {
                    continue;
                }
                let c = match (url, raw) {
                    (true, b'-') => b'+',
                    (true, b'_') => b'/',
                    _ => raw,
                };
                let v = if c == b'=' {
                    if end == 0 {
                        end = match pos {
                            2 => 1,
                            3 => 2,
                            _ => {
                                return Err(Error::new(format!(
                                    "unexpected \"=\" while decoding {name} sequence"
                                )))
                            }
                        };
                    }
                    0
                } else {
                    match BASE64
                        .iter()
                        .position(|&x| x == c)
                        .filter(|_| c > 0 && c < 127)
                    {
                        Some(v) => v as u32,
                        None => {
                            return Err(Error::new(format!(
                                "invalid symbol \"{}\" found while decoding {name} sequence",
                                char_at(s, i)
                            )))
                        }
                    }
                };
                buf = (buf << 6) + v;
                pos += 1;
                if pos == 4 {
                    out.push((buf >> 16) as u8);
                    if end == 0 || end > 1 {
                        out.push((buf >> 8) as u8);
                    }
                    if end == 0 || end > 2 {
                        out.push(buf as u8);
                    }
                    buf = 0;
                    pos = 0;
                }
            }
            if url && pos == 2 {
                buf <<= 12;
                out.push((buf >> 16) as u8);
            } else if url && pos == 3 {
                buf <<= 6;
                out.push((buf >> 16) as u8);
                out.push((buf >> 8) as u8);
            } else if pos != 0 {
                return Err(Error::new(format!("invalid {name} end sequence")));
            }
        }
        Format::Base32Hex => {
            let (mut buffer, mut bits, mut pos, mut end) = (0u32, 0, 0, false);
            for (i, &c) in b.iter().enumerate() {
                if is_blank(c) {
                    continue;
                }
                if c == b'=' {
                    if !end {
                        if !matches!(pos, 2 | 4 | 5 | 7) {
                            return Err(Error::new(
                                "unexpected \"=\" while decoding base32hex sequence",
                            ));
                        }
                        end = true;
                    }
                    pos += 1;
                    continue;
                }
                let refused = || {
                    Error::new(format!(
                        "invalid symbol \"{}\" found while decoding base32hex sequence",
                        char_at(s, i)
                    ))
                };
                if end {
                    return Err(refused());
                }
                let v = match c {
                    b'0'..=b'9' => c - b'0',
                    b'A'..=b'V' => c - b'A' + 10,
                    b'a'..=b'v' => c - b'a' + 10,
                    _ => return Err(refused()),
                };
                buffer = (buffer << 5) | u32::from(v);
                bits += 5;
                pos += 1;
                while bits >= 8 {
                    bits -= 8;
                    out.push((buffer >> bits) as u8);
                    buffer &= (1u32 << bits) - 1;
                }
                if pos == 8 {
                    pos = 0;
                }
            }
        }
        Format::Escape => {
            let refused = || Error::new("invalid input syntax for type bytea");
            let octal = |x: u8, top: u8| (b'0'..=top).contains(&x);
            let mut i = 0;
            while i < b.len() {
                if b[i] != b'\\' {
                    out.push(b[i]);
                    i += 1;
                } else if i + 3 < b.len()
                    && octal(b[i + 1], b'3')
                    && octal(b[i + 2], b'7')
                    && octal(b[i + 3], b'7')
                {
                    out.push((b[i + 1] - b'0') << 6 | (b[i + 2] - b'0') << 3 | (b[i + 3] - b'0'));
                    i += 4;
                } else if i + 1 < b.len() && b[i + 1] == b'\\' {
                    out.push(b'\\');
                    i += 2;
                } else {
                    return Err(refused());
                }
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Bytes and bits.

/// get_byte: the byte at `n`, counted from 0.
pub fn get_byte(b: &[u8], n: i64) -> Result<i64> {
    if n < 0 || n >= b.len() as i64 {
        return Err(Error::new(format!(
            "index {n} out of valid range, 0..{}",
            b.len() as i64 - 1
        )));
    }
    Ok(i64::from(b[n as usize]))
}

/// set_byte: the byte at `n` set to the low byte of `v`.
pub fn set_byte(b: &[u8], n: i64, v: i64) -> Result<Vec<u8>> {
    if n < 0 || n >= b.len() as i64 {
        return Err(Error::new(format!(
            "index {n} out of valid range, 0..{}",
            b.len() as i64 - 1
        )));
    }
    let mut out = b.to_vec();
    out[n as usize] = v as u8;
    Ok(out)
}

/// get_bit: the bit at `n`, each byte's least bit first.
pub fn get_bit(b: &[u8], n: i64) -> Result<i64> {
    let bits = b.len() as i64 * 8;
    if n < 0 || n >= bits {
        return Err(Error::new(format!(
            "index {n} out of valid range, 0..{}",
            bits - 1
        )));
    }
    Ok(i64::from(b[(n / 8) as usize] >> (n % 8) & 1))
}

/// set_bit: the bit at `n` set to `v`, which is 0 or 1.
pub fn set_bit(b: &[u8], n: i64, v: i64) -> Result<Vec<u8>> {
    let bits = b.len() as i64 * 8;
    if n < 0 || n >= bits {
        return Err(Error::new(format!(
            "index {n} out of valid range, 0..{}",
            bits - 1
        )));
    }
    if v != 0 && v != 1 {
        return Err(Error::new("new bit must be 0 or 1"));
    }
    let mut out = b.to_vec();
    let (byte, bit) = ((n / 8) as usize, n % 8);
    if v == 0 {
        out[byte] &= !(1 << bit);
    } else {
        out[byte] |= 1 << bit;
    }
    Ok(out)
}

/// bit_count: the bits set.
pub fn bit_count(b: &[u8]) -> i64 {
    b.iter().map(|x| i64::from(x.count_ones())).sum()
}

// ---------------------------------------------------------------------------------------------
// The encodings.

/// PostgreSQL 19's encodings by code: each one's name and its longest character in bytes; code 7,
/// once MULE_INTERNAL, names none.
const ENCODINGS: [(&str, i32); 42] = [
    ("SQL_ASCII", 1),
    ("EUC_JP", 3),
    ("EUC_CN", 3),
    ("EUC_KR", 3),
    ("EUC_TW", 4),
    ("EUC_JIS_2004", 3),
    ("UTF8", 4),
    ("", 0),
    ("LATIN1", 1),
    ("LATIN2", 1),
    ("LATIN3", 1),
    ("LATIN4", 1),
    ("LATIN5", 1),
    ("LATIN6", 1),
    ("LATIN7", 1),
    ("LATIN8", 1),
    ("LATIN9", 1),
    ("LATIN10", 1),
    ("WIN1256", 1),
    ("WIN1258", 1),
    ("WIN866", 1),
    ("WIN874", 1),
    ("KOI8R", 1),
    ("WIN1251", 1),
    ("WIN1252", 1),
    ("ISO_8859_5", 1),
    ("ISO_8859_6", 1),
    ("ISO_8859_7", 1),
    ("ISO_8859_8", 1),
    ("WIN1250", 1),
    ("WIN1253", 1),
    ("WIN1254", 1),
    ("WIN1255", 1),
    ("WIN1257", 1),
    ("KOI8U", 1),
    ("SJIS", 2),
    ("BIG5", 2),
    ("GBK", 2),
    ("UHC", 2),
    ("GB18030", 4),
    ("JOHAB", 3),
    ("SHIFT_JIS_2004", 2),
];

const SQL_ASCII: i32 = 0;
const UTF8: i32 = 6;
const LATIN1: i32 = 8;

/// The names PostgreSQL reads an encoding by, cleaned of all but letters and digits and lowered,
/// each with its code.
const NAMES: [(&str, i32); 80] = [
    ("abc", 19),
    ("alt", 20),
    ("big5", 36),
    ("euccn", 2),
    ("eucjis2004", 5),
    ("eucjp", 1),
    ("euckr", 3),
    ("euctw", 4),
    ("gb18030", 39),
    ("gbk", 37),
    ("iso88591", 8),
    ("iso885910", 13),
    ("iso885913", 14),
    ("iso885914", 15),
    ("iso885915", 16),
    ("iso885916", 17),
    ("iso88592", 9),
    ("iso88593", 10),
    ("iso88594", 11),
    ("iso88595", 25),
    ("iso88596", 26),
    ("iso88597", 27),
    ("iso88598", 28),
    ("iso88599", 12),
    ("johab", 40),
    ("koi8", 22),
    ("koi8r", 22),
    ("koi8u", 34),
    ("latin1", 8),
    ("latin10", 17),
    ("latin2", 9),
    ("latin3", 10),
    ("latin4", 11),
    ("latin5", 12),
    ("latin6", 13),
    ("latin7", 14),
    ("latin8", 15),
    ("latin9", 16),
    ("mskanji", 35),
    ("shiftjis", 35),
    ("shiftjis2004", 41),
    ("sjis", 35),
    ("sqlascii", 0),
    ("tcvn", 19),
    ("tcvn5712", 19),
    ("uhc", 38),
    ("unicode", 6),
    ("utf8", 6),
    ("vscii", 19),
    ("win", 23),
    ("win1250", 29),
    ("win1251", 23),
    ("win1252", 24),
    ("win1253", 30),
    ("win1254", 31),
    ("win1255", 32),
    ("win1256", 18),
    ("win1257", 33),
    ("win1258", 19),
    ("win866", 20),
    ("win874", 21),
    ("win932", 35),
    ("win936", 37),
    ("win949", 38),
    ("win950", 36),
    ("windows1250", 29),
    ("windows1251", 23),
    ("windows1252", 24),
    ("windows1253", 30),
    ("windows1254", 31),
    ("windows1255", 32),
    ("windows1256", 18),
    ("windows1257", 33),
    ("windows1258", 19),
    ("windows866", 20),
    ("windows874", 21),
    ("windows932", 35),
    ("windows936", 37),
    ("windows949", 38),
    ("windows950", 36),
];

fn valid(code: i32) -> bool {
    (0..ENCODINGS.len() as i32).contains(&code) && code != 7
}

/// pg_encoding_to_char: an encoding's name by its code; empty for no encoding.
pub fn pg_encoding_to_char(code: i32) -> String {
    if valid(code) {
        ENCODINGS[code as usize].0.to_string()
    } else {
        String::new()
    }
}

/// A text as a value of type name, PostgreSQL's: cut to its first 63 bytes, at a character's end.
fn as_name(s: &str) -> &str {
    let mut cut = s.len().min(63);
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    &s[..cut]
}

/// pg_char_to_encoding: an encoding's code by any of its names, in any case and with any marks
/// between its letters and digits; -1 for none.
pub fn pg_char_to_encoding(name: &str) -> i32 {
    let name = as_name(name);
    if name.is_empty() {
        return -1;
    }
    let key: String = name
        .bytes()
        .filter(u8::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase() as char)
        .collect();
    NAMES
        .iter()
        .find(|(n, _)| *n == key)
        .map_or(-1, |&(_, code)| code)
}

/// pg_encoding_max_length: an encoding's longest character in bytes; none for no encoding.
pub fn pg_encoding_max_length(code: i32) -> Option<i32> {
    valid(code).then(|| ENCODINGS[code as usize].1)
}

/// The bytes PostgreSQL names in a refusal of a character: those of the character that begins
/// there, as its encoding counts them, at most eight.
fn bytes_named(code: i32, b: &[u8]) -> String {
    let l = if b.is_empty() {
        usize::MAX
    } else if code == UTF8 {
        match b[0] {
            x if x & 0x80 == 0 => 1,
            x if x & 0xe0 == 0xc0 => 2,
            x if x & 0xf0 == 0xe0 => 3,
            x if x & 0xf8 == 0xf0 => 4,
            _ => 1,
        }
    } else {
        1
    };
    b.iter()
        .take(l.min(b.len()).min(8))
        .map(|x| format!("0x{x:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn invalid(code: i32, b: &[u8]) -> Error {
    Error::new(format!(
        "invalid byte sequence for encoding \"{}\": {}",
        ENCODINGS[code as usize].0,
        bytes_named(code, b)
    ))
}

/// The bytes of a UTF-8 character that is legal, as PostgreSQL holds one: a lead byte, then
/// continuation bytes in the ranges its lead allows.
fn utf8_legal(s: &[u8]) -> bool {
    let a = s[0];
    if (0x80..0xC2).contains(&a) || a > 0xF4 {
        return false;
    }
    if s.len() >= 2 {
        let b = s[1];
        let ok = match a {
            0xE0 => (0xA0..=0xBF).contains(&b),
            0xED => (0x80..=0x9F).contains(&b),
            0xF0 => (0x90..=0xBF).contains(&b),
            0xF4 => (0x80..=0x8F).contains(&b),
            _ => (0x80..=0xBF).contains(&b),
        };
        if !ok {
            return false;
        }
    }
    s[2.min(s.len())..]
        .iter()
        .all(|&c| (0x80..=0xBF).contains(&c))
}

/// How many bytes from the start of a string are valid in an encoding the snail reads.
fn valid_prefix(code: i32, b: &[u8]) -> usize {
    if code != UTF8 {
        return b.iter().position(|&x| x == 0).unwrap_or(b.len());
    }
    let mut i = 0;
    while i < b.len() {
        let l = match b[i] {
            0 => return i,
            x if x & 0x80 == 0 => 1,
            x if x & 0xe0 == 0xc0 => 2,
            x if x & 0xf0 == 0xe0 => 3,
            x if x & 0xf8 == 0xf0 => 4,
            _ => return i,
        };
        if i + l > b.len() || !utf8_legal(&b[i..i + l]) {
            return i;
        }
        i += l;
    }
    i
}

/// A string verified in its encoding, as pg_verify_mbstr verifies it.
fn verify(code: i32, b: &[u8]) -> Result<()> {
    let ok = valid_prefix(code, b);
    if ok != b.len() {
        return Err(invalid(code, &b[ok..]));
    }
    Ok(())
}

/// An encoding by its name: refused where PostgreSQL knows none of that name, or where it is one
/// the snail does not read.
fn encoding_named(name: &str, side: &str) -> Result<i32> {
    let code = pg_char_to_encoding(name);
    if code < 0 {
        return Err(Error::new(format!(
            "invalid {side} encoding name \"{}\"",
            as_name(name)
        )));
    }
    Ok(code)
}

fn not_read(code: i32) -> Error {
    Error::new(format!(
        "the encoding {} is not read: a snail converts between UTF8, SQL_ASCII and LATIN1",
        ENCODINGS[code as usize].0
    ))
}

/// convert: a string in one encoding written in another, as pg_convert writes it.
pub fn convert(b: &[u8], from: &str, to: &str) -> Result<Vec<u8>> {
    let src = encoding_named(from, "source")?;
    let dest = encoding_named(to, "destination")?;
    let read = |c: i32| matches!(c, SQL_ASCII | UTF8 | LATIN1);
    if !read(src) {
        if b.is_empty() {
            return Ok(Vec::new());
        }
        return Err(not_read(src));
    }
    verify(src, b)?;
    if b.is_empty() || src == dest || dest == SQL_ASCII {
        return Ok(b.to_vec());
    }
    if !read(dest) {
        return Err(not_read(dest));
    }
    if src == SQL_ASCII {
        verify(dest, b)?;
        return Ok(b.to_vec());
    }
    let mut out = Vec::with_capacity(b.len() * 2);
    if src == LATIN1 {
        for &c in b {
            if c < 0x80 {
                out.push(c);
            } else {
                out.push((c >> 6) | 0xc0);
                out.push((c & 0x3f) | 0x80);
            }
        }
        return Ok(out);
    }
    let untranslatable = |at: &[u8]| {
        Error::new(format!(
            "character with byte sequence {} in encoding \"UTF8\" has no equivalent in encoding \"LATIN1\"",
            bytes_named(UTF8, at)
        ))
    };
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c < 0x80 {
            out.push(c);
            i += 1;
            continue;
        }
        let l = match c {
            x if x & 0xe0 == 0xc0 => 2,
            x if x & 0xf0 == 0xe0 => 3,
            x if x & 0xf8 == 0xf0 => 4,
            _ => 1,
        };
        if l != 2 {
            return Err(untranslatable(&b[i..]));
        }
        let v = (u32::from(c & 0x1f) << 6) | u32::from(b[i + 1] & 0x3f);
        if !(0x80..=0xff).contains(&v) {
            return Err(untranslatable(&b[i..]));
        }
        out.push(v as u8);
        i += 2;
    }
    Ok(out)
}

/// convert_from: a string in an encoding written as the database's text, UTF8.
pub fn convert_from(b: &[u8], encoding: &str) -> Result<String> {
    let out = convert(b, encoding, "UTF8")?;
    String::from_utf8(out).map_err(|e| invalid(UTF8, &e.into_bytes()[..]))
}

/// convert_to: the database's text written in an encoding.
pub fn convert_to(s: &str, encoding: &str) -> Result<Vec<u8>> {
    convert(s.as_bytes(), "UTF8", encoding)
}

/// to_ascii's letters for the bytes of LATIN1, LATIN2 and LATIN9 from 160, and of WIN1250 from
/// 128.
const ASCII_LATIN1: &[u8] =
    b"  cL Y  \"Ca  -R     'u .,      ?AAAAAAACEEEEIIII NOOOOOxOUUUUYTBaaaaaaaceeeeiiii nooooo/ouuuuyty";
const ASCII_LATIN2: &[u8] =
    b" A L LS \"SSTZ-ZZ a,l'ls ,sstz\"zzRAAAALCCCEEEEIIDDNNOOOOxRUUUUYTBraaaalccceeeeiiddnnoooo/ruuuuyt.";
const ASCII_LATIN9: &[u8] =
    b"  cL YS sCa  -R     Zu .z   EeY?AAAAAAACEEEEIIII NOOOOOxOUUUUYTBaaaaaaaceeeeiiii nooooo/ouuuuyty";
const ASCII_WIN1250: &[u8] =
    b"  ' \"    %S<STZZ `'\"\".--  s>stzz   L A  \"CS  -RZ  ,l'u .,as L\"lzRAAAALCCCEEEEIIDDNNOOOOxRUUUUYTBraaaalccceeeeiiddnnoooo/ruuuuyt ";

/// to_ascii of a text's bytes read in an encoding by its code: each byte past ASCII its letter in
/// LATIN1, LATIN2, LATIN9 or WIN1250, any other encoding refused.
pub fn to_ascii_code(s: &str, code: i32) -> Result<String> {
    if !valid(code) {
        return Err(Error::new(format!("{code} is not a valid encoding code")));
    }
    let (table, range) = match code {
        8 => (ASCII_LATIN1, 160),
        9 => (ASCII_LATIN2, 160),
        16 => (ASCII_LATIN9, 160),
        29 => (ASCII_WIN1250, 128),
        _ => {
            return Err(Error::new(format!(
                "encoding conversion from {} to ASCII not supported",
                ENCODINGS[code as usize].0
            )))
        }
    };
    Ok(s.bytes()
        .map(|x| {
            if x < 128 {
                x as char
            } else if (x as usize) < range {
                ' '
            } else {
                table[x as usize - range] as char
            }
        })
        .collect())
}

/// to_ascii of a text in the encoding named, or in the database's, UTF8, which it refuses.
pub fn to_ascii(s: &str, encoding: Option<&str>) -> Result<String> {
    let code = match encoding {
        None => UTF8,
        Some(name) => {
            let code = pg_char_to_encoding(name);
            if code < 0 {
                return Err(Error::new(format!(
                    "{} is not a valid encoding name",
                    as_name(name)
                )));
            }
            code
        }
    };
    to_ascii_code(s, code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_digests_are_the_algorithms_own() {
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            encode(&sha256(b"abc"), "hex").unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            encode(&sha224(b""), "hex").unwrap(),
            "d14a028c2a3a2bc9476102bb288234c415a2b01f828ea62ac5b3e42f"
        );
        assert_eq!(
            &encode(&sha512(b"abc"), "hex").unwrap()[..32],
            "ddaf35a193617abacc417349ae204131"
        );
        assert_eq!(
            &encode(&sha384(b""), "hex").unwrap()[..32],
            "38b060a751ac96384cd9327eb1b1e36a"
        );
        assert_eq!(crc32(b"123456789"), 0xCBF43926);
        assert_eq!(crc32c(b"123456789"), 0xE3069283);
    }

    #[test]
    fn each_format_comes_back_as_it_went() {
        let b: Vec<u8> = (0..=255u8).collect();
        for f in ["hex", "base64", "base64url", "base32hex", "escape"] {
            assert_eq!(decode(&encode(&b, f).unwrap(), f).unwrap(), b, "{f}");
        }
        // base64 breaks its lines after 76 characters, the last line too.
        assert!(encode(&[0; 57], "base64").unwrap().ends_with("A\n"));
        assert_eq!(
            decode("é", "hex").unwrap_err().0,
            "invalid hexadecimal digit: \"é\""
        );
        assert_eq!(
            encode(b"x", "Bogus").unwrap_err().0,
            "unrecognized encoding: \"Bogus\""
        );
    }

    #[test]
    fn the_encodings_convert_and_refuse_as_postgresql_s() {
        assert_eq!(pg_char_to_encoding("Latin-1"), 8);
        assert_eq!(pg_encoding_to_char(7), "");
        assert_eq!(pg_encoding_max_length(6), Some(4));
        assert_eq!(convert_to("é", "LATIN1").unwrap(), [0xe9]);
        assert_eq!(convert_from(&[0xe9], "LATIN1").unwrap(), "é");
        assert_eq!(
            convert_to("€", "LATIN1").unwrap_err().0,
            "character with byte sequence 0xe2 0x82 0xac in encoding \"UTF8\" has no equivalent in encoding \"LATIN1\""
        );
        assert_eq!(
            convert_from(&[0xc3], "UTF8").unwrap_err().0,
            "invalid byte sequence for encoding \"UTF8\": 0xc3"
        );
        assert_eq!(to_ascii("é", Some("LATIN1")).unwrap(), "AC");
    }
}
