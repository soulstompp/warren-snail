//! The network addresses, inet and cidr: each read as PostgreSQL's `network_in` reads it, written as
//! `network_out` writes it, keyed as `network_cmp` orders it, and its operators and casts as
//! network.c has them.
//!
//! A value crosses as the text `inet_out` or `cidr_out` writes, the text the snail holds. An inet
//! and a cidr are held alike, the type aside, so either text serves where an operator takes an
//! inet, and every operator gives an inet. A held text that `inet_in` does not read is refused
//! where a function returns a `Result`, and elsewhere taken as `0.0.0.0/0`.

use crate::{Error, Result};
use std::cmp::Ordering;

/// An address as PostgreSQL holds one (utils/inet.h): its family, its mask's length in bits, and
/// its bytes in network order, the first 4 of them for IPv4 and all 16 for IPv6.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Inet {
    v6: bool,
    bits: u8,
    addr: [u8; 16],
}

impl Inet {
    /// `ip_addrsize`.
    fn size(&self) -> usize {
        if self.v6 {
            16
        } else {
            4
        }
    }

    /// `ip_maxbits`.
    fn max_bits(&self) -> u8 {
        if self.v6 {
            128
        } else {
            32
        }
    }

    fn bytes(&self) -> &[u8] {
        &self.addr[..self.size()]
    }

    /// The address written as `pg_inet_net_ntop` writes it with a mask of `bits`.
    fn ntop(&self, bits: u8) -> String {
        if self.v6 {
            ntop_ipv6(&self.addr, bits)
        } else {
            let a = &self.addr;
            let mut out = format!("{}.{}.{}.{}", a[0], a[1], a[2], a[3]);
            if bits != 32 {
                out.push_str(&format!("/{bits}"));
            }
            out
        }
    }

    /// The text `network_out` writes: an inet's mask left off where it is full, a cidr's always
    /// written.
    fn out(&self, cidr: bool) -> String {
        let mut out = self.ntop(self.bits);
        if cidr && !out.contains('/') {
            out.push_str(&format!("/{}", self.bits));
        }
        out
    }

    /// The value with its mask's length kept and every bit right of the mask cleared, as
    /// `cidr_set_masklen_internal` makes it.
    fn network(&self) -> Inet {
        let mut out = Inet {
            v6: self.v6,
            bits: self.bits,
            addr: [0; 16],
        };
        let bits = self.bits as usize;
        out.addr[..bits.div_ceil(8)].copy_from_slice(&self.addr[..bits.div_ceil(8)]);
        if !bits.is_multiple_of(8) {
            out.addr[bits / 8] &= !(0xff >> (bits % 8));
        }
        out
    }
}

/// The value a held text stands for, as `inet_in` reads it; `0.0.0.0/0` where it does not read.
fn held(text: &str) -> Inet {
    read(text, false).unwrap_or_default()
}

