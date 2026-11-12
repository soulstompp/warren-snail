//! The time types' text, as PostgreSQL 19 reads and writes it under the settings the snail answers
//! under: DateStyle ISO, MDY; IntervalStyle postgres; TimeZone UTC.
//!
//! A date is held as its days since 1970-01-01, a timestamp and a timestamptz as microseconds
//! since 1970-01-01 (UTC for a timestamptz), each with its infinities at the ends of its width; a
//! time as microseconds since midnight, 24:00:00 among them; a timetz as its time and its zone in
//! seconds west of UTC, as PostgreSQL stores it; an interval as its months, days and microseconds.
//! The reading of the text follows PostgreSQL's own: its fields, its keywords and its checks, in
//! the same order, so that a text PostgreSQL refuses is refused, and one it reads gives its value.

use crate::{Error, Result};
use std::cmp::Ordering;

mod funcs;
pub use funcs::*;

pub const MICROS_PER_DAY: i64 = 86_400_000_000;
/// The Julian day of 1970-01-01.
pub const UNIX_EPOCH_JDATE: i64 = 2_440_588;
/// 24:00:00, the last time of a day.
pub const TIME_END: i64 = 86_400_000_000;

/// The Julian day of 2000-01-01, where PostgreSQL counts its dates and instants from.
const POSTGRES_EPOCH_JDATE: i64 = 2_451_545;
/// Microseconds from 1970-01-01 to 2000-01-01.
const EPOCH_SHIFT: i64 = (POSTGRES_EPOCH_JDATE - UNIX_EPOCH_JDATE) * MICROS_PER_DAY;
const USECS_PER_HOUR: i64 = 3_600_000_000;
const USECS_PER_MINUTE: i64 = 60_000_000;
const USECS_PER_SEC: i64 = 1_000_000;
const SECS_PER_DAY: i64 = 86_400;
const MONTHS_PER_YEAR: i32 = 12;
const DAYS_PER_MONTH: i32 = 30;
const MAX_PRECISION: u32 = 6;
const MAX_TZDISP_HOUR: i32 = 15;
/// The first Julian day past a date, and past an instant.
const DATE_END_JULIAN: i64 = 2_147_483_494;
/// PostgreSQL's first instant and the first past its last, counted from 2000-01-01.
const MIN_TIMESTAMP: i64 = -211_813_488_000_000_000;
const END_TIMESTAMP: i64 = 9_223_371_331_200_000_000;

const MAXDATELEN: usize = 128;
const MAXDATEFIELDS: usize = 25;
const TOKMAXLEN: usize = 10;

/// An interval: its months, days and microseconds, kept apart as PostgreSQL keeps them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Interval {
    pub months: i32,
    pub days: i32,
    pub micros: i64,
}

impl Interval {
    /// Its own 16 bytes: its microseconds, then its days and its months, each big-endian.
    pub fn to_bytes(self) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[..8].copy_from_slice(&self.micros.to_be_bytes());
        b[8..12].copy_from_slice(&self.days.to_be_bytes());
        b[12..].copy_from_slice(&self.months.to_be_bytes());
        b
    }

    /// An interval from its own 16 bytes, as `to_bytes` writes them.
    pub fn from_bytes(b: &[u8]) -> Option<Interval> {
        let b: &[u8; 16] = b.try_into().ok()?;
        Some(Interval {
            micros: i64::from_be_bytes(b[..8].try_into().unwrap()),
            days: i32::from_be_bytes(b[8..12].try_into().unwrap()),
            months: i32::from_be_bytes(b[12..].try_into().unwrap()),
        })
    }

    /// `infinity`: every field at its greatest.
    pub const INFINITY: Interval = Interval {
        months: i32::MAX,
        days: i32::MAX,
        micros: i64::MAX,
    };
    /// `-infinity`: every field at its least.
    pub const NEG_INFINITY: Interval = Interval {
        months: i32::MIN,
        days: i32::MIN,
        micros: i64::MIN,
    };

    pub fn is_finite(self) -> bool {
        self != Interval::INFINITY && self != Interval::NEG_INFINITY
    }

    /// An interval of `total` as `total` gives it: the infinities as they are; any other as whole
    /// months of 30 days, then days, then microseconds, its fields each within their own range.
    pub fn of_total(total: i128) -> Interval {
        if total == Interval::INFINITY.total() {
            return Interval::INFINITY;
        }
        if total == Interval::NEG_INFINITY.total() {
            return Interval::NEG_INFINITY;
        }
        let day = i128::from(MICROS_PER_DAY);
        let months = (total / (30 * day)).clamp(i128::from(i32::MIN) + 1, i128::from(i32::MAX) - 1);
        let rest = total - months * 30 * day;
        let days = (rest / day).clamp(i128::from(i32::MIN) + 1, i128::from(i32::MAX) - 1);
        Interval {
            months: months as i32,
            days: days as i32,
            micros: (rest - days * day) as i64,
        }
    }

    /// The one number PostgreSQL compares intervals by: the microseconds, with a month counted as
    /// 30 days and a day as 24 hours.
    pub fn total(self) -> i128 {
        let days = i64::from(self.months) * 30 + i64::from(self.days);
        i128::from(self.micros) + i128::from(days) * i128::from(MICROS_PER_DAY)
    }
}

/// Intervals ordered by their totals, PostgreSQL's order, and intervals of one total by their
/// months, days and microseconds, so that two values with one total and other fields (`1 day` and
/// `24 hours`) are told apart as `Eq` tells them.
impl Ord for Interval {
    fn cmp(&self, other: &Self) -> Ordering {
        self.total()
            .cmp(&other.total())
            .then((self.months, self.days, self.micros).cmp(&(
                other.months,
                other.days,
                other.micros,
            )))
    }
}

impl PartialOrd for Interval {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// The Julian day of a date on the proleptic Gregorian calendar, its year as PostgreSQL counts it
/// (1 BC is 0).
pub fn date2j(year: i64, month: i64, day: i64) -> i64 {
    let (year, month) = if month > 2 {
        (year + 4800, month + 1)
    } else {
        (year + 4799, month + 13)
    };
    let century = year.div_euclid(100);
    let mut julian = year * 365 - 32167;
    julian += year.div_euclid(4) - century + century.div_euclid(4);
    julian += 7834 * month / 256 + day;
    julian
}

/// The date of a Julian day: its year as PostgreSQL counts it, its month and its day.
pub fn j2date(jd: i64) -> (i64, u32, u32) {
    let mut julian = jd + 32044;
    let mut quad = julian.div_euclid(146_097);
    let extra = (julian - quad * 146_097) * 4 + 3;
    julian += 60 + quad * 3 + extra.div_euclid(146_097);
    quad = julian.div_euclid(1461);
    julian -= quad * 1461;
    let mut y = julian * 4 / 1461;
    julian = if y != 0 {
        (julian + 305) % 365
    } else {
        (julian + 306) % 366
    } + 123;
    y += quad * 4;
    let quad = julian * 2141 / 65536;
    let day = julian - 7834 * quad / 256;
    let month = (quad + 10) % 12 + 1;
    (y - 4800, month as u32, day as u32)
}

/// An instant's year, month, day, hour, minute, second and microsecond on the UTC clock.
pub fn civil(micros: i64) -> (i64, u32, u32, u32, u32, u32, u32) {
    let days = micros.div_euclid(MICROS_PER_DAY);
    let t = micros.rem_euclid(MICROS_PER_DAY);
    let (y, m, d) = j2date(days + UNIX_EPOCH_JDATE);
    (
        y,
        m,
        d,
        (t / USECS_PER_HOUR) as u32,
        (t % USECS_PER_HOUR / USECS_PER_MINUTE) as u32,
        (t % USECS_PER_MINUTE / USECS_PER_SEC) as u32,
        (t % USECS_PER_SEC) as u32,
    )
}

// The C functions' int arithmetic, which PostgreSQL builds to wrap, where a parse reaches it
// before any range is checked.
fn date2j_c(year: i32, month: i32, day: i32) -> i32 {
    let (year, month) = if month > 2 {
        (year.wrapping_add(4800), month.wrapping_add(1))
    } else {
        (year.wrapping_add(4799), month.wrapping_add(13))
    };
    let century = year / 100;
    let mut julian = year.wrapping_mul(365).wrapping_sub(32167);
    julian = julian
        .wrapping_add(year / 4)
        .wrapping_sub(century)
        .wrapping_add(century / 4);
    julian.wrapping_add(7834i32.wrapping_mul(month) / 256 + day)
}

fn j2date_c(jd: i32) -> (i32, i32, i32) {
    let mut julian = jd as u32;
    julian = julian.wrapping_add(32044);
    let mut quad = julian / 146_097;
    let extra = (julian.wrapping_sub(quad.wrapping_mul(146_097)))
        .wrapping_mul(4)
        .wrapping_add(3);
    julian = julian
        .wrapping_add(60)
        .wrapping_add(quad.wrapping_mul(3))
        .wrapping_add(extra / 146_097);
    quad = julian / 1461;
    julian = julian.wrapping_sub(quad.wrapping_mul(1461));
    let mut y = (julian.wrapping_mul(4) / 1461) as i32;
    julian = if y != 0 {
        (julian.wrapping_add(305)) % 365
    } else {
        (julian.wrapping_add(306)) % 366
    }
    .wrapping_add(123);
    y = y.wrapping_add((quad.wrapping_mul(4)) as i32);
    let year = y.wrapping_sub(4800);
    let quad = julian.wrapping_mul(2141) / 65536;
    let day = julian.wrapping_sub(7834u32.wrapping_mul(quad) / 256) as i32;
    let month = ((quad.wrapping_add(10)) % 12 + 1) as i32;
    (year, month, day)
}

fn valid_julian(y: i32, m: i32) -> bool {
    (y > -4713 || (y == -4713 && m >= 11)) && (y < 5_874_898 || (y == 5_874_898 && m < 6))
}

fn is_leap(y: i32) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

const DAY_TAB: [[i32; 12]; 2] = [
    [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31],
    [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31],
];

/// A date and time taken apart, as PostgreSQL's `pg_tm` holds it.
#[derive(Clone, Copy, Debug, Default)]
struct Tm {
    year: i32,
    mon: i32,
    mday: i32,
    hour: i32,
    min: i32,
    sec: i32,
    yday: i32,
}

/// Why a text is not read, as PostgreSQL's DTERR codes say it, or a refusal of the snail's own.
#[derive(Debug)]
enum Dterr {
    BadFormat,
    FieldOverflow,
    MdFieldOverflow,
    TzdispOverflow,
    Refused(Error),
}

type D<T> = std::result::Result<T, Dterr>;

fn parse_error(e: Dterr, s: &str, datatype: &str) -> Error {
    match e {
        Dterr::FieldOverflow | Dterr::MdFieldOverflow => {
            Error::new(format!("date/time field value out of range: \"{s}\""))
        }
        Dterr::TzdispOverflow => {
            Error::new(format!("time zone displacement out of range: \"{s}\""))
        }
        Dterr::Refused(e) => e,
        Dterr::BadFormat => {
            Error::new(format!("invalid input syntax for type {datatype}: \"{s}\""))
        }
    }
}

// The kinds of field ParseDateTime cuts a text into.
const DTK_NUMBER: i32 = 0;
const DTK_STRING: i32 = 1;
const DTK_DATE: i32 = 2;
const DTK_TIME: i32 = 3;
const DTK_TZ: i32 = 4;
const DTK_SPECIAL: i32 = 6;

// What a value decodes to, besides a date and a time, and what a unit names.
const DTK_EARLY: i32 = 9;
const DTK_LATE: i32 = 10;
const DTK_EPOCH: i32 = 11;
const DTK_NOW: i32 = 12;
const DTK_YESTERDAY: i32 = 13;
const DTK_TODAY: i32 = 14;
const DTK_TOMORROW: i32 = 15;
const DTK_ZULU: i32 = 16;
const DTK_DELTA: i32 = 17;
const DTK_SECOND: i32 = 18;
const DTK_MINUTE: i32 = 19;
const DTK_HOUR: i32 = 20;
const DTK_DAY: i32 = 21;
const DTK_WEEK: i32 = 22;
const DTK_MONTH: i32 = 23;
const DTK_QUARTER: i32 = 24;
const DTK_YEAR: i32 = 25;
const DTK_DECADE: i32 = 26;
const DTK_CENTURY: i32 = 27;
const DTK_MILLENNIUM: i32 = 28;
const DTK_MILLISEC: i32 = 29;
const DTK_MICROSEC: i32 = 30;
const DTK_JULIAN: i32 = 31;
const DTK_DOW: i32 = 32;
const DTK_DOY: i32 = 33;
const DTK_TZ_HOUR: i32 = 34;
const DTK_TZ_MINUTE: i32 = 35;
const DTK_ISOYEAR: i32 = 36;
const DTK_ISODOW: i32 = 37;

// The kinds of keyword, and the fields a mask marks.
const RESERV: i32 = 0;
const MONTH: i32 = 1;
const YEAR: i32 = 2;
const DAY: i32 = 3;
const TZ: i32 = 5;
const DTZ: i32 = 6;
const IGNORE_DTF: i32 = 8;
const AMPM: i32 = 9;
const HOUR: i32 = 10;
const MINUTE: i32 = 11;
const SECOND: i32 = 12;
const MILLISECOND: i32 = 13;
const MICROSECOND: i32 = 14;
const DOY: i32 = 15;
const DOW: i32 = 16;
const UNITS: i32 = 17;
const ADBC: i32 = 18;
const AGO: i32 = 19;
const ISOTIME: i32 = 23;
const WEEK: i32 = 24;
const DECADE: i32 = 25;
const CENTURY: i32 = 26;
const MILLENNIUM: i32 = 27;
const DTZMOD: i32 = 28;
const UNKNOWN_FIELD: i32 = 31;

const AM: i32 = 0;
const PM: i32 = 1;
const HR24: i32 = 2;
const BC: i32 = 1;

fn m(t: i32) -> u32 {
    1u32 << t
}

const DATE_M: u32 = (1 << YEAR) | (1 << MONTH) | (1 << DAY);
const ALL_SECS_M: u32 = (1 << SECOND) | (1 << MILLISECOND) | (1 << MICROSECOND);
const TIME_M: u32 = (1 << HOUR) | (1 << MINUTE) | ALL_SECS_M;

/// The interval typmod's fields, each as a bit.
const fn interval_mask(b: i32) -> u16 {
    1 << b
}
pub const INTERVAL_FULL_RANGE: u16 = 0x7FFF;

/// The keywords of a date and a time, as PostgreSQL's `datetktbl` lists them.
const DATETKTBL: &[(&str, i32, i32)] = &[
    ("+infinity", RESERV, DTK_LATE),
    ("-infinity", RESERV, DTK_EARLY),
    ("ad", ADBC, 0),
    ("allballs", RESERV, DTK_ZULU),
    ("am", AMPM, AM),
    ("apr", MONTH, 4),
    ("april", MONTH, 4),
    ("at", IGNORE_DTF, 0),
    ("aug", MONTH, 8),
    ("august", MONTH, 8),
    ("bc", ADBC, BC),
    ("d", UNITS, DTK_DAY),
    ("dec", MONTH, 12),
    ("december", MONTH, 12),
    ("dow", UNITS, DTK_DOW),
    ("doy", UNITS, DTK_DOY),
    ("dst", DTZMOD, 3600),
    ("epoch", RESERV, DTK_EPOCH),
    ("feb", MONTH, 2),
    ("february", MONTH, 2),
    ("fri", DOW, 5),
    ("friday", DOW, 5),
    ("h", UNITS, DTK_HOUR),
    ("infinity", RESERV, DTK_LATE),
    ("isodow", UNITS, DTK_ISODOW),
    ("isoyear", UNITS, DTK_ISOYEAR),
    ("j", UNITS, DTK_JULIAN),
    ("jan", MONTH, 1),
    ("january", MONTH, 1),
    ("jd", UNITS, DTK_JULIAN),
    ("jul", MONTH, 7),
    ("julian", UNITS, DTK_JULIAN),
    ("july", MONTH, 7),
    ("jun", MONTH, 6),
    ("june", MONTH, 6),
    ("m", UNITS, DTK_MONTH),
    ("mar", MONTH, 3),
    ("march", MONTH, 3),
    ("may", MONTH, 5),
    ("mm", UNITS, DTK_MINUTE),
    ("mon", DOW, 1),
    ("monday", DOW, 1),
    ("nov", MONTH, 11),
    ("november", MONTH, 11),
    ("now", RESERV, DTK_NOW),
    ("oct", MONTH, 10),
    ("october", MONTH, 10),
    ("on", IGNORE_DTF, 0),
    ("pm", AMPM, PM),
    ("s", UNITS, DTK_SECOND),
    ("sat", DOW, 6),
    ("saturday", DOW, 6),
    ("sep", MONTH, 9),
    ("sept", MONTH, 9),
    ("september", MONTH, 9),
    ("sun", DOW, 0),
    ("sunday", DOW, 0),
    ("t", ISOTIME, DTK_TIME),
    ("thu", DOW, 4),
    ("thur", DOW, 4),
    ("thurs", DOW, 4),
    ("thursday", DOW, 4),
    ("today", RESERV, DTK_TODAY),
    ("tomorrow", RESERV, DTK_TOMORROW),
    ("tue", DOW, 2),
    ("tues", DOW, 2),
    ("tuesday", DOW, 2),
    ("wed", DOW, 3),
    ("wednesday", DOW, 3),
    ("weds", DOW, 3),
    ("y", UNITS, DTK_YEAR),
    ("yesterday", RESERV, DTK_YESTERDAY),
];

/// The units of an interval, as PostgreSQL's `deltatktbl` lists them.
const DELTATKTBL: &[(&str, i32, i32)] = &[
    ("@", IGNORE_DTF, 0),
    ("ago", AGO, 0),
    ("c", UNITS, DTK_CENTURY),
    ("cent", UNITS, DTK_CENTURY),
    ("centuries", UNITS, DTK_CENTURY),
    ("century", UNITS, DTK_CENTURY),
    ("d", UNITS, DTK_DAY),
    ("day", UNITS, DTK_DAY),
    ("days", UNITS, DTK_DAY),
    ("dec", UNITS, DTK_DECADE),
    ("decade", UNITS, DTK_DECADE),
    ("decades", UNITS, DTK_DECADE),
    ("decs", UNITS, DTK_DECADE),
    ("h", UNITS, DTK_HOUR),
    ("hour", UNITS, DTK_HOUR),
    ("hours", UNITS, DTK_HOUR),
    ("hr", UNITS, DTK_HOUR),
    ("hrs", UNITS, DTK_HOUR),
    ("m", UNITS, DTK_MINUTE),
    ("microsecon", UNITS, DTK_MICROSEC),
    ("mil", UNITS, DTK_MILLENNIUM),
    ("millennia", UNITS, DTK_MILLENNIUM),
    ("millennium", UNITS, DTK_MILLENNIUM),
    ("millisecon", UNITS, DTK_MILLISEC),
    ("mils", UNITS, DTK_MILLENNIUM),
    ("min", UNITS, DTK_MINUTE),
    ("mins", UNITS, DTK_MINUTE),
    ("minute", UNITS, DTK_MINUTE),
    ("minutes", UNITS, DTK_MINUTE),
    ("mon", UNITS, DTK_MONTH),
    ("mons", UNITS, DTK_MONTH),
    ("month", UNITS, DTK_MONTH),
    ("months", UNITS, DTK_MONTH),
    ("ms", UNITS, DTK_MILLISEC),
    ("msec", UNITS, DTK_MILLISEC),
    ("msecond", UNITS, DTK_MILLISEC),
    ("mseconds", UNITS, DTK_MILLISEC),
    ("msecs", UNITS, DTK_MILLISEC),
    ("qtr", UNITS, DTK_QUARTER),
    ("quarter", UNITS, DTK_QUARTER),
    ("s", UNITS, DTK_SECOND),
    ("sec", UNITS, DTK_SECOND),
    ("second", UNITS, DTK_SECOND),
    ("seconds", UNITS, DTK_SECOND),
    ("secs", UNITS, DTK_SECOND),
    ("timezone", UNITS, DTK_TZ),
    ("timezone_h", UNITS, DTK_TZ_HOUR),
    ("timezone_m", UNITS, DTK_TZ_MINUTE),
    ("us", UNITS, DTK_MICROSEC),
    ("usec", UNITS, DTK_MICROSEC),
    ("usecond", UNITS, DTK_MICROSEC),
    ("useconds", UNITS, DTK_MICROSEC),
    ("usecs", UNITS, DTK_MICROSEC),
    ("w", UNITS, DTK_WEEK),
    ("week", UNITS, DTK_WEEK),
    ("weeks", UNITS, DTK_WEEK),
    ("y", UNITS, DTK_YEAR),
    ("year", UNITS, DTK_YEAR),
    ("years", UNITS, DTK_YEAR),
    ("yr", UNITS, DTK_YEAR),
    ("yrs", UNITS, DTK_YEAR),
];

/// What a zone abbreviation names: an offset east of UTC, in seconds, and whether it is daylight
/// time; or a zone, whose rules give the offset it means at the time it is read at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Abbreviated {
    Fixed(i32, bool),
    Zone(String),
}

