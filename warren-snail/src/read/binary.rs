//! COPY's binary format, as PostgreSQL's: each value as its type's send function writes it and its
//! receive function reads it.

use super::statement::Datum;
use crate::catalog::ColumnType;
use crate::{Error, Result};

/// The header COPY's binary format begins with: its signature, no flags and no extension.
pub(super) const HEADER: &[u8] = b"PGCOPY\n\xff\r\n\0\0\0\0\0\0\0\0\0";

/// Days and microseconds from 1970-01-01, where the snail counts from, to 2000-01-01, where
/// PostgreSQL's send functions do.
const DAYS_TO_2000: i32 = 10_957;
const MICROS_TO_2000: i64 = 946_684_800_000_000;

fn unwritten(ty: ColumnType) -> Error {
    Error::new(format!(
        "COPY … BINARY of a column of type {} is refused: its send and receive functions are not \
         written",
        ty.pg_name()
    ))
}

/// A value as its type's send function writes it.
pub(super) fn send(ty: ColumnType, d: &Datum) -> Result<Vec<u8>> {
    let text = || crate::composed::scalar_out(ty, d);
    Ok(match (ty, d) {
        (ColumnType::Boolean, Datum::Int(v)) => vec![(*v != 0) as u8],
        (ColumnType::Int2, Datum::Int(v)) => (*v as i16).to_be_bytes().to_vec(),
        (ColumnType::Int4, Datum::Int(v)) => (*v as i32).to_be_bytes().to_vec(),
        (ColumnType::Int8, Datum::Int(v)) => v.to_be_bytes().to_vec(),
        (ColumnType::Float4, Datum::Float4(b)) => b.to_be_bytes().to_vec(),
        (ColumnType::Float8, Datum::Float8(b)) => b.to_be_bytes().to_vec(),
        (ColumnType::Text(_) | ColumnType::Bpchar(_) | ColumnType::Name, Datum::Text(s)) => {
            s.as_bytes().to_vec()
        }
        (ColumnType::Bytes, Datum::Bytes(b)) => b.clone(),
        (ColumnType::Date, Datum::Int(v)) => {
            let v = *v as i32;
            match v {
                i32::MAX | i32::MIN => v,
                v => v - DAYS_TO_2000,
            }
            .to_be_bytes()
            .to_vec()
        }
        (ColumnType::Timestamp(_) | ColumnType::Timestamptz | ColumnType::TimestamptzRounded(_), Datum::Int(v)) => {
            match *v {
                i64::MAX | i64::MIN => *v,
                v => v - MICROS_TO_2000,
            }
            .to_be_bytes()
            .to_vec()
        }
        (ColumnType::Time(_), Datum::Int(v)) => v.to_be_bytes().to_vec(),
        (ColumnType::Interval { .. }, Datum::Interval(i)) => {
            let mut out = i.micros.to_be_bytes().to_vec();
            out.extend(i.days.to_be_bytes());
            out.extend(i.months.to_be_bytes());
            out
        }
        (ColumnType::Money(_), Datum::Int(v)) => v.to_be_bytes().to_vec(),
        (ColumnType::Oid | ColumnType::Reg(..) | ColumnType::Xid | ColumnType::Cid, Datum::Int(v)) => {
            (*v as u32).to_be_bytes().to_vec()
        }
        (ColumnType::Xid8 | ColumnType::PgLsn | ColumnType::Oid8, Datum::Int(v)) => {
            (*v as u64).to_be_bytes().to_vec()
        }
        (ColumnType::Char, Datum::Int(v)) => vec![*v as u8],
        (ColumnType::Uuid, _) => hex(&text().replace('-', ""))?,
        (ColumnType::Macaddr | ColumnType::Macaddr8, _) => {
            hex(&text().replace([':', '-', '.'], ""))?
        }
        (ColumnType::Numeric { .. } | ColumnType::NumericText(_), _) => numeric_send(&text())?,
        (ColumnType::Bit(_) | ColumnType::Varbit(_), _) => {
            let bits = text();
            let mut out = (bits.len() as i32).to_be_bytes().to_vec();
            for chunk in bits.as_bytes().chunks(8) {
                let mut b = 0u8;
                for (k, c) in chunk.iter().enumerate() {
                    if *c == b'1' {
                        b |= 0x80 >> k;
                    }
                }
                out.push(b);
            }
            out
        }
        (ColumnType::Textual(t), Datum::Text(_)) => {
            let s = text();
            use crate::textual::Textual as T;
            match t {
                T::Jsonb => [&[1u8][..], s.as_bytes()].concat(),
                T::Json | T::Xml | T::Refcursor => s.into_bytes(),
                _ => return Err(unwritten(ty)),
            }
        }
        (ColumnType::Composed(r), Datum::Int(_))
            if matches!(r.def(), crate::composed::Composed::Enum { .. }) =>
        {
            text().into_bytes()
        }
        _ => return Err(unwritten(ty)),
    })
}