/// The byte at `i` of a C string: zero at its end and past it.
fn at(s: &[u8], i: usize) -> u8 {
    s.get(i).copied().unwrap_or(0)
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// The mask's length after an IPv4 address's `/`, as `inet_net_pton_ipv4` and
/// `inet_cidr_pton_ipv4` read it: decimal digits to the end, reckoned in C's int as PostgreSQL's
/// build wraps it, at most 32.
fn ipv4_mask(digits: &[u8]) -> Option<i32> {
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let bits = digits.iter().fold(0i32, |b, d| {
        b.wrapping_mul(10).wrapping_add(i32::from(d - b'0'))
    });
    (bits <= 32).then_some(bits)
}

/// An inet's IPv4 address as `inet_net_pton_ipv4` reads it into `dst`: one to four decimal octets,
/// each followed by a dot or not, then `/` and the mask's length; without a mask, all four octets
/// and a mask of 32. The mask's length, or none where it does not read.
fn inet_ipv4(s: &[u8], dst: &mut [u8; 16]) -> Option<i32> {
    let (mut i, mut n) = (0, 0);
    let mut ch;
    loop {
        ch = at(s, i);
        i += 1;
        if !ch.is_ascii_digit() {
            break;
        }
        let mut octet = 0u32;
        while ch.is_ascii_digit() {
            octet = octet * 10 + u32::from(ch - b'0');
            if octet > 255 {
                return None;
            }
            ch = at(s, i);
            i += 1;
        }
        if n == 4 {
            return None;
        }
        dst[n] = octet as u8;
        n += 1;
        if ch == 0 || ch == b'/' {
            break;
        }
        if ch != b'.' {
            return None;
        }
    }
    let mut bits = -1;
    if ch == b'/' && at(s, i).is_ascii_digit() && n > 0 {
        bits = ipv4_mask(&s[i..])?;
        ch = 0;
    }
    if ch != 0 {
        return None;
    }
    if bits == -1 {
        if n != 4 {
            return None;
        }
        bits = 32;
    }
    if n == 0 || bits / 8 > n as i32 {
        return None;
    }
    Some(bits)
}

/// A cidr's IPv4 address as `inet_cidr_pton_ipv4` reads it into `dst`: `0x` and up to eight hex
/// digits, or one to four decimal octets joined by dots, then `/` and the mask's length; without a
/// mask, the length its first octet's class gives, widened to the octets written. The mask's
/// length, or none where it does not read.
fn cidr_ipv4(s: &[u8], dst: &mut [u8; 16]) -> Option<i32> {
    let mut n = 0;
    let mut ch = at(s, 0);
    let mut i = 1;
    if ch == b'0' && matches!(at(s, 1), b'x' | b'X') && at(s, 2).is_ascii_hexdigit() {
        i = 2;
        let (mut tmp, mut nybbles) = (0u8, 0);
        loop {
            ch = at(s, i);
            i += 1;
            let Some(d) = hex_digit(ch) else {
                break;
            };
            tmp = if nybbles == 0 { d } else { (tmp << 4) | d };
            nybbles += 1;
            if nybbles == 2 {
                if n == 4 {
                    return None;
                }
                dst[n] = tmp;
                n += 1;
                nybbles = 0;
            }
        }
        if nybbles == 1 {
            if n == 4 {
                return None;
            }
            dst[n] = tmp << 4;
            n += 1;
        }
    } else if ch.is_ascii_digit() {
        loop {
            let mut octet = 0u32;
            while ch.is_ascii_digit() {
                octet = octet * 10 + u32::from(ch - b'0');
                if octet > 255 {
                    return None;
                }
                ch = at(s, i);
                i += 1;
            }
            if n == 4 {
                return None;
            }
            dst[n] = octet as u8;
            n += 1;
            if ch == 0 || ch == b'/' {
                break;
            }
            if ch != b'.' {
                return None;
            }
            ch = at(s, i);
            i += 1;
            if !ch.is_ascii_digit() {
                return None;
            }
        }
    } else {
        return None;
    }
    let mut bits = -1;
    if ch == b'/' && at(s, i).is_ascii_digit() && n > 0 {
        bits = ipv4_mask(&s[i..])?;
        ch = 0;
    }
    if ch != 0 || n == 0 {
        return None;
    }
    if bits == -1 {
        bits = match dst[0] {
            240.. => 32,
            224.. => 8,
            192.. => 24,
            128.. => 16,
            _ => 8,
        };
        bits = bits.max(8 * n as i32);
        if bits == 8 && dst[0] == 224 {
            bits = 4;
        }
    }
    while bits > 8 * n as i32 {
        if n == 4 {
            return None;
        }
        dst[n] = 0;
        n += 1;
    }
    Some(bits)
}

/// The mask's length after an IPv6 address's `/`, as `getbits` reads it: decimal digits to the
/// end, no leading zero, at most 128.
fn getbits(s: &[u8]) -> Option<i32> {
    let mut val = 0;
    for (n, &ch) in s.iter().enumerate() {
        if !ch.is_ascii_digit() || (n != 0 && val == 0) {
            return None;
        }
        val = val * 10 + i32::from(ch - b'0');
        if val > 128 {
            return None;
        }
    }
    (!s.is_empty()).then_some(val)
}

/// The IPv4 address ending an IPv6 one, as `getv4` reads it into `dst`: up to four decimal octets,
/// none with a leading zero, joined by dots, then `/` and the mask's length or the end. Where it
/// reads, whether a mask was read and its length.
fn getv4(s: &[u8], dst: &mut [u8]) -> Option<Option<i32>> {
    let (mut n, mut val, mut digits) = (0, 0u32, 0);
    for (i, &ch) in s.iter().enumerate() {
        if ch.is_ascii_digit() {
            if digits != 0 && val == 0 {
                return None;
            }
            digits += 1;
            val = val * 10 + u32::from(ch - b'0');
            if val > 255 {
                return None;
            }
        } else if ch == b'.' || ch == b'/' {
            if n > 3 {
                return None;
            }
            dst[n] = val as u8;
            n += 1;
            if ch == b'/' {
                return getbits(&s[i + 1..]).map(Some);
            }
            (val, digits) = (0, 0);
        } else {
            return None;
        }
    }
    if digits == 0 || n > 3 {
        return None;
    }
    dst[n] = val as u8;
    Some(None)
}

/// An IPv6 address as `inet_cidr_pton_ipv6` reads it into `dst`: up to eight groups of one to four
/// hex digits joined by colons, one `::` standing for one or more zero groups, the last four bytes
/// an IPv4 address or not, then `/` and the mask's length; without a mask, 128. The mask's length,
/// or none where it does not read.
fn pton_ipv6(s: &[u8], dst: &mut [u8; 16]) -> Option<i32> {
    let mut tmp = [0u8; 16];
    let (mut tp, mut colonp) = (0, None);
    let mut i = 0;
    if at(s, 0) == b':' {
        if at(s, 1) != b':' {
            return None;
        }
        i = 1;
    }
    let mut curtok = i;
    let (mut saw_xdigit, mut val, mut digits) = (false, 0u32, 0);
    let mut bits = -1;
    loop {
        let ch = at(s, i);
        i += 1;
        if ch == 0 {
            break;
        }
        if let Some(d) = hex_digit(ch) {
            val = (val << 4) | u32::from(d);
            digits += 1;
            if digits > 4 {
                return None;
            }
            saw_xdigit = true;
            continue;
        }
        if ch == b':' {
            curtok = i;
            if !saw_xdigit {
                if colonp.is_some() {
                    return None;
                }
                colonp = Some(tp);
                continue;
            }
            if at(s, i) == 0 || tp + 2 > 16 {
                return None;
            }
            tmp[tp..tp + 2].copy_from_slice(&(val as u16).to_be_bytes());
            tp += 2;
            (saw_xdigit, digits, val) = (false, 0, 0);
            continue;
        }
        if ch == b'.' && tp + 4 <= 16 {
            if let Some(mask) = getv4(&s[curtok..], &mut tmp[tp..tp + 4]) {
                bits = mask.unwrap_or(bits);
                tp += 4;
                saw_xdigit = false;
                break;
            }
        }
        if ch == b'/' {
            if let Some(mask) = getbits(&s[i..]) {
                bits = mask;
                break;
            }
        }
        return None;
    }
    if saw_xdigit {
        if tp + 2 > 16 {
            return None;
        }
        tmp[tp..tp + 2].copy_from_slice(&(val as u16).to_be_bytes());
        tp += 2;
    }
    if bits == -1 {
        bits = 128;
    }
    if let Some(c) = colonp {
        if tp == 16 {
            return None;
        }
        let n = tp - c;
        for k in 1..=n {
            tmp[16 - k] = tmp[c + n - k];
            tmp[c + n - k] = 0;
        }
        tp = 16;
    }
    if tp != 16 {
        return None;
    }
    *dst = tmp;
    Some(bits)
}

/// An IPv6 address as `inet_net_ntop_ipv6` writes it: its eight groups in lower-case hex without
/// leading zeros, the first longest run of two or more zero groups written `::`, the last four
/// bytes as an IPv4 address after six zero groups or after five and `ffff`, then `/` and the
/// mask's length unless it is 128.
fn ntop_ipv6(src: &[u8; 16], bits: u8) -> String {
    let words: Vec<u16> = src
        .chunks(2)
        .map(|w| u16::from_be_bytes([w[0], w[1]]))
        .collect();
    let (mut best, mut cur): (Option<(usize, usize)>, Option<(usize, usize)>) = (None, None);
    for (i, &w) in words.iter().enumerate() {
        if w == 0 {
            cur = Some(cur.map_or((i, 1), |(b, l)| (b, l + 1)));
        } else if let Some(c) = cur.take() {
            if best.is_none_or(|b| c.1 > b.1) {
                best = Some(c);
            }
        }
    }
    if let Some(c) = cur {
        if best.is_none_or(|b| c.1 > b.1) {
            best = Some(c);
        }
    }
    let best = best.filter(|b| b.1 >= 2);
    let mut out = String::with_capacity(48);
    for i in 0..8 {
        if let Some((base, len)) = best {
            if i >= base && i < base + len {
                if i == base {
                    out.push(':');
                }
                continue;
            }
        }
        if i != 0 {
            out.push(':');
        }
        let ipv4 = match best {
            Some((0, 6)) => true,
            Some((0, 7)) => words[7] != 1,
            Some((0, 5)) => words[5] == 0xffff,
            _ => false,
        };
        if i == 6 && ipv4 {
            out.push_str(&format!("{}.{}.{}.{}", src[12], src[13], src[14], src[15]));
            break;
        }
        out.push_str(&format!("{:x}", words[i]));
    }
    if best.is_some_and(|(base, len)| base + len == 8) {
        out.push(':');
    }
    if bits != 128 {
        out.push_str(&format!("/{bits}"));
    }
    out
}

/// Whether no bit right of the mask is set, as `addressOK` checks a cidr.
fn address_ok(v: &Inet) -> bool {
    v.network() == *v
}

/// A text read as `network_in` reads an inet or, where `cidr`, a cidr: IPv6 where it holds a
/// colon, else IPv4.
fn read(s: &str, cidr: bool) -> Result<Inet> {
    let b = s.as_bytes();
    let mut v = Inet {
        v6: b.contains(&b':'),
        ..Inet::default()
    };
    let bits = match (v.v6, cidr) {
        (true, _) => pton_ipv6(b, &mut v.addr),
        (false, false) => inet_ipv4(b, &mut v.addr),
        (false, true) => cidr_ipv4(b, &mut v.addr),
    };
    let ty = if cidr { "cidr" } else { "inet" };
    let bits = bits
        .filter(|&bits| (0..=i32::from(v.max_bits())).contains(&bits))
        .ok_or_else(|| Error::new(format!("invalid input syntax for type {ty}: \"{s}\"")))?;
    v.bits = bits as u8;
    if cidr && !address_ok(&v) {
        return Err(Error::new(format!("invalid cidr value: \"{s}\"")));
    }
    Ok(v)
}

/// An inet as `inet_in` reads it, or where `cidr` a cidr as `cidr_in` reads it, written as
/// `inet_out` or `cidr_out` writes it: an inet's mask left off where it is full, a cidr's always
/// written. A cidr with a bit set right of its mask is refused.
pub fn inet_in(s: &str, cidr: bool) -> Result<String> {
    Ok(read(s, cidr)?.out(cidr))
}

/// An inet's text, or where `cidr` a cidr's, from its key as `inet_key` writes it; none where it is
/// no such key.
pub fn inet_of_key(key: &[u8], cidr: bool) -> Option<String> {
    let v6 = match key.first()? {
        4 => false,
        6 => true,
        _ => return None,
    };
    let bits = key[1..].iter().position(|&b| b == 0)?;
    let bytes = &key[2 + bits..];
    let mut v = Inet {
        v6,
        bits: u8::try_from(bits).ok()?,
        addr: [0; 16],
    };
    (bytes.len() == v.size() && bits <= 8 * v.size()).then_some(())?;
    v.addr[..bytes.len()].copy_from_slice(bytes);
    Some(v.out(cidr))
}

/// The key of an inet or a cidr: its family, 4 before 6; each bit of its address up to its mask, a
/// byte each, 1 for a zero bit and 2 for a one; a zero; then its address's bytes. Its keys sort as
/// `network_cmp` orders the values, and are equal where it finds them equal.
pub fn inet_key(text: &str) -> Vec<u8> {
    let v = held(text);
    let mut out = Vec::with_capacity(2 + v.bits as usize + v.size());
    out.push(if v.v6 { 6 } else { 4 });
    out.extend((0..v.bits as usize).map(|r| 1 + ((v.addr[r / 8] >> (7 - r % 8)) & 1)));
    out.push(0);
    out.extend_from_slice(v.bytes());
    out
}

/// The first `n` bits of two addresses compared, as `bitncmp` compares them.
fn bitncmp(l: &[u8], r: &[u8], n: usize) -> Ordering {
    let b = n / 8;
    match l[..b].cmp(&r[..b]) {
        Ordering::Equal if !n.is_multiple_of(8) => {
            let mask = 0xffu8 << (8 - n % 8);
            (l[b] & mask).cmp(&(r[b] & mask))
        }
        o => o,
    }
}

/// Whether `a` and `b` are of one family, the mask of `a` reaching past that of `b` or, where
/// `eq`, as far, and their addresses alike up to the mask of `b`.
fn within(a: &Inet, b: &Inet, eq: bool) -> bool {
    a.v6 == b.v6
        && (a.bits > b.bits || (eq && a.bits == b.bits))
        && bitncmp(&a.addr, &b.addr, b.bits as usize).is_eq()
}

/// `a << b`, as `network_sub` tests it: `a` a network strictly inside `b`.
pub fn inet_sub(a: &str, b: &str) -> bool {
    within(&held(a), &held(b), false)
}

/// `a <<= b`, as `network_subeq` tests it: `a` inside `b` or the same network.
pub fn inet_subeq(a: &str, b: &str) -> bool {
    within(&held(a), &held(b), true)
}

/// `a >> b`, as `network_sup` tests it: `b` a network strictly inside `a`.
pub fn inet_sup(a: &str, b: &str) -> bool {
    within(&held(b), &held(a), false)
}

/// `a >>= b`, as `network_supeq` tests it: `b` inside `a` or the same network.
pub fn inet_supeq(a: &str, b: &str) -> bool {
    within(&held(b), &held(a), true)
}

/// The addresses an inet's network spans, its first and its last, as bytes that sort by family, 4
/// before 6, then by address.
pub fn inet_span(text: &str) -> (Vec<u8>, Vec<u8>) {
    let v = held(text);
    let family = if v.v6 { 6 } else { 4 };
    let (mut lo, mut hi) = (vec![family], vec![family]);
    for (i, &b) in v.bytes().iter().enumerate() {
        let kept = (v.bits as usize).saturating_sub(i * 8).min(8);
        let mask = if kept == 0 { 0 } else { 0xffu8 << (8 - kept) };
        lo.push(b & mask);
        hi.push(b | !mask);
    }
    (lo, hi)
}

/// `a && b`, as `network_overlap` tests it: one family, the addresses alike up to the shorter
/// mask.
pub fn inet_overlap(a: &str, b: &str) -> bool {
    let (a, b) = (held(a), held(b));
    a.v6 == b.v6 && bitncmp(&a.addr, &b.addr, a.bits.min(b.bits) as usize).is_eq()
}

/// `~a`, as `inetnot` makes it: every bit of the address flipped, the mask kept; an inet.
pub fn inet_not(a: &str) -> Result<String> {
    let mut v = read(a, false)?;
    let size = v.size();
    for b in &mut v.addr[..size] {
        *b = !*b;
    }
    Ok(v.out(false))
}

/// The bytes of two addresses of one family joined by `f`, the longer mask kept, as `inetand` and
/// `inetor` make them; refused across families with `what`'s message.
fn bitwise(a: &str, b: &str, what: &str, f: fn(u8, u8) -> u8) -> Result<String> {
    let (mut a, b) = (read(a, false)?, read(b, false)?);
    if a.v6 != b.v6 {
        return Err(Error::new(format!(
            "cannot {what} inet values of different sizes"
        )));
    }
    for (x, y) in a.addr.iter_mut().zip(b.addr) {
        *x = f(*x, y);
    }
    a.bits = a.bits.max(b.bits);
    Ok(a.out(false))
}

/// `a & b`, as `inetand` makes it: the addresses' bytes and-ed, the longer mask; an inet.
pub fn inet_and(a: &str, b: &str) -> Result<String> {
    bitwise(a, b, "AND", |x, y| x & y)
}

/// `a | b`, as `inetor` makes it: the addresses' bytes or-ed, the longer mask; an inet.
pub fn inet_or(a: &str, b: &str) -> Result<String> {
    bitwise(a, b, "OR", |x, y| x | y)
}

/// `a + n`, and `n + a`, as `internal_inetpl` makes it: the address as a number, `n` added, the
/// mask kept; an inet. Refused where the sum leaves the family's addresses.
pub fn inet_plus(a: &str, n: i64) -> Result<String> {
    let mut v = read(a, false)?;
    let (mut addend, mut carry) = (n, 0i32);
    for i in (0..v.size()).rev() {
        carry += i32::from(v.addr[i]) + (addend & 0xff) as i32;
        v.addr[i] = (carry & 0xff) as u8;
        carry >>= 8;
        addend = (addend & !0xff) / 0x100;
    }
    if !((addend == 0 && carry == 0) || (addend == -1 && carry == 1)) {
        return Err(Error::new("result is out of range"));
    }
    Ok(v.out(false))
}

/// `a - n`, as `inetmi_int8` makes it: `a + (-n)`, the negation as PostgreSQL's build wraps it.
pub fn inet_minus_int(a: &str, n: i64) -> Result<String> {
    inet_plus(a, n.wrapping_neg())
}

/// `a - b`, as `inetmi` makes it: the addresses as numbers, `b` taken from `a`, in a bigint.
/// Refused across families, and where the difference leaves a bigint.
pub fn inet_minus(a: &str, b: &str) -> Result<i64> {
    let (a, b) = (read(a, false)?, read(b, false)?);
    if a.v6 != b.v6 {
        return Err(Error::new("cannot subtract inet values of different sizes"));
    }
    let (mut res, mut carry) = (0i64, 1i32);
    let size = a.size();
    for (byte, i) in (0..size).rev().enumerate() {
        carry += i32::from(a.addr[i]) + i32::from(!b.addr[i]);
        let lobyte = carry & 0xff;
        if byte < 8 {
            res |= i64::from(lobyte) << (byte * 8);
        } else if (res < 0 && lobyte != 0xff) || (res >= 0 && lobyte != 0) {
            return Err(Error::new("result is out of range"));
        }
        carry >>= 8;
    }
    if carry == 0 && size < 8 {
        res |= (u64::MAX << (size * 8)) as i64;
    }
    Ok(res)
}

/// An inet cast to cidr, as `inet_to_cidr` makes it: every bit right of the mask cleared, written
/// as `cidr_out` writes it.
pub fn inet_to_cidr(text: &str) -> Result<String> {
    Ok(read(text, false)?.network().out(true))
}

/// A cidr cast to inet, the same value, written as `inet_out` writes it.
pub fn cidr_to_inet(text: &str) -> String {
    held(text).out(false)
}

/// An inet or a cidr cast to text, as `network_show` writes it: the whole address, then `/` and
/// the mask's length, always.
pub fn inet_text(text: &str) -> String {
    let v = held(text);
    format!("{}/{}", v.ntop(v.max_bits()), v.bits)
}

/// What a function of network.c gives: an inet's text, a cidr's, a text, or an integer.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum NetValue {
    Inet(String),
    Cidr(String),
    Text(String),
    Int(i32),
}