/// PostgreSQL's `Default` set of zone abbreviations (`src/timezone/tznames/Default`, 18.6's and
/// 19's alike), in its table's order, by each one's lowered name: each lowered, as written, and
/// what it names.
fn abbreviation_table() -> &'static [(String, &'static str, Abbreviated)] {
    static ALL: std::sync::OnceLock<Vec<(String, &'static str, Abbreviated)>> =
        std::sync::OnceLock::new();
    ALL.get_or_init(|| {
        include_str!("abbrevs.txt")
            .lines()
            .filter_map(|l| {
                let mut f = l.split('\t');
                let (name, value, kind) = (f.next()?, f.next()?, f.next().unwrap_or(""));
                Some((
                    name.to_ascii_lowercase(),
                    name,
                    match value.parse::<i32>() {
                        Ok(east) => Abbreviated::Fixed(east, kind == "D"),
                        Err(_) => Abbreviated::Zone(value.to_string()),
                    },
                ))
            })
            .collect()
    })
}

/// The abbreviations of PostgreSQL's `Default` set in its table's order: each as written, and what
/// it names.
pub fn abbreviations() -> impl Iterator<Item = (&'static str, &'static Abbreviated)> {
    abbreviation_table().iter().map(|(_, w, a)| (*w, a))
}

/// A dynamic abbreviation's kind of field, beside TZ and DTZ.
const DYNTZ: i32 = 7;

/// The abbreviation a lowered word names in the set, compared on the first `TOKMAXLEN` bytes.
fn abbreviation(key: &[u8]) -> Option<&'static Abbreviated> {
    abbreviation_table()
        .iter()
        .find(|(t, _, _)| token_eq(key, t))
        .map(|(_, _, a)| a)
}

/// Whether a field names a token, compared as PostgreSQL compares them: on their first
/// `TOKMAXLEN` bytes.
fn token_eq(key: &[u8], token: &str) -> bool {
    let t = token.as_bytes();
    if key.len() >= TOKMAXLEN {
        t.len() == TOKMAXLEN && key[..TOKMAXLEN] == *t
    } else {
        key == t
    }
}

fn lookup(key: &[u8], table: &[(&str, i32, i32)]) -> Option<(i32, i32)> {
    table
        .iter()
        .find(|(t, _, _)| token_eq(key, t))
        .map(|&(_, ty, v)| (ty, v))
}

fn decode_special(key: &[u8]) -> (i32, i32) {
    lookup(key, DATETKTBL).unwrap_or((UNKNOWN_FIELD, 0))
}

fn decode_units(key: &[u8]) -> (i32, i32) {
    lookup(key, DELTATKTBL).unwrap_or((UNKNOWN_FIELD, 0))
}

/// A zone abbreviation, as PostgreSQL's DecodeTimezoneAbbrev reads a lowered word: from 18 on the
/// statement zone's own first, then the set's. Its kind (TZ, DTZ or DYNTZ), its offset east of UTC
/// where it has one, and the zone a dynamic one names, read where it is met; none for a word that is
/// no abbreviation.
fn decode_timezone_abbrev(key: &[u8]) -> D<Option<(i32, i32, Option<String>)>> {
    let kind = |dst: bool| if dst { DTZ } else { TZ };
    if crate::zones::zone_abbreviations_first() {
        let upper = String::from_utf8_lossy(key).to_ascii_uppercase();
        match crate::zones::session_abbrev_known(&upper).map_err(Dterr::Refused)? {
            Some(Some((east, dst))) => return Ok(Some((kind(dst), east, None))),
            Some(None) => return Ok(Some((DYNTZ, 0, crate::zones::session_named()))),
            None => {}
        }
    }
    Ok(match abbreviation(key) {
        Some(Abbreviated::Fixed(east, dst)) => Some((kind(*dst), *east, None)),
        Some(Abbreviated::Zone(zone)) => {
            if let Err(e) = crate::zones::utc_offset(zone, 0) {
                #[cfg(feature = "zones")]
                let e = {
                    let _ = e;
                    Error::coded(
                        "22023",
                        format!(
                            "time zone \"{zone}\" not recognized\nDETAIL:  This time zone name \
                             appears in the configuration file for time zone abbreviation \"{}\".",
                            String::from_utf8_lossy(key)
                        ),
                    )
                };
                return Err(Dterr::Refused(e));
            }
            Some((DYNTZ, 0, Some(zone.clone())))
        }
        None => None,
    })
}

/// PostgreSQL's DetermineTimeZoneAbbrevOffset: the offset west of UTC that `abbrev`, an
/// abbreviation of zone `zone`, means at the date and time `tm` on that zone's clock; the zone's
/// own offset there where the zone gives it no meaning, and UTC's outside the Julian days.
fn abbrev_offset(zone: &str, abbrev: &str, tm: &Tm) -> D<i32> {
    let local = match valid_julian(tm.year, tm.mon) {
        true => {
            let day = i64::from(date2j_c(tm.year, tm.mon, tm.mday)) - UNIX_EPOCH_JDATE;
            Some(day * SECS_PER_DAY + i64::from(tm.sec + (tm.min + tm.hour * 60) * 60))
        }
        false => None,
    };
    let east = match local {
        Some(l) => crate::zones::local_offset(zone, l).map_err(Dterr::Refused)?,
        None => 0,
    };
    let probe = local.map_or(0, |l| l - i64::from(east));
    let meant = crate::zones::abbrev_meaning(zone, &abbrev.to_ascii_uppercase(), probe)
        .map_err(Dterr::Refused)?;
    Ok(-meant.map_or(east, |(e, _)| e))
}

/// The offset east of UTC that an abbreviation of zone `zone` means at a time `local` seconds after
/// 1970-01-01 00:00 on that zone's clock, as AT TIME ZONE reads a timestamp by it.
pub fn abbrev_local_offset(zone: &str, abbrev: &str, local: i64) -> Result<i32> {
    let east = crate::zones::local_offset(zone, local)?;
    let meant =
        crate::zones::abbrev_meaning(zone, &abbrev.to_ascii_uppercase(), local - i64::from(east))?;
    Ok(meant.map_or(east, |(e, _)| e))
}

/// The offset east of UTC, and whether it is daylight time, that an abbreviation of zone `zone`
/// means at the instant `utc` seconds after 1970-01-01 00:00 UTC (PostgreSQL's
/// DetermineTimeZoneAbbrevOffsetTS): the zone's own at that instant where it gives the abbreviation
/// no meaning.
pub fn abbrev_utc_offset(zone: &str, abbrev: &str, utc: i64) -> Result<(i32, bool)> {
    match crate::zones::abbrev_meaning(zone, &abbrev.to_ascii_uppercase(), utc)? {
        Some(m) => Ok(m),
        None => {
            let local = utc + i64::from(crate::zones::utc_offset(zone, utc)?);
            let east = crate::zones::local_offset(zone, local)?;
            Ok((east, crate::zones::dst_at(zone, local - i64::from(east))?))
        }
    }
}

/// What a lowered word names as an abbreviation where to_timestamp's TZ reads one, as PostgreSQL's
/// DecodeTimezoneAbbrevPrefix tries each length: from 18 on the statement zone's own first, then
/// the set's, a dynamic one whose zone cannot be read none.
pub fn abbreviation_prefix(key: &[u8]) -> Result<Option<Abbreviated>> {
    if crate::zones::zone_abbreviations_first() {
        let upper = String::from_utf8_lossy(key).to_ascii_uppercase();
        match crate::zones::session_abbrev_known(&upper)? {
            Some(Some((east, dst))) => return Ok(Some(Abbreviated::Fixed(east, dst))),
            Some(None) => return Ok(crate::zones::session_named().map(Abbreviated::Zone)),
            None => {}
        }
    }
    Ok(match abbreviation(key) {
        Some(Abbreviated::Zone(zone)) => match crate::zones::utc_offset(zone, 0) {
            Ok(_) => Some(Abbreviated::Zone(zone.clone())),
            // A zone the directory lacks is passed over; one a build cannot read refused by name.
            Err(e) if cfg!(not(feature = "zones")) => return Err(e),
            Err(_) => None,
        },
        other => other.cloned(),
    })
}

/// The offset west of UTC that `abbrev`, an abbreviation of zone `zone`, means at a date and a time
/// of day on that zone's clock, as to_timestamp reads it.
pub fn abbrev_west(
    zone: &str,
    abbrev: &str,
    (year, mon, mday): (i32, i32, i32),
    (hour, min, sec): (i32, i32, i32),
) -> Result<i32> {
    let tm = Tm {
        year,
        mon,
        mday,
        hour,
        min,
        sec,
        ..Tm::default()
    };
    abbrev_offset(zone, abbrev, &tm).map_err(|e| parse_error(e, abbrev, "timestamp"))
}

/// A zone named in AT TIME ZONE or timezone() as an abbreviation, as PostgreSQL's
/// DecodeTimezoneName reads it first, its name lowered and cut to 63 bytes: what it names; none for
/// a name that is no abbreviation, which is then a zone's.
pub fn zone_abbreviation(name: &str) -> Result<Option<Abbreviated>> {
    let mut key: Vec<u8> = name.bytes().map(|b| b.to_ascii_lowercase()).collect();
    key.truncate(63);
    match decode_timezone_abbrev(&key) {
        Ok(Some((DYNTZ, _, Some(zone)))) => Ok(Some(Abbreviated::Zone(zone))),
        Ok(Some((kind, east, _))) => Ok(Some(Abbreviated::Fixed(east, kind == DTZ))),
        Ok(None) => Ok(None),
        Err(Dterr::Refused(e)) => Err(e),
        Err(_) => Ok(None),
    }
}

fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// C's `strtol` in base 10 from `at`: the number, where it stops (`at` itself where it read no
/// digit), and whether it overflowed.
fn strtol(s: &[u8], at: usize) -> (i64, usize, bool) {
    let mut i = at;
    while i < s.len() && is_space(s[i]) {
        i += 1;
    }
    let mut neg = false;
    if i < s.len() && (s[i] == b'+' || s[i] == b'-') {
        neg = s[i] == b'-';
        i += 1;
    }
    let start = i;
    let mut v: i128 = 0;
    let mut over = false;
    while i < s.len() && s[i].is_ascii_digit() {
        if !over {
            v = v * 10 + i128::from(s[i] - b'0');
            if v > i128::from(i64::MAX) + 1 {
                over = true;
            }
        }
        i += 1;
    }
    if i == start {
        return (0, at, false);
    }
    let v = if neg { -v } else { v };
    if over || v > i128::from(i64::MAX) || v < i128::from(i64::MIN) {
        return (if neg { i64::MIN } else { i64::MAX }, i, true);
    }
    (v as i64, i, false)
}

/// PostgreSQL's `strtoint`: `strtol` held to 32 bits, past which it overflows.
fn strtoint(s: &[u8], at: usize) -> (i32, usize, bool) {
    let (v, end, over) = strtol(s, at);
    (v as i32, end, over || i64::from(v as i32) != v)
}

/// C's `atoi`, as glibc gives it: `strtol` cut to 32 bits.
fn atoi(s: &[u8]) -> i32 {
    strtol(s, 0).0 as i32
}

/// The longest start of `s` from `at` C's `strtod` reads, and its value, and whether it went out
/// of a double's range; none where it reads nothing.
fn strtod(s: &[u8], at: usize) -> Option<(f64, usize, bool)> {
    let mut i = at;
    while i < s.len() && is_space(s[i]) {
        i += 1;
    }
    let sign_at = i;
    let mut neg = false;
    if i < s.len() && (s[i] == b'+' || s[i] == b'-') {
        neg = s[i] == b'-';
        i += 1;
    }
    let rest = &s[i..];
    let lower: Vec<u8> = rest
        .iter()
        .take(8)
        .map(|b| b.to_ascii_lowercase())
        .collect();
    let signed = |v: f64| if neg { -v } else { v };
    if lower.starts_with(b"inf") {
        let n = if lower.starts_with(b"infinity") { 8 } else { 3 };
        return Some((signed(f64::INFINITY), i + n, false));
    }
    if lower.starts_with(b"nan") {
        let mut end = i + 3;
        if end < s.len() && s[end] == b'(' {
            let mut j = end + 1;
            while j < s.len() && (s[j].is_ascii_alphanumeric() || s[j] == b'_') {
                j += 1;
            }
            if j < s.len() && s[j] == b')' {
                end = j + 1;
            }
        }
        return Some((signed(f64::NAN), end, false));
    }
    if rest.len() >= 2 && rest[0] == b'0' && (rest[1] == b'x' || rest[1] == b'X') {
        let mut j = i + 2;
        let mut mantissa: u128 = 0;
        let mut exp: i64 = 0;
        let mut digits = 0;
        let mut nonzero = false;
        let mut seen_point = false;
        while j < s.len() {
            let c = s[j];
            if c == b'.' && !seen_point {
                seen_point = true;
            } else if let Some(d) = (c as char).to_digit(16) {
                digits += 1;
                nonzero |= d != 0;
                if mantissa < (1u128 << 120) {
                    mantissa = mantissa * 16 + u128::from(d);
                    if seen_point {
                        exp -= 4;
                    }
                } else if !seen_point {
                    exp += 4;
                }
            } else {
                break;
            }
            j += 1;
        }
        if digits == 0 {
            // "0x" alone reads as the 0 before the x.
            return Some((signed(0.0), i + 1, false));
        }
        if j < s.len() && (s[j] == b'p' || s[j] == b'P') {
            let (e, end, _) = strtol(s, j + 1);
            if end > j + 1 && !is_space(s[j + 1]) {
                exp = exp.saturating_add(e);
                j = end;
            }
        }
        let v = (mantissa as f64) * 2f64.powi(exp.clamp(-2000, 2000) as i32);
        let range = v.is_infinite() || (nonzero && (v == 0.0 || v.is_subnormal()));
        return Some((signed(v), j, range));
    }
    let mut j = i;
    let mut digits = 0;
    let mut nonzero = false;
    while j < s.len() && s[j].is_ascii_digit() {
        nonzero |= s[j] != b'0';
        digits += 1;
        j += 1;
    }
    if j < s.len() && s[j] == b'.' {
        j += 1;
        while j < s.len() && s[j].is_ascii_digit() {
            nonzero |= s[j] != b'0';
            digits += 1;
            j += 1;
        }
    }
    if digits == 0 {
        return None;
    }
    if j < s.len() && (s[j] == b'e' || s[j] == b'E') {
        let mut k = j + 1;
        if k < s.len() && (s[k] == b'+' || s[k] == b'-') {
            k += 1;
        }
        if k < s.len() && s[k].is_ascii_digit() {
            while k < s.len() && s[k].is_ascii_digit() {
                k += 1;
            }
            j = k;
        }
    }
    let text = std::str::from_utf8(&s[sign_at..j]).ok()?;
    let v: f64 = text.parse().ok()?;
    let range = v.is_infinite() || (nonzero && (v == 0.0 || v.is_subnormal()));
    Some((v, j, range))
}

/// PostgreSQL's ParseFraction: `.` and digits to the end, as a fraction below 1.
fn parse_fraction(s: &[u8]) -> D<f64> {
    if s.len() == 1 {
        return Ok(0.0);
    }
    if !s[1..].iter().all(u8::is_ascii_digit) {
        return Err(Dterr::BadFormat);
    }
    match strtod(s, 0) {
        Some((v, end, false)) if end == s.len() => Ok(v),
        _ => Err(Dterr::BadFormat),
    }
}

fn parse_fractional_second(s: &[u8]) -> D<i32> {
    Ok((parse_fraction(s)? * 1_000_000.0).round_ties_even() as i32)
}

/// One field of a text ParseDateTime has cut: its bytes, lower-cased where it holds letters, and
/// its kind.
struct Field {
    text: Vec<u8>,
    ftype: i32,
}

/// The room PostgreSQL's ParseDateTime writes its fields into, each with an end.
struct Room {
    used: usize,
    len: usize,
}

impl Room {
    fn push(&mut self, f: &mut Vec<u8>, b: u8) -> D<()> {
        if self.used + 1 >= self.len {
            return Err(Dterr::BadFormat);
        }
        self.used += 1;
        f.push(b);
        Ok(())
    }
}

/// PostgreSQL's ParseDateTime: a text cut into fields, in a room of `len` bytes.
fn parse_date_time(s: &[u8], len: usize) -> D<Vec<Field>> {
    let mut fields: Vec<Field> = Vec::new();
    let mut room = Room { used: 0, len };
    let at = |i: usize| s.get(i).copied().unwrap_or(0);
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        if is_space(c) {
            i += 1;
            continue;
        }
        if fields.len() >= MAXDATEFIELDS {
            return Err(Dterr::BadFormat);
        }
        let mut f = Vec::new();
        let ftype;
        if c.is_ascii_digit() {
            room.push(&mut f, c)?;
            i += 1;
            while at(i).is_ascii_digit() {
                room.push(&mut f, at(i))?;
                i += 1;
            }
            if at(i) == b':' {
                ftype = DTK_TIME;
                room.push(&mut f, b':')?;
                i += 1;
                while at(i).is_ascii_digit() || at(i) == b':' || at(i) == b'.' {
                    room.push(&mut f, at(i))?;
                    i += 1;
                }
            } else if matches!(at(i), b'-' | b'/' | b'.') {
                let delim = at(i);
                room.push(&mut f, delim)?;
                i += 1;
                if at(i).is_ascii_digit() {
                    let mut t = if delim == b'.' { DTK_NUMBER } else { DTK_DATE };
                    while at(i).is_ascii_digit() {
                        room.push(&mut f, at(i))?;
                        i += 1;
                    }
                    if at(i) == delim {
                        t = DTK_DATE;
                        room.push(&mut f, delim)?;
                        i += 1;
                        while at(i).is_ascii_digit() || at(i) == delim {
                            room.push(&mut f, at(i))?;
                            i += 1;
                        }
                    }
                    ftype = t;
                } else {
                    ftype = DTK_DATE;
                    while at(i).is_ascii_alphanumeric() || at(i) == delim {
                        room.push(&mut f, at(i).to_ascii_lowercase())?;
                        i += 1;
                    }
                }
            } else {
                ftype = DTK_NUMBER;
            }
        } else if c == b'.' {
            room.push(&mut f, c)?;
            i += 1;
            while at(i).is_ascii_digit() {
                room.push(&mut f, at(i))?;
                i += 1;
            }
            ftype = DTK_NUMBER;
        } else if c.is_ascii_alphabetic() {
            let mut t = DTK_STRING;
            room.push(&mut f, c.to_ascii_lowercase())?;
            i += 1;
            while at(i).is_ascii_alphabetic() {
                room.push(&mut f, at(i).to_ascii_lowercase())?;
                i += 1;
            }
            let is_date = if matches!(at(i), b'-' | b'/' | b'.') {
                true
            } else if at(i) == b'+' || at(i).is_ascii_digit() {
                lookup(&f, DATETKTBL).is_none()
            } else {
                false
            };
            if is_date {
                t = DTK_DATE;
                loop {
                    room.push(&mut f, at(i).to_ascii_lowercase())?;
                    i += 1;
                    let n = at(i);
                    if !(matches!(n, b'+' | b'-' | b'/' | b'_' | b'.' | b':')
                        || n.is_ascii_alphanumeric())
                    {
                        break;
                    }
                }
            }
            ftype = t;
        } else if c == b'+' || c == b'-' {
            room.push(&mut f, c)?;
            i += 1;
            while is_space(at(i)) {
                i += 1;
            }
            if at(i).is_ascii_digit() {
                ftype = DTK_TZ;
                room.push(&mut f, at(i))?;
                i += 1;
                while at(i).is_ascii_digit() || matches!(at(i), b':' | b'.' | b'-') {
                    room.push(&mut f, at(i))?;
                    i += 1;
                }
            } else if at(i).is_ascii_alphabetic() {
                ftype = DTK_SPECIAL;
                room.push(&mut f, at(i).to_ascii_lowercase())?;
                i += 1;
                while at(i).is_ascii_alphabetic() {
                    room.push(&mut f, at(i).to_ascii_lowercase())?;
                    i += 1;
                }
            } else {
                return Err(Dterr::BadFormat);
            }
        } else if c.is_ascii_punctuation() {
            i += 1;
            continue;
        } else {
            return Err(Dterr::BadFormat);
        }
        room.used += 1;
        fields.push(Field { text: f, ftype });
    }
    Ok(fields)
}

/// The statement's clock on the UTC clock, taken apart, and its microseconds in the second.
fn now_tm(now: i64) -> (Tm, i32) {
    let (y, mo, d, h, mi, s, us) = civil(now);
    (
        Tm {
            year: y as i32,
            mon: mo as i32,
            mday: d as i32,
            hour: h as i32,
            min: mi as i32,
            sec: s as i32,
            yday: 0,
        },
        us as i32,
    )
}

