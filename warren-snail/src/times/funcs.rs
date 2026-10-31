//! The time types' functions, as PostgreSQL 19 computes them under TimeZone UTC: extract and
//! date_part of each type, date_trunc, age, make_date and its kin, date_bin, to_timestamp of a
//! double and OVERLAPS. Each takes and gives the snail's own forms (a date's days and an instant's
//! microseconds since 1970-01-01), and works on PostgreSQL's counts from 2000-01-01 inside.

use super::*;
use crate::numeric::Numeric;

/// The longest unit PostgreSQL reads, in bytes: an identifier's.
const UNIT_BYTES: usize = 63;
const DAYS_PER_WEEK: i32 = 7;
const HOURS_PER_DAY: i32 = 24;
const MINS_PER_HOUR: i32 = 60;
const SECS_PER_MINUTE: i32 = 60;

const DATE_TYPE: &str = "date";
const TIME_TYPE: &str = "time without time zone";
const TIMETZ_TYPE: &str = "time with time zone";
const TIMESTAMP_TYPE: &str = "timestamp without time zone";
const TIMESTAMPTZ_TYPE: &str = "timestamp with time zone";
const INTERVAL_TYPE: &str = "interval";

/// A unit as PostgreSQL's `downcase_truncate_identifier` reads it: lower-cased in ASCII, cut to
/// 63 bytes at a character's end.
fn lowered(unit: &str) -> String {
    let mut s = unit.to_ascii_lowercase();
    if s.len() > UNIT_BYTES {
        let mut n = UNIT_BYTES;
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        s.truncate(n);
    }
    s
}

/// A unit's kind and value, as DecodeUnits reads it, then DecodeSpecial where it knows none.
fn unit_or_special(low: &str) -> (i32, i32) {
    match decode_units(low.as_bytes()) {
        (UNKNOWN_FIELD, _) => decode_special(low.as_bytes()),
        found => found,
    }
}

fn not_supported(low: &str, ty: &str) -> Error {
    Error::new(format!("unit \"{low}\" not supported for type {ty}"))
}

fn not_recognized(low: &str, ty: &str) -> Error {
    Error::new(format!("unit \"{low}\" not recognized for type {ty}"))
}

/// What a part comes out as: extract's numeric, or date_part's double.
enum Part {
    Numeric(Numeric),
    Double(f64),
}

impl Part {
    fn int(v: i64, numeric: bool) -> Part {
        if numeric {
            Part::Numeric(Numeric::of_int(v))
        } else {
            Part::Double(v as f64)
        }
    }

    fn infinite(negative: bool, numeric: bool) -> Part {
        if numeric {
            Part::Numeric(Numeric::infinity(negative))
        } else if negative {
            Part::Double(f64::NEG_INFINITY)
        } else {
            Part::Double(f64::INFINITY)
        }
    }

    /// `v / 10^scale` at that scale where numeric; the double given otherwise.
    fn scaled(v: i64, scale: u32, double: f64, numeric: bool) -> Part {
        if numeric {
            Part::Numeric(Numeric::of_units(i128::from(v), scale))
        } else {
            Part::Double(double)
        }
    }
}

fn numeric_of(p: Option<Part>) -> Option<Numeric> {
    match p {
        Some(Part::Numeric(n)) => Some(n),
        _ => None,
    }
}

fn double_of(p: Option<Part>) -> Option<f64> {
    match p {
        Some(Part::Double(d)) => Some(d),
        _ => None,
    }
}

/// PostgreSQL's j2day: the day of the week of a Julian day, Sunday 0.
fn j2day(jd: i32) -> i32 {
    let d = jd.wrapping_add(1) % 7;
    if d < 0 {
        d + 7
    } else {
        d
    }
}

/// The Julian day of the Monday that begins ISO week `week` of ISO year `year`.
fn isoweek2j(year: i32, week: i32) -> i32 {
    let day4 = date2j_c(year, 1, 4);
    let day0 = j2day(day4.wrapping_sub(1));
    (week.wrapping_sub(1))
        .wrapping_mul(7)
        .wrapping_add(day4.wrapping_sub(day0))
}

/// The ISO week of a date.
fn date2isoweek(year: i32, mon: i32, mday: i32) -> i32 {
    let dayn = date2j_c(year, mon, mday);
    let mut day4 = date2j_c(year, 1, 4);
    let mut day0 = j2day(day4.wrapping_sub(1));
    if dayn < day4.wrapping_sub(day0) {
        day4 = date2j_c(year.wrapping_sub(1), 1, 4);
        day0 = j2day(day4.wrapping_sub(1));
    }
    let mut week = (dayn.wrapping_sub(day4.wrapping_sub(day0))) / 7 + 1;
    if week >= 52 {
        day4 = date2j_c(year.wrapping_add(1), 1, 4);
        day0 = j2day(day4.wrapping_sub(1));
        if dayn >= day4.wrapping_sub(day0) {
            week = (dayn.wrapping_sub(day4.wrapping_sub(day0))) / 7 + 1;
        }
    }
    week
}

/// The ISO year of a date, year 0 being 1 BC.
fn date2isoyear(year: i32, mon: i32, mday: i32) -> i32 {
    let mut year = year;
    let dayn = date2j_c(year, mon, mday);
    let mut day4 = date2j_c(year, 1, 4);
    let mut day0 = j2day(day4.wrapping_sub(1));
    if dayn < day4.wrapping_sub(day0) {
        day4 = date2j_c(year.wrapping_sub(1), 1, 4);
        day0 = j2day(day4.wrapping_sub(1));
        year = year.wrapping_sub(1);
    }
    let week = (dayn.wrapping_sub(day4.wrapping_sub(day0))) / 7 + 1;
    if week >= 52 {
        day4 = date2j_c(year.wrapping_add(1), 1, 4);
        day0 = j2day(day4.wrapping_sub(1));
        if dayn >= day4.wrapping_sub(day0) {
            year = year.wrapping_add(1);
        }
    }
    year
}

/// A year's decade, century and millennium as extract counts them, and its year and ISO year
/// with no year 0.
fn year_part(val: i32, tm: &Tm, decade_from_zero: bool) -> Option<i64> {
    let y = tm.year;
    Some(i64::from(match val {
        DTK_YEAR => {
            if y > 0 {
                y
            } else {
                y - 1
            }
        }
        DTK_DECADE => {
            if (decade_from_zero && y >= 0) || (!decade_from_zero && y > 0) {
                y / 10
            } else {
                -((8 - (y - 1)) / 10)
            }
        }
        DTK_CENTURY => {
            if y > 0 {
                (y + 99) / 100
            } else {
                -((99 - (y - 1)) / 100)
            }
        }
        DTK_MILLENNIUM => {
            if y > 0 {
                (y + 999) / 1000
            } else {
                -((999 - (y - 1)) / 1000)
            }
        }
        DTK_ISOYEAR => {
            let r = date2isoyear(y, tm.mon, tm.mday);
            if r <= 0 {
                r - 1
            } else {
                r
            }
        }
        DTK_QUARTER => (tm.mon - 1) / 3 + 1,
        DTK_WEEK => date2isoweek(y, tm.mon, tm.mday),
        DTK_DOW | DTK_ISODOW => {
            let d = j2day(date2j_c(y, tm.mon, tm.mday));
            if val == DTK_ISODOW && d == 0 {
                7
            } else {
                d
            }
        }
        DTK_DOY => date2j_c(y, tm.mon, tm.mday)
            .wrapping_sub(date2j_c(y, 1, 1))
            .wrapping_add(1),
        DTK_DAY => tm.mday,
        DTK_MONTH => tm.mon,
        _ => return None,
    }))
}