/// A cidr's address written as `pg_inet_cidr_ntop` writes it with a mask of `bits`: IPv4's whole
/// octets and the one the mask ends in, IPv6's words the mask reaches, then `/` and the mask.
fn cidr_ntop(v: &Inet, bits: u8) -> String {
    let src = &v.addr;
    let bits = bits as usize;
    if !v.v6 {
        let mut out = String::new();
        if bits == 0 {
            out.push('0');
        }
        let mut s = 0;
        for b in (1..=bits / 8).rev() {
            out.push_str(&src[s].to_string());
            s += 1;
            if b > 1 {
                out.push('.');
            }
        }
        let b = bits % 8;
        if b > 0 {
            if !out.is_empty() {
                out.push('.');
            }
            let m = (((1u32 << b) - 1) << (8 - b)) as u8;
            out.push_str(&(src[s] & m).to_string());
        }
        out.push_str(&format!("/{bits}"));
        return out;
    }
    let mut out = String::new();
    if bits == 0 {
        out.push_str("::");
    } else {
        let p = bits.div_ceil(8);
        let mut s = [0u8; 16];
        s[..p].copy_from_slice(&src[..p]);
        if !bits.is_multiple_of(8) {
            s[p - 1] &= 0xff << (8 - bits % 8);
        }
        let words = bits.div_ceil(16).max(2);
        // The longest run of zero words, as the loop finds it.
        let (mut zero_s, mut zero_l, mut tmp_s, mut tmp_l) = (0, 0, 0, 0);
        for i in (0..words * 2).step_by(2) {
            if s[i] | s[i + 1] == 0 {
                if tmp_l == 0 {
                    tmp_s = i / 2;
                }
                tmp_l += 1;
            } else if tmp_l != 0 && zero_l < tmp_l {
                zero_s = tmp_s;
                zero_l = tmp_l;
                tmp_l = 0;
            }
        }
        if tmp_l != 0 && zero_l < tmp_l {
            zero_s = tmp_s;
            zero_l = tmp_l;
        }
        let is_ipv4 = zero_l != words
            && zero_s == 0
            && (zero_l == 6
                || (zero_l == 5 && s[10] == 0xff && s[11] == 0xff)
                || (zero_l == 7 && s[14] != 0 && s[15] != 1));
        let mut at = 0;
        for p in 0..words {
            if zero_l != 0 && p >= zero_s && p < zero_s + zero_l {
                if p == zero_s {
                    out.push(':');
                }
                if p == words - 1 {
                    out.push(':');
                }
                at += 2;
                continue;
            }
            if is_ipv4 && p > 5 {
                out.push(if p == 6 { ':' } else { '.' });
                out.push_str(&s[at].to_string());
                at += 1;
                if p != 7 || bits > 120 {
                    out.push('.');
                    out.push_str(&s[at].to_string());
                    at += 1;
                }
            } else {
                if !out.is_empty() {
                    out.push(':');
                }
                out.push_str(&format!("{:x}", u16::from_be_bytes([s[at], s[at + 1]])));
                at += 2;
            }
        }
    }
    out.push_str(&format!("/{bits}"));
    out
}