fn time_overflows(hour: i32, min: i32, sec: i32, fsec: i32) -> bool {
    if !(0..=24).contains(&hour) || !(0..60).contains(&min) || !(0..=60).contains(&sec) {
        return true;
    }
    if !(0..=1_000_000).contains(&fsec) {
        return true;
    }
    i64::from(((hour * 60 + min) * 60) + sec) * USECS_PER_SEC + i64::from(fsec) > MICROS_PER_DAY
}

/// What DecodeTimeCommon reads: an hour that may pass 24, its minute, second and microseconds.
struct Itm {
    hour: i64,
    min: i32,
    sec: i32,
    usec: i32,
}

/// PostgreSQL's DecodeTimeCommon.
fn decode_time_common(s: &[u8], range: u16) -> D<(u32, Itm)> {
    let mut itm = Itm {
        hour: 0,
        min: 0,
        sec: 0,
        usec: 0,
    };
    let mut fsec = 0;
    let (h, cp, over) = strtol(s, 0);
    if over {
        return Err(Dterr::FieldOverflow);
    }
    itm.hour = h;
    if s.get(cp) != Some(&b':') {
        return Err(Dterr::BadFormat);
    }
    let (mi, mut cp, over) = strtoint(s, cp + 1);
    if over {
        return Err(Dterr::FieldOverflow);
    }
    itm.min = mi;
    let to_minutes = |itm: &mut Itm| -> D<()> {
        if itm.hour > i64::from(i32::MAX) || itm.hour < i64::from(i32::MIN) {
            return Err(Dterr::FieldOverflow);
        }
        itm.sec = itm.min;
        itm.min = itm.hour as i32;
        itm.hour = 0;
        Ok(())
    };
    match s.get(cp) {
        None => {
            itm.sec = 0;
            if range == interval_mask(MINUTE) | interval_mask(SECOND) {
                to_minutes(&mut itm)?;
            }
        }
        Some(b'.') => {
            fsec = parse_fractional_second(&s[cp..])?;
            to_minutes(&mut itm)?;
        }
        Some(b':') => {
            let (sec, end, over) = strtoint(s, cp + 1);
            if over {
                return Err(Dterr::FieldOverflow);
            }
            itm.sec = sec;
            cp = end;
            match s.get(cp) {
                Some(b'.') => fsec = parse_fractional_second(&s[cp..])?,
                None => {}
                Some(_) => return Err(Dterr::BadFormat),
            }
        }
        Some(_) => return Err(Dterr::BadFormat),
    }
    if itm.hour < 0
        || !(0..=59).contains(&itm.min)
        || !(0..=60).contains(&itm.sec)
        || !(0..=1_000_000).contains(&fsec)
    {
        return Err(Dterr::FieldOverflow);
    }
    itm.usec = fsec;
    Ok((TIME_M, itm))
}

/// PostgreSQL's DecodeTime: a time of a date or of a day.
fn decode_time(s: &[u8], range: u16, tm: &mut Tm) -> D<(u32, i32)> {
    let (tmask, itm) = decode_time_common(s, range)?;
    if itm.hour > i64::from(i32::MAX) {
        return Err(Dterr::FieldOverflow);
    }
    tm.hour = itm.hour as i32;
    tm.min = itm.min;
    tm.sec = itm.sec;
    Ok((tmask, itm.usec))
}

/// PostgreSQL's DecodeNumberField: digits run together, a date or a time. Gives the kind it read
/// and the fields it set.
fn decode_number_field(
    s: &[u8],
    fmask: u32,
    tm: &mut Tm,
    fsec: &mut i32,
    is2digits: &mut bool,
) -> D<(i32, u32)> {
    if !s.iter().all(|&b| b.is_ascii_digit() || b == b'.') {
        return Err(Dterr::BadFormat);
    }
    let mut s = s;
    if let Some(p) = s.iter().position(|&b| b == b'.') {
        *fsec = parse_fractional_second(&s[p..])?;
        s = &s[..p];
    } else if fmask & DATE_M != DATE_M && s.len() >= 6 {
        let len = s.len();
        tm.mday = atoi(&s[len - 2..]);
        tm.mon = atoi(&s[len - 4..len - 2]);
        tm.year = atoi(&s[..len - 4]);
        if len - 4 == 2 {
            *is2digits = true;
        }
        return Ok((DTK_DATE, DATE_M));
    }
    if fmask & TIME_M != TIME_M {
        if s.len() == 6 {
            tm.sec = atoi(&s[4..]);
            tm.min = atoi(&s[2..4]);
            tm.hour = atoi(&s[..2]);
            return Ok((DTK_TIME, TIME_M));
        } else if s.len() == 4 {
            tm.sec = 0;
            tm.min = atoi(&s[2..]);
            tm.hour = atoi(&s[..2]);
            return Ok((DTK_TIME, TIME_M));
        }
    }
    Err(Dterr::BadFormat)
}

/// PostgreSQL's DecodeNumber: one number of a date, placed by what has been read before it and
/// by DateStyle's MDY. Gives the fields it set.
fn decode_number(
    s: &[u8],
    have_text_month: bool,
    fmask: u32,
    tm: &mut Tm,
    fsec: &mut i32,
    is2digits: &mut bool,
) -> D<u32> {
    let flen = s.len();
    let (val, cp, over) = strtoint(s, 0);
    if over {
        return Err(Dterr::FieldOverflow);
    }
    if cp == 0 {
        return Err(Dterr::BadFormat);
    }
    match s.get(cp) {
        Some(b'.') => {
            if cp > 2 {
                let (_, tmask) = decode_number_field(s, fmask | DATE_M, tm, fsec, is2digits)?;
                return Ok(tmask);
            }
            *fsec = parse_fractional_second(&s[cp..])?;
        }
        None => {}
        Some(_) => return Err(Dterr::BadFormat),
    }
    if flen == 3 && fmask & DATE_M == m(YEAR) && (1..=366).contains(&val) {
        tm.yday = val;
        return Ok(m(DOY) | m(MONTH) | m(DAY));
    }
    let tmask;
    match fmask & DATE_M {
        0 => {
            if flen >= 3 {
                tmask = m(YEAR);
                tm.year = val;
            } else {
                tmask = m(MONTH);
                tm.mon = val;
            }
        }
        x if x == m(YEAR) => {
            tmask = m(MONTH);
            tm.mon = val;
        }
        x if x == m(MONTH) => {
            if have_text_month && flen >= 3 {
                tmask = m(YEAR);
                tm.year = val;
            } else {
                tmask = m(DAY);
                tm.mday = val;
            }
        }
        x if x == m(YEAR) | m(MONTH) => {
            if have_text_month && flen >= 3 && *is2digits {
                tm.mday = tm.year;
                tm.year = val;
                *is2digits = false;
            } else {
                tm.mday = val;
            }
            tmask = m(DAY);
        }
        x if x == m(DAY) => {
            tmask = m(MONTH);
            tm.mon = val;
        }
        x if x == m(MONTH) | m(DAY) => {
            tmask = m(YEAR);
            tm.year = val;
        }
        DATE_M => {
            let (_, tmask) = decode_number_field(s, fmask, tm, fsec, is2digits)?;
            return Ok(tmask);
        }
        _ => return Err(Dterr::BadFormat),
    }
    if tmask == m(YEAR) {
        *is2digits = flen <= 2;
    }
    Ok(tmask)
}

/// PostgreSQL's DecodeDate: a date written with its delimiters. Gives the fields it set.
fn decode_date(s: &[u8], mut fmask: u32, is2digits: &mut bool, tm: &mut Tm) -> D<u32> {
    let mut tmask = 0;
    let mut parts: Vec<&[u8]> = Vec::new();
    let mut i = 0;
    while i < s.len() && parts.len() < MAXDATEFIELDS {
        while i < s.len() && !s[i].is_ascii_alphanumeric() {
            i += 1;
        }
        if i >= s.len() {
            return Err(Dterr::BadFormat);
        }
        let start = i;
        if s[i].is_ascii_digit() {
            while i < s.len() && s[i].is_ascii_digit() {
                i += 1;
            }
        } else {
            while i < s.len() && s[i].is_ascii_alphabetic() {
                i += 1;
            }
        }
        parts.push(&s[start..i]);
        if i < s.len() {
            i += 1;
        }
    }
    let mut have_text_month = false;
    let mut done = vec![false; parts.len()];
    for (k, part) in parts.iter().enumerate() {
        if part[0].is_ascii_alphabetic() {
            let (ty, val) = decode_special(part);
            if ty == IGNORE_DTF {
                continue;
            }
            let dmask = m(ty);
            if ty == MONTH {
                tm.mon = val;
                have_text_month = true;
            } else {
                return Err(Dterr::BadFormat);
            }
            if fmask & dmask != 0 {
                return Err(Dterr::BadFormat);
            }
            fmask |= dmask;
            tmask |= dmask;
            done[k] = true;
        }
    }
    for (k, part) in parts.iter().enumerate() {
        if done[k] {
            continue;
        }
        if part.is_empty() {
            return Err(Dterr::BadFormat);
        }
        let mut fsec = 0;
        let dmask = decode_number(part, have_text_month, fmask, tm, &mut fsec, is2digits)?;
        if fmask & dmask != 0 {
            return Err(Dterr::BadFormat);
        }
        fmask |= dmask;
        tmask |= dmask;
    }
    if fmask & !(m(DOY) | m(TZ)) != DATE_M {
        return Err(Dterr::BadFormat);
    }
    Ok(tmask)
}

/// PostgreSQL's ValidateDate: the year made from its era, a day of the year placed, and the month
/// and day checked.
fn validate_date(fmask: u32, isjulian: bool, is2digits: bool, bc: bool, tm: &mut Tm) -> D<()> {
    if fmask & m(YEAR) != 0 {
        if isjulian {
        } else if bc {
            if tm.year <= 0 {
                return Err(Dterr::FieldOverflow);
            }
            tm.year = -(tm.year - 1);
        } else if is2digits {
            if tm.year < 0 {
                return Err(Dterr::FieldOverflow);
            }
            if tm.year < 70 {
                tm.year += 2000;
            } else if tm.year < 100 {
                tm.year += 1900;
            }
        } else if tm.year <= 0 {
            return Err(Dterr::FieldOverflow);
        }
    }
    if fmask & m(DOY) != 0 {
        let jd = date2j_c(tm.year, 1, 1)
            .wrapping_add(tm.yday)
            .wrapping_sub(1);
        let (y, mo, d) = j2date_c(jd);
        tm.year = y;
        tm.mon = mo;
        tm.mday = d;
    }
    if fmask & m(MONTH) != 0 && !(1..=12).contains(&tm.mon) {
        return Err(Dterr::MdFieldOverflow);
    }
    if fmask & m(DAY) != 0 && !(1..=31).contains(&tm.mday) {
        return Err(Dterr::MdFieldOverflow);
    }
    if fmask & DATE_M == DATE_M
        && tm.mday > DAY_TAB[is_leap(tm.year) as usize][(tm.mon - 1) as usize]
    {
        return Err(Dterr::FieldOverflow);
    }
    Ok(())
}

/// PostgreSQL's DecodeTimezone: `+hh`, `+hh:mm`, `+hh:mm:ss` or `+hhmm`, as seconds west of UTC.
fn decode_timezone(s: &[u8]) -> D<i32> {
    if s.first() != Some(&b'+') && s.first() != Some(&b'-') {
        return Err(Dterr::BadFormat);
    }
    let (mut hr, mut cp, over) = strtoint(s, 1);
    if over {
        return Err(Dterr::TzdispOverflow);
    }
    let min;
    let mut sec = 0;
    if s.get(cp) == Some(&b':') {
        let (v, end, over) = strtoint(s, cp + 1);
        if over {
            return Err(Dterr::TzdispOverflow);
        }
        min = v;
        cp = end;
        if s.get(cp) == Some(&b':') {
            let (v, end, over) = strtoint(s, cp + 1);
            if over {
                return Err(Dterr::TzdispOverflow);
            }
            sec = v;
            cp = end;
        }
    } else if cp >= s.len() && s.len() > 3 {
        min = hr % 100;
        hr /= 100;
    } else {
        min = 0;
    }
    if !(0..=MAX_TZDISP_HOUR).contains(&hr) || !(0..60).contains(&min) || !(0..60).contains(&sec) {
        return Err(Dterr::TzdispOverflow);
    }
    let mut tz = (hr * 60 + min) * 60 + sec;
    if s[0] == b'-' {
        tz = -tz;
    }
    if cp < s.len() {
        return Err(Dterr::BadFormat);
    }
    Ok(-tz)
}

/// A zone named in a text. A name no zone has is a bad format, as PostgreSQL's pg_tzset finding
/// none makes it. A build without the feature `zones` cannot tell, so it reads the zone only where
/// its offset is taken, after the rest of the text: a text bad whatever the name is refused as bad,
/// and one that needs the zone refused by name.
fn named_zone(name: &[u8]) -> D<String> {
    let name = String::from_utf8_lossy(name).into_owned();
    #[cfg(feature = "zones")]
    if crate::zones::utc_offset(&name, 0).is_err() {
        return Err(Dterr::BadFormat);
    }
    Ok(name)
}

/// PostgreSQL's DetermineTimeZoneOffset for a named zone: the offset west of UTC the zone gives
/// the date and time `tm` on its own clock; none outside the Julian days.
fn zone_offset(name: &str, tm: &Tm) -> D<i32> {
    if !valid_julian(tm.year, tm.mon) {
        return Ok(0);
    }
    let day = i64::from(date2j_c(tm.year, tm.mon, tm.mday)) - UNIX_EPOCH_JDATE;
    let local = day * SECS_PER_DAY + i64::from(tm.sec + (tm.min + tm.hour * 60) * 60);
    let east = crate::zones::local_offset(name, local).map_err(Dterr::Refused)?;
    Ok(-east)
}

/// What DecodeDateTime and DecodeTimeOnly read: the date and time, the microseconds, the zone in
/// seconds west of UTC, what the text named (a date, `epoch` or an infinity), whether it named its
/// zone (where not, the session's zone is the caller's to read), and whether it named a date.
struct Decoded {
    tm: Tm,
    fsec: i32,
    tz: i32,
    dtype: i32,
    zoned: bool,
    dated: bool,
}

/// PostgreSQL's DecodeDateTime, its zone always asked for, under TimeZone UTC and DateStyle MDY.
fn decode_date_time(fields: &[Field], now: i64) -> D<Decoded> {
    let mut fmask: u32 = 0;
    let mut ptype = 0;
    let mut mer = HR24;
    let mut have_text_month = false;
    let mut isjulian = false;
    let mut is2digits = false;
    let mut bc = false;
    let mut named: Option<String> = None;
    let mut dynamic: Option<(String, String)> = None;
    let mut dtype = DTK_DATE;
    let mut tm = Tm::default();
    let mut fsec = 0;
    let mut tz = 0;
    for f in fields {
        let mut tmask: u32;
        let ftype = f.ftype;
        let field = f.text.clone();
        match ftype {
            DTK_DATE => {
                if ptype == DTK_JULIAN {
                    let (jday, cp, over) = strtoint(&field, 0);
                    if over || jday < 0 {
                        return Err(Dterr::FieldOverflow);
                    }
                    let (y, mo, d) = j2date_c(jday);
                    tm.year = y;
                    tm.mon = mo;
                    tm.mday = d;
                    isjulian = true;
                    tz = decode_timezone(&field[cp..])?;
                    tmask = DATE_M | TIME_M | m(TZ);
                    ptype = 0;
                } else if ptype != 0 || fmask & (m(MONTH) | m(DAY)) == m(MONTH) | m(DAY) {
                    if field[0].is_ascii_digit() || ptype != 0 {
                        if ptype != 0 {
                            if ptype != DTK_TIME {
                                return Err(Dterr::BadFormat);
                            }
                            ptype = 0;
                        }
                        if fmask & TIME_M == TIME_M {
                            return Err(Dterr::BadFormat);
                        }
                        let Some(cp) = field.iter().position(|&b| b == b'-') else {
                            return Err(Dterr::BadFormat);
                        };
                        tz = decode_timezone(&field[cp..])?;
                        let (_, t) = decode_number_field(
                            &field[..cp],
                            fmask,
                            &mut tm,
                            &mut fsec,
                            &mut is2digits,
                        )?;
                        tmask = t | m(TZ);
                    } else {
                        named = Some(named_zone(&field)?);
                        tmask = m(TZ);
                    }
                } else {
                    tmask = decode_date(&field, fmask, &mut is2digits, &mut tm)?;
                }
            }
            DTK_TIME => {
                if ptype != 0 {
                    if ptype != DTK_TIME {
                        return Err(Dterr::BadFormat);
                    }
                    ptype = 0;
                }
                let (t, f) = decode_time(&field, INTERVAL_FULL_RANGE, &mut tm)?;
                tmask = t;
                fsec = f;
                if time_overflows(tm.hour, tm.min, tm.sec, fsec) {
                    return Err(Dterr::FieldOverflow);
                }
            }
            DTK_TZ => {
                tz = decode_timezone(&field)?;
                tmask = m(TZ);
            }
            DTK_NUMBER => {
                if ptype != 0 {
                    let (value, cp, over) = strtoint(&field, 0);
                    if over {
                        return Err(Dterr::FieldOverflow);
                    }
                    if cp < field.len() && field[cp] != b'.' {
                        return Err(Dterr::BadFormat);
                    }
                    match ptype {
                        DTK_JULIAN => {
                            if value < 0 {
                                return Err(Dterr::FieldOverflow);
                            }
                            tmask = DATE_M;
                            let (y, mo, d) = j2date_c(value);
                            tm.year = y;
                            tm.mon = mo;
                            tm.mday = d;
                            isjulian = true;
                            if cp < field.len() {
                                let time = parse_fraction(&field[cp..])? * MICROS_PER_DAY as f64;
                                dt2time(time as i64, &mut tm, &mut fsec);
                                tmask |= TIME_M;
                            }
                        }
                        DTK_TIME => {
                            let (_, t) = decode_number_field(
                                &field,
                                fmask | DATE_M,
                                &mut tm,
                                &mut fsec,
                                &mut is2digits,
                            )?;
                            tmask = t;
                            if tmask != TIME_M {
                                return Err(Dterr::BadFormat);
                            }
                        }
                        _ => return Err(Dterr::BadFormat),
                    }
                    ptype = 0;
                    dtype = DTK_DATE;
                } else {
                    let flen = field.len();
                    let point = field.iter().position(|&b| b == b'.');
                    if point.is_some() && fmask & DATE_M == 0 {
                        tmask = decode_date(&field, fmask, &mut is2digits, &mut tm)?;
                    } else if point.is_some_and(|p| p > 2)
                        || (flen >= 6 && (fmask & DATE_M == 0 || fmask & TIME_M == 0))
                    {
                        let (_, t) =
                            decode_number_field(&field, fmask, &mut tm, &mut fsec, &mut is2digits)?;
                        tmask = t;
                    } else {
                        tmask = decode_number(
                            &field,
                            have_text_month,
                            fmask,
                            &mut tm,
                            &mut fsec,
                            &mut is2digits,
                        )?;
                    }
                }
            }
            DTK_STRING | DTK_SPECIAL => {
                let mut zone_of = None;
                let (ty, val) = match decode_timezone_abbrev(&field)? {
                    Some((ty, val, zone)) => {
                        zone_of = zone;
                        (ty, val)
                    }
                    None => decode_special(&field),
                };
                if ty == IGNORE_DTF {
                    continue;
                }
                tmask = m(ty);
                match ty {
                    RESERV => match val {
                        DTK_NOW => {
                            tmask = DATE_M | TIME_M | m(TZ);
                            dtype = DTK_DATE;
                            let (t, f, west) = now_local(now)?;
                            tm = t;
                            fsec = f;
                            tz = west;
                        }
                        DTK_YESTERDAY | DTK_TODAY | DTK_TOMORROW => {
                            tmask = DATE_M;
                            dtype = DTK_DATE;
                            let (t, _, _) = now_local(now)?;
                            let shift = match val {
                                DTK_YESTERDAY => -1,
                                DTK_TOMORROW => 1,
                                _ => 0,
                            };
                            let (y, mo, d) =
                                j2date_c(date2j_c(t.year, t.mon, t.mday).wrapping_add(shift));
                            tm.year = y;
                            tm.mon = mo;
                            tm.mday = d;
                        }
                        DTK_ZULU => {
                            tmask = TIME_M | m(TZ);
                            dtype = DTK_DATE;
                            tm.hour = 0;
                            tm.min = 0;
                            tm.sec = 0;
                            tz = 0;
                        }
                        _ => {
                            tmask = DATE_M | TIME_M | m(TZ);
                            dtype = val;
                        }
                    },
                    MONTH => {
                        if fmask & m(MONTH) != 0
                            && !have_text_month
                            && fmask & m(DAY) == 0
                            && (1..=31).contains(&tm.mon)
                        {
                            tm.mday = tm.mon;
                            tmask = m(DAY);
                        }
                        have_text_month = true;
                        tm.mon = val;
                    }
                    DTZMOD => {
                        tmask |= m(DTZ);
                        tz -= val;
                    }
                    DTZ => {
                        tmask |= m(TZ);
                        tz = -val;
                    }
                    TZ => {
                        tz = -val;
                    }
                    // A dynamic abbreviation's offset waits for the date it is read at.
                    DYNTZ => {
                        tmask |= m(TZ);
                        dynamic = zone_of.map(|z| (z, String::from_utf8_lossy(&field).into_owned()));
                    }
                    AMPM => mer = val,
                    ADBC => bc = val == BC,
                    DOW => {}
                    UNITS => {
                        tmask = 0;
                        if ptype != 0 {
                            return Err(Dterr::BadFormat);
                        }
                        ptype = val;
                    }
                    ISOTIME => {
                        tmask = 0;
                        if fmask & DATE_M != DATE_M {
                            return Err(Dterr::BadFormat);
                        }
                        if ptype != 0 {
                            return Err(Dterr::BadFormat);
                        }
                        ptype = val;
                    }
                    UNKNOWN_FIELD => {
                        named = Some(named_zone(&field)?);
                        tmask = m(TZ);
                    }
                    _ => return Err(Dterr::BadFormat),
                }
            }
            _ => return Err(Dterr::BadFormat),
        }
        if tmask & fmask != 0 {
            return Err(Dterr::BadFormat);
        }
        fmask |= tmask;
    }
    if ptype != 0 {
        return Err(Dterr::BadFormat);
    }
    if dtype == DTK_DATE {
        validate_date(fmask, isjulian, is2digits, bc, &mut tm)?;
        if mer != HR24 && tm.hour > 12 {
            return Err(Dterr::FieldOverflow);
        }
        if mer == AM && tm.hour == 12 {
            tm.hour = 0;
        } else if mer == PM && tm.hour != 12 {
            tm.hour += 12;
        }
        if fmask & DATE_M != DATE_M {
            // A time alone, which every caller refuses as it refuses a bad format.
            return Err(Dterr::BadFormat);
        }
        if let Some(name) = &named {
            if fmask & m(DTZMOD) != 0 {
                return Err(Dterr::BadFormat);
            }
            tz = zone_offset(name, &tm)?;
        }
        if let Some((zone, abbrev)) = &dynamic {
            if fmask & m(DTZMOD) != 0 {
                return Err(Dterr::BadFormat);
            }
            tz = abbrev_offset(zone, abbrev, &tm)?;
        }
        if fmask & m(TZ) == 0 {
            if fmask & m(DTZMOD) != 0 {
                return Err(Dterr::BadFormat);
            }
            tz = 0;
        }
    } else if let Some(name) = &named {
        // `epoch` or an infinity beside a zone: taken only where the name is a zone.
        crate::zones::utc_offset(name, 0).map_err(Dterr::Refused)?;
    }
    Ok(Decoded {
        tm,
        fsec,
        tz,
        dtype,
        zoned: fmask & m(TZ) != 0,
        dated: fmask & DATE_M == DATE_M,
    })
}

