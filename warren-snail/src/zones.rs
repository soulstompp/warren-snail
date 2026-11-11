//! Zones other than UTC, read only under the feature `zones`: without it, a zone other than UTC
//! is refused by name.
//!
//! A zone is read as PostgreSQL reads one: its compiled file found in a directory by a name matched
//! without regard to case, its transitions and their local time types, and past the last of them
//! the rule its footer writes; or, where no file has the name, the name itself read as a POSIX TZ
//! string. `GMT` is always read as its own name, never from a file.

// Without the feature only the names that need no file are read, and the rest stands unused.
#![cfg_attr(not(feature = "zones"), allow(dead_code))]

use crate::{Error, Result};

/// The refusal of a zone other than UTC by a build without the feature `zones`.
#[cfg_attr(feature = "zones", allow(dead_code))]
fn refused(name: &str) -> Error {
    Error::new(format!(
        "time zone \"{name}\" is a zone other than UTC, read only under the feature zones"
    ))
}

/// The refusal of a name PostgreSQL finds no zone for.
fn unknown(name: &str) -> Error {
    Error::new(format!("time zone \"{name}\" not recognized"))
}

/// The offset east of UTC, in seconds, that zone `name` gives a time `local` seconds after
/// 1970-01-01 00:00 on its own clock, as PostgreSQL resolves a local time: in the hour that never
/// happens, the offset before the change; in the hour that happens twice, the offset after it.
pub(crate) fn local_offset(name: &str, local: i64) -> Result<i32> {
    #[cfg(feature = "zones")]
    {
        let zone = read::zone(name)?;
        Ok(zone.state.local_offset(local) as i32)
    }
    #[cfg(not(feature = "zones"))]
    {
        let _ = local;
        fixed_offset(name).ok_or_else(|| refused(name))
    }
}

/// The offset east of UTC, in seconds, that zone `name` gives the instant `utc` seconds after
/// 1970-01-01 00:00 UTC.
pub(crate) fn utc_offset(name: &str, utc: i64) -> Result<i32> {
    #[cfg(feature = "zones")]
    {
        let zone = read::zone(name)?;
        zone.state
            .utc_offset(utc)
            .map(|o| o as i32)
            .ok_or_else(|| Error::new("timestamp out of range"))
    }
    #[cfg(not(feature = "zones"))]
    {
        let _ = utc;
        fixed_offset(name).ok_or_else(|| refused(name))
    }
}

/// The names of the zone files whose zone is UTC itself, with no change ever: read so without a
/// file. Each is spelled as PostgreSQL's directory spells it, beside the abbreviation its file
/// gives.
const UTC_NAMES: [(&str, &str); 17] = [
    ("UTC", "UTC"),
    ("Etc/UTC", "UTC"),
    ("UCT", "UTC"),
    ("Etc/UCT", "UTC"),
    ("Universal", "UTC"),
    ("Etc/Universal", "UTC"),
    ("Zulu", "UTC"),
    ("Etc/Zulu", "UTC"),
    ("Etc/GMT", "GMT"),
    ("GMT0", "GMT"),
    ("GMT+0", "GMT"),
    ("GMT-0", "GMT"),
    ("Etc/GMT0", "GMT"),
    ("Etc/GMT+0", "GMT"),
    ("Etc/GMT-0", "GMT"),
    ("Greenwich", "GMT"),
    ("Etc/Greenwich", "GMT"),
];

/// A name of UTC itself, as the directory spells it, and its abbreviation.
fn utc_named(name: &str) -> Option<(&'static str, &'static str)> {
    if name.eq_ignore_ascii_case("GMT") {
        return Some(("GMT", "GMT"));
    }
    UTC_NAMES
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .copied()
}

/// The offset east of UTC, in seconds, of a name that needs no zone file: `GMT`, which PostgreSQL
/// reads without one, a name of UTC itself, and a POSIX TZ string whose zone never changes (`UTC+3`,
/// `<+03>-03`, `+02`, `EST5`); none for any other.
pub fn fixed_offset(name: &str) -> Option<i32> {
    fixed(name).map(|(east, _)| east)
}

/// A name that needs no zone file, as `fixed_offset` reads it, with the abbreviation its zone
/// gives.
fn fixed(name: &str) -> Option<(i32, String)> {
    if name.len() > TZ_STRLEN_MAX {
        return None;
    }
    if let Some((_, abbrev)) = utc_named(name) {
        return Some((0, abbrev.to_string()));
    }
    let upper = name.to_ascii_uppercase();
    if upper.starts_with(':') {
        return None;
    }
    let st = State::parse(upper.as_bytes(), false)?;
    match (st.ttis.as_slice(), st.ats.len()) {
        ([one], 0) => Some((
            one.utoff as i32,
            String::from_utf8_lossy(cstr(&st.chars, one.desigidx)).into_owned(),
        )),
        _ => None,
    }
}

/// The zone a session's TimeZone is set to, as PostgreSQL's pg_tzset finds it: its name as SHOW
/// writes it back, the directory's spelling where a file has the name, a POSIX TZ string's upper
/// case where none does; none where no zone has the name. A zone keeping leap seconds is refused in
/// PostgreSQL's words, and a build without the feature `zones` refuses by name any name it cannot
/// read without a file.
pub(crate) fn set_name(name: &str) -> Result<Option<String>> {
    if name.is_empty() || name.len() > TZ_STRLEN_MAX {
        return Ok(None);
    }
    #[cfg(feature = "zones")]
    {
        let Ok(zone) = read::zone(name) else {
            return Ok(None);
        };
        // PostgreSQL's pg_tz_acceptable: midnight UTC of 2000-01-01 must fall on a whole minute.
        let t2000 = 946_684_800;
        if zone
            .state
            .utc_offset(t2000)
            .is_none_or(|east| (t2000 + east).rem_euclid(60) != 0)
        {
            return Err(Error::new(format!(
                "time zone \"{name}\" appears to use leap seconds: PostgreSQL does not support \
                 leap seconds."
            )));
        }
        Ok(Some(zone.canonical.clone()))
    }
    #[cfg(not(feature = "zones"))]
    {
        if let Some((spelled, _)) = utc_named(name.strip_prefix(':').unwrap_or(name)) {
            return Ok(Some(spelled.to_string()));
        }
        match fixed(name) {
            Some(_) => Ok(Some(name.to_ascii_uppercase())),
            None => Err(refused(name)),
        }
    }
}

/// The zone PostgreSQL's pg_tzset_offset names for an offset of `west` seconds west of UTC: a
/// POSIX TZ string of that offset alone, `<+05:30>-05:30`; none where PostgreSQL reads no such
/// string.
pub(crate) fn offset_name(west: i64) -> Option<String> {
    let abs = west.unsigned_abs();
    let mut text = format!("{:02}", abs / 3600);
    let rest = abs % 3600;
    if rest != 0 {
        text.push_str(&format!(":{:02}", rest / 60));
        if !rest.is_multiple_of(60) {
            text.push_str(&format!(":{:02}", rest % 60));
        }
    }
    let name = match west > 0 {
        true => format!("<-{text}>+{text}"),
        false => format!("<+{text}>-{text}"),
    };
    fixed(&name).map(|_| name)
}

/// The zone a statement reads under, its session's TimeZone: none for UTC.
#[derive(Clone, Debug)]
pub(crate) enum Session {
    /// A zone that never changes: its offset east of UTC and its abbreviation.
    Fixed(i32, String),
    /// A zone read from its file, or a POSIX TZ string with its rules, by its name.
    Named(String),
}

std::thread_local! {
    static SESSION: std::cell::RefCell<Option<Session>> = const { std::cell::RefCell::new(None) };
    /// Whether a statement reads its zone's own abbreviations before PostgreSQL's set, as
    /// PostgreSQL does from 18 on.
    static ZONE_FIRST: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
}