/// timestamp_part_common and timestamptz_part_common, on PostgreSQL's count from 2000-01-01: a
/// timestamptz's zone is UTC's.
fn instant_part(unit: &str, t: i64, zoned: bool, numeric: bool) -> Result<Option<Part>> {
    let ty = if zoned {
        TIMESTAMPTZ_TYPE
    } else {
        TIMESTAMP_TYPE
    };
    let low = lowered(unit);
    let (kind, val) = unit_or_special(&low);
    if t == i64::MIN || t == i64::MAX {
        if kind != UNITS && kind != RESERV {
            return Err(not_recognized(&low, ty));
        }
        return match val {
            DTK_MICROSEC | DTK_MILLISEC | DTK_SECOND | DTK_MINUTE | DTK_HOUR | DTK_DAY
            | DTK_MONTH | DTK_QUARTER | DTK_WEEK | DTK_DOW | DTK_ISODOW | DTK_DOY | DTK_TZ
            | DTK_TZ_MINUTE | DTK_TZ_HOUR => Ok(None),
            DTK_YEAR | DTK_DECADE | DTK_CENTURY | DTK_MILLENNIUM | DTK_JULIAN | DTK_ISOYEAR
            | DTK_EPOCH => Ok(Some(Part::infinite(t == i64::MIN, numeric))),
            _ => Err(not_supported(&low, ty)),
        };
    }
    if kind == UNITS {
        let (tm, fsec) = pg_tm(t).ok_or_else(|| out_of_range("timestamp"))?;
        let usec = i64::from(tm.sec) * USECS_PER_SEC + i64::from(fsec);
        let v = match val {
            DTK_MICROSEC => usec,
            DTK_MILLISEC => {
                let d = f64::from(tm.sec) * 1000.0 + f64::from(fsec) / 1000.0;
                return Ok(Some(Part::scaled(usec, 3, d, numeric)));
            }
            DTK_SECOND => {
                let d = f64::from(tm.sec) + f64::from(fsec) / 1_000_000.0;
                return Ok(Some(Part::scaled(usec, 6, d, numeric)));
            }
            DTK_MINUTE => i64::from(tm.min),
            DTK_HOUR => i64::from(tm.hour),
            DTK_TZ | DTK_TZ_MINUTE | DTK_TZ_HOUR if zoned => 0,
            DTK_JULIAN => {
                let jd = date2j_c(tm.year, tm.mon, tm.mday);
                let secs = (tm.hour * MINS_PER_HOUR + tm.min) * SECS_PER_MINUTE + tm.sec;
                if numeric {
                    let day = Numeric::of_int(i64::from(secs) * USECS_PER_SEC + i64::from(fsec))
                        .div(&Numeric::of_int(SECS_PER_DAY * USECS_PER_SEC))?;
                    return Ok(Some(Part::Numeric(
                        Numeric::of_int(i64::from(jd)).add(&day)?,
                    )));
                }
                let d = f64::from(jd)
                    + (f64::from(secs) + f64::from(fsec) / 1_000_000.0) / SECS_PER_DAY as f64;
                return Ok(Some(Part::Double(d)));
            }
            _ => match year_part(val, &tm, !zoned) {
                Some(v) => v,
                None => return Err(not_supported(&low, ty)),
            },
        };
        return Ok(Some(Part::int(v, numeric)));
    }
    if kind == RESERV {
        if val != DTK_EPOCH {
            return Err(not_supported(&low, ty));
        }
        let epoch = -EPOCH_SHIFT;
        if t < i64::MAX + epoch {
            let since = t - epoch;
            return Ok(Some(Part::scaled(
                since,
                6,
                since as f64 / 1_000_000.0,
                numeric,
            )));
        }
        if numeric {
            let n = Numeric::of_int(t)
                .sub(&Numeric::of_int(epoch))?
                .div(&Numeric::of_int(1_000_000))?
                .round(6);
            return Ok(Some(Part::Numeric(n)));
        }
        return Ok(Some(Part::Double((t as f64 - epoch as f64) / 1_000_000.0)));
    }
    Err(not_recognized(&low, ty))
}

/// `extract(unit FROM timestamp)`; none where PostgreSQL gives NULL.
pub fn extract_timestamp(unit: &str, t: i64) -> Result<Option<Numeric>> {
    Ok(numeric_of(instant_part(
        unit,
        instant_to_pg(t),
        false,
        true,
    )?))
}

/// `extract(unit FROM timestamptz)` under TimeZone UTC; none where PostgreSQL gives NULL.
pub fn extract_timestamptz(unit: &str, t: i64) -> Result<Option<Numeric>> {
    Ok(numeric_of(instant_part(
        unit,
        instant_to_pg(t),
        true,
        true,
    )?))
}

/// `date_part(unit, timestamp)`; none where PostgreSQL gives NULL.
pub fn date_part_timestamp(unit: &str, t: i64) -> Result<Option<f64>> {
    Ok(double_of(instant_part(
        unit,
        instant_to_pg(t),
        false,
        false,
    )?))
}

/// `date_part(unit, timestamptz)` under TimeZone UTC; none where PostgreSQL gives NULL.
pub fn date_part_timestamptz(unit: &str, t: i64) -> Result<Option<f64>> {
    Ok(double_of(instant_part(
        unit,
        instant_to_pg(t),
        true,
        false,
    )?))
}

/// `date_part(unit, date)`: the date's midnight as a timestamp, as PostgreSQL defines it; a date
/// past an instant's range refused as the cast refuses it.
pub fn date_part_date(unit: &str, d: i32) -> Result<Option<f64>> {
    let t = date2timestamp(date_to_pg(d))?;
    Ok(double_of(instant_part(unit, t, false, false)?))
}

/// `extract(unit FROM date)`; none where PostgreSQL gives NULL.
pub fn extract_date(unit: &str, d: i32) -> Result<Option<Numeric>> {
    let low = lowered(unit);
    let (kind, val) = unit_or_special(&low);
    if (d == i32::MIN || d == i32::MAX) && (kind == UNITS || kind == RESERV) {
        return match val {
            DTK_DAY | DTK_MONTH | DTK_QUARTER | DTK_WEEK | DTK_DOW | DTK_ISODOW | DTK_DOY => {
                Ok(None)
            }
            DTK_YEAR | DTK_DECADE | DTK_CENTURY | DTK_MILLENNIUM | DTK_JULIAN | DTK_ISOYEAR
            | DTK_EPOCH => Ok(Some(Numeric::infinity(d == i32::MIN))),
            _ => Err(not_supported(&low, DATE_TYPE)),
        };
    }
    let pg = date_to_pg(d);
    let v = if kind == UNITS {
        let (year, mon, mday) = j2date_c(pg.wrapping_add(POSTGRES_EPOCH_JDATE as i32));
        let tm = Tm {
            year,
            mon,
            mday,
            ..Tm::default()
        };
        match val {
            DTK_JULIAN => i64::from(pg) + POSTGRES_EPOCH_JDATE,
            DTK_DOW | DTK_ISODOW => {
                let r = j2day(pg.wrapping_add(POSTGRES_EPOCH_JDATE as i32));
                i64::from(if val == DTK_ISODOW && r == 0 { 7 } else { r })
            }
            _ => year_part(val, &tm, true).ok_or_else(|| not_supported(&low, DATE_TYPE))?,
        }
    } else if kind == RESERV {
        if val != DTK_EPOCH {
            return Err(not_supported(&low, DATE_TYPE));
        }
        (i64::from(pg) + POSTGRES_EPOCH_JDATE - UNIX_EPOCH_JDATE) * SECS_PER_DAY
    } else {
        return Err(not_recognized(&low, DATE_TYPE));
    };
    Ok(Some(Numeric::of_int(v)))
}