/// PostgreSQL's DecodeTimeOnly, its zone always asked for, under TimeZone UTC.
fn decode_time_only(fields: &mut [Field], now: i64) -> D<Decoded> {
    let nf = fields.len();
    let mut fmask: u32 = 0;
    let mut ptype = 0;
    let mut isjulian = false;
    let mut is2digits = false;
    let mut bc = false;
    let mut mer = HR24;
    let mut named: Option<String> = None;
    let mut dynamic: Option<(String, String)> = None;
    let dtype = DTK_TIME;
    let mut tm = Tm::default();
    let mut fsec = 0;
    let mut tz = 0;
    for i in 0..nf {
        let mut tmask: u32;
        let ftype = fields[i].ftype;
        let field = fields[i].text.clone();
        match ftype {
            DTK_DATE => {
                if i == 0
                    && nf >= 2
                    && (fields[nf - 1].ftype == DTK_DATE || fields[1].ftype == DTK_TIME)
                {
                    tmask = decode_date(&field, fmask, &mut is2digits, &mut tm)?;
                } else if field[0].is_ascii_digit() {
                    if fmask & TIME_M == TIME_M {
                        return Err(Dterr::BadFormat);
                    }
                    let Some(cp) = field.iter().position(|&b| b == b'-') else {
                        return Err(Dterr::BadFormat);
                    };
                    tz = decode_timezone(&field[cp..])?;
                    let (kind, t) = decode_number_field(
                        &field[..cp],
                        fmask | DATE_M,
                        &mut tm,
                        &mut fsec,
                        &mut is2digits,
                    )?;
                    fields[i].ftype = kind;
                    tmask = t | m(TZ);
                } else {
                    named = Some(named_zone(&field)?);
                    fields[i].ftype = DTK_TZ;
                    tmask = m(TZ);
                }
            }
            DTK_TIME => {
                if ptype != 0 {
                    if ptype != DTK_TIME {
                        return Err(Dterr::BadFormat);
                    }
                    ptype = 0;
                }
                let (t, f) = decode_time(&field, INTERVAL_FULL_RANGE, &mut tm)?;
                tmask = t;
                fsec = f;
            }
            DTK_TZ => {
                tz = decode_timezone(&field)?;
                tmask = m(TZ);
            }
            DTK_NUMBER => {
                if ptype != 0 {
                    let (value, cp, over) = strtoint(&field, 0);
                    if over {
                        return Err(Dterr::FieldOverflow);
                    }
                    if cp < field.len() && field[cp] != b'.' {
                        return Err(Dterr::BadFormat);
                    }
                    match ptype {
                        DTK_JULIAN => {
                            if value < 0 {
                                return Err(Dterr::FieldOverflow);
                            }
                            tmask = DATE_M;
                            let (y, mo, d) = j2date_c(value);
                            tm.year = y;
                            tm.mon = mo;
                            tm.mday = d;
                            isjulian = true;
                            if cp < field.len() {
                                let time = parse_fraction(&field[cp..])? * MICROS_PER_DAY as f64;
                                dt2time(time as i64, &mut tm, &mut fsec);
                                tmask |= TIME_M;
                            }
                        }
                        DTK_TIME => {
                            let (kind, t) = decode_number_field(
                                &field,
                                fmask | DATE_M,
                                &mut tm,
                                &mut fsec,
                                &mut is2digits,
                            )?;
                            fields[i].ftype = kind;
                            tmask = t;
                            if tmask != TIME_M {
                                return Err(Dterr::BadFormat);
                            }
                        }
                        _ => return Err(Dterr::BadFormat),
                    }
                    ptype = 0;
                } else {
                    let flen = field.len();
                    if let Some(p) = field.iter().position(|&b| b == b'.') {
                        if i == 0 && nf >= 2 && fields[nf - 1].ftype == DTK_DATE {
                            tmask = decode_date(&field, fmask, &mut is2digits, &mut tm)?;
                        } else if p > 2 {
                            let (kind, t) = decode_number_field(
                                &field,
                                fmask | DATE_M,
                                &mut tm,
                                &mut fsec,
                                &mut is2digits,
                            )?;
                            fields[i].ftype = kind;
                            tmask = t;
                        } else {
                            return Err(Dterr::BadFormat);
                        }
                    } else if flen > 4 {
                        let (kind, t) = decode_number_field(
                            &field,
                            fmask | DATE_M,
                            &mut tm,
                            &mut fsec,
                            &mut is2digits,
                        )?;
                        fields[i].ftype = kind;
                        tmask = t;
                    } else {
                        tmask = decode_number(
                            &field,
                            false,
                            fmask | DATE_M,
                            &mut tm,
                            &mut fsec,
                            &mut is2digits,
                        )?;
                    }
                }
            }
            DTK_STRING | DTK_SPECIAL => {
                let mut zone_of = None;
                let (ty, val) = match decode_timezone_abbrev(&field)? {
                    Some((ty, val, zone)) => {
                        zone_of = zone;
                        (ty, val)
                    }
                    None => decode_special(&field),
                };
                if ty == IGNORE_DTF {
                    continue;
                }
                tmask = m(ty);
                match ty {
                    RESERV => match val {
                        DTK_NOW => {
                            tmask = TIME_M;
                            let (t, f, _) = now_local(now)?;
                            tm = t;
                            fsec = f;
                        }
                        DTK_ZULU => {
                            tmask = TIME_M | m(TZ);
                            tm.hour = 0;
                            tm.min = 0;
                            tm.sec = 0;
                        }
                        _ => return Err(Dterr::BadFormat),
                    },
                    DTZMOD => {
                        tmask |= m(DTZ);
                        tz -= val;
                    }
                    DTZ => {
                        tmask |= m(TZ);
                        tz = -val;
                        fields[i].ftype = DTK_TZ;
                    }
                    TZ => {
                        tz = -val;
                        fields[i].ftype = DTK_TZ;
                    }
                    // A dynamic abbreviation's offset waits for the date it is read at.
                    DYNTZ => {
                        tmask |= m(TZ);
                        dynamic = zone_of.map(|z| (z, String::from_utf8_lossy(&field).into_owned()));
                        fields[i].ftype = DTK_TZ;
                    }
                    AMPM => mer = val,
                    ADBC => bc = val == BC,
                    UNITS | ISOTIME => {
                        tmask = 0;
                        if ptype != 0 {
                            return Err(Dterr::BadFormat);
                        }
                        ptype = val;
                    }
                    UNKNOWN_FIELD => {
                        named = Some(named_zone(&field)?);
                        tmask = m(TZ);
                    }
                    _ => return Err(Dterr::BadFormat),
                }
            }
            _ => return Err(Dterr::BadFormat),
        }
        if tmask & fmask != 0 {
            return Err(Dterr::BadFormat);
        }
        fmask |= tmask;
    }
    if ptype != 0 {
        return Err(Dterr::BadFormat);
    }
    validate_date(fmask, isjulian, is2digits, bc, &mut tm)?;
    if mer != HR24 && tm.hour > 12 {
        return Err(Dterr::FieldOverflow);
    }
    if mer == AM && tm.hour == 12 {
        tm.hour = 0;
    } else if mer == PM && tm.hour != 12 {
        tm.hour += 12;
    }
    if time_overflows(tm.hour, tm.min, tm.sec, fsec) {
        return Err(Dterr::FieldOverflow);
    }
    if fmask & TIME_M != TIME_M {
        return Err(Dterr::BadFormat);
    }
    if let Some(name) = &named {
        if fmask & m(DTZMOD) != 0 {
            return Err(Dterr::BadFormat);
        }
        // A zone of one fixed offset needs no date; which zones those are is the zones' to say,
        // so a named zone is read here only with its date.
        if fmask & DATE_M != DATE_M {
            return Err(Dterr::BadFormat);
        }
        tz = zone_offset(name, &tm)?;
    }
    // A dynamic abbreviation read at the date given, else at the statement's.
    if let Some((zone, abbrev)) = &dynamic {
        if fmask & m(DTZMOD) != 0 {
            return Err(Dterr::BadFormat);
        }
        let mut at = match fmask & DATE_M {
            0 => now_local(now)?.0,
            d if d != DATE_M => return Err(Dterr::BadFormat),
            _ => tm,
        };
        at.hour = tm.hour;
        at.min = tm.min;
        at.sec = tm.sec;
        tz = abbrev_offset(zone, abbrev, &at)?;
    }
    if fmask & m(TZ) == 0 {
        if fmask & m(DTZMOD) != 0 {
            return Err(Dterr::BadFormat);
        }
        if fmask & DATE_M != 0 && fmask & DATE_M != DATE_M {
            return Err(Dterr::BadFormat);
        }
        tz = 0;
    }
    Ok(Decoded {
        tm,
        fsec,
        tz,
        dtype,
        zoned: fmask & m(TZ) != 0,
        dated: fmask & DATE_M == DATE_M,
    })
}

/// PostgreSQL's dt2time: microseconds of a day, taken apart.
fn dt2time(jd: i64, tm: &mut Tm, fsec: &mut i32) {
    let mut time = jd;
    tm.hour = (time / USECS_PER_HOUR) as i32;
    time -= i64::from(tm.hour) * USECS_PER_HOUR;
    tm.min = (time / USECS_PER_MINUTE) as i32;
    time -= i64::from(tm.min) * USECS_PER_MINUTE;
    tm.sec = (time / USECS_PER_SEC) as i32;
    *fsec = (time - i64::from(tm.sec) * USECS_PER_SEC) as i32;
}

fn time2t(tm: &Tm, fsec: i32) -> i64 {
    i64::from(
        tm.hour
            .wrapping_mul(60)
            .wrapping_add(tm.min)
            .wrapping_mul(60)
            .wrapping_add(tm.sec),
    ) * USECS_PER_SEC
        + i64::from(fsec)
}

/// PostgreSQL's tm2timestamp: microseconds from 2000-01-01, the zone `tz` seconds west of UTC
/// taken off where one is given; none out of PostgreSQL's range.
fn tm2timestamp(tm: &Tm, fsec: i32, tz: Option<i32>) -> Option<i64> {
    if !valid_julian(tm.year, tm.mon) {
        return None;
    }
    let date = i64::from(date2j_c(tm.year, tm.mon, tm.mday)) - POSTGRES_EPOCH_JDATE;
    let mut result = date
        .checked_mul(MICROS_PER_DAY)?
        .checked_add(time2t(tm, fsec))?;
    if let Some(tz) = tz {
        result -= i64::from(-tz) * USECS_PER_SEC;
    }
    (MIN_TIMESTAMP..END_TIMESTAMP)
        .contains(&result)
        .then_some(result)
}

/// The snail's last instant, the microsecond before `infinity`, as PostgreSQL writes it.
const LAST_INSTANT: &str = "294247-01-10 04:00:54.775806";

/// An instant counted from 2000-01-01 counted from 1970-01-01, refused from the snail's last
/// instant on.
fn from_postgres_epoch(t: i64) -> Option<i64> {
    t.checked_add(EPOCH_SHIFT).filter(|&t| t < i64::MAX)
}

fn past_the_last(s: &str, datatype: &str, zone: &str) -> Error {
    Error::new(format!(
        "{datatype} \"{s}\" is past the snail's last instant, {LAST_INSTANT}{zone}, where Parquet's \
         64-bit timestamp ends"
    ))
}

fn decoded(s: &str, len: usize, datatype: &str, now: i64, time_only: bool) -> Result<Decoded> {
    let s = s.split('\0').next().unwrap_or_default();
    let mut fields = parse_date_time(s.as_bytes(), len).map_err(|e| parse_error(e, s, datatype))?;
    if time_only {
        decode_time_only(&mut fields, now)
    } else {
        decode_date_time(&fields, now)
    }
    .map_err(|e| parse_error(e, s, datatype))
}

fn epoch_tm() -> Tm {
    Tm {
        year: 1970,
        mon: 1,
        mday: 1,
        ..Tm::default()
    }
}

/// A date as PostgreSQL's `date_in` reads it: days since 1970-01-01, its infinities at the ends.
pub fn date_in(s: &str, now: i64) -> Result<i32> {
    let d = decoded(s, MAXDATELEN + 1, "date", now, false)?;
    let tm = match d.dtype {
        DTK_DATE => d.tm,
        DTK_EPOCH => epoch_tm(),
        DTK_LATE => return Ok(i32::MAX),
        DTK_EARLY => return Ok(i32::MIN),
        _ => return Err(parse_error(Dterr::BadFormat, s, "date")),
    };
    let out_of_range = || Error::new(format!("date out of range: \"{s}\""));
    if !valid_julian(tm.year, tm.mon) {
        return Err(out_of_range());
    }
    let jd = i64::from(date2j_c(tm.year, tm.mon, tm.mday));
    if !(0..DATE_END_JULIAN).contains(&jd) {
        return Err(out_of_range());
    }
    Ok((jd - UNIX_EPOCH_JDATE) as i32)
}

fn zero_pad(v: i64, width: usize) -> String {
    format!("{:0width$}", v.unsigned_abs(), width = width)
}

fn year_text(year: i64) -> String {
    zero_pad(if year > 0 { year } else { -(year - 1) }, 4)
}

/// A date as PostgreSQL's `date_out` writes it under DateStyle ISO.
pub fn date_out(d: i32) -> String {
    match d {
        i32::MIN => "-infinity".into(),
        i32::MAX => "infinity".into(),
        _ => {
            let (y, mo, day) = j2date(i64::from(d) + UNIX_EPOCH_JDATE);
            let mut out = format!(
                "{}-{}-{}",
                year_text(y),
                zero_pad(i64::from(mo), 2),
                zero_pad(i64::from(day), 2)
            );
            if y <= 0 {
                out.push_str(" BC");
            }
            out
        }
    }
}

/// PostgreSQL's AppendSeconds: the seconds, two digits where `fill`, and the microseconds without
/// their trailing zeros.
fn append_seconds(out: &mut String, sec: i32, fsec: i32, fill: bool) {
    if fill {
        out.push_str(&zero_pad(i64::from(sec), 2));
    } else {
        out.push_str(&sec.unsigned_abs().to_string());
    }
    if fsec != 0 {
        let digits = format!("{:06}", fsec.unsigned_abs());
        if digits.len() > 6 {
            out.push('.');
            out.push_str(&fsec.unsigned_abs().to_string());
        } else {
            out.push('.');
            out.push_str(digits.trim_end_matches('0'));
        }
    }
}

/// PostgreSQL's EncodeTimezone under ISO: the zone, `tz` seconds west of UTC, as east of it.
fn encode_timezone(out: &mut String, tz: i32) {
    let mut sec = tz.unsigned_abs();
    let mut min = sec / 60;
    sec -= min * 60;
    let hour = min / 60;
    min -= hour * 60;
    out.push(if tz <= 0 { '+' } else { '-' });
    out.push_str(&format!("{hour:02}"));
    if sec != 0 {
        out.push_str(&format!(":{min:02}:{sec:02}"));
    } else if min != 0 {
        out.push_str(&format!(":{min:02}"));
    }
}

fn time_parts(t: i64) -> (i32, i32, i32, i32) {
    let hour = t / USECS_PER_HOUR;
    let t = t - hour * USECS_PER_HOUR;
    let min = t / USECS_PER_MINUTE;
    let t = t - min * USECS_PER_MINUTE;
    let sec = t / USECS_PER_SEC;
    (
        hour as i32,
        min as i32,
        sec as i32,
        (t - sec * USECS_PER_SEC) as i32,
    )
}

fn encode_time(t: i64) -> String {
    let (h, mi, s, f) = time_parts(t);
    let mut out = format!(
        "{}:{}:",
        zero_pad(i64::from(h), 2),
        zero_pad(i64::from(mi), 2)
    );
    append_seconds(&mut out, s, f, true);
    out
}

/// A time of day as PostgreSQL's `time_in` reads it, rounded to `precision` digits where one is
/// given: microseconds since midnight.
pub fn time_in(s: &str, precision: Option<u32>, now: i64) -> Result<i64> {
    let d = decoded(s, MAXDATELEN + 1, "time", now, true)?;
    let t = time2t(&d.tm, d.fsec);
    Ok(match precision {
        Some(p) => round_time(t, p),
        None => t,
    })
}

/// A time of day as PostgreSQL's `time_out` writes it.
pub fn time_out(t: i64) -> String {
    encode_time(t)
}

/// A time with its zone as PostgreSQL's `timetz_in` reads it: microseconds since midnight, and
/// the zone in seconds west of UTC.
pub fn timetz_in(s: &str, precision: Option<u32>, now: i64) -> Result<(i64, i32)> {
    let mut d = decoded(s, MAXDATELEN + 1, "time with time zone", now, true)?;
    if !d.zoned {
        // The session's zone on the text's date, or on the transaction's own date there.
        let mut on = match d.dated {
            true => d.tm,
            false => now_tm(local_of(now)?.0).0,
        };
        on.hour = d.tm.hour;
        on.min = d.tm.min;
        on.sec = d.tm.sec;
        d.tz = session_offset(&on)?;
    }
    let t = time2t(&d.tm, d.fsec);
    let t = match precision {
        Some(p) => round_time(t, p),
        None => t,
    };
    Ok((t, d.tz))
}

/// A time with its zone, `zone` seconds west of UTC, as PostgreSQL's `timetz_out` writes it.
pub fn timetz_out(t: i64, zone: i32) -> String {
    let mut out = encode_time(t);
    encode_timezone(&mut out, zone);
    out
}

/// The offset east of UTC, in seconds, the statement's zone gives the instant `t`, microseconds
/// after 1970-01-01 UTC. The zone gives every instant the snail holds one.
fn east_of(t: i64) -> Result<i64> {
    if crate::zones::session_is_utc() {
        return Ok(0);
    }
    Ok(i64::from(crate::zones::session_utc_offset(
        t.div_euclid(USECS_PER_SEC),
    )?))
}