/// The zone a statement reads under, held while it runs; the one before it put back after, and
/// how it read abbreviations, where it says.
pub(crate) struct InZone(Option<Session>, Option<bool>);

impl InZone {
    /// The zone held for a statement of release `release`, whose own abbreviations are read before
    /// PostgreSQL's set from 18 on.
    pub(crate) fn of_release(mut self, release: u8) -> InZone {
        self.1 = Some(ZONE_FIRST.with(|f| f.replace(release >= 18)));
        self
    }
}

/// Whether the statement reads its zone's own abbreviations before PostgreSQL's set.
pub(crate) fn zone_abbreviations_first() -> bool {
    ZONE_FIRST.with(|f| f.get())
}

/// Holds the zone a session's TimeZone names, as SHOW writes it, for a statement: UTC where none.
pub(crate) fn in_zone(name: Option<&str>) -> InZone {
    let session = name.filter(|n| *n != "UTC").and_then(|n| match fixed(n) {
        Some((0, abbrev)) if abbrev == "UTC" => None,
        Some((east, abbrev)) => Some(Session::Fixed(east, abbrev)),
        None => Some(Session::Named(n.to_string())),
    });
    handed(session)
}

/// Holds a zone a statement reads under on a thread it runs a part of on.
pub(crate) fn handed(session: Option<Session>) -> InZone {
    InZone(SESSION.with(|s| s.replace(session)), None)
}

impl Drop for InZone {
    fn drop(&mut self) {
        SESSION.with(|s| *s.borrow_mut() = self.0.take());
        if let Some(first) = self.1 {
            ZONE_FIRST.with(|f| f.set(first));
        }
    }
}

/// The zone the statement reads under, to hand to a thread it runs a part of on.
pub(crate) fn session() -> Option<Session> {
    SESSION.with(|s| s.borrow().clone())
}

/// Whether the statement reads under UTC itself.
pub(crate) fn session_is_utc() -> bool {
    SESSION.with(|s| s.borrow().is_none())
}

/// The offset east of UTC, in seconds, that the statement's zone gives the instant `utc` seconds
/// after 1970-01-01 00:00 UTC.
pub(crate) fn session_utc_offset(utc: i64) -> Result<i32> {
    SESSION.with(|s| match &*s.borrow() {
        None => Ok(0),
        Some(Session::Fixed(east, _)) => Ok(*east),
        Some(Session::Named(n)) => utc_offset(n, utc),
    })
}

/// The offset east of UTC, in seconds, that the statement's zone gives a time `local` seconds after
/// 1970-01-01 00:00 on its own clock, as PostgreSQL's DetermineTimeZoneOffset resolves a local
/// time: UTC for a day outside the Julian days PostgreSQL counts.
pub(crate) fn session_local_offset(local: i64) -> Result<i32> {
    SESSION.with(|s| match &*s.borrow() {
        None => Ok(0),
        Some(Session::Fixed(east, _)) => {
            let day = local.div_euclid(SECSPERDAY);
            Ok(match (FIRST_JULIAN_DAY..END_JULIAN_DAY).contains(&day) {
                true => *east,
                false => 0,
            })
        }
        Some(Session::Named(n)) => local_offset(n, local),
    })
}

/// The abbreviation the statement's zone gives the instant `utc` seconds after 1970-01-01 00:00
/// UTC, as to_char's TZ writes it.
pub(crate) fn session_abbrev(utc: i64) -> Result<String> {
    SESSION.with(|s| match &*s.borrow() {
        None => Ok("UTC".to_string()),
        Some(Session::Fixed(_, abbrev)) => Ok(abbrev.clone()),
        Some(Session::Named(n)) => abbrev_at(n, utc),
    })
}

/// The abbreviation zone `name` gives the instant `utc` seconds after 1970-01-01 00:00 UTC.
fn abbrev_at(name: &str, utc: i64) -> Result<String> {
    #[cfg(feature = "zones")]
    {
        let zone = read::zone(name)?;
        zone.state
            .abbrev(utc)
            .ok_or_else(|| Error::new("timestamp out of range"))
    }
    #[cfg(not(feature = "zones"))]
    {
        let _ = utc;
        fixed(name).map(|(_, a)| a).ok_or_else(|| refused(name))
    }
}

/// A zone that never changes, as its one local time type and abbreviation.
fn fixed_state(east: i32, abbrev: &str) -> State {
    let mut chars = abbrev.as_bytes().to_vec();
    chars.extend([0, 0]);
    State {
        ttis: vec![Ttinfo::new(i64::from(east), false, 0)],
        chars,
        ..State::default()
    }
}

/// Zone `name`'s rules, as `f` reads them: its file's under the feature `zones`; without it, those
/// of a name that needs no file alone.
fn with_state<T>(name: &str, f: impl FnOnce(&State) -> T) -> Result<T> {
    #[cfg(feature = "zones")]
    {
        let zone = read::zone(name)?;
        Ok(f(&zone.state))
    }
    #[cfg(not(feature = "zones"))]
    {
        match fixed(name) {
            Some((east, abbrev)) => Ok(f(&fixed_state(east, &abbrev))),
            None => Err(refused(name)),
        }
    }
}

/// The statement zone's rules, as `f` reads them: UTC's where it reads under UTC.
fn with_session_state<T>(f: impl FnOnce(&State) -> T) -> Result<T> {
    match session() {
        None => Ok(f(&fixed_state(0, "UTC"))),
        Some(Session::Fixed(east, abbrev)) => Ok(f(&fixed_state(east, &abbrev))),
        Some(Session::Named(n)) => with_state(&n, f),
    }
}

/// Whether the statement's zone gives the abbreviation `abbrev`, in upper case, to any of its local
/// time types, and where all give it one meaning, that meaning: its offset east of UTC and whether
/// it is daylight time (PostgreSQL's pg_timezone_abbrev_is_known); none where it gives it none.
pub(crate) fn session_abbrev_known(abbrev: &str) -> Result<Option<Option<(i32, bool)>>> {
    with_session_state(|st| {
        st.abbrev_known(abbrev.as_bytes())
            .map(|m| m.map(|(east, dst)| (east as i32, dst)))
    })
}

/// Whether zone `name` keeps daylight time at the instant `utc` seconds after 1970-01-01 00:00 UTC.
pub(crate) fn dst_at(name: &str, utc: i64) -> Result<bool> {
    with_state(name, |st| st.type_at(utc).is_some_and(|(i, _)| st.ttis[i].isdst))
}

/// The statement zone's name, where it is a zone read by its rules.
pub(crate) fn session_named() -> Option<String> {
    match session() {
        Some(Session::Named(n)) => Some(n),
        _ => None,
    }
}

/// The meaning zone `name` gives the abbreviation `abbrev`, in upper case, at the instant `utc`
/// seconds after 1970-01-01 00:00 UTC (PostgreSQL's pg_interpret_timezone_abbrev); none where it
/// gives it none.
pub(crate) fn abbrev_meaning(name: &str, abbrev: &str, utc: i64) -> Result<Option<(i32, bool)>> {
    with_state(name, |st| {
        st.abbrev_meaning(abbrev.as_bytes(), utc)
            .map(|(east, dst)| (east as i32, dst))
    })
}

/// Each abbreviation of letters alone the statement's zone gives, with its meaning at the instant
/// `utc`, in the order its list holds them (PostgreSQL's pg_timezone_abbrevs_zone).
pub(crate) fn session_abbrevs_at(utc: i64) -> Result<Vec<(String, i32, bool)>> {
    with_session_state(|st| {
        st.abbrevs()
            .into_iter()
            .filter(|a| a.bytes().all(|b| b.is_ascii_uppercase()))
            .filter_map(|a| {
                let (east, dst) = st.abbrev_meaning(a.as_bytes(), utc)?;
                Some((a, east as i32, dst))
            })
            .collect()
    })
}