/// A value as its type's receive function reads it, written as the text its input function reads.
pub(super) fn recv(ty: ColumnType, b: &[u8]) -> Result<String> {
    let wrong = || {
        Error::new(format!(
            "incorrect binary data format in a value of type {}",
            ty.pg_name()
        ))
    };
    let int = |n: usize| -> Result<i64> {
        if b.len() != n {
            return Err(wrong());
        }
        let mut v: i64 = if b[0] & 0x80 != 0 { -1 } else { 0 };
        for x in b {
            v = (v << 8) | i64::from(*x);
        }
        Ok(v)
    };
    let uint = |n: usize| -> Result<u64> {
        if b.len() != n {
            return Err(wrong());
        }
        Ok(b.iter().fold(0u64, |v, x| (v << 8) | u64::from(*x)))
    };
    let text = |ty: ColumnType, d: Datum| crate::composed::scalar_out(ty, &d);
    let float = |v: f64| -> String {
        if v.is_nan() {
            "NaN".into()
        } else if v.is_infinite() {
            if v > 0.0 { "Infinity" } else { "-Infinity" }.into()
        } else {
            format!("{v:?}")
        }
    };
    Ok(match ty {
        ColumnType::Boolean => match b {
            [0] => "f".into(),
            [_] => "t".into(),
            _ => return Err(wrong()),
        },
        ColumnType::Int2 => int(2)?.to_string(),
        ColumnType::Int4 => int(4)?.to_string(),
        ColumnType::Int8 => int(8)?.to_string(),
        ColumnType::Float4 => float(f64::from(f32::from_bits(uint(4)? as u32))),
        ColumnType::Float8 => float(f64::from_bits(uint(8)?)),
        ColumnType::Text(_) | ColumnType::Bpchar(_) | ColumnType::Name => {
            String::from_utf8(b.to_vec()).map_err(|_| wrong())?
        }
        ColumnType::Bytes => {
            let mut s = String::from("\\x");
            for x in b {
                s.push_str(&format!("{x:02x}"));
            }
            s
        }
        ColumnType::Date => {
            let v = int(4)? as i32;
            let v = match v {
                i32::MAX | i32::MIN => v,
                v => v + DAYS_TO_2000,
            };
            text(ty, Datum::Int(i64::from(v)))
        }
        ColumnType::Timestamp(_) | ColumnType::Timestamptz | ColumnType::TimestamptzRounded(_) => {
            let v = match int(8)? {
                v @ (i64::MAX | i64::MIN) => v,
                v => v + MICROS_TO_2000,
            };
            text(ty, Datum::Int(v))
        }
        ColumnType::Time(_) => text(ty, Datum::Int(int(8)?)),
        ColumnType::Interval { .. } => {
            if b.len() != 16 {
                return Err(wrong());
            }
            let micros = i64::from_be_bytes(b[0..8].try_into().unwrap());
            let days = i32::from_be_bytes(b[8..12].try_into().unwrap());
            let months = i32::from_be_bytes(b[12..16].try_into().unwrap());
            crate::times::interval_out(crate::times::Interval {
                months,
                days,
                micros,
            })
        }
        ColumnType::Money(_) => text(ty, Datum::Int(int(8)?)),
        ColumnType::Oid | ColumnType::Reg(..) | ColumnType::Xid | ColumnType::Cid => {
            uint(4)?.to_string()
        }
        ColumnType::Xid8 | ColumnType::Oid8 => uint(8)?.to_string(),
        ColumnType::PgLsn => text(ty, Datum::Int(uint(8)? as i64)),
        ColumnType::Char => text(ty, Datum::Int(i64::from(*b.first().ok_or_else(wrong)?))),
        ColumnType::Uuid => {
            if b.len() != 16 {
                return Err(wrong());
            }
            let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
            format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
        }
        ColumnType::Macaddr | ColumnType::Macaddr8 => {
            if b.len() != if ty == ColumnType::Macaddr { 6 } else { 8 } {
                return Err(wrong());
            }
            b.iter()
                .map(|x| format!("{x:02x}"))
                .collect::<Vec<_>>()
                .join(":")
        }
        ColumnType::Numeric { .. } | ColumnType::NumericText(_) => numeric_recv(b).ok_or_else(wrong)?,
        ColumnType::Bit(_) | ColumnType::Varbit(_) => {
            if b.len() < 4 {
                return Err(wrong());
            }
            let n = i32::from_be_bytes(b[0..4].try_into().unwrap()).max(0) as usize;
            if b.len() != 4 + n.div_ceil(8) {
                return Err(wrong());
            }
            (0..n)
                .map(|k| if b[4 + k / 8] & (0x80 >> (k % 8)) != 0 { '1' } else { '0' })
                .collect()
        }
        ColumnType::Textual(crate::textual::Textual::Jsonb) => match b.split_first() {
                Some((1, rest)) => String::from_utf8(rest.to_vec()).map_err(|_| wrong())?,
                Some((v, _)) => {
                    return Err(Error::new(format!(
                        "unsupported jsonb version number {v}"
                    )))
                }
            None => return Err(wrong()),
        },
        ColumnType::Textual(
            crate::textual::Textual::Json
            | crate::textual::Textual::Xml
            | crate::textual::Textual::Refcursor,
        ) => String::from_utf8(b.to_vec()).map_err(|_| wrong())?,
        ColumnType::Composed(r) if matches!(r.def(), crate::composed::Composed::Enum { .. }) => {
            String::from_utf8(b.to_vec()).map_err(|_| wrong())?
        }
        _ => return Err(unwritten(ty)),
    })
}