/// time_part_common and timetz_part_common: a time's hour, minute, second and microseconds, and
/// a timetz's zone.
fn clock_part(unit: &str, t: i64, zone: Option<i32>, numeric: bool) -> Result<Option<Part>> {
    let ty = if zone.is_some() {
        TIMETZ_TYPE
    } else {
        TIME_TYPE
    };
    let low = lowered(unit);
    let (kind, val) = unit_or_special(&low);
    if kind == UNITS {
        let mut tm = Tm::default();
        let mut fsec = 0;
        dt2time(t, &mut tm, &mut fsec);
        let usec = i64::from(tm.sec) * USECS_PER_SEC + i64::from(fsec);
        let v = match (val, zone) {
            (DTK_TZ, Some(z)) => -i64::from(z),
            (DTK_TZ_MINUTE, Some(z)) => i64::from((-z / 60) % 60),
            (DTK_TZ_HOUR, Some(z)) => i64::from(-z / 3600),
            (DTK_MICROSEC, _) => usec,
            (DTK_MILLISEC, _) => {
                let d = f64::from(tm.sec) * 1000.0 + f64::from(fsec) / 1000.0;
                return Ok(Some(Part::scaled(usec, 3, d, numeric)));
            }
            (DTK_SECOND, _) => {
                let d = f64::from(tm.sec) + f64::from(fsec) / 1_000_000.0;
                return Ok(Some(Part::scaled(usec, 6, d, numeric)));
            }
            (DTK_MINUTE, _) => i64::from(tm.min),
            (DTK_HOUR, _) => i64::from(tm.hour),
            _ => return Err(not_supported(&low, ty)),
        };
        return Ok(Some(Part::int(v, numeric)));
    }
    if kind == RESERV && val == DTK_EPOCH {
        return Ok(Some(match zone {
            None => Part::scaled(t, 6, t as f64 / 1_000_000.0, numeric),
            Some(z) => Part::scaled(
                t + i64::from(z) * USECS_PER_SEC,
                6,
                t as f64 / 1_000_000.0 + f64::from(z),
                numeric,
            ),
        }));
    }
    Err(not_recognized(&low, ty))
}

/// `extract(unit FROM time)`.
pub fn extract_time(unit: &str, t: i64) -> Result<Option<Numeric>> {
    Ok(numeric_of(clock_part(unit, t, None, true)?))
}

/// `date_part(unit, time)`.
pub fn date_part_time(unit: &str, t: i64) -> Result<Option<f64>> {
    Ok(double_of(clock_part(unit, t, None, false)?))
}

/// `extract(unit FROM timetz)`, the zone in seconds west of UTC.
pub fn extract_timetz(unit: &str, t: i64, zone: i32) -> Result<Option<Numeric>> {
    Ok(numeric_of(clock_part(unit, t, Some(zone), true)?))
}

/// `date_part(unit, timetz)`, the zone in seconds west of UTC.
pub fn date_part_timetz(unit: &str, t: i64, zone: i32) -> Result<Option<f64>> {
    Ok(double_of(clock_part(unit, t, Some(zone), false)?))
}

/// An interval taken apart as PostgreSQL's `interval2itm` takes it: its years, months, days,
/// hours, minutes, seconds and microseconds, each with the interval's sign field by field.
#[derive(Clone, Copy, Debug, Default)]
struct Itm2 {
    year: i32,
    mon: i32,
    mday: i32,
    hour: i64,
    min: i32,
    sec: i32,
    usec: i32,
}

fn interval2itm(v: Interval) -> Itm2 {
    let mut time = v.micros;
    let hour = time / USECS_PER_HOUR;
    time -= hour * USECS_PER_HOUR;
    let min = time / USECS_PER_MINUTE;
    time -= min * USECS_PER_MINUTE;
    let sec = time / USECS_PER_SEC;
    time -= sec * USECS_PER_SEC;
    Itm2 {
        year: v.months / MONTHS_PER_YEAR,
        mon: v.months % MONTHS_PER_YEAR,
        mday: v.days,
        hour,
        min: min as i32,
        sec: sec as i32,
        usec: time as i32,
    }
}

/// PostgreSQL's itm2interval: none past an interval's fields, or where it would be an infinity.
fn itm2interval(itm: &Itm2) -> Option<Interval> {
    let months = i64::from(itm.year) * i64::from(MONTHS_PER_YEAR) + i64::from(itm.mon);
    let months = i32::try_from(months).ok()?;
    let micros = itm
        .hour
        .checked_mul(USECS_PER_HOUR)?
        .checked_add(i64::from(itm.min) * USECS_PER_MINUTE)?
        .checked_add(i64::from(itm.sec) * USECS_PER_SEC)?
        .checked_add(i64::from(itm.usec))?;
    let v = Interval {
        months,
        days: itm.mday,
        micros,
    };
    v.is_finite().then_some(v)
}

/// interval_part_common.
fn interval_part(unit: &str, v: Interval, numeric: bool) -> Result<Option<Part>> {
    let low = lowered(unit);
    let (kind, val) = unit_or_special(&low);
    if !v.is_finite() {
        if kind != UNITS && kind != RESERV {
            return Err(not_recognized(&low, INTERVAL_TYPE));
        }
        return match val {
            DTK_MICROSEC | DTK_MILLISEC | DTK_SECOND | DTK_MINUTE | DTK_WEEK | DTK_MONTH
            | DTK_QUARTER => Ok(None),
            DTK_HOUR | DTK_DAY | DTK_YEAR | DTK_DECADE | DTK_CENTURY | DTK_MILLENNIUM
            | DTK_EPOCH => Ok(Some(Part::infinite(v == Interval::NEG_INFINITY, numeric))),
            _ => Err(not_supported(&low, INTERVAL_TYPE)),
        };
    }
    if kind == UNITS {
        let tm = interval2itm(v);
        let usec = i64::from(tm.sec) * USECS_PER_SEC + i64::from(tm.usec);
        let r = match val {
            DTK_MICROSEC => usec,
            DTK_MILLISEC => {
                let d = f64::from(tm.sec) * 1000.0 + f64::from(tm.usec) / 1000.0;
                return Ok(Some(Part::scaled(usec, 3, d, numeric)));
            }
            DTK_SECOND => {
                let d = f64::from(tm.sec) + f64::from(tm.usec) / 1_000_000.0;
                return Ok(Some(Part::scaled(usec, 6, d, numeric)));
            }
            DTK_MINUTE => i64::from(tm.min),
            DTK_HOUR => tm.hour,
            DTK_DAY => i64::from(tm.mday),
            DTK_WEEK => i64::from(tm.mday / 7),
            DTK_MONTH => i64::from(tm.mon),
            DTK_QUARTER => i64::from(if v.months >= 0 {
                tm.mon / 3 + 1
            } else {
                -((v.months.wrapping_neg() % MONTHS_PER_YEAR) / 3 + 1)
            }),
            DTK_YEAR => i64::from(tm.year),
            DTK_DECADE => i64::from(tm.year / 10),
            DTK_CENTURY => i64::from(tm.year / 100),
            DTK_MILLENNIUM => i64::from(tm.year / 1000),
            _ => return Err(not_supported(&low, INTERVAL_TYPE)),
        };
        return Ok(Some(Part::int(r, numeric)));
    }
    if kind == RESERV && val == DTK_EPOCH {
        let years = i64::from(v.months / MONTHS_PER_YEAR);
        let months = i64::from(v.months % MONTHS_PER_YEAR);
        if numeric {
            let secs = (1461 * years + 120 * months + 4 * i64::from(v.days)) * (SECS_PER_DAY / 4);
            let total = i128::from(secs) * i128::from(USECS_PER_SEC) + i128::from(v.micros);
            return Ok(Some(Part::Numeric(Numeric::of_units(total, 6))));
        }
        let mut d = v.micros as f64 / 1_000_000.0;
        d += (365.25 * SECS_PER_DAY as f64) * years as f64;
        d += (f64::from(DAYS_PER_MONTH) * SECS_PER_DAY as f64) * months as f64;
        d += (SECS_PER_DAY as f64) * f64::from(v.days);
        return Ok(Some(Part::Double(d)));
    }
    Err(not_recognized(&low, INTERVAL_TYPE))
}