/// Every zone the directory holds as PostgreSQL's pg_tzenumerate walks it, each with its
/// abbreviation, offset east of UTC and daylight time at the instant `utc`: a file read as a zone,
/// by its path below the directory, one keeping leap seconds or of an abbreviation past 31 bytes
/// left out. Read only under the feature `zones`.
pub(crate) fn every_zone_at(utc: i64) -> Result<Vec<(String, String, i32, bool)>> {
    #[cfg(feature = "zones")]
    {
        read::every_zone_at(utc)
    }
    #[cfg(not(feature = "zones"))]
    {
        let _ = utc;
        Err(Error::new(
            "pg_timezone_names lists the zones compiled beside a server, read only under the \
             feature zones",
        ))
    }
}

/// The longest zone name PostgreSQL reads.
const TZ_STRLEN_MAX: usize = 255;
/// The most transitions, local time types, abbreviation bytes and leap seconds a zone holds.
const TZ_MAX_TIMES: usize = 2000;
const TZ_MAX_TYPES: usize = 256;
const TZ_MAX_CHARS: usize = 50;
const TZ_MAX_LEAPS: usize = 50;

const SECSPERMIN: i64 = 60;
const MINSPERHOUR: i64 = 60;
const SECSPERHOUR: i64 = 3600;
const HOURSPERDAY: i64 = 24;
const SECSPERDAY: i64 = 86_400;
const DAYSPERWEEK: i64 = 7;
const DAYSPERNYEAR: i64 = 365;
const DAYSPERLYEAR: i64 = 366;
const MONSPERYEAR: i64 = 12;
const EPOCH_YEAR: i64 = 1970;
const YEARSPERREPEAT: i64 = 400;
const AVGSECSPERYEAR: i64 = 31_556_952;
const SECSPERREPEAT: i64 = YEARSPERREPEAT * AVGSECSPERYEAR;

/// The rules a POSIX TZ string gives when it names a daylight zone and no rules: the United
/// States' since 2007.
const TZDEFRULESTRING: &[u8] = b",M3.2.0,M11.1.0";

const MON_LENGTHS: [[i64; 12]; 2] = [
    [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31],
    [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31],
];
const YEAR_LENGTHS: [i64; 2] = [DAYSPERNYEAR, DAYSPERLYEAR];

fn isleap(y: i64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

/// A local time type: its offset east of UTC, whether it is daylight time, where its abbreviation
/// lies, and whether its transitions were given in standard time or in UTC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Ttinfo {
    utoff: i64,
    isdst: bool,
    desigidx: usize,
    ttisstd: bool,
    ttisut: bool,
}

impl Ttinfo {
    fn new(utoff: i64, isdst: bool, desigidx: usize) -> Ttinfo {
        Ttinfo {
            utoff,
            isdst,
            desigidx,
            ttisstd: false,
            ttisut: false,
        }
    }
}

/// A leap second: when it falls, and the seconds corrected from then on.
#[derive(Clone, Copy, Debug)]
struct Leap {
    trans: i64,
    corr: i64,
}

/// A zone as PostgreSQL holds it: its transitions in UTC seconds, the type each begins, the types,
/// their abbreviations, the leap seconds, whether times before the first or after the last repeat
/// every 400 years, and the type of a time before the first.
#[derive(Clone, Debug, Default)]
struct State {
    ats: Vec<i64>,
    types: Vec<u8>,
    ttis: Vec<Ttinfo>,
    /// Each abbreviation ending in a NUL byte, and one NUL after the last.
    chars: Vec<u8>,
    leaps: Vec<Leap>,
    goback: bool,
    goahead: bool,
    defaulttype: usize,
}

/// The text at `at` of NUL-ended abbreviations: up to the next NUL.
fn cstr(chars: &[u8], at: usize) -> &[u8] {
    let rest = chars.get(at..).unwrap_or(&[]);
    &rest[..rest.iter().position(|&b| b == 0).unwrap_or(rest.len())]
}

/// `t + j`, none where it would leave 64 bits.
fn incremented(t: i64, j: i64) -> Option<i64> {
    t.checked_add(j)
}

/// A POSIX rule's day: a Julian day of a year without February 29, a day of the year counted
/// from 0, or the `n`th weekday `d` of a month.
#[derive(Clone, Copy, Debug)]
enum Rule {
    JulianDay(i64, i64),
    DayOfYear(i64, i64),
    MonthNthDayOfWeek {
        mon: i64,
        week: i64,
        day: i64,
        time: i64,
    },
}

impl Rule {
    fn time(self) -> i64 {
        match self {
            Rule::JulianDay(_, t) | Rule::DayOfYear(_, t) => t,
            Rule::MonthNthDayOfWeek { time, .. } => time,
        }
    }

    /// The seconds after January 1 00:00 UTC of `year` at which the rule takes effect, its
    /// clock offset `offset` west of UTC.
    fn transtime(self, year: i64, offset: i64) -> i64 {
        let leapyear = isleap(year) as usize;
        let value = match self {
            Rule::JulianDay(day, _) => {
                let mut v = (day - 1) * SECSPERDAY;
                if leapyear == 1 && day >= 60 {
                    v += SECSPERDAY;
                }
                v
            }
            Rule::DayOfYear(day, _) => day * SECSPERDAY,
            Rule::MonthNthDayOfWeek { mon, week, day, .. } => {
                // Zeller's congruence gives the weekday of the month's first day.
                let m1 = (mon + 9) % 12 + 1;
                let yy0 = if mon <= 2 { year - 1 } else { year };
                let yy1 = yy0 / 100;
                let yy2 = yy0 % 100;
                let mut dow = ((26 * m1 - 2) / 10 + 1 + yy2 + yy2 / 4 + yy1 / 4 - 2 * yy1) % 7;
                if dow < 0 {
                    dow += DAYSPERWEEK;
                }
                let mut d = day - dow;
                if d < 0 {
                    d += DAYSPERWEEK;
                }
                for _ in 1..week {
                    if d + DAYSPERWEEK >= MON_LENGTHS[leapyear][mon as usize - 1] {
                        break;
                    }
                    d += DAYSPERWEEK;
                }
                let before: i64 = MON_LENGTHS[leapyear][..(mon - 1) as usize].iter().sum();
                (d + before) * SECSPERDAY
            }
        };
        value + self.time() + offset
    }
}

/// Reads a POSIX TZ string as PostgreSQL's copy of the IANA code reads it.
struct Posix<'a> {
    s: &'a [u8],
    at: usize,
}

impl<'a> Posix<'a> {
    fn peek(&self) -> u8 {
        self.s.get(self.at).copied().unwrap_or(0)
    }

    fn done(&self) -> bool {
        self.peek() == 0
    }