/// An instant on the statement zone's clock, as PostgreSQL's timestamp2tm takes it apart there:
/// microseconds after 1970-01-01 on that clock, and the zone's offset east of UTC in seconds.
pub(crate) fn local_of(t: i64) -> Result<(i64, i64)> {
    let east = east_of(t)?;
    let local = t
        .checked_add(east * USECS_PER_SEC)
        .ok_or_else(|| out_of_range("timestamp"))?;
    Ok((local, east))
}

/// PostgreSQL's DetermineTimeZoneOffset under the statement's zone: the offset west of UTC, in
/// seconds, the zone gives the date and time `tm` on its own clock; UTC outside the Julian days.
fn session_offset(tm: &Tm) -> Result<i32> {
    if crate::zones::session_is_utc() || !valid_julian(tm.year, tm.mon) {
        return Ok(0);
    }
    let day = i64::from(date2j_c(tm.year, tm.mon, tm.mday)) - UNIX_EPOCH_JDATE;
    let local = day * SECS_PER_DAY + i64::from(tm.sec + (tm.min + tm.hour * 60) * 60);
    Ok(-crate::zones::session_local_offset(local)?)
}

/// Whether a literal's text reads as an instant or a time of day naming no zone of its own, so
/// that a timestamptz's or a timetz's input reads it on the zone it is read under: each bound or
/// element where it is a range's or an array's.
pub(crate) fn reads_on_the_zone(s: &str) -> bool {
    let one = |p: &str| {
        let p = p.trim().trim_matches('"').trim();
        if p.is_empty() {
            return false;
        }
        match decoded(p, MAXDATELEN + MAXDATEFIELDS, "timestamp with time zone", 0, false) {
            Ok(d) => d.dtype == DTK_DATE && !d.zoned,
            Err(_) => decoded(p, MAXDATELEN + 1, "time with time zone", 0, true)
                .is_ok_and(|d| !d.zoned),
        }
    };
    let t = s.trim();
    match t.as_bytes().first() {
        Some(b'[' | b'(' | b'{') => t[1..]
            .trim_end_matches([']', ')', '}'])
            .split(',')
            .any(one),
        _ => one(t),
    }
}

/// The offset west of UTC, in seconds, the statement's zone gives a date and time on its clock, as
/// PostgreSQL's DetermineTimeZoneOffset finds it.
pub(crate) fn zone_offset_at(year: i32, mon: i32, mday: i32, hour: i32, min: i32, sec: i32) -> Result<i32> {
    session_offset(&Tm {
        year,
        mon,
        mday,
        hour,
        min,
        sec,
        yday: 0,
    })
}

/// The statement's clock on its zone's clock, taken apart, its microseconds in the second, and the
/// zone's offset west of UTC, as PostgreSQL's GetCurrentTimeUsec gives them.
fn now_local(now: i64) -> D<(Tm, i32, i32)> {
    let (local, east) = local_of(now).map_err(Dterr::Refused)?;
    let (tm, fsec) = now_tm(local);
    Ok((tm, fsec, -(east as i32)))
}

fn instant_in(s: &str, precision: Option<u32>, now: i64, zoned: bool) -> Result<i64> {
    let datatype = if zoned {
        "timestamp with time zone"
    } else {
        "timestamp"
    };
    let zone = if zoned { "+00" } else { "" };
    let mut d = decoded(s, MAXDATELEN + MAXDATEFIELDS, datatype, now, false)?;
    if zoned && !d.zoned && d.dtype == DTK_DATE {
        d.tz = session_offset(&d.tm)?;
    }
    let t2000 = match d.dtype {
        DTK_DATE => tm2timestamp(&d.tm, d.fsec, zoned.then_some(d.tz))
            .ok_or_else(|| Error::new(format!("timestamp out of range: \"{s}\"")))?,
        DTK_EPOCH => -EPOCH_SHIFT,
        DTK_LATE => return Ok(i64::MAX),
        DTK_EARLY => return Ok(i64::MIN),
        _ => return Err(parse_error(Dterr::BadFormat, s, datatype)),
    };
    let t = from_postgres_epoch(t2000).ok_or_else(|| past_the_last(s, datatype, zone))?;
    match precision {
        Some(p) if p <= MAX_PRECISION => {
            round_instant(t, p).map_err(|_| past_the_last(s, datatype, zone))
        }
        Some(p) => round_instant(t, p),
        None => Ok(t),
    }
}

/// A timestamp as PostgreSQL's `timestamp_in` reads it, a zone written in it read and passed
/// over: microseconds since 1970-01-01 on its own clock.
pub fn timestamp_in(s: &str, precision: Option<u32>, now: i64) -> Result<i64> {
    instant_in(s, precision, now, false)
}

/// A timestamp with time zone as PostgreSQL's `timestamptz_in` reads it, a text naming no zone on
/// the statement zone's clock: microseconds since 1970-01-01 UTC.
pub fn timestamptz_in(s: &str, precision: Option<u32>, now: i64) -> Result<i64> {
    instant_in(s, precision, now, true)
}

fn instant_out(t: i64, zoned: bool) -> String {
    match t {
        i64::MIN => return "-infinity".into(),
        i64::MAX => return "infinity".into(),
        _ => {}
    }
    // A timestamptz is written on the statement zone's clock, its offset after it.
    let (t, east) = match zoned {
        true => local_of(t).unwrap_or((t, 0)),
        false => (t, 0),
    };
    let (y, mo, d, h, mi, s, us) = civil(t);
    let mut out = format!(
        "{}-{}-{} {}:{}:",
        year_text(y),
        zero_pad(i64::from(mo), 2),
        zero_pad(i64::from(d), 2),
        zero_pad(i64::from(h), 2),
        zero_pad(i64::from(mi), 2)
    );
    append_seconds(&mut out, s as i32, us as i32, true);
    if zoned {
        encode_timezone(&mut out, -(east as i32));
    }
    if y <= 0 {
        out.push_str(" BC");
    }
    out
}

/// A timestamp as PostgreSQL's `timestamp_out` writes it under DateStyle ISO.
pub fn timestamp_out(t: i64) -> String {
    instant_out(t, false)
}

/// A timestamp with time zone as PostgreSQL's `timestamptz_out` writes it under DateStyle ISO, on
/// the statement zone's clock.
pub fn timestamptz_out(t: i64) -> String {
    instant_out(t, true)
}

const SCALES: [i64; 7] = [1_000_000, 100_000, 10_000, 1_000, 100, 10, 1];
const OFFSETS: [i64; 7] = [500_000, 50_000, 5_000, 500, 50, 5, 0];

/// An instant rounded to `precision` digits of its seconds as PostgreSQL's
/// AdjustTimestampForTypmod rounds it: a half away from 2000-01-01. The infinities stay.
pub fn round_instant(t: i64, precision: u32) -> Result<i64> {
    if t == i64::MIN || t == i64::MAX || precision == MAX_PRECISION {
        return Ok(t);
    }
    if precision > MAX_PRECISION {
        return Err(Error::new(format!(
            "timestamp({precision}) precision must be between 0 and {MAX_PRECISION}"
        )));
    }
    let p = precision as usize;
    let t2000 = i128::from(t) - i128::from(EPOCH_SHIFT);
    let (scale, offset) = (i128::from(SCALES[p]), i128::from(OFFSETS[p]));
    let rounded = if t2000 >= 0 {
        (t2000 + offset) / scale * scale
    } else {
        -((-t2000 + offset) / scale * scale)
    } + i128::from(EPOCH_SHIFT);
    i64::try_from(rounded)
        .ok()
        .filter(|&r| r < i64::MAX)
        .ok_or_else(|| {
            Error::new(format!(
                "an instant rounded to {precision} digits is past the snail's last instant, \
                 {LAST_INSTANT}, where Parquet's 64-bit timestamp ends"
            ))
        })
}

/// A time rounded to `precision` digits of its seconds as PostgreSQL's AdjustTimeForTypmod rounds
/// it: a half up.
pub fn round_time(t: i64, precision: u32) -> i64 {
    if precision > MAX_PRECISION {
        return t;
    }
    let p = precision as usize;
    if t >= 0 {
        (t + OFFSETS[p]) / SCALES[p] * SCALES[p]
    } else {
        -((-t + OFFSETS[p]) / SCALES[p] * SCALES[p])
    }
}

/// The fields an interval's typmod names, as PostgreSQL's INTERVAL_MASK bits.
pub fn interval_range(fields: &str) -> Option<u16> {
    let (y, mo, d, h, mi, s) = (
        interval_mask(YEAR),
        interval_mask(MONTH),
        interval_mask(DAY),
        interval_mask(HOUR),
        interval_mask(MINUTE),
        interval_mask(SECOND),
    );
    Some(match fields {
        "year" => y,
        "month" => mo,
        "day" => d,
        "hour" => h,
        "minute" => mi,
        "second" => s,
        "year to month" => y | mo,
        "day to hour" => d | h,
        "day to minute" => d | h | mi,
        "day to second" => d | h | mi | s,
        "hour to minute" => h | mi,
        "hour to second" => h | mi | s,
        "minute to second" => mi | s,
        _ => return None,
    })
}

/// What DecodeInterval adds up: microseconds, days, months and years.
#[derive(Default)]
struct ItmIn {
    usec: i64,
    mday: i32,
    mon: i32,
    year: i32,
}

fn adjust_fract_microseconds(frac: f64, scale: i64, itm: &mut ItmIn) -> bool {
    if frac == 0.0 {
        return true;
    }
    let frac = frac * scale as f64;
    let mut usec = frac as i64;
    let rest = frac - usec as f64;
    if rest > 0.5 {
        usec += 1;
    } else if rest < -0.5 {
        usec -= 1;
    }
    match itm.usec.checked_add(usec) {
        Some(v) => {
            itm.usec = v;
            true
        }
        None => false,
    }
}

fn adjust_fract_days(frac: f64, scale: i32, itm: &mut ItmIn) -> bool {
    if frac == 0.0 {
        return true;
    }
    let frac = frac * f64::from(scale);
    let extra = frac as i32;
    match itm.mday.checked_add(extra) {
        Some(v) => itm.mday = v,
        None => return false,
    }
    adjust_fract_microseconds(frac - f64::from(extra), MICROS_PER_DAY, itm)
}

fn adjust_fract_years(frac: f64, scale: i32, itm: &mut ItmIn) -> bool {
    let extra = (frac * f64::from(scale) * f64::from(MONTHS_PER_YEAR)).round_ties_even() as i32;
    match itm.mon.checked_add(extra) {
        Some(v) => {
            itm.mon = v;
            true
        }
        None => false,
    }
}

fn adjust_microseconds(val: i64, fval: f64, scale: i64, itm: &mut ItmIn) -> bool {
    match val.checked_mul(scale).and_then(|p| itm.usec.checked_add(p)) {
        Some(v) => itm.usec = v,
        None => return false,
    }
    adjust_fract_microseconds(fval, scale, itm)
}

fn adjust_days(val: i64, scale: i32, itm: &mut ItmIn) -> bool {
    let Ok(val) = i32::try_from(val) else {
        return false;
    };
    match val.checked_mul(scale).and_then(|d| itm.mday.checked_add(d)) {
        Some(v) => {
            itm.mday = v;
            true
        }
        None => false,
    }
}

fn adjust_months(val: i64, itm: &mut ItmIn) -> bool {
    match i32::try_from(val).ok().and_then(|v| itm.mon.checked_add(v)) {
        Some(v) => {
            itm.mon = v;
            true
        }
        None => false,
    }
}

fn adjust_years(val: i64, scale: i32, itm: &mut ItmIn) -> bool {
    match i32::try_from(val)
        .ok()
        .and_then(|v| v.checked_mul(scale))
        .and_then(|y| itm.year.checked_add(y))
    {
        Some(v) => {
            itm.year = v;
            true
        }
        None => false,
    }
}

/// PostgreSQL's DecodeTimeForInterval: a time, its hours as long as they run, as microseconds.
fn decode_time_for_interval(s: &[u8], range: u16, itm: &mut ItmIn) -> D<u32> {
    let (tmask, t) = decode_time_common(s, range)?;
    itm.usec = i64::from(t.usec);
    let total = t
        .hour
        .checked_mul(USECS_PER_HOUR)
        .and_then(|h| itm.usec.checked_add(h))
        .and_then(|u| u.checked_add(i64::from(t.min) * USECS_PER_MINUTE))
        .and_then(|u| u.checked_add(i64::from(t.sec) * USECS_PER_SEC));
    match total {
        Some(u) => {
            itm.usec = u;
            Ok(tmask)
        }
        None => Err(Dterr::FieldOverflow),
    }
}

/// PostgreSQL's DecodeInterval under IntervalStyle postgres, the fields read from the last.
fn decode_interval(fields: &[Field], range: u16, itm: &mut ItmIn) -> D<i32> {
    let nf = fields.len();
    let mut dtype = DTK_DELTA;
    let mut ty = IGNORE_DTF;
    let mut is_before = false;
    let mut parsing_unit_val = false;
    let mut fmask: u32 = 0;
    *itm = ItmIn::default();
    for i in (0..nf).rev() {
        let field = &fields[i].text;
        let mut tmask: u32;
        let mut ftype = fields[i].ftype;
        if ftype == DTK_TIME {
            tmask = decode_time_for_interval(field, range, itm)?;
            ty = DTK_DAY;
            parsing_unit_val = false;
            if tmask & fmask != 0 {
                return Err(Dterr::BadFormat);
            }
            fmask |= tmask;
            continue;
        }
        if ftype == DTK_TZ {
            if field[1..].contains(&b':') {
                if let Ok(t) = decode_time_for_interval(&field[1..], range, itm) {
                    if field[0] == b'-' {
                        if itm.usec == i64::MIN {
                            return Err(Dterr::FieldOverflow);
                        }
                        itm.usec = -itm.usec;
                    }
                    ty = DTK_DAY;
                    parsing_unit_val = false;
                    if t & fmask != 0 {
                        return Err(Dterr::BadFormat);
                    }
                    fmask |= t;
                    continue;
                }
            }
            ftype = DTK_NUMBER;
        }
        match ftype {
            DTK_DATE | DTK_NUMBER => {
                if ty == IGNORE_DTF {
                    let (y, mo, d, h, mi) = (
                        interval_mask(YEAR),
                        interval_mask(MONTH),
                        interval_mask(DAY),
                        interval_mask(HOUR),
                        interval_mask(MINUTE),
                    );
                    ty = match range {
                        r if r == y => DTK_YEAR,
                        r if r == mo || r == y | mo => DTK_MONTH,
                        r if r == d => DTK_DAY,
                        r if r == h || r == d | h => DTK_HOUR,
                        r if r == mi || r == h | mi || r == d | h | mi => DTK_MINUTE,
                        _ => DTK_SECOND,
                    };
                }
                let (mut val, cp, over) = strtol(field, 0);
                if over {
                    return Err(Dterr::FieldOverflow);
                }
                let fval: f64;
                match field.get(cp) {
                    Some(b'-') => {
                        let (mut val2, end, over2) = strtoint(field, cp + 1);
                        if over2 || !(0..MONTHS_PER_YEAR).contains(&val2) {
                            return Err(Dterr::FieldOverflow);
                        }
                        if end < field.len() {
                            return Err(Dterr::BadFormat);
                        }
                        ty = DTK_MONTH;
                        if field[0] == b'-' {
                            val2 = -val2;
                        }
                        val = val
                            .checked_mul(i64::from(MONTHS_PER_YEAR))
                            .and_then(|v| v.checked_add(i64::from(val2)))
                            .ok_or(Dterr::FieldOverflow)?;
                        fval = 0.0;
                    }
                    Some(b'.') => {
                        let f = parse_fraction(&field[cp..])?;
                        fval = if field[0] == b'-' { -f } else { f };
                    }
                    None => fval = 0.0,
                    Some(_) => return Err(Dterr::BadFormat),
                }
                let ok = match ty {
                    DTK_MICROSEC => {
                        tmask = m(MICROSECOND);
                        adjust_microseconds(val, fval, 1, itm)
                    }
                    DTK_MILLISEC => {
                        tmask = m(MILLISECOND);
                        adjust_microseconds(val, fval, 1000, itm)
                    }
                    DTK_SECOND => {
                        tmask = if fval == 0.0 { m(SECOND) } else { ALL_SECS_M };
                        adjust_microseconds(val, fval, USECS_PER_SEC, itm)
                    }
                    DTK_MINUTE => {
                        tmask = m(MINUTE);
                        adjust_microseconds(val, fval, USECS_PER_MINUTE, itm)
                    }
                    DTK_HOUR => {
                        tmask = m(HOUR);
                        let ok = adjust_microseconds(val, fval, USECS_PER_HOUR, itm);
                        ty = DTK_DAY;
                        ok
                    }
                    DTK_DAY => {
                        tmask = m(DAY);
                        adjust_days(val, 1, itm)
                            && adjust_fract_microseconds(fval, MICROS_PER_DAY, itm)
                    }
                    DTK_WEEK => {
                        tmask = m(WEEK);
                        adjust_days(val, 7, itm) && adjust_fract_days(fval, 7, itm)
                    }
                    DTK_MONTH => {
                        tmask = m(MONTH);
                        adjust_months(val, itm) && adjust_fract_days(fval, DAYS_PER_MONTH, itm)
                    }
                    DTK_YEAR => {
                        tmask = m(YEAR);
                        adjust_years(val, 1, itm) && adjust_fract_years(fval, 1, itm)
                    }
                    DTK_DECADE => {
                        tmask = m(DECADE);
                        adjust_years(val, 10, itm) && adjust_fract_years(fval, 10, itm)
                    }
                    DTK_CENTURY => {
                        tmask = m(CENTURY);
                        adjust_years(val, 100, itm) && adjust_fract_years(fval, 100, itm)
                    }
                    DTK_MILLENNIUM => {
                        tmask = m(MILLENNIUM);
                        adjust_years(val, 1000, itm) && adjust_fract_years(fval, 1000, itm)
                    }
                    _ => return Err(Dterr::BadFormat),
                };
                if !ok {
                    return Err(Dterr::FieldOverflow);
                }
                parsing_unit_val = false;
            }
            DTK_STRING | DTK_SPECIAL => {
                if parsing_unit_val {
                    return Err(Dterr::BadFormat);
                }
                let (mut kind, mut uval) = decode_units(field);
                if kind == UNKNOWN_FIELD {
                    (kind, uval) = decode_special(field);
                }
                if kind == IGNORE_DTF {
                    continue;
                }
                tmask = 0;
                match kind {
                    UNITS => {
                        ty = uval;
                        parsing_unit_val = true;
                    }
                    AGO => {
                        if i != nf - 1 {
                            return Err(Dterr::BadFormat);
                        }
                        is_before = true;
                        ty = uval;
                    }
                    RESERV => {
                        tmask = DATE_M | TIME_M;
                        if uval != DTK_LATE && uval != DTK_EARLY {
                            return Err(Dterr::BadFormat);
                        }
                        if i != nf - 1 {
                            return Err(Dterr::BadFormat);
                        }
                        dtype = uval;
                    }
                    _ => return Err(Dterr::BadFormat),
                }
            }
            _ => return Err(Dterr::BadFormat),
        }
        if tmask & fmask != 0 {
            return Err(Dterr::BadFormat);
        }
        fmask |= tmask;
    }
    if fmask == 0 {
        return Err(Dterr::BadFormat);
    }
    if parsing_unit_val {
        return Err(Dterr::BadFormat);
    }
    if is_before {
        if itm.usec == i64::MIN
            || itm.mday == i32::MIN
            || itm.mon == i32::MIN
            || itm.year == i32::MIN
        {
            return Err(Dterr::FieldOverflow);
        }
        itm.usec = -itm.usec;
        itm.mday = -itm.mday;
        itm.mon = -itm.mon;
        itm.year = -itm.year;
    }
    Ok(dtype)
}

/// PostgreSQL's ParseISO8601Number: a number as `strtod` reads it, its whole part and its
/// fraction.
fn parse_iso8601_number(s: &[u8], at: usize) -> D<(i64, f64, usize)> {
    let first = s.get(at).copied().unwrap_or(0);
    if !(first.is_ascii_digit() || first == b'-' || first == b'.') {
        return Err(Dterr::BadFormat);
    }
    let Some((val, end, range)) = strtod(s, at) else {
        return Err(Dterr::BadFormat);
    };
    if end == at || range {
        return Err(Dterr::BadFormat);
    }
    if val.is_nan() || !(-1.0e15..=1.0e15).contains(&val) {
        return Err(Dterr::FieldOverflow);
    }
    let ipart = if val >= 0.0 {
        val.floor() as i64
    } else {
        -((-val).floor()) as i64
    };
    Ok((ipart, val - ipart as f64, end))
}

fn iso8601_integer_width(s: &[u8], at: usize) -> usize {
    let at = if s.get(at) == Some(&b'-') { at + 1 } else { at };
    s[at.min(s.len())..]
        .iter()
        .take_while(|b| b.is_ascii_digit())
        .count()
}