/// `extract(unit FROM interval)`; none where PostgreSQL gives NULL.
pub fn extract_interval(unit: &str, v: Interval) -> Result<Option<Numeric>> {
    Ok(numeric_of(interval_part(unit, v, true)?))
}

/// `date_part(unit, interval)`; none where PostgreSQL gives NULL.
pub fn date_part_interval(unit: &str, v: Interval) -> Result<Option<f64>> {
    Ok(double_of(interval_part(unit, v, false)?))
}

/// The units date_trunc cuts down to, longest first: a unit cuts every field from its own on.
const TRUNC_ORDER: [i32; 10] = [
    DTK_MILLENNIUM,
    DTK_CENTURY,
    DTK_DECADE,
    DTK_YEAR,
    DTK_QUARTER,
    DTK_MONTH,
    DTK_DAY,
    DTK_HOUR,
    DTK_MINUTE,
    DTK_SECOND,
];

/// A date and time cut down to a unit as date_trunc cuts them; whether the unit is a day or
/// longer, which asks a zone's offset again. None for a unit it does not cut to.
fn trunc_tm(val: i32, tm: &mut Tm, fsec: &mut i32) -> Option<bool> {
    match val {
        DTK_WEEK => {
            let woy = date2isoweek(tm.year, tm.mon, tm.mday);
            if woy >= 52 && tm.mon == 1 {
                tm.year -= 1;
            }
            if woy <= 1 && tm.mon == MONTHS_PER_YEAR {
                tm.year += 1;
            }
            let (y, m, d) = j2date_c(isoweek2j(tm.year, woy));
            (tm.year, tm.mon, tm.mday) = (y, m, d);
            (tm.hour, tm.min, tm.sec) = (0, 0, 0);
            *fsec = 0;
            Some(true)
        }
        DTK_MILLISEC => {
            *fsec = (*fsec / 1000) * 1000;
            Some(false)
        }
        DTK_MICROSEC => Some(false),
        _ => {
            let from = TRUNC_ORDER.iter().position(|&u| u == val)?;
            let mut longer = false;
            for &u in &TRUNC_ORDER[from..] {
                match u {
                    DTK_MILLENNIUM => {
                        tm.year = if tm.year > 0 {
                            ((tm.year + 999) / 1000) * 1000 - 999
                        } else {
                            -((999 - (tm.year - 1)) / 1000) * 1000 + 1
                        }
                    }
                    DTK_CENTURY => {
                        tm.year = if tm.year > 0 {
                            ((tm.year + 99) / 100) * 100 - 99
                        } else {
                            -((99 - (tm.year - 1)) / 100) * 100 + 1
                        }
                    }
                    DTK_DECADE if val == DTK_DECADE => {
                        tm.year = if tm.year > 0 {
                            (tm.year / 10) * 10
                        } else {
                            -((8 - (tm.year - 1)) / 10) * 10
                        }
                    }
                    DTK_YEAR => tm.mon = 1,
                    DTK_QUARTER => tm.mon = 3 * ((tm.mon - 1) / 3) + 1,
                    DTK_MONTH => tm.mday = 1,
                    DTK_DAY => {
                        tm.hour = 0;
                        longer = true;
                    }
                    DTK_HOUR => tm.min = 0,
                    DTK_MINUTE => tm.sec = 0,
                    DTK_SECOND => *fsec = 0,
                    _ => {}
                }
            }
            Some(longer)
        }
    }
}

/// Whether date_trunc keeps an infinite instant for this unit.
fn trunc_keeps_infinity(val: i32) -> bool {
    matches!(
        val,
        DTK_WEEK
            | DTK_MILLENNIUM
            | DTK_CENTURY
            | DTK_DECADE
            | DTK_YEAR
            | DTK_QUARTER
            | DTK_MONTH
            | DTK_DAY
            | DTK_HOUR
            | DTK_MINUTE
            | DTK_SECOND
            | DTK_MILLISEC
            | DTK_MICROSEC
    )
}

/// `date_trunc(unit, timestamp)`.
pub fn timestamp_trunc(unit: &str, t: i64) -> Result<i64> {
    let low = lowered(unit);
    let (kind, val) = decode_units(low.as_bytes());
    if kind != UNITS {
        return Err(not_recognized(&low, TIMESTAMP_TYPE));
    }
    if t == i64::MIN || t == i64::MAX {
        if trunc_keeps_infinity(val) {
            return Ok(t);
        }
        return Err(not_supported(&low, TIMESTAMP_TYPE));
    }
    let (mut tm, mut fsec) = pg_tm(instant_to_pg(t)).ok_or_else(|| out_of_range("timestamp"))?;
    trunc_tm(val, &mut tm, &mut fsec).ok_or_else(|| not_supported(&low, TIMESTAMP_TYPE))?;
    let r = tm2timestamp(&tm, fsec, None).ok_or_else(|| out_of_range("timestamp"))?;
    instant_from_pg(r)
}

/// timestamptz_trunc_internal, the zone given by its offsets: east of UTC at an instant's UTC
/// second, and west of UTC at a date and time on its own clock.
fn zoned_trunc(
    unit: &str,
    t: i64,
    east_at: &dyn Fn(i64) -> Result<i32>,
    west_of: &dyn Fn(&Tm) -> Result<i32>,
) -> Result<i64> {
    let low = lowered(unit);
    let (kind, val) = decode_units(low.as_bytes());
    if kind != UNITS {
        return Err(not_recognized(&low, TIMESTAMPTZ_TYPE));
    }
    if t == i64::MIN || t == i64::MAX {
        if trunc_keeps_infinity(val) {
            return Ok(t);
        }
        return Err(not_supported(&low, TIMESTAMPTZ_TYPE));
    }
    let pg = instant_to_pg(t);
    let (_, fsec0) = pg_tm(pg).ok_or_else(|| out_of_range("timestamp"))?;
    let unix = (pg - i64::from(fsec0)) / USECS_PER_SEC + EPOCH_SHIFT / USECS_PER_SEC;
    let east = east_at(unix)?;
    let local = unix + i64::from(east);
    let (year, mon, mday) = j2date(local.div_euclid(SECS_PER_DAY) + UNIX_EPOCH_JDATE);
    let secs = local.rem_euclid(SECS_PER_DAY) as i32;
    let mut tm = Tm {
        year: year as i32,
        mon: mon as i32,
        mday: mday as i32,
        hour: secs / 3600,
        min: secs / 60 % 60,
        sec: secs % 60,
        yday: 0,
    };
    let mut fsec = fsec0;
    let mut tz = -east;
    let longer =
        trunc_tm(val, &mut tm, &mut fsec).ok_or_else(|| not_supported(&low, TIMESTAMPTZ_TYPE))?;
    if longer {
        tz = west_of(&tm)?;
    }
    let r = tm2timestamp(&tm, fsec, Some(tz)).ok_or_else(|| out_of_range("timestamp"))?;
    instant_from_pg(r)
}