fn hex(s: &str) -> Result<Vec<u8>> {
    (0..s.len())
        .step_by(2)
        .map(|i| {
            s.get(i..i + 2)
                .and_then(|x| u8::from_str_radix(x, 16).ok())
                .ok_or_else(|| Error::new(format!("{s} is not hexadecimal")))
        })
        .collect()
}

/// A numeric's text as `numeric_send` writes it: its count of base-10000 digits, the weight of the
/// first, its sign, its scale, then the digits.
fn numeric_send(text: &str) -> Result<Vec<u8>> {
    let head = |n: i16, w: i16, sign: u16, scale: i16| {
        let mut out = n.to_be_bytes().to_vec();
        out.extend(w.to_be_bytes());
        out.extend(sign.to_be_bytes());
        out.extend(scale.to_be_bytes());
        out
    };
    match text {
        "NaN" => return Ok(head(0, 0, 0xC000, 0)),
        "Infinity" => return Ok(head(0, 0, 0xD000, 0)),
        "-Infinity" => return Ok(head(0, 0, 0xF000, 0)),
        _ => {}
    }
    let (neg, t) = match text.strip_prefix('-') {
        Some(t) => (true, t),
        None => (false, text),
    };
    let (int, frac) = t.split_once('.').unwrap_or((t, ""));
    let scale = frac.len() as i16;
    let int = int.trim_start_matches('0');
    // The integer part padded on the left and the fraction on the right to whole groups of four.
    let lead = (4 - int.len() % 4) % 4;
    let mut digits_text = "0".repeat(lead) + int;
    let int_groups = digits_text.len() / 4;
    digits_text.push_str(frac);
    digits_text.push_str(&"0".repeat((4 - frac.len() % 4) % 4));
    let mut groups: Vec<i16> = digits_text
        .as_bytes()
        .chunks(4)
        .map(|c| std::str::from_utf8(c).unwrap().parse::<i16>().unwrap())
        .collect();
    let mut weight = int_groups as i16 - 1;
    while groups.first() == Some(&0) {
        groups.remove(0);
        weight -= 1;
    }
    while groups.last() == Some(&0) {
        groups.pop();
    }
    if groups.is_empty() {
        weight = 0;
    }
    let mut out = head(
        groups.len() as i16,
        weight,
        if neg && !groups.is_empty() { 0x4000 } else { 0 },
        scale,
    );
    for g in groups {
        out.extend(g.to_be_bytes());
    }
    Ok(out)
}