/// PostgreSQL's DecodeISO8601Interval: `P` and its fields, or its alternative formats.
fn decode_iso8601_interval(s: &[u8], itm: &mut ItmIn) -> D<()> {
    *itm = ItmIn::default();
    if s.len() < 2 || s[0] != b'P' {
        return Err(Dterr::BadFormat);
    }
    let overflow = |ok: bool| {
        if ok {
            Ok(())
        } else {
            Err(Dterr::FieldOverflow)
        }
    };
    let mut datepart = true;
    let mut havefield = false;
    let mut i = 1;
    while i < s.len() {
        if s[i] == b'T' {
            datepart = false;
            havefield = false;
            i += 1;
            continue;
        }
        let fieldstart = i;
        let (val, fval, end) = parse_iso8601_number(s, i)?;
        i = end;
        let unit = s.get(i).copied().unwrap_or(0);
        i += 1;
        if datepart {
            let mut alternative = false;
            match unit {
                b'Y' => overflow(adjust_years(val, 1, itm) && adjust_fract_years(fval, 1, itm))?,
                b'M' => overflow(
                    adjust_months(val, itm) && adjust_fract_days(fval, DAYS_PER_MONTH, itm),
                )?,
                b'W' => overflow(adjust_days(val, 7, itm) && adjust_fract_days(fval, 7, itm))?,
                b'D' => overflow(
                    adjust_days(val, 1, itm)
                        && adjust_fract_microseconds(fval, MICROS_PER_DAY, itm),
                )?,
                b'T' | 0 => {
                    if iso8601_integer_width(s, fieldstart) == 8 && !havefield {
                        overflow(
                            adjust_years(val / 10000, 1, itm)
                                && adjust_months((val / 100) % 100, itm)
                                && adjust_days(val % 100, 1, itm)
                                && adjust_fract_microseconds(fval, MICROS_PER_DAY, itm),
                        )?;
                        if unit == 0 {
                            return Ok(());
                        }
                        datepart = false;
                        havefield = false;
                        continue;
                    }
                    alternative = true;
                }
                b'-' => alternative = true,
                _ => return Err(Dterr::BadFormat),
            }
            if alternative {
                if havefield {
                    return Err(Dterr::BadFormat);
                }
                overflow(adjust_years(val, 1, itm) && adjust_fract_years(fval, 1, itm))?;
                if unit == 0 {
                    return Ok(());
                }
                if unit == b'T' {
                    datepart = false;
                    havefield = false;
                    continue;
                }
                let (val, fval, end) = parse_iso8601_number(s, i)?;
                i = end;
                overflow(adjust_months(val, itm) && adjust_fract_days(fval, DAYS_PER_MONTH, itm))?;
                match s.get(i) {
                    None => return Ok(()),
                    Some(b'T') => {
                        datepart = false;
                        havefield = false;
                        continue;
                    }
                    Some(b'-') => i += 1,
                    Some(_) => return Err(Dterr::BadFormat),
                }
                let (val, fval, end) = parse_iso8601_number(s, i)?;
                i = end;
                overflow(
                    adjust_days(val, 1, itm)
                        && adjust_fract_microseconds(fval, MICROS_PER_DAY, itm),
                )?;
                match s.get(i) {
                    None => return Ok(()),
                    Some(b'T') => {
                        datepart = false;
                        havefield = false;
                        continue;
                    }
                    Some(_) => return Err(Dterr::BadFormat),
                }
            }
        } else {
            let mut alternative = false;
            match unit {
                b'H' => overflow(adjust_microseconds(val, fval, USECS_PER_HOUR, itm))?,
                b'M' => overflow(adjust_microseconds(val, fval, USECS_PER_MINUTE, itm))?,
                b'S' => overflow(adjust_microseconds(val, fval, USECS_PER_SEC, itm))?,
                0 => {
                    if iso8601_integer_width(s, fieldstart) == 6 && !havefield {
                        overflow(
                            adjust_microseconds(val / 10000, 0.0, USECS_PER_HOUR, itm)
                                && adjust_microseconds(
                                    (val / 100) % 100,
                                    0.0,
                                    USECS_PER_MINUTE,
                                    itm,
                                )
                                && adjust_microseconds(val % 100, 0.0, USECS_PER_SEC, itm)
                                && adjust_fract_microseconds(fval, 1, itm),
                        )?;
                        return Ok(());
                    }
                    alternative = true;
                }
                b':' => alternative = true,
                _ => return Err(Dterr::BadFormat),
            }
            if alternative {
                if havefield {
                    return Err(Dterr::BadFormat);
                }
                overflow(adjust_microseconds(val, fval, USECS_PER_HOUR, itm))?;
                if unit == 0 {
                    return Ok(());
                }
                let (val, fval, end) = parse_iso8601_number(s, i)?;
                i = end;
                overflow(adjust_microseconds(val, fval, USECS_PER_MINUTE, itm))?;
                match s.get(i) {
                    None => return Ok(()),
                    Some(b':') => i += 1,
                    Some(_) => return Err(Dterr::BadFormat),
                }
                let (val, fval, end) = parse_iso8601_number(s, i)?;
                i = end;
                overflow(adjust_microseconds(val, fval, USECS_PER_SEC, itm))?;
                if i >= s.len() {
                    return Ok(());
                }
                return Err(Dterr::BadFormat);
            }
        }
        havefield = true;
    }
    Ok(())
}

/// An interval as PostgreSQL's `interval_in` reads it under IntervalStyle postgres, held to the
/// fields `range` names and rounded to `precision` digits of its seconds where one is given.
pub fn interval_in(s: &str, range: u16, precision: Option<u32>) -> Result<Interval> {
    let s = s.split('\0').next().unwrap_or_default();
    let mut itm = ItmIn::default();
    let decoded = parse_date_time(s.as_bytes(), 256)
        .and_then(|fields| decode_interval(&fields, range, &mut itm));
    let dtype = match decoded {
        Err(Dterr::BadFormat) => {
            decode_iso8601_interval(s.as_bytes(), &mut itm).map(|()| DTK_DELTA)
        }
        other => other,
    }
    .map_err(|e| match e {
        Dterr::FieldOverflow => Error::new(format!("interval field value out of range: \"{s}\"")),
        e => parse_error(e, s, "interval"),
    })?;
    let v = match dtype {
        DTK_DELTA => {
            let months = i64::from(itm.year) * i64::from(MONTHS_PER_YEAR) + i64::from(itm.mon);
            let months = i32::try_from(months).map_err(|_| Error::new("interval out of range"))?;
            Interval {
                months,
                days: itm.mday,
                micros: itm.usec,
            }
        }
        DTK_LATE => Interval::INFINITY,
        _ => Interval::NEG_INFINITY,
    };
    round_interval(v, range, precision)
}

/// An interval as PostgreSQL's `interval_out` writes it under IntervalStyle postgres.
pub fn interval_out(v: Interval) -> String {
    if v == Interval::INFINITY {
        return "infinity".into();
    }
    if v == Interval::NEG_INFINITY {
        return "-infinity".into();
    }
    let year = i64::from(v.months / MONTHS_PER_YEAR);
    let mon = i64::from(v.months % MONTHS_PER_YEAR);
    let mday = i64::from(v.days);
    let mut time = v.micros;
    let hour = time / USECS_PER_HOUR;
    time -= hour * USECS_PER_HOUR;
    let min = (time / USECS_PER_MINUTE) as i32;
    time -= i64::from(min) * USECS_PER_MINUTE;
    let sec = (time / USECS_PER_SEC) as i32;
    let fsec = (time - i64::from(sec) * USECS_PER_SEC) as i32;
    let mut out = String::new();
    let mut is_zero = true;
    let mut is_before = false;
    for (value, unit) in [(year, "year"), (mon, "mon"), (mday, "day")] {
        if value == 0 {
            continue;
        }
        out.push_str(&format!(
            "{}{}{} {}{}",
            if is_zero { "" } else { " " },
            if is_before && value > 0 { "+" } else { "" },
            value,
            unit,
            if value != 1 { "s" } else { "" }
        ));
        is_before = value < 0;
        is_zero = false;
    }
    if is_zero || hour != 0 || min != 0 || sec != 0 || fsec != 0 {
        let minus = hour < 0 || min < 0 || sec < 0 || fsec < 0;
        out.push_str(&format!(
            "{}{}{:02}:{:02}:",
            if is_zero { "" } else { " " },
            if minus {
                "-"
            } else if is_before {
                "+"
            } else {
                ""
            },
            hour.unsigned_abs(),
            min.unsigned_abs()
        ));
        append_seconds(&mut out, sec, fsec, true);
    }
    out
}

/// An interval held to the fields `range` names and rounded to `precision` digits of its seconds,
/// as PostgreSQL's AdjustIntervalForTypmod does. The infinities stay.
pub fn round_interval(v: Interval, range: u16, precision: Option<u32>) -> Result<Interval> {
    if !v.is_finite() || (range == INTERVAL_FULL_RANGE && precision.is_none()) {
        return Ok(v);
    }
    let mut v = v;
    let (y, mo, d, h, mi, s) = (
        interval_mask(YEAR),
        interval_mask(MONTH),
        interval_mask(DAY),
        interval_mask(HOUR),
        interval_mask(MINUTE),
        interval_mask(SECOND),
    );
    let to_hour = |t: i64| t / USECS_PER_HOUR * USECS_PER_HOUR;
    let to_minute = |t: i64| t / USECS_PER_MINUTE * USECS_PER_MINUTE;
    match range {
        INTERVAL_FULL_RANGE => {}
        r if r == y => {
            v.months = v.months / MONTHS_PER_YEAR * MONTHS_PER_YEAR;
            v.days = 0;
            v.micros = 0;
        }
        r if r == mo || r == y | mo => {
            v.days = 0;
            v.micros = 0;
        }
        r if r == d => v.micros = 0,
        r if r == h || r == d | h => v.micros = to_hour(v.micros),
        r if r == mi || r == d | h | mi || r == h | mi => v.micros = to_minute(v.micros),
        r if r == s || r == d | h | mi | s || r == h | mi | s || r == mi | s => {}
        other => {
            return Err(Error::new(format!(
                "unrecognized interval typmod: {}",
                i32::from(other) << 16 | 0xFFFF
            )))
        }
    }
    if let Some(p) = precision {
        if p > MAX_PRECISION {
            return Err(Error::new(format!(
                "interval({p}) precision must be between 0 and {MAX_PRECISION}"
            )));
        }
        let p = p as usize;
        let shifted = if v.micros >= 0 {
            v.micros.checked_add(OFFSETS[p])
        } else {
            v.micros.checked_sub(OFFSETS[p])
        }
        .ok_or_else(|| Error::new("interval out of range"))?;
        v.micros = shifted - shifted % SCALES[p];
    }
    Ok(v)
}

// The time types' arithmetic, casts and comparisons across types, as PostgreSQL 19's operators and
// casts give them under TimeZone UTC, where a timestamptz behaves as a timestamp. Each works on
// PostgreSQL's own counts from 2000-01-01 inside, so a step past the snail's last instant on the
// way is taken as PostgreSQL takes it, and only an answer past it is refused.

/// Days from 1970-01-01 to 2000-01-01.
const DAYS_SHIFT: i32 = (POSTGRES_EPOCH_JDATE - UNIX_EPOCH_JDATE) as i32;
/// The first Julian day past an instant.
const TIMESTAMP_END_JULIAN: i64 = 109_203_528;

fn out_of_range(what: &str) -> Error {
    Error::new(format!("{what} out of range"))
}

/// A date as PostgreSQL counts it, from 2000-01-01; its infinities as they are.
fn date_to_pg(d: i32) -> i32 {
    match d {
        i32::MIN | i32::MAX => d,
        _ => d - DAYS_SHIFT,
    }
}

fn date_from_pg(d: i32) -> i32 {
    match d {
        i32::MIN | i32::MAX => d,
        _ => d + DAYS_SHIFT,
    }
}

/// An instant as PostgreSQL counts it, from 2000-01-01; its infinities as they are.
fn instant_to_pg(t: i64) -> i64 {
    match t {
        i64::MIN | i64::MAX => t,
        _ => t - EPOCH_SHIFT,
    }
}

/// PostgreSQL's instant as the snail holds it, refused from the snail's last instant on.
fn instant_from_pg(t: i64) -> Result<i64> {
    match t {
        i64::MIN | i64::MAX => Ok(t),
        _ => from_postgres_epoch(t).ok_or_else(|| {
            Error::new(format!(
                "the instant {} is past the snail's last instant, {LAST_INSTANT}, where Parquet's \
                 64-bit timestamp ends",
                pg_instant_text(t)
            ))
        }),
    }
}

/// An instant PostgreSQL holds and the snail may not, written as `timestamp_out` writes it.
fn pg_instant_text(t: i64) -> String {
    let (tm, fsec) = pg_tm(t).unwrap_or_default();
    let mut out = format!(
        "{}-{}-{} {}:{}:",
        year_text(i64::from(tm.year)),
        zero_pad(i64::from(tm.mon), 2),
        zero_pad(i64::from(tm.mday), 2),
        zero_pad(i64::from(tm.hour), 2),
        zero_pad(i64::from(tm.min), 2)
    );
    append_seconds(&mut out, tm.sec, fsec, true);
    if tm.year <= 0 {
        out.push_str(" BC");
    }
    out
}

/// PostgreSQL's timestamp2tm on its own count from 2000-01-01, with no zone: none outside the
/// Julian days its calendar reads.
fn pg_tm(t: i64) -> Option<(Tm, i32)> {
    let date = t.div_euclid(MICROS_PER_DAY) + POSTGRES_EPOCH_JDATE;
    let time = t.rem_euclid(MICROS_PER_DAY);
    if date < 0 || date > i64::from(i32::MAX) {
        return None;
    }
    let (year, mon, mday) = j2date_c(date as i32);
    let mut tm = Tm {
        year,
        mon,
        mday,
        ..Tm::default()
    };
    let mut fsec = 0;
    dt2time(time, &mut tm, &mut fsec);
    Some((tm, fsec))
}

/// PostgreSQL's date2timestamp: a date at its midnight, counted from 2000-01-01.
fn date2timestamp(d: i32) -> Result<i64> {
    match d {
        i32::MIN => Ok(i64::MIN),
        i32::MAX => Ok(i64::MAX),
        _ if i64::from(d) >= TIMESTAMP_END_JULIAN - POSTGRES_EPOCH_JDATE => {
            Err(Error::new("date out of range for timestamp"))
        }
        _ => Ok(i64::from(d) * MICROS_PER_DAY),
    }
}

fn valid_date(d: i64) -> bool {
    (-POSTGRES_EPOCH_JDATE..DATE_END_JULIAN - POSTGRES_EPOCH_JDATE).contains(&d)
}

fn valid_timestamp(t: i64) -> bool {
    (MIN_TIMESTAMP..END_TIMESTAMP).contains(&t)
}

/// `date + integer`, and `integer + date`: an infinity stays.
pub fn date_pli(d: i32, n: i64) -> Result<i32> {
    let n = i32::try_from(n).map_err(|_| out_of_range("integer"))?;
    if d == i32::MIN || d == i32::MAX {
        return Ok(d);
    }
    let r = i64::from(date_to_pg(d)) + i64::from(n);
    if i32::try_from(r).is_err() || !valid_date(r) {
        return Err(out_of_range("date"));
    }
    Ok(date_from_pg(r as i32))
}

/// `date - integer`: an infinity stays.
pub fn date_mii(d: i32, n: i64) -> Result<i32> {
    let n = i32::try_from(n).map_err(|_| out_of_range("integer"))?;
    if d == i32::MIN || d == i32::MAX {
        return Ok(d);
    }
    let r = i64::from(date_to_pg(d)) - i64::from(n);
    if i32::try_from(r).is_err() || !valid_date(r) {
        return Err(out_of_range("date"));
    }
    Ok(date_from_pg(r as i32))
}

/// `date - date`: the days between, refused on an infinity.
pub fn date_mi(a: i32, b: i32) -> Result<i64> {
    if a == i32::MIN || a == i32::MAX || b == i32::MIN || b == i32::MAX {
        return Err(Error::new("cannot subtract infinite dates"));
    }
    Ok(i64::from(a.wrapping_sub(b)))
}

/// `date + interval`: a timestamp.
pub fn date_pl_interval(d: i32, v: Interval) -> Result<i64> {
    instant_from_pg(pl_interval(date2timestamp(date_to_pg(d))?, v, false)?)
}

/// `date - interval`: a timestamp.
pub fn date_mi_interval(d: i32, v: Interval) -> Result<i64> {
    let at = date2timestamp(date_to_pg(d))?;
    instant_from_pg(pl_interval(at, interval_um(v)?, false)?)
}

/// `date + time`, and `time + date`: a timestamp.
pub fn datetime_pl(d: i32, t: i64) -> Result<i64> {
    let at = date2timestamp(date_to_pg(d))?;
    if at == i64::MIN || at == i64::MAX {
        return Ok(at);
    }
    let at = at + t;
    if !valid_timestamp(at) {
        return Err(out_of_range("timestamp"));
    }
    instant_from_pg(at)
}

/// `date + timetz`, and `timetz + date`: a timestamptz, the time's zone taken off.
pub fn datetimetz_pl(d: i32, t: i64, zone: i32) -> Result<i64> {
    let d = date_to_pg(d);
    match d {
        i32::MIN => return Ok(i64::MIN),
        i32::MAX => return Ok(i64::MAX),
        _ => {}
    }
    if i64::from(d) >= TIMESTAMP_END_JULIAN - POSTGRES_EPOCH_JDATE {
        return Err(Error::new("date out of range for timestamp"));
    }
    let at = i64::from(d) * MICROS_PER_DAY + t + i64::from(zone) * USECS_PER_SEC;
    if !valid_timestamp(at) {
        return Err(Error::new("date out of range for timestamp"));
    }
    instant_from_pg(at)
}

/// PostgreSQL's timestamp2tm on its own count from 2000-01-01, a timestamptz on the statement
/// zone's clock where `zoned`.
fn pg_tm_in(t: i64, zoned: bool) -> Result<(Tm, i32)> {
    let t = match zoned {
        true => t
            .checked_add(east_of(t.saturating_add(EPOCH_SHIFT))? * USECS_PER_SEC)
            .ok_or_else(|| out_of_range("timestamp"))?,
        false => t,
    };
    pg_tm(t).ok_or_else(|| out_of_range("timestamp"))
}

/// PostgreSQL's timestamp_pl_interval and timestamptz_pl_interval on its own count: the months
/// on the calendar, a day past a month's end held to its last, then the days, then the
/// microseconds. `zoned` reads the calendar on the statement zone's clock, each step's offset
/// there, and takes timestamptz's bound on the Julian day.
fn pl_interval(t: i64, v: Interval, zoned: bool) -> Result<i64> {
    let oor = || out_of_range("timestamp");
    if v == Interval::NEG_INFINITY {
        return if t == i64::MAX {
            Err(oor())
        } else {
            Ok(i64::MIN)
        };
    }
    if v == Interval::INFINITY {
        return if t == i64::MIN {
            Err(oor())
        } else {
            Ok(i64::MAX)
        };
    }
    if t == i64::MIN || t == i64::MAX {
        return Ok(t);
    }
    let mut t = t;
    let offset = |tm: &Tm| -> Result<Option<i32>> {
        Ok(match zoned {
            true => Some(session_offset(tm)?),
            false => None,
        })
    };
    if v.months != 0 {
        let (mut tm, fsec) = pg_tm_in(t, zoned)?;
        tm.mon = tm.mon.checked_add(v.months).ok_or_else(oor)?;
        if tm.mon > MONTHS_PER_YEAR {
            tm.year = tm.year.wrapping_add((tm.mon - 1) / MONTHS_PER_YEAR);
            tm.mon = (tm.mon - 1) % MONTHS_PER_YEAR + 1;
        } else if tm.mon < 1 {
            tm.year = tm.year.wrapping_add(tm.mon / MONTHS_PER_YEAR - 1);
            tm.mon = tm.mon % MONTHS_PER_YEAR + MONTHS_PER_YEAR;
        }
        let last = DAY_TAB[is_leap(tm.year) as usize][(tm.mon - 1) as usize];
        if tm.mday > last {
            tm.mday = last;
        }
        t = tm2timestamp(&tm, fsec, offset(&tm)?).ok_or_else(oor)?;
    }
    if v.days != 0 {
        let (mut tm, fsec) = pg_tm_in(t, zoned)?;
        let julian = date2j_c(tm.year, tm.mon, tm.mday)
            .checked_add(v.days)
            .filter(|&j| j >= if zoned { -1 } else { 0 })
            .ok_or_else(oor)?;
        let (y, mo, d) = j2date_c(julian);
        tm.year = y;
        tm.mon = mo;
        tm.mday = d;
        t = tm2timestamp(&tm, fsec, offset(&tm)?).ok_or_else(oor)?;
    }
    t = t.checked_add(v.micros).ok_or_else(oor)?;
    if !valid_timestamp(t) {
        return Err(oor());
    }
    Ok(t)
}

/// `timestamp + interval`, and `interval + timestamp`.
pub fn timestamp_pl_interval(t: i64, v: Interval) -> Result<i64> {
    instant_from_pg(pl_interval(instant_to_pg(t), v, false)?)
}