/// `date_trunc(unit, timestamptz)` under TimeZone UTC.
pub fn timestamptz_trunc(unit: &str, t: i64) -> Result<i64> {
    zoned_trunc(unit, t, &|_| Ok(0), &|_| Ok(0))
}

/// A zone's name cut to the 255 bytes PostgreSQL reads of one, at a character's end.
fn zone_text(zone: &str) -> &str {
    let mut cut = zone.len().min(255);
    while !zone.is_char_boundary(cut) {
        cut -= 1;
    }
    &zone[..cut]
}

/// `date_trunc(unit, timestamptz, zone)`: cut on the zone's clock. The zone is looked up as
/// PostgreSQL looks one up: an abbreviation first, UTC's read and any other refused by name, then
/// a zone as AT TIME ZONE reads one.
pub fn timestamptz_trunc_zone(unit: &str, t: i64, zone: &str) -> Result<i64> {
    let zone = zone_text(zone);
    match decode_timezone_abbrev(lowered(zone).as_bytes()) {
        Ok(Some(_)) => return timestamptz_trunc(unit, t),
        Err(Dterr::Refused(e)) => return Err(e),
        _ => {}
    }
    crate::zones::utc_offset(zone, 0)?;
    zoned_trunc(
        unit,
        t,
        &|unix| crate::zones::utc_offset(zone, unix),
        &|tm| {
            zone_offset(zone, tm).map_err(|e| match e {
                Dterr::Refused(e) => e,
                _ => out_of_range("timestamp"),
            })
        },
    )
}

/// `date_trunc(unit, interval)`.
pub fn interval_trunc(unit: &str, v: Interval) -> Result<Interval> {
    let low = lowered(unit);
    let (kind, val) = decode_units(low.as_bytes());
    if kind != UNITS {
        return Err(not_recognized(&low, INTERVAL_TYPE));
    }
    if !v.is_finite() {
        return match val {
            DTK_MILLENNIUM | DTK_CENTURY | DTK_DECADE | DTK_YEAR | DTK_QUARTER | DTK_MONTH
            | DTK_DAY | DTK_HOUR | DTK_MINUTE | DTK_SECOND | DTK_MILLISEC | DTK_MICROSEC => Ok(v),
            _ => Err(not_supported(&low, INTERVAL_TYPE)),
        };
    }
    let mut tm = interval2itm(v);
    match val {
        DTK_MILLISEC => tm.usec = (tm.usec / 1000) * 1000,
        DTK_MICROSEC => {}
        _ => {
            let from = TRUNC_ORDER
                .iter()
                .position(|&x| x == val)
                .ok_or_else(|| not_supported(&low, INTERVAL_TYPE))?;
            for &u in &TRUNC_ORDER[from..] {
                match u {
                    DTK_MILLENNIUM => tm.year = (tm.year / 1000) * 1000,
                    DTK_CENTURY => tm.year = (tm.year / 100) * 100,
                    DTK_DECADE => tm.year = (tm.year / 10) * 10,
                    DTK_YEAR => tm.mon = 0,
                    DTK_QUARTER => tm.mon = 3 * (tm.mon / 3),
                    DTK_MONTH => tm.mday = 0,
                    DTK_DAY => tm.hour = 0,
                    DTK_HOUR => tm.min = 0,
                    DTK_MINUTE => tm.sec = 0,
                    _ => tm.usec = 0,
                }
            }
        }
    }
    itm2interval(&tm).ok_or_else(|| out_of_range("interval"))
}

/// `age(timestamp, timestamp)`, and `age(timestamptz, timestamptz)` under TimeZone UTC, which is
/// the same: the difference field by field, years and months kept.
pub fn timestamp_age(a: i64, b: i64) -> Result<Interval> {
    let infinite = |t: i64| t == i64::MIN || t == i64::MAX;
    if infinite(a) || infinite(b) {
        return match (a, b) {
            (i64::MIN, i64::MIN) | (i64::MAX, i64::MAX) => Err(out_of_range("interval")),
            (i64::MIN, _) | (_, i64::MAX) => Ok(Interval::NEG_INFINITY),
            _ => Ok(Interval::INFINITY),
        };
    }
    let (dt1, dt2) = (instant_to_pg(a), instant_to_pg(b));
    let (tm1, fsec1) = pg_tm(dt1).ok_or_else(|| out_of_range("timestamp"))?;
    let (tm2, fsec2) = pg_tm(dt2).ok_or_else(|| out_of_range("timestamp"))?;
    let mut tm = Itm2 {
        usec: fsec1 - fsec2,
        sec: tm1.sec - tm2.sec,
        min: tm1.min - tm2.min,
        hour: i64::from(tm1.hour - tm2.hour),
        mday: tm1.mday - tm2.mday,
        mon: tm1.mon - tm2.mon,
        year: tm1.year - tm2.year,
    };
    let flip = |tm: &mut Itm2| {
        tm.usec = -tm.usec;
        tm.sec = -tm.sec;
        tm.min = -tm.min;
        tm.hour = -tm.hour;
        tm.mday = -tm.mday;
        tm.mon = -tm.mon;
        tm.year = -tm.year;
    };
    if dt1 < dt2 {
        flip(&mut tm);
    }
    while tm.usec < 0 {
        tm.usec += USECS_PER_SEC as i32;
        tm.sec -= 1;
    }
    while tm.sec < 0 {
        tm.sec += SECS_PER_MINUTE;
        tm.min -= 1;
    }
    while tm.min < 0 {
        tm.min += MINS_PER_HOUR;
        tm.hour -= 1;
    }
    while tm.hour < 0 {
        tm.hour += i64::from(HOURS_PER_DAY);
        tm.mday -= 1;
    }
    while tm.mday < 0 {
        let from = if dt1 < dt2 { &tm1 } else { &tm2 };
        tm.mday += DAY_TAB[is_leap(from.year) as usize][(from.mon - 1) as usize];
        tm.mon -= 1;
    }
    while tm.mon < 0 {
        tm.mon += MONTHS_PER_YEAR;
        tm.year -= 1;
    }
    if dt1 < dt2 {
        flip(&mut tm);
    }
    itm2interval(&tm).ok_or_else(|| out_of_range("interval"))
}