/// A numeric as `numeric_recv` reads it, written as its text.
fn numeric_recv(b: &[u8]) -> Option<String> {
    if b.len() < 8 {
        return None;
    }
    let at = |k: usize| i16::from_be_bytes([b[k], b[k + 1]]);
    let (n, weight, sign, scale) = (at(0) as usize, at(2), at(4) as u16, at(6).max(0) as usize);
    match sign {
        0xC000 => return Some("NaN".into()),
        0xD000 => return Some("Infinity".into()),
        0xF000 => return Some("-Infinity".into()),
        0 | 0x4000 => {}
        _ => return None,
    }
    if b.len() != 8 + 2 * n {
        return None;
    }
    let digits: Vec<i16> = (0..n).map(|k| at(8 + 2 * k)).collect();
    let mut int = String::new();
    for w in (0..=weight.max(-1)).rev() {
        let i = (weight - w) as usize;
        let g = digits.get(i).copied().unwrap_or(0);
        if int.is_empty() {
            int = g.to_string();
        } else {
            int.push_str(&format!("{g:04}"));
        }
    }
    if int.is_empty() {
        int = "0".into();
    }
    let mut frac = String::new();
    let mut w = -1;
    while frac.len() < scale {
        let g = match w > weight {
            true => 0,
            false => digits.get((weight - w) as usize).copied().unwrap_or(0),
        };
        frac.push_str(&format!("{g:04}"));
        w -= 1;
    }
    frac.truncate(scale);
    let neg = if sign == 0x4000 { "-" } else { "" };
    Some(match scale {
        0 => format!("{neg}{int}"),
        _ => format!("{neg}{int}.{frac}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_numeric_is_sent_and_received_as_postgresqls_base_ten_thousand_digits() {
        for (text, bytes) in [
            ("0", vec![0, 0, 0, 0, 0, 0, 0, 0]),
            ("1.5", vec![0, 2, 0, 0, 0, 0, 0, 1, 0, 1, 0x13, 0x88]),
            ("-2.25", vec![0, 2, 0, 0, 0x40, 0, 0, 2, 0, 2, 0x09, 0xc4]),
            ("12345678.90", vec![0, 3, 0, 1, 0, 0, 0, 2, 0x04, 0xd2, 0x16, 0x2e, 0x23, 0x28]),
            ("0.0001", vec![0, 1, 0xff, 0xff, 0, 0, 0, 4, 0, 1]),
            ("10000", vec![0, 1, 0, 1, 0, 0, 0, 0, 0, 1]),
            ("NaN", vec![0, 0, 0, 0, 0xc0, 0, 0, 0]),
        ] {
            assert_eq!(numeric_send(text).unwrap(), bytes, "{text}");
            assert_eq!(numeric_recv(&bytes).unwrap(), text, "{text}");
        }
    }
}
