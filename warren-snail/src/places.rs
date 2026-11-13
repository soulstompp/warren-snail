//! The places of an order's levels, outermost first: the sublevels its blocks are cut on.
//!
//! An integer is cut by its bits, the top one first, a uuid by its 128, and a boolean on one, false
//! before true. An
//! instant is cut on the UTC clock by its year, then its month, day, hour, minute, second and
//! microsecond. A month is cut by the year's halves, then the month. Text is cut by its bytes, the
//! first one first, a value that has ended coming before any byte.
//!
//! A numeric past 18 digits is cut by its decimal digits: one of 19 to 38 digits by those of its
//! integer offset by 10^38, the first one first; any other by the bytes of its class's key, whose
//! bytes past its kind and the place of its first digit are its digits.
//!
//! A level's NULL rows lie on their own cycle: a run after every value of the level at each of its
//! sublevels, or before them where the level holds its NULLs first, cut on the levels after it.

use crate::catalog::{ColumnType, Level, Order, Table};
use chrono::DateTime;
use std::cmp::Ordering;
use std::fmt;

pub fn civil_utc(micros: i64) -> DateTime<chrono::Utc> {
    DateTime::from_timestamp_micros(micros).expect("an instant PostgreSQL holds")
}

/// The month of an instant on the UTC clock, from its days since 1970-01-01 on the proleptic Gregorian
/// calendar.
pub fn month_utc(micros: i64) -> i64 {
    let z = micros.div_euclid(86_400_000_000) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    if mp < 10 {
        mp + 3
    } else {
        mp - 9
    }
}

/// The month of an instant as PostgreSQL extracts it: none for an infinite one.
pub fn month_of(micros: i64) -> Option<i64> {
    (micros != i64::MAX && micros != i64::MIN).then(|| month_utc(micros))
}

/// One level's value in an order's key: a number (an integer, an instant's microseconds, a month),
/// text, compared by its bytes, a value keyed in 128 bits (a uuid's key), or a value keyed by bytes
/// that need not be text (a bytea's, a bit string's); or a NULL, before every value of its level
/// where `first`, else after them all. Every value of one level is of one kind, and its NULLs lie on
/// one side.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum KeyValue {
    Int(i64),
    Text(Box<str>),
    Null { first: bool },
    Wide(i128),
    Bytes(Box<[u8]>),
}

impl KeyValue {
    /// The number a numeric level keys.
    pub fn int(&self) -> i64 {
        match self {
            KeyValue::Int(v) => *v,
            KeyValue::Text(_) => panic!("a numeric level holds text"),
            KeyValue::Bytes(_) => panic!("a numeric level holds bytes"),
            KeyValue::Wide(_) => panic!("a level keyed in 128 bits holds no 64-bit number"),
            KeyValue::Null { .. } => panic!("a NULL keys no number"),
        }
    }

    /// The number a numeric level keys, in 128 bits.
    pub fn wide(&self) -> i128 {
        match self {
            KeyValue::Wide(v) => *v,
            other => other.int() as i128,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, KeyValue::Null { .. })
    }

    /// Where the value's kind lies among a level's: its NULLs at one end, whatever kinds come after.
    fn rank(&self) -> u8 {
        match self {
            KeyValue::Null { first: true } => 0,
            KeyValue::Int(_) => 1,
            KeyValue::Text(_) => 2,
            KeyValue::Wide(_) => 3,
            KeyValue::Bytes(_) => 4,
            KeyValue::Null { first: false } => u8::MAX,
        }
    }
}

impl Ord for KeyValue {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (KeyValue::Int(a), KeyValue::Int(b)) => a.cmp(b),
            (KeyValue::Text(a), KeyValue::Text(b)) => a.cmp(b),
            (KeyValue::Wide(a), KeyValue::Wide(b)) => a.cmp(b),
            (KeyValue::Bytes(a), KeyValue::Bytes(b)) => a.cmp(b),
            _ => self.rank().cmp(&other.rank()),
        }
    }
}

impl PartialOrd for KeyValue {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for KeyValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyValue::Int(v) => write!(f, "{v}"),
            KeyValue::Text(s) => write!(f, "'{}'", s.replace('\'', "''")),
            KeyValue::Wide(v) => write!(f, "'{}'", crate::scalars::uuid_text(*v)),
            KeyValue::Bytes(b) => write!(f, "'{}'", crate::strings::bytea_out(b)),
            KeyValue::Null { .. } => write!(f, "NULL"),
        }
    }
}

/// The NULL of level `level` of an order, on the side the order holds its NULLs.
pub fn null_key(order: &Order, level: usize) -> KeyValue {
    KeyValue::Null {
        first: order.nulls_first[level],
    }
}