/// A double as PostgreSQL's `%g` writes it, `NaN` and `Infinity` for the special values, padded
/// with zeros after its sign to `width`.
fn printf_g(v: f64, width: usize) -> String {
    let (sign, body) = if v.is_nan() {
        ("", "NaN".to_string())
    } else {
        let sign = if v.is_sign_negative() { "-" } else { "" };
        let a = v.abs();
        let body = if a.is_infinite() {
            "Infinity".to_string()
        } else if a == 0.0 {
            "0".to_string()
        } else {
            let e = format!("{a:.5e}");
            let (mantissa, exp) = e.split_once('e').unwrap_or((&e, "0"));
            let x: i32 = exp.parse().unwrap_or(0);
            let trim = |s: String| {
                if s.contains('.') {
                    s.trim_end_matches('0').trim_end_matches('.').to_string()
                } else {
                    s
                }
            };
            if (-4..6).contains(&x) {
                trim(format!("{a:.*}", (5 - x) as usize))
            } else {
                format!(
                    "{}e{}{:02}",
                    trim(mantissa.to_string()),
                    if x < 0 { '-' } else { '+' },
                    x.abs()
                )
            }
        };
        (sign, body)
    };
    let mut pad = width.saturating_sub(body.len());
    let mut out = String::new();
    if pad > 0 {
        out.push_str(sign);
        pad -= sign.len();
        out.push_str(&"0".repeat(pad));
    } else {
        out.push_str(sign);
    }
    out.push_str(&body);
    out
}

/// PostgreSQL's float_time_overflows: an hour, a minute and a second past 24:00:00 or a field's
/// own range.
fn float_time_overflows(hour: i32, min: i32, sec: f64) -> bool {
    if !(0..=HOURS_PER_DAY).contains(&hour) || !(0..MINS_PER_HOUR).contains(&min) {
        return true;
    }
    if sec.is_nan() {
        return true;
    }
    let sec = (sec * USECS_PER_SEC as f64).round_ties_even();
    if sec < 0.0 || sec > (i64::from(SECS_PER_MINUTE) * USECS_PER_SEC) as f64 {
        return true;
    }
    i64::from((hour * MINS_PER_HOUR + min) * SECS_PER_MINUTE) * USECS_PER_SEC + sec as i64
        > MICROS_PER_DAY
}

/// A time's microseconds from its hour, minute and second, as make_time computes them.
fn clock_of(hour: i32, min: i32, sec: f64) -> i64 {
    i64::from((hour * MINS_PER_HOUR + min) * SECS_PER_MINUTE) * USECS_PER_SEC
        + (sec * USECS_PER_SEC as f64).round_ties_even() as i64
}

/// `make_date(year, month, day)`, a negative year BC.
pub fn make_date(year: i32, month: i32, day: i32) -> Result<i32> {
    let mut tm = Tm {
        year,
        mon: month,
        mday: day,
        ..Tm::default()
    };
    let field = |tm: &Tm| {
        Error::new(format!(
            "date field value out of range: {}-{:02}-{:02}",
            tm.year, tm.mon, tm.mday
        ))
    };
    let mut bc = false;
    if tm.year < 0 {
        bc = true;
        tm.year = tm.year.checked_neg().ok_or_else(|| field(&tm))?;
    }
    if validate_date(DATE_M, false, false, bc, &mut tm).is_err() {
        return Err(field(&tm));
    }
    let out = |tm: &Tm| {
        Error::new(format!(
            "date out of range: {}-{:02}-{:02}",
            tm.year, tm.mon, tm.mday
        ))
    };
    if !valid_julian(tm.year, tm.mon) {
        return Err(out(&tm));
    }
    let date = i64::from(date2j_c(tm.year, tm.mon, tm.mday)) - POSTGRES_EPOCH_JDATE;
    if !valid_date(date) {
        return Err(out(&tm));
    }
    Ok(date_from_pg(date as i32))
}

/// `make_time(hour, min, sec)`.
pub fn make_time(hour: i32, min: i32, sec: f64) -> Result<i64> {
    if float_time_overflows(hour, min, sec) {
        return Err(Error::new(format!(
            "time field value out of range: {hour}:{min:02}:{}",
            printf_g(sec, 2)
        )));
    }
    Ok(clock_of(hour, min, sec))
}

/// make_timestamp_internal, on PostgreSQL's count from 2000-01-01.
fn make_timestamp_pg(
    year: i32,
    month: i32,
    day: i32,
    hour: i32,
    min: i32,
    sec: f64,
) -> Result<i64> {
    let mut tm = Tm {
        year,
        mon: month,
        mday: day,
        ..Tm::default()
    };
    let mut bc = false;
    if tm.year < 0 {
        bc = true;
        tm.year = tm.year.wrapping_neg();
    }
    if validate_date(DATE_M, false, false, bc, &mut tm).is_err() {
        return Err(Error::new(format!(
            "date field value out of range: {year}-{month:02}-{day:02}"
        )));
    }
    if !valid_julian(tm.year, tm.mon) {
        return Err(Error::new(format!(
            "date out of range: {year}-{month:02}-{day:02}"
        )));
    }
    let date = i64::from(date2j_c(tm.year, tm.mon, tm.mday)) - POSTGRES_EPOCH_JDATE;
    if float_time_overflows(hour, min, sec) {
        return Err(Error::new(format!(
            "time field value out of range: {hour}:{min:02}:{}",
            printf_g(sec, 2)
        )));
    }
    let time = clock_of(hour, min, sec);
    let out = || {
        Error::new(format!(
            "timestamp out of range: {year}-{month:02}-{day:02} {hour}:{min:02}:{}",
            printf_g(sec, 2)
        ))
    };
    let r = date
        .checked_mul(MICROS_PER_DAY)
        .and_then(|r| r.checked_add(time))
        .ok_or_else(out)?;
    if !valid_timestamp(r) {
        return Err(out());
    }
    Ok(r)
}

/// `make_timestamp(year, month, mday, hour, min, sec)`, a negative year BC.
pub fn make_timestamp(
    year: i32,
    month: i32,
    mday: i32,
    hour: i32,
    min: i32,
    sec: f64,
) -> Result<i64> {
    instant_from_pg(make_timestamp_pg(year, month, mday, hour, min, sec)?)
}

/// PostgreSQL's parse_sane_timezone: a numeric zone (`+05`, `-03:30`), then an abbreviation, then
/// a zone's name, as seconds west of UTC at the date and time `tm` on that zone's clock.
fn sane_zone(tm: &Tm, zone: &str) -> Result<i32> {
    let name = zone_text(zone);
    if name.as_bytes().first().is_some_and(u8::is_ascii_digit) {
        return Err(Error::new(format!(
            "invalid input syntax for type numeric time zone: \"{name}\""
        )));
    }
    match decode_timezone(name.as_bytes()) {
        Ok(tz) => Ok(tz),
        Err(Dterr::TzdispOverflow) => Err(Error::new(format!(
            "numeric time zone \"{name}\" out of range"
        ))),
        Err(Dterr::BadFormat) => {
            let low = lowered(name);
            match decode_timezone_abbrev(low.as_bytes()) {
                Ok(Some((_, offset))) => Ok(-offset),
                Err(Dterr::Refused(e)) => Err(e),
                _ => zone_offset(name, tm).map_err(|e| match e {
                    Dterr::Refused(e) => e,
                    _ => Error::new(format!("time zone \"{name}\" not recognized")),
                }),
            }
        }
        Err(_) => Err(Error::new(format!("time zone \"{name}\" not recognized"))),
    }
}

/// `make_timestamptz(year, month, mday, hour, min, sec)` under TimeZone UTC, and with a seventh
/// argument, the zone its date and time are on.
pub fn make_timestamptz(
    year: i32,
    month: i32,
    mday: i32,
    hour: i32,
    min: i32,
    sec: f64,
    zone: Option<&str>,
) -> Result<i64> {
    let t = make_timestamp_pg(year, month, mday, hour, min, sec)?;
    let Some(zone) = zone else {
        return instant_from_pg(t);
    };
    let (tm, _) = pg_tm(t).ok_or_else(|| out_of_range("timestamp"))?;
    let tz = sane_zone(&tm, zone)?;
    let r = t
        .checked_add(i64::from(tz) * USECS_PER_SEC)
        .filter(|&r| valid_timestamp(r))
        .ok_or_else(|| out_of_range("timestamp"))?;
    instant_from_pg(r)
}