/// `timestamp - interval`.
pub fn timestamp_mi_interval(t: i64, v: Interval) -> Result<i64> {
    let v = interval_um(v)?;
    instant_from_pg(pl_interval(instant_to_pg(t), v, false)?)
}

/// `timestamptz + interval`, and `interval + timestamptz`, its months and days on the statement
/// zone's clock.
pub fn timestamptz_pl_interval(t: i64, v: Interval) -> Result<i64> {
    instant_from_pg(pl_interval(instant_to_pg(t), v, true)?)
}

/// `timestamptz - interval`, its months and days on the statement zone's clock.
pub fn timestamptz_mi_interval(t: i64, v: Interval) -> Result<i64> {
    let v = interval_um(v)?;
    instant_from_pg(pl_interval(instant_to_pg(t), v, true)?)
}

/// `timestamp - timestamp`, and `timestamptz - timestamptz`: the microseconds between, their
/// whole days made days as `justify_hours` makes them; an infinity less itself refused.
pub fn timestamp_mi(a: i64, b: i64) -> Result<Interval> {
    let oor = || out_of_range("interval");
    let infinite = |t: i64| t == i64::MIN || t == i64::MAX;
    if infinite(a) || infinite(b) {
        return match (a, b) {
            (i64::MIN, i64::MIN) | (i64::MAX, i64::MAX) => Err(oor()),
            (i64::MIN, _) | (_, i64::MAX) => Ok(Interval::NEG_INFINITY),
            _ => Ok(Interval::INFINITY),
        };
    }
    let micros = a.checked_sub(b).ok_or_else(oor)?;
    interval_justify_hours(Interval {
        months: 0,
        days: 0,
        micros,
    })
}

/// A time of day less its whole days, on the clock again.
fn on_the_clock(t: i64) -> i64 {
    let t = t - t / MICROS_PER_DAY * MICROS_PER_DAY;
    if t < 0 {
        t + MICROS_PER_DAY
    } else {
        t
    }
}

/// `time + interval`, and `interval + time`: its microseconds only, round the clock.
pub fn time_pl_interval(t: i64, v: Interval) -> Result<i64> {
    if !v.is_finite() {
        return Err(Error::new("cannot add infinite interval to time"));
    }
    Ok(on_the_clock(t.wrapping_add(v.micros)))
}

/// `time - interval`: its microseconds only, round the clock.
pub fn time_mi_interval(t: i64, v: Interval) -> Result<i64> {
    if !v.is_finite() {
        return Err(Error::new("cannot subtract infinite interval from time"));
    }
    Ok(on_the_clock(t.wrapping_sub(v.micros)))
}

/// `time - time`: an interval of microseconds.
pub fn time_mi_time(a: i64, b: i64) -> Interval {
    Interval {
        months: 0,
        days: 0,
        micros: a - b,
    }
}

/// `timetz + interval`, and `interval + timetz`: the time round the clock, its zone kept.
pub fn timetz_pl_interval(t: i64, zone: i32, v: Interval) -> Result<(i64, i32)> {
    Ok((time_pl_interval(t, v)?, zone))
}

/// `timetz - interval`: the time round the clock, its zone kept.
pub fn timetz_mi_interval(t: i64, zone: i32, v: Interval) -> Result<(i64, i32)> {
    Ok((time_mi_interval(t, v)?, zone))
}

/// `-interval`: each field negated, an infinity the other.
pub fn interval_um(v: Interval) -> Result<Interval> {
    if v == Interval::NEG_INFINITY {
        return Ok(Interval::INFINITY);
    }
    if v == Interval::INFINITY {
        return Ok(Interval::NEG_INFINITY);
    }
    let r = (|| {
        Some(Interval {
            months: 0i32.checked_sub(v.months)?,
            days: 0i32.checked_sub(v.days)?,
            micros: 0i64.checked_sub(v.micros)?,
        })
    })()
    .filter(|r| r.is_finite());
    r.ok_or_else(|| out_of_range("interval"))
}

/// `interval + interval`: field by field; an infinity less itself refused.
pub fn interval_pl(a: Interval, b: Interval) -> Result<Interval> {
    let oor = || out_of_range("interval");
    if a == Interval::NEG_INFINITY {
        return if b == Interval::INFINITY {
            Err(oor())
        } else {
            Ok(a)
        };
    }
    if a == Interval::INFINITY {
        return if b == Interval::NEG_INFINITY {
            Err(oor())
        } else {
            Ok(a)
        };
    }
    if !b.is_finite() {
        return Ok(b);
    }
    (|| {
        Some(Interval {
            months: a.months.checked_add(b.months)?,
            days: a.days.checked_add(b.days)?,
            micros: a.micros.checked_add(b.micros)?,
        })
    })()
    .filter(|r| r.is_finite())
    .ok_or_else(oor)
}

/// `interval - interval`: field by field; an infinity less itself refused.
pub fn interval_mi(a: Interval, b: Interval) -> Result<Interval> {
    let oor = || out_of_range("interval");
    if a == Interval::NEG_INFINITY {
        return if b == Interval::NEG_INFINITY {
            Err(oor())
        } else {
            Ok(a)
        };
    }
    if a == Interval::INFINITY {
        return if b == Interval::INFINITY {
            Err(oor())
        } else {
            Ok(a)
        };
    }
    if b == Interval::NEG_INFINITY {
        return Ok(Interval::INFINITY);
    }
    if b == Interval::INFINITY {
        return Ok(Interval::NEG_INFINITY);
    }
    (|| {
        Some(Interval {
            months: a.months.checked_sub(b.months)?,
            days: a.days.checked_sub(b.days)?,
            micros: a.micros.checked_sub(b.micros)?,
        })
    })()
    .filter(|r| r.is_finite())
    .ok_or_else(oor)
}

fn fits_i32(x: f64) -> bool {
    x >= f64::from(i32::MIN) && x < -f64::from(i32::MIN)
}

fn fits_i64(x: f64) -> bool {
    x >= i64::MIN as f64 && x < -(i64::MIN as f64)
}

/// PostgreSQL's TSROUND: a double held to six places.
fn tsround(x: f64) -> f64 {
    (x * 1_000_000.0).round_ties_even() / 1_000_000.0
}

/// The fields of `v` scaled by `scaled`, as interval_mul and interval_div make them: each field's
/// whole part, and the fractions of months and days carried down to days and seconds.
fn scale_interval(v: Interval, scaled: impl Fn(f64) -> f64) -> Result<Interval> {
    let oor = || out_of_range("interval");
    let months = scaled(f64::from(v.months));
    if months.is_nan() || !fits_i32(months) {
        return Err(oor());
    }
    let days = scaled(f64::from(v.days));
    if days.is_nan() || !fits_i32(days) {
        return Err(oor());
    }
    let mut r = Interval {
        months: months as i32,
        days: days as i32,
        micros: 0,
    };
    let month_remainder_days =
        tsround((scaled(f64::from(v.months)) - f64::from(r.months)) * f64::from(DAYS_PER_MONTH));
    let mut sec_remainder = tsround(
        (scaled(f64::from(v.days)) - f64::from(r.days) + month_remainder_days
            - f64::from(month_remainder_days as i32))
            * SECS_PER_DAY as f64,
    );
    if sec_remainder.abs() >= SECS_PER_DAY as f64 {
        let whole = (sec_remainder / SECS_PER_DAY as f64) as i32;
        r.days = r.days.checked_add(whole).ok_or_else(oor)?;
        sec_remainder -= f64::from(whole.wrapping_mul(SECS_PER_DAY as i32));
    }
    r.days = r
        .days
        .checked_add(month_remainder_days as i32)
        .ok_or_else(oor)?;
    let micros = (scaled(v.micros as f64) + sec_remainder * USECS_PER_SEC as f64).round_ties_even();
    if micros.is_nan() || !fits_i64(micros) {
        return Err(oor());
    }
    r.micros = micros as i64;
    if !r.is_finite() {
        return Err(oor());
    }
    Ok(r)
}

/// `interval * double precision`, and `double precision * interval`; a number of another type
/// reaches it as a double precision. `0 * infinity` and NaN refused.
pub fn interval_mul(v: Interval, f: f64) -> Result<Interval> {
    let oor = || out_of_range("interval");
    if f.is_nan() {
        return Err(oor());
    }
    if !v.is_finite() {
        if f == 0.0 {
            return Err(oor());
        }
        return if f < 0.0 { interval_um(v) } else { Ok(v) };
    }
    if f.is_infinite() {
        let sign = v.total().signum();
        if sign == 0 {
            return Err(oor());
        }
        return Ok(if f * (sign as f64) < 0.0 {
            Interval::NEG_INFINITY
        } else {
            Interval::INFINITY
        });
    }
    scale_interval(v, |x| x * f)
}

/// `interval / double precision`; a number of another type reaches it as a double precision.
/// Division by zero, NaN and infinity by infinity refused.
pub fn interval_div(v: Interval, f: f64) -> Result<Interval> {
    if f == 0.0 {
        return Err(Error::new("division by zero"));
    }
    if f.is_nan() {
        return Err(out_of_range("interval"));
    }
    if !v.is_finite() {
        if f.is_infinite() {
            return Err(out_of_range("interval"));
        }
        return if f < 0.0 { interval_um(v) } else { Ok(v) };
    }
    scale_interval(v, |x| x / f)
}

/// `justify_hours`: whole days of microseconds made days, the signs of days and time made one.
pub fn interval_justify_hours(v: Interval) -> Result<Interval> {
    if !v.is_finite() {
        return Ok(v);
    }
    let mut r = v;
    let wholeday = r.micros / MICROS_PER_DAY;
    r.micros -= wholeday * MICROS_PER_DAY;
    r.days = i32::try_from(wholeday)
        .ok()
        .and_then(|w| r.days.checked_add(w))
        .ok_or_else(|| out_of_range("interval"))?;
    if r.days > 0 && r.micros < 0 {
        r.micros += MICROS_PER_DAY;
        r.days -= 1;
    } else if r.days < 0 && r.micros > 0 {
        r.micros -= MICROS_PER_DAY;
        r.days += 1;
    }
    Ok(r)
}

/// `justify_days`: whole 30 days made months, the signs of months and days made one.
pub fn interval_justify_days(v: Interval) -> Result<Interval> {
    if !v.is_finite() {
        return Ok(v);
    }
    let mut r = v;
    let wholemonth = r.days / DAYS_PER_MONTH;
    r.days -= wholemonth * DAYS_PER_MONTH;
    r.months = r
        .months
        .checked_add(wholemonth)
        .ok_or_else(|| out_of_range("interval"))?;
    if r.months > 0 && r.days < 0 {
        r.days += DAYS_PER_MONTH;
        r.months -= 1;
    } else if r.months < 0 && r.days > 0 {
        r.days -= DAYS_PER_MONTH;
        r.months += 1;
    }
    Ok(r)
}

/// `justify_interval`: both, the signs of all three fields made one.
pub fn interval_justify_interval(v: Interval) -> Result<Interval> {
    if !v.is_finite() {
        return Ok(v);
    }
    let oor = || out_of_range("interval");
    let mut r = v;
    if (r.days > 0 && r.micros > 0) || (r.days < 0 && r.micros < 0) {
        let wholemonth = r.days / DAYS_PER_MONTH;
        r.days -= wholemonth * DAYS_PER_MONTH;
        r.months = r.months.checked_add(wholemonth).ok_or_else(oor)?;
    }
    let wholeday = r.micros / MICROS_PER_DAY;
    r.micros -= wholeday * MICROS_PER_DAY;
    r.days = r.days.wrapping_add(wholeday as i32);
    let wholemonth = r.days / DAYS_PER_MONTH;
    r.days -= wholemonth * DAYS_PER_MONTH;
    r.months = r.months.checked_add(wholemonth).ok_or_else(oor)?;
    if r.months > 0 && (r.days < 0 || (r.days == 0 && r.micros < 0)) {
        r.days += DAYS_PER_MONTH;
        r.months -= 1;
    } else if r.months < 0 && (r.days > 0 || (r.days == 0 && r.micros > 0)) {
        r.days -= DAYS_PER_MONTH;
        r.months += 1;
    }
    if r.days > 0 && r.micros < 0 {
        r.micros += MICROS_PER_DAY;
        r.days -= 1;
    } else if r.days < 0 && r.micros > 0 {
        r.micros -= MICROS_PER_DAY;
        r.days += 1;
    }
    Ok(r)
}

/// The cast `date::timestamp` (implicit): its midnight; a date past an instant's range refused.
pub fn date_timestamp(d: i32) -> Result<i64> {
    instant_from_pg(date2timestamp(date_to_pg(d))?)
}

/// The cast `date::timestamptz` (implicit): its midnight on the statement zone's clock.
pub fn date_timestamptz(d: i32) -> Result<i64> {
    match date_tz_rank(d)? {
        (0, _) => Ok(i64::MIN),
        (3, _) => Ok(i64::MAX),
        (1, at) if at != i128::MIN => instant_from_pg((at - i128::from(EPOCH_SHIFT)) as i64),
        _ => Err(Error::new("date out of range for timestamp")),
    }
}

/// The offset west of UTC, in seconds, the statement's zone gives a date's midnight on its clock,
/// as PostgreSQL's DetermineTimeZoneOffset finds it; UTC for an infinity.
pub fn date_zone_offset(d: i32) -> Result<i32> {
    match d {
        i32::MIN | i32::MAX => Ok(0),
        _ => session_offset(&midnight(date_to_pg(d))),
    }
}

/// The offset west of UTC, in seconds, the statement's zone gives a timestamp's time on its clock;
/// UTC for an infinity.
pub fn timestamp_zone_offset(t: i64) -> Result<i32> {
    match t {
        i64::MIN | i64::MAX => Ok(0),
        _ => Ok(-crate::zones::session_local_offset(
            t.div_euclid(USECS_PER_SEC),
        )?),
    }
}

/// The midnight of a date counted from 2000-01-01, taken apart.
fn midnight(d: i32) -> Tm {
    let (year, mon, mday) = j2date_c(d.wrapping_add(POSTGRES_EPOCH_JDATE as i32));
    Tm {
        year,
        mon,
        mday,
        ..Tm::default()
    }
}

/// Where a date's midnight on the statement zone's clock lies among instants, as PostgreSQL's
/// date2timestamptz_opt_overflow places it, ranked as `instant_rank` ranks: one past an instant's
/// range just inside the infinity on its side.
fn date_tz_rank(d: i32) -> Result<(u8, i128)> {
    let pg = date_to_pg(d);
    Ok(match pg {
        i32::MIN => (0, 0),
        i32::MAX => (3, 0),
        _ if i64::from(pg) >= TIMESTAMP_END_JULIAN - POSTGRES_EPOCH_JDATE => (2, 0),
        _ => {
            let west = session_offset(&midnight(pg))?;
            let at = i64::from(pg) * MICROS_PER_DAY + i64::from(west) * USECS_PER_SEC;
            past_instants(at)
        }
    })
}

/// A finite instant counted from 2000-01-01 ranked as `instant_rank` ranks one, past PostgreSQL's
/// range just inside the infinity on its side.
fn past_instants(at: i64) -> (u8, i128) {
    if at < MIN_TIMESTAMP {
        (1, i128::MIN)
    } else if at >= END_TIMESTAMP {
        (2, 0)
    } else {
        (1, i128::from(at) + i128::from(EPOCH_SHIFT))
    }
}

/// Where a timestamp, a time on the statement zone's clock, lies among instants, as PostgreSQL's
/// timestamp2timestamptz_opt_overflow places it, ranked as `date_tz_rank` ranks.
fn timestamp_tz_rank(t: i64) -> Result<(u8, i128)> {
    Ok(match t {
        i64::MIN => (0, 0),
        i64::MAX => (3, 0),
        _ => {
            let east = crate::zones::session_local_offset(t.div_euclid(USECS_PER_SEC))?;
            past_instants(t - EPOCH_SHIFT - i64::from(east) * USECS_PER_SEC)
        }
    })
}

/// The cast `timestamp::date` (by assignment): its day; an infinity the date's.
pub fn timestamp_date(t: i64) -> Result<i32> {
    match t {
        i64::MIN => Ok(i32::MIN),
        i64::MAX => Ok(i32::MAX),
        _ => Ok(t.div_euclid(MICROS_PER_DAY) as i32),
    }
}

/// The cast `timestamptz::date` (by assignment): its day on the statement zone's clock.
pub fn timestamptz_date(t: i64) -> Result<i32> {
    match t {
        i64::MIN | i64::MAX => timestamp_date(t),
        _ => timestamp_date(local_of(t)?.0),
    }
}

/// The cast `timestamp::time` (by assignment): its time of day; NULL for an infinity, as
/// PostgreSQL gives.
pub fn timestamp_time(t: i64) -> Result<Option<i64>> {
    match t {
        i64::MIN | i64::MAX => Ok(None),
        _ => Ok(Some(t.rem_euclid(MICROS_PER_DAY))),
    }
}

/// The cast `timestamptz::time` (by assignment): its time of day on the statement zone's clock;
/// NULL for an infinity.
pub fn timestamptz_time(t: i64) -> Result<Option<i64>> {
    match t {
        i64::MIN | i64::MAX => Ok(None),
        _ => timestamp_time(local_of(t)?.0),
    }
}

/// The cast `timestamptz::timetz` (by assignment): its time of day on the statement zone's clock,
/// at the zone's offset there; NULL for an infinity, as PostgreSQL gives.
pub fn timestamptz_timetz(t: i64) -> Result<Option<(i64, i32)>> {
    match t {
        i64::MIN | i64::MAX => Ok(None),
        _ => {
            let (local, east) = local_of(t)?;
            Ok(timestamp_time(local)?.map(|time| (time, -(east as i32))))
        }
    }
}

/// The cast `timestamp::timestamptz` (implicit): the instant its time names on the statement
/// zone's clock.
pub fn timestamp_timestamptz(t: i64) -> Result<i64> {
    match timestamp_tz_rank(t)? {
        (0, _) => Ok(i64::MIN),
        (3, _) => Ok(i64::MAX),
        (1, at) if at != i128::MIN => instant_from_pg((at - i128::from(EPOCH_SHIFT)) as i64),
        _ => Err(out_of_range("timestamp")),
    }
}

/// The cast `timestamptz::timestamp` (by assignment): the instant's time on the statement zone's
/// clock.
pub fn timestamptz_timestamp(t: i64) -> Result<i64> {
    match t {
        i64::MIN | i64::MAX => Ok(t),
        _ => {
            let local = local_of(t)?.0;
            if !valid_timestamp(local - EPOCH_SHIFT) {
                return Err(out_of_range("timestamp"));
            }
            instant_from_pg(local - EPOCH_SHIFT)
        }
    }
}

/// The cast `time::timetz` (implicit): the time at the statement zone's offset on the
/// transaction's own date there.
pub fn time_timetz(t: i64) -> Result<(i64, i32)> {
    if crate::zones::session_is_utc() {
        return Ok((t, 0));
    }
    let (mut tm, _) = now_tm(local_of(crate::scalars::transaction_clock())?.0);
    let mut fsec = 0;
    dt2time(t, &mut tm, &mut fsec);
    Ok((t, session_offset(&tm)?))
}

/// The cast `timetz::time` (by assignment): its time, its zone passed over.
pub fn timetz_time(t: i64, zone: i32) -> i64 {
    let _ = zone;
    t
}

/// The cast `time::interval` (implicit): its microseconds.
pub fn time_interval(t: i64) -> Interval {
    Interval {
        months: 0,
        days: 0,
        micros: t,
    }
}

/// The cast `interval::time` (by assignment): its microseconds round the clock; an infinity
/// refused.
pub fn interval_time(v: Interval) -> Result<i64> {
    if !v.is_finite() {
        return Err(Error::new("cannot convert infinite interval to time"));
    }
    let t = v.micros % MICROS_PER_DAY;
    Ok(if t < 0 { t + MICROS_PER_DAY } else { t })
}

/// Where a date lies among instants, as PostgreSQL's date_cmp_timestamp places it: at its
/// midnight; one past an instant's range above every finite instant and below `infinity`.
fn date_rank(d: i32) -> (u8, i128) {
    match d {
        i32::MIN => (0, 0),
        i32::MAX => (3, 0),
        _ if i64::from(date_to_pg(d)) >= TIMESTAMP_END_JULIAN - POSTGRES_EPOCH_JDATE => (2, 0),
        _ => (1, i128::from(d) * i128::from(MICROS_PER_DAY)),
    }
}

fn instant_rank(t: i64) -> (u8, i128) {
    match t {
        i64::MIN => (0, 0),
        i64::MAX => (3, 0),
        _ => (1, i128::from(t)),
    }
}

/// A date against a timestamp, as PostgreSQL's `date < timestamp` and the rest compare them.
pub fn date_cmp_timestamp(d: i32, t: i64) -> Ordering {
    date_rank(d).cmp(&instant_rank(t))
}