    /// An abbreviation unquoted: up to a digit, a comma, a sign or the end.
    fn zname(&mut self) -> &'a [u8] {
        let from = self.at;
        while !matches!(self.peek(), 0 | b'0'..=b'9' | b',' | b'-' | b'+') {
            self.at += 1;
        }
        &self.s[from..self.at]
    }

    /// An abbreviation in angle brackets, the opening one read: up to `>`, read too.
    fn qzname(&mut self) -> Option<&'a [u8]> {
        let from = self.at;
        while !matches!(self.peek(), 0 | b'>') {
            self.at += 1;
        }
        if self.peek() != b'>' {
            return None;
        }
        let name = &self.s[from..self.at];
        self.at += 1;
        Some(name)
    }

    fn num(&mut self, min: i64, max: i64) -> Option<i64> {
        if !self.peek().is_ascii_digit() {
            return None;
        }
        let mut n = 0i64;
        while self.peek().is_ascii_digit() {
            n = n * 10 + i64::from(self.peek() - b'0');
            if n > max {
                return None;
            }
            self.at += 1;
        }
        (n >= min).then_some(n)
    }

    /// `hh[:mm[:ss]]` as seconds; the hour up to a week's, as some rules write a time past the day.
    fn secs(&mut self) -> Option<i64> {
        let mut secs = self.num(0, HOURSPERDAY * DAYSPERWEEK - 1)? * SECSPERHOUR;
        if self.peek() == b':' {
            self.at += 1;
            secs += self.num(0, MINSPERHOUR - 1)? * SECSPERMIN;
            if self.peek() == b':' {
                self.at += 1;
                secs += self.num(0, SECSPERMIN)?;
            }
        }
        Some(secs)
    }

    /// `[+-]hh[:mm[:ss]]` as seconds, `-` before a negative one.
    fn offset(&mut self) -> Option<i64> {
        let neg = match self.peek() {
            b'-' => {
                self.at += 1;
                true
            }
            b'+' => {
                self.at += 1;
                false
            }
            _ => false,
        };
        let secs = self.secs()?;
        Some(if neg { -secs } else { secs })
    }

    /// A rule, `date[/time]`, its time 02:00 where it gives none.
    fn rule(&mut self) -> Option<Rule> {
        let kind = self.peek();
        let rule = match kind {
            b'J' => {
                self.at += 1;
                Rule::JulianDay(self.num(1, DAYSPERNYEAR)?, 0)
            }
            b'M' => {
                self.at += 1;
                let mon = self.num(1, MONSPERYEAR)?;
                if self.peek() != b'.' {
                    return None;
                }
                self.at += 1;
                let week = self.num(1, 5)?;
                if self.peek() != b'.' {
                    return None;
                }
                self.at += 1;
                let day = self.num(0, DAYSPERWEEK - 1)?;
                Rule::MonthNthDayOfWeek {
                    mon,
                    week,
                    day,
                    time: 0,
                }
            }
            b'0'..=b'9' => Rule::DayOfYear(self.num(0, DAYSPERLYEAR - 1)?, 0),
            _ => return None,
        };
        let time = if self.peek() == b'/' {
            self.at += 1;
            self.offset()?
        } else {
            2 * SECSPERHOUR
        };
        Some(match rule {
            Rule::JulianDay(d, _) => Rule::JulianDay(d, time),
            Rule::DayOfYear(d, _) => Rule::DayOfYear(d, time),
            Rule::MonthNthDayOfWeek { mon, week, day, .. } => Rule::MonthNthDayOfWeek {
                mon,
                week,
                day,
                time,
            },
        })
    }
}

/// The bytes PostgreSQL keeps of a zone's abbreviations: the most a file's or a POSIX string's
/// take.
const CHARS_CAP: usize = 2 * (TZ_STRLEN_MAX + 1);

impl State {
    /// A POSIX TZ string, as PostgreSQL's `tzparse` reads it; `lastditch` reads the whole name as
    /// a standard zone at UTC, as PostgreSQL reads `GMT`.
    fn parse(name: &[u8], lastditch: bool) -> Option<State> {
        let mut p = Posix { s: name, at: 0 };
        let (stdname, stdoffset): (&[u8], i64) = if lastditch {
            p.at = name.len();
            (name, 0)
        } else {
            let stdname = if p.peek() == b'<' {
                p.at += 1;
                p.qzname()?
            } else {
                p.zname()
            };
            // An empty standard abbreviation is read, as it is not by the IANA code.
            if p.done() {
                return None;
            }
            (stdname, p.offset()?)
        };
        let stdlen = stdname.len();
        let mut charcnt = stdlen + 1;
        if CHARS_CAP < charcnt {
            return None;
        }
        let mut sp = State::default();
        let mut dstname: &[u8] = &[];
        if !p.done() {
            if p.peek() == b'<' {
                p.at += 1;
                dstname = p.qzname()?;
            } else {
                dstname = p.zname();
            }
            if dstname.is_empty() {
                return None;
            }
            charcnt += dstname.len() + 1;
            if CHARS_CAP < charcnt {
                return None;
            }
            let dstoffset = if !matches!(p.peek(), 0 | b',' | b';') {
                p.offset()?
            } else {
                stdoffset - SECSPERHOUR
            };
            let mut rules = Posix { s: name, at: p.at };
            if rules.done() {
                rules = Posix {
                    s: TZDEFRULESTRING,
                    at: 0,
                };
            }
            if !matches!(rules.peek(), b',' | b';') {
                return None;
            }
            rules.at += 1;
            let start = rules.rule()?;
            if rules.peek() != b',' {
                return None;
            }
            rules.at += 1;
            let end = rules.rule()?;
            if !rules.done() {
                return None;
            }
            sp.ttis = vec![
                Ttinfo::new(-stdoffset, false, 0),
                Ttinfo::new(-dstoffset, true, stdlen + 1),
            ];
            sp.defaulttype = 0;
            // Two transitions a year, from 200 years before 1970 on, for 400 years past the
            // last that changes.
            let mut janfirst = 0i64;
            let mut janoffset = 0i64;
            let mut yearbeg = EPOCH_YEAR;
            loop {
                let yearsecs = YEAR_LENGTHS[isleap(yearbeg - 1) as usize] * SECSPERDAY;
                yearbeg -= 1;
                match incremented(janfirst, -yearsecs) {
                    Some(t) => janfirst = t,
                    None => {
                        janoffset = -yearsecs;
                        break;
                    }
                }
                if EPOCH_YEAR - YEARSPERREPEAT / 2 >= yearbeg {
                    break;
                }
            }
            let mut yearlim = yearbeg + YEARSPERREPEAT + 1;
            let mut year = yearbeg;
            while year < yearlim {
                let mut starttime = start.transtime(year, stdoffset);
                let mut endtime = end.transtime(year, dstoffset);
                let yearsecs = YEAR_LENGTHS[isleap(year) as usize] * SECSPERDAY;
                let reversed = endtime < starttime;
                if reversed {
                    std::mem::swap(&mut starttime, &mut endtime);
                }
                if reversed
                    || (starttime < endtime
                        && endtime - starttime < yearsecs + (stdoffset - dstoffset))
                {
                    if TZ_MAX_TIMES - 2 < sp.ats.len() {
                        break;
                    }
                    if let Some(t) = incremented(janfirst, janoffset + starttime) {
                        sp.ats.push(t);
                        sp.types.push(!reversed as u8);
                    }
                    if let Some(t) = incremented(janfirst, janoffset + endtime) {
                        sp.ats.push(t);
                        sp.types.push(reversed as u8);
                        yearlim = year + YEARSPERREPEAT + 1;
                    }
                }
                match incremented(janfirst, janoffset + yearsecs) {
                    Some(t) => janfirst = t,
                    None => break,
                }
                janoffset = 0;
                year += 1;
            }
            if sp.ats.is_empty() {
                // Daylight time always.
                sp.ttis = vec![sp.ttis[1]];
            } else if YEARSPERREPEAT < year - yearbeg {
                sp.goback = true;
                sp.goahead = true;
            }
        } else {
            sp.ttis = vec![Ttinfo::new(-stdoffset, false, 0)];
            sp.defaulttype = 0;
        }
        sp.chars = Vec::with_capacity(charcnt + 1);
        sp.chars.extend_from_slice(stdname);
        sp.chars.push(0);
        if !dstname.is_empty() {
            sp.chars.extend_from_slice(dstname);
            sp.chars.push(0);
        }
        sp.chars.push(0);
        Some(sp)
    }