/// `make_interval(years, months, weeks, days, hours, mins, secs)`.
pub fn make_interval(
    years: i32,
    months: i32,
    weeks: i32,
    days: i32,
    hours: i32,
    mins: i32,
    secs: f64,
) -> Result<Interval> {
    let out = || out_of_range("interval");
    if secs.is_infinite() || secs.is_nan() {
        return Err(out());
    }
    let months = years
        .checked_mul(MONTHS_PER_YEAR)
        .and_then(|m| m.checked_add(months))
        .ok_or_else(out)?;
    let days = weeks
        .checked_mul(DAYS_PER_WEEK)
        .and_then(|d| d.checked_add(days))
        .ok_or_else(out)?;
    let time = i64::from(hours) * USECS_PER_HOUR + i64::from(mins) * USECS_PER_MINUTE;
    let product = secs * USECS_PER_SEC as f64;
    if product.is_infinite() {
        return Err(Error::new("value out of range: overflow"));
    }
    let secs = product.round_ties_even();
    if !(secs >= i64::MIN as f64 && secs < -(i64::MIN as f64)) {
        return Err(out());
    }
    let micros = time.checked_add(secs as i64).ok_or_else(out)?;
    let v = Interval {
        months,
        days,
        micros,
    };
    if !v.is_finite() {
        return Err(out());
    }
    Ok(v)
}

/// `date_bin(stride, source, origin)` of timestamps, and of timestamptz, the same: the start of
/// the stride `source` falls in, strides counted from `origin`.
pub fn timestamp_bin(stride: Interval, source: i64, origin: i64) -> Result<i64> {
    if source == i64::MIN || source == i64::MAX {
        return Ok(source);
    }
    if origin == i64::MIN || origin == i64::MAX {
        return Err(Error::new("origin out of range"));
    }
    if !stride.is_finite() {
        return Err(Error::new(
            "timestamps cannot be binned into infinite intervals",
        ));
    }
    if stride.months != 0 {
        return Err(Error::new(
            "timestamps cannot be binned into intervals containing months or years",
        ));
    }
    let stride_usecs = i64::from(stride.days)
        .checked_mul(MICROS_PER_DAY)
        .and_then(|s| s.checked_add(stride.micros))
        .ok_or_else(|| out_of_range("interval"))?;
    if stride_usecs <= 0 {
        return Err(Error::new("stride must be greater than zero"));
    }
    let (t, o) = (instant_to_pg(source), instant_to_pg(origin));
    let diff = t.checked_sub(o).ok_or_else(|| out_of_range("interval"))?;
    let modulo = diff % stride_usecs;
    let mut r = o + (diff - modulo);
    if modulo < 0 {
        r = r
            .checked_sub(stride_usecs)
            .filter(|&r| valid_timestamp(r))
            .ok_or_else(|| out_of_range("timestamp"))?;
    }
    instant_from_pg(r)
}

/// `to_timestamp(double precision)`: seconds since 1970-01-01 UTC as an instant, NaN refused and
/// the infinities kept.
pub fn float8_timestamptz(f: f64) -> Result<i64> {
    if f.is_nan() {
        return Err(Error::new("timestamp cannot be NaN"));
    }
    if f.is_infinite() {
        return Ok(if f < 0.0 { i64::MIN } else { i64::MAX });
    }
    let out = || Error::new(format!("timestamp out of range: \"{}\"", printf_g(f, 0)));
    let low = SECS_PER_DAY as f64 * (0 - UNIX_EPOCH_JDATE) as f64;
    let high = SECS_PER_DAY as f64 * (TIMESTAMP_END_JULIAN - UNIX_EPOCH_JDATE) as f64;
    if f < low || f >= high {
        return Err(out());
    }
    let seconds = f - ((POSTGRES_EPOCH_JDATE - UNIX_EPOCH_JDATE) * SECS_PER_DAY) as f64;
    let r = (seconds * USECS_PER_SEC as f64).round_ties_even() as i64;
    if !valid_timestamp(r) {
        return Err(out());
    }
    instant_from_pg(r)
}