/// A date against a timestamptz, the date's midnight on the statement zone's clock.
pub fn date_cmp_timestamptz(d: i32, t: i64) -> Result<Ordering> {
    Ok(date_tz_rank(d)?.cmp(&instant_rank(t)))
}

/// A timestamp against a timestamptz, the timestamp's time on the statement zone's clock.
pub fn timestamp_cmp_timestamptz(a: i64, b: i64) -> Result<Ordering> {
    Ok(timestamp_tz_rank(a)?.cmp(&instant_rank(b)))
}

/// A timestamp against a date: the reverse of `date_cmp_timestamp`.
pub fn timestamp_cmp_date(t: i64, d: i32) -> Ordering {
    date_cmp_timestamp(d, t).reverse()
}

/// A timestamptz against a date: the reverse of `date_cmp_timestamptz`.
pub fn timestamptz_cmp_date(t: i64, d: i32) -> Result<Ordering> {
    Ok(date_cmp_timestamptz(d, t)?.reverse())
}

/// A timestamptz against a timestamp: the reverse of `timestamp_cmp_timestamptz`.
pub fn timestamptz_cmp_timestamp(a: i64, b: i64) -> Result<Ordering> {
    Ok(timestamp_cmp_timestamptz(b, a)?.reverse())
}

/// The cast `timetz::timetz(p)`: its time rounded to `precision` digits, its zone kept.
pub fn round_timetz(t: i64, zone: i32, precision: u32) -> (i64, i32) {
    (round_time(t, precision), zone)
}

/// What PostgreSQL's sum(interval) and avg(interval) keep as rows arrive: the finite rows and
/// their sum, and the rows of each infinity, counted apart.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct IntervalAccum {
    pub count: i64,
    pub sum: Interval,
    pub positive_infinities: i64,
    pub negative_infinities: i64,
}

impl IntervalAccum {
    /// One more row: an infinity counted, a finite interval added to the sum, refused where the
    /// sum leaves an interval's range.
    pub fn add(&mut self, v: Interval) -> Result<()> {
        if v == Interval::NEG_INFINITY {
            self.negative_infinities += 1;
        } else if v == Interval::INFINITY {
            self.positive_infinities += 1;
        } else {
            self.sum = interval_pl(self.sum, v)?;
            self.count += 1;
        }
        Ok(())
    }

    /// Another accumulation's rows taken in.
    pub fn combine(&mut self, other: &IntervalAccum) -> Result<()> {
        self.count += other.count;
        self.positive_infinities += other.positive_infinities;
        self.negative_infinities += other.negative_infinities;
        if other.count > 0 {
            self.sum = interval_pl(self.sum, other.sum)?;
        }
        Ok(())
    }

    /// The infinity the rows hold, where they hold one; both signs refused.
    fn infinity(&self) -> Result<Option<Interval>> {
        match (self.positive_infinities > 0, self.negative_infinities > 0) {
            (true, true) => Err(out_of_range("interval")),
            (true, false) => Ok(Some(Interval::INFINITY)),
            (false, true) => Ok(Some(Interval::NEG_INFINITY)),
            (false, false) => Ok(None),
        }
    }

    fn is_empty(&self) -> bool {
        self.count + self.positive_infinities + self.negative_infinities == 0
    }

    /// sum(interval): none over no rows.
    pub fn sum(&self) -> Result<Option<Interval>> {
        if self.is_empty() {
            return Ok(None);
        }
        Ok(Some(self.infinity()?.unwrap_or(self.sum)))
    }

    /// avg(interval): the sum divided by the finite rows as interval_div divides; none over no
    /// rows.
    pub fn avg(&self) -> Result<Option<Interval>> {
        if self.is_empty() {
            return Ok(None);
        }
        if let Some(v) = self.infinity()? {
            return Ok(Some(v));
        }
        interval_div(self.sum, self.count as f64).map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-10-30 12:34:56.789012 UTC, the statement's clock in these tests.
    const NOW: i64 = 1_793_363_696_789_012;

    fn date(s: &str) -> String {
        date_in(s, NOW).map_or_else(|e| format!("ERROR: {e}"), date_out)
    }

    fn ts(s: &str) -> String {
        timestamp_in(s, None, NOW).map_or_else(|e| format!("ERROR: {e}"), timestamp_out)
    }

    fn tstz(s: &str) -> String {
        timestamptz_in(s, None, NOW).map_or_else(|e| format!("ERROR: {e}"), timestamptz_out)
    }

    fn time(s: &str, p: Option<u32>) -> String {
        time_in(s, p, NOW).map_or_else(|e| format!("ERROR: {e}"), time_out)
    }

    fn timetz(s: &str) -> String {
        timetz_in(s, None, NOW).map_or_else(|e| format!("ERROR: {e}"), |(t, z)| timetz_out(t, z))
    }

    fn interval(s: &str) -> String {
        interval_in(s, INTERVAL_FULL_RANGE, None)
            .map_or_else(|e| format!("ERROR: {e}"), interval_out)
    }

    /// The day after a date on the proleptic Gregorian calendar.
    fn next_day((y, m, d): (i64, u32, u32)) -> (i64, u32, u32) {
        let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
        let last = DAY_TAB[leap as usize][m as usize - 1] as u32;
        match (d < last, m < 12) {
            (true, _) => (y, m, d + 1),
            (false, true) => (y, m + 1, 1),
            (false, false) => (y + 1, 1, 1),
        }
    }

    #[test]
    fn the_calendar_gives_back_every_julian_day_an_instant_uses_and_the_dates_past_them() {
        let walk = |from: i64, to: i64| {
            let mut day = j2date(from);
            for jd in from..to {
                assert_eq!(j2date(jd), day, "{jd}");
                assert_eq!(
                    date2j(day.0, i64::from(day.1), i64::from(day.2)),
                    jd,
                    "{jd}"
                );
                day = next_day(day);
            }
        };
        assert_eq!(j2date(0), (-4713, 11, 24));
        // Every day of an instant, 4714-11-24 BC to 294276-12-31, and the dates' last years.
        walk(0, 109_203_528 + 1);
        walk(DATE_END_JULIAN - 800_000, DATE_END_JULIAN + 1);
        for jd in (0..DATE_END_JULIAN).step_by(9973) {
            let (y, m, d) = j2date(jd);
            let c = j2date_c(jd as i32);
            assert_eq!((i64::from(c.0), c.1 as u32, c.2 as u32), (y, m, d), "{jd}");
            assert_eq!(i64::from(date2j_c(c.0, c.1, c.2)), jd, "{jd}");
        }
        assert_eq!(date2j(1970, 1, 1), UNIX_EPOCH_JDATE);
        assert_eq!(date2j(5_874_898, 1, 1), DATE_END_JULIAN);
        assert_eq!(j2date(DATE_END_JULIAN - 1), (5_874_897, 12, 31));
    }

    #[test]
    fn a_date_reads_and_writes_as_postgresql_does() {
        for (s, want) in [
            ("2024-03-05", "2024-03-05"),
            ("0044-03-15 BC", "0044-03-15 BC"),
            ("4714-11-24 BC", "4714-11-24 BC"),
            ("5874897-12-31", "5874897-12-31"),
            ("infinity", "infinity"),
            ("+infinity", "infinity"),
            ("-infinity", "-infinity"),
            ("epoch", "1970-01-01"),
            ("05/03/2024", "2024-05-03"),
            ("March 5, 2024", "2024-03-05"),
            ("5 Mar 24", "2024-03-05"),
            ("J2451545", "2000-01-01"),
            ("2024.065", "2024-03-05"),
            ("20240305", "2024-03-05"),
            ("2024-03-05 12:00+05", "2024-03-05"),
            ("today", "2026-10-30"),
            ("tomorrow", "2026-10-31"),
            ("yesterday", "2026-10-29"),
            ("now", "2026-10-30"),
        ] {
            assert_eq!(date(s), want, "{s}");
        }
        for (s, err) in [
            ("4714-11-23 BC", "date out of range: \"4714-11-23 BC\""),
            ("5874898-01-01", "date out of range: \"5874898-01-01\""),
            (
                "2024-02-30",
                "date/time field value out of range: \"2024-02-30\"",
            ),
            (
                "2024-13-01",
                "date/time field value out of range: \"2024-13-01\"",
            ),
            ("12:00", "invalid input syntax for type date: \"12:00\""),
            (
                "0000-01-01",
                "date/time field value out of range: \"0000-01-01\"",
            ),
            (
                "2024-01-01 é",
                "invalid input syntax for type date: \"2024-01-01 é\"",
            ),
        ] {
            assert_eq!(date(s), format!("ERROR: {err}"), "{s}");
        }
    }

    #[test]
    fn an_instant_reads_and_writes_as_postgresql_does_under_utc() {
        // Moscow's and New York's files are read through the directory another test may set.
        #[cfg(feature = "zones")]
        let _held = crate::zones::directory_held();
        for (s, want) in [
            ("2024-03-05 13:04:05.5", "2024-03-05 13:04:05.5+00"),
            ("2024-03-05 13:04:05+02", "2024-03-05 11:04:05+00"),
            (
                "2024-03-05T13:04:05.123456-05:30",
                "2024-03-05 18:34:05.123456+00",
            ),
            ("10000-01-01 00:00:00+00", "10000-01-01 00:00:00+00"),
            ("4714-11-24 00:00:00+00 BC", "4714-11-24 00:00:00+00 BC"),
            ("0001-01-01 00:00 BC", "0001-01-01 00:00:00+00 BC"),
            ("2024-03-05 13:04 UTC", "2024-03-05 13:04:00+00"),
            ("2024-03-05 13:04 zulu", "2024-03-05 13:04:00+00"),
            ("2024-03-05 allballs", "2024-03-05 00:00:00+00"),
            ("epoch", "1970-01-01 00:00:00+00"),
            ("infinity", "infinity"),
            ("-infinity", "-infinity"),
            ("Tue Mar 05 13:04:05 2024", "2024-03-05 13:04:05+00"),
            ("20240305T130405", "2024-03-05 13:04:05+00"),
            ("2024-03-05 1:04 pm", "2024-03-05 13:04:00+00"),
            (
                "294247-01-10 04:00:54.775806+00",
                "294247-01-10 04:00:54.775806+00",
            ),
            ("now", "2026-10-30 12:34:56.789012+00"),
        ] {
            assert_eq!(tstz(s), want, "{s}");
        }
        assert!(tstz("294247-01-10 04:00:54.775807+00").contains("past the snail's last instant"));
        assert!(tstz("294276-12-31 23:59:59.999999+00").contains("past the snail's last instant"));
        assert_eq!(
            tstz("294277-01-01 00:00:00+00"),
            "ERROR: timestamp out of range: \"294277-01-01 00:00:00+00\""
        );
        // Every abbreviation of PostgreSQL's set: a fixed one at its offset, a daylight one too, and
        // one of a zone by its rules, which only the feature zones reads.
        assert_eq!(tstz("2024-03-05 13:04 EST"), "2024-03-05 18:04:00+00");
        assert_eq!(tstz("2024-07-05 13:04 PDT"), "2024-07-05 20:04:00+00");
        assert_eq!(tstz("2024-07-05 13:04:05 MET DST"), "2024-07-05 11:04:05+00");
        let msk = tstz("2010-07-05 13:04 MSK");
        #[cfg(not(feature = "zones"))]
        assert!(msk.contains("Europe/Moscow") && msk.contains("zones"), "{msk}");
        #[cfg(feature = "zones")]
        assert_eq!(msk, "2010-07-05 10:04:00+00");
        let named = tstz("2024-03-05 13:04 America/New_York");
        #[cfg(not(feature = "zones"))]
        assert!(named.contains("zones"), "{named}");
        // Under zones, read by the zone's rules where the directory holds it: then New York kept
        // standard time, five hours west.
        #[cfg(feature = "zones")]
        assert!(
            named == "2024-03-05 18:04:00+00" || named.contains("not recognized"),
            "{named}"
        );
        assert_eq!(ts("2024-03-05 13:04:05+02"), "2024-03-05 13:04:05");
    }

    #[test]
    fn an_instant_rounds_a_half_away_from_2000_as_postgresql_does() {
        let at = |s: &str, p: u32| {
            timestamp_in(s, Some(p), NOW).map_or_else(|e| e.to_string(), timestamp_out)
        };
        assert_eq!(at("2024-01-01 00:00:00.05", 1), "2024-01-01 00:00:00.1");
        assert_eq!(at("1999-01-01 00:00:00.05", 1), "1999-01-01 00:00:00");
        assert_eq!(at("1999-12-31 23:59:59.95", 1), "1999-12-31 23:59:59.9");
        assert_eq!(at("1970-01-01 00:00:00.5", 0), "1970-01-01 00:00:00");
        assert!(at("294247-01-10 04:00:54.775806", 0).contains("past the snail's last instant"));
    }

    #[test]
    fn a_time_reads_and_writes_as_postgresql_does() {
        for (s, p, want) in [
            ("24:00:00", None, "24:00:00"),
            ("23:59:60", None, "24:00:00"),
            ("12:34:56.789", Some(1), "12:34:56.8"),
            ("12:34:56.75", Some(1), "12:34:56.8"),
            ("12:34:56.65", Some(1), "12:34:56.7"),
            ("23:59:59.9", Some(0), "24:00:00"),
            ("allballs", None, "00:00:00"),
            ("13:04:05.5+05", None, "13:04:05.5"),
            ("1:04 pm", None, "13:04:00"),
            ("130405", None, "13:04:05"),
            ("12:34.5", None, "00:12:34.5"),
            ("now", None, "12:34:56.789012"),
        ] {
            assert_eq!(time(s, p), want, "{s}");
        }
        assert_eq!(
            time("24:00:00.000001", None),
            "ERROR: date/time field value out of range: \"24:00:00.000001\""
        );
    }

    #[test]
    fn a_timetz_keeps_its_zone_as_postgresql_does() {
        for (s, want) in [
            ("12:00+01", "12:00:00+01"),
            ("13:04:05.5+05:30", "13:04:05.5+05:30"),
            ("00:00+15:59", "00:00:00+15:59"),
            ("00:00-15:59", "00:00:00-15:59"),
            ("12:00", "12:00:00+00"),
            ("12:00:00-01:02:03", "12:00:00-01:02:03"),
        ] {
            assert_eq!(timetz(s), want, "{s}");
        }
        assert_eq!(
            timetz("00:00+16"),
            "ERROR: time zone displacement out of range: \"00:00+16\""
        );
        assert_eq!(
            timetz_in("12:00+01", None, NOW).unwrap(),
            (43_200_000_000, -3600)
        );
    }

    #[test]
    fn an_interval_reads_and_writes_as_postgresql_does() {
        for (s, want) in [
            (
                "1 year 2 mons 3 days 04:05:06.7",
                "1 year 2 mons 3 days 04:05:06.7",
            ),
            (
                "-1 year -2 mons +3 days -04:05:06",
                "-1 years -2 mons +3 days -04:05:06",
            ),
            ("24 hours", "24:00:00"),
            ("0", "00:00:00"),
            ("-1 day +24 hours", "-1 days +24:00:00"),
            ("infinity", "infinity"),
            ("-infinity", "-infinity"),
            ("1.5 months", "1 mon 15 days"),
            ("1.5 years", "1 year 6 mons"),
            ("0.5 days", "12:00:00"),
            ("1.0000001 months", "1 mon 00:00:00.2592"),
            ("@ 1 day ago", "-1 days"),
            ("P1Y2M3DT4H5M6S", "1 year 2 mons 3 days 04:05:06"),
            ("P0001-02-03T04:05:06", "1 year 2 mons 3 days 04:05:06"),
            ("1-2", "1 year 2 mons"),
            ("-1 2:03:04", "-1 days +02:03:04"),
            ("178956970 years 7 mons", "178956970 years 7 mons"),
        ] {
            assert_eq!(interval(s), want, "{s}");
        }
        assert_eq!(
            interval("2562047788:00:54.775808"),
            "ERROR: interval field value out of range: \"2562047788:00:54.775808\""
        );
        let typed = |s: &str, f: &str, p: Option<u32>| {
            interval_in(s, interval_range(f).unwrap_or(INTERVAL_FULL_RANGE), p)
                .map_or_else(|e| e.to_string(), interval_out)
        };
        assert_eq!(typed("1 year 2 months", "year", None), "1 year");
        assert_eq!(
            typed("1 day 04:05:06.789", "day to minute", None),
            "1 day 04:05:00"
        );
        assert_eq!(typed("04:05:06.789", "", Some(1)), "04:05:06.8");
        assert_eq!(typed("1 day 04:05:06.789", "hour", None), "1 day 04:00:00");
        assert_eq!(
            typed("1 day 04:05:06.789", "minute to second", Some(2)),
            "1 day 04:05:06.79"
        );
    }

    #[test]
    fn an_interval_orders_by_its_total_then_its_fields() {
        let day = interval_in("1 day", INTERVAL_FULL_RANGE, None).unwrap();
        let hours = interval_in("24 hours", INTERVAL_FULL_RANGE, None).unwrap();
        assert_eq!(day.total(), hours.total());
        assert_ne!(day, hours);
        assert_ne!(day.cmp(&hours), Ordering::Equal);
        assert!(Interval::NEG_INFINITY < day && day < Interval::INFINITY);
    }

    fn iv(s: &str) -> Interval {
        interval_in(s, INTERVAL_FULL_RANGE, None).unwrap()
    }

    #[test]
    fn time_arithmetic_answers_as_postgresql_does_under_utc() {
        let d = |s: &str| date_in(s, NOW).unwrap();
        let t = |s: &str| timestamp_in(s, None, NOW).unwrap();
        let at = |r: Result<i64>| r.map_or_else(|e| e.to_string(), timestamp_out);
        let span = |r: Result<Interval>| r.map_or_else(|e| e.to_string(), interval_out);
        assert_eq!(
            at(date_pl_interval(d("2024-01-31"), iv("1 month"))),
            "2024-02-29 00:00:00"
        );
        assert_eq!(
            at(timestamp_pl_interval(t("2024-02-29"), iv("1 year"))),
            "2025-02-28 00:00:00"
        );
        assert_eq!(
            span(timestamp_mi(t("2024-03-01"), t("2024-02-01"))),
            "29 days"
        );
        assert_eq!(span(interval_mul(iv("1 day"), 1.5)), "1 day 12:00:00");
        assert_eq!(span(interval_div(iv("1 mon"), 3.0)), "10 days");
        assert_eq!(
            time_out(
                time_pl_interval(time_in("23:00", None, NOW).unwrap(), iv("2 hours")).unwrap()
            ),
            "01:00:00"
        );
        assert_eq!(
            span(interval_justify_interval(iv("1 mon -1 day 25 hours"))),
            "1 mon 01:00:00"
        );
        assert_eq!(
            date_out(date_pli(d("294276-12-31"), 1).unwrap()),
            "294277-01-01"
        );
        assert!(at(date_timestamp(d("294276-12-31"))).contains("past the snail's last instant"));
        assert_eq!(
            date_cmp_timestamp(d("5874897-12-31"), i64::MAX),
            Ordering::Less
        );
        assert_eq!(
            date_cmp_timestamp(d("5874897-12-31"), t("294247-01-10 04:00:54.775806")),
            Ordering::Greater
        );
        assert_eq!(timestamp_time(i64::MAX).unwrap(), None);
        assert_eq!(
            time_out(interval_time(iv("-25 hours")).unwrap()),
            "23:00:00"
        );
        assert_eq!(
            date_mi(i32::MAX, d("2024-01-01")).unwrap_err().to_string(),
            "cannot subtract infinite dates"
        );
        assert_eq!(
            span(timestamp_mi(i64::MAX, i64::MAX)),
            "interval out of range"
        );
        assert_eq!(
            span(interval_mul(Interval::INFINITY, 0.0)),
            "interval out of range"
        );
    }

    #[test]
    fn sum_and_avg_of_intervals_answer_as_postgresql_does() {
        let over = |vs: &[&str]| {
            let mut a = IntervalAccum::default();
            for v in vs {
                a.add(iv(v))?;
            }
            Ok::<_, Error>(a)
        };
        let text = |r: Result<Option<Interval>>| {
            r.map_or_else(|e| e.to_string(), |v| v.map_or("NULL".into(), interval_out))
        };
        let a = over(&["1 day", "2 days 03:00", "1 mon"]).unwrap();
        assert_eq!(text(a.sum()), "1 mon 3 days 03:00:00");
        assert_eq!(text(a.avg()), "11 days 01:00:00");
        let a = over(&["infinity", "1 day"]).unwrap();
        assert_eq!(
            (text(a.sum()), text(a.avg())),
            ("infinity".into(), "infinity".into())
        );
        let a = over(&["infinity", "-infinity"]).unwrap();
        assert_eq!(text(a.sum()), "interval out of range");
        let none = IntervalAccum::default();
        assert_eq!(
            (text(none.sum()), text(none.avg())),
            ("NULL".into(), "NULL".into())
        );
        assert!(over(&["178956970 years 7 mons", "1 mon"]).is_err());
    }
}