    /// The seconds leap seconds have corrected by `t`.
    fn leapcorr(&self, t: i64) -> i64 {
        self.leaps
            .iter()
            .rev()
            .find(|l| t >= l.trans)
            .map_or(0, |l| l.corr)
    }

    /// Whether two types are the same in every respect, abbreviation among them.
    fn typesequiv(&self, a: usize, b: usize) -> bool {
        match (self.ttis.get(a), self.ttis.get(b)) {
            (Some(x), Some(y)) => {
                x.utoff == y.utoff
                    && x.isdst == y.isdst
                    && x.ttisstd == y.ttisstd
                    && x.ttisut == y.ttisut
                    && cstr(&self.chars, x.desigidx) == cstr(&self.chars, y.desigidx)
            }
            _ => false,
        }
    }

    /// The offset east of UTC that the zone gives the instant `t`, leap seconds taken off; none
    /// where a time far outside its transitions cannot be brought inside them.
    fn utc_offset(&self, t: i64) -> Option<i64> {
        let (i, at) = self.type_at(t)?;
        Some(self.ttis[i].utoff - self.leapcorr(at))
    }

    /// The abbreviation the zone gives the instant `t`.
    #[cfg_attr(not(feature = "zones"), allow(dead_code))]
    fn abbrev(&self, t: i64) -> Option<String> {
        let (i, _) = self.type_at(t)?;
        Some(String::from_utf8_lossy(cstr(&self.chars, self.ttis[i].desigidx)).into_owned())
    }

    /// The abbreviations the zone lists, in its list's order.
    fn abbrevs(&self) -> Vec<String> {
        let count = self.chars.len().saturating_sub(1);
        let mut out = Vec::new();
        let mut at = 0;
        while at < count {
            let a = cstr(&self.chars, at);
            out.push(String::from_utf8_lossy(a).into_owned());
            at += a.len() + 1;
        }
        out
    }

    /// Where the zone's list holds the abbreviation `abbrev`.
    fn abbrev_index(&self, abbrev: &[u8]) -> Option<usize> {
        let count = self.chars.len().saturating_sub(1);
        let mut at = 0;
        while at < count {
            let a = cstr(&self.chars, at);
            if a == abbrev {
                return Some(at);
            }
            at += a.len() + 1;
        }
        None
    }

    /// Whether a local time type gives the abbreviation `abbrev`, and where every one giving it
    /// gives it the same offset and daylight time, those.
    fn abbrev_known(&self, abbrev: &[u8]) -> Option<Option<(i64, bool)>> {
        let at = self.abbrev_index(abbrev)?;
        let mut found = None;
        for t in self.ttis.iter().filter(|t| t.desigidx == at) {
            match found {
                None => found = Some((t.utoff, t.isdst)),
                Some(m) if m != (t.utoff, t.isdst) => return Some(None),
                Some(_) => {}
            }
        }
        found.map(Some)
    }

    /// The offset and daylight time the abbreviation `abbrev` means at the instant `t`: the last
    /// type giving it before `t`, else the type of a time before the first transition, else the
    /// first giving it after `t`.
    fn abbrev_meaning(&self, abbrev: &[u8], t: i64) -> Option<(i64, bool)> {
        let at = self.abbrev_index(abbrev)?;
        let of = |i: usize| {
            self.ttis
                .get(i)
                .filter(|x| x.desigidx == at)
                .map(|x| (x.utoff, x.isdst))
        };
        let cutoff = self.ats.partition_point(|&a| a <= t);
        let types = |r: std::ops::Range<usize>| r.map(|i| self.types[i] as usize);
        types(0..cutoff)
            .rev()
            .find_map(of)
            .or_else(|| of(self.defaulttype))
            .or_else(|| types(cutoff..self.ats.len()).find_map(of))
    }

    /// The local time type in force at the instant `t`, and the instant it was read at: `t`, or a
    /// time 400 years apart inside the transitions where `t` lies far outside them.
    fn type_at(&self, t: i64) -> Option<(usize, i64)> {
        let n = self.ats.len();
        if (self.goback && t < self.ats[0]) || (self.goahead && t > self.ats[n - 1]) {
            let seconds = if t < self.ats[0] {
                self.ats[0].checked_sub(t)?
            } else {
                t.checked_sub(self.ats[n - 1])?
            } - 1;
            let years = (seconds / SECSPERREPEAT + 1) * YEARSPERREPEAT;
            let shift = years.checked_mul(AVGSECSPERYEAR)?;
            let newt = if t < self.ats[0] {
                t.checked_add(shift)?
            } else {
                t.checked_sub(shift)?
            };
            if newt < self.ats[0] || newt > self.ats[n - 1] {
                return None;
            }
            return self.type_at(newt);
        }
        let i = if n == 0 || t < self.ats[0] {
            self.defaulttype
        } else {
            let (mut lo, mut hi) = (1, n);
            while lo < hi {
                let mid = (lo + hi) >> 1;
                if t < self.ats[mid] {
                    hi = mid;
                } else {
                    lo = mid + 1;
                }
            }
            self.types[lo - 1] as usize
        };
        Some((i, t))
    }