/// `(s1, e1) OVERLAPS (s2, e2)` on values of one type, each an end that may be NULL, as the SQL
/// standard has it and PostgreSQL computes it; none where the answer is NULL.
pub fn overlaps<T: Ord + Copy>(
    s1: Option<T>,
    e1: Option<T>,
    s2: Option<T>,
    e2: Option<T>,
) -> Option<bool> {
    let ordered = |s: Option<T>, e: Option<T>| -> Option<(T, Option<T>)> {
        match (s, e) {
            (None, None) => None,
            (None, Some(e)) => Some((e, None)),
            (Some(s), None) => Some((s, None)),
            (Some(s), Some(e)) => Some(if s > e { (e, Some(s)) } else { (s, Some(e)) }),
        }
    };
    let (ts1, te1) = ordered(s1, e1)?;
    let (ts2, te2) = ordered(s2, e2)?;
    match ts1.cmp(&ts2) {
        std::cmp::Ordering::Greater => {
            let te2 = te2?;
            if ts1 < te2 {
                return Some(true);
            }
            te1?;
            Some(false)
        }
        std::cmp::Ordering::Less => {
            let te1 = te1?;
            if ts2 < te1 {
                return Some(true);
            }
            te2?;
            Some(false)
        }
        std::cmp::Ordering::Equal => {
            te1?;
            te2?;
            Some(true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> i64 {
        timestamp_in(s, None, 0).unwrap()
    }

    fn date(s: &str) -> i32 {
        date_in(s, 0).unwrap()
    }

    fn interval(s: &str) -> Interval {
        interval_in(s, INTERVAL_FULL_RANGE, None).unwrap()
    }

    fn refusal<T: std::fmt::Debug>(r: Result<T>) -> String {
        r.unwrap_err().0
    }

    #[test]
    fn extract_gives_numerics_and_date_part_doubles_as_postgresql_computes_each() {
        let t = ts("2024-03-05 12:00:00.5");
        assert_eq!(
            extract_timestamp("julian", t).unwrap().unwrap().to_string(),
            "2460375.50000578703703703704"
        );
        assert_eq!(
            date_part_timestamp("JULIAN", t).unwrap(),
            Some(2460375.5000057872)
        );
        let v = interval("1 year 1 month 1 day 00:00:01.5");
        assert_eq!(
            extract_interval("epoch", v).unwrap().unwrap().to_string(),
            "34236001.500000"
        );
        assert_eq!(date_part_interval("epoch", v).unwrap(), Some(34236001.5));
        assert_eq!(
            extract_date("isoyear", date("2021-01-03"))
                .unwrap()
                .unwrap()
                .to_string(),
            "2020"
        );
        assert_eq!(
            extract_date("week", date("2024-12-30"))
                .unwrap()
                .unwrap()
                .to_string(),
            "1"
        );
        assert_eq!(
            extract_date("century", date("0001-12-31 BC"))
                .unwrap()
                .unwrap()
                .to_string(),
            "-1"
        );
        assert_eq!(
            extract_timestamp("decade", ts("0010-06-01 BC"))
                .unwrap()
                .unwrap()
                .to_string(),
            "-1"
        );
        assert_eq!(
            extract_interval("quarter", interval("-13 months"))
                .unwrap()
                .unwrap()
                .to_string(),
            "-1"
        );
        let (time, zone) = timetz_in("10:00:00-05:30", None, 0).unwrap();
        assert_eq!(
            extract_timetz("timezone_minute", time, zone)
                .unwrap()
                .unwrap()
                .to_string(),
            "-30"
        );
        assert_eq!(
            extract_timetz("epoch", time, zone)
                .unwrap()
                .unwrap()
                .to_string(),
            "55800.000000"
        );
        let clock = time_in("12:34:56.789012", None, 0).unwrap();
        assert_eq!(
            date_part_time("millisecond", clock).unwrap(),
            Some(56789.012)
        );
        assert_eq!(
            date_part_date("hour", date("2024-01-01")).unwrap(),
            Some(0.0)
        );
    }

    #[test]
    fn an_infinity_s_part_is_null_or_an_infinity_and_a_unit_is_refused_as_postgresql_refuses_it() {
        assert_eq!(
            extract_timestamptz("year", i64::MAX)
                .unwrap()
                .unwrap()
                .to_string(),
            "Infinity"
        );
        assert!(extract_date("month", i32::MIN).unwrap().is_none());
        assert_eq!(
            refusal(extract_date("hour", i32::MIN)),
            "unit \"hour\" not supported for type date"
        );
        assert_eq!(
            refusal(extract_timestamp("Foo", 0)),
            "unit \"foo\" not recognized for type timestamp without time zone"
        );
        assert_eq!(
            refusal(extract_timestamp("timezone", 0)),
            "unit \"timezone\" not supported for type timestamp without time zone"
        );
        assert_eq!(
            extract_timestamptz("timezone", 0)
                .unwrap()
                .unwrap()
                .to_string(),
            "0"
        );
        assert_eq!(
            refusal(timestamp_trunc("epoch", 0)),
            "unit \"epoch\" not recognized for type timestamp without time zone"
        );
        assert_eq!(
            refusal(interval_trunc("week", interval("1 month"))),
            "unit \"week\" not supported for type interval"
        );
    }

    #[test]
    fn date_trunc_and_age_answer_as_postgresql_does() {
        assert_eq!(
            timestamp_out(timestamp_trunc("week", ts("2021-01-01 10:00")).unwrap()),
            "2020-12-28 00:00:00"
        );
        assert_eq!(
            timestamp_out(timestamp_trunc("millennium", ts("2001-01-01")).unwrap()),
            "2001-01-01 00:00:00"
        );
        assert_eq!(
            timestamp_out(timestamp_trunc("decade", ts("0009-01-01 BC")).unwrap()),
            "0011-01-01 00:00:00 BC"
        );
        assert_eq!(
            interval_out(interval_trunc("quarter", interval("1 year 11 months 3 days")).unwrap()),
            "1 year 9 mons"
        );
        assert_eq!(
            interval_out(timestamp_age(ts("2024-03-01"), ts("2024-01-31 12:00")).unwrap()),
            "1 mon 12:00:00"
        );
        assert_eq!(
            interval_out(timestamp_age(ts("2024-01-31 12:00"), ts("2024-03-01")).unwrap()),
            "-1 mons -12:00:00"
        );
        assert_eq!(timestamp_age(i64::MAX, 0).unwrap(), Interval::INFINITY);
        assert_eq!(
            refusal(timestamp_age(i64::MIN, i64::MIN)),
            "interval out of range"
        );
        assert_eq!(timestamptz_trunc_zone("day", 0, "UTC").unwrap(), 0);
        assert_eq!(
            timestamptz_trunc_zone("hour", 1_800_000_000, "+05").unwrap(),
            0
        );
        assert!(refusal(timestamptz_trunc_zone("day", 0, "PST")).contains("abbreviations of UTC"));
    }

    #[test]
    fn the_constructors_round_and_refuse_as_postgresql_does() {
        assert_eq!(date_out(make_date(-44, 3, 15).unwrap()), "0044-03-15 BC");
        assert_eq!(
            refusal(make_date(2023, 2, 29)),
            "date field value out of range: 2023-02-29"
        );
        assert_eq!(
            time_out(make_time(23, 59, 59.999_999_5).unwrap()),
            "24:00:00"
        );
        assert_eq!(time_out(make_time(24, 0, 0.000_000_5).unwrap()), "24:00:00");
        assert_eq!(
            refusal(make_time(24, 0, 0.000_001)),
            "time field value out of range: 24:00:1e-06"
        );
        assert_eq!(
            timestamp_out(make_timestamp(2024, 2, 29, 24, 0, 0.0).unwrap()),
            "2024-03-01 00:00:00"
        );
        assert_eq!(
            refusal(make_timestamptz(2024, 1, 1, 0, 0, 0.0, Some("5"))),
            "invalid input syntax for type numeric time zone: \"5\""
        );
        assert_eq!(
            timestamptz_out(make_timestamptz(2024, 1, 1, 0, 0, 0.0, Some("-03:30")).unwrap()),
            "2024-01-01 03:30:00+00"
        );
        assert_eq!(
            interval_out(make_interval(1, 2, 3, 4, 5, 6, 7.5).unwrap()),
            "1 year 2 mons 25 days 05:06:07.5"
        );
        assert_eq!(
            refusal(make_interval(0, 0, 0, 0, 0, 0, 1.8e303)),
            "value out of range: overflow"
        );
        assert_eq!(
            refusal(make_interval(i32::MAX, 0, 0, 0, 0, 0, 0.0)),
            "interval out of range"
        );
    }

    #[test]
    fn date_bin_to_timestamp_and_overlaps_answer_as_postgresql_does() {
        let stride = interval("15 minutes");
        assert_eq!(
            timestamp_out(
                timestamp_bin(stride, ts("2024-01-01 10:07:00"), ts("2000-01-01")).unwrap()
            ),
            "2024-01-01 10:00:00"
        );
        assert_eq!(
            timestamp_out(
                timestamp_bin(stride, ts("1999-12-31 23:53:00"), ts("2000-01-01")).unwrap()
            ),
            "1999-12-31 23:45:00"
        );
        assert_eq!(
            refusal(timestamp_bin(interval("1 month"), 0, 0)),
            "timestamps cannot be binned into intervals containing months or years"
        );
        assert_eq!(
            timestamptz_out(float8_timestamptz(1.5e-6).unwrap()),
            "1970-01-01 00:00:00.000002+00"
        );
        assert_eq!(
            refusal(float8_timestamptz(f64::NAN)),
            "timestamp cannot be NaN"
        );
        assert_eq!(
            refusal(float8_timestamptz(1e300)),
            "timestamp out of range: \"1e+300\""
        );
        assert_eq!(float8_timestamptz(f64::NEG_INFINITY).unwrap(), i64::MIN);
        assert_eq!(overlaps(Some(1), Some(3), Some(2), Some(4)), Some(true));
        assert_eq!(overlaps(Some(3), Some(1), Some(3), None), None);
        assert_eq!(overlaps(Some(1), None, Some(2), Some(4)), None);
        assert_eq!(overlaps(Some(5), None, Some(2), Some(4)), None);
        assert_eq!(overlaps(Some(5), Some(6), Some(2), Some(4)), Some(false));
        assert_eq!(overlaps::<i64>(None, None, Some(2), Some(4)), None);
    }
}