/// The function `name` of network.c on an inet or, where `cidr`, a cidr, held as `text`: abbrev
/// (inet_abbrev, cidr_abbrev), broadcast, family, host, hostmask, masklen, netmask, network, and
/// set_masklen to `bits` (inet_set_masklen, cidr_set_masklen), -1 the family's whole mask. None
/// where network.c has no function of that name.
pub fn inet_function(
    name: &str,
    text: &str,
    cidr: bool,
    bits: Option<i32>,
) -> Option<Result<NetValue>> {
    let v = held(text);
    let with_bytes = |f: &dyn Fn(usize) -> u8, bits: u8| Inet {
        v6: v.v6,
        bits,
        addr: std::array::from_fn(|i| if i < v.size() { f(i) } else { 0 }),
    };
    // Byte `i`'s mask: the bits of the first `n` of the address.
    let mask = |n: usize, i: usize| -> u8 {
        match n.saturating_sub(8 * i) {
            0 => 0,
            k if k >= 8 => 0xff,
            k => 0xff << (8 - k),
        }
    };
    let n = v.bits as usize;
    let max = v.max_bits();
    Some(Ok(match (name, bits) {
        ("abbrev", None) if cidr => NetValue::Text(cidr_ntop(&v, v.bits)),
        ("abbrev", None) => NetValue::Text(v.ntop(v.bits)),
        ("broadcast", None) => {
            NetValue::Inet(with_bytes(&|i| v.addr[i] | !mask(n, i), v.bits).out(false))
        }
        ("family", None) => NetValue::Int(if v.v6 { 6 } else { 4 }),
        ("host", None) => NetValue::Text(v.ntop(max)),
        ("hostmask", None) => NetValue::Inet(with_bytes(&|i| !mask(n, i), max).out(false)),
        ("masklen", None) => NetValue::Int(i32::from(v.bits)),
        ("netmask", None) => NetValue::Inet(with_bytes(&|i| mask(n, i), max).out(false)),
        ("network", None) => NetValue::Cidr(v.network().out(true)),
        ("set_masklen", Some(b)) => {
            let b = if b == -1 { i32::from(max) } else { b };
            if !(0..=i32::from(max)).contains(&b) {
                return Some(Err(Error::new(format!("invalid mask length: {b}"))));
            }
            let set = Inet { bits: b as u8, ..v };
            if cidr {
                NetValue::Cidr(set.network().out(true))
            } else {
                NetValue::Inet(set.out(false))
            }
        }
        _ => return None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A result as the tables below hold PostgreSQL's: the value's text, or `ERR` and the message.
    fn res<T: ToString>(r: Result<T>) -> String {
        match r {
            Ok(v) => v.to_string(),
            Err(e) => format!("ERR {e}"),
        }
    }

    /// `network_cmp_internal`, line for line.
    fn network_cmp(a: &Inet, b: &Inet) -> Ordering {
        if a.v6 != b.v6 {
            return a.v6.cmp(&b.v6);
        }
        bitncmp(&a.addr, &b.addr, a.bits.min(b.bits) as usize)
            .then(a.bits.cmp(&b.bits))
            .then(bitncmp(&a.addr, &b.addr, a.max_bits() as usize))
    }

    /// A few hundred values, seeded, their addresses a few bits apart so that their networks meet:
    /// both families, every mask, inets and cidrs.
    fn corpus() -> Vec<Inet> {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let bases: [[u8; 16]; 5] = [
            [0; 16],
            [0xff; 16],
            [10, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            [192, 168, 1, 255, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 1, 2, 3, 4],
        ];
        let mut out = vec![];
        for _ in 0..800 {
            let v6 = next() % 3 == 0;
            let max = if v6 { 128 } else { 32 };
            let mut addr = bases[(next() % 5) as usize];
            if !v6 {
                addr[4..].fill(0);
            }
            for _ in 0..next() % 3 {
                let r = (next() % max) as usize;
                addr[r / 8] ^= 0x80 >> (r % 8);
            }
            let bits = (next() % (max + 1)) as u8;
            let v = Inet { v6, bits, addr };
            out.push(if next() % 3 == 0 { v.network() } else { v });
        }
        out
    }

    /// A spelling refused as PostgreSQL refuses a syntax it does not read.
    const SYNTAX: &str = "SYNTAX";
    /// A spelling refused as PostgreSQL refuses a cidr with a bit set right of its mask.
    const VALUE: &str = "VALUE";

    #[test]
    fn every_spelling_reads_as_postgresql_reads_it() {
        for (s, inet, cidr) in READS {
            for (want, is_cidr, ty) in [(inet, false, "inet"), (cidr, true, "cidr")] {
                let want = match *want {
                    SYNTAX => format!("ERR invalid input syntax for type {ty}: \"{s}\""),
                    VALUE => format!("ERR invalid cidr value: \"{s}\""),
                    w => w.to_string(),
                };
                assert_eq!(res(inet_in(s, is_cidr)), want, "{ty} {s:?}");
            }
        }
    }

    #[test]
    fn the_type_tables_values_come_out_and_sort_as_postgresql_has_them() {
        // types.inet and types.cidr: each value as written, as PostgreSQL writes it, then the
        // table's ORDER BY.
        let inet = [
            ("10.0.0.1/8", "10.0.0.1/8"),
            (
                "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
                "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
            ),
            ("0.0.0.0/0", "0.0.0.0/0"),
            ("255.255.255.255", "255.255.255.255"),
            ("::/0", "::/0"),
            ("10.0.0.0/8", "10.0.0.0/8"),
            ("10.0.0.0", "10.0.0.0"),
            ("9.255.255.255", "9.255.255.255"),
            ("10.0.0.0/7", "10.0.0.0/7"),
            ("11.0.0.0/8", "11.0.0.0/8"),
            ("::", "::"),
            ("::ffff:1.2.3.4", "::ffff:1.2.3.4"),
            ("1.2.3.4", "1.2.3.4"),
            ("10.0.0.1/32", "10.0.0.1"),
            ("2001:DB8::0:1", "2001:db8::1"),
            ("::1/128", "::1"),
        ];
        let inet_sorted = [
            "0.0.0.0/0",
            "1.2.3.4",
            "9.255.255.255",
            "10.0.0.0/7",
            "10.0.0.0/8",
            "10.0.0.1/8",
            "10.0.0.0",
            "10.0.0.1",
            "11.0.0.0/8",
            "255.255.255.255",
            "::/0",
            "::",
            "::1",
            "::ffff:1.2.3.4",
            "2001:db8::1",
            "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
        ];
        let cidr = [
            ("10.0.0.0/8", "10.0.0.0/8"),
            (
                "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff/128",
                "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff/128",
            ),
            ("0.0.0.0/0", "0.0.0.0/0"),
            ("255.255.255.255/32", "255.255.255.255/32"),
            ("::/0", "::/0"),
            ("10.0.0.0/16", "10.0.0.0/16"),
            ("10.0.0.1/32", "10.0.0.1/32"),
            ("2001:db8::/32", "2001:db8::/32"),
            ("10/8", "10.0.0.0/8"),
            ("10.1", "10.1.0.0/16"),
        ];
        let cidr_sorted = [
            "0.0.0.0/0",
            "10.0.0.0/8",
            "10.0.0.0/8",
            "10.0.0.0/16",
            "10.0.0.1/32",
            "10.1.0.0/16",
            "255.255.255.255/32",
            "::/0",
            "2001:db8::/32",
            "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff/128",
        ];
        for (table, is_cidr, sorted) in [
            (&inet[..], false, &inet_sorted[..]),
            (&cidr[..], true, &cidr_sorted[..]),
        ] {
            let mut out: Vec<String> = table
                .iter()
                .map(|(s, want)| {
                    let got = inet_in(s, is_cidr).unwrap();
                    assert_eq!(got, *want, "{s:?}");
                    got
                })
                .collect();
            out.sort_by_key(|t| inet_key(t));
            assert_eq!(out, sorted);
        }
    }

    #[test]
    fn keys_sort_as_postgresql_orders_the_regression_list() {
        // The regression's sort test (inet.sql), as PostgreSQL 19 orders it: keys sort it back
        // from reversed, and two neighbours share a key exactly where they are the same value.
        let mut rev: Vec<&str> = SORTED.iter().rev().copied().collect();
        rev.sort_by_key(|t| inet_key(t));
        assert_eq!(rev, SORTED);
        for w in SORTED.windows(2) {
            let (a, b) = (inet_key(w[0]), inet_key(w[1]));
            assert!(a <= b, "{w:?}");
            assert_eq!(a == b, w[0] == w[1], "{w:?}");
        }
        // An inet and a cidr of one value are equal, and print apart.
        assert_eq!(inet_key("10.0.0.1"), inet_key("10.0.0.1/32"));
        assert_eq!(inet_key("::"), inet_key("::/128"));
        assert_ne!(inet_key("1.2.3.4"), inet_key("::ffff:1.2.3.4"));
    }

    #[test]
    fn keys_order_as_network_cmp_and_tie_exactly_on_equal_values() {
        let values = corpus();
        let keys: Vec<Vec<u8>> = values.iter().map(|v| inet_key(&v.out(false))).collect();
        for (a, ka) in values.iter().zip(&keys) {
            for (b, kb) in values.iter().zip(&keys) {
                assert_eq!(ka.cmp(kb), network_cmp(a, b), "{a:?} {b:?}");
                assert_eq!(ka == kb, a == b);
            }
        }
    }

    #[test]
    fn every_value_read_back_from_its_text_is_itself() {
        for v in corpus() {
            for cidr in [false, true] {
                let v = if cidr { v.network() } else { v };
                let text = v.out(cidr);
                assert_eq!(read(&text, false).unwrap(), v, "{text}");
                if v == v.network() {
                    assert_eq!(read(&text, true).unwrap(), v, "{text}");
                }
                assert_eq!(inet_in(&text, cidr).unwrap(), text);
            }
        }
    }

    #[test]
    fn the_casts_write_as_postgresql_does() {
        for (inet, text, cidr, not) in INET_CASTS {
            assert_eq!(inet_text(inet), *text, "{inet}");
            assert_eq!(res(inet_to_cidr(inet)), *cidr, "{inet}");
            assert_eq!(res(inet_not(inet)), *not, "{inet}");
        }
        for (cidr, text, inet) in CIDR_CASTS {
            assert_eq!(inet_text(cidr), *text, "{cidr}");
            assert_eq!(cidr_to_inet(cidr), *inet, "{cidr}");
        }
    }

    #[test]
    fn the_operators_answer_as_postgresql_does() {
        let tf = |b: bool| if b { 't' } else { 'f' };
        for (a, b, tests, and, or, minus) in PAIRS {
            let got: String = [
                inet_sub(a, b),
                inet_subeq(a, b),
                inet_sup(a, b),
                inet_supeq(a, b),
                inet_overlap(a, b),
            ]
            .map(tf)
            .iter()
            .collect();
            assert_eq!(got, *tests, "{a} {b}");
            for (want, got) in [
                (and, res(inet_and(a, b))),
                (or, res(inet_or(a, b))),
                (minus, res(inet_minus(a, b))),
            ] {
                if !want.is_empty() {
                    assert_eq!(got, *want, "{a} {b}");
                }
            }
        }
        for (op, a, b, want) in ARITH {
            let got = match *op {
                "plus" | "rplus" => res(inet_plus(a, b.parse().unwrap())),
                "minus" => res(inet_minus_int(a, b.parse().unwrap())),
                "mi" => res(inet_minus(a, b)),
                "and" => res(inet_and(a, b)),
                "or" => res(inet_or(a, b)),
                "not" => res(inet_not(a)),
                _ => unreachable!(),
            };
            assert_eq!(got, *want, "{op} {a} {b}");
        }
    }

    // PostgreSQL 19's answers: each spelling read as inet (inet_in) and as cidr (cidr_in), SYNTAX
    // and VALUE its two refusals.
    const READS: &[(&str, &str, &str)] = &[
        ("", SYNTAX, SYNTAX),
        ("/", SYNTAX, SYNTAX),
        (".", SYNTAX, SYNTAX),
        (":", SYNTAX, SYNTAX),
        ("::", "::", "::/128"),
        (":::", SYNTAX, SYNTAX),
        ("::/0", "::/0", "::/0"),
        ("::/128", "::", "::/128"),
        ("::/129", SYNTAX, SYNTAX),
        ("::/00", SYNTAX, SYNTAX),
        ("::/01", SYNTAX, SYNTAX),
        ("::/1000", SYNTAX, SYNTAX),
        ("0.0.0.0", "0.0.0.0", "0.0.0.0/32"),
        ("0.0.0.0/0", "0.0.0.0/0", "0.0.0.0/0"),
        ("0/0", "0.0.0.0/0", "0.0.0.0/0"),
        ("0", SYNTAX, "0.0.0.0/8"),
        ("0.0", SYNTAX, "0.0.0.0/16"),
        ("0.0.0", SYNTAX, "0.0.0.0/24"),
        ("0.0.0.0.0", SYNTAX, SYNTAX),
        ("255.255.255.255", "255.255.255.255", "255.255.255.255/32"),
        ("256.0.0.0", SYNTAX, SYNTAX),
        ("1.2.3.4", "1.2.3.4", "1.2.3.4/32"),
        ("1.2.3.4.", "1.2.3.4", SYNTAX),
        ("1.2.3.4./24", "1.2.3.4/24", SYNTAX),
        ("1./8", "1.0.0.0/8", SYNTAX),
        ("1/8", "1.0.0.0/8", "1.0.0.0/8"),
        ("1/9", "1.0.0.0/9", "1.0.0.0/9"),
        ("1/16", SYNTAX, "1.0.0.0/16"),
        ("10", SYNTAX, "10.0.0.0/8"),
        ("10.", SYNTAX, SYNTAX),
        ("10/8", "10.0.0.0/8", "10.0.0.0/8"),
        ("10.1", SYNTAX, "10.1.0.0/16"),
        ("10.1/16", "10.1.0.0/16", "10.1.0.0/16"),
        ("10.1/15", "10.1.0.0/15", VALUE),
        ("10/15", "10.0.0.0/15", "10.0.0.0/15"),
        ("10.1.2", SYNTAX, "10.1.2.0/24"),
        ("10.1.2.3/8", "10.1.2.3/8", VALUE),
        ("10.0.0.0/8", "10.0.0.0/8", "10.0.0.0/8"),
        ("10.0.0.0/7", "10.0.0.0/7", "10.0.0.0/7"),
        ("10.0.0.1/32", "10.0.0.1", "10.0.0.1/32"),
        ("10.0.0.1/33", SYNTAX, SYNTAX),
        ("10.0.0.1/032", "10.0.0.1", "10.0.0.1/32"),
        ("10.0.0.1/4294967295", "10.0.0.1", "10.0.0.1/32"),
        ("10.0.0.0/4294967295", "10.0.0.0", "10.0.0.0/32"),
        ("10/4294967295", SYNTAX, "10.0.0.0/8"),
        ("10/4294967304", "10.0.0.0/8", "10.0.0.0/8"),
        ("1.2.3.4/4294967296", "1.2.3.4/0", VALUE),
        ("1.2.3.4/4294967328", "1.2.3.4", "1.2.3.4/32"),
        ("1.2.3.4/2147483648", SYNTAX, SYNTAX),
        ("1.2.3.4/-1", SYNTAX, SYNTAX),
        (" 1.2.3.4", SYNTAX, SYNTAX),
        ("1.2.3.4 ", SYNTAX, SYNTAX),
        ("1.2.3.4/ 8", SYNTAX, SYNTAX),
        ("1.2.3.4/8 ", SYNTAX, SYNTAX),
        ("1 .2.3.4", SYNTAX, SYNTAX),
        ("1..2.3", SYNTAX, SYNTAX),
        ("001.002.003.004", "1.2.3.4", "1.2.3.4/32"),
        ("0x", SYNTAX, SYNTAX),
        ("0x0", SYNTAX, "0.0.0.0/8"),
        ("0xa", SYNTAX, "160.0.0.0/16"),
        ("0x0a", SYNTAX, "10.0.0.0/8"),
        ("0X0A", SYNTAX, "10.0.0.0/8"),
        ("0x0a/8", SYNTAX, "10.0.0.0/8"),
        ("0x0a/7", SYNTAX, "10.0.0.0/7"),
        ("0x0A0B0C0D", SYNTAX, "10.11.12.13/32"),
        ("0x0A0B0C0D0E", SYNTAX, SYNTAX),
        ("0x0A0B0C0D0", SYNTAX, SYNTAX),
        ("0xg", SYNTAX, SYNTAX),
        ("0x/8", SYNTAX, SYNTAX),
        ("0xffffffff", SYNTAX, "255.255.255.255/32"),
        ("0xffffffff/32", SYNTAX, "255.255.255.255/32"),
        ("224", SYNTAX, "224.0.0.0/4"),
        ("224.0", SYNTAX, "224.0.0.0/16"),
        ("224/4", "224.0.0.0/4", "224.0.0.0/4"),
        ("224/3", "224.0.0.0/3", "224.0.0.0/3"),
        ("225", SYNTAX, "225.0.0.0/8"),
        ("239", SYNTAX, "239.0.0.0/8"),
        ("240", SYNTAX, "240.0.0.0/32"),
        ("255", SYNTAX, "255.0.0.0/32"),
        ("128", SYNTAX, "128.0.0.0/16"),
        ("128.1", SYNTAX, "128.1.0.0/16"),
        ("191", SYNTAX, "191.0.0.0/16"),
        ("192", SYNTAX, "192.0.0.0/24"),
        ("192.168", SYNTAX, "192.168.0.0/24"),
        ("192.168.1", SYNTAX, "192.168.1.0/24"),
        ("192.168.1/24", "192.168.1.0/24", "192.168.1.0/24"),
        ("192.168.1.0/24", "192.168.1.0/24", "192.168.1.0/24"),
        ("192.168.1.1/24", "192.168.1.1/24", VALUE),
        ("127.0.0.1", "127.0.0.1", "127.0.0.1/32"),
        ("127", SYNTAX, "127.0.0.0/8"),
        ("1.2.3.4:5", SYNTAX, SYNTAX),
        ("::1", "::1", "::1/128"),
        ("::1/128", "::1", "::1/128"),
        ("::1/127", "::1/127", VALUE),
        ("1::", "1::", "1::/128"),
        ("1::/16", "1::/16", "1::/16"),
        ("1:2:3:4:5:6:7:8", "1:2:3:4:5:6:7:8", "1:2:3:4:5:6:7:8/128"),
        ("1:2:3:4:5:6:7:8:9", SYNTAX, SYNTAX),
        ("1:2:3:4:5:6:7::8", SYNTAX, SYNTAX),
        ("1:2:3:4:5:6::7:8", SYNTAX, SYNTAX),
        ("1:2:3:4:5:6:7:8::", SYNTAX, SYNTAX),
        ("::1:2:3:4:5:6:7:8", SYNTAX, SYNTAX),
        ("::1:2:3:4:5:6:7", "0:1:2:3:4:5:6:7", "0:1:2:3:4:5:6:7/128"),
        ("1:2:3:4:5:6:7:8:/64", "1:2:3:4:5:6:7:8/64", VALUE),
        ("1:2:3:4:5:6:7:", SYNTAX, SYNTAX),
        ("1:", SYNTAX, SYNTAX),
        (":1", SYNTAX, SYNTAX),
        (":1::", SYNTAX, SYNTAX),
        ("1::2::3", SYNTAX, SYNTAX),
        ("12345::", SYNTAX, SYNTAX),
        ("fffff::", SYNTAX, SYNTAX),
        ("FFFF::", "ffff::", "ffff::/128"),
        (
            "ABCD:EF01:2345:6789:abcd:ef01:2345:6789",
            "abcd:ef01:2345:6789:abcd:ef01:2345:6789",
            "abcd:ef01:2345:6789:abcd:ef01:2345:6789/128",
        ),
        ("::ffff:1.2.3.4", "::ffff:1.2.3.4", "::ffff:1.2.3.4/128"),
        ("::ffff:1.2.3.4/96", "::ffff:1.2.3.4/96", VALUE),
        ("::ffff:1.2.3.4/120", "::ffff:1.2.3.4/120", VALUE),
        ("::1.2.3.4", "::1.2.3.4", "::1.2.3.4/128"),
        ("::0.1.2.3", "::0.1.2.3", "::0.1.2.3/128"),
        ("::1.2", "::1.2.0.0", "::1.2.0.0/128"),
        ("::.1.2.3", "::0.1.2.3", "::0.1.2.3/128"),
        ("::1..2.3", "::1.0.2.3", "::1.0.2.3/128"),
        ("::1.2.3.4.5", SYNTAX, SYNTAX),
        ("::01.2.3.4", SYNTAX, SYNTAX),
        ("::1.2.3.04", SYNTAX, SYNTAX),
        ("::256.1.1.1", SYNTAX, SYNTAX),
        ("::a.1.2.3", SYNTAX, SYNTAX),
        ("::1.2.3.4/", SYNTAX, SYNTAX),
        ("::1.2.3.4/0", "::1.2.3.4/0", VALUE),
        ("::1.2.3.4/00", SYNTAX, SYNTAX),
        ("::1.2.3.4/129", SYNTAX, SYNTAX),
        (
            "1:2:3:4:5:6:1.2.3.4",
            "1:2:3:4:5:6:102:304",
            "1:2:3:4:5:6:102:304/128",
        ),
        ("1:2:3:4:5:6:7:1.2.3.4", SYNTAX, SYNTAX),
        (
            "1:2:3:4:5::1.2.3.4",
            "1:2:3:4:5:0:102:304",
            "1:2:3:4:5:0:102:304/128",
        ),
        ("::ffff:0:0", "::ffff:0.0.0.0", "::ffff:0.0.0.0/128"),
        ("::1:0:0", "::1:0:0", "::1:0:0/128"),
        ("::0:1", "::1", "::1/128"),
        ("0:0:0:0:0:ffff:0:1", "::ffff:0.0.0.1", "::ffff:0.0.0.1/128"),
        ("0:0:0:0:0:0:0:2", "::2", "::2/128"),
        ("::2", "::2", "::2/128"),
        ("::1:2", "::0.1.0.2", "::0.1.0.2/128"),
        ("::ffff:1", "::255.255.0.1", "::255.255.0.1/128"),
        ("::fffe:1.2.3.4", "::fffe:102:304", "::fffe:102:304/128"),
        ("1::1.2.3.4", "1::102:304", "1::102:304/128"),
        ("1:0:0:0:0:0:0:0", "1::", "1::/128"),
        ("1:0:0:2:0:0:0:3", "1:0:0:2::3", "1:0:0:2::3/128"),
        ("1:0:0:2:0:0:3:4", "1::2:0:0:3:4", "1::2:0:0:3:4/128"),
        ("1:0:2:0:3:0:4:0", "1:0:2:0:3:0:4:0", "1:0:2:0:3:0:4:0/128"),
        ("0:1:0:0:2:0:0:0", "0:1:0:0:2::", "0:1:0:0:2::/128"),
        ("2001:DB8::0:1", "2001:db8::1", "2001:db8::1/128"),
        ("2001:db8::/32", "2001:db8::/32", "2001:db8::/32"),
        (
            "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
            "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
            "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff/128",
        ),
        (
            "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff/128",
            "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
            "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff/128",
        ),
        ("1:2:3:4:5:6:7:8%eth0", SYNTAX, SYNTAX),
        ("fe80::1%1", SYNTAX, SYNTAX),
        ("1.2.3.4/8/8", SYNTAX, SYNTAX),
        ("1.2.3.4//8", SYNTAX, SYNTAX),
        ("é", SYNTAX, SYNTAX),
        ("1.2.3.é", SYNTAX, SYNTAX),
        ("::é", SYNTAX, SYNTAX),
        ("١.2.3.4", SYNTAX, SYNTAX),
        ("1.2.3.4\u{a0}", SYNTAX, SYNTAX),
        ("::/", SYNTAX, SYNTAX),
        (":/", SYNTAX, SYNTAX),
        ("/8", SYNTAX, SYNTAX),
        ("/:", SYNTAX, SYNTAX),
        ("1.2.3.4:", SYNTAX, SYNTAX),
        ("a.b.c.d", SYNTAX, SYNTAX),
        ("10.0.0.256", SYNTAX, SYNTAX),
        ("10.0.0.255/31", "10.0.0.255/31", VALUE),
        ("10.0.0.254/31", "10.0.0.254/31", "10.0.0.254/31"),
        ("0.0.0.1/0", "0.0.0.1/0", VALUE),
    ];

    // PostgreSQL 19's answers for each inet read above: text() (network_show), cidr() (inet_to_cidr), ~.
    const INET_CASTS: &[(&str, &str, &str, &str)] = &[
        (
            "::",
            "::/128",
            "::/128",
            "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
        ),
        (
            "::/0",
            "::/0",
            "::/0",
            "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff/0",
        ),
        ("0.0.0.0", "0.0.0.0/32", "0.0.0.0/32", "255.255.255.255"),
        ("0.0.0.0/0", "0.0.0.0/0", "0.0.0.0/0", "255.255.255.255/0"),
        (
            "255.255.255.255",
            "255.255.255.255/32",
            "255.255.255.255/32",
            "0.0.0.0",
        ),
        ("1.2.3.4", "1.2.3.4/32", "1.2.3.4/32", "254.253.252.251"),
        (
            "1.2.3.4/24",
            "1.2.3.4/24",
            "1.2.3.0/24",
            "254.253.252.251/24",
        ),
        ("1.0.0.0/8", "1.0.0.0/8", "1.0.0.0/8", "254.255.255.255/8"),
        ("1.0.0.0/9", "1.0.0.0/9", "1.0.0.0/9", "254.255.255.255/9"),
        (
            "10.0.0.0/8",
            "10.0.0.0/8",
            "10.0.0.0/8",
            "245.255.255.255/8",
        ),
        (
            "10.1.0.0/16",
            "10.1.0.0/16",
            "10.1.0.0/16",
            "245.254.255.255/16",
        ),
        (
            "10.1.0.0/15",
            "10.1.0.0/15",
            "10.0.0.0/15",
            "245.254.255.255/15",
        ),
        (
            "10.0.0.0/15",
            "10.0.0.0/15",
            "10.0.0.0/15",
            "245.255.255.255/15",
        ),
        (
            "10.1.2.3/8",
            "10.1.2.3/8",
            "10.0.0.0/8",
            "245.254.253.252/8",
        ),
        (
            "10.0.0.0/7",
            "10.0.0.0/7",
            "10.0.0.0/7",
            "245.255.255.255/7",
        ),
        ("10.0.0.1", "10.0.0.1/32", "10.0.0.1/32", "245.255.255.254"),
        ("10.0.0.0", "10.0.0.0/32", "10.0.0.0/32", "245.255.255.255"),
        ("1.2.3.4/0", "1.2.3.4/0", "0.0.0.0/0", "254.253.252.251/0"),
        (
            "224.0.0.0/4",
            "224.0.0.0/4",
            "224.0.0.0/4",
            "31.255.255.255/4",
        ),
        (
            "224.0.0.0/3",
            "224.0.0.0/3",
            "224.0.0.0/3",
            "31.255.255.255/3",
        ),
        (
            "192.168.1.0/24",
            "192.168.1.0/24",
            "192.168.1.0/24",
            "63.87.254.255/24",
        ),
        (
            "192.168.1.1/24",
            "192.168.1.1/24",
            "192.168.1.0/24",
            "63.87.254.254/24",
        ),
        (
            "127.0.0.1",
            "127.0.0.1/32",
            "127.0.0.1/32",
            "128.255.255.254",
        ),
        (
            "::1",
            "::1/128",
            "::1/128",
            "ffff:ffff:ffff:ffff:ffff:ffff:ffff:fffe",
        ),
        (
            "::1/127",
            "::1/127",
            "::/127",
            "ffff:ffff:ffff:ffff:ffff:ffff:ffff:fffe/127",
        ),
        (
            "1::",
            "1::/128",
            "1::/128",
            "fffe:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
        ),
        (
            "1::/16",
            "1::/16",
            "1::/16",
            "fffe:ffff:ffff:ffff:ffff:ffff:ffff:ffff/16",
        ),
        (
            "1:2:3:4:5:6:7:8",
            "1:2:3:4:5:6:7:8/128",
            "1:2:3:4:5:6:7:8/128",
            "fffe:fffd:fffc:fffb:fffa:fff9:fff8:fff7",
        ),
        (
            "0:1:2:3:4:5:6:7",
            "0:1:2:3:4:5:6:7/128",
            "0:1:2:3:4:5:6:7/128",
            "ffff:fffe:fffd:fffc:fffb:fffa:fff9:fff8",
        ),
        (
            "1:2:3:4:5:6:7:8/64",
            "1:2:3:4:5:6:7:8/64",
            "1:2:3:4::/64",
            "fffe:fffd:fffc:fffb:fffa:fff9:fff8:fff7/64",
        ),
        (
            "ffff::",
            "ffff::/128",
            "ffff::/128",
            "0:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
        ),
        (
            "abcd:ef01:2345:6789:abcd:ef01:2345:6789",
            "abcd:ef01:2345:6789:abcd:ef01:2345:6789/128",
            "abcd:ef01:2345:6789:abcd:ef01:2345:6789/128",
            "5432:10fe:dcba:9876:5432:10fe:dcba:9876",
        ),
        (
            "::ffff:1.2.3.4",
            "::ffff:1.2.3.4/128",
            "::ffff:1.2.3.4/128",
            "ffff:ffff:ffff:ffff:ffff:0:fefd:fcfb",
        ),
        (
            "::ffff:1.2.3.4/96",
            "::ffff:1.2.3.4/96",
            "::ffff:0.0.0.0/96",
            "ffff:ffff:ffff:ffff:ffff:0:fefd:fcfb/96",
        ),
        (
            "::ffff:1.2.3.4/120",
            "::ffff:1.2.3.4/120",
            "::ffff:1.2.3.0/120",
            "ffff:ffff:ffff:ffff:ffff:0:fefd:fcfb/120",
        ),
        (
            "::1.2.3.4",
            "::1.2.3.4/128",
            "::1.2.3.4/128",
            "ffff:ffff:ffff:ffff:ffff:ffff:fefd:fcfb",
        ),
        (
            "::0.1.2.3",
            "::0.1.2.3/128",
            "::0.1.2.3/128",
            "ffff:ffff:ffff:ffff:ffff:ffff:fffe:fdfc",
        ),
        (
            "::1.2.0.0",
            "::1.2.0.0/128",
            "::1.2.0.0/128",
            "ffff:ffff:ffff:ffff:ffff:ffff:fefd:ffff",
        ),
        (
            "::1.0.2.3",
            "::1.0.2.3/128",
            "::1.0.2.3/128",
            "ffff:ffff:ffff:ffff:ffff:ffff:feff:fdfc",
        ),
        (
            "::1.2.3.4/0",
            "::1.2.3.4/0",
            "::/0",
            "ffff:ffff:ffff:ffff:ffff:ffff:fefd:fcfb/0",
        ),
        (
            "1:2:3:4:5:6:102:304",
            "1:2:3:4:5:6:102:304/128",
            "1:2:3:4:5:6:102:304/128",
            "fffe:fffd:fffc:fffb:fffa:fff9:fefd:fcfb",
        ),
        (
            "1:2:3:4:5:0:102:304",
            "1:2:3:4:5:0:102:304/128",
            "1:2:3:4:5:0:102:304/128",
            "fffe:fffd:fffc:fffb:fffa:ffff:fefd:fcfb",
        ),
        (
            "::ffff:0.0.0.0",
            "::ffff:0.0.0.0/128",
            "::ffff:0.0.0.0/128",
            "ffff:ffff:ffff:ffff:ffff:0:ffff:ffff",
        ),
        (
            "::1:0:0",
            "::1:0:0/128",
            "::1:0:0/128",
            "ffff:ffff:ffff:ffff:ffff:fffe:ffff:ffff",
        ),
        (
            "::ffff:0.0.0.1",
            "::ffff:0.0.0.1/128",
            "::ffff:0.0.0.1/128",
            "ffff:ffff:ffff:ffff:ffff:0:ffff:fffe",
        ),
        (
            "::2",
            "::2/128",
            "::2/128",
            "ffff:ffff:ffff:ffff:ffff:ffff:ffff:fffd",
        ),
        (
            "::0.1.0.2",
            "::0.1.0.2/128",
            "::0.1.0.2/128",
            "ffff:ffff:ffff:ffff:ffff:ffff:fffe:fffd",
        ),
        (
            "::255.255.0.1",
            "::255.255.0.1/128",
            "::255.255.0.1/128",
            "ffff:ffff:ffff:ffff:ffff:ffff:0:fffe",
        ),
        (
            "::fffe:102:304",
            "::fffe:102:304/128",
            "::fffe:102:304/128",
            "ffff:ffff:ffff:ffff:ffff:1:fefd:fcfb",
        ),
        (
            "1::102:304",
            "1::102:304/128",
            "1::102:304/128",
            "fffe:ffff:ffff:ffff:ffff:ffff:fefd:fcfb",
        ),
        (
            "1:0:0:2::3",
            "1:0:0:2::3/128",
            "1:0:0:2::3/128",
            "fffe:ffff:ffff:fffd:ffff:ffff:ffff:fffc",
        ),
        (
            "1::2:0:0:3:4",
            "1::2:0:0:3:4/128",
            "1::2:0:0:3:4/128",
            "fffe:ffff:ffff:fffd:ffff:ffff:fffc:fffb",
        ),
        (
            "1:0:2:0:3:0:4:0",
            "1:0:2:0:3:0:4:0/128",
            "1:0:2:0:3:0:4:0/128",
            "fffe:ffff:fffd:ffff:fffc:ffff:fffb:ffff",
        ),
        (
            "0:1:0:0:2::",
            "0:1:0:0:2::/128",
            "0:1:0:0:2::/128",
            "ffff:fffe:ffff:ffff:fffd:ffff:ffff:ffff",
        ),
        (
            "2001:db8::1",
            "2001:db8::1/128",
            "2001:db8::1/128",
            "dffe:f247:ffff:ffff:ffff:ffff:ffff:fffe",
        ),
        (
            "2001:db8::/32",
            "2001:db8::/32",
            "2001:db8::/32",
            "dffe:f247:ffff:ffff:ffff:ffff:ffff:ffff/32",
        ),
        (
            "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
            "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff/128",
            "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff/128",
            "::",
        ),
        (
            "10.0.0.255/31",
            "10.0.0.255/31",
            "10.0.0.254/31",
            "245.255.255.0/31",
        ),
        (
            "10.0.0.254/31",
            "10.0.0.254/31",
            "10.0.0.254/31",
            "245.255.255.1/31",
        ),
        ("0.0.0.1/0", "0.0.0.1/0", "0.0.0.0/0", "255.255.255.254/0"),
    ];

    // PostgreSQL 19's answers for each cidr read above: text() (network_show), and as inet.
    const CIDR_CASTS: &[(&str, &str, &str)] = &[
        ("::/128", "::/128", "::"),
        ("::/0", "::/0", "::/0"),
        ("0.0.0.0/32", "0.0.0.0/32", "0.0.0.0"),
        ("0.0.0.0/0", "0.0.0.0/0", "0.0.0.0/0"),
        ("0.0.0.0/8", "0.0.0.0/8", "0.0.0.0/8"),
        ("0.0.0.0/16", "0.0.0.0/16", "0.0.0.0/16"),
        ("0.0.0.0/24", "0.0.0.0/24", "0.0.0.0/24"),
        (
            "255.255.255.255/32",
            "255.255.255.255/32",
            "255.255.255.255",
        ),
        ("1.2.3.4/32", "1.2.3.4/32", "1.2.3.4"),
        ("1.0.0.0/8", "1.0.0.0/8", "1.0.0.0/8"),
        ("1.0.0.0/9", "1.0.0.0/9", "1.0.0.0/9"),
        ("1.0.0.0/16", "1.0.0.0/16", "1.0.0.0/16"),
        ("10.0.0.0/8", "10.0.0.0/8", "10.0.0.0/8"),
        ("10.1.0.0/16", "10.1.0.0/16", "10.1.0.0/16"),
        ("10.0.0.0/15", "10.0.0.0/15", "10.0.0.0/15"),
        ("10.1.2.0/24", "10.1.2.0/24", "10.1.2.0/24"),
        ("10.0.0.0/7", "10.0.0.0/7", "10.0.0.0/7"),
        ("10.0.0.1/32", "10.0.0.1/32", "10.0.0.1"),
        ("10.0.0.0/32", "10.0.0.0/32", "10.0.0.0"),
        ("160.0.0.0/16", "160.0.0.0/16", "160.0.0.0/16"),
        ("10.11.12.13/32", "10.11.12.13/32", "10.11.12.13"),
        ("224.0.0.0/4", "224.0.0.0/4", "224.0.0.0/4"),
        ("224.0.0.0/16", "224.0.0.0/16", "224.0.0.0/16"),
        ("224.0.0.0/3", "224.0.0.0/3", "224.0.0.0/3"),
        ("225.0.0.0/8", "225.0.0.0/8", "225.0.0.0/8"),
        ("239.0.0.0/8", "239.0.0.0/8", "239.0.0.0/8"),
        ("240.0.0.0/32", "240.0.0.0/32", "240.0.0.0"),
        ("255.0.0.0/32", "255.0.0.0/32", "255.0.0.0"),
        ("128.0.0.0/16", "128.0.0.0/16", "128.0.0.0/16"),
        ("128.1.0.0/16", "128.1.0.0/16", "128.1.0.0/16"),
        ("191.0.0.0/16", "191.0.0.0/16", "191.0.0.0/16"),
        ("192.0.0.0/24", "192.0.0.0/24", "192.0.0.0/24"),
        ("192.168.0.0/24", "192.168.0.0/24", "192.168.0.0/24"),
        ("192.168.1.0/24", "192.168.1.0/24", "192.168.1.0/24"),
        ("127.0.0.1/32", "127.0.0.1/32", "127.0.0.1"),
        ("127.0.0.0/8", "127.0.0.0/8", "127.0.0.0/8"),
        ("::1/128", "::1/128", "::1"),
        ("1::/128", "1::/128", "1::"),
        ("1::/16", "1::/16", "1::/16"),
        (
            "1:2:3:4:5:6:7:8/128",
            "1:2:3:4:5:6:7:8/128",
            "1:2:3:4:5:6:7:8",
        ),
        (
            "0:1:2:3:4:5:6:7/128",
            "0:1:2:3:4:5:6:7/128",
            "0:1:2:3:4:5:6:7",
        ),
        ("ffff::/128", "ffff::/128", "ffff::"),
        (
            "abcd:ef01:2345:6789:abcd:ef01:2345:6789/128",
            "abcd:ef01:2345:6789:abcd:ef01:2345:6789/128",
            "abcd:ef01:2345:6789:abcd:ef01:2345:6789",
        ),
        ("::ffff:1.2.3.4/128", "::ffff:1.2.3.4/128", "::ffff:1.2.3.4"),
        ("::1.2.3.4/128", "::1.2.3.4/128", "::1.2.3.4"),
        ("::0.1.2.3/128", "::0.1.2.3/128", "::0.1.2.3"),
        ("::1.2.0.0/128", "::1.2.0.0/128", "::1.2.0.0"),
        ("::1.0.2.3/128", "::1.0.2.3/128", "::1.0.2.3"),
        (
            "1:2:3:4:5:6:102:304/128",
            "1:2:3:4:5:6:102:304/128",
            "1:2:3:4:5:6:102:304",
        ),
        (
            "1:2:3:4:5:0:102:304/128",
            "1:2:3:4:5:0:102:304/128",
            "1:2:3:4:5:0:102:304",
        ),
        ("::ffff:0.0.0.0/128", "::ffff:0.0.0.0/128", "::ffff:0.0.0.0"),
        ("::1:0:0/128", "::1:0:0/128", "::1:0:0"),
        ("::ffff:0.0.0.1/128", "::ffff:0.0.0.1/128", "::ffff:0.0.0.1"),
        ("::2/128", "::2/128", "::2"),
        ("::0.1.0.2/128", "::0.1.0.2/128", "::0.1.0.2"),
        ("::255.255.0.1/128", "::255.255.0.1/128", "::255.255.0.1"),
        ("::fffe:102:304/128", "::fffe:102:304/128", "::fffe:102:304"),
        ("1::102:304/128", "1::102:304/128", "1::102:304"),
        ("1:0:0:2::3/128", "1:0:0:2::3/128", "1:0:0:2::3"),
        ("1::2:0:0:3:4/128", "1::2:0:0:3:4/128", "1::2:0:0:3:4"),
        (
            "1:0:2:0:3:0:4:0/128",
            "1:0:2:0:3:0:4:0/128",
            "1:0:2:0:3:0:4:0",
        ),
        ("0:1:0:0:2::/128", "0:1:0:0:2::/128", "0:1:0:0:2::"),
        ("2001:db8::1/128", "2001:db8::1/128", "2001:db8::1"),
        ("2001:db8::/32", "2001:db8::/32", "2001:db8::/32"),
        (
            "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff/128",
            "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff/128",
            "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
        ),
        ("10.0.0.254/31", "10.0.0.254/31", "10.0.0.254/31"),
    ];

    // PostgreSQL 19's answers over pairs: a, b, then <<, <<=, >>, >>=, && as t or f, then &, |
    // and a - b where it gave them.
    const PAIRS: &[(&str, &str, &str, &str, &str, &str)] = &[
        ("10.0.0.1/31", "2001:db8::/48", "fffff", "", "", ""),
        (
            "10.0.0.1/31",
            "192.0.0.0/8",
            "fffff",
            "0.0.0.0/31",
            "202.0.0.1/31",
            "-3053453311",
        ),
        (
            "10.0.0.1/31",
            "0.0.0.0/0",
            "ttfft",
            "0.0.0.0/31",
            "10.0.0.1/31",
            "167772161",
        ),
        (
            "10.0.0.1/31",
            "10.0.0.1/32",
            "ffttt",
            "10.0.0.1",
            "10.0.0.1",
            "0",
        ),
        ("2001:db8::/48", "10.0.0.1/31", "fffff", "", "", ""),
        (
            "2001:db8::/48",
            "::/11",
            "fffff",
            "::/48",
            "2001:db8::/48",
            "",
        ),
        (
            "2001:db8::/48",
            "2001:db8::400:0:0/8",
            "ttfft",
            "2001:db8::/48",
            "2001:db8::400:0:0/48",
            "",
        ),
        (
            "2001:db8::/48",
            "2001:db8::/128",
            "ffttt",
            "2001:db8::",
            "2001:db8::",
            "",
        ),
        (
            "192.0.0.0/8",
            "10.0.0.1/31",
            "fffff",
            "0.0.0.0/31",
            "202.0.0.1/31",
            "3053453311",
        ),
        ("192.0.0.0/8", "2001:db8::/48", "fffff", "", "", ""),
        (
            "192.0.0.0/8",
            "0.0.0.0/0",
            "ttfft",
            "0.0.0.0/8",
            "192.0.0.0/8",
            "3221225472",
        ),
        (
            "192.0.0.0/8",
            "192.168.1.255/9",
            "ffttt",
            "192.0.0.0/9",
            "192.168.1.255/9",
            "-11010559",
        ),
        ("::/11", "10.0.0.1/31", "fffff", "", "", ""),
        (
            "::/11",
            "2001:db8::/48",
            "fffff",
            "::/48",
            "2001:db8::/48",
            "",
        ),
        (
            "::/11",
            "::ffff:192.168.1.255/60",
            "ffttt",
            "::/60",
            "::ffff:192.168.1.255/60",
            "",
        ),
        (
            "::/11",
            "::ffff:127.0.4.1/0",
            "ttfft",
            "::/11",
            "::ffff:127.0.4.1/11",
            "",
        ),
        (
            "0.0.0.0/0",
            "10.0.0.1/31",
            "ffttt",
            "0.0.0.0/31",
            "10.0.0.1/31",
            "-167772161",
        ),
        ("0.0.0.0/0", "2001:db8::/48", "fffff", "", "", ""),
        (
            "0.0.0.0/0",
            "255.255.255.255/0",
            "ftftt",
            "0.0.0.0/0",
            "255.255.255.255/0",
            "-4294967295",
        ),
        (
            "ffff:ffff:ffff:ffff:fff0::/76",
            "10.0.0.1/31",
            "fffff",
            "",
            "",
            "",
        ),
        (
            "ffff:ffff:ffff:ffff:fff0::/76",
            "2001:db8::/48",
            "fffff",
            "2001:db8::/76",
            "ffff:ffff:ffff:ffff:fff0::/76",
            "",
        ),
        (
            "ffff:ffff:ffff:ffff:fff0::/76",
            "ffff:ffff:fff7:ffff:feff:ffff:ffff:ffff/25",
            "ttfft",
            "ffff:ffff:fff7:ffff:fef0::/76",
            "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff/76",
            "",
        ),
        (
            "ffff:ffff:ffff:ffff:fff0::/76",
            "ffff:ffff:ffff:ffff:ffff:ffff:8000:0/97",
            "ffttt",
            "ffff:ffff:ffff:ffff:fff0::/97",
            "ffff:ffff:ffff:ffff:ffff:ffff:8000:0/97",
            "",
        ),
        (
            "255.255.255.255/0",
            "0.0.0.0/0",
            "ftftt",
            "0.0.0.0/0",
            "255.255.255.255/0",
            "4294967295",
        ),
        (
            "0.4.0.0/19",
            "10.0.0.1/31",
            "fffff",
            "0.0.0.0/31",
            "10.4.0.1/31",
            "-167510017",
        ),
        (
            "0.4.0.0/19",
            "0.0.0.0/0",
            "ttfft",
            "0.0.0.0/19",
            "0.4.0.0/19",
            "262144",
        ),
        (
            "208.79.75.123/2",
            "192.0.0.0/2",
            "ftftt",
            "192.0.0.0/2",
            "208.79.75.123/2",
            "273632123",
        ),
        (
            "0:10::10:ffff:7f00:1/24",
            "::/24",
            "ftftt",
            "::/24",
            "0:10::10:ffff:7f00:1/24",
            "",
        ),
        (
            "::ffff:127.0.4.1/0",
            "2e44:f037:f330:3286:690f:89a4:5fed:a453/0",
            "ftftt",
            "::89a4:5f00:401/0",
            "2e44:f037:f330:3286:690f:ffff:7fed:a453/0",
            "",
        ),
        (
            "2e44:f037:f330:3286:690f:89a4:5fed:a453/0",
            "::ffff:127.0.4.1/0",
            "ftftt",
            "::89a4:5f00:401/0",
            "2e44:f037:f330:3286:690f:ffff:7fed:a453/0",
            "",
        ),
    ];

    // PostgreSQL 19's answers, one statement each: the operator, a, b or n, its result.
    const ARITH: &[(&str, &str, &str, &str)] = &[
        ("rplus", "10.0.0.1/31", "7", "10.0.0.8/31"),
        ("rplus", "192.168.1.255/5", "7", "192.168.2.6/5"),
        ("rplus", "14.0.1.0/19", "7", "14.0.1.7/19"),
        ("rplus", "10.0.0.0/12", "7", "10.0.0.7/12"),
        ("rplus", "100::/12", "7", "100::7/12"),
        (
            "rplus",
            "ffff:ffff:ffff:ffff:7fff:ffff:feff:ffff/116",
            "7",
            "ffff:ffff:ffff:ffff:7fff:ffff:ff00:6/116",
        ),
        ("plus", "0.0.0.0/0", "1", "0.0.0.1/0"),
        ("minus", "0.0.0.0/0", "1", "ERR result is out of range"),
        ("plus", "0.0.0.0/0", "-1", "ERR result is out of range"),
        ("minus", "0.0.0.0/0", "-1", "0.0.0.1/0"),
        (
            "plus",
            "0.0.0.0/0",
            "4294967296",
            "ERR result is out of range",
        ),
        (
            "plus",
            "0.0.0.0/0",
            "9223372036854775807",
            "ERR result is out of range",
        ),
        (
            "plus",
            "0.0.0.0/0",
            "-9223372036854775808",
            "ERR result is out of range",
        ),
        (
            "minus",
            "0.0.0.0/0",
            "-9223372036854775808",
            "ERR result is out of range",
        ),
        ("rplus", "0.0.0.0/0", "7", "0.0.0.7/0"),
        ("plus", "255.255.255.255", "1", "ERR result is out of range"),
        ("minus", "255.255.255.255", "1", "255.255.255.254"),
        ("plus", "255.255.255.255", "-1", "255.255.255.254"),
        (
            "minus",
            "255.255.255.255",
            "-1",
            "ERR result is out of range",
        ),
        (
            "plus",
            "255.255.255.255",
            "4294967296",
            "ERR result is out of range",
        ),
        (
            "plus",
            "255.255.255.255",
            "9223372036854775807",
            "ERR result is out of range",
        ),
        (
            "plus",
            "255.255.255.255",
            "-9223372036854775808",
            "ERR result is out of range",
        ),
        (
            "minus",
            "255.255.255.255",
            "-9223372036854775808",
            "ERR result is out of range",
        ),
        ("plus", "::/0", "1", "::1/0"),
        ("minus", "::/0", "1", "ERR result is out of range"),
        ("plus", "::/0", "-1", "ERR result is out of range"),
        ("minus", "::/0", "-1", "::1/0"),
        ("plus", "::/0", "4294967296", "::1:0:0/0"),
        (
            "plus",
            "::/0",
            "9223372036854775807",
            "::7fff:ffff:ffff:ffff/0",
        ),
        (
            "plus",
            "::/0",
            "-9223372036854775808",
            "ERR result is out of range",
        ),
        (
            "minus",
            "::/0",
            "-9223372036854775808",
            "ERR result is out of range",
        ),
        (
            "plus",
            "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
            "1",
            "ERR result is out of range",
        ),
        (
            "minus",
            "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
            "1",
            "ffff:ffff:ffff:ffff:ffff:ffff:ffff:fffe",
        ),
        (
            "plus",
            "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
            "-1",
            "ffff:ffff:ffff:ffff:ffff:ffff:ffff:fffe",
        ),
        (
            "minus",
            "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
            "-1",
            "ERR result is out of range",
        ),
        (
            "plus",
            "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
            "4294967296",
            "ERR result is out of range",
        ),
        (
            "plus",
            "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
            "9223372036854775807",
            "ERR result is out of range",
        ),
        (
            "plus",
            "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
            "-9223372036854775808",
            "ffff:ffff:ffff:ffff:7fff:ffff:ffff:ffff",
        ),
        (
            "minus",
            "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
            "-9223372036854775808",
            "ffff:ffff:ffff:ffff:7fff:ffff:ffff:ffff",
        ),
        ("plus", "::ffff:0.0.0.0/96", "1", "::ffff:0.0.0.1/96"),
        ("minus", "::ffff:0.0.0.0/96", "1", "::fffe:ffff:ffff/96"),
        ("plus", "::ffff:0.0.0.0/96", "-1", "::fffe:ffff:ffff/96"),
        ("minus", "::ffff:0.0.0.0/96", "-1", "::ffff:0.0.0.1/96"),
        ("plus", "::ffff:0.0.0.0/96", "4294967296", "::1:0:0:0/96"),
        (
            "plus",
            "::ffff:0.0.0.0/96",
            "9223372036854775807",
            "::8000:fffe:ffff:ffff/96",
        ),
        (
            "plus",
            "::ffff:0.0.0.0/96",
            "-9223372036854775808",
            "ERR result is out of range",
        ),
        (
            "minus",
            "::ffff:0.0.0.0/96",
            "-9223372036854775808",
            "ERR result is out of range",
        ),
        ("mi", "10.0.0.1/31", "10.0.0.1/31", "0"),
        ("mi", "10.0.0.1/31", "0.0.0.0/0", "167772161"),
        ("mi", "10.0.0.1/31", "127.0.32.1", "-1962942464"),
        ("mi", "10.0.0.1/31", "0.0.0.0/2", "167772161"),
        (
            "mi",
            "10.0.0.1/31",
            "fac5:cc5b:4b37:ff5e:d20b:a5fa:cbf0:15c5/122",
            "ERR cannot subtract inet values of different sizes",
        ),
        (
            "mi",
            "10.0.0.1/31",
            "2b8:f3d2:7a9:3a02:706c:e471:8914:982b",
            "ERR cannot subtract inet values of different sizes",
        ),
        (
            "mi",
            "10.0.0.1/31",
            "52b0::/16",
            "ERR cannot subtract inet values of different sizes",
        ),
        (
            "mi",
            "10.0.0.1/31",
            "3e46:eeb1:8487:da17:9b0a:401e:eab5:f0a3/84",
            "ERR cannot subtract inet values of different sizes",
        ),
        ("mi", "2001:db8::/48", "2001:db8::/48", "0"),
        (
            "mi",
            "2001:db8::/48",
            "ffff:ffff:ffff:ffff:fff0::/76",
            "ERR result is out of range",
        ),
        (
            "mi",
            "2001:db8::/48",
            "0.0.0.4",
            "ERR cannot subtract inet values of different sizes",
        ),
        (
            "mi",
            "2001:db8::/48",
            "208.79.75.123/2",
            "ERR cannot subtract inet values of different sizes",
        ),
        ("mi", "2001:db8::/48", "2001:db8::/128", "0"),
        (
            "mi",
            "2001:db8::/48",
            "128.168.1.0/31",
            "ERR cannot subtract inet values of different sizes",
        ),
        (
            "mi",
            "2001:db8::/48",
            "204.243.200.231/4",
            "ERR cannot subtract inet values of different sizes",
        ),
        ("mi", "2001:db8::/48", "::/0", "ERR result is out of range"),
        (
            "mi",
            "2001:db8::/48",
            "ffff:ffff:ffff:fffb:ffff:ffff:ffff:ffff/8",
            "ERR result is out of range",
        ),
        (
            "mi",
            "2001:db8::/48",
            "400::ffff:c0a8:0/111",
            "ERR result is out of range",
        ),
        ("mi", "2001:db8::/48", "2001:db8::80:4000/71", "-8404992"),
        ("mi", "::/11", "::/11", "0"),
        (
            "mi",
            "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
            "::7fff:ffff:ffff:ffff",
            "-9223372036854775808",
        ),
        ("mi", "127.0.0.1", "127.0.0.1", "0"),
        ("mi", "::8000:0:0:0", "::8000:0:0:0", "0"),
        ("mi", "::7fff:ffff:ffff:ffff", "::7fff:ffff:ffff:ffff", "0"),
        ("and", "10.0.0.1/31", "10.0.0.1/31", "10.0.0.1/31"),
        ("or", "10.0.0.1/31", "10.0.0.1/31", "10.0.0.1/31"),
        (
            "and",
            "10.0.0.1/31",
            "92d9:20da:6739:bf38:40bc:d522:dcaf:8423/22",
            "ERR cannot AND inet values of different sizes",
        ),
        (
            "or",
            "10.0.0.1/31",
            "92d9:20da:6739:bf38:40bc:d522:dcaf:8423/22",
            "ERR cannot OR inet values of different sizes",
        ),
        ("and", "10.0.0.1/31", "10.1.0.0/20", "10.0.0.0/31"),
        ("or", "10.0.0.1/31", "10.1.0.0/20", "10.1.0.1/31"),
        (
            "and",
            "10.0.0.1/31",
            "f475:bdae:b37e:327e:8c7b:977e:dcf7:36d4",
            "ERR cannot AND inet values of different sizes",
        ),
        (
            "or",
            "10.0.0.1/31",
            "f475:bdae:b37e:327e:8c7b:977e:dcf7:36d4",
            "ERR cannot OR inet values of different sizes",
        ),
        (
            "and",
            "10.0.0.1/31",
            "::/0",
            "ERR cannot AND inet values of different sizes",
        ),
        (
            "or",
            "10.0.0.1/31",
            "::/0",
            "ERR cannot OR inet values of different sizes",
        ),
        // The regression's own (inet.sql), and the ends of a bigint.
        ("plus", "127.0.0.1", "257", "127.0.1.2"),
        ("minus", "127.0.1.2", "257", "127.0.0.1"),
        ("plus", "127::1", "257", "127::102"),
        ("minus", "127::102", "257", "127::1"),
        ("mi", "127.0.0.2", "127.0.1.246", "-500"),
        ("mi", "127.0.0.2", "126.255.254.14", "500"),
        (
            "plus",
            "127.0.0.1",
            "10000000000",
            "ERR result is out of range",
        ),
        (
            "minus",
            "127.0.0.1",
            "10000000000",
            "ERR result is out of range",
        ),
        ("mi", "126::1", "127::2", "ERR result is out of range"),
        ("mi", "127::1", "126::2", "ERR result is out of range"),
        ("plus", "127::1", "10000000000", "127::2:540b:e401"),
        ("mi", "127::1", "127::2", "-1"),
        ("mi", "::8000:0:0:0", "::", "ERR result is out of range"),
        ("mi", "::7fff:ffff:ffff:ffff", "::", "9223372036854775807"),
        ("mi", "::", "::7fff:ffff:ffff:ffff", "-9223372036854775807"),
        ("mi", "::", "::8000:0:0:0", "-9223372036854775808"),
        ("mi", "::", "::8000:0:0:1", "ERR result is out of range"),
        ("mi", "255.255.255.255", "0.0.0.0", "4294967295"),
        ("mi", "0.0.0.0", "255.255.255.255", "-4294967295"),
        ("mi", "1::", "0:ffff:ffff:ffff:ffff:ffff:ffff:ffff", "1"),
        ("mi", "10.0.0.0/8", "10.0.0.255", "-255"),
        ("not", "::", "", "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff"),
        ("not", "0.0.0.0/0", "", "255.255.255.255/0"),
        ("not", "10.0.0.0/8", "", "245.255.255.255/8"),
        ("and", "10.0.0.0/8", "255.0.255.0/24", "10.0.0.0/24"),
        ("or", "10.0.0.0/8", "0.0.0.255", "10.0.0.255"),
        ("plus", "10.0.0.0/8", "257", "10.0.1.1/8"),
        ("minus", "10.0.0.0/8", "1", "9.255.255.255/8"),
    ];

    // The regression's sort test (inet.sql), as PostgreSQL 19 orders it.
    const SORTED: &[&str] = &[
        "0.0.0.0/0",
        "0.0.0.1/0",
        "127.126.127.127/0",
        "127.127.127.127/0",
        "127.128.127.127/0",
        "192.168.1.4/0",
        "192.168.1.5/0",
        "255.0.0.0/0",
        "255.1.0.0/0",
        "255.2.0.0/0",
        "255.3.0.0/0",
        "255.255.0.0/0",
        "255.255.0.0/0",
        "255.255.255.255/0",
        "255.255.255.255/0",
        "255.255.255.255/0",
        "0.0.0.0/1",
        "0.0.0.1/1",
        "0.0.0.0",
        "192.168.1.3/1",
        "255.255.255.255/1",
        "192.168.1.1/5",
        "192.168.1.255/5",
        "192.168.1.1/6",
        "192.168.1.255/6",
        "192.168.1.1/23",
        "192.168.1.2/23",
        "192.168.1.3/23",
        "192.168.1.0/24",
        "192.168.1.0/25",
        "192.168.1.1/25",
        "192.168.1.2/25",
        "192.168.1.1/26",
        "192.168.1.2/26",
        "255.255.0.0/15",
        "255.255.0.0/16",
        "255.255.255.255/16",
        "255.255.255.255/16",
        "255.255.255.1/31",
        "255.255.255.0",
        "255.255.255.1",
        "255.255.255.2/31",
        "255.255.255.3/31",
        "255.255.255.3",
        "255.255.255.252",
        "255.255.255.253",
        "255.255.255.255/31",
        "255.255.255.254",
        "255.255.255.255",
        "::/0",
        "ffff:fffa:ffff:ffff:ffff:ffff:ffff:ffff/0",
        "ffff:fffb:ffff:ffff:ffff:ffff:ffff:ffff/0",
        "ffff:fffc:ffff:ffff:ffff:ffff:ffff:ffff/0",
        "ffff:fffd:ffff:ffff:ffff:ffff:ffff:ffff/0",
        "ffff:fffe:ffff:ffff:ffff:ffff:ffff:ffff/0",
        "ffff:ffff:ffff:fffa:ffff:ffff:ffff:ffff/0",
        "ffff:ffff:ffff:fffb:ffff:ffff:ffff:ffff/0",
        "ffff:ffff:ffff:fffc:ffff:ffff:ffff:ffff/0",
        "ffff:ffff:ffff:fffd:ffff:ffff:ffff:ffff/0",
        "ffff:ffff:ffff:fffe:ffff:ffff:ffff:ffff/0",
        "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff/0",
        "::4:3:2:0/24",
        "::4:3:2:1/24",
        "::4:3:2:2/24",
        "::",
        "::1",
        "::1:ffff:ffff:ffff:ffff",
        "::2:ffff:ffff:ffff:ffff",
        "10:23::f1/64",
        "10:23::f1/65",
        "10:23::ffff",
        "127::1",
        "127::2",
        "8000::/1",
        "ffff:83e7:f118:57dc:6093:6d92:689d:58cf/70",
        "ffff:84b0:4775:536e:c3ed:7116:a6d6:34f0/44",
        "ffff:8566:f84:5867:47f1:7867:d2ba:8a1a/69",
        "ffff:8883:f028:7d2:4d68:d510:7d6b:ac43/73",
        "ffff:8ae8:7c14:65b3:196:8e4a:89ae:fb30/89",
        "ffff:8dd0:646:694c:7c16:7e35:6a26:171/104",
        "ffff:8eef:cbf:700:eda3:ae32:f4b4:318b/121",
        "ffff:90e7:e744:664:a93:8efe:1f25:7663/122",
        "ffff:9597:c69c:8b24:57a:8639:ec78:6026/111",
        "ffff:9e86:79ea:f16e:df31:8e4d:7783:532e/88",
        "ffff:a0c7:82d3:24de:f762:6e1f:316d:3fb2/23",
        "ffff:ffff:ffff:ffff:4:3:2:0/24",
        "ffff:ffff:ffff:ffff:4:3:2:1/24",
        "ffff:ffff:ffff:ffff:4:3:2:2/24",
        "ffff:ffff:ffff:fffd::",
        "ffff:ffff:ffff:fffe::",
        "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
    ];
}