    /// The type in force at `t`, and where the zone next changes after it with the type it
    /// changes to; none for the change where no transition follows.
    #[allow(clippy::type_complexity)]
    fn next_boundary(&self, t: i64) -> Option<(Ttinfo, Option<(i64, Ttinfo)>)> {
        let n = self.ats.len();
        if n == 0 {
            return Some((self.ttis[self.defaulttype], None));
        }
        if (self.goback && t < self.ats[0]) || (self.goahead && t > self.ats[n - 1]) {
            let seconds = if t < self.ats[0] {
                self.ats[0].checked_sub(t)?
            } else {
                t.checked_sub(self.ats[n - 1])?
            } - 1;
            let tcycles = seconds / YEARSPERREPEAT / AVGSECSPERYEAR + 1;
            let shift = tcycles
                .checked_mul(YEARSPERREPEAT)?
                .checked_mul(AVGSECSPERYEAR)?;
            let newt = if t < self.ats[0] {
                t.checked_add(shift)?
            } else {
                t.checked_sub(shift)?
            };
            if newt < self.ats[0] || newt > self.ats[n - 1] {
                return None;
            }
            let (before, next) = self.next_boundary(newt)?;
            let next = match next {
                Some((b, after)) => Some((
                    if t < self.ats[0] {
                        b.checked_sub(shift)?
                    } else {
                        b.checked_add(shift)?
                    },
                    after,
                )),
                None => None,
            };
            return Some((before, next));
        }
        if t >= self.ats[n - 1] {
            return Some((self.ttis[self.types[n - 1] as usize], None));
        }
        if t < self.ats[0] {
            return Some((
                self.ttis[self.defaulttype],
                Some((self.ats[0], self.ttis[self.types[0] as usize])),
            ));
        }
        let (mut lo, mut hi) = (1, n - 1);
        while lo < hi {
            let mid = (lo + hi) >> 1;
            if t < self.ats[mid] {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        Some((
            self.ttis[self.types[lo - 1] as usize],
            Some((self.ats[lo], self.ttis[self.types[lo] as usize])),
        ))
    }

    /// The offset east of UTC that the zone gives the local time `mytime`, seconds after 1970 on
    /// its own clock, as PostgreSQL's DetermineTimeZoneOffset resolves it: UTC for a day outside
    /// the Julian days PostgreSQL counts.
    fn local_offset(&self, mytime: i64) -> i64 {
        let day = mytime.div_euclid(SECSPERDAY);
        if !(FIRST_JULIAN_DAY..END_JULIAN_DAY).contains(&day) {
            return 0;
        }
        let Some(prevtime) = mytime.checked_sub(SECSPERDAY) else {
            return 0;
        };
        let Some((before, next)) = self.next_boundary(prevtime) else {
            return 0;
        };
        let Some((boundary, after)) = next else {
            return before.utoff;
        };
        let (Some(beforetime), Some(aftertime)) = (
            mytime.checked_sub(before.utoff),
            mytime.checked_sub(after.utoff),
        ) else {
            return 0;
        };
        if beforetime < boundary && aftertime < boundary {
            return before.utoff;
        }
        if beforetime > boundary && aftertime >= boundary {
            return after.utoff;
        }
        // A time that never happens takes the offset before the change; one that happens twice,
        // the offset after it.
        if beforetime > aftertime {
            before.utoff
        } else {
            after.utoff
        }
    }
}

/// The days after 1970-01-01 of the first day of PostgreSQL's first Julian month, November of
/// 4714 BC, and of the day after its last, May 5874898.
const FIRST_JULIAN_DAY: i64 = -2_440_611;
const END_JULIAN_DAY: i64 = 2_145_043_057;

#[cfg(feature = "zones")]
pub use read::{canonical, digest, transitions};
#[cfg(feature = "zones")]
pub use read::{directory, set_directory};

#[cfg(feature = "zones")]
mod read {
    use super::*;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex, RwLock};

    /// The bytes of a zone file PostgreSQL reads at most: the rest is never seen.
    const FILE_BYTES: usize = 59_064;
    /// The longest path PostgreSQL opens.
    const MAXPGPATH: usize = 1024;
    /// A file's header: its magic, version, reserved bytes and six counts.
    const TZHEAD: usize = 44;

    static DIRECTORY: RwLock<Option<PathBuf>> = RwLock::new(None);

    #[allow(clippy::type_complexity)]
    static ZONES: Mutex<Option<HashMap<(PathBuf, String), Arc<Zone>>>> = Mutex::new(None);

    /// Where compiled zones are read, for every snail the process opens.
    pub fn set_directory(dir: PathBuf) {
        *DIRECTORY.write().unwrap_or_else(|e| e.into_inner()) = Some(dir);
    }

    /// Where compiled zones are read: the machine's, unless one was set.
    pub fn directory() -> PathBuf {
        DIRECTORY
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_else(|| PathBuf::from("/usr/share/zoneinfo"))
    }

    /// A zone as read: its name as the lookup found it, its state, and the digest of what it was
    /// read from.
    pub(super) struct Zone {
        pub(super) canonical: String,
        pub(super) state: State,
        pub(super) digest: u64,
    }

    /// CRC-32 of the bytes above their count.
    fn digest_of(bytes: &[u8]) -> u64 {
        (u64::from(crc32fast::hash(bytes)) << 32) | (bytes.len() as u64 & 0xffff_ffff)
    }

    /// A digest of the zone's rules as read: of its file's bytes, or of its own name where it is a
    /// POSIX TZ string no file has.
    pub fn digest(name: &str) -> Result<u64> {
        Ok(zone(name)?.digest)
    }

    /// The UTC seconds of every transition of the zone as PostgreSQL loads it: its file's, then
    /// those its footer's rule adds.
    pub fn transitions(name: &str) -> Result<Vec<i64>> {
        Ok(zone(name)?.state.ats.clone())
    }

    /// The zone's name as PostgreSQL's lookup finds it, case and all.
    pub fn canonical(name: &str) -> Result<String> {
        Ok(zone(name)?.canonical.clone())
    }

    /// Every zone the directory holds, walked as PostgreSQL's pg_tzenumerate walks it: each entry
    /// in the order the directory gives it, a hidden one passed over, a directory entered where it
    /// stands, a file read as a zone file or passed over; with each zone's abbreviation, offset and
    /// daylight time at the instant `utc`.
    pub(super) fn every_zone_at(utc: i64) -> Result<Vec<(String, String, i32, bool)>> {
        let base = directory();
        let mut out = Vec::new();
        let mut stack = vec![std::fs::read_dir(&base).map_err(|e| {
            Error::new(format!("could not open directory \"{}\": {e}", base.display()))
        })?];
        while let Some(dir) = stack.last_mut() {
            let Some(entry) = dir.next() else {
                stack.pop();
                continue;
            };
            let Ok(entry) = entry else {
                continue;
            };
            let name = entry.file_name();
            if name.to_string_lossy().starts_with('.') {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                if stack.len() >= 10 {
                    return Err(Error::new("timezone directory stack overflow"));
                }
                stack.push(std::fs::read_dir(&path).map_err(|e| {
                    Error::new(format!("could not open directory \"{}\": {e}", path.display()))
                })?);
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let Some(state) = load_file(&bytes[..bytes.len().min(FILE_BYTES)]) else {
                continue;
            };
            // PostgreSQL's pg_tz_acceptable: midnight UTC of 2000-01-01 on a whole minute.
            let t2000 = 946_684_800;
            if state
                .utc_offset(t2000)
                .is_none_or(|east| (t2000 + east).rem_euclid(60) != 0)
            {
                continue;
            }
            let Some((i, at)) = state.type_at(utc) else {
                continue;
            };
            let tt = state.ttis[i];
            let abbrev = String::from_utf8_lossy(cstr(&state.chars, tt.desigidx)).into_owned();
            if abbrev.len() > 31 {
                continue;
            }
            let rel = path
                .strip_prefix(&base)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            let east = tt.utoff - state.leapcorr(at);
            out.push((rel, abbrev, east as i32, tt.isdst));
        }
        Ok(out)
    }

    /// The zone `name` names, read once for each directory and kept.
    pub(super) fn zone(name: &str) -> Result<Arc<Zone>> {
        if name.len() > TZ_STRLEN_MAX {
            return Err(unknown(name));
        }
        let dir = directory();
        let upper = name.to_ascii_uppercase();
        let key = (dir.clone(), upper.clone());
        let mut zones = ZONES.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(z) = zones.as_ref().and_then(|m| m.get(&key)) {
            return Ok(z.clone());
        }
        let zone = Arc::new(load(&dir, name, &upper)?);
        zones
            .get_or_insert_with(HashMap::new)
            .insert(key, zone.clone());
        Ok(zone)
    }

    /// The zone read as PostgreSQL's `pg_tzset` reads it.
    fn load(dir: &Path, name: &str, upper: &str) -> Result<Zone> {
        let posix = |why: Option<String>| -> Result<Zone> {
            match (!upper.starts_with(':'))
                .then(|| State::parse(upper.as_bytes(), false))
                .flatten()
            {
                Some(state) => Ok(Zone {
                    canonical: upper.to_string(),
                    state,
                    digest: digest_of(upper.as_bytes()),
                }),
                None => Err(match why {
                    Some(why) => Error::new(format!("{}: {why}", unknown(name))),
                    None => unknown(name),
                }),
            }
        };
        if upper == "GMT" {
            return Ok(Zone {
                canonical: upper.to_string(),
                state: State::parse(upper.as_bytes(), true)
                    .ok_or_else(|| Error::new("could not initialize GMT time zone"))?,
                digest: digest_of(upper.as_bytes()),
            });
        }
        let found = open(dir, upper.strip_prefix(':').unwrap_or(upper));
        let Some((path, canonical)) = found else {
            return posix(None);
        };
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(_) => return posix(None),
        };
        match load_file(&bytes[..bytes.len().min(FILE_BYTES)]) {
            Some(state) => Ok(Zone {
                canonical,
                state,
                digest: digest_of(&bytes),
            }),
            None => posix(Some(format!(
                "its file {} is not a zone file PostgreSQL reads",
                path.display()
            ))),
        }
    }

    /// The file a zone name names in `dir`, each part of the name matched without regard to case
    /// and no hidden entry matched, and the name as the directory spells it.
    fn open(dir: &Path, name: &str) -> Option<(PathBuf, String)> {
        let base = dir.as_os_str().len();
        if base + 1 + name.len() >= MAXPGPATH {
            return None;
        }
        let mut path = dir.to_path_buf();
        let mut canonical: Vec<String> = Vec::new();
        for part in name.split('/') {
            let entries = std::fs::read_dir(&path).ok()?;
            let found = entries.flatten().find_map(|e| {
                let n = e.file_name().into_string().ok()?;
                (!n.starts_with('.') && n.len() == part.len() && n.eq_ignore_ascii_case(part))
                    .then_some(n)
            })?;
            path.push(&found);
            canonical.push(found);
        }
        Some((path, canonical.join("/")))
    }

    fn code32(b: &[u8]) -> i64 {
        i64::from(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn code64(b: &[u8]) -> i64 {
        i64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
    }

    /// A compiled zone file, as PostgreSQL's `tzloadbody` reads one: its 32-bit part, then where the
    /// file has one its 64-bit part, then its footer's rule extending its transitions; none for a
    /// file it refuses.
    fn load_file(file: &[u8]) -> Option<State> {
        let mut buf: &[u8] = file;
        if buf.len() < TZHEAD {
            return None;
        }
        let mut sp = State::default();
        let mut stored = 4;
        while stored <= 8 {
            if buf.len() < TZHEAD {
                return None;
            }
            let count = |at: usize| code32(&buf[20 + 4 * at..]);
            let (ttisutcnt, ttisstdcnt, leapcnt, timecnt, typecnt, charcnt) =
                (count(0), count(1), count(2), count(3), count(4), count(5));
            let version = buf[4];
            if !((0..TZ_MAX_LEAPS as i64).contains(&leapcnt)
                && (0..TZ_MAX_TYPES as i64).contains(&typecnt)
                && (0..TZ_MAX_TIMES as i64).contains(&timecnt)
                && (0..TZ_MAX_CHARS as i64).contains(&charcnt)
                && (ttisstdcnt == typecnt || ttisstdcnt == 0)
                && (ttisutcnt == typecnt || ttisutcnt == 0))
            {
                return None;
            }
            let (leapcnt, timecnt, typecnt, charcnt) = (
                leapcnt as usize,
                timecnt as usize,
                typecnt as usize,
                charcnt as usize,
            );
            let (ttisstdcnt, ttisutcnt) = (ttisstdcnt as usize, ttisutcnt as usize);
            let need = TZHEAD
                + timecnt * stored
                + timecnt
                + typecnt * 6
                + charcnt
                + leapcnt * (stored + 4)
                + ttisstdcnt
                + ttisutcnt;
            if buf.len() < need {
                return None;
            }
            let mut p = TZHEAD;
            let code = |b: &[u8]| if stored == 4 { code32(b) } else { code64(b) };
            // Transitions in order, one equal to the one before it taking that one's place.
            let mut ats: Vec<i64> = Vec::with_capacity(timecnt);
            let mut kept = vec![true; timecnt];
            for i in 0..timecnt {
                let attime = code(&buf[p..]);
                if let Some(&last) = ats.last() {
                    if attime <= last {
                        if attime < last {
                            return None;
                        }
                        kept[i - 1] = false;
                        ats.pop();
                    }
                }
                ats.push(attime);
                p += stored;
            }
            let mut types = Vec::with_capacity(ats.len());
            for &k in kept.iter() {
                let typ = buf[p];
                p += 1;
                if typecnt <= typ as usize {
                    return None;
                }
                if k {
                    types.push(typ);
                }
            }
            let mut ttis = Vec::with_capacity(typecnt);
            for _ in 0..typecnt {
                let utoff = code32(&buf[p..]);
                let isdst = buf[p + 4];
                let desigidx = buf[p + 5] as usize;
                p += 6;
                if isdst >= 2 || desigidx >= charcnt {
                    return None;
                }
                ttis.push(Ttinfo::new(utoff, isdst == 1, desigidx));
            }
            let mut chars = buf[p..p + charcnt].to_vec();
            chars.push(0);
            p += charcnt;
            let mut leaps = Vec::with_capacity(leapcnt);
            let (mut prevtr, mut prevcorr) = (0i64, 0i64);
            for _ in 0..leapcnt {
                let tr = code(&buf[p..]);
                let corr = code32(&buf[p + stored..]);
                p += stored + 4;
                if tr < 0 {
                    return None;
                }
                if tr - prevtr < 28 * SECSPERDAY - 1
                    || (corr != prevcorr - 1 && corr != prevcorr + 1)
                {
                    return None;
                }
                prevtr = tr;
                prevcorr = corr;
                leaps.push(Leap { trans: tr, corr });
            }
            for (cnt, std) in [(ttisstdcnt, true), (ttisutcnt, false)] {
                for t in ttis.iter_mut() {
                    let flag = if cnt == 0 {
                        false
                    } else {
                        let b = buf[p];
                        p += 1;
                        if b > 1 {
                            return None;
                        }
                        b == 1
                    };
                    if std {
                        t.ttisstd = flag;
                    } else {
                        t.ttisut = flag;
                    }
                }
            }
            sp = State {
                ats,
                types,
                ttis,
                chars,
                leaps,
                ..State::default()
            };
            if version == 0 {
                buf = &[];
                break;
            }
            buf = &buf[p..];
            stored *= 2;
        }
        extend(&mut sp, buf);
        if sp.ttis.is_empty() {
            return None;
        }
        let n = sp.ats.len();
        if n > 1 {
            sp.goback = (1..n).any(|i| {
                sp.typesequiv(sp.types[i] as usize, sp.types[0] as usize)
                    && sp.ats[i].checked_sub(sp.ats[0]) == Some(SECSPERREPEAT)
            });
            sp.goahead = (0..n - 1).rev().any(|i| {
                sp.typesequiv(sp.types[n - 1] as usize, sp.types[i] as usize)
                    && sp.ats[n - 1].checked_sub(sp.ats[i]) == Some(SECSPERREPEAT)
            });
        }
        sp.defaulttype = defaulttype(&sp);
        Some(sp)
    }

    /// The footer's rule, `\n<POSIX TZ string>\n`, extending a zone's transitions past its last,
    /// where its abbreviations fit beside the file's.
    fn extend(sp: &mut State, footer: &[u8]) {
        let n = footer.len();
        if !(n > 2
            && footer[0] == b'\n'
            && footer[n - 1] == b'\n'
            && sp.ttis.len() + 2 <= TZ_MAX_TYPES)
        {
            return;
        }
        let text = &footer[1..n - 1];
        let text = &text[..text.iter().position(|&b| b == 0).unwrap_or(text.len())];
        let Some(mut ts) = State::parse(text, false) else {
            return;
        };
        let chars = sp.chars.clone();
        // The file's own abbreviations taken where they match, else added while they fit.
        let mut charcnt = sp.chars.len() - 1;
        let mut gotabbr = 0;
        for t in ts.ttis.iter_mut() {
            let abbr = cstr(&ts.chars, t.desigidx).to_vec();
            match (0..charcnt).find(|&j| cstr(&sp.chars, j) == abbr.as_slice()) {
                Some(j) => {
                    t.desigidx = j;
                    gotabbr += 1;
                }
                None => {
                    let j = charcnt;
                    if j + abbr.len() < TZ_MAX_CHARS {
                        sp.chars.truncate(j);
                        sp.chars.extend_from_slice(&abbr);
                        sp.chars.push(0);
                        sp.chars.push(0);
                        charcnt = j + abbr.len() + 1;
                        t.desigidx = j;
                        gotabbr += 1;
                    }
                }
            }
        }
        if gotabbr != ts.ttis.len() {
            sp.chars = chars;
            return;
        }
        sp.chars.truncate(charcnt);
        sp.chars.push(0);
        // Trailing transitions that change nothing are passed over.
        while sp.ats.len() > 1 && sp.types[sp.types.len() - 1] == sp.types[sp.types.len() - 2] {
            sp.ats.pop();
            sp.types.pop();
        }
        let mut i = 0;
        while i < ts.ats.len() {
            let at = ts.ats[i] + sp.leapcorr(ts.ats[i]);
            if sp.ats.last().is_none_or(|&last| last < at) {
                break;
            }
            i += 1;
        }
        let base = sp.ttis.len() as u8;
        while i < ts.ats.len() && sp.ats.len() < TZ_MAX_TIMES {
            sp.ats.push(ts.ats[i] + sp.leapcorr(ts.ats[i]));
            sp.types.push(base + ts.types[i]);
            i += 1;
        }
        sp.ttis.append(&mut ts.ttis);
    }

    /// The type of a time before the first transition, as PostgreSQL infers it from the file.
    fn defaulttype(sp: &State) -> usize {
        let n = sp.ats.len();
        let mut i: i64 = if sp.types.contains(&0) { -1 } else { 0 };
        if i < 0 && n > 0 && sp.ttis[sp.types[0] as usize].isdst {
            i = i64::from(sp.types[0]);
            loop {
                i -= 1;
                if i < 0 || !sp.ttis[i as usize].isdst {
                    break;
                }
            }
        }
        if i < 0 {
            i = 0;
            while sp.ttis[i as usize].isdst {
                i += 1;
                if i as usize >= sp.ttis.len() {
                    i = 0;
                    break;
                }
            }
        }
        i as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fixed_offset_is_read_as_posix_reads_it_west_of_utc_positive() {
        assert_eq!(fixed_offset("+02"), Some(-7200));
        assert_eq!(fixed_offset("utc+3"), Some(-10800));
        assert_eq!(fixed_offset("<+03>-03"), Some(10800));
        assert_eq!(fixed_offset("EST5"), Some(-18000));
        assert_eq!(fixed_offset("UTC-05:30"), Some(19800));
        assert_eq!(fixed_offset("gmt"), Some(0));
        assert_eq!(fixed_offset("Etc/UTC"), Some(0));
        assert_eq!(fixed_offset("EST5EDT"), None);
        assert_eq!(fixed_offset("Europe/Lisbon"), None);
        assert_eq!(fixed_offset(""), None);
        assert_eq!(fixed_offset(":+02"), None);
        assert_eq!(fixed_offset("<+03"), None);
        assert_eq!(fixed_offset("+169"), None);
        assert_eq!(fixed_offset("+167"), Some(-167 * 3600));
    }

    #[test]
    fn a_posix_rule_changes_twice_a_year_as_the_iana_code_computes_it() {
        let st = State::parse(b"EST5EDT", false).unwrap();
        // 2024-03-10 07:00 UTC and 2024-11-03 06:00 UTC, the United States' changes.
        let spring = 1_710_054_000;
        let fall = 1_730_613_600;
        assert_eq!(st.utc_offset(spring - 1), Some(-18000));
        assert_eq!(st.utc_offset(spring), Some(-14400));
        assert_eq!(st.utc_offset(fall - 1), Some(-14400));
        assert_eq!(st.utc_offset(fall), Some(-18000));
        // 02:30 that morning never happens: the offset before; 01:30 in November happens twice:
        // the offset after.
        assert_eq!(st.local_offset(spring - 18000 + 1800), -18000);
        assert_eq!(st.local_offset(fall - 14400 - 1800), -18000);
        assert!(st.goback && st.goahead);
    }

    #[cfg(not(feature = "zones"))]
    #[test]
    fn a_zone_other_than_utc_is_refused_by_name_without_the_feature() {
        let e = utc_offset("Europe/Lisbon", 0).unwrap_err().0;
        assert_eq!(
            e,
            "time zone \"Europe/Lisbon\" is a zone other than UTC, read only under the feature zones"
        );
        assert!(local_offset("EST5EDT", 0).is_err());
        assert_eq!(utc_offset("+02", 0).unwrap(), -7200);
        assert_eq!(local_offset("Etc/UTC", 123).unwrap(), 0);
    }

    /// The tests that set the directory, one at a time, as it is the process's.
    #[cfg(feature = "zones")]
    static DIRECTORY_SET: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[cfg(feature = "zones")]
    fn pg_directory() -> std::path::PathBuf {
        std::path::PathBuf::from(std::env::var("HOME").unwrap())
            .join(".pg-release/19/share/postgresql/timezone")
    }

    #[cfg(feature = "zones")]
    #[test]
    fn a_zone_reads_as_postgresql_19_reads_its_own_copy() {
        let _one = DIRECTORY_SET.lock().unwrap_or_else(|e| e.into_inner());
        set_directory(pg_directory());
        // Measured on 19 RC1: Lisbon's spring change of 2024 at 01:00 UTC, its clock from 01:00 to
        // 02:00; 1900 on its local mean time.
        assert_eq!(utc_offset("Europe/Lisbon", 1_711_846_799).unwrap(), 0);
        assert_eq!(utc_offset("europe/lisbon", 1_711_846_800).unwrap(), 3600);
        assert_eq!(utc_offset("Europe/Lisbon", -2_208_988_800).unwrap(), -2205);
        assert_eq!(canonical("europe/LISBON").unwrap(), "Europe/Lisbon");
        // 01:30 on the morning of the spring change never happens, and is read at the offset
        // before it; on the autumn morning it happens twice, and is read at the offset after.
        assert_eq!(local_offset("Europe/Lisbon", 1_711_848_600).unwrap(), 0);
        assert_eq!(local_offset("Europe/Lisbon", 1_729_992_600).unwrap(), 0);
        assert_eq!(local_offset("Europe/Lisbon", 1_729_989_000).unwrap(), 3600);
        // Past its file's transitions, the footer's rule: 2400's summer.
        assert_eq!(utc_offset("Europe/Lisbon", 13_585_233_600).unwrap(), 3600);
        assert_eq!(
            utc_offset("America/New_York", 13_585_233_600).unwrap(),
            -14400
        );
        // A POSIX name no file has; GMT read without its file.
        assert_eq!(utc_offset("+02", 0).unwrap(), -7200);
        assert_eq!(canonical("utc+3").unwrap(), "UTC+3");
        assert_eq!(canonical("gmt").unwrap(), "GMT");
        assert!(utc_offset("Mars/Olympus", 0)
            .unwrap_err()
            .0
            .starts_with("time zone \"Mars/Olympus\" not recognized"));
        assert!(utc_offset("../timezone/UTC", 0).is_err());
        assert!(!transitions("America/New_York").unwrap().is_empty());
        assert_ne!(digest("UTC").unwrap(), digest("Europe/Lisbon").unwrap());
    }

    #[cfg(feature = "zones")]
    #[test]
    fn a_short_or_malformed_zone_file_is_refused_by_name_never_read() {
        let _one = DIRECTORY_SET.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("zones-malformed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let good = std::fs::read(pg_directory().join("Europe/Lisbon")).unwrap();
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("Short", b"TZif2".to_vec()),
            ("Cut", good[..good.len() / 2].to_vec()),
            ("Counts", {
                let mut b = good.clone();
                b[32..36].copy_from_slice(&5000i32.to_be_bytes());
                b
            }),
            ("Types", {
                let mut b = good.clone();
                b[36..40].copy_from_slice(&(-1i32).to_be_bytes());
                b
            }),
            ("Empty", Vec::new()),
        ];
        for (name, bytes) in &cases {
            std::fs::write(dir.join(name), bytes).unwrap();
        }
        set_directory(dir.clone());
        for (name, _) in &cases {
            let e = utc_offset(name, 0).unwrap_err().0;
            assert!(e.contains("not recognized"), "{name}: {e}");
        }
        set_directory(pg_directory());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