/// A key as text: its values, outermost first.
pub fn shown(key: &[KeyValue]) -> String {
    format!(
        "[{}]",
        key.iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// A key as text, its values outermost first, each written as its column's type writes it.
pub fn shown_as(key: &[KeyValue], types: &[ColumnType]) -> String {
    format!(
        "[{}]",
        key.iter()
            .zip(types)
            .map(|(v, &ty)| match v {
                KeyValue::Bytes(k) if ty.is_bits() => format!("'{}'", crate::strings::bits_text(k)),
                KeyValue::Int(k) if crate::scalars::is_scalar(ty) => {
                    let t = crate::scalars::scalar_text(ty, *k);
                    if t.bytes().all(|b| b.is_ascii_digit()) {
                        t
                    } else {
                        format!("'{}'", t.replace('\'', "''"))
                    }
                }
                other => other.to_string(),
            })
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// A number in 128 bits against a numeric level's value, a NULL lying on its level's side.
pub fn wide_cmp(k: i128, b: &KeyValue) -> Ordering {
    match b {
        KeyValue::Int(x) => k.cmp(&(*x as i128)),
        KeyValue::Wide(x) => k.cmp(x),
        KeyValue::Null { first: true } => Ordering::Greater,
        KeyValue::Null { first: false } => Ordering::Less,
        KeyValue::Text(_) | KeyValue::Bytes(_) => Ordering::Less,
    }
}

/// Writes a part of varying length into a key of parts in a row: each 0x00 as 0x00 0xFF, then
/// 0x00 0x00 to end it, so that the key's bytes compare as the parts do, one after another.
pub fn varying_part(out: &mut Vec<u8>, part: &[u8]) {
    for &b in part {
        out.push(b);
        if b == 0 {
            out.push(0xff);
        }
    }
    out.extend_from_slice(&[0, 0]);
}

/// The first part of a key of parts in a row, a part of varying length, and the key after it.
pub fn split_varying(key: &[u8]) -> (Vec<u8>, &[u8]) {
    let mut part = Vec::with_capacity(key.len());
    let mut i = 0;
    while i < key.len() {
        if key[i] == 0 {
            if key.get(i + 1) == Some(&0xff) {
                part.push(0);
                i += 2;
                continue;
            }
            return (part, &key[(i + 2).min(key.len())..]);
        }
        part.push(key[i]);
        i += 1;
    }
    (part, &[])
}

/// The value a level of a string type, bytea or a bit string keys, from the bytes its value is
/// keyed by (`strings::key_bytes`): text for a string, bytes for the others.
pub fn byte_key(ty: ColumnType, key: &[u8]) -> KeyValue {
    if ty.keyed_by_bytes() {
        KeyValue::Bytes(key.into())
    } else {
        KeyValue::Text(String::from_utf8_lossy(key).into())
    }
}

/// The value a level of a column keys, from the bytes its value is keyed by: bytes for a string
/// under a library collation, else as `byte_key` says.
pub fn column_byte_key(column: &crate::catalog::Column, key: &[u8]) -> KeyValue {
    if column.collated() {
        KeyValue::Bytes(key.into())
    } else {
        byte_key(column.ty, key)
    }
}

/// The sublevels a level of a string type, bytea or a bit string is cut on so that a value keyed
/// by `key` lies whole on them, at least `had`: one more than its bytes, or than a bit varying's
/// bits; `BYTES_LEVEL` beside them for a level keyed by bytes, as `had` says of a string under a
/// library collation. Where `had` holds `BITS_LEVEL`, a bit a rung, a composed value's on its
/// structure, its key written as hex.
pub fn rungs_for(ty: ColumnType, key: &[u8], had: u32) -> u32 {
    use crate::directory::{BITS_LEVEL, BYTES_LEVEL};
    let bits = had & BITS_LEVEL != 0;
    let n = match ty {
        ColumnType::Bit(n) => n,
        ColumnType::Varbit(_) => crate::strings::bits_of_key(key).0 + 1,
        ColumnType::Composed(r) if bits && crate::composed::is_nested(r) => {
            crate::composed::structure_rungs(ty, &|i| hex_byte(key, i)) as u32 + 1
        }
        _ if bits => 8 * key.len() as u32 + 1,
        _ => key.len() as u32 + 1,
    };
    let n = n.max(had & !(BYTES_LEVEL | BITS_LEVEL));
    let n = if bits { n | BITS_LEVEL } else { n };
    if ty.keyed_by_bytes() || had & BYTES_LEVEL != 0 {
        n | BYTES_LEVEL
    } else {
        n
    }
}

/// Byte `i` of a key written as hex, two digits a byte.
fn hex_byte(hex: &[u8], i: usize) -> Option<u8> {
    let d = |c: u8| (c as char).to_digit(16).unwrap_or(0) as u8;
    let h = hex.get(2 * i..2 * i + 2)?;
    Some(d(h[0]) << 4 | d(h[1]))
}

/// The rungs a part of a composed value of type `ty` is cut on, as a level of the type is.
pub fn part_rungs(ty: ColumnType) -> usize {
    sublevel_count(Level::Column(0), ty, 0)
}

/// Rung `r` of a part of a composed value of type `ty` keyed `key`, as a level of the type is cut.
pub fn part_rung(ty: ColumnType, key: &KeyValue, r: usize) -> i64 {
    sublevel_of(Level::Column(0), ty, key, r, false)
}

/// The value a numeric level keys, from its column's value.
pub fn key(level: Level, value: i64) -> KeyValue {
    match level {
        Level::Column(_) => KeyValue::Int(value),
        Level::MonthUtc(_) => KeyValue::Int(month_utc(value)),
        Level::Scaled(_, k) => KeyValue::Int(scaled_key(value, k)),
    }
}

/// The key of a double precision keyed `class` (`scalars::float8_key`) times 10 to the `k`, floored,
/// as PostgreSQL's float8mul and floor compute it.
pub fn scaled_key(class: i64, k: u32) -> i64 {
    let f = float8_of_key(class);
    crate::scalars::float8_key((f * 10f64.powi(k as i32)).floor().to_bits())
}

/// The double precision a class key (`scalars::float8_key`) stands for, `-0` as `0`.
pub fn float8_of_key(class: i64) -> f64 {
    if class == 0x7ff0_0000_0000_0001 {
        return f64::NAN;
    }
    let s = if class >= 0 {
        class
    } else {
        class ^ 0x7fff_ffff_ffff_ffff
    };
    f64::from_bits(s as u64)
}

/// Rung `r` of a count of `k` places' units keyed `class`: the whole unit (a degree) first, then each
/// place's digit as two rungs, its half of the ten and its place in that half; an infinity a place at
/// either end, NaN past the top.
fn scaled_rung(class: i64, k: u32, r: usize) -> i64 {
    let f = float8_of_key(class);
    if !f.is_finite() {
        let at = if f.is_nan() {
            i64::MAX
        } else if f > 0.0 {
            i64::MAX - 1
        } else {
            i64::MIN + 1
        };
        return if r == 0 { at } else { 0 };
    }
    let n = f as i128;
    let unit = 10i128.pow(k);
    if r == 0 {
        return n.div_euclid(unit) as i64;
    }
    let place = (r - 1) / 2;
    let digit = (n.rem_euclid(unit) / 10i128.pow(k - 1 - place as u32)) % 10;
    (if (r - 1) % 2 == 0 {
        digit / 5
    } else {
        digit % 5
    }) as i64
}

/// The value level `li` of an order keys, from its column's value: an infinite instant's month is
/// NULL, as PostgreSQL's extract gives it, and lies on the level's NULL run.
pub fn level_key(order: &Order, li: usize, value: i64) -> KeyValue {
    match order.levels[li] {
        Level::MonthUtc(_) if month_of(value).is_none() => null_key(order, li),
        l => key(l, value),
    }
}

/// Whether a level is read by equality, rather than walked as a line.
pub fn by_equality(level: Level, table: &Table) -> bool {
    match level {
        Level::MonthUtc(_) => true,
        Level::Scaled(..) => true,
        Level::Column(c) => !matches!(
            table.columns[c].ty,
            ColumnType::Timestamptz
                | ColumnType::TimestamptzRounded(_)
                | ColumnType::Timestamp(_)
                | ColumnType::Time(_)
                | ColumnType::Timetz(_)
        ),
    }
}

fn sublevel_count(level: Level, ty: ColumnType, text_sublevels: u32) -> usize {
    match level {
        Level::MonthUtc(_) => 2,
        Level::Scaled(_, k) => 1 + 2 * k as usize,
        Level::Column(_) => match ty {
            ColumnType::Int2 => 16,
            ColumnType::Int4
            | ColumnType::Oid
            | ColumnType::Reg(..)
            | ColumnType::Xid
            | ColumnType::Cid => 32,
            ColumnType::Numeric { precision, .. } if precision > 18 => WIDE_DECIMAL_DIGITS as usize,
            ColumnType::Int8
            | ColumnType::Numeric { .. }
            | ColumnType::Xid8
            | ColumnType::PgLsn
            | ColumnType::Oid8
            | ColumnType::Money(_) => 64,
            ColumnType::Char => 8,
            ColumnType::Macaddr | ColumnType::Tid => 48,
            ColumnType::Macaddr8 => 64,
            ColumnType::Float4 => 32,
            ColumnType::Float8 => 64,
            ColumnType::Timestamptz
            | ColumnType::TimestamptzRounded(_)
            | ColumnType::Timestamp(_) => 7,
            ColumnType::Date => 3,
            ColumnType::Time(_) => 4,
            ColumnType::Timetz(_) => 5,
            ColumnType::Interval { .. } => 6,
            ColumnType::Boolean => 1,
            ColumnType::Uuid | ColumnType::Geometric(_) => 128,
            ColumnType::Bit(n) => n as usize,
            ColumnType::Text(_)
            | ColumnType::Bpchar(_)
            | ColumnType::Name
            | ColumnType::NumericText(_)
            | ColumnType::Bytes
            | ColumnType::Varbit(_)
            | ColumnType::Textual(_)
            | ColumnType::Internal(_) => {
                (text_sublevels & !(crate::directory::BYTES_LEVEL | crate::directory::BITS_LEVEL))
                    as usize
            }
            // An enum by its rank's bits; any other composed type by its key written as text.
            ColumnType::Composed(r) if crate::composed::is_nested(r) => {
                (text_sublevels & !crate::directory::BITS_LEVEL) as usize
            }
            ColumnType::Composed(_) => 32,
        },
    }
}

/// The decimal digits a numeric of 19 to 38 digits is cut by: those of its integer offset by 10^38.
const WIDE_DECIMAL_DIGITS: u32 = 39;

/// Decimal digit `r` of a numeric's integer offset by 10^38, the first one first.
fn decimal_digit(k: i128, r: usize) -> i64 {
    let biased = (k as u128).wrapping_add(10u128.pow(38));
    ((biased / 10u128.pow(WIDE_DECIMAL_DIGITS - 1 - r as u32)) % 10) as i64
}

fn sublevel_of(level: Level, ty: ColumnType, key: &KeyValue, r: usize, bits: bool) -> i64 {
    let bit = |b: &[u8]| {
        b.get(r / 8)
            .map_or(-1, |&x| ((x >> (7 - r % 8)) & 1) as i64)
    };
    match key {
        KeyValue::Text(s) if bits => {
            return match ty {
                ColumnType::Composed(t) if crate::composed::is_nested(t) => {
                    crate::composed::structure_rung(ty, &|i| hex_byte(s.as_bytes(), i), r)
                }
                _ => bit(s.as_bytes()),
            }
        }
        KeyValue::Text(s) => return s.as_bytes().get(r).map_or(-1, |&b| b as i64),
        KeyValue::Null { first: true } => return i64::MIN,
        KeyValue::Null { first: false } => return i64::MAX,
        KeyValue::Wide(k) if ty.is_numeric() => return decimal_digit(*k, r),
        KeyValue::Wide(k) if matches!(ty, ColumnType::Interval { .. }) => {
            return interval_rung(*k, r)
        }
        KeyValue::Wide(k) => return ((k.wrapping_sub(i128::MIN) as u128 >> (127 - r)) & 1) as i64,
        // A bit string is cut by its bits, each a place of three, an ended one first; bytea by its
        // bytes, as text is.
        KeyValue::Bytes(b) if ty.is_bits() => return crate::strings::bit_of_key(b, r),
        KeyValue::Bytes(b) if bits => return bit(b),
        KeyValue::Bytes(b) => return b.get(r).map_or(-1, |&x| x as i64),
        KeyValue::Int(_) => {}
    }
    let key = key.int();
    match level {
        Level::MonthUtc(_) => match r {
            0 => (key - 1) / 6,
            _ => key,
        },
        Level::Scaled(_, k) => scaled_rung(key, k, r),
        Level::Column(_) => match ty {
            ColumnType::Int2
            | ColumnType::Int4
            | ColumnType::Int8
            | ColumnType::Numeric { .. }
            | ColumnType::Oid
            | ColumnType::Reg(..)
            | ColumnType::Oid8
            | ColumnType::Xid
            | ColumnType::Cid
            | ColumnType::Xid8
            | ColumnType::PgLsn
            | ColumnType::Char
            | ColumnType::Money(_)
            | ColumnType::Macaddr
            | ColumnType::Macaddr8
            | ColumnType::Float4
            | ColumnType::Float8
            | ColumnType::Tid
            | ColumnType::Composed(_) => {
                let bits = sublevel_count(level, ty, 0);
                let low = -(1i128 << (bits - 1));
                let biased = (key as i128 - low) as u64;
                ((biased >> (bits - 1 - r)) & 1) as i64
            }
            ColumnType::Timestamptz
            | ColumnType::TimestamptzRounded(_)
            | ColumnType::Timestamp(_) => match end_rung(key, i64::MIN, i64::MAX, r) {
                Some(v) => v,
                None => {
                    let (year, month, day, hour, minute, second, micro) = crate::times::civil(key);
                    match r {
                        0 => year,
                        1 => month as i64,
                        2 => day as i64,
                        3 => hour as i64,
                        4 => minute as i64,
                        5 => second as i64,
                        _ => micro as i64,
                    }
                }
            },
            ColumnType::Date => match end_rung(key, i32::MIN as i64, i32::MAX as i64, r) {
                Some(v) => v,
                None => {
                    let (year, month, day) =
                        crate::times::j2date(key + crate::times::UNIX_EPOCH_JDATE);
                    match r {
                        0 => year,
                        1 => month as i64,
                        _ => day as i64,
                    }
                }
            },
            ColumnType::Time(_) => clock_rung(key, r),
            ColumnType::Timetz(_) => {
                let (utc, zone) = crate::scalars::timetz_utc(key);
                match r {
                    0..=3 => clock_rung(utc, r),
                    _ => zone,
                }
            }
            ColumnType::Interval { .. } => unreachable!("an interval level keys 128 bits"),
            ColumnType::Boolean => key,
            ColumnType::Text(_)
            | ColumnType::Bytes
            | ColumnType::Bpchar(_)
            | ColumnType::Name
            | ColumnType::Bit(_)
            | ColumnType::Varbit(_)
            | ColumnType::NumericText(_)
            | ColumnType::Geometric(_)
            | ColumnType::Textual(_)
            | ColumnType::Internal(_) => {
                unreachable!("a level of a string or a numeric text keys text; of a bit string, bytea or a value held as its text, bytes")
            }
            ColumnType::Uuid => unreachable!("a uuid level keys 128 bits"),
        },
    }
}

/// An infinity's rung `r` on the calendar: a place before every year or after the last, the same
/// at every rung after; none for a finite value.
fn end_rung(key: i64, start: i64, end: i64, r: usize) -> Option<i64> {
    let end_place = |at: i64| Some(if r == 0 { at } else { 0 });
    if key == start {
        end_place(i64::MIN + 1)
    } else if key == end {
        end_place(i64::MAX - 1)
    } else {
        None
    }
}

/// Rung `r` of microseconds on the clock: the hour, then the minute, the second and the microsecond.
fn clock_rung(micros: i64, r: usize) -> i64 {
    match r {
        0 => micros.div_euclid(3_600_000_000),
        1 => micros.rem_euclid(3_600_000_000) / 60_000_000,
        2 => micros.rem_euclid(60_000_000) / 1_000_000,
        _ => micros.rem_euclid(1_000_000),
    }
}

/// Rung `r` of an interval's total: its months of 30 days, then the day among them, then the clock;
/// its infinities a place at each end.
fn interval_rung(total: i128, r: usize) -> i64 {
    use crate::times::Interval;
    let month = 30 * crate::times::MICROS_PER_DAY as i128;
    if total == Interval::INFINITY.total() || total == Interval::NEG_INFINITY.total() {
        let at = if total > 0 {
            i64::MAX - 1
        } else {
            i64::MIN + 1
        };
        return if r == 0 { at } else { 0 };
    }
    let within = total.rem_euclid(month) as i64;
    match r {
        0 => total.div_euclid(month) as i64,
        1 => within / crate::times::MICROS_PER_DAY,
        _ => clock_rung(within.rem_euclid(crate::times::MICROS_PER_DAY), r - 2),
    }
}

/// The sublevels an order's blocks are cut on, outermost first, each a sublevel of one of its levels.
pub struct Sublevels {
    map: Vec<(usize, usize)>,
    levels: Vec<(Level, ColumnType, bool)>,
}

impl Sublevels {
    /// `text_sublevels` holds, for each level, the sublevels a text level is cut on (its longest value's bytes
    /// and one more), and 0 for a numeric level, whose sublevels its type gives.
    pub fn new(order: &Order, table: &Table, text_sublevels: &[u32]) -> Sublevels {
        let levels: Vec<(Level, ColumnType, bool)> = order
            .levels
            .iter()
            .enumerate()
            .map(|(i, &l)| {
                let text = text_sublevels.get(i).copied().unwrap_or(0);
                let bits = text != crate::directory::WIDE_LEVEL
                    && text & crate::directory::BITS_LEVEL != 0;
                (l, table.columns[l.column()].ty, bits)
            })
            .collect();
        let mut map = Vec::new();
        for (i, &(l, ty, _)) in levels.iter().enumerate() {
            for r in 0..sublevel_count(l, ty, text_sublevels.get(i).copied().unwrap_or(0)) {
                map.push((i, r));
            }
        }
        Sublevels { map, levels }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Each level's sublevels, from its first to one past its last.
    pub(crate) fn level_rungs(&self) -> Vec<(usize, usize)> {
        let mut at = 0;
        (0..self.levels.len())
            .map(|i| {
                let n = self.map.iter().filter(|m| m.0 == i).count();
                at += n;
                (at - n, at)
            })
            .collect()
    }

    /// The value at sublevel `r` for a row whose levels key `key`.
    pub fn value(&self, key: &[KeyValue], r: usize) -> i64 {
        let (i, sub) = self.map[r];
        let (l, ty, bits) = self.levels[i];
        sublevel_of(l, ty, &key[i], sub, bits)
    }

    /// The values at every sublevel for a row whose levels key `key`, a composed level's read in one
    /// walk of its key.
    pub fn all(&self, key: &[KeyValue]) -> Vec<i64> {
        let mut out = Vec::with_capacity(self.map.len());
        for (r, &(i, sub)) in self.map.iter().enumerate() {
            let (l, ty, bits) = self.levels[i];
            match (&key[i], ty) {
                (KeyValue::Text(s), ColumnType::Composed(t))
                    if bits && sub == 0 && crate::composed::is_nested(t) =>
                {
                    let n = self.map[r..].iter().take_while(|m| m.0 == i).count();
                    let mut walk =
                        crate::composed::structure_all(ty, &|b| hex_byte(s.as_bytes(), b));
                    walk.resize(n, -1);
                    out.extend(walk);
                }
                (KeyValue::Text(_), ColumnType::Composed(t))
                    if bits && crate::composed::is_nested(t) => {}
                _ => out.push(sublevel_of(l, ty, &key[i], sub, bits)),
            }
        }
        out
    }
}

/// Rows' keys, one per level of an order, laid flat.
pub struct Keys {
    pub stride: usize,
    pub data: Vec<KeyValue>,
}

impl Keys {
    pub fn row(&self, i: usize) -> &[KeyValue] {
        &self.data[i * self.stride..(i + 1) * self.stride]
    }

    pub fn len(&self) -> usize {
        self.data.len().checked_div(self.stride).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

/// Keys of one numeric level each, from the numbers given.
pub fn ints(values: impl IntoIterator<Item = i64>) -> Vec<KeyValue> {
    values.into_iter().map(KeyValue::Int).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Catalog;
    use chrono::Datelike;

    #[test]
    fn an_integer_is_cut_by_its_bits_top_first_and_keeps_its_order() {
        let c = Catalog::parse("CREATE SCHEMA s; CREATE TABLE s.t (a integer NOT NULL, PRIMARY KEY (a));").unwrap();
        let t = &c.tables[0];
        let sublevels = Sublevels::new(&t.orders[0], t, &[0]);
        assert_eq!(sublevels.len(), 32);
        let bits = |v: i64| {
            (0..32)
                .map(|r| sublevels.value(&[KeyValue::Int(v)], r))
                .collect::<Vec<_>>()
        };
        assert_eq!(bits(-1)[..2], [0, 1]);
        assert_eq!(bits(0)[..2], [1, 0]);
        assert_eq!(bits(5)[29..], [1, 0, 1]);
        let mut all: Vec<Vec<i64>> = [-7, -1, 0, 3, 1 << 20].iter().map(|&v| bits(v)).collect();
        let sorted = all.clone();
        all.sort();
        assert_eq!(all, sorted);
    }

    #[test]
    fn a_uuid_is_cut_by_its_128_bits_top_first_and_keeps_the_order_of_its_bytes() {
        let c = Catalog::parse("CREATE SCHEMA s; CREATE TABLE s.t (a uuid NOT NULL, PRIMARY KEY (a));").unwrap();
        let t = &c.tables[0];
        let sublevels = Sublevels::new(&t.orders[0], t, &[0]);
        assert_eq!(sublevels.len(), 128);
        let bits = |b: [u8; 16]| {
            (0..128)
                .map(|r| sublevels.value(&[KeyValue::Wide(crate::scalars::uuid_key(b))], r))
                .collect::<Vec<_>>()
        };
        let mut low = [0u8; 16];
        low[15] = 5;
        assert_eq!(bits(low)[125..], [1, 0, 1]);
        assert_eq!(bits([0x80; 16])[..2], [1, 0]);
        let mut all: Vec<[u8; 16]> = [0u8, 1, 0x7f, 0x80, 0xff]
            .iter()
            .flat_map(|&a| {
                [0u8, 0x80, 0xff].map(|z| {
                    let mut b = [0u8; 16];
                    b[0] = a;
                    b[15] = z;
                    b
                })
            })
            .collect();
        let mut by_bits = all.clone();
        by_bits.sort_by_key(|&b| bits(b));
        all.sort();
        assert_eq!(by_bits, all);
    }

    #[test]
    fn an_instant_is_cut_by_its_calendar_on_the_utc_clock() {
        let c = Catalog::parse(
            "CREATE SCHEMA s; CREATE FUNCTION s.c(timestamptz) RETURNS timestamp LANGUAGE sql AS $$ SELECT $1 AT TIME ZONE 'UTC' $$;
             CREATE TABLE s.t (at timestamptz NOT NULL);
             CREATE INDEX m ON s.t ((extract(month FROM s.c(at))::smallint), at);
             ALTER TABLE s.t CLUSTER ON m;",
        )
        .unwrap();
        let t = &c.tables[0];
        let sublevels = Sublevels::new(&t.orders[0], t, &[0, 0]);
        assert_eq!(sublevels.len(), 9);
        // 2017-12-31 23:59:59.000001 UTC
        let at = 1_514_764_799_000_001;
        let key = [KeyValue::Int(month_utc(at)), KeyValue::Int(at)];
        let v: Vec<i64> = (0..9).map(|r| sublevels.value(&key, r)).collect();
        assert_eq!(v, [1, 12, 2017, 12, 31, 23, 59, 59, 1]);
        assert_eq!(month_utc(at + 999_999), 1);
    }

    /// Rung `r` of every level of a key, for the order's one level.
    fn rungs(ddl: &str, key: KeyValue) -> Vec<i64> {
        let c = Catalog::parse(ddl).unwrap();
        let t = &c.tables[0];
        let sublevels = Sublevels::new(&t.orders[0], t, &[0]);
        (0..sublevels.len())
            .map(|r| sublevels.value(std::slice::from_ref(&key), r))
            .collect()
    }

    #[test]
    fn every_date_s_calendar_rises_with_it_and_its_infinities_lie_at_the_ends() {
        let ddl = "CREATE SCHEMA s; CREATE TABLE s.t (d date NOT NULL, PRIMARY KEY (d));";
        let c = Catalog::parse(ddl).unwrap();
        let t = &c.tables[0];
        let sublevels = Sublevels::new(&t.orders[0], t, &[0]);
        assert_eq!(sublevels.len(), 3);
        let at = |d: i64| -> [i64; 3] {
            let k = [KeyValue::Int(d)];
            [0, 1, 2].map(|r| sublevels.value(&k, r))
        };
        // From PostgreSQL's first date, 4714-11-24 BC, to its last, 5874897-12-31, day by day, each
        // day's year, month and day counted on alongside.
        let (first, last) = (
            crate::times::date2j(-4713, 11, 24) - crate::times::UNIX_EPOCH_JDATE,
            crate::times::date2j(5_874_897, 12, 31) - crate::times::UNIX_EPOCH_JDATE,
        );
        let leap =
            |y: i64| y.rem_euclid(4) == 0 && (y.rem_euclid(100) != 0 || y.rem_euclid(400) == 0);
        let length = |y: i64, m: i64| match m {
            2 if leap(y) => 29,
            2 => 28,
            4 | 6 | 9 | 11 => 30,
            _ => 31,
        };
        // Every date's calendar, as the rungs read it; the rungs themselves on every 61st day.
        let mut civil = [-4713i64, 11, 24];
        for d in first..=last {
            let (y, m, day) = crate::times::j2date(d + crate::times::UNIX_EPOCH_JDATE);
            assert!(
                y == civil[0] && i64::from(m) == civil[1] && i64::from(day) == civil[2],
                "{d}"
            );
            if d % 61 == 0 || d == last {
                assert_eq!(at(d), civil, "{d}");
            }
            civil[2] += 1;
            if civil[2] > length(civil[0], civil[1]) {
                civil[2] = 1;
                civil[1] += 1;
                if civil[1] > 12 {
                    civil[1] = 1;
                    civil[0] += 1;
                }
            }
        }
        let (start, end) = (at(i32::MIN as i64), at(i32::MAX as i64));
        assert!(start < at(first) && at(last) < end);
        assert_eq!(
            (start[1..].to_vec(), end[1..].to_vec()),
            (vec![0, 0], vec![0, 0])
        );
        let null = [KeyValue::Null { first: false }];
        assert!(end < [0, 1, 2].map(|r| sublevels.value(&null, r)));
    }

    #[test]
    fn an_instant_s_infinities_lie_before_every_year_and_after_the_last() {
        for ddl in [
            "CREATE SCHEMA s; CREATE TABLE s.t (at timestamptz NOT NULL, PRIMARY KEY (at));",
            "CREATE SCHEMA s; CREATE TABLE s.t (at timestamp NOT NULL, PRIMARY KEY (at));",
        ] {
            let first = crate::times::date2j(-4713, 11, 24) - crate::times::UNIX_EPOCH_JDATE;
            let low = rungs(ddl, KeyValue::Int(first * 86_400_000_000));
            let high = rungs(ddl, KeyValue::Int(i64::MAX - 1));
            assert_eq!(low, [-4713, 11, 24, 0, 0, 0, 0]);
            assert_eq!(high, [294_247, 1, 10, 4, 0, 54, 775_806]);
            let (start, end) = (
                rungs(ddl, KeyValue::Int(i64::MIN)),
                rungs(ddl, KeyValue::Int(i64::MAX)),
            );
            assert!(start < low && high < end, "{ddl}");
        }
    }

    #[test]
    fn a_time_is_cut_by_the_clock_its_hour_running_0_to_24() {
        let ddl = "CREATE SCHEMA s; CREATE TABLE s.t (v time NOT NULL, PRIMARY KEY (v));";
        assert_eq!(rungs(ddl, KeyValue::Int(0)), [0, 0, 0, 0]);
        assert_eq!(
            rungs(ddl, KeyValue::Int(86_399_999_999)),
            [23, 59, 59, 999_999]
        );
        assert_eq!(rungs(ddl, KeyValue::Int(86_400_000_000)), [24, 0, 0, 0]);
    }

    #[test]
    fn a_timetz_sorts_and_is_cut_by_its_utc_clock_then_its_zone() {
        let ddl = "CREATE SCHEMA s; CREATE TABLE s.t (v timetz NOT NULL, PRIMARY KEY (v));";
        let h = 3_600_000_000i64;
        // PostgreSQL's order of these (types/time-ranges-composed.md, timetz), each zone west of UTC.
        let sorted = [
            (0, -14 * 3600),
            (h, 0),
            (12 * h, -3600),
            (11 * h, 0),
            (10 * h, 3600),
            (0, 12 * 3600),
            (23 * h, 5 * 3600),
        ];
        let keys: Vec<i64> = sorted
            .iter()
            .map(|&(t, z)| crate::scalars::timetz_key(t, z))
            .collect();
        assert!(keys.windows(2).all(|w| w[0] < w[1]));
        for (&k, &(t, z)) in keys.iter().zip(&sorted) {
            assert_eq!(crate::scalars::timetz_parts(k), (t, z));
        }
        let cut: Vec<Vec<i64>> = keys.iter().map(|&k| rungs(ddl, KeyValue::Int(k))).collect();
        assert!(cut.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(cut[0][0], -14);
        assert_eq!(cut[6][0], 28);
        // The same instant in two zones: one place on the clock, two on the zone.
        assert_eq!(cut[2][..4], cut[3][..4]);
        let (most, least) = (15 * 3600 + 59 * 60, -(15 * 3600 + 59 * 60));
        assert!(
            crate::scalars::timetz_key(0, most) > crate::scalars::timetz_key(0, least),
            "a more westerly zone sorts later"
        );
        assert_eq!(
            rungs(
                ddl,
                KeyValue::Int(crate::scalars::timetz_key(86_400_000_000, most))
            )[0],
            39
        );
    }

    #[test]
    fn an_interval_sorts_and_is_cut_by_its_total_of_30_day_months() {
        use crate::times::Interval;
        let ddl = "CREATE SCHEMA s; CREATE TABLE s.t (v interval NOT NULL, PRIMARY KEY (v));";
        let (d, u) = (86_400_000_000i64, 3_600_000_000i64);
        let v = |months, days, micros| Interval {
            months,
            days,
            micros,
        };
        // PostgreSQL's order of these (types/time-ranges-composed.md, interval).
        let sorted = [
            v(2, -31, 0),
            v(1, 0, -1),
            v(1, 0, 0),
            v(0, 29, 25 * u),
            v(-1, 61, 0),
        ];
        let cut: Vec<Vec<i64>> = sorted
            .iter()
            .map(|x| rungs(ddl, KeyValue::Wide(x.total())))
            .collect();
        assert!(cut.windows(2).all(|w| w[0] <= w[1]));
        assert_eq!(cut[0], [0, 29, 0, 0, 0, 0]);
        assert_eq!(cut[1], [0, 29, 23, 59, 59, 999_999]);
        assert_eq!(cut[2], [1, 0, 0, 0, 0, 0]);
        assert_eq!(cut[3], [1, 0, 1, 0, 0, 0]);
        // Equal intervals share a place: 1 day and 24 hours, 1 mon and 30 days.
        assert_eq!(v(0, 1, 0).total(), v(0, 0, d).total());
        assert_eq!(v(1, 0, 0).total(), v(0, 30, 0).total());
        let (start, end) = (
            rungs(ddl, KeyValue::Wide(Interval::NEG_INFINITY.total())),
            rungs(ddl, KeyValue::Wide(Interval::INFINITY.total())),
        );
        let near = rungs(
            ddl,
            KeyValue::Wide(v(i32::MAX, i32::MAX, i64::MAX - 1).total()),
        );
        assert!(start < cut[0] && near < end);
    }

    #[test]
    fn the_month_is_the_calendar_s_on_the_utc_clock_and_none_for_an_infinite_instant() {
        let day = 86_400_000_000i64;
        // Every day from 1600 to 2400, at its first and last microsecond.
        let (first, last) = (-11_676_096_000_000_000i64, 13_569_465_600_000_000i64);
        let mut at = first;
        while at < last {
            for t in [at, at + day - 1] {
                assert_eq!(month_utc(t), civil_utc(t).month() as i64, "{t}");
            }
            at += day;
        }
        assert_eq!(
            (month_of(i64::MAX), month_of(i64::MIN), month_of(0)),
            (None, None, Some(1))
        );
    }

    #[test]
    fn a_null_key_lies_after_every_value_of_its_level_or_before_them_all_at_every_sublevel() {
        let (first, last) = (
            KeyValue::Null { first: true },
            KeyValue::Null { first: false },
        );
        for v in [
            KeyValue::Int(i64::MIN),
            KeyValue::Int(0),
            KeyValue::Int(i64::MAX),
            KeyValue::Text("".into()),
            KeyValue::Text("\u{10ffff}".into()),
        ] {
            assert!(first < v && v < last, "{v}");
        }
        assert_eq!(last.cmp(&last.clone()), Ordering::Equal);
        assert_eq!(first.cmp(&first.clone()), Ordering::Equal);
        // A key holding a NULL on its first level lies past every key holding a value there,
        // whatever follows; on the levels after it, keys sharing the NULL keep their order.
        let mut keys = [
            [last.clone(), KeyValue::Int(1)],
            [KeyValue::Int(i64::MAX), KeyValue::Int(9)],
            [last.clone(), KeyValue::Int(0)],
            [KeyValue::Int(-1), KeyValue::Int(0)],
        ];
        keys.sort();
        assert_eq!(
            keys.iter().map(|k| shown(k)).collect::<Vec<_>>(),
            [
                "[-1, 0]",
                "[9223372036854775807, 9]",
                "[NULL, 0]",
                "[NULL, 1]"
            ]
        );
        let c = Catalog::parse(
            "CREATE SCHEMA s; CREATE TABLE s.t (a integer NOT NULL, b integer, PRIMARY KEY (a));
             CREATE INDEX f ON s.t (b NULLS FIRST, a);
             CREATE INDEX l ON s.t (b, a);",
        )
        .unwrap();
        let t = &c.tables[0];
        for (order, null) in [(&t.orders[1], &first), (&t.orders[2], &last)] {
            let sublevels = Sublevels::new(order, t, &[0, 0]);
            let at =
                |key: &[KeyValue]| (0..32).map(|r| sublevels.value(key, r)).collect::<Vec<_>>();
            let null_at = at(&[null.clone(), KeyValue::Int(0)]);
            for v in [i32::MIN as i64, -1, 0, i32::MAX as i64] {
                let value_at = at(&[KeyValue::Int(v), KeyValue::Int(0)]);
                assert_eq!(null_at < value_at, null == &first, "{v}");
                assert_ne!(null_at[0], value_at[0], "{v}");
            }
        }
    }

    #[test]
    fn text_is_cut_by_its_bytes_and_an_ended_value_comes_first() {
        let c = Catalog::parse("CREATE SCHEMA s; CREATE TABLE s.t (s text NOT NULL, PRIMARY KEY (s));").unwrap();
        let t = &c.tables[0];
        let sublevels = Sublevels::new(&t.orders[0], t, &[4]);
        assert_eq!(sublevels.len(), 4);
        let bytes = |s: &str| {
            (0..4)
                .map(|r| sublevels.value(&[KeyValue::Text(s.into())], r))
                .collect::<Vec<_>>()
        };
        assert_eq!(bytes("ab"), [97, 98, -1, -1]);
        let mut all: Vec<(Vec<i64>, &str)> = ["b", "ab", "abc", "", "a"]
            .iter()
            .map(|s| (bytes(s), *s))
            .collect();
        all.sort();
        assert_eq!(
            all.iter().map(|x| x.1).collect::<Vec<_>>(),
            ["", "a", "ab", "abc", "b"]
        );
        let mut keys = vec![
            KeyValue::Text("b".into()),
            KeyValue::Text("ab".into()),
            KeyValue::Text("".into()),
        ];
        keys.sort();
        assert_eq!(shown(&keys), "['', 'ab', 'b']");
    }

    #[test]
    fn parts_in_a_row_compare_as_a_tuple_does_an_ended_part_first() {
        // Every byte string of up to 3 bytes over 0x00, 0x01 and 0xFF, and every pair of two of them.
        let mut parts: Vec<Vec<u8>> = vec![Vec::new()];
        for _ in 0..3 {
            let longer: Vec<Vec<u8>> = parts
                .iter()
                .filter(|p| p.len() == parts.last().unwrap().len())
                .flat_map(|p| {
                    [0u8, 1, 0xff].map(|b| {
                        let mut p = p.clone();
                        p.push(b);
                        p
                    })
                })
                .collect();
            parts.extend(longer);
        }
        assert_eq!(parts.len(), 40);
        let tuples: Vec<(&[u8], &[u8])> = parts
            .iter()
            .flat_map(|a| parts.iter().map(move |b| (a.as_slice(), b.as_slice())))
            .collect();
        let key = |(a, b): (&[u8], &[u8])| {
            let mut k = Vec::new();
            varying_part(&mut k, a);
            varying_part(&mut k, b);
            k
        };
        let keys: Vec<Vec<u8>> = tuples.iter().map(|&t| key(t)).collect();
        for (x, kx) in tuples.iter().zip(&keys) {
            for (y, ky) in tuples.iter().zip(&keys) {
                assert_eq!(kx.cmp(ky), x.cmp(y), "{x:?} {y:?}");
            }
            let (a, rest) = split_varying(kx);
            let (b, end) = split_varying(rest);
            assert_eq!((a.as_slice(), b.as_slice(), end), (x.0, x.1, &[][..]));
        }
    }

    /// Every rung of one level of type `ddl`'s column, cut on `text` rungs, for each value read from its
    /// text.
    fn rungs_of(ddl: &str, values: &[&str]) -> Vec<Vec<i64>> {
        let c = Catalog::parse(ddl).unwrap();
        let t = &c.tables[0];
        let ty = t.columns[0].ty;
        let keys: Vec<KeyValue> = values
            .iter()
            .map(|s| {
                let item = crate::composed::text_in(ty, s).unwrap();
                byte_key(ty, crate::composed::Compound::new(ty, item).hex.as_bytes())
            })
            .collect();
        let text = keys
            .iter()
            .fold(crate::directory::BITS_LEVEL | 1, |had, k| match k {
                KeyValue::Text(s) => rungs_for(ty, s.as_bytes(), had),
                _ => had,
            });
        let sublevels = Sublevels::new(&t.orders[0], t, &[text]);
        keys.iter()
            .map(|k| {
                (0..sublevels.len())
                    .map(|r| sublevels.value(std::slice::from_ref(k), r))
                    .collect()
            })
            .collect()
    }

    #[test]
    fn a_count_of_thousandths_rises_on_its_degree_then_each_place_in_halves_of_5() {
        let key = |x: f64| crate::scalars::float8_key(x.to_bits());
        let rungs = |x: f64| -> Vec<i64> { (0..7).map(|r| scaled_rung(key(x), 3, r)).collect() };
        assert_eq!(rungs(51_507.0), [51, 1, 0, 0, 0, 1, 2]);
        assert_eq!(rungs(-0.0), [0; 7]);
        assert_eq!(rungs(-1.0), [-1, 1, 4, 1, 4, 1, 4]);
        let mut all = vec![f64::NEG_INFINITY];
        all.extend((-2_100..2_100).map(f64::from));
        all.extend([f64::INFINITY, f64::NAN]);
        let by_rungs: Vec<Vec<i64>> = all.iter().map(|&x| rungs(x)).collect();
        assert!(by_rungs.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(
            scaled_key(key(51.5074), 3),
            key(51_507.0),
            "floor(51.5074 * 1000)"
        );
    }

    #[test]
    fn a_text_of_format_nine_is_cut_a_bit_a_rung_an_ended_value_first() {
        let c = Catalog::parse("CREATE SCHEMA s; CREATE TABLE s.t (a text NOT NULL, PRIMARY KEY (a));").unwrap();
        let t = &c.tables[0];
        let values = ["", "a", "a\u{1}", "ab", "b", "\u{7f}", "\u{80}"];
        let text = values
            .iter()
            .fold(crate::directory::BITS_LEVEL | 1, |had, v| {
                rungs_for(t.columns[0].ty, v.as_bytes(), had)
            });
        assert_eq!(text & !crate::directory::BITS_LEVEL, 8 * 2 + 1);
        let sublevels = Sublevels::new(&t.orders[0], t, &[text]);
        let rungs: Vec<Vec<i64>> = values
            .iter()
            .map(|v| {
                (0..sublevels.len())
                    .map(|r| sublevels.value(&[KeyValue::Text((*v).into())], r))
                    .collect()
            })
            .collect();
        assert_eq!(rungs[1][..9], [0, 1, 1, 0, 0, 0, 0, 1, -1]);
        assert!(rungs.windows(2).all(|w| w[0] < w[1]));
        assert!(rungs.iter().flatten().all(|v| (-1..=1).contains(v)));
    }

    #[test]
    fn a_range_is_cut_on_its_structure_its_bounds_each_on_its_own_rungs() {
        // An int4range: its mark, its lower bound's mark, 32 bits and its mark, the upper's the same.
        let ints = rungs_of(
            "CREATE SCHEMA s; CREATE TABLE s.t (r int4range NOT NULL, PRIMARY KEY (r));",
            &["empty", "(,3)", "[-2,3)", "[0,1)", "[0,7)", "[5,)"],
        );
        assert_eq!(ints[0][..2], [0, -1]);
        assert_eq!(ints[3].len(), 1 + 2 * (1 + 32 + 1) + 1);
        let bits =
            |v: i64| (0..32).map(move |r| (((v as i128 + (1 << 31)) >> (31 - r)) & 1) as i64);
        assert!(ints[3][2..34].iter().copied().eq(bits(0)));
        assert!(ints[4][36..68].iter().copied().eq(bits(7)));
        assert!(ints.windows(2).all(|w| w[0] < w[1]));
        // A tstzrange's bounds on the calendar, year first.
        let instants = rungs_of(
            "CREATE SCHEMA s; CREATE TABLE s.t (r tstzrange NOT NULL, PRIMARY KEY (r));",
            &[
                "[-infinity,2019-06-01 00:00:00+00)",
                "[2017-12-31 23:59:59.5+00,2018-01-01 00:00:00+00)",
                "[2018-02-03 04:05:06.000007+00,infinity)",
            ],
        );
        assert_eq!(instants[2][2..9], [2018, 2, 3, 4, 5, 6, 7]);
        assert_eq!(instants[1][11..14], [2018, 1, 1]);
        assert!(instants.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn an_array_of_text_is_cut_by_the_bits_of_its_elements_bytes_in_postgresql_s_order() {
        let arrays = rungs_of(
            "CREATE SCHEMA s; CREATE TABLE s.t (a text[] NOT NULL, PRIMARY KEY (a));",
            &["{}", "{a,b}", "{a,NULL}", "{ab}", "{b}", "{b,a}"],
        );
        // A mark, then the element's byte as eight bits, then the bits of its end.
        assert_eq!(arrays[1][..9], [1, 0, 1, 1, 0, 0, 0, 0, 1]);
        assert!(arrays.windows(2).all(|w| w[0] < w[1]));
        let dates = rungs_of(
            "CREATE SCHEMA s; CREATE TABLE s.t (a date[] NOT NULL, PRIMARY KEY (a));",
            &[
                "{2017-12-31}",
                "{2018-01-01,1999-01-01}",
                "{2018-01-01,2000-01-01}",
            ],
        );
        assert_eq!(dates[1][..8], [1, 2018, 1, 1, 1, 1999, 1, 1]);
        assert!(dates.windows(2).all(|w| w[0] < w[1]));
    }
}
